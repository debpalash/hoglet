# Hoglet — System Spec

The specification of the target system. High-level on purpose: it routes to the
doc that owns each detail rather than repeating it, states the envelope we build
toward, names the invariants a change must not break, and matches every public
claim to the evidence that proves it.

Read this first. Then load only the deeper doc your task needs.

**Status legend:** ✅ built · ◐ partial · ○ planned. Tags mark current reality;
the prose describes the target system regardless of tag.

## What it is

A drop-in PostHog-compatible product-analytics backend in one Rust binary.
Stock PostHog SDKs point at it and work unchanged. One static binary, one
`$5` server, no Kafka/ClickHouse/Redis/Zookeeper.

Strategy and market: `why-hoglet.md`, `moats.md`, `industry-map.md`.
Rules and scope: `CLAUDE.md`. Rationale for every locked choice: `decisions.md`.

## Goals and non-goals

Every design choice serves these, in order:

1. **Drop-in compatibility.** Change `api_host`, nothing else. The wedge.
2. **One light binary.** Runs and survives on 1 vCPU / 1 GB. The moat
   incumbents can't follow us to.
3. **Numbers you can trust.** No ghost profiles, no silent loss. The moat that
   compounds. Correctness outranks features and speed.
4. **Both worlds.** Frontend and backend events, identity, flags in one place.

**Non-goals** (`CLAUDE.md` scope ladder): session replay, surveys, experiments,
warehouse, and observability (traces/logs/metrics) are not built. We store a
correlation id and link out; we never become an observability platform. Multi-
node write scaling is out — scale-up before scale-out.

## Design targets — the envelope

Internal engineering targets we build and measure against. **Not published
claims** — a number ships only after `claims.md` proves it on stated hardware.
Target box: 1 vCPU, 1 GB RAM, spinning-disk-class IO.

| Dimension | Target | Notes |
|---|---|---|
| Sustained ingest | ≥ 5k events/s on 1 vCPU | group-commit amortizes fsync |
| Ack latency p99 | < 50 ms under load | dominated by group-commit fsync |
| RSS idle / under load | < 150 MB / < 400 MB | ingest allocations bounded |
| Cold start to ready | < 1 s | readiness endpoint gates traffic |
| Binary size (stripped) | < 60 MB | DuckDB static lib dominates |
| Funnel over 10 M events | < 2 s | design goal for the query lane |
| Recovery time after crash | < 5 s for a full WAL | replay is linear scan |

When a target and a measured reality diverge, the measurement wins and the
target is revised here.

## Owners — where truth lives

Don't duplicate these; change them at the source.

| Concern | Owning doc |
|---|---|
| PostHog wire contract (endpoints, bodies, codes) | `compat-spec.md` |
| What we promise + how it's proven | `claims.md` |
| Stack choices and rejected alternatives | `stack.md` |
| Who decides what; locked decisions | `decisions.md` |
| Communication, scope ladder, engineering standard | `CLAUDE.md` |

## Shape

One process. Two lanes that never starve each other (`decisions.md`
"one binary, two lanes"):

- **Ingest lane** — hot, must never lose data. Reserved worker budget, bounded
  queues. Wins under contention.
- **Query lane** ✅ — heavy CPU, bursty. DuckDB over Parquet with a semaphore
  concurrency cap and a `memory_limit` (spill-to-disk), run on blocking threads
  so a funnel query can never starve or OOM the ingest lane.

```
SDK ─HTTP─▶ capture ─▶ WAL (fsync) ─ack▶ 2xx
                          │                └─▶ identity (async) ─▶ SQLite
                          ▼ 5s flush
                       Parquet segments ◀─ compactor
                          ▲
                       DuckDB (read-only)  ─▶ query API ─▶ dashboard   ○ M2
```

## Data flow — ingest ✅

