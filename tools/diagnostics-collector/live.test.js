// Live dashboard updates: the LiveHub Durable Object with a stand-in for the
// hibernation API, and the Worker paths that feed it. The 101 upgrade itself
// needs the Workers runtime (Node's Response refuses status 101), so these
// tests stop at the hub's refusals and check the accepted path's arguments.

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { all } from "./dashboard/db.js";
import { indexSubmitted } from "./dashboard/sync.js";
import { CHANGED, LiveHub, MAX_SOCKETS } from "./dashboard/live.js";
import { dashboardFetch, dashboardIndexSubmitted } from "./dashboard/routes.js";
import { createSession } from "./dashboard/session.js";
import { createD1 } from "./test-support/d1.js";

const ORIGIN = "https://diagnostics.example.test";
const NOW = Date.UTC(2026, 9, 24, 9, 30, 0);

/** A hibernated socket stand-in that records what it was sent. */
function fakeSocket(expiresAt) {
  return {
    sent: [],
    closed: null,
    deserializeAttachment: () => (expiresAt === undefined ? null : { expiresAt }),
    send(msg) {
      if (this.closed) throw new Error("closed");
      this.sent.push(msg);
    },
    close(code, reason) {
      if (this.closed) throw new Error("already closed");
      this.closed = { code, reason };
    },
  };
}

/** Durable Object state stand-in holding the given sockets. */
function fakeState(sockets) {
  return { getWebSockets: () => sockets.filter((s) => !s.closed) };
}

describe("live hub", () => {
  it("tells current pages that something changed and closes expired ones", async () => {
    const current = fakeSocket(NOW + 60_000);
    const expired = fakeSocket(NOW - 1);
    const unreadable = fakeSocket(undefined);
    const hub = new LiveHub(fakeState([current, expired, unreadable]), {});
    const res = await hub.fetch(new Request("https://live/changed", { method: "POST" }), NOW);
    assert.equal(res.status, 204);
    assert.deepEqual(current.sent, [CHANGED]);
    assert.deepEqual(expired.sent, []);
    assert.equal(expired.closed.code, 1008);
    assert.equal(unreadable.closed.code, 1008);
  });

  it("closes every page when everyone is signed out", async () => {
    const a = fakeSocket(NOW + 60_000);
    const b = fakeSocket(NOW + 60_000);
    const hub = new LiveHub(fakeState([a, b]), {});
    assert.equal((await hub.fetch(new Request("https://live/close-all", { method: "POST" }), NOW)).status, 204);
    assert.equal(a.closed.code, 1008);
    assert.equal(b.closed.code, 1008);
  });

  it("refuses a connect without an upgrade, with a bad expiry, or past the cap", async () => {
    const hub = new LiveHub(fakeState([]), {});
    const upgrade = { headers: { Upgrade: "websocket" } };
    assert.equal((await hub.fetch(new Request(`https://live/connect?expires=${NOW + 1000}`), NOW)).status, 426);
    for (const bad of ["", "abc", String(NOW), "1e400", String(NOW + 0.5)]) {
      assert.equal((await hub.fetch(new Request(`https://live/connect?expires=${bad}`, upgrade), NOW)).status, 400, bad);
    }
    const full = new LiveHub(fakeState(Array.from({ length: MAX_SOCKETS }, () => fakeSocket(NOW + 60_000))), {});
    assert.equal((await full.fetch(new Request(`https://live/connect?expires=${NOW + 1000}`, upgrade), NOW)).status, 503);
    assert.equal((await hub.fetch(new Request("https://live/other", { method: "POST" }), NOW)).status, 404);
  });

  it("ignores page messages and answers a close", () => {
    const ws = fakeSocket(NOW + 60_000);
    const hub = new LiveHub(fakeState([ws]), {});
    hub.webSocketMessage(ws, "anything at all");
    assert.equal(ws.closed, null);
    hub.webSocketClose(ws, 1005);
    assert.equal(ws.closed.code, 1000);
    hub.webSocketClose(ws, 1000); // already closed: no throw
  });
});

/** A `LIVE` binding stand-in that records the hub commands it receives. */
function fakeLive() {
  const calls = [];
  return {
    calls,
    idFromName: (name) => `id:${name}`,
    get: (id) => ({
      fetch: async (url, init = {}) => {
        calls.push({ id, url: String(url), method: init.method || "GET", upgrade: init.headers?.Upgrade ?? null });
        return new Response(null, { status: 204 });
      },
    }),
  };
}

/** Worker env with the dashboard and a fake hub. */
function makeEnv() {
  return {
    DASHBOARD_DB: createD1(),
    LIVE: fakeLive(),
    GITHUB_REPOSITORY: "Bookclerk/bookclerk",
    GITHUB_APP_CLIENT_ID: "Iv23liTestClient",
    GITHUB_APP_CLIENT_SECRET: "test-client-secret",
    GITHUB_APP_PRIVATE_KEY: "unused",
  };
}

