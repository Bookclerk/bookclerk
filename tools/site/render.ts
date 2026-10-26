// Renders one doc's Markdown into a site page body, with no file system or
// Node APIs, so the build (docs.ts) and the Worker (worker.ts) share it.
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
import type { MarkdownIt as Markdown, RendererRule, Token } from "markdown-it";

/** One sidebar group: its heading and the docs it lists, by key. */
export interface NavSpec {
  /** The heading readers see. */
  title: string;
  /** Docs paths without `.md`, in sidebar order. */
  pages: string[];
}

/** One sidebar group as the site renders it, holding only docs that exist. */
export interface NavGroup {
  /** The heading readers see. */
  title: string;
  /** Each doc's key (path without `.md`) and title, in sidebar order. */
  pages: { key: string; title: string }[];
}

/** One docs version the site serves. */
export interface DocsVersion {
  /** Its git ref in the docs repository (`main` or a release tag). */
  ref: string;
  /** The name readers see in the version picker. */
  label: string;
  /** Its URL prefix, ending in `/` (`/docs/` or `/docs/v/<ref>/`). */
  base: string;
}

/** A rendered doc: what the build and the Worker take from one Markdown file. */
export interface RenderedDoc {
  /** The doc's title (see docTitle). */
  title: string;
  /** Its summary for search results and previews (see describe); may be empty. */
  description: string;
  /** The HTML body. */
  html: string;
  /** The Markdown copy with links rewritten for the site (see siteMarkdown). */
  markdown: string;
}

/** A full page and its Markdown version. */
export interface RenderedPage {
  /** The full HTML page. */
  html: string;
  /** The Markdown version. */
  markdown: string;
}

/** Maps a doc's key to its site URL. */
export type UrlFor = (key: string) => string;

/**
 * Sidebar groups, in order. Each entry is a docs path without `.md`. A doc
 * that exists but is not listed lands in the final "More" group, so a new doc
 * is never left out of the site.
 */
export const NAV: NavSpec[] = [
  {
    title: "Use Bookclerk",
    pages: [
      "guide/what-is-bookclerk",
      "guide/getting-started",
      "guide/stores",
      "guide/where-books-go",
      "guide/discover",
      "guide/web-app",
      "guide/sharing",
      "guide/branches",
      "guide/from-libation",
      "guide/troubleshooting",
      "guide/glossary",
    ],
  },
  {
    title: "Run a server",
    pages: [
      "server/operations",
      "server/configuration",
      "server/security",
      "server/database",
      "server/stores",
      "server/output",
      "server/sign-in",
      "server/integrations",
      "server/plugins",
      "server/cluster-hosts",
      "server/media",
      "server/backups",
      "server/diagnostics",
    ],
  },
  {
    title: "Build plugins",
    pages: ["plugins/README", "plugins/manifest", "plugins/capabilities", "plugins/abi", "plugins/registry", "sql-contract/v1"],
  },
  {
    title: "API reference",
    pages: ["api/README", "api/typescript/README", "api/python/README", "api/rust/bookclerk_plugin_sdk/index", "api/rust/README"],
  },
  {
    title: "How it works",
    pages: ["internals/architecture", "internals/discovery", "internals/jobs", "internals/storage-bounds", "internals/guest-jail", "internals/workerd-bridge", "internals/node-api", "internals/windows-appcontainer"],
  },
  {
    title: "Contribute",
    pages: [
      "contributing/build-from-source",
      "contributing/devcontainer",
      "contributing/ui",
      "contributing/ci",
      "contributing/fuzzing",
      "contributing/automation",
      "contributing/packaging",
      "contributing/code-documentation",
      "contributing/research/source-candidates",
      "contributing/research/libation-parity",
    ],
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
      "adr/observability",
    ],
  },
];

/** Sidebar titles for pages that are generated, not in docs/. */
export const GENERATED_TITLES: Readonly<Record<string, string>> = {
  "api/typescript/README": "TypeScript plugin SDK",
  "api/python/README": "Python plugin SDK",
  "api/rust/bookclerk_plugin_sdk/index": "Rust plugin SDK",
  "api/rust/README": "Rust crates",
};

/**
 * Normalizes a POSIX path the way `path.posix.normalize` does.
 *
 * @param path - A relative path.
 * @returns The path without `.` segments and with `..` applied;
 *   leading `..` segments are kept, as is a trailing slash.
 */
