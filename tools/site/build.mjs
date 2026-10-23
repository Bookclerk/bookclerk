// Builds the static bookclerk.app website into dist/.
//
// Dependency-free on purpose: the site is plain HTML and CSS, so the only npm
// package in this directory is wrangler (deploy tooling).
//
// Release state comes from the environment, set by
// .github/workflows/site-deploy.yml:
//
//   SITE_RELEASE_TAG   newest published release tag (e.g. v0.1.0); empty or
//                      unset means "no release yet", which renders every
//                      pre-release notice
//   SITE_RELEASE_DATE  that release's publish date, YYYY-MM-DD
//   SITE_COMMIT        commit the site is built from (7-40 hex digits)
//   SITE_BUILT_ON      build date, YYYY-MM-DD (defaults to today, UTC)
//
// Every value is validated before it reaches a template, and every template
// value is HTML-escaped.

import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** Directory holding this script. */
const HERE = dirname(fileURLToPath(import.meta.url));

/** Repository root (tools/site/../..). */
const REPO_ROOT = join(HERE, "..", "..");

/** GitHub repository the site links to. */
export const REPOSITORY_URL = "https://github.com/Bookclerk/bookclerk";

/** Pages rendered from src/<name> to dist/<name>. */
const PAGES = ["index.html", "download.html", "404.html"];

/** Files copied verbatim from src/ to dist/. */
const STATIC_FILES = ["site.css", "_headers", "robots.txt"];

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

/** How deep partials may include other partials. */
const MAX_PARTIAL_DEPTH = 3;

/** Month names for human-readable dates. */
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/**
 * Escapes text for an HTML element body or a double-quoted attribute.
 *
 * @param {string} value Raw text.
 * @returns {string} The text with `& < > " '` replaced by entities.
 */
export function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

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
 * @returns {{release: {tag: string, date: string} | null, commit: string, builtOn: string}}
 *   `release` is null when no release exists yet.
 * @throws {Error} When a value is present but malformed, or a release tag
 *   comes without its date.
 */
export function readState(env) {
  const tag = (env.SITE_RELEASE_TAG ?? "").trim();
  const date = (env.SITE_RELEASE_DATE ?? "").trim();
  const commit = (env.SITE_COMMIT ?? "").trim().toLowerCase();
  const builtOn = (env.SITE_BUILT_ON ?? "").trim() || new Date().toISOString().slice(0, 10);

  if (!COMMIT_RE.test(commit)) {
    throw new Error(`SITE_COMMIT must be 7-40 lowercase hex digits, got ${JSON.stringify(commit)}`);
  }
  if (!DATE_RE.test(builtOn)) {
    throw new Error(`SITE_BUILT_ON must be YYYY-MM-DD, got ${JSON.stringify(builtOn)}`);
  }
  if (tag === "") {
    return { release: null, commit, builtOn };
  }
  if (!TAG_RE.test(tag)) {
    throw new Error(`SITE_RELEASE_TAG must look like v1.2.3, got ${JSON.stringify(tag)}`);
  }
  if (!DATE_RE.test(date)) {
    throw new Error(`SITE_RELEASE_DATE must be YYYY-MM-DD when a release tag is set, got ${JSON.stringify(date)}`);
  }
  return { release: { tag, date }, commit, builtOn };
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
  const ref = state.release ? state.release.tag : "main";
  return {
    values: {
      repo: REPOSITORY_URL,
      commit: short,
      commitUrl: `${REPOSITORY_URL}/commit/${state.commit}`,
      builtOn: displayDate(state.builtOn),
      docsUrl: `${REPOSITORY_URL}/blob/${ref}/docs/README.md`,
      gettingStartedUrl: `${REPOSITORY_URL}/blob/${ref}/docs/getting-started.md`,
      pluginsUrl: `${REPOSITORY_URL}/blob/${ref}/docs/plugins.md`,
      architectureUrl: `${REPOSITORY_URL}/blob/${ref}/docs/architecture.md`,
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

/**
 * Builds the whole site.
 *
 * @param {Record<string, string | undefined>} env Release state (see top).
 * @param {string} outDir Output directory; replaced entirely.
 * @returns {string[]} Published file names, sorted.
 */
export function build(env, outDir) {
  const context = templateContext(readState(env));
  const src = join(HERE, "src");
  const partials = {};
  for (const file of readdirSync(join(src, "partials"))) {
    if (file.endsWith(".html")) partials[file.slice(0, -5)] = readFileSync(join(src, "partials", file), "utf8");
  }
  partials.logo = inlineLogo(readFileSync(join(REPO_ROOT, "assets/brand/svg/bookclerk-logo.svg"), "utf8"));

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });
  for (const page of PAGES) {
    writeFileSync(join(outDir, page), render(readFileSync(join(src, page), "utf8"), context, partials));
  }
  for (const file of STATIC_FILES) cpSync(join(src, file), join(outDir, file));
  for (const [from, to] of BRAND_FILES) cpSync(join(REPO_ROOT, "assets/brand", from), join(outDir, to));
  return readdirSync(outDir).sort();
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const files = build(process.env, join(HERE, "dist"));
  console.log(`built ${files.length} files into dist/`);
}
