// Builds the static bookclerk.app website into dist/.
//
// The site is plain HTML and CSS with no client-side scripts. Its npm
// packages are markdown-it (render.mjs, bundled into the Worker) and
// wrangler. Beside the pages it writes what search engines, link previews and
// agents read: canonical and Open Graph tags, JSON-LD, sitemap.xml,
// robots.txt and llms.txt / llms-full.txt.
//
// Docs pages are not written here: the Worker renders them from the docs
// repository at request time (worker.js). The build bakes what the Worker
// needs for that into generated/docs-shell.json: src/doc.html rendered for
// this site with the per-page parts left as holes, the sidebar, and the list
// of docs versions.
//
// Release state comes from the environment, set by
// .github/workflows/site-deploy.yml:
//
//   SITE_RELEASE_TAG   newest published release tag (e.g. v0.1.0); empty or
//                      unset means "no release yet", which renders every
//                      pre-release notice
//   SITE_RELEASE_DATE  that release's publish date, YYYY-MM-DD
//   SITE_DOCS_VERSIONS every published release tag, whitespace-separated,
//                      newest first; each gets docs at /docs/v/<tag>/ (the
//                      newest release is also /docs/)
//   SITE_COMMIT        commit the site is built from (7-40 hex digits)
//   SITE_BUILT_ON      build date, YYYY-MM-DD (defaults to today, UTC)
//   SITE_ORIGIN        origin the site is served from, for canonical and
//                      preview URLs (default https://bookclerk.app)
//
// Every value is validated before it reaches a template, and every template
// value is HTML-escaped.

import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { docMarkdownUrl, docUrl, GENERATED_TITLES, NAV, renderDocs } from "./docs.mjs";
import { escapeHtml, jsonLd, SHELL_MARKERS, SHELL_VALUES } from "./render.mjs";

export { escapeHtml, jsonLd };

/** Directory holding this script. */
const HERE = dirname(fileURLToPath(import.meta.url));

/** Repository root (tools/site/../..). */
const REPO_ROOT = join(HERE, "..", "..");

/** GitHub repository the site links to. */
export const REPOSITORY_URL = "https://github.com/Bookclerk/bookclerk";

/** Origin used when SITE_ORIGIN is unset. */
const DEFAULT_ORIGIN = "https://bookclerk.app";

/** One-sentence summary of Bookclerk, shared by the home page, JSON-LD and llms.txt. */
const SUMMARY =
  "Bookclerk gathers the audiobooks you have bought from Audible, Libro.fm, Chirp and GraphicAudio into one library on your own computer, and keeps copies you control.";

/**
 * Pages rendered from src/<file> to dist/<file>.
 *
 * `path` is the page's URL path, `title` and `description` fill its
 * `<title>`, description and preview tags, and `indexable` is false for pages
 * search engines should skip (no canonical link, `noindex`, not in the
 * sitemap). `markdown` is the page's Markdown version, rendered from the
 * template of the same name to the same name in dist/; the Worker serves it
 * to requests that ask for `text/markdown` (worker.js). `alternate` is false
 * when the page should not advertise it with `rel="alternate"`.
 */
export const PAGES = [
  {
    file: "index.html",
    path: "/",
    title: "Bookclerk: every purchase, properly placed",
    description: SUMMARY,
    indexable: true,
    markdown: "/index.md",
  },
  {
    file: "download.html",
    path: "/download",
    title: "Download Bookclerk",
    description: "Get Bookclerk, the self-hosted audiobook library manager for your Audible, Libro.fm, Chirp and GraphicAudio purchases.",
    indexable: true,
    markdown: "/download.md",
  },
  {
    file: "404.html",
    path: "/404",
    title: "Page not found · Bookclerk",
    description: "This page is not on the shelf.",
    indexable: false,
    markdown: "/404.md",
    alternate: false,
  },
];

/** Files copied verbatim from src/ to dist/. */
const STATIC_FILES = ["site.css", "_headers", "social-card.png"];

