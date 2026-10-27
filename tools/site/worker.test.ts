// Tests for worker.ts: Accept parsing, path mapping and the responses the
// Worker gives for HTML and Markdown requests, against a real build served
// by a stand-in for the ASSETS binding and the repository's docs/ served by
// a stand-in for the docs repository. Run with `npm test`.

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";
import { after, before, test } from "node:test";

import { build, PAGES } from "./build.ts";
import {
  acceptWeights,
  docsTarget,
  DOCS_RAW_BASE,
  handle,
  htmlPath,
  markdownPath,
  MAX_DOC_BYTES,
  PAGE_MARKDOWN,
  pickEncoding,
  prefersMarkdown,
  SECURITY_HEADERS,
} from "./worker.ts";
import type { DocsFetch, Env, PageCache } from "./worker.ts";
import type { BuildEnv } from "./build.ts";
import type { DocsShell } from "./render.ts";

/** This directory. */
const HERE = new URL(".", import.meta.url).pathname;

/** Release state after two releases: v0.2.0 is the default docs version. */
const ENV: BuildEnv = {
  SITE_COMMIT: "b6adae4d0123456789abcdef0123456789abcdef",
  SITE_BUILT_ON: "2026-10-23",
  SITE_RELEASE_TAG: "v0.2.0",
  SITE_RELEASE_DATE: "2026-12-01",
  SITE_DOCS_VERSIONS: "v0.2.0 v0.1.0",
};

/** The repository's docs/, served as every version of the docs repository. */
const DOCS = join(HERE, "..", "..", "docs");

/** Generated pages the stand-in docs repository adds to docs/. */
const GENERATED: Readonly<Record<string, string>> = {
  "api/rust/demo/index.md": "# Crate `demo`\n\nA demo crate. See [the SDK](../../typescript/README.md) and [`Thing`](#thing).\n\n## Thing\n",
};

/** What a browser sends. */
const BROWSER = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/** Content types the stand-in assets serve, by extension. */
const TYPES: Readonly<Record<string, string>> = { ".html": "text/html; charset=utf-8", ".md": "text/markdown; charset=utf-8", ".css": "text/css" };

/** Temporary directory holding the build. */
let root = "";

/** The built dist/. */
let dist = "";

/** The docs shell the build baked. */
let shell: DocsShell;

/** URLs the stand-in docs repository was asked for. */
const fetched: string[] = [];

/** Refs the stand-in docs repository holds. */
const REFS = new Set(["main", "v0.1.0", "v0.2.0"]);

/**
 * Fails as the Workers runtime does on fetch options it does not support.
 *
 * Node's fetch accepts `redirect: "error"`, but the Workers runtime throws a
 * TypeError for anything other than "follow" or "manual", so a stand-in that
 * accepted it would hide a Worker that fails on every docs request.
 *
 * @param init - Fetch options.
 * @throws {TypeError} When `redirect` is not "follow" or "manual".
 */
function assertWorkersFetchInit(init: RequestInit): void {
  if (init.redirect !== undefined && init.redirect !== "follow" && init.redirect !== "manual") {
    throw new TypeError(`Invalid redirect value ${init.redirect}`);
  }
}

/**
 * Serves the repository's docs/ as raw.githubusercontent.com serves the docs
 * repository.
 *
 * @param url - The file's URL.
 * @param init - Fetch options.
 * @returns The file, or 404.
 */
async function docsFetch(url: string, init: RequestInit): Promise<Response> {
  fetched.push(url);
  assertWorkersFetchInit(init);
  assert.ok(url.startsWith(DOCS_RAW_BASE), url);
  const [ref = "", ...rest] = url.slice(DOCS_RAW_BASE.length).split("/");
  const file = rest.join("/");
  if (!REFS.has(ref)) return new Response("404: Not Found", { status: 404 });
  const generated = Object.hasOwn(GENERATED, file) ? GENERATED[file] : undefined;
  if (generated !== undefined) return new Response(generated);
  const path = join(DOCS, file);
  if (!file.includes("..") && existsSync(path) && statSync(path).isFile()) return new Response(readFileSync(path));
  return new Response("404: Not Found", { status: 404 });
}

