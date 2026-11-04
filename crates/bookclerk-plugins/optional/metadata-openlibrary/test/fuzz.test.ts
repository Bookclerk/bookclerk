/**
 * Fuzzing for the Open Library response mapping, as the fuzzing ADR asks of
 * boundaries outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
 * decision 6).
 *
 * Each seed in `fuzz/search_response/` is replayed, then seeded mutations of
 * the seeds (edits to the parsed JSON tree and to its text) are checked
 * against an independent oracle: what a record may contain, read from the
 * input and the ABI's rules rather than from the mapping code. `FUZZ_SEED`
 * and `FUZZ_ITERATIONS` pick another run; a failure prints the seed,
 * iteration and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import type { MetadataRecord } from "@bookclerk/plugin-sdk/workerd";

import {
  MAX_AUTHORS,
  MAX_IDS_PER_SCHEME,
  MAX_SUBJECTS,
  MAX_TEXT,
  ProviderFailure,
  planSearch,
  recordsFromSearch,
  type SearchRequest,
} from "../src/openlibrary.ts";

/** Directory of the seed corpus. */
const CORPUS = new URL("../fuzz/search_response/", import.meta.url);

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0x5eed);

/** Mutated inputs checked per request shape. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 3000);

/** Every seed, by file name, as JSON text. */
const SEEDS: ReadonlyMap<string, string> = new Map(
  readdirSync(CORPUS)
    .filter((name) => name.endsWith(".json"))
    .sort()
    .map((name) => [name, readFileSync(new URL(name, CORPUS), "utf8")]),
);

/** The request shapes a body is mapped under: a lookup and a title search. */
const REQUESTS: readonly SearchRequest[] = [
  { url: "", limit: 20 },
  { url: "", limit: 2 },
  planSearch({ title: "the hobbit" }),
];

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
  0xffff_ffff,
  0x1_0000_0000,
  Number.MAX_SAFE_INTEGER,
  1e308,
  "",
  "   ",
  "OL1W",
  "/works/OL27482W",
  "/books/OL7M",
  "ol99w",
  "OL1234567890123W",
  "978-0-547-92822-7",
  "054792822x",
  "\uD800",
  "a\uDC00b",
  "📚",
  "x".repeat(MAX_TEXT - 1) + "📚",
  " ".repeat(MAX_TEXT) + "y",
  [],
  {},
  [null, "", "A", "A", 3],
  { docs: [] },
  { value: "sentence" },
];

