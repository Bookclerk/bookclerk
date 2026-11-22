/**
 * Live connections through the relay hub and the plugin's upgrade handling:
 * fake plugin sockets carry notices to `serveRequest`, a fake `ORIGIN`
 * accepts or refuses upgrades, and fake client sockets stand in for the
 * Worker's.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import type {
  HostWebSocket,
  OriginBinding,
  OriginConnectOptions,
} from "@bookclerk/plugin-sdk/workerd";
import {
  RelayHub,
  framed,
  type LiveClient,
  type LiveClients,
  type RelaySocket,
} from "../src/relay-core.ts";
import {
  MAX_CLOSE_REASON_BYTES,
  MAX_LIVE_MESSAGE_BYTES,
  RELAY_PULL_PATH,
  RELAY_PUSH_PATH,
  RELAY_SOCKET_HEADER,
  encodeFrame,
  encodeLiveFrame,
  isSendableCloseCode,
  messageFrame,
  parseLiveFrame,
  parseNotice,
  sendableCloseCode,
  sendableCloseReason,
} from "../src/protocol.ts";
import { serveRequest, type Fetch, type ServeOutcome } from "../src/serve.ts";

/** The relay's token in these tests. */
const TOKEN = "relay-token-0123456789abcdef";

/** The relay's host in these tests. */
const HOST = "bookclerk-relay.example.workers.dev";

/** Bearer header for relay paths. */
const AUTH = { authorization: `Bearer ${TOKEN}` };

/** A fake client socket. */
interface FakeClient extends LiveClient {
  /** Messages the client received. */
  got: Array<string | number[]>;
  /** Close code and reason, once closed. */
  closed: [number, string] | null;
}

/**
 * A fake client socket.
 *
 * @returns The socket.
 */
function fakeClient(): FakeClient {
  const client: FakeClient = {
    got: [],
    closed: null,
    send(message) {
      if (client.closed) throw new Error("closed");
      client.got.push(typeof message === "string" ? message : [...message]);
    },
    close(code, reason) {
      client.closed = [code, reason];
    },
  };
  return client;
}

/** The fake Worker's client sockets and what it accepted. */
interface FakeClients extends LiveClients<RelaySocket> {
  /** Client sockets by connection id. */
  byId: Map<string, FakeClient>;
  /** Each accept: id, plugin socket, local id, subprotocol. */
  accepted: Array<{ id: string; via: RelaySocket; local: string; protocol: string }>;
}

/**
 * The fake Worker's client sockets.
 *
 * @returns Them.
 */
function fakeClients(): FakeClients {
  const byId = new Map<string, FakeClient>();
  const accepted: FakeClients["accepted"] = [];
  return {
    byId,
    accepted,
    accept(id, via, local, protocol) {
      accepted.push({ id, via, local, protocol });
      const client = fakeClient();
      byId.set(id, client);
      // Node's Response refuses 101; the Worker answers 101 with the socket.
      return {
        response: new Response("upgraded", { headers: { "x-protocol": protocol } }),
        client,
      };
    },
    find(id) {
      return byId.get(id) ?? null;
    },
  };
}

/** A fake socket to the library that `connect` hands out. */
interface FakeLocal extends HostWebSocket {
  /** Close code, once closed. */
  closedWith: number | null;
}

/**
 * A fake socket to the library.
 *
 * @param id - Its id.
 * @param protocol - Its subprotocol.
 * @returns The socket.
 */
function fakeLocal(id: string, protocol: string): FakeLocal {
  const ws: FakeLocal = {
    id,
    url: `https://${HOST}/socket`,
    protocol,
    closedWith: null,
    send: () => Promise.resolve(),
    close: (code?: number) => {
      ws.closedWith = code ?? 1000;
      return Promise.resolve();
    },
    serializeAttachment: () => Promise.resolve(),
    deserializeAttachment: () => null,
  };
  return ws;
}

/** What the fake library saw of each upgrade. */
interface Upgrade {
  /** Method. */
  method: string;
  /** Path and query. */
  target: string;
  /** Request headers. */
  headers: Headers;
  /** Options the plugin passed. */
  options: OriginConnectOptions | undefined;
}

