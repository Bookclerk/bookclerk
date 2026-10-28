// The Web API reference's layout: the pages scripts/openapi_markdown.py
// writes under api/web/ are plain Markdown that reads well on GitHub, and
// the site lays each operation out the way OpenAPI viewers such as Scalar
// do: a method badge beside the path, the description, parameters and
// responses in one column, and the example request and response in a
// second column beside them on wide screens. The index gets the same look:
// its quick start beside the introduction and a card per route group with
// method badges. Nothing here runs in the browser; it rearranges
// markdown-it tokens before they are rendered.

import type { Token } from "markdown-it";

/** HTTP methods an operation heading can start with. */
export const HTTP_METHODS = [
  "GET",
  "PUT",
  "POST",
  "DELETE",
  "OPTIONS",
  "HEAD",
  "PATCH",
  "TRACE",
] as const;

/** One of {@link HTTP_METHODS}. */
export type HttpMethod = (typeof HTTP_METHODS)[number];

/** What an operation heading names, kept on its `heading_open` token's `meta`. */
export interface ApiOperation {
  /** The HTTP method the heading starts with, in upper case. */
  method: HttpMethod;
  /** The path template, such as `/api/jobs/{id}/cancel`. */
  path: string;
}

/** Headings of the subsections that move to the examples column. */
const EXAMPLE_HEADINGS: ReadonlySet<string> = new Set(["Example request", "Example response"]);

/** An operation heading's text: a method, one space, a path. */
const OPERATION_RE = /^([A-Z]+) (\/\S*)$/;

/** Makes a token, as markdown-it's `state.Token` constructor does. */
export type MakeToken = (type: string, tag: string, nesting: -1 | 0 | 1) => Token;

/**
 * Whether a doc is a page of the Web API reference that lists operations.
 *
 * @param key - The doc's key.
 * @returns True for `api/web/<group>`, false for its index and schema page.
 */
export function isApiOperationsPage(key: string): boolean {
  return key.startsWith("api/web/") && key !== API_INDEX && key !== API_SCHEMAS;
}

/** Key of the Web API reference's index, which {@link layoutIndex} lays out. */
export const API_INDEX = "api/web/README";

/** Key of the Web API reference's schema page. */
export const API_SCHEMAS = "api/web/schemas";

/** The heading of the index section that sits beside its introduction. */
const QUICK_START = "Quick start";

/**
 * Reads the operation an `##` heading names.
 *
 * @param inline - The heading's `inline` token.
 * @returns The method and path when the heading is exactly one code span
 *   such as `` `GET /api/jobs` ``, else null.
 */
export function operationOf(inline: Token | undefined): ApiOperation | null {
  const children = inline?.children ?? [];
  const only = children[0];
  return children.length === 1 && only !== undefined ? operationOfCode(only) : null;
}

/**
 * Reads the operation a code span names.
 *
 * @param token - An inline token.
 * @returns The method and path when it is a code span such as
 *   `` `GET /api/jobs` ``, else null.
 */
function operationOfCode(token: Token): ApiOperation | null {
  if (token.type !== "code_inline") return null;
  const match = OPERATION_RE.exec(token.content);
  const method = match?.[1];
  const path = match?.[2];
  if (method === undefined || path === undefined || !isHttpMethod(method)) return null;
  return { method, path };
}

/**
 * Reads the operation {@link layoutOperations} recorded on a heading.
 *
 * @param token - A `heading_open` token.
 * @returns The operation, or null for any other heading.
 */
export function operationMeta(token: Token): ApiOperation | null {
  const meta: unknown = token.meta;
  if (typeof meta !== "object" || meta === null) return null;
  const method: unknown = "method" in meta ? meta.method : undefined;
  const path: unknown = "path" in meta ? meta.path : undefined;
  if (typeof method !== "string" || typeof path !== "string" || !isHttpMethod(method)) return null;
  return { method, path };
}

/**
 * Narrows a string to an {@link HttpMethod}.
 *
 * @param value - Upper-case text.
 * @returns Whether it is one of {@link HTTP_METHODS}.
 */
