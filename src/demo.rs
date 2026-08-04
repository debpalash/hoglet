//! Demo seeder — populates the database with realistic sample data when
//! HOGLET_DEMO=1. Idempotent: skips if the event store already has data.

use std::sync::Arc;

use chrono::{Duration, Utc};
use rand::Rng;
use serde_json::Map;
use uuid::Uuid;

use crate::capture::event::CapturedEvent;
use crate::dashboard_store::DashboardStore;
use crate::flags::{FlagStore, Variant};
use crate::store::EventStore;

pub fn seed_if_demo(
    store: &EventStore,
    flags: &FlagStore,
    dash: Option<&DashboardStore>,
    catalog: Option<&Arc<crate::catalog::CatalogStore>>,
    session: Option<&Arc<crate::session::SessionStore>>,
) {
    let demo = std::env::var("HOGLET_DEMO").unwrap_or_default();
    if demo != "1" { return; }

    // Skip if data already exists
    if let Ok(files) = store.list_files() {
        if !files.is_empty() {
            tracing::info!("demo mode: data already exists, skipping seed");
            return;
        }
    }

    tracing::info!("demo mode: seeding sample data...");

    let token = "phc_demo";
    let now = Utc::now();
    let events_per_day = 55;
    let days = 90;
    let event_types = ["$pageview", "$click", "$identify"];
    let browsers = ["Chrome", "Firefox", "Safari", "Edge"];
    let countries = ["US", "DE", "GB", "FR", "JP"];
    let persons: Vec<String> = (0..100).map(|i| format!("demo_user_{i:03}")).collect();

    let mut rng = rand::thread_rng();
    let mut all_events: Vec<CapturedEvent> = Vec::new();

    for day in 0..days {
        // More events per recent day for a livelier dashboard
        let per_day = if day < 2 { events_per_day * 3 } else { events_per_day };
        for _ in 0..per_day {
            let person = &persons[rng.gen_range(0..persons.len())];
            let event_type = event_types[rng.gen_range(0..event_types.len())];
            let hours_ago = (days - day) * 24 + rng.gen_range(0..24);
            let minutes_ago = rng.gen_range(0..60);
            let ts = now - Duration::hours(hours_ago as i64) - Duration::minutes(minutes_ago as i64);

            let mut props = Map::new();
            props.insert("$browser".into(), browsers[rng.gen_range(0..browsers.len())].into());
            props.insert("$geoip_country_code".into(), countries[rng.gen_range(0..countries.len())].into());
            if event_type == "$pageview" {
                props.insert("$current_url".into(), format!("https://demo.hoglet.dev/page/{}", rng.gen_range(1..20)).into());
            }
            if event_type == "$click" {
                props.insert("$click_target".into(), format!("btn_{}", rng.gen_range(1..10)).into());
            }

            all_events.push(CapturedEvent {
                uuid: Uuid::new_v4(),
                event: event_type.into(),
                distinct_id: person.clone(),
                token: token.into(),
                timestamp: ts,
                properties: props,
            });
        }
    }

    // Write in chunks of 1000 to avoid giant Parquet files
    for chunk in all_events.chunks(1000) {
        let _ = store.write_events(chunk);
    }
    let _ = store.compact();

    // Update catalog from written events
    if let Some(cat) = catalog {
        let _ = cat.ingest(&all_events, token);
    }
    if let Some(sess) = session {
        let _ = sess.ingest(&all_events, token);
    }

    // Create feature flags
    let _ = flags.upsert_full(
        token, "new-dashboard", true, 50.0,
        &[Variant { key: "control".into(), rollout: 50.0 }, Variant { key: "treatment".into(), rollout: 50.0 }],
        None,
    );
    let _ = flags.upsert(token, "dark-mode", true, 100.0);
    let _ = flags.upsert(token, "beta-search", false, 0.0);

    // Create saved insights + dashboard
    if let Some(dash) = dash {
        let trends_ir = serde_json::json!({
            "kind": "Trends",
            "series": [{"event": {"type": "name", "value": "$pageview"}, "math": {"type": "total"}}],
            "filters": {"op": "AND", "values": []},
            "range": {"from": null, "to": null, "last_n": null},
            "interval": "Day",
            "formulas": []
        });
        let i1 = dash.save_insight(token, &crate::dashboard_store::SavedInsight {
            id: String::new(), token: token.into(), name: "Pageviews".into(),
            description: "Daily pageview count".into(),
            query_ir: trends_ir, created_by: "demo".into(), created_at: 0, updated_at: 0,
        }).ok();

        let funnel_ir = serde_json::json!({
            "kind": "Funnels",
            "series": [
                {"event": {"type": "name", "value": "$pageview"}, "math": {"type": "total"}},
                {"event": {"type": "name", "value": "$click"}, "math": {"type": "total"}},
                {"event": {"type": "name", "value": "$identify"}, "math": {"type": "total"}}
            ],
            "filters": {"op": "AND", "values": []},
            "range": {"from": null, "to": null, "last_n": null},
            "interval": "Day",
            "funnel_config": {"order_type": "Ordered", "conversion_window_seconds": null, "exclusions": [], "attribution": "AllSteps"},
            "formulas": []
        });
        let i2 = dash.save_insight(token, &crate::dashboard_store::SavedInsight {
            id: String::new(), token: token.into(), name: "Conversion funnel".into(),
            description: "Pageview → Click → Identify".into(),
            query_ir: funnel_ir, created_by: "demo".into(), created_at: 0, updated_at: 0,
        }).ok();

        if let (Some(i1), Some(i2)) = (i1, i2) {
            let _ = dash.save_dashboard(token, &crate::dashboard_store::Dashboard {
                id: String::new(), token: token.into(), name: "Demo dashboard".into(),
                tiles: vec![
                    crate::dashboard_store::DashboardTile { insight_id: i1.id, x: 0, y: 0, w: 6, h: 3, insight: None },
                    crate::dashboard_store::DashboardTile { insight_id: i2.id, x: 0, y: 3, w: 6, h: 3, insight: None },
                ],
                created_by: "demo".into(), created_at: 0,
            }).ok();
        }
    }

    tracing::info!(
        "demo mode: seeded {} events, {} persons, 3 flags, 2 insights, 1 dashboard",
        all_events.len(), persons.len()
    );
}
