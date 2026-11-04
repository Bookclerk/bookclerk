/**
 * Fuzzing for the Audnexus response mapping, as the fuzzing ADR asks of
 * boundaries outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
 * decision 6).
 *
 * Two targets, each with a seed corpus under `fuzz/`: `book` (a
 * `/books/{asin}` body through `recordFromBook`) and `chapters` (a
 * `/books/{asin}/chapters` body through `chaptersFromBody`). Each seed is
 * replayed, then seeded mutations of the seeds (edits to the parsed JSON tree
 * and to its text) are checked against an independent oracle. `FUZZ_SEED` and
 * `FUZZ_ITERATIONS` pick another run; a failure prints the seed, iteration and
 * input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import type { MetadataChapters, MetadataRecord } from "@bookclerk/plugin-sdk/workerd";

import {
  MAX_CHAPTERS,
  MAX_DEPTH,
  MAX_GENRES,
  MAX_PEOPLE,
  MAX_TEXT,
  chaptersFromBody,
  recordFromBook,
  type AsinRequest,
} from "../src/audnexus.ts";

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0x5eed);

/** Mutated inputs checked per target. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 3000);

/**
 * Every seed of one target, as JSON text.
 *
 * @param target - Directory name under `fuzz/`.
 * @returns The seeds, sorted by file name.
 */
function seeds(target: string): string[] {
  const dir = new URL(`../fuzz/${target}/`, import.meta.url);
  return readdirSync(dir)
    .filter((name) => name.endsWith(".json"))
    .sort()
    .map((name) => readFileSync(new URL(name, dir), "utf8"));
}

/** The request a book body is mapped under. */
const REQUEST: AsinRequest = { asin: "B002V0QK4C", region: "uk" };

/** Values a tree mutation puts in place of a node: wrong types, edge numbers and odd strings. */
const VALUES: readonly unknown[] = [
  null,
  true,
  false,
  0,
  -1,
  1.5,
  5,
  5.0001,
  "4.5",
  "007",
  " 12 ",
  "1e3",
  0xffff_ffff,
  0x1_0000_0000,
  Number.MAX_SAFE_INTEGER,
  "9007199254740993",
  1e308,
  "",
  "   ",
  "b002v0qk4c",
  "B002V0QK4",
  "054792822x",
  "9780547928227",
  "http://insecure.example/cover.jpg",
  "https://m.media-amazon.com/images/I/x.jpg",
  "2021-05-04T00:00:00.000Z",
  "May 2021",
  "Unabridged",
  "abridged",
  "genre",
  "tag",
  "\uD800",
  "a\uDC00b",
  "\uD83D\uDCDA",
  "x".repeat(MAX_TEXT - 1) + "\uD83D\uDCDA",
  [],
  {},
  [null, "", { name: "A" }, { name: "A" }, 3],
  { name: "Series", position: 1, asin: "B000000001" },
  [{ title: "Nested", startOffsetMs: 0, chapters: [{ title: "Deeper", startOffsetMs: 5 }] }],
];

/** Keys a tree mutation may add to an object. */
const KEYS: readonly string[] = [
  "asin",
  "title",
  "subtitle",
  "authors",
  "narrators",
  "name",
  "type",
  "genres",
  "seriesPrimary",
  "seriesSecondary",
  "position",
  "isbn",
  "image",
  "formatType",
  "releaseDate",
  "runtimeLengthMin",
  "rating",
  "language",
  "chapters",
  "startOffsetMs",
  "lengthMs",
  "isAccurate",
  "brandIntroDurationMs",
  "brandOutroDurationMs",
  "runtimeLengthMs",
];

/** Fragments a text mutation splices into the JSON. */
const FRAGMENTS: readonly string[] = [
  ",",
  ":",
  "[",
  "]",
  "{",
  "}",
  '"',
  "null",
  "0",
  "-",
  "\\u0000",
];

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
 * Picks one entry of a list.
 *
 * @param list - A non-empty list.
 * @param rand - The PRNG.
 * @returns The entry.
 */
function pick<T>(list: readonly T[], rand: () => number): T {
  return list[Math.floor(rand() * list.length)] as T;
}