function isHttpMethod(value: string): value is HttpMethod {
  return (HTTP_METHODS as readonly string[]).includes(value);
}

/**
 * The CSS class of a method badge.
 *
 * @param method - The operation's HTTP method, which picks the badge color.
 * @returns `api-method api-method-<method in lower case>`.
 */
export function methodClass(method: HttpMethod): string {
  return `api-method api-method-${method.toLowerCase()}`;
}

/**
 * The CSS class of a status code: its class of response.
 *
 * @param status - A status such as `200`, `4XX` or `default`.
 * @returns `api-status api-status-<1-5>xx`, or `api-status` alone.
 */
export function statusClass(status: string): string {
  const first = status[0];
  return first !== undefined && /^[1-5]\d\d$|^[1-5]XX$/i.test(status)
    ? `api-status api-status-${first}xx`
    : "api-status";
}

/**
 * Lays out an operations page: each `##` operation becomes a `section`
 * with a method badge in its heading, its example subsections are moved
 * into a second column, and status codes in a Responses table get a
 * colored mark. The whole body is wrapped in `div.api-ref`, which site.css
 * uses to widen the page.
 *
 * Headings keep their ids and their place in the text; only the order of
 * the example subsections changes (they follow the rest of the operation,
 * which is where the Markdown has them anyway).
 *
 * @param tokens - The page's block tokens, after heading ids are set.
 * @param make - markdown-it's token constructor.
 * @returns The new token list; the input when the page has no operation.
 */
export function layoutOperations(tokens: readonly Token[], make: MakeToken): Token[] {
  const html = (content: string): Token => {
    const token = make("html_block", "", 0);
    token.content = content;
    return token;
  };
  const sections: { start: number; end: number; op: ApiOperation }[] = [];
  tokens.forEach((token, i) => {
    // Only top-level headings split the page: a section that opened inside
    // a list or quote would close outside it.
    if (token.type !== "heading_open" || token.tag !== "h2" || token.level !== 0) return;
    const op = operationOf(tokens[i + 1]);
    const last = sections[sections.length - 1];
    if (last !== undefined && last.end === tokens.length) last.end = i;
    if (op !== null) sections.push({ start: i, end: tokens.length, op });
  });
  if (sections.length === 0) return [...tokens];

  // The title stays ahead of the wrapper, where the page's meta line
  // (Last updated, View as Markdown) is placed after it.
  const titled = tokens[0]?.type === "heading_open" && tokens[0].tag === "h1" ? 3 : 0;
  const out: Token[] = [...tokens.slice(0, titled), html('<div class="api-ref">\n')];
  let at = titled;
  for (const { start, end, op } of sections) {
    out.push(...tokens.slice(at, start));
    const open = tokens[start];
    const inline = tokens[start + 1];
    const close = tokens[start + 2];
    if (open === undefined || inline === undefined || close === undefined) break;
    open.meta = { method: op.method, path: op.path };
    open.attrJoin("class", "api-op-title");
    const badge = make("html_inline", "", 0);
    badge.content = `<span class="${methodClass(op.method)}">${op.method}</span> `;
    const path = make("code_inline", "code", 0);
    path.content = op.path;
    path.markup = "`";
    inline.children = [badge, path];
    const id = String(open.attrGet("id") ?? "");
    out.push(html(`<section class="api-op" aria-labelledby="${id}">\n`), open, inline, close);

    const main: Token[] = [];
    const examples: Token[] = [];
    let target = main;
    for (let i = start + 3; i < end; i++) {
      const token = tokens[i];
      if (token === undefined) continue;
      if (token.type === "heading_open" && token.tag === "h3" && token.level === 0) {
        target = EXAMPLE_HEADINGS.has(tokens[i + 1]?.content ?? "") ? examples : main;
      }
      target.push(token);
    }
    markStatuses(main, make);
    out.push(
      html('<div class="api-op-body">\n<div class="api-op-main">\n'),
      ...main,
      html("</div>\n"),
    );
    if (examples.length > 0)
      out.push(html('<div class="api-op-examples">\n'), ...examples, html("</div>\n"));
    out.push(html("</div>\n</section>\n"));
    at = end;
  }
  out.push(...tokens.slice(at), html("</div>\n"));
  return out;
}

