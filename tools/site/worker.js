// The bookclerk-site Worker: serves dist/ through the ASSETS binding, renders
// /docs/ from the docs repository, and answers requests that prefer Markdown
// with each page's Markdown version.
//
// wrangler.toml runs this script first only for page paths (`run_worker_first`);
// stylesheets, images and the like are served straight from the assets.
//
// Docs: `/docs/<path>` is the default docs version (the newest release, else
// main) and `/docs/v/<ref>/<path>` any other version the site was built with
// (main and every release tag). Each page is read from
// raw.githubusercontent.com/Bookclerk/bookclerk-docs at that ref and rendered
// with render.mjs into the shell build.mjs baked (generated/docs-shell.json).
// Only refs in the shell's version list and paths made of plain segments are
// fetched, responses over MAX_DOC_BYTES are refused, and rendered pages are
// cached at the edge (tags for a day, main for five minutes).
//
// Content negotiation: when the Accept header ranks `text/markdown` (or
// `text/x-markdown`) at least as high as HTML, `/`, `/download` and
// `/docs/<path>` return their Markdown version instead, with
// `Content-Type: text/markdown` and a `Link: rel="canonical"` to the HTML page.
// HTML responses carry `Link: rel="alternate"` to their Markdown version.
// Every page response says `Vary: Accept` so caches keep the two apart.
//
// The script reads only the request's method, path and Accept header, and
// fetches only from its own assets and the docs repository.

import { docMarkdownUrl, docUrl, renderDocPage } from "./render.mjs";

/** Response headers every page gets; the same policy as src/_headers. */
export const SECURITY_HEADERS = {
  "Content-Security-Policy":
    "default-src 'none'; style-src 'self' https://fonts.googleapis.com; style-src-attr 'unsafe-inline'; font-src https://fonts.gstatic.com; img-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
  "X-Content-Type-Options": "nosniff",
  "Referrer-Policy": "strict-origin-when-cross-origin",
  "Permissions-Policy": "camera=(), microphone=(), geolocation=(), interest-cohort=()",
  "Strict-Transport-Security": "max-age=31536000",
};

/** Media types that ask for Markdown. */
const MARKDOWN_TYPES = new Set(["text/markdown", "text/x-markdown"]);

/** Media types that ask for HTML. */
const HTML_TYPES = new Set(["text/html", "application/xhtml+xml"]);

/** Content type of every Markdown response. */
const MARKDOWN_CONTENT_TYPE = "text/markdown; charset=utf-8";

/** Pages outside /docs/ that have a Markdown version: path -> Markdown file. */
export const PAGE_MARKDOWN = new Map([
  ["/", "/index.md"],
  ["/index.html", "/index.md"],
  ["/download", "/download.md"],
  ["/download.html", "/download.md"],
]);

/** Request headers passed on when fetching a Markdown version. */
const CONDITIONAL_HEADERS = ["If-None-Match", "If-Modified-Since"];

/** Markdown version of the not-found page. */
const NOT_FOUND_MARKDOWN = "/404.md";

/** A docs path inside a version: segments of letters, digits, `.`, `_` and `-`, none starting with `.`. */
const DOC_REST_RE = /^(?:[A-Za-z0-9_-][A-Za-z0-9._-]*\/)*(?:[A-Za-z0-9_-][A-Za-z0-9._-]*)?$/;

/** Where docs versions are read from: `<base><ref>/<file>`. */
export const DOCS_RAW_BASE = "https://raw.githubusercontent.com/Bookclerk/bookclerk-docs/";

/** Largest docs file the Worker renders, in bytes; the biggest today is under 400 KB. */
export const MAX_DOC_BYTES = 4 * 1024 * 1024;

/** Edge cache lifetime of a release's docs, in seconds; tags never move. */
const TAG_TTL = 24 * 60 * 60;

/** Edge cache lifetime of main's docs, in seconds. */
const MAIN_TTL = 5 * 60;

/** Edge cache lifetime of a missing docs file, in seconds, so a new tag shows up soon. */
const MISSING_TTL = 60;

/**
 * Browser cache lifetime of a rendered docs page, in seconds. Every page
 * carries the deploy's version picker and sidebar, so browsers keep it no
 * longer than main's pages even when the edge keeps it for a day.
 */
const BROWSER_TTL = MAIN_TTL;

