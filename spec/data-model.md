# Data Model Maturity — Implementation Spec

P2. Properties catalog, sessions, persons v2, enrichment. Every piece the
query lane needs to filter, break down, and compute over — beyond the raw
event stream.

Status: ◐ implementing
Oracle: handwritten SQL vs IR for autocomplete queries; sessionization
         unit-tested on known timelines; person merge regression suite
         already exists (P0).
Exit: filter autocomplete works; session math (avg duration, bounce rate)
      answerable through the IR; person list queryable from the dashboard.

---

## 1. Properties catalog (autocomplete)

The dashboard's event picker and filter builder need to know which event names
and property keys exist for a project, and for each key, the top-N values.

### SQLite tables

Maintained by the flusher as it writes Parquet — single writer, no contention.

```sql
CREATE TABLE event_names (
    token TEXT NOT NULL,
    name TEXT NOT NULL,
    last_seen INTEGER NOT NULL,  -- unix epoch seconds
    count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (token, name)
);

CREATE TABLE property_keys (
    token TEXT NOT NULL,
    source TEXT NOT NULL,   -- 'event' | 'person'
    key TEXT NOT NULL,
    type_guess TEXT,        -- 'string' | 'number' | 'boolean' | 'null'
    last_seen INTEGER NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (token, source, key)
);

CREATE TABLE property_values (
    token TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (token, key, value)
);
```

`property_values` is LRU-capped per (token, key) — keep the top 200 values by
count, evict the rest. This prevents unbounded growth from high-cardinality
properties.

### Flusher integration

`flush.rs` already iterates events as it writes Parquet. Add a catalog-update
step: for each event in the flushed segment, UPSERT into `event_names`,
`property_keys`, and `property_values`. Batch the UPSERTs in a single
transaction for efficiency.

The flusher calls `CatalogStore::ingest(events, token)` after each Parquet
write, before the WAL segment is deleted.

### CatalogStore

```rust
// src/catalog.rs (new module)
pub struct CatalogStore {
    conn: rusqlite::Connection,
}

impl CatalogStore {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn ingest(&self, events: &[CapturedEvent], token: &str) -> Result<()>;

    // Autocomplete APIs
    pub fn event_names(&self, token: &str, prefix: &str, limit: usize) -> Vec<String>;
    pub fn property_keys(&self, token: &str, source: FilterSource) -> Vec<PropertyKeyInfo>;
    pub fn property_values(&self, token: &str, key: &str, prefix: &str, limit: usize) -> Vec<String>;
}
```

### API surface

- `GET /api/catalog/events?token=X&prefix=page&limit=20` → `["pageview", "page_load"]`
- `GET /api/catalog/properties?token=X&source=event` → `[{key: "browser", type_guess: "string"}, ...]`
- `GET /api/catalog/values?token=X&key=browser&prefix=Ch&limit=20` → `["Chrome", "Chromium"]`

No new table for catalog — the catalog *is* the current event stream's property
surface. If a property hasn't been seen recently, it doesn't appear. This is
how most analytics tools work; it's honest and simple.

---

## 2. Sessions

PostHog SDKs send `$session_id` as an event property. For events that lack it
(older SDKs, server events), Hoglet assigns sessions via a 30-minute idle gap.

### Sessionization strategy

Two-pass approach, run by the flusher per Parquet segment:

1. **Resolve session IDs:** If `$session_id` is present on the event, use it.
   Otherwise, group by (distinct_id, timestamp) with a 30-min window. A new
   session starts when the gap between consecutive events for the same person
   exceeds 30 minutes.

2. **Roll up to sessions table:** After session IDs are assigned, compute per-
   session stats and upsert into SQLite.

### SQLite schema

```sql
CREATE TABLE sessions (
    session_id TEXT NOT NULL PRIMARY KEY,
    token TEXT NOT NULL,
    distinct_id TEXT NOT NULL,
    start_time INTEGER NOT NULL,   -- epoch seconds
    end_time INTEGER NOT NULL,
    duration_seconds INTEGER NOT NULL,
    event_count INTEGER NOT NULL DEFAULT 0,
    entry_event TEXT,               -- first event name
    exit_event TEXT,                -- last event name
    is_bounce INTEGER NOT NULL DEFAULT 0,  -- single-event session
    properties JSON NOT NULL DEFAULT '{}',  -- merged session properties
    last_updated INTEGER NOT NULL
);

CREATE INDEX idx_sessions_token_start ON sessions(token, start_time);
CREATE INDEX idx_sessions_person ON sessions(token, distinct_id);
```

### Query-lane integration

The flusher exports session data to a Parquet file
(`sessions/{token}/{date}/sessions.parquet`) alongside event files.
The query lane attaches it with a separate `read_parquet` call and joins on
`distinct_id` when session math is requested.

Session math variants in the IR (`UniqueSessions`, session duration aggregates)
compile to joins against the sessions Parquet view.

### Dashboard surface

- Session count per day (UniqueSessions math in trends)
- Avg session duration (PropertyAvg on `$session_duration`)
- Bounce rate (sessions with 1 event / total sessions)

---

## 3. Persons v2

Persons are already stored in SQLite via the identity store (P0). P2 adds
query-lane access and a person list API.

### Person list query

```json
POST /api/query
{
  "token": "phc_...",
  "query": {
    "kind": "Actors",
    "series": [],
    "filters": { "op": "AND", "values": [] },
    "person_filters": {
      "op": "AND",
      "values": [
        { "filter": { "source": "person", "key": "email", "operator": "contains", "value": "gmail" } }
      ]
    },
    "range": {},
    "interval": "Day"
  }
}
```

