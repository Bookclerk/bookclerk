// The demo Worker's request handling under Node: launch and admission,
// Turnstile, the route check in front of the container, cookie handling,
// the banner, sign-in limits and the sweep. The session class is replaced
// by a fake that records what the Worker asks of it.

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { type Deps, handle, sweep, withBanner } from "../src/app.ts";
import type { Env, SessionApi } from "../src/env.ts";
import { finish } from "../src/limits.ts";
import { createD1 } from "../test-support/d1.ts";

const ORIGIN = "https://demo.example.test";

/** A fake session object: state machine plus a scripted container. */
class FakeStub implements SessionApi {
  /** Where the session is. */
  state = "none";
  /** Arguments of open(). */
  opened: Parameters<SessionApi["open"]>[0] | null = null;
  /** Requests forwarded to the container. */
  forwarded: Request[] = [];
  /** Reasons end() was called with. */
  ended: string[] = [];
  /** The container's next answer. */
  reply: (request: Request) => Response = () => new Response("ok", { headers: { "Content-Type": "text/plain" } });

  /** @param p - Session id, hard stop, version and origin. */
  async open(p: Parameters<SessionApi["open"]>[0]): Promise<void> {
    this.opened = p;
    this.state = "starting";
  }

  /** @returns State, hard stop and version. */
  async status(): Promise<{ state: string; endsAt: number; version: string }> {
    return { state: this.state, endsAt: this.opened?.endsAt ?? 0, version: this.opened?.version ?? "" };
  }

  /** @returns A portal session once ready. */
  async portalSession(): Promise<string | null> {
    return this.state === "ready" ? "p".repeat(64) : null;
  }

  /**
   * @param reason - Why the session ends.
   * @returns Zero sign-outs.
   */
  async end(reason: string): Promise<{ revoked: number; failed: number }> {
    this.ended.push(reason);
    this.state = "ended";
    return { revoked: 0, failed: 0 };
  }

  /**
   * @param request - Forwarded request.
   * @returns The scripted answer, or 503 when not ready.
   */
  async fetch(request: Request): Promise<Response> {
    if (this.state !== "ready") {
      return Response.json({}, { status: 503, headers: { "X-Demo-State": this.state } });
    }
    this.forwarded.push(request);
    return this.reply(request);
  }
}

/**
 * A Durable Object id that carries its name, as `idFromName` gives one.
 *
 * @param name - Object name.
 * @returns The id.
 */
function fakeId(name: string): DurableObjectId {
  return { name, toString: () => name, equals: (other) => other.name === name };
}

/** The test env: the Worker's bindings plus the fake stubs by session id. */
type TestEnv = Env & { stubs: Map<string, FakeStub> };

/**
 * A Worker env with a D1 stand-in, fake sessions and Turnstile keys.
 *
 * @param vars - Extra vars.
 * @returns The env.
 */
function makeEnv(vars: Partial<Env> = {}): TestEnv {
  const stubs = new Map<string, FakeStub>();
  return {
    DB: createD1(),
    SESSIONS: {
      idFromName: fakeId,
      get: (id) => {
        const name = id.toString();
        let stub = stubs.get(name);
        if (!stub) {
          stub = new FakeStub();
          stubs.set(name, stub);
        }
        return stub;
      },
    },
    SESSION_KEY: "k".repeat(64),
    TURNSTILE_SITE_KEY: "0x4AAAAAAA-site",
    TURNSTILE_SECRET: "0x4AAAAAAA-secret",
    DEMO_VERSION: "abc1234",
    SITE_URL: "https://bookclerk.example.test",
    stubs,
    ...vars,
  };
}

/** Test deps: the Worker's, plus the Siteverify calls made and a clock to move. */
type TestDeps = Deps & {
  /** Siteverify calls, with their JSON bodies. */
  calls: Array<{ url: string; body: Record<string, unknown> }>;
  /** Moves the clock forward. */
  advance: (ms: number) => number;
};

/**
 * Deps whose Siteverify answers `success` for this host and action.
 *
 * @param overrides - Fields to change in the Siteverify answer.
 * @returns The deps.
 */
