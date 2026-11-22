/**
 * The ingress lifecycle and the socket handlers, with the SDK's isolate
 * module replaced by stand-ins (the real one imports `cloudflare:workers`,
 * which only exists inside workerd), fake sockets and a fake Cloudflare.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { mock, test } from "node:test";

import type {
  HostWebSocket,
  OriginBinding,
  SocketConnectOptions,
  SocketsBinding,
} from "@bookclerk/plugin-sdk/workerd";

/** Stand-in for the SDK's `PluginError`. */
class PluginError extends Error {
  /** Wire code. */
  readonly code: string;

  /**
   * Creates an error.
   *
   * @param code - Wire code.
   * @param message - Message.
   */
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }

  /**
   * Builds an error from its wire form, as the SDK does.
   *
   * @param code - Wire code.
   * @param message - Message.
   * @returns The error.
   */
  static fromWire(code: string, message: string): PluginError {
    return new PluginError(code, message);
  }
}

/** Stand-in for the SDK's entrypoint base classes. */
class Entrypoint {
  /** Bindings. */
  readonly env: unknown;

  /**
   * Keeps the bindings, as `WorkerEntrypoint` does.
   *
   * @param _ctx - Execution context (unused).
   * @param env - Bindings.
   */
  constructor(_ctx: unknown, env: unknown) {
    this.env = env;
  }
}

mock.module("@bookclerk/plugin-sdk/workerd", {
  namedExports: { BookclerkEntrypoint: Entrypoint, IngressEntrypoint: Entrypoint, PluginError },
});
const { Ingress, default: Plugin } = await import("../src/index.ts");
const { handleMessage, relayHealth, relaySockets, startRelay, stopRelay, targetOf, topUpSockets } =
  await import("../src/lifecycle.ts");
const { API_BASE, MIGRATION_TAG, relayToken } = await import("../src/cloudflare.ts");
const { RELAY_HEALTH_PATH, RELAY_PULL_PATH, RELAY_PUSH_PATH, RELAY_SOCKETS, encodeFrame } =
  await import("../src/protocol.ts");
const { VERSION } = await import("../src/version.ts");

/** The account in these tests. */
const ACCOUNT = "0123456789abcdef0123456789abcdef";

/** The relay host the fake API deploys to. */
const HOST = "bookclerk-relay.acme.workers.dev";

/** A fake socket the fake binding hands out. */
interface FakeSocket extends HostWebSocket {
  /** Close code, once closed. */
  closedWith: number | null;
}

/**
 * A fake socket binding.
 *
 * @param refuseAfter - Connections refused after this many succeeded.
 * @returns The binding, its sockets and the connect options it saw.
 */
function fakeSockets(refuseAfter = Infinity): {
  binding: SocketsBinding;
  open: FakeSocket[];
  connects: Array<{ url: string; options: SocketConnectOptions | undefined }>;
} {
  const open: FakeSocket[] = [];
  const connects: Array<{ url: string; options: SocketConnectOptions | undefined }> = [];
  /**
   * Makes a socket handle.
   *
   * @param url - Its URL.
   * @param attachment - Its attachment.
   * @returns The handle.
   */
  const socket = (url: string, attachment: unknown): FakeSocket => {
    const ws: FakeSocket = {
      id: `s${open.length}`,
      url,
      protocol: "",
      closedWith: null,
      send: () => Promise.resolve(),
      close: (code?: number) => {
        ws.closedWith = code ?? 1000;
        const index = open.indexOf(ws);
        if (index >= 0) open.splice(index, 1);
        return Promise.resolve();
      },
      serializeAttachment: () => Promise.resolve(),
      deserializeAttachment: () => attachment,
    };
    return ws;
  };
  const binding: SocketsBinding = {
    connect(url, options) {
      connects.push({ url, options });
      if (connects.length > refuseAfter)
        return Promise.reject(new PluginError("limit_exceeded", "full"));
      const ws = socket(url, options?.attachment ?? null);
      open.push(ws);
      return Promise.resolve(ws);
    },
    getWebSockets: () => Promise.resolve([...open]),
  };
  return { binding, open, connects };
}

/**
 * The plugin's bindings.
 *
 * @param sockets - The socket binding.
 * @param origin - The library.
 * @returns The bindings.
 */
function envOf(sockets: SocketsBinding, origin?: OriginBinding): Parameters<typeof targetOf>[0] {
  return {
    CONFIG: { account_id: ACCOUNT, worker_name: "bookclerk-relay" },
    SECRETS: { api_token: "cf-api-token" },
    SOCKETS: sockets,
    ORIGIN: origin ?? { fetch: () => Promise.resolve(new Response("library")) },
  } as unknown as Parameters<typeof targetOf>[0];
}

