//! Feature flag definitions and evaluation (spec/wire-compat.md "Flags").
//!
//! Bucketing matches PostHog byte-for-byte: `sha1("{key}.{distinct_id}")`,
//! first 60 bits as a fraction in [0,1), enabled when that fraction is below
//! `rollout_percentage/100`. Matching their hash means a flag at 30% rollout
//! selects the *same* users on Hoglet as on PostHog — the compatibility that
//! makes us a true drop-in (`why-hoglet.md`), not just wire-shaped.
//!
//! Supports the three things real flags need: **rollout %**, **multivariate
//! variants** (return a variant string, bucketed consistently), and
//! **property conditions** matched against the person properties the SDK
//! passes on the flags request (PostHog's local-evaluation model).
//!
//! Definitions live in SQLite, keyed by token. Response *shapes* are owned by
//! `routes/flags.rs`; this module owns *which flags are on for whom, and which
//! variant*.

use std::sync::Mutex;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha1::{Digest, Sha1};

/// 60-bit scale: the max value of the first 15 hex chars of the SHA1 digest.
const LONG_SCALE: f64 = 0xfff_ffff_ffff_ffff_u64 as f64;

/// One variant of a multivariate flag. `rollout` values across a flag's
/// variants are cumulative buckets summing to ~100.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Variant {
    pub key: String,
    pub rollout: f64,
}

