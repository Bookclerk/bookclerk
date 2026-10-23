// The bookclerk-site Worker: serves dist/ through the ASSETS binding and
// answers requests that prefer Markdown with each page's Markdown version.
//
// wrangler.toml runs this script first only for page paths (`run_worker_first`);
// stylesheets, images and the like are served straight from the assets.
//
// Content negotiation: when the Accept header ranks `text/markdown` (or
// `text/x-markdown`) at least as high as HTML, `/`, `/download` and
// `/docs/<path>` return their `.md` file instead, with
// `Content-Type: text/markdown` and a `Link: rel="canonical"` to the HTML page.
// HTML responses carry `Link: rel="alternate"` to their Markdown version.
// Every page response says `Vary: Accept` so caches keep the two apart.
//
// The script reads only the request's method, path and Accept header, and
// fetches only from its own assets.

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

/** A /docs/ path that names a doc: segments of letters, digits, `.`, `_` and `-`. */
const DOC_PATH_RE = /^\/docs\/(?:[A-Za-z0-9_-][A-Za-z0-9._-]*\/)*(?:[A-Za-z0-9_-][A-Za-z0-9._-]*)?$/;

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
 * Maps a page path to the path of its Markdown version.
 *
 * @param {string} pathname The request path.
 * @returns {string | null} The `.md` asset path, or null when the path is not
 *   a page (an asset, or already Markdown). A `/docs/<name>/` path maps to
 *   `<name>/README.md`; handle() falls back to `<name>.md`.
 */
export function markdownPath(pathname) {
  if (PAGE_MARKDOWN.has(pathname)) return PAGE_MARKDOWN.get(pathname);
  if (!DOC_PATH_RE.test(pathname)) return null;
  // A file with an extension (`.md`, `.svg`, …) is not a page; only `.html`
  // names one.
  const last = pathname.slice(pathname.lastIndexOf("/") + 1);
  if (last.includes(".") && !last.endsWith(".html")) return null;
  if (pathname.endsWith("/")) return `${pathname}README.md`;
  if (pathname.endsWith("/index.html")) return `${pathname.slice(0, -"index.html".length)}README.md`;
  if (pathname.endsWith(".html")) return `${pathname.slice(0, -".html".length)}.md`;
  return `${pathname}.md`;
}

/**
 * Maps a Markdown asset path back to the HTML page it is a version of.
 *
 * @param {string} pathname A `.md` path.
 * @returns {string | null} The page path, or null for other paths.
 */
export function htmlPath(pathname) {
  for (const [page, md] of PAGE_MARKDOWN) {
    if (md === pathname && !page.endsWith(".html")) return page;
  }
  if (!pathname.startsWith("/docs/") || !pathname.endsWith(".md") || !DOC_PATH_RE.test(pathname)) return null;
  if (pathname.endsWith("/README.md")) return pathname.slice(0, -"README.md".length);
  return pathname.slice(0, -".md".length);
}

/**
 * Lists the Markdown files that can hold a page, most likely first.
 *
 * @param {string} md Output of markdownPath().
 * @returns {string[]} `md`, then for `/docs/<name>.md` the directory's
 *   `README.md` (a directory linked without its slash), or for
 *   `/docs/<name>/README.md` the file `<name>.md` (a doc linked with a slash).
 */
export function markdownCandidates(md) {
  if (!md.startsWith("/docs/") || md === "/docs/README.md") return [md];
  if (md.endsWith("/README.md")) return [md, `${md.slice(0, -"/README.md".length)}.md`];
  return [md, `${md.slice(0, -".md".length)}/README.md`];
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
 * Handles one request.
 *
 * @param {Request} request The incoming request.
 * @param {{ASSETS: {fetch: (input: Request | URL | string, init?: RequestInit) => Promise<Response>}}} env
 *   Bindings: the static assets.
 * @returns {Promise<Response>} The response.
 */
export async function handle(request, env) {
  const url = new URL(request.url);
  const isRead = request.method === "GET" || request.method === "HEAD";
  const md = isRead ? markdownPath(url.pathname) : null;

  if (md !== null && prefersMarkdown(request.headers.get("Accept"))) {
    // Keep conditional headers so agents can revalidate with an ETag.
    const headers = new Headers({ Accept: MARKDOWN_CONTENT_TYPE });
    for (const name of CONDITIONAL_HEADERS) {
      const value = request.headers.get(name);
      if (value !== null) headers.set(name, value);
    }
    const assetInit = { method: request.method, headers };
    for (const candidate of markdownCandidates(md)) {
      const found = await env.ASSETS.fetch(new URL(candidate, url), assetInit);
      if (found.ok || found.status === 304) {
        const page = htmlPath(candidate) ?? url.pathname;
        return withHeaders(found, {
          "Content-Type": MARKDOWN_CONTENT_TYPE,
          Vary: varyAccept(found.headers),
          Link: `<${url.origin}${page}>; rel="canonical"`,
        });
      }
    }
    const missing = await env.ASSETS.fetch(new URL(NOT_FOUND_MARKDOWN, url), { method: request.method });
    const notFound = missing.ok ? missing : new Response("# Not found\n");
    return withHeaders(notFound, { "Content-Type": MARKDOWN_CONTENT_TYPE, Vary: "Accept", "X-Robots-Tag": "noindex" }, 404);
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

export default { fetch: handle };
