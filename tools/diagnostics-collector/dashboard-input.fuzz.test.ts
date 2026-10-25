// Dashboard untrusted-input boundaries: seed corpus replay and seeded
// mutation fuzzing (fuzzing ADR decision 6).
//
// - parseIssueReportIds reads issue bodies people with triage rights can
//   edit; its result decides which reports an issue claims.
// - summarizeReport reads stored reports anyone can submit; its result is
//   written to D1 and shown on every page.
// - eventLog renders the events of a stored report on its page; a throw
//   there would make the report unviewable.
// - sessionToken and memberFromRow decide, from a request's cookies and the
//   session row they name, whether a request is signed in and as whom.
// - parseCallback reads the sign-in callback's query and cookies, which
//   anyone can send; its result decides whether a code is traded with
//   GitHub and which page the signed-in member lands on.
// - safeNext reads the return page from a query parameter or the sign-in
//   cookie; its result becomes a redirect target, so it must never leave
//   the dashboard's own pages.
// - membershipDecision reads GitHub's organization membership answer; its
//   result decides whether sign-in grants member or owner access.
// - decodeIssuePage reads GitHub's issue listing; its result moves the
//   sync cursor and is what storeIssue writes.
// - decodeInstallationId, decodeInstallationToken, decodeAccessToken,
//   oauthErrorCode and decodeUser read GitHub's answers during sign-in and token minting.
// - parseIssueNotify reads the ingest Action's issue notice and decodeIssue
//   the issue it names, read back from GitHub.
// - decodeListPage reads B2's file listing; its result names the objects
//   report viewing downloads and where reindexing continues.
// - decodePullRequests reads the GraphQL answer about each issue's labels
//   and pull requests; its result decides whether the sync drops an issue
//   or replaces its stored pull request links, and which merge commit the
//   fixed-in lookup compares against release tags.
// - fixedInLabel reads an issue's labels (people with triage rights can add
//   them); its result overrides the version an issue was fixed in.
// - decodeReleases and decodeCompare read GitHub's release list and compare
//   answers; their results decide which release an issue's fix is recorded
//   under, and so which reports count as regressions.
// - parseLinkAction reads the ledger correction forms members post; its
//   result decides which report is attached to, detached from or moved
//   between which issues, and which issue is merged into which.
// - decodeComments reads one page of an issue's comments; its result is the
//   comment the owner's "Update GitHub now" button edits in place, or that a
//   new one is posted. isLockedMessage reads GitHub's refusal of that write;
//   its result decides whether the issue is recorded as written (locked, no
//   retry) or left for the next run.
//
// Each result is checked against an independent oracle written without the
// module's own helpers.

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { DEFAULT_NEXT, MAX_CODE_CHARS, MAX_NEXT_CHARS, OAUTH_COOKIE, parseCallback, safeNext } from "./dashboard/auth.ts";
import {
  DIAGNOSTICS_LABEL,
  decodeAccessToken,
  decodeCompare,
  decodeComments,
  isLockedMessage,
  decodeIssue,
  decodeReleases,
  fixedInLabel,
  parseIssueNotify,
  parseSummaryDone,
  oauthErrorCode,
  decodeInstallationId,
  decodeInstallationToken,
  decodeIssuePage,
  decodePullRequests,
  decodeUser,
  membershipDecision,
} from "./dashboard/github.ts";
import { LINK_ACTIONS, MAX_FORM_CHARS, MAX_ISSUE_NUMBER, parseLinkAction } from "./dashboard/links.ts";
import { eventLog } from "./dashboard/pages.ts";
import { decodeListPage } from "./dashboard/storage.ts";
import { memberFromRow, SESSION_COOKIE, sessionToken } from "./dashboard/session.ts";
import { FALLBACK_MARKER, MAX_IDS_PER_ISSUE, TRIAGE_MARKER, parseIssueReportIds } from "./dashboard/report-ids.ts";
import { MAX_FIELD_CHARS, MAX_LABEL_CHARS, summarizeReport } from "./dashboard/signature.ts";
import { choose, mutate, rng, seedsIn } from "./test-support/fuzz.ts";

/** Seed inputs of a corpus directory, keyed by file name. */
function seeds(dir: string): [string, string][] {
  return seedsIn(new URL(`./corpus/${dir}/`, import.meta.url));
}

/**
 * A seed file's JSON, typed as the corpus documents it. Seeds are this
 * repository's own test data, so their shape is the test's word.
 *
 * @param raw - Seed text.
 * @returns The parsed seed.
 */
function parseSeed<T>(raw: string): T {
  return JSON.parse(raw);
}

/**
 * The seed with a given file name.
 *
 * @param all - Seeds.
 * @param name - File name.
 * @returns Its text.
 * @throws {Error} When there is no such seed.
 */
function seedNamed(all: [string, string][], name: string): string {
  const found = all.find(([n]) => n === name);
  if (!found) throw new Error(`no seed ${name}`);
  return found[1];
}

/**
 * A JSON object's fields, or null for anything else. Oracle-side, so the
 * checks do not lean on the modules' own json.ts.
 *
 * @param v - Any value.
 * @returns The object, or null.
 */
function fields(v: unknown): Record<string, unknown> | null {
  return v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
}

/**
 * Like `fields`, but arrays count too (as `typeof` does).
 *
 * @param v - Any value.
 * @returns The value as a record, or null for a primitive.
 */
function record(v: unknown): Record<string, unknown> | null {
  return v !== null && typeof v === "object" ? (v as Record<string, unknown>) : null;
}

/**
 * Whether a value is a non-null object (arrays included, as `typeof`
 * says). Oracle-side, so the checks do not lean on the modules' json.ts.
 *
 * @param v - Any value.
 * @returns True for an object.
 */
function isObj(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object";
}

/**
 * A value when it is a string, else "".
 *
 * @param v - Any value.
 * @returns The string.
 */
function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * Oracle for parseIssueReportIds: recomputes the expected IDs with a
 * different walk (the last hidden block by lastIndexOf over trimmed lines,
 * else the last heading over normalized lines).
 *
 * @param body - Issue body.
 * @param label - Input name for failure messages.
 */
