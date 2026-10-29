// Tests for the IndexNow submission (indexnow.ts) and its command line
// (indexnow-run.ts), against a stand-in for the site and IndexNow. Run with
// `npm test`; the readers are also fuzzed in fuzz.test.ts.

import assert from "node:assert/strict";
import { test } from "vitest";

import {
  INDEXNOW_ENDPOINT,
  MAX_SITEMAP_ENTRIES,
  MAX_URLS_PER_REQUEST,
  parseKey,
  parseSiteUrl,
  planSubmission,
  readSitemap,
  readSnapshot,
  submissionBodies,
  writeSnapshot,
} from "./indexnow.ts";
import type { IndexNowKey, SiteUrl, Snapshot } from "./indexnow.ts";
import { run } from "./indexnow-run.ts";
import type { RunDeps } from "./indexnow-run.ts";
import { parseIsoDate } from "./last-updated.ts";
import type { IsoDate } from "./last-updated.ts";
import { MAX_SITEMAP_URLS, sitemapIndexXml, sitemapXml } from "./sitemap.ts";

/** The origin the tests serve. */
const ORIGIN = "https://bookclerk.app";

/** A valid key. */
const KEY = "4f0c9d2b7e1a4c3d8b6a5e9f0d1c2b3a";

/**
 * A checked date.
 *
 * @param text - `YYYY-MM-DD`.
 * @returns The date.
 */
function day(text: string): IsoDate {
  const date = parseIsoDate(text);
  assert.ok(date !== null, text);
  return date;
}

/**
 * A checked site URL.
 *
 * @param path - Its path.
 * @returns The URL.
 */
function url(path: string): SiteUrl {
  const parsed = parseSiteUrl(ORIGIN + path, ORIGIN);
  assert.ok(parsed !== null, path);
  return parsed;
}

test("indexnow.ts keeps the sitemap protocol's limit", () => {
  assert.equal(MAX_SITEMAP_ENTRIES, MAX_SITEMAP_URLS);
});

test("parseKey takes the protocol's keys only", () => {
  assert.equal(parseKey(KEY), KEY);
  assert.equal(parseKey("abcd-123"), "abcd-123");
  for (const bad of ["", "short", "x".repeat(129), "with space", "dot.ted-key", "ключключключ"])
    assert.equal(parseKey(bad), null, bad);
});

test("parseSiteUrl takes the site's own URLs in normal form", () => {
  assert.equal(parseSiteUrl(`${ORIGIN}/`, ORIGIN), `${ORIGIN}/`);
  assert.equal(parseSiteUrl(`${ORIGIN}/docs/guide/stores`, ORIGIN), `${ORIGIN}/docs/guide/stores`);
  for (const bad of [
    ORIGIN, // `new URL` adds the slash
    "https://example.com/",
    "http://bookclerk.app/",
    `${ORIGIN}/a?b=c`,
    `${ORIGIN}/a#b`,
    `${ORIGIN}/?`,
    `${ORIGIN}/#`,
    "https://user@bookclerk.app/",
    `${ORIGIN}/a b`,
    "not a url",
  ])
    assert.equal(parseSiteUrl(bad, ORIGIN), null, bad);
});

test("readSitemap reads what sitemap.ts writes", () => {
  const index = readSitemap(
    sitemapIndexXml(ORIGIN, [
      { path: "/sitemap-site.xml", lastmod: null },
      { path: "/sitemap-guide.xml", lastmod: day("2026-10-27") },
    ]),
    ORIGIN,
  );
  assert.deepEqual(index, {
    kind: "index",
    entries: [
      { url: `${ORIGIN}/sitemap-site.xml`, lastmod: null },
      { url: `${ORIGIN}/sitemap-guide.xml`, lastmod: "2026-10-27" },
    ],
  });
  const pages = readSitemap(
    sitemapXml(ORIGIN, [
      { path: "/", lastmod: null },
      { path: "/docs/a&b", lastmod: day("2026-10-28") },
    ]),
    ORIGIN,
  );
  assert.deepEqual(pages, {
    kind: "urlset",
    entries: [
      { url: `${ORIGIN}/`, lastmod: null },
      { url: `${ORIGIN}/docs/a&b`, lastmod: "2026-10-28" },
    ],
  });
});