function makeDeps(overrides: object = {}): TestDeps {
  const calls: TestDeps["calls"] = [];
  let now = Date.UTC(2026, 9, 24, 12, 0, 0);
  return {
    calls,
    advance: (ms) => (now += ms),
    now: () => now,
    fetch: async (url, init) => {
      calls.push({ url, body: JSON.parse(String(init.body)) as Record<string, unknown> });
      return Response.json({ success: true, hostname: "demo.example.test", action: "demo_start", ...overrides });
    },
  };
}

/**
 * Starts a session through the launch form.
 *
 * @returns The response and the
 *   `Cookie` header the browser would send next.
 */
async function startSession(env: TestEnv, deps: TestDeps, ip = "203.0.113.7"): Promise<{ res: Response; cookie: string; }> {
  const res = await handle(
    new Request(`${ORIGIN}/_demo/start`, {
      method: "POST",
      headers: {
        Origin: ORIGIN,
        "Content-Type": "application/x-www-form-urlencoded",
        "CF-Connecting-IP": ip,
      },
      body: "cf-turnstile-response=token123",
    }),
    env,
    deps,
  );
  const set = res.headers.getSetCookie()[0] ?? "";
  return { res, cookie: set.split(";")[0] ?? "" };
}

/**
 * The only fake stub, after one session started.
 *
 * @param env - Test env.
 * @returns The stub.
 */
function onlyStub(env: TestEnv): FakeStub {
  const stubs = [...env.stubs.values()];
  assert.equal(stubs.length, 1);
  return stubs[0]!;
}

describe("launch and admission", () => {
  it("shows the launch page with the Turnstile widget and a strict CSP", async () => {
    const res = await handle(new Request(`${ORIGIN}/`, { headers: { Accept: "text/html" } }), makeEnv(), makeDeps());
    assert.equal(res.status, 200);
    const html = await res.text();
    assert.match(html, /data-sitekey="0x4AAAAAAA-site"/);
    assert.match(html, /data-action="demo_start"/);
    assert.match(res.headers.get("Content-Security-Policy") ?? "", /script-src 'self' https:\/\/challenges\.cloudflare\.com/);
  });

  it("refuses to start when the Worker is not set up", async () => {
    const env = makeEnv({ TURNSTILE_SECRET: "" });
    const { res } = await startSession(env, makeDeps());
    assert.equal(res.status, 503);
    assert.equal(env.stubs.size, 0);
  });

  it("starts a session after Turnstile passes", async () => {
    const env = makeEnv();
    const deps = makeDeps();
    const { res, cookie } = await startSession(env, deps);
    assert.equal(res.status, 303);
    assert.equal(res.headers.get("Location"), "/_demo/wait");
    const set = res.headers.getSetCookie()[0] ?? "";
    assert.match(set, /^__Host-demo=[A-Za-z0-9_-]{22}; Path=\/; HttpOnly; Secure; SameSite=Lax; Max-Age=1800$/);
    assert.equal(deps.calls[0]!.body.response, "token123");
    assert.equal(deps.calls[0]!.body.remoteip, "203.0.113.7");
    const stub = onlyStub(env);
    assert.equal(stub.opened!.version, "abc1234");
    const row = await env.DB.prepare("SELECT ends_at - started_at AS len FROM sessions").first();
    assert.equal(row!.len, 1800);
    assert.ok(cookie.startsWith("__Host-demo="));
  });

  it("refuses a failed, foreign or replayed Turnstile answer", async () => {
    for (const bad of [{ success: false }, { hostname: "evil.test" }, { action: "other" }]) {
      const env = makeEnv();
      const { res } = await startSession(env, makeDeps(bad));
      assert.equal(res.status, 403, JSON.stringify(bad));
      assert.equal(env.stubs.size, 0);
    }
  });

  it("refuses a start posted from another site", async () => {
    const env = makeEnv();
    const res = await handle(
      new Request(`${ORIGIN}/_demo/start`, {
        method: "POST",
        headers: { Origin: "https://evil.test", "Content-Type": "application/x-www-form-urlencoded" },
        body: "cf-turnstile-response=t",
      }),
      env,
      makeDeps(),
    );
    assert.equal(res.status, 403);
  });

  it("limits sessions per address per hour", async () => {
    const env = makeEnv();
    const deps = makeDeps();
    for (let i = 0; i < 3; i++) assert.equal((await startSession(env, deps)).res.status, 303);
    const { res } = await startSession(env, deps);
    assert.equal(res.status, 429);
    assert.match(await res.text(), /several demos/);
    assert.equal((await startSession(env, deps, "198.51.100.1")).res.status, 303);
  });

  it("says the demo is busy at the live-session cap", async () => {
    const env = makeEnv({ MAX_LIVE_SESSIONS: "2" });
    const deps = makeDeps();
    await startSession(env, deps, "198.51.100.1");
    await startSession(env, deps, "198.51.100.2");
    const { res } = await startSession(env, deps, "198.51.100.3");
    assert.equal(res.status, 429);
    assert.match(await res.text(), /busy/);
  });

  it("stops at the daily budget and gives unused time back", async () => {
    const env = makeEnv({ DAILY_BUDGET_HOURS: "1", MAX_LIVE_SESSIONS: "9" });
    const deps = makeDeps();
    const first = await startSession(env, deps, "198.51.100.1");
    await startSession(env, deps, "198.51.100.2");
    const third = await startSession(env, deps, "198.51.100.3");
    assert.match(await third.res.text(), /limit for today/);
    // Ten minutes in, twenty come back: still short of a new half hour.
    deps.advance(600_000);
    const end = await handle(
      new Request(`${ORIGIN}/_demo/end`, { method: "POST", headers: { Origin: ORIGIN, Cookie: first.cookie } }),
      env,
      deps,
    );
    assert.equal(end.status, 303);
    const id = [...env.stubs.keys()][0]!;
    assert.equal(await finish(env.DB, { id, now: Math.floor(deps.now() / 1000), reason: "visitor", revoked: 0, failed: 0 }), true);
    assert.equal((await startSession(env, deps, "198.51.100.4")).res.status, 429);
    // The second leaves at once: its whole half hour comes back.
    const second = [...env.stubs.keys()][1]!;
    assert.equal(await finish(env.DB, { id: second, now: Math.floor(deps.now() / 1000) - 600, reason: "visitor", revoked: 0, failed: 0 }), true);
    assert.equal((await startSession(env, deps, "198.51.100.5")).res.status, 303);
  });
});

