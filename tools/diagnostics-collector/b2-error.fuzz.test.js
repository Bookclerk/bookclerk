// B2 error-code extraction: seed corpus replay and seeded mutation fuzzing.
//
// B2 error bodies are bytes the collector did not produce, and the code read
// from them reaches logs and authenticated responses, so b2ErrorCode is an
// untrusted-input boundary (fuzzing ADR decision 6). Every input must yield
// either nothing or a short lowercase identifier that is exactly the body's
// own `code`, as an independent check recomputes it.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { UpstreamError, b2ErrorCode } from "./worker.js";

const CORPUS = new URL("./corpus/b2_error/", import.meta.url);
const LIMIT = 4096;

/** Seed inputs, keyed by file name. */
function seeds() {
  return readdirSync(CORPUS)
    .sort()
    .map((name) => [name, readFileSync(new URL(name, CORPUS), "utf8")]);
}

/** True when every character is a-z or underscore and there are 1 to 40. */
function isShortIdentifier(s) {
  if (s.length < 1 || s.length > 40) return false;
  for (const ch of s) {
    if (ch !== "_" && (ch < "a" || ch > "z")) return false;
  }
  return true;
}

/**
 * Checks one extraction against the oracle.
 *
 * @param {string} text Response body.
 * @param {string} label Input name for failure messages.
 */
function check(text, label) {
  const got = b2ErrorCode(text);
  let expected;
  if (text.length <= LIMIT) {
    try {
      const body = JSON.parse(text);
      const code = body !== null && typeof body === "object" && !Array.isArray(body) ? body.code : undefined;
      expected = typeof code === "string" && isShortIdentifier(code) ? code : undefined;
    } catch {
      expected = undefined;
    }
  }
  assert.equal(got, expected, label);
  // Whatever is kept goes into a message verbatim, so it must stay inert.
  const message = new UpstreamError("b2 list", 401, got).message;
  assert.match(message, /^b2 list failed: HTTP 401(: [a-z_]{1,40})?$/, label);
}

/** Fragments mutations splice in: JSON syntax and code-shaped text. */
const FRAGMENTS = [
  "{", "}", "[", "]", ",", ":", '"', "\\", "\\u0041", '"code"', '"code":', '"unauthorized"',
  '"bad_auth_token"', "null", "true", "1", "<", ">", "\n", " ", "é", "😀", "\0", "_", "A",
];

/** Deterministic PRNG (mulberry32) so failures replay from the seed. */
function rng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function mutate(rand, text) {
  const pick = (n) => Math.floor(rand() * n);
  for (let k = 1 + pick(6); k > 0; k--) {
    const pos = pick(text.length + 1);
    const op = rand();
    if (op < 0.5) {
      text = text.slice(0, pos) + FRAGMENTS[pick(FRAGMENTS.length)] + text.slice(pos);
    } else if (op < 0.75) {
      text = text.slice(0, pos) + text.slice(pos + 1 + pick(8));
    } else {
      text = text.slice(0, pos) + String.fromCharCode(pick(0x3000)) + text.slice(pos);
    }
  }
  return text;
}

describe("b2ErrorCode", () => {
  it("replays the seed corpus", () => {
    const all = seeds();
    assert.ok(all.length > 0);
    for (const [name, text] of all) check(text, name);
    assert.equal(b2ErrorCode(readFileSync(new URL("unauthorized.json", CORPUS), "utf8")), "unauthorized");
    assert.equal(b2ErrorCode(readFileSync(new URL("markup_code.json", CORPUS), "utf8")), undefined);
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds().map(([, text]) => text);
    const rand = rng(20261024);
    for (let n = 0; n < 4000; n++) {
      const text = mutate(rand, all[Math.floor(rand() * all.length)]);
      check(text, `iteration ${n}: ${JSON.stringify(text).slice(0, 200)}`);
    }
  });

  it("ignores bodies past the size limit", () => {
    const body = JSON.stringify({ code: "unauthorized", pad: "x".repeat(LIMIT) });
    check(body, "oversized");
    assert.equal(b2ErrorCode(body), undefined);
  });
});
