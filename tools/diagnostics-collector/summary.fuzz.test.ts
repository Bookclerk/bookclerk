// Cross-language replay and mutation fuzzing of the summary comment renderer.
//
// The owner's "Update GitHub now" button renders an issue's summary comment
// in the Worker (dashboard/summary.ts); the scheduled ingest run renders the
// same comment with scripts/summary_comment.py. The figures are the
// collector's, but versions and platforms come from reports anyone can
// submit, so the renderer is an untrusted-input boundary. The Python module
// is the independent oracle: every seed input and every mutation of one is
// rendered by both, through scripts/tests/summary_comment_oracle.py in one
// process, and the output must be identical, byte for byte. The seed corpus
// is the Python test's (scripts/tests/summary_comment_corpus/), so the two
// suites share one set of inputs. The origin check that decides whether
// report links are rendered is compared the same way.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath, URL } from "node:url";
import { describe, it } from "node:test";
import { MARKER, MAX_SUMMARIES, dashboardOrigin, defang, renderSummaries } from "./dashboard/summary.ts";
import { mutate, rng } from "./test-support/fuzz.ts";

const ROOT = new URL("../../", import.meta.url);
const CORPUS = new URL("scripts/tests/summary_comment_corpus/", ROOT);
const ORACLE = fileURLToPath(new URL("scripts/tests/summary_comment_oracle.py", ROOT));
const DASHBOARD = "https://diagnostics.example.test";
const UUID_A = "11111111-1111-4111-8111-111111111111";

/** One oracle answer: the rendering, the origin decision, or a decode error. */
type OracleAnswer = { rendered?: unknown; origin?: unknown; error?: string };

/** One renderer input with its failure label. */
type Case = { payload: unknown; dashboard: string | null; label: string };

/** The seed corpus as `[name, text]`, sorted by name. */
function seeds(): [string, string][] {
  return readdirSync(CORPUS)
    .sort()
    .map((name): [string, string] => [name, readFileSync(new URL(name, CORPUS), "utf8")]);
}

/** JSON.parse, or undefined when the text is not JSON. */
function parse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/**
 * Runs the Python oracle once over every request and returns its answers,
 * one per request, in order.
 *
 * @param requests - Request objects (see the oracle's docstring).
 * @returns Answers.
 */
function oracle(requests: object[]): OracleAnswer[] {
  const input = requests.map((r) => JSON.stringify(r)).join("\n") + "\n";
  const run = spawnSync("python3", ["-I", ORACLE], { input, encoding: "utf8", maxBuffer: 1 << 28 });
  assert.equal(run.status, 0, `oracle failed: ${run.stderr}`);
  const lines = run.stdout.split("\n").filter((l) => l !== "");
  assert.equal(lines.length, requests.length, "one answer per request");
  return lines.map((l): OracleAnswer => JSON.parse(l));
}

/**
 * Compares the JavaScript rendering of each payload with the oracle's.
 *
 * @param cases - Inputs.
 */
function agree(cases: Case[]): void {
  // Payloads go to Python as the JSON text JavaScript writes, so both sides
  // start from the same value (one number form, no duplicate keys).
  const answers = oracle(cases.map((c) => ({ payload: c.payload, dashboard: c.dashboard })));
  cases.forEach(({ payload, dashboard, label }, i) => {
    const answer = answers[i];
    assert.ok(answer !== undefined && !("error" in answer), `${label}: the oracle could not decode the input`);
    const ours = renderSummaries(payload, dashboardOrigin(dashboard)).map((r) => [r.issue, r.fingerprint, r.body]);
    assert.deepEqual(ours, answer.rendered, label);
  });
}

