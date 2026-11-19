/**
 * Playwright configuration for the SPA's visual and accessibility checks.
 *
 * Every story in the static Storybook build is opened at each viewport
 * project below, in light and dark, checked with axe-core and compared with
 * a baseline screenshot. Baselines do not live in this repository: CI checks
 * them out of the baselines repository into `VISUAL_BASELINES_DIR`. See
 * docs/contributing/ui/visual-testing.md.
 */
import path from "node:path";
import { defineConfig } from "@playwright/test";

/** Port the static Storybook build is served on while the checks run. */
const PORT = 6106;

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
  outputDir: path.join(out, "test-results"),
  snapshotPathTemplate: path.join(baselines, "{projectName}", "{arg}{ext}"),
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  // A retry would hide a flaky screenshot, which is exactly what to fix.
  retries: 0,
  reporter: [["list"], ["html", { outputFolder: path.join(out, "report"), open: "never" }]],
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
    baseURL: `http://127.0.0.1:${PORT}`,
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
  webServer: {
    command: `npx vite preview --outDir storybook-static --host 127.0.0.1 --port ${PORT} --strictPort`,
    cwd: path.join(import.meta.dirname, ".."),
    url: `http://127.0.0.1:${PORT}/index.json`,
    reuseExistingServer: !process.env.CI,
  },
});