/**
 * Lays out the Web API index the way OpenAPI viewers lay out their
 * introduction: the opening text and the `## Quick start` section that
 * follows it sit side by side on wide screens, each `###` heading that is a
 * link (a route group) becomes a `section` card, and each route link in a
 * card whose text is one code span such as `` `GET /api/jobs` `` gets a
 * method badge. The body is wrapped in `div.api-ref api-index`, as
 * {@link layoutOperations} wraps an operations page.
 *
 * A group heading's `heading_open` token keeps `{ group: true }` on its
 * `meta`, and a badged link's `link_open` token keeps its operation, for
 * {@link indexRoutes}.
 *
 * @param tokens - The page's block tokens, after heading ids are set.
 * @param make - markdown-it's token constructor.
 * @returns The new token list.
 */
export function layoutIndex(tokens: readonly Token[], make: MakeToken): Token[] {
  const html = (content: string): Token => {
    const token = make("html_block", "", 0);
    token.content = content;
    return token;
  };
  // Top-level h2 and h3 headings bound the sections; one that opened inside
  // a list or quote would close outside it.
  const isHeading = (t: Token | undefined, tag: string): boolean =>
    t?.type === "heading_open" && t.tag === tag && t.level === 0;
  const bounds = tokens.flatMap((t, i) => (isHeading(t, "h2") || isHeading(t, "h3") ? [i] : []));
  const nextBound = (after: number, tags: readonly string[]): number =>
    bounds.find((i) => i > after && tags.includes(tokens[i]?.tag ?? "")) ?? tokens.length;

  const titled = tokens[0]?.type === "heading_open" && tokens[0].tag === "h1" ? 3 : 0;
  const out: Token[] = [...tokens.slice(0, titled), html('<div class="api-ref api-index">\n')];
  let at = titled;
  const firstH2 = nextBound(titled - 1, ["h2"]);
  const side = tokens[firstH2];
  if (firstH2 > titled && side !== undefined && tokens[firstH2 + 1]?.content === QUICK_START) {
    const end = nextBound(firstH2, ["h2"]);
    const id = String(side.attrGet("id") ?? "");
    out.push(
      html('<div class="api-intro">\n<div class="api-intro-main">\n'),
      ...tokens.slice(titled, firstH2),
      html(`</div>\n<aside class="api-intro-side" aria-labelledby="${id}">\n`),
      ...tokens.slice(firstH2, end),
      html("</aside>\n</div>\n"),
    );
    at = end;
  }
  for (const start of bounds) {
    if (start < at || !isHeading(tokens[start], "h3") || !isLinkHeading(tokens[start + 1]))
      continue;
    const end = nextBound(start, ["h2", "h3"]);
    const open = tokens[start];
    if (open === undefined) continue;
    open.meta = { group: true };
    out.push(
      ...tokens.slice(at, start),
      html(`<section class="api-group" aria-labelledby="${String(open.attrGet("id") ?? "")}">\n`),
    );
    for (const token of tokens.slice(start, end)) {
      if (token.type === "inline") badgeRouteLinks(token, make);
      out.push(token);
    }
    out.push(html("</section>\n"));
    at = end;
  }
  out.push(...tokens.slice(at), html("</div>\n"));
  return out;
}

/**
 * Whether a heading's text is exactly one link.
 *
 * @param inline - The heading's `inline` token.
 * @returns True when its children open a link first and close it last.
 */
function isLinkHeading(inline: Token | undefined): boolean {
  const children = inline?.children ?? [];
  if (children[0]?.type !== "link_open" || children[children.length - 1]?.type !== "link_close")
    return false;
  return children.filter((t) => t.type === "link_open").length === 1;
}

/**
 * Gives each link whose text is one operation code span a method badge and
 * keeps the operation on its `link_open` token's `meta`.
 *
 * @param inline - An `inline` token; its children change in place.
 * @param make - markdown-it's token constructor.
 */
