// Members' triage forms (fuzzing ADR decision 6): seed corpus replay and
// seeded mutation of the triage mark (`corpus/triage_form/`), mute
// (`corpus/mute_form/`) and owner close (`corpus/close_form/`) bodies,
// against oracles that split and decode the form body by hand.
//
// - parseTriageMark decides which reports a member marks seen or noise.
// - parseMuteAction decides which signature leaves the triage queue and the
//   ingest pull, why, and until when.
// - parseCloseAction decides which issue an owner closes on GitHub, and the
//   fixed-in label it gets.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { MAX_FORM_CHARS, MAX_ISSUE_NUMBER } from "./dashboard/links.ts";
import {
  MAX_MUTE_DAYS,
  MAX_MUTE_REASON,
  MAX_TRIAGE_REPORTS,
  parseCloseAction,
  parseMuteAction,
  parseTriageMark,
} from "./dashboard/triage-forms.ts";
import { choose, mutate, rng, seed, seeds } from "./test-support/fuzz.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * Decodes one form component the way `application/x-www-form-urlencoded`
 * says, written out by hand: `+` is a space, `%XX` with two hex digits is a
 * byte, anything else stays as it is, and the bytes are read as UTF-8 with
 * replacement characters.
 *
 * @param text - Raw component.
 * @returns The decoded text.
 */
function decodeComponent(text: string): string {
  const raw = new TextEncoder().encode(text.replace(/\+/g, " "));
  const out: number[] = [];
  const hex = (b: number | undefined): number =>
    b === undefined
      ? -1
      : b >= 48 && b <= 57
        ? b - 48
        : b >= 65 && b <= 70
          ? b - 55
          : b >= 97 && b <= 102
            ? b - 87
            : -1;
  for (let i = 0; i < raw.length; i++) {
    const b = raw[i] ?? 0;
    const hi = hex(raw[i + 1]);
    const lo = hex(raw[i + 2]);
    if (b === 37 && hi >= 0 && lo >= 0) {
      out.push(hi * 16 + lo);
      i += 2;
    } else {
      out.push(b);
    }
  }
  return new TextDecoder().decode(new Uint8Array(out));
}

/**
 * Splits a form body by hand into `[name, value]` pairs, in order.
 *
 * @param body - Form body.
 * @returns The pairs.
 */
function pairs(body: string): [string, string][] {
  return body
    .split("&")
    .filter((p) => p !== "")
    .map((p): [string, string] => {
      const eq = p.indexOf("=");
      return eq < 0
        ? [decodeComponent(p), ""]
        : [decodeComponent(p.slice(0, eq)), decodeComponent(p.slice(eq + 1))];
    });
}

/**
 * The first value of a field.
 *
 * @param list - Pairs.
 * @param name - Field name.
 * @returns The value, or undefined.
 */
function get(list: [string, string][], name: string): string | undefined {
  return list.find(([n]) => n === name)?.[1];
}

/**
 * Oracle: an issue number as the forms take it.
 *
 * @param t - Field value.
 * @returns The number, or null.
 */
function issue(t: string | undefined): number | null {
  if (t === undefined || !/^[1-9][0-9]{0,9}$/.test(t)) return null;
  const n = Number(t);
  return n <= MAX_ISSUE_NUMBER ? n : null;
}

/**
 * Oracle: a release version, after an optional leading `v` is dropped.
 *
 * @param t - Field value without the `v`.
 * @returns Whether it is one.
 */
function release(t: string): boolean {
  const [core = "", ...rest] = t.split("-");
  const pre = rest.join("-");
  const nums = core.split(".");
  return (
    nums.length === 3 &&
    nums.every((n) => /^[0-9]{1,6}$/.test(n)) &&
    (!t.includes("-") || (pre.length >= 1 && pre.length <= 40 && /^[0-9A-Za-z.-]+$/.test(pre)))
  );
}

/**
 * Oracle for parseTriageMark.
 *
 * @param body - Form body (any value).
 * @param label - Input name for failure messages.
 */
function checkMark(body: unknown, label: string): void {
  const got = parseTriageMark(body);
  if (typeof body !== "string" || body.length > MAX_FORM_CHARS) {
    assert.equal(got, null, label);
    return;
  }
  const list = pairs(body);
  const state = get(list, "state");
  const reports: string[] = [];
  for (const [n, v] of list) if (n === "report" && !reports.includes(v)) reports.push(v);
  const ok =
    (state === "seen" || state === "noise" || state === "clear") &&
    reports.length >= 1 &&
    reports.length <= MAX_TRIAGE_REPORTS &&
    reports.every((r) => UUID.test(r));
  assert.deepEqual(got, ok ? { state, reports } : null, label);
}

/**
 * Oracle for parseMuteAction.
 *
 * @param body - Form body (any value).
 * @param label - Input name for failure messages.
 */
