// Tests for worker.js: Accept parsing, path mapping and the responses the
// Worker gives for HTML and Markdown requests, against a real build served
// by a stand-in for the ASSETS binding. Run with `npm test`.

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";
import { after, before, test } from "node:test";

import { build } from "./build.mjs";
import { acceptWeights, handle, htmlPath, markdownPath, prefersMarkdown, SECURITY_HEADERS } from "./worker.js";

/** Release state of a repository with no release yet. */
const ENV = { SITE_COMMIT: "b6adae4d0123456789abcdef0123456789abcdef", SITE_BUILT_ON: "2026-10-23" };

/** What a browser sends. */
const BROWSER = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/** Content types the stand-in assets serve, by extension. */
const TYPES = { ".html": "text/html; charset=utf-8", ".md": "text/markdown; charset=utf-8", ".css": "text/css" };

let dist;

/**
 * Serves dist/ roughly as Workers static assets do with
 * `html_handling = "auto-trailing-slash"` and `not_found_handling = "404-page"`.
 *
 * @param {Request | URL | string} input The asset request.
 * @returns {Promise<Response>} The asset, or 404.html with status 404.
 */
async function assetsFetch(input) {
  const path = new URL(input instanceof Request ? input.url : input).pathname;
  const candidates = path.endsWith("/") ? [`${path}index.html`] : [path, `${path}.html`];
  for (const candidate of candidates) {
    const file = join(dist, candidate);
    if (existsSync(file) && statSync(file).isFile()) {
      return new Response(readFileSync(file), { status: 200, headers: { "Content-Type": TYPES[extname(file)] ?? "application/octet-stream" } });
    }
  }
  return new Response(readFileSync(join(dist, "404.html")), { status: 404, headers: { "Content-Type": TYPES[".html"] } });
}

/** Bindings for handle(). */
const env = { ASSETS: { fetch: assetsFetch } };

/**
 * Sends one request through the Worker.
 *
 * @param {string} path Request path.
 * @param {string} accept Accept header.
 * @param {string} [method] HTTP method.
 * @returns {Promise<Response>} The Worker's response.
 */
function get(path, accept, method = "GET") {
  return handle(new Request(`https://bookclerk.app${path}`, { method, headers: { Accept: accept } }), env);
}

before(() => {
  dist = mkdtempSync(join(tmpdir(), "bookclerk-site-worker-"));
  build(ENV, dist);
});

after(() => rmSync(dist, { recursive: true, force: true }));

test("acceptWeights reads quality values and counts wildcards as HTML", () => {
  assert.deepEqual(acceptWeights("text/markdown"), { markdown: 1, html: 0 });
  assert.deepEqual(acceptWeights("text/markdown;q=0.5, text/html"), { markdown: 0.5, html: 1 });
  assert.deepEqual(acceptWeights("*/*"), { markdown: 0, html: 1 });
  assert.deepEqual(acceptWeights("text/x-markdown; q=0.7, text/*;q=0.2"), { markdown: 0.7, html: 0.2 });
  assert.deepEqual(acceptWeights("text/markdown;q=banana"), { markdown: 0, html: 0 });
  assert.deepEqual(acceptWeights(null), { markdown: 0, html: 0 });
});

test("prefersMarkdown only when Markdown ranks at least as high as HTML", () => {
  assert.equal(prefersMarkdown("text/markdown"), true);
  assert.equal(prefersMarkdown("text/markdown, text/html;q=0.9"), true);
  assert.equal(prefersMarkdown("text/markdown, text/html"), true);
  assert.equal(prefersMarkdown("text/html, text/markdown;q=0.9"), false);
  assert.equal(prefersMarkdown(BROWSER), false);
  assert.equal(prefersMarkdown("*/*"), false);
  assert.equal(prefersMarkdown(""), false);
  assert.equal(prefersMarkdown("text/markdown;q=0"), false);
});

