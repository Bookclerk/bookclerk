// /triage request admission: seed corpus replay and seeded mutation fuzzing.
//
// The body is report-derived, so admitBatch is an untrusted-input boundary
// (fuzzing ADR decision 6). Every input must be either refused for a reason
// the oracle confirms, or admitted with a prompt and reservation the oracle
// recomputes with Buffer instead of the module's own helpers.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";
import {
  MAX_OUTPUT_TOKENS,
  MAX_TRIAGE_BODY_BYTES,
  MODELS,
  SYSTEM_PROMPT,
  TEMPLATE_OVERHEAD_TOKENS,
  admitBatch,
  type ModelRates,
} from "./triage.ts";
import { mutate, rng } from "./test-support/fuzz.ts";

const CORPUS = new URL("./corpus/triage_admit/", import.meta.url);
const RATES = MODELS["@cf/ibm-granite/granite-4.0-h-micro"];
const SMALL_CONTEXT: ModelRates = { ...RATES, context: 4096 };

/** Seed inputs, keyed by file name. */
function seeds(): [string, string][] {
  return readdirSync(CORPUS)
    .sort()
    .map((name): [string, string] => [name, readFileSync(new URL(name, CORPUS), "utf8")]);
}

/**
 * Checks one admission result against an independent oracle.
 *
 * @param raw - Request body.
 * @param rates - Model rates.
 * @param label - Input name for failure messages.
 */
function check(raw: string, rates: ModelRates, label: string): void {
  const got = admitBatch(raw, rates);
  let parsed: unknown;
  let parses = true;
  try {
    parsed = JSON.parse(raw);
  } catch {
    parses = false;
  }
  const rawBytes = Buffer.byteLength(raw, "utf8");
  if (!got.ok) {
    const reasons = [
      rawBytes > MAX_TRIAGE_BODY_BYTES,
      !parses,
      parses && (!Array.isArray(parsed) || parsed.length === 0),
    ];
    if (parses && Array.isArray(parsed) && parsed.length > 0) {
      const msg = `UNTRUSTED_DATA_BEGIN\n${JSON.stringify(parsed)}\nUNTRUSTED_DATA_END`;
      const msgBytes = Buffer.byteLength(msg, "utf8");
      const bound =
        Buffer.byteLength(SYSTEM_PROMPT, "utf8") + msgBytes + TEMPLATE_OVERHEAD_TOKENS;
      reasons.push(msgBytes > MAX_TRIAGE_BODY_BYTES, bound + MAX_OUTPUT_TOKENS > rates.context);
    }
    assert.ok(reasons.some(Boolean), `${label}: refused without cause (${got.error})`);
    assert.ok([400, 413].includes(got.status), label);
    return;
  }
  assert.ok(rawBytes <= MAX_TRIAGE_BODY_BYTES, label);
  assert.ok(parses && Array.isArray(parsed) && parsed.length > 0, label);
  assert.equal(
    got.userMessage,
    `UNTRUSTED_DATA_BEGIN\n${JSON.stringify(parsed)}\nUNTRUSTED_DATA_END`,
    label,
  );
  const msgBytes = Buffer.byteLength(got.userMessage, "utf8");
  assert.ok(msgBytes <= MAX_TRIAGE_BODY_BYTES, label);
  const bound = Buffer.byteLength(SYSTEM_PROMPT, "utf8") + msgBytes + TEMPLATE_OVERHEAD_TOKENS;
  assert.ok(got.inputBound >= bound, `${label}: input bound below the byte count`);
  assert.ok(got.inputBound + MAX_OUTPUT_TOKENS <= rates.context, label);
  const worst = (bound * rates.input + MAX_OUTPUT_TOKENS * rates.output) / 1_000_000;
  assert.ok(got.reserved >= worst, `${label}: reservation below the worst case`);
  assert.ok(Number.isSafeInteger(got.reserved) && got.reserved > 0, label);
}

/** Fragments mutations splice in: JSON syntax, growth and Unicode. */
const FRAGMENTS = [
  "[", "]", "{", "}", ",", ":", '"', "\\", "\\u", "\\ud83d", "1e308", "1e20", "-0", "0.1",
  "null", "true", "[]", "{}", '"report_id"', "é", "中", "😀", " ", "\0",
];

describe("admitBatch", () => {
  it("replays the seed corpus", () => {
    const all = seeds();
    assert.ok(all.length > 0);
    for (const [name, raw] of all) {
      check(raw, RATES, name);
      check(raw, SMALL_CONTEXT, `${name} (small context)`);
    }
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds().map(([, raw]) => raw);
    const rand = rng(20261024);
    for (let n = 0; n < 4000; n++) {
      const raw = mutate(rand, all[Math.floor(rand() * all.length)] ?? "", FRAGMENTS);
      check(raw, n % 2 ? RATES : SMALL_CONTEXT, `iteration ${n}: ${JSON.stringify(raw).slice(0, 200)}`);
    }
  });

  it("refuses a batch that only grows past the limit when re-serialized", () => {
    const n = Math.floor((150 * 1024) / 5);
    const raw = `[{"f":[${Array(n).fill("1e20").join(",")}]}]`;
    assert.ok(Buffer.byteLength(raw) < MAX_TRIAGE_BODY_BYTES);
    const refused = admitBatch(raw, RATES);
    assert.ok(!refused.ok && refused.status === 413);
    check(raw, RATES, "growth");
  });

  it("prices the largest batch the context admits within one call's worst case", () => {
    // Bytes bound tokens, so the context window, not the body cap, is the
    // binding limit for this model.
    const room =
      RATES.context -
      MAX_OUTPUT_TOKENS -
      TEMPLATE_OVERHEAD_TOKENS -
      Buffer.byteLength(SYSTEM_PROMPT, "utf8") -
      200;
    const raw = JSON.stringify([{ s: "a".repeat(room) }]);
    check(raw, RATES, "at limit");
    const got = admitBatch(raw, RATES);
    assert.ok(got.ok);
    assert.ok(got.reserved < 300, `one call reserves ${got.reserved} neurons`);
  });
});
