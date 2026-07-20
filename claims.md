# Claims and Evidence

What Hoglet promises, and what would prove it. Written before implementation, on purpose — this determines what we build, not just what we test.

Rule: a green check proves only its own assertion. Unit tests, type checks, and builds establish internal properties. None of them prove a claim below. Evidence must be gathered at the boundary where someone actually experiences the outcome.

Nothing here is proven until its evidence exists. Until then, do not make the claim in public.

---

## Claim 1 — Stock PostHog SDKs work unchanged

**Who experiences it:** a developer who changes `api_host` to point at Hoglet and touches nothing else.

**Starting state:** an app already instrumented with an unmodified PostHog SDK at its published latest version.

**Must happen:** the SDK initializes without error, events arrive with distinct_id, properties, and timestamps intact, `identify()` links the anonymous history to the identified person, and the SDK never enters a retry loop.

**Invariants:** we never ask the developer to change SDK code, pin a version, or set a Hoglet-specific option. Response codes stay on PostHog's contract — 4xx never retried, 5xx retried.

**Evidence that proves it:** real posthog-js running in a real browser under Playwright, against a real Hoglet process, asserting on what arrived in storage. Run nightly against SDK `latest`, not a pinned version — the claim is about *their* current SDK, so pinning would prove the wrong thing.

**Distinguishing success from a plausible imitation:** a mock server that returns 200 to everything also passes a naive check. The test must assert on stored events and on SDK-observable behavior — flags resolving, no retry queue growth, config fetch succeeding — not merely that a request was accepted.

**Outside the claim:** session replay, surveys, experiments, warehouse. Not implemented, not claimed.

---

## Claim 2 — We do not lose events

**Who experiences it:** the operator whose dashboard numbers have to be defensible.

**Starting state:** events in flight, some in memory, some in the WAL, some not yet flushed to Parquet.

**Must happen:** every event acknowledged with a 2xx is recoverable after a crash. Events not yet acknowledged may be lost — that's the contract, and the SDK retries them.

**Invariants:** acknowledgement happens only after the WAL write is durable, never before. Recovery truncates at the first checksum failure and loses nothing before it. A corrupted tail never silently becomes valid data.

**Evidence that proves it:** deterministic fault injection under turmoil — kill mid-write, tear a write, fill the disk, partition storage — then replay and reconcile acknowledged events against recovered events. Seeded so any failure is reproducible. Plus property tests over batch shapes.

**Distinguishing success from a plausible imitation:** a WAL that fsyncs after acknowledging passes every happy-path test and fails only under crash. The test must kill the process at adversarial points, not at convenient ones.

**Outside the claim:** events an ad-blocker prevented from ever reaching us. Different problem, tracked separately.

---

## Claim 3 — One binary, small enough for a $5 VPS

**Who experiences it:** someone who wants analytics without operating infrastructure.

**Starting state:** a single-core box with 1 GB RAM and a fresh install.

**Must happen:** one executable, no sidecar services, no container orchestration, no external database. It starts, serves, and survives normal traffic within that envelope.

**Invariants:** no Kafka, no ClickHouse, no Redis, no Zookeeper, no separate worker process. Memory does not grow unbounded under a traffic spike — ingest allocations are explicitly bounded.

**Evidence that proves it:** measured RSS under sustained load on a box with that spec, using oha or k6, reported as a real number. A dependency assertion in CI that fails if a forbidden service creeps into the deployment. A load test that spikes hard and shows memory returning to baseline. The load test must run a heavy analytical query (funnel over the full dataset) *concurrently* with sustained ingest on the 1 GB box — DuckDB's memory appetite under a capped `memory_limit` is the biggest untested assumption in this claim, and sequential tests would hide it.

**Distinguishing success from a plausible imitation:** "it compiled to one binary" proves nothing about runtime footprint. The number that matters is RSS under load, measured, not idle memory at startup.

**Outside the claim:** any specific throughput figure. We do not publish an events-per-second number until we have measured one on stated hardware.

---

## Claim 4 — Your person counts are honest

**Who experiences it:** anyone who has been burned by ghost profiles inflating their user count.

**Starting state:** a user browsing anonymously across sessions and devices, then logging in.

**Must happen:** anonymous history stitches to the identified person exactly once. No duplicate persons. Merge order does not change the outcome.

**Invariants:** merges are deterministic and order-insensitive. A distinct_id maps to exactly one person at any point in time. The distinct_id-to-person ratio is inspectable, not hidden.

**Evidence that proves it:** property tests generating identify/alias sequences in every order and asserting the same final person graph. A reconciliation check that counts persons against distinct_ids. Adversarial cases — shared device, cleared cookies, identify called before and after, concurrent identifies on the same person.

**Distinguishing success from a plausible imitation:** merging correctly in the common ordering is easy; PostHog does that and still produces ghosts. The test must permute the order and assert convergence.

**Outside the claim:** cross-device stitching without a login. Not solvable, not claimed.

---

## Adding a claim

Any new public promise gets an entry here before it ships, with its evidence named. If we cannot say what would distinguish it from a plausible imitation, we do not yet understand the claim well enough to make it.