/// A property filter matched against the request's person properties.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyFilter {
    pub key: String,
    #[serde(default = "op_exact")]
    pub operator: String,
    pub value: Value,
}
fn op_exact() -> String {
    "exact".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Conditions {
    #[serde(default)]
    pub properties: Vec<PropertyFilter>,
    #[serde(default)]
    pub cohort_ids: Vec<String>,
    #[serde(default)]
    pub payload: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluatedFlag {
    pub key: String,
    pub enabled: bool,
    pub variant: Option<String>,
    pub payload: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct FlagDef {
    pub key: String,
    pub active: bool,
    pub rollout_percentage: f64,
    pub variants: Vec<Variant>,
    pub payload: Option<String>,
}

pub struct FlagStore {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS feature_flags (
    token TEXT NOT NULL,
    key TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    rollout_percentage REAL NOT NULL DEFAULT 100.0,
    variants TEXT,
    conditions TEXT,
    PRIMARY KEY (token, key)
);
";

/// One row's stored definition.
struct StoredFlag {
    key: String,
    rollout: f64,
    variants: Vec<Variant>,
    conditions: Option<Conditions>,
}

impl FlagStore {
    pub fn open(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::open(Connection::open_in_memory()?)
    }

    /// Simple boolean flag (no variants, no conditions).
    pub fn upsert(&self, token: &str, key: &str, active: bool, rollout: f64) -> rusqlite::Result<()> {
        self.upsert_full(token, key, active, rollout, &[], None)
    }

    /// Full definition with optional variants and conditions.
    pub fn upsert_full(
        &self,
        token: &str,
        key: &str,
        active: bool,
        rollout: f64,
        variants: &[Variant],
        conditions: Option<&Conditions>,
    ) -> rusqlite::Result<()> {
        let variants_json = if variants.is_empty() {
            None
        } else {
            Some(serde_json::to_string(variants).unwrap())
        };
        let conditions_json = conditions.map(|c| serde_json::to_string(c).unwrap());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO feature_flags (token, key, active, rollout_percentage, variants, conditions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(token, key) DO UPDATE SET
                active=?3, rollout_percentage=?4, variants=?5, conditions=?6",
            params![token, key, active as i64, rollout, variants_json, conditions_json],
        )?;
        Ok(())
    }

    fn load(&self, token: &str, active_only: bool) -> Vec<StoredFlag> {
        let conn = self.conn.lock().unwrap();
        let sql = if active_only {
            "SELECT key, rollout_percentage, variants, conditions FROM feature_flags WHERE token=?1 AND active=1"
        } else {
            "SELECT key, rollout_percentage, variants, conditions FROM feature_flags WHERE token=?1 ORDER BY key"
        };
        let Ok(mut stmt) = conn.prepare(sql) else {
            return vec![];
        };
        let rows = stmt.query_map(params![token], |r| {
            let variants: Option<String> = r.get(2)?;
            let conditions: Option<String> = r.get(3)?;
            Ok(StoredFlag {
                key: r.get(0)?,
                rollout: r.get(1)?,
                variants: variants
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
                conditions: conditions.and_then(|s| serde_json::from_str(&s).ok()),
            })
        });
        rows.map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// List all flag definitions for a token (dashboard view).
    pub fn list(&self, token: &str) -> Vec<FlagDef> {
        self.load(token, false)
            .into_iter()
            .map(|f| FlagDef {
                key: f.key,
                active: true,
                rollout_percentage: f.rollout,
                variants: f.variants,
                payload: f.conditions.and_then(|c| c.payload),
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|mut d| {
                d.active = self.is_active(token, &d.key);
                d
            })
            .collect()
    }

    fn is_active(&self, token: &str, key: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT active FROM feature_flags WHERE token=?1 AND key=?2",
            params![token, key],
            |r| r.get::<_, i64>(0),
        )
        .map(|a| a != 0)
        .unwrap_or(false)
    }

    /// Evaluate every active flag for this token against `distinct_id` and the
    /// person properties the SDK passed on the request.
    pub fn evaluate(
        &self,
        token: &str,
        distinct_id: &str,
        person_properties: &Map<String, Value>,
        cohort_check: &dyn Fn(&str, &str) -> bool,
        first_seen_key: Option<&str>,
    ) -> Vec<EvaluatedFlag> {
        let bucket_key = first_seen_key.unwrap_or(distinct_id);
        self.load(token, true)
            .into_iter()
            .map(|f| evaluate_one(&f, bucket_key, person_properties, cohort_check))
            .collect()
    }
}

fn evaluate_one(
    f: &StoredFlag,
    bucket_key: &str,
    person_properties: &Map<String, Value>,
    cohort_check: &dyn Fn(&str, &str) -> bool,
) -> EvaluatedFlag {
    let payload = f.conditions.as_ref().and_then(|c| c.payload.clone());
    if let Some(cond) = &f.conditions {
        if !cond.properties.iter().all(|p| match_filter(person_properties.get(&p.key), &p.operator, &p.value)) {
            return EvaluatedFlag { key: f.key.clone(), enabled: false, variant: None, payload };
        }
        if !cond.cohort_ids.iter().all(|cid| cohort_check(bucket_key, cid)) {
            return EvaluatedFlag { key: f.key.clone(), enabled: false, variant: None, payload };
        }
    }

    if !in_rollout(&f.key, bucket_key, f.rollout) {
        return EvaluatedFlag { key: f.key.clone(), enabled: false, variant: None, payload };
    }

    let variant = if f.variants.is_empty() { None } else { Some(pick_variant(&f.key, bucket_key, &f.variants)) };
    EvaluatedFlag { key: f.key.clone(), enabled: true, variant, payload }
}

/// PostHog's consistent-hash bucketing. A user's fraction is stable across
/// calls, so raising the rollout only ever adds users, never reshuffles them.
fn hash_fraction(key: &str, distinct_id: &str, salt: &str) -> f64 {
    let mut hasher = Sha1::new();
    hasher.update(format!("{key}.{distinct_id}{salt}").as_bytes());
    let digest = hasher.finalize();
    let hex = hex::encode(digest);
    let val = u64::from_str_radix(&hex[..15], 16).unwrap_or(0);
    val as f64 / LONG_SCALE
}

fn in_rollout(key: &str, distinct_id: &str, rollout_percentage: f64) -> bool {
    if rollout_percentage >= 100.0 {
        return true;
    }
    if rollout_percentage <= 0.0 {
        return false;
    }
    hash_fraction(key, distinct_id, "") <= rollout_percentage / 100.0
}

/// Assign a variant by consistent hash into cumulative rollout ranges
/// (PostHog uses a distinct "variant" salt so variant choice is independent of
/// the enabled roll).
fn pick_variant(key: &str, distinct_id: &str, variants: &[Variant]) -> String {
    let frac = hash_fraction(key, distinct_id, "variant") * 100.0;
    let mut cumulative = 0.0;
    for v in variants {
        cumulative += v.rollout;
        if frac < cumulative {
            return v.key.clone();
        }
    }
    // Rounding slack: fall back to the last variant.
    variants.last().map(|v| v.key.clone()).unwrap_or_default()
}

fn match_filter(prop: Option<&Value>, operator: &str, expected: &Value) -> bool {
    let Some(prop) = prop else {
        // Missing property matches only "is_not" against a present value.
        return operator == "is_not";
    };
    match operator {
        "exact" => prop == expected,
        "is_not" => prop != expected,
        "icontains" => {
            let (Some(a), Some(b)) = (prop.as_str(), expected.as_str()) else {
                return false;
            };
            a.to_lowercase().contains(&b.to_lowercase())
        }
        "gt" => num(prop).zip(num(expected)).is_some_and(|(a, b)| a > b),
        "lt" => num(prop).zip(num(expected)).is_some_and(|(a, b)| a < b),
        _ => false,
    }
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }
    fn eval(s: &FlagStore, did: &str) -> Vec<EvaluatedFlag> {
        s.evaluate("phc_t", did, &Map::new(), &|_, _| true, None)
    }

    #[test]
    fn full_rollout_always_on() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert("phc_t", "new-ui", true, 100.0).unwrap();
        assert_eq!(
            eval(&s, "anyone"),
            vec![EvaluatedFlag { key: "new-ui".into(), enabled: true, variant: None, payload: None }]
        );
    }

    #[test]
    fn zero_rollout_always_off() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert("phc_t", "off", true, 0.0).unwrap();
        assert!(!eval(&s, "anyone")[0].enabled);
    }

    #[test]
    fn inactive_flags_excluded() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert("phc_t", "dead", false, 100.0).unwrap();
        assert!(eval(&s, "u").is_empty());
    }

    #[test]
    fn bucketing_is_consistent_and_monotonic() {
        for i in 0..500 {
            let u = format!("u{i}");
            if in_rollout("f", &u, 30.0) {
                assert!(in_rollout("f", &u, 60.0), "user {u} dropped when rollout rose");
            }
        }
    }

    #[test]
    fn rollout_fraction_roughly_accurate() {
        let n = 2000;
        let on = (0..n).filter(|i| in_rollout("f", &format!("u{i}"), 50.0)).count();
        assert!((0.42..0.58).contains(&(on as f64 / n as f64)));
    }

    #[test]
    fn multivariate_returns_a_variant() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert_full(
            "phc_t",
            "exp",
            true,
            100.0,
            &[
                Variant { key: "control".into(), rollout: 50.0 },
                Variant { key: "test".into(), rollout: 50.0 },
            ],
            None,
        )
        .unwrap();
        let f = &eval(&s, "user-1")[0];
        assert!(f.enabled);
        assert!(matches!(f.variant.as_deref(), Some("control") | Some("test")));
    }

    #[test]
    fn variant_split_is_roughly_even() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert_full(
            "phc_t",
            "exp",
            true,
            100.0,
            &[
                Variant { key: "a".into(), rollout: 50.0 },
                Variant { key: "b".into(), rollout: 50.0 },
            ],
            None,
        )
        .unwrap();
        let mut a = 0;
        for i in 0..2000 {
            if eval(&s, &format!("u{i}"))[0].variant.as_deref() == Some("a") {
                a += 1;
            }
        }
        assert!((0.42..0.58).contains(&(a as f64 / 2000.0)), "split {a}/2000");
    }

    #[test]
    fn condition_gates_on_person_property() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert_full(
            "phc_t",
            "pro-only",
            true,
            100.0,
            &[],
            Some(&Conditions { cohort_ids: vec![], payload: None,
                properties: vec![PropertyFilter {
                    key: "plan".into(),
                    operator: "exact".into(),
                    value: json!("pro"),
                }],
            }),
        )
        .unwrap();
        // pro user: on. free user: off.
        let pro = s.evaluate("phc_t", "u", &props(json!({"plan": "pro"})), &|_, _| true, None);
        assert!(pro[0].enabled);
        let free = s.evaluate("phc_t", "u", &props(json!({"plan": "free"})), &|_, _| true, None);
        assert!(!free[0].enabled);
        // missing property: off.
        let none = s.evaluate("phc_t", "u", &Map::new(), &|_, _| true, None);
        assert!(!none[0].enabled);
    }

    #[test]
    fn numeric_gt_condition() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert_full(
            "phc_t",
            "whales",
            true,
            100.0,
            &[],
            Some(&Conditions { cohort_ids: vec![], payload: None,
                properties: vec![PropertyFilter {
                    key: "spend".into(),
                    operator: "gt".into(),
                    value: json!(100),
                }],
            }),
        )
        .unwrap();
        assert!(s.evaluate("phc_t", "u", &props(json!({"spend": 500})), &|_, _| true, None)[0].enabled);
        assert!(!s.evaluate("phc_t", "u", &props(json!({"spend": 50})), &|_, _| true, None)[0].enabled);
    }

    #[test]
    fn list_reports_variants_and_active() {
        let s = FlagStore::in_memory().unwrap();
        s.upsert("phc_t", "b", false, 25.0).unwrap();
        s.upsert_full("phc_t", "a", true, 100.0, &[Variant { key: "x".into(), rollout: 100.0 }], None).unwrap();
        let list = s.list("phc_t");
        let a = list.iter().find(|f| f.key == "a").unwrap();
        assert!(a.active && a.variants.len() == 1);
        let b = list.iter().find(|f| f.key == "b").unwrap();
        assert!(!b.active && b.rollout_percentage == 25.0);
    }
}
