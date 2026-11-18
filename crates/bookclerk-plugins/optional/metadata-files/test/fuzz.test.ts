/**
 * Fuzzing for every dialect, as the fuzzing ADR asks of boundaries outside
 * the Rust workspace (docs/adr/untrusted-input-fuzzing.md, decision 6).
 *
 * Sidecar files come from storage the host does not control, so each
 * dialect's `parse` and `render` read untrusted bytes. Each seed in
 * `fuzz/<dialect>/` is replayed, then seeded byte and text mutations are
 * checked against an independent oracle written from the ABI's record rules
 * (`validate_metadata_file_record` and `check_record_against_dialect` in
 * `bookclerk-plugin-abi`) and the manifest row, not from the dialects' code:
 *
 * - `parse` either refuses with `invalid_params` or returns a record the
 *   host accepts, stating only the fields and roles the row declares;
 * - `render` over any existing file either refuses with `invalid_params`
 *   (only where `parse` refuses the same file) or returns a file within the
 *   size cap that parses back to the values it was asked to write.
 *
 * `FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run; a failure prints the
 * seed, iteration and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import type { CreditRole, MetadataField, MetadataFileRecord } from "@bookclerk/plugin-sdk/workerd";
import { DIALECTS, parseFile, renderFile } from "../src/dialects.ts";
import { DialectFailure } from "../src/text.ts";

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0x5eed);

/** Mutated inputs checked per dialect. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 1500);

/** ABI caps, restated from `plugin.capnp` rather than imported. */
const CAP = {
  file: 262144,
  description: 65536,
  text: 1024,
  list: 256,
  chapters: 2000,
} as const;

/** One `[[metadata_files.dialects]]` row of `plugin.toml`. */
interface Row {
  /** Dialect id. */
  id: string;
  /** Fields the row declares. */
  fields: MetadataField[];
  /** Credit roles the row declares. */
  roles: CreditRole[];
  /** Whether the row is writable. */
  writes: boolean;
}

/**
 * Reads a TOML string array from one row.
 *
 * @param row - The row's text.
 * @param key - The key.
 * @returns The strings; empty when the key is missing.
 */
function tomlList(row: string, key: string): string[] {
  const match = new RegExp(`^${key} = \\[([^\\]]*)\\]$`, "m").exec(row);
  return match?.[1] === undefined
    ? []
    : [...match[1].matchAll(/"([^"]*)"/g)].map((m) => m[1] ?? "");
}

/** The manifest's dialect rows. */
const ROWS: Row[] = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8")
  .split("[[metadata_files.dialects]]")
  .slice(1)
  .map((row) => ({
    id: /^id = "([^"]+)"$/m.exec(row)?.[1] ?? "",
    fields: tomlList(row, "fields") as MetadataField[],
    roles: tomlList(row, "credit_roles") as CreditRole[],
    writes: /^writes = true$/m.test(row),
  }));

/**
 * Bytes of a string in UTF-8.
 *
 * @param text - Text.
 * @returns Its UTF-8 length.
 */
