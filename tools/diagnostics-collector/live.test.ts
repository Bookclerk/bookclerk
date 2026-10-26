// Live dashboard updates: the LiveHub Durable Object with a stand-in for the
// hibernation API, and the Worker paths that feed it. The 101 upgrade itself
// needs the Workers runtime (Node's Response refuses status 101), so these
// tests stop at the hub's refusals and check the accepted path's arguments.

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { all, linkFingerprint, run } from "./dashboard/db.ts";
import { indexSubmitted } from "./dashboard/sync.ts";
import { CHANGED, LiveHub, MAX_SOCKETS } from "./dashboard/live.ts";
import { dashboardFetch, dashboardIndexSubmitted } from "./dashboard/routes.ts";
import { createSession } from "./dashboard/session.ts";
import { makeB2 } from "./test-support/b2.ts";
import { createD1 } from "./test-support/d1.ts";
import type { DashboardEnv, Env } from "./env.ts";
import type { B2Access } from "./dashboard/storage.ts";

const ORIGIN = "https://diagnostics.example.test";
const NOW = Date.UTC(2026, 9, 24, 9, 30, 0);

/**
 * A hibernated socket stand-in that records what it was sent. It has only
 * the members LiveHub touches, so tests hand it over through `asSocket`.
 */
class FakeSocket {
  /** Messages sent, in order. */
  readonly sent: string[] = [];
  /** The close frame, once closed. */
  closed: { code: number; reason: string } | null = null;
  /** Session expiry in the attachment, Unix ms; undefined for none. */
  readonly expiresAt: number | undefined;

  /**
   * @param expiresAt - Session expiry, or undefined for an unreadable
   *   attachment.
   */
  constructor(expiresAt: number | undefined) {
    this.expiresAt = expiresAt;
  }

  /** @returns 1 (open) or 2 (closing). */
  get readyState(): number {
    return this.closed ? 2 : 1;
  }

  /** @returns The attachment, or null. */
  deserializeAttachment(): { expiresAt: number } | null {
    return this.expiresAt === undefined ? null : { expiresAt: this.expiresAt };
  }

  /**
   * @param msg - Message.
   * @throws {Error} When closed.
   */
  send(msg: string): void {
    if (this.closed) throw new Error("closed");
    this.sent.push(msg);
  }

  /**
   * @param code - Close code.
   * @param reason - Close reason.
   * @throws {Error} When already closed.
   */
  close(code: number, reason: string): void {
    if (this.closed) throw new Error("already closed");
    this.closed = { code, reason };
  }
}

/**
 * @param expiresAt - Session expiry, Unix ms.
 * @returns A fresh socket stand-in.
 */
function fakeSocket(expiresAt: number | undefined): FakeSocket {
  return new FakeSocket(expiresAt);
}

/**
 * The close code a socket received. Read through a call so an earlier
 * assertion on `closed` does not narrow it for later reads.
 *
 * @param ws - Socket stand-in.
 * @returns The code, or undefined while open.
 */
function closeCode(ws: FakeSocket): number | undefined {
  return ws.closed?.code;
}

/** Durable Object state stand-in holding the given sockets. */
type FakeState = {
  /** What the hub stored. */
  stored: Map<string, unknown>;
  /** The open sockets. */
  getWebSockets: () => FakeSocket[];
  /** Storage over `stored`. */
  storage: { get: (k: string) => Promise<unknown>; put: (k: string, v: unknown) => Promise<void> };
};

/**
 * @param sockets - Sockets the state holds.
 * @returns A state stand-in.
 */
function fakeState(sockets: FakeSocket[]): FakeState {
  const stored = new Map<string, unknown>();
  return {
    stored,
    getWebSockets: () => sockets.filter((s) => !s.closed),
    storage: { get: async (k) => stored.get(k), put: async (k, v) => void stored.set(k, v) },
  };
}

// The stand-ins implement only the hibernation calls LiveHub makes, so they
// are handed over as the runtime types here and nowhere else.

/**
 * @param state - State stand-in.
 * @returns It, typed as the runtime's state.
 */
function asState(state: FakeState): DurableObjectState {
  return state as unknown as DurableObjectState;
}

/**
 * @param ws - Socket stand-in.
 * @returns It, typed as the runtime's socket.
 */
function asSocket(ws: FakeSocket): WebSocket {
  return ws as unknown as WebSocket;
}

/**
 * @param sockets - Sockets the hub holds.
 * @returns A hub over a state stand-in, with no bindings.
 */
function makeHub(sockets: FakeSocket[]): LiveHub {
  return new LiveHub(asState(fakeState(sockets)), {});
}

