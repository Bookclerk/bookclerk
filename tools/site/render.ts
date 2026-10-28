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

import {
  API_INDEX,
  API_SCHEMAS,
  indexRoutes,
  isApiOperationsPage,
  layoutIndex,
  layoutOperations,
  layoutProperties,
  methodClass,
  operationMeta,
  routeTree,
} from "./api-reference.ts";
import type { ApiRouteGroup, HttpMethod } from "./api-reference.ts";
import { diagram, drawable, PAGE_DIAGRAM_COUNT_MAX, PAGE_DIAGRAM_MAX } from "./diagram.ts";
import { highlight } from "./highlight.ts";
import { formatDate } from "./last-updated.ts";
import type { IsoDate } from "./last-updated.ts";

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

/** A section heading of a doc, as the "On this page" list shows it. */
export interface DocHeading {
  /** 2 for a section (`##`), 3 for a subsection (`###`). */
  level: 2 | 3;
  /** The heading's element id, the fragment that links to it. */
  id: string;
  /** Its plain text. */
  text: string;
  /** For a Web API operation heading: its method (the text is its path). */
  method?: HttpMethod;
}

/** A rendered doc: what the build and the Worker take from one Markdown file. */
export interface RenderedDoc {
  /** The doc's title (see docTitle). */
  title: string;
  /** Its summary for search results and previews (see describe); may be empty. */
  description: string;
  /** The HTML body. */
  html: string;
  /** Its `##` and `###` headings in order, for the "On this page" list. */
  headings: DocHeading[];
  /** The Markdown copy with links rewritten for the site (see siteMarkdown). */
  markdown: string;
  /** The route groups the Web API index lists, links rewritten for the site; empty on any other page. */
  routes: ApiRouteGroup[];
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
      "server/cli-reference",
    ],
  },
  {
    title: "Build plugins",
    pages: [
      "plugins/README",
      "plugins/manifest",
      "plugins/capabilities",
      "plugins/abi",
      "plugins/registry",
      "sql-contract/v1",
    ],
  },
  {
    title: "API reference",
    pages: [
      "api/README",
      "api/typescript/README",
      "api/python/README",
      "api/rust/bookclerk_plugin_sdk/index",
      "api/web/README",
      "api/rust/README",
    ],
  },
  {
    title: "How it works",
    pages: [
      "internals/architecture",
      "internals/discovery",
      "internals/jobs",
      "internals/storage-bounds",
      "internals/guest-jail",
      "internals/workerd-bridge",
      "internals/node-api",
      "internals/windows-appcontainer",
    ],
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
      "contributing/writing-docs",
      "contributing/licensing",
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
      "adr/licensing",
    ],
  },
];

/** Sidebar titles for pages that are generated, not in docs/. */
export const GENERATED_TITLES: Readonly<Record<string, string>> = {
  "api/typescript/README": "TypeScript plugin SDK",
  "api/python/README": "Python plugin SDK",
  "api/rust/bookclerk_plugin_sdk/index": "Rust plugin SDK",
  "api/web/README": "Web API",
  "api/rust/README": "Rust crates",
};

/**
 * Repository files outside docs/ that the site renders as pages of their
 * own (build.ts `PAGES`): repository path -> page path. A doc linking to one
 * links to the page, or to its Markdown copy (`<page>.md`) from Markdown.
 */
export const SITE_PAGES: ReadonlyMap<string, string> = new Map([["SECURITY.md", "/security"]]);

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
 * @returns `<base><key>`, with no trailing slash: the index is `<base>`
 *   without its slash (`/docs`), and a nested README maps to its directory
 *   (`api/README` → `<base>api`).
 */
