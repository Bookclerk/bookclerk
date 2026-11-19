// The `POST /checkin` body boundary (fuzzing ADR decision 6): seed corpus
// replay (`corpus/checkin/`) and seeded mutation against an oracle written
// without checkin.ts's own patterns.
//
// parseCheckin reads a body anyone can send; its result is what a check-in
// row stores (version, platform and kind) and the install ID hashed into
// the install key, so nothing else may get through.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { MAX_CHECKIN_CHARS, parseCheckin } from "./checkin.ts";
import { choose, mutate, rng, seed, seeds } from "./test-support/fuzz.ts";

/**
 * Whether every character of a string is in a set.
 *
 * @param s - Text.
 * @param ok - Test for one character code.
 * @returns True for a non-empty string whose characters all pass.
 */
function every(s: string, ok: (c: number) => boolean): boolean {
  if (s.length === 0) return false;
  for (let i = 0; i < s.length; i++) if (!ok(s.charCodeAt(i))) return false;
  return true;
}

/**
 * Lowercase hex digit.
 *
 * @param c - Character code.
 * @returns True for 0-9 or a-f.
 */
function hexDigit(c: number): boolean {
  return (c >= 48 && c <= 57) || (c >= 97 && c <= 102);
}

/**
 * Oracle: a version-4 UUID, lowercase, hyphenated, by position.
 *
 * @param v - Candidate.
 * @returns Whether it is one.
 */
function oracleUuid(v: unknown): boolean {
  if (typeof v !== "string" || v.length !== 36) return false;
  for (let i = 0; i < 36; i++) {
    const c = v.charCodeAt(i);
    if (i === 8 || i === 13 || i === 18 || i === 23) {
      if (c !== 45) return false;
    } else if (!hexDigit(c)) return false;
  }
  return v[14] === "4" && "89ab".includes(v[19] ?? "");
}

/**
 * Oracle for parseCheckin.
 *
 * @param text - Body (any value).
 * @param label - Input name for failure messages.
 */
function check(text: unknown, label: string): void {
  const got = parseCheckin(text);
  if (typeof text !== "string" || text.length > MAX_CHECKIN_CHARS) {
    assert.equal(got, null, label);
    return;
  }
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    assert.equal(got, null, label);
    return;
  }
  const obj =
    data !== null && typeof data === "object" && !Array.isArray(data)
      ? (data as Record<string, unknown>)
      : null;
  const wanted = ["arch", "install_id", "kind", "os", "version"];
  const keys = obj ? Object.keys(obj) : [];
  const platform = (v: unknown): v is string =>
    typeof v === "string" &&
    v.length <= 32 &&
    every(v, (c) => (c >= 97 && c <= 122) || (c >= 48 && c <= 57) || c === 95);
  const version = (v: unknown): v is string =>
    typeof v === "string" && v.length <= 64 && every(v, (c) => c >= 33 && c <= 126);
  const ok =
    obj !== null &&
    keys.length === wanted.length &&
    wanted.every((k) => keys.includes(k)) &&
    oracleUuid(obj["install_id"]) &&
    version(obj["version"]) &&
    platform(obj["os"]) &&
    platform(obj["arch"]) &&
    (obj["kind"] === "daemon" || obj["kind"] === "desktop");
  if (!ok || obj === null) {
    assert.equal(got, null, label);
    return;
  }
  assert.deepEqual(
    got,
    {
      install_id: obj["install_id"],
      version: obj["version"],
      os: obj["os"],
      arch: obj["arch"],
      kind: obj["kind"],
    },
    label,
  );
}

const FRAGMENTS = [
  "{",
  "}",
  "[",
  "]",
  ",",
  ":",
  '"',
  "\\",
  "null",
  "1",
  " ",
  '"install_id":',
  '"version":',
  '"os":',
  '"arch":',
  '"kind":',
  '"daemon"',
  '"desktop"',
  '"demo"',
  '"__proto__":{}',
  '"ip":"203.0.113.9"',
  "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f",
  "-",
  "A",
  "4",
  "8",
  "c",
  "é",
  "\\u0000",
  "x".repeat(40),
];

describe("parseCheckin", () => {
  it("replays the seed corpus", () => {
    const all = seeds("checkin");
    for (const [name, raw] of all) check(raw, name);
    assert.ok(parseCheckin(seed("checkin", "daemon.json")));
    assert.equal(parseCheckin(seed("checkin", "desktop_reordered.json"))?.kind, "desktop");
    for (const off of [
      "demo_kind.json",
      "upper_id.json",
      "v1_id.json",
      "extra_field.json",
      "bad_tokens.json",
      "array.json",
    ]) {
      assert.equal(parseCheckin(seed("checkin", off)), null, off);
    }
    // JSON keeps the last of duplicate keys; the oracle agrees either way.
    assert.equal(
      parseCheckin(seed("checkin", "duplicate_key.json"))?.install_id,
      "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d",
    );
    assert.equal(parseCheckin(" ".repeat(MAX_CHECKIN_CHARS + 1)), null);
    for (const v of [null, undefined, 7, {}, []]) check(v, String(v));
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("checkin").map(([, raw]) => raw);
    const rand = rng(20261118);
    for (let n = 0; n < 6000; n++) {
      const input = mutate(rand, choose(rand, all), FRAGMENTS);
      check(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});
