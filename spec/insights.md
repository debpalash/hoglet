# Insight Suite — Implementation Spec

P3. Funnels, retention, lifecycle, stickiness, formulas, SQL access, actor
drill-down. Each insight = an IR kind + compiler template + oracle + (later)
UI panel. The trend is already the phase that puts the IR to real use.

Status: ◐ implementing
Oracle: independent Rust implementation of every insight kind, cross-checked
         against DuckDB output; funnel oracle already exists (P0 hand-written
         test), extended to IR shape.
Exit: six insight types oracle-proven; dashboard drives any of them.

---

## 1. IR extensions

### New query kinds

```rust
enum QueryKind {
    Trends,        // P1
    Funnels,       // P3: ordered/unordered/strict
    Retention,     // P3: recurring + first-time
    Lifecycle,     // P3: new/returning/resurrecting/dormant
    Stickiness,    // P3: repeat usage frequency
    Actors,        // P3: person list behind a result cell
    Sql,           // P3: raw SQL access
}
```

### Funnels IR

```rust
struct Query {
    kind: QueryKind,  // Funnels
    series: Vec<Series>,  // Funnel steps (ordered list)
    filters: PropertyGroup,
    breakdown: Option<Breakdown>,
    range: DateRange,
    interval: Interval,

    // Funnel-specific:
    funnel_config: Option<FunnelConfig>,
}

struct FunnelConfig {
    /// Ordered: steps must occur in order. Unordered: any order within window.
    /// Strict: no other events between steps (not in P3).
    order_type: FunnelOrder,
    /// Conversion must happen within this many seconds of step 1.
    conversion_window_seconds: Option<u32>,
    /// Exclude persons who completed step N before starting the funnel.
    exclusions: Vec<FunnelExclusion>,
    /// Which step to attribute conversion to (first-touch / last-touch).
    attribution: FunnelAttribution,
}

enum FunnelOrder { Ordered, Unordered, Strict }
enum FunnelAttribution { FirstTouch, LastTouch, AllSteps }
struct FunnelExclusion { step: usize, event: String }
```

### Compilation (funnels)

Ordered funnels use the same chained-CTE pattern as the current hand-written
SQL (P0). Each step joins on the previous step's distinct_id + timestamp.

Unordered funnels: all steps within the conversion window, regardless of order.
Compile to: each step's events in the window, then COUNT(DISTINCT distinct_id)
WHERE they completed ALL steps (not necessarily in order).

Strict funnels: ordered + no intervening events. Handled in a later pass; stub
for P3.

Conversion window: add `e.timestamp >= step1_time AND e.timestamp <= step1_time + window`
to each step's CTE.

Exclusions: LEFT JOIN to the excluded event, filter out persons who match.

Attribution: post-SQL in Rust. The SQL returns per-step counts; attribution
re-distributes the counts (e.g. first-touch always credits step 1).

### Breakdown with funnels

Each graph series = one funnel step, broken down by property value.
Compiled as: each step CTE includes the breakdown column; step counts are
GROUP BY step, breakdown_value. This mirrors PostHog's funnel breakdown chart.

### Retention IR

```rust
enum QueryKind {
    ...
    Retention,
}

struct Query {
    ...
    retention_config: Option<RetentionConfig>,
}

struct RetentionConfig {
    /// The starting event (cohort entry).
    cohort_event: EventMatch,
    /// The repeating event.
    retention_event: EventMatch,
    /// Recurring: user did event in period N. First-time: user did event for
    /// the first time ever in period N.
    retention_type: RetentionType,
    /// How to bucket periods.
    period: RetentionPeriod,
    /// Number of periods to show.
    total_periods: u32,
}

enum RetentionType { Recurring, FirstTime }
enum RetentionPeriod { Day, Week, Month }
```

### Compilation (retention)

For each cohort (period 0 group), count how many returned in each subsequent
period. The canonical SQL pattern:

```sql
WITH cohort AS (
    SELECT distinct_id, date_trunc('day', min(timestamp)) cohort_day
    FROM e WHERE event = $cohort_event GROUP BY distinct_id
), retained AS (
    SELECT cohort.distinct_id, cohort.cohort_day,
           date_trunc('day', e.timestamp) activity_day,
           (date_trunc('day', e.timestamp) - date_trunc('day', cohort.cohort_day)) period
    FROM cohort
    JOIN e ON cohort.distinct_id = e.distinct_id AND e.event = $retention_event
)
SELECT cohort_day, period, count(DISTINCT distinct_id) users
FROM retained
GROUP BY cohort_day, period
ORDER BY cohort_day, period
```

The Rust result is a matrix: each row = cohort, each column = period index,
value = number retained. The frontend renders it as a heatmap table.

### Lifecycle IR

```rust
enum QueryKind {
    ...
    Lifecycle,
}

struct Query {
    ...
    lifecycle_config: Option<LifecycleConfig>,
}

struct LifecycleConfig {
    event: EventMatch,
    /// How far back to look for prior activity.
    prior_period: LifecyclePeriod,
}

enum LifecycleStatus { New, Returning, Resurrecting, Dormant }
enum LifecyclePeriod { Day, Week, Month }
```

Classification:
- **New**: first-ever event was in this period.
- **Returning**: event in this period AND in the immediately previous period.
- **Resurrecting**: event in this period AND in some prior period (not
  adjacent).
- **Dormant**: had events, but none in this period (not shown; inferred from absence).

Compiled as: two-pass CTE — one for "any event in period", one for "prior
period activity", then classify with CASE expressions.

### Stickiness IR

