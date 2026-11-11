// Checks of the Worker's source text behind the cache-class contract
// (cache-control.test.ts): every Cache-Control the Worker sets comes from
// the table, and dashboard/asset-urls.ts is exactly what
// `npm run hash-assets` writes.
//
// The nightly mutation run leaves this file out (MUTATION_TESTING in
// vitest.config.ts). Stryker rewrites the sources in place while it runs,
// so these checks would read its instrumented code and fail, and they
// cannot catch a mutant anyway: Stryker switches mutants on at run time,
// never in the text these tests read.

import assert from "node:assert/strict";
import { describe, it } from "vitest";

import { ASSETS_PATH, assetUrlsModule, HASHED_ASSETS, type HashedAsset } from "./asset-names.ts";
import { ASSET_URLS } from "./dashboard/asset-urls.ts";
// Read by Vite when it transforms this test: it runs inside the Workers
// runtime, which has no view of the repository.
import ASSET_URLS_SOURCE from "./dashboard/asset-urls.ts?raw";

/** The Worker's own sources besides the table, by path. */
const SOURCES: Readonly<Record<string, string>> = import.meta.glob<string>(
  ["./*.ts", "./dashboard/*.ts", "!./*.test.ts", "!./cache-control.ts", "!./vitest.config.ts"],
  { query: "?raw", import: "default", eager: true },
);

describe("cache classes in the source", () => {
  it("the Worker sets Cache-Control only from the table", () => {
    assert.ok(Object.keys(SOURCES).length > 10);
    let assignments = 0;
    for (const [path, source] of Object.entries(SOURCES)) {
      // Every assignment, in any casing, takes a table value.
      for (const [line, value] of source.matchAll(/["']cache-control["']\s*[:,]\s*([^\n]+)/gi)) {
        assignments += 1;
        assert.match(value ?? "", /^CACHE_CONTROL\.\w+/, `${path}: ${line}`);
      }
      assert.doesNotMatch(
        source,
        /["'][^"'\n]*\b(?:no-store|no-cache|must-revalidate|s-maxage|immutable)\b/i,
        path,
      );
    }
    assert.ok(assignments >= 4);
  });

  it("dashboard/asset-urls.ts is exactly what npm run hash-assets writes", () => {
    // cache-control.test.ts checks each URL against the hashed file on disk.
    const names = Object.fromEntries(
      HASHED_ASSETS.map((file) => [file, ASSET_URLS[file].slice(ASSETS_PATH.length)]),
    ) as Record<HashedAsset, string>;
    assert.equal(ASSET_URLS_SOURCE, assetUrlsModule(names));
  });
});
