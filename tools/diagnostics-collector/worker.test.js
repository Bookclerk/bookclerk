// Error handling of the authenticated routes: a failed upstream call comes
// back as JSON naming the stage, never as Cloudflare's opaque error 1101.
// Also the /report filter that passes over reports an issue already lists.
// Also the ingest cursor the Action stores and pulls from.

import assert from "node:assert/strict";
import { afterEach, describe, it } from "node:test";
import { createD1 } from "./test-support/d1.js";
import worker, { UpstreamError } from "./worker.js";

const KEY = "k".repeat(32);
const ENV = {
  REPORT_API_KEY: KEY,
  B2_KEY_ID: "id",
  B2_APPLICATION_KEY: "secret-application-key",
  B2_BUCKET_ID: "bucket",
};
const realFetch = globalThis.fetch;
const realError = console.error;

afterEach(() => {
  globalThis.fetch = realFetch;
  console.error = realError;
});

/**
 * Replaces fetch with a router over URL substrings.
 *
 * @param {Record<string, () => Response>} routes Substring to response.
 */
function stubFetch(routes) {
  globalThis.fetch = async (input) => {
    const url = String(input);
    for (const [part, respond] of Object.entries(routes)) {
      if (url.includes(part)) return respond();
    }
    throw new Error(`unexpected fetch ${url}`);
  };
}

function report(headers = {}) {
  return new Request("https://collector.test/report?since=0", { headers });
}

describe("worker errors", () => {
  it("names the failed B2 stage to a caller with the report key", async () => {
    const logged = [];
    console.error = (line) => logged.push(line);
    stubFetch({
      "releases/latest": () => new Response("{}", { status: 404 }),
      b2_authorize_account: () =>
        Response.json({ apiUrl: "https://b2.test", downloadUrl: "https://d.test", authorizationToken: "t" }),
      b2_list_file_names: () => Response.json({ code: "unauthorized", message: "no listFiles" }, { status: 401 }),
    });
    const res = await worker.fetch(report({ authorization: `Bearer ${KEY}` }), ENV);
    assert.equal(res.status, 502);
    assert.deepEqual(await res.json(), { error: "upstream failed", detail: "b2 list failed: HTTP 401: unauthorized" });
    assert.equal(logged.length, 1);
    assert.ok(!logged[0].includes(ENV.B2_APPLICATION_KEY));
  });

  it("names a rate-limited GitHub lookup", async () => {
    console.error = () => {};
    stubFetch({ "releases/latest": () => new Response("{}", { status: 403 }) });
    const env = { ...ENV, GITHUB_REPOSITORY: "Bookclerk/bookclerk" };
    const res = await worker.fetch(report({ authorization: `Bearer ${KEY}` }), env);
    assert.equal(res.status, 502);
    assert.equal((await res.json()).detail, "github releases/latest failed: HTTP 403");
  });

  it("names the stage when the request itself fails", async () => {
    const logged = [];
    console.error = (line) => logged.push(line);
    globalThis.fetch = async (input) => {
      throw new TypeError(`connect failed for ${input} with ${ENV.B2_APPLICATION_KEY}`);
    };
    const res = await worker.fetch(report({ authorization: `Bearer ${KEY}` }), ENV);
    assert.equal(res.status, 502);
    assert.equal((await res.json()).detail, "b2 authorize failed");
    assert.ok(!logged[0].includes(ENV.B2_APPLICATION_KEY));
  });

  it("stops reading an oversized B2 error body", async () => {
    console.error = () => {};
    let pulled = 0;
    let cancelled = false;
    const chunk = new TextEncoder().encode(`{"code":"unauthorized","pad":"${"x".repeat(2048)}`);
    const body = new ReadableStream({
      pull(controller) {
        pulled += 1;
        controller.enqueue(chunk);
      },
      cancel() {
        cancelled = true;
      },
    });
    stubFetch({ b2_authorize_account: () => new Response(body, { status: 401 }) });
    const res = await worker.fetch(report({ authorization: `Bearer ${KEY}` }), ENV);
    assert.equal(res.status, 502);
    assert.equal((await res.json()).detail, "b2 authorize failed: HTTP 401");
    assert.ok(cancelled, "the rest of the body is cancelled");
    assert.ok(pulled <= 4, `read ${pulled} chunks`);
  });

  it("keeps B2 codes that are not short identifiers out of the detail", () => {
    assert.equal(new UpstreamError("b2 list", 400, "<b>x</b>").message, "b2 list failed: HTTP 400");
  });

  it("gives /submit callers no upstream detail", async () => {
    console.error = () => {};
    stubFetch({ b2_authorize_account: () => new Response("{}", { status: 503 }) });
    const body = JSON.stringify({ version: "0.1.0", trigger: "test", os: "linux", events: [] });
    const res = await worker.fetch(
      new Request("https://collector.test/submit", { method: "POST", body, headers: { "content-type": "application/json" } }),
      ENV,
    );
    assert.equal(res.status, 502);
    assert.deepEqual(await res.json(), { error: "upstream failed" });
  });

  it("still refuses /report without the key", async () => {
    const res = await worker.fetch(report(), ENV);
    assert.equal(res.status, 401);
  });
});