```rust
enum QueryKind {
    ...
    Stickiness,
}

struct Query {
    ...
    stickiness_config: Option<StickinessConfig>,
}

struct StickinessConfig {
    event: EventMatch,
    /// Window to count repeat activity in.
    window_days: u32,
}
```

Shows: of users who did the event, how many did it 1x, 2x, 3x, ... Nx in the
window. Compiled as: per-user count in window, then GROUP BY count buckets.

### Actors (drill-down)

POST /api/query with kind=Actors returns a paginated list of distinct_ids
matching a result cell. The IR carries the cell's context (insight kind, series
index, breakdown value, day).

```rust
enum QueryKind {
    ...
    Actors,
}

struct Query {
    ...
    actors_config: Option<ActorsConfig>,
}

struct ActorsConfig {
    /// Which series/step was clicked.
    series_index: usize,
    /// Which interval day was clicked.
    day: String,
    /// Offset for pagination.
    offset: usize,
    limit: usize,
}
```

The compilation is a direct event query:
```sql
SELECT distinct_id, count(*) event_count
FROM e
WHERE event = $event AND strftime(timestamp, '%Y-%m-%d') = $day
  AND <filters>
GROUP BY distinct_id
ORDER BY event_count DESC
LIMIT $limit OFFSET $offset
```

### SQL access

```rust
enum QueryKind {
    ...
    Sql,
}

struct Query {
    ...
    sql_config: Option<SqlConfig>,
}

struct SqlConfig {
    sql: String,
}
```

The user writes raw SQL against a read-only view named `events` and
`sessions` and `persons`. Hoglet prepends:

```sql
WITH events AS (
    SELECT * FROM read_parquet('{$glob}')
    WHERE token = $token
    QUALIFY row_number() OVER (PARTITION BY uuid ORDER BY timestamp) = 1
)
{$user_sql}
```

Capped: memory_limit=256MB, query timeout 30s, no write operations.
The `token` is forced (the user doesn't control it — it comes from the
authenticated request, not the SQL). Only SELECT allowed.

---

## 2. Formulas

Post-SQL arithmetic between series results. A formula references series by
index: `A` = series[0], `B` = series[1], etc.

```rust
struct Query {
    ...
    formulas: Vec<Formula>,
}

struct Formula {
    /// Label for the computed series.
    label: String,
    /// Expression: "A / B", "A - B", "A * 100 / B", etc.
    expression: String,
}
```

The compiler runs each series independently through DuckDB, then the formula
engine computes derived series in Rust:

1. Align data points by interval label.
2. For each interval, evaluate the expression with A, B, ... substituted.
3. Produce a new series result in the response.

Supported ops: `+`, `-`, `*`, `/`, `%`, `^` (power). Parentheses for grouping.
Division by zero → null. Missing series value → null.

---

## 3. Multi-breakdown

Current breakdown is a single property. Multi-breakdown adds a second dimension:

```rust
struct Query {
    breakdown: Option<Breakdown>,        // primary
    breakdown2: Option<Breakdown>,       // secondary (P3 addition)
}
```

Compilation: `GROUP BY interval, breakdown_value, breakdown2_value`.
Result shape: nested — each primary breakdown value contains sub-series for
each secondary value. The frontend renders this as a grouped bar chart or a
table with two header rows.

---

## 4. Module changes

```
src/query/
  ir.rs        → +Funnels, Retention, Lifecycle, Stickiness, Actors, Sql kinds
                 +FunnelConfig, RetentionConfig, LifecycleConfig, StickinessConfig,
                  ActorsConfig, SqlConfig, Formula, multi-breakdown
  compile.rs   → +compile_funnels, compile_retention, compile_lifecycle,
                  compile_stickiness, compile_actors, compile_sql
  oracle.rs    → +oracle_funnels, oracle_retention, oracle_lifecycle,
                  oracle_stickiness, oracle_actors
  formulas.rs  → NEW: expression parser, evaluator (Rust, post-SQL)
  mod.rs       → run_ir dispatches on QueryKind
src/routes/
  api.rs       → POST /api/query dispatches all insight kinds
```

---

## 5. Invariants

1. **Funnel correctness:** same as P0 hand-written SQL for ordered funnels.
   New features (unordered, window, exclusion) have independent oracle tests.
2. **Retention matrix:** matches the "slow obvious" Rust implementation
   (HashMap-based cohort builder).
3. **Formula engine:** pure arithmetic, no state, deterministic. Same inputs
   produce same outputs.
4. **SQL access:** user SQL runs with token forced + read-only view + time +
   memory caps. No writes possible.
5. **Oracle coverage:** every insight kind has at least one end-to-end test
   comparing DuckDB output to the Rust oracle.

---

## 6. Exit criteria

1. Funnels through the IR produce identical results to the hand-written SQL
   (regression test passes).
2. Funnels with breakdown produce a breakdown of each step by property value.
3. Retention query returns a cohort×period matrix, oracle-matched.
4. Lifecycle query classifies users as new/returning/resurrecting/dormant,
   oracle-matched.
5. Stickiness query returns 1x/2x/3x frequency buckets, oracle-matched.
6. Actors query returns persons for a given cell.
7. SQL access endpoint accepts raw SQL, returns results, rejects non-SELECT.
8. Formula `A / B` over two trends series computes correctly.
9. All existing tests (164) continue to pass.

---

## 7. What P3 does NOT do

- Paths (Sankey) — this is post-v1; basic paths (top-next-events) is in P3
  spec but deferred.
- World-map display — UI concern, not backend.
- Multi-breakdown beyond 2 dimensions.
- Funnel strict mode (no intervening events).
- Dashboard UI for any of this (P6).
- Saved insight persistence (P6).
