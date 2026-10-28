// Diagrams for the docs' ```mermaid fences, drawn by the Worker as inline SVG
// so pages need no client-side script and GitHub and the site read the same
// Markdown. beautiful-mermaid lays the diagram out without a DOM; its SVG
// carries a <style> element (with a Google Fonts import) and colors through
// CSS variables, so the site keeps only the shapes and text and styles them
// from site.css (`.doc .diagram`), in its own fonts and colors.
//
// The source comes from the docs repository, so the SVG is not trusted as
// markup: `cleanSvg` rebuilds it from an allowlist of SVG elements and
// presentation attributes, and anything else makes the fence fall back to a
// plain code block. It is sans-IO and checked by fuzz.test.ts.
//
// Layout costs milliseconds of CPU per diagram (the first in an isolate
// about a quarter of a second, while elkjs starts), so sources are capped
// per fence and per page (render.ts) and the layouts an isolate has made are
// kept for the next request.

import { renderMermaidSVG } from "beautiful-mermaid";

/** Longest diagram source drawn, in characters; longer ones stay code. */
export const DIAGRAM_SOURCE_MAX = 4000;

/** Most diagram source drawn on one page, in characters; later fences stay code. */
export const PAGE_DIAGRAM_MAX = 16000;

/** Most diagrams drawn on one page; later fences stay code. Layout cost is mostly per diagram. */
export const PAGE_DIAGRAM_COUNT_MAX = 12;

/** Layouts kept per isolate, by source; the oldest goes first. */
const LAYOUT_CACHE_SIZE = 64;

/** beautiful-mermaid's SVG for recently drawn sources, or null where it threw. */
const layouts = new Map<string, string | null>();

/** One diagram ready for a docs page. */
export interface Diagram {
  /** The cleaned `<svg>` markup. */
  svg: string;
  /** Its natural width in CSS pixels, from the root's `width`. */
  width: number;
  /** What kind of diagram it is, for its accessible name ("Flowchart"). */
  label: string;
}

/**
 * Diagram types the site draws, by the keyword that opens the source, with
 * the name a screen reader announces for each. These
 * are the types both GitHub's Mermaid and beautiful-mermaid support and the
 * site's colors cover; the writing guide lists the same ones.
 */
const DIAGRAM_TYPES: ReadonlyMap<string, string> = new Map([
  ["flowchart", "Flowchart"],
  ["graph", "Flowchart"],
  ["stateDiagram", "State diagram"],
  ["stateDiagram-v2", "State diagram"],
  ["sequenceDiagram", "Sequence diagram"],
  ["classDiagram", "Class diagram"],
  ["erDiagram", "Entity relationship diagram"],
]);

/** SVG elements kept; see SKIPPED for those dropped, anything else rejects the diagram. */
const ELEMENTS: ReadonlySet<string> = new Set([
  "svg",
  "g",
  "defs",
  "marker",
  "path",
  "polygon",
  "polyline",
  "line",
  "rect",
  "circle",
  "ellipse",
  "text",
  "tspan",
]);

/**
 * Elements dropped with their contents: the renderer's stylesheet (site.css
 * replaces it) and tooltips on ER attributes (the comment is not shown).
 */
const SKIPPED: ReadonlySet<string> = new Set(["style", "title"]);

/** The only `class` kept: monospace member lines in class and ER diagrams. */
const MONO_CLASS = "mono";

/** Attributes kept; `class` (but `mono`), `style` and `data-*` are dropped, anything else rejects the diagram. */
const ATTRIBUTES: ReadonlySet<string> = new Set([
  "xmlns",
  "viewBox",
  "width",
  "height",
  "id",
  "transform",
  "x",
  "y",
  "x1",
  "y1",
  "x2",
  "y2",
  "cx",
  "cy",
  "r",
  "rx",
  "ry",
  "d",
  "points",
  "dy",
  "markerWidth",
  "markerHeight",
  "refX",
  "refY",
  "orient",
  "marker-start",
  "marker-end",
  "fill",
  "stroke",
  "stroke-width",
  "stroke-dasharray",
  "stroke-linecap",
  "stroke-linejoin",
  "font-size",
  "font-weight",
  "font-style",
  "text-anchor",
  "text-decoration",
]);

