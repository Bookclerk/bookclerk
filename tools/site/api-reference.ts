// The Web API reference's layout: the pages scripts/openapi_markdown.py
// writes under api/web/ are plain Markdown that reads well on GitHub, and
// the site lays each operation out the way OpenAPI viewers such as Scalar
// do: a method badge beside the path, the description, parameters and
// responses in one column, and the example request and response in a
// second column beside them on wide screens. Nothing here runs in the
// browser; it rearranges markdown-it tokens before they are rendered.

import type { Token } from "markdown-it";

/** HTTP methods an operation heading can start with. */
export const HTTP_METHODS = ["GET", "PUT", "POST", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE"] as const;

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
  return key.startsWith("api/web/") && key !== "api/web/README" && key !== "api/web/schemas";
}

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
  if (children.length !== 1 || only === undefined || only.type !== "code_inline") return null;
  const match = OPERATION_RE.exec(only.content);
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
  return first !== undefined && /^[1-5]\d\d$|^[1-5]XX$/i.test(status) ? `api-status api-status-${first}xx` : "api-status";
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

  const out: Token[] = [html('<div class="api-ref">\n')];
  let at = 0;
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
    out.push(html('<div class="api-op-body">\n<div class="api-op-main">\n'), ...main, html("</div>\n"));
    if (examples.length > 0) out.push(html('<div class="api-op-examples">\n'), ...examples, html("</div>\n"));
    out.push(html("</div>\n</section>\n"));
    at = end;
  }
  out.push(...tokens.slice(at), html("</div>\n"));
  return out;
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
