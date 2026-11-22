/**
 * The relay Worker's Durable Object under Node: a fake `DurableObjectState`
 * with tags and a stand-in `WebSocketPair` carry plugin and client sockets.
 * Node's `Response` refuses 101, so a stand-in `Response` global accepts it
 * for the tests that open sockets.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  RELAY_CONNECT_PATH,
  RELAY_PULL_PATH,
  RELAY_PUSH_PATH,
  RELAY_SOCKET_HEADER,
  encodeFrame,
  encodeLiveFrame,
  parseLiveFrame,
  parseNotice,
} from "../src/protocol.ts";
import { framed } from "../src/relay-core.ts";
import { Relay, type RelayState, type WorkerSocket } from "../src/relay-worker.ts";

/** The relay's token in these tests. */
const TOKEN = "relay-token-0123456789abcdef";

/** Bearer header for relay paths. */
const AUTH = { authorization: `Bearer ${TOKEN}` };

/** A fake Workers socket. */
interface FakeSocket extends WorkerSocket {
  /** Messages sent on it. */
  sent: Array<string | Uint8Array>;
  /** Close code and reason, once closed. */
  closed: [number, string] | null;
  /** The attachment. */
  attachment: unknown;
  /** Tags it was accepted with. */
  tags: string[];
}

/**
 * A fake Workers socket.
 *
 * @returns The socket.
 */
function fakeSocket(): FakeSocket {
  const ws: FakeSocket = {
    sent: [],
    closed: null,
    attachment: null,
    tags: [],
    send(message) {
      if (ws.closed) throw new Error("closed");
      ws.sent.push(message);
    },
    close(code, reason) {
      if (ws.closed) throw new Error("closed");
      ws.closed = [code, reason];
    },
    serializeAttachment(value) {
      ws.attachment = structuredClone(value);
    },
    deserializeAttachment() {
      return ws.attachment;
    },
  };
  return ws;
}

/** A fake Durable Object state holding accepted sockets. */
interface FakeState extends RelayState {
  /** Every accepted socket, open or not. */
  accepted: FakeSocket[];
  /**
   * The open sockets.
   *
   * @param tag - Only those with this tag.
   * @returns Them.
   */
  getWebSockets(tag?: string): FakeSocket[];
}

/**
 * A fake Durable Object state.
 *
 * @returns The state.
 */
function fakeState(): FakeState {
  const accepted: FakeSocket[] = [];
  return {
    accepted,
    acceptWebSocket(socket, tags) {
      const ws = socket as FakeSocket;
      ws.tags = tags;
      accepted.push(ws);
    },
    getWebSockets(tag) {
      return accepted.filter((ws) => !ws.closed && (tag === undefined || ws.tags.includes(tag)));
    },
  };
}

/** The client ends the stand-in `WebSocketPair` handed out. */
const clientEnds: FakeSocket[] = [];

/** A `Response` that also takes 101 and a `webSocket`, as the Workers one does. */
class WorkersResponse extends Response {
  /** The client end, on a 101. */
  readonly webSocket: unknown;

  /**
   * @param body - Body.
   * @param init - Init, possibly with status 101 and `webSocket`.
   */
  constructor(body?: BodyInit | null, init?: ResponseInit & { webSocket?: unknown }) {
    const upgraded = init?.status === 101;
    super(body, upgraded ? { ...init, status: 200 } : init);
    this.webSocket = init?.webSocket ?? null;
    if (upgraded) Object.defineProperty(this, "status", { value: 101 });
  }
}

Object.assign(globalThis, {
  WebSocketPair: class {
    /** Client end. */
    0: FakeSocket;
    /** Server end. */
    1: FakeSocket;
    constructor() {
      this[0] = fakeSocket();
      this[1] = fakeSocket();
      clientEnds.push(this[0]);
    }
  },
});

/**
 * Runs `body` with the stand-in `Response` installed.
 *
 * @param body - The test.
 * @returns Its result.
 */
async function withWorkersResponse<T>(body: () => Promise<T>): Promise<T> {
  const real = globalThis.Response;
  globalThis.Response = WorkersResponse as typeof Response;
  try {
    return await body();
  } finally {
    globalThis.Response = real;
  }
}

/**
 * A relay object with one plugin socket whose notices are served by hand:
 * the head is pushed as 101, naming `o1`.
 *
 * @returns The object, its state and the plugin's server-side socket.
 */
async function relayWithPlugin(): Promise<{ relay: Relay; state: FakeState; plugin: FakeSocket }> {
  const state = fakeState();
  const relay = new Relay(state, { RELAY: undefined as never, RELAY_TOKEN: TOKEN });
  const connected = await relay.fetch(
    new Request(`https://relay.example${RELAY_CONNECT_PATH}`, {
      headers: { ...AUTH, upgrade: "websocket" },
    }),
  );
  assert.equal(connected.status, 101);
  const plugin = state.getWebSockets("plugin")[0];
  assert.ok(plugin);
  return { relay, state, plugin };
}

/**
 * Serves the upgrade the plugin socket was told about with a 101 push.
 *
 * @param relay - The relay object.
 * @param plugin - The plugin socket that got the notice.
 * @returns The push's answer.
 */