/**
 * A fake library whose `connect` answers with `answer`.
 *
 * @param seen - Collects each upgrade.
 * @param answer - The socket or refusal, or a throw.
 * @returns The binding.
 */
function library(
  seen: Upgrade[],
  answer: (
    options: OriginConnectOptions | undefined,
  ) => Promise<{ socket: HostWebSocket } | { response: Response }>,
): OriginBinding {
  return {
    fetch: () => Promise.reject(new Error("no plain requests here")),
    async connect(request, options) {
      const url = new URL(request.url);
      seen.push({
        method: request.method,
        target: `${url.pathname}${url.search}`,
        headers: request.headers,
        options,
      });
      return answer(options);
    },
  };
}

/**
 * A plugin socket: each notice is served as the plugin's handler does;
 * each binary message is recorded.
 *
 * @param hub - The hub.
 * @param origin - The library.
 * @param outcomes - Collects each outcome.
 * @param frames - Collects each binary message.
 * @param fetch - The plugin's request function; straight to the hub by default.
 * @returns The socket.
 */
function pluginSocket(
  hub: RelayHub,
  origin: OriginBinding,
  outcomes: ServeOutcome[],
  frames: Uint8Array[] = [],
  fetch?: Fetch,
): RelaySocket {
  const toHub: Fetch = fetch ?? ((input, init) => hub.fetch(new Request(input, init)));
  return {
    send(message) {
      if (typeof message !== "string") {
        frames.push(message);
        return;
      }
      const notice = parseNotice(message);
      if (typeof notice === "string") return;
      void serveRequest(
        { fetch: toHub, origin, host: HOST, token: TOKEN, via: "r0" },
        notice.id,
      ).then((outcome) => outcomes.push(outcome));
    },
  };
}

/**
 * A public WebSocket upgrade.
 *
 * @param init - Extra headers and method.
 * @returns The request.
 */
function upgrade(init: { method?: string; headers?: Record<string, string> } = {}): Request {
  return new Request("https://lib.example/api/node/v1/socket?x=1", {
    method: init.method ?? "GET",
    headers: {
      upgrade: "websocket",
      connection: "Upgrade",
      "sec-websocket-key": "dGhlIHNhbXBsZSBub25jZQ==",
      "sec-websocket-version": "13",
      "cf-connecting-ip": "203.0.113.9",
      ...init.headers,
    },
  });
}

/**
 * Waits for pending promise callbacks.
 *
 * @returns Resolves on the next timer turn.
 */
function settle(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 10));
}

test("an upgrade the library accepts becomes a live connection both ways", async () => {
  const clients = fakeClients();
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 }, clients);
  const seen: Upgrade[] = [];
  const outcomes: ServeOutcome[] = [];
  const frames: Uint8Array[] = [];
  const origin = library(seen, async () => ({ socket: fakeLocal("o1", "chat") }));
  const via = pluginSocket(hub, origin, outcomes, frames);
  hub.attach(via);
  const response = await hub.fetch(upgrade({ headers: { "sec-websocket-protocol": "chat" } }));
  assert.equal(await response.text(), "upgraded");
  assert.equal(response.headers.get("x-protocol"), "chat");
  const [first] = seen;
  assert.ok(first);
  assert.equal(first.method, "GET");
  assert.equal(first.target, "/api/node/v1/socket?x=1");
  assert.equal(first.headers.get("sec-websocket-protocol"), "chat");
  assert.equal(first.options?.clientAddress, "203.0.113.9");
  const [accepted] = clients.accepted;
  assert.ok(accepted);
  assert.equal(accepted.via, via);
  assert.equal(accepted.local, "o1");
  assert.deepEqual(first.options?.attachment, { live: accepted.id, relay: "r0" });
  await settle();
  assert.deepEqual(outcomes, ["served"]);

  // Library to client: text, binary and close.
  const client = clients.byId.get(accepted.id);
  assert.ok(client);
  const id = accepted.id;
  assert.equal(hub.pluginFrame(encodeLiveFrame({ op: "text", id, text: "hi" })), "delivered");
  const data = new Uint8Array([1, 2, 3]);
  assert.equal(hub.pluginFrame(encodeLiveFrame({ op: "binary", id, data })), "delivered");
  assert.equal(
    hub.pluginFrame(encodeLiveFrame({ op: "close", id, code: 4001, reason: "done" })),
    "delivered",
  );
  assert.deepEqual(client.got, ["hi", [1, 2, 3]]);
  assert.deepEqual(client.closed, [4001, "done"]);
  // A client that is gone does not fail the hub.
  assert.equal(hub.pluginFrame(encodeLiveFrame({ op: "text", id, text: "late" })), "delivered");
  assert.equal(hub.pluginFrame(new Uint8Array([9])), "malformed");
  assert.equal(
    hub.pluginFrame(encodeLiveFrame({ op: "text", id: "nobody", text: "x" })),
    "dropped",
  );
  // The plugin socket is free for the next request.
  assert.equal(hub.sockets, 1);
});

