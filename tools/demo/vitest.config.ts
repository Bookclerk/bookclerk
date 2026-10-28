// Test runner configuration (`npm test`). Two projects:
//
// - `workers` runs the Worker's tests inside the Workers runtime through
//   the Workers Vitest integration, with the Worker's compatibility date and
//   real D1 databases (test-support/d1.ts) carrying the demo migrations.
// - `node` runs the tests of what the browser runs: the pages' scripts and
//   static files, which need node:vm and the files themselves.

import { readFileSync } from "node:fs";
import { fileURLToPath, URL } from "node:url";

import { cloudflareTest, readD1Migrations } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

import { TEST_DB_POOL_SIZE } from "./test-support/d1-pool.ts";

/** wrangler.toml's compatibility_date, so tests run on the deployed runtime's behavior. */
const COMPATIBILITY_DATE = /^compatibility_date = "([0-9-]+)"$/m.exec(
  readFileSync(new URL("wrangler.toml", import.meta.url), "utf8"),
)?.[1];
if (COMPATIBILITY_DATE === undefined) throw new Error("wrangler.toml has no compatibility_date");

/** Tests that run outside the Workers runtime. */
const NODE_TESTS = ["test/pages.test.ts", "test/live-page.test.ts"];

export default defineConfig({
  test: {
    // `npm run coverage`: V8 coverage from both projects, merged.
    coverage: {
      provider: "v8",
      include: ["src/**/*.ts"],
      reporter: ["text-summary", "lcov"],
    },
    projects: [
      {
        plugins: [
          cloudflareTest(async () => ({
            miniflare: {
              compatibilityDate: COMPATIBILITY_DATE,
              // Test files use node:assert; the Worker itself does not.
              compatibilityFlags: ["nodejs_compat"],
              d1Databases: Array.from({ length: TEST_DB_POOL_SIZE }, (_, i) => `TEST_DB_${i}`),
              bindings: {
                TEST_MIGRATIONS: await readD1Migrations(
                  fileURLToPath(new URL("migrations", import.meta.url)),
                ),
              },
            },
          })),
        ],
        test: {
          name: "workers",
          include: ["test/*.test.ts"],
          exclude: NODE_TESTS,
          setupFiles: ["test-support/workers-setup.ts"],
        },
      },
      {
        test: { name: "node", include: NODE_TESTS, environment: "node" },
      },
    ],
  },
});
