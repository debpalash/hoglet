# Scale — Implementation Spec

P7. Scale the engine from thousands to tens of millions of events. Every
optimization is structural — layout, rollups, caching — not just faster SQL.

Status: ◐ implementing
Exit: design targets met on a 1 vCPU/1 GB box; no query rescans the full store.

---

## 1. Parquet partitioning

Current layout: all events in a flat `events/*.parquet`. Every query reads every
file, even when the date range covers a single day. DuckDB's `read_parquet` does
no file-level pruning — it scans all files and does row-group stats filtering.

New layout: `events/{token}/{yyyy-mm-dd}/{uuid}.parquet`

### Store changes (`src/store/mod.rs`)

`write_events` currently takes `&[CapturedEvent]` and writes one file. After
P7, it partitions by (token, date) from each event's timestamp:

```
events/phc_abc/2026-07-22/a1b2c3.parquet
events/phc_abc/2026-07-22/d4e5f6.parquet
events/phc_def/2026-07-22/g7h8i9.parquet
```

Each call to `write_events` may produce multiple Parquet files (one per
(token, date) combination present in the batch).

### Query lane changes

Instead of `read_parquet('events/*.parquet')` (all files), the compiler now
generates file lists with directory-level pruning:

- Token filter → `read_parquet('events/{token}/*/*.parquet')`
- Date range → iterate directories within range, build file list

DuckDB reads the explicit file list rather than a glob, giving us directory-
level pruning for free:

```sql
SELECT * FROM read_parquet([
  'events/phc_abc/2026-07-20/...parquet',
  'events/phc_abc/2026-07-21/...parquet',
  'events/phc_abc/2026-07-22/...parquet'
]) WHERE ...
```

The file list is built by listing subdirectories under `events/{token}/` that
fall within the date range, then collecting all `.parquet` files within.

### Compaction

Compaction is per (token, date) partition. Small files within the same day are
merged into fewer, larger files (target: 64–128 MB per file). This keeps
file count bounded while respecting the partition layout.

### Retention enforcement

Deletion is partition-level: if all events in a date directory are beyond the
retention cutoff, delete the entire directory. No per-file scanning needed.

---

## 2. Daily rollups

Pre-computed aggregates stored as Parquet alongside event files. The flusher
writes rollups after each compaction cycle.

### Event×day rollup

```
events/{token}/rollups/{yyyy-mm-dd}/events.parquet

Schema: event (Utf8), day (Utf8), count (Int64)
```

Used by trends-total queries (no breakdown, no property filter). The IR
compiler checks if a query can use the rollup path:

- No property filters → rollup eligible
- No breakdown → rollup eligible
- Interval = Day → rollup eligible

When eligible, the compiler reads the rollup file instead of the event files,
avoiding the dedup CTE and per-row aggregation entirely. This cuts query cost
from O(events) to O(days × events_types).

### Person-day rollup (DAU sketches)

```
events/{token}/rollups/{yyyy-mm-dd}/dau.parquet

Schema: day (Utf8), persons (Int64)  -- count of distinct distinct_ids
```

Used by trends-dau queries with no event filter. Even simpler than the event
rollup — just one row per day.

### Oracle verification

Rollup paths produce the same results as full-scan paths. The oracle tests
verify this: for every dataset, the full-scan DuckDB query and the rollup-
backed query return identical counts. This is claims.md claim 4 applied to
the optimization path.

---

## 3. Result cache

Keyed by (token, IR hash, data version). The data version is the flush sequence
number — a monotonically increasing counter bumped on each Parquet write.

Cache entries are valid as long as the flush seq hasn't changed. The `refresh`
flag on `/api/query` (stub in P1) forces a re-query even when the cache is
fresh.

### Cache store

In-memory LRU (100 entries, 10 MB max). No persistence — the cache is
reconstructed on restart. For a single-node server, this is sufficient.

```rust
struct ResultCache {
    entries: Mutex<LruCache<CacheKey, CacheEntry>>,
}

struct CacheKey {
    token: String,
    ir_hash: u64,
    data_version: u64,
}

struct CacheEntry {
    response_json: Vec<u8>,
    inserted_at: Instant,
}
```

