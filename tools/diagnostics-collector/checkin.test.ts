// Opt-in usage check-ins: the install key, the weekly row, the daily cap,
// pruning, the counts, the `POST /checkin` route, and `/submit` swapping an
// install ID for the install key before anything is stored.

import assert from "node:assert/strict";
import { afterEach, describe, it } from "vitest";
import {
  RETENTION_MS,
  checkinDailyRows,
  dayOf,
  installKey,
  monthOf,
  pruneUsage,
  recordCheckin,
  usageStats,
  type Checkin,
} from "./checkin.ts";
import { all, first } from "./dashboard/db.ts";
import { summarizeReport } from "./dashboard/signature.ts";
import type { Env } from "./env.ts";
import { createD1 } from "./test-support/d1.ts";
import worker from "./worker.ts";

const NOW = Date.UTC(2026, 10, 18, 12, 0, 0);
const DAY = 86_400_000;
const ID = "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f";
const OTHER = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
/** Background work is awaited by hand where a test needs it. */
const CTX = { waitUntil(): void {} };
const realFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = realFetch;
});

/**
 * A well-formed check-in.
 *
 * @param overrides - Fields to replace.
 * @returns The check-in.
 */
function checkin(overrides: Partial<Checkin> = {}): Checkin {
  return {
    install_id: ID,
    version: "0.9.0",
    os: "linux",
    arch: "x86_64",
    kind: "daemon",
    ...overrides,
  };
}

/**
 * Posts a check-in body to the Worker.
 *
 * @param env - Bindings.
 * @param body - Request body.
 * @returns The answer.
 */
function post(env: Env, body: string): Promise<Response> {
  return worker.fetch(
    new Request("https://collector.test/checkin", {
      method: "POST",
      body,
      headers: { "content-type": "application/json" },
    }),
    env,
    CTX,
  );
}

describe("install key", () => {
  it("is stable within a month, differs between installs and months, and keeps no ID", async () => {
    const db = createD1();
    const a = await installKey(db, ID, NOW);
    assert.match(a, /^[0-9a-f]{64}$/);
    assert.equal(await installKey(db, ID, NOW + 5 * DAY), a, "same month, same key");
    assert.notEqual(await installKey(db, OTHER, NOW), a, "another install, another key");
    const next = await installKey(db, ID, Date.UTC(2026, 11, 2));
    assert.notEqual(next, a, "a new month salts afresh");
    const salts = await all(db, "SELECT month FROM install_salts ORDER BY month");
    assert.deepEqual(
      salts.map((r) => r.month),
      ["2026-11", "2026-12"],
    );
  });
});

describe("check-in rows", () => {
  it("stores one row per install and week, under the daily cap", async () => {
    const db = createD1();
    assert.equal(await recordCheckin(db, checkin(), NOW, 10), "stored");
    assert.equal(
      await recordCheckin(db, checkin({ version: "0.9.1" }), NOW + 60_000, 10),
      "repeat",
    );
    assert.equal(await recordCheckin(db, checkin({ install_id: OTHER }), NOW, 1), "capped");
    const rows = await all(db, "SELECT * FROM checkins");
    assert.equal(rows.length, 1);
    const row = rows[0] ?? {};
    assert.equal(row.version, "0.9.0", "the week's first check-in is kept");
    assert.equal(row.install_key, await installKey(db, ID, NOW));
    assert.ok(!JSON.stringify(rows).includes(ID), "the install ID is never stored");
  });

  it("counts installs by week, month, kind and version", async () => {
    const db = createD1();
    await recordCheckin(db, checkin(), NOW, 100);
    await recordCheckin(db, checkin({ install_id: OTHER, kind: "desktop" }), NOW - DAY, 100);
    // Earlier this month but more than a week ago: counts for the month only.
    const third = "1b2c3d4e-5f6a-4b7c-8d9e-0f1a2b3c4d5e";
    await recordCheckin(db, checkin({ install_id: third }), NOW - 10 * DAY, 100);
    const stats = await usageStats(db, NOW);
    assert.equal(stats.weekly, 2);
    assert.equal(stats.monthly, 3);
    assert.deepEqual(stats.byKind, [
      { kind: "daemon", n: 1 },
      { kind: "desktop", n: 1 },
    ]);
    assert.deepEqual(stats.byVersion, [{ version: "0.9.0", n: 2 }]);
  });

  it("prunes old rows and every salt but this month's", async () => {
    const db = createD1();
    await recordCheckin(db, checkin(), NOW - RETENTION_MS - 2 * DAY, 100);
    await recordCheckin(db, checkin({ install_id: OTHER }), NOW, 100);
    await pruneUsage(db, NOW);
    const rows = await all(db, "SELECT day FROM checkins");
    assert.deepEqual(
      rows.map((r) => r.day),
      [dayOf(NOW)],
    );
    const salts = await all(db, "SELECT month FROM install_salts");
    assert.deepEqual(
      salts.map((r) => r.month),
      [monthOf(NOW)],
    );
  });

  it("reads the daily cap from its variable", () => {
    assert.equal(checkinDailyRows("12"), 12);
    for (const bad of [undefined, "", "0", "-3", "1.5", "lots"]) {
      assert.equal(checkinDailyRows(bad), 5000, String(bad));
    }
  });
});

