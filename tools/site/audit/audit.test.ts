// Tests for audit.ts's pure helpers. Run with `node --test audit.test.ts`
// (CI runs them before the audit).

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import type { LaunchedChrome, Options } from "chrome-launcher";

import { ERROR_PAGE, PAGE, PAGES, UNAVAILABLE_KEY, docsFile, isTransientStartFailure, launchChrome, lighthouseMisses } from "./audit.ts";
import type { ChromeLauncher, LighthouseScores } from "./audit.ts";

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

/**
 * A chrome-launcher stand-in whose first `failures` starts reject the way a
 * browser that never reports its debugging port does (or with `error`), after
 * writing a `chrome-err.log` into the profile directory it was given.
 *
 * @param failures - How many starts fail before one works.
 * @param error - What a failed start rejects with.
 * @returns The stand-in, the options of every start, and how often `killAll` ran.
 */
function fakeLauncher(failures: number, error: () => Error = () => new Error("waiting for dynamic debugging port in chrome-err.log")): ChromeLauncher & { calls: Options[]; kills: number[] } {
  const fake = {
    calls: [] as Options[],
    kills: [] as number[],
    launch(opts: Options = {}): Promise<LaunchedChrome> {
      fake.calls.push(opts);
      if (typeof opts.userDataDir === "string") writeFileSync(join(opts.userDataDir, "chrome-err.log"), `start ${fake.calls.length}\n`);
      if (fake.calls.length <= failures) return Promise.reject(error());
      const chrome: LaunchedChrome = { pid: 1, port: 9222, process: {} as LaunchedChrome["process"], remoteDebuggingPipes: null, kill: () => undefined };
      return Promise.resolve(chrome);
    },
    killAll(): Error[] {
      fake.kills.push(fake.calls.length);
      return [];
    },
  };
  return fake;
}

test("a Chrome that misses its debugging port once is started again, and the failed start's log is kept", async () => {
  const out = mkdtempSync(join(tmpdir(), "bookclerk-audit-test-"));
  const cleanup = [out];
  try {
    const launcher = fakeLauncher(1);
    const { calls } = launcher;
    const running = await launchChrome(launcher, "/usr/bin/chrome", ["--headless=new"], out);
    cleanup.push(running.userDataDir);
    assert.equal(calls.length, 2);
    assert.deepEqual(launcher.kills, [1], "the browser that never answered is killed before the next start");
    assert.equal(running.chrome.port, 9222);
    assert.notEqual(calls[0]?.userDataDir, calls[1]?.userDataDir, "each start gets a fresh profile directory");
    assert.equal(calls[1]?.userDataDir, running.userDataDir);
    assert.equal(readFileSync(join(out, "chrome-err-1.log"), "utf8"), "start 1\n");
    assert.ok(!existsSync(join(out, "chrome-err-2.log")), "a start that worked leaves no log in the reports");
    assert.ok(!existsSync(String(calls[0]?.userDataDir)), "the failed start's profile directory is removed");
  } finally {
    for (const dir of cleanup) rmSync(dir, { recursive: true, force: true });
  }
});

test("a Chrome that never starts fails the audit after the last attempt, with every start's log kept", async () => {
  const out = mkdtempSync(join(tmpdir(), "bookclerk-audit-test-"));
  try {
    const launcher = fakeLauncher(5);
    const { calls } = launcher;
    await assert.rejects(launchChrome(launcher, "/usr/bin/chrome", [], out, 3), /dynamic debugging port/);
    assert.equal(calls.length, 3);
    assert.deepEqual(launcher.kills, [1, 2, 3]);
    for (const n of [1, 2, 3]) assert.equal(readFileSync(join(out, `chrome-err-${n}.log`), "utf8"), `start ${n}\n`);
    for (const call of calls) assert.ok(!existsSync(String(call.userDataDir)));
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
});

test("a start that would fail the same way again is not repeated", async () => {
  const out = mkdtempSync(join(tmpdir(), "bookclerk-audit-test-"));
  try {
    const launcher = fakeLauncher(5, () => Object.assign(new Error("No Chrome installations found."), { code: "ERR_LAUNCHER_NOT_INSTALLED" }));
    await assert.rejects(launchChrome(launcher, "/usr/bin/chrome", [], out, 3), /No Chrome installations/);
    assert.equal(launcher.calls.length, 1);
    assert.equal(readFileSync(join(out, "chrome-err-1.log"), "utf8"), "start 1\n", "the log is still kept");
    assert.ok(!existsSync(String(launcher.calls[0]?.userDataDir)));
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
});

test("a start fails for good only on chrome-launcher's own errors", () => {
  assert.ok(isTransientStartFailure(new Error("waiting for dynamic debugging port in chrome-err.log")));
  assert.ok(isTransientStartFailure(Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:9222"), { code: "ECONNREFUSED" })), "a port that was reported but not served");
  assert.ok(!isTransientStartFailure(Object.assign(new Error("unsupported"), { code: "ERR_LAUNCHER_UNSUPPORTED_PLATFORM" })));
  assert.ok(!isTransientStartFailure(Object.assign(new Error("No Chrome installations found."), { code: "ERR_LAUNCHER_NOT_INSTALLED" })));
  assert.ok(!isTransientStartFailure("not an error"));
});
