// Renders the repository's docs/ Markdown into site pages under /docs/.
//
// The docs are read from the checkout the site is built from, so a release
// build shows the docs at that release's tag and a development build shows
// main. Raw HTML in Markdown is not rendered (it is shown as text), so docs
// that mention placeholders such as <branch> read correctly and nothing in a
// doc can add markup the site's CSP would have to trust.

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
 * Lists every Markdown file under a directory.
 *
 * @param {string} root The docs directory.
 * @returns {string[]} Paths relative to `root`, POSIX separators, sorted.
 */
function markdownFiles(root) {
  const out = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const full = join(dir, name);
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
 * @returns {string} A site URL for another doc, a GitHub URL for any other
 *   repository file, or the link unchanged when it is absolute or an anchor.
 */
export function rewriteLink(href, docKey, keys, blobBase) {
  if (href === "" || href.startsWith("#") || /^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("//")) {
    return href;
  }
  const hashAt = href.indexOf("#");
  const path = hashAt === -1 ? href : href.slice(0, hashAt);
  const hash = hashAt === -1 ? "" : href.slice(hashAt);
  const target = posix.normalize(posix.join("docs", posix.dirname(docKey), decodeURI(path)));
  if (target.startsWith("docs/") && target.endsWith(".md")) {
    const key = target.slice("docs/".length, -".md".length);
    if (keys.has(key)) return docUrl(key) + hash;
  }
  if (target === "docs" || target === "docs/") return "/docs/" + hash;
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

/**
 * Reads and renders every doc.
 *
 * @param {string} docsRoot The repository's docs/ directory.
 * @param {string} blobBase See {@link rewriteLink}.
 * @returns {{nav: {title: string, pages: {key: string, title: string}[]}[],
 *   docs: {key: string, title: string, source: string, html: string}[]}}
 *   The sidebar (only docs that exist) and each doc's title (its first `#`
 *   heading, else its path), source path and HTML body.
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
    return { key, title, source: `docs/${file}`, html: renderer(key, keys, blobBase).render(source) };
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