export function normalizePath(path: string): string {
  const out: string[] = [];
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
 * @param key - Docs path without `.md`.
 * @returns `adr` for `adr/control-plane`, `""` for `plugins`.
 */
function keyDir(key: string): string {
  const at = key.lastIndexOf("/");
  return at === -1 ? "" : key.slice(0, at);
}

/**
 * The site URL of a doc.
 *
 * @param key - Docs path without `.md` (`plugins`, `adr/control-plane`).
 * @param base - URL prefix of the docs version, ending in `/`.
 * @returns `<base>` for the index, else `<base><key>`; a nested
 *   README or index maps to its directory (`api/README` → `<base>api/`).
 */
export function docUrl(key: string, base = "/docs/"): string {
  if (key === "README") return base;
  if (key.endsWith("/README")) return base + key.slice(0, -"README".length);
  return base + key;
}

/**
 * The site URL of a doc's Markdown copy.
 *
 * @param key - Docs path without `.md`.
 * @param base - URL prefix of the docs version, ending in `/`.
 * @returns `<base><key>.md`; the index is `<base>README.md`.
 */
export function docMarkdownUrl(key: string, base = "/docs/"): string {
  return `${base}${key}.md`;
}

/**
 * Turns heading text into an anchor id the way GitHub does, so existing
 * `#section` links in the docs keep working.
 *
 * @param text - Heading text content.
 * @returns Lowercase id: letters, digits, `-` and `_`.
 */
export function slugify(text: string): string {
  return text
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");
}

/**
 * Rewrites a link found in a doc.
 *
 * @param href - The link as written in the Markdown.
 * @param docKey - The doc the link appears in (path without `.md`).
 * @param keys - Every doc on the site, or null when any
 *   `.md` file inside the docs tree counts as a doc.
 * @param blobBase - GitHub URL prefix for files at the site's ref,
 *   ending in `/blob/<ref>/`.
 * @param urlFor - Site URL of another doc:
 *   {@link docUrl} for pages, {@link docMarkdownUrl} for Markdown copies.
 * @returns A site URL for another doc, a GitHub URL for any other
 *   repository file, or the link unchanged when it is absolute or an anchor.
 */
export function rewriteLink(href: string, docKey: string, keys: ReadonlySet<string> | null, blobBase: string, urlFor: UrlFor = docUrl): string {
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
 * @param docKey - The doc being rendered.
 * @param keys - See {@link rewriteLink}.
 * @param blobBase - See {@link rewriteLink}.
 * @param urlFor - Site URL of another doc's page.
 * @returns A renderer with raw HTML off, GitHub-style heading
 *   ids, rewritten links, and code blocks and tables that keyboard users
 *   can scroll sideways.
 */
function renderer(docKey: string, keys: ReadonlySet<string> | null, blobBase: string, urlFor: UrlFor): Markdown {
  const md = new MarkdownIt({ html: false, linkify: false, typographer: false });
  const seen = new Map<string, number>();
  md.core.ruler.push("heading_ids", (state) => {
    const tokens = state.tokens;
    tokens.forEach((token, i) => {
      if (token.type !== "heading_open") return;
      const children = tokens[i + 1]?.children ?? [];
      const text = children.filter((t) => t.type === "text" || t.type === "code_inline").map((t) => t.content).join("");
      const base = slugify(text);
      const n = seen.get(base) ?? 0;
      seen.set(base, n + 1);
      token.attrSet("id", n === 0 ? base : `${base}-${n}`);
    });
  });
  const linkOpen: RendererRule = md.renderer.rules["link_open"] ?? ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
  md.renderer.rules["link_open"] = (tokens, idx, options, env, self) => {
    const token = tokens[idx];
    if (token === undefined) return linkOpen(tokens, idx, options, env, self);
    token.attrSet("href", rewriteLink(String(token.attrGet("href") ?? ""), docKey, keys, blobBase, urlFor));
    return linkOpen(tokens, idx, options, env, self);
  };
  // Code blocks and wide tables scroll sideways on narrow screens; tabindex
  // lets keyboard users scroll them too.
  for (const rule of ["fence", "code_block"] as const) {
    const base = md.renderer.rules[rule];
    if (base === undefined) throw new Error(`markdown-it has no ${rule} rule`);
    md.renderer.rules[rule] = (tokens, idx, options, env, self) => base(tokens, idx, options, env, self).replace(/^<pre/, '<pre tabindex="0"');
  }
  md.renderer.rules["table_open"] = () => '<div class="table-scroll" tabindex="0"><table>\n';
  md.renderer.rules["table_close"] = () => "</table></div>\n";
  return md;
}

/** Longest page description, in characters, before it is shortened. */
const DESCRIPTION_MAX = 160;

/**
 * Plain text of an inline token's children.
 *
 * @param inline - An `inline` token.
 * @returns Its text and code spans, whitespace collapsed.
 */
function inlineText(inline: Token | undefined): string {
  return (inline?.children ?? [])
    .filter((t) => t.type === "text" || t.type === "code_inline" || t.type === "softbreak")
    .map((t) => (t.type === "softbreak" ? " " : t.content))
    .join("")
    .replace(/\s+/g, " ")
    .trim();
}

/**
 * Cuts text at a word boundary.
 *
 * @param text - Plain text.
 * @returns The text, or at most DESCRIPTION_MAX characters of it
 *   ending in an ellipsis.
 */
function shorten(text: string): string {
  if (text.length <= DESCRIPTION_MAX) return text;
  const cut = text.slice(0, DESCRIPTION_MAX - 1);
  return `${cut.slice(0, cut.lastIndexOf(" ")).replace(/[\s,;:.]+$/, "")}…`;
}

/**
 * Summarizes a doc for search results and link previews.
 *
 * @param tokens - The doc's parsed tokens.
 * @returns The doc's introduction (its first top-level paragraph,
 *   when that comes before the first section heading), else a list of its
 *   sections; shortened (see shorten). Empty when it has neither.
 */
function describe(tokens: Token[]): string {
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
 * @param source - The doc's Markdown.
 * @param docKey - The doc's key.
 * @param keys - See {@link rewriteLink}.
 * @param blobBase - See {@link rewriteLink}.
 * @param base - URL prefix of the docs version, ending in `/`.
 * @returns The Markdown with doc links pointing at the other
 *   Markdown copies and repository links pointing at GitHub.
 */
export function siteMarkdown(source: string, docKey: string, keys: ReadonlySet<string> | null, blobBase: string, base = "/docs/"): string {
  const urlFor: UrlFor = (key) => docMarkdownUrl(key, base);
  const rewrite = (text: string): string =>
    text
      .replace(INLINE_LINK_RE, (_, href: string, title: string) => `](${rewriteLink(href, docKey, keys, blobBase, urlFor)}${title})`)
      .replace(REFERENCE_DEF_RE, (_, head: string, href: string) => head + rewriteLink(href, docKey, keys, blobBase, urlFor));
  // Odd-numbered pieces of a split on backtick runs are inline code.
  const outsideCode = (line: string): string =>
    line
      .split(/(`+[^`]*`+)/)
      .map((piece, i) => (i % 2 === 1 ? piece : rewrite(piece)))
      .join("");
  let fence: string | null = null;
  return source
    .split("\n")
    .map((line) => {
      const run = line.match(/^ {0,3}(`{3,}|~{3,})/)?.[1];
      if (run !== undefined && (fence === null || run.startsWith(fence))) {
        fence = fence === null ? run : null;
        return line;
      }
      return fence === null ? outsideCode(line) : line;
    })
    .join("\n");
}

