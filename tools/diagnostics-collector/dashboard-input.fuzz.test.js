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
//   GitHub.
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
//   or replaces its stored pull request links.
//
// Each result is checked against an independent oracle written without the
// module's own helpers.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { MAX_CODE_CHARS, OAUTH_COOKIE, parseCallback } from "./dashboard/auth.js";
import {
  DIAGNOSTICS_LABEL,
  decodeAccessToken,
  decodeIssue,
  parseIssueNotify,
  oauthErrorCode,
  decodeInstallationId,
  decodeInstallationToken,
  decodeIssuePage,
  decodePullRequests,
  decodeUser,
  membershipDecision,
} from "./dashboard/github.js";
import { eventLog } from "./dashboard/pages.js";
import { decodeListPage } from "./dashboard/storage.js";
import { memberFromRow, SESSION_COOKIE, sessionToken } from "./dashboard/session.js";
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

/**
 * Oracle for the session decision: a separate cookie walk for the token,
 * and the member rules restated (live, positive id, GitHub-shaped login,
 * admin only when the row says so exactly).
 *
 * @param {{cookie: string, row: unknown, now: number}} input Decision inputs.
 * @param {string} label Input name for failure messages.
 */
function checkSession(input, label) {
  assert.equal(SESSION_COOKIE, "__Host-bc_session");
  const m = /(?:^|;)\s*__Host-bc_session\s*=([^;]*)/.exec(input.cookie);
  const value = m ? m[1].trim() : "";
  assert.equal(sessionToken(input.cookie), /^[A-Za-z0-9_-]{43}$/.test(value) ? value : null, label);

  const got = memberFromRow(input.row, input.now);
  const r = input.row !== null && typeof input.row === "object" ? input.row : null;
  const live =
    r !== null &&
    Number.isFinite(Number(r.expires_at)) &&
    Number(r.expires_at) > input.now &&
    Number.isSafeInteger(Number(r.user_id)) &&
    Number(r.user_id) > 0 &&
    typeof r.login === "string" &&
    /^[A-Za-z0-9-]{1,39}$/.test(r.login);
  assert.equal(got !== null, live, label);
  if (got) {
    assert.equal(got.role, r.role === "admin" ? "admin" : "member", label);
    assert.equal(got.login, r.login, label);
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
    for (const [name, raw] of all) checkSession(JSON.parse(raw), name);
    const valid = JSON.parse(all.find(([n]) => n === "valid.json")[1]);
    assert.ok(memberFromRow(valid.row, valid.now), "the valid seed is a member");
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("session").map(([, raw]) => JSON.parse(raw));
    const rand = rng(20261027);
    for (let n = 0; n < 4000; n++) {
      const base = all[Math.floor(rand() * all.length)];
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
 * @param {{status: number, body: string, user: unknown, owner: unknown}} input Decision inputs.
 * @param {string} label Input name for failure messages.
 */
function checkMembership(input, label) {
  const { status, body, user, owner } = input;
  const login = user !== null && typeof user === "object" ? user.login : undefined;
  const id = user !== null && typeof user === "object" ? user.id : undefined;
  let expected;
  if (status === 404) {
    expected = typeof login === "string" && typeof owner === "string" && login.toLowerCase() === owner.toLowerCase() ? "admin" : null;
  } else if (typeof status !== "number" || status < 200 || status > 299) {
    assert.throws(() => membershipDecision(status, body, user, owner), label);
    return;
  } else {
    let m = null;
    try {
      m = JSON.parse(body);
    } catch {
      m = null;
    }
    const ok =
      m !== null &&
      typeof m === "object" &&
      m.state === "active" &&
      typeof id === "number" &&
      Number.isInteger(id) &&
      id > 0 &&
      id <= Number.MAX_SAFE_INTEGER &&
      m.user !== null &&
      typeof m.user === "object" &&
      m.user.id === id;
    expected = ok ? (m.role === "admin" ? "admin" : "member") : null;
  }
  assert.equal(membershipDecision(status, body, user, owner), expected, label);
}

const MEMBERSHIP_FRAGMENTS = [
  '"active"', '"pending"', '"admin"', '"member"', '"state":', '"role":', '"user":', '"id":', "7", "8", "-7", "0", "1e400",
  "null", "{}", "[]", ",", ":", '"', "{", "}", "Bookclerk", "octo", "OCTO", "é", " ",
];

describe("membership decision", () => {
  it("replays the seed corpus", () => {
    const all = seeds("membership");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkMembership(JSON.parse(raw), name);
    const admin = JSON.parse(all.find(([n]) => n === "active_admin.json")[1]);
    assert.equal(membershipDecision(admin.status, admin.body, admin.user, admin.owner), "admin");
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("membership").map(([, raw]) => JSON.parse(raw));
    const rand = rng(20261028);
    const statuses = [200, 201, 204, 299, 300, 301, 401, 403, 404, 422, 500, 0, -1, 404.5];
    for (let n = 0; n < 4000; n++) {
      const base = all[Math.floor(rand() * all.length)];
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
        input.status = statuses[Math.floor(rand() * statuses.length)];
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
 * string fields only.
 *
 * @param {{data: unknown, numbers: number[], repository: string}} input Decoder inputs.
 * @param {string} label Input name for failure messages.
 */
function checkPullRequests(input, label) {
  const { data, numbers, repository } = input;
  const isObj = (v) => v !== null && typeof v === "object";
  const errors = isObj(data) && Array.isArray(data.errors) && data.errors.length > 0;
  const repo = isObj(data) && isObj(data.data) ? data.data.repository : undefined;
  if (errors || !isObj(repo)) {
    assert.throws(() => decodePullRequests(data, numbers, repository), label);
    return;
  }
  const got = decodePullRequests(data, numbers, repository);
  const str = (v) => (typeof v === "string" ? v : "");
  const states = new Map([["OPEN", "open"], ["CLOSED", "closed"], ["MERGED", "merged"]]);
  const expected = new Map();
  numbers.forEach((n, i) => {
    const issue = repo["i" + i];
    if (!isObj(issue)) return;
    const prs = new Map();
    const consider = (pr) => {
      if (!isObj(pr) || typeof pr.number !== "number" || !Number.isInteger(pr.number) || pr.number < 1 || pr.number > Number.MAX_SAFE_INTEGER) return;
      const where = isObj(pr.repository) ? str(pr.repository.nameWithOwner) : "";
      if (where.toLowerCase() !== repository.toLowerCase()) return;
      if (typeof pr.state !== "string" || !states.has(pr.state)) return;
      const t = Date.parse(str(pr.mergedAt));
      prs.set(pr.number, { number: pr.number, title: str(pr.title).slice(0, 300), state: states.get(pr.state), mergedAt: Number.isNaN(t) ? null : t });
    };
    const nodes = (v) => (isObj(v) && Array.isArray(v.nodes) ? v.nodes : []);
    for (const pr of nodes(issue.closedByPullRequestsReferences)) consider(pr);
    for (const ev of nodes(issue.timelineItems)) {
      if (!isObj(ev)) continue;
      consider(ev.source !== undefined && ev.source !== null ? ev.source : ev.subject);
    }
    const labels = isObj(issue.labels) && Array.isArray(issue.labels.nodes) ? issue.labels.nodes : null;
    const complete = labels !== null && isObj(issue.labels.pageInfo) && issue.labels.pageInfo.hasNextPage === false;
    const labeled = !complete || labels.some((l) => isObj(l) && str(l.name).toLowerCase() === DIAGNOSTICS_LABEL);
    expected.set(n, { labeled, prs: [...prs.values()] });
  });
  assert.deepEqual(got, expected, label);
}

const PR_FRAGMENTS = [
  '"OPEN"', '"CLOSED"', '"MERGED"', '"__proto__"', '"diagnostics"', '"DIAGNOSTICS"', '"Bookclerk/bookclerk"', '"other/repo"',
  '"number":', '"state":', '"nodes":', '"errors":', '"repository":', '"mergedAt":', '"2026-10-20T00:00:00Z"', '"toString"',
  "null", "{}", "[]", "0", "-1", "1e400", "9007199254740993", '"5"', ",", ":", "{", "}", "[", "]", '"',
  '"hasNextPage":', "true", "false",
];

describe("pull request decoding", () => {
  it("replays the seed corpus", () => {
    const all = seeds("pull_requests");
    assert.ok(all.length > 0);
    for (const [name, raw] of all) checkPullRequests(JSON.parse(raw), name);
    const normal = JSON.parse(all.find(([n]) => n === "normal.json")[1]);
    const got = decodePullRequests(normal.data, normal.numbers, normal.repository);
    assert.deepEqual(got.get(5).prs.map((p) => p.number), [10, 11, 12]);
    assert.equal(got.get(6).labeled, false);
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("pull_requests").map(([, raw]) => JSON.parse(raw));
    const rand = rng(20261029);
    for (let n = 0; n < 3000; n++) {
      const base = all[Math.floor(rand() * all.length)];
      const text = mutate(rand, JSON.stringify(base.data), PR_FRAGMENTS);
      let data;
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
 * @param {unknown} data Parsed listing.
 * @param {string} label Input name for failure messages.
 */
function checkListPage(data, label) {
  const got = decodeListPage(data);
  const isObj = (v) => v !== null && typeof v === "object";
  const files = [];
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
    for (const [name, raw] of all) checkListPage(JSON.parse(raw), name);
    const normal = JSON.parse(all.find(([n]) => n === "normal.json")[1]);
    assert.equal(decodeListPage(normal).files.length, 2);
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("b2_list");
    const rand = rng(20261030);
    for (let n = 0; n < 3000; n++) {
      const [, raw] = all[Math.floor(rand() * all.length)];
      const text = mutate(rand, raw, LIST_FRAGMENTS);
      let data;
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
 * @param {unknown} data Parsed page.
 * @param {string} label Input name for failure messages.
 */
function checkIssuePage(data, label) {
  const got = decodeIssuePage(data);
  const isObj = (v) => v !== null && typeof v === "object";
  const str = (v) => (typeof v === "string" ? v : "");
  const list = Array.isArray(data) ? data : [];
  const issues = [];
  let newest = null;
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
    });
  }
  assert.deepEqual(got, { issues, full: list.length >= 100, newest: newest === null ? "" : new Date(newest).toISOString() }, label);
}

/**
 * Oracle for the sign-in and token decoders, by kind.
 *
 * @param {{kind: string, data: unknown}} input Decoder and body.
 * @param {string} label Input name for failure messages.
 */
function checkAuthResponse(input, label) {
  const d = input.data;
  const o = d !== null && typeof d === "object" ? d : {};
  const posInt = (v) => typeof v === "number" && Number.isInteger(v) && v > 0 && v <= Number.MAX_SAFE_INTEGER;
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
  "null", "{}", "[]", "0", "-1", "7.5", "1e400", "9007199254740993", ",", ":", "{", "}", "[", "]", '"', "é",
];

/**
 * Mutates a seed's JSON text and parses it back, falling back to the text.
 *
 * @param {() => number} rand Random source.
 * @param {unknown} value Seed value.
 * @returns {unknown} Mutated value.
 */
function mutateJson(rand, value) {
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
    const full = decodeIssuePage(JSON.parse(pages.find(([n]) => n === "full_page.json")[1]));
    assert.equal(full.full, true);
    const auth = seeds("github_auth");
    assert.ok(auth.length > 0);
    for (const [name, raw] of auth) checkAuthResponse(JSON.parse(raw), name);
    assert.deepEqual(decodeUser(JSON.parse(auth.find(([n]) => n === "user.json")[1]).data), { id: 7, login: "octo" });
  });

  it("holds the oracles under seeded mutation", () => {
    const pages = seeds("issue_page").map(([, raw]) => JSON.parse(raw));
    const auth = seeds("github_auth").map(([, raw]) => JSON.parse(raw));
    const rand = rng(20261031);
    for (let n = 0; n < 3000; n++) {
      if (rand() < 0.5) {
        const data = mutateJson(rand, pages[Math.floor(rand() * pages.length)]);
        checkIssuePage(data, `page ${n}: ${JSON.stringify(data)?.slice(0, 300)}`);
      } else {
        const base = auth[Math.floor(rand() * auth.length)];
        const input = { kind: base.kind, data: mutateJson(rand, base.data) };
        checkAuthResponse(input, `auth ${n}: ${JSON.stringify(input).slice(0, 300)}`);
      }
    }
  });
});

/**
 * Oracle for parseIssueNotify: a separate walk over the parsed value.
 *
 * @param {string} text Body.
 * @param {string} label Failure label.
 */
function checkIssueNotify(text, label) {
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
 * Oracle for decodeIssue: one well-numbered non-PR issue, labeled exactly
 * when a label (string or object name) is "diagnostics".
 *
 * @param {unknown} data Parsed body.
 * @param {string} label Failure label.
 */
function checkIssue(data, label) {
  const got = decodeIssue(data);
  const isObj = (v) => v !== null && typeof v === "object";
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
    assert.equal(parseIssueNotify(notices.find(([n]) => n === "normal.json")[1]), 400);
    const issues = seeds("issue");
    assert.ok(issues.length > 0);
    for (const [name, raw] of issues) checkIssue(JSON.parse(raw), name);
    assert.equal(decodeIssue(JSON.parse(issues.find(([n]) => n === "labeled.json")[1])).labeled, true);
  });

  it("holds the oracles under seeded mutation", () => {
    const notices = seeds("issue_notify").map(([, raw]) => raw);
    const issues = seeds("issue").map(([, raw]) => JSON.parse(raw));
    const rand = rng(20261024);
    for (let n = 0; n < 3000; n++) {
      if (rand() < 0.5) {
        const text = mutate(rand, notices[Math.floor(rand() * notices.length)], ['"issue"', "1e9", "-1", "null", "{}", "[]", "9007199254740993", '"', ":"]);
        checkIssueNotify(text, `notice ${n}: ${text.slice(0, 200)}`);
      } else {
        const data = mutateJson(rand, issues[Math.floor(rand() * issues.length)]);
        checkIssue(data, `issue ${n}: ${JSON.stringify(data)?.slice(0, 300)}`);
      }
    }
  });
});
