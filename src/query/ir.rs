//! Query IR — the typed, serializable description of an analytics question.
//!
//! Every insight kind maps to one `Query`. The frontend builds `Query` JSON;
//! the backend compiles it to DuckDB SQL. This struct is the saved-insight
//! format on disk and the `ts-rs` seam to the dashboard.
//!
//! Design: IR is additive. New query kinds, math variants, and filter operators
//! are added as new enum variants — never remove or renumber.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

// ── Query root ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Query {
    pub kind: QueryKind,
    pub series: Vec<Series>,
    #[serde(default)]
    pub filters: PropertyGroup,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<Breakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdown2: Option<Breakdown>,
    pub range: DateRange,
    #[serde(default = "default_interval")]
    pub interval: Interval,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formulas: Vec<Formula>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funnel_config: Option<FunnelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_config: Option<RetentionConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_config: Option<LifecycleConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stickiness_config: Option<StickinessConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actors_config: Option<ActorsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_config: Option<SqlConfig>,
}

fn default_interval() -> Interval {
    Interval::Day
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
/// Aliases are deserialize-only, so the wire output and the generated TS types
/// are unchanged. They exist because `meta.kind` comes back lowercase
/// ("trends"), and an API that will not accept the spelling it just emitted is
/// a trap for anyone hand-writing a query.
pub enum QueryKind {
    #[serde(alias = "trends")]
    Trends,
    #[serde(alias = "funnels")]
    Funnels,
    #[serde(alias = "retention")]
    Retention,
    #[serde(alias = "lifecycle")]
    Lifecycle,
    #[serde(alias = "stickiness")]
    Stickiness,
    #[serde(alias = "actors")]
    Actors,
    #[serde(alias = "sql")]
    Sql,
}

// ── Series ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Series {
    pub event: EventMatch,
    #[serde(default)]
    pub math: Math,
}