/**
 * A doc's title: its first `#` heading without code marks.
 *
 * @param source - The doc's Markdown.
 * @param key - Fallback: the doc's key.
 * @returns The title.
 */
export function docTitle(source: string, key: string): string {
  const heading = source.match(/^#\s+(.+?)\s*#*\s*$/m)?.[1];
  return heading === undefined ? key : heading.replace(/`/g, "");
}

/**
 * Renders one doc.
 *
 * @param source - The doc's Markdown.
 * @param key - The doc's key (path without `.md`).
 * @param options - Docs that exist (null: any `.md` in the tree), the GitHub prefix for
 *   repository files, and the URL prefix of the docs version.
 * @returns The title (see docTitle), summary (see describe), HTML body and Markdown
 *   copy (see siteMarkdown).
 */
export function renderDoc(
  source: string,
  key: string,
  { keys = null, blobBase, base = "/docs/" }: { keys?: ReadonlySet<string> | null; blobBase: string; base?: string },
): RenderedDoc {
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
 * @param value - Raw text.
 * @returns The text with `& < > " '` replaced by entities.
 */
export function escapeHtml(value: string): string {
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
 * @param data - A schema.org JSON-LD object, or null for none.
 * @returns The script element, with `<` escaped so the JSON can
 *   never close it; empty when `data` is null.
 */
export function jsonLd(data: object | null): string {
  if (data === null) return "";
  return `<script type="application/ld+json">${JSON.stringify(data).replaceAll("<", "\\u003c")}</script>`;
}

/**
 * The schema.org description of one doc page.
 *
 * @param origin - The site's origin.
 * @param base - URL prefix of the docs version, ending in `/`.
 * @param doc - The doc.
 * @returns A JSON-LD `@graph` with the TechArticle and its
 *   breadcrumb trail (Docs › title).
 */
export function docJsonLd(origin: string, base: string, doc: { key: string; title: string; description: string }): object {
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
 * @param titles - Title of every doc by key.
 * @returns NAV without missing docs, plus a "More" group for docs NAV leaves out.
 */
export function buildNav(titles: ReadonlyMap<string, string>): NavGroup[] {
  const listed = new Set(NAV.flatMap((g) => g.pages));
  /** The docs among `keys` that exist, with their titles. */
  const pages = (keys: string[]): NavGroup["pages"] =>
    keys.flatMap((key) => {
      const title = titles.get(key);
      return title === undefined ? [] : [{ key, title }];
    });
  const nav = NAV.map((g) => ({ title: g.title, pages: pages(g.pages) })).filter((g) => g.pages.length > 0);
  const extra = [...titles.keys()].filter((k) => k !== "README" && !listed.has(k) && !k.startsWith("api/"));
  if (extra.length > 0) nav.push({ title: "More", pages: pages(extra) });
  return nav;
}

/**
 * Renders the sidebar for one page.
 *
 * @param nav - The sidebar groups.
 * @param current - Key of the page being shown.
 * @param base - URL prefix of the docs version, ending in `/`.
 * @returns One heading and list per group.
 */
export function navHtml(nav: readonly NavGroup[], current: string, base: string): string {
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
 * @param versions - Every docs version, the default first.
 * @param current - The ref being shown.
 * @param key - Key of the page being shown; the picker links to the
 *   same page in each version.
 * @returns A `<details>` element listing the versions.
 */
export function versionsHtml(versions: readonly DocsVersion[], current: string, key: string): string {
  const shown = versions.find((v) => v.ref === current) ?? versions[0];
  if (shown === undefined) return "";
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
 * Fills the docs page shell that build.ts writes.
 *
 * Values are HTML-escaped; markers take HTML the caller built (rendered
 * Markdown, which has raw HTML off, and the helpers above). Values are filled
 * before markers, and markers in one pass, so nothing inserted is ever read
 * as a hole or a marker.
 *
 * @param shell - The shell's HTML.
 * @param values - One entry per SHELL_VALUES name.
 * @param markers - One entry per SHELL_MARKERS name.
 * @returns The page.
 * @throws {Error} When a hole has no value or a marker is missing.
 */
export function fillShell(shell: string, values: Readonly<Record<string, string>>, markers: Readonly<Record<string, string>>): string {
  const out = shell.replace(SHELL_VALUE_RE, (_, name: string) => {
    const value = Object.hasOwn(values, name) ? values[name] : undefined;
    if (value === undefined) throw new Error(`docs shell hole ${name} has no value`);
    return escapeHtml(value);
  });
  for (const name of SHELL_MARKERS) {
    if (!out.includes(`<!--${name}-->`)) throw new Error(`docs shell has no <!--${name}-->`);
  }
  // One pass, so HTML put in for one marker is never searched for another.
  const markerRe = new RegExp(`<!--(${SHELL_MARKERS.join("|")})-->`, "g");
  return out.replace(markerRe, (_, name: string) => markers[name] ?? "");
}

/** Everything build.ts bakes into the Worker for docs pages. */
export interface DocsShell {
  /** A hash of the rest of the shell, so caches keyed by it drop pages from an earlier deploy. */
  id: string;
  /** src/doc.html rendered for the site, with SHELL_VALUES holes and SHELL_MARKERS markers left to fill. */
  html: string;
  /** The site's origin. */
  origin: string;
  /** GitHub URL of the source repository. */
  repository: string;
  /** Every docs version the site serves, the default (served at `/docs/`) first. */
  versions: DocsVersion[];
  /** The sidebar, from the docs the site was built with. */
  nav: NavGroup[];
}

/**
 * Renders one docs page from its Markdown.
 *
 * @param shell - The baked page shell.
 * @param source - The doc's Markdown.
 * @param key - The doc's key (path without `.md`).
 * @param at - The docs version: its ref, its URL prefix and the docs it
 *   holds (null when unknown).
 * @returns The full HTML page and the Markdown copy with links rewritten for
 *   the site.
 */
export function renderDocPage(shell: DocsShell, source: string, key: string, at: { ref: string; base: string; keys?: ReadonlySet<string> | null }): RenderedPage {
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
 * @param shell - The baked page shell.
 * @param key - The doc's most likely key (path without `.md`), for
 *   the page's own links.
 * @param at - The docs version: its ref and its URL prefix, and the key the
 *   GitHub link names (default: `key`).
 * @returns The full HTML page and its Markdown version.
 */
export function renderUnavailablePage(shell: DocsShell, key: string, at: { ref: string; base: string; githubKey?: string }): RenderedPage {
  const github = `${DOCS_REPOSITORY_URL}/blob/${at.ref}/${at.githubKey ?? key}.md`;
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
