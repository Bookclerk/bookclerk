// Renders the repository's docs/ Markdown into site pages under /docs/.
//
// The docs are read from the checkout the site is built from, so a release
// build shows the docs at that release's tag and a development build shows
// main. Raw HTML in Markdown is not rendered (it is shown as text), so docs
// that mention placeholders such as <branch> read correctly and nothing in a
// doc can add markup the site's CSP would have to trust.
//
// Each doc is also published as Markdown beside its page (/docs/<key>.md),
// with links rewritten the same way, for readers and agents that prefer the
// source text.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, posix, relative, sep } from "node:path";

import MarkdownIt from "markdown-it";

/**
 * Sidebar groups, in order. Each entry is a docs path without `.md`. A doc
 * that exists but is not listed lands in the final "More" group, so a new doc
 * is never left out of the site.
 */
export const NAV = [
  {
    title: "Using Bookclerk",
    pages: ["getting-started", "sources", "destinations", "integrations", "discovery", "configuration", "gui", "operations", "cluster-hosts", "migration", "libation-parity"],
  },
  {
    title: "How it works",
    pages: ["architecture", "database", "jobs", "storage-bounds", "media", "diagnostics", "windows-appcontainer"],
  },
  {
    title: "Plugins",
    pages: ["plugins", "plugin-registry", "workerd-bridge", "sql-contract/v1", "source-candidates"],
  },
  {
    title: "Contributing",
    pages: ["devcontainer", "ci", "packaging", "code-documentation", "api/README"],
  },
  {
    title: "Design decisions",
    pages: [
      "adr/plugin-capabilities-v3",
      "adr/plugin-workers-rpc-workerd",
      "adr/first-party-identity",
      "adr/plugin-oidc-clients",
      "adr/sql-database-contract",
      "adr/schema-versioning",
      "adr/control-plane",
      "adr/untrusted-input-fuzzing",
      "adr/security-advisory-automation",
    ],
  },
];

/**
 * Directories under docs/ that scripts/generate-api-docs.sh fills with
 * generated (gitignored) API reference. Their Markdown is generator output,
 * not docs; build.mjs publishes the trees as they are.
 */
export const GENERATED_DIRS = ["api/rust", "api/typescript", "api/ui", "api/python"];

/**
 * Lists every Markdown file under a directory, skipping GENERATED_DIRS.
 *
 * @param {string} root The docs directory.
 * @returns {string[]} Paths relative to `root`, POSIX separators, sorted.
 */
export function markdownFiles(root) {
  const out = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const full = join(dir, name);
      if (GENERATED_DIRS.includes(relative(root, full).split(sep).join("/"))) continue;
      if (statSync(full).isDirectory()) walk(full);
      else if (name.endsWith(".md")) out.push(relative(root, full).split(sep).join("/"));
    }
  };
  walk(root);
  return out.sort();
}

/**
 * The site URL of a doc.
 *
 * @param {string} key Docs path without `.md` (`plugins`, `adr/control-plane`).
 * @returns {string} `/docs/` for the index, else `/docs/<key>`; a nested
 *   README maps to its directory (`api/README` → `/docs/api/`).
 */
export function docUrl(key) {
  if (key === "README") return "/docs/";
  if (key.endsWith("/README")) return `/docs/${key.slice(0, -"README".length)}`;
  return `/docs/${key}`;
}

/**
 * The site URL of a doc's Markdown copy.
 *
 * @param {string} key Docs path without `.md`.
 * @returns {string} `/docs/<key>.md`; the index is `/docs/README.md`.
 */
export function docMarkdownUrl(key) {
  return `/docs/${key}.md`;
}

/**
 * The output file for a doc, relative to dist/.
 *
 * @param {string} key Docs path without `.md`.
 * @returns {string} `docs/index.html`, `docs/<dir>/index.html` or `docs/<key>.html`.
 */
export function docFile(key) {
  if (key === "README") return "docs/index.html";
  if (key.endsWith("/README")) return `docs/${key.slice(0, -"README".length)}index.html`;
  return `docs/${key}.html`;
}

/**
 * Turns heading text into an anchor id the way GitHub does, so existing
 * `#section` links in the docs keep working.
 *
 * @param {string} text Heading text content.
 * @returns {string} Lowercase id: letters, digits, `-` and `_`.
 */
export function slugify(text) {
  return text
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");
}

