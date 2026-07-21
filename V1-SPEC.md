# Hoglet — v1 Maturity Spec

The definition of a **real** v1: not a demo, not a vertical slice — a product a
team can self-host and actually run their analytics on, replacing a hosted tool.
This document is the bar. `SPEC.md` maps what is *built today* (a small early
slice); this maps what *done* means and everything between.

Status legend: ✅ built · ◐ partial · ○ not started. Today the honest headline
is **most of this is ○**. That's the point of writing it down.

---

## 0. What v1 is (definition of done)

A developer points a PostHog SDK at Hoglet, and a non-trivial team uses it daily:
they build funnels and retention on their own events, filter and break down by any
property, define cohorts, run feature flags with real targeting, save insights to
dashboards, share them, manage projects and API keys — all on one binary they
operate themselves, at a scale of tens of millions of events without the query
page timing out.

v1 is cleared when all of §2–§10 marked **[v1]** are ✅ and the claim tests in
`claims.md` pass on real hardware. Items marked **[post-v1]** are explicitly out.

---

## 1. The load-bearing decision: a query layer, not fixed queries

Every mature analytics product is built the same way: a **typed query model**
that compiles to SQL, and insight types are *templates* over it. Trends, funnels,
retention, breakdowns, cohorts, and person drill-down are all expressed as the
same query IR. Building fixed SQL per endpoint (what we have now) does not scale
to the capability surface and must be replaced.

**[v1] Hoglet Query Layer (HQL):**
- A query IR (Rust enum/struct tree) representing: series (event/action + math),
  filters (property groups), breakdowns, date range + interval, and the insight
  kind. Serializable — it is the saved-insight format and the dashboard tile
  format.
- A compiler from the IR to DuckDB SQL over the Parquet event store, with the
  property/session/person model below.
- One query endpoint (`POST /api/query`) that dispatches on `kind`, with sync and
  async (polled) execution and result caching.
- The independent Rust oracle (already built for basic queries) extends to every
  new query kind — each insight is cross-checked against a plain-Rust
  computation. Correctness is the moat; every query type earns an oracle test.

Build this first. The insight types in §4 become templates once it exists.

---

## 2. Wire compatibility (the drop-in contract) [v1]

Byte-for-byte at the edge. The full contract, beyond what `compat-spec.md`
already covers:

- **Init:** `GET /array/{token}/config` is the first call and gates the SDK; must
  return parseable JSON negotiating `supportedCompression`, `analytics.endpoint`,
  `hasFeatureFlags`, and every feature-off flag. ✅ (minimal today)
- **Capture:** aliased across `/e`, `/batch`, `/capture`, `/i/v0/e`, `/track`,
  `/engage`, `/identify`, `/alias`, `/groups`, `/s`. ✅ (core)
  - Browser **batched events carry `offset` (ms), not `timestamp`** — reconstruct
    wall-clock from receive-time/`sent_at`. ✅
  - Browser carries `token`/`distinct_id` **inside `properties`**; server SDKs at
    top level + `api_key` at batch level. ✅
  - gzip requests carry **no** `compression` param and `text/plain` — sniff magic
    bytes; base64 uses `data=` form body; sendBeacon forced to base64. ✅
- **Groups:** `$groupidentify` with `$group_type`/`$group_key`/`$group_set`;
  group properties stored and queryable. ○
- **Flags request/response:** `POST /flags/?v=2` accepting `person_properties`,
  `groups`, `group_properties`, `$anon_distinct_id`, `flag_keys`; response is the
  **v2 top-level `flags` object** with `enabled`, `variant`, `reason`,
  `metadata.payload` (**payloads are JSON-encoded strings**), plus
  `errorsWhileComputingFlags`, `requestId`, `quotaLimited`. ◐ (shapes + basic
  eval built; groups/reason/quota partial)
- **Local evaluation:** `GET /flags/definitions?token=…` returning full flag
  definitions for server-side SDK evaluation (personal `phx_` key required). ○
- **Retry contract (load-bearing):** 4xx never retried, 5xx/network retried
  (backoff 3000ms×2^n, cap 30min, 10 tries; network 3 tries); `beacon=1`→204;
  `quota_limited` in a 200 body self-limits the client. ✅
- **Adjacent endpoints returning empty-but-valid:** `GET /api/surveys/`,
  `GET /api/early_access_features/`, `POST /s/` (204). ○ (surveys/EAF stubs)
- **Exception capture:** `$exception` events with `$exception_list`/stack frames
  ingested as normal events (no rendering). ○

---

## 3. Data model maturity [v1]

The current model (uuid, event, distinct_id, token, timestamp, properties-JSON) is
the floor. v1 needs:

- **Properties as a queryable map** with a **distinct-values catalog** per
  (token, property) so the dashboard can autocomplete filter values and list
  known properties. Every competitor maintains this; without it the UI can't
  offer filters. ○
