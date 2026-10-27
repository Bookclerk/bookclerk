// Fuzzing for the Worker's untrusted-input parsers, as the fuzzing ADR asks
// of boundaries outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
// decision 6): each seed in fuzz/<target>/ is replayed, then seeded mutations
// of the seeds are checked against an independent oracle. Run with `npm test`;
// FUZZ_SEED and FUZZ_ITERATIONS pick another run, and a failure prints the
// seed and input that reproduce it.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath, URL } from "node:url";

import { renderMermaidSVG } from "beautiful-mermaid";

import { cleanSvg, diagram } from "./diagram.ts";
import { HTTP_METHODS } from "./api-reference.ts";
import { highlight } from "./highlight.ts";
import { datedPages, formatDate, MAX_DOC_KEY_LENGTH, pageDate } from "./last-updated.ts";
import { renderDoc } from "./render.ts";
import { pickEncoding } from "./worker.ts";
import type { Encoding } from "./worker.ts";

/** This directory. */
const HERE = fileURLToPath(new URL(".", import.meta.url));

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0x5eed);

/** Mutated inputs checked per target. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 20000);

/** Fragments Accept-Encoding mutations splice in: list and parameter syntax, codings, odd qvalues and whitespace. */
const FRAGMENTS = [",", ";", "=", " ", "\t", "q", "Q", "q=", "br", "BR", "gzip", "*", "identity", "0", "1", ".", "0.5", "1.000", "1.0001", ".5", "0x1", "1e0", "-0", " ", "\u0000", "é"];

/**
 * Seeds a small deterministic PRNG (mulberry32).
 *
 * @param seed - Any 32-bit integer.
 * @returns A generator of floats in [0, 1).
 */
function prng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * Applies one to four random edits to an input.
 *
 * @param input - The input to mutate.
 * @param seeds - Every seed, for splicing.
 * @param rand - The PRNG.
 * @param fragments - Snippets of the target's syntax to splice in.
 * @returns The mutated input.
 */
function mutate(input: string, seeds: readonly string[], rand: () => number, fragments: readonly string[] = FRAGMENTS): string {
  let out = input;
  const pick = (list: readonly string[]): string => list[Math.floor(rand() * list.length)] ?? "";
  for (let n = 1 + Math.floor(rand() * 4); n > 0; n--) {
    const at = Math.floor(rand() * (out.length + 1));
    switch (Math.floor(rand() * 5)) {
      case 0:
        out = out.slice(0, at) + pick(fragments) + out.slice(at);
        break;
      case 1:
        out = out.slice(0, at) + out.slice(at + 1 + Math.floor(rand() * 4));
        break;
      case 2:
        out = out.slice(0, at) + out.slice(at).toUpperCase();
        break;
      case 3:
        out = `${out}, ${pick(seeds)}`;
        break;
      default:
        out = out.slice(0, at) + out.slice(Math.floor(rand() * (out.length + 1)));
    }
  }
  return out;
}

/**
 * Reads a qvalue the way RFC 9110 §12.4.2 spells it, without a regular
 * expression: "0" or "1", optionally followed by "." and up to three digits,
 * which must be zeros after a "1".
 *
 * @param text - The parameter value.
 * @returns The weight, or 0 for anything else.
 */
function oracleQvalue(text: string): number {
  if (text !== "0" && text !== "1" && !text.startsWith("0.") && !text.startsWith("1.")) return 0;
  const decimals = text.slice(2);
  if (decimals.length > 3) return 0;
  for (const c of decimals) {
    if (c < "0" || c > "9") return 0;
    if (text[0] === "1" && c !== "0") return 0;
  }
  return text[0] === "1" ? 1 : Number(`0.${decimals || "0"}`);
}

/**
 * The coding an Accept-Encoding header should get, computed independently of
 * worker.ts: every member's weight is collected first, then Brotli and gzip
 * are compared with `*` standing in for an unlisted coding.
 *
 * @param header - The header.
 * @returns The expected choice.
 */
