// Cross-language replay and mutation fuzzing of the Triage now reader.
//
// A member's "Triage now" button asks the model about one report and reads
// its reply with dashboard/triage-now.ts into the issue or attachment the
// member may file. The reply is model output over report text anyone can
// submit, so the reader is an untrusted-input boundary. The ingest run's plan
// validator (scripts/validate_plan.py) is the independent oracle: every seed
// reply and every mutation of one is read by both, the validator with that
// report as its whole batch (through scripts/tests/diagnostics_plan_oracle.py
// in one process), and the proposals must be identical, filed body included.
// The seed corpus is the validator's own (scripts/tests/diagnostics_plan_corpus/),
// whose test holds the validator to its publishing rules.
//
// The reader is stricter in three places, where it may refuse a reply the
// validator takes: replies over MAX_REPLY_CHARS, JSON nested deeper than
// MAX_DEPTH, and NaN or Infinity, which Python's JSON reader accepts.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath, URL } from "node:url";
import { describe, it } from "vitest";
import {
  MAX_DEPTH,
  MAX_REPLY_CHARS,
  issueBody,
  proposalFor,
  type Proposal,
} from "./dashboard/triage-now.ts";
import { mutate, rng } from "./test-support/fuzz.ts";

const ROOT = new URL("../../", import.meta.url);
const CORPUS = new URL("scripts/tests/diagnostics_plan_corpus/", ROOT);
const ORACLE = fileURLToPath(new URL("scripts/tests/diagnostics_plan_oracle.py", ROOT));
const REPORT_A = "11111111-1111-4111-8111-111111111111";
const REPORT_B = "22222222-2222-4222-8222-222222222222";
/** The issues the validator's test offers the model. */
const OFFERED = [412, 408];

/** One oracle answer: the proposal (null for a refused reply), or a decode error. */
type OracleAnswer = { proposal?: unknown; error?: string };

/** One reply to read, with the report it is read for. */
type Case = { text: string; reportId: string; label: string };

/** The seed corpus as `[name, text]`, sorted by name. */
function seeds(): [string, string][] {
  return readdirSync(CORPUS)
    .sort()
    .map((name): [string, string] => [name, readFileSync(new URL(name, CORPUS), "utf8")]);
}

/**
 * Runs the Python oracle once over every case and returns its answers, one
 * per case, in order.
 *
 * @param cases - Replies to read.
 * @returns Answers.
 */
function oracle(cases: Case[]): OracleAnswer[] {
  const input =
    cases
      .map((c) => JSON.stringify({ text: c.text, report_id: c.reportId, offered: OFFERED }))
      .join("\n") + "\n";
  const run = spawnSync("python3", ["-I", ORACLE], { input, encoding: "utf8", maxBuffer: 1 << 28 });
  assert.equal(run.status, 0, `oracle failed: ${run.stderr}`);
  const lines = run.stdout.split("\n").filter((l) => l !== "");
  assert.equal(lines.length, cases.length, "one answer per case");
  return lines.map((l): OracleAnswer => JSON.parse(l));
}

/**
 * The reader's proposal in the oracle's form: an issue carries its whole
 * filed body.
 *
 * @param proposal - The reader's proposal, or null.
 * @param reportId - The report it was read for.
 * @returns The comparable value.
 */
function comparable(proposal: Proposal | null, reportId: string): unknown {
  if (proposal === null || proposal.kind !== "issue") return proposal;
  return { ...proposal, body: issueBody(proposal.body, reportId) };
}

/**
 * Whether the reader may refuse a reply the validator takes: one of the
 * three places it is stricter.
 *
 * @param text - The reply.
 * @returns True when the reply nests too deeply (counting
 *   brackets inside strings too, which only widens the excuse) or holds a
 *   NaN or Infinity literal.
 */
function stricterRefusal(text: string): boolean {
  if (/NaN|Infinity/.test(text)) return true;
  let depth = 0;
  for (const ch of text) {
    if (ch === "{" || ch === "[") {
      if (++depth > MAX_DEPTH) return true;
    } else if ((ch === "}" || ch === "]") && depth > 0) {
      depth--;
    }
  }
  return false;
}

/**
 * Requires the reader and the validator to propose the same thing for each
 * case, or the reader to refuse for one of its stricter reasons (a reply
 * over the cap always).
 *
 * @param cases - Replies to read.
 * @returns How many cases the reader refused for a stricter reason.
 */
function agree(cases: Case[]): number {
  // A reply over the cap is refused before it is read; the oracle is not
  // asked about it, since Python's scan for a plan is quadratic in its length.
  const long = cases.filter((c) => c.text.length > MAX_REPLY_CHARS);
  for (const { text, reportId, label } of long) {
    assert.equal(proposalFor(text, reportId, OFFERED), null, `${label}: over the cap`);
  }
  cases = cases.filter((c) => c.text.length <= MAX_REPLY_CHARS);
  const answers = oracle(cases);
  let excused = long.length;
  cases.forEach(({ text, reportId, label }, i) => {
    const answer = answers[i];
    assert.ok(answer !== undefined && !("error" in answer), `${label}: oracle decode error`);
    const ours = comparable(proposalFor(text, reportId, OFFERED), reportId);
    if (ours === null && answer.proposal !== null && stricterRefusal(text)) {
      excused++;
      return;
    }
    assert.deepEqual(ours, answer.proposal, label);
  });
  return excused;
}