/**
 * Every object and array inside a JSON value, the value itself included.
 *
 * @param value - Parsed JSON.
 * @returns The containers, outermost first.
 */
function containers(value: unknown): (Record<string, unknown> | unknown[])[] {
  const out: (Record<string, unknown> | unknown[])[] = [];
  const stack: unknown[] = [value];
  while (stack.length > 0) {
    const v = stack.pop();
    if (typeof v !== "object" || v === null) continue;
    const c = v as Record<string, unknown> | unknown[];
    out.push(c);
    stack.push(...Object.values(c));
  }
  return out;
}

/**
 * Applies one to four random edits to a parsed JSON tree, in place.
 *
 * @param root - The tree.
 * @param donors - Other parsed seeds, whose subtrees may be grafted in.
 * @param rand - The PRNG.
 * @returns The edited tree (a new root when the root itself is replaced).
 */
function mutateTree(root: unknown, donors: readonly unknown[], rand: () => number): unknown {
  let out = root;
  for (let n = 1 + Math.floor(rand() * 4); n > 0; n--) {
    const all = containers(out);
    if (all.length === 0 || rand() < 0.02) {
      out = pick(VALUES, rand);
      continue;
    }
    const target = pick(all, rand);
    const donor = pick(containers(pick(donors, rand)), rand) ?? null;
    const replacement = rand() < 0.7 ? structuredClone(pick(VALUES, rand)) : structuredClone(donor);
    if (Array.isArray(target)) {
      const at = Math.floor(rand() * (target.length + 1));
      switch (Math.floor(rand() * 4)) {
        case 0:
          target.splice(at, 1);
          break;
        case 1:
          if (target.length > 0)
            target.push(...Array.from({ length: 30 }, () => structuredClone(target[0])));
          break;
        default:
          target.splice(at, 0, replacement);
      }
    } else {
      const keys = Object.keys(target);
      const key = keys.length > 0 && rand() < 0.7 ? pick(keys, rand) : pick(KEYS, rand);
      if (rand() < 0.2) delete target[key];
      else target[key] = replacement;
    }
  }
  return out;
}

/**
 * Applies one to three random edits to JSON text.
 *
 * @param input - The text.
 * @param rand - The PRNG.
 * @returns The edited text, which may no longer be JSON.
 */
function mutateText(input: string, rand: () => number): string {
  let out = input;
  for (let n = 1 + Math.floor(rand() * 3); n > 0; n--) {
    const at = Math.floor(rand() * (out.length + 1));
    out =
      rand() < 0.5
        ? out.slice(0, at) + pick(FRAGMENTS, rand) + out.slice(at)
        : out.slice(0, at) + out.slice(at + 1 + Math.floor(rand() * 8));
  }
  return out;
}

/**
 * Whether a value is a JSON object (not an array or null).
 *
 * @param value - Any value.
 * @returns `true` for a plain object.
 */
function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Checks a string field: non-empty, well-formed UTF-16, within `MAX_TEXT`.
 *
 * @param value - The field.
 * @param what - Its name, for the message.
 */
function checkText(value: string, what: string): void {
  assert.ok(value.length > 0, `${what} is empty`);
  assert.ok(value.length <= MAX_TEXT, `${what} is longer than MAX_TEXT`);
  assert.ok(wellFormed(value), `${what} has a lone surrogate`);
}

/**
 * Whether every surrogate in a string is half of a pair, read code unit by
 * code unit.
 *
 * @param value - Any string.
 * @returns `true` when the string is well-formed UTF-16.
 */
function wellFormed(value: string): boolean {
  for (let i = 0; i < value.length; i++) {
    const c = value.charCodeAt(i);
    if (c >= 0xdc00 && c <= 0xdfff) return false;
    if (c >= 0xd800 && c <= 0xdbff) {
      const next = value.charCodeAt(i + 1);
      if (!(next >= 0xdc00 && next <= 0xdfff)) return false;
      i++;
    }
  }
  return true;
}

/**
 * Checks a whole number field: an integer in `min..=max`.
 *
 * @param value - The field.
 * @param min - Lowest allowed.
 * @param max - Highest allowed.
 * @param what - Its name, for the message.
 */