`ActorsKind` extends the IR:
```rust
enum QueryKind {
    Trends,
    Funnels,
    Actors,  // P2 addition
}

struct ActorsQuery {
    // Person-only filters
    person_filters: PropertyGroup,
    // Sort by property or event count
    order_by: Option<ActorsOrder>,
    // Pagination
    offset: usize,
    limit: usize,
}
```

### Person-property filter path

For P2, person filters compile to a semi-join against a fully materialized
person table in Parquet:

1. Flusher exports `persons/{token}/persons.parquet` with columns:
   `distinct_id`, `properties_json`, `created_at`, `is_identified`.

2. Query lane loads it alongside events and joins:
   ```sql
   WITH persons AS (SELECT * FROM read_parquet('persons/*.parquet')),
        e AS (SELECT * FROM read_parquet('events/*.parquet') ...)
   SELECT ...
   FROM e JOIN persons ON e.distinct_id = persons.distinct_id
   WHERE json_extract_string(persons.properties_json, '$.email') LIKE '%gmail%'
   ```

This removes the SQLite ATTACH from the query lane entirely (the P1 temporary
path).

### Person details endpoint

`GET /api/person?token=X&distinct_id=Y` → person profile with properties,
events timeline (paginated), and session list.

### Groups

`$groupidentify` events land in capture (already accepted by the event parser).
Flusher extracts them into a `groups` SQLite table:

```sql
CREATE TABLE groups (
    token TEXT NOT NULL,
    group_type TEXT NOT NULL,
    group_key TEXT NOT NULL,
    properties JSON NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    PRIMARY KEY (token, group_type, group_key)
);
```

Group properties are also exported to Parquet for query-lane joins (P3, when
group-aware insights land).

---

## 4. Enrichment

### GeoIP

Optional. Enabled when `HOGLET_GEOIP_DB` points to a MaxMind-format `.mmdb` file.
At capture time, resolve `$ip` → `$geoip_country_code`, `$geoip_city_name`,
`$geoip_latitude`, `$geoip_longitude`. Only resolved when properties are absent
(never overwrite client-set values).

Uses the `maxminddb` crate. The DB file is mmap'd once at startup; resolution
is cheap (binary trie lookup, <1µs).

### User-agent parsing

Optional. Enabled by default, disabled with `HOGLET_NO_UA_PARSE=1`.
At capture time, parse the `User-Agent` header (or `$user_agent` property) into
`$browser`, `$browser_version`, `$os`, `$os_version`, `$device_type`.

Uses the `woothee` crate (pure Rust, no external data files). Lightweight
enough to run on every capture.

### Cookieless mode

`HOGLET_COOKIELESS_SALT=...` enables cookieless device identification. When
`$device_id` is absent, generate a salted hash of (IP + User-Agent + daily
salt). The salt rotates daily, so the same browser on the same network gets
the same device ID within a 24h window but not across days.

This is a privacy layer for environments where third-party cookies are blocked
or consent is denied. It's coarse — no fingerprinting.

---

## 5. Module layout

```
src/
  catalog.rs           → CatalogStore (event_names, property_keys, property_values)
  session.rs           → SessionStore + sessionization logic
  enrichment.rs        → GeoIP + UA parsing + cookieless device ID
  identity/
    mod.rs             → unchanged: merge logic, persons SQLite store
    export.rs           → Person-to-Parquet export for query lane
  flush.rs             → updated: calls catalog.ingest, sessionize, export persons
```

---

## 6. Invariants

1. **Catalog is eventual.** The flusher updates the catalog after Parquet writes.
   A property seen in the last 5s might not appear yet — acceptable for
   autocomplete. Never block ingest on catalog updates.
2. **Session IDs are deterministic.** Given the same event stream, sessionization
   produces the same IDs. Replay-safe.
3. **Person merge is order-insensitive** (preserved from P0). The Parquet export
   reflects the post-merge state at export time.
4. **Enrichment is additive.** Never overwrite a property the client already
   set. `$ip` set by the client takes precedence over the connection IP.
5. **Catalog is bounded.** `property_values` is LRU-capped per key. `event_names`
   and `property_keys` grow with project diversity, which is naturally
   self-limiting (a project won't have 100k distinct event names).

---

## 7. Exit criteria

1. `GET /api/catalog/events?token=X` returns event names seen in flushed data.
2. `GET /api/catalog/properties?token=X` returns property keys with type guesses.
3. `GET /api/catalog/values?token=X&key=browser` returns top values.
4. Dashboard event picker reads from catalog API (any frontend wiring works).
5. Session rollup table populated and queryable; `UniqueSessions` math in
   trends IR returns session count.
6. Person list endpoint returns persons matching person-property filters.
7. GeoIP and UA enrichment produce `$geoip_*` / `$browser` / `$os` properties
   on captured events.
8. All existing tests (143) continue to pass.
9. New tests: catalog source-of-truth test (write events → flush → catalog
   matches), sessionization unit tests (known timelines), enrichment unit
   tests, person-export roundtrip.

---

## 8. What P2 does NOT do

- Group-aware insights in the query lane (P3).
- Materialized Parquet person tables for query-lane joins (stub the export path;
  the real join optimization lands in P3).
- Cohort membership evaluation (P5).
- Session replay ingestion (never).
- Full-text search on property values (uses prefix match only).