const FRAGMENTS = [
  "@", "#1", "GH-2", "](", "]:", "[", "]", "(", ")", "<a href=x>", "<img src=y>", "&#64;", "&amp;",
  "https://", "www.", "WWW.", "\\n### Report IDs\\n", "​", "\\\\", '"', "{", "}", ",", ":", "issues",
  "report_id", "fingerprint", "newest", "versions", "platforms", "sources", "reports", "installs",
  "true", "null", "0", "-1", "1e3", "1.0", "999999999999", "0.9.", "+build", UUID_A, "a".repeat(64),
  "\\u000b", "\\u00a0", "\\u2028", "\\ud83d\\ude00", "é", "ſ", "K", "İ", " ", "\\t",
];

const HOSTILE_VALUES: unknown[] = [
  "https://evil.test/x", "www.evil.test", "WWW.EVIL.TEST", "@octocat", "#1", "GH-1", "gh-1", "[x](y)", "[x]: y",
  "<script>", "&#64;", "a\nb", "1.2.3-www.x", "1.2.3+GH-1", "linux #1", "x".repeat(1000), "", " ", "\u000b1.2.3\u000c",
  " 1.2.3", "linux x64", "linux x64", "linux  x64 ", "1.2.3​", "ſ", "K", "İ", "é", "\u0000",
  null, true, false, 0, -1, 1e12, 1e21, 1.5, 1.0, 86_400_000, 253_402_300_799_000, 253_402_300_800_000, [], {},
  { n: 1 }, UUID_A, UUID_A.toUpperCase(), "others", "a".repeat(64), "A".repeat(64), "😀",
];

const KEYS = ["issue", "fingerprint", "reports", "installs", "first_seen", "last_seen", "version", "platform", "report_id", "received_at", "n", "x"];

/**
 * Mutates a parsed payload in place of the Python test's `mutate_value`:
 * drops and adds keys, appends and repeats list items, swaps leaves.
 *
 * @param rand - PRNG.
 * @param value - Parsed JSON.
 * @param depth - Recursion depth.
 * @returns Mutated value.
 */
function mutateValue(rand: () => number, value: unknown, depth: number = 0): unknown {
  const pick = <T,>(list: readonly T[]): T => list[Math.floor(rand() * list.length)] as T;
  if (depth < 6 && Array.isArray(value)) {
    const out = value.map((v) => mutateValue(rand, v, depth + 1));
    if (rand() < 0.3) out.push(pick(HOSTILE_VALUES));
    if (rand() < 0.3 && out.length) out.push(pick(out));
    return out;
  }
  if (depth < 6 && value !== null && typeof value === "object") {
    const out: Record<string, unknown> = Object.fromEntries(Object.entries(value).map(([k, v]) => [k, mutateValue(rand, v, depth + 1)]));
    const keys = Object.keys(out);
    if (rand() < 0.2 && keys.length) delete out[pick(keys)];
    if (rand() < 0.15) out[pick(KEYS)] = pick(HOSTILE_VALUES);
    return out;
  }
  return rand() < 0.15 ? pick(HOSTILE_VALUES) : value;
}

/**
 * Edits every string in a parsed payload with `mutate`, leaving the shape.
 *
 * @param rand - PRNG.
 * @param value - Parsed JSON.
 * @returns The value with its strings edited.
 */
function mutateStrings(rand: () => number, value: unknown): unknown {
  if (typeof value === "string") return rand() < 0.7 ? mutate(rand, value, HOSTILE_VALUES.filter((v): v is string => typeof v === "string")) : value;
  if (Array.isArray(value)) return value.map((v) => mutateStrings(rand, v));
  if (value !== null && typeof value === "object") return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, mutateStrings(rand, v)]));
  return value;
}