function checkWhole(value: number, min: number, max: number, what: string): void {
  assert.ok(Number.isSafeInteger(value) && value >= min && value <= max, `${what} out of range`);
}

/**
 * The oracle for `recordFromBook`.
 *
 * @param body - Parsed body.
 */
function checkBook(body: unknown): void {
  const record: MetadataRecord | undefined = recordFromBook(body, REQUEST);
  const asin =
    isObject(body) && typeof body["asin"] === "string" ? body["asin"].trim().toUpperCase() : "";
  const title = isObject(body) && typeof body["title"] === "string" ? body["title"].trim() : "";
  const expected = /^[A-Z0-9]{10}$/.test(asin) && title !== "";
  assert.equal(record !== undefined, expected, "record presence does not follow asin and title");
  if (record === undefined) return;
  assert.deepEqual(JSON.parse(JSON.stringify(record)), record, "record does not survive JSON");
  assert.deepEqual(recordFromBook(body, REQUEST), record, "mapping is not deterministic");

  checkText(record.title, "title");
  for (const key of ["subtitle", "description", "publisher", "language", "categories"] as const) {
    const v = record[key];
    if (v !== undefined) checkText(v, key);
  }
  if (record.subtitle !== undefined) assert.notEqual(record.subtitle, record.title);
  if (record.language !== undefined) assert.equal(record.language, record.language.toLowerCase());
  assert.ok(["unabridged", "abridged", "unknown"].includes(record.abridgement));
  assert.equal(record.workKind, "book");
  assert.equal(record.expressionKind, "narration");
  assert.equal(record.mediaKind, "audiobook");

  const [first, ...rest] = record.identifiers;
  assert.deepEqual(first, { scheme: "asin", value: asin, marketplace: "uk" });
  assert.ok(rest.length <= 1);
  for (const i of rest) {
    assert.equal(i.scheme, "isbn");
    assert.match(i.value, /^(\d{9}[\dX]|\d{13})$/);
  }

  const authors = record.credits.filter((c) => c.role === "author");
  const narrators = record.credits.filter((c) => c.role === "narrator");
  assert.equal(authors.length + narrators.length, record.credits.length, "unexpected credit role");
  for (const group of [authors, narrators]) {
    assert.ok(group.length <= MAX_PEOPLE);
    assert.equal(new Set(group.map((c) => c.name)).size, group.length);
    for (const c of group) {
      checkText(c.name, "credit name");
      assert.equal(c.level, "roleDefault");
    }
  }
  assert.ok(record.seriesEntries.length <= 2);
  for (const s of record.seriesEntries) {
    checkText(s.name, "series name");
    if (s.position !== undefined) checkText(s.position, "series position");
    if (s.storeSeriesId !== undefined) assert.match(s.storeSeriesId, /^[A-Z0-9]{10}$/);
  }
  if (record.categories !== undefined) {
    assert.ok(record.categories.split("; ").length <= MAX_GENRES);
  }
  if (record.coverUrl !== undefined) {
    checkText(record.coverUrl, "coverUrl");
    assert.ok(record.coverUrl.startsWith("https://"));
  }
  if (record.publishedAt !== undefined) assert.match(record.publishedAt, /^\d{4}-\d{2}-\d{2}$/);
  if (record.lengthMinutes !== undefined) {
    checkWhole(record.lengthMinutes, 1, 0xffff_ffff, "lengthMinutes");
  }
  if (record.ratingOverall !== undefined) {
    assert.ok(record.ratingOverall > 0 && record.ratingOverall <= 5);
  }
  assert.equal(record.ratingCount, undefined);
  assert.equal(record.url, undefined);
}

/**
 * The chapters an independent reading of the body expects, as
 * `[startMs, depth]` pairs: an explicit-stack pre-order walk that keeps
 * entries with a whole `startOffsetMs` (number or digit string) down to
 * `MAX_DEPTH`, and stops at `MAX_CHAPTERS`.
 *
 * @param body - Parsed body.
 * @returns The expected starts and depths.
 */