function oracleEncoding(header: string): Encoding | null {
  const weights: Record<string, number> = Object.create(null);
  for (const member of header.split(",")) {
    const [name = "", ...params] = member.split(";");
    const coding = name.trim().toLowerCase();
    if (coding === "") continue;
    const qs = params.filter((p) => (p.split("=")[0] ?? "").trim().toLowerCase() === "q");
    const last = qs.at(-1);
    const weight = last === undefined ? 1 : last.includes("=") ? oracleQvalue(last.slice(last.indexOf("=") + 1).trim()) : 0;
    weights[coding] = Math.max(weights[coding] ?? 0, weight);
  }
  const of = (coding: string): number => (coding in weights ? (weights[coding] ?? 0) : (weights["*"] ?? 0));
  const br = of("br");
  const gzip = of("gzip");
  if (br === 0 && gzip === 0) return null;
  return br >= gzip ? "br" : "gzip";
}

/**
 * Reads a target's seed corpus.
 *
 * @param target - The directory under fuzz/.
 * @returns The seeds.
 */
function corpus(target: string): string[] {
  const dir = join(HERE, "fuzz", target);
  return readdirSync(dir)
    .sort()
    .map((name) => readFileSync(join(dir, name), "utf8"));
}

test("pickEncoding agrees with the oracle on its seed corpus", () => {
  const seeds = corpus("accept_encoding");
  assert.ok(seeds.length >= 10);
  for (const seed of seeds) assert.equal(pickEncoding(seed), oracleEncoding(seed), JSON.stringify(seed));
});

test("pickEncoding agrees with the oracle on mutated inputs", () => {
  const seeds = corpus("accept_encoding");
  const rand = prng(SEED);
  for (let i = 0; i < ITERATIONS; i++) {
    const input = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand);
    const got = pickEncoding(input);
    assert.ok(got === null || got === "br" || got === "gzip", JSON.stringify(input));
    assert.equal(got, oracleEncoding(input), `FUZZ_SEED=${SEED} iteration ${i}: ${JSON.stringify(input)}`);
  }
});

/** Fragments code-block mutations splice in: quotes, escapes, comment marks, HTML-special characters and names. */
const CODE_FRAGMENTS = ['"', "'", "`", "\\", "#", "//", "/*", "*/", "--", "-", "$", "${", "}", "[", "]", "[[", "=", ":", "\n", " ", "\t", "<", ">", "&", "&amp;", "0x1", "1.5", "true", "fn", "SELECT", "sudo", "|", ";", "é", "\u0000", "<span class=\"hl-c\">"];

/** Language names the highlight seeds name, plus names it does not know. */
const LANGUAGES = ["bash", "sh", "console", "toml", "yaml", "json", "ts", "rust", "python", "sql", "nginx", "http", "text", "", "mermaid", "TOML"];

/** The entities highlight.ts writes, and the characters they stand for. */
const ENTITIES: readonly [string, string][] = [["&amp;", "&"], ["&lt;", "<"], ["&gt;", ">"], ["&quot;", '"'], ["&#39;", "'"]];

/**
 * Reads highlight.ts's output back to text, written without reference to
 * highlight.ts: the output may only be escaped text and non-empty, unnested
 * `<span class="hl-X">` elements with X one of c, s, n, k, v, f.
 *
 * @param html - The output.
 * @returns The text it shows, or null when it holds anything else.
 */
function oracleText(html: string): string | null {
  const open = '<span class="hl-';
  let out = "";
  let inSpan = false;
  let spanText = 0;
  let i = 0;
  while (i < html.length) {
    if (html.startsWith(open, i)) {
      const kind = html[i + open.length] ?? "";
      if (inSpan || !"csnkvf".includes(kind) || kind === "" || html.slice(i + open.length + 1, i + open.length + 3) !== '">') return null;
      inSpan = true;
      spanText = 0;
      i += open.length + 3;
      continue;
    }
    if (html.startsWith("</span>", i)) {
      if (!inSpan || spanText === 0) return null;
      inSpan = false;
      i += "</span>".length;
      continue;
    }
    const c = html[i] ?? "";
    if (c === "<" || c === ">" || c === '"' || c === "'") return null;
    if (c === "&") {
      const entity = ENTITIES.find(([e]) => html.startsWith(e, i));
      if (entity === undefined) return null;
      out += entity[1];
      i += entity[0].length;
    } else {
      out += c;
      i += 1;
    }
    spanText++;
  }
  return inSpan ? null : out;
}

