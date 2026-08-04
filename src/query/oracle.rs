//! Independent reference implementation of analytics, in plain Rust.
use std::collections::{BTreeMap, HashMap};
use chrono::{DateTime, Duration, Utc};
use crate::capture::event::CapturedEvent;
use crate::query::ir::*;

fn deduped<'a>(events: &'a [CapturedEvent], token: &str) -> Vec<&'a CapturedEvent> {
    let mut by_uuid: HashMap<uuid::Uuid, &CapturedEvent> = HashMap::new();
    for e in events { by_uuid.entry(e.uuid).and_modify(|k| { if e.timestamp < k.timestamp { *k = e; } }).or_insert(e); }
    let mut v: Vec<&CapturedEvent> = by_uuid.into_values().filter(|e| e.token == token).collect();
    v.sort_by_key(|e| e.timestamp);
    v
}

pub fn total_and_persons(events: &[CapturedEvent], token: &str) -> (i64, i64) { let d = deduped(events, token); let p: std::collections::HashSet<&str> = d.iter().map(|e| e.distinct_id.as_str()).collect(); (d.len() as i64, p.len() as i64) }
pub fn events_since(events: &[CapturedEvent], token: &str, cutoff: DateTime<Utc>) -> i64 { deduped(events, token).iter().filter(|e| e.timestamp >= cutoff).count() as i64 }
pub fn top_events(events: &[CapturedEvent], token: &str) -> HashMap<String, i64> { let mut c = HashMap::new(); for e in deduped(events, token) { *c.entry(e.event.clone()).or_insert(0) += 1; } c }
pub fn funnel(events: &[CapturedEvent], token: &str, steps: &[String]) -> Vec<i64> {
    if steps.is_empty() { return vec![]; }
    let d = deduped(events, token);
    let mut bp: HashMap<&str, Vec<(&DateTime<Utc>, &str)>> = HashMap::new();
    for e in &d { bp.entry(e.distinct_id.as_str()).or_default().push((&e.timestamp, e.event.as_str())); }
    for v in bp.values_mut() { v.sort_by_key(|(t,_)| **t); }
    let mut r = vec![0i64; steps.len()];
    for evs in bp.values() { let mut s = 0usize; for (_, ev) in evs { if *ev == steps[s] { r[s] += 1; s += 1; if s == steps.len() { break; } } } }
    r
}

