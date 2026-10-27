// Builds the static bookclerk.app website into dist/.
//
// The site is plain HTML and CSS with no client-side scripts of its own (the
// edge adds Cloudflare's Web Analytics beacon; see SECURITY_HEADERS in
// worker.ts). Its npm packages are markdown-it (render.ts, bundled into the
// Worker) and wrangler. Beside the pages it writes what search engines, link previews and
// agents read: canonical and Open Graph tags, JSON-LD, sitemap.xml,
// robots.txt and llms.txt / llms-full.txt.
//
// Docs pages are not written here: the Worker renders them from the docs
// repository at request time (worker.ts). The build bakes what the Worker
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
//   SITE_DOCS_VERSIONS the published release tags that bookclerk-docs has a
//                      tag for, whitespace-separated, newest first; each gets
//                      docs at /docs/v/<tag>/ and the first is also /docs/.
//                      A release whose docs are not published yet is left
//                      out, and with none listed /docs/ stays on main
//   SITE_COMMIT        commit the site is built from (7-40 hex digits)
//   SITE_BUILT_ON      build date, YYYY-MM-DD (defaults to today, UTC)
//   SITE_ORIGIN        origin the site is served from, for canonical and
//                      preview URLs (default https://bookclerk.app)
//   SITE_DEMO_URL      origin of the hosted demo (tools/demo), e.g.
//                      https://demo.bookclerk.app; empty or unset means the
//                      deployment has no demo, and every demo link is left out
//
// Every value is validated before it reaches a template, and every template
// value is HTML-escaped.

import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { docMarkdownUrl, docUrl, GENERATED_TITLES, NAV, renderDocs } from "./docs.ts";
import { escapeHtml, jsonLd, renderDoc, SHELL_MARKERS, SHELL_VALUES, SITEMAP_MARKER } from "./render.ts";
import { sitemapXml } from "./sitemap.ts";
import type { DocsShell, DocsVersion } from "./render.ts";
import type { RenderedDocs } from "./docs.ts";

/** Environment variables the build reads (see top); usually `process.env`. */
export type BuildEnv = Readonly<Record<string, string | undefined>>;

/** The release state, validated (see readState). */
export interface ReleaseState {
  /** The newest published release, or null before the first one. */
  release: { tag: string; date: string } | null;
  /** The commit the site is built from, lowercase hex. */
  commit: string;
  /** The build date, YYYY-MM-DD. */
  builtOn: string;
  /** The origin the site is served from. */
  origin: string;
  /** The hosted demo's origin, or null when the deployment has none. */
  demoUrl: string | null;
  /** Release tags with published docs, newest first. */
  docsTags: string[];
}

/**
 * What templates can read: `{{name}}` values and `{{#name}}` flags.
 *
 * @typeParam V - The values; SiteValues for the site-wide context.
 * @typeParam F - The flags; SiteFlags for the site-wide context.
 */
export interface TemplateContext<V extends Readonly<Record<string, string>> = Readonly<Record<string, string>>, F extends Readonly<Record<string, boolean>> = Readonly<Record<string, boolean>>> {
  /** Values that fill `{{name}}` holes, unescaped. */
  values: V;
  /** Flags that drive `{{#name}}` and `{{^name}}` sections. */
  flags: F;
}

/**
 * The values every page's template can read (see templateContext). A type
 * alias rather than an interface, so it stays assignable to the plain record
 * render() takes.
 */
export type SiteValues = {
  /** The site's origin. */
  origin: string;
  /** Absolute URL of the link-preview image. */
  socialImage: string;
  /** GitHub URL of the repository. */
  repo: string;
  /** The built commit, abbreviated to seven digits. */
  commit: string;
  /** GitHub URL of the built commit. */
  commitUrl: string;
  /** The build date for display. */
  builtOn: string;
  /** The public contact address. */
  contactEmail: string;
  /** URL path of the docs index. */
  docsUrl: string;
  /** URL path of the getting-started guide. */
  gettingStartedUrl: string;
  /** URL path of the plugins guide. */
  pluginsUrl: string;
  /** URL path of the architecture guide. */
  architectureUrl: string;
  /** GitHub URL prefix for repository files at the docs' ref, ending in `/blob/<ref>/`. */
  blobBase: string;
  /** The default docs version's name. */
  docsVersion: string;
  /** GitHub URL of the releases page. */
  releasesUrl: string;
  /** The newest release tag, or empty before the first release. */
  releaseTag: string;
  /** That release's date for display, or empty. */
  releaseDate: string;
  /** GitHub URL of that release, or empty. */
  releaseUrl: string;
  /** The hosted demo's origin, or empty when there is none. */
  demoUrl: string;
};