/**
 * Splits a highlight seed into its language (first line) and code (the rest).
 *
 * @param seed - The seed file's text.
 * @returns The language name and the code.
 */
function splitSeed(seed: string): [string, string] {
  const newline = seed.indexOf("\n");
  return newline === -1 ? [seed, ""] : [seed.slice(0, newline), seed.slice(newline + 1)];
}

/**
 * Checks one highlight run against the oracle.
 *
 * @param code - The code block.
 * @param lang - The fence's language.
 * @param context - What to print on failure.
 */
function checkHighlight(code: string, lang: string, context: string): void {
  const html = highlight(code, lang);
  if (html === null) {
    assert.ok(["text", "", "mermaid"].includes(lang), `${context}: no grammar for ${JSON.stringify(lang)}`);
    return;
  }
  assert.equal(oracleText(html), code, `${context}: ${JSON.stringify(code)} -> ${JSON.stringify(html)}`);
  if (lang.toLowerCase() === "console") {
    // Terminal output stays plain: only a `$ ` line, or a line continuing
    // one that ends in an unescaped `\` (an odd run), may carry spans.
    // Spans never cross a newline.
    const codeLines = code.split("\n");
    const htmlLines = html.split("\n");
    assert.equal(htmlLines.length, codeLines.length, context);
    let command = false;
    codeLines.forEach((line, n) => {
      command = line.startsWith("$ ") || (command && /(^|[^\\])(\\\\)*\\$/.test(codeLines[n - 1] ?? ""));
      if (!command) assert.ok(!(htmlLines[n] ?? "").includes("<span"), `${context}: output line ${n} colored in ${JSON.stringify(code)} -> ${JSON.stringify(html)}`);
    });
  }
}

test("highlight keeps its seed corpus intact and markup-free", () => {
  const seeds = corpus("highlight");
  assert.ok(seeds.length >= 10);
  for (const seed of seeds) {
    const [lang, code] = splitSeed(seed);
    checkHighlight(code, lang, "seed");
    assert.notEqual(highlight(code, lang), null, lang);
  }
});

test("highlight keeps mutated code intact and markup-free", () => {
  const seeds = corpus("highlight").map((seed) => splitSeed(seed)[1]);
  const rand = prng(SEED);
  for (let i = 0; i < ITERATIONS; i++) {
    const code = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand, CODE_FRAGMENTS);
    const lang = LANGUAGES[Math.floor(rand() * LANGUAGES.length)] ?? "bash";
    checkHighlight(code, lang, `FUZZ_SEED=${SEED} iteration ${i} (${lang})`);
  }
});

/** Fragments dates-file mutations splice in: JSON syntax, keys, numbers, dates and odd characters. */
const DATE_FRAGMENTS = ['"', "{", "}", "[", "]", ":", ",", "\\", "\\u0000", " ", "\n", "1", "2", "0", "9", "-", "-02-29", "-13-", "1.0", "1e0", "null", "true", '"version"', '"pages"', '"README"', '"__proto__"', "2026-10-26", "<", "&", "é"];

/** Page keys the dates-file fuzzing asks for. */
const DATE_KEYS = ["README", "guide/getting-started", "adr/control-plane", "server/backups", "__proto__", "constructor", "toString", "", "missing"];

/** Full month names, written without reference to last-updated.ts. */
const MONTH_NAMES = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/**
 * The date pageDate should find, computed independently of last-updated.ts:
 * the structure is checked with `in`/`typeof` lookups on a null-prototype copy,
 * and the date with digit-by-digit reading and a days-in-month table.
 *
 * @param json - The dates file.
 * @param key - The page key.
 * @returns The expected `YYYY-MM-DD`, or null.
 */
