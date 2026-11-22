/**
 * Playwright configuration for the visual and accessibility checks of every
 * web page Bookclerk ships.
 *
 * Every story in the static Storybook build (stories.spec.ts), every page of
 * the web app's production build (app.spec.ts) and every page of the website
 * and the diagnostics collector (workers.spec.ts) is opened at
 * each viewport project below, in light and dark, checked with axe-core and
 * compared with a baseline screenshot. Baselines do not live in this repository: CI checks
 * them out of the baselines repository into `VISUAL_BASELINES_DIR`. See
 * docs/contributing/ui/visual-testing.md.
 */
import path from "node:path";
import { defineConfig } from "@playwright/test";
import { APP_PORT, COLLECTOR_PORT, SITE_PORT, STORYBOOK_PORT } from "./servers.ts";

/** The repository root. */
const ROOT = path.join(import.meta.dirname, "..", "..");

/** Directory holding baseline screenshots, one folder per project. */
const baselines = path.resolve(
  process.env.VISUAL_BASELINES_DIR ?? path.join(import.meta.dirname, "baselines"),
);

/** Directory for the HTML report and the actual / diff images of failures. */
const out = path.resolve(
  process.env.VISUAL_OUTPUT_DIR ?? path.join(import.meta.dirname, "..", "visual-results"),
);

export default defineConfig({
  testDir: import.meta.dirname,
  testMatch: "*.spec.ts",
  // Fails the run when a web app component never rendered (coverage.ts);
  // only with VISUAL_COVERAGE_CHECK=1, which the CI recording run sets.
  globalTeardown: path.join(import.meta.dirname, "coverage-check.ts"),
  outputDir: path.join(out, "test-results"),
  snapshotPathTemplate: path.join(baselines, "{projectName}", "{arg}{ext}"),
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  // A retry would hide a flaky screenshot, which is exactly what to fix.
  retries: 0,
  // results.json tells the baselines bot whether a comparison run finished
  // (scripts/visual_baselines.py compare_finished).
  reporter: [
    ["list"],
    ["html", { outputFolder: path.join(out, "report"), open: "never" }],
    ["json", { outputFile: path.join(out, "results.json") }],
  ],
  expect: {
    toHaveScreenshot: {
      // Baselines come from the pinned CI image, so the comparison is exact
      // apart from Playwright's default per-pixel colour threshold.
      maxDiffPixels: 0,
      animations: "disabled",
      caret: "hide",
      scale: "css",
    },
  },
  use: {
    baseURL: `http://127.0.0.1:${STORYBOOK_PORT}`,
    deviceScaleFactor: 1,
    locale: "en-US",
    timezoneId: "UTC",
    // Stories pick their theme from the `theme` global, not the OS setting.
    colorScheme: "light",
    reducedMotion: "reduce",
    // Only for a host whose preinstalled Chromium is not the one this
    // Playwright release downloads; CI uses the pinned Playwright image.
    launchOptions: { executablePath: process.env.VISUAL_CHROMIUM_PATH || undefined },
  },
  projects: [
    { name: "desktop", use: { viewport: { width: 1280, height: 800 } } },
    {
      name: "phone",
      use: { viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true },
    },
  ],
  webServer: [
    {
      command: `npx vite preview --outDir storybook-static --host 127.0.0.1 --port ${STORYBOOK_PORT} --strictPort`,
      cwd: path.join(import.meta.dirname, ".."),
      url: `http://127.0.0.1:${STORYBOOK_PORT}/index.json`,
      reuseExistingServer: !process.env.CI,
    },
    // The web app's production build, unminified so the coverage check
    // knows its components by name, with no daemon behind it: app.spec.ts
    // answers its API requests from sample data. Needs `npm run
    // build:visual` first.
    {
      command: `npx vite preview --outDir visual-dist --host 127.0.0.1 --port ${APP_PORT} --strictPort`,
      cwd: path.join(import.meta.dirname, ".."),
      url: `http://127.0.0.1:${APP_PORT}/`,
      reuseExistingServer: !process.env.CI,
    },
    // The website through its real Worker under `wrangler dev`, with the
    // docs fixture and a fixed catalog sample. Needs `npm ci` and a build in
    // tools/site first (docs/contributing/ui/visual-testing.md).
    {
      command: `node audit/serve.ts --port ${SITE_PORT} --fixed-sample --log ${path.join(out, "wrangler.log")}`,
      cwd: path.join(ROOT, "tools", "site"),
      url: `http://127.0.0.1:${SITE_PORT}/`,
      reuseExistingServer: !process.env.CI,
      // wrangler dev starts workerd and bundles the Worker first.
      timeout: 150_000,
      // SIGTERM, not the default SIGKILL, so serve.ts stops wrangler too.
      gracefulShutdown: { signal: "SIGTERM", timeout: 5_000 },
    },
    // The collector's page templates, rendered from fixed sample data.
    {
      command: `node visual/serve.ts --port ${COLLECTOR_PORT}`,
      cwd: path.join(ROOT, "tools", "diagnostics-collector"),
      url: `http://127.0.0.1:${COLLECTOR_PORT}/`,
      reuseExistingServer: !process.env.CI,
    },
  ],
});
