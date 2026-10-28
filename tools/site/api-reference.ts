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
      out.push(
        html('<div class="api-op-examples">\n'),
        ...layoutRequests(layoutResponses(examples, make, id), make, id),
        html("</div>\n"),
      );
    out.push(html("</div>\n</section>\n"));
    at = end;
  }
  out.push(...tokens.slice(at), html("</div>\n"));
  return out;
}

/** The heading of the examples subsection {@link layoutResponses} lays out. */
const EXAMPLE_RESPONSE = "Example response";

/**
 * Finds where an examples subsection ends: at the next top-level `###`
 * heading, or the page's end. An `html_block` just before that heading was
 * put there by the other examples layout (the page itself has no raw HTML),
 * so it stays outside this subsection and the two cards do not nest.
 *
 * @param tokens - An operation's example subsections.
 * @param start - The index of the subsection's `heading_open`.
 * @returns The index of the first token after the subsection.
 */
function subsectionEnd(tokens: readonly Token[], start: number): number {
  const isH3 = (t: Token | undefined): boolean =>
    t?.type === "heading_open" && t.tag === "h3" && t.level === 0;
  const nextH3 = tokens.findIndex((t, i) => i > start && isH3(t));
  let end = nextH3 < 0 ? tokens.length : nextH3;
  while (end > start + 3 && tokens[end - 1]?.type === "html_block") end -= 1;
  return end;
}

/** The heading of the examples subsection {@link layoutRequests} lays out. */
const EXAMPLE_REQUEST = "Example request";

/**
 * The name shown for each language a request sample is written in, by the
 * fence's language name. A language not listed is shown by that name.
 */
const SAMPLE_LANGUAGES: ReadonlyMap<string, string> = new Map([
  ["console", "Shell"],
  ["bash", "Shell"],
  ["sh", "Shell"],
  ["shell", "Shell"],
  ["powershell", "PowerShell"],
  ["pwsh", "PowerShell"],
  ["python", "Python"],
  ["py", "Python"],
  ["javascript", "JavaScript"],
  ["js", "JavaScript"],
  ["typescript", "TypeScript"],
  ["ts", "TypeScript"],
  ["rust", "Rust"],
  ["rs", "Rust"],
]);

/**
 * Lays out an operation's `### Example request` subsection as a card with
 * one panel per language, the way OpenAPI viewers offer a request in several
 * languages: each fenced block directly under the heading becomes a
 * `div.api-request` (id `<operation id>-request-<language>`) carrying the
 * fence's language name in `data-lang` and the name to show in
 * `data-label`. docs.js turns the panels into a language picker; without it
 * they stack. Text between the blocks, and anything after the first
 * non-fence block, passes through.
 *
 * @param tokens - An operation's example subsections.
 * @param make - markdown-it's token constructor.
 * @param id - The operation heading's id, which panel ids start with.
 * @returns The new token list.
 */
export function layoutRequests(tokens: readonly Token[], make: MakeToken, id: string): Token[] {
  const html = (content: string): Token => {
    const token = make("html_block", "", 0);
    token.content = content;
    return token;
  };
  const isH3 = (t: Token | undefined): boolean =>
    t?.type === "heading_open" && t.tag === "h3" && t.level === 0;
  const start = tokens.findIndex((t, i) => isH3(t) && tokens[i + 1]?.content === EXAMPLE_REQUEST);
  if (start < 0) return [...tokens];
  const end = subsectionEnd(tokens, start);
  const out: Token[] = [...tokens.slice(0, start), html('<div class="api-requests">\n')];
  out.push(...tokens.slice(start, start + 3));
  const used = new Set<string>();
  let i = start + 3;
  // Only a top-level fence is a sample: one in a list or quote stays where
  // it is, since a panel opened there would close outside it.
  for (; i < end; i++) {
    const token = tokens[i];
    if (token?.type !== "fence" || token.level !== 0) break;
    const lang = token.info.trim().split(/\s+/)[0]?.toLowerCase() ?? "";
    if (lang === "" || used.has(lang)) break;
    used.add(lang);
    const label = SAMPLE_LANGUAGES.get(lang) ?? lang;
    out.push(
      html(
        `<div class="api-request" id="${escapeCode(`${id}-request-${lang}`)}" data-lang="${escapeCode(lang)}" data-label="${escapeCode(label)}">\n`,
      ),
      token,
      html("</div>\n"),
    );
  }
  out.push(...tokens.slice(i, end), html("</div>\n"), ...tokens.slice(end));
  return out;
}

/**
 * Reads a status line of an example response: a paragraph whose inline
 * text starts with `` Status `200`: ``.
 *
 * @param inline - The paragraph's `inline` token.
 * @returns The status, or null for any other paragraph.
 */