/**
 * Edge cache lifetime of a docs version's files and pages.
 *
 * @param {string} ref The docs version's git ref.
 * @returns {number} Seconds: MAIN_TTL for main, TAG_TTL for a release tag.
 */
function ttlFor(ref) {
  return ref === "main" ? MAIN_TTL : TAG_TTL;
}

/**
 * Reads how strongly an Accept header asks for Markdown and for HTML.
 *
 * The most specific range that matches decides, as RFC 9110 says: an HTML
 * type listed by name overrides `text/*`, which overrides `*\/*`. Wildcards
 * count for HTML only, so a browser or a generic client keeps getting HTML.
 *
 * @param {string | null} accept The request's Accept header.
 * @returns {{markdown: number, html: number}} The quality value (0-1) given
 *   to Markdown and to HTML.
 */
export function acceptWeights(accept) {
  let markdown = 0;
  /** Highest q per specificity: 2 = an HTML type by name, 1 = text/*, 0 = *\/*. */
  const html = [null, null, null];
  for (const part of (accept ?? "").split(",")) {
    const [rawType, ...params] = part.split(";");
    const type = rawType.trim().toLowerCase();
    if (!type) continue;
    let q = 1;
    for (const param of params) {
      const [key, value] = param.split("=").map((s) => s.trim().toLowerCase());
      if (key === "q") {
        const parsed = Number(value);
        q = value !== "" && Number.isFinite(parsed) && parsed >= 0 && parsed <= 1 ? parsed : 0;
      }
    }
    const rank = HTML_TYPES.has(type) ? 2 : type === "text/*" ? 1 : type === "*/*" ? 0 : -1;
    if (MARKDOWN_TYPES.has(type)) markdown = Math.max(markdown, q);
    else if (rank >= 0) html[rank] = Math.max(html[rank] ?? 0, q);
  }
  return { markdown, html: html[2] ?? html[1] ?? html[0] ?? 0 };
}

/**
 * Decides whether a request wants Markdown.
 *
 * @param {string | null} accept The request's Accept header.
 * @returns {boolean} True when a Markdown type is acceptable and ranked at
 *   least as high as HTML.
 */
export function prefersMarkdown(accept) {
  const { markdown, html } = acceptWeights(accept);
  return markdown > 0 && markdown >= html;
}

/**
 * Maps a page path outside /docs/ to the path of its Markdown version.
 *
 * @param {string} pathname The request path.
 * @returns {string | null} The `.md` asset path, or null when the path is not
 *   such a page.
 */
export function markdownPath(pathname) {
  return PAGE_MARKDOWN.get(pathname) ?? null;
}

/**
 * Maps a Markdown asset path outside /docs/ back to its page.
 *
 * @param {string} pathname A `.md` path.
 * @returns {string | null} The page path, or null for other paths.
 */
export function htmlPath(pathname) {
  for (const [page, md] of PAGE_MARKDOWN) {
    if (md === pathname && !page.endsWith(".html")) return page;
  }
  return null;
}

/**
 * What a /docs/ request names.
 *
 * @typedef {object} DocsTarget
 * @property {"page" | "markdown" | "redirect" | "missing"} kind A page, a
 *   Markdown copy asked for by name (`<path>.md`), a URL to send the reader
 *   on to, or nothing the site serves.
 * @property {string} [location] For `redirect`: the path to go to.
 * @property {{ref: string, label: string, base: string}} [version] The docs
 *   version, for `page` and `markdown`.
 * @property {string[]} [keys] For `page` and `markdown`: the doc keys that
 *   can hold it, most likely first.
 */

/**
 * Parses a /docs/ request path.
 *
 * `/docs/v/<ref>/` selects a version from `versions`; the default version
 * (the first) is served without the prefix, so its prefixed URLs redirect.
 * A directory path names its README (`api/` → `api/README`), a path without
 * a slash may also name a directory (`api` → `api/README`), and `.html` paths
 * left from the static site redirect to the page. Other dots are part of the
 * name (`api/python/bookclerk_plugin_sdk.workerd`); the docs publish only
 * Markdown, so a path like `x.svg` is looked up as `x.svg.md` and not found.
 *
 * @param {string} pathname The request path, starting with `/docs`.
 * @param {{ref: string, label: string, base: string}[]} versions Every docs
 *   version, the default first.
 * @returns {DocsTarget} What to serve.
 */