const FRAGMENTS = [
  "@",
  "@fritz-fritz",
  "#1",
  "#412",
  "GH-2",
  "o/r#9",
  "](",
  "]:",
  "[",
  "]",
  "<a href=x>",
  "<!--",
  "-->",
  "<!-- bookclerk-diagnostics: report ids",
  "&#64;",
  "https://",
  "HTTPS://",
  "www.",
  "\\n### Report IDs\\n",
  "\\n  ## report ids x\\n",
  "\\n\\n# REPORT İDS",
  "report ıdſ",
  "\\r\\n",
  "\\n",
  "\\u2028",
  "\\u0085",
  "\\u001c",
  "\\ufeff",
  "\\u00a0",
  "\\t",
  " ",
  "​",
  "\\\\",
  '"',
  "{",
  "}",
  "[",
  "]",
  ",",
  ":",
  '"issues"',
  '"attach"',
  '"issue"',
  '"title"',
  '"body"',
  '"bug"',
  '"report_ids"',
  "[diagnostics] ",
  "true",
  "false",
  "null",
  "412",
  "408",
  "7",
  "412.0",
  "4.12e2",
  "-0",
  "999999999999999999999",
  `"${REPORT_A}"`,
  `"${REPORT_B}"`,
  `"${REPORT_A.toUpperCase()}"`,
  "x".repeat(130),
  "é",
  "ſ",
  "İ",
  "\\ud83d\\ude00",
];

describe("Triage now reader against the plan validator", () => {
  const corpus = seeds();

  it("reads every seed reply as the validator does", () => {
    assert.ok(corpus.length >= 10, "seed corpus is present");
    const cases = corpus.flatMap(([name, text]) =>
      [REPORT_A, REPORT_B].map((reportId) => ({
        text,
        reportId,
        label: `${name} for ${reportId}`,
      })),
    );
    agree(cases);
  });

  it("reads mutated replies as the validator does", () => {
    const rand = rng(0x7213a6e);
    const cases: Case[] = [];
    for (let n = 0; n < 4000; n++) {
      const [name, text] = corpus[Math.floor(rand() * corpus.length)] ?? ["", ""];
      const reportId = rand() < 0.5 ? REPORT_A : REPORT_B;
      cases.push({ text: mutate(rand, text, FRAGMENTS), reportId, label: `${name} #${n}` });
    }
    const excused = agree(cases);
    assert.ok(excused < cases.length / 4, `few stricter refusals (${excused})`);
  });

  it("reads plans built from hostile values as the validator does", () => {
    const rand = rng(0x5eed);
    const pick = <T>(list: readonly T[]): T => list[Math.floor(rand() * list.length)] as T;
    const strings = FRAGMENTS.map((f) => JSON.parse(`"${f.replaceAll('"', '\\"')}"`) as string);
    const text = (): string =>
      Array.from({ length: 1 + Math.floor(rand() * 6) }, () => pick(strings)).join(
        pick(["", " ", "\n"]),
      );
    const ids = (): unknown[] =>
      Array.from({ length: Math.floor(rand() * 3) }, () =>
        pick<unknown>([REPORT_A, REPORT_B, REPORT_A.toUpperCase(), 7, null, text()]),
      );
    const cases: Case[] = [];
    for (let n = 0; n < 2000; n++) {
      const issues = Array.from({ length: Math.floor(rand() * 7) }, () =>
        pick<unknown>([
          { title: text(), body: text(), report_ids: ids(), bug: pick([true, false, "true", 1]) },
          { title: text(), report_ids: ids() },
          { title: 5, body: text(), report_ids: ids() },
          { title: text(), body: text(), report_ids: REPORT_A },
          null,
          [],
        ]),
      );
      const attach = Array.from({ length: Math.floor(rand() * 3) }, () => ({
        issue: pick<unknown>([412, 408, 7, -412, "412", true, null, 412.5]),
        report_ids: ids(),
      }));
      const plan: Record<string, unknown> = { issues };
      if (rand() < 0.6) plan.attach = rand() < 0.9 ? attach : { issue: 412 };
      const wrapped = pick([
        (p: string) => p,
        (p: string) => `Here is the plan:\n\`\`\`json\n${p}\n\`\`\``,
        (p: string) => `{"note": "first"} ${p} {"issues": []}`,
      ]);
      // Python reads `412.0` and `4.12e2` as floats, which never match an issue.
      const numbers = pick([
        (p: string) => p,
        (p: string) => p.replace(/"issue":(\d+)/g, '"issue":$1.0'),
        (p: string) => p.replace(/"issue":412\b/g, '"issue":4.12e2'),
      ]);
      cases.push({
        text: wrapped(numbers(JSON.stringify(plan))),
        reportId: rand() < 0.5 ? REPORT_A : REPORT_B,
        label: `built #${n}`,
      });
    }
    assert.equal(agree(cases), 0, "built plans are never refused for a stricter reason");
  });
});
