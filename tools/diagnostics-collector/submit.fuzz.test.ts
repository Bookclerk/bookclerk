// The `POST /submit` body boundary (fuzzing ADR decision 6): seed corpus
// replay (`corpus/submit/`) and seeded mutation against an oracle written
// without submit.ts's own checks.
//
// parseSubmit reads a body anyone can send; what it admits is enriched and
// stored in B2, so an admitted report must have exactly the shape the
// dashboard and triage read, and nothing the oracle refuses may get through.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { MAX_SUBMIT_EVENTS, parseSubmit } from "./submit.ts";
import { choose, mutate, rng, seed, seeds } from "./test-support/fuzz.ts";

/**
 * Whether a value is a string of at most `max` UTF-16 units.
 *
 * @param v - Candidate.
 * @param max - Longest allowed length.
 * @returns True when it is one.
 */
function shortString(v: unknown, max: number): v is string {
  return typeof v === "string" && v.length <= max;
}

/**
 * Oracle: the reason a parsed body is refused, or null when it is a report.
 *
 * @param data - Parsed JSON.
 * @returns Null for a report, otherwise any non-null marker.
 */
function refusal(data: unknown): string | null {
  if (data === null || typeof data !== "object" || Array.isArray(data)) return "not an object";
  const obj = data as Record<string, unknown>;
  const trigger = obj["trigger"];
  if (!shortString(trigger, 64) || trigger === "") return "trigger";
  const version = obj["version"];
  if (!shortString(version, 64) || /^\s*$/.test(version)) return "version";
  if (!shortString(obj["os"], 64)) return "os";
  const archived = obj["archived_at_unix_ms"];
  if (!(archived === undefined || typeof archived === "number")) return "archived";
  const events = obj["events"];
  if (!Array.isArray(events) || events.length > MAX_SUBMIT_EVENTS) return "events";
  for (const ev of events) {
    if (ev === null || typeof ev !== "object") return "event";
    const rec = ev as Record<string, unknown>;
    const limits: [string, number][] = [
      ["message", 8000],
      ["level", 32],
      ["target", 256],
    ];
    for (const [key, max] of limits) {
      const v = rec[key];
      if (typeof v === "string" && v.length > max) return key;
    }
  }
  return null;
}

/**
 * Oracle for parseSubmit.
 *
 * @param text - Body text.
 * @param label - Input name for failure messages.
 */
function check(text: string, label: string): void {
  const got = parseSubmit(text);
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    assert.equal(typeof got, "string", label);
    return;
  }
  const why = text === "" ? "empty" : refusal(data);
  if (why !== null) {
    assert.equal(typeof got, "string", `${label}: oracle refuses (${why})`);
    return;
  }
  assert.notEqual(typeof got, "string", `${label}: oracle admits, parser said ${String(got)}`);
  // An admitted report is the body as sent, field for field.
  assert.deepEqual(got, data, label);
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
  "true",
  "1",
  "-0",
  "1e400",
  " ",
  " ",
  "﻿",
  '"trigger":',
  '"version":',
  '"os":',
  '"events":',
  '"archived_at_unix_ms":',
  '"message":',
  '"level":',
  '"target":',
  '"__proto__":{}',
  '{"message":"m"}',
  "[]",
  "{}",
  "é",
  "\\u0000",
  "\\ud800",
  "x".repeat(70),
  "y".repeat(300),
];

describe("parseSubmit", () => {
  it("replays the seed corpus", () => {
    for (const [name, raw] of seeds("submit")) check(raw, name);
    for (const ok of ["report.json", "empty_events.json", "extra_fields.json"]) {
      assert.notEqual(typeof parseSubmit(seed("submit", ok)), "string", ok);
    }
    for (const off of [
      "archived_as_string.json",
      "blank_version.json",
      "empty_trigger.json",
      "events_object.json",
      "long_message.json",
      "long_trigger.json",
      "null_event.json",
      "too_many_events.json",
      "top_array.json",
      "truncated.json",
    ]) {
      assert.equal(typeof parseSubmit(seed("submit", off)), "string", off);
    }
    check("", "empty body");
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("submit").map(([, raw]) => raw);
    const rand = rng(20261119);
    for (let n = 0; n < 6000; n++) {
      const input = mutate(rand, choose(rand, all), FRAGMENTS);
      check(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});