function responseStatusOf(inline: Token | undefined): string | null {
  const children = inline?.children ?? [];
  const lead = children[0];
  const status = children[1];
  const after = children[2];
  if (lead?.type !== "text" || lead.content !== "Status ") return null;
  if (status?.type !== "code_inline" || !/^(?:[1-5]\d\d|[1-5]XX|default)$/i.test(status.content))
    return null;
  if (after?.type !== "text" || !after.content.startsWith(":")) return null;
  return status.content;
}

/**
 * Lays out an operation's `### Example response` subsection as a card with
 * one panel per status, the way OpenAPI viewers show example responses:
 * each `` Status `200`: <description> `` paragraph and the JSON block after
 * it become a `div.api-response` (id `<operation id>-response-<status>`)
 * whose caption carries a colored status mark (`span.api-response-code`,
 * which keeps every character of `` Status `200`: ``). What follows the
 * caption is the panel's example view (`div.api-response-example`); a
 * `Schema:` paragraph in the panel starts its schema view
 * (`div.api-response-schema`, the paragraph marked `api-response-view-label`),
 * which holds the body's JSON Schema. docs.js turns the panels into tabs and
 * the two views into an Example/Schema switch; without it they stack.
 * Everything else passes through.
 *
 * @param tokens - An operation's example subsections.
 * @param make - markdown-it's token constructor.
 * @param id - The operation heading's id, which panel ids start with.
 * @returns The new token list.
 */
export function layoutResponses(tokens: readonly Token[], make: MakeToken, id: string): Token[] {
  const html = (content: string): Token => {
    const token = make("html_block", "", 0);
    token.content = content;
    return token;
  };
  const isH3 = (t: Token | undefined): boolean =>
    t?.type === "heading_open" && t.tag === "h3" && t.level === 0;
  const start = tokens.findIndex((t, i) => isH3(t) && tokens[i + 1]?.content === EXAMPLE_RESPONSE);
  if (start < 0) return [...tokens];
  const end = subsectionEnd(tokens, start);
  // A status line is a top-level paragraph: one in a list or quote stays
  // where it is, since a panel opened there would close outside it.
  const statusAt = (i: number): string | null =>
    tokens[i]?.type === "paragraph_open" && tokens[i]?.level === 0
      ? responseStatusOf(tokens[i + 1])
      : null;
  // The heading and anything before the first status line stay as they are.
  let first = start + 3;
  while (first < end && statusAt(first) === null) first += 1;
  const out: Token[] = [...tokens.slice(0, start), html('<div class="api-responses">\n')];
  out.push(...tokens.slice(start, first));
  let open = false;
  // Inside a panel: the caption, then the example view, then the schema view.
  let view: "caption" | "example" | "schema" = "caption";
  const closePanel = (): void => {
    if (view !== "caption") out.push(html("</div>\n"));
    out.push(html("</div>\n"));
  };
  const used = new Set<string>();
  for (let i = first; i < end; i++) {
    const token = tokens[i];
    if (token === undefined) continue;
    const found = statusAt(i);
    if (found !== null) {
      if (open) closePanel();
      view = "caption";
      const slug = found.toLowerCase();
      // A status listed twice keeps its first panel's id unique.
      const panel = used.has(slug) ? `${id}-response-${slug}-${i}` : `${id}-response-${slug}`;
      used.add(slug);
      out.push(
        html(
          `<div class="api-response" id="${escapeCode(panel)}" data-status="${escapeCode(found)}">\n`,
        ),
      );
      open = true;
      token.attrJoin("class", "api-response-status");
      const inline = tokens[i + 1];
      const children = inline?.children ?? [];
      // `Status `200`: text` becomes a status mark and the text; the word
      // stays for screen readers, and tabs hide the whole mark.
      const after = children[2];
      if (inline !== undefined && after !== undefined) {
        const mark = make("html_inline", "", 0);
        mark.content =
          `<span class="api-response-code"><span class="visually-hidden">Status </span>` +
          `<span class="${statusClass(found)}">${escapeCode(found)}</span>:</span>`;
        after.content = after.content.slice(1);
        inline.children = [mark, ...children.slice(2)];
      }
    } else if (open && view === "example" && isSchemaLabel(tokens, i)) {
      out.push(html('</div>\n<div class="api-response-schema">\n'));
      view = "schema";
      token.attrJoin("class", "api-response-view-label");
    }
    out.push(token);
    if (open && view === "caption" && token.type === "paragraph_close" && token.level === 0) {
      out.push(html('<div class="api-response-example">\n'));
      view = "example";
    }
  }
  if (open) closePanel();
  out.push(html("</div>\n"), ...tokens.slice(end));
  return out;
}

/**
 * Whether the token at `i` opens a top-level paragraph that reads `Schema:`,
 * the line the generator puts before a response body's JSON Schema.
 *
 * @param tokens - The token list.
 * @param i - The index of a candidate `paragraph_open` token.
 * @returns True for the schema label.
 */