async function pushAccept(relay: Relay, plugin: FakeSocket): Promise<Response> {
  const notice = parseNotice(String(plugin.sent.at(-1)));
  assert.notEqual(typeof notice, "string");
  if (typeof notice === "string") throw new Error(notice);
  const pulled = await relay.fetch(
    new Request(`https://relay.example${RELAY_PULL_PATH}${notice.id}`, { headers: AUTH }),
  );
  await pulled.body?.cancel();
  const head = {
    status: 101,
    headers: [
      { name: RELAY_SOCKET_HEADER, value: "o1" },
      { name: "sec-websocket-protocol", value: "chat" },
    ],
  };
  return relay.fetch(
    new Request(`https://relay.example${RELAY_PUSH_PATH}${notice.id}`, {
      method: "POST",
      headers: AUTH,
      body: framed(encodeFrame(head), null),
      duplex: "half",
    } as RequestInit),
  );
}

test("the relay object carries a live connection both ways and passes closes on", async () => {
  await withWorkersResponse(async () => {
    const { relay, state, plugin } = await relayWithPlugin();
    const answer = relay.fetch(
      new Request("https://lib.example/socket", { headers: { upgrade: "websocket" } }),
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal((await pushAccept(relay, plugin)).status, 204);
    const response = (await answer) as WorkersResponse;
    assert.equal(response.status, 101);
    assert.equal(response.headers.get("sec-websocket-protocol"), "chat");
    const client = state.getWebSockets("client")[0];
    assert.ok(client);
    const { id } = client.attachment as { id: string };

    // Client to plugin, addressed to the plugin's socket.
    relay.webSocketMessage(client, "hello");
    relay.webSocketMessage(client, new Uint8Array([1, 2]).buffer);
    const frames = plugin.sent.filter((m): m is Uint8Array => m instanceof Uint8Array);
    assert.deepEqual(parseLiveFrame(frames[0] ?? new Uint8Array()), {
      op: "text",
      id: "o1",
      text: "hello",
    });
    assert.deepEqual(parseLiveFrame(frames[1] ?? new Uint8Array()), {
      op: "binary",
      id: "o1",
      data: new Uint8Array([1, 2]),
    });
    // Plugin to client, addressed to the connection.
    relay.webSocketMessage(plugin, encodeLiveFrame({ op: "text", id, text: "back" }).buffer);
    relay.webSocketMessage(plugin, "ping");
    assert.deepEqual(client.sent, ["back"]);
    // An oversized client message closes with 1009 and tells the plugin.
    relay.webSocketMessage(client, new ArrayBuffer(300_000));
    assert.deepEqual(client.closed, [1009, "message too large"]);
    const last = plugin.sent.at(-1);
    assert.ok(last instanceof Uint8Array);
    assert.deepEqual(parseLiveFrame(last), {
      op: "close",
      id: "o1",
      code: 1009,
      reason: "message too large",
    });
  });
});

test("a client's close reaches the plugin, and a plugin socket's close ends its clients", async () => {
  await withWorkersResponse(async () => {
    const { relay, state, plugin } = await relayWithPlugin();
    const answer = relay.fetch(
      new Request("https://lib.example/socket", { headers: { upgrade: "websocket" } }),
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    await pushAccept(relay, plugin);
    await answer;
    const client = state.getWebSockets("client")[0];
    assert.ok(client);
    relay.webSocketClose(client, 1005, "");
    const last = plugin.sent.at(-1);
    assert.ok(last instanceof Uint8Array);
    assert.deepEqual(parseLiveFrame(last), { op: "close", id: "o1", code: 1000, reason: "" });

    // A second connection ends when its plugin socket fails.
    const second = relay.fetch(
      new Request("https://lib.example/socket", { headers: { upgrade: "websocket" } }),
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    await pushAccept(relay, plugin);
    await second;
    const next = state.getWebSockets("client").find((ws) => ws !== client);
    assert.ok(next);
    plugin.closed = [1006, ""];
    relay.webSocketError(plugin);
    assert.deepEqual(next.closed, [1011, "the library's connection to the relay closed"]);
    // Its messages now find no plugin socket.
    const late = fakeSocket();
    late.attachment = { kind: "client", id: "x", via: "missing", local: "o3" };
    relay.webSocketMessage(late, "x");
    assert.deepEqual(late.closed, [1011, "the library's connection to the relay closed"]);
    relay.webSocketClose(late, 1000, "");
  });
});

test("the relay object adopts only tagged plugin sockets after waking", () => {
  const state = fakeState();
  const plugin = fakeSocket();
  plugin.attachment = { kind: "plugin", key: "k1" };
  state.acceptWebSocket(plugin, ["plugin", "plugin:k1"]);
  const client = fakeSocket();
  client.attachment = { kind: "client", id: "c1", via: "k1", local: "o1" };
  state.acceptWebSocket(client, ["client", "client:c1", "via:k1"]);
  const relay = new Relay(state, { RELAY: undefined as never, RELAY_TOKEN: TOKEN });
  // A live frame for the woken client is delivered.
  relay.webSocketMessage(plugin, encodeLiveFrame({ op: "text", id: "c1", text: "hi" }).buffer);
  assert.deepEqual(client.sent, ["hi"]);
  // A socket with no attachment the relay wrote is ignored.
  relay.webSocketMessage(fakeSocket(), "x");
});