describe("live hub", () => {
  it("tells current pages that something changed and closes expired ones", async () => {
    const current = fakeSocket(NOW + 60_000);
    const expired = fakeSocket(NOW - 1);
    const unreadable = fakeSocket(undefined);
    const state = fakeState([current, expired, unreadable]);
    const hub = new LiveHub(asState(state), {});
    const res = await hub.fetch(new Request("https://live/changed", { method: "POST" }), NOW);
    assert.equal(res.status, 204);
    // Remembered so a page connecting just after gets a catch-up refresh.
    assert.equal(state.stored.get("last_changed"), NOW);
    assert.deepEqual(current.sent, [CHANGED]);
    assert.deepEqual(expired.sent, []);
    assert.equal(closeCode(expired), 1008);
    assert.equal(closeCode(unreadable), 1008);
  });

  it("closes every page when everyone is signed out", async () => {
    const a = fakeSocket(NOW + 60_000);
    const b = fakeSocket(NOW + 60_000);
    const hub = makeHub([a, b]);
    assert.equal((await hub.fetch(new Request("https://live/close-all", { method: "POST" }), NOW)).status, 204);
    assert.equal(closeCode(a), 1008);
    assert.equal(closeCode(b), 1008);
  });

  it("refuses a connect without an upgrade, with a bad expiry, or past the cap", async () => {
    const hub = makeHub([]);
    const upgrade = { headers: { Upgrade: "websocket" } };
    assert.equal((await hub.fetch(new Request(`https://live/connect?expires=${NOW + 1000}`), NOW)).status, 426);
    for (const bad of ["", "abc", String(NOW), "1e400", String(NOW + 0.5)]) {
      assert.equal((await hub.fetch(new Request(`https://live/connect?expires=${bad}`, upgrade), NOW)).status, 400, bad);
    }
    const full = makeHub(Array.from({ length: MAX_SOCKETS }, () => fakeSocket(NOW + 60_000)));
    assert.equal((await full.fetch(new Request(`https://live/connect?expires=${NOW + 1000}`, upgrade), NOW)).status, 503);
    assert.equal((await hub.fetch(new Request("https://live/other", { method: "POST" }), NOW)).status, 404);
  });

  it("ignores page messages and answers a close", () => {
    const ws = fakeSocket(NOW + 60_000);
    const hub = makeHub([ws]);
    hub.webSocketMessage(asSocket(ws), "anything at all");
    assert.equal(ws.closed, null);
    hub.webSocketClose(asSocket(ws), 1005);
    assert.equal(closeCode(ws), 1000);
    hub.webSocketClose(asSocket(ws), 1000); // already closed: no throw
  });
});

/** One hub command the fake `LIVE` binding received. */
type HubCall = { id: string; url: string; method: string; upgrade: string | null };

/**
 * A `LIVE` binding stand-in that records the hub commands it receives in
 * `calls`. It implements only `idFromName` and `get(id).fetch`, the calls
 * live.ts makes, so it is handed over as the runtime type.
 *
 * @param calls - Where commands are recorded.
 * @returns The binding.
 */
function fakeLive(calls: HubCall[]): DurableObjectNamespace {
  const binding = {
    idFromName: (name: string) => `id:${name}`,
    get: (id: string) => ({
      fetch: async (url: string, init: RequestInit = {}) => {
        const upgrade = init.headers === undefined ? null : new Headers(init.headers).get("upgrade");
        calls.push({ id, url: String(url), method: init.method || "GET", upgrade });
        return new Response(null, { status: 204 });
      },
    }),
  };
  return binding as unknown as DurableObjectNamespace;
}

/** Worker env with the dashboard and a fake hub, plus the hub's call log. */
type LiveEnv = DashboardEnv & { calls: HubCall[] };

/** @returns Worker env with the dashboard and a fake hub. */
function makeEnv(): LiveEnv {
  const calls: HubCall[] = [];
  return {
    calls,
    DASHBOARD_DB: createD1(),
    LIVE: fakeLive(calls),
    GITHUB_REPOSITORY: "Bookclerk/bookclerk",
    GITHUB_APP_CLIENT_ID: "Iv23liTestClient",
    GITHUB_APP_CLIENT_SECRET: "test-client-secret",
    GITHUB_APP_PRIVATE_KEY: "unused",
  };
}

/**
 * Calls the dashboard the way worker.ts does.
 *
 * @param env - Bindings.
 * @param req - Request.
 * @param b2 - Storage stand-in.
 * @returns The dashboard's answer, or null for a path it does not serve.
 */
async function call(env: Env, req: Request, b2: B2Access = makeB2()): Promise<Response> {
  const path = new URL(req.url).pathname.replace(/\/+$/, "") || "/";
  const res = await dashboardFetch(req, env, { waitUntil() {} }, path, b2, NOW);
  assert.ok(res, `the dashboard serves ${path}`);
  return res;
}

/**
 * The Cookie header of a fresh member session.
 *
 * @param env - Bindings.
 * @param role - Member role.
 * @returns `name=value`.
 */
