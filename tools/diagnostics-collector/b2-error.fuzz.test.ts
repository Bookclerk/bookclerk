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
import { URL } from "node:url";
import { mutate, rng } from "./test-support/fuzz.ts";
import { UpstreamError, b2ErrorCode } from "./worker.ts";

const CORPUS = new URL("./corpus/b2_error/", import.meta.url);
const LIMIT = 4096;

/** Seed inputs, keyed by file name. */
function seeds(): [string, string][] {
  return readdirSync(CORPUS)
    .sort()
    .map((name): [string, string] => [name, readFileSync(new URL(name, CORPUS), "utf8")]);
}

/** True when every character is a-z or underscore and there are 1 to 40. */
function isShortIdentifier(s: string): boolean {
  if (s.length < 1 || s.length > 40) return false;
  for (const ch of s) {
    if (ch !== "_" && (ch < "a" || ch > "z")) return false;
  }
  return true;
}

/**
 * Checks one extraction against the oracle.
 *
 * @param text - Response body.
 * @param label - Input name for failure messages.
 */
function check(text: string, label: string): void {
  const got = b2ErrorCode(text);
  let expected: string | undefined;
  if (text.length <= LIMIT) {
    try {
      const body: unknown = JSON.parse(text);
      const code = body !== null && typeof body === "object" && !Array.isArray(body) ? (body as Record<string, unknown>)["code"] : undefined;
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
      const text = mutate(rand, all[Math.floor(rand() * all.length)] ?? "", FRAGMENTS);
      check(text, `iteration ${n}: ${JSON.stringify(text).slice(0, 200)}`);
    }
  });

  it("ignores bodies past the size limit", () => {
    const body = JSON.stringify({ code: "unauthorized", pad: "x".repeat(LIMIT) });
    check(body, "oversized");
    assert.equal(b2ErrorCode(body), undefined);
  });
});
