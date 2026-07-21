# Hoglet — System Spec

The map of the system. High-level on purpose: it routes to the doc that owns
each detail rather than repeating it, names the invariants a change must not
break, and matches every public claim to the evidence that proves it.

Read this first. Then load only the deeper doc your task needs.

## What it is

A drop-in PostHog-compatible product-analytics backend in one Rust binary.
Stock PostHog SDKs point at it and work unchanged. One static binary, one
`$5` server, no Kafka/ClickHouse/Redis/Zookeeper.

Strategy and market: `why-hoglet.md`, `moats.md`, `industry-map.md`.
Rules and scope: `CLAUDE.md`. Rationale for every locked choice:
`decisions.md`.

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

- **Ingest lane** — hot, must never lose data. Reserved capacity, bounded
  queues. Ingest wins under contention.
- **Query lane** — heavy CPU, bursty. Capped concurrency and memory. Not
  built yet (M2).

```
SDK ─HTTP─▶ capture ─▶ WAL (fsync) ─ack▶ 2xx
                          │                └─▶ identity (async) ─▶ SQLite
                          ▼ 5s flush
                       Parquet segments ◀─ compactor
                          ▲
                       DuckDB (read-only)  ─▶ query API ─▶ dashboard   [M2]
```

## Data flow — ingest (built)

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

## Data flow — query (M2, not built)

DuckDB opens Parquet segments **read-only** as a query engine (never a live
`.duckdb` file — `stack.md`). Funnels: CTE-chain → `LEAD IGNORE NULLS` →
custom Rust operator. Dashboard: React + TS embedded via `rust-embed`,
`ts-rs` across the seam.

## Module map

Each module owns one invariant. An edit that changes behaviour here must keep
the invariant or update this table.

| Module | Owns |
|---|---|
| `token.rs` | Token validity (empty/len/ascii/null/`phx_`) |
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
5. **Wire semantics byte-for-byte at the edge.** Proprietary formats live
   behind the edge, never at it. 4xx never retried, 5xx retried.
6. **One new database at a time.** DuckDB + SQLite are the only datastores.
   No second young/unproven engine. (`decisions.md`)

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

## Claims ↔ evidence

A green unit test proves its own assertion, not a claim. Claims are proven at
the boundary where someone experiences them (`claims.md`). Current status:

| Claim | Evidence | Status |
|---|---|---|
| 1 — stock SDKs work unchanged | real posthog-node latest → real binary, asserts persisted bytes | ✅ node passing; browser (Playwright) scaffolded |
| 2 — we do not lose events | SIGKILL the running binary, reconcile acked vs recovered | ✅ passing |
| 3 — one binary, `$5` VPS | measured RSS under concurrent query+ingest on 1 GB box | ⬜ not measured — no number published until it is |
| 4 — honest person counts | permute merge order, assert convergence | ✅ 6/6 permutations converge |

## Milestones

- **M1 — a real app works (done).** Config, capture+decompression, WAL,
  Parquet+compactor, flags, identity, SDK contract harness. 72 tests + SIGKILL
  + node contract test green.
- **M2 — usable product.** Query layer (trends/funnels/retention over DuckDB),
  React dashboard embedded in the binary, the claim-3 load test.
- **M3 — launch.** One-curl install, demo, measured benchmark, Show HN.

Scope ladder (`CLAUDE.md`): now = client+server events, identity, flags; later
= error tracking, AI-analytics dashboard, MCP; never = observability
traces/logs/metrics.

## How to extend

- New endpoint → copy `routes/config.rs`; cite the `compat-spec.md` section;
  keep response codes on contract.
- New hard component → name its oracle first, run property-based tests, and
  where an oracle exists, shadow-mode against it (`decisions.md` build method).
- New public promise → add it to `claims.md` with named evidence *before* it
  ships. If you can't say what distinguishes it from a plausible imitation,
  it's not understood well enough to claim.