describe("POST /checkin", () => {
  it("stores a check-in and answers 204, then 204 again for the same week", async () => {
    const env: Env = { DASHBOARD_DB: createD1() };
    const body = JSON.stringify(checkin());
    assert.equal((await post(env, body)).status, 204);
    assert.equal((await post(env, body)).status, 204);
    const n = await first(env.DASHBOARD_DB as D1Database, "SELECT COUNT(*) AS n FROM checkins");
    assert.equal(Number(n?.n), 1);
  });

  it("refuses a malformed body, a flood past the cap and a missing database", async () => {
    const env: Env = { DASHBOARD_DB: createD1(), CHECKIN_DAILY_ROWS: "1" };
    assert.equal((await post(env, "{}")).status, 400);
    assert.equal((await post(env, JSON.stringify({ ...checkin(), extra: 1 }))).status, 400);
    assert.equal((await post(env, "x".repeat(5000))).status, 413);
    assert.equal((await post(env, JSON.stringify(checkin()))).status, 204);
    assert.equal(
      (await post(env, JSON.stringify(checkin({ install_id: OTHER })))).status,
      429,
      "the daily cap holds",
    );
    assert.equal((await post({}, JSON.stringify(checkin()))).status, 503);
  });
});

describe("POST /submit with an install ID", () => {
  it("stores the install key instead of the ID, in storage and in the index", async () => {
    const db = createD1();
    const env: Env = {
      DASHBOARD_DB: db,
      B2_KEY_ID: "id",
      B2_APPLICATION_KEY: "secret-application-key",
      B2_BUCKET_ID: "bucket",
    };
    let uploaded = "";
    globalThis.fetch = async (input: RequestInfo | URL, init: RequestInit = {}) => {
      const url = String(input);
      if (url.includes("b2_authorize_account")) {
        return Response.json({ apiUrl: "https://b2.test", authorizationToken: "auth" });
      }
      if (url.includes("b2_get_upload_url")) {
        return Response.json({ uploadUrl: "https://b2.test/upload", authorizationToken: "up" });
      }
      if (url === "https://b2.test/upload") {
        uploaded = await new Response(init.body).text();
        return Response.json({ fileId: "f" });
      }
      throw new Error(`unexpected fetch ${url}`);
    };
    const pending: Promise<unknown>[] = [];
    const res = await worker.fetch(
      new Request("https://collector.test/submit", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          version: "0.9.0",
          trigger: "slow_burst",
          os: "linux",
          deployment: "",
          install_id: ID,
          events: [
            {
              ts_unix_ms: NOW,
              level: "WARN",
              target: "bookclerk::slow",
              message: "slow response",
              fields: [
                ["route", "/api/library/works/{id}"],
                ["latency_ms", "2896"],
              ],
            },
          ],
        }),
      }),
      env,
      { waitUntil: (p: Promise<unknown>) => void pending.push(p) },
    );
    assert.equal(res.status, 201);
    await Promise.all(pending);
    assert.ok(uploaded, "the report was uploaded");
    assert.ok(!uploaded.includes(ID), "the install ID never reaches storage");
    const stored: unknown = JSON.parse(uploaded);
    const key = await installKey(db, ID, Date.now());
    assert.equal((stored as { install_key?: string }).install_key, key);
    const summary = await summarizeReport(stored);
    assert.equal(summary.installKey, key);
    assert.equal(summary.route, "/api/library/works/{id}");
    assert.equal(summary.peakLatencyMs, 2896);
    const row = await first(db, "SELECT install_key, route, peak_latency_ms FROM reports");
    assert.deepEqual(
      { ...row },
      { install_key: key, route: "/api/library/works/{id}", peak_latency_ms: 2896 },
    );
  });
});