describe("a running session", () => {
  /** Starts a session and marks it ready. */
  async function ready() {
    const env = makeEnv();
    const deps = makeDeps();
    const { cookie } = await startSession(env, deps);
    const stub = onlyStub(env);
    stub.state = "ready";
    return { env, deps, cookie, stub };
  }

  it("enters the app with the visitor's portal session", async () => {
    const { env, deps, cookie } = await ready();
    const res = await handle(new Request(`${ORIGIN}/_demo/enter`, { headers: { Cookie: cookie } }), env, deps);
    assert.equal(res.status, 303);
    assert.equal(res.headers.get("Location"), "/");
    assert.match(res.headers.getSetCookie()[0] ?? "", /^bookclerk_portal_session=p{64}; Path=\/; HttpOnly; Secure; SameSite=Lax$/);
  });

  it("refuses acquire without contacting the container", async () => {
    const { env, deps, cookie, stub } = await ready();
    const res = await handle(
      new Request(`${ORIGIN}/api/library/acquire`, {
        method: "POST",
        headers: { Cookie: cookie, Origin: ORIGIN, "Content-Type": "application/json" },
        body: "{}",
      }),
      env,
      deps,
    );
    assert.equal(res.status, 403);
    assert.equal(stub.forwarded.length, 0);
  });

  it("forwards allowed calls with their origin but without the Worker's cookie, auth or IP headers", async () => {
    const { env, deps, cookie, stub } = await ready();
    const res = await handle(
      new Request(`${ORIGIN}/api/library/scan`, {
        method: "POST",
        headers: {
          Cookie: `${cookie}; bookclerk_portal_session=${"p".repeat(64)}; __Secure-x=1`,
          Origin: ORIGIN,
          Authorization: "Bearer visitor-guess",
          Forwarded: "for=127.0.0.1",
          "X-Real-IP": "127.0.0.1",
          "Content-Type": "application/json",
        },
        body: "{}",
      }),
      env,
      deps,
    );
    assert.equal(res.status, 200);
    const sent = stub.forwarded[0];
    assert.equal(sent!.headers.get("Cookie"), `bookclerk_portal_session=${"p".repeat(64)}`);
    assert.equal(sent!.headers.get("Authorization"), null);
    assert.equal(sent!.headers.get("Origin"), ORIGIN);
    assert.equal(onlyStub(env).opened!.origin, ORIGIN);
    assert.equal(sent!.headers.get("Forwarded"), null);
    assert.equal(sent!.headers.get("X-Real-IP"), null);
    assert.equal(res.headers.get("X-Frame-Options"), "DENY");
  });

  it("refuses cross-site writes the browser would carry cookies for", async () => {
    const { env, deps, cookie, stub } = await ready();
    const res = await handle(
      new Request(`${ORIGIN}/api/library/scan`, { method: "POST", headers: { Cookie: cookie }, body: "{}" }),
      env,
      deps,
    );
    assert.equal(res.status, 403);
    assert.equal(stub.forwarded.length, 0);
  });

  it("makes container cookies Secure and drops ones naming the Worker's", async () => {
    const { env, deps, cookie, stub } = await ready();
    stub.reply = () => {
      const h = new Headers({ "Content-Type": "application/json" });
      h.append("Set-Cookie", "bookclerk_session=abc; Path=/; HttpOnly; Domain=evil.test");
      h.append("Set-Cookie", "__Host-demo=stolen; Path=/");
      return new Response("{}", { headers: h });
    };
    const res = await handle(new Request(`${ORIGIN}/api/status`, { headers: { Cookie: cookie } }), env, deps);
    assert.deepEqual(res.headers.getSetCookie(), ["bookclerk_session=abc; Path=/; HttpOnly; Secure"]);
  });

  it("adds the banner to the app's HTML", async () => {
    const { env, deps, cookie, stub } = await ready();
    stub.reply = () =>
      new Response("<html><head><title>Bookclerk</title></head><body></body></html>", {
        headers: { "Content-Type": "text/html; charset=utf-8" },
      });
    const res = await handle(new Request(`${ORIGIN}/library`, { headers: { Cookie: cookie, Accept: "text/html" } }), env, deps);
    const html = await res.text();
    assert.match(html, /<script src="\/_demo\/banner\.js" defer><\/script><\/head>/);
  });

  it("limits store sign-in attempts per session", async () => {
    const { env, deps, cookie, stub } = await ready();
    const attempt = () =>
      handle(
        new Request(`${ORIGIN}/api/portal/libro/login`, {
          method: "POST",
          headers: { Cookie: cookie, Origin: ORIGIN, "Content-Type": "application/json" },
          body: "{}",
        }),
        env,
        deps,
      );
    for (let i = 0; i < 5; i++) assert.equal((await attempt()).status, 200);
    assert.equal((await attempt()).status, 429);
    assert.equal(stub.forwarded.length, 5);
  });

  it("sends a browser to the waiting page while the copy starts", async () => {
    const { env, deps, cookie, stub } = await ready();
    stub.state = "starting";
    const res = await handle(new Request(`${ORIGIN}/`, { headers: { Cookie: cookie, Accept: "text/html" } }), env, deps);
    assert.equal(res.status, 303);
    assert.equal(res.headers.get("Location"), "/_demo/wait");
  });

  it("ends on Start over and clears the cookies", async () => {
    const { env, deps, cookie, stub } = await ready();
    const res = await handle(
      new Request(`${ORIGIN}/_demo/end`, { method: "POST", headers: { Cookie: cookie, Origin: ORIGIN } }),
      env,
      deps,
    );
    assert.equal(res.status, 303);
    assert.deepEqual(stub.ended, ["visitor"]);
    assert.ok(res.headers.getSetCookie().every((c) => c.includes("Max-Age=0")));
    const after = await handle(new Request(`${ORIGIN}/library`, { headers: { Cookie: cookie, Accept: "text/html" } }), env, deps);
    assert.equal(after.status, 410);
    assert.ok(after.headers.getSetCookie().some((c) => c.startsWith("__Host-demo=;")));
    assert.match(await after.text(), /Your demo has ended/);
  });

  it("keeps the session when bookclerkd itself answers 410", async () => {
    const { env, deps, cookie, stub } = await ready();
    stub.reply = () => Response.json({ error: "invite expired" }, { status: 410 });
    const res = await handle(new Request(`${ORIGIN}/invite`, { headers: { Cookie: cookie } }), env, deps);
    assert.equal(res.status, 410);
    assert.deepEqual(res.headers.getSetCookie(), []);
  });

  it("ends the running session when the visitor starts again", async () => {
    const { env, deps, cookie, stub } = await ready();
    const again = await handle(
      new Request(`${ORIGIN}/_demo/start`, {
        method: "POST",
        headers: {
          Origin: ORIGIN,
          Cookie: cookie,
          "Content-Type": "application/x-www-form-urlencoded",
          "CF-Connecting-IP": "203.0.113.7",
        },
        body: "cf-turnstile-response=token123",
      }),
      env,
      deps,
    );
    assert.equal(again.status, 303);
    assert.deepEqual(stub.ended, ["restart"]);
    assert.equal(env.stubs.size, 2);
  });

  it("refuses an oversized launch form without reading all of it", async () => {
    const env = makeEnv();
    const deps = makeDeps();
    let pulled = 0;
    const body = new ReadableStream({
      pull(controller) {
        pulled++;
        controller.enqueue(new Uint8Array(8192).fill(97));
        if (pulled > 1000) controller.close();
      },
    });
    // Node needs `duplex` for a streamed body; the Workers RequestInit type
    // does not list it, so the options are not a checked literal.
    const init = {
      method: "POST",
      headers: { Origin: ORIGIN, "Content-Type": "application/x-www-form-urlencoded" },
      body,
      duplex: "half",
    };
    const res = await handle(new Request(`${ORIGIN}/_demo/start`, init), env, deps);
    assert.equal(res.status, 403);
    assert.ok(pulled < 10, `read ${pulled} chunks`);
    assert.equal(deps.calls.length, 0);
  });

  it("streams HTML too big for the banner unchanged", async () => {
    const { env, deps, cookie, stub } = await ready();
    const big = "<html><head></head><body>" + "x".repeat(1024 * 1024 + 10) + "</body></html>";
    stub.reply = () => new Response(big, { headers: { "Content-Type": "text/html", "Content-Length": String(big.length) } });
    const res = await handle(new Request(`${ORIGIN}/library`, { headers: { Cookie: cookie, Accept: "text/html" } }), env, deps);
    assert.equal(res.headers.get("Content-Length"), null);
    assert.equal(await res.text(), big);
  });

  it("refuses Start over from another site", async () => {
    const { env, deps, cookie, stub } = await ready();
    const res = await handle(
      new Request(`${ORIGIN}/_demo/end`, { method: "POST", headers: { Cookie: cookie, Origin: "https://evil.test" } }),
      env,
      deps,
    );
    assert.equal(res.status, 403);
    assert.deepEqual(stub.ended, []);
  });
});