function checkIds(body: string, label: string) {
  const got = parseIssueReportIds(body);
  const text = body.slice(0, 65536);
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const firstLine = (lines[0] ?? "").trim();
  // A regression marker names an issue number: 1 to 9 digits, no leading zero.
  const regression = /^<!-- bookclerk-diagnostics: regression of #(\d+) -->$/.exec(firstLine);
  const regressionOf = regression?.[1] !== undefined && /^[1-9]\d{0,8}$/.test(regression[1]) ? Number(regression[1]) : null;
  assert.equal(
    got.kind,
    firstLine === TRIAGE_MARKER ? "triage" : firstLine === FALLBACK_MARKER ? "fallback" : regressionOf !== null ? "regression" : "other",
    label,
  );
  assert.equal(got.regressionOf, regressionOf, label);
  if (regressionOf !== null) assert.ok(got.regressionOf !== null && Number.isSafeInteger(got.regressionOf) && got.regressionOf > 0, label);
  const uuidOf = (raw: string): string | null => {
    const id = raw.toLowerCase();
    const fromKey = id.match(/^diagnostics\/[a-z0-9._+-]{1,64}\/(.{36})\.json$/);
    return UUID.test(id) ? id : fromKey?.[1] !== undefined && UUID.test(fromKey[1]) ? fromKey[1] : null;
  };
  const expected: string[] = [];
  const add = (raw: string): void => {
    const uuid = uuidOf(raw);
    if (uuid && !expected.includes(uuid) && expected.length < MAX_IDS_PER_ISSUE) expected.push(uuid);
  };
  // The hidden block wins over the legacy heading wherever either sits.
  const trimmed = lines.map((l) => l.replace(/[ \t]+$/, ""));
  const block = trimmed.lastIndexOf("<!-- bookclerk-diagnostics: report ids");
  const headings = lines.map((l, i) => (/^###\s+Report IDs\s*$/.test(l) ? i : -1)).filter((i) => i >= 0);
  const last = headings[headings.length - 1];
  if (block >= 0) {
    const run: string[] = [];
    let end = block + 1;
    for (let line = trimmed[end]; line !== undefined && /^[A-Za-z0-9._:/+-]{1,160}$/.test(line); line = trimmed[++end]) run.push(line);
    // An unclosed block lists nothing: GitHub would hide the rest of the body.
    if (trimmed[end] === "-->") run.forEach(add);
  } else if (last !== undefined) {
    for (const line of lines.slice(last + 1)) {
      // An optional " · [dashboard](url)" suffix is allowed and ignored.
      const m = line.match(/^[-*] `([^`]*)`( · \[dashboard\]\(([^)]*)\))?\s*$/);
      if (!m || m[1] === undefined || !/^[A-Za-z0-9._:/+-]{1,160}$/.test(m[1])) break;
      const url = m[3] ?? "";
      if (m[2] !== undefined && (url.length < 1 || url.length > 300 || /[\s(]/.test(url))) break;
      add(m[1]);
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
 * @param input - Stored report (any JSON value).
 * @param label - Input name for failure messages.
 */
async function checkSummary(input: unknown, label: string) {
  const got = await summarizeReport(input);
  const payload = fields(fields(input)?.["payload"]) ?? {};
  const events: unknown[] = Array.isArray(payload["events"]) ? payload["events"] : [];
  const level = (e: unknown): string => {
    const lv = record(e)?.["level"];
    return typeof lv === "string" ? lv.trim().toUpperCase() : "";
  };
  const errors = events.filter((e) => level(e) === "ERROR").length;
  const warnings = events.filter((e) => ["WARN", "WARNING"].includes(level(e))).length;
  assert.equal(got.events, events.length, label);
  assert.equal(got.errors, errors, label);
  assert.equal(got.warnings, warnings, label);
  assert.equal(got.signature === null, errors + warnings === 0, label);
  if (got.signature !== null) {
    assert.match(got.signature, /^[0-9a-f]{16}$/, label);
    const sigLabel = got.signatureLabel ?? "";
    assert.ok(sigLabel.length <= MAX_LABEL_CHARS, label);
    assert.ok(!/\d/.test(sigLabel.split(" · ").slice(1).join(" · ")), `${label}: digits left in the message`);
  }
  for (const k of ["version", "os", "arch", "distro", "trigger"] as const) {
    assert.equal(typeof got[k], "string", label);
    assert.ok(got[k].length <= MAX_FIELD_CHARS, label);
    assert.ok(!/[\u0000-\u001f\u007f]/.test(got[k]), `${label}: control character in ${k}`);
  }
  assert.match(got.installHash, /^([0-9a-f]{1,64})?$/, label);
  const again = await summarizeReport(input);
  assert.deepEqual(again, got, `${label}: not deterministic`);
  checkEventLog(payload["events"], label);
}

/**
 * Oracle for eventLog: never throws, one row per event up to 500, level
 * counts add up to the rows, and no event text survives as markup.
 *
 * @param events - `payload.events` (any JSON value).
 * @param label - Input name for failure messages.
 */
function checkEventLog(events: unknown, label: string) {
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
  "<!-- bookclerk-diagnostics: regression of #412 -->", "regression of #", "#0", "#9999999999", "1",
  " · [dashboard](https://d.test/reports/x)", " · ", "[dashboard]", "(", ")", "\t",
  "<!-- bookclerk-diagnostics: report ids", "\n<!-- bookclerk-diagnostics: report ids\n", "-->", "\n-->", "--", ">",
  "\n7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4", "\ndiagnostics/0.1.0/7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4.json",
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
      const body = mutate(rand, choose(rand, all), ID_FRAGMENTS);
      checkIds(body, `iteration ${n}: ${JSON.stringify(body).slice(0, 200)}`);
    }
  });

  it("caps the IDs read from one issue", () => {
    const ids = Array.from({ length: 600 }, (_, i) => `00000000-0000-4000-8000-${String(i).padStart(12, "0")}`);
    const body = `### Report IDs\n${ids.map((i) => `- \`${i}\``).join("\n")}`;
    assert.equal(parseIssueReportIds(body).ids.length, MAX_IDS_PER_ISSUE);
    checkIds(body, "cap");
    const hidden = `<!-- bookclerk-diagnostics: report ids\n${ids.join("\n")}\n-->`;
    assert.equal(parseIssueReportIds(hidden).ids.length, MAX_IDS_PER_ISSUE);
    checkIds(hidden, "hidden cap");
  });

  it("reads the hidden block before the visible list", () => {
    const a = "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4";
    const b = "5cee862c-0309-4786-8778-6b2b124087e6";
    const body = `${TRIAGE_MARKER}\nText.\n\n### Report IDs\n- \`${a}\`\n\n<!-- bookclerk-diagnostics: report ids\n${b}\n-->\n`;
    assert.deepEqual(parseIssueReportIds(body), { kind: "triage", ids: [b], regressionOf: null });
    assert.deepEqual(parseIssueReportIds(body.replace("\n-->\n", "\n")).ids, [], "an unclosed block lists nothing");
    assert.deepEqual(parseIssueReportIds(`<!-- bookclerk-diagnostics: report ids\n${b} · x\n-->`).ids, [], "a stray line ends the block unclosed");
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
      const raw = mutate(rand, choose(rand, all), JSON_FRAGMENTS);
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

const ORACLE_ORIGIN = "https://dashboard.oracle.invalid";

/**
 * Whether a return page is one the dashboard may redirect to, decided with
 * the URL parser instead of safeNext's own patterns: it resolves on this
 * origin to a dashboard GET page without changing on the way (no dot
 * segments, no backslashes, no encoded path), and its query has no
 * characters that HTML or a header would treat specially.
 *
 * @param next - Candidate return page.
 * @returns True when it is safe to redirect to.
 */
function isDashboardPage(next: string): boolean {
  if (!next.startsWith("/") || next.startsWith("//") || next.length > MAX_NEXT_CHARS) return false;
  let url: URL;
  try {
    url = new URL(next, ORACLE_ORIGIN);
  } catch {
    return false;
  }
  const pages = new Set(["/dash", "/reports", "/issues"]);
  const page = pages.has(url.pathname)
    || (url.pathname.startsWith("/reports/") && UUID.test(url.pathname.slice(9)))
    || (url.pathname.startsWith("/issues/") && /^[1-9][0-9]{0,8}$/.test(url.pathname.slice(8)));
  return (
    url.origin === ORACLE_ORIGIN &&
    page &&
    url.pathname + url.search === next &&
    url.hash === "" &&
    ![..."<>\"'\\ ;,#"].some((c) => next.includes(c)) &&
    !/[\u0000-\u001f\u007f-\uffff]/.test(next)
  );
}

/**
 * Oracle for safeNext: anything it returns passes isDashboardPage and is the
 * input or the input's path with the query cut off; a page path on its own
 * is always kept.
 *
 * @param input - Candidate.
 * @param label - Input name for failure messages.
 */
function checkNext(input: unknown, label: string) {
  const got = safeNext(input);
  if (typeof input !== "string") {
    assert.equal(got, null, label);
    return;
  }
  if (got !== null) {
    assert.ok(isDashboardPage(got), `${label}: ${JSON.stringify(got)} is not a dashboard page`);
    assert.ok(got === input || got === input.slice(0, input.indexOf("?")), label);
  }
  const path = input.split("?")[0] ?? "";
  if (input.length <= MAX_NEXT_CHARS && isDashboardPage(path)) {
    assert.ok(got === path || got === input, `${label}: a dashboard page was refused`);
  }
}

const NEXT_FRAGMENTS = [
  "/", "//", "\\", "..", ".", "?", "&", "=", "%", "%2F", "%2f", "%5C", "%0d%0a", "#", ":", "@", "https:", "javascript:",
  "dash", "reports", "issues", "auth", "@", "/:", "live", "admin", "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4", " ", "\t", "\n", "\0", "é", "'", '"', "<",
];

describe("safeNext", () => {
  it("replays the seed corpus", () => {
    const all = seeds("next_path");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkNext(raw, name);
    const byName = (name: string): string => seedNamed(all, name);
    assert.equal(safeNext(byName("report.txt")), byName("report.txt"));
    assert.equal(safeNext(byName("reports_query.txt")), byName("reports_query.txt"));
    for (const off of ["protocol_relative.txt", "absolute.txt", "backslash.txt", "dot_segments.txt", "javascript.txt", "auth.txt", "live.txt"]) {
      assert.equal(safeNext(byName(off)), null, off);
    }
    assert.equal(safeNext(byName("quote_query.txt")), "/dash", "an unsafe query is dropped");
    for (const v of [null, undefined, 7, {}, ["/dash"]]) checkNext(v, String(v));
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("next_path").map(([, raw]) => raw);
    const rand = rng(20261024);
    for (let n = 0; n < 6000; n++) {
      const input = mutate(rand, choose(rand, all), NEXT_FRAGMENTS);
      checkNext(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});

/**
 * Oracle for parseCallback: re-derives acceptance with a separate cookie
 * walk (a regex over the whole header) and checks what an accepted callback
 * returns, including that the return page is a dashboard page and equals
 * the cookie's decoded page whenever that one is.
 *
 * @param input - Callback inputs.
 * @param label - Input name for failure messages.
 */
/** One parseCallback input: the callback's query and Cookie header. */
type CallbackInput = { search: string; cookie: string };

function checkCallback(input: CallbackInput, label: string) {
  const got = parseCallback(input.search, input.cookie);
  const q = new URLSearchParams(input.search);
  const state = q.get("state") ?? "";
  const code = q.get("code") ?? "";
  assert.equal(OAUTH_COOKIE, "__Host-bc_oauth");
  const m = /(?:^|;)\s*__Host-bc_oauth\s*=([^;]*)/.exec(input.cookie);
  const value = (m?.[1] ?? "").trim();
  const token = /^[A-Za-z0-9_-]{43}$/;
  const cm = /^([A-Za-z0-9_-]{43})\.([A-Za-z0-9_-]{43})(?:\.([^.]*))?$/.exec(value);
  const accept =
    cm !== null && token.test(state) && cm[1] === state && /^[A-Za-z0-9_-]+$/.test(code) && code.length <= MAX_CODE_CHARS;
  assert.equal(got !== null, accept, label);
  if (got && cm) {
    assert.equal(got.code, code, label);
    assert.equal(got.verifier, cm[2], label);
    assert.ok(got.next === DEFAULT_NEXT || isDashboardPage(got.next), `${label}: next ${JSON.stringify(got.next)}`);
    const tail = cm[3];
    // Only canonical base64url decodes to one answer; Buffer is lenient
    // where atob is not, so compare on those only.
    if (tail && /^[A-Za-z0-9_-]+$/.test(tail) && Buffer.from(tail, "base64url").toString("base64url") === tail) {
      const decoded = Buffer.from(tail, "base64url").toString("latin1");
      if (decoded.length <= MAX_NEXT_CHARS && isDashboardPage(decoded)) assert.equal(got.next, decoded, label);
    } else if (tail === undefined) {
      assert.equal(got.next, DEFAULT_NEXT, label);
    }
  }
}

const CALLBACK_FRAGMENTS = [
  "&", "=", "?", "%", "%41", "+", ";", " ", ".", "state=", "code=", `${OAUTH_COOKIE}=`, "__Host-bc_session=",
  "A", "_", "-", "/", "é", "\0", "\n", "Ly9ldmlsLmV4YW1wbGU", "L2Rhc2g", "L2lzc3Vlcw",
];

describe("parseCallback", () => {
  it("replays the seed corpus", () => {
    const all = seeds("oauth_callback");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkCallback(parseSeed<CallbackInput>(raw), name);
    const valid = parseSeed<CallbackInput>(seedNamed(all, "valid.json"));
    assert.ok(parseCallback(valid.search, valid.cookie), "the valid seed is accepted");
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("oauth_callback").map(([, raw]) => parseSeed<CallbackInput>(raw));
    const rand = rng(20261026);
    for (let n = 0; n < 4000; n++) {
      const base = choose(rand, all);
      const input = rand() < 0.5
        ? { search: mutate(rand, base.search, CALLBACK_FRAGMENTS), cookie: base.cookie }
        : { search: base.search, cookie: mutate(rand, base.cookie, CALLBACK_FRAGMENTS) };
      checkCallback(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});

/**
 * Oracle for the session decision: a separate cookie walk for the token,
 * and the member rules restated (live, positive id, GitHub-shaped login,
 * admin only when the row says so exactly).
 *
 * @param input - Decision inputs.
 * @param label - Input name for failure messages.
 */
/** One session decision input: Cookie header, the row it names, and the time. */
type SessionInput = { cookie: string; row: unknown; now: number };

function checkSession(input: SessionInput, label: string) {
  assert.equal(SESSION_COOKIE, "__Host-bc_session");
  const m = /(?:^|;)\s*__Host-bc_session\s*=([^;]*)/.exec(input.cookie);
  const value = (m?.[1] ?? "").trim();
  assert.equal(sessionToken(input.cookie), /^[A-Za-z0-9_-]{43}$/.test(value) ? value : null, label);

  const got = memberFromRow(fields(input.row), input.now);
  const r = record(input.row);
  const live =
    r !== null &&
    Number.isFinite(Number(r["expires_at"])) &&
    Number(r["expires_at"]) > input.now &&
    Number.isSafeInteger(Number(r["user_id"])) &&
    Number(r["user_id"]) > 0 &&
    typeof r["login"] === "string" &&
    /^[A-Za-z0-9-]{1,39}$/.test(r["login"]);
  assert.equal(got !== null, live, label);
  if (got && r) {
    assert.equal(got.role, r["role"] === "admin" ? "admin" : "member", label);
    assert.equal(got.login, r["login"], label);
    assert.ok(got.expiresAt > input.now, label);
  }
}

const SESSION_FRAGMENTS = [
  ";", "=", " ", "__Host-bc_session=", "__Host-bc_oauth=", "A", "-", "_", ".", "%", "\n", "\"", "0", "-1",
  '"admin"', '"member"', "null", "1e400", "{}", "[]", '"expires_at":', '"role":', "é",
];

describe("session decision", () => {
  it("replays the seed corpus", () => {
    const all = seeds("session");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkSession(parseSeed<SessionInput>(raw), name);
    const valid = parseSeed<SessionInput>(seedNamed(all, "valid.json"));
    assert.ok(memberFromRow(fields(valid.row), valid.now), "the valid seed is a member");
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("session").map(([, raw]) => parseSeed<SessionInput>(raw));
    const rand = rng(20261027);
    for (let n = 0; n < 4000; n++) {
      const base = choose(rand, all);
      const input = { ...base };
      const pick = rand();
      if (pick < 0.45) {
        input.cookie = mutate(rand, base.cookie, SESSION_FRAGMENTS);
      } else if (pick < 0.9) {
        const text = mutate(rand, JSON.stringify(base.row), SESSION_FRAGMENTS);
        try {
          input.row = JSON.parse(text);
        } catch {
          input.row = text;
        }
      } else {
        input.now = Math.floor((rand() - 0.25) * 4000);
      }
      checkSession(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});

/**
 * Oracle for membershipDecision: the access rules restated. A 404 passes
 * only the owner itself; another non-2xx throws; a 2xx passes only a JSON
 * object with `state: "active"` whose `user.id` is the signed-in user's
 * positive integer id, as admin exactly when `role` is "admin".
 *
 * @param input - Decision inputs.
 * @param label - Input name for failure messages.
 */
/**
 * The signed-in user as membershipDecision takes it. The decision is
 * fuzzed with any JSON value there, so the value is passed through as is.
 *
 * @param user - Any value.
 * @returns It, typed as the decision's parameter.
 */
function asUser(user: unknown): { id: number; login: string } {
  return user as { id: number; login: string };
}

/** One membership decision input: GitHub's answer, the user and the owner. */
type MembershipInput = { status: number; body: string; user: unknown; owner: string };

function checkMembership(input: MembershipInput, label: string) {
  const { status, body, user, owner } = input;
  const login = isObj(user) ? user.login : undefined;
  const id = isObj(user) ? user.id : undefined;
  let expected: "admin" | "member" | null;
  if (status === 404) {
    expected = typeof login === "string" && typeof owner === "string" && login.toLowerCase() === owner.toLowerCase() ? "admin" : null;
  } else if (typeof status !== "number" || status < 200 || status > 299) {
    assert.throws(() => membershipDecision(status, body, asUser(user), owner), label);
    return;
  } else {
    let m: unknown = null;
    try {
      m = JSON.parse(body);
    } catch {
      m = null;
    }
    const ok =
      isObj(m) &&
      m.state === "active" &&
      typeof id === "number" &&
      Number.isInteger(id) &&
      id > 0 &&
      id <= Number.MAX_SAFE_INTEGER &&
      isObj(m.user) &&
      m.user.id === id;
    expected = ok && isObj(m) ? (m.role === "admin" ? "admin" : "member") : null;
  }
  assert.equal(membershipDecision(status, body, asUser(user), owner), expected, label);
}

const MEMBERSHIP_FRAGMENTS = [
  '"active"', '"pending"', '"admin"', '"member"', '"state":', '"role":', '"user":', '"id":', "7", "8", "-7", "0", "1e400",
  "null", "{}", "[]", ",", ":", '"', "{", "}", "Bookclerk", "octo", "OCTO", "é", " ",
];

describe("membership decision", () => {
  it("replays the seed corpus", () => {
    const all = seeds("membership");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkMembership(parseSeed<MembershipInput>(raw), name);
    const admin = parseSeed<MembershipInput>(seedNamed(all, "active_admin.json"));
    assert.equal(membershipDecision(admin.status, admin.body, asUser(admin.user), admin.owner), "admin");
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("membership").map(([, raw]) => parseSeed<MembershipInput>(raw));
    const rand = rng(20261028);
    const statuses = [200, 201, 204, 299, 300, 301, 401, 403, 404, 422, 500, 0, -1, 404.5];
    for (let n = 0; n < 4000; n++) {
      const base = choose(rand, all);
      const input = { ...base };
      const pick = rand();
      if (pick < 0.5) {
        input.body = mutate(rand, base.body, MEMBERSHIP_FRAGMENTS);
      } else if (pick < 0.75) {
        const text = mutate(rand, JSON.stringify(base.user), MEMBERSHIP_FRAGMENTS);
        try {
          input.user = JSON.parse(text);
        } catch {
          input.user = text;
        }
      } else if (pick < 0.9) {
        input.status = choose(rand, statuses);
      } else {
        input.owner = mutate(rand, base.owner, MEMBERSHIP_FRAGMENTS);
      }
      checkMembership(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});

/**
 * Oracle for decodePullRequests: GraphQL errors or no repository object
 * throw; otherwise an issue alias that is not an object is left out, and
 * each other issue is labeled unless its complete label list (no next page)
 * lacks `diagnostics` (any case), with one entry per positive integer pull request number in
 * this repository with a known state, the timeline's last word winning,
 * string fields only, and a merge commit only for a merged pull request
 * whose oid is 40 lowercase hex characters.
 *
 * @param input - Decoder inputs.
 * @param label - Input name for failure messages.
 */
/** One decodePullRequests input. */
type PullRequestsInput = { data: unknown; numbers: number[]; repository: string };

function checkPullRequests(input: PullRequestsInput, label: string) {
  const { data, numbers, repository } = input;
  const errors = isObj(data) && Array.isArray(data.errors) && data.errors.length > 0;
  const repo = isObj(data) && isObj(data.data) ? data.data.repository : undefined;
  if (errors || !isObj(repo)) {
    assert.throws(() => decodePullRequests(data, numbers, repository), label);
    return;
  }
  const got = decodePullRequests(data, numbers, repository);
  const states = new Map([["OPEN", "open"], ["CLOSED", "closed"], ["MERGED", "merged"]]);
  const expected = new Map<number, { labeled: boolean; prs: unknown[] }>();
  numbers.forEach((n, i) => {
    const issue = repo["i" + i];
    if (!isObj(issue)) return;
    const prs = new Map<number, unknown>();
    const consider = (pr: unknown): void => {
      if (!isObj(pr) || typeof pr.number !== "number" || !Number.isInteger(pr.number) || pr.number < 1 || pr.number > Number.MAX_SAFE_INTEGER) return;
      const where = isObj(pr.repository) ? str(pr.repository.nameWithOwner) : "";
      if (where.toLowerCase() !== repository.toLowerCase()) return;
      const state = typeof pr.state === "string" ? states.get(pr.state) : undefined;
      if (state === undefined) return;
      const t = Date.parse(str(pr.mergedAt));
      const oid = isObj(pr.mergeCommit) ? str(pr.mergeCommit.oid) : "";
      const sha = pr.state === "MERGED" && oid.length === 40 && [...oid].every((c) => "0123456789abcdef".includes(c)) ? oid : null;
      prs.set(pr.number, { number: pr.number, title: str(pr.title).slice(0, 300), state, mergedAt: Number.isNaN(t) ? null : t, mergeSha: sha });
    };
    const nodes = (v: unknown): unknown[] => (isObj(v) && Array.isArray(v.nodes) ? v.nodes : []);
    for (const pr of nodes(issue.closedByPullRequestsReferences)) consider(pr);
    for (const ev of nodes(issue.timelineItems)) {
      if (!isObj(ev)) continue;
      consider(ev.source !== undefined && ev.source !== null ? ev.source : ev.subject);
    }
    const labelList = issue.labels;
    const labels: unknown[] | null = isObj(labelList) && Array.isArray(labelList.nodes) ? labelList.nodes : null;
    const complete = labels !== null && isObj(labelList) && isObj(labelList.pageInfo) && labelList.pageInfo.hasNextPage === false;
    const labeled = !complete || labels.some((l) => isObj(l) && str(l.name).toLowerCase() === DIAGNOSTICS_LABEL);
    expected.set(n, { labeled, prs: [...prs.values()] });
  });
  assert.deepEqual(got, expected, label);
}

const PR_FRAGMENTS = [
  '"OPEN"', '"CLOSED"', '"MERGED"', '"__proto__"', '"diagnostics"', '"DIAGNOSTICS"', '"Bookclerk/bookclerk"', '"other/repo"',
  '"number":', '"state":', '"nodes":', '"errors":', '"repository":', '"mergedAt":', '"2026-10-20T00:00:00Z"', '"toString"',
  '"mergeCommit":', '"oid":', '"' + "a".repeat(40) + '"', '"' + "A".repeat(40) + '"', '"abc"',
  "null", "{}", "[]", "0", "-1", "1e400", "9007199254740993", '"5"', ",", ":", "{", "}", "[", "]", '"',
  '"hasNextPage":', "true", "false",
];

describe("pull request decoding", () => {
  it("replays the seed corpus", () => {
    const all = seeds("pull_requests");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkPullRequests(parseSeed<PullRequestsInput>(raw), name);
    const normal = parseSeed<PullRequestsInput>(seedNamed(all, "normal.json"));
    const got = decodePullRequests(normal.data, normal.numbers, normal.repository);
    assert.deepEqual(got.get(5)?.prs.map((p) => p.number), [10, 11, 12]);
    assert.equal(got.get(6)?.labeled, false);
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("pull_requests").map(([, raw]) => parseSeed<PullRequestsInput>(raw));
    const rand = rng(20261029);
    for (let n = 0; n < 3000; n++) {
      const base = choose(rand, all);
      const text = mutate(rand, JSON.stringify(base.data), PR_FRAGMENTS);
      let data: unknown;
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
      checkPullRequests({ ...base, data }, `iteration ${n}: ${text.slice(0, 300)}`);
    }
  });
});

/**
 * Oracle for decodeListPage: never throws; keeps, in order, entries that are
 * objects with action "upload" and string name and id, with a finite
 * numeric timestamp or 0; the next start only as a non-empty string.
 *
 * @param data - Parsed listing.
 * @param label - Input name for failure messages.
 */
function checkListPage(data: unknown, label: string) {
  const got = decodeListPage(data);
  const files: unknown[] = [];
  if (isObj(data) && Array.isArray(data.files)) {
    for (const f of data.files) {
      if (!isObj(f) || f.action !== "upload" || typeof f.fileName !== "string" || typeof f.fileId !== "string") continue;
      const t = f.uploadTimestamp;
      files.push({ fileName: f.fileName, fileId: f.fileId, uploadTimestamp: typeof t === "number" && t - t === 0 ? t : 0 });
    }
  }
  const next = isObj(data) && typeof data.nextFileName === "string" && data.nextFileName.length > 0 ? data.nextFileName : null;
  assert.deepEqual(got, { files, nextFileName: next }, label);
}

const LIST_FRAGMENTS = [
  '"upload"', '"hide"', '"folder"', '"start"', '"fileName":', '"fileId":', '"action":', '"uploadTimestamp":', '"nextFileName":',
  '"files":', '"diagnostics/0.9.0/x.json"', '""', "1e400", "-1", "0", "null", "{}", "[]", ",", ":", "{", "}", "[", "]", '"', "é",
];

describe("B2 listing decoding", () => {
  it("replays the seed corpus", () => {
    const all = seeds("b2_list");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkListPage(parseSeed<unknown>(raw), name);
    const normal: unknown = JSON.parse(seedNamed(all, "normal.json"));
    assert.equal(decodeListPage(normal).files.length, 2);
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("b2_list");
    const rand = rng(20261030);
    for (let n = 0; n < 3000; n++) {
      const [, raw] = choose(rand, all);
      const text = mutate(rand, raw, LIST_FRAGMENTS);
      let data: unknown;
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
      checkListPage(data, `iteration ${n}: ${text.slice(0, 300)}`);
    }
  });
});

/**
 * Oracle for decodeIssuePage: entries that are objects with a positive
 * integer number and no pull_request, text fields kept only as strings,
 * "closed" only when GitHub says so; full when the raw page held 100
 * entries; newest is the latest parseable updated_at over every object
 * entry, as ISO-8601.
 *
 * @param data - Parsed page.
 * @param label - Input name for failure messages.
 */
function checkIssuePage(data: unknown, label: string) {
  const got = decodeIssuePage(data);
  const list: unknown[] = Array.isArray(data) ? data : [];
  const issues: unknown[] = [];
  let newest: number | null = null;
  for (const i of list) {
    if (!isObj(i)) continue;
    const t = new Date(str(i.updated_at)).getTime();
    if (!Number.isNaN(t) && (newest === null || t > newest)) newest = t;
    if (i.pull_request != null) continue;
    if (typeof i.number !== "number" || !Number.isInteger(i.number) || i.number < 1 || i.number > Number.MAX_SAFE_INTEGER) continue;
    issues.push({
      number: i.number,
      title: str(i.title),
      state: i.state === "closed" ? "closed" : "open",
      body: str(i.body),
      created_at: str(i.created_at),
      updated_at: str(i.updated_at),
      closed_at: typeof i.closed_at === "string" ? i.closed_at : null,
      fixed_in_label: expectedFixedIn(i.labels),
    });
  }
  assert.deepEqual(got, { issues, full: list.length >= 100, newest: newest === null ? "" : new Date(newest).toISOString() }, label);
}

/**
 * Oracle for fixedInLabel: the first label (a string, or an object's string
 * `name`) of at most 100 characters that is `fixed-in:` followed by an
 * optional `v` and `N.N.N` (each number 1 to 6 digits), then optionally
 * `-` and 1 to 64 identifier characters, then optionally `+` and 1 to 64,
 * giving the part after the `v`.
 *
 * @param labels - Labels as GitHub lists them.
 * @returns Expected version.
 */
function expectedFixedIn(labels: unknown): string | null {
  if (!Array.isArray(labels)) return null;
  const ident = (t: string): boolean => t.length >= 1 && t.length <= 64 && [...t].every((c) => /[0-9A-Za-z.-]/.test(c));
  const num = (t: string): boolean => t.length >= 1 && t.length <= 6 && [...t].every((c) => c >= "0" && c <= "9");
  for (const l of labels) {
    const name = typeof l === "string" ? l : isObj(l) ? str(l.name) : "";
    if (name.length > 100 || !name.startsWith("fixed-in:")) continue;
    let v = name.slice("fixed-in:".length);
    if (v.startsWith("v")) v = v.slice(1);
    const plus = v.indexOf("+");
    const build = plus >= 0 ? v.slice(plus + 1) : null;
    const beforeBuild = plus >= 0 ? v.slice(0, plus) : v;
    const dash = beforeBuild.indexOf("-");
    const pre = dash >= 0 ? beforeBuild.slice(dash + 1) : null;
    const core = dash >= 0 ? beforeBuild.slice(0, dash) : beforeBuild;
    const parts = core.split(".");
    const ok = parts.length === 3 && parts.every(num) && (pre === null || ident(pre)) && (build === null || ident(build));
    if (ok) return v;
  }
  return null;
}

/**
 * Oracle for decodeReleases: never throws; keeps, oldest first (ties by
 * tag), one entry per tag for the objects that are not drafts, whose
 * `tag_name` is 1 to 100 characters of letters, digits, `.`, `_` or `-`,
 * and whose `published_at` is a string the Date parser accepts; prerelease
 * only when exactly true.
 *
 * @param data - Parsed body.
 * @param label - Failure label.
 */
function checkReleases(data: unknown, label: string) {
  const got = decodeReleases(data);
  const seen = new Set<string>();
  const expected: { tag: string; publishedAt: number; prerelease: boolean }[] = [];
  const list: unknown[] = Array.isArray(data) ? data : [];
  for (const r of list) {
    if (!isObj(r) || r.draft === true) continue;
    const tag = typeof r.tag_name === "string" ? r.tag_name : "";
    if (tag.length < 1 || tag.length > 100 || ![...tag].every((c) => /[A-Za-z0-9._-]/.test(c)) || seen.has(tag)) continue;
    const t = typeof r.published_at === "string" ? new Date(r.published_at).getTime() : NaN;
    if (Number.isNaN(t)) continue;
    seen.add(tag);
    expected.push({ tag, publishedAt: t, prerelease: r.prerelease === true });
  }
  expected.sort((a, b) => a.publishedAt - b.publishedAt || (a.tag < b.tag ? -1 : a.tag > b.tag ? 1 : 0));
  assert.deepEqual(got, expected, label);
}

/**
 * Oracle for decodeCompare: true for status `identical` or `behind`, false
 * for `ahead` or `diverged`, null for anything else.
 *
 * @param data - Parsed body.
 * @param label - Failure label.
 */
function checkCompare(data: unknown, label: string) {
  const status = isObj(data) ? str(data.status) : "";
  const answers: Record<string, boolean> = { identical: true, behind: true, ahead: false, diverged: false };
  const expected = Object.hasOwn(answers, status) ? answers[status] : undefined;
  assert.equal(decodeCompare(data), expected === undefined ? null : expected, label);
}

const RELEASE_FRAGMENTS = [
  '"tag_name":', '"published_at":', '"draft":', '"prerelease":', '"status":', '"behind"', '"ahead"', '"identical"', '"diverged"',
  '"v0.4.0"', '"0.4.0-rc.1"', '"release/x"', '"2026-10-21T00:00:00Z"', '"zzz"', "true", "false", "null", "{}", "[]", "0", ",", ":", "{", "}", "[", "]", '"', "é",
];

const LABEL_FRAGMENTS = [
  '"fixed-in:', '"fixed-in:v0.4.2"', '"fixed-in:0.4.2-rc.1+build.7"', '"fixed-in:1.2"', '"fixed-in:v"', '"diagnostics"', '"name":', '"labels":',
  "0", "1234567", ".", "-", "+", "v", "é", "null", "{}", "[]", ",", ":", "{", "}", "[", "]", '"',
];

/**
 * Oracle for the sign-in and token decoders, by kind.
 *
 * @param input - Decoder and body.
 * @param label - Input name for failure messages.
 */
function checkAuthResponse(input: { kind: string; data: unknown; }, label: string) {
  const d = input.data;
  const o: Record<string, unknown> = isObj(d) ? d : {};
  const posInt = (v: unknown): v is number => typeof v === "number" && Number.isInteger(v) && v > 0 && v <= Number.MAX_SAFE_INTEGER;
  switch (input.kind) {
    case "installation":
      assert.equal(decodeInstallationId(d), posInt(o.id) ? o.id : null, label);
      break;
    case "token": {
      const t = typeof o.expires_at === "string" ? new Date(o.expires_at).getTime() : NaN;
      const ok = typeof o.token === "string" && o.token.length > 0 && !Number.isNaN(t);
      assert.deepEqual(decodeInstallationToken(d), ok ? { token: o.token, expiresAt: t } : null, label);
      break;
    }
    case "access":
      assert.equal(decodeAccessToken(d), typeof o.access_token === "string" && o.access_token.length > 0 ? o.access_token : null, label);
      assert.equal(
        oauthErrorCode(d),
        typeof o.error === "string" && o.error.length >= 1 && o.error.length <= 40 && [...o.error].every((c) => c === "_" || (c >= "a" && c <= "z")) ? o.error : null,
        label,
      );
      break;
    case "user": {
      const ok = posInt(o.id) && typeof o.login === "string" && o.login.length >= 1 && o.login.length <= 39 && [...o.login].every((c) => /[A-Za-z0-9-]/.test(c));
      assert.deepEqual(decodeUser(d), ok ? { id: o.id, login: o.login } : null, label);
      break;
    }
    default:
      assert.fail(`unknown kind ${input.kind}`);
  }
}

const REST_FRAGMENTS = [
  '"number":', '"state":', '"closed"', '"open"', '"pull_request":', '"updated_at":', '"2026-10-21T00:00:00Z"', '"9999-99-99"', '"zzz"',
  '"title":', '"body":', '"id":', '"login":', '"token":', '"expires_at":', '"access_token":', '"octo"', '"a b"', '""',
  '"labels":', '"name":', '"fixed-in:v0.4.2"', '"fixed-in:0.4"',
  "null", "{}", "[]", "0", "-1", "7.5", "1e400", "9007199254740993", ",", ":", "{", "}", "[", "]", '"', "é",
];

/**
 * Mutates a seed's JSON text and parses it back, falling back to the text.
 *
 * @param rand - Random source.
 * @param value - Seed value.
 * @returns Mutated value.
 */
function mutateJson(rand: () => number, value: unknown): unknown {
  const text = mutate(rand, JSON.stringify(value), REST_FRAGMENTS);
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

describe("GitHub REST decoding", () => {
  it("replays the seed corpora", () => {
    const pages = seeds("issue_page");
    assert.ok(pages.length > 0);
    for (const [name, raw] of pages) checkIssuePage(JSON.parse(raw), name);
    const full = decodeIssuePage(JSON.parse(seedNamed(pages, "full_page.json")));
    assert.equal(full.full, true);
    const auth = seeds("github_auth");
    assert.ok(auth.length > 0);
    for (const [name, raw] of auth) checkAuthResponse(JSON.parse(raw), name);
    assert.deepEqual(decodeUser(JSON.parse(seedNamed(auth, "user.json")).data), { id: 7, login: "octo" });
    const labeled = decodeIssuePage(JSON.parse(seedNamed(pages, "labels.json"))).issues;
    assert.deepEqual(labeled.map((i) => i.fixed_in_label), ["0.4.2", "0.4.2-rc.1+build.7", null, null]);
    const releases = seeds("releases");
    assert.ok(releases.length > 0);
    for (const [name, raw] of releases) checkReleases(JSON.parse(raw), name);
    assert.deepEqual(decodeReleases(JSON.parse(seedNamed(releases, "normal.json"))).map((r) => r.tag), ["v0.3.2", "v0.4.0-rc.1", "v0.4.0"]);
    const compares = seeds("compare");
    assert.ok(compares.length > 0);
    for (const [name, raw] of compares) checkCompare(JSON.parse(raw), name);
    assert.equal(decodeCompare(JSON.parse(seedNamed(compares, "behind.json"))), true);
  });

  it("holds the release, compare and label oracles under seeded mutation", () => {
    const releases = seeds("releases").map(([, raw]) => JSON.parse(raw));
    const compares = seeds("compare").map(([, raw]) => JSON.parse(raw));
    const labels = [["diagnostics", "fixed-in:v0.4.2"], [{ name: "fixed-in:0.4.2-rc.1+build.7" }, { name: "bug" }], [{ name: "fixed-in:1.2" }], ["fixed-in:v1.2.3.4"], [{ name: `fixed-in:1.0.0-${"a".repeat(64)}` }]];
    const rand = rng(20261101);
    for (let n = 0; n < 3000; n++) {
      const pick = rand();
      if (pick < 0.4) {
        const text = mutate(rand, JSON.stringify(choose(rand, releases)), RELEASE_FRAGMENTS);
        let data;
        try {
          data = JSON.parse(text);
        } catch {
          data = text;
        }
        checkReleases(data, `releases ${n}: ${text.slice(0, 300)}`);
      } else if (pick < 0.6) {
        const text = mutate(rand, JSON.stringify(choose(rand, compares)), RELEASE_FRAGMENTS);
        let data;
        try {
          data = JSON.parse(text);
        } catch {
          data = text;
        }
        checkCompare(data, `compare ${n}: ${text.slice(0, 300)}`);
      } else {
        const text = mutate(rand, JSON.stringify(choose(rand, labels)), LABEL_FRAGMENTS);
        let data;
        try {
          data = JSON.parse(text);
        } catch {
          data = text;
        }
        assert.equal(fixedInLabel(data), expectedFixedIn(data), `labels ${n}: ${text.slice(0, 300)}`);
      }
    }
  });

  it("holds the oracles under seeded mutation", () => {
    const pages = seeds("issue_page").map(([, raw]) => JSON.parse(raw));
    const auth = seeds("github_auth").map(([, raw]) => JSON.parse(raw));
    const rand = rng(20261031);
    for (let n = 0; n < 3000; n++) {
      if (rand() < 0.5) {
        const data = mutateJson(rand, choose(rand, pages));
        checkIssuePage(data, `page ${n}: ${JSON.stringify(data)?.slice(0, 300)}`);
      } else {
        const base = choose(rand, auth);
        const input = { kind: base.kind, data: mutateJson(rand, base.data) };
        checkAuthResponse(input, `auth ${n}: ${JSON.stringify(input).slice(0, 300)}`);
      }
    }
  });
});

/**
 * Oracle for parseIssueNotify: a separate walk over the parsed value.
 *
 * @param text - Body.
 * @param label - Failure label.
 */
function checkIssueNotify(text: string, label: string) {
  let expected = null;
  if (text.length <= 256) {
    let v;
    let ok = true;
    try {
      v = JSON.parse(text);
    } catch {
      ok = false;
    }
    if (ok && v !== null && typeof v === "object" && !Array.isArray(v)) {
      const n = v.issue;
      if (typeof n === "number" && Number.isInteger(n) && n >= 1 && n <= 1_000_000_000) expected = n;
    }
  }
  assert.equal(parseIssueNotify(text), expected, label);
}

/**
 * Oracle for parseSummaryDone: a separate walk over the parsed value, the
 * fingerprint checked character by character.
 *
 * @param text - Body.
 * @param label - Failure label.
 */
function checkSummaryDone(text: string, label: string) {
  let expected = null;
  if (text.length <= 512) {
    let v;
    let ok = true;
    try {
      v = JSON.parse(text);
    } catch {
      ok = false;
    }
    if (ok && v !== null && typeof v === "object" && !Array.isArray(v)) {
      const n = v.issue;
      const fp = v.fingerprint;
      const hex = typeof fp === "string" && fp.length === 64 && [...fp].every((c) => "0123456789abcdef".includes(c));
      if (typeof n === "number" && Number.isInteger(n) && n >= 1 && n <= 1_000_000_000 && hex) expected = { issue: n, fingerprint: fp };
    }
  }
  assert.deepEqual(parseSummaryDone(text), expected, label);
}

/**
 * Oracle for decodeIssue: one well-numbered non-PR issue, labeled exactly
 * when a label (string or object name) is "diagnostics".
 *
 * @param data - Parsed body.
 * @param label - Failure label.
 */
function checkIssue(data: unknown, label: string) {
  const got = decodeIssue(data);
  const valid = isObj(data) && !Array.isArray(data) && data.pull_request == null
    && typeof data.number === "number" && Number.isInteger(data.number) && data.number >= 1 && data.number <= Number.MAX_SAFE_INTEGER;
  if (!valid) {
    assert.equal(got, null, label);
    return;
  }
  assert.ok(got, label);
  assert.equal(got.issue.number, data.number, label);
  assert.equal(got.issue.body, typeof data.body === "string" ? data.body : "", label);
  let labeled = false;
  if (Array.isArray(data.labels)) {
    for (const l of data.labels) {
      if (l === "diagnostics" || (isObj(l) && l.name === "diagnostics")) labeled = true;
    }
  }
  assert.equal(got.labeled, labeled, label);
}

describe("issue notice decoding", () => {
  it("replays the seed corpora", () => {
    const notices = seeds("issue_notify");
    assert.ok(notices.length > 0);
    for (const [name, raw] of notices) checkIssueNotify(raw, name);
    assert.equal(parseIssueNotify(seedNamed(notices, "normal.json")), 400);
    const done = seeds("summary_done");
    assert.ok(done.length > 0);
    for (const [name, raw] of done) checkSummaryDone(raw, name);
    assert.equal(parseSummaryDone(seedNamed(done, "normal.json"))?.issue, 400);
    assert.equal(parseSummaryDone(seedNamed(done, "uppercase.json")), null);
    const issues = seeds("issue");
    assert.ok(issues.length > 0);
    for (const [name, raw] of issues) checkIssue(JSON.parse(raw), name);
    assert.equal(decodeIssue(JSON.parse(seedNamed(issues, "labeled.json")))?.labeled, true);
  });

  it("holds the oracles under seeded mutation", () => {
    const notices = seeds("issue_notify").map(([, raw]) => raw);
    const issues = seeds("issue").map(([, raw]) => JSON.parse(raw));
    const done = seeds("summary_done").map(([, raw]) => raw);
    const rand = rng(20261024);
    for (let n = 0; n < 3000; n++) {
      const pick = rand();
      if (pick < 0.35) {
        const text = mutate(rand, choose(rand, notices), ['"issue"', "1e9", "-1", "null", "{}", "[]", "9007199254740993", '"', ":"]);
        checkIssueNotify(text, `notice ${n}: ${text.slice(0, 200)}`);
      } else if (pick < 0.7) {
        const text = mutate(rand, choose(rand, done), ['"issue"', '"fingerprint"', "1e9", "-1", "null", "{}", "[]", '"', ":", "A", "g", "0", "\u0000"]);
        checkSummaryDone(text, `summary done ${n}: ${text.slice(0, 200)}`);
      } else {
        const data = mutateJson(rand, choose(rand, issues));
        checkIssue(data, `issue ${n}: ${JSON.stringify(data)?.slice(0, 300)}`);
      }
    }
  });
});

/**
 * Oracle for parseLinkAction: re-derives the expected correction with a
 * separate walk over the body (split on `&`, first value per name, `+` as
 * space, strict percent-decoding) and separate rules for each field.
 *
 * @param body - Form body.
 * @param label - Input name for failure messages.
 */
function checkLinkAction(body: unknown, label: string) {
  const got = parseLinkAction(body);
  if (typeof body !== "string" || body.length > MAX_FORM_CHARS) {
    assert.equal(got, null, label);
    return;
  }
  const fields = new Map<string, string>();
  for (const pair of body.split("&")) {
    if (pair === "") continue;
    const eq = pair.indexOf("=");
    const rawName = eq < 0 ? pair : pair.slice(0, eq);
    const rawValue = eq < 0 ? "" : pair.slice(eq + 1);
    const decode = (t: string): string => {
      try {
        return decodeURIComponent(t.replace(/\+/g, " "));
      } catch {
        return "\u0000malformed";
      }
    };
    const name = decode(rawName);
    if (!fields.has(name)) fields.set(name, decode(rawValue));
  }
  const action = fields.get("action");
  const number = (t: string | undefined): number | null => (t !== undefined && /^[1-9][0-9]{0,9}$/.test(t) && Number(t) <= MAX_ISSUE_NUMBER ? Number(t) : null);
  const issue = number(fields.get("issue"));
  const report = fields.get("report");
  const to = number(fields.get("to"));
  const ok = LINK_ACTIONS.some((a) => a === action) && issue !== null
    && (action === "merge" || (report !== undefined && UUID.test(report)))
    && (!(action === "move" || action === "merge") || (to !== null && to !== issue));
  if (!ok) {
    assert.equal(got, null, label);
    return;
  }
  assert.deepEqual(
    got,
    {
      action,
      issue,
      report: action === "merge" ? null : report,
      to: action === "move" || action === "merge" ? to : null,
    },
    label,
  );
}

const LINK_FRAGMENTS = [
  "&", "=", "+", "%", "%2", "%30", "action=", "issue=", "report=", "to=", "back=", "attach", "detach", "move", "merge",
  "400", "403", "0", "-1", "1000000001", "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4", "A", "f", "-", " ", "\u0000", "é",
];

/** The marker the summary comment starts with (spelled out, not imported). */
const ORACLE_MARKER = "<!-- bookclerk-diagnostics: summary -->";

/**
 * Oracle for decodeComments: walks the first hundred entries by hand and
 * takes the first with a positive safe integer `id`, a `user` object whose
 * `type` is exactly "Bot", and a string `body` that begins with the marker.
 *
 * @param data - Parsed comments page.
 * @param label - Input name for failure messages.
 */
function checkComments(data: unknown, label: string) {
  const got = decodeComments(data);
  if (!Array.isArray(data)) {
    assert.equal(got, null, label);
    return;
  }
  const page = data.slice(0, 100);
  let summary = null;
  for (const c of page) {
    if (summary !== null || c === null || typeof c !== "object") continue;
    const idOk = typeof c.id === "number" && Number.isSafeInteger(c.id) && c.id >= 1;
    const userOk = c.user !== null && typeof c.user === "object" && c.user.type === "Bot";
    const bodyOk = typeof c.body === "string" && c.body.slice(0, ORACLE_MARKER.length) === ORACLE_MARKER;
    if (idOk && userOk && bodyOk) summary = c.id;
  }
  assert.deepEqual(got, { summary, count: page.length }, label);
}

const COMMENT_FRAGMENTS = ['"id"', '"user"', '"type"', '"Bot"', '"User"', '"body"', ORACLE_MARKER, "<!-- ", "-->", " ", "\n", "0", "-1", "1.5", "9007199254740993", "null", "[", "]", "{", "}", ",", ":", '"', "é", "\u0000"];

describe("ledger correction form", () => {
  it("replays the seed corpus", () => {
    const all = seeds("link_action");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkLinkAction(raw, name);
    const byName = (name: string): string => seedNamed(all, name);
    assert.deepEqual(parseLinkAction(byName("move.txt")), { action: "move", issue: 400, report: "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4", to: 403 });
    assert.deepEqual(parseLinkAction(byName("merge.txt")), { action: "merge", issue: 400, report: null, to: 403 });
    assert.equal(parseLinkAction(byName("encoded_digits.txt"))?.issue, 400);
    for (const off of ["merge_self.txt", "missing_report.txt", "upper_uuid.txt", "bad_action.txt", "zero_issue.txt", "huge_target.txt", "plus_issue.txt", "too_long.txt", "empty.txt", "json.txt"]) {
      assert.equal(parseLinkAction(byName(off)), null, off);
    }
    for (const v of [null, undefined, 7, {}, ["action=merge"]]) checkLinkAction(v, String(v));
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("link_action").map(([, raw]) => raw);
    const rand = rng(20261025);
    for (let n = 0; n < 6000; n++) {
      const input = mutate(rand, choose(rand, all), LINK_FRAGMENTS);
      checkLinkAction(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});

/**
 * Oracle for isLockedMessage: true only for a plain object whose `message`
 * is a string of at most 1000 characters containing "issue is locked" in
 * any ASCII letter case, single-spaced, with no word character (ASCII letter,
 * digit or underscore) touching either end.
 *
 * @param data - Parsed 403 body.
 * @param label - Input name for failure messages.
 */
function checkLockedMessage(data: unknown, label: string) {
  const got = isLockedMessage(data);
  let want = false;
  const message = isObj(data) ? data.message : undefined;
  if (typeof message === "string" && message.length <= 1000) {
    const text = message.replace(/[A-Z]/g, (c) => c.toLowerCase());
    const isWord = (ch: string | undefined): boolean => ch !== undefined && /[a-z0-9_]/.test(ch);
    for (let at = text.indexOf("issue is locked"); at !== -1 && !want; at = text.indexOf("issue is locked", at + 1)) {
      want = !isWord(text[at - 1]) && !isWord(text[at + "issue is locked".length]);
    }
  }
  assert.equal(got, want, label);
}

const LOCKED_FRAGMENTS = ['"message"', "issue is locked", "ISSUE IS LOCKED", "issue", "is", "locked", "unlocked", "issues", "_", " ", ".", '"', "{", "}", "[", "]", "null", "é", "\u0000", "x".repeat(200)];

describe("issue comments decoding", () => {
  it("replays the seed corpus", () => {
    const all = seeds("issue_comments");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkComments(JSON.parse(raw), name);
    const byName = (name: string): string => seedNamed(all, name);
    assert.deepEqual(decodeComments(JSON.parse(byName("normal.json"))), { summary: 202, count: 3 });
    assert.deepEqual(decodeComments(JSON.parse(byName("user_marker.json"))), { summary: 205, count: 2 }, "a member's forged marker is not the summary");
    assert.deepEqual(decodeComments(JSON.parse(byName("first_wins.json"))), { summary: 207, count: 2 });
    assert.deepEqual(decodeComments(JSON.parse(byName("bad_id.json"))), { summary: 210, count: 5 });
    assert.deepEqual(decodeComments(JSON.parse(byName("full_page.json"))), { summary: 399, count: 100 });
    assert.deepEqual(decodeComments(JSON.parse(byName("over_page.json"))), { summary: null, count: 100 }, "past the page size is not read");
    for (const none of ["empty.json", "no_marker.json", "marker_inside.json", "no_user.json", "body_not_string.json"]) {
      assert.equal(decodeComments(JSON.parse(byName(none)))?.summary, null, none);
    }
    for (const off of ["not_array.json", "null.json", "string.json"]) assert.equal(decodeComments(JSON.parse(byName(off))), null, off);
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("issue_comments").filter(([n]) => n !== "full_page.json" && n !== "over_page.json").map(([, raw]) => raw);
    const rand = rng(20261026);
    for (let n = 0; n < 3000; n++) {
      const text = mutate(rand, choose(rand, all), COMMENT_FRAGMENTS);
      let data: unknown;
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
      checkComments(data, `iteration ${n}: ${text.slice(0, 300)}`);
    }
  });
});

describe("locked issue message", () => {
  it("replays the seed corpus", () => {
    const all = seeds("locked_message");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkLockedMessage(JSON.parse(raw), name);
    const byName = (name: string): string => seedNamed(all, name);
    assert.equal(isLockedMessage(JSON.parse(byName("locked.json"))), true);
    assert.equal(isLockedMessage(JSON.parse(byName("locked_edit.json"))), true);
    for (const off of ["rate_limit.json", "secondary_limit.json", "forbidden.json", "account_locked.json", "message_not_string.json", "array.json", "null.json", "long.json"]) {
      assert.equal(isLockedMessage(JSON.parse(byName(off))), false, off);
    }
    for (const v of [undefined, "issue is locked", 7, {}, { message: null }]) assert.equal(isLockedMessage(v), false, String(v));
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("locked_message").map(([, raw]) => raw);
    const rand = rng(20261028);
    for (let n = 0; n < 3000; n++) {
      const text = mutate(rand, choose(rand, all), LOCKED_FRAGMENTS);
      let data: unknown;
      try {
        data = JSON.parse(text);
      } catch {
        data = text;
      }
      checkLockedMessage(data, `iteration ${n}: ${text.slice(0, 300)}`);
    }
  });
});
