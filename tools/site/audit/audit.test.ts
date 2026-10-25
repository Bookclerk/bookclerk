// Tests for audit.ts's pure helpers. Run with `node --test audit.test.ts`
// (CI runs them before the audit).

import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";

import { ERROR_PAGE, PAGE, PAGES, UNAVAILABLE_KEY, docsFile, lighthouseMisses } from "./audit.ts";
import type { LighthouseScores } from "./audit.ts";

/**
 * A Lighthouse result with the given category scores (0-100) and CLS.
 *
 * @param scores - Category scores.
 * @param cls - Cumulative layout shift.
 * @returns The parts of a Lighthouse result the thresholds read.
 */
function lhr(scores: Readonly<Record<string, number | null>>, cls = 0): LighthouseScores {
  const categories = Object.fromEntries(Object.entries(scores).map(([k, v]) => [k, { score: v === null ? null : v / 100 }]));
  return { categories, audits: { "cumulative-layout-shift": { numericValue: cls } } };
}

/** Every category at 100. */
const PERFECT = { performance: 100, accessibility: 100, "best-practices": 100, seo: 100 };

test("a page at every limit passes", () => {
  assert.deepEqual(lighthouseMisses(lhr({ performance: 90, accessibility: 100, "best-practices": 95, seo: 100 }, 0.05), PAGE), []);
});

test("each miss is reported", () => {
  const misses = lighthouseMisses(lhr({ performance: 89, accessibility: 99, "best-practices": 94, seo: 92 }, 0.06), PAGE);
  assert.equal(misses.length, 5);
  assert.ok(misses.some((m) => m.startsWith("performance 89")));
  assert.ok(misses.some((m) => m.startsWith("CLS 0.06")));
});

test("a missing score or CLS is a miss, not a pass", () => {
  assert.deepEqual(lighthouseMisses(lhr({ ...PERFECT, performance: null }), PAGE), ["performance missing < 90"]);
  const noCls = { categories: lhr(PERFECT).categories, audits: {} };
  assert.deepEqual(lighthouseMisses(noCls, PAGE), ["CLS missing > 0.05"]);
});

test("error pages are held to accessibility only", () => {
  assert.deepEqual(lighthouseMisses(lhr({ performance: 10, accessibility: 100, "best-practices": 50, seo: 40 }, 1), ERROR_PAGE), []);
  assert.equal(lighthouseMisses(lhr({ ...PERFECT, accessibility: 95 }), ERROR_PAGE).length, 1);
});

test("the audited pages cover both error pages", () => {
  assert.deepEqual(PAGES.filter((p) => p.limits === ERROR_PAGE).map((p) => p.status).sort(), [404, 503]);
  assert.ok(PAGES.some((p) => p.path === `/docs/${UNAVAILABLE_KEY}`));
});

test("docs paths map into the docs directory only", () => {
  const root = "/repo/docs";
  assert.equal(docsFile(root, "/main/getting-started.md"), join(root, "getting-started.md"));
  assert.equal(docsFile(root, "/v0.1.0/adr/x.md"), join(root, "adr", "x.md"));
  assert.equal(docsFile(root, "/main/../secret.md"), null);
  assert.equal(docsFile(root, "/main/%2e%2e/secret.md"), null);
  assert.equal(docsFile(root, "/main/getting-started"), null);
  assert.equal(docsFile(root, "/getting-started.md"), null);
});
