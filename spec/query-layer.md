# Query Layer — Implementation Spec

P1. The load-bearing rebuild. Everything after this — insights, actors, cohorts,
rollup equivalence — is expressed in this layer. Fixed per-endpoint SQL in
`src/query/mod.rs` is replaced by a typed query IR compiled to DuckDB SQL.

Status: ◐ implementing
Oracle: independent Rust IR interpreter cross-checked against DuckDB results
Exit: trends-total/dau + one filter + one breakdown IR end-to-end, oracle-matched

---

## 1. Query IR (`src/query/ir.rs`)

A serializable query description. Every insight kind (trends, funnels, retention,
etc.) maps to one `Query` struct; the frontend builds and stores `Query` JSON.
This IR *is* the saved-insight format and the `ts-rs` seam to the dashboard.

### Types

```rust
struct Query {
    kind: QueryKind,            // Trends, Funnels, Retention (stub for P1), Actors (stub)
    series: Vec<Series>,        // One or more event series to compute
    filters: PropertyGroup,     // Global filters (AND/OR two-level)
    breakdown: Option<Breakdown>, // Group by a property value
    range: DateRange,           // Time window
    interval: Interval,         // Bucket: hour, day, week, month
}

enum QueryKind {
    Trends,
    Funnels,
    // Retention, Lifecycle, Stickiness, Paths, Actors — later phases
}

struct Series {
    event: EventMatch,          // Which events this series counts
    math: Math,                 // How to aggregate
}
```

#### EventMatch

```rust
enum EventMatch {
    Name(String),               // exact event name
    Regex(String),              // event name regex (post-v1)
    Any,                        // any event (total volume)
}
```

P1 ships only `EventMatch::Name`. Regex and Any are stubs.

#### Math

```rust
enum Math {
    Total,
    Dau,                        // distinct persons per day
    Wau,                        // distinct persons per week (rolling)
    Mau,                        // distinct persons per month (rolling)
    UniqueSessions,
    FirstTime,                  // first-ever time a person did this event
    PropertySum(String),
    PropertyAvg(String),
    PropertyMin(String),
    PropertyMax(String),
    PropertyMedian(String),
    PropertyP75(String),        // percentiles
    PropertyP90(String),
    PropertyP95(String),
    PropertyP99(String),
    CountPerActor(ActorAgg),    // count of events per person, aggregate the result
}

enum ActorAgg {
    Avg,
    Min,
    Max,
    Median,
    P90,
}
```

P1 ships `Total` and `Dau`. Remaining math stubs for insights phase.

#### PropertyGroup (Filters)

Two-level AND/OR tree — the standard analytics filter model.

```rust
struct PropertyGroup {
    op: GroupOp,
    values: Vec<GroupOrFilter>,
}

enum GroupOp { And, Or }

enum GroupOrFilter {
    Group(PropertyGroup),
    Filter(Filter),
}

struct Filter {
    source: FilterSource,       // which entity holds the property
    key: String,                // property name
    operator: FilterOperator,
    value: serde_json::Value,
}

enum FilterSource {
    Event,
    Person,
    // Session, Cohort — later phases
}

enum FilterOperator {
    /// Exact match
    Exact,
    /// Case-insensitive exact match
    IExact,
    /// Does not equal
    NotEqual,
    /// Contains substring
    Contains,
    /// Does not contain substring
    NotContains,
    /// Case-insensitive contains
    IContains,
    /// Regex match (post-v1)
    Regex,
    /// Not regex match (post-v1)
    NotRegex,
    /// Is set (property exists and is not null/empty)
    IsSet,
    /// Is not set
    IsNotSet,
    /// Equal to (numeric)
    Equal,
    /// Greater than
    Gt,
    /// Less than
    Lt,
    /// Greater than or equal
    Gte,
    /// Less than or equal
    Lte,
    /// Between (inclusive)
    Between,
    /// In list
    In(Vec<serde_json::Value>),
    /// Not in list
    NotIn(Vec<serde_json::Value>),
}
```