describe("GET /report?unfiled=1", () => {
  const FILED = "11111111-1111-4111-8111-111111111111";
  const FRESH = "22222222-2222-4222-8222-222222222222";

  /** Stubs B2 with one filed and one fresh report and returns the D1 stand-in. */
  async function setup() {
    const db = createD1();
    await db.prepare("INSERT INTO report_issue (report_id, issue_number) VALUES (?, ?)").bind(FILED, 400).run();
    const files = [FILED, FRESH].map((id, i) => ({
      action: "upload",
      fileName: `diagnostics/0.0.0-synthetic/${id}.json`,
      fileId: `file-${id}`,
      uploadTimestamp: 1000 + i,
      contentLength: 2,
    }));
    stubFetch({
      b2_authorize_account: () =>
        Response.json({ apiUrl: "https://b2.test", downloadUrl: "https://d.test", authorizationToken: "t" }),
      b2_list_file_names: () => Response.json({ files, nextFileName: null }),
      [`file-${FILED}`]: () => Response.json({ report_id: FILED }),
      [`file-${FRESH}`]: () => Response.json({ report_id: FRESH }),
    });
    return db;
  }

  /** Pulls /report from the start with the given extra query. */
  async function pull(db, query) {
    const req = new Request(`https://collector.test/report?since=0&baseline_version=${query}`, {
      headers: { authorization: `Bearer ${KEY}` },
    });
    return (await worker.fetch(req, { ...ENV, DASHBOARD_DB: db })).json();
  }

  it("skips reports an issue lists and still moves the cursor past them", async () => {
    const out = await pull(await setup(), "&unfiled=1");
    assert.deepEqual(out.reports.map((r) => r.report_id), [FRESH]);
    assert.equal(out.skipped_filed, 1);
    assert.equal(out.filed_filter, "on");
    assert.equal(out.next_since, 1001);
  });

  it("returns every report without the flag", async () => {
    const out = await pull(await setup(), "");
    assert.deepEqual(out.reports.map((r) => r.report_id), [FILED, FRESH]);
    assert.equal(out.skipped_filed, 0);
    assert.equal(out.filed_filter, "off");
    assert.deepEqual(out.linked, []);
  });

  it("links a report whose signature an open issue tracks instead of returning it", async () => {
    const db = await setup();
    // FILED is indexed with a signature and tracked by open issue #400;
    // FRESH shares the signature on the same version line.
    await db
      .prepare("INSERT INTO issues (number, title, state, kind, created_at, updated_at) VALUES (400, 't', 'open', 'triage', 1, 1)")
      .run();
    for (const [id, version] of [[FILED, "0.0.0-synthetic"], [FRESH, "0.0.1"]]) {
      await db
        .prepare("INSERT INTO reports (report_id, object_key, received_at, version, signature) VALUES (?, ?, 1, ?, 'sig')")
        .bind(id, `diagnostics/${version}/${id}.json`, version)
        .run();
    }
    let downloads = 0;
    const listing = globalThis.fetch;
    globalThis.fetch = async (input, init) => {
      if (String(input).includes("file-")) downloads += 1;
      return listing(input, init);
    };
    const out = await pull(db, "&unfiled=1");
    assert.deepEqual(out.reports, []);
    assert.deepEqual(out.linked, [{ report_id: FRESH, issue: 400, state: "open" }]);
    assert.equal(out.linked_signature, 1);
    assert.equal(out.next_since, 1001, "the cursor moves past a linked report");
    assert.equal(downloads, 0, "a linked report is not downloaded");
    const row = await db.prepare("SELECT source, linked_by FROM report_issue WHERE report_id = ?").bind(FRESH).first();
    assert.deepEqual(row, { source: "signature", linked_by: "ingest" });
    assert.deepEqual((await pull(db, "")).linked, [], "a force_full pull links nothing");
  });
});