/**
 * Paint values kept on `fill` and `stroke`: the site's variables, `none` and
 * references inside the diagram. A literal color (from a `style`,
 * `classDef` or `linkStyle` line) would ignore the light and dark palettes,
 * so it rejects the diagram.
 */
const PAINT = /^(?:none|var\(--[A-Za-z0-9_-]+\)|url\(#[A-Za-z][A-Za-z0-9_-]*\))$/;

/** The only `xmlns` value kept. */
const SVG_NAMESPACE = "http://www.w3.org/2000/svg";

/**
 * Whether an attribute is dropped without rejecting the diagram.
 *
 * @param name - The attribute's name.
 * @returns True for `class`, `style` and `data-*`.
 */
function dropped(name: string): boolean {
  return name === "class" || name === "style" || name.startsWith("data-");
}

/**
 * The diagram type a Mermaid source declares: the first word of its first
 * line that is not blank or a `%%` comment.
 *
 * @param source - The fence's content.
 * @returns The keyword, or an empty string.
 */
function diagramType(source: string): string {
  for (const line of source.split("\n")) {
    const text = line.trim();
    if (text === "" || text.startsWith("%%")) continue;
    return text.split(/\s/)[0] ?? "";
  }
  return "";
}

/**
 * Rebuilds beautiful-mermaid's SVG from allowed elements and attributes only.
 *
 * The input is read as a run of tags and text: every `<` must open an
 * allowed element or its end tag, every attribute must be `name="value"`
 * whose value uses only the characters numbers, keywords, `var(--…)` and
 * `url(#…)` need, text must hold no `>` or bare `&`, and nothing may follow
 * the root `<svg>`. The `<style>` and `<title>` elements and their contents
 * are dropped, as are `style`, `data-*` and every `class` but `mono`. Each `id`, and each
 * `url(#…)` reference to one, gets `prefix` so several diagrams on a page do
 * not share ids; any other `url(`, in any letter case, rejects the diagram,
 * so nothing in it can load a resource.
 *
 * @param svg - The SVG markup.
 * @param prefix - Prepended to every id; letters, digits and `-` only.
 * @returns The rebuilt markup, or null when the input has anything else.
 */
export function cleanSvg(svg: string, prefix: string): string | null {
  if (!/^[A-Za-z][A-Za-z0-9-]*$/.test(prefix)) return null;
  const out: string[] = [];
  const open: string[] = [];
  let closed = false;
  let i = 0;
  while (i < svg.length) {
    const lt = svg.indexOf("<", i);
    const text = svg.slice(i, lt === -1 ? svg.length : lt);
    if (text.includes(">") || /&(?![a-zA-Z]+;|#[0-9]+;|#x[0-9a-fA-F]+;)/.test(text)) return null;
    if (open.length === 0 && text.trim() !== "") return null;
    if (open.length > 0) out.push(text);
    if (lt === -1) break;
    const gt = svg.indexOf(">", lt);
    if (gt === -1) return null;
    const tag = svg.slice(lt + 1, gt);
    i = gt + 1;
    const close = /^\/([a-zA-Z]+)\s*$/.exec(tag);
    if (close !== null) {
      if (open.pop() !== close[1]) return null;
      out.push(`</${close[1]}>`);
      closed = open.length === 0;
      continue;
    }
    const start = /^([a-zA-Z]+)((?:\s+[a-zA-Z][a-zA-Z0-9:-]*="[^"<]*")*)\s*(\/?)$/.exec(tag);
    if (start === null) return null;
    const [, name = "", attrs = "", selfClosing = ""] = start;
    if (SKIPPED.has(name) && selfClosing === "" && attrs === "" && open.length > 0) {
      const end = svg.indexOf(`</${name}>`, i);
      if (end === -1) return null;
      i = end + name.length + 3;
      continue;
    }
    if (closed || !ELEMENTS.has(name) || (name === "svg") !== (open.length === 0)) return null;
    let kept = "";
    for (const attr of attrs.matchAll(/([a-zA-Z][a-zA-Z0-9:-]*)="([^"]*)"/g)) {
      const [, key = "", raw = ""] = attr;
      if (key === "class" && raw.split(" ").includes(MONO_CLASS)) {
        kept += ` class="${MONO_CLASS}"`;
        continue;
      }
      if (dropped(key)) continue;
      if (!ATTRIBUTES.has(key)) return null;
      let value = raw;
      if (key === "xmlns") {
        if (value !== SVG_NAMESPACE) return null;
      } else if (key === "id") {
        if (!/^[A-Za-z][A-Za-z0-9_-]*$/.test(value)) return null;
        value = `${prefix}-${value}`;
      } else {
        // Numbers, keywords, `var(--…)` and `url(#…)` need nothing else; no
        // `:`, `;`, `/`, `\\` or `&` leaves room to name another resource.
        if (!/^[A-Za-z0-9 #.,%()_+-]*$/.test(value)) return null;
        if ((key === "fill" || key === "stroke") && !PAINT.test(value)) return null;
        if (/url\s*\(/i.test(value)) {
          const ref = /^url\(#([A-Za-z][A-Za-z0-9_-]*)\)$/.exec(value);
          if (ref === null) return null;
          value = `url(#${prefix}-${ref[1]})`;
        }
      }
      kept += ` ${key}="${value}"`;
    }
    out.push(`<${name}${kept}${selfClosing === "" ? "" : " /"}>`);
    if (selfClosing === "") open.push(name);
    if (name === "svg" && selfClosing !== "") return null;
  }
  if (open.length > 0 || out.length === 0) return null;
  return out.join("").replace(/\n\s*\n/g, "\n");
}

/**
 * Draws one Mermaid fence as inline SVG for a docs page.
 *
 * @param source - The fence's content.
 * @param prefix - Unique on the page: prepended to the diagram's ids.
 * @returns The diagram, or null when the source is too long, of a type the
 *   site does not draw, fails to lay out, or comes out with markup `cleanSvg`
 *   rejects; the caller then shows it as a code block.
 */
export function diagram(source: string, prefix: string): Diagram | null {
  if (!drawable(source)) return null;
  const label = DIAGRAM_TYPES.get(diagramType(source)) ?? "Diagram";
  const raw = layout(source);
  const svg = raw === null ? null : cleanSvg(raw.trim(), prefix);
  if (svg === null) return null;
  const width = Number(/^<svg [^>]*?\swidth="([0-9.]+)"/.exec(svg)?.[1]);
  return Number.isFinite(width) && width > 0 ? { svg, width, label } : null;
}

/**
 * Whether a source is short enough and of a type the site draws, checked
 * before any layout so a page's budget is spent only on diagrams it tries.
 *
 * @param source - The fence's content.
 * @returns True when `diagram` will lay it out.
 */
export function drawable(source: string): boolean {
  return source.length <= DIAGRAM_SOURCE_MAX && DIAGRAM_TYPES.has(diagramType(source));
}

/**
 * Lays one source out, or takes the layout this isolate made before.
 *
 * @param source - The fence's content.
 * @returns beautiful-mermaid's SVG, or null when it threw.
 */
function layout(source: string): string | null {
  const known = layouts.get(source);
  if (known !== undefined) {
    // Re-insert so the Map's order runs from least to most recently used.
    layouts.delete(source);
    layouts.set(source, known);
    return known;
  }
  let svg: string | null;
  try {
    svg = withNodeGlobal(() => renderMermaidSVG(source, { transparent: true, padding: 16 }));
  } catch {
    svg = null;
  }
  layouts.set(source, svg);
  if (layouts.size > LAYOUT_CACHE_SIZE) layouts.delete(layouts.keys().next().value ?? "");
  return svg;
}

/**
 * Runs `draw` with `globalThis.global` defined, as it is in Node.
 *
 * beautiful-mermaid lays diagrams out with elkjs, whose bundled build starts
 * on the first layout and looks for Node's `global` to find the global scope
 * (beautiful-mermaid hides `self` while it does, so elkjs runs in-process
 * rather than spawning a web worker). Workers have no `global` without
 * `nodejs_compat`, which the site does not otherwise need, so it is defined
 * for the call and removed again.
 *
 * @param draw - The render call.
 * @returns What `draw` returns.
 */
function withNodeGlobal<T>(draw: () => T): T {
  const scope = globalThis as { global?: unknown };
  if ("global" in scope) return draw();
  scope.global = globalThis;
  try {
    return draw();
  } finally {
    delete scope.global;
  }
}
