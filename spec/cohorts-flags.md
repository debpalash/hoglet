# Cohorts & Flags Complete — Implementation Spec

P5. Cohorts as a first-class filter source in the IR, flag payloads and cohort
targeting, local evaluation endpoint, experience continuity.

Status: ◐ implementing
Exit: flag targeted at a behavioral cohort evaluates correctly through a
      stock SDK; local-eval passes the SDK contract test.

---

## 1. Cohorts (`src/cohort.rs` — new)

### SQLite schema

```sql
CREATE TABLE cohorts (
    id TEXT NOT NULL PRIMARY KEY,
    token TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,  -- 'static' | 'behavioral'
    definition JSON NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE cohort_members (
    cohort_id TEXT NOT NULL REFERENCES cohorts(id),
    distinct_id TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (cohort_id, distinct_id)
);

CREATE INDEX idx_cohorts_token ON cohorts(token);
```

### Types

```rust
struct CohortDef {
    id: String,
    token: String,
    name: String,
    kind: CohortKind,
    definition: CohortDefinition,
}

enum CohortKind { Static, Behavioral }

enum CohortDefinition {
    Static { distinct_ids: Vec<String> },
    Behavioral {
        event: String,
        operator: CohortOp,
        count: i64,
        window_days: u32,
    },
}

enum CohortOp { Gte, Lte, Eq }
```

### Operations

**Static:** set membership. Add/remove distinct_ids from a list. Use case:
import a CSV, or add people from a manual selection.

**Behavioral:** "people who performed event X ≥ N times in the last D days."
Evaluated by a scheduled query (recomputed every 5 minutes by the flusher).

The flusher calls `CohortStore::reevaluate(token)` which:
1. For each behavioral cohort, runs a DuckDB query against Parquet segments.
2. Clears and repopulates `cohort_members` for that cohort.

Behavioral cohorts can also reference other cohorts: "people who did X AND are
in cohort Y." This is compiled as a semi-join against `cohort_members`.

### Cohort as IR filter source

`FilterSource::Cohort` is added to the IR (stub exists, fully implemented now).
A cohort filter compiles to:

```sql
AND e.distinct_id IN (
    SELECT distinct_id FROM cohort_members WHERE cohort_id = ?
)
```

The `CohortStore` is passed to the query engine so the compiler can look up
cohort IDs by name when compiling filters.

### API

`POST /api/admin/cohorts` — create a cohort. Requires admin.

`GET /api/cohorts?token=X` — list cohorts for a project.

`GET /api/cohorts/:id/members` — paginated member list.

`DELETE /api/admin/cohorts/:id` — delete a cohort. Requires admin.

---

## 2. Flags — payloads + cohort conditions

### Flag payloads

Each flag (and each variant) can carry a JSON payload. The flags endpoint
returns payloads alongside the evaluation result:

```json
{
  "featureFlags": {
    "new-dashboard": "treatment"
  },
  "featureFlagPayloads": {
    "new-dashboard": "{\"color\": \"blue\"}"
  }
}
```

Payloads are returned in all `?v=` response shapes. The SDKs use them to
deliver configuration (e.g., a feature toggle also carries the component
to render).

### Cohort conditions

Flags can target cohorts. A flag's rollout condition can say:
"if person is in cohort 'beta-users', show the 'treatment' variant."

The flag store's evaluation function already supports property conditions.
Add cohort condition support: at evaluation time, check `cohort_members` for
the given distinct_id + cohort_id.

### Experience continuity

When a person is identified (anonymous → identified), their flag evaluations
should not change. This means: store the first-seen bucketing hash key on the
person record. On subsequent evaluations, use that stored key instead of the
current distinct_id for bucketing.

Implementation: when `$identify` fires, copy the anonymous person's `first_seen`
hash key to the identified person (if the identified person doesn't already
have one). The bucketing key is `sha1("{flag_key}:{hash_id}")` — the hash_id
is the stored `first_seen` key, not the current distinct_id.

---

## 3. Local evaluation endpoint

`GET /flags/definitions` — returns all flag definitions for a project
(authenticated by personal API key). The SDK caches this and evaluates
flags locally.

Response:
```json
{
  "flags": [
    {
      "key": "new-dashboard",
      "enabled": true,
      "variants": [...],
      "filters": { "groups": [...] },
      "rollout_percentage": 100
    }
  ]
}
```

This endpoint requires a personal API key (`phx_...`) via Authorization header.
Project write tokens (`phc_...`) are rejected — only personal keys can access
flag definitions.

The SDK uses this to evaluate flags without a server round-trip per flag
check. The response shape matches PostHog's `api/feature_flag/local_evaluation`
endpoint.

---

## 4. Cohort evaluation in the flusher

The flusher already ticks every 5 seconds. For cohorts, add a slower tick
(every 5 minutes):

```
flusher tick (5s) → Parquet write + catalog update
cohort tick (5min) → reevaluate behavioral cohorts via DuckDB
```

Each reevaluation:
1. Read the cohort definition from SQLite
2. Compile the behavioral query: count events per distinct_id in window
3. Run against DuckDB over Parquet
4. Populate cohort_members

This is fully consistent: a person entering a behavioral cohort sees the
cohort take effect within 5 minutes (the reevaluation interval).

---

## 5. Module changes

```
src/
  cohort.rs           → CohortStore (CRUD + behavioral reevaluation)
  query/
    ir.rs             → +FilterSource::Cohort
    compile.rs        → cohort filters compile to semi-joins
  flags/
    mod.rs            → +payloads, +cohort conditions, +experience continuity
  routes/
    flags.rs          → +cohort condition evaluation
    local_eval.rs     → GET /flags/definitions (NEW)
  flush.rs            → +cohort reevaluation tick
```

---

## 6. Invariants

1. Cohort membership is consistent within a reevaluation cycle. No partial updates.
2. Behavioral cohorts are recomputed from scratch — no incremental updates.
3. Flag evaluation with cohort conditions matches PostHog's semantics:
   cohort membership + rollout fraction + property conditions all AND'd.
4. Experience continuity: `$identify` preserves the first-seen bucketing key.
5. Local eval requires a personal API key; project tokens are rejected.
6. Flag payloads are returned as JSON strings (matching PostHog format).

---

## 7. Exit criteria

1. Create a static cohort, add members, use it as a filter in a trends query.
2. Define a behavioral cohort ("3+ pageviews in 7 days"), wait 5s for
   reevaluation, verify membership.
3. Create a flag targeting a cohort, evaluate for a person in the cohort.
4. Flag carries a payload, returned in all `?v=` shapes.
5. Local eval endpoint returns flag definitions, authenticated by personal key.
6. `$identify` preserves flag bucketing (pre- and post-identify evaluation
   returns the same variant).
7. All existing tests (184) continue to pass.

---

## 8. What P5 does NOT do

- Cohort time travel ("who was in this cohort 30 days ago?"). Cohort membership
  is current-only.
- Complex behavioral queries (AND/OR across multiple events). Single event,
  count operator only.
- Cohort overlap analysis ("people in both cohort A and cohort B").
- Flag experiment statistical analysis (A/B test significance).
- Scheduled cohort recalc beyond the flusher tick (no cron-like schedule).