function oraclePageDate(json: string, key: string): string | null {
  let document: unknown;
  try {
    document = JSON.parse(json);
  } catch {
    return null;
  }
  if (typeof document !== "object" || document === null || document instanceof Array) return null;
  const top: Record<string, unknown> = Object.assign(Object.create(null), document);
  if (top["version"] !== 1) return null;
  const pages = top["pages"];
  if (typeof pages !== "object" || pages === null || pages instanceof Array) return null;
  if (!Object.getOwnPropertyNames(pages).includes(key)) return null;
  const value = Object.getOwnPropertyDescriptor(pages, key)?.value;
  if (typeof value !== "string" || value.length !== 10 || value[4] !== "-" || value[7] !== "-") return null;
  let year = 0;
  let month = 0;
  let day = 0;
  for (let i = 0; i < 10; i++) {
    if (i === 4 || i === 7) continue;
    const code = value.charCodeAt(i) - 48;
    if (code < 0 || code > 9) return null;
    if (i < 4) year = year * 10 + code;
    else if (i < 7) month = month * 10 + code;
    else day = day * 10 + code;
  }
  const leap = (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;
  const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1];
  if (year === 0 || days === undefined || day < 1 || day > days) return null;
  return value;
}

/**
 * Checks one pageDate run, and the text the page would show, against the oracle.
 *
 * @param json - The dates file.
 * @param key - The page key.
 * @param context - What to print on failure.
 */
function checkPageDate(json: string, key: string, context: string): void {
  const got = pageDate(json, key);
  assert.equal(got, oraclePageDate(json, key), `${context}: ${JSON.stringify(key)} in ${JSON.stringify(json)}`);
  if (got === null) return;
  const shown = formatDate(got);
  const [year = "", month = "", day = ""] = got.split("-");
  assert.equal(shown, `${MONTH_NAMES[Number(month) - 1]?.slice(0, 3)} ${Number(day)}, ${year}`, context);
  assert.match(shown, /^[A-Za-z]{3} [0-9]{1,2}, [0-9]{4}$/, context);
}

test("pageDate agrees with the oracle on its seed corpus", () => {
  const seeds = corpus("last_updated");
  assert.ok(seeds.length >= 10);
  let found = 0;
  for (const seed of seeds) {
    for (const key of DATE_KEYS) {
      checkPageDate(seed, key, "seed");
      if (pageDate(seed, key) !== null) found++;
    }
  }
  assert.ok(found >= 5, "the seeds hold valid dates");
});

test("pageDate agrees with the oracle on mutated inputs", () => {
  const seeds = corpus("last_updated");
  const rand = prng(SEED);
  for (let i = 0; i < ITERATIONS; i++) {
    const json = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand, DATE_FRAGMENTS);
    const key = DATE_KEYS[Math.floor(rand() * DATE_KEYS.length)] ?? "README";
    checkPageDate(json, key, `FUZZ_SEED=${SEED} iteration ${i}`);
  }
});

/**
 * Tells whether a key may become a sitemap URL, written apart from
 * last-updated.ts's pattern: `/`-separated segments, each non-empty, not
 * starting with `.`, and made of ASCII letters, digits, `.`, `_` and `-`.
 *
 * @param key - The page key.
 * @returns True when the sitemap may list it.
 */
function oracleDocKey(key: string): boolean {
  if (key.length === 0 || key.length > MAX_DOC_KEY_LENGTH) return false;
  return key.split("/").every((segment) => {
    if (segment === "" || segment.startsWith(".")) return false;
    for (const ch of segment) {
      const ok = (ch >= "a" && ch <= "z") || (ch >= "A" && ch <= "Z") || (ch >= "0" && ch <= "9") || ch === "." || ch === "_" || ch === "-";
      if (!ok) return false;
    }
    return true;
  });
}

/**
 * Checks one datedPages run against the oracle: null exactly when the
 * document is not a version 1 dates file, else every listed key with a real
 * date that may become a URL, in the file's order, and nothing else.
 *
 * @param json - The dates file.
 * @param context - What to print on failure.
 */