/**
 * A fake Cloudflare API plus relay health endpoint.
 *
 * @param held - What the relay's health reports, per call.
 * @returns The fetch.
 */
function fakeCloudflare(
  held: () => number | null,
): (input: string, init?: RequestInit) => Promise<Response> {
  const ok = (result: unknown): Response => Response.json({ success: true, errors: [], result });
  return async (input, init) => {
    if (input === `https://${HOST}${RELAY_HEALTH_PATH}`) {
      const n = held();
      return n === null
        ? new Response("down", { status: 502 })
        : Response.json({ sockets: n, waiting: 0 });
    }
    const path = input.slice(API_BASE.length);
    const method = init?.method ?? "GET";
    if (path.endsWith("/workers/scripts") && method === "GET") {
      return ok([{ id: "bookclerk-relay", migration_tag: MIGRATION_TAG }]);
    }
    if (path.endsWith("/workers/subdomain")) return ok({ subdomain: "acme" });
    return ok({});
  };
}

test("start deploys the relay, opens its sockets and returns the origin", async () => {
  const { binding, open, connects } = fakeSockets();
  const started = await startRelay({
    env: envOf(binding),
    fetch: fakeCloudflare(() => RELAY_SOCKETS),
    sleep: () => Promise.resolve(),
    startWaitMs: 1000,
  });
  assert.deepEqual(started, { origin: `https://${HOST}` });
  assert.equal(open.length, RELAY_SOCKETS);
  const first = connects[0];
  assert.ok(first);
  assert.equal(first.url, `wss://${HOST}/__bookclerk/relay/connect`);
  const token = await relayToken({
    accountId: ACCOUNT,
    workerName: "bookclerk-relay",
    apiToken: "cf-api-token",
  });
  assert.deepEqual(first.options?.headers, { authorization: `Bearer ${token}` });
  assert.deepEqual(first.options?.attachment, { host: HOST });
});

test("start waits for the relay, then gives up", async () => {
  const { binding } = fakeSockets();
  let calls = 0;
  const started = await startRelay({
    env: envOf(binding),
    fetch: fakeCloudflare(() => (++calls < 3 ? null : 1)),
    sleep: () => Promise.resolve(),
    startWaitMs: 60_000,
  });
  assert.equal(started.origin, `https://${HOST}`);
  await assert.rejects(
    startRelay({
      env: envOf(fakeSockets().binding),
      fetch: fakeCloudflare(() => 0),
      sleep: () => Promise.resolve(),
      startWaitMs: 0,
    }),
    (err: unknown) => {
      assert.ok(err instanceof PluginError);
      assert.equal(err.code, "unavailable");
      return true;
    },
  );
});

test("a failed deploy is unavailable with Cloudflare's reason", async () => {
  const failing = async (): Promise<Response> =>
    new Response(JSON.stringify({ success: false, errors: [{ message: "nope" }] }), {
      status: 400,
    });
  await assert.rejects(
    startRelay({
      env: envOf(fakeSockets().binding),
      fetch: failing,
      sleep: () => Promise.resolve(),
      startWaitMs: 0,
    }),
    (err: unknown) => {
      assert.ok(err instanceof PluginError);
      assert.equal(err.code, "unavailable");
      assert.match(err.message, /nope/);
      return true;
    },
  );
});

test("missing settings are invalid_params", () => {
  const env = envOf(fakeSockets().binding);
  assert.throws(
    () => targetOf({ ...env, CONFIG: { ...env.CONFIG, account_id: "" } }),
    (err: unknown) => err instanceof PluginError && err.code === "invalid_params",
  );
  assert.throws(
    () => targetOf({ ...env, SECRETS: {} }),
    (err: unknown) => err instanceof PluginError && err.code === "invalid_params",
  );
  assert.equal(
    targetOf({ ...env, CONFIG: { ...env.CONFIG, worker_name: "" } }).workerName,
    "bookclerk-relay",
  );
});

test("topping up closes sockets to another relay and stops when the host refuses", async () => {
  const { binding, open } = fakeSockets(2);
  await binding.connect("wss://old.example.workers.dev/x", {
    attachment: { host: "old.example.workers.dev" },
  });
  const count = await topUpSockets(binding, HOST, "token");
  assert.equal(count, 1);
  assert.deepEqual(
    open.map((ws) => ws.deserializeAttachment()),
    [{ host: HOST }],
  );
});