export function docUrl(key: string, base = "/docs/"): string {
  if (key === "README") return base.slice(0, -1);
  if (key.endsWith("/README")) return base + key.slice(0, -"/README".length);
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
 * @returns A site URL for another doc or for a repository file the site
 *   renders (see {@link SITE_PAGES}), a GitHub URL for any other repository
 *   file, or the link unchanged when it is absolute or an anchor.
 */
export function rewriteLink(
  href: string,
  docKey: string,
  keys: ReadonlySet<string> | null,
  blobBase: string,
  urlFor: UrlFor = docUrl,
): string {
  if (
    href === "" ||
    href.startsWith("#") ||
    /^[a-z][a-z0-9+.-]*:/i.test(href) ||
    href.startsWith("//")
  ) {
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
  const page = SITE_PAGES.get(target);
  // Markdown copies link to Markdown copies; urlFor tells which this is.
  if (page !== undefined) return (urlFor("README").endsWith(".md") ? `${page}.md` : page) + hash;
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
function renderer(
  docKey: string,
  keys: ReadonlySet<string> | null,
  blobBase: string,
  urlFor: UrlFor,
): Markdown {
  const md = new MarkdownIt({ html: false, linkify: false, typographer: false });
  const seen = new Map<string, number>();
  md.core.ruler.push("heading_ids", (state) => {
    const tokens = state.tokens;
    tokens.forEach((token, i) => {
      if (token.type !== "heading_open") return;
      const children = tokens[i + 1]?.children ?? [];
      const text = children
        .filter((t) => t.type === "text" || t.type === "code_inline")
        .map((t) => t.content)
        .join("");
      const base = slugify(text);
      const n = seen.get(base) ?? 0;
      seen.set(base, n + 1);
      token.attrSet("id", n === 0 ? base : `${base}-${n}`);
    });
  });
  md.core.ruler.after("block", "callouts", (state) => markCallouts(state.tokens));
  if (isApiOperationsPage(docKey)) {
    md.core.ruler.after("heading_ids", "api_operations", (state) => {
      state.tokens = layoutOperations(
        state.tokens,
        (type, tag, nesting) => new state.Token(type, tag, nesting),
      );
    });
  }
  if (docKey.startsWith("api/web/")) {
    md.core.ruler.after("heading_ids", "api_properties", (state) => {
      state.tokens = layoutProperties(
        state.tokens,
        (type, tag, nesting) => new state.Token(type, tag, nesting),
      );
    });
  }
  if (docKey === API_INDEX) {
    md.core.ruler.after("heading_ids", "api_index", (state) => {
      state.tokens = layoutIndex(
        state.tokens,
        (type, tag, nesting) => new state.Token(type, tag, nesting),
      );
    });
  }
  for (const rule of ["blockquote_open", "blockquote_close"] as const) {
    const base: RendererRule =
      md.renderer.rules[rule] ??
      ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
    md.renderer.rules[rule] = (tokens, idx, options, env, self) => {
      const kind = calloutKind(tokens[idx]);
      if (kind === null) return base(tokens, idx, options, env, self);
      return rule === "blockquote_close"
        ? "</div>\n"
        : `<div class="callout callout-${kind}" role="note"><p class="callout-title">${CALLOUT_TITLES[kind]}</p>\n`;
    };
  }
  const linkOpen: RendererRule =
    md.renderer.rules["link_open"] ??
    ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
  md.renderer.rules["link_open"] = (tokens, idx, options, env, self) => {
    const token = tokens[idx];
    if (token === undefined) return linkOpen(tokens, idx, options, env, self);
    token.attrSet(
      "href",
      rewriteLink(String(token.attrGet("href") ?? ""), docKey, keys, blobBase, urlFor),
    );
    return linkOpen(tokens, idx, options, env, self);
  };
  // Code blocks and wide tables scroll sideways on narrow screens; tabindex
  // lets keyboard users scroll them too. Each code block sits in a
  // `div.code`, which holds the copy button docs.js adds; a fence in a
  // language highlight.ts knows gets syntax colors and a `data-lang` label.
  // A `mermaid` fence is drawn as a diagram (drawDiagram) when it can be,
  // and is otherwise shown as code.
  for (const rule of ["fence", "code_block"] as const) {
    const base = md.renderer.rules[rule];
    if (base === undefined) throw new Error(`markdown-it has no ${rule} rule`);
    md.renderer.rules[rule] = (tokens, idx, options, env, self) => {
      const token = tokens[idx];
      const lang = rule === "fence" ? (token?.info.trim().split(/\s+/)[0] ?? "") : "";
      if (token !== undefined && lang.toLowerCase() === "mermaid") {
        const drawn = drawDiagram(token.content, idx, (env ?? {}) as RenderEnv);
        if (drawn !== null) return drawn;
      }
      const colored = token === undefined || lang === "" ? null : highlight(token.content, lang);
      if (colored === null)
        return `<div class="code">${base(tokens, idx, options, env, self).replace(/^<pre/, '<pre tabindex="0"')}</div>\n`;
      const name = escapeHtml(lang.toLowerCase());
      return `<div class="code" data-lang="${name}"><pre tabindex="0"><code class="language-${name}">${colored}</code></pre></div>\n`;
    };
  }
  md.renderer.rules["table_open"] = () => '<div class="table-scroll" tabindex="0"><table>\n';
  md.renderer.rules["table_close"] = () => "</table></div>\n";
  // Every section heading below the title carries a link to itself, shown
  // beside it on hover or focus (docs.js also copies the link). The link
  // sits after the heading in a wrapper, so it is not part of the
  // heading's name.
  md.renderer.rules["heading_open"] = (tokens, idx, options, _env, self) => {
    const token = tokens[idx];
    const open = self.renderToken(tokens, idx, options);
    return token === undefined || !anchored(token)
      ? open
      : `<div class="heading-wrapper level-${token.tag}">${open}`;
  };
  md.renderer.rules["heading_close"] = (tokens, idx, options, _env, self) => {
    const close = self.renderToken(tokens, idx, options);
    // A heading is always heading_open, inline, heading_close.
    const open = tokens[idx - 2];
    if (open === undefined || open.type !== "heading_open" || !anchored(open)) return close;
    const id = escapeHtml(String(open.attrGet("id")));
    const name = escapeHtml(inlineText(tokens[idx - 1]));
    return `${close.trimEnd()}<a class="heading-anchor" href="#${id}" aria-label="Link to section: ${name}">${LINK_ICON}</a></div>\n`;
  };
  return md;
}

/** The chain-link icon of a heading's link to itself; drawn here, not taken from an icon set. */
const LINK_ICON =
  '<svg aria-hidden="true" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7.07 0l3-3a5 5 0 0 0-7.07-7.07l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7.07 0l-3 3a5 5 0 0 0 7.07 7.07l1.5-1.5"/></svg>';

/**
 * Whether a heading gets a link to itself: every heading below the title
 * that has an id.
 *
 * @param token - A `heading_open` token.
 * @returns True for `h2` to `h6` with an id.
 */
function anchored(token: Token): boolean {
  return (
    token.tag !== "h1" && typeof token.attrGet("id") === "string" && token.attrGet("id") !== ""
  );
}

/** Per-render state markdown-it passes to every rule: the diagrams drawn so far. */
interface RenderEnv {
  /** Characters of Mermaid source this page has laid out. */
  diagramSource?: number;
  /** Diagrams this page has laid out. */
  diagramCount?: number;
}

/**
 * Renders a `mermaid` fence as a diagram, within the page's budget: at most
 * PAGE_DIAGRAM_COUNT_MAX diagrams and PAGE_DIAGRAM_MAX characters of
 * source, counting only fences that are laid out.
 *
 * The figure scrolls sideways like a code block: a wide diagram shrinks to
 * the column only down to 70% of its natural width (`--w`), so its labels
 * stay readable on a phone. The ids get a capitalized prefix, which no
 * heading slug can match. The figure is named for its kind, so keyboard and
 * screen-reader users know what they reached; the labels inside stay text.
 *
 * @param source - The fence's content.
 * @param idx - The fence's token index, unique on the page.
 * @param env - markdown-it's per-render state, holding the page's budget.
 * @returns The figure, or null to show the fence as code.
 */
function drawDiagram(source: string, idx: number, env: RenderEnv): string | null {
  if (!drawable(source)) return null;
  const used = (env.diagramSource ?? 0) + source.length;
  const count = (env.diagramCount ?? 0) + 1;
  if (used > PAGE_DIAGRAM_MAX || count > PAGE_DIAGRAM_COUNT_MAX) return null;
  // Charged before drawing: a fence that fails layout or cleaning has still
  // cost the layout's CPU, which is what the budget limits.
  env.diagramSource = used;
  env.diagramCount = count;
  const drawn = diagram(source, `Diagram${idx}`);
  if (drawn === null) return null;
  return `<figure class="diagram" tabindex="0" role="group" aria-label="${escapeHtml(drawn.label)}" style="--w: ${Math.ceil(drawn.width)}px">${drawn.svg}</figure>\n`;
}

/** The callout kinds a doc can use, as GitHub's alert syntax names them. */
type CalloutKind = "note" | "tip" | "important" | "warning" | "caution";

/** The title each callout shows above its text. */
const CALLOUT_TITLES: Readonly<Record<CalloutKind, string>> = {
  note: "Note",
  tip: "Tip",
  important: "Important",
  warning: "Warning",
  caution: "Caution",
};

/**
 * A callout marker alone on a blockquote's first line, as on GitHub:
 * `> [!NOTE]`. The kind is matched without regard to case.
 */
const CALLOUT_MARKER_RE = /^\[!(note|tip|important|warning|caution)\][ \t]*(?:\n|$)/i;

/**
 * Turns blockquotes that open with a callout marker into callouts.
 *
 * Runs on block tokens, before inline parsing: the marker line is cut from
 * the first paragraph (dropping the paragraph when nothing else is in it),
 * and the blockquote's open and close tokens are tagged with the kind for
 * the renderer. Any other blockquote is left alone.
 *
 * @param tokens - The doc's block tokens, changed in place.
 */
function markCallouts(tokens: Token[]): void {
  for (let i = 0; i < tokens.length; i++) {
    const open = tokens[i];
    const inline = tokens[i + 2];
    if (
      open?.type !== "blockquote_open" ||
      tokens[i + 1]?.type !== "paragraph_open" ||
      inline?.type !== "inline"
    )
      continue;
    const marker = CALLOUT_MARKER_RE.exec(inline.content);
    const kind = marker?.[1]?.toLowerCase();
    if (marker === null || !isCalloutKind(kind)) continue;
    const close = tokens.findIndex(
      (t, j) => j > i && t.type === "blockquote_close" && t.level === open.level,
    );
    if (close === -1) continue;
    open.meta = { callout: kind };
    (tokens[close] as Token).meta = { callout: kind };
    inline.content = inline.content.slice(marker[0].length);
    if (inline.content.trim() === "") tokens.splice(i + 1, 3);
  }
}

/**
 * Tells whether a string names a callout kind.
 *
 * @param kind - The candidate, lower case.
 * @returns True for a kind in CALLOUT_TITLES.
 */
function isCalloutKind(kind: string | undefined): kind is CalloutKind {
  return kind !== undefined && Object.hasOwn(CALLOUT_TITLES, kind);
}

/**
 * The callout kind markCallouts tagged a blockquote token with.
 *
 * @param token - A `blockquote_open` or `blockquote_close` token.
 * @returns The kind, or null for a plain blockquote.
 */
function calloutKind(token: Token | undefined): CalloutKind | null {
  const kind: unknown = (token?.meta as { callout?: unknown } | null | undefined)?.callout;
  return typeof kind === "string" && isCalloutKind(kind) ? kind : null;
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
 * @param max - Longest result, in characters, ellipsis included.
 * @returns The text, or at most `max` UTF-16 code units of it ending in
 *   an ellipsis; cut mid-word only when the first word alone is too long,
 *   and never inside a surrogate pair.
 */
function shorten(text: string, max: number = DESCRIPTION_MAX): string {
  if (text.length <= max) return text;
  const cut = text.slice(0, max - 1).replace(/[\uD800-\uDBFF]$/, "");
  const space = cut.lastIndexOf(" ");
  return `${(space > 0 ? cut.slice(0, space) : cut).replace(/[\s,;:.]+$/, "")}…`;
}

/** Longest page `<title>`, in characters: search engines cut longer titles in their results, and Bing reports them. */
export const TITLE_MAX = 70;

/** What a docs page's `<title>` adds after the doc's own title. */
const DOC_TITLE_SUFFIX = " · Bookclerk docs";

/**
 * The `<title>` of a docs page.
 *
 * The doc's title followed by the site's name, or, when that is longer than
 * TITLE_MAX, the doc's title alone, shortened if it is still too long. A
 * title with no spaces (an API path such as `crate::module::item`) keeps
 * its end, which names the item, and drops leading path segments; prose
 * keeps its start (see shorten). The page's heading always shows the
 * whole title.
 *
 * @param title - The doc's title, plain text.
 * @returns At most TITLE_MAX characters.
 */
export function docPageTitle(title: string): string {
  const full = `${title}${DOC_TITLE_SUFFIX}`;
  if (full.length <= TITLE_MAX) return full;
  if (title.length <= TITLE_MAX) return title;
  if (/\s/.test(title)) return shorten(title, TITLE_MAX);
  const tail = title.slice(title.length - (TITLE_MAX - 1)).replace(/^[\uDC00-\uDFFF]/, "");
  const boundary = tail.search(/(?<=::|[./])[^:./]/);
  // Starting at a segment is tidier, but not when it would leave only a
  // short last segment that no longer identifies the page.
  return `…${boundary > 0 && tail.length - boundary >= TITLE_MAX / 2 ? tail.slice(boundary) : tail}`;
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
  if (intro !== -1 && (firstSection === -1 || intro < firstSection))
    return shorten(inlineText(tokens[intro + 1]));
  const sections = tokens.flatMap((t, i) =>
    t.type === "heading_open" && t.tag === "h2" ? [inlineText(tokens[i + 1])] : [],
  );
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
export function siteMarkdown(
  source: string,
  docKey: string,
  keys: ReadonlySet<string> | null,
  blobBase: string,
  base = "/docs/",
): string {
  const urlFor: UrlFor = (key) => docMarkdownUrl(key, base);
  const rewrite = (text: string): string =>
    text
      .replace(
        INLINE_LINK_RE,
        (_, href: string, title: string) =>
          `](${rewriteLink(href, docKey, keys, blobBase, urlFor)}${title})`,
      )
      .replace(
        REFERENCE_DEF_RE,
        (_, head: string, href: string) => head + rewriteLink(href, docKey, keys, blobBase, urlFor),
      );
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

/** The name of a docs version's search index, under its URL prefix. */
export const SEARCH_INDEX = "search-index.json";

/**
 * Where a docs version's search index is served (see worker.ts).
 *
 * @param base - URL prefix of the docs version, ending in `/`.
 * @returns The URL path.
 */
export function searchIndexUrl(base: string): string {
  return `${base}${SEARCH_INDEX}`;
}

/**
 * Reads a doc's `##` and `###` headings from its parsed tokens. A Web API
 * page lists its operations, not their subsections (Parameters,
 * Responses, …), which repeat on every one.
 *
 * @param tokens - The doc's block tokens, after heading ids are set.
 * @returns The headings in order.
 */
function headingsOf(tokens: readonly Token[]): DocHeading[] {
  let inOperation = false;
  return tokens.flatMap((t, i): DocHeading[] => {
    if (t.type !== "heading_open" || (t.tag !== "h2" && t.tag !== "h3")) return [];
    const id = String(t.attrGet("id") ?? "");
    const op = operationMeta(t);
    if (t.tag === "h2") inOperation = op !== null;
    if (op !== null) return [{ level: 2, id, text: op.path, method: op.method }];
    if (inOperation) return [];
    return [{ level: t.tag === "h2" ? 2 : 3, id, text: inlineText(tokens[i + 1]) }];
  });
}

/** A heading the sidebar filter can find, and where it links. */
export interface SearchHeading {
  /** Its text; an operation's is its method and path (`POST /api/jobs/{id}/cancel`). */
  text: string;
  /** Its URL path with the fragment. */
  url: string;
}

/** One page the sidebar filter can find. */
export interface SearchPage {
  /** What it sits under: its sidebar group, or `Web API` for a route group page. */
  section: string;
  /** Its title. */
  title: string;
  /** Its URL path. */
  url: string;
  /** Its headings, in order. */
  headings: SearchHeading[];
}

/**
 * Reads what the sidebar filter needs from one doc: its headings, linked
 * at the docs version. Only parses the Markdown, so it costs much less
 * than {@link renderDoc}.
 *
 * @param source - The doc's Markdown.
 * @param key - The doc's key.
 * @param section - Its sidebar group's title.
 * @param title - Its title as the sidebar shows it.
 * @param base - URL prefix of the docs version.
 * @returns The page's entry.
 */
export function searchPage(
  source: string,
  key: string,
  section: string,
  title: string,
  base: string,
): SearchPage {
  const url = docUrl(key, base);
  const tokens = renderer(key, null, "", (k) => docUrl(k, base)).parse(source, {});
  const headings = headingsOf(tokens).map((h) => ({
    text: h.method === undefined ? h.text : `${h.method} ${h.text}`,
    url: `${url}#${h.id}`,
  }));
  return { section, title, url, headings };
}

/**
 * The entries of the Web API's route group pages, read from its index:
 * each group page with its routes as headings, so a filter for `cancel`
 * finds Web API / Jobs without fetching every group page.
 *
 * @param source - The Web API index's Markdown.
 * @param base - URL prefix of the docs version.
 * @returns One entry per route group.
 */
export function apiSearchPages(source: string, base: string): SearchPage[] {
  return apiRouteGroups(source, { blobBase: "", base }).map((group) => ({
    section: "Web API",
    title: group.title,
    url: group.href,
    headings: group.routes.map((route) => ({
      text: `${route.method} ${route.path}`,
      url: route.href,
    })),
  }));
}

/**
 * Renders one doc.
 *
 * @param source - The doc's Markdown.
 * @param key - The doc's key (path without `.md`).
 * @param options - Docs that exist (null: any `.md` in the tree), the GitHub prefix for
 *   repository files, and the URL prefix of the docs version.
 * @returns The title (see docTitle), summary (see describe), HTML body,
 *   section headings and Markdown copy (see siteMarkdown).
 */
export function renderDoc(
  source: string,
  key: string,
  {
    keys = null,
    blobBase,
    base = "/docs/",
  }: { keys?: ReadonlySet<string> | null; blobBase: string; base?: string },
): RenderedDoc {
  const md = renderer(key, keys, blobBase, (k) => docUrl(k, base));
  const tokens = md.parse(source, {});
  const headings = headingsOf(tokens);
  // Read before rendering, which rewrites the links in place.
  const routes = routeLinks(indexRoutes(tokens), keys, blobBase, base);
  return {
    title: docTitle(source, key),
    description: describe(tokens),
    html: md.renderer.render(tokens, md.options, {}),
    headings,
    markdown: siteMarkdown(source, key, keys, blobBase, base),
    routes,
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
 * @param doc - The doc, with the date it last changed when known
 *   (`dateModified`).
 * @param trail - The page's breadcrumb trail (see crumbTrail); Docs alone
 *   when not given.
 * @returns A JSON-LD `@graph` with the TechArticle and its
 *   breadcrumb trail, the same as the page shows (the docs home has Docs only).
 */
export function docJsonLd(
  origin: string,
  base: string,
  doc: { key: string; title: string; description: string; updated?: IsoDate | null },
  trail: readonly Crumb[] = [
    { title: "Docs", key: null },
    { title: doc.title, key: doc.key },
  ],
): object {
  const url = origin + docUrl(doc.key, base);
  const crumbs = (doc.key === "README" ? trail.slice(0, 1) : trail).map((c) => ({
    name: c.title,
    item: origin + docUrl(c.key ?? "README", base),
  }));
  return {
    "@context": "https://schema.org",
    "@graph": [
      {
        "@type": "TechArticle",
        headline: doc.title,
        ...(doc.description ? { description: doc.description } : {}),
        ...(doc.updated ? { dateModified: doc.updated } : {}),
        url,
        inLanguage: "en",
        isPartOf: { "@id": `${origin}/#website` },
      },
      {
        "@type": "BreadcrumbList",
        itemListElement: crumbs.map((c, i) => ({
          "@type": "ListItem",
          position: i + 1,
          name: c.name,
          item: c.item,
        })),
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
  const nav = NAV.map((g) => ({ title: g.title, pages: pages(g.pages) })).filter(
    (g) => g.pages.length > 0,
  );
  const extra = [...titles.keys()].filter(
    (k) => k !== "README" && !listed.has(k) && !k.startsWith("api/"),
  );
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
export function navHtml(
  nav: readonly NavGroup[],
  current: string,
  base: string,
  nested: NavNested | null = null,
): string {
  return nav
    .map((group) => {
      const links = group.pages
        .map((p) => {
          const mark = p.key === current ? ' aria-current="page"' : "";
          const inner = nested !== null && nested.under === p.key ? nested.html : "";
          return `<li><a href="${escapeHtml(docUrl(p.key, base))}"${mark}>${escapeHtml(p.title)}</a>${inner}</li>`;
        })
        .join("");
      const open = group.pages.some((p) => p.key === current || p.key === nested?.under)
        ? " open"
        : "";
      return `<details class="nav-group"${open}><summary>${escapeHtml(group.title)}</summary><ul>${links}</ul></details>`;
    })
    .join("\n");
}

/** A list the sidebar shows under one of its pages. */
export interface NavNested {
  /** Key of the sidebar page the list goes under. */
  under: string;
  /** The list's HTML. */
  html: string;
}

/**
 * The sidebar list for a Web API page, shown beneath the Web API entry:
 * every route group (from the index), then the schemas page, with the page
 * being read marked. The routes themselves are listed in the page's "On
 * this page" rail, not here.
 *
 * @param key - The page's key.
 * @param title - Its title.
 * @param base - URL prefix of the docs version.
 * @param groups - The index's route groups (see renderDoc and
 *   apiRouteGroups); when empty, the list holds only this page.
 * @returns The list, or null for a page outside `api/web/` and for an
 *   index that lists no group.
 */
export function apiNavNested(
  key: string,
  title: string,
  base: string,
  groups: readonly ApiRouteGroup[] = [],
): NavNested | null {
  if (!key.startsWith("api/web/") || (key === API_INDEX && groups.length === 0)) return null;
  const here = docUrl(key, base);
  const item = (href: string, text: string): string =>
    `<li><a href="${escapeHtml(href)}"${href === here ? ' aria-current="page"' : ""}>${escapeHtml(text)}</a></li>`;
  const schemas = docUrl(API_SCHEMAS, base);
  const listed =
    groups.length === 0
      ? [item(here, title)]
      : [...groups.map((g) => item(g.href, g.title)), item(schemas, "Schemas")];
  return { under: API_INDEX, html: `<ul class="nav-sub">${listed.join("")}</ul>` };
}

/**
 * Reads the route groups the Web API index lists, for the sidebar of the
 * reference's other pages.
 *
 * @param source - The index's Markdown.
 * @param options - Docs that exist (null: any `.md`), the GitHub prefix for
 *   repository files, and the URL prefix of the docs version.
 * @returns The groups with their links rewritten for the site.
 */
export function apiRouteGroups(
  source: string,
  {
    keys = null,
    blobBase,
    base = "/docs/",
  }: { keys?: ReadonlySet<string> | null; blobBase: string; base?: string },
): ApiRouteGroup[] {
  const md = renderer(API_INDEX, keys, blobBase, (k) => docUrl(k, base));
  return routeLinks(indexRoutes(md.parse(source, {})), keys, blobBase, base);
}

/**
 * Rewrites the links of route groups for the site.
 *
 * @param groups - Groups as indexRoutes reads them from the index.
 * @param keys - See rewriteLink.
 * @param blobBase - See rewriteLink.
 * @param base - URL prefix of the docs version.
 * @returns The groups with site links.
 */
function routeLinks(
  groups: readonly ApiRouteGroup[],
  keys: ReadonlySet<string> | null,
  blobBase: string,
  base: string,
): ApiRouteGroup[] {
  const link = (href: string): string =>
    rewriteLink(href, API_INDEX, keys, blobBase, (k) => docUrl(k, base));
  return groups.map((group) => ({
    ...group,
    href: link(group.href),
    routes: group.routes.map((route) => ({ ...route, href: link(route.href) })),
  }));
}

/** Where a doc sits in the sidebar. */
export interface NavPlace {
  /** Title of the sidebar group that lists it, or null when none does. */
  group: string | null;
  /** Key of that group's first page, which its breadcrumb crumb links to; null with `group`. */
  groupFirst: string | null;
  /** The doc before it in sidebar order, if any. */
  previous: { key: string; title: string } | null;
  /** The doc after it in sidebar order, if any. */
  next: { key: string; title: string } | null;
}

/**
 * Finds a doc in the sidebar, for its breadcrumb and its previous and next links.
 *
 * @param nav - The sidebar.
 * @param key - The doc's key.
 * @returns Its group and neighbours, reading the groups as one list;
 *   all null when the sidebar does not list it.
 */
export function navPlace(nav: readonly NavGroup[], key: string): NavPlace {
  const flat = nav.flatMap((g) => g.pages.map((p) => ({ ...p, group: g.title })));
  const at = flat.findIndex((p) => p.key === key);
  if (at === -1) return { group: null, groupFirst: null, previous: null, next: null };
  const pick = (i: number): { key: string; title: string } | null => {
    const page = flat[i];
    return page === undefined ? null : { key: page.key, title: page.title };
  };
  const group = flat[at]?.group ?? null;
  const groupFirst = flat.find((p) => p.group === group)?.key ?? null;
  return { group, groupFirst, previous: pick(at - 1), next: pick(at + 1) };
}

/** A sidebar page that a doc the sidebar does not list sits under. */
export interface NavParent {
  /** The sidebar page's key. */
  key: string;
  /** Its sidebar title. */
  title: string;
}

/**
 * Finds the sidebar page a Web API page sits under, which its breadcrumb
 * names: the sidebar lists only the Web API index, and the route group and
 * schema pages beneath it.
 *
 * @param nav - The sidebar.
 * @param key - The doc's key.
 * @returns The Web API index for any other page under `api/web/`, when the
 *   sidebar lists the index, else null.
 */
export function navParent(nav: readonly NavGroup[], key: string): NavParent | null {
  if (!key.startsWith("api/web/") || key === "api/web/README") return null;
  const page = nav.flatMap((g) => g.pages).find((p) => p.key === "api/web/README");
  return page === undefined ? null : { key: page.key, title: page.title };
}

/** One step of a breadcrumb trail. */
export interface Crumb {
  /** The text shown. */
  title: string;
  /** Key of the doc it links to, or null for the docs home. */
  key: string | null;
}

/**
 * Works out a doc's breadcrumb trail, which the page shows and its JSON-LD
 * repeats: Docs, the sidebar group (linking to the group's first page), the
 * sidebar page it sits under (see navParent), then the doc itself.
 *
 * @param nav - The sidebar.
 * @param key - The doc's key.
 * @param title - The doc's title.
 * @returns The crumbs, Docs first and the doc last.
 */
export function crumbTrail(nav: readonly NavGroup[], key: string, title: string): Crumb[] {
  const parent = navParent(nav, key);
  const place = navPlace(nav, parent?.key ?? key);
  const trail: Crumb[] = [{ title: "Docs", key: null }];
  if (place.group !== null && place.groupFirst !== null)
    trail.push({ title: place.group, key: place.groupFirst });
  if (parent !== null) trail.push(parent);
  trail.push({ title, key });
  return trail;
}

/**
 * Renders a doc's breadcrumb trail.
 *
 * @param trail - The crumbs (see crumbTrail); the last is the doc itself.
 * @param base - URL prefix of the docs version.
 * @returns A `nav` with each crumb a link, the last marked as the current page.
 */
export function crumbsHtml(trail: readonly Crumb[], base: string): string {
  const items = trail.map((c, i) => {
    const current = i === trail.length - 1 ? ' aria-current="page"' : "";
    return `<li><a href="${escapeHtml(docUrl(c.key ?? "README", base))}"${current}>${escapeHtml(c.title)}</a></li>`;
  });
  return `<nav class="crumbs" aria-label="Breadcrumb"><ol>${items.join("")}</ol></nav>`;
}

/**
 * Renders the previous and next links at the end of a doc.
 *
 * @param place - Where the doc sits in the sidebar (see navPlace).
 * @param base - URL prefix of the docs version.
 * @returns A `nav` with one link per neighbour; empty when it has none.
 */
export function pagerHtml(place: NavPlace, base: string): string {
  const link = (
    page: { key: string; title: string } | null,
    rel: "prev" | "next",
    label: string,
  ): string =>
    page === null
      ? ""
      : `<a class="pager-${rel}" rel="${rel}" href="${escapeHtml(docUrl(page.key, base))}"><span>${label}</span>${escapeHtml(page.title)}</a>`;
  const links = link(place.previous, "prev", "Previous") + link(place.next, "next", "Next");
  return links === "" ? "" : `<nav class="pager" aria-label="Previous and next">${links}</nav>`;
}

/**
 * Renders the list of a doc's sections.
 *
 * @param headings - The doc's headings (see renderDoc).
 * @returns A `ul` of links to each heading, subsections marked `sub`;
 *   empty when the doc has no headings.
 */
export function tocListHtml(headings: readonly DocHeading[]): string {
  if (headings.length === 0) return "";
  const ops = headings.flatMap((h) =>
    h.method === undefined ? [] : [{ method: h.method, path: h.text, id: h.id }],
  );
  // A route group's page lists its routes nested by path; other docs list
  // their sections and subsections.
  const items =
    ops.length === headings.length
      ? routeTree(ops).map((e) => {
          const path = `<code>${escapeHtml(e.label).replaceAll("/", "/<wbr>")}</code>`;
          if (e.kind === "path")
            return `<li class="toc-path depth-${e.depth}"><span class="toc-label">${path}</span></li>`;
          return `<li class="depth-${e.depth}"><a class="toc-op" href="#${escapeHtml(e.id)}"><span class="${methodClass(e.method)}">${e.method}</span> ${path}</a></li>`;
        })
      : headings.map(
          (h) =>
            `<li class="${h.level === 3 ? "sub depth-1" : "depth-0"}"><a href="#${escapeHtml(h.id)}">${headingLabel(h)}</a></li>`,
        );
  return `<ul>${items.join("")}</ul>`;
}

/**
 * A heading's link text: a Web API operation as a method badge and its
 * path, anything else as its text.
 *
 * @param heading - The heading.
 * @returns HTML.
 */
function headingLabel(heading: DocHeading): string {
  if (heading.method === undefined) return escapeHtml(heading.text);
  // Long paths wrap after a slash rather than inside a segment.
  const path = escapeHtml(heading.text).replaceAll("/", "/<wbr>");
  return `<span class="${methodClass(heading.method)}">${heading.method}</span> <code>${path}</code>`;
}

/**
 * Renders the "On this page" rail beside a doc.
 *
 * @param headings - The doc's headings (see renderDoc).
 * @param links - The page's source on GitHub (null for generated pages) and
 *   a link to report a problem with it.
 * @returns An `aside` with the section list and the page's links.
 */
export function tocHtml(
  headings: readonly DocHeading[],
  links: { source: string | null; issue: string },
): string {
  const list = tocListHtml(headings);
  const sections =
    list === "" ? "" : `<p class="toc-title">On this page</p><div class="toc-list">${list}</div>`;
  const source =
    links.source === null
      ? ""
      : `<li><a href="${escapeHtml(links.source)}">${ICONS.edit}Source on GitHub</a></li>`;
  const actions = `<ul class="toc-actions">${source}<li><a href="${escapeHtml(links.issue)}">${ICONS.report}Report a problem</a></li></ul>`;
  return `<aside class="toc" aria-label="On this page">${sections}${actions}</aside>`;
}

/**
 * Small line icons for the page's actions, drawn here (not taken from an
 * icon set) and hidden from assistive technology: the text beside each
 * says what it does.
 */
const ICONS = {
  /** A clock turning back: when the page last changed. */
  updated: icon('<path d="M3 12a9 9 0 1 0 2.6-6.4"/><path d="M3 4v5h5"/><path d="M12 8v4l3 2"/>'),
  /** Two sheets: copy the page. */
  copy: icon(
    '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>',
  ),
  /** A page with a folded corner: the Markdown version. */
  markdown: icon(
    '<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/><path d="M9 13h6M9 17h4"/>',
  ),
  /** A pencil: the page's source. */
  edit: icon('<path d="M4 20h4L19 9a2.8 2.8 0 0 0-4-4L4 16z"/><path d="M13.5 6.5l4 4"/>'),
  /** A flag: report a problem. */
  report: icon('<path d="M5 21V4"/><path d="M5 4h12l-2.5 4L17 12H5"/>'),
} as const;

/**
 * Wraps icon paths in an `svg` that takes the text's color and size.
 *
 * @param paths - The icon's SVG elements, drawn on a 24-unit grid.
 * @returns The `svg`, hidden from assistive technology.
 */
function icon(paths: string): string {
  return `<svg class="icon" aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${paths}</svg>`;
}

/**
 * Renders the line under a doc's title: the development note, when the
 * docs follow main, then the page's date and its Markdown actions.
 *
 * The Copy as Markdown button starts hidden; docs.js shows it when the
 * browser can write to the clipboard.
 *
 * @param note - The development note's HTML, or empty.
 * @param updated - The date the page last changed, or null to leave it out.
 * @param markdownUrl - The page's Markdown version.
 * @returns A `div.doc-meta`.
 */
export function metaHtml(note: string, updated: IsoDate | null, markdownUrl: string): string {
  const date = updated
    ? `<span class="doc-action doc-updated">${ICONS.updated}Last updated <time datetime="${updated}">${formatDate(updated)}</time></span>`
    : "";
  const url = escapeHtml(markdownUrl);
  const copy = `<button type="button" class="doc-action doc-copy-markdown" data-markdown="${url}" hidden>${ICONS.copy}<span>Copy as Markdown</span></button>`;
  const view = `<a class="doc-action" href="${url}" type="text/markdown">${ICONS.markdown}View as Markdown</a>`;
  return `<div class="doc-meta">${note === "" ? "" : `<p class="doc-note-line">${note}</p>`}<p class="doc-actions">${date}${copy}${view}</p></div>`;
}

/**
 * Renders the menus that stand in for the sidebar and the "On this page"
 * rail below their breakpoints: the section menu alone below 1200px, where
 * the rail goes, and both below 900px, where the sidebar goes too.
 *
 * @param sidebar - The sidebar's HTML (version picker, overview link, groups).
 * @param headings - The doc's headings; no section menu when empty.
 * @returns A bar of two `details` menus.
 */
export function mobileMenusHtml(sidebar: string, headings: readonly DocHeading[]): string {
  const list = tocListHtml(headings);
  const sections =
    list === ""
      ? ""
      : `<details class="menu menu-toc" name="docs-menu"><summary>On this page</summary><div class="menu-body">${list}</div></details>`;
  return `<div class="docs-menus"><details class="menu menu-nav" name="docs-menu"><summary>Menu</summary><div class="menu-body">${sidebar}</div></details>${sections}</div>`;
}

/**
 * Renders the sidebar's link to the docs home.
 *
 * @param base - URL prefix of the docs version.
 * @param current - Whether the page shown is the docs home.
 * @returns The link.
 */
function homeLink(base: string, current: boolean): string {
  return `<a class="docs-home" href="${escapeHtml(docUrl("README", base))}"${current ? ' aria-current="page"' : ""}>Overview</a>`;
}

/**
 * Puts HTML right after a doc's title.
 *
 * @param body - The rendered doc.
 * @param html - What to insert.
 * @returns The body with `html` after its first `</h1>`, or in front
 *   when the doc does not open with one.
 */
function afterTitle(body: string, html: string): string {
  const end = body.indexOf("</h1>");
  if (!body.startsWith("<h1") || end === -1) return html + body;
  const at = end + "</h1>".length;
  return body.slice(0, at) + html + body.slice(at);
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
export function versionsHtml(
  versions: readonly DocsVersion[],
  current: string,
  key: string,
): string {
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
export const SHELL_VALUES = [
  "pageTitle",
  "pageDescription",
  "pagePath",
  "markdownUrl",
  "docsBase",
  "searchIndex",
];

/** `<!--name-->` markers in the docs shell, filled with trusted HTML. */
export const SHELL_MARKERS = [
  "jsonld",
  "doc-menus",
  "doc-versions",
  "doc-nav",
  "doc-crumbs",
  "doc-body",
  "doc-pager",
  "doc-footer",
  "doc-toc",
];

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
export function fillShell(
  shell: string,
  values: Readonly<Record<string, string>>,
  markers: Readonly<Record<string, string>>,
): string {
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
  /** URL paths of the indexable pages outside /docs/ (build.ts `PAGES`), which the sitemap lists first. */
  pages: string[];
  /** The titles of `pages`, in the same order, for the human-readable sitemap. */
  pageTitles: string[];
  /** src/sitemap.html rendered for the site, with SITEMAP_MARKER where the list goes. */
  sitemapHtml: string;
  /**
   * Generated reference folders under `api/` (`api/web`, `api/rust/<crate>`)
   * mapped to the repository path each is generated from (build.ts
   * `generatedSources`).
   */
  sources: Readonly<Record<string, string>>;
}

/**
 * Finds the repository path a generated page under `api/` comes from: the
 * entry of `sources` for the longest folder holding the page.
 *
 * @param key - The page's key.
 * @param sources - DocsShell `sources`.
 * @returns The repository path, or null when no folder holds the page.
 */
export function generatedSource(
  key: string,
  sources: Readonly<Record<string, string>>,
): string | null {
  for (let at = key.lastIndexOf("/"); at > 0; at = key.lastIndexOf("/", at - 1)) {
    const folder = key.slice(0, at);
    if (Object.hasOwn(sources, folder)) return sources[folder] ?? null;
  }
  return null;
}

/**
 * Renders one docs page from its Markdown.
 *
 * @param shell - The baked page shell.
 * @param source - The doc's Markdown.
 * @param key - The doc's key (path without `.md`).
 * @param at - The docs version: its ref, its URL prefix and the docs it
 *   holds (null when unknown), and the date the page last changed (null
 *   when unknown, which leaves out the "Last updated" line), and for a Web
 *   API page other than the index, the index's Markdown (null when it could
 *   not be read), whose route groups the sidebar lists.
 * @returns The full HTML page and the Markdown copy with links rewritten for
 *   the site.
 */
export function renderDocPage(
  shell: DocsShell,
  source: string,
  key: string,
  at: {
    ref: string;
    base: string;
    keys?: ReadonlySet<string> | null;
    updated?: IsoDate | null;
    apiIndex?: string | null;
  },
): RenderedPage {
  const blobBase = `${shell.repository}/blob/${at.ref}/`;
  const doc = renderDoc(source, key, { keys: at.keys ?? null, blobBase, base: at.base });
  const groups =
    key === API_INDEX
      ? doc.routes
      : at.apiIndex
        ? apiRouteGroups(at.apiIndex, { keys: at.keys ?? null, blobBase, base: at.base })
        : [];
  const markdownUrl = docMarkdownUrl(key, at.base);
  const generated = key.startsWith("api/");
  // A generated page links to what it is generated from: a file (blob) or a
  // folder (tree) of the repository.
  const sourcePath = generated ? generatedSource(key, shell.sources) : `docs/${key}.md`;
  const sourceUrl =
    sourcePath === null
      ? null
      : `${/\.[a-z]+$/.test(sourcePath) ? blobBase : `${shell.repository}/tree/${at.ref}/`}${sourcePath}`;
  const sourceAnchor =
    sourceUrl === null || sourcePath === null
      ? ""
      : `<a href="${escapeHtml(sourceUrl)}">${escapeHtml(sourcePath)}</a>`;
  const sourceLink = generated
    ? `Generated from ${sourceAnchor || "the source"} at ${escapeHtml(at.ref)}`
    : `Source: ${sourceAnchor} at ${escapeHtml(at.ref)}`;
  const issueUrl = `${shell.repository}/issues/new?title=${encodeURIComponent(`Docs: ${doc.title}`)}&body=${encodeURIComponent(`Page: ${shell.origin}${docUrl(key, at.base)}\n\n`)}`;
  const note =
    at.ref === "main"
      ? '<span class="doc-note" role="note"><span class="badge unstable-badge">Development</span> These docs follow the development version and change with every merge.</span>'
      : "";
  const meta = metaHtml(note, at.updated ?? null, markdownUrl);
  const place = navPlace(shell.nav, key);
  const trail = crumbTrail(shell.nav, key, doc.title);
  const versions = versionsHtml(shell.versions, at.ref, key);
  const nav =
    homeLink(at.base, key === "README") +
    navHtml(shell.nav, key, at.base, apiNavNested(key, doc.title, at.base, groups));
  const html = fillShell(
    shell.html,
    {
      pageTitle: docPageTitle(doc.title),
      pageDescription: doc.description || `${doc.title}: Bookclerk documentation.`,
      pagePath: docUrl(key, at.base),
      markdownUrl,
      docsBase: docUrl("README", at.base),
      searchIndex: searchIndexUrl(at.base),
    },
    {
      jsonld: jsonLd(
        docJsonLd(
          shell.origin,
          at.base,
          { key, title: doc.title, description: doc.description, updated: at.updated ?? null },
          trail,
        ),
      ),
      "doc-menus": mobileMenusHtml(versions + nav, doc.headings),
      "doc-versions": versions,
      "doc-nav": nav,
      "doc-crumbs": crumbsHtml(trail, at.base),
      "doc-body": afterTitle(doc.html, meta),
      "doc-pager": pagerHtml(place, at.base),
      "doc-footer": `<p class="doc-source faint">${sourceLink} · <a href="${escapeHtml(markdownUrl)}" type="text/markdown">View as Markdown</a></p>`,
      "doc-toc": tocHtml(doc.headings, { source: sourceUrl, issue: issueUrl }),
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
export function renderUnavailablePage(
  shell: DocsShell,
  key: string,
  at: { ref: string; base: string; githubKey?: string },
): RenderedPage {
  const versions = versionsHtml(shell.versions, at.ref, key);
  const home = homeLink(at.base, false);
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
      pageTitle: `Docs unavailable${DOC_TITLE_SUFFIX}`,
      pageDescription: "The Bookclerk documentation could not be loaded right now.",
      pagePath: docUrl(key, at.base),
      markdownUrl: docMarkdownUrl(key, at.base),
      docsBase: docUrl("README", at.base),
      searchIndex: searchIndexUrl(at.base),
    },
    {
      jsonld: "",
      "doc-menus": mobileMenusHtml(versions + home, []),
      "doc-versions": versions,
      "doc-nav": home,
      "doc-crumbs": "",
      "doc-body": body,
      "doc-pager": "",
      "doc-footer": "",
      "doc-toc": "",
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

/** Where the human-readable sitemap's list goes in src/sitemap.html. */
export const SITEMAP_MARKER = "<!--sitemap-->";

/** One page the human-readable sitemap lists. */
export interface SitemapPageLink {
  /** Its doc key, or null for a page outside /docs/. */
  key: string | null;
  /** Its URL path. */
  path: string;
  /** Its title, or null to name it from its path. */
  title: string | null;
  /** When it last changed, or null when unknown. */
  updated: IsoDate | null;
}

/** One heading of the human-readable sitemap and the pages under it. */
interface SitemapGroup {
  /** The heading. */
  title: string;
  /** Pages listed straight under the heading. */
  links: SitemapPageLink[];
  /** Subheadings (one per API reference language), in first-seen order. */
  parts: Map<string, SitemapPageLink[]>;
}

/** Subheadings for the API reference's folders on the sitemap page; others show the folder name. */
const API_PART_TITLES: Readonly<Record<string, string>> = {
  python: "Python plugin SDK",
  rust: "Rust crates",
  typescript: "TypeScript plugin SDK",
  ui: "Web interface",
  web: "Web API",
};

/**
 * Names a doc for the sitemap page when the sidebar does not: its path
 * below `prefix`, without a trailing README or index.
 *
 * @param key - The doc's key.
 * @param prefix - The part of the key its heading already says, ending in `/`, or "".
 * @returns A readable label.
 */
function sitemapLabel(key: string, prefix: string): string {
  const rest = key.startsWith(prefix) ? key.slice(prefix.length) : key;
  const trimmed = rest.replace(/(?:^|\/)(?:README|index)$/, "");
  return trimmed === "" ? "Overview" : trimmed;
}

/**
 * Renders /sitemap: every page the site serves, grouped the way the docs
 * sidebar groups them, for people (sitemap.xml is for search engines). It
 * is a website page (src/sitemap.html), not a docs page.
 *
 * Site pages come first, then each sidebar group in order with every doc
 * whose key, or top folder, the group lists; the API reference is split by
 * language. Docs the sidebar does not place go under "More".
 *
 * @param shell - The baked page shell.
 * @param links - The pages, site pages first, then docs in key order.
 * @returns The full HTML page and its Markdown version.
 */
export function renderSitemapPage(
  shell: DocsShell,
  links: readonly SitemapPageLink[],
): RenderedPage {
  const site: SitemapGroup = { title: "Website", links: [], parts: new Map() };
  const groups = shell.nav.map((g): SitemapGroup => ({
    title: g.title,
    links: [],
    parts: new Map(),
  }));
  const more: SitemapGroup = { title: "More", links: [], parts: new Map() };
  const byKey = new Map<string, { group: number; title: string }>();
  const byFolder = new Map<string, number>();
  for (const [i, g] of shell.nav.entries()) {
    for (const p of g.pages) {
      byKey.set(p.key, { group: i, title: p.title });
      const folder = p.key.split("/")[0] ?? "";
      if (p.key.includes("/") && !byFolder.has(folder)) byFolder.set(folder, i);
    }
  }
  for (const link of links) {
    if (link.key === null) {
      site.links.push(link);
      continue;
    }
    const placed = byKey.get(link.key);
    const segments = link.key.split("/");
    const index = placed?.group ?? byFolder.get(segments[0] ?? "");
    const group = index === undefined ? more : (groups[index] ?? more);
    if (link.key === "README") {
      site.links.push({ ...link, title: link.title ?? "Documentation" });
    } else if (segments[0] === "api" && segments.length > 2) {
      // The API reference runs to hundreds of pages: one subheading per language.
      const part = segments[1] ?? "";
      const label = placed?.title ?? sitemapLabel(link.key, `api/${part}/`);
      const list = group.parts.get(part) ?? [];
      if (list.length === 0) group.parts.set(part, list);
      list.push({ ...link, title: label });
    } else {
      group.links.push({
        ...link,
        title: placed?.title ?? link.title ?? sitemapLabel(link.key, ""),
      });
    }
  }
  // Docs the sidebar lists keep its order; the rest follow in key order.
  const order = new Map(
    shell.nav.flatMap((g) => g.pages.map((p) => p.key)).map((key, i) => [key, i] as const),
  );
  const rank = (l: SitemapPageLink): number =>
    l.key === null ? -1 : (order.get(l.key) ?? Number.MAX_SAFE_INTEGER);
  for (const g of [...groups, more]) g.links.sort((a, b) => rank(a) - rank(b));
  const all = [site, ...groups, more].filter((g) => g.links.length > 0 || g.parts.size > 0);
  const total = links.length;

  const itemHtml = (l: SitemapPageLink): string => {
    const date = l.updated
      ? ` <span class="faint">· <time datetime="${l.updated}">${formatDate(l.updated)}</time></span>`
      : "";
    return `<li><a href="${escapeHtml(l.path)}">${escapeHtml(l.title ?? l.path)}</a>${date}</li>`;
  };
  const itemMd = (l: SitemapPageLink): string => {
    const date = l.updated ? ` (${formatDate(l.updated)})` : "";
    return `- [${(l.title ?? l.path).replace(/[\\[\]]/g, "\\$&")}](${shell.origin}${l.path})${date}`;
  };
  const ids = all.map((g) => `sitemap-${slugify(g.title)}`);
  const html: string[] = [
    "<h1>Sitemap</h1>",
    `<p class="muted">Every page on this site, ${total} in all. Search engines read the same list from <a href="/sitemap.xml">sitemap.xml</a>.</p>`,
    `<nav aria-label="Sections"><p>${all.map((g, i) => `<a href="#${escapeHtml(ids[i] ?? "")}">${escapeHtml(g.title)}</a>`).join(" · ")}</p></nav>`,
  ];
  const md: string[] = [
    "# Sitemap",
    "",
    `Every page on this site, ${total} in all. Search engines read the same list from ${shell.origin}/sitemap.xml.`,
    "",
  ];
  for (const [i, g] of all.entries()) {
    html.push(`<h2 id="${escapeHtml(ids[i] ?? "")}">${escapeHtml(g.title)}</h2>`);
    md.push(`## ${g.title}`, "");
    if (g.links.length > 0) {
      html.push(`<ul>${g.links.map(itemHtml).join("")}</ul>`);
      md.push(...g.links.map(itemMd), "");
    }
    for (const [part, list] of g.parts) {
      const name = API_PART_TITLES[part] ?? part;
      list.sort((a, b) => rank(a) - rank(b));
      html.push(`<h3>${escapeHtml(name)}</h3>`, `<ul>${list.map(itemHtml).join("")}</ul>`);
      md.push(`### ${name}`, "", ...list.map(itemMd), "");
    }
  }
  const at = shell.sitemapHtml.indexOf(SITEMAP_MARKER);
  if (at === -1) throw new Error(`the sitemap shell has no ${SITEMAP_MARKER}`);
  const page =
    shell.sitemapHtml.slice(0, at) +
    html.join("\n") +
    shell.sitemapHtml.slice(at + SITEMAP_MARKER.length);
  return { html: page, markdown: md.join("\n") };
}