/** Calls the dashboard the way worker.js does. */
function call(env, req, b2 = {}) {
  const path = new URL(req.url).pathname.replace(/\/+$/, "") || "/";
  return dashboardFetch(req, env, { waitUntil() {} }, path, b2, NOW);
}

describe("live route", () => {
  it("hands a signed-in, same-origin upgrade to the hub with the session expiry", async () => {
    const env = makeEnv();
    const cookie = (await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role: "member" }, NOW)).split(";")[0];
    const res = await call(env, new Request(`${ORIGIN}/live`, { headers: { cookie, upgrade: "websocket", origin: ORIGIN } }));
    assert.equal(res.status, 204); // the fake hub's answer
    const [c] = env.LIVE.calls;
    assert.equal(c.id, "id:dashboard");
    assert.equal(c.upgrade, "websocket");
    const expires = Number(new URL(c.url).searchParams.get("expires"));
    assert.ok(expires > NOW && expires <= NOW + 8 * 3_600_000);
  });

  it("refuses other origins, plain requests and visitors", async () => {
    const env = makeEnv();
    const cookie = (await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role: "member" }, NOW)).split(";")[0];
    const cross = await call(env, new Request(`${ORIGIN}/live`, { headers: { cookie, upgrade: "websocket", origin: "https://evil.example" } }));
    assert.equal(cross.status, 400);
    const plain = await call(env, new Request(`${ORIGIN}/live`, { headers: { cookie, origin: ORIGIN } }));
    assert.equal(plain.status, 400);
    const visitor = await call(env, new Request(`${ORIGIN}/live`, { headers: { upgrade: "websocket", origin: ORIGIN } }));
    assert.equal(visitor.status, 303);
    assert.equal(env.LIVE.calls.length, 0);
  });

  it("is a 404 path when the dashboard is off", async () => {
    const res = await dashboardFetch(new Request(`${ORIGIN}/live`), { LIVE: fakeLive() }, { waitUntil() {} }, "/live", {}, NOW);
    assert.equal(res, null);
  });

  it("announces a newly indexed report, but not a capped one", async () => {
    const env = makeEnv();
    const enriched = {
      report_id: "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4",
      received_at_unix_ms: NOW,
      payload: { trigger: "manual", version: "0.9.0", os: "linux", arch: "x86_64", events: [] },
    };
    assert.equal(await dashboardIndexSubmitted(env, enriched, "diagnostics/0.9.0/a.json"), "indexed");
    assert.deepEqual(env.LIVE.calls.map((c) => [c.url, c.method]), [["https://live/changed", "POST"]]);
    // Same report again under a cap of zero: nothing new, no push.
    assert.equal(await indexSubmitted(env.DASHBOARD_DB, { ...enriched, report_id: "d2b80f13-0000-4000-8000-000000000002" }, "k", 0), "capped");
    assert.equal((await all(env.DASHBOARD_DB, "SELECT report_id FROM reports")).length, 1);
    assert.equal(env.LIVE.calls.length, 1);
  });

  it("closes live pages when an owner signs everyone out", async () => {
    const env = makeEnv();
    const cookie = (await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role: "admin" }, NOW)).split(";")[0];
    const res = await call(env, new Request(`${ORIGIN}/admin/sign-out-all`, { method: "POST", headers: { cookie, origin: ORIGIN } }));
    assert.equal(res.status, 303);
    assert.deepEqual(env.LIVE.calls.map((c) => c.url), ["https://live/close-all"]);
  });

  it("refreshes a report page without reading storage or logging another view", async () => {
    const env = makeEnv();
    const id = "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4";
    await indexSubmitted(env.DASHBOARD_DB, { report_id: id, received_at_unix_ms: NOW, payload: { events: [] } }, "diagnostics/0.9.0/a.json", Infinity);
    const cookie = (await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role: "member" }, NOW)).split(";")[0];
    const b2 = { authorize: () => assert.fail("storage read on a live refresh") };
    const res = await call(env, new Request(`${ORIGIN}/reports/${id}`, { headers: { cookie, "bookclerk-live": "1" } }), b2);
    assert.equal(res.status, 200);
    assert.match(await res.text(), /data-live="tracked"/);
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM access_log")).length, 0);
  });

  it("keeps working without the binding", async () => {
    const env = { ...makeEnv(), LIVE: undefined };
    const enriched = { report_id: "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4", received_at_unix_ms: NOW, payload: { events: [] } };
    assert.equal(await dashboardIndexSubmitted(env, enriched, "k"), "indexed");
    const res = await call(env, new Request(`${ORIGIN}/live`, {
      headers: {
        cookie: (await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role: "member" }, NOW)).split(";")[0],
        upgrade: "websocket",
        origin: ORIGIN,
      },
    }));
    assert.equal(res.status, 404);
  });
});
