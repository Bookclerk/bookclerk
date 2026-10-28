// The bookclerk-site Worker: serves dist/ through the ASSETS binding, renders
// /docs/ from the docs repository, and answers requests that prefer Markdown
// with each page's Markdown version.
//
// wrangler.toml runs this script first only for page paths (`run_worker_first`);
// stylesheets, images and the like are served straight from the assets.
//
// Docs: `/docs/<path>` is the default docs version (the newest release with
// published docs, else main) and `/docs/v/<ref>/<path>` any other version the
// site was built with (main and every published release tag). Each page is read from
// raw.githubusercontent.com/Bookclerk/bookclerk-docs at that ref and rendered
// with render.ts into the shell build.ts baked (generated/docs-shell.json).
// Only refs in the shell's version list and paths made of plain segments are
// fetched, responses over MAX_DOC_BYTES are refused, and rendered pages are
// cached at the edge (tags for a day, main for five minutes). When the docs
// repository cannot be read, the reader gets a 503 page in the site's look
// that links to the same doc on GitHub. Beside each HTML page the Worker
// reads the version's last-updated.json (parsed by last-updated.ts) for the
// page's "Last updated" date; when that file cannot be read the page simply
// has no date.
//
// Sitemap: /sitemap.xml is a sitemap index with one sitemap per section:
// /sitemap-site.xml for the pages outside /docs/ and the docs home, and one per top docs
// folder (/sitemap-guide.xml, /sitemap-api.xml). Together they list every
// page of the default docs version, from that version's last-updated.json
// (which names each page, the generated API reference included), with each
// page's date as `<lastmod>`. They are cached at the edge like the version's
// pages. When the file cannot be read /sitemap.xml is the flat sitemap
// build.ts wrote (the pages and the sidebar's docs, no dates), for a minute.
//
// Content negotiation: when the Accept header ranks `text/markdown` (or
// `text/x-markdown`) at least as high as HTML, `/`, `/download`, `/privacy`, `/security` and
// `/docs/<path>` return their Markdown version instead, with
// `Content-Type: text/markdown` and a `Link: rel="canonical"` to the HTML page.
// HTML responses carry `Link: rel="alternate"` to their Markdown version.
// Every page response says `Vary: Accept` so caches keep the two apart.
//
// Compression: the Worker compresses the text it answers with (Brotli, else
// gzip, as the client's Accept-Encoding allows) by setting Content-Encoding
// and letting the runtime encode the body. Cloudflare's edge compresses only
// the media types on its default list, which has `text/x-markdown` but not
// `text/markdown`, so without this the Markdown versions went out
// uncompressed. Pages are cached at the edge uncompressed and encoded per
// request; responses that are already encoded or say `no-transform` are left
// alone. Files served without the Worker (styles, fonts, llms.txt) are
// compressed by the edge, whose default list covers their types.
//
// The script reads only the request's method, path, Accept and
// Accept-Encoding headers (and conditional headers it passes on), and
// fetches only from its own assets and the docs repository.

import { API_INDEX, API_SCHEMAS } from "./api-reference.ts";
import { datedPages, LAST_UPDATED_FILE, MAX_LAST_UPDATED_BYTES, pageDate } from "./last-updated.ts";
import type { IsoDate } from "./last-updated.ts";
import {
  apiSearchPages,
  docMarkdownUrl,
  docUrl,
  renderDocPage,
  renderSitemapPage,
  renderUnavailablePage,
  SEARCH_INDEX,
  searchPage,
} from "./render.ts";
import type { DocsShell, DocsVersion, SearchPage, SitemapPageLink } from "./render.ts";
import { MAX_SITEMAP_URLS, sitemapIndexXml, sitemapXml } from "./sitemap.ts";
import type { SitemapEntry } from "./sitemap.ts";

/** The static assets binding, or a stand-in in tests. */
export interface Assets {
  /** Fetches one asset; the URL's path names the file. */
  fetch(input: Request | URL | string, init?: RequestInit): Promise<Response>;
}

/** Fetches a docs file from the docs repository: `fetch`, or a stand-in in tests. */
export type DocsFetch = (url: string, init: RequestInit) => Promise<Response>;

/** The edge cache for rendered docs pages (`caches.default`), or a stand-in in tests. */
export interface PageCache {
  /** Looks a page up by its cache key. */
  match(key: Request): Promise<Response | undefined>;
  /** Stores a page under its cache key. */
  put(key: Request, response: Response): Promise<void>;
}

/** Worker bindings the site uses. */
export interface Env {
  /** The static assets: dist/. */
  ASSETS: Assets;
}

/** What the docs route needs from the entry point: see route(). */
export interface DocsOptions {
  /** The baked page shell. */
  shell: DocsShell;
  /** Fetches from the docs repository; default: the global `fetch`. */
  fetch?: DocsFetch;
  /** The cache for rendered pages; default: none. */
  cache?: PageCache | null;
  /** The request context's `waitUntil`; default: does nothing. */
  waitUntil?: (promise: Promise<unknown>) => void;
}

/**
 * Response headers every page gets; the same policy as src/_headers.
 *
 * The site runs no scripts of its own. The one exception, approved by the
 * project owner, is Cloudflare Web Analytics: the edge injects its beacon
 * (`static.cloudflareinsights.com/beacon.min.js`, with an `integrity` hash)
 * into HTML pages, and the beacon reports to `/cdn-cgi/rum` on this origin,
 * or to `cloudflareinsights.com` when embedded by hand. The beacon's URL
 * carries a version after the file name, so its host is allowed rather than
 * one path.
 */