function checkMute(body: unknown, label: string): void {
  const got = parseMuteAction(body);
  if (typeof body !== "string" || body.length > MAX_FORM_CHARS) {
    assert.equal(got, null, label);
    return;
  }
  const list = pairs(body);
  const action = get(list, "action");
  const signature = get(list, "signature") ?? "";
  if (!/^[0-9a-f]{16}$/.test(signature) || (action !== "mute" && action !== "unmute")) {
    assert.equal(got, null, label);
    return;
  }
  if (action === "unmute") {
    assert.deepEqual(got, { action, signature }, label);
    return;
  }
  const reason = (get(list, "reason") ?? "").trim();
  const days = get(list, "days") ?? "";
  const rawVersion = get(list, "until_version") ?? "";
  const version = rawVersion.startsWith("v") ? rawVersion.slice(1) : rawVersion;
  let control = false;
  for (const ch of reason) {
    const c = ch.codePointAt(0) ?? 0;
    if (c < 32 || c === 127) control = true;
  }
  const reasonOk = reason.length >= 1 && reason.length <= MAX_MUTE_REASON && !control;
  const daysOk = /^[1-9][0-9]{0,2}$/.test(days) && Number(days) <= MAX_MUTE_DAYS;
  if (!reasonOk || (days === "") === (version === "")) {
    assert.equal(got, null, label);
  } else if (days !== "") {
    assert.deepEqual(
      got,
      daysOk ? { action, signature, reason, days: Number(days), untilVersion: null } : null,
      label,
    );
  } else {
    assert.deepEqual(
      got,
      release(version) ? { action, signature, reason, days: null, untilVersion: version } : null,
      label,
    );
  }
}

/**
 * Oracle for parseCloseAction.
 *
 * @param body - Form body (any value).
 * @param label - Input name for failure messages.
 */
function checkClose(body: unknown, label: string): void {
  const got = parseCloseAction(body);
  if (typeof body !== "string" || body.length > MAX_FORM_CHARS) {
    assert.equal(got, null, label);
    return;
  }
  const list = pairs(body);
  const n = issue(get(list, "issue"));
  const how = get(list, "how");
  const rawVersion = get(list, "version") ?? "";
  const version = rawVersion.startsWith("v") ? rawVersion.slice(1) : rawVersion;
  let want = null;
  if (n !== null && how === "not_planned") want = { issue: n, how, version: null };
  if (n !== null && how === "fixed" && release(version)) want = { issue: n, how, version };
  assert.deepEqual(got, want, label);
}

const FRAGMENTS = [
  "&",
  "=",
  "+",
  "%",
  "%2",
  "%0A",
  "%7F",
  "%zz",
  "%E2%80%8B",
  "state=",
  "report=",
  "action=",
  "signature=",
  "reason=",
  "days=",
  "until_version=",
  "issue=",
  "how=",
  "version=",
  "seen",
  "noise",
  "clear",
  "mute",
  "unmute",
  "fixed",
  "not_planned",
  "0123456789abcdef",
  "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4",
  "v",
  "1.2.3",
  "-rc.1",
  "0",
  "365",
  "366",
  " ",
  "\u0000",
  "é",
  "A",
];

/**
 * Replays one corpus and mutates it.
 *
 * @param target - Corpus directory.
 * @param oracle - The check.
 * @param seedNumber - PRNG seed.
 */
function fuzz(target: string, oracle: (b: unknown, l: string) => void, seedNumber: number): void {
  const all = seeds(target);
  for (const [name, raw] of all) oracle(raw, name);
  for (const v of [null, undefined, 7, {}, ["state=seen"], "x".repeat(MAX_FORM_CHARS + 1)])
    oracle(v, String(v).slice(0, 20));
  const rand = rng(seedNumber);
  const bodies = all.map(([, raw]) => raw);
  for (let n = 0; n < 5000; n++) {
    const input = mutate(rand, choose(rand, bodies), FRAGMENTS);
    oracle(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
  }
}

describe("triage mark form", () => {
  it("reads the seeds as documented", () => {
    assert.deepEqual(parseTriageMark(seed("triage_form", "bulk_noise.txt")), {
      state: "noise",
      reports: ["7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4", "d2b80f13-0000-4000-8000-000000000002"],
    });
    for (const off of ["no_reports.txt", "bad_state.txt", "upper_uuid.txt", "empty.txt"]) {
      assert.equal(parseTriageMark(seed("triage_form", off)), null, off);
    }
  });

  it("holds the oracle under seeded mutation", () => fuzz("triage_form", checkMark, 20261119));
});

describe("mute form", () => {
  it("reads the seeds as documented", () => {
    assert.deepEqual(parseMuteAction(seed("mute_form", "until_version.txt")), {
      action: "mute",
      signature: "0123456789abcdef",
      reason: "fixed in next",
      days: null,
      untilVersion: "0.9.1",
    });
    assert.equal(parseMuteAction(seed("mute_form", "days.txt"))?.action, "mute");
    const odd = parseMuteAction(seed("mute_form", "malformed_percent.txt"));
    assert.equal(odd?.action === "mute" ? odd.reason : null, "100% sure%zz");
    for (const off of [
      "both_ends.txt",
      "no_end.txt",
      "blank_reason.txt",
      "control_reason.txt",
      "too_many_days.txt",
      "short_signature.txt",
    ]) {
      assert.equal(parseMuteAction(seed("mute_form", off)), null, off);
    }
  });

  it("holds the oracle under seeded mutation", () => fuzz("mute_form", checkMute, 20261120));
});

describe("close form", () => {
  it("reads the seeds as documented", () => {
    assert.deepEqual(parseCloseAction(seed("close_form", "fixed.txt")), {
      issue: 412,
      how: "fixed",
      version: "0.9.1",
    });
    for (const off of [
      "fixed_no_version.txt",
      "fixed_bad_version.txt",
      "zero_issue.txt",
      "bad_how.txt",
    ]) {
      assert.equal(parseCloseAction(seed("close_form", off)), null, off);
    }
  });

  it("holds the oracle under seeded mutation", () => fuzz("close_form", checkClose, 20261121));
});
