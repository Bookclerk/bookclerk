// Tests for worker.ts: Accept parsing, path mapping and the responses the
// Worker gives for HTML and Markdown requests, against a real build served
// by a stand-in for the ASSETS binding and the repository's docs/ served by
// a stand-in for the docs repository. Run with `npm test`.

import assert from "node:assert/strict";
import { inject, test } from "vitest";

import { PAGES } from "./build.ts";
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
  sitemapSection,
  sitePageLocation,
  withCatalogSample,
} from "./worker.ts";
import { catalogGroups } from "./catalog.ts";
import type { DocsFetch, Env, PageCache } from "./worker.ts";
import { docPageTitle, docUrl, TITLE_MAX } from "./render.ts";
import type { DocsShell, SearchPage } from "./render.ts";
import { MAX_LAST_UPDATED_BYTES } from "./last-updated.ts";
// Read by Vite when it transforms this test: it runs inside the Workers
// runtime, which has no view of the repository.
import HEADERS_FILE from "./src/_headers?raw";
import WRANGLER_TOML from "./wrangler.toml?raw";

/**
 * The repository's docs/, served as every version of the docs repository:
 * text by path from docs/. The generated API reference that
 * scripts/generate-api-docs.sh may have written there is left out, as on CI
 * (GENERATED stands in for it).
 */
const DOCS: Readonly<Record<string, string>> = Object.fromEntries(
  Object.entries(
    import.meta.glob<string>(
      [
        "../../docs/**/*",
        "!../../docs/api/rust/**",
        "!../../docs/api/typescript/**",
        "!../../docs/api/ui/**",
        "!../../docs/api/python/**",
      ],
      { query: "?raw", import: "default", eager: true },
    ),
  ).map(([path, text]) => [path.slice("../../docs/".length), text]),
);

/** Generated pages the stand-in docs repository adds to docs/. */
const GENERATED: Readonly<Record<string, string>> = {
  "api/rust/demo/index.md":
    "# Crate `demo`\n\nA demo crate. See [the SDK](../../typescript/README.md) and [`Thing`](#thing).\n\n## Thing\n",
  "api/web/README.md":
    "# Web API\n\n## Routes\n\n### [Jobs](jobs.md)\n\n| Route | Summary |\n| --- | --- |\n| [`GET /api/jobs`](jobs.md#get-apijobs) | Lists. |\n\n### [Health](health.md)\n\n| Route | Summary |\n| --- | --- |\n| [`GET /health`](health.md#get-health) | Liveness. |\n",
  "api/web/jobs.md": "# Jobs\n\n## `GET /api/jobs`\n\nLists jobs.\n",
  "last-updated.json": JSON.stringify({
    version: 1,
    pages: { "plugins/README": "2026-10-26", "api/rust/demo/index": "2026-10-01" },
  }),
};

/** What a browser sends. */
const BROWSER = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

/** Content types the stand-in assets serve, by extension. */
const TYPES: Readonly<Record<string, string>> = {
  ".html": "text/html; charset=utf-8",
  ".md": "text/markdown; charset=utf-8",
  ".css": "text/css",
};

/** The site built (for test-support/build-env.ts) by test-support/site-build.ts before the tests start. */
const BUILT = inject("siteBuild");

/** The docs shell the build baked. */
const shell: DocsShell = BUILT.shell;

/**
 * One built file.
 *
 * @param path - Path from dist/, such as `sitemap.xml`.
 * @returns Its bytes, or undefined when the build has no such file.
 */
function distFile(path: string): Uint8Array<ArrayBuffer> | undefined {
  const base64 = Object.hasOwn(BUILT.files, path) ? BUILT.files[path] : undefined;
  return base64 === undefined ? undefined : Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
}

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
  const doc = Object.hasOwn(DOCS, file) ? DOCS[file] : undefined;
  if (doc !== undefined) return new Response(doc);
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
    const body = distFile(candidate.slice(1));
    if (body !== undefined) {
      const ext = /\.[^./]*$/.exec(candidate)?.[0] ?? "";
      return new Response(body, {
        status: 200,
        headers: { "Content-Type": TYPES[ext] ?? "application/octet-stream" },
      });
    }
  }
  return new Response(distFile("404.html") ?? null, {
    status: 404,
    headers: { "Content-Type": "text/html; charset=utf-8" },
  });
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
  return handle(
    new Request(`https://bookclerk.app${path}`, { method, headers: { Accept: accept } }),
    env,
    { shell, fetch: docsFetch },
  );
}

test("acceptWeights reads quality values and counts wildcards as HTML", () => {
  assert.deepEqual(acceptWeights("text/markdown"), { markdown: 1, html: 0 });
  assert.deepEqual(acceptWeights("text/markdown;q=0.5, text/html"), { markdown: 0.5, html: 1 });
  assert.deepEqual(acceptWeights("*/*"), { markdown: 0, html: 1 });
  assert.deepEqual(acceptWeights("text/x-markdown; q=0.7, text/*;q=0.2"), {
    markdown: 0.7,
    html: 0.2,
  });
  assert.deepEqual(acceptWeights("text/markdown;q=banana"), { markdown: 0, html: 0 });
  assert.deepEqual(acceptWeights(null), { markdown: 0, html: 0 });
});

test("acceptWeights lets a named type override a wildcard", () => {
  assert.deepEqual(acceptWeights("text/markdown;q=0.5, text/html;q=0, */*;q=0.8"), {
    markdown: 0.5,
    html: 0,
  });
  assert.deepEqual(acceptWeights("text/*;q=0.9, text/html;q=0.1, text/markdown;q=0.5"), {
    markdown: 0.5,
    html: 0.1,
  });
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
  // Every redirect lands on the final URL in one hop.
  assert.deepEqual(docsTarget("/docs/v/v0.2.0/guide/install.html", versions), {
    kind: "redirect",
    location: "/docs/guide/install",
  });
  assert.deepEqual(docsTarget("/docs/v/v0.2.0/api/web/index.html", versions), {
    kind: "redirect",
    location: "/docs/api/web",
  });
  assert.deepEqual(docsTarget("/docs/a.html/index.html", versions), {
    kind: "redirect",
    location: "/docs/a",
  });
  assert.deepEqual(docsTarget("/docs", versions), {
    kind: "page",
    version: latest,
    keys: ["README", "index"],
  });
  assert.deepEqual(docsTarget("/docs/", versions), {
    kind: "page",
    version: latest,
    keys: ["README", "index"],
  });
  assert.deepEqual(docsTarget("/docs/plugins", versions), {
    kind: "page",
    version: latest,
    keys: ["plugins", "plugins/README", "plugins/index"],
  });
  assert.deepEqual(docsTarget("/docs/api/", versions), {
    kind: "page",
    version: latest,
    keys: ["api/README", "api/index", "api"],
  });
  assert.deepEqual(docsTarget("/docs/plugins.md", versions), {
    kind: "markdown",
    version: latest,
    keys: ["plugins"],
  });
  assert.deepEqual(docsTarget("/docs/v/main/sql-contract/v1", versions), {
    kind: "page",
    version: main,
    keys: ["sql-contract/v1", "sql-contract/v1/README", "sql-contract/v1/index"],
  });
  assert.deepEqual(docsTarget("/docs/v/main", versions), {
    kind: "page",
    version: main,
    keys: ["README", "index"],
  });
  assert.deepEqual(docsTarget("/docs/v/v0.2.0", versions), { kind: "redirect", location: "/docs" });
  assert.deepEqual(docsTarget("/docs/v/v0.2.0/", versions), {
    kind: "redirect",
    location: "/docs",
  });
  assert.deepEqual(docsTarget("/docs/index.html", versions), {
    kind: "redirect",
    location: "/docs",
  });
  assert.deepEqual(docsTarget("/docs/v/v0.2.0/plugins", versions), {
    kind: "redirect",
    location: "/docs/plugins",
  });
  assert.deepEqual(docsTarget("/docs/v/v0.2.0/plugins/", versions), {
    kind: "redirect",
    location: "/docs/plugins",
  });
  assert.deepEqual(docsTarget("/docs/plugins.html", versions), {
    kind: "redirect",
    location: "/docs/plugins",
  });
  assert.deepEqual(docsTarget("/docs/api/index.html", versions), {
    kind: "redirect",
    location: "/docs/api",
  });
  assert.deepEqual(docsTarget("/docs/api/python/sdk.workerd", versions), {
    kind: "page",
    version: latest,
    keys: [
      "api/python/sdk.workerd",
      "api/python/sdk.workerd/README",
      "api/python/sdk.workerd/index",
    ],
  });
  for (const path of [
    "/docs/v/v9.9.9/plugins",
    "/docs/v/",
    "/docs/../worker.js",
    "/docs/.hidden",
    "/docs/a//b",
    "/docs/a/.git/x",
    "/docs/v/v0.2.0//evil.example",
  ]) {
    assert.deepEqual(docsTarget(path, versions), { kind: "missing" }, path);
  }
});