function checkDatedPages(json: string, context: string): void {
  const got = datedPages(json);
  let pages: unknown = null;
  try {
    const document: unknown = JSON.parse(json);
    if (typeof document === "object" && document !== null && !(document instanceof Array)) {
      const top: Record<string, unknown> = Object.assign(Object.create(null), document);
      if (top["version"] === 1 && typeof top["pages"] === "object" && top["pages"] !== null && !(top["pages"] instanceof Array)) pages = top["pages"];
    }
  } catch {
    pages = null;
  }
  if (pages === null) {
    assert.equal(got, null, `${context}: ${JSON.stringify(json)}`);
    return;
  }
  const expected = Object.getOwnPropertyNames(pages)
    .filter((key) => oracleDocKey(key))
    .flatMap((key) => {
      const date = oraclePageDate(json, key);
      return date === null ? [] : [{ key, date }];
    });
  assert.deepEqual(got, expected, `${context}: ${JSON.stringify(json)}`);
}

test("datedPages agrees with the oracle on its seed corpus", () => {
  const seeds = corpus("last_updated");
  let listed = 0;
  for (const seed of seeds) {
    checkDatedPages(seed, "seed");
    listed += datedPages(seed)?.length ?? 0;
  }
  assert.ok(listed >= 5, "the seeds list valid pages");
});

test("datedPages agrees with the oracle on mutated inputs", () => {
  const seeds = corpus("last_updated");
  const rand = prng(SEED ^ 0x51);
  for (let i = 0; i < ITERATIONS; i++) {
    const json = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand, [...DATE_FRAGMENTS, "/", "//", "./", "../", ".", "_"]);
    checkDatedPages(json, `FUZZ_SEED=${SEED} iteration ${i}`);
  }
});

/** Fragments diagram-source mutations splice in: Mermaid arrows, brackets, directives and HTML-special text. */
const DIAGRAM_FRAGMENTS = ["-->", "---", "-.->", "==>", "--", "|", "[", "]", "(", ")", "{", "}", "((", "[(", ">", "<", "&", '"', "'", ":", ";", "\n", " ", "%%", "subgraph X", "end", "direction LR", "<br/>", "<b>", "<i>", "style A fill:#000", "classDef c fill:red", "url(", "url(#x)", "javascript:", "onload=", "<script>", "&amp;", "é", "\u0000", "[*]", "note right of A: x"];

/** Fragments SVG mutations splice in: tags, attributes, references and the markup cleanSvg must refuse. */
const SVG_FRAGMENTS = ["<", ">", "/>", "</g>", "<g>", '"', "=", " ", "<style>", "</style>", "<script>", "<foreignObject>", "<a href=\"x\">", ' onload="x"', ' href="#x"', ' xlink:href="x"', ' style="fill:red"', ' class="x"', ' data-x="y"', ' fill="url(#arrowhead)"', ' fill="url(http://x/)"', ' id="x y"', "&", "&amp;", "&#60;", "<!--", "-->", "<![CDATA[", "<svg>", "</svg>", "<text>", "</text>", "é", "\u0000"];

/** Elements a drawn diagram may hold, written apart from diagram.ts's list. */
const SVG_ELEMENTS = /^(svg|g|defs|marker|path|polygon|polyline|line|rect|circle|ellipse|text|tspan)$/;

/**
 * Checks markup the docs page will embed as a diagram, independently of
 * diagram.ts: it splits on `<…>` runs and checks each tag and text, then
 * that elements nest, that the root is one `<svg>`, that no attribute can run
 * script, style, or load anything, that the only class is `mono`, that
 * paints are the site's variables rather than literal colors, and that every
 * id and `url(#…)` carries the page prefix.
 *
 * @param svg - The markup.
 * @param prefix - The id prefix it was drawn with.
 * @param context - What to print on failure.
 */