describe("GET /report?unfiled=1 against closed issues", () => {
  const OLD = "11111111-1111-4111-8111-111111111111";
  const NEW = "22222222-2222-4222-8222-222222222222";
  const SHA = "a".repeat(40);

  /**
   * Stubs B2 with one report a closed issue tracks and one fresh report of
   * the given version sharing its signature, and returns the D1 stand-in.
   * The closed issue #412 has its fix recorded as shipped in `fixedIn`
   * (null for no fix row).
   */
  async function setup(version, fixedIn) {
    const db = createD1();
    await db
      .prepare("INSERT INTO issues (number, title, state, kind, created_at, updated_at, closed_at) VALUES (412, 'Libro stalls', 'closed', 'triage', 1, 2, 2)")
      .run();
    await db.prepare("INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, 412, 'filed')").bind(OLD).run();
    if (fixedIn) {
      await db
        .prepare("INSERT INTO issue_fix (issue_number, recorded_at, source, fixed_in, pr_number, merge_sha, recorded_by) VALUES (412, 3, 'release', ?, 418, ?, 'sync')")
        .bind(fixedIn, SHA)
        .run();
    }
    for (const [id, v] of [[OLD, "0.9.0"], [NEW, version]]) {
      await db
        .prepare("INSERT INTO reports (report_id, object_key, received_at, version, signature) VALUES (?, ?, 1, ?, 'sig')")
        .bind(id, `diagnostics/${v}/${id}.json`, v)
        .run();
    }
    const files = [[OLD, "0.9.0"], [NEW, version]].map(([id, v], i) => ({
      action: "upload",
      fileName: `diagnostics/${v}/${id}.json`,
      fileId: `file-${id}`,
      uploadTimestamp: 1000 + i,
      contentLength: 2,
    }));
    stubFetch({
      b2_authorize_account: () =>
        Response.json({ apiUrl: "https://b2.test", downloadUrl: "https://d.test", authorizationToken: "t" }),
      b2_list_file_names: () => Response.json({ files, nextFileName: null }),
      [`file-${OLD}`]: () => Response.json({ report_id: OLD }),
      [`file-${NEW}`]: () => Response.json({ report_id: NEW }),
    });
    return db;
  }

  /** Pulls /report from the start with the filed filter on. */
  async function pull(db) {
    const req = new Request("https://collector.test/report?since=0&baseline_version=&unfiled=1", {
      headers: { authorization: `Bearer ${KEY}` },
    });
    return (await worker.fetch(req, { ...ENV, DASHBOARD_DB: db })).json();
  }

  it("returns a report from the fixed version on as a regression of the closed issue", async () => {
    const db = await setup("0.9.3", "0.9.2");
    const out = await pull(db);
    assert.equal(out.regressions, 1);
    assert.deepEqual(out.reports.map((r) => [r.report_id, r.regression_of]), [[NEW, { issue: 412, fixed_in: "0.9.2", pr_number: 418, title: "Libro stalls" }]]);
    assert.deepEqual(out.linked, []);
    assert.equal(await db.prepare("SELECT COUNT(*) AS n FROM report_issue WHERE report_id = ?").bind(NEW).first().then((r) => r.n), 0, "a regression is linked by the issue ingest files, not here");
  });

  it("links a report from an older build to the closed issue quietly", async () => {
    const db = await setup("0.9.1", "0.9.2");
    const out = await pull(db);
    assert.equal(out.regressions, 0);
    assert.deepEqual(out.reports, []);
    assert.deepEqual(out.linked, [{ report_id: NEW, issue: 412, state: "closed" }]);
    const row = await db.prepare("SELECT source, linked_by FROM report_issue WHERE report_id = ?").bind(NEW).first();
    assert.deepEqual(row, { source: "signature", linked_by: "ingest" });
  });

  it("links to a closed issue without a fix only on the same version line", async () => {
    let out = await pull(await setup("0.9.7", null));
    assert.deepEqual(out.linked.map((l) => l.issue), [412]);
    assert.deepEqual(out.reports, []);
    out = await pull(await setup("0.10.0", null));
    assert.deepEqual(out.linked, []);
    assert.deepEqual(out.reports.map((r) => [r.report_id, r.regression_of]), [[NEW, null]], "a new line goes to triage");
  });

  it("honors a member's exclusion from the closed issue", async () => {
    const db = await setup("0.9.3", "0.9.2");
    await db.prepare("INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, 412, 'excluded')").bind(NEW).run();
    const out = await pull(db);
    assert.deepEqual(out.reports.map((r) => r.regression_of), [null]);
  });

  /** Adds a second closed issue tracking the same signature through a report of `version`. */
  async function closedIssue(db, number, version, fixedIn) {
    const id = `${String(number).padStart(8, "0")}-0000-4000-8000-000000000000`;
    await db
      .prepare("INSERT INTO issues (number, title, state, kind, created_at, updated_at, closed_at) VALUES (?, 'Again', 'closed', 'triage', 4, 5, 5)")
      .bind(number)
      .run();
    await db.prepare("INSERT INTO reports (report_id, object_key, received_at, version, signature) VALUES (?, ?, 2, ?, 'sig')").bind(id, `diagnostics/${version}/${id}.json`, version).run();
    await db.prepare("INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, ?, 'filed')").bind(id, number).run();
    if (fixedIn) {
      await db
        .prepare("INSERT INTO issue_fix (issue_number, recorded_at, source, fixed_in, pr_number, merge_sha, recorded_by) VALUES (?, 6, 'release', ?, 430, ?, 'sync')")
        .bind(number, fixedIn, SHA)
        .run();
    }
  }

  it("goes back to the closed issue with the newest fix", async () => {
    const db = await setup("0.9.9", "0.9.2");
    await closedIssue(db, 420, "0.9.3", "0.9.5");
    const out = await pull(db);
    assert.deepEqual(out.reports.map((r) => [r.regression_of.issue, r.regression_of.fixed_in, r.regression_of.pr_number]), [[420, "0.9.5", 430]]);
    // Before the newer fix shipped, the older fix is the one that came back.
    const older = await setup("0.9.4", "0.9.2");
    await closedIssue(older, 420, "0.9.3", "0.9.5");
    assert.deepEqual((await pull(older)).reports.map((r) => r.regression_of.issue), [412]);
  });

  it("joins a closed regression issue on the line instead of filing another", async () => {
    // #420 was the regression issue for the 0.9 line and a member closed it
    // without a fix (not a regression after all); the next 0.9 report joins
    // it quietly rather than reopening the question with a new issue.
    const db = await setup("0.9.9", "0.9.2");
    await closedIssue(db, 420, "0.9.3", null);
    const out = await pull(db);
    assert.equal(out.regressions, 0);
    assert.deepEqual(out.reports, []);
    assert.deepEqual(out.linked, [{ report_id: NEW, issue: 420, state: "closed" }]);
    // A report from another line is still a regression of the fixed issue.
    const next = await setup("0.10.0", "0.9.2");
    await closedIssue(next, 420, "0.9.3", null);
    assert.deepEqual((await pull(next)).reports.map((r) => r.regression_of.issue), [412]);
  });
});