test("an upgrade the library refuses is answered with its refusal", async () => {
  const clients = fakeClients();
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 }, clients);
  const origin = library([], async () => ({
    response: new Response("sign in first", {
      status: 401,
      headers: { [RELAY_SOCKET_HEADER]: "o9", "www-authenticate": "Bearer" },
    }),
  }));
  hub.attach(pluginSocket(hub, origin, []));
  const response = await hub.fetch(upgrade());
  assert.equal(response.status, 401);
  assert.equal(await response.text(), "sign in first");
  assert.equal(response.headers.get("www-authenticate"), "Bearer");
  assert.equal(response.headers.get(RELAY_SOCKET_HEADER), null);
  assert.equal(clients.accepted.length, 0);
});

test("frames the library sends before the client is accepted are held, then delivered", async () => {
  const clients = fakeClients();
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 }, clients);
  const held: string[] = [];
  const origin = library([], async (options) => {
    const { live } = options?.attachment as { live: string };
    held.push(hub.pluginFrame(encodeLiveFrame({ op: "text", id: live, text: "welcome" })));
    // Past the hold's frame budget, frames are dropped.
    for (let i = 0; i < 64; i++)
      held.push(
        hub.pluginFrame(encodeLiveFrame({ op: "binary", id: live, data: new Uint8Array(1) })),
      );
    return { socket: fakeLocal("o2", "") };
  });
  hub.attach(pluginSocket(hub, origin, []));
  await (await hub.fetch(upgrade())).text();
  assert.equal(held[0], "held");
  assert.equal(held.at(-1), "dropped");
  const [accepted] = clients.accepted;
  assert.ok(accepted);
  const client = clients.byId.get(accepted.id);
  assert.equal(client?.got[0], "welcome");
  assert.equal(client?.got.length, 64);
});

test("the relay refuses upgrades it cannot carry", async () => {
  const plain = new RelayHub(TOKEN);
  plain.attach({ send() {} });
  assert.equal((await plain.fetch(upgrade())).status, 501);

  const hub = new RelayHub(TOKEN, undefined, fakeClients());
  hub.attach({ send() {} });
  assert.equal((await hub.fetch(upgrade({ method: "POST" }))).status, 400);
  assert.equal((await hub.fetch(upgrade({ headers: { upgrade: "h2c" } }))).status, 400);
});

/**
 * A hub whose one plugin socket pushes `head` by hand for each notice.
 *
 * @param head - The response head to push.
 * @param clients - The client sockets, or none.
 * @returns The hub and the plugin's push answers.
 */
function handPushed(
  head: { status: number; headers: Array<{ name: string; value: string }> },
  clients?: FakeClients,
): { hub: RelayHub; pushes: Response[] } {
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 }, clients);
  const pushes: Response[] = [];
  hub.attach({
    send(message) {
      if (typeof message !== "string") return;
      const notice = parseNotice(message);
      if (typeof notice === "string") return;
      void (async () => {
        const pulled = await hub.fetch(
          new Request(`https://${HOST}${RELAY_PULL_PATH}${notice.id}`, { headers: AUTH }),
        );
        await pulled.body?.cancel();
        pushes.push(
          await hub.fetch(
            new Request(`https://${HOST}${RELAY_PUSH_PATH}${notice.id}`, {
              method: "POST",
              body: framed(encodeFrame(head), null),
              headers: AUTH,
              duplex: "half",
            } as RequestInit),
          ),
        );
      })();
    },
  });
  return { hub, pushes };
}