1. **Edge.** `capture/mod.rs` — one handler aliased across `/e`, `/capture`,
   `/batch`, `/track`, `/engage`, `/i/v0/e`. Response codes are load-bearing
   (`compat-spec.md`): 200 normal, 204 beacon, 4xx never-retry, 503 retryable.
2. **Decode.** `capture/decompress.rs` — form-unwrap, speculative base64,
   gzip magic-sniff (ignore the hint), lz64 legacy, raw JSON. Bomb-resistant.
3. **Resolve.** `capture/event.rs` — untagged-union body; token, distinct_id,
   timestamp, and UUIDv7-from-event-time resolution per PostHog precedence.
4. **Durable.** `sink.rs` → `wal/mod.rs` — group-commit fsync; ack (2xx) only
   after the write is durable.
5. **Identity.** `identity/mod.rs` — runs after the ack decision; person state
   is rebuildable from the log, so it never fails an already-durable batch.
6. **Flush.** `flush.rs` — every 5s: seal WAL segment → write Parquet →
   delete segment → compact. Parquet durable *before* segment delete.

## Data flow — query ◐

DuckDB opens Parquet segments **read-only** as a query engine (never a live
`.duckdb` file — `stack.md`), uuid-deduped so counts are honest. Built:
stats, trend, top-events, funnel (CTE-chain, order-respecting), recent. The
dashboard (`dashboard/index.html`, embedded via `include_str!`) polls the
`/api/*` endpoints. Still ○: `LEAD IGNORE NULLS` / custom Rust funnel operator
for scale, retention/cohort views, a real React build via `rust-embed`+`ts-rs`,
and the query-semantics oracle (`decisions.md`).

## Data model

The canonical shapes. Parquet schema is wire-adjacent — additive only.

- **Event** (Parquet, `store/parquet.rs`): `uuid`, `event`, `distinct_id`,
  `token`, `timestamp` (µs UTC), `properties` (JSON). Segments partition by
  arrival; `uuid` is the dedup key (a replayed segment yields duplicates, never
  loss). Retention/TTL ✅ — `HOGLET_RETENTION_DAYS`, whole-file drop hourly.
- **Person** (SQLite, `identity/mod.rs`): `id`, `token`, `created_at`,
  `is_identified`, `properties` (JSON). `distinct_ids(token, distinct_id →
  person_id)` is the resolution map, unique per `(token, distinct_id)`.
- **Flag definitions** ✅ (`flags/`): rollout %, multivariate variants, and
  property conditions in SQLite. Evaluated per `distinct_id` with PostHog's
  exact SHA1 bucketing (variant string or bool), conditions matched against the
  request's person properties, returned in every `?v=` shape. Managed via the
  admin API. Still ○: named cohort objects (property conditions cover the
  common case).

## Module map

Each module owns one invariant. An edit that changes behaviour here must keep
the invariant or update this table.

| Module | Owns |
|---|---|
| `token.rs` | Token *shape* validity (empty/len/ascii/null/`phx_`) |
| `capture/decompress.rs` | Payload decode order + gzip-bomb ceiling |
| `capture/event.rs` | Body union + field resolution precedence |
| `capture/mod.rs` | Endpoint aliases, body limits, response codes |
| `sink.rs` | The `EventSink` durability contract |
| `wal/segment.rs` | Segment record format + replay-truncate recovery |
| `wal/mod.rs` | The durability boundary (ack-after-fsync) |
| `store/parquet.rs` | Parquet schema v1 (additive changes only) |
| `store/mod.rs` | Atomic file publish + compaction |
| `flush.rs` | WAL→Parquet ordering (duplicates never loss) |
| `identity/mod.rs` | One-person-per-distinct_id + merge precedence |
| `routes/config.rs` | The mandatory first-request config |
| `routes/flags.rs` | Flag response shapes by `?v=` |

Canonical example to copy when adding an endpoint: `routes/config.rs` — small,
typed, tested, cites its spec section.

## Invariants — do not break