/** The flags every page's template can read (see templateContext). */
export type SiteFlags = {
  /** No release yet. */
  prerelease: boolean;
  /** At least one release. */
  released: boolean;
  /** The deployment has a hosted demo. */
  demo: boolean;
  /** A demo and no release: the demo is the main way to try Bookclerk. */
  demolead: boolean;
  /** A demo and a release: the demo follows the download. */
  demofollow: boolean;
  /** No demo and no release. */
  previewlead: boolean;
};

/** The site-wide template context. */
export type SiteContext = TemplateContext<SiteValues, SiteFlags>;

/** One page rendered from src/ (see PAGES). */
export interface Page {
  /** The template under src/ and the file written to dist/. */
  file: string;
  /** The page's URL path. */
  path: string;
  /** Its `<title>`. */
  title: string;
  /** Its description and preview text. */
  description: string;
  /** False for pages search engines should skip. */
  indexable: boolean;
  /** Its Markdown version's URL path, if it has one. */
  markdown?: string;
  /** False when the Markdown version is not advertised with `rel="alternate"`. */
  alternate?: boolean;
  /**
   * A repository Markdown file (path from the repository root) rendered
   * into the template's `<!--source-->` marker; its Markdown version is that
   * file with links rewritten for the site, rather than a template.
   */
  source?: string;
}

/** What the rendered home page shows (see homeFacts). */
export interface HomeFacts {
  /** Each feature card's heading and text, in page order. */
  features: { name: string; text: string }[];
  /** Each FAQ entry's question and answer, in page order. */
  faq: { question: string; answer: string }[];
}

export { escapeHtml, jsonLd };

/** Directory holding this script. */
const HERE = dirname(fileURLToPath(import.meta.url));

/** Repository root (tools/site/../..). */
const REPO_ROOT = join(HERE, "..", "..");

/** GitHub repository the site links to. */
export const REPOSITORY_URL = "https://github.com/Bookclerk/bookclerk";

/** Origin used when SITE_ORIGIN is unset. */
const DEFAULT_ORIGIN = "https://bookclerk.app";

/** Public contact address, as the footer shows it. */
const CONTACT_EMAIL = "contact@bookclerk.app";

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
 * to requests that ask for `text/markdown` (worker.ts). `alternate` is false
 * when the page should not advertise it with `rel="alternate"`.
 */
