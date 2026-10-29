// Test runner configuration (`npm test`). Two projects:
//
// - `workers` runs the Worker's tests inside the Workers runtime through
//   the Workers Vitest integration, on the Worker's compatibility date.
//   worker.test.ts serves a real build, which test-support/site-build.ts
//   makes in Node before the tests start.
// - `node` runs the tests of the build itself (build.ts writes dist/ and
//   reads the repository) and of the docs pages against the whole docs/
//   tree and the browser scripts, and the IndexNow command line
//   (indexnow.test.ts), which runs in Node from the indexnow workflow.

import { readFileSync } from "node:fs";
import { URL } from "node:url";

import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

/** wrangler.toml's compatibility_date, so tests run on the deployed runtime's behavior. */
const COMPATIBILITY_DATE = /^compatibility_date = "([0-9-]+)"$/m.exec(
  readFileSync(new URL("wrangler.toml", import.meta.url), "utf8"),
)?.[1];
if (COMPATIBILITY_DATE === undefined) throw new Error("wrangler.toml has no compatibility_date");

/** Tests that run outside the Workers runtime. */
const NODE_TESTS = ["build.test.ts", "docs.test.ts", "indexnow.test.ts"];

/**
 * FUZZ_SEED and FUZZ_ITERATIONS from the shell, passed on to fuzz.test.ts,
 * which reads them from process.env inside the Workers runtime.
 */
const FUZZ_VARS = Object.fromEntries(
  ["FUZZ_SEED", "FUZZ_ITERATIONS"].flatMap((name) => {
    const value = process.env[name];
    return value === undefined ? [] : [[name, value]];
  }),
);

export default defineConfig({
  test: {
    // node:test had no per-test timeout; the fuzz tests and coverage runs
    // take longer than Vitest's 5 s default.
    testTimeout: 120_000,
    // `npm run coverage`: V8 coverage from both projects, merged.
    coverage: {
      provider: "v8",
      include: ["*.ts"],
      exclude: ["*.test.ts", "vitest.config.ts"],
      reporter: ["text-summary", "lcov"],
    },
    projects: [
      {
        plugins: [
          cloudflareTest({
            miniflare: {
              compatibilityDate: COMPATIBILITY_DATE,
              // Test files use node:assert; the Worker itself does not.
              compatibilityFlags: ["nodejs_compat"],
              bindings: FUZZ_VARS,
            },
          }),
        ],
        test: {
          name: "workers",
          include: ["*.test.ts"],
          exclude: NODE_TESTS,
          globalSetup: ["test-support/site-build.ts"],
          setupFiles: ["test-support/workers-setup.ts"],
        },
      },
      {
        test: { name: "node", include: NODE_TESTS, environment: "node" },
      },
    ],
  },
});