/**
 * Rewrites a link found in a doc.
 *
 * @param {string} href The link as written in the Markdown.
 * @param {string} docKey The doc the link appears in (path without `.md`).
 * @param {Set<string>} keys Every doc on the site.
 * @param {string} blobBase GitHub URL prefix for files at the site's ref,
 *   ending in `/blob/<ref>/`.
 * @param {(key: string) => string} [urlFor] Site URL of another doc:
 *   {@link docUrl} for pages, {@link docMarkdownUrl} for Markdown copies.
 * @returns {string} A site URL for another doc, a GitHub URL for any other
 *   repository file, or the link unchanged when it is absolute or an anchor.
 */
export function rewriteLink(href, docKey, keys, blobBase, urlFor = docUrl) {
  if (href === "" || href.startsWith("#") || /^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("//")) {
    return href;
  }
  const hashAt = href.indexOf("#");
  const path = hashAt === -1 ? href : href.slice(0, hashAt);
  const hash = hashAt === -1 ? "" : href.slice(hashAt);
  const target = posix.normalize(posix.join("docs", posix.dirname(docKey), decodeURI(path)));
  if (target.startsWith("docs/") && target.endsWith(".md")) {
    const key = target.slice("docs/".length, -".md".length);
    if (keys.has(key)) return urlFor(key) + hash;
  }
  if (target === "docs" || target === "docs/") return urlFor("README") + hash;
  if (target.startsWith("..")) return href;
  // posix.normalize keeps a trailing slash, which marks a directory.
  const base = target.endsWith("/") ? blobBase.replace(/\/blob\//, "/tree/") : blobBase;
  return base + encodeURI(target) + hash;
}

/**
 * Builds the Markdown renderer for one doc.
 *
 * @param {string} docKey The doc being rendered.
 * @param {Set<string>} keys Every doc on the site.
 * @param {string} blobBase See {@link rewriteLink}.
 * @returns {MarkdownIt} A renderer with raw HTML off, GitHub-style heading
 *   ids, rewritten links and tables wrapped for horizontal scrolling.
 */
function renderer(docKey, keys, blobBase) {
  const md = new MarkdownIt({ html: false, linkify: false, typographer: false });
  const seen = new Map();
  md.core.ruler.push("heading_ids", (state) => {
    const tokens = state.tokens;
    for (let i = 0; i < tokens.length; i += 1) {
      if (tokens[i].type !== "heading_open") continue;
      const text = tokens[i + 1].children.filter((t) => t.type === "text" || t.type === "code_inline").map((t) => t.content).join("");
      const base = slugify(text);
      const n = seen.get(base) ?? 0;
      seen.set(base, n + 1);
      tokens[i].attrSet("id", n === 0 ? base : `${base}-${n}`);
    }
  });
  const linkOpen = md.renderer.rules.link_open ?? ((tokens, idx, options, env, self) => self.renderToken(tokens, idx, options));
  md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
    const token = tokens[idx];
    token.attrSet("href", rewriteLink(token.attrGet("href") ?? "", docKey, keys, blobBase));
    return linkOpen(tokens, idx, options, env, self);
  };
  md.renderer.rules.table_open = () => '<div class="table-scroll"><table>\n';
  md.renderer.rules.table_close = () => "</table></div>\n";
  return md;
}

/** Longest page description, in characters, before it is shortened. */
const DESCRIPTION_MAX = 160;

/**
 * Plain text of an inline token's children.
 *
 * @param {import("markdown-it/lib/token.mjs").default} inline An `inline` token.
 * @returns {string} Its text and code spans, whitespace collapsed.
 */
function inlineText(inline) {
  return inline.children
    .filter((t) => t.type === "text" || t.type === "code_inline" || t.type === "softbreak")
    .map((t) => (t.type === "softbreak" ? " " : t.content))
    .join("")
    .replace(/\s+/g, " ")
    .trim();
}

/**
 * Cuts text at a word boundary.
 *
 * @param {string} text Plain text.
 * @returns {string} The text, or at most DESCRIPTION_MAX characters of it
 *   ending in an ellipsis.
 */
function shorten(text) {
  if (text.length <= DESCRIPTION_MAX) return text;
  const cut = text.slice(0, DESCRIPTION_MAX - 1);
  return `${cut.slice(0, cut.lastIndexOf(" ")).replace(/[\s,;:.]+$/, "")}…`;
}

/**
 * Summarizes a doc for search results and link previews.
 *
 * @param {import("markdown-it/lib/token.mjs").default[]} tokens The doc's
 *   parsed tokens.
 * @returns {string} The doc's introduction (its first top-level paragraph,
 *   when that comes before the first section heading), else a list of its
 *   sections; shortened (see shorten). Empty when it has neither.
 */
