// Dashboard untrusted-input boundaries: seed corpus replay and seeded
// mutation fuzzing (fuzzing ADR decision 6).
//
// - parseIssueReportIds reads issue bodies people with triage rights can
//   edit; its result decides which reports an issue claims.
// - summarizeReport reads stored reports anyone can submit; its result is
//   written to D1 and shown on every page.
// - eventLog renders the events of a stored report on its page; a throw
//   there would make the report unviewable.
// - parseCallback reads the sign-in callback's query and cookies, which
//   anyone can send; its result decides whether a code is traded with
//   GitHub.
//
// Each result is checked against an independent oracle written without the
// module's own helpers.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { MAX_CODE_CHARS, OAUTH_COOKIE, parseCallback } from "./dashboard/auth.js";
import { eventLog } from "./dashboard/pages.js";
import { FALLBACK_MARKER, MAX_IDS_PER_ISSUE, TRIAGE_MARKER, parseIssueReportIds } from "./dashboard/report-ids.js";
import { MAX_FIELD_CHARS, MAX_LABEL_CHARS, summarizeReport } from "./dashboard/signature.js";
import { mutate, rng } from "./test-support/fuzz.js";

/** Seed inputs of a corpus directory, keyed by file name. */
function seeds(dir) {
  const url = new URL(`./corpus/${dir}/`, import.meta.url);
  return readdirSync(url)
    .sort()
    .map((name) => [name, readFileSync(new URL(name, url), "utf8")]);
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * Oracle for parseIssueReportIds: recomputes the expected IDs with a
 * different walk (last heading by lastIndexOf over normalized lines).
 *
 * @param {string} body Issue body.
 * @param {string} label Input name for failure messages.
 */
function checkIds(body, label) {
  const got = parseIssueReportIds(body);
  const text = body.slice(0, 65536);
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const firstLine = lines[0].trim();
  assert.equal(got.kind, firstLine === TRIAGE_MARKER ? "triage" : firstLine === FALLBACK_MARKER ? "fallback" : "other", label);
  const headings = lines.map((l, i) => (/^###\s+Report IDs\s*$/.test(l) ? i : -1)).filter((i) => i >= 0);
  const expected = [];
  if (headings.length) {
    for (const line of lines.slice(headings[headings.length - 1] + 1)) {
      const m = line.match(/^[-*] `([^`]*)`\s*$/);
      if (!m || !/^[A-Za-z0-9._:/+-]{1,160}$/.test(m[1])) break;
      const id = m[1].toLowerCase();
      const fromKey = id.match(/^diagnostics\/[a-z0-9._+-]{1,64}\/(.{36})\.json$/);
      const uuid = UUID.test(id) ? id : fromKey && UUID.test(fromKey[1]) ? fromKey[1] : null;
      if (uuid && !expected.includes(uuid)) expected.push(uuid);
      if (expected.length >= MAX_IDS_PER_ISSUE) break;
    }
  }
  assert.deepEqual(got.ids, expected, label);
  for (const id of got.ids) {
    assert.match(id, UUID, label);
    assert.ok(text.toLowerCase().includes(id), `${label}: ${id} not in the body`);
  }
}

/**
 * Oracle for summarizeReport: recounts levels, checks bounds and that the
 * signature exists exactly when an error or warning does.
 *
 * @param {unknown} input Stored report (any JSON value).
 * @param {string} label Input name for failure messages.
 */
async function checkSummary(input, label) {
  const got = await summarizeReport(input);
  const payload = input && typeof input === "object" && !Array.isArray(input) && input.payload && typeof input.payload === "object" && !Array.isArray(input.payload) ? input.payload : {};
  const events = Array.isArray(payload.events) ? payload.events : [];
  const level = (e) => (e && typeof e === "object" && typeof e.level === "string" ? e.level.trim().toUpperCase() : "");
  const errors = events.filter((e) => level(e) === "ERROR").length;
  const warnings = events.filter((e) => ["WARN", "WARNING"].includes(level(e))).length;
  assert.equal(got.events, events.length, label);
  assert.equal(got.errors, errors, label);
  assert.equal(got.warnings, warnings, label);
  assert.equal(got.signature === null, errors + warnings === 0, label);
  if (got.signature !== null) {
    assert.match(got.signature, /^[0-9a-f]{16}$/, label);
    assert.ok(got.signatureLabel.length <= MAX_LABEL_CHARS, label);
    assert.ok(!/\d/.test(got.signatureLabel.split(" · ").slice(1).join(" · ")), `${label}: digits left in the message`);
  }
  for (const k of ["version", "os", "arch", "distro", "trigger"]) {
    assert.equal(typeof got[k], "string", label);
    assert.ok(got[k].length <= MAX_FIELD_CHARS, label);
    assert.ok(!/[\u0000-\u001f\u007f]/.test(got[k]), `${label}: control character in ${k}`);
  }
  assert.match(got.installHash, /^([0-9a-f]{1,64})?$/, label);
  const again = await summarizeReport(input);
  assert.deepEqual(again, got, `${label}: not deterministic`);
  checkEventLog(payload.events, label);
}

/**
 * Oracle for eventLog: never throws, one row per event up to 500, level
 * counts add up to the rows, and no event text survives as markup.
 *
 * @param {unknown} events `payload.events` (any JSON value).
 * @param {string} label Input name for failure messages.
 */
function checkEventLog(events, label) {
  const log = eventLog(events);
  const out = String(log.rows);
  const expected = Array.isArray(events) ? Math.min(events.length, 500) : 0;
  assert.equal((out.match(/<div class="row /g) ?? []).length, expected, label);
  assert.equal(Object.values(log.counts).reduce((a, b) => a + b, 0), expected, label);
  assert.deepEqual(Object.keys(log.counts), ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"], label);
  assert.ok(!/<script/i.test(out), `${label}: unescaped markup`);
}

const ID_FRAGMENTS = [
  "\n", "\r\n", "### Report IDs", "###  Report IDs ", "- `", "* `", "`", "\n- `7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4`",
  "diagnostics/", ".json", "../", "/", "-", "A", "f", " ", TRIAGE_MARKER, FALLBACK_MARKER, "é", "\0",
];

const JSON_FRAGMENTS = [
  "[", "]", "{", "}", ",", ":", '"', "\\", "null", "true", "1e308", '"level":"ERROR"', '"level":"warn"',
  '"message":"', '"target":"', "0xff", "/tmp/a", "12345678", "C:\\\\x", "😀", "\\u0000",
  '"fields":[["k",{"toString":null}]]', '"ts_unix_ms":1e20', '"level":"__proto__"', "<script>",
];

describe("parseIssueReportIds", () => {
  it("replays the seed corpus", () => {
    const all = seeds("report_ids");
    assert.ok(all.length > 0);
    for (const [name, body] of all) checkIds(body, name);
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("report_ids").map(([, b]) => b);
    const rand = rng(20261024);
    for (let n = 0; n < 4000; n++) {
      const body = mutate(rand, all[Math.floor(rand() * all.length)], ID_FRAGMENTS);
      checkIds(body, `iteration ${n}: ${JSON.stringify(body).slice(0, 200)}`);
    }
  });

  it("caps the IDs read from one issue", () => {
    const ids = Array.from({ length: 600 }, (_, i) => `00000000-0000-4000-8000-${String(i).padStart(12, "0")}`);
    const body = `### Report IDs\n${ids.map((i) => `- \`${i}\``).join("\n")}`;
    assert.equal(parseIssueReportIds(body).ids.length, MAX_IDS_PER_ISSUE);
    checkIds(body, "cap");
  });
});

describe("summarizeReport", () => {
  it("replays the seed corpus", async () => {
    const all = seeds("signature");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) await checkSummary(JSON.parse(raw), name);
  });

  it("holds the oracle under seeded mutation", async () => {
    const all = seeds("signature").map(([, raw]) => raw);
    const rand = rng(20261025);
    for (let n = 0; n < 3000; n++) {
      const raw = mutate(rand, all[Math.floor(rand() * all.length)], JSON_FRAGMENTS);
      let value = raw;
      try {
        value = JSON.parse(raw);
      } catch {
        // Unparseable text still must not break the summary.
      }
      await checkSummary(value, `iteration ${n}: ${JSON.stringify(raw).slice(0, 200)}`);
    }
  });
});

/**
 * Oracle for parseCallback: re-derives acceptance with a separate cookie
 * walk (a regex over the whole header) and checks what an accepted callback
 * returns.
 *
 * @param {{search: string, cookie: string}} input Callback inputs.
 * @param {string} label Input name for failure messages.
 */
function checkCallback(input, label) {
  const got = parseCallback(input.search, input.cookie);
  const q = new URLSearchParams(input.search);
  const state = q.get("state") ?? "";
  const code = q.get("code") ?? "";
  assert.equal(OAUTH_COOKIE, "__Host-bc_oauth");
  const m = /(?:^|;)\s*__Host-bc_oauth\s*=([^;]*)/.exec(input.cookie);
  const value = m ? m[1].trim() : "";
  const token = /^[A-Za-z0-9_-]{43}$/;
  const cm = /^([A-Za-z0-9_-]{43})\.([A-Za-z0-9_-]{43})$/.exec(value);
  const accept =
    Boolean(cm) && token.test(state) && cm[1] === state && /^[A-Za-z0-9_-]+$/.test(code) && code.length <= MAX_CODE_CHARS;
  assert.equal(got !== null, accept, label);
  if (got) {
    assert.equal(got.code, code, label);
    assert.equal(got.verifier, cm[2], label);
  }
}

const CALLBACK_FRAGMENTS = [
  "&", "=", "?", "%", "%41", "+", ";", " ", ".", "state=", "code=", `${OAUTH_COOKIE}=`, "__Host-bc_session=",
  "A", "_", "-", "/", "é", "\0", "\n",
];

describe("parseCallback", () => {
  it("replays the seed corpus", () => {
    const all = seeds("oauth_callback");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkCallback(JSON.parse(raw), name);
    const valid = JSON.parse(all.find(([n]) => n === "valid.json")[1]);
    assert.ok(parseCallback(valid.search, valid.cookie), "the valid seed is accepted");
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("oauth_callback").map(([, raw]) => JSON.parse(raw));
    const rand = rng(20261026);
    for (let n = 0; n < 4000; n++) {
      const base = all[Math.floor(rand() * all.length)];
      const input = rand() < 0.5
        ? { search: mutate(rand, base.search, CALLBACK_FRAGMENTS), cookie: base.cookie }
        : { search: base.search, cookie: mutate(rand, base.cookie, CALLBACK_FRAGMENTS) };
      checkCallback(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});
