# Hoglet Spec Index

The master specification and roadmap. This file is the **index**: it defines the
product, the system architecture, the phased road to v1, and — per phase — a
high-level implementation blueprint. Each phase gets its own detailed spec file
in this folder **before its code is written**; this index names them, tracks
their status, and holds the invariants that bind them all.

Rules for this folder:
- One spec file per phase/subsystem, named in the catalog below. A spec is
  written *before* implementation and updated when reality diverges.
- Every spec states: data model, module boundaries, API surface, invariants,
  its oracle/test strategy, and exit criteria. No spec ships without naming how
  it is proven (claims.md discipline).
- This index owns cross-cutting truth (invariants, limits, authority). Child
  specs own their subsystem's detail. Don't duplicate — link.

Status legend: ✅ built · ◐ partial · ○ not started.

---

## 1. Product definition

A drop-in PostHog-compatible product-analytics backend in one Rust binary. Stock
PostHog SDKs point at it and work unchanged. One static binary, one $5 server —
no Kafka, ClickHouse, Redis, or Zookeeper.

**v1 is done when:** a developer points a PostHog SDK at Hoglet and a
non-trivial team uses it daily — funnels and retention on their own events,
filtering and breakdowns by any property, cohorts, feature flags with real
targeting, saved insights on dashboards, sharing, projects and API keys — on one
self-operated binary, at tens of millions of events, without query timeouts.

**Honest current state:** the ingest spine, the query layer, and the data model
are built and tested; auth, saved insights, dashboards, and the insight builder
work end to end. Roughly a third of v1. What remains is breadth rather than
foundations: paths, cohort depth, flag continuity, rollups, and the measured
benchmark. Phase status below is the ground truth — update it in the same change
that moves the code, or it stops being true.

Strategy and market: `../why-hoglet.md`, `../moats.md`, `../industry-map.md`.
Rules and scope ladder: `../CLAUDE.md`. Decisions and build method:
`../decisions.md`. Claims and evidence: `../claims.md`. Stack: `../stack.md`.

Out of scope for v1 (post-v1 or never — see CLAUDE.md scope ladder): session
replay, surveys UI, experiments/stats, warehouse, CDP, Sankey paths graph,
funnel correlation, multi-node replication, observability (never — store
trace_id, link out).

---

## 2. System architecture blueprint

The shape every phase builds inside. One process, two lanes; ingest always wins
under contention.

```
SDK ─HTTP─▶ capture ─▶ WAL (fsync) ─ack▶ 2xx
                          │                └─▶ identity/sessions (async) ─▶ SQLite
                          ▼ 5s flush
                Parquet segments (day-partitioned) ◀─ compactor ─▶ rollups
                          ▲
              DuckDB (read-only, capped) ◀─ Query IR compiler ◀─ POST /api/query
                          │                                          ▲
                          └────────── result cache ──────────────────┘
                                                              dashboard (React)
```

**Datastores** (locked, `../decisions.md`): Parquet + DuckDB for events (OLAP,
read-only query engine, never a live .duckdb file); SQLite for persons,
identity, sessions rollup, flags, cohorts, registry, users (OLTP, single
writer). Hand-rolled WAL owns durability. No other engine.

**Invariants — every phase must preserve:**
1. Ack only after fsync. A 2xx means durable.
2. Flush order: Parquet durable before WAL segment delete; crash between yields
   duplicates (uuid-deduped at query), never loss.
3. One person per (token, distinct_id); merges deterministic, order-insensitive.
4. No unbounded work in the ingest path; overload sheds 503.
5. Wire semantics byte-for-byte at the edge; 4xx never retried, 5xx retried.
6. Query lane can never OOM or starve ingest (memory cap, concurrency cap,
   blocking pool).
7. On-disk formats versioned, forward-only; newer binary opens older data;
   downgrade fails loud. Parquet schema additive-only.
8. Every query kind has an independent Rust oracle; a SQL bug must not be able
   to silently skew a number.
