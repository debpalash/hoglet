# Moats

## Real

**PostHog can't follow us down-market.** Their product assumes ClickHouse, Kafka, Redis, Postgres. Matching a single binary means rewriting storage and carrying both stacks for years, with no upside — their money is in cloud, where operational weight is invisible to the customer. Mixpanel and Amplitude are worse off: no self-hosted story at all.

We lose this if PostHog ships a genuinely light single-node mode. We widen it by keeping the deployment rules in `CLAUDE.md` absolute.

## Compounding

**Correctness reputation.** Nobody switches for speed. They switch when their numbers stop being believable — ghost profiles are the most repeated complaint in the category. TigerBeetle's playbook: win on trust, built from an absence of incidents. Lost by one silent data-loss bug.

**SDK contract harness.** Real PostHog SDKs at `latest`, nightly, against real Hoglet. Expensive to rebuild, gets more so as their surface grows. Lost the moment we pin versions to keep CI green.

## Not moats

Rust. Benchmarks. Being open source. All commodity.

**Wire compatibility is a wedge, not a moat** — zero switching cost in means zero out, and anyone can implement a wire contract. Right trade, cheap front door for an open back door. Just don't call it defensibility.

**Trace correlation** is a hypothesis. No user has asked for it by name. Cheap and additive is fine; load-bearing is not.

## Build order

The deployment constraint and the correctness claims *are* the moat. Features get us looked at; those get us kept.
