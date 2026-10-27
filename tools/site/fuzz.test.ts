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

import { highlight } from "./highlight.ts";
import { formatDate, pageDate } from "./last-updated.ts";
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
const LANGUAGES = ["bash", "sh", "console", "toml", "yaml", "json", "ts", "rust", "python", "sql", "text", "", "nginx", "TOML"];

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
    assert.ok(["text", "", "nginx"].includes(lang), `${context}: no grammar for ${JSON.stringify(lang)}`);
    return;
  }
  assert.equal(oracleText(html), code, `${context}: ${JSON.stringify(code)} -> ${JSON.stringify(html)}`);
  if (lang.toLowerCase() === "console") {
    // Terminal output stays plain: only a `$ ` line, or a line continuing
    // one that ends in `\`, may carry spans. Spans never cross a newline.
    const codeLines = code.split("\n");
    const htmlLines = html.split("\n");
    assert.equal(htmlLines.length, codeLines.length, context);
    let command = false;
    codeLines.forEach((line, n) => {
      command = line.startsWith("$ ") || (command && (codeLines[n - 1] ?? "").endsWith("\\"));
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
