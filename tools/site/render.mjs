// Renders one doc's Markdown into a site page body, with no file system or
// Node APIs, so the build (docs.mjs) and the Worker (worker.js) share it.
//
// The Worker reads docs from the docs repository (Bookclerk/bookclerk-docs)
// at the version a request names, so the renderer cannot list every doc. A
// link to a `.md` file inside the docs tree is taken to be a doc: the docs
// build fails on links that do not resolve, so the tree it publishes only
// has links that do. Other relative links point at repository files and go
// to GitHub at the docs' ref.
//
// Raw HTML in Markdown is not rendered (it is shown as text), so docs that
// mention placeholders such as <branch> read correctly and nothing in a doc
// can add markup the site's CSP would have to trust.

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
    title: "API reference",
    pages: ["api/README", "api/typescript/README", "api/python/README", "api/rust/bookclerk_plugin_sdk/index", "api/rust/README"],
  },
  {
    title: "Contributing",
    pages: ["devcontainer", "ci", "packaging", "code-documentation"],
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

/** Sidebar titles for pages that are generated, not in docs/. */
export const GENERATED_TITLES = {
  "api/typescript/README": "TypeScript plugin SDK",
  "api/python/README": "Python plugin SDK",
  "api/rust/bookclerk_plugin_sdk/index": "Rust plugin SDK",
  "api/rust/README": "Rust crates",
};

/**
 * Normalizes a POSIX path the way `path.posix.normalize` does.
 *
 * @param {string} path A relative path.
 * @returns {string} The path without `.` segments and with `..` applied;
 *   leading `..` segments are kept, as is a trailing slash.
 */
export function normalizePath(path) {
  const out = [];
  for (const part of path.split("/")) {
    if (part === "" || part === ".") continue;
    if (part === ".." && out.length > 0 && out[out.length - 1] !== "..") out.pop();
    else out.push(part);
  }
  const joined = out.join("/") || ".";
  return path.endsWith("/") && joined !== "." ? `${joined}/` : joined;
}

/**
 * The directory part of a docs key.
 *
 * @param {string} key Docs path without `.md`.
 * @returns {string} `adr` for `adr/control-plane`, `""` for `plugins`.
 */
function keyDir(key) {
  const at = key.lastIndexOf("/");
  return at === -1 ? "" : key.slice(0, at);
}

/**
 * The site URL of a doc.
 *
 * @param {string} key Docs path without `.md` (`plugins`, `adr/control-plane`).
 * @param {string} [base] URL prefix of the docs version, ending in `/`.
 * @returns {string} `<base>` for the index, else `<base><key>`; a nested
 *   README or index maps to its directory (`api/README` → `<base>api/`).
 */
export function docUrl(key, base = "/docs/") {
  if (key === "README") return base;
  if (key.endsWith("/README")) return base + key.slice(0, -"README".length);
  return base + key;
}

/**
 * The site URL of a doc's Markdown copy.
 *
 * @param {string} key Docs path without `.md`.
 * @param {string} [base] URL prefix of the docs version, ending in `/`.
 * @returns {string} `<base><key>.md`; the index is `<base>README.md`.
 */
export function docMarkdownUrl(key, base = "/docs/") {
  return `${base}${key}.md`;
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
 * @param {Set<string> | null} keys Every doc on the site, or null when any
 *   `.md` file inside the docs tree counts as a doc.
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
  let decoded;
  try {
    decoded = decodeURI(path);
  } catch {
    return href;
  }
  const dir = keyDir(docKey);
  const target = normalizePath(`docs/${dir ? `${dir}/` : ""}${decoded}`);
  if (target.startsWith("docs/") && target.endsWith(".md")) {
    const key = target.slice("docs/".length, -".md".length);
    if (keys === null || keys.has(key)) return urlFor(key) + hash;
  }
  if (target === "docs" || target === "docs/") return urlFor("README") + hash;
  if (target.startsWith("..")) return href;
  // normalizePath keeps a trailing slash, which marks a directory.
  const base = target.endsWith("/") ? blobBase.replace(/\/blob\//, "/tree/") : blobBase;
  return base + encodeURI(target) + hash;
}

/**
 * Builds the Markdown renderer for one doc.
 *
 * @param {string} docKey The doc being rendered.
 * @param {Set<string> | null} keys See {@link rewriteLink}.
 * @param {string} blobBase See {@link rewriteLink}.
 * @param {(key: string) => string} urlFor Site URL of another doc's page.
 * @returns {MarkdownIt} A renderer with raw HTML off, GitHub-style heading
 *   ids, rewritten links, and code blocks and tables that keyboard users
 *   can scroll sideways.
 */
function renderer(docKey, keys, blobBase, urlFor) {
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
    token.attrSet("href", rewriteLink(token.attrGet("href") ?? "", docKey, keys, blobBase, urlFor));
    return linkOpen(tokens, idx, options, env, self);
  };
  // Code blocks and wide tables scroll sideways on narrow screens; tabindex
  // lets keyboard users scroll them too.
  for (const rule of ["fence", "code_block"]) {
    const base = md.renderer.rules[rule];
    md.renderer.rules[rule] = (tokens, idx, options, env, self) => base(tokens, idx, options, env, self).replace(/^<pre/, '<pre tabindex="0"');
  }
  md.renderer.rules.table_open = () => '<div class="table-scroll" tabindex="0"><table>\n';
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
 * @param {Set<string> | null} keys See {@link rewriteLink}.
 * @param {string} blobBase See {@link rewriteLink}.
 * @param {string} [base] URL prefix of the docs version, ending in `/`.
 * @returns {string} The Markdown with doc links pointing at the other
 *   Markdown copies and repository links pointing at GitHub.
 */
export function siteMarkdown(source, docKey, keys, blobBase, base = "/docs/") {
  const urlFor = (key) => docMarkdownUrl(key, base);
  const rewrite = (text) =>
    text
      .replace(INLINE_LINK_RE, (_, href, title) => `](${rewriteLink(href, docKey, keys, blobBase, urlFor)}${title})`)
      .replace(REFERENCE_DEF_RE, (_, head, href) => head + rewriteLink(href, docKey, keys, blobBase, urlFor));
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
 * A doc's title: its first `#` heading without code marks.
 *
 * @param {string} source The doc's Markdown.
 * @param {string} key Fallback: the doc's key.
 * @returns {string} The title.
 */
export function docTitle(source, key) {
  const heading = source.match(/^#\s+(.+?)\s*#*\s*$/m);
  return heading ? heading[1].replace(/`/g, "") : key;
}

/**
 * Renders one doc.
 *
 * @param {string} source The doc's Markdown.
 * @param {string} key The doc's key (path without `.md`).
 * @param {{keys?: Set<string> | null, blobBase: string, base?: string}} options
 *   Docs that exist (null: any `.md` in the tree), the GitHub prefix for
 *   repository files, and the URL prefix of the docs version.
 * @returns {{title: string, description: string, html: string, markdown: string}}
 *   The title (see docTitle), summary (see describe), HTML body and Markdown
 *   copy (see siteMarkdown).
 */
export function renderDoc(source, key, { keys = null, blobBase, base = "/docs/" }) {
  const md = renderer(key, keys, blobBase, (k) => docUrl(k, base));
  const tokens = md.parse(source, {});
  return {
    title: docTitle(source, key),
    description: describe(tokens),
    html: md.renderer.render(tokens, md.options, {}),
    markdown: siteMarkdown(source, key, keys, blobBase, base),
  };
}

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
 * Serializes structured data for a `<script type="application/ld+json">`.
 *
 * @param {object | null} data A schema.org JSON-LD object, or null for none.
 * @returns {string} The script element, with `<` escaped so the JSON can
 *   never close it; empty when `data` is null.
 */
export function jsonLd(data) {
  if (data === null) return "";
  return `<script type="application/ld+json">${JSON.stringify(data).replaceAll("<", "\\u003c")}</script>`;
}

/**
 * The schema.org description of one doc page.
 *
 * @param {string} origin The site's origin.
 * @param {string} base URL prefix of the docs version, ending in `/`.
 * @param {{key: string, title: string, description: string}} doc The doc.
 * @returns {object} A JSON-LD `@graph` with the TechArticle and its
 *   breadcrumb trail (Docs › title).
 */
export function docJsonLd(origin, base, doc) {
  const url = origin + docUrl(doc.key, base);
  const crumbs = [{ name: "Docs", item: origin + base }];
  if (doc.key !== "README") crumbs.push({ name: doc.title, item: url });
  return {
    "@context": "https://schema.org",
    "@graph": [
      {
        "@type": "TechArticle",
        headline: doc.title,
        ...(doc.description ? { description: doc.description } : {}),
        url,
        inLanguage: "en",
        isPartOf: { "@id": `${origin}/#website` },
      },
      {
        "@type": "BreadcrumbList",
        itemListElement: crumbs.map((c, i) => ({ "@type": "ListItem", position: i + 1, name: c.name, item: c.item })),
      },
    ],
  };
}

/**
 * Builds the sidebar from the docs that exist.
 *
 * @param {Map<string, string>} titles Title of every doc by key.
 * @returns {{title: string, pages: {key: string, title: string}[]}[]} NAV
 *   without missing docs, plus a "More" group for docs NAV leaves out.
 */
export function buildNav(titles) {
  const listed = new Set(NAV.flatMap((g) => g.pages));
  const nav = NAV.map((g) => ({
    title: g.title,
    pages: g.pages.filter((k) => titles.has(k)).map((k) => ({ key: k, title: titles.get(k) })),
  })).filter((g) => g.pages.length > 0);
  const extra = [...titles.keys()].filter((k) => k !== "README" && !listed.has(k) && !k.startsWith("api/"));
  if (extra.length > 0) nav.push({ title: "More", pages: extra.map((k) => ({ key: k, title: titles.get(k) })) });
  return nav;
}

/**
 * Renders the sidebar for one page.
 *
 * @param {ReturnType<typeof buildNav>} nav The sidebar groups.
 * @param {string} current Key of the page being shown.
 * @param {string} base URL prefix of the docs version, ending in `/`.
 * @returns {string} One heading and list per group.
 */
export function navHtml(nav, current, base) {
  return nav
    .map((group) => {
      const links = group.pages
        .map((p) => {
          const mark = p.key === current ? ' aria-current="page"' : "";
          return `<li><a href="${escapeHtml(docUrl(p.key, base))}"${mark}>${escapeHtml(p.title)}</a></li>`;
        })
        .join("");
      return `<h2>${escapeHtml(group.title)}</h2><ul>${links}</ul>`;
    })
    .join("\n");
}

/**
 * Renders the version picker for one page.
 *
 * @param {{ref: string, label: string, base: string}[]} versions Every docs
 *   version, the default first.
 * @param {string} current The ref being shown.
 * @param {string} key Key of the page being shown; the picker links to the
 *   same page in each version.
 * @returns {string} A `<details>` element listing the versions.
 */
export function versionsHtml(versions, current, key) {
  const shown = versions.find((v) => v.ref === current) ?? versions[0];
  const badge = shown.ref === "main" ? ' <span class="badge unstable-badge">unstable</span>' : "";
  const items = versions
    .map((v) => {
      const mark = v.ref === current ? ' aria-current="true"' : "";
      return `<li><a href="${escapeHtml(docUrl(key, v.base))}"${mark}><strong>${escapeHtml(v.label)}</strong><span class="faint">From ${escapeHtml(v.ref)}</span></a></li>`;
    })
    .join("");
  return `<details class="version-picker"><summary><span class="faint">Version</span> <strong>${escapeHtml(shown.label)}</strong>${badge}</summary><ul>${items}</ul></details>`;
}

/** A `%%name%%` hole in the docs shell, filled with an escaped value. */
const SHELL_VALUE_RE = /%%([A-Za-z]+)%%/g;

/** Values every docs shell hole takes. */
export const SHELL_VALUES = ["pageTitle", "pageDescription", "pagePath", "markdownUrl", "docsBase"];

/** `<!--name-->` markers in the docs shell, filled with trusted HTML. */
export const SHELL_MARKERS = ["jsonld", "doc-versions", "doc-nav", "doc-note", "doc-body", "doc-footer"];

/**
 * Fills the docs page shell that build.mjs writes.
 *
 * Values are HTML-escaped; markers take HTML the caller built (rendered
 * Markdown, which has raw HTML off, and the helpers above). Values are filled
 * before markers, and markers in one pass, so nothing inserted is ever read
 * as a hole or a marker.
 *
 * @param {string} shell The shell's HTML.
 * @param {Record<string, string>} values One entry per SHELL_VALUES name.
 * @param {Record<string, string>} markers One entry per SHELL_MARKERS name.
 * @returns {string} The page.
 * @throws {Error} When a hole has no value or a marker is missing.
 */
export function fillShell(shell, values, markers) {
  const out = shell.replace(SHELL_VALUE_RE, (_, name) => {
    if (!(name in values)) throw new Error(`docs shell hole ${name} has no value`);
    return escapeHtml(values[name]);
  });
  for (const name of SHELL_MARKERS) {
    if (!out.includes(`<!--${name}-->`)) throw new Error(`docs shell has no <!--${name}-->`);
  }
  // One pass, so HTML put in for one marker is never searched for another.
  const markerRe = new RegExp(`<!--(${SHELL_MARKERS.join("|")})-->`, "g");
  return out.replace(markerRe, (_, name) => markers[name] ?? "");
}

/**
 * Everything build.mjs bakes into the Worker for docs pages.
 *
 * @typedef {object} DocsShell
 * @property {string} id A hash of the rest of the shell, so caches keyed by
 *   it drop pages from an earlier deploy.
 * @property {string} html src/doc.html rendered for the site, with
 *   SHELL_VALUES holes and SHELL_MARKERS markers left to fill.
 * @property {string} origin The site's origin.
 * @property {string} repository GitHub URL of the source repository.
 * @property {{ref: string, label: string, base: string}[]} versions Every
 *   docs version the site serves, the default (served at `/docs/`) first:
 *   its git ref in the docs repository, the name readers see, and its URL
 *   prefix.
 * @property {ReturnType<typeof buildNav>} nav The sidebar, from the docs the
 *   site was built with.
 */

/**
 * Renders one docs page from its Markdown.
 *
 * @param {DocsShell} shell The baked page shell.
 * @param {string} source The doc's Markdown.
 * @param {string} key The doc's key (path without `.md`).
 * @param {{ref: string, base: string, keys?: Set<string> | null}} at The
 *   docs version: its ref, its URL prefix and the docs it holds (null when
 *   unknown).
 * @returns {{html: string, markdown: string}} The full HTML page and the
 *   Markdown copy with links rewritten for the site.
 */
export function renderDocPage(shell, source, key, at) {
  const blobBase = `${shell.repository}/blob/${at.ref}/`;
  const doc = renderDoc(source, key, { keys: at.keys ?? null, blobBase, base: at.base });
  const markdownUrl = docMarkdownUrl(key, at.base);
  const generated = key.startsWith("api/");
  const sourceLink = generated
    ? `Generated from the source at ${escapeHtml(at.ref)}`
    : `Source: <a href="${escapeHtml(`${blobBase}docs/${key}.md`)}">docs/${escapeHtml(key)}.md</a> at ${escapeHtml(at.ref)}`;
  const note =
    at.ref === "main"
      ? '<p class="doc-note" role="note">These docs describe the development version. They change with every merge and may cover features that are not finished.</p>'
      : "";
  const html = fillShell(
    shell.html,
    {
      pageTitle: `${doc.title} · Bookclerk docs`,
      pageDescription: doc.description || `${doc.title}: Bookclerk documentation.`,
      pagePath: docUrl(key, at.base),
      markdownUrl,
      docsBase: at.base,
    },
    {
      jsonld: jsonLd(docJsonLd(shell.origin, at.base, { key, title: doc.title, description: doc.description })),
      "doc-versions": versionsHtml(shell.versions, at.ref, key),
      "doc-nav": navHtml(shell.nav, key, at.base),
      "doc-note": note,
      "doc-body": doc.html,
      "doc-footer": `<p class="doc-source faint">${sourceLink} · <a href="${escapeHtml(markdownUrl)}" type="text/markdown">View as Markdown</a></p>`,
    },
  );
  return { html, markdown: doc.markdown };
}

/** GitHub URL of the docs repository the Worker reads; readers are sent there when it cannot. */
export const DOCS_REPOSITORY_URL = "https://github.com/Bookclerk/bookclerk-docs";

/**
 * Renders the page shown when a doc cannot be read from the docs repository.
 *
 * The page keeps the site's header and the version picker but leaves out the
 * sidebar, whose links would fail the same way (and on a phone would push the
 * message below the fold). It says the docs are unavailable for now and links
 * to the same doc on GitHub so the reader is not stuck.
 *
 * @param {DocsShell} shell The baked page shell.
 * @param {string} key The doc's most likely key (path without `.md`).
 * @param {{ref: string, base: string}} at The docs version: its ref and its
 *   URL prefix.
 * @returns {{html: string, markdown: string}} The full HTML page and its
 *   Markdown version.
 */
export function renderUnavailablePage(shell, key, at) {
  const github = `${DOCS_REPOSITORY_URL}/blob/${at.ref}/${key}.md`;
  const all = `${DOCS_REPOSITORY_URL}/tree/${at.ref}`;
  const body = [
    '<span class="eyebrow">Temporarily off the shelf</span>',
    "<h1>The docs could not be loaded</h1>",
    '<p class="muted">The site reads its documentation from GitHub, and that did not work just now. Try again in a minute, or read this page on GitHub in the meantime.</p>',
    '<p class="btn-row">',
    `<a class="btn btn-primary" href="${escapeHtml(github)}">Read this page on GitHub</a>`,
    `<a class="btn btn-secondary" href="${escapeHtml(docUrl(key, at.base))}">Try again</a>`,
    "</p>",
    `<p class="muted">You can also <a href="${escapeHtml(all)}">browse all the docs on GitHub</a>.</p>`,
  ].join("\n");
  const html = fillShell(
    shell.html,
    {
      pageTitle: "Docs unavailable · Bookclerk docs",
      pageDescription: "The Bookclerk documentation could not be loaded right now.",
      pagePath: docUrl(key, at.base),
      markdownUrl: docMarkdownUrl(key, at.base),
      docsBase: at.base,
    },
    {
      jsonld: "",
      "doc-versions": versionsHtml(shell.versions, at.ref, key),
      "doc-nav": "",
      "doc-note": "",
      "doc-body": body,
      "doc-footer": "",
    },
  );
  const markdown = [
    "# The docs could not be loaded",
    "",
    "The site reads its documentation from GitHub, and that did not work just now. Try again in a minute.",
    "",
    `Read this page on GitHub: <${github}>`,
    "",
    `Browse all the docs on GitHub: <${all}>`,
    "",
  ].join("\n");
  return { html, markdown };
}
