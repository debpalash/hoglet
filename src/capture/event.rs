//! Request-body parsing and per-event field resolution
//! (compat-spec.md "Request body").
//!
//! The body is an untagged union of four shapes; token, distinct_id,
//! timestamp, and uuid resolution each follow PostHog's exact precedence.
//! Get these wrong and real SDK contract tests fail — nothing here is
//! discretionary.

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::token;

/// distinct_id is truncated to this many characters (chars, not bytes).
pub const MAX_DISTINCT_ID_CHARS: usize = 200;

/// Events stamped further than this into the future are clamped to now.
const MAX_FUTURE_HOURS: i64 = 23;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CapturedEvent {
    pub uuid: Uuid,
    pub event: String,
    pub distinct_id: String,
    pub token: String,
    pub timestamp: DateTime<Utc>,
    pub properties: Map<String, Value>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// 400 — malformed body, missing event name, missing distinct_id, bad
    /// timestamp.
    Malformed(&'static str),
    /// 401 — missing, mismatched, or invalid token.
    Unauthorized(&'static str),
}

#[derive(Debug)]
pub struct ParsedBatch {
    pub events: Vec<CapturedEvent>,
    pub historical_migration: bool,
}

/// Parse a decoded JSON body into resolved events.
///
/// `sent_at_query` is the `_` query param (ms since epoch, used for clock-skew
/// correction when the batch has no `sent_at` field).
pub fn parse_body(
    body: &str,
    sent_at_query: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<ParsedBatch, CaptureError> {
    let value: Value =
        serde_json::from_str(body).map_err(|_| CaptureError::Malformed("invalid JSON"))?;

    let mut batch_token: Option<String> = None;
    let mut sent_at = sent_at_query;
    let mut historical_migration = false;

    let raw_events: Vec<Value> = match value {
        // Bare array of events — what browser posthog-js posts to /e.
        Value::Array(events) => events,
        Value::Object(mut obj) => {
            if let Some(batch) = obj.remove("batch") {
                // Batch shape — server SDKs. Batch-level token wins.
                batch_token = obj
                    .get("token")
                    .or_else(|| obj.get("api_key"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(s) = obj.get("sent_at").and_then(Value::as_str) {
                    sent_at = parse_timestamp(s);
                }
                historical_migration = obj
                    .get("historical_migration")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match batch {
                    Value::Array(events) => events,
                    _ => return Err(CaptureError::Malformed("batch must be an array")),
                }
            } else {
                // Single event, or engage shape (no `event` field →
                // synthesize $identify).
                if !obj.contains_key("event") {
                    obj.insert("event".into(), Value::String("$identify".into()));
                }
                vec![Value::Object(obj)]
            }
        }
        _ => return Err(CaptureError::Malformed("body must be an object or array")),
    };

    let mut events = Vec::with_capacity(raw_events.len());
    // Uniformity check: without a batch-level token, every event must carry
    // the same one. The batch token itself must never leak into per-event
    // resolution as if the event carried it.
    let mut seen_token: Option<String> = None;

    for raw in raw_events {
        let Value::Object(obj) = raw else {
            return Err(CaptureError::Malformed("event must be an object"));
        };

        // Drop $performance_event silently; empty result is still a 200.
        if obj.get("event").and_then(Value::as_str) == Some("$performance_event") {
            continue;
        }

        let event = resolve_event(obj, batch_token.as_deref(), sent_at, now)?;

        match &seen_token {
            None => seen_token = Some(event.token.clone()),
            Some(t) if *t != event.token => {
                return Err(CaptureError::Unauthorized("mismatched tokens in batch"));
            }
            Some(_) => {}
        }

        events.push(event);
    }

    Ok(ParsedBatch {
        events,
        historical_migration,
    })
}

fn resolve_event(
    mut obj: Map<String, Value>,
    batch_token: Option<&str>,
    sent_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<CapturedEvent, CaptureError> {
    let name = match obj.get("event").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => return Err(CaptureError::Malformed("missing event name")),
    };

    let mut properties = match obj.remove("properties") {
        Some(Value::Object(map)) => map,
        Some(_) => return Err(CaptureError::Malformed("properties must be an object")),
        None => Map::new(),
    };

    // $set / $set_once at the top level fold into properties.
    for key in ["$set", "$set_once"] {
        if let Some(v) = obj.remove(key)
            && !properties.contains_key(key)
        {
            properties.insert(key.into(), v);
        }
    }

    let token = resolve_token(&obj, &properties, batch_token)?;
    let distinct_id = resolve_distinct_id(&obj, &properties)?;
    let timestamp = resolve_timestamp(&obj, &properties, sent_at, now)?;

    // Client-supplied uuid wins; otherwise UUIDv7 seeded from the *resolved
    // event timestamp*, not ingestion time (compat-spec.md).
    let uuid = match obj.get("uuid").and_then(Value::as_str) {
        Some(s) => Uuid::parse_str(s).map_err(|_| CaptureError::Malformed("invalid uuid"))?,
        None => uuid_v7_at(timestamp),
    };

    Ok(CapturedEvent {
        uuid,
        event: name,
        distinct_id,
        token,
        timestamp,
        properties,
    })
}

/// Token precedence: batch-level wins; then event `token`/`$token`/`api_key`;
/// then `properties.token`.
fn resolve_token(
    obj: &Map<String, Value>,
    properties: &Map<String, Value>,
    batch_token: Option<&str>,
) -> Result<String, CaptureError> {
    let raw = batch_token
        .map(str::to_owned)
        .or_else(|| {
            ["token", "$token", "api_key"]
                .iter()
                .find_map(|k| obj.get(*k).and_then(Value::as_str).map(str::to_owned))
        })
        .or_else(|| {
            properties
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or(CaptureError::Unauthorized("no token"))?;

    token::validate(&raw).map_err(|_| CaptureError::Unauthorized("invalid token"))?;
    Ok(raw)
}

/// distinct_id precedence: top-level (alias `$distinct_id`), then
/// `properties.distinct_id`. Arbitrary JSON stringifies; null bytes become
/// U+FFFD; empty-after-trim rejects; truncate to 200 chars keeping the
/// untrimmed value.
fn resolve_distinct_id(
    obj: &Map<String, Value>,
    properties: &Map<String, Value>,
) -> Result<String, CaptureError> {
    let raw = obj
        .get("distinct_id")
        .or_else(|| obj.get("$distinct_id"))
        .or_else(|| properties.get("distinct_id"))
        .ok_or(CaptureError::Malformed("missing distinct_id"))?;

    let s = match raw {
        Value::Null => return Err(CaptureError::Malformed("null distinct_id")),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    let s = s.replace('\0', "\u{FFFD}");
    if s.trim().is_empty() {
        return Err(CaptureError::Malformed("empty distinct_id"));
    }
    Ok(s.chars().take(MAX_DISTINCT_ID_CHARS).collect())
}

/// Timestamp precedence (compat-spec.md "Timestamp"):
/// `offset` (ms ago) wins outright → now - offset. Else parse `timestamp`
/// and correct clock skew by `sent_at - now` unless `$ignore_sent_at`.
/// Clamp >23h future to now. No timestamp → now.
fn resolve_timestamp(
    obj: &Map<String, Value>,
    properties: &Map<String, Value>,
    sent_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, CaptureError> {
    if let Some(offset_ms) = obj.get("offset").and_then(Value::as_i64) {
        return Ok(now - Duration::milliseconds(offset_ms));
    }

    let raw = match obj.get("timestamp").and_then(Value::as_str) {
        Some(s) => s,
        None => return Ok(now),
    };
    let mut ts = parse_timestamp(raw).ok_or(CaptureError::Malformed("bad timestamp"))?;

    let ignore_sent_at = properties
        .get("$ignore_sent_at")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(sent_at) = sent_at
        && !ignore_sent_at
    {
        let skew = sent_at - now;
        ts -= skew;
    }

    if ts > now + Duration::hours(MAX_FUTURE_HOURS) {
        ts = now;
    }
    Ok(ts)
}

/// RFC3339 first, then looser fallbacks; normalize a bare `+NN` offset to
/// `+NN:00`.
pub fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(raw) {
        return Some(ts.with_timezone(&Utc));
    }
    // Bare +NN / -NN timezone offset.
    if raw.len() > 3 {
        let (head, tail) = raw.split_at(raw.len() - 3);
        if (tail.starts_with('+') || tail.starts_with('-'))
            && tail[1..].chars().all(|c| c.is_ascii_digit())
            && let Ok(ts) = DateTime::parse_from_rfc3339(&format!("{head}{tail}:00"))
        {
            return Some(ts.with_timezone(&Utc));
        }
    }
    // Naive datetime → UTC.
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// The `_` query param: milliseconds since epoch.
pub fn parse_sent_at_ms(raw: &str) -> Option<DateTime<Utc>> {
    let ms: i64 = raw.parse().ok()?;
    Utc.timestamp_millis_opt(ms).single()
}

fn uuid_v7_at(ts: DateTime<Utc>) -> Uuid {
    let secs = ts.timestamp().max(0) as u64;
    let nanos = ts.timestamp_subsec_nanos();
    Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap()
    }

    fn parse(body: &str) -> Result<ParsedBatch, CaptureError> {
        parse_body(body, None, now())
    }

    #[test]
    fn bare_array_shape() {
        let batch = parse(
            r#"[{"event":"click","distinct_id":"u1","properties":{"token":"phc_t"}}]"#,
        )
        .unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].event, "click");
        assert_eq!(batch.events[0].token, "phc_t");
    }

    #[test]
    fn batch_shape_token_wins() {
        let batch = parse(
            r#"{"api_key":"phc_batch","batch":[{"event":"e1","distinct_id":"u1","properties":{"token":"phc_other"}}]}"#,
        )
        .unwrap();
        assert_eq!(batch.events[0].token, "phc_batch");
    }

    #[test]
    fn engage_shape_synthesizes_identify() {
        let batch = parse(r#"{"distinct_id":"u1","token":"phc_t","$set":{"name":"Ada"}}"#).unwrap();
        assert_eq!(batch.events[0].event, "$identify");
        assert_eq!(batch.events[0].properties["$set"]["name"], "Ada");
    }

    #[test]
    fn mismatched_tokens_reject_401() {
        let err = parse(
            r#"[{"event":"a","distinct_id":"u","token":"phc_1"},{"event":"b","distinct_id":"u","token":"phc_2"}]"#,
        )
        .unwrap_err();
        assert!(matches!(err, CaptureError::Unauthorized(_)));
    }

    #[test]
    fn missing_distinct_id_rejects_400() {
        let err = parse(r#"[{"event":"a","token":"phc_1"}]"#).unwrap_err();
        assert_eq!(err, CaptureError::Malformed("missing distinct_id"));
    }

    #[test]
    fn numeric_distinct_id_stringifies() {
        let batch = parse(r#"[{"event":"a","distinct_id":42,"token":"phc_1"}]"#).unwrap();
        assert_eq!(batch.events[0].distinct_id, "42");
    }

    #[test]
    fn distinct_id_truncates_to_200_chars() {
        let long = "x".repeat(300);
        let batch = parse(&format!(
            r#"[{{"event":"a","distinct_id":"{long}","token":"phc_1"}}]"#
        ))
        .unwrap();
        assert_eq!(batch.events[0].distinct_id.chars().count(), 200);
    }

    #[test]
    fn performance_events_dropped_but_batch_accepted() {
        let batch = parse(
            r#"[{"event":"$performance_event","distinct_id":"u","token":"phc_1"}]"#,
        )
        .unwrap();
        assert!(batch.events.is_empty());
    }

    #[test]
    fn offset_wins_over_timestamp() {
        let batch = parse(
            r#"[{"event":"a","distinct_id":"u","token":"phc_1","offset":60000,"timestamp":"2020-01-01T00:00:00Z"}]"#,
        )
        .unwrap();
        assert_eq!(batch.events[0].timestamp, now() - Duration::minutes(1));
    }

    #[test]
    fn clock_skew_corrected_via_sent_at() {
        // Client clock 10 minutes fast: sent_at = now+10m, event stamped
        // now+10m → corrected back to now.
        let sent_at = now() + Duration::minutes(10);
        let batch = parse_body(
            r#"[{"event":"a","distinct_id":"u","token":"phc_1","timestamp":"2026-07-21T12:10:00Z"}]"#,
            Some(sent_at),
            now(),
        )
        .unwrap();
        assert_eq!(batch.events[0].timestamp, now());
    }

    #[test]
    fn far_future_clamps_to_now() {
        let batch = parse(
            r#"[{"event":"a","distinct_id":"u","token":"phc_1","timestamp":"2027-01-01T00:00:00Z"}]"#,
        )
        .unwrap();
        assert_eq!(batch.events[0].timestamp, now());
    }

    #[test]
    fn bare_offset_timezone_normalizes() {
        assert!(parse_timestamp("2026-07-21T12:00:00+03").is_some());
    }

    #[test]
    fn generated_uuid_is_v7_from_event_time() {
        let batch = parse(
            r#"[{"event":"a","distinct_id":"u","token":"phc_1","timestamp":"2026-07-21T11:00:00Z"}]"#,
        )
        .unwrap();
        let uuid = batch.events[0].uuid;
        assert_eq!(uuid.get_version_num(), 7);
        // v7 embeds the ms timestamp in the first 48 bits.
        let (secs, _) = uuid.get_timestamp().unwrap().to_unix();
        assert_eq!(secs, batch.events[0].timestamp.timestamp() as u64);
    }
}
