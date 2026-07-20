import { defineConfig } from "@playwright/test";
export default defineConfig({
  testMatch: "**/*.spec.mjs",
  timeout: 60_000,
  use: { headless: true },
});