Cache hits return the cached JSON directly without touching DuckDB. This is
the biggest latency win for dashboard use — the same query run repeatedly (like
auto-refreshing stats every 2s) hits the cache.

### Data version

The flusher increments an atomic `u64` on each successful `write_events` call.
The query engine reads this version to form cache keys. A new event invalidates
all caches for that token. Coarse but correct.

For finer invalidation: track the max timestamp per token and only invalidate
if new events fall within the query's date range. Deferred to post-v1.

---

## 4. DuckDB connection pool

Current: each query opens a new in-memory DuckDB, executes, closes. This adds
~50ms of startup overhead per query (DuckDB init, catalog load).

After P7: a pool of N pre-opened in-memory DuckDB connections (N=4, matching
the concurrency cap). Connections are recycled — a query thread acquires one,
executes, and returns it. No `.duckdb` file on disk (invariant preserved).

```rust
struct DuckDbPool {
    connections: Mutex<VecDeque<Connection>>,
}
```

Each connection has `PRAGMA memory_limit='256MB'` set at pool creation.
Connections are not persistent across restarts — the pool is created at startup.

---

## 5. File-listing optimization

Instead of `std::fs::read_dir` per query (which hits the filesystem and can
stall under I/O pressure), the store maintains an in-memory file index:

```rust
struct FileIndex {
    // token → date → [parquet_files]
    files: RwLock<HashMap<String, BTreeMap<String, Vec<String>>>>,
    rollups: RwLock<HashMap<String, BTreeMap<String, Vec<String>>>>,
    version: AtomicU64,
}
```

Updated by the flusher after each successful write + compaction cycle. The
query compiler reads from this index instead of listing directories. This
removes filesystem I/O from the query path entirely.

---

## 6. Query-lane optimization: rollup recognition

The compiler inspects the IR and decides whether the rollup path is usable.
Decision logic added to `compile_trends`:

```
fn can_use_rollup(query: &Query) -> bool {
    query.kind == Trends
    && query.series.iter().all(|s| matches!(s.math, Total | Dau))
    && query.filters.values.is_empty()
    && query.breakdown.is_none()
    && query.interval == Day
    && no property math variants
}
```

When true, the compiled SQL reads from `rollups/events.parquet` instead of the
full event files. This is a ~100x improvement for the dashboard overview query
(which is exactly this pattern — total events + DAU over N days).

---

## 7. Module changes

```
src/store/
  mod.rs            → +partitioned write (token/date), file index
  parquet.rs        → unchanged
  rollup.rs         → NEW: rollup Parquet writer + schema
src/query/
  compile.rs        → +rollup recognition, file-list gen from index
  mod.rs            → +connection pool, result cache
  cache.rs          → NEW: ResultCache
  pool.rs           → NEW: DuckDbPool
src/flush.rs        → +rollup writes, file index update, data version bump
```

---

## 8. Invariants

1. Partition layout is deterministic: same event always lands in the same
   token/date directory.
2. Rollups are correct: the oracle verifies full-scan == rollup for every query
   that uses the rollup path.
3. Cache invalidation is conservative: stale data is allowed (cache entries are
   at most one flush cycle behind), incorrect data is not (cache key includes
   token + IR hash + version).
4. Connection pool never blocks ingest: pool contention just delays queries.
5. File index is write-on-update: replaced atomically, never modified in place.

---

## 9. Exit criteria

1. Events are written to `events/{token}/{date}/*.parquet`.
2. Compaction merges within a partition, not across dates or tokens.
3. Rollup files are written alongside event files; rollup-backed trends query
   matches the full-scan result.
4. Result cache returns cached response on repeat query with same IR + version.
5. Query file listing reads from the file index, not the filesystem.
6. All existing tests (187+) continue to pass.
7. A 10M event dataset queries in <2s for trends and <5s for funnel on the
   reference box (verified in P8 benchmark).