#### Breakdown

```rust
struct Breakdown {
    source: FilterSource,       // Event or Person property
    key: String,                // property name
    limit: usize,               // top N values to return
}
```

#### DateRange & Interval

```rust
struct DateRange {
    from: Option<DateTime<Utc>>, // None = everything
    to: Option<DateTime<Utc>>,
    // Convenience: relative to now
    last_n: Option<LastN>,
}

enum LastN { Hours(u32), Days(u32), Weeks(u32), Months(u32) }

enum Interval { Hour, Day, Week, Month }
```

---

## 2. Compiler (`src/query/compile.rs`)

Takes `Query` + token → `CompiledQuery` (SQL string + params for DuckDB).

### Compilation pipeline

1. **Resolve range** → WHERE clause on `timestamp`.
2. **Compile filters** → SQL predicate chain.
   - Event filters: DuckDB JSON extraction on `properties` column
     (`json_extract_string(properties, '$.{key}')`)
   - Person filters: semi-join against a temp view from SQLite identity store
     (exported to Parquet or attached read-only — see §5)
   - Operators fold into SQL: `=`, `ILIKE`, `>`, `<`, etc.
3. **Compile math** → aggregation expression.
   - `Total` → `count(*)`
   - `Dau` → `count(DISTINCT distinct_id)`
   - Property aggregations → `CAST(json_extract(properties, '$.{key}') AS DOUBLE)`
4. **Compile breakdown** → additional GROUP BY + ORDER BY + LIMIT.
5. **Compile interval** → `strftime` or `date_trunc` bucket expression.
6. **Wrap in base CTE** → same deduped-events CTE used today.

### SQL template (Trends, kind=Trends)

```
WITH deduped AS (
    SELECT * FROM read_parquet('*.parquet')
    QUALIFY row_number() OVER (PARTITION BY uuid ORDER BY timestamp) = 1
), e AS (
    SELECT * FROM deduped WHERE token = ?{0}
)
SELECT
    strftime(e.timestamp, '%Y-%m-%d') AS interval,
    count(*) AS count                     -- Total math
    <breakdown_expr>                      -- if breakdown
FROM e
WHERE {filters} AND {date_range}
GROUP BY interval <breakdown_group>
ORDER BY interval
```

### Person filters

Until the flusher materializes person properties to Parquet (P2), person
filters compile to a semi-join against an **attached SQLite database** from the
identity store. The query engine receives an `Arc<rusqlite::Connection>` and
attaches it as a read-only DuckDB view via:

```
ATTACH 'file:identity.db?mode=ro' AS persons_db (TYPE SQLITE);
-- then:
AND e.distinct_id IN (SELECT distinct_id FROM persons_db.persons WHERE ...)
```

This is the P1 temporary path. P2 ships materialized Parquet person tables,
removing the SQLite dependency from the query lane.

---

## 3. API surface

### `POST /api/query`

Replaces and subsumes the current `/api/stats`, `/api/trend`, `/api/funnel`
endpoints. Those become thin IR builders that forward to the IR compiler, then
are removed (P3).

**Request:**

```json
{
  "token": "phc_...",
  "query": { <Query IR> },
  "refresh": false
}
```

- `token` authenticates the project.
- `query` is the serialized IR.
- `refresh` (optional, default false): bypass result cache when true.

**Response (Trends):**

```json
{
  "results": [
    {
      "label": "pageview — Total",
      "data": [
        { "interval": "2026-07-01", "count": 1423 },
        ...
      ],
      "breakdown_value": null
    }
  ],
  "meta": {
    "kind": "trends",
    "elapsed_ms": 12,
    "cached": false
  }
}
```

**Errors:** 400 for invalid IR, 401 for unknown/unauthorized token, 504 for
timeout, 500 for DuckDB failure.

### Legacy endpoints preserved

