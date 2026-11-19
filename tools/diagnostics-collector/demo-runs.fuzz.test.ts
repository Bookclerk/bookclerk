// The demo run event boundary (fuzzing ADR decision 6): seed corpus replay
// (`corpus/demo_run/`) and seeded mutation against an oracle written
// without demo-runs.ts's own patterns.
//
// parseDemoRunEvent reads the value the demo Worker passes over its service
// binding; its result is what a demo_runs row stores, so nothing else may
// get through. Inputs are JSON text, parsed here into the value the RPC
// call would carry.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { parseDemoRunEvent } from "./demo-runs.ts";
import { choose, mutate, rng, seed, seeds } from "./test-support/fuzz.ts";

/**
 * Oracle: a lowercase hyphenated version-4 UUID, group by group.
 *
 * @param v - Candidate.
 * @returns Whether it is one.
 */
function oracleUuid(v: unknown): boolean {
  if (typeof v !== "string") return false;
  const groups = v.split("-");
  const lens = [8, 4, 4, 4, 12];
  if (groups.length !== 5) return false;
  for (let i = 0; i < 5; i++) {
    const g = groups[i] ?? "";
    if (g.length !== lens[i]) return false;
    for (const ch of g) if (!"0123456789abcdef".includes(ch)) return false;
  }
  return (groups[2] ?? "")[0] === "4" && "89ab".includes((groups[3] ?? "")[0] ?? "");
}

/**
 * Oracle: a whole number of seconds from 2020-01-01 to before 2100-01-01.
 *
 * @param v - Candidate.
 * @returns Whether it is one.
 */
function oracleTime(v: unknown): v is number {
  return (
    typeof v === "number" &&
    Number.isInteger(v) &&
    v >= Date.UTC(2020, 0, 1) / 1000 &&
    v < Date.UTC(2100, 0, 1) / 1000
  );
}

/**
 * Oracle for parseDemoRunEvent.
 *
 * @param value - The event value.
 * @param label - Input name for failure messages.
 */
function check(value: unknown, label: string): void {
  const got = parseDemoRunEvent(value);
  const obj =
    value !== null && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  const common = ["kind", "run_id", "started_at", "ends_at", "version"];
  const extra: Record<string, string[]> = {
    start: [],
    ready: ["startup_seconds"],
    stop: ["ended_at", "reason"],
  };
  const kind = obj?.["kind"];
  const wanted =
    typeof kind === "string" && Object.hasOwn(extra, kind)
      ? [...common, ...(extra[kind] ?? [])]
      : null;
  const keys = obj ? Object.keys(obj).sort() : [];
  let ok =
    obj !== null &&
    wanted !== null &&
    keys.join(",") === [...wanted].sort().join(",") &&
    oracleUuid(obj["run_id"]) &&
    typeof obj["version"] === "string" &&
    obj["version"].length <= 64 &&
    [...obj["version"]].every((ch) => /[A-Za-z0-9]/.test(ch) || "._+-".includes(ch)) &&
    oracleTime(obj["started_at"]) &&
    oracleTime(obj["ends_at"]) &&
    obj["ends_at"] > obj["started_at"] &&
    obj["ends_at"] - obj["started_at"] <= 86_400;
  if (ok && obj && kind === "ready") {
    const s = obj["startup_seconds"];
    ok = typeof s === "number" && Number.isInteger(s) && s >= 0 && s <= 600;
  }
  if (ok && obj && kind === "stop") {
    const e = obj["ended_at"];
    ok =
      oracleTime(e) &&
      e >= Number(obj["started_at"]) &&
      e <= Number(obj["ends_at"]) + 86_400 &&
      ["visitor", "idle", "time", "failed", "stopped", "sweep", "restart"].includes(
        String(obj["reason"]),
      ) &&
      typeof obj["reason"] === "string";
  }
  if (!ok || obj === null) {
    assert.equal(got, null, label);
    return;
  }
  assert.deepEqual(got, { ...obj }, label);
}

/**
 * The value an input's JSON text carries, or the text itself when it is not JSON.
 *
 * @param text - Input.
 * @returns The value.
 */
function valueOf(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

const FRAGMENTS = [
  "{",
  "}",
  "[",
  "]",
  ",",
  ":",
  '"',
  "null",
  "true",
  "1",
  "-1",
  "0.5",
  "1e3",
  " ",
  '"kind":',
  '"start"',
  '"ready"',
  '"stop"',
  '"run_id":',
  '"started_at":',
  '"ends_at":',
  '"ended_at":',
  '"startup_seconds":',
  '"reason":',
  '"version":',
  '"idle"',
  '"missed"',
  '"__proto__":{}',
  "1792843200",
  "1792864801",
  "1792929601",
  "4102444800",
  "601",
  "A",
  "4",
  "c",
  "é",
  "x".repeat(70),
];

describe("parseDemoRunEvent", () => {
  it("replays the seed corpus", () => {
    for (const [name, raw] of seeds("demo_run")) check(valueOf(raw), name);
    for (const ok of [
      "start.json",
      "ready.json",
      "stop.json",
      "stop_reordered.json",
      "stop_restart.json",
      "start_seven_hours.json",
    ]) {
      assert.ok(parseDemoRunEvent(valueOf(seed("demo_run", ok))), ok);
    }
    for (const off of [
      "missed_reason.json",
      "extra_field.json",
      "zero_length.json",
      "fractional_startup.json",
      "bad_version.json",
      "upper_id.json",
      "stop_before_start.json",
      "array.json",
      "regression_inherited_kind.json",
    ]) {
      assert.equal(parseDemoRunEvent(valueOf(seed("demo_run", off))), null, off);
    }
    for (const v of [null, undefined, 7, "start", {}, []]) check(v, String(v));
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("demo_run").map(([, raw]) => raw);
    const rand = rng(20261119);
    for (let n = 0; n < 6000; n++) {
      const input = mutate(rand, choose(rand, all), FRAGMENTS);
      check(valueOf(input), `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});