9. A resource has exactly one writer (WAL→writer thread, sealed segments→
   flusher, Parquet merge→compactor, SQLite→its store's mutex).

**Explicit limits** (TigerBeetle discipline; the table lives with the code
constants): body 2/20 MB, decompressed 64 MB, WAL record/segment 64 MB, queue
1024 batches, group commit 256, distinct_id 200 chars, funnel 12 steps, DuckDB
256 MB + 4 concurrent, compaction 64 files/run.

**Design targets** (internal, not published claims): ≥5k events/s sustained on
1 vCPU; ack p99 <50 ms; RSS <400 MB under load; cold start <1 s; funnel over
10 M events <2 s; recovery <5 s. Published numbers require the real 1 GB box
(`../claims.md` claim 3).

**Security model:** token shape + registry authenticity (open mode → closed on
first project); per-token rate limit; bomb-resistant decode; admin API off
unless `HOGLET_ADMIN_TOKEN`; dashboard auth arrives in P6 (until then: do not
expose the dashboard publicly); TLS at a reverse proxy; GDPR erasure is
physical.

**Failure matrix** (hard vs degrade): disk-full WAL → 503 never false 2xx; torn
WAL tail → truncate to valid prefix; Parquet flush failure → segment retained,
retried; SQLite busy → identity retried, self-heals from log; query OOM/timeout
→ that query fails, ingest untouched; flusher crash → WAL replays; queue full →
503 shed.

---

## 3. Spec catalog

| Spec file | Scope | Phase | Status |
|---|---|---|---|
| `README.md` (this) | Index, architecture, roadmap | — | ✅ living |
| `wire-compat.md` | PostHog wire contract (capture, config, flags shapes, retry) | P0 | ✅ needs P8 additions |
| `query-layer.md` | Query IR, SQL compiler, /api/query, caching, oracle | P1 | ✅ built |
| `data-model.md` | Properties catalog, sessions, persons/profiles, groups, enrichment | P2 | ✅ built |
| `insights.md` | Trends/funnels/retention/lifecycle/stickiness/paths templates + actor drill-down | P3 | ◐ six kinds + actor drill-down built, paths deferred |
| `auth.md` | Users, orgs/projects/roles, API keys, dashboard auth | P4 | ✅ built |
| `cohorts-flags.md` | Cohorts (static+behavioral), flag targeting/payloads/continuity/local-eval | P5 | ◐ core built, continuity deferred |
| `dashboards.md` | Saved insights, dashboard grid, sharing, insight-builder UI | P6 | ◐ backend + insight builder built, drag-layout deferred |
| `scale.md` | Partitioning, rollups, session table, values catalog, cache, benchmark | P7 | ◐ partitioning + cache + index built, rollups/benchmark deferred |
| `launch.md` | Install story, demo, measured numbers, docs site | P8 | ✅ spec written, boss-gated |

Writing a phase's spec is that phase's first task. The blueprint sections below
are the brief each spec file expands.

---

## 4. Roadmap

### P0 — Ingest spine & wire edge ✅ (built)

What exists and is tested: aliased capture endpoints with sniff-first
decompression (gzip magic bytes, base64, form, lz64-legacy) and full field
resolution (token precedence, distinct_id rules, offset/sent_at clock-skew
timestamps, UUIDv7-from-event-time); WAL with group-commit fsync, CRC segments,
replay-truncate recovery, SIGKILL-proven; Parquet store with atomic publish,
compaction, retention TTL, schema-version stamps; identity merge with PostHog
precedence, permutation-proven; flags with rollout/variants/property-conditions
and all `?v=` shapes; registry (open/closed mode); rate limiting; readiness;
metrics; GDPR erasure; Scalar/OpenAPI docs; React dashboard shell with ts-rs
types; SDK contract test (real posthog-node, unpinned) + CI.

Remaining P0 debt, folded into later phases: groups ingestion (P2), exception
events (P2), local-eval endpoint (P5), surveys/EAF stubs (P8).

### P1 — Query layer (the load-bearing rebuild) ✅ (built) → `query-layer.md`

Everything after this rides on it. Fixed per-endpoint SQL is replaced by a
typed query IR compiled to DuckDB SQL.

Blueprint:
- **IR** (`src/query/ir.rs`): `Query { kind, series: Vec<Series>, filters:
  PropertyGroup, breakdown: Option<Breakdown>, range: DateRange, interval }`.
  `Series { event: EventMatch, math: Math }`. `Math` enum: Total, Dau, Wau,
  Mau, UniqueSessions, FirstTime, PropertySum/Avg/Min/Max/Median/P75/P90/P95/P99
  (property name), CountPerActor(agg). `PropertyGroup { op: And|Or, values:
  Vec<GroupOrFilter> }` (two-level). `Filter { source: Event|Person|Session|
  Cohort, key, operator, value }` with ~20 operators. Serde-serializable — this
  IR *is* the saved-insight format and the ts-rs seam to the frontend.
- **Compiler** (`src/query/compile.rs`): IR → SQL string + params over the
  deduped-events CTE; property access via DuckDB JSON functions on the
  properties column; person/session/cohort filters compile to semi-joins
  against SQLite-exported tables (attached read-only or materialized to
  Parquet by the flusher — decide in the spec).
- **Endpoint**: `POST /api/query { query, refresh? }` dispatching on `kind`;
  sync first, async+poll later; result cache keyed by (IR hash, data version =
  last flush seq). Old `/api/*` endpoints become thin IR builders, then die.
- **Oracle**: `oracle.rs` grows the same IR interpreter over `&[CapturedEvent]`;
  every kind cross-checked on varied + property-tested datasets.
- Exit: ✅ trends-total/dau + filters + one breakdown run through IR end-to-end,
  oracle-matched; dashboard switched to /api/query.

### P2 — Data model maturity ✅ (built; GeoIP deferred) → `data-model.md`

Blueprint:
- **Properties catalog**: SQLite tables `event_names(token, name, last_seen,
  count)`, `property_keys(token, source, key, type_guess)`,
  `property_values(token, key, value, count)` (top-N per key, LRU-capped),
  maintained by the flusher as it writes Parquet. Serves autocomplete
  (`GET /api/catalog/...`) and breakdown-limit decisions.
- **Sessions**: `$session_id` from SDK events respected; sessionization for
  events lacking it (30-min idle) in a SQLite `sessions` rollup (session_id,
  person, start, end, duration, entry/exit, event_count, bounce) updated by the
  flusher; exported to Parquet for query-lane joins.
- **Persons v2**: profile surface — `GET/POST` person list query via IR
  (PersonsKind), person properties from identity store joined in queries;
  `$groupidentify` → `groups(token, type, key, properties)` in SQLite.
- **Enrichment**: GeoIP (MaxMind-format file, optional path via env) + UA
  parsing at capture into `$geoip_*`, `$browser`, `$os`, `$device_type` when
  absent. Cookieless mode: salted-hash device id (daily rotation) behind env.
- **Groups + exceptions ingestion** land here.
- Exit: filter autocomplete works; session math answers; person list queryable.

### P3 — Insight suite ◐ (six kinds + actor drill-down built; paths deferred) → `insights.md`

Each insight = an IR kind + compiler template + oracle + UI panel. Order:
1. **Trends complete**: full math matrix, formulas (A/B arithmetic over series
   results — post-SQL in Rust), multi-breakdown, intervals hour→month,
   world-map/table displays.
2. **Funnels complete**: ordered/unordered/strict; conversion window; breakdown
   + first/last-touch attribution; exclusions; time-to-convert histogram.
3. **Retention**: recurring + first-time, Day/Week/Month grid, cohort×period
   matrix.
4. **Lifecycle** (new/returning/resurrecting/dormant) and **Stickiness**.
5. **Paths-basic**: top-next-events table after X (Sankey is post-v1).
6. **SQL access**: `POST /api/query {kind: Sql}` — user SQL against a read-only
   attached view, capped/timeboxed; the power-user escape hatch.
7. **Actor drill-down**: every result cell → ActorsKind query returning the
   people behind the number; UI modal.
- Exit: the six insight types oracle-proven and driven from the dashboard.

### P4 — Auth & accounts ✅ (built) → `auth.md`

Required before anyone runs this for real. Built: the dashboard shell is
public, every `/api/*` route is gated, and the SPA renders setup or login off
that 401.

Blueprint:
- SQLite `users(id, email, pw_hash argon2, created)`, `orgs`, `org_members
  (role: owner|admin|member)`, `projects.org_id`, `api_keys(kind: project_write
  | personal_read phx_, hash, last_used)`.
- Session-cookie auth (SameSite, HttpOnly; no JWT needed single-node), login/
  logout/first-run-creates-owner flow; all `/api/*` gated by session or
  personal key; capture edge stays token-authed; admin API absorbed into
  role=admin.
- UI: login page, org/project switcher, members page, key management.
- Exit: fresh install forces account creation; anonymous `/api/*` is 401.

### P5 — Cohorts & flags complete ◐ (core built; continuity + local-eval deferred) → `cohorts-flags.md`

Blueprint:
- **Cohorts**: SQLite `cohorts(token, name, kind: static|behavioral, definition
  JSON)`; static membership table; behavioral subset (performed ≥/≤/= N in
  window, did-not-perform, first-time) evaluated by a scheduled query into a
  membership table (recomputed on interval — precalculated like the big
  players). Cohort = filter source (P1 IR) + flag condition.
- **Flags**: payloads (per-flag/variant, JSON-string in metadata.payload);
  cohort + group conditions; experience continuity (stable bucketing key
  through identify — store first-seen hash key on person); `GET
  /flags/definitions` for SDK local evaluation gated by personal key; flag UI
  (list/edit in dashboard).
- Exit: flag targeted at a behavioral cohort evaluates correctly through a
  stock SDK; local-eval passes the SDK contract test.

### P6 — Dashboards, saved insights, sharing ◐ (builder + sharing built; drag-layout deferred) → `dashboards.md`

Blueprint:
- SQLite `insights(id, project, name, description, query_ir JSON, created_by)`,
  `dashboards(id, name, layout JSON)`, `dashboard_tiles`, `share_links(token,
  object, expires)`.
- API: CRUD for insights/dashboards, `GET /shared/{token}` read-only render.
- UI: the **insight builder** (event picker fed by the catalog, math dropdown,
  filter rows, breakdown selector — an IR editor), dashboard grid w/
  drag-layout, date-override, share dialog.
- Exit: build → save → pin → share loop works end to end.

### P7 — Scale ◐ (partitioning + cache + index built; rollups + benchmark deferred) → `scale.md`

Blueprint:
- **Layout** ✅: Parquet partitioned `events/{token}/{yyyy-mm-dd}/*.parquet`;
  queries build an explicit pruned file list; pre-partition flat files stay
  readable, so upgrading needs no migration;
  compactor per-partition; queries prune by range + token before DuckDB sees a
  file list.
- **Engine**: persistent DuckDB connection pool with attached views instead of
  per-request open+glob; rely on row-group stats for pushdown.
- **Rollups** ○ (next): flusher-maintained daily Parquet rollups (event×day×count,
  person-day for DAU sketches); trends hits rollups when the IR allows, raw
  otherwise; correctness guarded by the oracle comparing rollup vs raw paths.
- **Caching**: result cache with data-version invalidation (already speced in
  P1) + catalog-backed breakdown limits.
- **Benchmark**: the claim-3 run on a real 1 vCPU/1 GB box: sustained ingest +
  funnel over ≥10 M events; numbers into `../claims.md`; only then published.
- Exit: design targets met on the box; no query rescans the full store.

### P8 — Launch (M3) ○ → `launch.md`

One-curl install script + prebuilt binaries (macOS/Linux, arm+x86); docker
image (single container); demo dataset seeder (`HOGLET_DEMO=1`); README demo
GIF; measured-numbers benchmark post; docs site from the spec folder; Show HN.
Wire-compat spec gains: surveys/EAF empty endpoints, remote-config script
(`config.js`), anything the nightly SDK matrix flags. Boss owns timing,
naming, publishing (decisions.md).

---

## 5. Cross-phase workstreams

- **Compatibility harness** (continuous): nightly contract tests vs SDK
  `latest` (node ✅, browser ✅ Chromium via Playwright, python/mobile ○). Each phase that
  touches the edge adds a contract case, not just unit tests.
- **Oracle discipline** (continuous): every query kind, rollup path, and flag
  evaluation has an independent Rust check. This is claim 4's spirit applied
  everywhere: the numbers reconcile, provably.
- **Docs** (continuous): `/openapi.json` + Scalar `/docs` updated in the same
  PR as any endpoint change; the spec file of the active phase updated when
  implementation diverges.

## 6. Sequencing rationale

Query layer first because five later phases are expressed in it (insights,
actors, cohorts, saved insights, rollup equivalence). Data model second because
filters/breakdowns are only as good as the catalog and sessions beneath them.
Auth before dashboards because sharing without login is a liability, and before
launch for obvious reasons. Scale second-to-last because rollups need the IR to
know when they're usable, and the benchmark is only honest against the real
feature set. Launch last and boss-gated.