function badgeRouteLinks(inline: Token, make: MakeToken): void {
  const children = inline.children ?? [];
  const out: Token[] = [];
  children.forEach((token, i) => {
    const open = children[i - 1];
    const close = children[i + 1];
    const op =
      open?.type === "link_open" && close?.type === "link_close" ? operationOfCode(token) : null;
    if (op === null || open === undefined) {
      out.push(token);
      return;
    }
    open.meta = { method: op.method, path: op.path };
    // The path may wrap after a slash, never inside a segment.
    const label = make("html_inline", "", 0);
    const path = escapeCode(op.path).replaceAll("/", "/<wbr>");
    label.content = `<span class="${methodClass(op.method)}">${op.method}</span> <code>${path}</code>`;
    out.push(label);
  });
  inline.children = out;
}

/** Entities for the characters markdown-it escapes in a code span. */
const CODE_ENTITIES: Readonly<Record<string, string>> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
};

/**
 * Escapes text as markdown-it escapes a code span's content.
 *
 * @param text - Raw text.
 * @returns The text with `& < > "` replaced by entities.
 */
function escapeCode(text: string): string {
  return text.replace(/[&<>"]/g, (c) => CODE_ENTITIES[c] ?? c);
}

/** A route group the index lists, for the sidebar. */
export interface ApiRouteGroup {
  /** The group's name. */
  title: string;
  /** The link to its page, as the Markdown has it. */
  href: string;
  /** Its routes, in order: each operation and the link to it, as the Markdown has it. */
  routes: (ApiOperation & { href: string })[];
}

/**
 * Reads the route groups {@link layoutIndex} marked.
 *
 * @param tokens - The laid-out tokens.
 * @returns Each group heading's link text and target, with the badged
 *   route links that follow it up to the next group.
 */
export function indexRoutes(tokens: readonly Token[]): ApiRouteGroup[] {
  const groups: ApiRouteGroup[] = [];
  tokens.forEach((token, i) => {
    const meta: unknown = token.meta;
    if (
      token.type === "heading_open" &&
      typeof meta === "object" &&
      meta !== null &&
      "group" in meta
    ) {
      const children = tokens[i + 1]?.children ?? [];
      const title = children
        .filter((t) => t.type === "text" || t.type === "code_inline")
        .map((t) => t.content)
        .join("")
        .trim();
      groups.push({ title, href: String(children[0]?.attrGet("href") ?? ""), routes: [] });
      return;
    }
    const group = groups[groups.length - 1];
    if (token.type !== "inline" || group === undefined) return;
    for (const child of token.children ?? []) {
      const op = child.type === "link_open" ? operationMeta(child) : null;
      if (op !== null) group.routes.push({ ...op, href: String(child.attrGet("href") ?? "") });
    }
  });
  return groups;
}

/**
 * Marks the status codes in the first column of each table whose first
 * header cell reads `Status`.
 *
 * @param tokens - Block tokens of one operation; changed in place.
 * @param make - markdown-it's token constructor.
 */
function markStatuses(tokens: Token[], make: MakeToken): void {
  let inStatusTable = false;
  let column = 0;
  tokens.forEach((token, i) => {
    if (token.type === "table_open") inStatusTable = tokens[i + 4]?.content === "Status";
    if (token.type === "tr_open") column = 0;
    if (token.type !== "td_open") return;
    column += 1;
    const inline = tokens[i + 1];
    if (!inStatusTable || column !== 1 || inline === undefined || inline.type !== "inline") return;
    const status = inline.content.trim();
    if (!/^[\w]{1,8}$/.test(status)) return;
    const mark = make("html_inline", "", 0);
    mark.content = `<span class="${statusClass(status)}">`;
    const text = make("text", "", 0);
    text.content = status;
    const end = make("html_inline", "", 0);
    end.content = "</span>";
    inline.children = [mark, text, end];
  });
}

/** How many levels deep {@link routeTree} nests routes (0 is the top). */
export const ROUTE_TREE_MAX_DEPTH = 2;

/** One line of a route group's "On this page" list (see {@link routeTree}). */
export type RouteTreeEntry =
  /** An operation: its badge, its section and the path below its parent line. */
  | { kind: "op"; method: HttpMethod; id: string; label: string; depth: number }
  /** A path several routes share that is not a route itself: a label only. */
  | { kind: "path"; label: string; depth: number };

/** A path segment's place in {@link routeTree}'s trie. */
interface RouteNode {
  /** The operations whose path ends here, in page order. */
  ops: { method: HttpMethod; id: string }[];
  /** Longer paths, by their next segment, in order of first appearance. */
  children: Map<string, RouteNode>;
}

/**
 * Nests a route group's operations by path, the way the docs nest
 * subsections: a route sits under the nearest shorter route (or shared
 * path) it extends, and shows only the part of its path past that line.
 *
 * The prefix every route shares is left out, since the page's headings
 * show it: on a page of `/api/auth/...` routes the top lines read `/login`,
 * and a route at the prefix itself reads `/`. A page whose routes all share
 * one path keeps its last segment. A shared path that is not a route becomes a label line only
 * when two or more routes branch from it; a single chain of segments is
 * kept on one line. Nesting stops at {@link ROUTE_TREE_MAX_DEPTH}: deeper
 * routes stay on that level with their longer remainder.
 *
 * @param ops - The page's operations, in page order, with their section ids.
 * @returns The lines, in list order: each node's routes, then what extends them.
 */
export function routeTree(
  ops: readonly { method: HttpMethod; path: string; id: string }[],
): RouteTreeEntry[] {
  const segments = (path: string): string[] => path.split("/").filter((s) => s !== "");
  const paths = ops.map((op) => segments(op.path));
  // The shared prefix, which may be a whole route's path, unless it is every
  // route's: then each route keeps its last segment.
  let shared = 0;
  const shortest = Math.min(...paths.map((p) => p.length));
  while (shared < shortest && paths.every((p) => p[shared] === paths[0]?.[shared])) shared += 1;
  if (paths.every((p) => p.length === shared)) shared = Math.max(0, shared - 1);
  const root: RouteNode = { ops: [], children: new Map() };
  ops.forEach((op, i) => {
    let node = root;
    for (const segment of (paths[i] ?? []).slice(shared)) {
      let next = node.children.get(segment);
      if (next === undefined) {
        next = { ops: [], children: new Map() };
        node.children.set(segment, next);
      }
      node = next;
    }
    node.ops.push({ method: op.method, id: op.id });
  });
  const entries: RouteTreeEntry[] = [];
  /**
   * Lists the lines for one node and what extends it.
   *
   * @param node - The node.
   * @param label - Its path past the last line above it.
   * @param depth - The level its own lines go on.
   */
  const visit = (node: RouteNode, label: string[], depth: number): void => {
    const branches = node.children.size + (node.ops.length > 0 ? 1 : 0);
    const shown = node.ops.length > 0 || (branches > 1 && depth < ROUTE_TREE_MAX_DEPTH);
    if (!shown) {
      for (const [segment, child] of node.children) visit(child, [...label, segment], depth);
      return;
    }
    const text = `/${label.join("/")}`;
    if (node.ops.length > 0)
      for (const op of node.ops) entries.push({ kind: "op", ...op, label: text, depth });
    else entries.push({ kind: "path", label: text, depth });
    const below = Math.min(depth + 1, ROUTE_TREE_MAX_DEPTH);
    // At the deepest level, what extends a line keeps that line's path.
    const carried = depth + 1 > ROUTE_TREE_MAX_DEPTH ? label : [];
    for (const [segment, child] of node.children) visit(child, [...carried, segment], below);
  };
  // A route at the shared prefix itself, if any, reads `/`.
  for (const op of root.ops) entries.push({ kind: "op", ...op, label: "/", depth: 0 });
  for (const [segment, child] of root.children) visit(child, [segment], 0);
  return entries;
}