function checkSvg(svg: string, prefix: string, context: string): void {
  const stack: string[] = [];
  let roots = 0;
  for (const part of svg.split(/(<[^<>]*>)/)) {
    if (!part.startsWith("<")) {
      assert.ok(!part.includes("<") && !part.includes(">"), `${context}: stray bracket in text ${JSON.stringify(part)}`);
      assert.ok(!/&(?!(?:[a-zA-Z]+|#[0-9]+|#x[0-9a-fA-F]+);)/.test(part), `${context}: bare & in ${JSON.stringify(part)}`);
      if (stack.length === 0) assert.equal(part.trim(), "", `${context}: text outside the svg`);
      continue;
    }
    const end = /^<\/([^\s>]+)>$/.exec(part);
    if (end !== null) {
      assert.equal(stack.pop(), end[1], `${context}: unbalanced ${part}`);
      continue;
    }
    const tag = /^<([^\s/>]+)((?: [^\s=]+="[^"]*")*)( \/)?>$/.exec(part);
    assert.ok(tag !== null, `${context}: malformed tag ${part}`);
    const [, name = "", attrs = "", self] = tag;
    assert.match(name, SVG_ELEMENTS, `${context}: element ${name}`);
    if (stack.length === 0) {
      roots++;
      assert.equal(name, "svg", context);
    }
    for (const [, key = "", value = ""] of attrs.matchAll(/ ([^\s=]+)="([^"]*)"/g)) {
      const lower = key.toLowerCase();
      assert.ok(!lower.startsWith("on") && !lower.includes("href") && !["style", "src"].includes(lower) && !lower.startsWith("data-"), `${context}: attribute ${key}`);
      if (lower === "class") assert.equal(value, "mono", `${context}: class ${value}`);
      if (lower === "fill" || lower === "stroke") assert.ok(value === "none" || value.startsWith("var(--") || value.startsWith(`url(#${prefix}-`), `${context}: literal paint ${key}="${value}"`);
      assert.ok(!/&(?!(?:[a-zA-Z]+|#[0-9]+|#x[0-9a-fA-F]+);)/.test(value), `${context}: bare & in ${key}`);
      if (lower === "id") assert.ok(value.startsWith(`${prefix}-`), `${context}: id ${value}`);
      for (const ref of value.matchAll(/url\s*\(([^)]*)\)?/gi)) assert.ok((ref[1] ?? "").startsWith(`#${prefix}-`) && ref[0].endsWith(")"), `${context}: reference ${ref[0]}`);
      assert.ok(!value.includes("\\"), `${context}: escape in ${key}`);
      assert.ok(!/javascript:|@import|https?:/i.test(value) || lower === "xmlns", `${context}: ${key}="${value}"`);
    }
    if (self === undefined) stack.push(name);
  }
  assert.deepEqual(stack, [], `${context}: unclosed elements`);
  assert.equal(roots, 1, `${context}: one root`);
}

/** Mutated diagram sources drawn per run: each layout takes milliseconds, so fewer than the other targets. */
const DIAGRAM_ITERATIONS = Math.max(1, Math.floor(ITERATIONS / 40));

test("diagram draws its seed corpus as clean SVG, or leaves it as code", () => {
  const seeds = corpus("diagram");
  assert.ok(seeds.length >= 10);
  let drawn = 0;
  for (const seed of seeds) {
    const one = diagram(seed, "seed");
    if (one === null) continue;
    drawn++;
    checkSvg(one.svg, "seed", JSON.stringify(seed.slice(0, 40)));
  }
  // Everything but the unsupported type and the stylesheet injection draws.
  assert.equal(drawn, seeds.length - 2);
});

test("diagram keeps mutated sources clean or leaves them as code", () => {
  const seeds = corpus("diagram");
  const rand = prng(SEED);
  for (let i = 0; i < DIAGRAM_ITERATIONS; i++) {
    const source = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand, DIAGRAM_FRAGMENTS);
    const drawn = diagram(source, "f");
    if (drawn !== null) checkSvg(drawn.svg, "f", `FUZZ_SEED=${SEED} iteration ${i}: ${JSON.stringify(source)}`);
  }
});

test("cleanSvg passes only clean markup on mutated renderer output", () => {
  // The renderer's own SVG, before cleaning: its <style>, font import,
  // classes, data-* attributes and inline style are what cleanSvg sees.
  // Every seed the site draws must come through cleanSvg from raw output.
  const svgs = corpus("diagram").flatMap((seed) => {
    let svg: string;
    try {
      svg = renderMermaidSVG(seed, { transparent: true });
    } catch {
      return [];
    }
    if (diagram(seed, "c") !== null) assert.notEqual(cleanSvg(svg, "c"), null, svg.slice(0, 200));
    return [svg];
  });
  assert.ok(svgs.length >= 10);
  const rand = prng(SEED);
  for (let i = 0; i < ITERATIONS; i++) {
    const input = mutate(svgs[Math.floor(rand() * svgs.length)] ?? "", svgs, rand, SVG_FRAGMENTS);
    const out = cleanSvg(input, "c");
    if (out !== null) checkSvg(out, "c", `FUZZ_SEED=${SEED} iteration ${i}: ${JSON.stringify(input.slice(0, 2000))}`);
  }
});

/** Fragments Web API page mutations splice in: headings, code spans, methods, quotes, lists, tables and HTML. */
const API_FRAGMENTS = ["\n", "\n\n", "## ", "### ", "#", "`", "``", "> ", "- ", "1. ", "    ", "|", " | ", "| --- |", "| Status |", "GET ", "POST ", "get ", "/api/", "{id}", " ", "Example request", "Example response", "Responses", "200", "4XX", "default", "```", "```json\n", "---", "===", "<", ">", "&", '"', "<section>", "</div>", "é", "\u0000"];

/** Elements that never take a closing tag. */
const VOID_ELEMENTS = /^(br|hr|img|input|wbr|col|meta|link|source)$/;

/**
 * Checks a rendered Web API page against an oracle that does not use
 * api-reference.ts: the HTML nests properly, every operation section is
 * labelled by a heading on the page that carries one badge for an HTTP
 * method, and the visible text is the same as the page rendered without
 * the layout, apart from the badges.
 *
 * @param source - The Markdown.
 * @param context - What to print when a check fails.
 */
function checkApiPage(source: string, context: string): void {
  const laid = renderDoc(source, "api/web/fuzz", { blobBase: "https://example.invalid/" }).html;
  const plain = renderDoc(source, "guide/fuzz", { blobBase: "https://example.invalid/" }).html;
  assert.ok(!/<script/i.test(laid), `script element: ${context}`);

  checkNesting(laid, context);

  const ids = new Set([...laid.matchAll(/ id="([^"]*)"/g)].map((m) => m[1]));
  const sections = [...laid.matchAll(/<section class="api-op" aria-labelledby="([^"]*)">\n<h2 id="([^"]*)" class="api-op-title"><span class="api-method api-method-([a-z]+)">([A-Z]+)<\/span> <code>/g)];
  assert.equal(sections.length, (laid.match(/<section/g) ?? []).length, `section without its heading: ${context}`);
  for (const [, labelled, id, lower, method] of sections) {
    assert.equal(labelled, id, context);
    assert.ok(ids.has(id), context);
    assert.ok((HTTP_METHODS as readonly string[]).includes(method ?? ""), context);
    assert.equal(lower, method?.toLowerCase(), context);
  }
  const badges = (laid.match(/<span class="api-method /g) ?? []).length;
  assert.equal(badges, sections.length, `badge outside an operation heading: ${context}`);

  // Same characters as the page without the layout: the badge's method
  // was part of the heading's code span there. Splitting on tags keeps
  // the text runs (even indices) and the tags before them (odd ones).
  const chars = (html: string): string => {
    const parts = html.split(/(<[^>]*>)/);
    const text = parts.filter((_part, i) => i % 2 === 0 && !(parts[i - 1] ?? "").startsWith('<span class="api-method '));
    return [...text.join("").replace(/\s/g, "")].sort().join("");
  };
  const methods = sections.map((m) => m[4] ?? "").join("");
  assert.equal(chars(laid + methods), chars(plain), `text changed: ${context}`);
}

/** Fragments Web API index mutations splice in, on top of {@link API_FRAGMENTS}: links, group headings and the Quick start. */
const INDEX_FRAGMENTS = [...API_FRAGMENTS, "Quick start", "## Quick start\n\n", "### [", "[", "]", "](jobs.md)", "](x.md#a)", "[`GET /x`](x.md)", "(deprecated)", "<aside>", "</section>"];

/**
 * Checks a rendered Web API index against an oracle that does not use
 * api-reference.ts: the HTML nests properly, every card and the Quick
 * start aside are labelled by their own heading, every badge names a known
 * method, and the visible text is the same as the page rendered without
 * the layout (a badge and its path spell out the code span they replace).
 *
 * @param source - The Markdown.
 * @param context - What to print when a check fails.
 */
function checkApiIndex(source: string, context: string): void {
  const laid = renderDoc(source, "api/web/README", { blobBase: "https://example.invalid/" }).html;
  const plain = renderDoc(source, "guide/fuzz", { blobBase: "https://example.invalid/" }).html;
  assert.ok(!/<script/i.test(laid), `script element: ${context}`);
  checkNesting(laid, context);

  const ids = new Set([...laid.matchAll(/ id="([^"]*)"/g)].map((m) => m[1]));
  const cards = [...laid.matchAll(/<section class="api-group" aria-labelledby="([^"]*)">\n<h3 id="([^"]*)"><a /g)];
  assert.equal(cards.length, (laid.match(/<section/g) ?? []).length, `card without its heading: ${context}`);
  const asides = [...laid.matchAll(/<aside class="api-intro-side" aria-labelledby="([^"]*)">\n<h2 id="([^"]*)">Quick start<\/h2>/g)];
  assert.equal(asides.length, (laid.match(/<aside/g) ?? []).length, `aside without its heading: ${context}`);
  assert.ok(asides.length <= 1, context);
  for (const [, labelled, id] of [...cards, ...asides]) {
    assert.equal(labelled, id, context);
    assert.ok(ids.has(id), context);
  }
  for (const [, lower, method] of laid.matchAll(/<span class="api-method api-method-([a-z]+)">([^<]*)<\/span>/g)) {
    assert.ok((HTTP_METHODS as readonly string[]).includes(method ?? ""), context);
    assert.equal(lower, method?.toLowerCase(), context);
  }
  const chars = (html: string): string => [...html.split(/<[^>]*>/).join("").replace(/\s/g, "")].sort().join("");
  assert.equal(chars(laid), chars(plain), `text changed: ${context}`);
}

/**
 * Checks that every element in some HTML is closed, in order.
 *
 * @param html - The HTML.
 * @param context - What to print when the check fails.
 */
function checkNesting(html: string, context: string): void {
  const stack: string[] = [];
  for (const [tag, close, name] of html.matchAll(/<(\/?)([a-zA-Z][a-zA-Z0-9]*)\b[^>]*>/g)) {
    const element = (name ?? "").toLowerCase();
    if (VOID_ELEMENTS.test(element) || tag.endsWith("/>")) continue;
    if (close === "") stack.push(element);
    else assert.equal(stack.pop(), element, `misnested </${element}>: ${context}`);
  }
  assert.deepEqual(stack, [], `unclosed elements: ${context}`);
}

/** Mutated Web API pages checked per run; each renders twice, highlighting included. */
const API_ITERATIONS = Math.max(1, Math.floor(ITERATIONS / 40));

test("the Web API layout keeps its seed corpus well formed and complete", () => {
  const seeds = corpus("api_reference");
  assert.ok(seeds.length >= 10);
  for (const seed of seeds) checkApiPage(seed, JSON.stringify(seed.slice(0, 60)));
});

test("the Web API layout keeps mutated pages well formed and complete", () => {
  const seeds = corpus("api_reference");
  const rand = prng(SEED);
  for (let i = 0; i < API_ITERATIONS; i++) {
    const source = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand, API_FRAGMENTS);
    checkApiPage(source, `FUZZ_SEED=${SEED} iteration ${i}: ${JSON.stringify(source)}`);
  }
});

test("the Web API index layout keeps its seed corpus well formed and complete", () => {
  const seeds = corpus("api_index");
  assert.ok(seeds.length >= 5);
  for (const seed of seeds) checkApiIndex(seed, JSON.stringify(seed.slice(0, 60)));
});

test("the Web API index layout keeps mutated pages well formed and complete", () => {
  const seeds = corpus("api_index");
  const rand = prng(SEED);
  for (let i = 0; i < API_ITERATIONS; i++) {
    const source = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand, INDEX_FRAGMENTS);
    checkApiIndex(source, `FUZZ_SEED=${SEED} iteration ${i}: ${JSON.stringify(source)}`);
  }
});