async function signedIn(env: DashboardEnv, role: "admin" | "member" = "member"): Promise<string> {
  return (await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role }, NOW)).split(";")[0] ?? "";
}

describe("link fingerprint", () => {
  it("changes with a pull request's title, even at the same length", async () => {
    const db = createD1();
    await run(db, "INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (400, 9, 'Fix A', 'open', NULL)");
    const before = await linkFingerprint(db);
    assert.equal(await linkFingerprint(db), before);
    await run(db, "UPDATE issue_pr SET title = 'Fix B'");
    assert.notEqual(await linkFingerprint(db), before);
  });
});

describe("live route", () => {
  it("hands a signed-in, same-origin upgrade to the hub with the session expiry", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env);
    const res = await call(env, new Request(`${ORIGIN}/live`, { headers: { cookie, upgrade: "websocket", origin: ORIGIN } }));
    assert.equal(res.status, 204); // the fake hub's answer
    const [c] = env.calls;
    assert.ok(c !== undefined);
    assert.equal(c.id, "id:dashboard");
    assert.equal(c.upgrade, "websocket");
    const expires = Number(new URL(c.url).searchParams.get("expires"));
    assert.ok(expires > NOW && expires <= NOW + 8 * 3_600_000);
  });

  it("refuses other origins, plain requests and visitors", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env);
    const cross = await call(env, new Request(`${ORIGIN}/live`, { headers: { cookie, upgrade: "websocket", origin: "https://evil.example" } }));
    assert.equal(cross.status, 400);
    const plain = await call(env, new Request(`${ORIGIN}/live`, { headers: { cookie, origin: ORIGIN } }));
    assert.equal(plain.status, 400);
    const visitor = await call(env, new Request(`${ORIGIN}/live`, { headers: { upgrade: "websocket", origin: ORIGIN } }));
    assert.equal(visitor.status, 303);
    assert.equal(env.calls.length, 0);
  });

  it("is a 404 path when the dashboard is off", async () => {
    const res = await dashboardFetch(new Request(`${ORIGIN}/live`), { LIVE: fakeLive([]) }, { waitUntil() {} }, "/live", makeB2(), NOW);
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
    assert.deepEqual(env.calls.map((c) => [c.url, c.method]), [["https://live/changed", "POST"]]);
    // Same report again under a cap of zero: nothing new, no push.
    assert.equal(await indexSubmitted(env.DASHBOARD_DB, { ...enriched, report_id: "d2b80f13-0000-4000-8000-000000000002" }, "k", 0), "capped");
    assert.equal((await all(env.DASHBOARD_DB, "SELECT report_id FROM reports")).length, 1);
    assert.equal(env.calls.length, 1);
  });

  it("closes live pages when an owner signs everyone out", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env, "admin");
    const res = await call(env, new Request(`${ORIGIN}/admin/sign-out-all`, { method: "POST", headers: { cookie, origin: ORIGIN } }));
    assert.equal(res.status, 303);
    assert.deepEqual(env.calls.map((c) => c.url), ["https://live/close-all"]);
  });

  it("refreshes a report page without reading storage or logging the view again", async () => {
    const env = makeEnv();
    const id = "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4";
    await indexSubmitted(env.DASHBOARD_DB, { report_id: id, received_at_unix_ms: NOW, payload: { events: [] } }, "diagnostics/0.9.0/a.json", Infinity);
    const cookie = await signedIn(env);
    const views = async () => (await all(env.DASHBOARD_DB, "SELECT * FROM access_log WHERE action = 'view-report'")).length;
    // The header cannot hide a first view: it is read and logged as usual.
    let reads = 0;
    const b2: B2Access = { ...makeB2(), authorize: async () => { reads += 1; throw new Error("offline"); } };
    const live = { headers: { cookie, "bookclerk-live": "1" } };
    assert.equal((await call(env, new Request(`${ORIGIN}/reports/${id}`, live), b2)).status, 200);
    assert.equal(await views(), 1);
    assert.equal(reads, 1);
    // Later live refreshes skip storage and the log.
    const res = await call(env, new Request(`${ORIGIN}/reports/${id}`, live), b2);
    assert.equal(res.status, 200);
    assert.match(await res.text(), /data-live="tracked"/);
    assert.equal(await views(), 1);
    assert.equal(reads, 1);
  });

  it("keeps working without the binding", async () => {
    const { LIVE: _, ...env } = makeEnv();
    const enriched = { report_id: "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4", received_at_unix_ms: NOW, payload: { events: [] } };
    assert.equal(await dashboardIndexSubmitted(env, enriched, "k"), "indexed");
    const res = await call(env, new Request(`${ORIGIN}/live`, {
      headers: {
        cookie: await signedIn(env),
        upgrade: "websocket",
        origin: ORIGIN,
      },
    }));
    assert.equal(res.status, 404);
  });
});