function describe(tokens) {
  const firstSection = tokens.findIndex((t) => t.type === "heading_open" && t.tag !== "h1");
  const intro = tokens.findIndex((t) => t.type === "paragraph_open" && t.level === 0);
  if (intro !== -1 && (firstSection === -1 || intro < firstSection)) return shorten(inlineText(tokens[intro + 1]));
  const sections = tokens.flatMap((t, i) => (t.type === "heading_open" && t.tag === "h2" ? [inlineText(tokens[i + 1])] : []));
  return sections.length === 0 ? "" : shorten(`Covers ${sections.join(", ")}.`);
}

/** An inline link's target and optional title: `](target "title")`. */
const INLINE_LINK_RE = /\]\(([^()\s<>]+)((?:\s+(?:"[^"]*"|'[^']*'))?)\)/g;

/** A link reference definition: `[label]: target`. */
const REFERENCE_DEF_RE = /^( {0,3}\[[^\]]+\]:[ \t]*)(\S+)/gm;

/**
 * Rewrites a doc's Markdown source for publishing beside its page.
 *
 * Link targets in inline links and reference definitions are rewritten
 * whole; fenced code blocks and inline code spans are left alone.
 *
 * @param {string} source The doc's Markdown.
 * @param {string} docKey The doc's key.
 * @param {Set<string>} keys Every doc on the site.
 * @param {string} blobBase See {@link rewriteLink}.
 * @returns {string} The Markdown with doc links pointing at the other
 *   Markdown copies and repository links pointing at GitHub.
 */
export function siteMarkdown(source, docKey, keys, blobBase) {
  const rewrite = (text) =>
    text
      .replace(INLINE_LINK_RE, (_, href, title) => `](${rewriteLink(href, docKey, keys, blobBase, docMarkdownUrl)}${title})`)
      .replace(REFERENCE_DEF_RE, (_, head, href) => head + rewriteLink(href, docKey, keys, blobBase, docMarkdownUrl));
  // Odd-numbered pieces of a split on backtick runs are inline code.
  const outsideCode = (line) =>
    line
      .split(/(`+[^`]*`+)/)
      .map((piece, i) => (i % 2 === 1 ? piece : rewrite(piece)))
      .join("");
  let fence = null;
  return source
    .split("\n")
    .map((line) => {
      const marker = line.match(/^ {0,3}(`{3,}|~{3,})/);
      if (marker && (fence === null || marker[1].startsWith(fence))) {
        fence = fence === null ? marker[1] : null;
        return line;
      }
      return fence === null ? outsideCode(line) : line;
    })
    .join("\n");
}

/**
 * Reads and renders every doc.
 *
 * @param {string} docsRoot The repository's docs/ directory.
 * @param {string} blobBase See {@link rewriteLink}.
 * @returns {{nav: {title: string, pages: {key: string, title: string}[]}[],
 *   docs: {key: string, title: string, description: string, source: string,
 *   html: string, markdown: string}[]}}
 *   The sidebar (only docs that exist) and each doc's title (its first `#`
 *   heading, else its path), summary (see describe), source path, HTML body
 *   and Markdown copy (see siteMarkdown).
 * @throws {Error} When the docs index (`README.md`) is missing.
 */
export function renderDocs(docsRoot, blobBase) {
  const files = markdownFiles(docsRoot);
  const keys = new Set(files.map((f) => f.slice(0, -".md".length)));
  if (!keys.has("README")) throw new Error(`no README.md in ${docsRoot}`);

  const docs = files.map((file) => {
    const key = file.slice(0, -".md".length);
    const source = readFileSync(join(docsRoot, file), "utf8");
    const heading = source.match(/^#\s+(.+?)\s*#*\s*$/m);
    const title = heading ? heading[1].replace(/`/g, "") : key;
    const md = renderer(key, keys, blobBase);
    const tokens = md.parse(source, {});
    const markdown = siteMarkdown(source, key, keys, blobBase);
    return {
      key,
      title,
      description: describe(tokens),
      source: `docs/${file}`,
      html: md.renderer.render(tokens, md.options, {}),
      markdown,
    };
  });
  const byKey = new Map(docs.map((d) => [d.key, d]));

  const listed = new Set(NAV.flatMap((g) => g.pages));
  const nav = NAV.map((g) => ({
    title: g.title,
    pages: g.pages.filter((k) => byKey.has(k)).map((k) => ({ key: k, title: byKey.get(k).title })),
  }));
  const extra = docs.filter((d) => d.key !== "README" && !listed.has(d.key));
  if (extra.length > 0) nav.push({ title: "More", pages: extra.map((d) => ({ key: d.key, title: d.title })) });
  return { nav, docs };
}