describe("ingest cursor routes", () => {
  /** Posts a cursor body with the report key. */
  function commit(env, body) {
    return worker.fetch(
      new Request("https://collector.test/report/cursor", {
        method: "POST",
        body,
        headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
      }),
      env,
    );
  }

  it("stores a later cursor, keeps it on an earlier one, and pulls from it", async () => {
    const env = { ...ENV, DASHBOARD_DB: createD1() };
    const later = { since: 1792831701483, after: "diagnostics/0.0.0-synthetic/b.json" };
    let res = await commit(env, JSON.stringify(later));
    assert.equal(res.status, 200);
    assert.deepEqual(await res.json(), { ok: true, advanced: true, ...later });
    // A forced full pull ends earlier: the stored cursor stays.
    res = await commit(env, JSON.stringify({ since: 5, after: "" }));
    assert.deepEqual(await res.json(), { ok: true, advanced: false, ...later });
    // Same time, later name: moves.
    res = await commit(env, JSON.stringify({ ...later, after: "diagnostics/0.0.0-synthetic/c.json" }));
    assert.equal((await res.json()).advanced, true);

    let listedFrom = null;
    stubFetch({
      b2_authorize_account: () =>
        Response.json({ apiUrl: "https://b2.test", downloadUrl: "https://d.test", authorizationToken: "t" }),
      b2_list_file_names: () => {
        listedFrom = true;
        return Response.json({
          files: [
            { action: "upload", fileName: "diagnostics/0.0.0-synthetic/b.json", fileId: "1", uploadTimestamp: later.since },
            { action: "upload", fileName: "diagnostics/0.0.0-synthetic/c.json", fileId: "2", uploadTimestamp: later.since },
          ],
          nextFileName: null,
        });
      },
    });
    res = await worker.fetch(
      new Request("https://collector.test/report?cursor=stored&baseline_version=", { headers: { authorization: `Bearer ${KEY}` } }),
      env,
    );
    assert.equal(res.status, 200);
    const body = await res.json();
    assert.ok(listedFrom);
    assert.equal(body.cursor_source, "stored");
    assert.equal(body.since, later.since);
    assert.equal(body.after, "diagnostics/0.0.0-synthetic/c.json");
    assert.equal(body.count, 0);
  });

  it("starts from the beginning with nothing stored, and an explicit since wins", async () => {
    const env = { ...ENV, DASHBOARD_DB: createD1() };
    await commit(env, JSON.stringify({ since: 99, after: "x" }));
    stubFetch({
      b2_authorize_account: () =>
        Response.json({ apiUrl: "https://b2.test", downloadUrl: "https://d.test", authorizationToken: "t" }),
      b2_list_file_names: () => Response.json({ files: [], nextFileName: null }),
    });
    const get = (q, e) =>
      worker.fetch(new Request(`https://collector.test/report?${q}&baseline_version=`, { headers: { authorization: `Bearer ${KEY}` } }), e);
    const fresh = await (await get("cursor=stored", { ...ENV, DASHBOARD_DB: createD1() })).json();
    assert.deepEqual([fresh.since, fresh.after, fresh.cursor_source], [0, "", "stored"]);
    const forced = await (await get("cursor=stored&since=0", env)).json();
    assert.deepEqual([forced.since, forced.after, forced.cursor_source], [0, "", "query"]);
  });

  it("refuses a missing key, a malformed body and a missing database", async () => {
    const env = { ...ENV, DASHBOARD_DB: createD1() };
    const anon = await worker.fetch(new Request("https://collector.test/report/cursor", { method: "POST", body: '{"since":1}' }), env);
    assert.equal(anon.status, 401);
    for (const bad of ['{"since":-1}', '{"since":1,"after":"a\\u0000"}', "x".repeat(2000), "[]"]) {
      assert.equal((await commit(env, bad)).status, 400, bad);
    }
    assert.equal((await commit(ENV, '{"since":1}')).status, 503);
    const stored = await worker.fetch(
      new Request("https://collector.test/report?cursor=stored", { headers: { authorization: `Bearer ${KEY}` } }),
      ENV,
    );
    assert.equal(stored.status, 503);
  });
});