test("a 101 push that does not fit answers the client 502 and the plugin 400", async () => {
  const accepted = { status: 101, headers: [{ name: RELAY_SOCKET_HEADER, value: "o1" }] };
  // 101 for a plain request.
  const plain = handPushed(accepted, fakeClients());
  assert.equal((await plain.hub.fetch(new Request("https://lib.example/"))).status, 502);
  await settle();
  assert.equal(plain.pushes[0]?.status, 400);
  // 101 for an upgrade without the plugin's socket id.
  const nameless = handPushed({ status: 101, headers: [] }, fakeClients());
  assert.equal((await nameless.hub.fetch(upgrade())).status, 502);
  await settle();
  assert.equal(nameless.pushes[0]?.status, 400);
});

test("serveRequest reports a library that cannot connect and a relay that drops the 101", async () => {
  const clients = fakeClients();
  const hub = new RelayHub(TOKEN, { queueMs: 1000, headMs: 1000 }, clients);
  const outcomes: ServeOutcome[] = [];
  hub.attach(
    pluginSocket(
      hub,
      library([], () => Promise.reject(new Error("down"))),
      outcomes,
    ),
  );
  const response = await hub.fetch(upgrade());
  assert.equal(response.status, 502);
  assert.match(await response.text(), /could not open the connection/);
  await settle();
  assert.deepEqual(outcomes, ["library_failed"]);

  // The relay is gone by the time the 101 is pushed: the socket is closed.
  const local = fakeLocal("o5", "");
  const pull = encodeFrame({ method: "GET", target: "/s", headers: [], upgrade: true });
  const relay: Fetch = async (input) =>
    input.includes(RELAY_PULL_PATH) ? new Response(pull) : Promise.reject(new Error("offline"));
  const origin = library([], async () => ({ socket: local }));
  assert.equal(
    await serveRequest({ fetch: relay, origin, host: HOST, token: TOKEN, via: "r1" }, "u1"),
    "push_failed",
  );
  assert.equal(local.closedWith, 1011);
});

test("live frames round-trip and refuse what a peer may not send", () => {
  assert.deepEqual(parseLiveFrame(encodeLiveFrame({ op: "text", id: "o1", text: "é" })), {
    op: "text",
    id: "o1",
    text: "é",
  });
  assert.throws(() => encodeLiveFrame({ op: "text", id: "bad id", text: "" }), RangeError);
  assert.throws(
    () => encodeLiveFrame({ op: "close", id: "o1", code: 1006, reason: "" }),
    RangeError,
  );
  assert.throws(
    () =>
      encodeLiveFrame({
        op: "close",
        id: "o1",
        code: 1000,
        reason: "x".repeat(MAX_CLOSE_REASON_BYTES + 1),
      }),
    RangeError,
  );
  assert.throws(
    () =>
      encodeLiveFrame({
        op: "binary",
        id: "o1",
        data: new Uint8Array(MAX_LIVE_MESSAGE_BYTES + 63),
      }),
    RangeError,
  );
  assert.equal(messageFrame("o1", "x".repeat(MAX_LIVE_MESSAGE_BYTES + 1)), null);
  assert.equal(messageFrame("o1", new ArrayBuffer(MAX_LIVE_MESSAGE_BYTES + 1)), null);
  const binary = messageFrame("o1", new Uint8Array([5]).buffer);
  assert.ok(binary);
  assert.deepEqual(parseLiveFrame(binary), { op: "binary", id: "o1", data: new Uint8Array([5]) });
});

test("close codes and reasons are mapped to what both ends may send", () => {
  assert.equal(isSendableCloseCode(1000), true);
  assert.equal(isSendableCloseCode(1005), false);
  assert.equal(isSendableCloseCode(4999), true);
  assert.equal(sendableCloseCode(4000), 4000);
  assert.equal(sendableCloseCode(1006), 1011);
  assert.equal(sendableCloseCode(1005), 1000);
  assert.equal(sendableCloseReason("short"), "short");
  const long = sendableCloseReason("é".repeat(100));
  assert.equal(long, "é".repeat(61));
  assert.ok(new TextEncoder().encode(long).length <= MAX_CLOSE_REASON_BYTES);
});