export const PAGES: readonly Page[] = [
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
    file: "privacy.html",
    path: "/privacy",
    title: "Privacy · Bookclerk",
    description: "What the Bookclerk website and services collect, which cookies they set and why, who else handles it, and how long it is kept.",
    indexable: true,
    markdown: "/privacy.md",
  },
  {
    file: "security.html",
    path: "/security",
    title: "Security policy · Bookclerk",
    description: "How to report a security problem in Bookclerk privately, which versions are supported, and how fixes are published.",
    indexable: true,
    markdown: "/security.md",
    source: "SECURITY.md",
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

/**
 * The human-readable sitemap (src/sitemap.html). The Worker renders it at
 * request time from the docs it serves (renderSitemapPage in render.ts), so
 * it is not in PAGES and has no file in dist/.
 */
const SITEMAP_PAGE: Omit<Page, "file"> = {
  path: "/sitemap",
  title: "Sitemap · Bookclerk",
  description: "Every page on the Bookclerk website and in its documentation, grouped by section.",
  indexable: true,
  markdown: "/sitemap.md",
};

/** The Worker and its page and sitemap renderers, whose sources are part of the docs shell's id (see buildDocsShell). */
const RENDER_SOURCES = ["render.ts", "diagram.ts", "highlight.ts", "last-updated.ts", "sitemap.ts", "worker.ts"];

/**
 * Libraries the renderers run, whose installed versions are part of the docs
 * shell's id too: an update can change the markup of every page.
 */
// beautiful-mermaid's own copy of entities escapes diagram labels; markdown-it
// has another, so it is named by its nested path.
const RENDER_LIBRARIES = ["markdown-it", "@shikijs/core", "@shikijs/engine-javascript", "@shikijs/langs", "beautiful-mermaid", "beautiful-mermaid/node_modules/entities", "elkjs"];

/** Files copied verbatim from src/ to dist/. */
const STATIC_FILES = ["site.css", "sitemap.css", "docs.js", "_headers", "social-card.png"];

/** Web fonts and their licenses copied from src/fonts/ to dist/fonts/ (see src/fonts/README.md). */
const FONT_FILE = /^(?:[a-z0-9-]+\.woff2|OFL-[a-z0-9]+\.txt)$/;

/** Brand assets copied from assets/brand/ (source path, published name). */
const BRAND_FILES: readonly (readonly [string, string])[] = [
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
 * @param iso - A date already checked against DATE_RE.
 * @returns The date for display.
 */
function displayDate(iso: string): string {
  const [y, m = 1, d] = iso.split("-").map(Number);
  return `${d} ${MONTHS[m - 1]} ${y}`;
}

/**
 * Validates the release state from the environment.
 *
 * @param env - Usually `process.env`.
 * @returns `release` is null when no release exists yet, and `demoUrl` when the
 *   deployment has no hosted demo. `docsTags` are the release
 *   tags with published docs, newest first, at most MAX_DOCS_VERSIONS of
 *   them; the newest release is not among them until its docs are.
 * @throws {Error} When a value is present but malformed, or a release tag
 *   comes without its date.
 */
export function readState(env: BuildEnv): ReleaseState {
  const tag = (env["SITE_RELEASE_TAG"] ?? "").trim();
  const date = (env["SITE_RELEASE_DATE"] ?? "").trim();
  const commit = (env["SITE_COMMIT"] ?? "").trim().toLowerCase();
  const builtOn = (env["SITE_BUILT_ON"] ?? "").trim() || new Date().toISOString().slice(0, 10);
  const origin = (env["SITE_ORIGIN"] ?? "").trim() || DEFAULT_ORIGIN;
  const listed = (env["SITE_DOCS_VERSIONS"] ?? "").split(/\s+/).filter((t) => t !== "");
  const demoUrl = (env["SITE_DEMO_URL"] ?? "").trim() || null;

  if (!COMMIT_RE.test(commit)) {
    throw new Error(`SITE_COMMIT must be 7-40 lowercase hex digits, got ${JSON.stringify(commit)}`);
  }
  if (!DATE_RE.test(builtOn)) {
    throw new Error(`SITE_BUILT_ON must be YYYY-MM-DD, got ${JSON.stringify(builtOn)}`);
  }
  if (!ORIGIN_RE.test(origin)) {
    throw new Error(`SITE_ORIGIN must be https://<lowercase hostname>, got ${JSON.stringify(origin)}`);
  }
  if (demoUrl !== null && !ORIGIN_RE.test(demoUrl)) {
    throw new Error(`SITE_DEMO_URL must be https://<lowercase hostname>, got ${JSON.stringify(demoUrl)}`);
  }
  for (const t of listed) {
    if (!TAG_RE.test(t)) throw new Error(`SITE_DOCS_VERSIONS must hold tags like v1.2.3, got ${JSON.stringify(t)}`);
  }
  if (tag === "") {
    if (listed.length > 0) throw new Error("SITE_DOCS_VERSIONS is set but SITE_RELEASE_TAG is not");
    return { release: null, commit, builtOn, origin, demoUrl, docsTags: [] };
  }
  if (!TAG_RE.test(tag)) {
    throw new Error(`SITE_RELEASE_TAG must look like v1.2.3, got ${JSON.stringify(tag)}`);
  }
  if (!DATE_RE.test(date)) {
    throw new Error(`SITE_RELEASE_DATE must be YYYY-MM-DD when a release tag is set, got ${JSON.stringify(date)}`);
  }
  // The picker keeps the newest MAX_DOCS_VERSIONS tags; older ones drop off.
  const docsTags = [...new Set(listed)].slice(0, MAX_DOCS_VERSIONS);
  return { release: { tag, date }, commit, builtOn, origin, demoUrl, docsTags };
}

/**
 * Lists the docs versions the site serves.
 *
 * @param state - Validated release state.
 * @returns The default version (served at `/docs/`: the newest release with
 *   published docs, else main) first, then main and older releases under
 *   `/docs/v/<ref>/`.
 */
export function docsVersions(state: ReleaseState): DocsVersion[] {
  const dev = { ref: "main", label: "Development (main)" };
  const [latest, ...older] = state.docsTags;
  if (latest === undefined) return [{ ...dev, base: "/docs/" }];
  return [
    { ref: latest, label: `${latest} (latest)`, base: "/docs/" },
    { ...dev, base: "/docs/v/main/" },
    ...older.map((t) => ({ ref: t, label: t, base: `/docs/v/${t}/` })),
  ];
}

/**
 * Builds the values templates can read.
 *
 * @param state - Validated release state.
 * @returns `values` fill `{{name}}` holes; `flags` drive `{{#name}}` sections.
 */
export function templateContext(state: ReleaseState): SiteContext {
  const short = state.commit.slice(0, 7);
  // Source links follow the commit the docs came from, so line anchors and
  // moved files stay right until the next deploy.
  const docsTag = state.docsTags[0] ?? null;
  const ref = docsTag ?? state.commit;
  return {
    values: {
      origin: state.origin,
      socialImage: `${state.origin}/social-card.png`,
      repo: REPOSITORY_URL,
      commit: short,
      commitUrl: `${REPOSITORY_URL}/commit/${state.commit}`,
      builtOn: displayDate(state.builtOn),
      contactEmail: CONTACT_EMAIL,
      docsUrl: "/docs/",
      gettingStartedUrl: "/docs/guide/getting-started",
      pluginsUrl: "/docs/plugins/",
      architectureUrl: "/docs/internals/architecture",
      blobBase: `${REPOSITORY_URL}/blob/${ref}/`,
      docsVersion: docsTag ?? "Development (main)",
      releasesUrl: `${REPOSITORY_URL}/releases`,
      releaseTag: state.release?.tag ?? "",
      releaseDate: state.release ? displayDate(state.release.date) : "",
      releaseUrl: state.release ? `${REPOSITORY_URL}/releases/tag/${state.release.tag}` : "",
      demoUrl: state.demoUrl ?? "",
    },
    // Sections do not nest, so the demo's place on a page in each release
    // state gets a flag of its own: before a release the demo is the main
    // way to try Bookclerk (`demolead`), after one it follows the download
    // (`demofollow`); `previewlead` is a pre-release build with no demo.
    flags: {
      prerelease: state.release === null,
      released: state.release !== null,
      demo: state.demoUrl !== null,
      demolead: state.demoUrl !== null && state.release === null,
      demofollow: state.demoUrl !== null && state.release !== null,
      previewlead: state.demoUrl === null && state.release === null,
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
 * @param template - Template text.
 * @param context - Values and flags.
 * @param partials - Partial templates by name.
 * @returns The rendered HTML.
 * @throws {Error} On an unknown partial, flag or value, so a typo fails the
 *   build instead of shipping an empty hole.
 */
export function render(template: string, context: TemplateContext, partials: Readonly<Record<string, string>>): string {
  let out = template;
  for (let depth = 0; depth < MAX_PARTIAL_DEPTH && out.includes("{{>"); depth += 1) {
    out = out.replace(/\{\{>\s*([a-z-]+)\s*\}\}/g, (_, name: string) => {
      const partial = Object.hasOwn(partials, name) ? partials[name] : undefined;
      if (partial === undefined) throw new Error(`unknown partial ${name}`);
      return partial;
    });
  }
  out = out.replace(/\{\{([#^])([a-z]+)\}\}([\s\S]*?)\{\{\/\2\}\}/g, (_, kind: string, name: string, body: string) => {
    const flag = Object.hasOwn(context.flags, name) ? context.flags[name] : undefined;
    if (flag === undefined) throw new Error(`unknown flag ${name}`);
    return flag === (kind === "#") ? body : "";
  });
  out = out.replace(/\{\{([A-Za-z]+)\}\}/g, (_, name: string) => {
    const value = Object.hasOwn(context.values, name) ? context.values[name] : undefined;
    if (value === undefined) throw new Error(`unknown value ${name}`);
    return escapeHtml(value);
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
 * @param svg - Contents of assets/brand/svg/bookclerk-logo.svg.
 * @returns An `<svg>` element ready to inline.
 */
export function inlineLogo(svg: string): string {
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

/** Where a page's rendered `source` file goes in its template (see Page). */
const SOURCE_MARKER = "<!--source-->";

/**
 * Adds one page's metadata to the site-wide template context.
 *
 * @param context - Site-wide values.
 * @param page - The page: URL path, title, description, whether search engines may index it, Open
 *   Graph type (default `website`), the URL of its Markdown copy, if any, and
 *   whether to link that copy with `rel="alternate"` (default true).
 * @param extra - More values the page's template reads.
 * @returns The context for that page.
 */
function pageContext(
  context: SiteContext,
  page: Omit<Page, "file"> & { type?: string },
  extra: Readonly<Record<string, string>> = {},
): TemplateContext {
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
 * Turns a fragment of rendered HTML into plain text.
 *
 * A scan rather than a tag-stripping regular expression: everything from a
 * `<` to the next `>` is dropped, so no `<` survives. `&lt;` and `&gt;` stay
 * encoded for the same reason; the site's own copy uses neither.
 *
 * @param html - Markup from a rendered template, escaped by escapeHtml.
 * @returns Its text: tags dropped, quotes and ampersands decoded and
 *   whitespace collapsed.
 */
export function plainText(html: string): string {
  let text = "";
  let inTag = false;
  for (const ch of html) {
    if (ch === "<") inTag = true;
    else if (ch === ">") inTag = false;
    else if (!inTag) text += ch;
  }
  return text
    .replace(/&(quot|#39|amp);/g, (_, e: string) => ({ quot: '"', "#39": "'", amp: "&" })[e] ?? "")
    .replace(/\s+/g, " ")
    .trim();
}

/**
 * Reads the features and questions the rendered home page shows, so the
 * structured data and llms.txt repeat what visitors see instead of a second
 * copy that could drift from it.
 *
 * @param html - The rendered home page.
 * @returns Each feature card's heading and text, and each FAQ entry's
 *   question and answer, in page order.
 * @throws {Error} When the page has no feature cards or no FAQ entries, so a
 *   markup change cannot silently empty them.
 */
export function homeFacts(html: string): HomeFacts {
  // Each card is read up to its own closing tag, so a card that loses its
  // heading fails the build instead of borrowing the next card's.
  const features = [...html.matchAll(/<div class="feature">((?:(?!<\/div>)[\s\S])*)<\/div>/g)].map(([, card = ""]) => {
    const [, name, text] = card.match(/<h3>([\s\S]*?)<\/h3>\s*<p[^>]*>([\s\S]*?)<\/p>/) ?? [];
    if (name === undefined || text === undefined) throw new Error(`a feature card has no heading and text: ${card.slice(0, 80)}`);
    return { name: plainText(name), text: plainText(text) };
  });
  const faq = [...html.matchAll(/<details[^>]*>\s*<summary>([\s\S]*?)<\/summary>([\s\S]*?)<\/details>/g)].map(
    ([, question = "", answer = ""]) => ({ question: plainText(question), answer: plainText(answer) }),
  );
  if (features.length === 0 || faq.length === 0) throw new Error("the home page lost its feature cards or FAQ");
  return { features, faq };
}

/**
 * The schema.org description of the site and of Bookclerk itself, for the
 * home page.
 *
 * Everything in it is also on the page: the features and questions come from
 * the rendered page (homeFacts), as search engines require of structured data.
 *
 * @param context - Site-wide values.
 * @param facts - What the home page shows.
 * @returns A JSON-LD `@graph` with the WebSite, the organization
 *   behind it, the SoftwareApplication, its source code and the FAQ.
 */
function homeJsonLd(context: SiteContext, facts: HomeFacts): object {
  const { origin, releaseTag, socialImage, contactEmail } = context.values;
  const organization = `${origin}/#organization`;
  return {
    "@context": "https://schema.org",
    "@graph": [
      {
        "@type": "WebSite",
        "@id": `${origin}/#website`,
        name: "Bookclerk",
        url: `${origin}/`,
        inLanguage: "en",
        publisher: { "@id": organization },
      },
      {
        "@type": "Organization",
        "@id": organization,
        name: "Bookclerk",
        url: `${origin}/`,
        logo: `${origin}/android-chrome-512x512.png`,
        email: contactEmail,
        sameAs: [REPOSITORY_URL],
      },
      {
        "@type": "SoftwareSourceCode",
        "@id": `${origin}/#source`,
        name: "Bookclerk",
        codeRepository: REPOSITORY_URL,
        programmingLanguage: ["Rust", "TypeScript"],
        license: "https://www.gnu.org/licenses/agpl-3.0.html",
        targetProduct: { "@id": `${origin}/#app` },
      },
      {
        "@type": "FAQPage",
        "@id": `${origin}/#faq`,
        mainEntity: facts.faq.map(({ question, answer }) => ({
          "@type": "Question",
          name: question,
          acceptedAnswer: { "@type": "Answer", text: answer },
        })),
      },
      {
        "@type": "SoftwareApplication",
        "@id": `${origin}/#app`,
        name: "Bookclerk",
        description: SUMMARY,
        url: `${origin}/`,
        image: socialImage,
        applicationCategory: "MultimediaApplication",
        operatingSystem: "Windows, macOS, Linux",
        license: "https://www.gnu.org/licenses/agpl-3.0.html",
        isAccessibleForFree: true,
        offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
        downloadUrl: `${origin}/download`,
        featureList: facts.features.map(({ name, text }) => `${name}: ${text}`),
        publisher: { "@id": organization },
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
 * have `<!--name-->` markers (render.ts fills both).
 *
 * @param context - Site-wide values.
 * @param partials - Partial templates by name.
 * @param rendered - The local docs, for the sidebar.
 * @param versions - The docs versions served.
 * @returns The shell.
 * @throws {Error} When the rendered template has a hole or marker render.ts
 *   does not fill, or lacks one it does.
 */
export function buildDocsShell(
  context: SiteContext,
  partials: Readonly<Record<string, string>>,
  rendered: RenderedDocs,
  versions: DocsVersion[],
): DocsShell {
  const template = readFileSync(join(HERE, "src", "doc.html"), "utf8");
  const hole = (name: string): string => `%%${name}%%`;
  const page = pageContext(context, {
    path: hole("pagePath"),
    title: hole("pageTitle"),
    description: hole("pageDescription"),
    indexable: true,
    type: "article",
    markdown: hole("markdownUrl"),
  });
  const html = render(template, page, partials);
  const holes = new Set([...html.matchAll(/%%([A-Za-z]+)%%/g)].map((m) => m[1] ?? ""));
  const unknown = [...holes].filter((h) => !SHELL_VALUES.includes(h));
  if (unknown.length > 0 || html.replace(/%%[A-Za-z]+%%/g, "").includes("%%")) {
    throw new Error(`docs shell has holes render.ts does not fill: ${unknown.join(", ") || "stray %%"}`);
  }
  for (const marker of SHELL_MARKERS) {
    if (html.split(`<!--${marker}-->`).length !== 2) throw new Error(`docs shell needs exactly one <!--${marker}-->`);
  }
  // The human-readable sitemap is a website page whose list the Worker
  // fills in at request time, so its shell is baked here, not written to dist/.
  const sitemapHtml = render(readFileSync(join(HERE, "src", "sitemap.html"), "utf8"), pageContext(context, SITEMAP_PAGE), partials);
  if (sitemapHtml.split(SITEMAP_MARKER).length !== 2) throw new Error(`src/sitemap.html needs exactly one ${SITEMAP_MARKER}`);
  const indexable = PAGES.filter((p) => p.indexable);
  const pages = indexable.map((p) => p.path);
  // Titles without the site name, as a list of pages shows them.
  const pageTitles = indexable.map((p) => (p.path === "/" ? "Home" : p.title.replace(/ · Bookclerk$/, "")));
  const shell = { html, origin: context.values.origin, repository: REPOSITORY_URL, versions, nav: rendered.nav, pages, pageTitles, sitemapHtml };
  // The renderers' sources and libraries go into the id too: a change to how
  // pages are rendered must drop pages cached by an earlier deploy, like a
  // shell change.
  const hash = createHash("sha256").update(JSON.stringify(shell));
  for (const file of RENDER_SOURCES) hash.update(readFileSync(join(HERE, file)));
  for (const name of RENDER_LIBRARIES) hash.update(readFileSync(join(HERE, "node_modules", name, "package.json")));
  const id = hash.digest("hex").slice(0, 16);
  return { id, ...shell };
}

/**
 * Escapes text for the label of a Markdown link.
 *
 * @param text - Plain text.
 * @returns The text with backslashes and square brackets escaped.
 */
function markdownLinkText(text: string): string {
  return text.replace(/[\\[\]]/g, "\\$&");
}

/**
 * Writes llms.txt (an index of the docs for language models, per
 * https://llmstxt.org) and llms-full.txt (every doc in one file).
 *
 * @param context - Site-wide values.
 * @param rendered - The docs and sidebar.
 * @param facts - What the home page shows, so llms.txt states the same facts in the same words.
 * @param outDir - Output directory.
 */
function writeLlmsTxt(context: SiteContext, rendered: RenderedDocs, facts: HomeFacts, outDir: string): void {
  const { origin, docsVersion, repo } = context.values;
  const byKey = new Map(rendered.docs.map((d) => [d.key, d]));
  const status = context.flags.prerelease
    ? "Bookclerk has not had a release yet: these docs describe the unstable development version on main."
    : `These docs describe Bookclerk ${docsVersion}.`;
  const lines = [
    "# Bookclerk",
    "",
    `> ${SUMMARY} It is free, self-hosted software (GNU AGPL v3 or later) with a command-line tool, a daemon with a web interface, and a plugin system for stores and destinations.`,
    "",
    `${status} Each doc below is Markdown; the same page as HTML is at the URL without \`.md\`. Source code: ${repo}. Every doc in one file: ${origin}/llms-full.txt. Every page also answers a request with \`Accept: text/markdown\` in Markdown; the home, download, privacy and security pages are at ${origin}/index.md, ${origin}/download.md, ${origin}/privacy.md and ${origin}/security.md.`,
    "",
  ];
  if (context.flags.demo) {
    lines.push(
      `Live demo: ${context.values.demoUrl} gives a visitor their own private copy of Bookclerk in the browser for 30 minutes, to sign in to their stores, scan their library and search it. It cannot download books, and it signs the stores out and deletes the copy when the demo ends.`,
      "",
    );
  }
  // llms.txt allows no headings before the first link section, so the home
  // page's features and questions go in as lists.
  lines.push("What Bookclerk does:", "", ...facts.features.map(({ name, text }) => `- **${name}.** ${text}`), "");
  lines.push("Common questions:", "", ...facts.faq.map(({ question, answer }) => `- **${question}** ${answer}`), "");
  const groups = rendered.nav;
  const designAt = groups.findIndex((g) => g.title === NAV.at(-1)?.title);
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
    if (doc === undefined) continue;
    full.push("---", "", `Source: ${origin}${docMarkdownUrl(key)}`, "", doc.markdown.trim(), "");
  }
  writeFileSync(join(outDir, "llms-full.txt"), full.join("\n"));
}

/**
 * Writes the fallback sitemap.xml and robots.txt.
 *
 * @param context - Site-wide values.
 * @param paths - URL paths of every indexable page.
 * @param outDir - Output directory.
 */
function writeCrawlerFiles(context: SiteContext, paths: readonly string[], outDir: string): void {
  const { origin } = context.values;
  // The Worker answers /sitemap.xml with every docs page and its date; this
  // copy is what it falls back to when the docs repository cannot be read.
  writeFileSync(join(outDir, "sitemap.xml"), sitemapXml(origin, paths.map((path) => ({ path, lastmod: null }))));
  writeFileSync(join(outDir, "robots.txt"), `User-agent: *\nAllow: /\n\nSitemap: ${origin}/sitemap.xml\n`);
}

/**
 * Builds the whole site.
 *
 * @param env - Release state (see top).
 * @param outDir - Output directory; replaced entirely.
 * @param shellFile - Where to write the docs shell (JSON).
 * @returns Published top-level file names, sorted.
 */
export function build(env: BuildEnv, outDir: string, shellFile: string = DOCS_SHELL_FILE): string[] {
  const state = readState(env);
  const context = templateContext(state);
  const src = join(HERE, "src");
  const partials: Record<string, string> = {};
  for (const file of readdirSync(join(src, "partials"))) {
    if (file.endsWith(".html")) partials[file.slice(0, -5)] = readFileSync(join(src, "partials", file), "utf8");
  }
  partials["logo"] = inlineLogo(readFileSync(join(REPO_ROOT, "assets/brand/svg/bookclerk-logo.svg"), "utf8"));

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });
  const indexable: string[] = [];
  let facts: HomeFacts | null = null;
  const rendered = renderDocs(join(REPO_ROOT, "docs"), context.values.blobBase, GENERATED_TITLES);
  const docKeys = new Set(rendered.docs.map((d) => d.key));
  for (const page of PAGES) {
    let html = render(readFileSync(join(src, page.file), "utf8"), pageContext(context, page), partials);
    const pageFacts = page.path === "/" ? homeFacts(html) : null;
    if (pageFacts !== null) facts = pageFacts;
    html = html.replace(JSONLD_MARKER, () => jsonLd(pageFacts === null ? null : homeJsonLd(context, pageFacts)));
    if (page.source) {
      if (html.split(SOURCE_MARKER).length !== 2) throw new Error(`${page.file} needs exactly one ${SOURCE_MARKER}`);
      // The key places the file at the repository root, so its relative
      // links resolve from there (`docs/…` becomes a docs page). Once a
      // release's docs are published they come from its tag, so a link to a
      // newer doc falls back to GitHub; build.test.ts fails the deploy then.
      const doc = renderDoc(readFileSync(join(REPO_ROOT, page.source), "utf8"), `../${page.source.replace(/\.md$/, "")}`, {
        keys: docKeys,
        blobBase: context.values.blobBase,
      });
      html = html.replace(SOURCE_MARKER, () => doc.html);
      if (page.markdown) writeFileSync(join(outDir, page.markdown.slice(1)), doc.markdown);
    }
    writeFileSync(join(outDir, page.file), html);
    if (page.markdown && !page.source) {
      // Template values are validated tags, dates, commit ids and URLs, so
      // HTML escaping leaves them unchanged in Markdown too.
      const md = render(readFileSync(join(src, page.markdown.slice(1)), "utf8"), pageContext(context, page), partials);
      writeFileSync(join(outDir, page.markdown.slice(1)), md);
    }
    if (page.indexable) indexable.push(page.path);
  }
  indexable.push(docUrl("README"), ...rendered.nav.flatMap((g) => g.pages.map((p) => docUrl(p.key))));
  mkdirSync(dirname(shellFile), { recursive: true });
  writeFileSync(shellFile, JSON.stringify(buildDocsShell(context, partials, rendered, docsVersions(state))));
  writeCrawlerFiles(context, indexable, outDir);
  if (facts === null) throw new Error("PAGES has no home page");
  writeLlmsTxt(context, rendered, facts, outDir);
  for (const file of STATIC_FILES) cpSync(join(src, file), join(outDir, file));
  mkdirSync(join(outDir, "fonts"));
  for (const file of readdirSync(join(src, "fonts"))) {
    if (FONT_FILE.test(file)) cpSync(join(src, "fonts", file), join(outDir, "fonts", file));
  }
  for (const [from, to] of BRAND_FILES) cpSync(join(REPO_ROOT, "assets/brand", from), join(outDir, to));
  return readdirSync(outDir).sort();
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const files = build(process.env, join(HERE, "dist"));
  console.log(`built ${files.length} files into dist/`);
}
