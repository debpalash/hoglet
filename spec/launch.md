# Launch — Implementation Spec

P8. One-curl install, prebuilt binaries, demo, docs, Show HN. The final
mile before the repo goes public.

Status: ○ to implement
Boss-gated: timing, naming, publishing (decisions.md).
Exit: a stranger can install Hoglet with one command and see a working
      dashboard with demo data in under 60 seconds.

---

## 1. Distribution

### Prebuilt binaries

Build matrix (via GitHub Actions):
- linux/amd64, linux/arm64
- macOS/amd64, macOS/arm64
- Single static binary, no runtime dependencies

Build command: `cargo build --release && strip target/release/hoglet`
Output: `hoglet-{version}-{os}-{arch}.tar.gz` containing one binary.

### Docker image

```dockerfile
FROM scratch
COPY hoglet /hoglet
EXPOSE 8000
ENTRYPOINT ["/hoglet"]
```

Single-layer, ~15 MB compressed. Published to GitHub Container Registry.

### One-curl install

```bash
curl -sSL https://get.hoglet.dev | bash
```

The script:
1. Detects OS + arch
2. Downloads the right binary from GitHub Releases
3. Places it in `/usr/local/bin/hoglet`
4. Prints: `hoglet installed. Run 'hoglet' to start.`

---

## 2. Demo mode

`HOGLET_DEMO=1 hoglet` seeds the database with realistic demo data:
- 5,000 events from 3 event types over 90 days
- 100 distinct persons
- 2 feature flags (one boolean, one multivariate)
- 2 saved insights (trends + funnel)
- 1 dashboard

This gives the first-run user something to look at immediately. Without
demo mode, the dashboard is empty until events are ingested.

The demo seeder runs at startup before the server binds. It checks if
the store already has data — if so, it skips (idempotent).

---

## 3. Docs site

Static site generated from the `spec/` directory. Served from `docs.hoglet.dev`
but also available at `/docs` in the running binary.

Pages:
- Quickstart (`README.md`)
- API Reference (auto-generated from OpenAPI spec + Scalar)
- SDK Setup — copy-paste snippets for posthog-js, posthog-python, posthog-node
- Architecture (`spec/README.md`)
- Deployment guide — systemd, Docker, fly.io
- FAQ

The docs are built with the same `rust-embed` pattern as the dashboard.

---

## 4. Show HN readiness

Checklist before the Show HN post:

- [ ] Binary downloads work from a fresh macOS and Linux VM
- [ ] Docker `docker run -p 8000:8000 ghcr.io/hoglet/hoglet:latest` works
- [ ] Demo mode shows a populated dashboard with trends + funnel data
- [ ] posthog-js points at Hoglet and events appear in the live stream
- [ ] posthog-python points at Hoglet and events appear
- [ ] Feature flag evaluates correctly through a stock SDK
- [ ] Funnel query returns correct numbers
- [ ] Login/setup flow works on first visit
- [ ] README.md is compelling (one-paragraph pitch, quickstart, demo GIF)
- [ ] Landing page (hoglet.dev) has a "Try it" button → demo
- [ ] claims.md is updated with measured numbers from the benchmark box
- [ ] Spec files are polished (no TODO markers, no WIP notes)
- [ ] Terminal output is friendly (emojis, colors, clear next steps)

---

## 5. Measured numbers (claims.md)

Run on a 1 vCPU / 1 GB RAM VPS (Hetzner CX22 or equivalent):

- Ingest: ≥5,000 events/s sustained
- Ack p99: <50ms
- Memory: <400 MB RSS under ingest load
- Cold start: <1s to ready
- Funnel over 10M events: <2s
- Recovery from crash: <5s

These replace the "design targets" in spec/README.md with measured reality.

---

## 6. Repo polish

- [ ] AGPL-3.0 license header on every source file
- [ ] CONTRIBUTING.md
- [ ] CODE_OF_CONDUCT.md
- [ ] SECURITY.md (reporting, scope, GPG key)
- [ ] CHANGELOG.md (v0.1.0 — initial public release)
- [ ] `.github/workflows/ci.yml` updated with release build + publish
- [ ] `.github/workflows/contract-tests.yml` runs nightly against SDK `latest`
- [ ] GitHub repo description + topics + website link
- [ ] `ref/` directory removed or replaced with SPDX-compatible references
- [ ] No hardcoded secrets, no `.env` files, no AWS keys in git history

---

## 7. What P8 does NOT do

- Cloud hosting (separate commercial decision, boss-gated).
- Paid pricing model.
- Blog posts, launch partners, press outreach.
- Hacker News comment monitoring.
- Email list, newsletter.
- Social media presence beyond the Discord (already spec'd, launch week).