export function docsTarget(pathname, versions) {
  if (pathname === "/docs") return { kind: "redirect", location: "/docs/" };
  let version = versions[0];
  let rest = pathname.slice("/docs/".length);
  if (rest === "v" || rest.startsWith("v/")) {
    const [, ref = "", ...tail] = rest.split("/");
    const found = versions.find((v) => v.ref === ref);
    if (found === undefined) return { kind: "missing" };
    if (tail.length === 0) return { kind: "redirect", location: `${found.base}` };
    rest = tail.join("/");
    if (!DOC_REST_RE.test(rest)) return { kind: "missing" };
    if (found === versions[0]) return { kind: "redirect", location: `/docs/${rest}` };
    version = found;
  }
  if (!DOC_REST_RE.test(rest)) return { kind: "missing" };
  const last = rest.slice(rest.lastIndexOf("/") + 1);
  if (last.endsWith(".md")) return { kind: "markdown", version, keys: [rest.slice(0, -".md".length)] };
  if (last.endsWith(".html")) {
    const page = last === "index.html" ? rest.slice(0, -"index.html".length) : rest.slice(0, -".html".length);
    return { kind: "redirect", location: version.base + page };
  }
  if (rest === "" || rest.endsWith("/")) {
    const keys = [`${rest}README`, `${rest}index`];
    if (rest !== "") keys.push(rest.slice(0, -1));
    return { kind: "page", version, keys };
  }
  return { kind: "page", version, keys: [rest, `${rest}/README`] };
}

/**
 * Reads one docs file from the docs repository, through the edge cache.
 *
 * @param {string} ref The docs version's git ref.
 * @param {string} key The doc's key.
 * @param {typeof fetch} fetcher `fetch`, or a stand-in in tests.
 * @returns {Promise<string | null>} The Markdown, or null when the file does
 *   not exist at that ref.
 * @throws {Error} When the docs repository fails or the file is over
 *   MAX_DOC_BYTES.
 */
export async function fetchDoc(ref, key, fetcher) {
  const ttl = ttlFor(ref);
  const response = await fetcher(`${DOCS_RAW_BASE}${ref}/${key}.md`, {
    redirect: "error",
    cf: { cacheEverything: true, cacheTtlByStatus: { "200-299": ttl, 404: MISSING_TTL, "500-599": 0 } },
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`docs repository answered ${response.status} for ${ref}/${key}.md`);
  const declared = Number(response.headers.get("Content-Length") ?? "0");
  if (declared > MAX_DOC_BYTES) throw new Error(`${ref}/${key}.md is ${declared} bytes`);
  return new TextDecoder("utf-8", { fatal: false }).decode(await readCapped(response, `${ref}/${key}.md`));
}

/**
 * Reads a response body, giving up as soon as it passes MAX_DOC_BYTES, so a
 * body without Content-Length cannot make the Worker buffer it whole.
 *
 * @param {Response} response The docs repository's answer.
 * @param {string} name The file, for the error message.
 * @returns {Promise<Uint8Array>} The body.
 * @throws {Error} When the body is over MAX_DOC_BYTES.
 */
async function readCapped(response, name) {
  if (response.body === null) return new Uint8Array(0);
  const reader = response.body.getReader();
  const chunks = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > MAX_DOC_BYTES) {
      await reader.cancel();
      throw new Error(`${name} is over ${MAX_DOC_BYTES} bytes`);
    }
    chunks.push(value);
  }
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

/**
 * Copies a response with extra headers.
 *
 * @param {Response} response The asset response.
 * @param {Record<string, string>} headers Headers to set.
 * @param {number} [status] Status to use instead of the response's.
 * @returns {Response} The new response.
 */
function withHeaders(response, headers, status) {
  const init = status === undefined ? { status: response.status, statusText: response.statusText } : { status };
  const out = new Response(response.body, { ...init, headers: response.headers });
  for (const [name, value] of Object.entries(SECURITY_HEADERS)) out.headers.set(name, value);
  for (const [name, value] of Object.entries(headers)) out.headers.set(name, value);
  return out;
}

/**
 * Adds `Accept` to a response's Vary header.
 *
 * @param {Headers} headers Response headers.
 * @returns {string} The new Vary value.
 */