Existing `/api/stats`, `/api/top_events`, `/api/trend`, `/api/funnel`,
`/api/recent` continue working. They compile their query to the IR then run
through the same compiler — this proves the IR covers the old surface. Removed
in P3.

---

## 4. Oracle (`src/query/oracle.rs`)

Grows an IR interpreter that operates on `&[CapturedEvent]` directly.

For a given `Query`:
1. Dedup events by uuid (same rule as base CTE).
2. Filter by token.
3. Apply `PropertyGroup` filters with the same operator semantics as the SQL
   compiler.
4. Apply `DateRange`.
5. Compute `Math` for each `Series` in Rust.
6. Apply `Breakdown` via HashMap grouping.

Every new `QueryKind` and `Math` variant added to the compiler comes with an
oracle counterpart. Test: generate a varied dataset, compile to SQL, run both
the DuckDB path and the Rust oracle path over the same data, assert agreement.

The oracle is the defence against SQL compiler bugs, not against DuckDB. The
contract tests are the defence against wire-level incompatibility.

---

## 5. Person-filter temporary path

P1 compounds person-keyed filters by reading SQLite via DuckDB's ATTACH:

```
ATTACH 'file:{path}' AS persons (TYPE SQLITE);
```

The identity store exposes a `persons(token, distinct_id, properties_json)`
view. Person filters compile to:

```sql
e.distinct_id IN (
    SELECT distinct_id FROM persons
    WHERE token = ? AND <filter on json_extract(properties_json, '$.{key}')>
)
```

DuckDB's SQLite attachment reads the write-locked file snapshot-consistent
(WAL mode). The identity store uses SQLite WAL mode for this reason.

Exit for P2: person properties are materialized to Parquet by the flusher and
the SQLite attach path is removed from the query lane.

---

## 6. Module layout

```
src/query/
  mod.rs       → QueryEngine updated: runs compiled IR, holds compiler + identity conn
  ir.rs        → Query IR types, serde, ts-rs
  compile.rs   → IR → CompiledQuery (DuckDB SQL + params)
  oracle.rs    → IR interpreter for correctness cross-check
```

---

## 7. Invariants

1. **IR round-trips.** Serialize(`Query`) → deserialize → compile produces the
   same SQL as the original. fuzzed.
2. **DuckDB never owns data.** It reads Parquet files read-only. No `.duckdb`
   file on disk.
3. **Oracle agrees.** Every query kind + math variant in P1 has a test:
   DuckDB result == Rust oracle result over the same events.
4. **IR is additive.** New query kinds, math variants, and operators are added
   as new enum variants. Never remove or renumber — the IR is the saved-insight
   format.
5. **Filter operators are exact.** `FilterOperator` semantics are PostHog's,
   not simplified. `Exact` is case-sensitive, `IExact` is case-insensitive.
6. **Token gating.** Every query includes a token; the compiler passes it as
   a parameter, never string-interpolated.

---

## 8. Exit criteria

1. `POST /api/query` accepts a `Query` IR with kind=Trends, series=Total and Dau.
2. At least one `Filter` compiles to predicate SQL and filters correctly (oracle
   agrees).
3. One `Breakdown` (event property) compiles and groups correctly (oracle
   agrees).
4. Existing `/api/stats`, `/api/trend` are backed by the IR compiler and produce
   identical results.
5. Dashboard trends panel switched to `POST /api/query`.
6. All existing tests pass (query mod tests, oracle tests).

---

## 9. What P1 does NOT do

- Funnels through the IR (stays on hand-written SQL for now; migrated in P3
  when the full insight suite lands).
- Retention, lifecycle, stickiness, paths, SQL access (P3).
- Person-filter via SQLite attach (stub the path; ship the real thing in P2
  with materialized Parquet person tables).
- Result cache (the `refresh` param is accepted and ignored).
- Actor drill-down queries.
- Dynamic interval (interval bound to `Day`; `Hour`/`Week`/`Month` are stubs).
