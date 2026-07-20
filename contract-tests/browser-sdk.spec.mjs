// Browser contract test: unmodified posthog-js at latest, loaded in a real
// page under Playwright, api_host pointed at a real Hoglet (claims.md
// claim 1). Run via `npm run test:browser` after `npx playwright install
// chromium`.

import { test, expect } from "@playwright/test";
import { spawn } from "node:child_process";
import { mkdtempSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const PORT = 18898;
const TOKEN = "phc_browser_contract";
const BIN =
  process.env.HOGLET_BIN ?? new URL("../target/debug/hoglet", import.meta.url).pathname;

let hoglet;
let dataDir;

test.beforeAll(async () => {
  dataDir = mkdtempSync(join(tmpdir(), "hoglet-browser-"));
  hoglet = spawn(BIN, {
    env: { ...process.env, HOGLET_ADDR: `127.0.0.1:${PORT}`, HOGLET_DATA: dataDir },
    stdio: "ignore",
  });
  for (let i = 0; i < 100; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${PORT}/array/${TOKEN}/config`);
      if (res.ok) return;
    } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("hoglet did not start");
});

test.afterAll(() => hoglet?.kill("SIGKILL"));

test("posthog-js initializes, captures, and identifies against hoglet", async ({ page }) => {
  const failed = [];
  page.on("requestfailed", (req) => {
    if (req.url().includes(`127.0.0.1:${PORT}`)) failed.push(req.url());
  });

  await page.setContent(`<html><body><h1>contract</h1></body></html>`);
  await page.addScriptTag({ url: "https://unpkg.com/posthog-js@latest/dist/array.js" });
  await page.evaluate(
    ([token, port]) => {
      window.posthog.init(token, {
        api_host: `http://127.0.0.1:${port}`,
        loaded: () => (window.__loaded = true),
      });
      window.posthog.capture("browser_pageview", { page: "/contract" });
      window.posthog.identify("browser-user@contract.test");
    },
    [TOKEN, PORT]
  );

  await page.waitForFunction(() => window.__loaded === true, null, { timeout: 10_000 });
  // Let the SDK's batching flush.
  await page.waitForTimeout(3_000);

  expect(failed, `SDK requests failed: ${failed.join(", ")}`).toHaveLength(0);

  // Events must be on disk, not merely 200'd.
  const walDir = join(dataDir, "wal");
  const walBytes = readdirSync(walDir)
    .filter((f) => f.endsWith(".wal"))
    .reduce((sum, f) => sum + statSync(join(walDir, f)).size, 0);
  let parquet = 0;
  try {
    parquet = readdirSync(join(dataDir, "events")).filter((f) => f.endsWith(".parquet")).length;
  } catch {}
  expect(walBytes + parquet).toBeGreaterThan(0);
});