describe("sweep", () => {
  it("ends sessions past their hard stop and closes their rows", async () => {
    const env = makeEnv();
    const deps = makeDeps();
    await startSession(env, deps);
    deps.advance(1800_000 + 121_000);
    assert.equal(await sweep(env, deps), 1);
    assert.deepEqual(onlyStub(env).ended, ["sweep"]);
    const row = await env.DB.prepare("SELECT end_reason FROM sessions").first();
    assert.equal(row!.end_reason, "sweep");
    assert.equal(await sweep(env, deps), 0);
  });

  it("keeps a session live while its object fails to end, then gives up", async () => {
    const env = makeEnv();
    const deps = makeDeps();
    await startSession(env, deps);
    const stub = onlyStub(env);
    stub.end = async () => {
      throw new Error("object unavailable");
    };
    const live = () => env.DB.prepare("SELECT ended_at, revoke_failed FROM sessions").first();
    deps.advance(1800_000 + 121_000);
    assert.equal(await sweep(env, deps), 1);
    assert.equal((await live())!.ended_at, null);
    deps.advance(1800_000);
    assert.equal(await sweep(env, deps), 1);
    assert.equal((await live())!.ended_at, null);
    deps.advance(1800_000);
    assert.equal(await sweep(env, deps), 1);
    const row = await live();
    assert.notEqual(row!.ended_at, null);
    assert.equal(row!.revoke_failed, 1);
    assert.equal(await sweep(env, deps), 0);
  });
});

describe("withBanner", () => {
  it("inserts before </head>, or at the start without one", () => {
    assert.match(withBanner("<HEAD></HEAD >x"), /^<HEAD><link[^>]+><script[^>]+><\/script><\/HEAD >x$/);
    assert.match(withBanner("plain"), /^<link[^>]+><script[^>]+><\/script>plain$/);
  });
});