export const SECURITY_HEADERS: Readonly<Record<string, string>> = {
  "Content-Security-Policy":
    "default-src 'none'; style-src 'self'; style-src-attr 'unsafe-inline'; font-src 'self'; img-src 'self'; manifest-src 'self'; script-src 'self' https://static.cloudflareinsights.com; connect-src 'self' https://cloudflareinsights.com; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
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
export const PAGE_MARKDOWN: ReadonlyMap<string, string> = new Map([
  ["/", "/index.md"],
  ["/index.html", "/index.md"],
  ["/download", "/download.md"],
  ["/download.html", "/download.md"],
  ["/privacy", "/privacy.md"],
  ["/privacy.html", "/privacy.md"],
  ["/security", "/security.md"],
  ["/security.html", "/security.md"],
]);

/**
 * Media types the Worker compresses: text, and the structured-text types the
 * site serves. Fonts and images are already compressed.
 */
const COMPRESSIBLE_RE =
  /^(?:text\/[a-z0-9.+-]+|application\/(?:[a-z0-9.-]+\+)?(?:json|xml)|image\/svg\+xml)$/;

/** Content codings the Worker can apply, most preferred first. */
const ENCODINGS = ["br", "gzip"] as const;

/** A content coding the Worker can apply. */
export type Encoding = (typeof ENCODINGS)[number];

/** Request headers passed on when fetching a Markdown version. */
const CONDITIONAL_HEADERS = ["If-None-Match", "If-Modified-Since"];

/** Markdown version of the not-found page. */
const NOT_FOUND_MARKDOWN = "/404.md";

/** The sitemap's path, which robots.txt names. */
const SITEMAP_PATH = "/sitemap.xml";

/** The human-readable sitemap page; its Markdown version adds `.md`. */
const SITEMAP_PAGE_PATH = "/sitemap";

/** Content type of the sitemap. */
const XML_CONTENT_TYPE = "application/xml; charset=utf-8";

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
 * @param ref - The docs version's git ref.
 * @returns Seconds: MAIN_TTL for main, TAG_TTL for a release tag.
 */
function ttlFor(ref: string): number {
  return ref === "main" ? MAIN_TTL : TAG_TTL;
}

/** A quality value as RFC 9110 writes it: 0 to 1 with at most three decimals. */
const QVALUE_RE = /^(?:0(?:\.[0-9]{0,3})?|1(?:\.0{0,3})?)$/;

/**
 * Reads the quality value of one Accept or Accept-Encoding list member.
 *
 * @param params - The member's `;`-separated parameters.
 * @returns Its `q` (0-1): 1 when absent, 0 when not a valid qvalue.
 */
function qualityOf(params: readonly string[]): number {
  let q = 1;
  for (const param of params) {
    const eq = param.indexOf("=");
    const key = (eq < 0 ? param : param.slice(0, eq)).trim().toLowerCase();
    if (key !== "q") continue;
    const value = eq < 0 ? "" : param.slice(eq + 1).trim();
    q = QVALUE_RE.test(value) ? Number(value) : 0;
  }
  return q;
}

/**
 * Reads how strongly an Accept header asks for Markdown and for HTML.
 *
 * The most specific range that matches decides, as RFC 9110 says: an HTML
 * type listed by name overrides `text/*`, which overrides `*\/*`. Wildcards
 * count for HTML only, so a browser or a generic client keeps getting HTML.
 *
 * @param accept - The request's Accept header.
 * @returns The quality value (0-1) given
 *   to Markdown and to HTML.
 */
export function acceptWeights(accept: string | null): { markdown: number; html: number } {
  let markdown = 0;
  /** Highest q per specificity: 2 = an HTML type by name, 1 = text/*, 0 = *\/*. */
  const html: [number | null, number | null, number | null] = [null, null, null];
  for (const part of (accept ?? "").split(",")) {
    const [rawType = "", ...params] = part.split(";");
    const type = rawType.trim().toLowerCase();
    if (!type) continue;
    const q = qualityOf(params);
    const rank = HTML_TYPES.has(type) ? 2 : type === "text/*" ? 1 : type === "*/*" ? 0 : null;
    if (MARKDOWN_TYPES.has(type)) markdown = Math.max(markdown, q);
    else if (rank !== null) html[rank] = Math.max(html[rank] ?? 0, q);
  }
  return { markdown, html: html[2] ?? html[1] ?? html[0] ?? 0 };
}

/**
 * Decides whether a request wants Markdown.
 *
 * @param accept - The request's Accept header.
 * @returns True when a Markdown type is acceptable and ranked at
 *   least as high as HTML.
 */
export function prefersMarkdown(accept: string | null): boolean {
  const { markdown, html } = acceptWeights(accept);
  return markdown > 0 && markdown >= html;
}

/**
 * Maps a page path outside /docs/ to the path of its Markdown version.
 *
 * @param pathname - The request path.
 * @returns The `.md` asset path, or null when the path is not
 *   such a page.
 */
export function markdownPath(pathname: string): string | null {
  return PAGE_MARKDOWN.get(pathname) ?? null;
}

/**
 * Maps a Markdown asset path outside /docs/ back to its page.
 *
 * @param pathname - A `.md` path.
 * @returns The page path, or null for other paths.
 */
export function htmlPath(pathname: string): string | null {
  for (const [page, md] of PAGE_MARKDOWN) {
    if (md === pathname && !page.endsWith(".html")) return page;
  }
  return null;
}

/**
 * What a /docs/ request names: a page, a Markdown copy asked for by name
 * (`<path>.md`), a URL to send the reader on to, or nothing the site serves.
 */
export type DocsTarget =
  | {
      /** Send the reader on. */
      kind: "redirect";
      /** The path to go to. */
      location: string;
    }
  | {
      /** Nothing the site serves. */
      kind: "missing";
    }
  | {
      /** The version's search index, which the sidebar filter reads. */
      kind: "search";
      /** The docs version. */
      version: DocsVersion;
    }
  | {
      /** A page, or its Markdown copy asked for by name. */
      kind: "page" | "markdown";
      /** The docs version. */
      version: DocsVersion;
      /** The doc keys that can hold it, most likely first. */
      keys: [string, ...string[]];
    };

/**
 * Parses a /docs/ request path.
 *
 * `/docs/v/<ref>` selects a version from `versions`; the default version
 * (the first) is served without the prefix, so its prefixed URLs redirect.
 * A path may name a directory (`api` → `api/README`); the slash form
 * (`api/`) is looked up too, and the caller redirects it to the page's
 * slash-free URL. `.html` paths left from the static site redirect to the page. Other dots are part of the
 * name (`api/python/bookclerk_plugin_sdk.workerd`); the docs publish only
 * Markdown, so a path like `x.svg` is looked up as `x.svg.md` and not found.
 *
 * @param pathname - The request path, starting with `/docs`.
 * @param versions - Every docs version, the default first.
 * @returns What to serve.
 */
export function docsTarget(pathname: string, versions: readonly DocsVersion[]): DocsTarget {
  let version = versions[0];
  if (version === undefined) return { kind: "missing" };
  let rest = pathname === "/docs" ? "" : pathname.slice("/docs/".length);
  if (rest === "v" || rest.startsWith("v/")) {
    const [, ref = "", ...tail] = rest.split("/");
    const found = versions.find((v) => v.ref === ref);
    if (found === undefined) return { kind: "missing" };
    rest = tail.join("/");
    if (!DOC_REST_RE.test(rest)) return { kind: "missing" };
    if (found === versions[0]) {
      // Straight to the final URL: the unprefixed path may itself redirect
      // (`x.html`), and a redirect should take one hop.
      const location = `/docs/${rest}`.replace(/\/$/, "");
      const next = docsTarget(location, versions);
      return next.kind === "redirect" ? next : { kind: "redirect", location };
    }
    version = found;
  }
  if (!DOC_REST_RE.test(rest)) return { kind: "missing" };
  if (rest === SEARCH_INDEX) return { kind: "search", version };
  const last = rest.slice(rest.lastIndexOf("/") + 1);
  if (last.endsWith(".md"))
    return { kind: "markdown", version, keys: [rest.slice(0, -".md".length)] };
  if (last.endsWith(".html")) {
    const page =
      last === "index.html" ? rest.slice(0, -"index.html".length) : rest.slice(0, -".html".length);
    const location = (version.base + page).replace(/\/$/, "");
    // `a.html/index.html` lands on `a.html`, which would redirect again.
    const next = docsTarget(location, versions);
    return next.kind === "redirect" ? next : { kind: "redirect", location };
  }
  if (rest === "" || rest.endsWith("/")) {
    const keys: [string, ...string[]] = [`${rest}README`, `${rest}index`];
    if (rest !== "") keys.push(rest.slice(0, -1));
    return { kind: "page", version, keys };
  }
  return { kind: "page", version, keys: [rest, `${rest}/README`, `${rest}/index`] };
}

/**
 * Reads one docs file from the docs repository, through the edge cache.
 *
 * @param ref - The docs version's git ref.
 * @param key - The doc's key.
 * @param fetcher - `fetch`, or a stand-in in tests.
 * @returns The Markdown, or null when the file does
 *   not exist at that ref.
 * @throws {Error} When the docs repository fails or redirects, or the file
 *   is over MAX_DOC_BYTES.
 */
export async function fetchDoc(
  ref: string,
  key: string,
  fetcher: DocsFetch,
): Promise<string | null> {
  return fetchDocsFile(ref, `${key}.md`, fetcher, MAX_DOC_BYTES);
}

/**
 * Reads a docs version's Web API index, whose route groups the sidebar of
 * the reference's other pages lists.
 *
 * Like the date, the list is a nicety: any failure is logged and the page
 * is shown with a shorter sidebar.
 *
 * @param ref - The docs version's git ref.
 * @param fetcher - `fetch`, or a stand-in in tests.
 * @returns The index, or whether it is missing or could not be read.
 */
async function fetchApiIndex(ref: string, fetcher: DocsFetch): Promise<DatesFile> {
  try {
    const text = await fetchDoc(ref, API_INDEX, fetcher);
    return text === null ? { kind: "missing" } : { kind: "file", text };
  } catch (err) {
    console.error(
      `Web API index fetch failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return { kind: "failed" };
  }
}

/** What reading a docs version's last-updated.json gave. */
type DatesFile =
  /** The file's text, not yet validated. */
  | { kind: "file"; text: string }
  /** The version has no such file (published before it existed). */
  | { kind: "missing" }
  /** The read failed for now; a page rendered without its date is not cached at the edge. */
  | { kind: "failed" };

/**
 * Reads a docs version's last-updated.json, which dates its pages.
 *
 * The date is a nicety: any failure is logged and gives no date, so a page
 * never fails because its date could not be read.
 *
 * @param ref - The docs version's git ref.
 * @param fetcher - `fetch`, or a stand-in in tests.
 * @returns The file, or whether it is missing or could not be read.
 */
async function fetchLastUpdated(ref: string, fetcher: DocsFetch): Promise<DatesFile> {
  try {
    const text = await fetchDocsFile(ref, LAST_UPDATED_FILE, fetcher, MAX_LAST_UPDATED_BYTES);
    return text === null ? { kind: "missing" } : { kind: "file", text };
  } catch (err) {
    console.error(`docs dates unavailable: ${err instanceof Error ? err.message : String(err)}`);
    return { kind: "failed" };
  }
}

/**
 * Reads one file of a docs version from the docs repository, through the
 * edge cache.
 *
 * @param ref - The docs version's git ref.
 * @param file - The file's path in the version.
 * @param fetcher - `fetch`, or a stand-in in tests.
 * @param limit - Largest body accepted, in bytes.
 * @returns The text, or null when the file does not exist at that ref.
 * @throws {Error} When the docs repository fails or redirects, or the file
 *   is over `limit`.
 */
async function fetchDocsFile(
  ref: string,
  file: string,
  fetcher: DocsFetch,
  limit: number,
): Promise<string | null> {
  const ttl = ttlFor(ref);
  const response = await fetcher(`${DOCS_RAW_BASE}${ref}/${file}`, {
    // The Workers runtime rejects `redirect: "error"` outright, so redirects
    // are taken as answers and refused below.
    redirect: "manual",
    cf: {
      cacheEverything: true,
      cacheTtlByStatus: { "200-299": ttl, 404: MISSING_TTL, "500-599": 0 },
    },
  });
  if (response.status === 404) return null;
  if (response.status >= 300 && response.status < 400)
    throw new Error(`docs repository redirected ${ref}/${file}`);
  if (!response.ok)
    throw new Error(`docs repository answered ${response.status} for ${ref}/${file}`);
  const declared = Number(response.headers.get("Content-Length") ?? "0");
  if (declared > limit) throw new Error(`${ref}/${file} is ${declared} bytes`);
  // UTF-8, with invalid bytes replaced rather than refused.
  return new TextDecoder().decode(await readCapped(response, `${ref}/${file}`, limit));
}

/**
 * Reads a response body, giving up as soon as it passes `limit`, so a body
 * without Content-Length cannot make the Worker buffer it whole.
 *
 * @param response - The docs repository's answer.
 * @param name - The file, for the error message.
 * @param limit - Largest body accepted, in bytes.
 * @returns The body.
 * @throws {Error} When the body is over `limit`.
 */
async function readCapped(response: Response, name: string, limit: number): Promise<Uint8Array> {
  if (response.body === null) return new Uint8Array(0);
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > limit) {
      await reader.cancel();
      throw new Error(`${name} is over ${limit} bytes`);
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
 * @param response - The asset response.
 * @param headers - Headers to set.
 * @param status - Status to use instead of the response's.
 * @returns The new response.
 */
function withHeaders(
  response: Response,
  headers: Readonly<Record<string, string>>,
  status?: number,
): Response {
  const init =
    status === undefined
      ? { status: response.status, statusText: response.statusText }
      : { status };
  const out = new Response(response.body, { ...init, headers: response.headers });
  for (const [name, value] of Object.entries(SECURITY_HEADERS)) out.headers.set(name, value);
  for (const [name, value] of Object.entries(headers)) out.headers.set(name, value);
  return out;
}

/**
 * Adds a request header name to a response's Vary header.
 *
 * @param headers - Response headers.
 * @param name - The header name to add, such as `Accept`.
 * @returns The new Vary value.
 */
function varyAdd(headers: Headers, name: string): string {
  const current = headers.get("Vary");
  if (!current) return name;
  return current.split(",").some((v) => v.trim().toLowerCase() === name.toLowerCase())
    ? current
    : `${current}, ${name}`;
}

/**
 * Sends a reader on to another docs URL.
 *
 * @param url - The request URL.
 * @param location - The path to go to.
 * @returns A permanent redirect that keeps the query string.
 */
function redirect(url: URL, location: string): Response {
  return withHeaders(new Response(null, { status: 301 }), {
    Location: `${url.origin}${location}${url.search}`,
  });
}

/**
 * Sends a reader whose docs link leads nowhere to the docs home page, so an
 * old or mistyped link still lands somewhere useful. The redirect is
 * temporary: the path may name a page again later.
 *
 * @param url - The request URL.
 * @param base - URL prefix of the docs version the request named, or
 *   `/docs/` when it named none the site serves.
 * @param markdown - Whether the request asked for a Markdown copy by name, in
 *   which case it goes to the home page's Markdown copy.
 * @returns A 302 to the docs home page, without the query string.
 */
function toDocsHome(url: URL, base: string, markdown: boolean): Response {
  const location = markdown ? docMarkdownUrl("README", base) : docUrl("README", base);
  return withHeaders(new Response(null, { status: 302 }), {
    Location: `${url.origin}${location}`,
    "Cache-Control": "no-store",
  });
}

/**
 * The not-found page, as HTML or Markdown.
 *
 * @param url - The request URL.
 * @param method - The request method.
 * @param markdown - Whether to answer in Markdown.
 * @param assets - The static assets.
 * @returns The page with status 404.
 */
async function notFound(
  url: URL,
  method: string,
  markdown: boolean,
  assets: Assets,
): Promise<Response> {
  const file = markdown ? NOT_FOUND_MARKDOWN : "/404.html";
  const found = await assets.fetch(new URL(file, url), { method });
  const fallback = markdown
    ? "# Not found\n"
    : "<!doctype html><title>Not found</title><h1>Not found</h1>";
  const body = found.ok ? found : new Response(fallback);
  const type = markdown ? MARKDOWN_CONTENT_TYPE : "text/html; charset=utf-8";
  return withHeaders(
    body,
    { "Content-Type": type, Vary: "Accept", "X-Robots-Tag": "noindex" },
    404,
  );
}

/**
 * Serves one /docs/ request.
 *
 * @param request - The incoming GET or HEAD request.
 * @param url - Its URL.
 * @param deps - The shell, assets and fetcher.
 * @returns The page, its Markdown, a redirect or a 404.
 */
async function serveDocs(request: Request, url: URL, deps: DocsDeps): Promise<Response> {
  const target = docsTarget(url.pathname, deps.shell.versions);
  const wantsMarkdown =
    target.kind === "markdown" || prefersMarkdown(request.headers.get("Accept"));
  if (target.kind === "redirect") return redirect(url, target.location);
  if (target.kind === "missing") return toDocsHome(url, "/docs/", url.pathname.endsWith(".md"));
  if (target.kind === "search") return serveSearchIndex(request, url, deps, target.version);

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
  // Fetched beside the page and only for HTML, which shows the date.
  const dates = wantsMarkdown ? null : fetchLastUpdated(version.ref, deps.fetch);
  // A Web API page's sidebar lists the route groups its index names.
  const apiIndex =
    wantsMarkdown || !target.keys.some((k) => k.startsWith("api/web/") && k !== API_INDEX)
      ? null
      : fetchApiIndex(version.ref, deps.fetch);
  let key: string | null = null;
  let source: string | null = null;
  // The last key asked for, so the unavailable page links on GitHub to the
  // file that could not be read rather than one already known to be missing.
  // A slash-free path can name a page or a directory; trying the spelling
  // the sidebar lists first saves a docs repository miss for directories.
  const listed = new Set(deps.shell.nav.flatMap((group) => group.pages.map((p) => p.key)));
  const keys = [
    ...target.keys.filter((k) => listed.has(k)),
    ...target.keys.filter((k) => !listed.has(k)),
  ];
  let attempted = keys[0] ?? target.keys[0];
  try {
    for (const candidate of keys) {
      attempted = candidate;
      source = await fetchDoc(version.ref, candidate, deps.fetch);
      if (source !== null) {
        key = candidate;
        break;
      }
    }
  } catch (err) {
    console.error(`docs fetch failed: ${err instanceof Error ? err.message : String(err)}`);
    const page = renderUnavailablePage(deps.shell, keys[0] ?? target.keys[0], {
      ref: version.ref,
      base: version.base,
      githubKey: attempted,
    });
    const body = request.method === "HEAD" ? null : wantsMarkdown ? page.markdown : page.html;
    const type = wantsMarkdown ? MARKDOWN_CONTENT_TYPE : "text/html; charset=utf-8";
    return withHeaders(
      new Response(body),
      {
        "Content-Type": type,
        "Retry-After": "60",
        "Cache-Control": "no-store",
        Vary: "Accept",
        "X-Robots-Tag": "noindex",
      },
      503,
    );
  }
  if (key === null || source === null) {
    // The home page itself missing would send the reader in a loop.
    const home =
      target.kind === "markdown"
        ? docMarkdownUrl("README", version.base)
        : docUrl("README", version.base);
    if (url.pathname === home) return notFound(url, request.method, wantsMarkdown, deps.assets);
    return toDocsHome(url, version.base, target.kind === "markdown");
  }

  // A doc found under the other spelling of its path is sent to its own URL.
  const page = docUrl(key, version.base);
  if (target.kind === "page" && page !== url.pathname) return redirect(url, page);

  const datesFile = dates === null ? null : await dates;
  const updated = datesFile?.kind === "file" ? pageDate(datesFile.text, key) : null;
  // A page whose date could not be read is shown, but kept out of the edge
  // cache so the date comes back once the docs repository answers again.
  const index = apiIndex === null || key === API_INDEX ? null : await apiIndex;
  const cacheable = datesFile?.kind !== "failed" && index?.kind !== "failed";
  const rendered = renderDocPage(deps.shell, source, key, {
    ref: version.ref,
    base: version.base,
    updated,
    apiIndex: index?.kind === "file" ? index.text : null,
  });
  const cacheControl = cacheable
    ? `public, max-age=${BROWSER_TTL}, s-maxage=${ttlFor(version.ref)}`
    : `public, max-age=${MISSING_TTL}, s-maxage=0`;
  const common: Record<string, string> = { "Cache-Control": cacheControl, Vary: "Accept" };
  // Only the default version is for search engines; older releases and main
  // (once a release exists) are reachable but not indexed.
  if (version !== deps.shell.versions[0]) common["X-Robots-Tag"] = "noindex";
  const response = wantsMarkdown
    ? withHeaders(new Response(rendered.markdown), {
        ...common,
        "Content-Type": MARKDOWN_CONTENT_TYPE,
        Link: `<${url.origin}${page}>; rel="canonical"`,
      })
    : withHeaders(new Response(rendered.html), {
        ...common,
        "Content-Type": "text/html; charset=utf-8",
        Link: `<${url.origin}${docMarkdownUrl(key, version.base)}>; rel="alternate"; type="text/markdown"`,
      });
  if (deps.cache && cacheable) deps.waitUntil(deps.cache.put(cacheKey, response.clone()));
  return request.method === "HEAD" ? new Response(null, response) : response;
}

/** How many docs the search index reads from the docs repository at once. */
const SEARCH_FETCHES = 6;

/**
 * Serves a docs version's search index: every sidebar page with its
 * headings, the Web API's route groups with their routes (read from its
 * index), and the schemas page, as JSON for the sidebar filter in docs.js.
 *
 * Built from the docs repository at request time, like the pages, and
 * cached at the edge by version and deploy. A doc that cannot be read is
 * listed by its title alone, and the index is then cached only briefly.
 *
 * @param request - The request (GET or HEAD).
 * @param url - Its URL.
 * @param deps - The shell, fetcher and cache.
 * @param version - The docs version.
 * @returns The index, `{ "pages": SearchPage[] }`.
 */
async function serveSearchIndex(
  request: Request,
  url: URL,
  deps: DocsDeps,
  version: DocsVersion,
): Promise<Response> {
  const cacheKey = new Request(
    `${url.origin}${url.pathname}?ref=${encodeURIComponent(version.ref)}&shell=${encodeURIComponent(deps.shell.id)}`,
  );
  if (deps.cache) {
    const hit = await deps.cache.match(cacheKey);
    if (hit) return request.method === "HEAD" ? new Response(null, hit) : hit;
  }
  const listed = deps.shell.nav.flatMap((group) =>
    group.pages.map((page) => ({ ...page, section: group.title })),
  );
  if (listed.some((page) => page.key === API_INDEX))
    listed.push({ key: API_SCHEMAS, title: "Schemas", section: "Web API" });
  let failed = false;
  const pages: SearchPage[][] = Array.from({ length: listed.length }, () => []);
  let next = 0;
  const work = async (): Promise<void> => {
    for (let i = next++; i < listed.length; i = next++) {
      const page = listed[i];
      if (page === undefined) continue;
      const entry: SearchPage = {
        section: page.section,
        title: page.title,
        url: docUrl(page.key, version.base),
        headings: [],
      };
      try {
        const source = await fetchDoc(version.ref, page.key, deps.fetch);
        if (source === null) continue;
        const found = searchPage(source, page.key, page.section, page.title, version.base);
        pages[i] =
          page.key === API_INDEX ? [found, ...apiSearchPages(source, version.base)] : [found];
      } catch (err) {
        console.error(
          `search index: ${page.key}: ${err instanceof Error ? err.message : String(err)}`,
        );
        failed = true;
        pages[i] = [entry];
      }
    }
  };
  await Promise.all(Array.from({ length: SEARCH_FETCHES }, work));
  // A partial index is kept only briefly, so the missing headings come back
  // once the docs repository answers again without every request in the
  // meantime repeating the whole fan-out.
  const response = withHeaders(new Response(JSON.stringify({ pages: pages.flat() })), {
    "Content-Type": "application/json; charset=utf-8",
    "Cache-Control": failed
      ? `public, max-age=${MISSING_TTL}, s-maxage=${MISSING_TTL}`
      : `public, max-age=${BROWSER_TTL}, s-maxage=${ttlFor(version.ref)}`,
    "X-Robots-Tag": "noindex",
  });
  if (deps.cache) deps.waitUntil(deps.cache.put(cacheKey, response.clone()));
  return request.method === "HEAD" ? new Response(null, response) : response;
}

/**
 * Sitemap sections, as `/sitemap-<name>.xml`. They sit at the site root
 * because a sitemap may only list URLs under its own directory
 * (https://www.sitemaps.org/protocol.html#location).
 */
const SECTION_PATH_RE = /^\/sitemap-([a-z0-9][a-z0-9-]*)\.xml$/;

/** A docs folder name that can be a section name as it is. */
const SECTION_NAME_RE = /^[a-z0-9][a-z0-9-]*$/;

/** The section holding the pages outside /docs/. */
const SITE_SECTION = "site";

/** The section holding docs in folders whose name cannot be a section name. */
const DOCS_SECTION = "docs";

/**
 * Names the sitemap section a doc belongs to: its top folder (`guide`,
 * `server`, `api`), so Search Console reports indexing per shelf. Docs at
 * the top of a version (the docs home) join the site's pages.
 *
 * @param key - The doc's key.
 * @returns The section name, made of lowercase letters, digits and `-`.
 */
export function sitemapSection(key: string): string {
  const slash = key.indexOf("/");
  if (slash === -1) return SITE_SECTION;
  const top = key.slice(0, slash);
  return SECTION_NAME_RE.test(top) && top !== SITE_SECTION ? top : DOCS_SECTION;
}

/**
 * Serves /sitemap.xml and its sections.
 *
 * `/sitemap.xml` is a sitemap index naming one sitemap per section:
 * `/sitemap-site.xml` for the pages outside /docs/ and the docs home, then one per top docs
 * folder of the default version (`/sitemap-guide.xml`, `/sitemap-api.xml`),
 * each with the newest date of its pages. The sections list every page that
 * version's last-updated.json dates and the docs route serves at its URL,
 * with its date.
 *
 * @param request - The incoming GET or HEAD request.
 * @param url - Its URL: `/sitemap.xml` or `/sitemap-<name>.xml`.
 * @param deps - The shell, assets and fetcher.
 * @returns The index or section. When the dates file is missing, cannot be
 *   read or lists no page the docs route serves, `/sitemap.xml` is the flat
 *   sitemap build.ts wrote and sections are 503 (a failed read) or 404.
 */
async function serveSitemap(request: Request, url: URL, deps: DocsDeps): Promise<Response> {
  const isIndex = url.pathname === SITEMAP_PATH;
  const [version] = deps.shell.versions;
  if (version === undefined)
    return isIndex
      ? fallbackSitemap(request, url, deps.assets, MISSING_TTL)
      : notFound(url, request.method, false, deps.assets);
  const cacheKey = new Request(
    `${url.origin}${url.pathname}?ref=${encodeURIComponent(version.ref)}&shell=${encodeURIComponent(deps.shell.id)}`,
  );
  if (deps.cache) {
    const hit = await deps.cache.match(cacheKey);
    if (hit) return request.method === "HEAD" ? new Response(null, hit) : hit;
  }
  const { file, docs: listed } = await listDocs(version, deps);
  const sections = new Map<string, SitemapEntry[]>([
    [SITE_SECTION, siteSitemapPages(deps.shell).map((path) => ({ path, lastmod: null }))],
  ]);
  const docs = listed.length;
  for (const { key, path, date } of listed) {
    const name = sitemapSection(key);
    const section = sections.get(name) ?? [];
    if (section.length === 0) sections.set(name, section);
    section.push({ path, lastmod: date });
  }

  let response: Response;
  const cached = `public, max-age=${BROWSER_TTL}, s-maxage=${ttlFor(version.ref)}`;
  if (docs === 0) {
    // A tag published without the file never gets one, so its fallback
    // keeps like its pages; a failed or empty read is retried in a minute.
    if (!isIndex) {
      if (file.kind === "missing") return notFound(url, request.method, false, deps.assets);
      return withHeaders(
        new Response(null),
        { "Retry-After": "60", "Cache-Control": "no-store" },
        503,
      );
    }
    const ttl = file.kind === "missing" ? ttlFor(version.ref) : MISSING_TTL;
    response = await fallbackSitemap(request, url, deps.assets, ttl);
    if (file.kind !== "missing" || !response.ok) return response;
  } else if (isIndex) {
    const index = [...sections].map(([name, entries]) => ({
      path: `/sitemap-${name}.xml`,
      lastmod: entries.reduce<SitemapEntry["lastmod"]>(
        (newest, e) =>
          e.lastmod !== null && (newest === null || e.lastmod > newest) ? e.lastmod : newest,
        null,
      ),
    }));
    response = withHeaders(new Response(sitemapIndexXml(deps.shell.origin, index)), {
      "Content-Type": XML_CONTENT_TYPE,
      "Cache-Control": cached,
    });
  } else {
    const entries = sections.get(SECTION_PATH_RE.exec(url.pathname)?.[1] ?? "");
    if (entries === undefined) return notFound(url, request.method, false, deps.assets);
    if (entries.length > MAX_SITEMAP_URLS)
      console.error(
        `${url.pathname} lists the first ${MAX_SITEMAP_URLS} of ${entries.length} pages`,
      );
    response = withHeaders(new Response(sitemapXml(deps.shell.origin, entries)), {
      "Content-Type": XML_CONTENT_TYPE,
      "Cache-Control": cached,
    });
  }
  if (deps.cache && request.method === "GET")
    deps.waitUntil(deps.cache.put(cacheKey, response.clone()));
  return request.method === "HEAD" ? new Response(null, response) : response;
}

/** A doc of the default version that the sitemaps list. */
interface ListedDoc {
  /** Its key. */
  key: string;
  /** Its URL path. */
  path: string;
  /** When it last changed. */
  date: IsoDate;
}

/**
 * Lists the docs of a version that its last-updated.json dates and that the
 * docs route serves at their URL as themselves (not a `v/…` path, a
 * Markdown copy or another spelling that redirects).
 *
 * @param version - The docs version.
 * @param deps - The shell and fetcher.
 * @returns What reading the dates file gave, and the docs in its order.
 */
async function listDocs(
  version: DocsVersion,
  deps: DocsDeps,
): Promise<{ file: DatesFile; docs: ListedDoc[] }> {
  const file = await fetchLastUpdated(version.ref, deps.fetch);
  const docs: ListedDoc[] = [];
  for (const { key, date } of file.kind === "file" ? (datedPages(file.text) ?? []) : []) {
    const path = docUrl(key, version.base);
    const target = docsTarget(path, deps.shell.versions);
    if (target.kind === "page" && target.version === version && target.keys.includes(key))
      docs.push({ key, path, date });
  }
  return { file, docs };
}

/**
 * The pages outside /docs/ that the sitemaps list: build.ts's indexable
 * pages and the human-readable sitemap.
 *
 * @param shell - The baked page shell.
 * @returns Their URL paths.
 */
function siteSitemapPages(shell: DocsShell): string[] {
  return [...shell.pages, SITEMAP_PAGE_PATH];
}

/**
 * Serves /sitemap (and /sitemap.md): every page the site serves, grouped
 * like the docs sidebar, for people. It lists the same docs as the
 * sitemaps with their dates; when the dates file cannot be read it lists the
 * sidebar's docs without dates, for a minute.
 *
 * @param request - The incoming GET or HEAD request.
 * @param url - Its URL.
 * @param deps - The shell, assets and fetcher.
 * @param markdown - Whether to answer with the Markdown version.
 * @returns The page.
 */
async function serveSitemapPage(
  request: Request,
  url: URL,
  deps: DocsDeps,
  markdown: boolean,
): Promise<Response> {
  const [version] = deps.shell.versions;
  if (version === undefined) return notFound(url, request.method, markdown, deps.assets);
  const format = markdown ? "md" : "html";
  const cacheKey = new Request(
    `${url.origin}${SITEMAP_PAGE_PATH}?format=${format}&ref=${encodeURIComponent(version.ref)}&shell=${encodeURIComponent(deps.shell.id)}`,
  );
  if (deps.cache) {
    const hit = await deps.cache.match(cacheKey);
    if (hit) return request.method === "HEAD" ? new Response(null, hit) : hit;
  }
  const { file, docs } = await listDocs(version, deps);
  const titles = new Map(
    deps.shell.nav.flatMap((g) => g.pages.map((p) => [p.key, p.title] as const)),
  );
  const pageTitles = new Map(
    deps.shell.pages.map((path, i) => [path, deps.shell.pageTitles[i] ?? path] as const),
  );
  const links: SitemapPageLink[] = siteSitemapPages(deps.shell)
    .filter((path) => path !== SITEMAP_PAGE_PATH)
    .map((path) => ({ key: null, path, title: pageTitles.get(path) ?? path, updated: null }));
  if (docs.length > 0) {
    for (const d of docs)
      links.push({ key: d.key, path: d.path, title: titles.get(d.key) ?? null, updated: d.date });
  } else {
    links.push({ key: "README", path: docUrl("README", version.base), title: null, updated: null });
    for (const [key, title] of titles)
      links.push({ key, path: docUrl(key, version.base), title, updated: null });
  }
  const page = renderSitemapPage(deps.shell, links);
  const cacheable = docs.length > 0 || file.kind === "missing";
  const cacheControl = cacheable
    ? `public, max-age=${BROWSER_TTL}, s-maxage=${ttlFor(version.ref)}`
    : `public, max-age=${MISSING_TTL}, s-maxage=0`;
  const response = markdown
    ? withHeaders(new Response(page.markdown), {
        "Cache-Control": cacheControl,
        Vary: "Accept",
        "Content-Type": MARKDOWN_CONTENT_TYPE,
        Link: `<${url.origin}${SITEMAP_PAGE_PATH}>; rel="canonical"`,
      })
    : withHeaders(new Response(page.html), {
        "Cache-Control": cacheControl,
        Vary: "Accept",
        "Content-Type": "text/html; charset=utf-8",
        Link: `<${url.origin}${SITEMAP_PAGE_PATH}.md>; rel="alternate"; type="text/markdown"`,
      });
  if (deps.cache && cacheable && request.method === "GET")
    deps.waitUntil(deps.cache.put(cacheKey, response.clone()));
  return request.method === "HEAD" ? new Response(null, response) : response;
}

/**
 * Serves the sitemap build.ts wrote, which lists the pages and the
 * sidebar's docs without dates.
 *
 * @param request - The incoming GET or HEAD request.
 * @param url - Its URL.
 * @param assets - The static assets.
 * @param ttl - How long the edge may keep it, in seconds; browsers keep it
 *   no longer than a minute.
 * @returns The built sitemap, or the assets' answer unchanged when it is
 *   not there.
 */
async function fallbackSitemap(
  request: Request,
  url: URL,
  assets: Assets,
  ttl: number,
): Promise<Response> {
  const built = await assets.fetch(new URL(SITEMAP_PATH, url), { method: request.method });
  if (!built.ok) return built;
  return withHeaders(built, {
    "Content-Type": XML_CONTENT_TYPE,
    "Cache-Control": `public, max-age=${MISSING_TTL}, s-maxage=${ttl}`,
  });
}

/** Everything the docs route needs besides the request. */
interface DocsDeps {
  /** The baked page shell. */
  shell: DocsShell;
  /** The static assets. */
  assets: Assets;
  /** Fetches from the docs repository. */
  fetch: DocsFetch;
  /** The edge cache for rendered pages, or null. */
  cache: PageCache | null;
  /** Keeps the Worker alive until a cache write finishes. */
  waitUntil: (promise: Promise<unknown>) => void;
}

/**
 * Picks the content coding for a response from an Accept-Encoding header.
 *
 * A coding is acceptable when it is listed, or `*` is, with a quality value
 * above 0; a coding listed by name overrides `*`. Among acceptable codings the
 * highest quality wins, and ENCODINGS' order breaks ties.
 *
 * @param acceptEncoding - The client's Accept-Encoding header.
 * @returns The coding to apply, or null for none.
 */
export function pickEncoding(acceptEncoding: string | null | undefined): Encoding | null {
  const listed = new Map<string, number>();
  for (const part of (acceptEncoding ?? "").split(",")) {
    const [rawCoding = "", ...params] = part.split(";");
    const coding = rawCoding.trim().toLowerCase();
    if (!coding) continue;
    const q = qualityOf(params);
    listed.set(coding, Math.max(listed.get(coding) ?? 0, q));
  }
  let best: Encoding | null = null;
  let bestQ = 0;
  for (const coding of ENCODINGS) {
    const q = listed.get(coding) ?? listed.get("*") ?? 0;
    if (q > bestQ) {
      best = coding;
      bestQ = q;
    }
  }
  return best;
}

/**
 * Reads the Accept-Encoding header as the client sent it, before Cloudflare
 * normalizes it: `request.cf.clientAcceptEncoding`, which only requests in
 * the Workers runtime carry.
 *
 * @param request - The incoming request.
 * @returns The client's header, or null when the request has none.
 */
function clientAcceptEncoding(request: Request): string | null {
  const cf: unknown = request.cf;
  if (typeof cf !== "object" || cf === null || !("clientAcceptEncoding" in cf)) return null;
  return typeof cf.clientAcceptEncoding === "string" ? cf.clientAcceptEncoding : null;
}

/**
 * Asks the runtime to compress a response the client can take compressed.
 *
 * Setting Content-Encoding on a response whose body is not encoded makes the
 * Workers runtime encode it on the way out. Only text media types are
 * compressed, never a response that is already encoded, is a byte range, has
 * no body or says `Cache-Control: no-transform`. A compressed response gets
 * `Vary: Accept-Encoding`, loses its Content-Length and has its ETag made
 * weak, since the bytes differ from the asset the tag names; a 304 for such
 * a response gets the same Vary and ETag, and no Content-Encoding.
 *
 * @param request - The incoming request; its `cf.clientAcceptEncoding`
 *   (the header the client sent, before Cloudflare normalizes it) is preferred
 *   over its Accept-Encoding header.
 * @param response - The response to send.
 * @returns The response, compressed when it qualifies.
 */
export function compress(request: Request, response: Response): Response {
  const notModified = response.status === 304;
  if (response.body === null && request.method !== "HEAD" && !notModified) return response;
  if (response.status === 204 || response.status === 206) return response;
  if (response.headers.has("Content-Encoding") || response.headers.has("Content-Range"))
    return response;
  if (/(?:^|,)\s*no-transform\s*(?:,|$)/i.test(response.headers.get("Cache-Control") ?? ""))
    return response;
  const type =
    (response.headers.get("Content-Type") ?? "").split(";", 1)[0]?.trim().toLowerCase() ?? "";
  // A 304 may leave out Content-Type; it stands for the page the client has.
  if (!COMPRESSIBLE_RE.test(type) && !(notModified && type === "")) return response;
  const encoding = pickEncoding(
    clientAcceptEncoding(request) ?? request.headers.get("Accept-Encoding"),
  );
  const out = new Response(response.body, response);
  out.headers.set("Vary", varyAdd(out.headers, "Accept-Encoding"));
  if (encoding === null) return out;
  const etag = out.headers.get("ETag");
  if (etag !== null && !etag.startsWith("W/")) out.headers.set("ETag", `W/${etag}`);
  // A 304 has no body to encode; it only repeats the compressed answer's
  // Vary and validator so caches that refresh their copy from it keep them.
  if (notModified) return out;
  out.headers.set("Content-Encoding", encoding);
  out.headers.delete("Content-Length");
  return out;
}

/**
 * Handles one request, compressing the answer when the client accepts it.
 *
 * @param request - The incoming request.
 * @param env - Bindings: the static assets.
 * @param docs - As for route().
 * @returns The response.
 */
export async function handle(request: Request, env: Env, docs: DocsOptions): Promise<Response> {
  const response = await route(request, env, docs);
  const isRead = request.method === "GET" || request.method === "HEAD";
  return isRead ? compress(request, response) : response;
}

/**
 * Finds the slash-free URL of a website page asked for by another spelling:
 * `/download/` and `/download.html` for `/download`, `/index.html` for `/`.
 * The assets binding would redirect these too, but temporarily (307); a
 * permanent redirect tells search engines which URL to keep.
 *
 * @param pathname - The request path.
 * @param pages - The website's page paths (build.ts `PAGES`).
 * @returns The page's path, or null when the path is not another spelling of one.
 */
export function sitePageLocation(pathname: string, pages: readonly string[]): string | null {
  if (pathname === "/index.html") return "/";
  const stem = pathname.endsWith("/")
    ? pathname.slice(0, -1)
    : pathname.endsWith(".html")
      ? pathname.slice(0, -".html".length)
      : null;
  return stem !== null && stem.length > 1 && pages.includes(stem) ? stem : null;
}

/**
 * Answers one request, before compression.
 *
 * @param request - The incoming request.
 * @param env - Bindings: the static assets.
 * @param docs - The docs shell, the fetcher for the docs repository (default: the global
 *   `fetch`), the cache for rendered pages (default: none) and the request
 *   context's `waitUntil`.
 * @returns The response.
 */
async function route(request: Request, env: Env, docs: DocsOptions): Promise<Response> {
  const url = new URL(request.url);
  const isRead = request.method === "GET" || request.method === "HEAD";
  const deps: DocsDeps = {
    shell: docs.shell,
    assets: env.ASSETS,
    fetch: docs.fetch ?? fetch,
    cache: docs.cache ?? null,
    waitUntil: docs.waitUntil ?? (() => {}),
  };
  if (isRead && (url.pathname === "/docs" || url.pathname.startsWith("/docs/")))
    return serveDocs(request, url, deps);
  const canonical = isRead ? sitePageLocation(url.pathname, deps.shell.pages) : null;
  if (canonical !== null) return redirect(url, canonical);
  if (isRead && (url.pathname === SITEMAP_PATH || SECTION_PATH_RE.test(url.pathname)))
    return serveSitemap(request, url, deps);
  if (isRead && (url.pathname === SITEMAP_PAGE_PATH || url.pathname === `${SITEMAP_PAGE_PATH}/`)) {
    if (url.pathname.endsWith("/")) return redirect(url, SITEMAP_PAGE_PATH);
    return serveSitemapPage(request, url, deps, prefersMarkdown(request.headers.get("Accept")));
  }
  if (isRead && url.pathname === `${SITEMAP_PAGE_PATH}.md`)
    return serveSitemapPage(request, url, deps, true);
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
        Vary: varyAdd(found.headers, "Accept"),
        Link: `<${url.origin}${htmlPath(md) ?? url.pathname}>; rel="canonical"`,
      });
    }
    return notFound(url, request.method, true, env.ASSETS);
  }

  const response = await env.ASSETS.fetch(request);
  if (md !== null) {
    const contentType = response.headers.get("Content-Type") ?? "";
    const headers: Record<string, string> = { Vary: varyAdd(response.headers, "Accept") };
    if (response.ok && contentType.startsWith("text/html")) {
      headers.Link = `<${url.origin}${md}>; rel="alternate"; type="text/markdown"`;
    }
    return withHeaders(response, headers);
  }
  if (isRead && url.pathname === NOT_FOUND_MARKDOWN) {
    return withHeaders(response, {
      "Content-Type": MARKDOWN_CONTENT_TYPE,
      "X-Robots-Tag": "noindex",
    });
  }
  const page = isRead ? htmlPath(url.pathname) : null;
  if (page !== null && (response.ok || response.status === 304)) {
    // A Markdown copy requested by name: point search engines at the page.
    return withHeaders(response, {
      "Content-Type": MARKDOWN_CONTENT_TYPE,
      Link: `<${url.origin}${page}>; rel="canonical"`,
    });
  }
  return response;
}
