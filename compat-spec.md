# PostHog Wire Contract — Implementation Spec

Reverse-engineered from PostHog's own open-source Rust services (`rust/capture`, `rust/feature-flags`) and `posthog-js` at HEAD, 2026-07-20. This is source-derived, not behavioural guesswork. Build against this.

## Endpoints

One capture handler aliased across all of these:

| Path | Body limit | Notes |
|---|---|---|
| `/e`, `/e/` | 2 MB | **what browser posthog-js actually posts to** |
| `/batch`, `/batch/` | 20 MB | what server SDKs post to |
| `/capture`, `/track`, `/engage`, `/i/v0/e` | 2 MB | aliases; `/engage` is person-props-only, synthesize into `$identify` |
| `/s` | 25 MB | session replay — out of scope for now |

Legacy endpoints (`/e`, `/capture`, `/engage`, `/track` — *not* `/batch` or `/i/v0/e`) get a second base64-unwrap pass after decompression.

Flags: `/flags` and `/decide` hit **one handler**; `?v=` selects response shape. Current posthog-js calls `/flags/?v=2`. `/decide` exists only for old SDKs.

Config: `GET /array/:token/config` — **fetched first, before anything else**. This one is mandatory.

## Request body

Untagged union — accept all four shapes:
- bare JSON array of events (browser → `/e`)
- `{token|api_key, batch: [...], historical_migration?, sent_at?}` (→ `/batch`)
- single bare event object
- engage shape (no `event` field) → synthesize `event: "$identify"`

Event fields: `token` (aliases `$token`, `api_key`), `distinct_id` (alias `$distinct_id`, arbitrary JSON value), `uuid`, `event` (required, non-empty), `properties`, `timestamp`, `offset`, `$set`, `$set_once`.