/**
 * Serves dist/ roughly as Workers static assets do with
 * `html_handling = "auto-trailing-slash"` and `not_found_handling = "404-page"`.
 *
 * @param input - The asset request.
 * @returns The asset, or 404.html with status 404.
 */
async function assetsFetch(input: Request | URL | string): Promise<Response> {
  const path = new URL(input instanceof Request ? input.url : input).pathname;
  const candidates = path.endsWith("/") ? [`${path}index.html`] : [path, `${path}.html`];
  for (const candidate of candidates) {
    const file = join(dist, candidate);
    if (existsSync(file) && statSync(file).isFile()) {
      return new Response(readFileSync(file), { status: 200, headers: { "Content-Type": TYPES[extname(file)] ?? "application/octet-stream" } });
    }
  }
  return new Response(readFileSync(join(dist, "404.html")), { status: 404, headers: { "Content-Type": "text/html; charset=utf-8" } });
}

/** Bindings for handle(). */
const env: Env = { ASSETS: { fetch: assetsFetch } };

/**
 * Sends one request through the Worker.
 *
 * @param path - Request path.
 * @param accept - Accept header.
 * @param method - HTTP method.
 * @returns The Worker's response.
 */
function get(path: string, accept: string, method = "GET"): Promise<Response> {
  return handle(new Request(`https://bookclerk.app${path}`, { method, headers: { Accept: accept } }), env, { shell, fetch: docsFetch });
}

before(() => {
  root = mkdtempSync(join(tmpdir(), "bookclerk-site-worker-"));
  dist = join(root, "dist");
  build(ENV, dist, join(root, "docs-shell.json"));
  shell = JSON.parse(readFileSync(join(root, "docs-shell.json"), "utf8"));
});

after(() => rmSync(root, { recursive: true, force: true }));

test("acceptWeights reads quality values and counts wildcards as HTML", () => {
  assert.deepEqual(acceptWeights("text/markdown"), { markdown: 1, html: 0 });
  assert.deepEqual(acceptWeights("text/markdown;q=0.5, text/html"), { markdown: 0.5, html: 1 });
  assert.deepEqual(acceptWeights("*/*"), { markdown: 0, html: 1 });
  assert.deepEqual(acceptWeights("text/x-markdown; q=0.7, text/*;q=0.2"), { markdown: 0.7, html: 0.2 });
  assert.deepEqual(acceptWeights("text/markdown;q=banana"), { markdown: 0, html: 0 });
  assert.deepEqual(acceptWeights(null), { markdown: 0, html: 0 });
});