/** Brand assets copied from assets/brand/ (source path, published name). */
const BRAND_FILES = [
  ["svg/bookclerk-mark.svg", "bookclerk-mark.svg"],
  ["web/favicon.ico", "favicon.ico"],
  ["web/favicon-16x16.png", "favicon-16x16.png"],
  ["web/favicon-32x32.png", "favicon-32x32.png"],
  ["web/apple-touch-icon.png", "apple-touch-icon.png"],
  ["web/android-chrome-192x192.png", "android-chrome-192x192.png"],
  ["web/android-chrome-512x512.png", "android-chrome-512x512.png"],
  ["web/site.webmanifest", "site.webmanifest"],
];

/** A release tag: `v` plus a semver version, optional pre-release suffix. */
const TAG_RE = /^v\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

/** A calendar date as GitHub and `date -u +%F` print it. */
const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;

/** An abbreviated or full git commit id. */
const COMMIT_RE = /^[0-9a-f]{7,40}$/;

/** An https origin with a lowercase hostname and no port or path. */
const ORIGIN_RE = /^https:\/\/([a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/;

/** Most release tags the docs version picker lists. */
const MAX_DOCS_VERSIONS = 100;

/** Where the build writes the docs shell the Worker imports (see top). */
export const DOCS_SHELL_FILE = join(HERE, "generated", "docs-shell.json");

/** How deep partials may include other partials. */
const MAX_PARTIAL_DEPTH = 3;

/** Month names for human-readable dates. */
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/**
 * Formats a YYYY-MM-DD date as "23 Oct 2026".
 *
 * @param {string} iso A date already checked against DATE_RE.
 * @returns {string} The date for display.
 */
function displayDate(iso) {
  const [y, m, d] = iso.split("-").map(Number);
  return `${d} ${MONTHS[m - 1]} ${y}`;
}

/**
 * Validates the release state from the environment.
 *
 * @param {Record<string, string | undefined>} env Usually `process.env`.
 * @returns {{release: {tag: string, date: string} | null, commit: string,
 *   builtOn: string, origin: string, docsTags: string[]}}
 *   `release` is null when no release exists yet. `docsTags` are the release
 *   tags with docs, newest first, starting with the release's tag.
 * @throws {Error} When a value is present but malformed, a release tag
 *   comes without its date, or there are more than MAX_DOCS_VERSIONS tags.
 */
export function readState(env) {
  const tag = (env.SITE_RELEASE_TAG ?? "").trim();
  const date = (env.SITE_RELEASE_DATE ?? "").trim();
  const commit = (env.SITE_COMMIT ?? "").trim().toLowerCase();
  const builtOn = (env.SITE_BUILT_ON ?? "").trim() || new Date().toISOString().slice(0, 10);
  const origin = (env.SITE_ORIGIN ?? "").trim() || DEFAULT_ORIGIN;
  const listed = (env.SITE_DOCS_VERSIONS ?? "").split(/\s+/).filter((t) => t !== "");

  if (!COMMIT_RE.test(commit)) {
    throw new Error(`SITE_COMMIT must be 7-40 lowercase hex digits, got ${JSON.stringify(commit)}`);
  }
  if (!DATE_RE.test(builtOn)) {
    throw new Error(`SITE_BUILT_ON must be YYYY-MM-DD, got ${JSON.stringify(builtOn)}`);
  }
  if (!ORIGIN_RE.test(origin)) {
    throw new Error(`SITE_ORIGIN must be https://<lowercase hostname>, got ${JSON.stringify(origin)}`);
  }
  for (const t of listed) {
    if (!TAG_RE.test(t)) throw new Error(`SITE_DOCS_VERSIONS must hold tags like v1.2.3, got ${JSON.stringify(t)}`);
  }
  if (tag === "") {
    if (listed.length > 0) throw new Error("SITE_DOCS_VERSIONS is set but SITE_RELEASE_TAG is not");
    return { release: null, commit, builtOn, origin, docsTags: [] };
  }
  if (!TAG_RE.test(tag)) {
    throw new Error(`SITE_RELEASE_TAG must look like v1.2.3, got ${JSON.stringify(tag)}`);
  }
  if (!DATE_RE.test(date)) {
    throw new Error(`SITE_RELEASE_DATE must be YYYY-MM-DD when a release tag is set, got ${JSON.stringify(date)}`);
  }
  const docsTags = [...new Set([tag, ...listed])];
  if (docsTags.length > MAX_DOCS_VERSIONS) {
    throw new Error(`SITE_DOCS_VERSIONS lists ${docsTags.length} tags; the picker holds at most ${MAX_DOCS_VERSIONS}`);
  }
  return { release: { tag, date }, commit, builtOn, origin, docsTags };
}

/**
 * Lists the docs versions the site serves.
 *
 * @param {ReturnType<typeof readState>} state Validated release state.
 * @returns {{ref: string, label: string, base: string}[]} The default
 *   version (served at `/docs/`: the newest release, else main) first, then
 *   main and older releases under `/docs/v/<ref>/`.
 */
export function docsVersions(state) {
  const dev = { ref: "main", label: "Development (main)" };
  if (state.release === null) return [{ ...dev, base: "/docs/" }];
  const [latest, ...older] = state.docsTags;
  return [
    { ref: latest, label: `${latest} (latest)`, base: "/docs/" },
    { ...dev, base: "/docs/v/main/" },
    ...older.map((t) => ({ ref: t, label: t, base: `/docs/v/${t}/` })),
  ];
}

/**
 * Builds the values templates can read.
 *
 * @param {ReturnType<typeof readState>} state Validated release state.
 * @returns {{values: Record<string, string>, flags: Record<string, boolean>}}
 *   `values` fill `{{name}}` holes; `flags` drive `{{#name}}` sections.
 */
export function templateContext(state) {
  const short = state.commit.slice(0, 7);
  // Source links follow the commit the docs came from, so line anchors and
  // moved files stay right until the next deploy.
  const ref = state.release ? state.release.tag : state.commit;
  return {
    values: {
      origin: state.origin,
      socialImage: `${state.origin}/social-card.png`,
      repo: REPOSITORY_URL,
      commit: short,
      commitUrl: `${REPOSITORY_URL}/commit/${state.commit}`,
      builtOn: displayDate(state.builtOn),
      docsUrl: "/docs/",
      gettingStartedUrl: "/docs/getting-started",
      pluginsUrl: "/docs/plugins",
      architectureUrl: "/docs/architecture",
      blobBase: `${REPOSITORY_URL}/blob/${ref}/`,
      docsVersion: state.release ? state.release.tag : "Development (main)",
      releasesUrl: `${REPOSITORY_URL}/releases`,
      releaseTag: state.release?.tag ?? "",
      releaseDate: state.release ? displayDate(state.release.date) : "",
      releaseUrl: state.release ? `${REPOSITORY_URL}/releases/tag/${state.release.tag}` : "",
    },
    flags: {
      prerelease: state.release === null,
      released: state.release !== null,
    },
  };
}

/**
 * Renders a template.
 *
 * Supported syntax, applied in this order:
 * - `{{> name}}` includes `partials[name]`; partials may include partials,
 *   up to MAX_PARTIAL_DEPTH levels.
 * - `{{#flag}}…{{/flag}}` keeps its body when the flag is true;
 *   `{{^flag}}…{{/flag}}` when it is false. Sections do not nest.
 * - `{{name}}` is replaced by the escaped value.
 *
 * @param {string} template Template text.
 * @param {ReturnType<typeof templateContext>} context Values and flags.
 * @param {Record<string, string>} partials Partial templates by name.
 * @returns {string} The rendered HTML.
 * @throws {Error} On an unknown partial, flag or value, so a typo fails the
 *   build instead of shipping an empty hole.
 */
export function render(template, context, partials) {
  let out = template;
  for (let depth = 0; depth < MAX_PARTIAL_DEPTH && out.includes("{{>"); depth += 1) {
    out = out.replace(/\{\{>\s*([a-z-]+)\s*\}\}/g, (_, name) => {
      if (!(name in partials)) throw new Error(`unknown partial ${name}`);
      return partials[name];
    });
  }
  out = out.replace(/\{\{([#^])([a-z]+)\}\}([\s\S]*?)\{\{\/\2\}\}/g, (_, kind, name, body) => {
    if (!(name in context.flags)) throw new Error(`unknown flag ${name}`);
    return context.flags[name] === (kind === "#") ? body : "";
  });
  out = out.replace(/\{\{([A-Za-z]+)\}\}/g, (_, name) => {
    if (!(name in context.values)) throw new Error(`unknown value ${name}`);
    return escapeHtml(context.values[name]);
  });
  if (out.includes("{{")) {
    throw new Error(`unrendered template syntax near ${JSON.stringify(out.slice(out.indexOf("{{"), out.indexOf("{{") + 40))}`);
  }
  return out;
}

/**
 * Prepares the horizontal logo for inlining in HTML.
 *
 * The wordmark switches to `currentColor` through site.css so it stays
 * legible in dark mode (assets/brand/README.md allows recoloring only the
 * wordmark). Editor-only attributes are dropped and the SVG's ids are
 * prefixed so they cannot collide with the page.
 *
 * @param {string} svg Contents of assets/brand/svg/bookclerk-logo.svg.
 * @returns {string} An `<svg>` element ready to inline.
 */
export function inlineLogo(svg) {
  return svg
    .replace(/<\?xml[^>]*\?>\s*/, "")
    .replace(/\s+xmlns:bx="[^"]*"/g, "")
    .replace(/\s+bx:shape="[^"]*"/g, "")
    .replace('aria-labelledby="title desc"', 'aria-labelledby="logo-title"')
    .replace('<title id="title">Bookclerk mark</title>', '<title id="logo-title">Bookclerk</title>')
    .replace(/\s*<desc id="desc">[^<]*<\/desc>/, "")
    .replace("<svg ", '<svg class="logo" ')
    .trim();
}

/** Marker in the head partial replaced by the page's JSON-LD after templating. */
const JSONLD_MARKER = "<!--jsonld-->";

/**
 * Adds one page's metadata to the site-wide template context.
 *
 * @param {ReturnType<typeof templateContext>} context Site-wide values.
 * @param {{path: string, title: string, description: string, indexable: boolean,
 *   type?: string, markdown?: string, alternate?: boolean}} page The page:
 *   URL path, title, description, whether search engines may index it, Open
 *   Graph type (default `website`), the URL of its Markdown copy, if any, and
 *   whether to link that copy with `rel="alternate"` (default true).
 * @param {Record<string, string>} [extra] More values the page's template reads.
 * @returns {ReturnType<typeof templateContext>} The context for that page.
 */
function pageContext(context, page, extra = {}) {
  return {
    values: {
      ...context.values,
      ...extra,
      pageTitle: page.title,
      pageDescription: page.description,
      pageUrl: context.values.origin + page.path,
      pageType: page.type ?? "website",
      markdownUrl: page.markdown ?? "",
    },
    flags: { ...context.flags, indexable: page.indexable, markdown: page.markdown !== undefined && page.alternate !== false },
  };
}

/**
 * The schema.org description of the site and of Bookclerk itself, for the
 * home page.
 *
 * @param {ReturnType<typeof templateContext>} context Site-wide values.
 * @returns {object} A JSON-LD `@graph` with the WebSite and the
 *   SoftwareApplication.
 */
function homeJsonLd(context) {
  const { origin, releaseTag, socialImage } = context.values;
  return {
    "@context": "https://schema.org",
    "@graph": [
      { "@type": "WebSite", "@id": `${origin}/#website`, name: "Bookclerk", url: `${origin}/`, inLanguage: "en" },
      {
        "@type": "SoftwareApplication",
        name: "Bookclerk",
        description: SUMMARY,
        url: `${origin}/`,
        image: socialImage,
        applicationCategory: "MultimediaApplication",
        operatingSystem: "Windows, macOS, Linux",
        license: "https://www.gnu.org/licenses/gpl-3.0.html",
        isAccessibleForFree: true,
        offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
        downloadUrl: `${origin}/download`,
        ...(releaseTag ? { softwareVersion: releaseTag.slice(1) } : {}),
        sameAs: [REPOSITORY_URL],
      },
    ],
  };
}

/**
 * Renders the docs page shell the Worker fills for every docs page.
 *
 * Per-page values become `%%name%%` holes, which HTML escaping leaves as
 * they are, and per-page HTML goes where src/doc.html and the head partial
 * have `<!--name-->` markers (render.mjs fills both).
 *
 * @param {ReturnType<typeof templateContext>} context Site-wide values.
 * @param {Record<string, string>} partials Partial templates by name.
 * @param {ReturnType<typeof renderDocs>} rendered The local docs, for the sidebar.
 * @param {ReturnType<typeof docsVersions>} versions The docs versions served.
 * @returns {import("./render.mjs").DocsShell} The shell.
 * @throws {Error} When the rendered template has a hole or marker render.mjs
 *   does not fill, or lacks one it does.
 */
export function buildDocsShell(context, partials, rendered, versions) {
  const template = readFileSync(join(HERE, "src", "doc.html"), "utf8");
  const hole = (name) => `%%${name}%%`;
  const page = pageContext(context, {
    path: hole("pagePath"),
    title: hole("pageTitle"),
    description: hole("pageDescription"),
    indexable: true,
    type: "article",
    markdown: hole("markdownUrl"),
  });
  const html = render(template, page, partials);
  const holes = new Set([...html.matchAll(/%%([A-Za-z]+)%%/g)].map((m) => m[1]));
  const unknown = [...holes].filter((h) => !SHELL_VALUES.includes(h));
  if (unknown.length > 0 || html.replace(/%%[A-Za-z]+%%/g, "").includes("%%")) {
    throw new Error(`docs shell has holes render.mjs does not fill: ${unknown.join(", ") || "stray %%"}`);
  }
  for (const marker of SHELL_MARKERS) {
    if (html.split(`<!--${marker}-->`).length !== 2) throw new Error(`docs shell needs exactly one <!--${marker}-->`);
  }
  return { html, origin: context.values.origin, repository: REPOSITORY_URL, versions, nav: rendered.nav };
}

/**
 * Escapes text for the label of a Markdown link.
 *
 * @param {string} text Plain text.
 * @returns {string} The text with backslashes and square brackets escaped.
 */
function markdownLinkText(text) {
  return text.replace(/[\\[\]]/g, "\\$&");
}

/**
 * Writes llms.txt (an index of the docs for language models, per
 * https://llmstxt.org) and llms-full.txt (every doc in one file).
 *
 * @param {ReturnType<typeof templateContext>} context Site-wide values.
 * @param {ReturnType<typeof renderDocs>} rendered The docs and sidebar.
 * @param {string} outDir Output directory.
 */
function writeLlmsTxt(context, rendered, outDir) {
  const { origin, docsVersion, repo } = context.values;
  const byKey = new Map(rendered.docs.map((d) => [d.key, d]));
  const status = context.flags.prerelease
    ? "Bookclerk has not had a release yet: these docs describe the unstable development version on main."
    : `These docs describe Bookclerk ${docsVersion}.`;
  const lines = [
    "# Bookclerk",
    "",
    `> ${SUMMARY} It is free, self-hosted software (GNU GPL v3 or later) with a command-line tool, a daemon with a web interface, and a plugin system for stores and destinations.`,
    "",
    `${status} Each doc below is Markdown; the same page as HTML is at the URL without \`.md\`. Source code: ${repo}. Every doc in one file: ${origin}/llms-full.txt. Every page also answers a request with \`Accept: text/markdown\` in Markdown; the home and download pages are at ${origin}/index.md and ${origin}/download.md.`,
    "",
  ];
  const groups = rendered.nav;
  const designAt = groups.findIndex((g) => g.title === NAV[NAV.length - 1].title);
  for (const [i, group] of groups.entries()) {
    // llms.txt reserves "Optional" for links a reader can skip.
    lines.push(`## ${i === designAt ? "Optional" : group.title}`, "");
    if (i === 0) lines.push(`- [Overview](${origin}${docMarkdownUrl("README")}): How the documentation is organized`);
    for (const { key, title } of group.pages) {
      const description = byKey.get(key)?.description ?? "";
      lines.push(`- [${markdownLinkText(title)}](${origin}${docMarkdownUrl(key)})${description ? `: ${description}` : ""}`);
    }
    lines.push("");
  }
  writeFileSync(join(outDir, "llms.txt"), lines.join("\n"));

  // The generated API reference is too large for one file; llms.txt links it.
  const full = [`# Bookclerk documentation (${docsVersion})`, "", `> ${SUMMARY}`, ""];
  for (const key of ["README", ...groups.flatMap((g) => g.pages.map((p) => p.key)).filter((k) => byKey.has(k))]) {
    const doc = byKey.get(key);
    full.push("---", "", `Source: ${origin}${docMarkdownUrl(key)}`, "", doc.markdown.trim(), "");
  }
  writeFileSync(join(outDir, "llms-full.txt"), full.join("\n"));
}

/**
 * Writes sitemap.xml and robots.txt.
 *
 * @param {ReturnType<typeof templateContext>} context Site-wide values.
 * @param {string[]} paths URL paths of every indexable page.
 * @param {string} outDir Output directory.
 */
function writeCrawlerFiles(context, paths, outDir) {
  const { origin } = context.values;
  const urls = paths.map((p) => `  <url><loc>${escapeHtml(origin + p)}</loc></url>`).join("\n");
  writeFileSync(
    join(outDir, "sitemap.xml"),
    `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls}\n</urlset>\n`,
  );
  writeFileSync(join(outDir, "robots.txt"), `User-agent: *\nAllow: /\n\nSitemap: ${origin}/sitemap.xml\n`);
}

/**
 * Builds the whole site.
 *
 * @param {Record<string, string | undefined>} env Release state (see top).
 * @param {string} outDir Output directory; replaced entirely.
 * @param {string} [shellFile] Where to write the docs shell (JSON).
 * @returns {string[]} Published top-level file names, sorted.
 */
export function build(env, outDir, shellFile = DOCS_SHELL_FILE) {
  const state = readState(env);
  const context = templateContext(state);
  const src = join(HERE, "src");
  const partials = {};
  for (const file of readdirSync(join(src, "partials"))) {
    if (file.endsWith(".html")) partials[file.slice(0, -5)] = readFileSync(join(src, "partials", file), "utf8");
  }
  partials.logo = inlineLogo(readFileSync(join(REPO_ROOT, "assets/brand/svg/bookclerk-logo.svg"), "utf8"));

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });
  const indexable = [];
  for (const page of PAGES) {
    const html = render(readFileSync(join(src, page.file), "utf8"), pageContext(context, page), partials);
    writeFileSync(join(outDir, page.file), html.replace(JSONLD_MARKER, () => jsonLd(page.path === "/" ? homeJsonLd(context) : null)));
    if (page.markdown) {
      // Template values are validated tags, dates, commit ids and URLs, so
      // HTML escaping leaves them unchanged in Markdown too.
      const md = render(readFileSync(join(src, page.markdown.slice(1)), "utf8"), pageContext(context, page), partials);
      writeFileSync(join(outDir, page.markdown.slice(1)), md);
    }
    if (page.indexable) indexable.push(page.path);
  }
  const rendered = renderDocs(join(REPO_ROOT, "docs"), context.values.blobBase, GENERATED_TITLES);
  indexable.push(docUrl("README"), ...rendered.nav.flatMap((g) => g.pages.map((p) => docUrl(p.key))));
  mkdirSync(dirname(shellFile), { recursive: true });
  writeFileSync(shellFile, JSON.stringify(buildDocsShell(context, partials, rendered, docsVersions(state))));
  writeCrawlerFiles(context, indexable, outDir);
  writeLlmsTxt(context, rendered, outDir);
  for (const file of STATIC_FILES) cpSync(join(src, file), join(outDir, file));
  for (const [from, to] of BRAND_FILES) cpSync(join(REPO_ROOT, "assets/brand", from), join(outDir, to));
  return readdirSync(outDir).sort();
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const files = build(process.env, join(HERE, "dist"));
  console.log(`built ${files.length} files into dist/`);
}
