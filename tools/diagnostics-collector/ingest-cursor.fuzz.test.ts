// The ingest cursor body (POST /report/cursor, ingest-cursor.ts): seed
// corpus replay and seeded mutation fuzzing (fuzzing ADR decision 6). The
// parsed cursor is stored in D1 and decides which stored reports the next
// ingest run pulls, so each result is checked against an independent oracle
// written without the module's helpers.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { URL } from "node:url";
import { MAX_AFTER_CHARS, MAX_CURSOR_CHARS, parseCursor } from "./ingest-cursor.ts";
import { mutate, rng } from "./test-support/fuzz.ts";

/** Seed inputs, keyed by file name. */
function seeds(): [string, string][] {
  const url = new URL("./corpus/ingest_cursor/", import.meta.url);
  return readdirSync(url)
    .sort()
    .map((name): [string, string] => [name, readFileSync(new URL(name, url), "utf8")]);
}

/**
 * Oracle for parseCursor: a separate walk over the parsed value, checking
 * characters one by one.
 *
 * @param text - Body.
 * @param label - Failure label.
 */
function check(text: string, label: string) {
  let expected = null;
  let v;
  let ok = text.length <= MAX_CURSOR_CHARS;
  if (ok) {
    try {
      v = JSON.parse(text);
    } catch {
      ok = false;
    }
  }
  if (ok && v !== null && typeof v === "object" && !Array.isArray(v)) {
    const s = v.since;
    const a = v.after === undefined || v.after === null ? "" : v.after;
    const sinceOk = typeof s === "number" && Number.isInteger(s) && s >= 0 && s <= Number.MAX_SAFE_INTEGER;
    const afterOk =
      typeof a === "string" &&
      a.length <= MAX_AFTER_CHARS &&
      [...a].every((c) => c.charCodeAt(0) >= 0x20 && c.charCodeAt(0) !== 0x7f);
    if (sinceOk && afterOk) expected = { since: s, after: a };
  }
  assert.deepEqual(parseCursor(text), expected, label);
}

/** Fragments mutations splice in: JSON syntax, numbers, controls and Unicode. */
const FRAGMENTS = [
  "{", "}", "[", "]", ",", ":", '"', "\\", "\\n", "\\u0000", "\\u007f", "1e3", "-1", "0.5",
  "9007199254740993", "null", "true", '"since"', '"after"', "é", "😀", " ", "\u0001",
];

describe("ingest cursor parser", () => {
  it("replays the seed corpus", () => {
    const all = seeds();
    assert.ok(all.length > 0);
    for (const [name, text] of all) check(text, name);
    assert.deepEqual(parseCursor(readFileSync(new URL("./corpus/ingest_cursor/normal.json", import.meta.url), "utf8")), {
      since: 1792831701483,
      after: "diagnostics/0.0.0-synthetic/0a3df5f0-f339-49b7-b81f-df217a50797f.json",
    });
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds().map(([, text]) => text);
    const rand = rng(20261024);
    for (let n = 0; n < 4000; n++) {
      const text = mutate(rand, all[Math.floor(rand() * all.length)] ?? "", FRAGMENTS);
      check(text, `iteration ${n}: ${JSON.stringify(text).slice(0, 200)}`);
    }
  });

  it("refuses non-strings and oversized bodies", () => {
    for (const v of [undefined, null, 1, {}, ["{}"]]) assert.equal(parseCursor(v), null);
    assert.equal(parseCursor(`{"since":1,"after":"${"a".repeat(MAX_CURSOR_CHARS)}"}`), null);
  });
});