- **Sessions.** A `session_id` per event and a **session rollup** (start, end,
  duration, entry/exit page, bounce, event count) maintained incrementally —
  recomputed from Parquet or kept in SQLite with an idle-timeout (30 min) close.
  Sessionization is required for `unique_session` math, bounce rate, session
  duration, and paths. ○
- **Persons/profiles.** Beyond the identity graph we have: a person row with
  merged `properties`, first/last seen, and a **person query surface** (list,
  filter by person property, property-value distribution). `$set`/`$set_once`/
  `$unset` fold into person properties. ◐ (merge graph built; profile props +
  query surface ○)
- **Groups.** Group-type entities (org/company) with properties, for
  group-level flags and `unique_group` math. ○
- **Cookieless mode.** Salted-hash device/session id (rotating salt) for a
  no-consent-banner deployment — a headline demand. ○
- **Ingest enrichment.** GeoIP (country/region/city) and User-Agent parsing
  (browser/OS/device) at ingest, since these are the default breakdown
  dimensions. ○

---

## 4. Insight types [v1 unless noted]

Each is a template over the query layer (§1), cross-checked by a Rust oracle.

- **Trends [v1].** Time series with the full **math matrix**: total, unique
  users (DAU), WAU, MAU, unique sessions, first-time-for-user; property math
  (sum/avg/min/max/median/p75/p90/p95/p99); count-per-actor math; `unique_group`.
  **Formulas** across series (A/B, A+B). **Breakdowns** by event property, person
  property, session property, or cohort (single and multiple). **Intervals**
  hour/day/week/month. Relative + absolute date ranges. Display types
  (line/bar/area/pie/number/table/world-map). ◐ (fixed count/trend only today)
- **Funnels [v1].** Ordered / unordered / strict step order; **conversion
  window** (N minutes/hours/days); **breakdown** by property with attribution
  (first-touch/last-touch/step); step exclusions; viz sub-modes steps +
  time-to-convert; per-step drop-off. ◐ (basic ordered CTE-chain only)
- **Retention [v1].** Cohort × period grid; **recurring** vs **first-time**;
  Day/Week/Month periods; target event → returning event; per-cell actor
  drill-down. ○
- **Lifecycle [v1].** Per interval, classify actors as new / returning /
  resurrecting / dormant. ○
- **Stickiness [v1].** Distribution of "how many distinct intervals did an actor
  do X." ○
- **Paths [post-v1 for the graph; v1 for a basic next-event table].** Full
  Sankey user-journey is large; v1 ships a "top next events after X" table, the
  Sankey graph is post-v1. ○
- **SQL access [v1].** A direct query box: the user writes DuckDB SQL against
  their events (read-only, memory-capped, timeout). This is our "HogQL" — the
  escape hatch power users expect, and cheap since DuckDB already parses SQL. ○

**Actor drill-down [v1].** Every insight data point is clickable → the list of
persons/groups behind it (an actors query over the same IR). Without this the
numbers are dead ends. ○

---

## 5. Filtering & breakdowns [v1]

The capability everything else rides on.

- **Operators:** exact, is_not, icontains, not_icontains, regex, not_regex,
  gt/gte/lt/lte, is_set, is_not_set, in, not_in, between, date before/after/exact.
  (~20 core; semver/multi ops post-v1.) ○
- **Sources:** event property, person property, session property, cohort
  membership, group property. ○
- **Boolean groups:** two-level nested AND/OR property groups (`{type: AND|OR,
  values: [...]}`). ○
- **Breakdown = filter:** every breakdown value is clickable to become a filter
  (the drill loop). ○

Today: none of this exists — queries take a token and fixed params. This is the
single largest gap between "basic example" and "product."

---

## 6. Feature flags maturity [v1]

- **Targeting conditions [v1]:** release-condition groups, each `{properties:
  [...], rollout_percentage, variant?}`, OR'd. Properties = person property,
  cohort membership, or group property. ◐ (property conditions built; cohort +
  group conditions ○)
- **Rollout [v1]:** deterministic hash bucketing, monotonic. ✅
- **Multivariate [v1]:** 2–20 weighted variants summing to 100, consistent
  variant assignment. ✅
- **Payloads [v1]:** per-flag and per-variant JSON payloads, returned as
  JSON-encoded strings in `metadata.payload`. ○
- **Experience continuity [v1]:** sticky bucketing across `$identify` so a user
  doesn't flip variant on login (hash on a stable identifier). ○
- **Local-eval endpoint [v1]:** flag definitions for SDK-side evaluation. ○
- **Experiments [post-v1]:** A/B stats on multivariate flags. Out of v1.

---

## 7. Cohorts [v1 static + basic behavioral]