function isSchemaLabel(tokens: readonly Token[], i: number): boolean {
  const token = tokens[i];
  const inline = tokens[i + 1];
  if (token?.type !== "paragraph_open" || token.level !== 0 || inline?.type !== "inline")
    return false;
  const children = inline.children ?? [];
  return children.length === 1 && children[0]?.type === "text" && children[0].content === "Schema:";
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

/** Where a parameter goes, as the generator writes it after the name. */
const PARAMETER_LOCATIONS: ReadonlySet<string> = new Set(["path", "query", "header", "cookie"]);

/**
 * Lays out the property lists of a Web API page as property rows, the way
 * OpenAPI viewers show a body: the generator writes each parameter, body
 * field and response as a list item (`` `name` (path) *type*, **required** —
 * description `` and `` `200` — description Body: … ``), which reads as a
 * list on GitHub. Here a list whose every item is one of those becomes
 * `ul.api-props`; each item is `li.api-prop` whose head holds the name,
 * location, type and required mark in spans, with the description after it,
 * and a nested list (an inline object's fields, or a response's) folds into
 * a `details`. A response item is `li.api-prop api-response-item` with a
 * status mark; the first response's fold starts open and docs.js keeps one
 * open at a time. Lists with any other item pass through, as do tables, so
 * pages generated before this format still render.
 *
 * @param tokens - The page's block tokens.
 * @param make - markdown-it's token constructor.
 * @returns The same token list, with the lists re-marked.
 */
export function layoutProperties(tokens: readonly Token[], make: MakeToken): Token[] {
  const out = [...tokens];
  const html = (content: string): Token => {
    const token = make("html_block", "", 0);
    token.content = content;
    return token;
  };
  // Innermost lists first, so a nested list is already laid out (and its
  // item count known) when its parent folds it.
  const lists: { open: number; close: number }[] = [];
  const stack: number[] = [];
  out.forEach((token, i) => {
    if (token.type === "bullet_list_open") stack.push(i);
    if (token.type === "bullet_list_close") {
      const open = stack.pop();
      if (open !== undefined) lists.push({ open, close: i });
    }
  });
  const inserts: { at: number; token: Token }[] = [];
  for (const { open, close } of lists) {
    const items: { open: number; close: number; inline: Token }[] = [];
    let depth = 0;
    for (let i = open + 1; i < close; i++) {
      const token = out[i];
      if (token === undefined) continue;
      if (token.type === "list_item_open" && depth === 0) {
        const inline = out[i + 2];
        if (out[i + 1]?.type !== "paragraph_open" || inline?.type !== "inline") {
          items.length = 0;
          break;
        }
        let end = i + 3;
        let inner = 1;
        while (end < close && inner > 0) {
          if (out[end]?.type === "list_item_open") inner += 1;
          if (out[end]?.type === "list_item_close") inner -= 1;
          end += 1;
        }
        items.push({ open: i, close: end - 1, inline });
      }
      if (token.nesting === 1) depth += 1;
      if (token.nesting === -1) depth -= 1;
    }
    if (items.length === 0) continue;
    const parsed = items.map((item) => propertyItem(item.inline.children ?? []));
    if (parsed.some((p) => p === null)) continue;
    out[open]?.attrJoin("class", "api-props");
    // The first response body's fields start open; docs.js keeps one open.
    let opened = false;
    items.forEach((item, n) => {
      const prop = parsed[n];
      if (prop === null || prop === undefined) return;
      out[item.open]?.attrJoin(
        "class",
        prop.kind === "response" ? "api-prop api-response-item" : "api-prop",
      );
      item.inline.children = propertyChildren(prop, make);
      // A nested list (fields of an object or of a response body) folds.
      for (let i = item.open + 3; i < item.close; i++) {
        const token = out[i];
        if (token?.type !== "bullet_list_open" || token.level !== (out[item.open]?.level ?? 0) + 1)
          continue;
        // A nested list that is not a property list stays a plain list.
        if (!String(token.attrGet("class") ?? "").includes("api-props")) break;
        let end = i + 1;
        while (
          end < item.close &&
          !(out[end]?.type === "bullet_list_close" && out[end]?.level === token.level)
        )
          end += 1;
        const count = out
          .slice(i, end)
          .filter((t) => t.type === "list_item_open" && t.level === token.level + 1).length;
        const noun = count === 1 ? "field" : "fields";
        // An object's fields start folded; the first response body's start open.
        const state = prop.kind === "response" && !opened ? " open" : "";
        if (prop.kind === "response") opened = true;
        inserts.push({
          at: i,
          token: html(
            `<details class="api-prop-nested"${state}><summary>${count} ${noun}</summary>\n`,
          ),
        });
        inserts.push({ at: end + 1, token: html("</details>\n") });
        break;
      }
    });
  }
  // Insert from the back so earlier indices stay valid.
  inserts.sort((a, b) => b.at - a.at);
  for (const { at, token } of inserts) out.splice(at, 0, token);
  return out;
}

/** One property item of a generated list, as {@link propertyItem} reads it. */
interface PropertyItem {
  /** A parameter or body field, or a response. */
  kind: "property" | "response";
  /** The name, or the status; the leading code span. */
  name: string;
  /** Where a parameter goes, or "". */
  location: string;
  /** The inline tokens of the type (a response has none). */
  type: Token[];
  /** Whether the property is marked required. */
  required: boolean;
  /** The inline tokens of the description, after the em dash. */
  description: Token[];
}

/**
 * Reads a list item's inline tokens as a property or response item.
 *
 * @param inline - The item's inline children.
 * @returns The item, or null when the tokens are not in the generator's form.
 */
export function propertyItem(inline: readonly Token[]): PropertyItem | null {
  // markdown-it leaves an empty text token after a closing `**`.
  const children = inline.filter((t) => !(t.type === "text" && t.content === ""));
  const name = children[0];
  if (name?.type !== "code_inline") return null;
  let i = 1;
  const text = (): string => (children[i]?.type === "text" ? (children[i]?.content ?? "") : "");
  // A response: `` `200` — description `` or `` `204` — No body. ``
  if (/^(?:[1-5]\d\d|[1-5]XX|default)$/i.test(name.content) && text().startsWith(" — ")) {
    const first = Object.assign(Object.create(Object.getPrototypeOf(children[i])), children[i]);
    first.content = text().slice(3);
    return {
      kind: "response",
      name: name.content,
      location: "",
      type: [],
      required: false,
      description: [first, ...children.slice(i + 1)],
    };
  }
  let location = "";
  const where = /^ \((\w+)\) $/.exec(text());
  if (where !== null && PARAMETER_LOCATIONS.has(where[1] ?? "")) {
    location = where[1] ?? "";
    i += 1;
  } else if (text() !== " ") {
    return null;
  } else {
    i += 1;
  }
  if (children[i]?.type !== "em_open") return null;
  i += 1;
  const type: Token[] = [];
  let open = 1;
  for (; i < children.length && open > 0; i++) {
    const token = children[i];
    if (token === undefined) return null;
    if (token.type === "em_open") open += 1;
    if (token.type === "em_close") open -= 1;
    if (open > 0) type.push(token);
  }
  if (open > 0) return null;
  let required = false;
  let description: Token[] = [];
  if (i < children.length) {
    const rest = children.slice(i);
    const [lead, strongOpen, word, strongClose] = rest;
    if (
      lead?.type === "text" &&
      lead.content === ", " &&
      strongOpen?.type === "strong_open" &&
      word?.type === "text" &&
      word.content === "required" &&
      strongClose?.type === "strong_close"
    ) {
      required = true;
      description = rest.slice(4);
    } else {
      description = rest;
    }
    if (description.length > 0) {
      const dash = description[0];
      if (dash?.type !== "text" || !dash.content.startsWith(" — ")) return null;
      const first = Object.assign(Object.create(Object.getPrototypeOf(dash)), dash);
      first.content = dash.content.slice(3);
      description = [first, ...description.slice(1)];
    }
  }
  return { kind: "property", name: name.content, location, type, required, description };
}

/**
 * Writes a property item back as inline tokens with the row's spans.
 *
 * @param prop - The item.
 * @param make - markdown-it's token constructor.
 * @returns The inline children: a `span.api-prop-head` holding the name
 *   (or status mark), location, type and required mark, then the
 *   description in `span.api-prop-desc`.
 */
function propertyChildren(prop: PropertyItem, make: MakeToken): Token[] {
  const html = (content: string): Token => {
    const token = make("html_inline", "", 0);
    token.content = content;
    return token;
  };
  const out: Token[] = [html('<span class="api-prop-head">')];
  if (prop.kind === "response") {
    out.push(html(`<span class="${statusClass(prop.name)}">${escapeCode(prop.name)}</span>`));
  } else {
    const name = make("code_inline", "code", 0);
    name.content = prop.name;
    name.markup = "`";
    name.attrJoin("class", "api-prop-name");
    out.push(name);
    if (prop.location !== "")
      out.push(html(`<span class="api-prop-in">${escapeCode(prop.location)}</span>`));
    out.push(html('<span class="api-prop-type">'), ...prop.type, html("</span>"));
    if (prop.required) out.push(html('<span class="api-prop-required">required</span>'));
  }
  out.push(html("</span>"));
  if (prop.description.length > 0)
    out.push(html('<span class="api-prop-desc">'), ...prop.description, html("</span>"));
  return out;
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
