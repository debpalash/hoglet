//! Event enrichment — user-agent parsing and cookieless device
//! identification.
//!
//! Enrichment runs at capture time, before the event hits the WAL. It adds
//! properties to events in-place — the enriched event is what gets stored.
//!
//! Rules:
//! - Never overwrite a property the client already set.
//! - UA parsing is on by default (disabled with `HOGLET_NO_UA_PARSE=1`).
//! - Cookieless mode requires `HOGLET_COOKIELESS_SALT`.
//! - GeoIP is deferred (P3+) — requires a pure-Rust MaxMind DB reader or the
//!   C library, neither of which ships in the single static binary today.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::Utc;

use crate::capture::event::CapturedEvent;

// ── Configuration ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct EnrichmentConfig {
    pub user_agent_parse: bool,
    pub cookieless_salt: Option<String>,
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        EnrichmentConfig {
            user_agent_parse: true,
            cookieless_salt: None,
        }
    }
}

impl EnrichmentConfig {
    pub fn from_env() -> Self {
        EnrichmentConfig {
            user_agent_parse: std::env::var("HOGLET_NO_UA_PARSE").is_err(),
            cookieless_salt: std::env::var("HOGLET_COOKIELESS_SALT").ok(),
        }
    }
}

// ── Enricher ──────────────────────────────────────────────────────

pub struct Enricher {
    config: EnrichmentConfig,
}

impl Enricher {
    pub fn new(config: EnrichmentConfig) -> Self {
        Enricher { config }
    }

    /// Enrich a single event in-place. `ip` is the connection IP; `user_agent`
    /// is the User-Agent header. Both optional.
    pub fn enrich(
        &self,
        event: &mut CapturedEvent,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) {
        self.enrich_user_agent(event, user_agent);
        self.enrich_device_id(event, ip);
    }

    fn enrich_user_agent(&self, event: &mut CapturedEvent, user_agent: Option<&str>) {
        if !self.config.user_agent_parse {
            return;
        }
        let ua_prop = event
            .properties
            .get("$user_agent")
            .and_then(|v| v.as_str())
            .or_else(|| {
                event
                    .properties
                    .get("$useragent")
                    .and_then(|v| v.as_str())
            });

        let header_ua = user_agent;

        let ua_str = header_ua.or(ua_prop);

        let ua_str = match ua_str {
            Some(s) if !s.is_empty() => s.to_owned(),
            _ => return,
        };

        let parser = woothee::parser::Parser::new();
        let result = match parser.parse(&ua_str) {
            Some(r) => r,
            None => return,
        };

        let props = &mut event.properties;
        if !props.contains_key("$browser") {
            props.insert("$browser".into(), result.name.into());
        }
        if !props.contains_key("$browser_version") {
            props.insert("$browser_version".into(), result.version.into());
        }
        if !props.contains_key("$os") {
            props.insert("$os".into(), result.os.into());
        }
        if !props.contains_key("$os_version") {
            props.insert("$os_version".into(), result.os_version.into());
        }
        if !props.contains_key("$device_type") {
            props.insert("$device_type".into(), result.category.into());
        }
    }

    fn enrich_device_id(&self, event: &mut CapturedEvent, ip: Option<&str>) {
        if event.properties.contains_key("$device_id") {
            return;
        }
        let salt = match &self.config.cookieless_salt {
            Some(s) => s,
            None => return,
        };
        if salt.is_empty() {
            return;
        }

        let ip = match ip.and_then(|v| if v.is_empty() { None } else { Some(v) }) {
            Some(ip) => ip,
            None => return,
        };

        // Daily salt: salt + YYYY-MM-DD
        let daily = format!("{salt}:{}", Utc::now().format("%Y-%m-%d"));
        let input = format!("{ip}:{daily}");

        let mut hasher = DefaultHasher::new();
        input.hash(&mut hasher);
        let device_id = format!("{:x}", hasher.finish());

        event
            .properties
            .insert("$device_id".to_string(), device_id.into());
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn event(ip: Option<&str>, ua: Option<&str>) -> CapturedEvent {
        let mut props = serde_json::Map::new();
        if let Some(ip) = ip {
            props.insert("$ip".into(), ip.into());
        }
        if let Some(ua) = ua {
            props.insert("$user_agent".into(), ua.into());
        }
        CapturedEvent {
            uuid: Uuid::new_v4(),
            event: "pageview".into(),
            distinct_id: "u1".into(),
            token: "phc_t".into(),
            timestamp: Utc::now(),
            properties: props,
        }
    }

    #[test]
    fn user_agent_parsing_populates_browser_and_os() {
        let config = EnrichmentConfig {
            user_agent_parse: true,
            cookieless_salt: None,
        };
        let enricher = Enricher::new(config);

        let mut ev = event(
            Some("1.2.3.4"),
            Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"),
        );
        enricher.enrich(&mut ev, Some("1.2.3.4"), Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"));

        let browser = ev.properties.get("$browser").and_then(|v| v.as_str());
        assert!(browser.is_some(), "should have set $browser");
    }

    #[test]
    fn does_not_overwrite_existing_properties() {
        let config = EnrichmentConfig {
            user_agent_parse: true,
            cookieless_salt: None,
        };
        let enricher = Enricher::new(config);

        let mut ev = event(Some("1.2.3.4"), Some("Mozilla/5.0 Chrome/120 Safari/537.36"));
        ev.properties
            .insert("$browser".into(), "MyCustomBrowser".into());
        enricher.enrich(&mut ev, Some("1.2.3.4"), Some("Mozilla/5.0 Chrome/120 Safari/537.36"));

        assert_eq!(
            ev.properties.get("$browser").and_then(|v| v.as_str()),
            Some("MyCustomBrowser"),
            "should not overwrite client-set properties"
        );
    }

    #[test]
    fn user_agent_parse_can_be_disabled() {
        let config = EnrichmentConfig {
            user_agent_parse: false,
            cookieless_salt: None,
        };
        let enricher = Enricher::new(config);

        let mut ev = event(Some("1.2.3.4"), Some("Chrome/120"));
        enricher.enrich(&mut ev, Some("1.2.3.4"), Some("Chrome/120"));

        assert!(
            ev.properties.get("$browser").is_none(),
            "should not set browser when UA parsing disabled"
        );
    }

    #[test]
    fn cookieless_device_id_generated() {
        let config = EnrichmentConfig {
            user_agent_parse: false,
            cookieless_salt: Some("test_salt".into()),
        };
        let enricher = Enricher::new(config);

        let mut ev = event(Some("1.2.3.4"), None);
        enricher.enrich(&mut ev, Some("1.2.3.4"), None);

        let device_id = ev.properties.get("$device_id").and_then(|v| v.as_str());
        assert!(device_id.is_some(), "should have generated a $device_id");
        assert!(!device_id.unwrap().is_empty());
    }

    #[test]
    fn cookieless_disabled_without_salt() {
        let config = EnrichmentConfig {
            user_agent_parse: false,
            cookieless_salt: None,
        };
        let enricher = Enricher::new(config);

        let mut ev = event(Some("1.2.3.4"), None);
        enricher.enrich(&mut ev, Some("1.2.3.4"), None);

        assert!(
            ev.properties.get("$device_id").is_none(),
            "should not generate $device_id without salt"
        );
    }
}