test("readSitemap leaves out other origins, odd entities and bad dates", () => {
  const xml =
    '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">' +
    "<url><loc>https://example.com/x</loc></url>" +
    `<url><loc>${ORIGIN}/a&nbsp;b</loc></url>` +
    `<url><loc>${ORIGIN}/c</loc><lastmod>2026-02-30</lastmod></url>` +
    "</urlset>";
  assert.deepEqual(readSitemap(xml, ORIGIN), {
    kind: "urlset",
    entries: [{ url: `${ORIGIN}/c`, lastmod: null }],
  });
});

test("readSitemap refuses documents that are not one kind of sitemap", () => {
  assert.equal(readSitemap("", ORIGIN), null);
  assert.equal(readSitemap("<html></html>", ORIGIN), null);
  assert.equal(readSitemap("<urlset><sitemapindex>", ORIGIN), null);
  assert.equal(
    readSitemap(`<urlset><sitemap><loc>${ORIGIN}/x</loc></sitemap></urlset>`, ORIGIN),
    null,
  );
  assert.equal(readSitemap(`<urlset>${" ".repeat(10 * 1024 * 1024)}</urlset>`, ORIGIN), null);
});

test("snapshots round-trip and drop malformed entries", () => {
  const snapshot: Snapshot = new Map([
    [url("/b"), { fp: "lastmod:2026-10-27", sent: day("2026-10-27") }],
    [url("/a"), { fp: "sha256:00", sent: day("2026-10-28") }],
  ]);
  const text = writeSnapshot(snapshot);
  assert.ok(text.indexOf(`${ORIGIN}/a`) < text.indexOf(`${ORIGIN}/b`), "sorted");
  assert.deepEqual(readSnapshot(text, ORIGIN), snapshot);
  const messy = JSON.stringify({
    version: 1,
    urls: {
      [`${ORIGIN}/ok`]: { fp: "x", sent: "2026-10-01" },
      "https://example.com/": { fp: "x", sent: "2026-10-01" },
      [`${ORIGIN}/no-fp`]: { sent: "2026-10-01" },
      [`${ORIGIN}/empty-fp`]: { fp: "", sent: "2026-10-01" },
      [`${ORIGIN}/long-fp`]: { fp: "x".repeat(129), sent: "2026-10-01" },
      [`${ORIGIN}/bad-date`]: { fp: "x", sent: "2026-13-01" },
      [`${ORIGIN}/no-date`]: { fp: "x", sent: 20261001 },
      [`${ORIGIN}/array`]: ["x"],
    },
  });
  assert.deepEqual(
    readSnapshot(messy, ORIGIN),
    new Map([[url("/ok"), { fp: "x", sent: "2026-10-01" }]]),
  );
  for (const bad of ["", "[]", "null", '{"version":2,"urls":{}}', '{"version":1,"urls":[]}', "{"])
    assert.equal(readSnapshot(bad, ORIGIN), null, bad);
  assert.equal(readSnapshot(" ".repeat(10 * 1024 * 1024 + 1), ORIGIN), null);
});

test("planSubmission submits everything without a snapshot", () => {
  const today = day("2026-10-29");
  const current = new Map([
    [url("/"), "sha256:1"],
    [url("/docs"), "lastmod:2026-10-27"],
  ]);
  const plan = planSubmission(null, current, today);
  assert.deepEqual(plan.submit, [url("/"), url("/docs")]);
  assert.deepEqual(
    plan.next,
    new Map([
      [url("/"), { fp: "sha256:1", sent: today }],
      [url("/docs"), { fp: "lastmod:2026-10-27", sent: today }],
    ]),
  );
});

test("planSubmission submits new, changed and removed URLs once a day", () => {
  const today = day("2026-10-29");
  const earlier = day("2026-10-28");
  const previous: Snapshot = new Map([
    [url("/same"), { fp: "a", sent: earlier }],
    [url("/changed"), { fp: "a", sent: earlier }],
    [url("/changed-again"), { fp: "a", sent: today }],
    [url("/removed"), { fp: "a", sent: earlier }],
    [url("/removed-today"), { fp: "a", sent: today }],
  ]);
  const current = new Map([
    [url("/same"), "a"],
    [url("/changed"), "b"],
    [url("/changed-again"), "b"],
    [url("/new"), "a"],
  ]);
  const plan = planSubmission(previous, current, today);
  assert.deepEqual(plan.submit, [url("/changed"), url("/new"), url("/removed")]);
  assert.deepEqual(
    plan.next,
    new Map([
      [url("/same"), { fp: "a", sent: earlier }],
      [url("/changed"), { fp: "b", sent: today }],
      // Kept as it was, so tomorrow's run still sees the change.
      [url("/changed-again"), { fp: "a", sent: today }],
      [url("/new"), { fp: "a", sent: today }],
      [url("/removed"), { fp: "removed", sent: today }],
      [url("/removed-today"), { fp: "a", sent: today }],
    ]),
  );
});