pub fn run_ir(events: &[CapturedEvent], query: &Query, token: &str) -> QueryResponse {
    let start = std::time::Instant::now();
    let d = deduped(events, token);
    let f: Vec<&&CapturedEvent> = d.iter().filter(|e| pg_matches(e, &query.filters) && dr_matches(e, &query.range)).collect();
    match query.kind { QueryKind::Trends => trends_oracle(&f, query, start), _ => QueryResponse { results: vec![], meta: QueryMeta { kind: format!("{:?}", query.kind).to_lowercase(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } } }
}

fn trends_oracle(events: &[&&CapturedEvent], query: &Query, start: std::time::Instant) -> QueryResponse {
    let ifmt = match query.interval { Interval::Hour => "%Y-%m-%dT%H:00:00Z", Interval::Day => "%Y-%m-%d", Interval::Week => "%Y-%W", Interval::Month => "%Y-%m" };
    let bd = &query.breakdown;
    let mut buckets: BTreeMap<(String, Option<String>), Vec<&&CapturedEvent>> = BTreeMap::new();
    for e in events { let iv = e.timestamp.format(ifmt).to_string(); let bv = bd.as_ref().map(|b| gpv(e, &b.key, &b.source).unwrap_or_default()); buckets.entry((iv, bv)).or_default().push(*e); }
    let mut results = Vec::new();
    for (_si, series) in query.series.iter().enumerate() {
        let label = match &series.event { EventMatch::Name(n) => n.clone(), EventMatch::Any => "any event".into() };
        let mut data: BTreeMap<(String, Option<String>), i64> = BTreeMap::new();
        let mut tpb: HashMap<Option<String>, i64> = HashMap::new();
        for ((iv, bv), be) in &buckets {
            let m: Vec<&&CapturedEvent> = be.iter().filter(|e| em_matches(e, &series.event)).copied().collect();
            let c = cm_math(&m, &series.math); data.insert((iv.clone(), bv.clone()), c); *tpb.entry(bv.clone()).or_default() += c;
        }
        if let Some(b) = bd {
            let mut bt: Vec<(Option<String>, i64)> = tpb.into_iter().collect(); bt.sort_by(|a,b| b.1.cmp(&a.1)); bt.truncate(b.limit);
            for (bv, _) in &bt { let pts: Vec<DataPoint> = data.iter().filter(|((_, b), _)| *b == *bv).map(|((iv, _), c)| DataPoint { interval: iv.clone(), count: *c }).collect(); results.push(SeriesResult { label: label.clone(), data: pts, breakdown_value: bv.clone() }); }
        } else {
            let pts: Vec<DataPoint> = data.iter().filter(|((_, b), _)| b.is_none()).map(|((iv, _), c)| DataPoint { interval: iv.clone(), count: *c }).collect(); results.push(SeriesResult { label, data: pts, breakdown_value: None });
        }
    }
    QueryResponse { results, meta: QueryMeta { kind: "trends".into(), elapsed_ms: start.elapsed().as_millis() as u64, cached: false } }
}

fn pg_matches(event: &CapturedEvent, group: &PropertyGroup) -> bool { if group.values.is_empty() { return true; } let r: Vec<bool> = group.values.iter().map(|i| match i { GroupOrFilter::Filter(f) => f_matches(event, f), GroupOrFilter::Group(g) => pg_matches(event, g) }).collect(); match group.op { GroupOp::And => r.iter().all(|&v| v), GroupOp::Or => r.iter().any(|&v| v) } }
fn f_matches(event: &CapturedEvent, filter: &Filter) -> bool { let rv = match filter.source { FilterSource::Event => gpv(event, &filter.key, &FilterSource::Event), _ => return true }; match &filter.operator { FilterOperator::Exact => rv.as_deref() == Some(filter.value.as_str().unwrap_or("")), FilterOperator::IExact => rv.as_deref().map(|v| v.to_lowercase()) == Some(filter.value.as_str().unwrap_or("").to_lowercase()), FilterOperator::NotEqual => rv.as_deref() != Some(filter.value.as_str().unwrap_or("")), FilterOperator::Contains => rv.as_deref().unwrap_or("").contains(filter.value.as_str().unwrap_or("")), FilterOperator::NotContains => !rv.as_deref().unwrap_or("").contains(filter.value.as_str().unwrap_or("")), FilterOperator::IContains => rv.as_deref().unwrap_or("").to_lowercase().contains(&filter.value.as_str().unwrap_or("").to_lowercase()), FilterOperator::IsSet => rv.is_some(), FilterOperator::IsNotSet => rv.is_none(), _ => true } }
fn dr_matches(event: &CapturedEvent, range: &DateRange) -> bool { if let Some(ref f) = range.from { if let Ok(dt) = DateTime::parse_from_rfc3339(f) { if event.timestamp < dt.with_timezone(&Utc) { return false; } } } if let Some(ref t) = range.to { if let Ok(dt) = DateTime::parse_from_rfc3339(t) { if event.timestamp > dt.with_timezone(&Utc) { return false; } } } if let Some(ref ln) = range.last_n { let now = Utc::now(); let cut = match ln { LastN::Hours(h) => now - Duration::hours(*h as i64), LastN::Days(d) => now - Duration::days(*d as i64), LastN::Weeks(w) => now - Duration::weeks(*w as i64), LastN::Months(m) => now - Duration::days(*m as i64 * 30) }; if event.timestamp < cut { return false; } } true }
fn em_matches(event: &CapturedEvent, em: &EventMatch) -> bool { match em { EventMatch::Name(n) => event.event == *n, EventMatch::Any => true } }
fn gpv(event: &CapturedEvent, key: &str, source: &FilterSource) -> Option<String> { match source { FilterSource::Event => event.properties.get(key).and_then(|v| match v { serde_json::Value::String(s) => Some(s.clone()), serde_json::Value::Number(n) => Some(n.to_string()), serde_json::Value::Bool(b) => Some(b.to_string()), serde_json::Value::Null => None, _ => Some(v.to_string()) }), _ => None } }
fn cm_math(events: &[&&CapturedEvent], math: &Math) -> i64 { match math { Math::Total => events.len() as i64, Math::Dau | Math::Wau | Math::Mau | Math::UniqueSessions => { let p: std::collections::HashSet<&str> = events.iter().map(|e| e.distinct_id.as_str()).collect(); p.len() as i64 }, Math::FirstTime => events.len() as i64, _ => 0 } }