function utf8Length(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** A Rust `char::is_control` character (Unicode category Cc). */
const RUST_CONTROL = /\p{Cc}/u;

/**
 * Oracle for one text value, after the ABI's `check_text`.
 *
 * @param value - The value.
 * @param max - Byte cap.
 * @param multiline - Whether tab and line breaks are allowed.
 * @returns Why it fails, or `undefined`.
 */
function badText(value: string, max: number, multiline: boolean): string | undefined {
  if (utf8Length(value) > max) return "over the cap";
  for (const char of value) {
    if (RUST_CONTROL.test(char) && !(multiline && "\n\r\t".includes(char))) return "control";
  }
  if (/[\uD800-\uDFFF]/u.test(value)) return "lone surrogate";
  return undefined;
}

/**
 * Oracle for a required single-line value.
 *
 * @param value - The value.
 * @returns Why it fails, or `undefined`.
 */
function badLabel(value: string): string | undefined {
  return value.trim() === "" ? "blank" : badText(value, CAP.text, false);
}

/**
 * Whether a date is `YYYY`, `YYYY-MM` or `YYYY-MM-DD` and real, written
 * from the calendar rather than from the dialects' helper.
 *
 * @param date - The date.
 * @returns True when the host accepts it.
 */
function realDate(date: string): boolean {
  if (!/^\d{4}(-\d{2}(-\d{2})?)?$/.test(date)) return false;
  const [y, m, d] = date.split("-").map(Number);
  if (m === undefined) return true;
  if (m < 1 || m > 12) return false;
  if (d === undefined) return true;
  const probe = new Date(Date.UTC(y ?? 0, m - 1, d));
  return probe.getUTCMonth() === m - 1 && probe.getUTCDate() === d && d >= 1;
}

/**
 * The fields a record holds a value for (the ABI's `fields_with_values`).
 *
 * @param r - The record.
 * @returns The fields.
 */
function valued(r: MetadataFileRecord): MetadataField[] {
  const out: MetadataField[] = [];
  const add = (field: MetadataField, has: boolean): void => {
    if (has) out.push(field);
  };
  add("title", r.title !== undefined);
  add("subtitle", r.subtitle !== undefined);
  add("description", r.description !== undefined);
  add("publisher", r.publisher !== undefined);
  add("publishedAt", r.publishedAt !== undefined);
  add("language", r.language !== undefined);
  add("genres", r.genres.length > 0);
  add("tags", r.tags.length > 0);
  add("abridgement", r.abridgement !== "unknown");
  add("expressionKind", r.expressionKind !== "unspecified");
  add("identifiers", r.identifiers.length > 0);
  add("credits", r.credits.length > 0 || r.creditRoles.length > 0);
  add("series", r.seriesEntries.length > 0);
  add("chapters", r.chapters.length > 0);
  add("duration", r.durationMs !== undefined);
  return out;
}

/**
 * The oracle: why the host would refuse a parsed record, or `undefined`.
 *
 * @param r - The record a dialect returned.
 * @param row - The dialect's manifest row.
 * @returns The first broken rule.
 */
function hostRefusal(r: MetadataFileRecord, row: Row): string | undefined {
  if (new Set(r.present).size !== r.present.length) return "duplicate field";
  if (new Set(r.creditRoles).size !== r.creditRoles.length) return "duplicate role";
  for (const field of valued(r)) if (!r.present.includes(field)) return `${field} not present`;
  for (const key of ["title", "subtitle", "publisher", "language"] as const) {
    const value = r[key];
    if (value !== undefined && badLabel(value) !== undefined) return key;
  }
  if (r.description !== undefined) {
    if (r.description.trim() === "") return "blank description";
    if (badText(r.description, CAP.description, true) !== undefined) return "description";
  }
  if (r.publishedAt !== undefined && !realDate(r.publishedAt)) return "publishedAt";
  for (const key of ["genres", "tags"] as const) {
    if (r[key].length > CAP.list) return `${key} count`;
    if (r[key].some((v) => badLabel(v) !== undefined)) return key;
  }
  if (r.identifiers.length > CAP.list) return "identifier count";
  for (const id of r.identifiers) {
    if (badLabel(id.value) !== undefined) return "identifier";
    if (id.marketplace !== undefined && badLabel(id.marketplace) !== undefined)
      return "marketplace";
  }
  if (r.credits.length > CAP.list) return "credit count";
  for (const credit of r.credits) {
    if (!r.creditRoles.includes(credit.role)) return "credit role not stated";
    if (badLabel(credit.name) !== undefined) return "credit name";
  }
  if (r.seriesEntries.length > CAP.list) return "series count";
  for (const s of r.seriesEntries) {
    if (badLabel(s.name) !== undefined) return "series name";
    if (s.position !== undefined && badLabel(s.position) !== undefined) return "series position";
    if (s.storeSeriesId !== undefined && badLabel(s.storeSeriesId) !== undefined) {
      return "series id";
    }
  }
  if (r.chapters.length > CAP.chapters) return "chapter count";
  let previous: { start: number; depth: number } | undefined;
  for (const c of r.chapters) {
    if (badText(c.title, CAP.text, false) !== undefined) return "chapter title";
    if (!Number.isSafeInteger(c.startMs) || c.startMs < 0) return "chapter start";
    if (c.lengthMs !== undefined && (!Number.isSafeInteger(c.lengthMs) || c.lengthMs < 0)) {
      return "chapter length";
    }
    if (!Number.isInteger(c.depth) || c.depth < 0) return "chapter depth";
    const ordered =
      previous === undefined
        ? c.depth === 0
        : c.startMs >= previous.start && c.depth <= previous.depth + 1;
    if (!ordered) return "chapter order";
    previous = { start: c.startMs, depth: c.depth };
  }
  if (r.durationMs !== undefined && (!Number.isSafeInteger(r.durationMs) || r.durationMs < 0)) {
    return "duration";
  }
  for (const field of r.present) if (!row.fields.includes(field)) return `${field} not carried`;
  for (const role of r.creditRoles) if (!row.roles.includes(role)) return `${role} not carried`;
  return undefined;
}

/**
 * Small, fast PRNG (mulberry32).
 *
 * @param seed - Starting state.
 * @returns A function giving floats in [0, 1).
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

/** Text fragments that touch each format's syntax. */
const TOKENS = [
  "{",
  "}",
  "[",
  "]",
  '"',
  ",",
  ":",
  "null",
  "true",
  "-1",
  "1e999",
  '"metadata":{',
  '"chapters":[{"start":',
  '"series":["a #',
  '"\\u0000"',
  '"\\ud800"',
  "\n",
  "\r\n",
  "\r",
  "Title: ",
  "Author: ",
  "Abridged: ",
  "Release Date: ",
  "Book Description\n",
  "====\n",
  " & ",
  " and ",
  ";",
  "#",
  "\u0000",
  "\u0085",
  "é",
  "😀",
  "﻿",
];

/** Bytes that are invalid or unusual in UTF-8. */
const ODD_BYTES = [0x00, 0x0a, 0x0d, 0x22, 0x3a, 0x80, 0x9d, 0xc3, 0xe9, 0xed, 0xf0, 0xff];

/**
 * Mutates one input.
 *
 * @param rand - PRNG.
 * @param input - Seed bytes.
 * @param pool - Every seed, for splicing.
 * @returns The mutated bytes.
 */
function mutate(rand: () => number, input: Uint8Array, pool: Uint8Array[]): Uint8Array {
  let bytes = [...input];
  const pick = <T>(list: readonly T[]): T => list[Math.floor(rand() * list.length)] as T;
  const at = (): number => Math.floor(rand() * (bytes.length + 1));
  const rounds = 1 + Math.floor(rand() * 4);
  for (let i = 0; i < rounds; i += 1) {
    const op = Math.floor(rand() * 6);
    if (op === 0 && bytes.length > 0) {
      bytes[Math.min(at(), bytes.length - 1)] = pick(ODD_BYTES);
    } else if (op === 1) {
      bytes.splice(at(), 0, ...new TextEncoder().encode(pick(TOKENS)));
    } else if (op === 2 && bytes.length > 0) {
      const start = at();
      bytes.splice(start, 1 + Math.floor(rand() * 16));
    } else if (op === 3) {
      const other = [...pick(pool)];
      const from = Math.floor(rand() * (other.length + 1));
      bytes.splice(at(), 0, ...other.slice(from, from + 1 + Math.floor(rand() * 64)));
    } else if (op === 4 && bytes.length > 0) {
      const start = at();
      const chunk = bytes.slice(start, start + 1 + Math.floor(rand() * 32));
      bytes.splice(at(), 0, ...chunk);
    } else {
      bytes = bytes.slice(0, at());
    }
  }
  return Uint8Array.from(bytes);
}

/** Letters for generated values: no separator any format treats specially. */
const ALPHABET = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789éßΩ";

/**
 * A clean value every dialect can write and read back unchanged.
 *
 * @param rand - PRNG.
 * @param words - Most words.
 * @returns A trimmed, single-spaced phrase.
 */
function phrase(rand: () => number, words = 3): string {
  const n = 1 + Math.floor(rand() * words);
  return Array.from({ length: n }, () =>
    Array.from(
      { length: 1 + Math.floor(rand() * 8) },
      () => ALPHABET[Math.floor(rand() * ALPHABET.length)],
    ).join(""),
  ).join(" ");
}

/**
 * Distinct phrases.
 *
 * @param rand - PRNG.
 * @param max - Most entries.
 * @returns Phrases, possibly none.
 */
function phrases(rand: () => number, max: number): string[] {
  const out = new Set<string>();
  const n = Math.floor(rand() * (max + 1));
  for (let i = 0; i < n; i += 1) out.add(phrase(rand));
  return [...out];
}

/**
 * Builds a render request's record with clean values, each field empty or
 * set at random.
 *
 * @param rand - PRNG.
 * @param row - The dialect's manifest row.
 * @returns The record.
 */
function cleanRecord(rand: () => number, row: Row): MetadataFileRecord {
  const maybe = <T>(value: T): T | undefined => (rand() < 0.75 ? value : undefined);
  const roles = row.roles.filter(() => rand() < 0.8);
  const startMs: number[] = [];
  let at = 0;
  for (let i = Math.floor(rand() * 5); i > 0; i -= 1) {
    startMs.push(at);
    at += 1 + Math.floor(rand() * 5_000_000);
  }
  const r: MetadataFileRecord = {
    present: [],
    genres: phrases(rand, 3),
    tags: phrases(rand, 3),
    abridgement:
      (["unknown", "unabridged", "abridged"] as const)[Math.floor(rand() * 3)] ?? "unknown",
    expressionKind: "unspecified",
    identifiers: [
      ...(rand() < 0.6
        ? [{ scheme: "isbn" as const, value: `978${Math.floor(rand() * 1e9)}` }]
        : []),
      ...(rand() < 0.6
        ? [{ scheme: "asin" as const, value: `B0${Math.floor(rand() * 1e8)}` }]
        : []),
    ],
    credits: roles.flatMap((role) =>
      phrases(rand, 3).map((name) => ({ name, role, level: "roleDefault" as const })),
    ),
    creditRoles: roles,
    seriesEntries: phrases(rand, 2).map((name) => ({
      name,
      ...(rand() < 0.7 ? { position: String(1 + Math.floor(rand() * 20)) } : {}),
    })),
    chapters: startMs.map((start) => ({
      title: phrase(rand),
      startMs: start,
      lengthMs: 1 + Math.floor(rand() * 1000),
      depth: 0,
    })),
  };
  const set = <K extends "title" | "subtitle" | "publisher" | "language" | "publishedAt">(
    key: K,
    value: string | undefined,
  ): void => {
    if (value !== undefined) r[key] = value;
  };
  set("title", maybe(phrase(rand)));
  set("subtitle", maybe(phrase(rand)));
  set("publisher", maybe(phrase(rand)));
  set("language", maybe(phrase(rand, 1)));
  const year = String(1900 + Math.floor(rand() * 150));
  set("publishedAt", maybe([year, `${year}-07`, `${year}-07-16`][Math.floor(rand() * 3)] ?? year));
  const description = maybe(
    Array.from({ length: 1 + Math.floor(rand() * 4) }, () => phrase(rand, 8)).join(
      rand() < 0.5 ? "\n" : "\n\n",
    ),
  );
  if (description !== undefined) r.description = description;
  return r;
}

/**
 * What a dialect can write of one field, for comparing a record with the
 * file it rendered.
 *
 * @param r - A record.
 * @param field - The field.
 * @param row - The dialect's row.
 * @param roles - Roles the request stated.
 * @returns A comparable value.
 */
function project(
  r: MetadataFileRecord,
  field: MetadataField,
  row: Row,
  roles: CreditRole[],
): unknown {
  switch (field) {
    case "identifiers":
      return (["isbn", "asin"] as const).map(
        (s) => r.identifiers.find((id) => id.scheme === s)?.value,
      );
    case "credits":
      return roles.map((role) => r.credits.filter((c) => c.role === role).map((c) => c.name));
    case "series":
      // Every writable dialect keeps one series.
      return r.seriesEntries.slice(0, 1).map((s) => [s.name, s.position]);
    case "chapters":
      // CUE times are whole 1/75 s frames and carry no lengths.
      return row.id === "cue"
        ? r.chapters.map((c) => [c.title, Math.round((c.startMs * 75) / 1000)])
        : r.chapters.map((c) => [c.title, c.startMs, c.lengthMs]);
    case "genres":
    case "tags":
      return r[field];
    case "abridgement":
      return r.abridgement;
    case "duration":
      return r.durationMs;
    case "expressionKind":
      return r.expressionKind;
    default:
      return r[field];
  }
}

/**
 * Runs `work`, returning its refusal instead of throwing it.
 *
 * @param work - The call.
 * @returns The result, or the refusal.
 */
function attempt<T>(work: () => T): { ok: T } | { refused: DialectFailure } {
  try {
    return { ok: work() };
  } catch (err) {
    if (err instanceof DialectFailure && err.code === "invalid_params") return { refused: err };
    throw err;
  }
}

/**
 * Checks one input against the oracle.
 *
 * @param row - The dialect's row.
 * @param bytes - The input file.
 * @param rand - PRNG for the render request.
 */
function check(row: Row, bytes: Uint8Array, rand: () => number): void {
  const parsed = attempt(() => parseFile(row.id, bytes));
  if ("ok" in parsed) {
    assert.equal(hostRefusal(parsed.ok, row), undefined, "parsed record breaks the ABI rules");
    assert.deepEqual(parseFile(row.id, bytes), parsed.ok, "parse is not deterministic");
  }
  if (!row.writes) return;
  const record = cleanRecord(rand, row);
  const fields = row.fields.filter(() => rand() < 0.6);
  const rendered = attempt(() => renderFile(row.id, bytes, record, fields));
  if ("refused" in rendered) {
    // A file parse reads may still be refused when the answer would pass
    // the size cap, or when the format cannot hold what was asked for.
    const allowed =
      "refused" in parsed || /^(?:rendered file is|cannot write )/.test(rendered.refused.message);
    assert.ok(allowed, `render refused a file parse reads: ${rendered.refused.message}`);
    return;
  }
  assert.ok(rendered.ok.length <= CAP.file, "rendered file over the cap");
  const reread = parseFile(row.id, rendered.ok);
  assert.equal(hostRefusal(reread, row), undefined, "re-read record breaks the ABI rules");
  for (const field of fields) {
    const roles = record.creditRoles;
    assert.deepEqual(
      project(reread, field, row, roles),
      project(record, field, row, roles),
      `field ${field} did not round-trip`,
    );
  }
  if (!("ok" in parsed)) return;
  // Fields the request left out read back as the existing file stated them.
  for (const field of row.fields) {
    if (fields.includes(field)) continue;
    assert.deepEqual(
      project(reread, field, row, row.roles),
      project(parsed.ok, field, row, row.roles),
      `unselected field ${field} changed`,
    );
  }
  // A credit role the request does not state is unknown, so it stays.
  if (fields.includes("credits")) {
    const unstated = row.roles.filter((role) => !record.creditRoles.includes(role));
    assert.deepEqual(
      project(reread, "credits", row, unstated),
      project(parsed.ok, "credits", row, unstated),
      "an unstated credit role changed",
    );
  }
}

/**
 * Reads one dialect's corpus.
 *
 * @param id - Dialect id.
 * @returns Each seed's bytes, by file name order.
 */
function corpus(id: string): Uint8Array[] {
  const dir = new URL(`../fuzz/${id}/`, import.meta.url);
  return readdirSync(dir)
    .sort()
    .map((file) => readFileSync(new URL(file, dir)));
}

/** Every seed of every dialect, for splicing across formats. */
const POOL = ROWS.flatMap((row) => corpus(row.id));

for (const row of ROWS) {
  test(`${row.id}: seeds replay against the oracle`, () => {
    const seeds = corpus(row.id);
    assert.ok(seeds.length > 0, "corpus is empty");
    const rand = prng(SEED);
    for (const [index, bytes] of [new Uint8Array(), ...seeds].entries()) {
      try {
        check(row, bytes, rand);
      } catch (err) {
        throw new Error(`seed ${index}: ${String(err)}`, { cause: err });
      }
    }
  });

  test(`${row.id}: mutations hold the oracle`, () => {
    const seeds = corpus(row.id);
    const rand = prng(SEED ^ row.id.length);
    for (let i = 0; i < ITERATIONS; i += 1) {
      const input = mutate(rand, seeds[i % seeds.length] ?? new Uint8Array(), POOL);
      try {
        check(row, input, rand);
      } catch (err) {
        const shown = JSON.stringify(new TextDecoder().decode(input));
        throw new Error(`FUZZ_SEED=${SEED} iteration ${i} input ${shown}: ${String(err)}`, {
          cause: err,
        });
      }
    }
  });
}

test("every manifest row has a corpus and a dialect", () => {
  assert.deepEqual(ROWS.map((r) => r.id).sort(), [...DIALECTS.keys()].sort());
  assert.deepEqual(
    readdirSync(new URL("../fuzz/", import.meta.url)).sort(),
    ROWS.map((r) => r.id).sort(),
  );
});