describe("summary comment renderer against the Python oracle", () => {
  it("replays the seed corpus", () => {
    const all = seeds();
    assert.ok(all.length > 0);
    const cases: Case[] = [];
    for (const [name, text] of all) {
      const payload = parse(text);
      // Inputs Python cannot decode either (not JSON, nested past its
      // recursion limit) are the ingest script's concern, not the renderer's.
      if (payload === undefined || name === "deep_nesting.json") continue;
      cases.push({ payload, dashboard: DASHBOARD, label: `${name} with dashboard` });
      cases.push({ payload, dashboard: null, label: `${name} without dashboard` });
    }
    agree(cases);
    const seedText = (name: string): string => all.find(([n]) => n === name)?.[1] ?? "";
    const normal = renderSummaries(parse(seedText("normal.json")), DASHBOARD);
    assert.ok(normal[0] !== undefined);
    assert.ok(normal[0].body.startsWith(`${MARKER}\n**Diagnostics summary**`));
    assert.ok(normal.every((r) => r.body.endsWith("never posted here._\n")));
    assert.equal(renderSummaries(parse(seedText("many.json")), null).length, MAX_SUMMARIES);
    assert.deepEqual(renderSummaries("x"), []);
    assert.deepEqual(renderSummaries(null), []);
  });

  it("agrees with the oracle under seeded text mutation", () => {
    const texts = seeds().filter(([n]) => n !== "deep_nesting.json" && n !== "many.json").map(([, t]) => t);
    const rand = rng(20261025);
    const cases: Case[] = [];
    let raw = 0;
    for (let n = 0; n < 3000; n++) {
      const seed = texts[Math.floor(rand() * texts.length)] ?? "";
      // Half the time the JSON text itself is edited, and the input counts
      // when it still parses; the other half edits every string inside it,
      // which is where report-derived text sits.
      let payload: unknown;
      if (n % 2) {
        payload = parse(mutate(rand, seed, FRAGMENTS));
        if (payload === undefined) continue;
        raw++;
      } else {
        const parsed = parse(seed);
        if (parsed === undefined) continue;
        payload = mutateStrings(rand, parsed);
      }
      cases.push({ payload, dashboard: n % 4 < 2 ? DASHBOARD : null, label: `text mutation ${n}: ${JSON.stringify(payload).slice(0, 300)}` });
    }
    assert.ok(raw > 50, `enough edited texts still parse (${raw})`);
    agree(cases);
  });

  it("agrees with the oracle under seeded value mutation", () => {
    const values = seeds().filter(([n]) => !["deep_nesting.json", "many.json", "not_json.json"].includes(n)).map(([, t]) => parse(t));
    const rand = rng(7);
    const cases: Case[] = [];
    for (let n = 0; n < 1500; n++) {
      const payload = mutateValue(rand, values[Math.floor(rand() * values.length)]);
      cases.push({ payload, dashboard: n % 2 ? DASHBOARD : null, label: `value mutation ${n}: ${JSON.stringify(payload).slice(0, 300)}` });
    }
    agree(cases);
  });

  it("breaks links, mentions and references like the Python sanitizer", () => {
    assert.equal(
      defang("see https://a.test/x and www.b.test #12 @me GH-4 <b> &amp; [x](y)"),
      "see [link removed] and [link removed] #​12 @​me GH-​4 &lt;b> &amp;amp; [x]​(y)",
    );
  });

  it("accepts the same origins as the Python check", () => {
    const seedsOrigins: unknown[] = [
      DASHBOARD, `${DASHBOARD}/`, `${DASHBOARD}//`, "https://x", "https://x:8443", "https://x:99999", "https://x:123456",
      "http://x", "https://", "https://-x", "https://x-", "https://X.Test", "https://x/path", "https://x?q", "https://x#f",
      " https://x ", "\u000bhttps://x\u000c", " https://x", "https://ſ", "https://K", `https://${"a".repeat(253)}`,
      `https://${"a".repeat(254)}`, "", null, 7, true, [], {},
    ];
    const rand = rng(20261027);
    const origins = [...seedsOrigins];
    for (let n = 0; n < 1500; n++) {
      origins.push(mutate(rand, DASHBOARD, ["/", ":", ".", "-", "443", " ", "\t", "http", "HTTPS", "é", "ſ", "K", "\u0000", " ", "?", "#", "@"]));
    }
    const answers = oracle(origins.map((origin) => ({ origin })));
    origins.forEach((origin, i) => assert.equal(dashboardOrigin(origin), answers[i]?.origin, JSON.stringify(origin)));
  });
});