test("markdownPath maps pages to their Markdown versions", () => {
  assert.equal(markdownPath("/"), "/index.md");
  assert.equal(markdownPath("/download"), "/download.md");
  assert.equal(markdownPath("/docs/"), "/docs/README.md");
  assert.equal(markdownPath("/docs/getting-started"), "/docs/getting-started.md");
  assert.equal(markdownPath("/docs/sql-contract/"), "/docs/sql-contract/README.md");
  assert.equal(markdownPath("/docs/sql-contract/v1"), "/docs/sql-contract/v1.md");
  assert.equal(markdownPath("/docs/plugins.html"), "/docs/plugins.md");
  assert.equal(markdownPath("/docs/plugins.md"), null);
  assert.equal(markdownPath("/site.css"), null);
  assert.equal(markdownPath("/docs/../worker.js"), null);
  assert.equal(markdownPath("/docs/.hidden"), null);
});

test("htmlPath maps Markdown versions back to their pages", () => {
  assert.equal(htmlPath("/index.md"), "/");
  assert.equal(htmlPath("/download.md"), "/download");
  assert.equal(htmlPath("/docs/README.md"), "/docs/");
  assert.equal(htmlPath("/docs/sql-contract/README.md"), "/docs/sql-contract/");
  assert.equal(htmlPath("/docs/plugins.md"), "/docs/plugins");
  assert.equal(htmlPath("/llms.txt"), null);
});

test("a Markdown request for a page gets its Markdown version", async () => {
  for (const [path, heading, canonical] of [
    ["/", "# Bookclerk: every purchase, properly placed", "/"],
    ["/download", "# Get Bookclerk", "/download"],
    ["/docs/", "# ", "/docs/"],
    ["/docs/getting-started", "# ", "/docs/getting-started"],
    ["/docs/api", "# ", "/docs/api/"],
  ]) {
    const res = await get(path, "text/markdown");
    assert.equal(res.status, 200, path);
    assert.equal(res.headers.get("Content-Type"), "text/markdown; charset=utf-8", path);
    assert.match(res.headers.get("Vary"), /Accept/, path);
    assert.equal(res.headers.get("Link"), `<https://bookclerk.app${canonical}>; rel="canonical"`, path);
    assert.equal(res.headers.get("Content-Security-Policy"), SECURITY_HEADERS["Content-Security-Policy"], path);
    const body = await res.text();
    assert.ok(body.startsWith(heading), `${path}: ${body.slice(0, 40)}`);
    assert.doesNotMatch(body, /<html|\{\{/, path);
  }
});

test("a Markdown request for a missing doc gets a Markdown 404", async () => {
  const res = await get("/docs/no-such-doc", "text/markdown");
  assert.equal(res.status, 404);
  assert.equal(res.headers.get("Content-Type"), "text/markdown; charset=utf-8");
  assert.equal(res.headers.get("X-Robots-Tag"), "noindex");
  assert.match(await res.text(), /^# We could not find that page/);
});

test("a browser gets HTML that links its Markdown version", async () => {
  const res = await get("/docs/plugins", BROWSER);
  assert.equal(res.status, 200);
  assert.match(res.headers.get("Content-Type"), /^text\/html/);
  assert.equal(res.headers.get("Vary"), "Accept");
  assert.equal(res.headers.get("Link"), '<https://bookclerk.app/docs/plugins.md>; rel="alternate"; type="text/markdown"');
  assert.match(await res.text(), /^<!doctype html>/);
  const home = await get("/", "*/*");
  assert.equal(home.headers.get("Link"), '<https://bookclerk.app/index.md>; rel="alternate"; type="text/markdown"');
});

test("a Markdown copy fetched by name points back at its page", async () => {
  const res = await get("/docs/plugins.md", "*/*");
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("Content-Type"), "text/markdown; charset=utf-8");
  assert.equal(res.headers.get("Link"), '<https://bookclerk.app/docs/plugins>; rel="canonical"');
});

test("other methods are passed to the assets unchanged", async () => {
  const res = await get("/", "text/markdown", "POST");
  assert.match(res.headers.get("Content-Type"), /^text\/html/);
});