impl Series {
    pub fn event_name(&self) -> String {
        match &self.event {
            EventMatch::Name(n) => n.clone(),
            EventMatch::Any => "any event".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
#[serde(tag = "type", content = "value")]
pub enum EventMatch {
    #[serde(rename = "name")]
    Name(String),
    #[serde(rename = "any")]
    Any,
}

impl Default for EventMatch {
    fn default() -> Self {
        EventMatch::Any
    }
}

// ── Math ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
#[serde(tag = "type", content = "value")]
pub enum Math {
    #[serde(rename = "total")]
    Total,
    /// Distinct persons within each requested interval.
    #[serde(rename = "unique_persons")]
    UniquePersons,
    #[serde(rename = "dau")]
    Dau,
    #[serde(rename = "wau")]
    Wau,
    #[serde(rename = "mau")]
    Mau,
    #[serde(rename = "unique_sessions")]
    UniqueSessions,
    #[serde(rename = "first_time")]
    FirstTime,
    #[serde(rename = "property_sum")]
    PropertySum(String),
    #[serde(rename = "property_avg")]
    PropertyAvg(String),
    #[serde(rename = "property_min")]
    PropertyMin(String),
    #[serde(rename = "property_max")]
    PropertyMax(String),
    #[serde(rename = "property_median")]
    PropertyMedian(String),
    #[serde(rename = "property_p75")]
    PropertyP75(String),
    #[serde(rename = "property_p90")]
    PropertyP90(String),
    #[serde(rename = "property_p95")]
    PropertyP95(String),
    #[serde(rename = "property_p99")]
    PropertyP99(String),
    #[serde(rename = "count_per_actor")]
    CountPerActor(ActorAgg),
}

impl Default for Math {
    fn default() -> Self {
        Math::Total
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum ActorAgg {
    Avg,
    Min,
    Max,
    Median,
    P90,
}

// ── PropertyGroup (filters) ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct PropertyGroup {
    #[serde(rename = "op")]
    pub op: GroupOp,
    pub values: Vec<GroupOrFilter>,
}

impl Default for PropertyGroup {
    fn default() -> Self {
        PropertyGroup {
            op: GroupOp::And,
            values: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum GroupOp {
    #[serde(rename = "AND", alias = "and")]
    And,
    #[serde(rename = "OR", alias = "or")]
    Or,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
#[serde(tag = "type")]
pub enum GroupOrFilter {
    #[serde(rename = "group")]
    Group(PropertyGroup),
    #[serde(rename = "filter")]
    Filter(Filter),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Filter {
    pub source: FilterSource,
    pub key: String,
    pub operator: FilterOperator,
    #[ts(type = "any")]
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum FilterSource {
    #[serde(rename = "event")]
    Event,
    #[serde(rename = "person")]
    Person,
    #[serde(rename = "cohort")]
    Cohort,
}

// ── Filter operators ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
#[serde(tag = "op", content = "value")]
pub enum FilterOperator {
    #[serde(rename = "exact")]
    Exact,
    #[serde(rename = "iexact")]
    IExact,
    #[serde(rename = "not_equal")]
    NotEqual,
    #[serde(rename = "contains")]
    Contains,
    #[serde(rename = "not_contains")]
    NotContains,
    #[serde(rename = "icontains")]
    IContains,
    #[serde(rename = "is_set")]
    IsSet,
    #[serde(rename = "is_not_set")]
    IsNotSet,
    #[serde(rename = "eq")]
    Equal,
    #[serde(rename = "gt")]
    Gt,
    #[serde(rename = "lt")]
    Lt,
    #[serde(rename = "gte")]
    Gte,
    #[serde(rename = "lte")]
    Lte,
    #[serde(rename = "between")]
    Between,
    #[serde(rename = "in")]
    In,
    #[serde(rename = "not_in")]
    NotIn,
}

// ── Breakdown ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Breakdown {
    pub source: FilterSource,
    pub key: String,
    #[serde(default = "default_breakdown_limit")]
    pub limit: usize,
}

fn default_breakdown_limit() -> usize {
    10
}

// ── DateRange & Interval ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct DateRange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_n: Option<LastN>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
#[serde(tag = "unit", content = "value")]
pub enum LastN {
    #[serde(rename = "h")]
    Hours(u32),
    #[serde(rename = "d")]
    Days(u32),
    #[serde(rename = "w")]
    Weeks(u32),
    #[serde(rename = "m")]
    Months(u32),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum Interval {
    #[serde(alias = "hour")]
    Hour,
    #[serde(alias = "day")]
    Day,
    #[serde(alias = "week")]
    Week,
    #[serde(alias = "month")]
    Month,
}

// ── Formulas ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct Formula {
    pub label: String,
    pub expression: String,
}

// ── Funnel config ──────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum FunnelOrder {
    Ordered,
    Unordered,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum FunnelAttribution {
    FirstTouch,
    LastTouch,
    AllSteps,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct FunnelExclusion {
    pub step: usize,
    pub event: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct FunnelConfig {
    #[serde(default)]
    pub order_type: FunnelOrder,
    pub conversion_window_seconds: Option<u32>,
    #[serde(default)]
    pub exclusions: Vec<FunnelExclusion>,
    #[serde(default)]
    pub attribution: FunnelAttribution,
}

impl Default for FunnelOrder {
    fn default() -> Self {
        FunnelOrder::Ordered
    }
}

impl Default for FunnelAttribution {
    fn default() -> Self {
        FunnelAttribution::AllSteps
    }
}

// ── Retention config ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum RetentionType {
    Recurring,
    FirstTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum RetentionPeriod {
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, Default)]
#[ts(export, export_to = "../web/src/types/")]
pub struct RetentionConfig {
    pub cohort_event: EventMatch,
    pub retention_event: EventMatch,
    #[serde(default)]
    pub retention_type: RetentionType,
    #[serde(default)]
    pub period: RetentionPeriod,
    #[serde(default = "default_total_periods")]
    pub total_periods: u32,
}

impl Default for RetentionType {
    fn default() -> Self {
        RetentionType::Recurring
    }
}
impl Default for RetentionPeriod {
    fn default() -> Self {
        RetentionPeriod::Day
    }
}
fn default_total_periods() -> u32 {
    10
}

// ── Lifecycle config ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum LifecycleStatus {
    New,
    Returning,
    Resurrecting,
    Dormant,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub enum LifecyclePeriod {
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct LifecycleConfig {
    pub event: EventMatch,
    #[serde(default)]
    pub prior_period: LifecyclePeriod,
}

impl Default for LifecyclePeriod {
    fn default() -> Self {
        LifecyclePeriod::Day
    }
}

// ── Stickiness config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct StickinessConfig {
    pub event: EventMatch,
    #[serde(default = "default_stickiness_window")]
    pub window_days: u32,
}

fn default_stickiness_window() -> u32 {
    7
}

// ── Actors config ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct ActorsConfig {
    pub series_index: usize,
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_actors_limit")]
    pub limit: usize,
}

fn default_actors_limit() -> usize {
    100
}

// ── SQL access config ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct SqlConfig {
    pub sql: String,
}

// ── Response types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct QueryResponse {
    pub results: Vec<SeriesResult>,
    pub meta: QueryMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct SeriesResult {
    pub label: String,
    pub data: Vec<DataPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakdown_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct DataPoint {
    pub interval: String,
    #[ts(type = "number")]
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web/src/types/")]
pub struct QueryMeta {
    pub kind: String,
    #[ts(type = "number")]
    pub elapsed_ms: u64,
    #[ts(type = "number")]
    pub generation_id: u64,
    pub cached: bool,
}

// ── IR compiler result ────────────────────────────────────────────

/// Parameter value for a compiled SQL query. DuckDB binds these as the
/// appropriate type at execution.
#[derive(Debug, Clone)]
pub enum ParamValue {
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

/// The output of compiling a Query IR to DuckDB SQL.
#[derive(Debug, Clone)]
pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<ParamValue>,
}

// ── IR validation ─────────────────────────────────────────────────

#[derive(Debug)]
pub enum IrError {
    Invalid(&'static str),
}

impl std::fmt::Display for IrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IrError::Invalid(msg) => write!(f, "invalid query IR: {msg}"),
        }
    }
}

impl std::error::Error for IrError {}

impl Query {
    /// Convenience constructor for a trends query with defaults.
    pub fn trends(series: Vec<Series>, range: DateRange) -> Self {
        Query {
            kind: QueryKind::Trends,
            series,
            filters: PropertyGroup::default(),
            breakdown: None,
            breakdown2: None,
            range,
            interval: Interval::Day,
            formulas: vec![],
            funnel_config: None,
            retention_config: None,
            lifecycle_config: None,
            stickiness_config: None,
            actors_config: None,
            sql_config: None,
        }
    }

    pub fn validate(&self) -> Result<(), IrError> {
        // Series are required for Trends, Funnels, Actors
        let needs_series = matches!(
            self.kind,
            QueryKind::Trends | QueryKind::Funnels | QueryKind::Actors
        );
        if needs_series && self.series.is_empty() {
            return Err(IrError::Invalid("at least one series is required"));
        }
        if self.series.len() > 10 {
            return Err(IrError::Invalid("max 10 series per query"));
        }
        if let Some(ref b) = self.breakdown {
            if b.key.is_empty() {
                return Err(IrError::Invalid("breakdown key must not be empty"));
            }
        }
        for s in &self.series {
            match &s.event {
                EventMatch::Name(n) if n.is_empty() => {
                    return Err(IrError::Invalid("event name must not be empty"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_serde_roundtrip() {
        let q = Query {
            kind: QueryKind::Trends,
            series: vec![Series {
                event: EventMatch::Name("pageview".into()),
                math: Math::Total,
            }],
            filters: PropertyGroup::default(),
            breakdown: None,
            breakdown2: None,
            range: DateRange {
                from: None,
                to: None,
                last_n: None,
            },
            interval: Interval::Day,
            formulas: vec![],
            funnel_config: None,
            retention_config: None,
            lifecycle_config: None,
            stickiness_config: None,
            actors_config: None,
            sql_config: None,
        };

        let json = serde_json::to_string_pretty(&q).unwrap();
        println!("Serialized:\n{json}");

        let parsed: Query = serde_json::from_str(&json).unwrap();
        let rejson = serde_json::to_string_pretty(&parsed).unwrap();
        assert_eq!(json, rejson);
    }

    /// `meta.kind` comes back lowercase, so lowercase has to parse. Both
    /// spellings must land on the same variant, and the canonical spelling must
    /// still be what we emit — the generated TS types describe the output.
    #[test]
    fn lowercase_spellings_are_accepted() {
        assert!(matches!(
            serde_json::from_str::<QueryKind>("\"trends\"").unwrap(),
            QueryKind::Trends
        ));
        assert!(matches!(
            serde_json::from_str::<Interval>("\"day\"").unwrap(),
            Interval::Day
        ));
        assert!(matches!(
            serde_json::from_str::<GroupOp>("\"and\"").unwrap(),
            GroupOp::And
        ));

        // Canonical spellings keep working...
        assert!(matches!(
            serde_json::from_str::<QueryKind>("\"Trends\"").unwrap(),
            QueryKind::Trends
        ));
        assert!(matches!(
            serde_json::from_str::<GroupOp>("\"AND\"").unwrap(),
            GroupOp::And
        ));

        // ...and remain what we serialize, so the TS types stay honest.
        assert_eq!(
            serde_json::to_string(&QueryKind::Trends).unwrap(),
            "\"Trends\""
        );
        assert_eq!(serde_json::to_string(&Interval::Day).unwrap(), "\"Day\"");
        assert_eq!(serde_json::to_string(&GroupOp::And).unwrap(), "\"AND\"");
    }
}