- **Static [v1]:** an explicit person list. ○
- **Behavioral [v1 subset]:** performed event (≥/≤/exactly N times in a window),
  did-not-perform, performed-first-time. Full sequence/regularly behavioral
  types are **post-v1**. ○
- Cohorts are usable as a filter source and a flag targeting condition (§5, §6). ○

---

## 8. Dashboards, saved insights, sharing [v1]

- **Saved insights [v1]:** persist a query IR with name/description; re-run with
  caching. ○
- **Dashboards [v1]:** a collection of insight tiles with layout and a
  dashboard-level date/filter override. ○
- **Sharing [v1]:** public read-only share link per insight/dashboard with an
  access token; optional expiry. ○
- **Real dashboard app [◐→v1]:** the React app exists ✅ but shows fixed views;
  v1 needs the insight *builder* (pick event, math, breakdown, filters), the
  dashboard grid, and the actor drill-down modal. The current four tabs are a
  starting shell, not the product.

---

## 9. Multi-tenancy, auth & accounts [v1]

Today the dashboard has **no authentication at all** and the API is open. That
alone disqualifies "usable product." v1 needs:

- **Users + password/session auth [v1]** for the dashboard. ○
- **Organizations → projects → members with roles [v1].** ○ (registry has
  projects; no users/roles)
- **API key management [v1]:** issue/revoke project write keys and personal
  (`phx_`) read keys, in the UI. ◐ (registry exists; no UI, no personal keys)
- **TLS guidance [v1]:** documented reverse-proxy termination; no secrets logged.
  ◐

---

## 10. Scale & performance [v1]

The current engine opens a fresh in-memory DuckDB and **rescans every Parquet
file on every request**. That is fine at 10² events and dead at 10⁷. v1 needs:

- **Partition pruning [v1]:** Parquet segments partitioned by day (and project);
  queries read only the date range they need, not the whole store. ○
- **A persistent/pooled DuckDB [v1]** with the Parquet files attached once, not
  re-globbed per query; rely on Parquet row-group statistics for predicate
  pushdown. ○
- **Rollup/aggregation tables [v1]:** pre-aggregated daily counts per
  (event, breakdown-dimension) and a DAU sketch, refreshed by the flusher, so the
  common trends query hits a rollup, not raw events. ○
- **Session rollup table [v1]** (§3). ○
- **Distinct-values catalog [v1]** (§3) so filter autocomplete is O(1), not a
  full scan. ○
- **Query result cache [v1]** keyed by IR + data version. ○
- **The published benchmark [v1]:** ingest ≥ target and a funnel over ≥10M events
  under the time budget in `SPEC.md`, measured on the 1 vCPU / 1 GB box, run
  concurrently with ingest. ○

Ingest is already solid (WAL, group-commit, crash-safe) — the scale gap is
entirely on the **query/storage-layout side**.

---

## 11. Explicitly out of v1 (post-v1)

Session replay ingestion+player, surveys UI, experiments/stats, data warehouse,
CDP, marketing analytics, revenue analytics, the full Sankey paths graph, funnel
correlation, LLM/AI-observability dashboards, cohort sequence/regularly behaviors,
multi-node replication. We store `trace_id` and link out; we never render spans.

---

## 12. Honest current state

Built and solid: the ingest spine (WAL, fsync, crash recovery), Parquet storage +
compaction, identity merge correctness, basic flag bucketing + variants +
property conditions, GDPR erasure, rate limiting, retention TTL, metrics, the SDK
contract test, and a React dashboard *shell*. That is real, well-tested plumbing.

Against this spec, that is roughly **10–15% of v1**, and it is the easy 10–15%.
The remaining bulk is: the query layer (§1), filtering/breakdowns (§5), the real
insight types (§4), sessions/persons/cohorts (§3, §7), auth/accounts (§9), and
scale (§10). None of it is blocked; all of it is the hard, valuable part.

---

## 13. Suggested build order to v1

1. **Query layer (§1)** + property model & distinct-values catalog (§3) — nothing
   else is real without these.
2. **Filtering & breakdowns (§5)** — turns fixed queries into a product.
3. **Trends full (§4)** on the new layer, oracle-checked.
4. **Sessions (§3)** → funnels-complete + paths-basic + retention + lifecycle +
   stickiness (§4).
5. **Persons/profiles + actor drill-down (§3, §4).**
6. **Auth & accounts (§9)** — required before anyone runs it for real.
7. **Cohorts (§7)** → wire into filters and flags (§5, §6).
8. **Flags-complete (§6)** — payloads, cohort/group conditions, continuity, local
   eval.
9. **Dashboards/saved insights/sharing + the insight builder UI (§8).**
10. **Scale: partitioning, rollups, caching, the published benchmark (§10).**

Each step ends with oracle/property tests and, where it touches the edge, an SDK
contract test. v1 ships when §0's definition holds and `claims.md` passes on real
hardware.
