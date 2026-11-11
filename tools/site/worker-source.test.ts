// Checks of worker.ts's source text: every Cache-Control the Worker sets
// comes from the cache-class table (cache-control.ts) or shared().
//
// The nightly mutation run leaves this file out (MUTATION_TESTING in
// vitest.config.ts). Stryker rewrites the sources in place while it runs,
// so this check would read its instrumented code and fail, and it cannot
// catch a mutant anyway: Stryker switches mutants on at run time, never in
// the text this test reads.

import assert from "node:assert/strict";
import { test } from "vitest";

// Read by Vite when it transforms this test: it runs inside the Workers
// runtime, which has no view of the repository.
import WORKER_SOURCE from "./worker.ts?raw";

test("the Worker sets Cache-Control only from the cache-class table", () => {
  // Every assignment, in any casing, takes a table value, shared() or a local
  // built from shared() alone.
  const assigned = [...WORKER_SOURCE.matchAll(/["']cache-control["']\s*[:,]\s*([^\n]+)/gi)];
  assert.ok(assigned.length > 10);
  for (const [line, value] of assigned) {
    assert.match(value ?? "", /^(?:CACHE_CONTROL\.\w+|shared\(|cacheControl,|cached,)/, line);
  }
  const locals = [...WORKER_SOURCE.matchAll(/const (?:cacheControl|cached) =([^;]+);/g)];
  assert.ok(locals.length > 0);
  for (const [line, value] of locals) {
    assert.doesNotMatch(value ?? "", /["'`]/, line);
    assert.match(value ?? "", /shared\(/, line);
  }
  assert.doesNotMatch(
    WORKER_SOURCE,
    /["'][^"'\n]*\b(?:no-store|no-cache|must-revalidate|s-maxage|immutable)\b/i,
  );
});