/** Keys a tree mutation may add to an object. */
const KEYS: readonly string[] = [
  "key",
  "title",
  "subtitle",
  "author_name",
  "first_sentence",
  "cover_i",
  "first_publish_year",
  "subject",
  "ratings_average",
  "ratings_count",
  "editions",
  "docs",
  "isbn",
  "publisher",
  "publish_date",
  "language",
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
 * The bare Open Library ids of one kind that the body mentions anywhere, as
 * the oracle reads them: the last path segment of any string, upper-cased.
 *
 * @param body - Parsed body.
 * @param suffix - `W` or `M`.
 * @returns The ids.
 */
function mentionedIds(body: unknown, suffix: "W" | "M"): Set<string> {
  const out = new Set<string>();
  const stack: unknown[] = [body];
  while (stack.length > 0) {
    const v = stack.pop();
    if (typeof v === "string") {
      const id = (v.trim().split("/").at(-1) ?? "").toUpperCase();
      if (new RegExp(`^OL\\d{1,12}${suffix}$`).test(id)) out.add(id);
    } else if (typeof v === "object" && v !== null) {
      stack.push(...Object.values(v));
    }
  }
  return out;
}

/**
 * Every string stored under a `title` key anywhere in the body.
 *
 * @param body - Parsed body.
 * @returns The titles.
 */
function titlesIn(body: unknown): string[] {
  const out: string[] = [];
  const stack: unknown[] = [body];
  while (stack.length > 0) {
    const v = stack.pop();
    if (typeof v !== "object" || v === null) continue;
    if (!Array.isArray(v) && typeof (v as Record<string, unknown>)["title"] === "string") {
      out.push((v as Record<string, string>)["title"] as string);
    }
    stack.push(...Object.values(v));
  }
  return out;
}

/**
 * The oracle for one record.
 *
 * @param record - A mapped record.
 * @param body - The body it came from.
 * @param request - The request it was mapped under.
 */
function checkRecord(record: MetadataRecord, body: unknown, request: SearchRequest): void {
  assert.deepEqual(JSON.parse(JSON.stringify(record)), record, "record does not survive JSON");
  checkText(record.title, "title");
  for (const key of ["subtitle", "description", "publisher", "publishedAt", "language"] as const) {
    const v = record[key];
    if (v !== undefined) checkText(v, key);
  }
  if (record.subtitle !== undefined) assert.notEqual(record.subtitle, record.title);
  const wanted = request.titleFilter;
  if (wanted !== undefined) {
    // The record's own title or a work title in the body overlaps the asked one.
    const overlaps = (t: string): boolean => {
      const have = t.trim().toLowerCase();
      return have !== "" && (have.includes(wanted) || wanted.includes(have));
    };
    assert.ok(overlaps(record.title) || titlesIn(body).some(overlaps), "title does not match");
  }
  assert.equal(record.abridgement, "unknown");
  assert.equal(record.workKind, "book");
  assert.equal(record.expressionKind, "text");
  assert.equal(record.mediaKind, "print");
  assert.deepEqual(record.seriesEntries, []);

  const [work, ...rest] = record.identifiers;
  assert.ok(work !== undefined && work.scheme === "openLibraryWork");
  assert.ok(mentionedIds(body, "W").has(work.value), "work id not in the body");
  const editions = rest.filter((i) => i.scheme === "openLibraryEdition");
  const isbns = rest.filter((i) => i.scheme === "isbn");
  assert.equal(editions.length + isbns.length, rest.length, "unexpected identifier scheme");
  assert.ok(editions.length <= 1);
  for (const e of editions)
    assert.ok(mentionedIds(body, "M").has(e.value), "edition id not in the body");
  assert.ok(isbns.length <= MAX_IDS_PER_SCHEME);
  for (const i of isbns) assert.match(i.value, /^(\d{9}[\dX]|\d{13})$/);
  const edition = editions[0];
  assert.equal(
    record.url,
    edition !== undefined
      ? `https://openlibrary.org/books/${edition.value}`
      : `https://openlibrary.org/works/${work.value}`,
  );

  assert.ok(record.credits.length <= MAX_AUTHORS);
  assert.equal(new Set(record.credits.map((c) => c.name)).size, record.credits.length);
  for (const c of record.credits) {
    checkText(c.name, "credit name");
    assert.equal(c.role, "author");
    assert.equal(c.level, "roleDefault");
  }
  if (record.categories !== undefined) {
    checkText(record.categories, "categories");
    assert.ok(record.categories.split("; ").length <= MAX_SUBJECTS);
  }
  if (record.coverUrl !== undefined) {
    assert.match(record.coverUrl, /^https:\/\/covers\.openlibrary\.org\/b\/id\/[1-9]\d*-L\.jpg$/);
  }
  if (record.ratingOverall !== undefined) {
    assert.ok(record.ratingOverall > 0 && record.ratingOverall <= 5);
  }
  if (record.ratingCount !== undefined) {
    assert.ok(Number.isInteger(record.ratingCount));
    assert.ok(record.ratingCount >= 1 && record.ratingCount <= 0xffff_ffff);
  }
  assert.equal(record.lengthMinutes, undefined);
}

/**
 * Maps a body under a request and checks the answer against the oracle.
 *
 * @param body - Parsed body.
 * @param request - The request shape.
 */
function check(body: unknown, request: SearchRequest): void {
  const docs = isObject(body) ? body["docs"] : undefined;
  const refused = !isObject(body) || (docs !== undefined && !Array.isArray(docs));
  let records: MetadataRecord[];
  try {
    records = recordsFromSearch(body, request);
  } catch (err) {
    assert.ok(err instanceof ProviderFailure, `threw ${String(err)}`);
    assert.equal(err.code, "unavailable");
    assert.ok(refused, "refused a search result");
    return;
  }
  assert.ok(!refused, "accepted a body that is not a search result");
  assert.ok(records.length <= request.limit);
  assert.ok(records.length <= (Array.isArray(docs) ? docs.length : 0));
  for (const record of records) checkRecord(record, body, request);
  assert.deepEqual(recordsFromSearch(body, request), records, "mapping is not deterministic");
}

/**
 * Runs `check` and names the input when it fails.
 *
 * @param body - Parsed body.
 * @param request - The request shape.
 * @param label - Where the input came from.
 */
function checkLabelled(body: unknown, request: SearchRequest, label: string): void {
  try {
    check(body, request);
  } catch (err) {
    const shown = JSON.stringify(body)?.slice(0, 500);
    throw new Error(`${label} (limit ${request.limit}): ${String(err)}\ninput: ${shown}`, {
      cause: err,
    });
  }
}

test("search_response corpus replays", () => {
  assert.ok(SEEDS.size >= 5, "seed corpus is missing");
  for (const [name, text] of SEEDS) {
    for (const request of REQUESTS) checkLabelled(JSON.parse(text) as unknown, request, name);
  }
});

test("search_response mutations hold the oracle", () => {
  const seeds = [...SEEDS.values()];
  const parsed = seeds.map((s) => JSON.parse(s) as unknown);
  for (const [r, request] of REQUESTS.entries()) {
    const rand = prng(SEED + r);
    for (let i = 0; i < ITERATIONS; i++) {
      const label = `FUZZ_SEED=${SEED} request ${r} iteration ${i}`;
      if (rand() < 0.8) {
        const body = mutateTree(structuredClone(pick(parsed, rand)), parsed, rand);
        checkLabelled(body, request, label);
      } else {
        let body: unknown;
        try {
          body = JSON.parse(mutateText(pick(seeds, rand), rand)) as unknown;
        } catch {
          continue;
        }
        checkLabelled(body, request, label);
      }
    }
  }
});
