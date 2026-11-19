// Report lookup by access code: /submit hands the sender a code and keeps
// only its hash, /lookup answers with the report, its issues and fix (JSON
// for Bookclerk, HTML for the public page), and /lookup/delete removes the
// stored copy while the index row and issue links stay.

import assert from "node:assert/strict";
import { afterEach, describe, it } from "vitest";
import { all, first } from "./dashboard/db.ts";
import type { Env, RateLimiter } from "./env.ts";
import { lookupFetch, type LookupAnswer } from "./lookup.ts";
import {
  ACCESS_CODE_ALPHABET,
  ACCESS_RETENTION_MS,
  accessHash,
  formatAccessCode,
  newAccessCode,
  normalizeAccessCode,
  accessCounterDay,
  pruneAccess,
  storeAccess,
} from "./report-access.ts";
import { makeB2 } from "./test-support/b2.ts";
import { createD1 } from "./test-support/d1.ts";
import worker from "./worker.ts";

const NOW = Date.UTC(2026, 10, 19, 12, 0, 0);
/** Background work is collected so a test can wait for it. */
const realFetch = globalThis.fetch;
const realError = console.error;

afterEach(() => {
  globalThis.fetch = realFetch;
  console.error = realError;
});

/** The payload the tests submit. */
const PAYLOAD = {
  version: "0.9.0",
  trigger: "cli_error",
  os: "linux",
  arch: "x86_64",
  deployment: "",
  events: [
    {
      ts_unix_ms: NOW,
      level: "ERROR",
      target: "bookclerk::library",
      message: "scan failed",
      fields: [["account", "[REDACTED]"]],
    },
  ],
};

/**
 * Stubs B2 for an upload and remembers what was stored.
 *
 * @returns The stored body, filled in once the upload runs.
 */
function stubUpload(): { body: string } {
  const out = { body: "" };
  globalThis.fetch = async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const url = String(input);
    if (url.includes("b2_authorize_account")) {
      return Response.json({ apiUrl: "https://b2.test", authorizationToken: "auth" });
    }
    if (url.includes("b2_get_upload_url")) {
      return Response.json({ uploadUrl: "https://b2.test/upload", authorizationToken: "up" });
    }
    if (url === "https://b2.test/upload") {
      out.body = await new Response(init.body).text();
      return Response.json({ fileId: "f" });
    }
    throw new Error(`unexpected fetch ${url}`);
  };
  return out;
}

/**
 * Submits PAYLOAD.
 *
 * @param env - Worker bindings.
 * @returns The reply and the stored body.
 */
async function submit(env: Env): Promise<{ reply: Record<string, unknown>; stored: string }> {
  const upload = stubUpload();
  const pending: Promise<unknown>[] = [];
  const res = await worker.fetch(
    new Request("https://collector.test/submit", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(PAYLOAD),
    }),
    env,
    { waitUntil: (p: Promise<unknown>) => void pending.push(p) },
  );
  assert.equal(res.status, 201);
  await Promise.all(pending);
  return { reply: await res.json(), stored: upload.body };
}

/**
 * A lookup request.
 *
 * @param path - `/lookup` or `/lookup/delete`.
 * @param body - Body text.
 * @param type - Content type.
 * @returns The request.
 */
function post(path: string, body: string, type: string = "application/json"): Request {
  return new Request(`https://collector.test${path}`, {
    method: "POST",
    headers: { "content-type": type },
    body,
  });
}

/**
 * A rate limiter that allows `n` attempts per key.
 *
 * @param n - Attempts allowed.
 * @returns The limiter.
 */
function limiter(n: number): RateLimiter {
  const seen = new Map<string, number>();
  return {
    limit: async ({ key }) => {
      const count = (seen.get(key) ?? 0) + 1;
      seen.set(key, count);
      return { success: count <= n };
    },
  };
}