test("every page with a Markdown version runs the Worker and is mapped", () => {
  const toml = WRANGLER_TOML;
  const routes: string[] = JSON.parse(toml.match(/^run_worker_first = (\[.*\])$/m)?.[1] ?? "[]");
  assert.ok(routes.includes("/docs/*") && routes.includes("/docs"));
  for (const page of PAGES.filter((p) => p.markdown)) {
    assert.ok(routes.includes(page.markdown ?? ""), `${page.markdown} not in run_worker_first`);
    if (page.alternate === false) continue;
    assert.equal(PAGE_MARKDOWN.get(page.path), page.markdown, page.path);
    assert.ok(routes.includes(page.path), `${page.path} not in run_worker_first`);
  }
  for (const path of PAGE_MARKDOWN.keys())
    assert.ok(routes.includes(path), `${path} not in run_worker_first`);
});

test("the Worker sends the same security headers as src/_headers", () => {
  const block = HEADERS_FILE.split(/\n\s*\n/)[0] ?? "";
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
    for (const source of sources)
      assert.ok(source.startsWith("'"), `${name} names a host: ${source}`);
  }
});

test("a Markdown request for a page gets its Markdown version", async () => {
  for (const [path, heading, canonical] of [
    ["/", "# Bookclerk: every purchase, properly placed", "/"],
    ["/download", "# Get Bookclerk", "/download"],
    ["/docs", "# ", "/docs"],
    ["/docs/guide/getting-started", "# ", "/docs/guide/getting-started"],
    ["/docs/v/main/plugins", "# ", "/docs/v/main/plugins"],
  ] as const) {
    const res = await get(path, "text/markdown");
    assert.equal(res.status, 200, path);
    assert.equal(res.headers.get("Content-Type"), "text/markdown; charset=utf-8", path);
    assert.match(res.headers.get("Vary") ?? "", /Accept/, path);
    assert.equal(
      res.headers.get("Link"),
      `<https://bookclerk.app${canonical}>; rel="canonical"`,
      path,
    );
    assert.equal(
      res.headers.get("Content-Security-Policy"),
      SECURITY_HEADERS["Content-Security-Policy"],
      path,
    );
    const body = await res.text();
    assert.ok(body.startsWith(heading), `${path}: ${body.slice(0, 40)}`);
    assert.doesNotMatch(body, /<html|\{\{|%%/, path);
  }
});

test("the Markdown version links to the site's own Markdown copies", async () => {
  const body = await (await get("/docs/plugins/README.md", "*/*")).text();
  assert.ok(body.includes("](/docs/"));
  assert.doesNotMatch(
    body,
    /\]\(https:\/\/raw\.githubusercontent\.com\//,
    "no link points at the docs repository's raw files",
  );
  assert.ok(!/\]\((?!https?:|\/|#|mailto:)[^)]*\.md/.test(body));
  const main = await (await get("/docs/v/main/plugins/README.md", "*/*")).text();
  assert.ok(main.includes("](/docs/v/main/"));
});

test("the other spelling of a doc path redirects to the doc", async () => {
  for (const [path, location] of [
    ["/docs/guide/getting-started/", "/docs/guide/getting-started"],
    ["/docs/api/", "/docs/api"],
    ["/docs/api/web/", "/docs/api/web"],
    ["/docs/plugins.html", "/docs/plugins"],
    ["/docs/v/v0.2.0/plugins/?x=1", "/docs/plugins?x=1"],
    ["/docs/", "/docs"],
    ["/docs/v/main/", "/docs/v/main"],
    ["/docs/v/main/plugins/", "/docs/v/main/plugins"],
  ] as const) {
    const res = await get(path, BROWSER);
    assert.equal(res.status, 301, path);
    assert.equal(res.headers.get("Location"), `https://bookclerk.app${location}`, path);
  }
});

test("website pages have one slash-free URL; other spellings move there permanently", async () => {
  const pages = ["/", "/download", "/privacy"];
  assert.equal(sitePageLocation("/index.html", pages), "/");
  assert.equal(sitePageLocation("/download/", pages), "/download");
  assert.equal(sitePageLocation("/download.html", pages), "/download");
  for (const path of [
    "/",
    "/download",
    "/download.md",
    "/nope/",
    "/nope.html",
    "/.html",
    "/404.html",
  ])
    assert.equal(sitePageLocation(path, pages), null, path);
  const toml = WRANGLER_TOML;
  assert.match(toml, /^html_handling = "drop-trailing-slash"$/m);
  const routes: string[] = JSON.parse(toml.match(/^run_worker_first = (\[.*\])$/m)?.[1] ?? "[]");
  for (const page of shell.pages.filter((p) => p !== "/")) {
    for (const path of [`${page}/`, `${page}.html`]) {
      assert.ok(routes.includes(path), `${path} not in run_worker_first`);
      const res = await get(`${path}?x=1`, BROWSER);
      assert.equal(res.status, 301, path);
      assert.equal(res.headers.get("Location"), `https://bookclerk.app${page}?x=1`, path);
    }
  }
  const index = await get("/index.html", BROWSER);
  assert.equal(index.status, 301);
  assert.equal(index.headers.get("Location"), "https://bookclerk.app/");
});

test("a docs link that leads nowhere goes to the docs home page", async () => {
  for (const [path, accept, location] of [
    ["/docs/no-such-doc", BROWSER, "/docs"],
    ["/docs/getting-started?x=1", BROWSER, "/docs"],
    ["/docs/no-such-doc", "text/markdown", "/docs"],
    ["/docs/no-such-doc.md", "*/*", "/docs/README.md"],
    ["/docs/v/main/no-such-doc", BROWSER, "/docs/v/main"],
    ["/docs/v/v9.9.9/plugins/", BROWSER, "/docs"],
    ["/docs/constructor", BROWSER, "/docs"],
  ] as const) {
    const res = await get(path, accept);
    assert.equal(res.status, 302, path);
    assert.equal(res.headers.get("Location"), `https://bookclerk.app${location}`, path);
    assert.equal(res.headers.get("Cache-Control"), "no-store", path);
  }
});

test("a missing docs home page is a 404, not a redirect loop", async () => {
  const empty: DocsFetch = async () => new Response("404: Not Found", { status: 404 });
  for (const [path, accept] of [
    ["/docs", BROWSER],
    ["/docs/README.md", "*/*"],
  ] as const) {
    const res = await handle(
      new Request(`https://bookclerk.app${path}`, { headers: { Accept: accept } }),
      env,
      { shell, fetch: empty },
    );
    assert.equal(res.status, 404, path);
    assert.equal(res.headers.get("X-Robots-Tag"), "noindex", path);
  }
});

test("only listed versions and plain paths reach the docs repository", async () => {
  fetched.length = 0;
  for (const path of [
    "/docs/v/v9.9.9/plugins/",
    "/docs/v/feature/plugins/",
    "/docs/%2e%2e/secret",
    "/docs/.env",
    "/docs/a%00b",
  ]) {
    // Bad docs paths go to the docs home page; `%2e%2e` leaves /docs/ and is a plain 404.
    assert.ok([302, 404].includes((await get(path, BROWSER)).status), path);
  }
  assert.deepEqual(fetched, []);
  await get("/docs/v/v0.1.0/plugins", BROWSER);
  assert.deepEqual(fetched, [
    `${DOCS_RAW_BASE}v0.1.0/last-updated.json`,
    `${DOCS_RAW_BASE}v0.1.0/plugins/README.md`,
  ]);
});

test("a browser gets the doc in the site's page, with its version and sidebar", async () => {
  const res = await get("/docs/plugins", BROWSER);
  assert.equal(res.status, 200);
  assert.match(res.headers.get("Content-Type") ?? "", /^text\/html/);
  assert.equal(res.headers.get("Vary"), "Accept, Accept-Encoding");
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=300, s-maxage=86400");
  assert.equal(res.headers.get("X-Robots-Tag"), null);
  assert.equal(
    res.headers.get("Link"),
    '<https://bookclerk.app/docs/plugins/README.md>; rel="alternate"; type="text/markdown"',
  );
  const html = await res.text();
  assert.match(html, /^<!doctype html>/);
  assert.doesNotMatch(html, /%%|<!--doc-|\{\{/);
  assert.match(html, /<link rel="canonical" href="https:\/\/bookclerk.app\/docs\/plugins">/);
  assert.match(html, /<meta property="og:type" content="article">/);
  const description = html.match(/<meta name="description" content="([^"]*)">/)?.[1] ?? "";
  assert.ok(description.length > 40 && description.length <= 160, description);
  assert.match(html, /<a href="\/docs\/plugins" aria-current="page">/);
  assert.match(html, /<strong>v0.2.0 \(latest\)<\/strong>/);
  assert.match(html, /href="\/docs\/v\/main\/plugins"/);
  assert.match(
    html,
    /href="https:\/\/github.com\/Bookclerk\/bookclerk\/blob\/v0.2.0\/docs\/plugins\/README.md"/,
  );
  assert.ok(!html.includes("doc-note"), "a release's docs carry no development note");
  assert.match(
    html,
    /<nav class="crumbs" aria-label="Breadcrumb"><ol><li><a href="\/docs">Docs<\/a><\/li><li><a href="\/docs\/plugins">Build plugins<\/a><\/li>/,
  );
  assert.match(html, /<aside class="toc" aria-label="On this page">/);
  assert.match(
    html,
    /<div class="docs-menus"><details class="menu menu-nav" name="docs-menu"><summary>Menu<\/summary>/,
  );
  assert.match(html, /<details class="nav-group" open><summary>Build plugins<\/summary>/);
  assert.match(html, /<script src="\/docs.js" defer><\/script>/);
  const ld: { "@graph": { "@type": string; url: string }[] } = JSON.parse(
    html.match(/<script type="application\/ld\+json">(.*?)<\/script>/)?.[1] ?? "{}",
  );
  assert.equal(ld["@graph"][0]?.["@type"], "TechArticle");
  assert.equal(ld["@graph"][0]?.url, "https://bookclerk.app/docs/plugins");
  assert.match(
    html,
    /<span class="doc-action doc-updated"><svg [^>]*>.*?<\/svg>Last updated <time datetime="2026-10-26">Oct 26, 2026<\/time><\/span>/,
  );
  assert.match(
    html,
    /<button type="button" class="doc-action doc-copy-markdown" data-markdown="\/docs\/plugins\/README.md" hidden>/,
  );
  assert.match(
    html,
    /<a class="doc-action" href="\/docs\/plugins\/README.md" type="text\/markdown">/,
  );
  assert.equal(
    (ld["@graph"][0] as { dateModified?: string } | undefined)?.dateModified,
    "2026-10-26",
  );

  const home = await get("/", "*/*");
  assert.equal(
    home.headers.get("Link"),
    '<https://bookclerk.app/index.md>; rel="alternate"; type="text/markdown"',
  );
});

/**
 * Reads the record numbers and groups of a page's catalog stack.
 *
 * @param html - The homepage.
 * @returns One entry per card, front to back.
 */
function stackOf(html: string): { record: string; group: string }[] {
  return [...html.matchAll(/data-record="([^"]+)" data-group="([^"]+)"/g)].map((m) => ({
    record: m[1] ?? "",
    group: m[2] ?? "",
  }));
}

test("the homepage gets four cards from four groups, picked per request", async () => {
  const seen = new Set<string>();
  for (let i = 0; i < 12; i += 1) {
    const res = await get("/", BROWSER);
    assert.equal(res.status, 200);
    assert.equal(res.headers.get("ETag"), null, "a sampled page is not one cacheable version");
    assert.equal(res.headers.get("Cache-Control"), "no-cache");
    assert.equal(
      res.headers.get("Content-Security-Policy"),
      SECURITY_HEADERS["Content-Security-Policy"],
    );
    const html = await res.text();
    const stack = stackOf(html);
    assert.equal(stack.length, 4);
    assert.equal(new Set(stack.map((c) => c.group)).size, 4);
    for (const card of stack) {
      const group = catalogGroups.find((g) => g.key === card.group);
      assert.ok(
        group?.books.some((b) => b.number === card.record),
        card.record,
      );
    }
    assert.ok(!html.includes("audible.com"), "source links stay out of the page");
    seen.add(stack.map((c) => c.record).join(","));
  }
  assert.ok(seen.size > 1, "different requests see different samples");
});

test("the catalog sample follows the random source and leaves other answers alone", async () => {
  const page = (random: () => number) =>
    handle(new Request("https://bookclerk.app/", { headers: { Accept: BROWSER } }), env, {
      shell,
      fetch: docsFetch,
      random,
    }).then(async (res) => stackOf(await res.text()));
  assert.deepEqual(await page(() => 0), await page(() => 0));
  assert.notDeepEqual(await page(() => 0), await page(() => 0.999999));
  const notModified = new Response(null, { status: 304, headers: { ETag: '"x"' } });
  assert.equal(withCatalogSample(notModified), notModified);
  const text = new Response('<div class="catalog-stack"></div>', {
    headers: { "Content-Type": "text/plain" },
  });
  assert.equal(withCatalogSample(text), text);
  const md = await get("/", "text/markdown");
  assert.ok(!(await md.text()).includes("catalog-card"));
});

test("main's docs say they are the development version and link within main", async () => {
  const res = await get("/docs/v/main/plugins", BROWSER);
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=300, s-maxage=300");
  assert.equal(res.headers.get("X-Robots-Tag"), "noindex", "only the default version is indexed");
  const html = await res.text();
  assert.match(html, /class="doc-note"/);
  assert.match(html, /<a href="\/docs\/v\/main\/plugins" aria-current="page">/);
  assert.match(
    html,
    /<link rel="canonical" href="https:\/\/bookclerk.app\/docs\/v\/main\/plugins">/,
  );
  assert.match(html, /unstable-badge/);
});

test("generated pages link to what they are generated from and keep links inside the tree", async () => {
  const html = await (await get("/docs/api/rust/demo/index", BROWSER)).text();
  // A crate the workspace does not hold falls back to the generator itself.
  const generator = "https://github.com/Bookclerk/bookclerk/blob/v0.2.0/scripts/docs-markdown.py";
  assert.ok(
    html.includes(`Generated from <a href="${generator}">scripts/docs-markdown.py</a> at v0.2.0`),
  );
  assert.ok(html.includes(`<a href="${generator}">`) && html.includes("Source on GitHub"));
  const web = await (await get("/docs/api/web/jobs", BROWSER)).text();
  assert.ok(
    web.includes(
      'href="https://github.com/Bookclerk/bookclerk/blob/v0.2.0/crates/bookclerkd/openapi.json"',
    ),
  );
  assert.match(html, /href="\/docs\/api\/typescript"/);
  assert.match(html, /<h2 id="thing">/);
  // The publish step dates generated pages by when their content last changed.
  assert.match(html, /Last updated <time datetime="2026-10-01">Oct 1, 2026<\/time>/);
});

test("the search index lists every sidebar page with its headings, and the Web API's routes", async () => {
  const response = await get("/docs/search-index.json", "*/*");
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("Content-Type"), "application/json; charset=utf-8");
  assert.equal(response.headers.get("X-Robots-Tag"), "noindex");
  assert.match(response.headers.get("Cache-Control") ?? "", /s-maxage=[1-9]/);
  const { pages } = (await response.json()) as { pages: SearchPage[] };
  const config = pages.find((p) => p.url === "/docs/server/configuration");
  assert.equal(config?.section, "Run a server");
  assert.equal(config?.title, "Configuration");
  assert.ok(
    config?.headings.some(
      (h) =>
        h.text === "Tables in config.toml" &&
        h.url === "/docs/server/configuration#tables-in-configtoml",
    ),
    JSON.stringify(config?.headings.slice(0, 3)),
  );
  const jobs = pages.find((p) => p.section === "Web API" && p.title === "Jobs");
  assert.deepEqual(jobs, {
    section: "Web API",
    title: "Jobs",
    url: "/docs/api/web/jobs",
    headings: [{ text: "GET /api/jobs", url: "/docs/api/web/jobs#get-apijobs" }],
  });
  const html = await (await get("/docs/server/configuration", BROWSER)).text();
  assert.match(
    html,
    /<nav class="docs-nav" aria-label="Documentation" data-search-index="\/docs\/search-index.json">/,
  );
});

test("each docs version has its own search index, linked at that version", async () => {
  const main = await get("/docs/v/main/search-index.json", "*/*");
  const { pages } = (await main.json()) as { pages: SearchPage[] };
  assert.ok(pages.length > 0 && pages.every((p) => p.url.startsWith("/docs/v/main/")));
  assert.ok(pages.every((p) => p.headings.every((h) => h.url.startsWith("/docs/v/main/"))));
  const prefixed = await get("/docs/v/v0.2.0/search-index.json", "*/*");
  assert.equal(prefixed.status, 301);
  assert.equal(
    new URL(prefixed.headers.get("Location") ?? "", "https://x").pathname,
    "/docs/search-index.json",
  );
  assert.equal((await get("/docs/v/nope/search-index.json", "*/*")).status, 302);
  const head = await get("/docs/search-index.json", "*/*", "HEAD");
  assert.equal(head.status, 200);
  assert.equal(await head.text(), "");
});

test("a search index missing a doc lists it by title and is cached only briefly", async () => {
  const flaky: DocsFetch = async (url, init) =>
    url.endsWith("/server/configuration.md")
      ? new Response("oops", { status: 502 })
      : docsFetch(url, init);
  const response = await handle(new Request("https://bookclerk.app/docs/search-index.json"), env, {
    shell,
    fetch: flaky,
  });
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("Cache-Control"), "public, max-age=60, s-maxage=60");
  const { pages } = (await response.json()) as { pages: SearchPage[] };
  const config = pages.find((p) => p.url === "/docs/server/configuration");
  assert.deepEqual(config?.headings, []);
  assert.equal(config?.title, "Configuration");
});

test("a page shows no date when the dates file leaves it out, is broken or cannot be read", async () => {
  assert.doesNotMatch(
    await (await get("/docs/server/cluster-hosts", BROWSER)).text(),
    /Last updated|dateModified/,
  );
  const broken =
    (answer: () => Promise<Response>): DocsFetch =>
    // Awaited, not returned: the Workers runtime reports a rejection that an
    // async function passes on by returning it as unhandled.
    async (url, init) =>
      url.endsWith("/last-updated.json") ? await answer() : docsFetch(url, init);
  const answers = [
    async () => new Response("oops", { status: 502 }),
    async () => new Response(null, { status: 302, headers: { Location: "https://example.com/" } }),
    async () => new Response("{not json"),
    async () =>
      new Response(JSON.stringify({ version: 2, pages: { "plugins/README": "2026-10-26" } })),
    async () =>
      new Response(JSON.stringify({ version: 1, pages: { "plugins/README": "2026-02-30" } })),
    async () =>
      new Response(JSON.stringify({ version: 1, pages: { "plugins/README": "<b>x</b>" } })),
    async () =>
      new Response("x", { headers: { "Content-Length": String(MAX_LAST_UPDATED_BYTES + 1) } }),
    async () => {
      throw new Error("network");
    },
  ];
  // Answers that are the file itself (however wrong) are cached like the page; failed reads are not.
  const failed = new Set([0, 1, 6, 7]);
  for (const [i, answer] of answers.entries()) {
    const store = new Map<string, Response>();
    const cache: PageCache = {
      match: async (req) => store.get(req.url)?.clone(),
      put: async (req, res) => void store.set(req.url, res),
    };
    const pending: Promise<unknown>[] = [];
    const deps = {
      shell,
      fetch: broken(answer),
      cache,
      waitUntil: (p: Promise<unknown>) => void pending.push(p),
    };
    const res = await handle(
      new Request("https://bookclerk.app/docs/plugins", { headers: { Accept: BROWSER } }),
      env,
      deps,
    );
    await Promise.all(pending);
    assert.equal(res.status, 200);
    assert.doesNotMatch(await res.text(), /Last updated|dateModified/);
    assert.equal(store.size, failed.has(i) ? 0 : 1, `answer ${i}`);
    assert.equal(
      res.headers.get("Cache-Control"),
      failed.has(i) ? "public, max-age=60, s-maxage=0" : "public, max-age=300, s-maxage=86400",
      `answer ${i}`,
    );
  }
});

test("a Markdown request does not read the dates file", async () => {
  fetched.length = 0;
  assert.equal((await get("/docs/plugins", "text/markdown")).status, 200);
  assert.deepEqual(fetched, [`${DOCS_RAW_BASE}v0.2.0/plugins/README.md`]);
});

test("a directory whose entry page is index serves it", async () => {
  const dir = await get("/docs/api/rust/demo", BROWSER);
  assert.equal(dir.status, 301);
  assert.equal(dir.headers.get("Location"), "https://bookclerk.app/docs/api/rust/demo/index");
  const old = await get("/docs/api/rust/demo/index.html", BROWSER);
  assert.equal(old.status, 301);
  assert.equal(old.headers.get("Location"), "https://bookclerk.app/docs/api/rust/demo");
});

test("raw HTML in a doc is shown as text", async () => {
  // docs/server/cluster-hosts.md says "Let **<branch>** join".
  assert.match(await (await get("/docs/server/cluster-hosts", BROWSER)).text(), /&lt;branch&gt;/);
});

test("a failing or oversized docs repository answer is a 503, not a page", async () => {
  const down = async () => new Response("oops", { status: 502 });
  const big = async () =>
    new Response("x", { headers: { "Content-Length": String(MAX_DOC_BYTES + 1) } });
  // No Content-Length: the body is cut off once it passes the cap.
  const chunk = new Uint8Array(1024 * 1024);
  let sent = 0;
  // The dates file is missing here, so only the page's body is counted.
  const endless: DocsFetch = async (url) =>
    url.endsWith("/last-updated.json")
      ? new Response(null, { status: 404 })
      : new Response(
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
  const fetchers: DocsFetch[] = [
    down,
    big,
    endless,
    moved,
    async () => {
      throw new Error("network");
    },
  ];
  for (const fetcher of fetchers) {
    const req = new Request("https://bookclerk.app/docs/plugins", { headers: { Accept: BROWSER } });
    const res = await handle(req, env, { shell, fetch: fetcher });
    assert.equal(res.status, 503);
    assert.equal(res.headers.get("Cache-Control"), "no-store");
  }
  assert.ok(sent <= MAX_DOC_BYTES + 2 * chunk.byteLength, `read ${sent} bytes`);
});

test("the docs-unavailable page keeps the site's look and links to the doc on GitHub", async () => {
  const down = async () => new Response("oops", { status: 502 });
  const page = await handle(
    new Request("https://bookclerk.app/docs/plugins", { headers: { Accept: BROWSER } }),
    env,
    { shell, fetch: down },
  );
  assert.equal(page.status, 503);
  assert.equal(page.headers.get("Content-Type"), "text/html; charset=utf-8");
  assert.equal(page.headers.get("Retry-After"), "60");
  assert.equal(page.headers.get("X-Robots-Tag"), "noindex");
  assert.equal(
    page.headers.get("Content-Security-Policy"),
    SECURITY_HEADERS["Content-Security-Policy"],
  );
  const html = await page.text();
  assert.match(html, /<link rel="stylesheet" href="\/site\.css/);
  assert.match(html, /class="docs-nav"/);
  assert.doesNotMatch(html, /<h2>Using Bookclerk<\/h2>/, "no sidebar links that would fail too");
  assert.match(html, /<h1>The docs could not be loaded<\/h1>/);
  assert.ok(
    html.includes(
      'href="https://github.com/Bookclerk/bookclerk-docs/blob/v0.2.0/plugins/README.md"',
    ),
    "links the doc on GitHub",
  );
  assert.ok(
    html.includes('href="https://github.com/Bookclerk/bookclerk-docs/tree/v0.2.0"'),
    "links every doc on GitHub",
  );

  // A directory names its README; Markdown readers get the same links.
  const md = await handle(
    new Request("https://bookclerk.app/docs/api", { headers: { Accept: "text/markdown" } }),
    env,
    { shell, fetch: down },
  );
  assert.equal(md.status, 503);
  assert.equal(md.headers.get("Content-Type"), "text/markdown; charset=utf-8");
  assert.match(
    await md.text(),
    /<https:\/\/github\.com\/Bookclerk\/bookclerk-docs\/blob\/v0\.2\.0\/api\/README\.md>/,
  );

  // When the first key is missing and the next one fails, GitHub gets the one that failed.
  const missingThenDown: DocsFetch = async (url) =>
    url.endsWith("/plugins/README.md") ? new Response("404: Not Found", { status: 404 }) : down();
  const later = await (
    await handle(
      new Request("https://bookclerk.app/docs/plugins", { headers: { Accept: BROWSER } }),
      env,
      { shell, fetch: missingThenDown },
    )
  ).text();
  assert.ok(
    later.includes('href="https://github.com/Bookclerk/bookclerk-docs/blob/v0.2.0/plugins.md"'),
    "links the key that failed",
  );
  assert.ok(later.includes('href="/docs/plugins"'), "tries the page's own URL again");

  const head = await handle(
    new Request("https://bookclerk.app/docs/plugins", { method: "HEAD" }),
    env,
    { shell, fetch: down },
  );
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
  const deps = {
    shell,
    fetch: docsFetch,
    cache,
    waitUntil: (p: Promise<unknown>) => void pending.push(p),
  };
  fetched.length = 0;
  for (let i = 0; i < 2; i += 1) {
    const res = await handle(
      new Request("https://bookclerk.app/docs/plugins?utm=x", { headers: { Accept: BROWSER } }),
      env,
      deps,
    );
    assert.equal(res.status, 200);
    await Promise.all(pending);
  }
  // The dates file is asked for first, beside the page.
  assert.deepEqual(fetched, [
    `${DOCS_RAW_BASE}v0.2.0/last-updated.json`,
    `${DOCS_RAW_BASE}v0.2.0/plugins/README.md`,
  ]);
  assert.deepEqual(
    [...store.keys()],
    [`https://bookclerk.app/docs/plugins?format=html&ref=v0.2.0&shell=${shell.id}`],
  );
  assert.match(shell.id, /^[0-9a-f]{16}$/);
  const head = await handle(
    new Request("https://bookclerk.app/docs/plugins", {
      method: "HEAD",
      headers: { Accept: BROWSER },
    }),
    env,
    deps,
  );
  assert.equal(head.status, 200);
  assert.equal(await head.text(), "");
});

test("the sitemap is an index of one sitemap per section, listing every dated page of the default docs version", async () => {
  const routes: string[] = JSON.parse(
    WRANGLER_TOML.match(/^run_worker_first = (\[.*\])$/m)?.[1] ?? "[]",
  );
  assert.ok(routes.includes("/sitemap.xml") && routes.includes("/sitemap-*"));
  fetched.length = 0;
  const res = await get("/sitemap.xml", "*/*");
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("Content-Type"), "application/xml; charset=utf-8");
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=300, s-maxage=86400");
  assert.equal(res.headers.get("X-Content-Type-Options"), "nosniff");
  assert.deepEqual(fetched, [`${DOCS_RAW_BASE}v0.2.0/last-updated.json`]);
  const index = await res.text();
  assert.match(
    index,
    /^<\?xml version="1.0" encoding="UTF-8"\?>\n<\?xml-stylesheet type="text\/css" href="\/sitemap.css"\?>\n<sitemapindex xmlns="http:\/\/www.sitemaps.org\/schemas\/sitemap\/0.9">\n/,
  );
  const sitemaps = [
    ...index.matchAll(/<sitemap><loc>([^<]*)<\/loc>(?:<lastmod>([^<]*)<\/lastmod>)?<\/sitemap>/g),
  ].map((m) => [m[1], m[2] ?? null]);
  assert.deepEqual(sitemaps, [
    ["https://bookclerk.app/sitemap-site.xml", null],
    ["https://bookclerk.app/sitemap-plugins.xml", "2026-10-26"],
    ["https://bookclerk.app/sitemap-api.xml", "2026-10-01"],
  ]);
  const urls = async (path: string): Promise<(string | null)[][]> => {
    const section = await get(path, "*/*");
    assert.equal(section.status, 200, path);
    assert.equal(section.headers.get("Content-Type"), "application/xml; charset=utf-8", path);
    const xml = await section.text();
    assert.match(xml, /<urlset xmlns="http:\/\/www.sitemaps.org\/schemas\/sitemap\/0.9">/, path);
    return [
      ...xml.matchAll(/<url><loc>([^<]*)<\/loc>(?:<lastmod>([^<]*)<\/lastmod>)?<\/url>/g),
    ].map((m) => [m[1] ?? "", m[2] ?? null]);
  };
  assert.deepEqual(
    await urls("/sitemap-site.xml"),
    [...PAGES.filter((p) => p.indexable).map((p) => p.path), "/sitemap"].map((p) => [
      `https://bookclerk.app${p}`,
      null,
    ]),
  );
  assert.deepEqual(await urls("/sitemap-plugins.xml"), [
    ["https://bookclerk.app/docs/plugins", "2026-10-26"],
  ]);
  assert.deepEqual(await urls("/sitemap-api.xml"), [
    ["https://bookclerk.app/docs/api/rust/demo/index", "2026-10-01"],
  ]);
  for (const path of ["/sitemap-guide.xml", "/sitemap-x.xml"])
    assert.equal((await get(path, "*/*")).status, 404, path);
  const head = await get("/sitemap.xml", "*/*", "HEAD");
  assert.equal(head.status, 200);
  assert.equal(await head.text(), "");
});

test("/sitemap lists every page for people, grouped like the sidebar, with dates", async () => {
  const routes: string[] = JSON.parse(
    WRANGLER_TOML.match(/^run_worker_first = (\[.*\])$/m)?.[1] ?? "[]",
  );
  for (const path of ["/sitemap", "/sitemap/", "/sitemap.md"])
    assert.ok(routes.includes(path), path);
  const res = await get("/sitemap", BROWSER);
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("Content-Type"), "text/html; charset=utf-8");
  assert.equal(
    res.headers.get("Link"),
    '<https://bookclerk.app/sitemap.md>; rel="alternate"; type="text/markdown"',
  );
  const html = await res.text();
  assert.match(html, /<title>Sitemap · Bookclerk<\/title>/);
  assert.match(html, /<link rel="canonical" href="https:\/\/bookclerk.app\/sitemap">/);
  // A website page, like /privacy, not a docs page.
  assert.match(html, /<main id="main" class="reading">/);
  assert.doesNotMatch(html, /version-picker|docs-menus|class="toc"/);
  assert.match(
    html,
    /<nav aria-label="Sections"><p><a href="#sitemap-website">Website<\/a> · <a href="#sitemap-build-plugins">/,
  );
  assert.match(
    html,
    /<h2 id="sitemap-website">Website<\/h2>\n<ul><li><a href="\/">Home<\/a><\/li><li><a href="\/download">Download Bookclerk<\/a><\/li>/,
  );
  assert.match(
    html,
    /<h2 id="sitemap-build-plugins">Build plugins<\/h2>\n<ul><li><a href="\/docs\/plugins">[^<]+<\/a> <span class="faint">· <time datetime="2026-10-26">Oct 26, 2026<\/time><\/span><\/li><\/ul>/,
  );
  assert.match(
    html,
    /<h2 id="sitemap-api-reference">API reference<\/h2>\n<h3>Rust crates<\/h3>\n<ul><li><a href="\/docs\/api\/rust\/demo\/index">demo<\/a>/,
  );
  assert.match(html, /<a href="\/sitemap">Sitemap<\/a>/, "footer link");
  assert.doesNotMatch(html, /<script(?! type="application\/ld\+json")(?! src="\/)/);
  for (const [path, accept] of [
    ["/sitemap.md", "*/*"],
    ["/sitemap", "text/markdown"],
  ] as const) {
    const md = await get(path, accept);
    assert.equal(md.status, 200, path);
    assert.equal(md.headers.get("Content-Type"), "text/markdown; charset=utf-8", path);
    assert.equal(md.headers.get("Link"), '<https://bookclerk.app/sitemap>; rel="canonical"', path);
    assert.match(
      await md.text(),
      /^# Sitemap\n[\s\S]*## API reference\n\n### Rust crates\n\n- \[demo\]\(https:\/\/bookclerk.app\/docs\/api\/rust\/demo\/index\) \(Oct 1, 2026\)/,
      path,
    );
  }
  const slash = await get("/sitemap/", BROWSER);
  assert.equal(slash.status, 301);
  assert.equal(slash.headers.get("Location"), "https://bookclerk.app/sitemap");
});

test("/sitemap lists the sidebar without dates when the dates file cannot be read", async () => {
  const down: DocsFetch = async (url, init) =>
    url.endsWith("/last-updated.json")
      ? new Response("oops", { status: 502 })
      : docsFetch(url, init);
  const res = await handle(
    new Request("https://bookclerk.app/sitemap", { headers: { Accept: BROWSER } }),
    env,
    { shell, fetch: down },
  );
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=60, s-maxage=0");
  const html = await res.text();
  assert.match(html, /<a href="\/docs\/guide\/getting-started">/);
  assert.doesNotMatch(html, /<time /);
});

test("sitemapSection names a doc's top folder, site for the docs home, or docs", () => {
  assert.equal(sitemapSection("guide/getting-started"), "guide");
  assert.equal(sitemapSection("api/rust/demo/index"), "api");
  assert.equal(sitemapSection("sql-contract/v1"), "sql-contract");
  assert.equal(sitemapSection("README"), "site");
  assert.equal(sitemapSection("Guide/x"), "docs");
  assert.equal(sitemapSection("a.b/x"), "docs");
  assert.equal(sitemapSection("site/x"), "docs");
});

test("the sitemap leaves out keys the docs route does not serve as themselves and dates that are not real", async () => {
  const dates = {
    version: 1,
    pages: {
      "guide/stores": "2026-10-20",
      "../x": "2026-10-20",
      "a//b": "2026-10-20",
      ".git/x": "2026-10-20",
      "a<b": "2026-10-20",
      "server/backups": "2026-02-30",
      "": "2026-10-20",
      "v/main/guide/stores": "2026-10-20",
      "guide/stores.md": "2026-10-20",
      "guide/stores.html": "2026-10-20",
    },
  };
  const fetcher: DocsFetch = async (url, init) =>
    url.endsWith("/last-updated.json") ? new Response(JSON.stringify(dates)) : docsFetch(url, init);
  const index = await (
    await handle(new Request("https://bookclerk.app/sitemap.xml"), env, { shell, fetch: fetcher })
  ).text();
  assert.deepEqual(
    [...index.matchAll(/<loc>([^<]*)<\/loc>/g)].map((m) => m[1]),
    ["https://bookclerk.app/sitemap-site.xml", "https://bookclerk.app/sitemap-guide.xml"],
  );
  const xml = await (
    await handle(new Request("https://bookclerk.app/sitemap-guide.xml"), env, {
      shell,
      fetch: fetcher,
    })
  ).text();
  const docs = [...xml.matchAll(/<loc>https:\/\/bookclerk.app(\/docs\/[^<]*)<\/loc>/g)].map(
    (m) => m[1],
  );
  assert.deepEqual(docs, ["/docs/guide/stores"]);
});

test("the sitemap falls back to the built one when the dates file is missing, unreadable or lists no page", async () => {
  const built = new TextDecoder().decode(distFile("sitemap.xml"));
  assert.match(built, /<loc>https:\/\/bookclerk.app\/docs\/plugins<\/loc>/);
  assert.doesNotMatch(built, /<lastmod>/);
  const answers = [
    async () => new Response("404: Not Found", { status: 404 }),
    async () => new Response("oops", { status: 502 }),
    async () => new Response("{not json"),
    async () => new Response(JSON.stringify({ version: 2, pages: {} })),
    async () => new Response(JSON.stringify({ version: 1, pages: {} })),
    async () => new Response(JSON.stringify({ version: 1, pages: { "v/x": "2026-10-20" } })),
    async () =>
      new Response("x", { headers: { "Content-Length": String(MAX_LAST_UPDATED_BYTES + 1) } }),
    async () => {
      throw new Error("network");
    },
  ];
  // A release published without the file never gets one, so its fallback is
  // kept like its pages; every other answer is retried in a minute.
  const missing = 0;
  for (const [i, answer] of answers.entries()) {
    const store = new Map<string, Response>();
    const cache: PageCache = {
      match: async (req) => store.get(req.url)?.clone(),
      put: async (req, res) => void store.set(req.url, res),
    };
    const pending: Promise<unknown>[] = [];
    const fetcher: DocsFetch = async (url, init) =>
      url.endsWith("/last-updated.json") ? await answer() : docsFetch(url, init);
    const res = await handle(new Request("https://bookclerk.app/sitemap.xml"), env, {
      shell,
      fetch: fetcher,
      cache,
      waitUntil: (p) => void pending.push(p),
    });
    await Promise.all(pending);
    assert.equal(res.status, 200, `answer ${i}`);
    assert.equal(res.headers.get("Content-Type"), "application/xml; charset=utf-8", `answer ${i}`);
    assert.equal(
      res.headers.get("Cache-Control"),
      i === missing ? "public, max-age=60, s-maxage=86400" : "public, max-age=60, s-maxage=60",
      `answer ${i}`,
    );
    assert.equal(await res.text(), built, `answer ${i}`);
    assert.equal(store.size, i === missing ? 1 : 0, `answer ${i}`);
    // The flat fallback names no sections, and a section asked for anyway is not served.
    const section = await handle(new Request("https://bookclerk.app/sitemap-guide.xml"), env, {
      shell,
      fetch: fetcher,
    });
    assert.equal(section.status, i === missing ? 404 : 503, `answer ${i}`);
    if (i !== missing)
      assert.equal(section.headers.get("Cache-Control"), "no-store", `answer ${i}`);
  }
});

test("the sitemap is cached per deploy and docs version", async () => {
  const store = new Map<string, Response>();
  const cache: PageCache = {
    match: async (req) => store.get(req.url)?.clone(),
    put: async (req, res) => void store.set(req.url, res),
  };
  const pending: Promise<unknown>[] = [];
  const deps = {
    shell,
    fetch: docsFetch,
    cache,
    waitUntil: (p: Promise<unknown>) => void pending.push(p),
  };
  fetched.length = 0;
  const bodies: string[] = [];
  for (const path of ["/sitemap.xml", "/sitemap.xml", "/sitemap-api.xml", "/sitemap-api.xml"]) {
    bodies.push(
      await (await handle(new Request(`https://bookclerk.app${path}`), env, deps)).text(),
    );
    await Promise.all(pending);
  }
  assert.equal(bodies[0], bodies[1]);
  assert.equal(bodies[2], bodies[3]);
  assert.equal(fetched.length, 2);
  assert.deepEqual(
    [...store.keys()],
    [
      `https://bookclerk.app/sitemap.xml?ref=v0.2.0&shell=${shell.id}`,
      `https://bookclerk.app/sitemap-api.xml?ref=v0.2.0&shell=${shell.id}`,
    ],
  );
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
  const req = new Request("https://bookclerk.app/download", {
    headers: { Accept: "text/markdown", "If-None-Match": '"abc"' },
  });
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
    ["/docs/plugins", BROWSER],
    ["/docs/plugins", "text/markdown"],
    ["/no-such-page", BROWSER],
  ] as const) {
    const req = new Request(`https://bookclerk.app${path}`, {
      headers: { Accept: accept, "Accept-Encoding": "gzip, deflate, br" },
    });
    const res = await handle(req, env, { shell, fetch: docsFetch });
    assert.equal(res.headers.get("Content-Encoding"), "br", `${path} as ${accept}`);
    assert.equal(res.headers.get("Content-Length"), null, path);
    assert.ok((res.headers.get("Vary") ?? "").split(", ").includes("Accept-Encoding"), path);
  }
  const gzipOnly = await handle(
    new Request("https://bookclerk.app/", { headers: { "Accept-Encoding": "gzip" } }),
    env,
    { shell },
  );
  assert.equal(gzipOnly.headers.get("Content-Encoding"), "gzip");
  // Cloudflare rewrites Accept-Encoding before the Worker runs; what the client sent wins.
  const viaEdge = new Request("https://bookclerk.app/", {
    headers: { "Accept-Encoding": "gzip, br" },
  });
  Object.defineProperty(viaEdge, "cf", { value: { clientAcceptEncoding: "identity" } });
  assert.equal((await handle(viaEdge, env, { shell })).headers.get("Content-Encoding"), null);
});

test("compression leaves alone what must not or need not be compressed", async () => {
  const plain = await get("/", BROWSER);
  assert.equal(plain.headers.get("Content-Encoding"), null);
  assert.ok((plain.headers.get("Vary") ?? "").split(", ").includes("Accept-Encoding"));
  const answer = (headers: Record<string, string>, status = 200): Env => ({
    ASSETS: { fetch: async () => new Response(status === 304 ? null : "x", { status, headers }) },
  });
  const cases: [Record<string, string>, number][] = [
    [{ "Content-Type": "text/html", "Content-Encoding": "gzip" }, 200],
    [{ "Content-Type": "text/html", "Cache-Control": "public, no-transform" }, 200],
    [{ "Content-Type": "image/png" }, 200],
    [{ "Content-Type": "text/html", "Content-Range": "bytes 0-0/20" }, 206],
  ];
  for (const [headers, status] of cases) {
    const req = new Request("https://bookclerk.app/download", {
      headers: { "Accept-Encoding": "br" },
    });
    const res = await handle(req, answer(headers, status), { shell });
    assert.equal(
      res.headers.get("Content-Encoding"),
      headers["Content-Encoding"] ?? null,
      JSON.stringify(headers),
    );
  }
  const tagged = await handle(
    new Request("https://bookclerk.app/download", { headers: { "Accept-Encoding": "br" } }),
    answer({ "Content-Type": "text/html", ETag: '"abc"' }),
    { shell },
  );
  assert.equal(tagged.headers.get("ETag"), 'W/"abc"');
  for (const headers of [{ "Content-Type": "text/html", ETag: '"abc"' }, { ETag: '"abc"' }]) {
    // A 304 keeps the compressed answer's Vary and weak validator, with no body to encode.
    const req = new Request("https://bookclerk.app/download", {
      headers: { "Accept-Encoding": "br", "If-None-Match": 'W/"abc"' },
    });
    const res = await handle(req, answer(headers, 304), { shell });
    assert.equal(res.status, 304);
    assert.equal(res.headers.get("Content-Encoding"), null);
    assert.equal(res.headers.get("ETag"), 'W/"abc"');
    assert.ok((res.headers.get("Vary") ?? "").split(", ").includes("Accept-Encoding"));
  }
  const md304 = await handle(
    new Request("https://bookclerk.app/download", {
      headers: { Accept: "text/markdown", "Accept-Encoding": "gzip", "If-None-Match": 'W/"abc"' },
    }),
    answer({ ETag: '"abc"' }, 304),
    { shell },
  );
  assert.equal(md304.headers.get("Vary"), "Accept, Accept-Encoding");
  assert.equal(md304.headers.get("ETag"), 'W/"abc"');
  const post = await handle(
    new Request("https://bookclerk.app/", { method: "POST", headers: { "Accept-Encoding": "br" } }),
    env,
    { shell },
  );
  assert.equal(post.headers.get("Content-Encoding"), null);
});

/**
 * Decodes the character references escapeHtml writes.
 *
 * @param text - Escaped HTML text.
 * @returns The text a browser or crawler reads.
 */
function unescapeHtml(text: string): string {
  return text.replace(/&(#\d+|#x[0-9a-f]+|amp|lt|gt|quot|apos);/gi, (_, ref: string) => {
    const named: Readonly<Record<string, string>> = {
      amp: "&",
      lt: "<",
      gt: ">",
      quot: '"',
      apos: "'",
    };
    if (!ref.startsWith("#")) return named[ref.toLowerCase()] ?? "";
    return String.fromCodePoint(
      ref[1] === "x" || ref[1] === "X" ? parseInt(ref.slice(2), 16) : parseInt(ref.slice(1), 10),
    );
  });
}

test("every page has a description, a title search engines show whole, and alt text on every image", async () => {
  const docs = [
    ...Object.keys(DOCS)
      .filter((file) => file.endsWith(".md"))
      .map((file) => file.slice(0, -".md".length)),
    ...Object.keys(GENERATED)
      .filter((f) => f.endsWith(".md"))
      .map((f) => f.slice(0, -".md".length)),
  ];
  const paths = [
    ...PAGES.map((page) => page.path),
    "/no-such-page",
    ...docs.map((key) => docUrl(key)),
  ];
  assert.ok(docs.length > 50, "the docs were found");
  for (const path of paths) {
    const res = await get(path, BROWSER);
    const html = await res.text();
    assert.equal(res.status, path === "/no-such-page" ? 404 : 200, path);
    assert.match(res.headers.get("Content-Type") ?? "", /^text\/html/, path);
    const description = /<meta name="description" content="([^"]*)">/.exec(html)?.[1] ?? "";
    assert.ok(description.trim().length > 0, `${path} has no description`);
    const title = unescapeHtml(/<title>([^<]*)<\/title>/.exec(html)?.[1] ?? "");
    assert.ok(
      title.length > 0 && title.length <= TITLE_MAX,
      `${path} title is ${title.length} characters: ${title}`,
    );
    for (const img of html.match(/<img\b[^>]*>/g) ?? []) {
      assert.match(img, /\salt="[^"]*\S[^"]*"/, `${path} has an image without alt text: ${img}`);
    }
  }
});

test("docPageTitle names the site when it fits and otherwise keeps what identifies the page", () => {
  assert.equal(docPageTitle("Getting started"), "Getting started · Bookclerk docs");
  const prose = docPageTitle(
    "ADR: Control plane — bootstrap, host identity, and database configuration",
  );
  assert.equal(prose, "ADR: Control plane — bootstrap, host identity, and database…");
  const path = docPageTitle(
    "bookclerk_plugin_abi::plugin_capnp::adapter_database_session::assert_restore_constraints_results",
  );
  assert.equal(path, "…adapter_database_session::assert_restore_constraints_results");
  assert.equal(docPageTitle("x".repeat(200)).length, TITLE_MAX);
  const shortLast = docPageTitle(`crate.${"m".repeat(80)}.io`);
  assert.equal(shortLast.length, TITLE_MAX);
  assert.ok(shortLast.endsWith(".io"));
  assert.doesNotMatch(
    docPageTitle(`${"😀".repeat(40)} title`),
    /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/,
  );
});

test("a Web API page's sidebar lists every route group from the index, marking its own", async () => {
  const res = await get("/docs/api/web/jobs", BROWSER);
  assert.equal(res.status, 200);
  const html = await res.text();
  assert.ok(
    html.includes(
      '<ul class="nav-sub"><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a></li>',
    ),
  );
  assert.ok(
    html.includes(
      '<li><a href="/docs/api/web/health">Health</a></li><li><a href="/docs/api/web/schemas">Schemas</a></li></ul>',
    ),
  );
  // The Markdown version needs no sidebar, so it does not read the index.
  fetched.length = 0;
  await get("/docs/api/web/jobs.md", "text/markdown");
  assert.ok(!fetched.some((url) => url.endsWith("/api/web/README.md")), fetched.join(" "));
});

test("a Web API page is shown, and not cached, when its index cannot be read", async () => {
  const broken: DocsFetch = async (url, init) =>
    url.endsWith("/api/web/README.md")
      ? new Response("oops", { status: 502 })
      : docsFetch(url, init);
  const res = await handle(
    new Request("https://bookclerk.app/docs/api/web/jobs", { headers: { Accept: BROWSER } }),
    env,
    { shell, fetch: broken },
  );
  assert.equal(res.status, 200);
  assert.equal(res.headers.get("Cache-Control"), "public, max-age=60, s-maxage=0");
  assert.ok(
    (await res.text()).includes(
      '<ul class="nav-sub"><li><a href="/docs/api/web/jobs" aria-current="page">Jobs</a>',
    ),
  );
});