test("health reports the relay's view and reopens sockets", async () => {
  const { binding, open } = fakeSockets();
  const ctx = (held: number | null) => ({
    env: envOf(binding),
    fetch: fakeCloudflare(() => held),
    sleep: () => Promise.resolve(),
    startWaitMs: 0,
  });
  // With every socket gone, health finds the deployed relay and reconnects.
  assert.deepEqual(await relayHealth(ctx(RELAY_SOCKETS)), { serving: true });
  assert.equal(open.length, RELAY_SOCKETS);
  await stopRelay(binding);
  const offline = await relayHealth({
    ...ctx(1),
    fetch: () => Promise.reject(new Error("offline")),
  });
  assert.equal(offline.serving, false);
  assert.match(offline.detail ?? "", /^not connected to the relay: could not reach/);
  const odd = await relayHealth({
    ...ctx(1),
    fetch: () => Promise.resolve(Response.json({ success: true, result: { subdomain: 7 } })),
  });
  assert.match(odd.detail ?? "", /^not connected to the relay/);
  await binding.connect(`wss://${HOST}/__bookclerk/relay/connect`, { attachment: { host: HOST } });
  assert.deepEqual(await relayHealth(ctx(RELAY_SOCKETS)), { serving: true });
  assert.equal(open.length, RELAY_SOCKETS);
  assert.match((await relayHealth(ctx(null))).detail ?? "", /does not answer/);
  assert.match((await relayHealth(ctx(0))).detail ?? "", /holds none/);
  await stopRelay(binding);
  assert.equal(open.length, 0);
});

test("relaySockets reads only a well-formed count", async () => {
  const answer = (body: unknown) => async (): Promise<Response> => Response.json(body);
  assert.equal(await relaySockets(answer({ sockets: 3 }), HOST, "t"), 3);
  assert.equal(await relaySockets(answer({ sockets: "3" }), HOST, "t"), null);
  assert.equal(await relaySockets(async () => new Response("x"), HOST, "t"), null);
  assert.equal(await relaySockets(() => Promise.reject(new Error("down")), HOST, "t"), null);
});

test("a notice is served through the library; other messages are ignored", async () => {
  const { binding } = fakeSockets();
  const ws = await binding.connect(`wss://${HOST}/c`, { attachment: { host: HOST } });
  const pushed: string[] = [];
  const seen: string[] = [];
  const origin: OriginBinding = {
    fetch(request) {
      seen.push(new URL(request.url).pathname);
      return Promise.resolve(new Response("hi"));
    },
  };
  const relay = async (input: string, init?: RequestInit): Promise<Response> => {
    if (input.includes(RELAY_PULL_PATH))
      return new Response(encodeFrame({ method: "GET", target: "/x", headers: [] }));
    if (input.includes(RELAY_PUSH_PATH)) {
      pushed.push(await new Request(input, init).text());
      return new Response(null, { status: 204 });
    }
    return new Response("?", { status: 404 });
  };
  const env = envOf(binding, origin);
  await handleMessage(env, relay, ws, JSON.stringify({ t: "request", id: "abc-1" }));
  assert.deepEqual(seen, ["/x"]);
  assert.equal(pushed.length, 1);
  assert.match(pushed[0] ?? "", /hi$/);
  await handleMessage(env, relay, ws, new ArrayBuffer(2));
  await handleMessage(env, relay, ws, "{}");
  const orphan = await binding.connect(`wss://${HOST}/c`, { attachment: null });
  await handleMessage(env, relay, orphan, JSON.stringify({ t: "request", id: "abc-2" }));
  assert.equal(seen.length, 1);
});

test("the entrypoint classes delegate to the lifecycle functions", async () => {
  const { binding, open } = fakeSockets();
  const env = envOf(binding);
  const ingress = new Ingress({} as never, env);
  const realFetch = globalThis.fetch;
  globalThis.fetch = () => Promise.reject(new Error("offline"));
  try {
    const health = await ingress.health();
    assert.equal(health.serving, false);
    assert.match(health.detail ?? "", /^not connected to the relay/);
  } finally {
    globalThis.fetch = realFetch;
  }
  const plugin = new Plugin({} as never, env);
  assert.deepEqual(
    await plugin.webSocketMessage(await binding.connect("wss://x/y", { attachment: null }), "x"),
    {},
  );
  const ws = await binding.connect(`wss://${HOST}/c`, { attachment: { host: HOST } });
  const before = open.length;
  // A close the plugin made itself (1000) is not reopened.
  await plugin.webSocketClose(ws, 1000);
  assert.equal(open.length, before);
  await ingress.stop();
  assert.equal(open.length, 0);
});

test("the main module exports only entrypoint classes", async () => {
  // workerd treats each named export of the main module as an entrypoint and
  // refuses one that is not a class or handler.
  const main = await import("../src/index.ts");
  assert.deepEqual(Object.keys(main).sort(), ["Ingress", "default"]);
});

test("the version matches plugin.toml", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  const line = manifest.split("\n").find((l) => l.startsWith("version = "));
  assert.equal(line, `version = "${VERSION}"`);
});