describe("access codes", () => {
  it("are 16 alphabet characters that read back as themselves", () => {
    for (let i = 0; i < 200; i++) {
      const code = newAccessCode();
      assert.equal(code.length, 16);
      assert.ok(
        [...code].every((c) => ACCESS_CODE_ALPHABET.includes(c)),
        code,
      );
      assert.equal(normalizeAccessCode(code), code);
      assert.equal(normalizeAccessCode(formatAccessCode(code).toLowerCase()), code);
    }
    assert.equal(formatAccessCode("K7QM3XRDPW9TH2CF"), "K7QM-3XRD-PW9T-H2CF");
    assert.equal(normalizeAccessCode("k7qm 3xrd-pw9t-h2cf"), "K7QM3XRDPW9TH2CF");
    // Crockford's look-alikes read as digits; U is not in the alphabet.
    assert.equal(normalizeAccessCode("OIL0-0000-0000-0000"), "0110000000000000");
    assert.equal(normalizeAccessCode("U000-0000-0000-0000"), null);
    assert.equal(normalizeAccessCode("K7QM-3XRD-PW9T-H2C"), null);
  });

  it("bind the hash to the report", async () => {
    const a = await accessHash("3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f", "K7QM3XRDPW9TH2CF");
    const b = await accessHash("9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d", "K7QM3XRDPW9TH2CF");
    assert.match(a, /^[0-9a-f]{64}$/);
    assert.notEqual(a, b);
  });

  it("stop at the daily index cap and are pruned after their reports", async () => {
    const db = createD1();
    const day = new Date(NOW).toISOString().slice(0, 10);
    await db
      .prepare("INSERT INTO index_days (day, rows) VALUES (?, 2)")
      .bind(accessCounterDay(day))
      .run();
    const row = (id: string, at: number) => ({
      report_id: id,
      object_key: `k/${id}`,
      created_at: at,
    });
    assert.equal(await storeAccess(db, row("a", NOW), "K7QM3XRDPW9TH2CF", 3), true);
    assert.equal(await storeAccess(db, row("b", NOW), "K7QM3XRDPW9TH2CF", 3), false);
    assert.equal(await storeAccess(db, row("a", NOW), "K7QM3XRDPW9TH2CF", 9), false);
    assert.equal(await storeAccess(db, row("z", NOW), "K7QM3XRDPW9TH2CF", 0), false);
    // The report index's own count is untouched, and a refused duplicate
    // handed its slot back.
    assert.equal(await first(db, "SELECT rows FROM index_days WHERE day = ?", day), null);
    assert.equal(
      (await first(db, "SELECT rows FROM index_days WHERE day = ?", accessCounterDay(day)))?.rows,
      3,
    );
    assert.equal(
      await storeAccess(db, row("c", NOW - ACCESS_RETENTION_MS - 1), "K7QM3XRDPW9TH2CF", 3),
      true,
    );
    assert.equal(await pruneAccess(db, NOW), 1);
    const left = await all(db, "SELECT report_id FROM report_access");
    assert.deepEqual(
      left.map((r) => r.report_id),
      ["a"],
    );
  });
});

describe("POST /submit", () => {
  it("answers with an access code and keeps only its hash, never in storage", async () => {
    const db = createD1();
    const env: Env = {
      DASHBOARD_DB: db,
      B2_KEY_ID: "id",
      B2_APPLICATION_KEY: "secret-application-key",
      B2_BUCKET_ID: "bucket",
    };
    const { reply, stored } = await submit(env);
    const code = String(reply.access_code);
    assert.equal(normalizeAccessCode(code), code);
    assert.ok(!stored.includes(code), "the code never reaches storage");
    const row = await first(db, "SELECT * FROM report_access");
    assert.equal(row?.report_id, reply.report_id);
    assert.equal(row?.object_key, reply.key);
    assert.equal(row?.code_hash, await accessHash(String(reply.report_id), code));
    assert.equal(row?.deleted_at, null);
  });

  it("answers without a code when there is no database", async () => {
    const env: Env = {
      B2_KEY_ID: "id",
      B2_APPLICATION_KEY: "secret-application-key",
      B2_BUCKET_ID: "bucket",
    };
    const { reply } = await submit(env);
    assert.equal(reply.access_code, undefined);
  });
});