**Token resolution:** batch-level token wins on `/batch`. Otherwise every event must carry the same token, else 401. Falls back to `properties.token`. Reject tokens that are empty, >64 bytes, non-ASCII, contain a null byte, or start with `phx_` (that's a personal API key).

**distinct_id resolution:** top-level wins, then `properties.distinct_id`. Stringify numbers/arrays/objects. Null bytes → U+FFFD. Trim only for the emptiness check, keep the untrimmed value. Truncate to 200 *chars*, not bytes. Missing or null → 400.

**Timestamp:** `offset` (ms ago) wins outright → `now - offset`. Else parse `timestamp` (RFC3339, then looser fallbacks; normalize `+03` to `+03:00`). If both `sent_at` (query `_` or batch field) and timestamp exist, correct clock skew: `skew = sent_at - now`, `timestamp -= skew`. Skip if `$ignore_sent_at` is true. Clamp >23h future to now. No timestamp → server now.

**UUID:** if client omits it, generate **UUIDv7 seeded from the resolved event timestamp**, not ingestion time.

Drop `$performance_event` silently. If filtering empties the batch, still return 200 — never make clients retry.

## Decompression

Priority order matters. Do not trust the `compression` hint — PostHog's own comment says a sizable portion of requests have it missing or wrong.

1. Peek for gzip magic bytes → gzip decompress regardless of what the hint says
2. Else hint says `lz64`/`lz-string` → `lz_str::decompress_from_base64`, returns `Vec<u16>`, then UTF-16 → UTF-8
3. Else raw UTF-8 JSON

Before all that: if the whole payload looks like strict base64 (correct alphabet, length % 4 == 0), speculatively decode it first.

`Content-Type: application/x-www-form-urlencoded` → parse body as `data=<json>&compression=<hint>`. Watch the `+`→space urldecode quirk; restore it if the field looks base64. This is the `sendBeacon` path.

Gzip decode must be bomb-resistant: chunked reads with a hard decompressed-size ceiling, fail before allocating.

**Current posthog-js only sends gzip or base64.** lz64 is legacy-only. When it uses gzip it *removes* the `compression` query param entirely and relies on content sniffing.

## Config response (`/array/:token/config`)

Minimum viable, modelled on PostHog's own server-side fallback:

```json
{
  "token": "...",
  "supportedCompression": ["gzip", "gzip-js"],
  "hasFeatureFlags": true,
  "sessionRecording": false,
  "surveys": false,
  "heatmaps": false,
  "capturePerformance": false,
  "autocaptureExceptions": false,
  "isAuthenticated": false,
  "toolbarParams": {},
  "analytics": {"endpoint": "/i/v0/e/"},
  "defaultIdentifiedOnly": true,
  "siteApps": [],
  "config": {"enable_collect_everything": true}
}
```

If `hasFeatureFlags` isn't false, the SDK then calls `/flags/?v=2`. If *that* fails the SDK degrades gracefully — so config is the hard requirement, flags is best-effort.

## Flags response shapes

| `?v=` | Shape |
|---|---|
| unset | `{feature_flags: {key: bool\|string}, feature_flag_payloads: {key: json}}` |
| `1` | `{feature_flags: [keys]}` |
| `2` | `{feature_flags: {key: bool\|string}}` |
| `/flags` default | `{flags: {key: FlagDetails}}` ← current |

`FlagDetails` is camelCase: `{key, enabled, variant|null, reason: {code, condition_index, description}, metadata: {id, version, description, payload, hasExperiment}}`.

All shapes flatten a `config` object and carry `request_id`, `evaluated_at`, `errors_while_computing_flags`, optional `quota_limited`.

Flags request body is parsed as **json5** — NaN/Infinity must map to null, and tolerate lossy UTF-8 (Android clients send malformed sequences).

Bucketing: `sha1("{salt}:{hash_id}")`, consistent per user.

## Identity

`$identify` fires only on the anonymous → identified transition, carrying `$anon_distinct_id` as an **event property**.

Dispatch:
- `$create_alias` / `$merge_dangerously` with `properties.alias` → `merge(alias, current_distinct_id)`
- `$identify` with `$anon_distinct_id` → `merge($anon_distinct_id, current_distinct_id)`

Rules:
- `$identify` and `$create_alias` **refuse to merge if the other person is already identified** — log a warning, accept the event, don't error. Only `$merge_dangerously` overrides this.
- Property precedence: `{...loser, ...winner}` — the person keyed by the event's own `distinct_id` wins conflicts.
- Surviving `created_at` = oldest of the two.
- Self-merge is a silent no-op.

Get the winner backwards and real SDK tests will catch it. This is behavioural, not schema.

## Responses

- 200 `{"status":1}` normally
- 204 empty when `beacon=1`
- 400 malformed / missing event name / missing distinct_id / bad timestamp
- 401 no token, mismatched tokens, malformed token
- 413 too large · 429 rate/billing limited · 408 client stalled · 503 retryable sink failure · 500 otherwise

**posthog-js retries 5xx and network errors, never 4xx.** Backoff 3000ms × 2^n, capped 30 min, ±50% jitter, 10 retries max — but only 3 for network-level failure (status 0). Billing limits and empty-after-filter deliberately return 200 to avoid retry storms.

CORS: be maximally permissive, mirror whatever the client sent, allow credentials. Old SDKs and reverse proxies send funky headers.

## Query params

- `_` — sent_at, also cache buster. Always present.
- `ver` / `lib_version` — SDK version. Capture ignores it. Not sent to `/e/` or `/s/`.
- `v` — flags response shape
- `beacon=1` — return 204
- `compression` — hint only, never trust it

Client IP comes from the connection / `X-Forwarded-For`, not a query param. Redact to 127.0.0.1 when `properties.capture_internal` is set.

## Prior art

[`sidequery/hogflare`](https://github.com/sidequery/hogflare) — Rust, PostHog-compatible, actively maintained, targets Cloudflare Workers + R2 Iceberg. Read its `docs/posthog-compatibility.md`. Explicitly skips cohorts and event-based flag filters.
