# Stack

One binary. Two lanes inside it — ingest always wins under contention.

## Core
- **Rust**, single static binary
- **tokio** runtime, **axum** HTTP

## Ingest — must never lose data
- **Hand-rolled WAL**: segment files, CRC, group-commit fsync, replay-truncate recovery. Ack only after WAL write.
- Bounded queues, reserved capacity. Its own lane; a dashboard query can never starve it.
- Vector's disk buffer is the design reference. Read it, never paste it (MPL-2.0).

## Events (OLAP)
- **Parquet** segments on disk, `object_store` for S3/R2 later
- **DuckDB** embedded, **read-only as a query engine over Parquet**. Never a live read-write `.duckdb` file — that hits the single-writer wall.
- **Arrow** in memory
- Funnels: CTE-chain (v1) → `LEAD ... IGNORE NULLS` → custom Rust operator (endgame)
- **Compaction job**: frequent flushes make many tiny segments; a background compactor merges them or query perf dies. Part of the storage design from day one.
- **DuckDB memory is capped** (`memory_limit` + spill-to-disk + query concurrency cap). A funnel query must never OOM the process — ingest lives here too.
- Binary size: DuckDB's static lib is tens of MB. Fine, but never publish a size claim without measuring.

## Persons / identity / flags / metadata (OLTP)
- **SQLite**, WAL mode. Concurrent readers, one writer; identity merges serialize through a single writer task with a bounded queue. Boring on purpose.

## Wire edge
- PostHog-compatible, byte-for-byte. Config, capture, flags/decide on aliased handlers.

## Dashboard
- **React + TypeScript**, embedded via `rust-embed`, served from axum
- **ts-rs**: Rust structs generate the TS types. One source of truth; a breaking change fails the build, not the browser.

## Correlation
- Accept W3C `traceparent`, store `trace_id` as an event property. Additive, not milestone-one. A hypothesis, not a differentiator.

## Build discipline
- Oracle + background property-based testing + shadow-mode, per hard component
- **turmoil** for WAL fault injection, **proptest** for identity convergence
- SDK contract tests vs real PostHog SDKs at `latest`, nightly

---

## Rejected

**topcoat** (tokio team). Server-rendered all-Rust framework — genuinely tempting, kills the ts-rs seam, same org as our runtime. Two reasons no. It's v0 with churning APIs, and we already spend our young-system budget on Turso + DuckDB; the presentation layer is the wrong place to spend more, we get zero moat from risk there. Decisive one: an analytics dashboard *is* charts, and the mature charting + interaction ecosystem (Observable Plot, uPlot, visx) is JS. Server-rendered-Rust charting is thin no matter how good the authors are. Reconsider for non-charting surfaces (marketing, docs, settings).

**Dioxus.** Best Rust-frontend option — mature, funded, React-like, so no v0 risk. Still no, same charting-ecosystem reason: WASM-to-DOM means JS interop exactly where the UI is hardest, plus WASM cold-start and a thin contributor pool vs React. Clear runner-up. The real test if we ever lean this way: build one funnel view and feel the interop friction.

**ClickHouse.** The heavy thing we're beating. Server, wants 16GB+, drags ZooKeeper/Kafka. Bundling it forfeits the moat. Where it genuinely wins — tuned funnels over billions of events — our answer is a custom Rust operator, not the server.

**QuestDB.** JVM server, separate service, blows the 1GB envelope. Great time-series ingest design — study it, never bundle.

**DataFusion.** A query-engine toolkit, not a database. Picking it means building a DB around it on top of building Hoglet. DuckDB is complete and reads our Parquet directly.

**Turso (vs SQLite).** Was the lean, killed by research (2026-07-20): its one advantage — MVCC concurrent writes — is flagged "not production ready, do not use for critical data" in Turso's own manual, corruption bugs still being fixed, on-disk format not frozen, 842 deps. Running it in safe mode gives SQLite semantics on an unproven engine — pointless. Revisit at: Turso 1.0 + MVCC stable + frozen format. Their DST/Antithesis testing is genuinely good; the timing is just wrong.
