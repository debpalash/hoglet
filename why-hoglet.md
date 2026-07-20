# Why Hoglet

**Hoglet is a drop-in, PostHog-compatible product-analytics backend written in Rust — one binary, megabytes of RAM, your hardware, your data.** You keep the PostHog SDKs, wire format, and ecosystem. You replace the backend with something radically lighter, faster, and boring to operate.

This document is grounded in a deep sweep of real user complaints — GitHub issues with verbatim quotes, Hacker News threads, public post-mortems, review aggregations, pricing intelligence, and migration stories — across PostHog **and** the entire multi-product analytics industry (Mixpanel, Amplitude, FullStory, LogRocket, Hotjar, Heap, Pendo, LaunchDarkly, GA4, Segment). The pattern that emerges is bigger than any one vendor: **the multi-product analytics suite, as a category, systematically fails its users in the same six ways.** Hoglet is designed against those failure modes, not just against PostHog.

---

## Part 1 — What real users say about PostHog

### 1.1 Billing that punishes you for using the product

- An HN user: *"I did some napkin math, turned on their product, and ran up a $10,000+ bill in my first half day."* Billing limits existed but were **hidden and off by default**: *"Not only do they not have reasonable billing limits or triggers in place, but these are hidden... I'm convinced this is a dark pattern for their sales funnel."* They migrated away for cost predictability. ([HN thread](https://news.ycombinator.com/item?id=39983612))
- Identified events cost **up to 4x** anonymous ones via "person profiles" — a separate billing meter most teams discover only on the invoice. Users report being charged for person profiles *even after* disabling `identify()`. PostHog maintains a **standing, formalized refund workflow** specifically for person-profile over-billing (claim within 30 days, first occurrence only) — a refund policy that exists because the surprise is routine. ([refund policy](https://posthog.com/handbook/growth/sales/refunds), [community thread](https://posthog.com/questions/why-am-i-still-getting-charged-for-person-profiles-even-if-i-changed-my-config-and-stopped-identifying-users))
- PostHog's 2024 *"We've decided to make less money"* price cuts (replay: $430/mo → $85/mo for 25k recordings) are themselves evidence of how much billing pain users were expressing. ([pricing post](https://posthog.com/blog/session-replay-pricing))

### 1.2 Ghost profiles: the numbers can't be trusted

- Identity merging is unreliable enough that a GitHub issue requesting basic duplicate-person cleanup has been **open since August 2020**. ([#1350](https://github.com/PostHog/posthog/issues/1350))
- Merges silently fail when `identify()` is called in the "wrong" order — a PostHog engineer's response: *"This is an implementation error... identify shouldn't be used this way"* — yet it recurs across years of tickets. The existence of a `$merge_dangerously` API is an admission the default merge logic doesn't always work. ([#23690](https://github.com/PostHog/posthog/issues/23690))
- Independent analysis of the resulting "ghost profiles": a dashboard reporting **50,000 users may represent ~35,000 real people**. Consequences: broken funnels, unreliable retention, polluted cohorts, noisy experiment results — and inflated billing, since each duplicate is a billable person profile. ([DEV analysis](https://dev.to/red_bean_37803fd04e673991/you-dont-have-50000-users-how-ghost-profiles-pollute-your-posthog-data-4bha))

### 1.3 Ad-blockers silently eat your data — and the workaround defeats itself

- Even behind PostHog's recommended reverse-proxy setup, uBlock Origin still blocks capture because the asset is literally named **`posthog-recorder.js`**. User quote: *"You come up with a whole mechanism to workaround AdBlockers around the name of the domain but you go and name the file `posthog-recorder.js`? That's a giveaway."* A PostHog engineer conceded there's no complete fix. ([posthog-js #2866](https://github.com/PostHog/posthog-js/issues/2866))
- Related: infinite retry loops when blocked ([#1598](https://github.com/PostHog/posthog-js/issues/1598)), web-vitals data blocked ([#26206](https://github.com/PostHog/posthog/issues/26206)).

### 1.4 The SDK is eating your bundle

- Documented growth trajectory in an issue titled *"This library is large"* (open since ~2020): 16 KB → 30 KB → 38 KB → by 2025, *"the single largest dependency in my Next.js website bundle... larger than react-dom itself (168kb vs 142kb)."* A PostHog engineer admitted ~40% of the unminified library is class-field identifiers that minifiers structurally cannot compress. ([posthog-js #65](https://github.com/PostHog/posthog-js/issues/65))
- Real-world: one team spent **5 days** hunting a performance regression that turned out to be PostHog, and removed it entirely — *"the performance tax was not worth it."* ([#1514](https://github.com/PostHog/posthog-js/issues/1514), [#1905](https://github.com/PostHog/posthog-js/issues/1905))

### 1.5 Reliability: 8 public post-mortems in ~5 months

From PostHog's own [post-mortems repo](https://github.com/PostHog/post-mortems) (Sept 2025 – Feb 2026):

- **Feature flags failed three separate times** (Sept 29, Oct 21, Feb 6). The Sept 29 incident: **78% of US flag-evaluation requests failed** with 504s for 1h48m — no circuit breakers, Kubernetes routing traffic to crash-looping pods for 45 minutes, hardcoded config blocking fast rollback. Flags sit in customers' live request paths; this breaks *their* products, not just their dashboards. ([post-mortem](https://posthog.com/handbook/company/post-mortems/2025-09-29-flags-is-down))
- **Permanent data loss**: the Logs product lost all data older than 3 days in the US region (Feb 2026). ([post-mortem](https://posthog.com/handbook/company/post-mortems/2026-02-20-posthog-us-logs-data-loss))
- Ingestion delays of up to **13 hours** (Nov 2025), plus a supply-chain attack (Shai Hulud, Nov 2025), replay SDK incident, surveys SDK bug. ([incident](https://isdown.app/status/posthog/incidents/474178-data-processing-delays-events-and-persons-ingestion))
- And there's no in-product ingestion-lag indicator — during delays, dashboards silently show stale numbers. ([#17273](https://github.com/PostHog/posthog/issues/17273))

### 1.6 Self-hosting: officially abandoned at scale

- PostHog discontinued Kubernetes support (2023): *"hosting at scale is complex, with issues cropping up in every part of the stack — event ingestion, Kafka, ClickHouse, Postgres, Redis and the application itself... often the issue is something a couple of layers deep and very hard to debug."* Their words, not ours.
- Third-party guidance: without a dedicated DevOps function, *"self-hosting PostHog is probably not the right call"*; expect *"significant operational overhead during the first six months"* and an estimated **$3,000–8,000/month** in infra/ops cost. ([guide](https://cotera.co/articles/posthog-self-hosted-guide), [12-hours-debugging post](https://dev.to/ismailmirza/i-spent-12-hours-debugging-posthog-self-hosting-so-you-dont-have-to-pb0))

### 1.7 Jack of all trades, shallow where it counts

- Experimentation lacks CUPED, sequential testing, SRM checks vs. specialists like Statsig; the data warehouse is admitted by PostHog's own docs to be *"not quite ready for data engineers"* and *"not quite ready for data analysts."* ([analysis](https://www.definite.app/blog/posthog-data-warehouse))
- HogQL becomes a hidden labor tax: an estimated **10–20 hours of analyst time per month** reconciling queries for non-technical teammates.

### 1.8 Sovereignty: EU hosting doesn't solve the US problem

- PostHog's founder, on HN, verbatim: *"we cannot guarantee not sharing data in the scenario that the US government forces us to transfer data to them from our EU Cloud."* CLOUD Act exposure applies to the Frankfurt region because PostHog Inc. is a US entity. ([HN](https://news.ycombinator.com/item?id=33163819))

---

## Part 2 — This is the whole category, not one company

The same sweep covered Mixpanel, Amplitude, FullStory, LogRocket, Hotjar, Heap, Pendo, LaunchDarkly, GA4, and Segment. Six failure modes recur across **every** multi-product analytics suite:

### 2.1 Pricing as a success tax

You pay the most at exactly the moment your product starts working — and you can't predict it.

- **Amplitude's own founder** publicly admitted pricing has been *"the top cited issue for adopting Product Analytics generally and Amplitude specifically"* **for five years running**. Median real contract: ~$64K/year (Vendr, 410 purchases); contracts include automatic 8% renewal increases; overage billed 20–50% above contracted rates.
- **Mixpanel**: HN user found upgrading from free to paid *dropped* their event allowance from 20M to 10K — the equivalent paid volume was $2,289/month. *"The most obscure pricing model of any SaaS I've ever tried to integrate with."*
- **FullStory**: median customer pays ~$28K/year; a grandfathered customer: *"despite our best effort to try to pay them more, they don't let us. Our only option is to triple our cost even though we are only barely over."*
- **LogRocket**: *"pricing increased by more than 2x on contract renewal"*; a 327% jump between tiers. **LaunchDarkly**: 50K MAU B2C products quoted **$100K–150K/year** for feature flags. **Pendo**: *"pricing mainly depends on what the customer is willing to pay"*; one user quoted **$30K/year for webhook access**. **Segment**: MTU pricing makes anonymous-heavy B2C traffic *"extremely pricey"*; $25K–$200K/year range.
- Every single one of these vendors hides pricing behind a sales process. Negotiation guides openly advise citing competitors to extract 15–35% discounts — list price is an anchor, not a price.

### 2.2 The free-tier cliff

Free tiers are land-grabs with a cliff: Amplitude's founder described the free plan as a decades-horizon land-grab strategy; startup programs (1 year free) graduate companies straight into $50K/year quotes — *"nobody wants to go from getting something for free to suddenly paying $50,000 a year."* Mixpanel has repeatedly rewritten its free-tier terms after signup.

### 2.3 Data you stop trusting

- **Ghost profiles** (PostHog, §1.2) have cousins everywhere: Mixpanel event counts off *"by as much as 70% or more"* vs. GA due to differing report semantics; GA4's sampling and thresholding — *"for teams making precise budget decisions, 'mostly right' isn't good enough."*
- **Taxonomy rot**: a real Mixpanel project had 47 events named things like `test_event_2`, `DEBUG_purchase`, `new_checkout_FINAL_v2` — *"their analysts had stopped trusting the event list entirely."* Mixpanel's Event Approval feature (built to prevent this) **auto-disables once a project accumulates 3,000 unreviewed events** — the guardrail breaks exactly when you need it.
- **Silent disappearance**: Mixpanel hard-deletes events after 5 years regardless of plan; users report months of data vanishing with support unable to explain why.
- **Breach**: Nov 2025, Mixpanel was breached via smishing; data tied to ~8,000 corporate customers exported; **OpenAI terminated Mixpanel from production** over it; disclosure took 16 days for some customers.

### 2.4 The performance tax on *your* product

Session-replay and analytics SDKs weigh 150–553 KB of JavaScript across the category (Hotjar alone: 150–250 KB compressed), with measured 20–25% CPU increases. This is the vendor externalizing their cost onto your users' page loads and your Core Web Vitals.

### 2.5 Multi-product creep produces shallow bolt-ons

Amplitude's replay can't capture canvas/WebGL/cross-origin iframes; its flags were dismissed by specialists; its experimentation is "historically secondary." Pendo's funnels are "less flexible than dedicated tools." LaunchDarkly's A/B testing "lacks the statistical rigor" and forces separate analytics tools. Users buy the suite to consolidate, then end up buying the specialist anyway — now paying twice.

### 2.6 Consolidation churn: the tools people love get absorbed

- **June.so** (beloved indie product analytics): acquired by Amplitude, shut down with **30 days'** notice to export data.
- **Statsig**: bought by OpenAI for $1.1B, then handed to Amplitude 7 months later *without its engineering team* — Optimizely's CEO: *"a race car without a driver."*
- The pattern: best-of-breed tools users chose *specifically to escape the suites* get folded into the suites. Closed-source SaaS gives users no exit. **Open source is the only structural defense** — a project can be forked; an acquired SaaS just dies.

### 2.7 The market is already voting with its feet

- **GA4 lost 23% of active domains in four months** (3.69M → 2.84M, Mar–Jul 2025); ~3M domains have abandoned it; 75% of surveyed SEO professionals are dissatisfied. EU regulators in **seven countries** have ruled Google Analytics unlawful under GDPR.
- **RudderStack vs. Segment is the proof of hoglet's playbook**: RudderStack won Segment's users by being **API-compatible** — *"API compatible with Segment, which makes migrating your event instrumentation easy... automatically grabs the Segment anonymousId... so you won't have any data inconsistency."* Wire compatibility + transparent pricing + self-hosting beat the incumbent's ecosystem moat. That is exactly the strategy hoglet runs against PostHog.

---

## Part 3 — What switchers actually want (the demand side)

Mining "what should I use instead" threads and switcher testimonials produces a remarkably consistent ranked list:

1. **Privacy without ceremony** — cookieless, no consent banner. One switcher saw traffic numbers *increase 14% immediately* after moving to Plausible, because consent-rejecting visitors became visible.
2. **A tiny script** — Plausible <1 KB vs GA4's ~135 KB is cited constantly as a headline reason to switch.
3. **Predictable cost** — *"you're paying for servers, not events."* The single most-repeated phrase pattern in switcher threads.
4. **Setup in minutes, no developer required** — one line of code, running today.
5. **Data sovereignty** — self-hosting as first-class, not a deprecated afterthought.
6. **GDPR compliance without legal risk** — especially post-GA rulings in the EU.

**And the unfilled gap:** the lightweight tools that deliver 1–6 stop at pageviews. *"Plausible only tracks pageviews, goals, and UTM campaigns"* — no funnels, no retention cohorts, no user-level journeys. Users are forced into a binary: **simplicity without depth** (Plausible, Umami, Fathom) or **depth without simplicity** (PostHog, Mixpanel, Amplitude). Multiple new entrants are converging on this same gap — evidence it's real demand, and still unclaimed at the level hoglet targets: full product-analytics depth, single binary, PostHog's entire SDK ecosystem inherited for free.

---

## Part 4 — Hoglet's design principles, mapped to the evidence

| What users suffer | Hoglet's answer |
|---|---|
| Bill shock, success tax, hidden meters (§1.1, §2.1) | Open source on your hardware. The marginal cost of an event is disk and CPU — and the backend is 10–100x lighter, so that cost is trivial. No meters. No sales call. Capture everything. |
| Ghost profiles, broken merges (§1.2, §2.3) | Identity resolution as a first-class engineering problem: deterministic, order-insensitive merging; an honest distinct-id-to-person audit view; counts you can defend. |
| Ad-blocker data loss (§1.3) | First-party by default: unbranded asset names, same-domain ingest, proxy-trivial deployment. Never ship a file named `*-recorder.js` and call it a workaround. |
| SDK bundle bloat (§1.4, §2.4) | Day one: stock PostHog SDKs work unchanged (drop-in). Roadmap: a Rust-core SDK (native + WASM), kilobytes not hundreds of kilobytes, batching/retry in one audited core with thin idiomatic wrappers. |
| Flag outages breaking live products (§1.5) | Flags architecturally isolated from analytics load — in-process, sub-millisecond evaluation; an analytics query can never take down a flag decision. Edge evaluation on the roadmap. |
| Stale dashboards with no warning (§1.5) | Ingestion lag is a first-class, always-visible metric in the UI. If numbers are behind, the product says so. |
| Self-hosting abandoned / 8–32 GB stacks (§1.6) | One static binary. Embedded columnar storage (Parquet + DuckDB-class engine), local WAL instead of Kafka, no Redis/Zookeeper/plugin-server. `hoglet serve` is the deployment. Runs on a $5 VPS — or a laptop (`hoglet dev`: see your events, funnels, and flags live while developing). |
| Shallow bolt-on products (§1.7, §2.5) | Honest scope: the 20% of the suite that 95% of users touch — events, trends, funnels, retention, flags — done exceptionally well. No ninth product in the nav bar. |
| US-jurisdiction exposure even on EU cloud (§1.8) | Your server, your jurisdiction. There is no vendor to subpoena. |
| Taxonomy graveyards (§2.3) | Schema governance that doesn't self-destruct: event registry with review flow designed for the messy 3,000-event reality, not the demo. |
| Roll-up churn killing loved tools (§2.6) | MIT/Apache open source. If the company disappears, the community forks. Structural, not promised. |
| Simplicity-vs-depth binary (§3) | The gap itself: product-analytics depth (funnels, retention, cohorts, user-level) at lightweight-tool simplicity — plus PostHog's SDK ecosystem inherited via wire compatibility, the same playbook RudderStack proved against Segment (§2.7). |

### The thesis, restated

PostHog's backend is a 2020-era architecture (Kafka + ClickHouse + Django) built when that was the only route to scale. The industry has since learned — DuckDB, Arrow, object storage, single-binary systems — that one machine can do 10–100x more. **Hoglet arbitrages that generational gap while inheriting PostHog's greatest asset, its SDK ecosystem, through strict wire compatibility** — kept honest by a contract-test harness that runs real PostHog SDKs against hoglet nightly, so upstream changes break CI, not users.

Compatibility is the acquisition strategy. The things the suites structurally cannot offer — one binary, honest billing (none), honest numbers, flags that don't go down with the dashboard, data that never leaves your infrastructure — are the retention strategy.

**Analytics is not big data for 99% of companies. Stop paying the 1%'s complexity tax.**

---

## Evidence quality note

GitHub-issue, HN, and post-mortem findings above are primary-sourced with verbatim quotes. Reddit and G2/Capterra/Trustpilot block automated fetching (403s), so themes attributed to those platforms rest on search-snippet synthesis of the original content rather than fetched page text — directionally consistent across multiple aggregators, but softer evidence. Some competitor-comparison claims (Statsig, OpenPanel, Amplitude marketing pages) are vendor-authored; they're used here only where they align with independent user reports.

## Sources

**PostHog (primary):** [HN: $10K bill](https://news.ycombinator.com/item?id=39983612) · [person-profiles refund policy](https://posthog.com/handbook/growth/sales/refunds) · [duplicate persons #1350 (open since 2020)](https://github.com/PostHog/posthog/issues/1350) · [merge failures #23690](https://github.com/PostHog/posthog/issues/23690) · [ghost profiles analysis](https://dev.to/red_bean_37803fd04e673991/you-dont-have-50000-users-how-ghost-profiles-pollute-your-posthog-data-4bha) · [ad-blocker #2866](https://github.com/PostHog/posthog-js/issues/2866) · [bundle size #65](https://github.com/PostHog/posthog-js/issues/65) · [#1905](https://github.com/PostHog/posthog-js/issues/1905) · [#1514](https://github.com/PostHog/posthog-js/issues/1514) · [post-mortems repo](https://github.com/PostHog/post-mortems) · [flags outage post-mortem](https://posthog.com/handbook/company/post-mortems/2025-09-29-flags-is-down) · [logs data-loss post-mortem](https://posthog.com/handbook/company/post-mortems/2026-02-20-posthog-us-logs-data-loss) · [founder on CLOUD Act (HN)](https://news.ycombinator.com/item?id=33163819) · [self-hosting guide](https://cotera.co/articles/posthog-self-hosted-guide) · [12-hours-debugging](https://dev.to/ismailmirza/i-spent-12-hours-debugging-posthog-self-hosting-so-you-dont-have-to-pb0) · [warehouse analysis](https://www.definite.app/blog/posthog-data-warehouse)

**Mixpanel / Amplitude:** [HN: Mixpanel pricing](https://news.ycombinator.com/item?id=40443453) · [Amplitude founder on pricing](https://x.com/spenserskates/status/1714427193600995552) · [Vendr contract data](https://www.vendr.com/marketplace/amplitude) · [OpenAI on Mixpanel breach](https://openai.com/index/mixpanel-incident/) · [TechCrunch on breach](https://techcrunch.com/2025/12/02/a-data-breach-at-analytics-giant-mixpanel-leaves-a-lot-of-open-questions/) · [taxonomy rot / Event Approval](https://www.optizent.com/blog/how-to-use-mixpanel-event-approval-to-stop-rogue-events-before-they-pollute-your-data/) · [June.so shutdown](https://www.june.so/blog/a-new-chapter) · [Statsig→Amplitude questions](https://martech.org/amplitude-and-statsig-deal-raises-questions-for-customers/) · [Indie Hackers thread](https://www.indiehackers.com/post/are-you-using-product-analytics-amplitude-mixpanel-posthog-etc-402c98a268) · [Mixpanel/GA discrepancies](https://segment.com/blog/debugging-mixpanel-and-google-analytics-discrepancies/)

**Replay / flags / DXP tools:** [FullStory pricing](https://www.failory.com/blog/fullstory-pricing) · [LogRocket pricing](https://www.fullsession.io/blog/logrocket-pricing/) · [Hotjar pricing/complaints](https://blog.uxtweak.com/hotjar-pricing/) · [Heap overview](https://www.factors.ai/blog/what-is-heap-analytics-heap-io-overview) · [Pendo reviews](https://userguiding.com/blog/pendo-reviews) · [LaunchDarkly costs](https://www.statsig.com/blog/comparing-feature-flag-platform-costs)

**GA4 / Segment / demand side:** [GA4 backlash](https://www.searchenginejournal.com/google-analytics-4-backlash/411392/) · [GA4 abandonment data](https://technologychecker.io/blog/google-analytics-4-migration-insights) · [GA illegal in EU](https://www.simpleanalytics.com/blog/is-google-analytics-illegal-in-europe) · [RudderStack Segment-compat playbook](https://www.rudderstack.com/stuck-with-segment-guide/) · [Segment pricing](https://www.spendflo.com/blog/segment-pricing-guide) · [Plausible switcher evidence](https://thestacc.com/reviews/plausible/) · [the unfilled gap (cookieless product analytics)](https://openpanel.dev/articles/cookieless-analytics) · [self-hosted analytics landscape](https://openpanel.dev/articles/open-source-web-analytics)