function varyAccept(headers) {
  const current = headers.get("Vary");
  if (!current) return "Accept";
  return current.split(",").some((v) => v.trim().toLowerCase() === "accept") ? current : `${current}, Accept`;
}

/**
 * Sends a reader on to another docs URL.
 *
 * @param {URL} url The request URL.
 * @param {string} location The path to go to.
 * @returns {Response} A permanent redirect that keeps the query string.
 */
function redirect(url, location) {
  return withHeaders(new Response(null, { status: 301 }), { Location: `${url.origin}${location}${url.search}` });
}

/**
 * The not-found page, as HTML or Markdown.
 *
 * @param {URL} url The request URL.
 * @param {string} method The request method.
 * @param {boolean} markdown Whether to answer in Markdown.
 * @param {{fetch: (input: Request | URL | string, init?: RequestInit) => Promise<Response>}} assets
 *   The static assets.
 * @returns {Promise<Response>} The page with status 404.
 */
async function notFound(url, method, markdown, assets) {
  const file = markdown ? NOT_FOUND_MARKDOWN : "/404.html";
  const found = await assets.fetch(new URL(file, url), { method });
  const fallback = markdown ? "# Not found\n" : "<!doctype html><title>Not found</title><h1>Not found</h1>";
  const body = found.ok ? found : new Response(fallback);
  const type = markdown ? MARKDOWN_CONTENT_TYPE : "text/html; charset=utf-8";
  return withHeaders(body, { "Content-Type": type, Vary: "Accept", "X-Robots-Tag": "noindex" }, 404);
}

/**
 * Serves one /docs/ request.
 *
 * @param {Request} request The incoming GET or HEAD request.
 * @param {URL} url Its URL.
 * @param {DocsDeps} deps The shell, assets and fetcher.
 * @returns {Promise<Response>} The page, its Markdown, a redirect or a 404.
 */
async function serveDocs(request, url, deps) {
  const target = docsTarget(url.pathname, deps.shell.versions);
  const wantsMarkdown = target.kind === "markdown" || prefersMarkdown(request.headers.get("Accept"));
  if (target.kind === "redirect") return redirect(url, target.location);
  if (target.kind === "missing") return notFound(url, request.method, wantsMarkdown, deps.assets);

  // Rendered pages are cached by deploy, version, path and format: the shell
  // id changes whenever the picker, sidebar or page template does, and the ref
  // changes what `/docs/<path>` holds after a release. The query string is
  // not part of a docs page, so it is left out of the key.
  const format = wantsMarkdown ? "md" : "html";
  const cacheKey = new Request(
    `${url.origin}${url.pathname}?format=${format}&ref=${encodeURIComponent(target.version.ref)}&shell=${encodeURIComponent(deps.shell.id)}`,
  );
  if (deps.cache) {
    const hit = await deps.cache.match(cacheKey);
    if (hit) return request.method === "HEAD" ? new Response(null, hit) : hit;
  }

  const { version } = target;
  let key = null;
  let source = null;
  try {
    for (const candidate of target.keys) {
      source = await fetchDoc(version.ref, candidate, deps.fetch);
      if (source !== null) {
        key = candidate;
        break;
      }
    }
  } catch (err) {
    console.error(`docs fetch failed: ${err instanceof Error ? err.message : String(err)}`);
    const body = wantsMarkdown ? "# Docs unavailable\n\nThe docs could not be loaded. Try again in a minute.\n" : "The docs could not be loaded. Try again in a minute.";
    const type = wantsMarkdown ? MARKDOWN_CONTENT_TYPE : "text/plain; charset=utf-8";
    return withHeaders(new Response(body), { "Content-Type": type, "Retry-After": "60", "Cache-Control": "no-store" }, 503);
  }
  if (key === null) return notFound(url, request.method, wantsMarkdown, deps.assets);

  // A doc found under the other spelling of its path is sent to its own URL.
  const page = docUrl(key, version.base);
  if (target.kind === "page" && page !== url.pathname) return redirect(url, page);

  const rendered = renderDocPage(deps.shell, source, key, { ref: version.ref, base: version.base });
  /** @type {Record<string, string>} */
  const common = { "Cache-Control": `public, max-age=${BROWSER_TTL}, s-maxage=${ttlFor(version.ref)}`, Vary: "Accept" };
  // Only the default version is for search engines; older releases and main
  // (once a release exists) are reachable but not indexed.
  if (version !== deps.shell.versions[0]) common["X-Robots-Tag"] = "noindex";
  const response = wantsMarkdown
    ? withHeaders(new Response(rendered.markdown), { ...common, "Content-Type": MARKDOWN_CONTENT_TYPE, Link: `<${url.origin}${page}>; rel="canonical"` })
    : withHeaders(new Response(rendered.html), {
        ...common,
        "Content-Type": "text/html; charset=utf-8",
        Link: `<${url.origin}${docMarkdownUrl(key, version.base)}>; rel="alternate"; type="text/markdown"`,
      });
  if (deps.cache) deps.waitUntil(deps.cache.put(cacheKey, response.clone()));
  return request.method === "HEAD" ? new Response(null, response) : response;
}