1. **Ack only after fsync.** A 2xx means durable. Nothing acks before its WAL
   write syncs. (`wal/mod.rs`, claim 2)
2. **Flush order.** Parquet file durable before its WAL segment is deleted. A
   crash in between yields duplicates (uuid-deduped at query), never loss.
3. **One person per distinct_id.** A `(token, distinct_id)` maps to exactly one
   person at any time. Merges are deterministic and order-insensitive.
   (`identity/mod.rs`, claim 4)
4. **No unbounded work in the ingest path.** Every queue and allocation is
   bounded; overload sheds as 503, never grows without limit.
5. **Wire semantics byte-for-byte at the edge.** Proprietary formats live behind
   the edge, never at it. 4xx never retried, 5xx retried.
6. **One new database at a time.** DuckDB + SQLite are the only datastores.
   No second young/unproven engine. (`decisions.md`)
7. **Formats are versioned and forward-safe.** No release reads an old on-disk
   file as something else or drops it silently. See Format evolution.

## Explicit limits

| Limit | Value | Where |
|---|---|---|
| Browser body | 2 MB | `capture/mod.rs` |
| Batch body | 20 MB | `capture/mod.rs` |
| Decompressed ceiling | 64 MB | `capture/decompress.rs` |
| WAL record max | 64 MB | `wal/segment.rs` |
| WAL segment roll | 64 MB | `wal/segment.rs` |
| Queued-but-not-durable batches | 1024 | `wal/mod.rs` |
| Group-commit batches | 256 | `wal/mod.rs` |
| distinct_id length | 200 chars | `capture/event.rs` |
| Compaction files/run | 64 | `store/mod.rs` |

## Authority — who mutates what (running system)

A resource has exactly one writer. This is what makes SQLite's single-writer
model and the WAL's ordering safe without locks in the hot path.

| Resource | Sole mutator |
|---|---|
| WAL segments (append) | the WAL writer thread |
| Sealed segments (delete) | the flusher |
| Parquet files (merge) | the compactor |
| Identity DB (write) | `IdentityStore` behind one mutex |

## Format evolution ◐

Self-hosted means users have data on disk we must never betray. Every persistent
format carries a version and evolves forward-only:

- **WAL** — segment records are self-describing (len+CRC). A format bump adds a
  new record kind or a segment header; recovery of an older segment must never
  fail or misread. Corrupt/unknown tail truncates, never guesses.
- **Parquet schema** — additive columns only; readers tolerate missing columns
  with defaults. A breaking change is a new schema version written to new
  segments; the reader unions versions.
- **SQLite** — a `schema_version` row and forward-only migrations run at open.
- **Config/flags wire** — additive fields only; we never remove a field an SDK
  reads.

Upgrade contract: a newer binary opens an older data dir and runs. Downgrade is
not supported and must fail loudly, not corrupt.

## Security and tenancy ◐

- **Token authenticity** ✅ — `registry/` gives projects real identity. Open mode
  (zero projects) accepts any shape-valid token so a hobby install needs no
  setup; creating a project flips to closed mode where only registered tokens
  are accepted. Managed via the admin API (`routes/admin.rs`), guarded by
  `HOGLET_ADMIN_TOKEN` and disabled by default. Still ○: revocation, a UI.
- **Untrusted input.** The capture edge takes internet input: bounded bodies,
  bomb-resistant decode, no unbounded allocation. Per-token rate limiting ✅
  (fixed-window, 429 on the retry-safe contract, `HOGLET_MAX_EVENTS_PER_SEC`).
- **PII.** Event properties may contain personal data. GDPR erasure ✅: the
  admin `forget` endpoint physically removes a person's events from Parquet and
  their identity, synchronously and completely.
- **Network.** Binds where configured; TLS is expected to terminate at a
  reverse proxy for a `$5`-VPS deploy. No secrets logged.

## Operational contract ◐

A single operator runs this without a platform team.