test("acceptWeights lets a named type override a wildcard", () => {
  assert.deepEqual(acceptWeights("text/markdown;q=0.5, text/html;q=0, */*;q=0.8"), { markdown: 0.5, html: 0 });
  assert.deepEqual(acceptWeights("text/*;q=0.9, text/html;q=0.1, text/markdown;q=0.5"), { markdown: 0.5, html: 0.1 });
  assert.deepEqual(acceptWeights("text/*;q=0.3, */*"), { markdown: 0, html: 0.3 });
  assert.equal(prefersMarkdown("text/markdown;q=0.5, text/html;q=0, */*;q=0.8"), true);
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

test("markdownPath and htmlPath map pages outside /docs/", () => {
  assert.equal(markdownPath("/"), "/index.md");
  assert.equal(markdownPath("/download"), "/download.md");
  assert.equal(markdownPath("/site.css"), null);
  assert.equal(htmlPath("/index.md"), "/");
  assert.equal(htmlPath("/download.md"), "/download");
  assert.equal(htmlPath("/llms.txt"), null);
});

test("docsTarget reads versions and paths", () => {
  const versions = [
    { ref: "v0.2.0", label: "v0.2.0 (latest)", base: "/docs/" },
    { ref: "main", label: "Development (main)", base: "/docs/v/main/" },
  ];
  const [latest, main] = versions;
  assert.deepEqual(docsTarget("/docs/", versions), { kind: "page", version: latest, keys: ["README", "index"] });
  assert.deepEqual(docsTarget("/docs/plugins", versions), { kind: "page", version: latest, keys: ["plugins", "plugins/README"] });
  assert.deepEqual(docsTarget("/docs/api/", versions), { kind: "page", version: latest, keys: ["api/README", "api/index", "api"] });
  assert.deepEqual(docsTarget("/docs/plugins.md", versions), { kind: "markdown", version: latest, keys: ["plugins"] });
  assert.deepEqual(docsTarget("/docs/v/main/sql-contract/v1", versions), {
    kind: "page",
    version: main,
    keys: ["sql-contract/v1", "sql-contract/v1/README"],
  });
  assert.deepEqual(docsTarget("/docs", versions), { kind: "redirect", location: "/docs/" });
  assert.deepEqual(docsTarget("/docs/v/main", versions), { kind: "redirect", location: "/docs/v/main/" });
  assert.deepEqual(docsTarget("/docs/v/v0.2.0/plugins", versions), { kind: "redirect", location: "/docs/plugins" });
  assert.deepEqual(docsTarget("/docs/plugins.html", versions), { kind: "redirect", location: "/docs/plugins" });
  assert.deepEqual(docsTarget("/docs/api/index.html", versions), { kind: "redirect", location: "/docs/api/" });
  assert.deepEqual(docsTarget("/docs/api/python/sdk.workerd", versions), {
    kind: "page",
    version: latest,
    keys: ["api/python/sdk.workerd", "api/python/sdk.workerd/README"],
  });
  for (const path of ["/docs/v/v9.9.9/plugins", "/docs/v/", "/docs/../worker.js", "/docs/.hidden", "/docs/a//b", "/docs/a/.git/x", "/docs/v/v0.2.0//evil.example"]) {
    assert.deepEqual(docsTarget(path, versions), { kind: "missing" }, path);
  }
});

test("every page with a Markdown version runs the Worker and is mapped", () => {
  const toml = readFileSync(join(HERE, "wrangler.toml"), "utf8");
  const routes: string[] = JSON.parse(toml.match(/^run_worker_first = (\[.*\])$/m)?.[1] ?? "[]");
  assert.ok(routes.includes("/docs/*") && routes.includes("/docs"));
  for (const page of PAGES.filter((p) => p.markdown)) {
    assert.ok(routes.includes(page.markdown ?? ""), `${page.markdown} not in run_worker_first`);
    if (page.alternate === false) continue;
    assert.equal(PAGE_MARKDOWN.get(page.path), page.markdown, page.path);
    assert.ok(routes.includes(page.path), `${page.path} not in run_worker_first`);
  }
  for (const path of PAGE_MARKDOWN.keys()) assert.ok(routes.includes(path), `${path} not in run_worker_first`);
});

test("the Worker sends the same security headers as src/_headers", () => {
  const block = readFileSync(join(HERE, "src", "_headers"), "utf8").split(/\n\s*\n/)[0] ?? "";
  const fromFile = Object.fromEntries(
    block
      .split("\n")
      .slice(1)
      .map((line) => line.trim())
      .filter(Boolean)
      .map((line) => [line.slice(0, line.indexOf(":")), line.slice(line.indexOf(":") + 1).trim()]),
  );
  assert.deepEqual(fromFile, SECURITY_HEADERS);
});

test("the CSP lets only the site's own scripts and Cloudflare's analytics beacon run", () => {
  const directives: Record<string, string[]> = Object.fromEntries(
    (SECURITY_HEADERS["Content-Security-Policy"] ?? "").split(";").map((d) => {
      const [name, ...sources] = d.trim().split(/\s+/);
      return [name, sources];
    }),
  );
  assert.deepEqual(directives["default-src"], ["'none'"]);
  assert.deepEqual(directives["script-src"], ["'self'", "https://static.cloudflareinsights.com"]);
  assert.deepEqual(directives["connect-src"], ["'self'", "https://cloudflareinsights.com"]);
  assert.equal(directives["script-src-attr"], undefined);
  for (const [name, sources] of Object.entries(directives)) {
    if (name === "script-src" || name === "connect-src") continue;
    for (const source of sources) assert.ok(source.startsWith("'"), `${name} names a host: ${source}`);
  }
});

test("a Markdown request for a page gets its Markdown version", async () => {
  for (const [path, heading, canonical] of [
    ["/", "# Bookclerk: every purchase, properly placed", "/"],
    ["/download", "# Get Bookclerk", "/download"],
    ["/docs/", "# ", "/docs/"],
    ["/docs/guide/getting-started", "# ", "/docs/guide/getting-started"],
    ["/docs/v/main/plugins/", "# ", "/docs/v/main/plugins/"],
  ] as const) {
    const res = await get(path, "text/markdown");
    assert.equal(res.status, 200, path);
    assert.equal(res.headers.get("Content-Type"), "text/markdown; charset=utf-8", path);
    assert.match(res.headers.get("Vary") ?? "", /Accept/, path);
    assert.equal(res.headers.get("Link"), `<https://bookclerk.app${canonical}>; rel="canonical"`, path);
    assert.equal(res.headers.get("Content-Security-Policy"), SECURITY_HEADERS["Content-Security-Policy"], path);
    const body = await res.text();
    assert.ok(body.startsWith(heading), `${path}: ${body.slice(0, 40)}`);
    assert.doesNotMatch(body, /<html|\{\{|%%/, path);
  }
});

test("the Markdown version links to the site's own Markdown copies", async () => {
  const body = await (await get("/docs/plugins/README.md", "*/*")).text();
  assert.ok(body.includes("](/docs/"));
  assert.doesNotMatch(body, /\]\(https:\/\/raw\.githubusercontent\.com\//, "no link points at the docs repository's raw files");
  assert.ok(!/\]\((?!https?:|\/|#|mailto:)[^)]*\.md/.test(body));
  const main = await (await get("/docs/v/main/plugins/README.md", "*/*")).text();
  assert.ok(main.includes("](/docs/v/main/"));
});

test("the other spelling of a doc path redirects to the doc", async () => {
  for (const [path, location] of [
    ["/docs/guide/getting-started/", "/docs/guide/getting-started"],
    ["/docs/api", "/docs/api/"],
    ["/docs/plugins.html", "/docs/plugins"],
    ["/docs/v/v0.2.0/plugins/?x=1", "/docs/plugins/?x=1"],
    ["/docs", "/docs/"],
  ] as const) {
    const res = await get(path, BROWSER);
    assert.equal(res.status, 301, path);
    assert.equal(res.headers.get("Location"), `https://bookclerk.app${location}`, path);
  }
});

test("a docs link that leads nowhere goes to the docs home page", async () => {
  for (const [path, accept, location] of [
    ["/docs/no-such-doc", BROWSER, "/docs/"],
    ["/docs/getting-started?x=1", BROWSER, "/docs/"],
    ["/docs/no-such-doc", "text/markdown", "/docs/"],
    ["/docs/no-such-doc.md", "*/*", "/docs/README.md"],
    ["/docs/v/main/no-such-doc", BROWSER, "/docs/v/main/"],
    ["/docs/v/v9.9.9/plugins/", BROWSER, "/docs/"],
    ["/docs/constructor", BROWSER, "/docs/"],
  ] as const) {
    const res = await get(path, accept);
    assert.equal(res.status, 302, path);
    assert.equal(res.headers.get("Location"), `https://bookclerk.app${location}`, path);
    assert.equal(res.headers.get("Cache-Control"), "no-store", path);
  }
});

test("a missing docs home page is a 404, not a redirect loop", async () => {
  const empty: DocsFetch = async () => new Response("404: Not Found", { status: 404 });
  for (const [path, accept] of [["/docs/", BROWSER], ["/docs/README.md", "*/*"]] as const) {
    const res = await handle(new Request(`https://bookclerk.app${path}`, { headers: { Accept: accept } }), env, { shell, fetch: empty });
    assert.equal(res.status, 404, path);
    assert.equal(res.headers.get("X-Robots-Tag"), "noindex", path);
  }
});

test("only listed versions and plain paths reach the docs repository", async () => {
  fetched.length = 0;
  for (const path of ["/docs/v/v9.9.9/plugins/", "/docs/v/feature/plugins/", "/docs/%2e%2e/secret", "/docs/.env", "/docs/a%00b"]) {
    // Bad docs paths go to the docs home page; `%2e%2e` leaves /docs/ and is a plain 404.
    assert.ok([302, 404].includes((await get(path, BROWSER)).status), path);
  }
  assert.deepEqual(fetched, []);
  await get("/docs/v/v0.1.0/plugins/", BROWSER);
  assert.deepEqual(fetched, [`${DOCS_RAW_BASE}v0.1.0/plugins/README.md`]);
});

test("a browser gets the doc in the site's page, with its version and sidebar", async () => {
  const res = await get("/docs/plugins/", BROWSER);
  assert.equal(res.status, 200);
  assert.match(res.headers.get("Content-Type") ?? "", /^text\/html/);
  assert.equal(res.headers.get("Vary"), "Accept, Accept-Encoding");
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=300, s-maxage=86400");
  assert.equal(res.headers.get("X-Robots-Tag"), null);
  assert.equal(res.headers.get("Link"), '<https://bookclerk.app/docs/plugins/README.md>; rel="alternate"; type="text/markdown"');
  const html = await res.text();
  assert.match(html, /^<!doctype html>/);
  assert.doesNotMatch(html, /%%|<!--doc-|\{\{/);
  assert.match(html, /<link rel="canonical" href="https:\/\/bookclerk.app\/docs\/plugins\/">/);
  assert.match(html, /<meta property="og:type" content="article">/);
  const description = html.match(/<meta name="description" content="([^"]*)">/)?.[1] ?? "";
  assert.ok(description.length > 40 && description.length <= 160, description);
  assert.match(html, /<a href="\/docs\/plugins\/" aria-current="page">/);
  assert.match(html, /<strong>v0.2.0 \(latest\)<\/strong>/);
  assert.match(html, /href="\/docs\/v\/main\/plugins\/"/);
  assert.match(html, /href="https:\/\/github.com\/Bookclerk\/bookclerk\/blob\/v0.2.0\/docs\/plugins\/README.md"/);
  assert.ok(!html.includes("doc-note"), "a release's docs carry no development note");
  assert.match(html, /<nav class="crumbs" aria-label="Breadcrumb"><ol><li><a href="\/docs\/">Docs<\/a><\/li><li>Build plugins<\/li>/);
  assert.match(html, /<aside class="toc" aria-label="On this page">/);
  assert.match(html, /<div class="docs-menus"><details class="menu menu-nav"><summary>Menu<\/summary>/);
  assert.match(html, /<details class="nav-group" open><summary>Build plugins<\/summary>/);
  assert.match(html, /<script src="\/docs.js" defer><\/script>/);
  const ld: { "@graph": { "@type": string; url: string }[] } = JSON.parse(html.match(/<script type="application\/ld\+json">(.*?)<\/script>/)?.[1] ?? "{}");
  assert.equal(ld["@graph"][0]?.["@type"], "TechArticle");
  assert.equal(ld["@graph"][0]?.url, "https://bookclerk.app/docs/plugins/");

  const home = await get("/", "*/*");
  assert.equal(home.headers.get("Link"), '<https://bookclerk.app/index.md>; rel="alternate"; type="text/markdown"');
});

test("main's docs say they are the development version and link within main", async () => {
  const res = await get("/docs/v/main/plugins/", BROWSER);
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=300, s-maxage=300");
  assert.equal(res.headers.get("X-Robots-Tag"), "noindex", "only the default version is indexed");
  const html = await res.text();
  assert.match(html, /class="doc-note"/);
  assert.match(html, /<a href="\/docs\/v\/main\/plugins\/" aria-current="page">/);
  assert.match(html, /<link rel="canonical" href="https:\/\/bookclerk.app\/docs\/v\/main\/plugins\/">/);
  assert.match(html, /unstable-badge/);
});

test("generated pages render without a source link and keep links inside the tree", async () => {
  const html = await (await get("/docs/api/rust/demo/index", BROWSER)).text();
  assert.match(html, /Generated from the source at v0.2.0/);
  assert.match(html, /href="\/docs\/api\/typescript\/"/);
  assert.match(html, /<h2 id="thing">/);
});

test("a directory whose entry page is index serves it", async () => {
  const dir = await get("/docs/api/rust/demo/", BROWSER);
  assert.equal(dir.status, 301);
  assert.equal(dir.headers.get("Location"), "https://bookclerk.app/docs/api/rust/demo/index");
  const old = await get("/docs/api/rust/demo/index.html", BROWSER);
  assert.equal(old.status, 301);
  assert.equal(old.headers.get("Location"), "https://bookclerk.app/docs/api/rust/demo/");
});

test("raw HTML in a doc is shown as text", async () => {
  // docs/server/cluster-hosts.md says "Let **<branch>** join".
  assert.match(await (await get("/docs/server/cluster-hosts", BROWSER)).text(), /&lt;branch&gt;/);
});

test("a failing or oversized docs repository answer is a 503, not a page", async () => {
  const down = async () => new Response("oops", { status: 502 });
  const big = async () => new Response("x", { headers: { "Content-Length": String(MAX_DOC_BYTES + 1) } });
  // No Content-Length: the body is cut off once it passes the cap.
  const chunk = new Uint8Array(1024 * 1024);
  let sent = 0;
  const endless = async () =>
    new Response(
      new ReadableStream({
        pull(controller) {
          sent += chunk.byteLength;
          controller.enqueue(chunk);
        },
      }),
    );
  const moved: DocsFetch = async (_url, init) => {
    assertWorkersFetchInit(init);
    assert.equal(init.redirect, "manual");
    return new Response(null, { status: 302, headers: { Location: "https://example.com/" } });
  };
  const fetchers: DocsFetch[] = [down, big, endless, moved, async () => Promise.reject(new Error("network"))];
  for (const fetcher of fetchers) {
    const req = new Request("https://bookclerk.app/docs/plugins/", { headers: { Accept: BROWSER } });
    const res = await handle(req, env, { shell, fetch: fetcher });
    assert.equal(res.status, 503);
    assert.equal(res.headers.get("Cache-Control"), "no-store");
  }
  assert.ok(sent <= MAX_DOC_BYTES + 2 * chunk.byteLength, `read ${sent} bytes`);
});

test("the docs-unavailable page keeps the site's look and links to the doc on GitHub", async () => {
  const down = async () => new Response("oops", { status: 502 });
  const page = await handle(new Request("https://bookclerk.app/docs/plugins/", { headers: { Accept: BROWSER } }), env, { shell, fetch: down });
  assert.equal(page.status, 503);
  assert.equal(page.headers.get("Content-Type"), "text/html; charset=utf-8");
  assert.equal(page.headers.get("Retry-After"), "60");
  assert.equal(page.headers.get("X-Robots-Tag"), "noindex");
  assert.equal(page.headers.get("Content-Security-Policy"), SECURITY_HEADERS["Content-Security-Policy"]);
  const html = await page.text();
  assert.match(html, /<link rel="stylesheet" href="\/site\.css/);
  assert.match(html, /class="docs-nav"/);
  assert.doesNotMatch(html, /<h2>Using Bookclerk<\/h2>/, "no sidebar links that would fail too");
  assert.match(html, /<h1>The docs could not be loaded<\/h1>/);
  assert.ok(html.includes('href="https://github.com/Bookclerk/bookclerk-docs/blob/v0.2.0/plugins/README.md"'), "links the doc on GitHub");
  assert.ok(html.includes('href="https://github.com/Bookclerk/bookclerk-docs/tree/v0.2.0"'), "links every doc on GitHub");

  // A directory names its README; Markdown readers get the same links.
  const md = await handle(new Request("https://bookclerk.app/docs/api/", { headers: { Accept: "text/markdown" } }), env, { shell, fetch: down });
  assert.equal(md.status, 503);
  assert.equal(md.headers.get("Content-Type"), "text/markdown; charset=utf-8");
  assert.match(await md.text(), /<https:\/\/github\.com\/Bookclerk\/bookclerk-docs\/blob\/v0\.2\.0\/api\/README\.md>/);

  // When the first key is missing and the next one fails, GitHub gets the one that failed.
  const missingThenDown: DocsFetch = async (url) => (url.endsWith("/plugins/README.md") ? new Response("404: Not Found", { status: 404 }) : down());
  const later = await (await handle(new Request("https://bookclerk.app/docs/plugins/", { headers: { Accept: BROWSER } }), env, { shell, fetch: missingThenDown })).text();
  assert.ok(later.includes('href="https://github.com/Bookclerk/bookclerk-docs/blob/v0.2.0/plugins/index.md"'), "links the key that failed");
  assert.ok(later.includes('href="/docs/plugins/"'), "tries the page's own URL again");

  const head = await handle(new Request("https://bookclerk.app/docs/plugins/", { method: "HEAD" }), env, { shell, fetch: down });
  assert.equal(head.status, 503);
  assert.equal(await head.text(), "");
});

test("rendered pages are cached per deploy, version, path and format", async () => {
  const store = new Map<string, Response>();
  const cache: PageCache = {
    match: async (req) => store.get(req.url)?.clone(),
    put: async (req, res) => void store.set(req.url, res),
  };
  const pending: Promise<unknown>[] = [];
  const deps = { shell, fetch: docsFetch, cache, waitUntil: (p: Promise<unknown>) => void pending.push(p) };
  fetched.length = 0;
  for (let i = 0; i < 2; i += 1) {
    const res = await handle(new Request("https://bookclerk.app/docs/plugins/?utm=x", { headers: { Accept: BROWSER } }), env, deps);
    assert.equal(res.status, 200);
    await Promise.all(pending);
  }
  assert.equal(fetched.length, 1);
  assert.deepEqual([...store.keys()], [`https://bookclerk.app/docs/plugins/?format=html&ref=v0.2.0&shell=${shell.id}`]);
  assert.match(shell.id, /^[0-9a-f]{16}$/);
  const head = await handle(new Request("https://bookclerk.app/docs/plugins/", { method: "HEAD", headers: { Accept: BROWSER } }), env, deps);
  assert.equal(head.status, 200);
  assert.equal(await head.text(), "");
});

test("Markdown files fetched by name point at their pages", async () => {
  const home = await get("/index.md", "*/*");
  assert.equal(home.headers.get("Link"), '<https://bookclerk.app/>; rel="canonical"');
  const missing = await get("/404.md", "*/*");
  assert.equal(missing.headers.get("X-Robots-Tag"), "noindex");
  assert.equal(missing.headers.get("Content-Type"), "text/markdown; charset=utf-8");
});

test("a Markdown request passes conditional headers to the assets", async () => {
  const seen: (string | null)[] = [];
  const spy: Env = {
    ASSETS: {
      fetch: async (_input, init) => {
        seen.push(new Headers(init?.headers).get("If-None-Match"));
        return new Response(null, { status: 304, headers: { ETag: '"abc"' } });
      },
    },
  };
  const req = new Request("https://bookclerk.app/download", { headers: { Accept: "text/markdown", "If-None-Match": '"abc"' } });
  const res = await handle(req, spy, { shell });
  assert.equal(res.status, 304);
  assert.deepEqual(seen, ['"abc"']);
});

test("other methods are passed to the assets unchanged", async () => {
  const res = await get("/", "text/markdown", "POST");
  assert.match(res.headers.get("Content-Type") ?? "", /^text\/html/);
});

test("pickEncoding prefers Brotli, honours quality values and refuses q=0", () => {
  assert.equal(pickEncoding("gzip, deflate, br, zstd"), "br");
  assert.equal(pickEncoding("gzip"), "gzip");
  assert.equal(pickEncoding("br;q=0.5, gzip"), "gzip");
  assert.equal(pickEncoding("br;q=0, gzip;q=0"), null);
  assert.equal(pickEncoding("*"), "br");
  assert.equal(pickEncoding("*, br;q=0"), "gzip");
  assert.equal(pickEncoding("identity"), null);
  assert.equal(pickEncoding(""), null);
  assert.equal(pickEncoding(null), null);
});

test("text answers are compressed for clients that accept it", async () => {
  for (const [path, accept] of [
    ["/", BROWSER],
    ["/", "text/markdown"],
    ["/download.md", "*/*"],
    ["/docs/plugins/", BROWSER],
    ["/docs/plugins/", "text/markdown"],
    ["/no-such-page", BROWSER],
  ] as const) {
    const req = new Request(`https://bookclerk.app${path}`, { headers: { Accept: accept, "Accept-Encoding": "gzip, deflate, br" } });
    const res = await handle(req, env, { shell, fetch: docsFetch });
    assert.equal(res.headers.get("Content-Encoding"), "br", `${path} as ${accept}`);
    assert.equal(res.headers.get("Content-Length"), null, path);
    assert.ok((res.headers.get("Vary") ?? "").split(", ").includes("Accept-Encoding"), path);
  }
  const gzipOnly = await handle(new Request("https://bookclerk.app/", { headers: { "Accept-Encoding": "gzip" } }), env, { shell });
  assert.equal(gzipOnly.headers.get("Content-Encoding"), "gzip");
  // Cloudflare rewrites Accept-Encoding before the Worker runs; what the client sent wins.
  const viaEdge = new Request("https://bookclerk.app/", { headers: { "Accept-Encoding": "gzip, br" } });
  Object.defineProperty(viaEdge, "cf", { value: { clientAcceptEncoding: "identity" } });
  assert.equal((await handle(viaEdge, env, { shell })).headers.get("Content-Encoding"), null);
});

test("compression leaves alone what must not or need not be compressed", async () => {
  const plain = await get("/", BROWSER);
  assert.equal(plain.headers.get("Content-Encoding"), null);
  assert.ok((plain.headers.get("Vary") ?? "").split(", ").includes("Accept-Encoding"));
  const answer = (headers: Record<string, string>, status = 200): Env => ({ ASSETS: { fetch: async () => new Response(status === 304 ? null : "x", { status, headers }) } });
  const cases: [Record<string, string>, number][] = [
    [{ "Content-Type": "text/html", "Content-Encoding": "gzip" }, 200],
    [{ "Content-Type": "text/html", "Cache-Control": "public, no-transform" }, 200],
    [{ "Content-Type": "image/png" }, 200],
    [{ "Content-Type": "text/html", "Content-Range": "bytes 0-0/20" }, 206],
  ];
  for (const [headers, status] of cases) {
    const req = new Request("https://bookclerk.app/download", { headers: { "Accept-Encoding": "br" } });
    const res = await handle(req, answer(headers, status), { shell });
    assert.equal(res.headers.get("Content-Encoding"), headers["Content-Encoding"] ?? null, JSON.stringify(headers));
  }
  const tagged = await handle(new Request("https://bookclerk.app/download", { headers: { "Accept-Encoding": "br" } }), answer({ "Content-Type": "text/html", ETag: '"abc"' }), { shell });
  assert.equal(tagged.headers.get("ETag"), 'W/"abc"');
  for (const headers of [{ "Content-Type": "text/html", ETag: '"abc"' }, { ETag: '"abc"' }]) {
    // A 304 keeps the compressed answer's Vary and weak validator, with no body to encode.
    const req = new Request("https://bookclerk.app/download", { headers: { "Accept-Encoding": "br", "If-None-Match": 'W/"abc"' } });
    const res = await handle(req, answer(headers, 304), { shell });
    assert.equal(res.status, 304);
    assert.equal(res.headers.get("Content-Encoding"), null);
    assert.equal(res.headers.get("ETag"), 'W/"abc"');
    assert.ok((res.headers.get("Vary") ?? "").split(", ").includes("Accept-Encoding"));
  }
  const md304 = await handle(
    new Request("https://bookclerk.app/download", { headers: { Accept: "text/markdown", "Accept-Encoding": "gzip", "If-None-Match": 'W/"abc"' } }),
    answer({ ETag: '"abc"' }, 304),
    { shell },
  );
  assert.equal(md304.headers.get("Vary"), "Accept, Accept-Encoding");
  assert.equal(md304.headers.get("ETag"), 'W/"abc"');
  const post = await handle(new Request("https://bookclerk.app/", { method: "POST", headers: { "Accept-Encoding": "br" } }), env, { shell });
  assert.equal(post.headers.get("Content-Encoding"), null);
});