test("planSubmission holds a page removed and restored on one day for the next", () => {
  const today = day("2026-10-29");
  const tomorrow = day("2026-10-30");
  const previous: Snapshot = new Map([
    [url("/back"), { fp: "removed", sent: today }],
    [url("/gone"), { fp: "removed", sent: today }],
  ]);
  const restored = planSubmission(previous, new Map([[url("/back"), "a"]]), today);
  assert.deepEqual(restored.submit, []);
  assert.deepEqual(restored.next, previous);

  const later = planSubmission(restored.next, new Map([[url("/back"), "a"]]), tomorrow);
  // The restored page is submitted again; the removal is not repeated.
  assert.deepEqual(later.submit, [url("/back")]);
  assert.deepEqual(later.next, new Map([[url("/back"), { fp: "a", sent: tomorrow }]]));
});

test("planSubmission resubmits a page once after a submission on its own date", () => {
  const dated = day("2026-10-28");
  const previous: Snapshot = new Map([
    [url("/same-day"), { fp: "lastmod:2026-10-28", sent: dated }],
    [url("/later"), { fp: "lastmod:2026-10-27", sent: dated }],
  ]);
  const current = new Map([
    [url("/same-day"), "lastmod:2026-10-28"],
    [url("/later"), "lastmod:2026-10-27"],
  ]);
  assert.deepEqual(planSubmission(previous, current, dated).submit, []);
  const next = planSubmission(previous, current, day("2026-10-29"));
  assert.deepEqual(next.submit, [url("/same-day")]);
  assert.deepEqual(planSubmission(next.next, current, day("2026-10-30")).submit, []);
});

test("submissionBodies splits at the protocol's limit", () => {
  const key = parseKey(KEY) as IndexNowKey;
  assert.deepEqual(submissionBodies(ORIGIN, key, []), []);
  const urls = Array.from({ length: MAX_URLS_PER_REQUEST + 1 }, (_, i) => url(`/p${i}`));
  const bodies = submissionBodies(ORIGIN, key, urls).map((b) => JSON.parse(b));
  assert.equal(bodies.length, 2);
  assert.deepEqual(Object.keys(bodies[0]), ["host", "key", "keyLocation", "urlList"]);
  assert.equal(bodies[0].host, "bookclerk.app");
  assert.equal(bodies[0].key, KEY);
  assert.equal(bodies[0].keyLocation, `${ORIGIN}/${KEY}.txt`);
  assert.equal(bodies[0].urlList.length, MAX_URLS_PER_REQUEST);
  assert.deepEqual(bodies[1].urlList, [url(`/p${MAX_URLS_PER_REQUEST}`)]);
});

/** A request the stand-in fetcher saw. */
interface Seen {
  /** Its URL. */
  url: string;
  /** Its method. */
  method: string;
  /** Its body, for a POST. */
  body: string | null;
}

/** A stand-in for the site, IndexNow and the files, and what a run did with them. */
interface Harness {
  /** What the run gets. */
  deps: RunDeps;
  /** Requests made, in order. */
  seen: Seen[];
  /** Files by path: the snapshots and the summary. */
  files: Map<string, string>;
  /** Lines printed. */
  lines: string[];
}

/**
 * Builds a stand-in for the live site and IndexNow.
 *
 * @param site - Body by URL the site answers 200 with; anything else is 404.
 * @param indexNowStatus - What IndexNow answers.
 * @param env - Extra environment.
 * @returns The harness.
 */
function harness(
  site: Readonly<Record<string, string>>,
  indexNowStatus = 200,
  env: Readonly<Record<string, string>> = {},
): Harness {
  const seen: Seen[] = [];
  const files = new Map<string, string>();
  const lines: string[] = [];
  const fakeFetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const target = String(input);
    const body = typeof init?.body === "string" ? init.body : null;
    seen.push({ url: target, method: init?.method ?? "GET", body });
    if (target === INDEXNOW_ENDPOINT) return new Response("refused", { status: indexNowStatus });
    const page = site[target];
    return page === undefined ? new Response("", { status: 404 }) : new Response(page);
  }) as typeof fetch;
  const deps: RunDeps = {
    env: { INDEXNOW_KEY: KEY, SITE_ORIGIN: ORIGIN, GITHUB_STEP_SUMMARY: "summary.md", ...env },
    fetch: fakeFetch,
    today: "2026-10-29",
    readFile: (path) => files.get(path) ?? null,
    writeFile: (path, text) => files.set(path, text),
    appendFile: (path, text) => files.set(path, (files.get(path) ?? "") + text),
    log: (line) => lines.push(line),
  };
  return { deps, seen, files, lines };
}

