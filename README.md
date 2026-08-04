# 🦔 Hoglet

**PostHog-compatible product analytics. One binary. One $5 server.**

Point your existing PostHog SDK at Hoglet — change `api_host`, nothing else — and
it just works. No ClickHouse, no Kafka, no Redis, no Zookeeper. One static Rust
binary that runs on a 1 vCPU / 1 GB box.

> Status: pre-release, built in the open. The ingest spine, query layer, data
> model, auth, and dashboards are built and tested; see
> [`spec/README.md`](spec/README.md) for exactly what's built (✅), partial (◐),
> and planned (○), phase by phase.

## Why

Product analytics today makes you choose: the depth of PostHog/Mixpanel with the
operational weight of a cluster, or the simplicity of Plausible without funnels
and identity. Hoglet is the missing corner — real product analytics (funnels,
retention, identity, flags) that runs as a single process you can operate
yourself. See [`why-hoglet.md`](why-hoglet.md).

## Quick start

```sh
cargo build --release
./target/release/hoglet          # listens on 0.0.0.0:8000
```

Point any PostHog SDK at it:

```js
posthog.init('phc_yourtoken', { api_host: 'http://localhost:8000' })
```

Open `http://localhost:8000/` for the live dashboard.

## What works

- **Capture** — every PostHog endpoint (`/e`, `/batch`, `/capture`, …), gzip/base64
  decompression, the full body union, PostHog's timestamp/identity resolution.
- **Durability** — write-ahead log with ack-after-fsync; a `kill -9` mid-stream
  loses zero acknowledged events (there's a test that proves it).
- **Identity** — `$identify` / `$create_alias` / `$merge_dangerously` with PostHog's
  merge precedence. No ghost profiles; merge order doesn't change the result.
- **Flags** — `/flags` and `/decide` with real evaluation, bucketed per user with
  PostHog's exact SHA1 hash (a 30% rollout selects the same users PostHog would).
- **Queries** — DuckDB over Parquet: stats, trends, top events, funnels.
- **Dashboard** — a live view served from the binary itself.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `HOGLET_ADDR` | `0.0.0.0:8000` | listen address |
| `HOGLET_DATA` | `hoglet-data` | data directory (WAL, Parquet, SQLite) |
| `HOGLET_RETENTION_DAYS` | none | drop events older than N days |
| `HOGLET_MAX_EVENTS_PER_SEC` | 10000 | per-token rate limit |
| `HOGLET_ADMIN_TOKEN` | none | enables the admin API (create projects/flags) |

## Architecture

One binary, two lanes: a hot ingest path (capture → WAL → Parquet) that must
never lose data, and a query lane (DuckDB, memory-capped) that can't starve it.
Persons/identity/flags live in SQLite. Full map in [`spec/README.md`](spec/README.md);
compatibility contract in [`spec/wire-compat.md`](spec/wire-compat.md).

## Testing

```sh
cargo test                       # unit + integration, incl. the SIGKILL crash test
cd contract-tests && npm test    # real PostHog SDKs against a real Hoglet
scripts/loadtest.sh              # throughput + RSS under concurrent load
```

## License

AGPL-3.0.