/**
 * Everything the docs route needs besides the request.
 *
 * @typedef {object} DocsDeps
 * @property {import("./render.mjs").DocsShell} shell The baked page shell.
 * @property {{fetch: (input: Request | URL | string, init?: RequestInit) => Promise<Response>}} assets
 *   The static assets.
 * @property {typeof fetch} fetch Fetches from the docs repository.
 * @property {Cache | null} cache The edge cache for rendered pages, or null.
 * @property {(promise: Promise<unknown>) => void} waitUntil Keeps the Worker
 *   alive until a cache write finishes.
 */

/**
 * Handles one request.
 *
 * @param {Request} request The incoming request.
 * @param {{ASSETS: {fetch: (input: Request | URL | string, init?: RequestInit) => Promise<Response>}}} env
 *   Bindings: the static assets.
 * @param {{shell: import("./render.mjs").DocsShell, fetch?: typeof fetch, cache?: Cache | null,
 *   waitUntil?: (promise: Promise<unknown>) => void}} docs
 *   The docs shell, the fetcher for the docs repository (default: the global
 *   `fetch`), the cache for rendered pages (default: none) and the request
 *   context's `waitUntil`.
 * @returns {Promise<Response>} The response.
 */
export async function handle(request, env, docs) {
  const url = new URL(request.url);
  const isRead = request.method === "GET" || request.method === "HEAD";
  if (isRead && (url.pathname === "/docs" || url.pathname.startsWith("/docs/"))) {
    return serveDocs(request, url, {
      shell: docs.shell,
      assets: env.ASSETS,
      fetch: docs.fetch ?? fetch,
      cache: docs.cache ?? null,
      waitUntil: docs.waitUntil ?? (() => {}),
    });
  }
  const md = isRead ? markdownPath(url.pathname) : null;

  if (md !== null && prefersMarkdown(request.headers.get("Accept"))) {
    // Keep conditional headers so agents can revalidate with an ETag.
    const headers = new Headers({ Accept: MARKDOWN_CONTENT_TYPE });
    for (const name of CONDITIONAL_HEADERS) {
      const value = request.headers.get(name);
      if (value !== null) headers.set(name, value);
    }
    const found = await env.ASSETS.fetch(new URL(md, url), { method: request.method, headers });
    if (found.ok || found.status === 304) {
      return withHeaders(found, {
        "Content-Type": MARKDOWN_CONTENT_TYPE,
        Vary: varyAccept(found.headers),
        Link: `<${url.origin}${htmlPath(md) ?? url.pathname}>; rel="canonical"`,
      });
    }
    return notFound(url, request.method, true, env.ASSETS);
  }

  const response = await env.ASSETS.fetch(request);
  if (md !== null) {
    const contentType = response.headers.get("Content-Type") ?? "";
    const headers = { Vary: varyAccept(response.headers) };
    if (response.ok && contentType.startsWith("text/html")) {
      headers.Link = `<${url.origin}${md}>; rel="alternate"; type="text/markdown"`;
    }
    return withHeaders(response, headers);
  }
  if (isRead && url.pathname === NOT_FOUND_MARKDOWN) {
    return withHeaders(response, { "Content-Type": MARKDOWN_CONTENT_TYPE, "X-Robots-Tag": "noindex" });
  }
  const page = isRead ? htmlPath(url.pathname) : null;
  if (page !== null && (response.ok || response.status === 304)) {
    // A Markdown copy requested by name: point search engines at the page.
    return withHeaders(response, { "Content-Type": MARKDOWN_CONTENT_TYPE, Link: `<${url.origin}${page}>; rel="canonical"` });
  }
  return response;
}