- **Health/readiness** ✅ — `/health` (liveness) and `/ready` (gated until stores
  are open and the flusher is running). Closes the cold-start window.
- **Backup/restore** — the data dir is the whole state (WAL + Parquet +
  `identity.db` + `projects.db`). A file-level copy of a quiesced dir is a valid
  backup; document the quiesce.
- **Retention/TTL** ✅ — operator-set max age (`HOGLET_RETENTION_DAYS`); expired
  Parquet files dropped hourly, whole-file only (never drops a live event).
- **Upgrade** — drop-in binary swap; format evolution guarantees the old data
  dir opens. Downgrade unsupported.
- **Self-observability** ✅ — structured logs plus a Prometheus `/metrics`
  endpoint (events captured/acked, rejects, sink errors, uptime). This is *our*
  telemetry for operators, not the observability product we said we'd never
  build. RSS stays with the OS.

## Failure and degradation

What fails hard vs degrades. The ingest ack path is the one that must never lie.

| Condition | Behaviour |
|---|---|
| Disk full on WAL write | ack fails → 503 (retryable); never a false 2xx |
| WAL tail torn/corrupt | truncate at first bad record; prefix recovered |
| Parquet write fails at flush | segment kept, retried next tick; no loss, no ack affected |
| SQLite busy/locked | identity retried; event already durable — counts self-heal on replay |
| DuckDB query OOM/timeout ○ | query lane fails that query; ingest untouched |
| Flusher crashes | events stay in WAL; next start replays them |
| Overload (queue full) | 503 shed; SDK retries with backoff |

## Claims ↔ evidence

A green unit test proves its own assertion, not a claim. Claims are proven at
the boundary where someone experiences them (`claims.md`). Current status:

| Claim | Evidence | Status |
|---|---|---|
| 1 — stock SDKs work unchanged | real posthog-node latest → real binary, asserts persisted bytes | ✅ node passing; browser (Playwright) ◐ scaffolded |
| 2 — we do not lose events | SIGKILL the running binary, reconcile acked vs recovered | ✅ passing |
| 3 — one binary, `$5` VPS | measured RSS under concurrent query+ingest | ◐ dev-box: ~146 MB RSS, ~10k ev/s @64 conns (clears targets); 1 GB-box number still ○ |
| 4 — honest person counts | permute merge order, assert convergence | ✅ 6/6 permutations converge |

## Milestones

- **M1 — a real app works ✅.** Config, capture+decompression, WAL,
  Parquet+compactor, flags shapes, identity, SDK contract harness. 72 tests +
  SIGKILL + node contract test green.
- **M2 — usable product ✅ (~97%).** Query layer (stats/trend/top/funnel over
  DuckDB) with lane isolation and a Rust cross-check oracle; four-tab embedded
  dashboard; project/token registry + admin API; flags with variants +
  conditions (PostHog SHA1 bucketing); readiness; per-token rate limiting;
  retention; format versioning; GDPR erasure; `/metrics`; local load test.
  Remaining: a real React build (the inline dashboard works) and the published
  1 GB-box benchmark (needs the real hardware) — both noted below.
- **M3 — launch ○.** One-curl install, demo, measured benchmark, Show HN.

Scope ladder (`CLAUDE.md`): now = client+server events, identity, flags; later =
error tracking, AI-analytics dashboard, MCP; never = observability
traces/logs/metrics.

## How to extend

- New endpoint → copy `routes/config.rs`; cite the `compat-spec.md` section;
  keep response codes on contract.
- New hard component → name its oracle first, run property-based tests, and
  where an oracle exists, shadow-mode against it (`decisions.md` build method).
- New persistent format or field → additive by default; a breaking change gets a
  version bump and a reader that unions versions (Format evolution). Never a
  silent reinterpretation of existing data.
- New public promise → add it to `claims.md` with named evidence *before* it
  ships. If you can't say what distinguishes it from a plausible imitation, it's
  not understood well enough to claim.