function expectedChapters(body: unknown): [number, number][] {
  const out: [number, number][] = [];
  const top = isObject(body) ? body["chapters"] : undefined;
  if (!Array.isArray(top)) return out;
  const stack: [unknown[], number, number][] = [[top, 0, 0]];
  while (stack.length > 0 && out.length < MAX_CHAPTERS) {
    const frame = stack[stack.length - 1] as [unknown[], number, number];
    const [list, index, depth] = frame;
    if (index >= list.length) {
      stack.pop();
      continue;
    }
    frame[1] = index + 1;
    const entry = list[index];
    if (!isObject(entry)) continue;
    const raw = entry["startOffsetMs"];
    const start =
      typeof raw === "string" && /^\d+$/.test(raw.trim()) ? Number(raw) : (raw as unknown);
    if (typeof start !== "number" || !Number.isSafeInteger(start) || start < 0) continue;
    out.push([start, depth]);
    const children = entry["chapters"];
    if (Array.isArray(children) && depth + 1 <= MAX_DEPTH) stack.push([children, 0, depth + 1]);
  }
  return out;
}

/**
 * The oracle for `chaptersFromBody`.
 *
 * @param body - Parsed body.
 */
function checkChapters(body: unknown): void {
  const layout: MetadataChapters = chaptersFromBody(body);
  assert.deepEqual(JSON.parse(JSON.stringify(layout)), layout, "layout does not survive JSON");
  const expected = expectedChapters(body);
  assert.deepEqual(
    layout.chapters.map((c) => [c.startMs, c.depth]),
    expected,
    "chapters differ from an independent pre-order walk",
  );
  assert.equal(layout.found, expected.length > 0);
  if (!layout.found) {
    assert.deepEqual(layout, { found: false, chapters: [], accurate: false });
    return;
  }
  assert.equal(layout.accurate, isObject(body) && body["isAccurate"] === true);
  for (const c of layout.chapters) {
    assert.ok(c.title.length <= MAX_TEXT);
    if (c.title !== "") checkText(c.title, "chapter title");
    if (c.lengthMs !== undefined) checkWhole(c.lengthMs, 1, Number.MAX_SAFE_INTEGER, "lengthMs");
  }
  for (const key of ["brandIntroMs", "brandOutroMs", "runtimeMs"] as const) {
    const v = layout[key];
    if (v !== undefined) checkWhole(v, 1, Number.MAX_SAFE_INTEGER, key);
  }
}

/**
 * Runs an oracle and names the input when it fails.
 *
 * @param oracle - The target's oracle.
 * @param body - Parsed body.
 * @param label - Where the input came from.
 */
function checkLabelled(oracle: (body: unknown) => void, body: unknown, label: string): void {
  try {
    oracle(body);
  } catch (err) {
    const shown = JSON.stringify(body)?.slice(0, 500);
    throw new Error(`${label}: ${String(err)}\ninput: ${shown}`, { cause: err });
  }
}

/**
 * Replays a target's corpus, then checks seeded mutations of it.
 *
 * @param target - Directory name under `fuzz/`.
 * @param oracle - The target's oracle.
 * @param salt - Added to `SEED` so targets draw different streams.
 */
function fuzz(target: string, oracle: (body: unknown) => void, salt: number): void {
  const texts = seeds(target);
  assert.ok(texts.length >= 3, `${target} seed corpus is missing`);
  const parsed = texts.map((s) => JSON.parse(s) as unknown);
  for (const [i, body] of parsed.entries()) checkLabelled(oracle, body, `${target} seed ${i}`);
  const rand = prng(SEED + salt);
  for (let i = 0; i < ITERATIONS; i++) {
    const label = `FUZZ_SEED=${SEED} ${target} iteration ${i}`;
    if (rand() < 0.8) {
      checkLabelled(oracle, mutateTree(structuredClone(pick(parsed, rand)), parsed, rand), label);
    } else {
      let body: unknown;
      try {
        body = JSON.parse(mutateText(pick(texts, rand), rand)) as unknown;
      } catch {
        continue;
      }
      checkLabelled(oracle, body, label);
    }
  }
}

test("book corpus replays and mutations hold the oracle", () => {
  fuzz("book", checkBook, 0);
});

test("chapters corpus replays and mutations hold the oracle", () => {
  fuzz("chapters", checkChapters, 1);
});