/** A live site with two sections: undated site pages and dated docs. */
const SITE: Readonly<Record<string, string>> = {
  [`${ORIGIN}/${KEY}.txt`]: `${KEY}\n`,
  [`${ORIGIN}/sitemap.xml`]: sitemapIndexXml(ORIGIN, [
    { path: "/sitemap-site.xml", lastmod: null },
    { path: "/sitemap-guide.xml", lastmod: day("2026-10-27") },
  ]),
  [`${ORIGIN}/sitemap-site.xml`]: sitemapXml(ORIGIN, [{ path: "/", lastmod: null }]),
  [`${ORIGIN}/sitemap-guide.xml`]: sitemapXml(ORIGIN, [
    { path: "/docs/guide/stores", lastmod: day("2026-10-27") },
  ]),
  [`${ORIGIN}/`]: "# Bookclerk\n",
};

test("a first run submits every URL and keeps a snapshot", async () => {
  const h = harness(SITE);
  assert.equal(await run(["in.json", "out.json"], h.deps), 0);
  const posts = h.seen.filter((s) => s.method === "POST");
  assert.equal(posts.length, 1);
  assert.deepEqual(JSON.parse(posts[0]?.body ?? "").urlList, [
    `${ORIGIN}/`,
    `${ORIGIN}/docs/guide/stores`,
  ]);
  const snapshot = readSnapshot(h.files.get("out.json") ?? "", ORIGIN);
  assert.equal(snapshot?.get(url("/docs/guide/stores"))?.fp, "lastmod:2026-10-27");
  assert.match(snapshot?.get(url("/"))?.fp ?? "", /^sha256:[0-9a-f]{64}$/);
  assert.match(h.files.get("summary.md") ?? "", /2 submitted \(no previous snapshot/);
});

test("a run with nothing changed posts nothing", async () => {
  const first = harness(SITE);
  await run(["in.json", "out.json"], first.deps);
  const h = harness(SITE);
  h.files.set("in.json", first.files.get("out.json") ?? "");
  assert.equal(await run(["in.json", "out.json"], h.deps), 0);
  assert.equal(h.seen.filter((s) => s.method === "POST").length, 0);
  assert.match(h.lines.join("\n"), /submitting 0/);
});

test("a dry run plans without posting or keeping a snapshot", async () => {
  const h = harness(SITE, 200, { INDEXNOW_DRY_RUN: "1" });
  assert.equal(await run(["in.json", "out.json"], h.deps), 0);
  assert.equal(h.seen.filter((s) => s.method === "POST").length, 0);
  assert.equal(h.files.has("out.json"), false);
  assert.ok(h.lines.includes(`${ORIGIN}/docs/guide/stores`));
});

test("a run without a key does nothing", async () => {
  const h = harness(SITE, 200, { INDEXNOW_KEY: "" });
  assert.equal(await run(["in.json", "out.json"], h.deps), 0);
  assert.deepEqual(h.seen, []);
  assert.match(h.lines.join("\n"), /not set/);
});

test("bad configuration fails the run", async () => {
  assert.equal(await run([], harness(SITE).deps), 2);
  for (const env of [{ INDEXNOW_KEY: "bad key" }, { SITE_ORIGIN: "http://bookclerk.app" }]) {
    const h = harness(SITE, 200, env);
    assert.equal(await run(["in.json", "out.json"], h.deps), 1, JSON.stringify(env));
    assert.deepEqual(h.seen, []);
  }
  const clock = harness(SITE);
  assert.equal(await run(["in.json", "out.json"], { ...clock.deps, today: "soon" }), 1);
});

test("a run that cannot read the site skips without a snapshot", async () => {
  const cases: Record<string, string | undefined>[] = [
    { [`${ORIGIN}/${KEY}.txt`]: undefined },
    { [`${ORIGIN}/${KEY}.txt`]: "another-key\n" },
    { [`${ORIGIN}/sitemap.xml`]: sitemapXml(ORIGIN, [{ path: "/", lastmod: null }]) },
    {
      [`${ORIGIN}/sitemap.xml`]: sitemapIndexXml(ORIGIN, [{ path: "/docs/x", lastmod: null }]),
    },
    { [`${ORIGIN}/sitemap-guide.xml`]: undefined },
    { [`${ORIGIN}/sitemap-guide.xml`]: SITE[`${ORIGIN}/sitemap.xml`] },
  ];
  for (const change of cases) {
    const site: Record<string, string> = { ...SITE };
    for (const [key, value] of Object.entries(change)) {
      if (value === undefined) delete site[key];
      else site[key] = value;
    }
    const h = harness(site);
    assert.equal(await run(["in.json", "out.json"], h.deps), 0, JSON.stringify(change));
    assert.equal(h.seen.filter((s) => s.method === "POST").length, 0);
    assert.equal(h.files.has("out.json"), false);
    assert.match(h.lines.join("\n"), /::warning::IndexNow skipped/);
  }
});

test("a refused submission fails the run and keeps no snapshot", async () => {
  for (const status of [400, 403, 422, 429]) {
    const h = harness(SITE, status);
    assert.equal(await run(["in.json", "out.json"], h.deps), 1, String(status));
    assert.equal(h.files.has("out.json"), false);
    assert.match(h.lines.join("\n"), new RegExp(`::error::IndexNow answered ${status}: refused`));
    assert.equal(h.files.has("summary.md"), false, "no summary claims a submission");
  }
  const pending = harness(SITE, 202);
  assert.equal(await run(["in.json", "out.json"], pending.deps), 0, "202: key still checked");
  assert.ok(pending.files.has("out.json"));
});

test("a run that cannot reach the site skips without a snapshot", async () => {
  const h = harness(SITE);
  const failing = (async () => {
    throw new TypeError("fetch failed");
  }) as typeof fetch;
  assert.equal(await run(["in.json", "out.json"], { ...h.deps, fetch: failing }), 0);
  assert.equal(h.files.has("out.json"), false);
  assert.match(h.lines.join("\n"), /::warning::IndexNow skipped: .*fetch failed/);
});

test("a page that cannot be fetched keeps its previous fingerprint", async () => {
  const first = harness(SITE);
  await run(["in.json", "out.json"], first.deps);
  const before = readSnapshot(first.files.get("out.json") ?? "", ORIGIN);
  for (const body of [undefined, "x".repeat(10 * 1024 * 1024 + 1)]) {
    const site: Record<string, string> = { ...SITE };
    if (body === undefined) delete site[`${ORIGIN}/`];
    else site[`${ORIGIN}/`] = body;
    const h = harness(site);
    h.files.set("in.json", first.files.get("out.json") ?? "");
    assert.equal(await run(["in.json", "out.json"], h.deps), 0);
    assert.equal(h.seen.filter((s) => s.method === "POST").length, 0, "nothing changed");
    assert.match(h.lines.join("\n"), /::warning::.*keeps its previous fingerprint/);
    assert.deepEqual(readSnapshot(h.files.get("out.json") ?? "", ORIGIN), before);
  }
  // With no previous fingerprint the page waits for a run that can fetch it.
  const site: Record<string, string> = { ...SITE };
  delete site[`${ORIGIN}/`];
  const fresh = harness(site);
  assert.equal(await run(["in.json", "out.json"], fresh.deps), 0);
  const posted = fresh.seen.find((s) => s.method === "POST");
  assert.deepEqual(JSON.parse(posted?.body ?? "").urlList, [`${ORIGIN}/docs/guide/stores`]);
});

test("a declared oversized response is refused before it is read", async () => {
  const h = harness(SITE);
  let read = false;
  // No high-water mark, so the stream is pulled only when something reads it.
  const body = new ReadableStream<Uint8Array>(
    {
      pull(controller) {
        read = true;
        controller.close();
      },
    },
    { highWaterMark: 0 },
  );
  const big = (async () =>
    new Response(body, {
      headers: { "Content-Length": String(10 * 1024 * 1024 + 1) },
    })) as typeof fetch;
  assert.equal(await run(["in.json", "out.json"], { ...h.deps, fetch: big }), 0);
  assert.match(h.lines.join("\n"), /larger than the reader accepts/);
  assert.equal(read, false);
});

test("a body that fails while it is read skips the run", async () => {
  const h = harness(SITE);
  const broken = (async () =>
    new Response(
      new ReadableStream<Uint8Array>({
        pull(controller) {
          controller.error(new Error("connection reset"));
        },
      }),
    )) as typeof fetch;
  assert.equal(await run(["in.json", "out.json"], { ...h.deps, fetch: broken }), 0);
  assert.match(h.lines.join("\n"), /connection reset/);
  assert.equal(h.seen.filter((s) => s.method === "POST").length, 0);
});