describe("/lookup", () => {
  /**
   * A database with one submitted report and its issue.
   *
   * @returns Bindings, the report ID, its code and the stored object.
   */
  async function setUp(): Promise<{
    env: Env;
    id: string;
    code: string;
    key: string;
    objects: Record<string, string>;
  }> {
    const db = createD1();
    const env: Env = {
      DASHBOARD_DB: db,
      GITHUB_REPOSITORY: "Bookclerk/bookclerk",
      B2_KEY_ID: "id",
      B2_APPLICATION_KEY: "secret-application-key",
      B2_BUCKET_ID: "bucket",
    };
    const { reply, stored } = await submit(env);
    const id = String(reply.report_id);
    await db.batch([
      db
        .prepare(
          "INSERT INTO issues (number, title, state, kind, created_at, updated_at) VALUES (42, 'Scan fails', 'closed', 'triage', ?, ?)",
        )
        .bind(NOW, NOW),
      db
        .prepare(
          "INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, 42, 'body')",
        )
        .bind(id),
      db
        .prepare(
          "INSERT INTO issue_fix (issue_number, recorded_at, source, fixed_in, pr_number, recorded_by) VALUES (42, ?, 'release', '0.9.1', 77, 'sync')",
        )
        .bind(NOW),
    ]);
    return {
      env,
      id,
      code: String(reply.access_code),
      key: String(reply.key),
      objects: { f: stored },
    };
  }

  /**
   * B2 that serves the stored report by its key.
   *
   * @param key - Object key.
   * @param objects - Stored text by file ID.
   * @param deleted - Collects deleted keys.
   * @returns The helpers.
   */
  function b2For(key: string, objects: Record<string, string>, deleted: string[] = []) {
    globalThis.fetch = async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("b2_list_file_names")) {
        return Response.json({ files: [{ action: "upload", fileName: key, fileId: "f" }] });
      }
      throw new Error(`unexpected fetch ${url}`);
    };
    return makeB2(objects, deleted);
  }

  it("takes a fix only from the tracking issue's own pull requests", async () => {
    const { env, id, code, key, objects } = await setUp();
    const db = env.DASHBOARD_DB!;
    await db.batch([
      db.prepare("DELETE FROM issue_fix"),
      db
        .prepare(
          "INSERT INTO issues (number, title, state, kind, created_at, updated_at) VALUES (43, 'Other', 'closed', 'triage', ?, ?)",
        )
        .bind(NOW, NOW),
      db
        .prepare(
          "INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, 43, 'body')",
        )
        .bind(id),
      db
        .prepare(
          "INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (43, 88, 'Fix other', 'merged', ?)",
        )
        .bind(NOW),
    ]);
    const look = async (): Promise<LookupAnswer> => {
      const res = await lookupFetch(
        post("/lookup", JSON.stringify({ report_id: id, access_code: code })),
        env,
        "/lookup",
        b2For(key, objects),
        NOW,
      );
      assert.equal(res?.status, 200);
      return res!.json<LookupAnswer>();
    };
    const first = await look();
    assert.deepEqual(
      first.issues.map((i) => i.number),
      [42, 43],
    );
    assert.equal(first.fix, null, "another issue's pull request is not this report's fix");
    await db
      .prepare(
        "INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (42, 70, 'Fix scan', 'merged', ?)",
      )
      .bind(NOW - 1)
      .run();
    assert.deepEqual((await look()).fix, {
      pr_number: 70,
      pr_url: "https://github.com/Bookclerk/bookclerk/pull/70",
      fixed_in: null,
    });
  });

  it("serves the form page with no script", async () => {
    const { env } = await setUp();
    const res = await worker.fetch(new Request("https://collector.test/lookup"), env, {
      waitUntil() {},
    });
    assert.equal(res.status, 200);
    const page = await res.text();
    assert.match(page, /<form method="post" action="\/lookup"/);
    assert.ok(!page.includes("<script"), "the public page loads no script");
    assert.match(res.headers.get("Content-Security-Policy") ?? "", /form-action 'self'/);
  });

  it("returns the report, its issue and fix as JSON for the right code only", async () => {
    const { env, id, code, key, objects } = await setUp();
    const b2 = b2For(key, objects);
    const res = await lookupFetch(
      post("/lookup", JSON.stringify({ report_id: id, access_code: formatAccessCode(code) })),
      env,
      "/lookup",
      b2,
      NOW,
    );
    assert.equal(res?.status, 200);
    const answer = await res!.json<LookupAnswer>();
    assert.equal(answer.report_id, id);
    assert.equal(answer.deleted_at_unix_ms, null);
    assert.equal(answer.report?.version, "0.9.0");
    assert.deepEqual(answer.report?.events[0], {
      ts_unix_ms: NOW,
      level: "ERROR",
      target: "bookclerk::library",
      message: "scan failed",
      fields: [["account", "[REDACTED]"]],
    });
    assert.deepEqual(answer.issues, [
      {
        number: 42,
        title: "Scan fails",
        state: "closed",
        url: "https://github.com/Bookclerk/bookclerk/issues/42",
      },
    ]);
    assert.deepEqual(answer.fix, {
      pr_number: 77,
      pr_url: "https://github.com/Bookclerk/bookclerk/pull/77",
      fixed_in: "0.9.1",
    });

    const wrong = code[0] === "A" ? `B${code.slice(1)}` : `A${code.slice(1)}`;
    const bad = await lookupFetch(
      post("/lookup", JSON.stringify({ report_id: id, access_code: wrong })),
      env,
      "/lookup",
      b2,
      NOW,
    );
    assert.equal(bad?.status, 404);
    const unknown = await lookupFetch(
      post(
        "/lookup",
        JSON.stringify({ report_id: "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d", access_code: code }),
      ),
      env,
      "/lookup",
      b2,
      NOW,
    );
    assert.equal(unknown?.status, 404);
    assert.deepEqual(await bad!.json(), await unknown!.json(), "the same answer either way");
    const malformed = await lookupFetch(
      post("/lookup", JSON.stringify({ report_id: id })),
      env,
      "/lookup",
      b2,
      NOW,
    );
    assert.equal(malformed?.status, 400);
  });

  it("renders the report as a page for the form, with a delete form", async () => {
    const { env, id, code, key, objects } = await setUp();
    const res = await lookupFetch(
      post(
        "/lookup",
        new URLSearchParams({ report_id: id.toUpperCase(), access_code: code }).toString(),
        "application/x-www-form-urlencoded",
      ),
      env,
      "/lookup",
      b2For(key, objects),
      NOW,
    );
    assert.equal(res?.status, 200);
    const page = await res!.text();
    assert.ok(page.includes("scan failed"));
    assert.ok(page.includes("https://github.com/Bookclerk/bookclerk/issues/42"));
    assert.ok(page.includes("in Bookclerk 0.9.1"));
    assert.match(page, /action="\/lookup\/delete"/);
    assert.ok(!page.includes("<script"));

    const miss = await lookupFetch(
      post(
        "/lookup",
        new URLSearchParams({ report_id: id, access_code: "0000-0000-0000-0000" }).toString(),
        "application/x-www-form-urlencoded",
      ),
      env,
      "/lookup",
      b2For(key, objects),
      NOW,
    );
    assert.equal(miss?.status, 404);
    assert.ok((await miss!.text()).includes("No report matches"));
  });

  it("limits attempts per report", async () => {
    const { env, id, key, objects } = await setUp();
    const limited = { ...env, LOOKUP_LIMIT: limiter(2) };
    const attempt = () =>
      lookupFetch(
        post("/lookup", JSON.stringify({ report_id: id, access_code: "0000000000000000" })),
        limited,
        "/lookup",
        b2For(key, objects),
        NOW,
      );
    assert.equal((await attempt())?.status, 404);
    assert.equal((await attempt())?.status, 404);
    assert.equal((await attempt())?.status, 429);
  });

  it("deletes the stored copy but keeps the index row and issue links", async () => {
    const { env, id, code, key, objects } = await setUp();
    const deleted: string[] = [];
    const db = env.DASHBOARD_DB!;
    const unconfirmed = await lookupFetch(
      post(
        "/lookup/delete",
        new URLSearchParams({ report_id: id, access_code: code }).toString(),
        "application/x-www-form-urlencoded",
      ),
      env,
      "/lookup/delete",
      b2For(key, objects, deleted),
      NOW,
    );
    assert.equal(unconfirmed?.status, 400);
    assert.deepEqual(deleted, []);

    const res = await lookupFetch(
      post(
        "/lookup/delete",
        new URLSearchParams({ report_id: id, access_code: code, confirm: "yes" }).toString(),
        "application/x-www-form-urlencoded",
      ),
      env,
      "/lookup/delete",
      b2For(key, objects, deleted),
      NOW,
    );
    assert.equal(res?.status, 200);
    assert.ok((await res!.text()).includes("Report deleted"));
    assert.deepEqual(deleted, [key]);
    assert.equal((await first(db, "SELECT deleted_at FROM reports"))?.deleted_at, NOW);
    assert.equal((await first(db, "SELECT deleted_at FROM report_access"))?.deleted_at, NOW);
    assert.equal((await all(db, "SELECT * FROM report_issue")).length, 1);

    // Deleting again does nothing more; the lookup now says it is deleted.
    const again = await lookupFetch(
      post("/lookup/delete", JSON.stringify({ report_id: id, access_code: code })),
      env,
      "/lookup/delete",
      b2For(key, objects, deleted),
      NOW + 1000,
    );
    assert.deepEqual(await again!.json(), { report_id: id, deleted_at_unix_ms: NOW });
    assert.deepEqual(deleted, [key]);
    const after = await lookupFetch(
      post("/lookup", JSON.stringify({ report_id: id, access_code: code })),
      env,
      "/lookup",
      b2For(key, objects, deleted),
      NOW + 2000,
    );
    const answer = await after!.json<LookupAnswer>();
    assert.equal(answer.report, null);
    assert.equal(answer.deleted_at_unix_ms, NOW);
    assert.equal(answer.issues.length, 1);
  });

  it("removes every B2 version of the object through the Worker", async () => {
    const { env, id, code, key } = await setUp();
    const calls: string[] = [];
    let page = 0;
    globalThis.fetch = async (input: RequestInfo | URL, init: RequestInit = {}) => {
      const url = String(input);
      const body = init.body ? JSON.parse(String(init.body)) : {};
      if (url.includes("b2_authorize_account")) {
        return Response.json({ apiUrl: "https://b2.test", authorizationToken: "auth" });
      }
      if (url.includes("b2_list_file_versions")) {
        page += 1;
        calls.push(`list ${body.startFileId ?? ""}`);
        return Response.json(
          page === 1
            ? {
                files: [
                  { fileName: key, fileId: "v2" },
                  { fileName: `${key}.other`, fileId: "x" },
                ],
                nextFileName: key,
                nextFileId: "v1",
              }
            : { files: [{ fileName: key, fileId: "v1" }], nextFileName: null, nextFileId: null },
        );
      }
      if (url.includes("b2_delete_file_version")) {
        calls.push(`delete ${body.fileId}`);
        return body.fileId === "v1"
          ? Response.json({ code: "file_not_present" }, { status: 400 })
          : Response.json({});
      }
      throw new Error(`unexpected fetch ${url}`);
    };
    const res = await worker.fetch(
      post("/lookup/delete", JSON.stringify({ report_id: id, access_code: code })),
      env,
      { waitUntil() {} },
    );
    assert.equal(res.status, 200);
    assert.deepEqual(calls, ["list ", "delete v2", "list v1", "delete v1"]);

    // A refused deletion leaves the report as it was, so the sender can retry.
    const fresh = await setUp();
    globalThis.fetch = async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("b2_authorize_account")) {
        return Response.json({ apiUrl: "https://b2.test", authorizationToken: "auth" });
      }
      if (url.includes("b2_list_file_versions")) {
        return Response.json({ files: [{ fileName: fresh.key, fileId: "v9" }] });
      }
      return Response.json({ code: "unauthorized" }, { status: 401 });
    };
    console.error = () => {};
    const refused = await worker.fetch(
      post("/lookup/delete", JSON.stringify({ report_id: fresh.id, access_code: fresh.code })),
      fresh.env,
      { waitUntil() {} },
    );
    assert.equal(refused.status, 502);
    const row = await first(fresh.env.DASHBOARD_DB!, "SELECT deleted_at FROM report_access");
    assert.equal(row?.deleted_at, null);
  });

  it("says lookup is unavailable without the database", async () => {
    const env: Env = {};
    const page = await lookupFetch(
      new Request("https://collector.test/lookup"),
      env,
      "/lookup",
      makeB2(),
      NOW,
    );
    assert.equal(page?.status, 503);
    const api = await lookupFetch(post("/lookup", "{}"), env, "/lookup", makeB2(), NOW);
    assert.equal(api?.status, 503);
    assert.equal(await lookupFetch(post("/other", "{}"), env, "/other", makeB2(), NOW), null);
  });
});
