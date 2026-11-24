/**
 * The ingress lifecycle and the socket handlers, with the SDK's isolate
 * module replaced by stand-ins (the real one imports `cloudflare:workers`,
 * which only exists inside workerd), fake sockets and a small Cloudflare
 * edge emulator behind them.
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

import { MessageFramer, readRoot } from "../src/capnp.ts";
import {
  ErrorCode,
  Flag,
  FrameReader,
  FrameType,
  PREFACE,
  encodeData,
  encodeFrame,
  encodeGoAway,
  encodeHeaders as encodeHeaderFrames,
  encodePing,
  encodeSettings,
  join,
  type Frame,
} from "../src/h2.ts";
import { HpackDecoder, encodeHeaders } from "../src/hpack.ts";
import { EDGE, bytes, hex, text } from "./fake.ts";

/** Stand-in for the SDK's `PluginError`. */
class PluginError extends Error {
  /** Wire code. */
  readonly wireCode: string;

  /**
   * Creates an error.
   *
   * @param code - Wire code.
   * @param message - Message.
   */
  constructor(code: string, message: string) {
    super(message);
    this.wireCode = code;
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
  exports: {
    BookclerkEntrypoint: Entrypoint,
    IngressEntrypoint: Entrypoint,
    ProvisioningEntrypoint: Entrypoint,
    PluginError,
    provisioned: () => ({}),
  },
});
const { Ingress, default: Plugin } = await import("../src/index.ts");
const {
  CLOSE_BROKEN,
  CLOSE_FAILED,
  CLOSE_REPLACED,
  CLOSE_RESTARTED,
  CLOSE_STOPPED,
  EDGE_SERVER_NAME,
  EDGE_URLS,
  agentHealth,
  handleClose,
  handleMessage,
  ioFor,
  liveOf,
  parseToken,
  sessionOf,
  settingsOf,
  startAgent,
  stopAgent,
} = await import("../src/lifecycle.ts");
const { CLOUDFLARE_ROOT_CAS } = await import("../src/ca.ts");
const { VERSION } = await import("../src/version.ts");

/** The hostname these tests serve. */
const HOSTNAME = "books.example.com";

/** The account tag in {@link TOKEN}. */
const ACCOUNT = "0123456789abcdef0123456789abcdef";

/** The tunnel secret in {@link TOKEN}. */
const SECRET = new Uint8Array(32).map((_, i) => i + 1);

/**
 * A tunnel token as the dashboard shows it.
 *
 * @param fields - Overrides of its JSON fields.
 * @returns The token.
 */
function token(fields: Record<string, unknown> = {}): string {
  return Buffer.from(
    JSON.stringify({
      a: ACCOUNT,
      t: "c0ffee00-1234-4abc-8def-0123456789ab",
      s: Buffer.from(SECRET).toString("base64"),
      ...fields,
    }),
  ).toString("base64");
}

/** How the emulator answers. */
interface Behaviour {
  /** The `Return` it answers the registration with, as hex. */
  answer: string;
  /** Whether pings are answered. */
  echo: boolean;
  /** Whether anything is answered at all. */
  silent: boolean;
}

/** A fake socket the fake binding hands out. */
interface FakeSocket extends HostWebSocket {
  /** Close code, once closed. */
  closedWith: number | null;
}

/** The fake world: sockets, emulator and library. */
interface World {
  /** The socket binding. */
  binding: SocketsBinding;
  /** Open sockets. */
  open: FakeSocket[];
  /** Every connect call. */
  connects: Array<{ url: string; options: SocketConnectOptions | undefined }>;
  /** How the emulator answers. */
  behaviour: Behaviour;
  /** The plugin's bindings. */
  env: Parameters<typeof settingsOf>[0];
  /** Requests the library saw. */
  seen: Request[];
  /** Waits for queued deliveries. */
  settle: () => Promise<void>;
  /** Raw bytes written per socket id. */
  written: Map<string, Uint8Array[]>;
  /** Registration messages received per socket id. */
  registrations: Map<string, Uint8Array[][]>;
  /** Messages sent per library socket id. */
  liveSent: Map<string, Array<string | ArrayBuffer>>;
  /** Every `ORIGIN.connect` call's options. */
  upgrades: Array<{ clientAddress?: string; attachment?: unknown }>;
}

/**
 * Builds the fake world. Each socket feeds what the plugin writes to a
 * small edge emulator, whose answers are delivered to `handleMessage` on
 * the next turn, one at a time, as the host does.
 *
 * @param partial - Emulator behaviour overrides.
 * @param refuse - A wire code every `connect` fails with, if any.
 * @returns The world.
 */
function world(partial: Partial<Behaviour> = {}, refuse: string | null = null): World {
  const behaviour: Behaviour = { answer: EDGE.registered, echo: true, silent: false, ...partial };
  const open: FakeSocket[] = [];
  const connects: World["connects"] = [];
  const seen: Request[] = [];
  const written = new Map<string, Uint8Array[]>();
  const registrations = new Map<string, Uint8Array[][]>();
  const liveSent = new Map<string, Array<string | ArrayBuffer>>();
  const upgrades: World["upgrades"] = [];
  let minted = 0;
  let queue = Promise.resolve();
  const env = {
    CONFIG: { hostname: HOSTNAME },
    SECRETS: { token: token() },
    SOCKETS: undefined as unknown as SocketsBinding,
    ORIGIN: {
      fetch: (request: Request) => {
        seen.push(request);
        return Promise.resolve(new Response("library says hi"));
      },
      connect: (_request: Request, options?: { clientAddress?: string; attachment?: unknown }) => {
        upgrades.push(options ?? {});
        const ws = liveSocket(options?.attachment ?? null);
        open.push(ws);
        return Promise.resolve({ socket: ws });
      },
    } satisfies OriginBinding,
  } as unknown as World["env"];
  /**
   * Delivers bytes to the plugin after the current turn.
   *
   * @param ws - The socket.
   * @param chunk - The bytes.
   */
  const deliver = (ws: FakeSocket, chunk: Uint8Array): void => {
    queue = queue.then(async () => {
      await new Promise((resolve) => setImmediate(resolve));
      if (ws.closedWith !== null) return;
      await handleMessage(env, ws, chunk.slice().buffer);
    });
  };
  /**
   * Makes a socket that answers like the edge.
   *
   * @param url - Its URL.
   * @param attachment - Its attachment.
   * @returns The socket.
   */
  const socket = (url: string, attachment: unknown): FakeSocket => {
    // The emulator reads the session's frames as a client would: after a preface.
    const reader = new FrameReader();
    reader.push(PREFACE);
    const framer = new MessageFramer();
    const id = `s${minted++}`;
    const received: Uint8Array[][] = [];
    written.set(id, []);
    registrations.set(id, received);
    let opened = false;
    const ws: FakeSocket = {
      id,
      url,
      protocol: "",
      closedWith: null,
      send: (message) => {
        if (ws.closedWith !== null) return Promise.reject(new PluginError("not_found", "closed"));
        const chunk = new Uint8Array(message as ArrayBuffer);
        written.get(id)?.push(chunk);
        if (behaviour.silent) return Promise.resolve();
        for (const frame of reader.push(chunk)) {
          if (!opened && frame.kind === "settings") {
            opened = true;
            deliver(ws, join(PREFACE, encodeSettings([]), controlRequest()));
          } else if (frame.kind === "data" && frame.stream === 1) {
            const before = received.length;
            received.push(...framer.push(frame.data));
            if (before < 2 && received.length >= 2) {
              deliver(ws, encodeData(1, hex(behaviour.answer)));
            }
          } else if (frame.kind === "ping" && !frame.ack && behaviour.echo) {
            deliver(ws, encodePing(frame.data, true));
          }
        }
        return Promise.resolve();
      },
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
  /**
   * Makes a socket to the library that records what it is sent.
   *
   * @param attachment - Its attachment.
   * @returns The socket.
   */
  const liveSocket = (attachment: unknown): FakeSocket => {
    const id = `live${minted++}`;
    const sent: Array<string | ArrayBuffer> = [];
    liveSent.set(id, sent);
    const ws: FakeSocket = {
      id,
      url: "wss://library/",
      protocol: "",
      closedWith: null,
      send: (message) => {
        if (ws.closedWith !== null) return Promise.reject(new PluginError("not_found", "closed"));
        sent.push(typeof message === "string" ? message : (message as ArrayBuffer));
        return Promise.resolve();
      },
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
      if (refuse !== null) return Promise.reject(new PluginError(refuse, `refused: ${refuse}`));
      const ws = socket(url, options?.attachment ?? null);
      open.push(ws);
      return Promise.resolve(ws);
    },
    getWebSockets: () => Promise.resolve([...open]),
    getWebSocket(id) {
      const found = open.find((ws) => ws.id === id);
      if (found) return found;
      throw new Error(`no socket ${id}`);
    },
  };
  (env as { SOCKETS: SocketsBinding }).SOCKETS = binding;
  return {
    binding,
    open,
    connects,
    behaviour,
    env,
    seen,
    written,
    registrations,
    liveSent,
    upgrades,
    settle: async () => {
      for (let i = 0; i < 20; i++) {
        await queue;
        await new Promise((resolve) => setImmediate(resolve));
      }
    },
  };
}

/**
 * The edge's request that opens the control stream (stream 1).
 *
 * @returns The HEADERS frame.
 */
function controlRequest(): Uint8Array {
  return encodeHeaderFrames(
    1,
    encodeHeaders([
      [":method", "GET"],
      [":scheme", "https"],
      [":path", "/"],
      ["cf-cloudflared-proxy-connection-upgrade", "control-stream"],
    ]),
    false,
    16384,
  );
}

/**
 * A proxied request from the edge.
 *
 * @param stream - Its stream (odd, above 1).
 * @param path - Its path.
 * @param extra - More fields.
 * @param endStream - Whether it has no body.
 * @returns The HEADERS frame.
 */
function request(
  stream: number,
  path: string,
  extra: Array<[string, string]> = [],
  endStream = true,
): ArrayBuffer {
  return encodeHeaderFrames(
    stream,
    encodeHeaders([
      [":method", "GET"],
      [":scheme", "https"],
      [":authority", HOSTNAME],
      [":path", path],
      ["cf-connecting-ip", "203.0.113.9"],
      ...extra,
    ]),
    endStream,
    16384,
  ).slice().buffer;
}

/**
 * A context over the world with instant waits.
 *
 * @param w - The world.
 * @param waitMs - How long `start` and `health` wait.
 * @returns The context.
 */
function ctxOf(w: World, waitMs = 1_000): Parameters<typeof startAgent>[0] {
  return {
    env: w.env,
    sleep: () => new Promise((resolve) => setTimeout(resolve, 1)),
    random: (n) => new Uint8Array(n).fill(7),
    startWaitMs: waitMs,
    beatWaitMs: waitMs,
  };
}

/** What the session wrote on one stream of a connector socket. */
interface Written {
  /** Decoded header blocks, in order. */
  heads: Array<Array<[string, string]>>;
  /** DATA bytes, joined. */
  body: Uint8Array;
  /** Whether END_STREAM went out. */
  ended: boolean;
  /** The RST_STREAM code, if one went out. */
  reset: number | null;
}

/**
 * Reads what the session wrote on one stream.
 *
 * @param w - The world.
 * @param ws - The connector's socket.
 * @param stream - The stream.
 * @returns Its frames, decoded.
 */
function streamOf(w: World, ws: FakeSocket, stream: number): Written {
  const reader = new FrameReader();
  reader.push(PREFACE);
  const hpack = new HpackDecoder(1 << 20);
  const out: Written = { heads: [], body: new Uint8Array(), ended: false, reset: null };
  const parts: Uint8Array[] = [];
  for (const frame of w.written.get(ws.id)!.flatMap((chunk): Frame[] => reader.push(chunk))) {
    if (frame.kind === "headers") {
      const fields = hpack.decode(frame.block).fields;
      if (frame.stream !== stream) continue;
      out.heads.push(fields);
      out.ended ||= frame.endStream;
    } else if (frame.kind === "data" && frame.stream === stream) {
      parts.push(frame.data);
      out.ended ||= frame.endStream;
    } else if (frame.kind === "rst" && frame.stream === stream) {
      out.reset = frame.code;
    }
  }
  out.body = join(...parts, new Uint8Array());
  return out;
}

/**
 * Bytes as Latin-1, one character per octet.
 *
 * @param data - The bytes.
 * @returns The text.
 */
function latin1(data: Uint8Array): string {
  return Buffer.from(data).toString("latin1");
}

test("tokens: the dashboard's token, alone or in its install command, decodes", () => {
  const decoded = parseToken(token());
  assert.ok(typeof decoded !== "string");
  assert.equal(decoded.accountTag, ACCOUNT);
  assert.deepEqual(decoded.secret, SECRET);
  assert.deepEqual(decoded.tunnelId, hex("c0ffee0012344abc8def0123456789ab"));
  // URL-safe and unpadded base64 work as well, and the install command.
  const urlSafe = token().replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  assert.deepEqual(parseToken(`sudo cloudflared service install ${urlSafe}`), decoded);
  assert.ok(typeof parseToken(token({ a: ACCOUNT.toUpperCase() })) !== "string");

  const cases: Array<[string, RegExp]> = [
    ["", /not the base64 token/],
    ["!!!!", /not the base64 token/],
    ["a", /not the base64 token/],
    [Buffer.from("not json").toString("base64"), /expected JSON/],
    [Buffer.from([0xff, 0xfe]).toString("base64"), /expected JSON/],
    [Buffer.from("42").toString("base64"), /expected JSON/],
    [Buffer.from("null").toString("base64"), /expected JSON/],
    [token({ e: "fed.argotunnel.com" }), /dedicated Cloudflare endpoint/],
    [token({ a: "abc" }), /account tag/],
    [token({ t: "nope" }), /tunnel id/],
    [token({ s: Buffer.from("short").toString("base64") }), /tunnel secret/],
    [token({ s: 7 }), /tunnel secret/],
    [token({ s: "@@@" }), /tunnel secret/],
  ];
  for (const [raw, message] of cases) {
    const result = parseToken(raw);
    assert.equal(typeof result, "string", raw);
    assert.match(result as string, message);
  }
  assert.ok(typeof parseToken(token({ e: "" })) !== "string");
});

test("settings: hostname must be a host name; the token is required and must decode", () => {
  const w = world();
  assert.equal(settingsOf(w.env).hostname, HOSTNAME);
  const env = (config: Record<string, unknown>, secrets: Record<string, string>) =>
    ({ ...w.env, CONFIG: config, SECRETS: secrets }) as typeof w.env;
  assert.equal(
    settingsOf(env({ hostname: " https://Books.Example.com./ " }, { token: token() })).hostname,
    "books.example.com",
  );
  assert.throws(() => settingsOf(env({ hostname: "" }, { token: token() })), /set hostname/);
  assert.throws(() => settingsOf(env({}, { token: token() })), /set hostname/);
  assert.throws(
    () => settingsOf(env({ hostname: "books.example.com/x" }, { token: token() })),
    /host name/,
  );
  assert.throws(() => settingsOf(env({ hostname: HOSTNAME }, {})), /token secret/);
  assert.throws(
    () => settingsOf(env({ hostname: HOSTNAME }, { token: "garbage" })),
    (err: PluginError) => err.wireCode === "invalid_params" && /JSON/.test(err.message),
  );
});

test("start dials the edge over TLS as h2.cftunnel.com, registers and answers the origin", async () => {
  const w = world();
  const started = await startAgent(ctxOf(w));
  assert.deepEqual(started, { origin: `https://${HOSTNAME}` });
  assert.equal(w.connects.length, 1);
  assert.equal(w.connects[0]?.url, EDGE_URLS[0]);
  assert.equal(w.connects[0]?.options?.serverName, EDGE_SERVER_NAME);
  assert.deepEqual(w.connects[0]?.options?.trustAnchors, [...CLOUDFLARE_ROOT_CAS]);
  assert.deepEqual(w.connects[0]?.options?.attachment, { hostname: HOSTNAME });
  const ws = w.open[0]!;
  assert.equal(sessionOf(ws.id)?.phase, "serving");
  assert.equal(sessionOf(ws.id)?.location, "SJC");
  // The registration carries the token's parts, and the secret goes only
  // inside the TLS stream.
  const call = readRoot(w.registrations.get(ws.id)![1]!).struct(0)!;
  const params = call.struct(1)!.struct(0)!;
  assert.equal(params.struct(0)!.text(0), ACCOUNT);
  assert.deepEqual(params.struct(0)!.data(1), SECRET);
  const client = params.struct(2)!.struct(0)!;
  assert.deepEqual(client.data(0), new Uint8Array(16).fill(7));
  assert.equal(client.text(2), `bookclerk-${VERSION}`);
  assert.ok(!JSON.stringify(w.connects).includes(token()));
});

test("start fails with Cloudflare's reason and closes the socket without reconnecting", async () => {
  const refused = world({ answer: EDGE.refused });
  await assert.rejects(startAgent(ctxOf(refused)), (err: PluginError) => {
    assert.equal(err.wireCode, "unavailable");
    assert.match(err.message, /too many connections/);
    return true;
  });
  await refused.settle();
  assert.equal(refused.open.length, 0);

  const failed = world({ answer: EDGE.exception });
  await assert.rejects(startAgent(ctxOf(failed)), /no such tunnel/);

  const silent = world({ silent: true });
  await assert.rejects(startAgent(ctxOf(silent, 20)), /did not register the tunnel/);
});

test("start tries both regions and both root sets, and passes the host's refusals", async () => {
  const down = world({}, "unavailable");
  await assert.rejects(startAgent(ctxOf(down)), /refused: unavailable/);
  assert.deepEqual(
    down.connects.map((c) => [c.url, c.options?.trustAnchors?.length]),
    [
      [EDGE_URLS[0], CLOUDFLARE_ROOT_CAS.length],
      [EDGE_URLS[0], 0],
      [EDGE_URLS[1], CLOUDFLARE_ROOT_CAS.length],
      [EDGE_URLS[1], 0],
    ],
  );
  // A missing grant won't change with region or roots.
  const ungranted = world({}, "not_granted");
  await assert.rejects(startAgent(ctxOf(ungranted)), /refused: not_granted/);
  assert.equal(ungranted.connects.length, 1);
});

test("start replaces an earlier connection", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const first = w.open[0]!;
  await startAgent(ctxOf(w));
  assert.equal(first.closedWith, CLOSE_STOPPED);
  assert.equal(w.open.length, 1);
});

test("a proxied request is served through ORIGIN with the client's address", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const ws = w.open[0]!;
  await handleMessage(w.env, ws, request(3, "/hello?x=1"));
  assert.equal(w.seen.length, 1);
  assert.equal(w.seen[0]!.url, `https://${HOSTNAME}/hello?x=1`);
  const out = streamOf(w, ws, 3);
  assert.equal(out.heads[0]?.[0]?.[1], "200");
  assert.equal(text(out.body), "library says hi");
  assert.ok(out.ended);
  // A string message (never sent on a stream) is ignored.
  await handleMessage(w.env, ws, "text");
});

test("health: a registered session that answers a ping serves", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  assert.deepEqual(await agentHealth(ctxOf(w)), { serving: true });

  w.behaviour.echo = false;
  assert.deepEqual(await agentHealth(ctxOf(w, 20)), {
    serving: false,
    detail: "the Cloudflare edge did not answer a ping",
  });
});

test("health reconnects when no session is held, and reports a refused one", async () => {
  const w = world();
  assert.deepEqual(await agentHealth(ctxOf(w)), { serving: true });
  assert.equal(w.connects.length, 1);

  const bad = world({ answer: EDGE.refusedEmpty });
  const health = await agentHealth(ctxOf(bad));
  assert.equal(health.serving, false);
  assert.match(health.detail ?? "", /no reason given/);

  const refused = world({}, "unavailable");
  assert.deepEqual(await agentHealth(ctxOf(refused)), {
    serving: false,
    detail: "could not connect to the Cloudflare edge",
  });

  const unset = world();
  (unset.env as { SECRETS: Record<string, string> }).SECRETS = {};
  assert.match((await agentHealth(ctxOf(unset))).detail ?? "", /token/);
});

test("health replaces a socket whose session this isolate lost or that serves another name", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const kept = w.open[0]!;
  const other = (await w.binding.connect(EDGE_URLS[0], {
    attachment: { hostname: "old.example.com" },
  })) as FakeSocket;
  assert.deepEqual(await agentHealth(ctxOf(w)), { serving: true });
  assert.equal(kept.closedWith, null, "the live session is kept");
  assert.equal(other.closedWith, CLOSE_REPLACED);
});

test("stop closes the connection for good", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const ws = w.open[0]!;
  await stopAgent(w.binding);
  assert.equal(ws.closedWith, CLOSE_STOPPED);
  assert.equal(sessionOf(ws.id), undefined);
  await handleClose(ctxOf(w), ws, CLOSE_STOPPED);
  assert.equal(w.connects.length, 1);
});

test("a message for a lost session closes the socket, and that close reconnects", async () => {
  const w = world();
  const ws = (await w.binding.connect(EDGE_URLS[0], {
    attachment: { hostname: HOSTNAME },
  })) as FakeSocket;
  await handleMessage(w.env, ws, new Uint8Array([1, 2, 3]).buffer);
  assert.equal(ws.closedWith, CLOSE_RESTARTED);
  await handleClose(ctxOf(w), ws, CLOSE_RESTARTED);
  await w.settle();
  assert.equal(w.connects.length, 2);
  assert.equal(sessionOf(w.open[0]!.id)?.phase, "serving");
});

test("closes: the edge ending the stream reconnects; failed, replaced and foreign ones don't", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const ws = w.open[0]!;
  await handleClose(ctxOf(w), ws, 1000);
  assert.equal(w.connects.length, 2);
  for (const code of [CLOSE_FAILED, CLOSE_REPLACED, CLOSE_STOPPED]) {
    await handleClose(ctxOf(w), ws, code);
  }
  assert.equal(w.connects.length, 2);
  const foreign = { ...ws, deserializeAttachment: () => null } as HostWebSocket;
  await handleClose(ctxOf(w), foreign, 1006);
  assert.equal(w.connects.length, 2);
  const refused = world({}, "unavailable");
  await handleClose(ctxOf(refused), ws, 1006);
  assert.equal(refused.connects.length, 4);
});

test("a session the edge ends, that breaks the protocol or is refused late is closed", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const ws = w.open[0]!;
  await handleMessage(w.env, ws, encodeGoAway(0, 0, "bye").slice().buffer);
  assert.equal(ws.closedWith, CLOSE_BROKEN);

  await startAgent(ctxOf(w));
  const next = w.open[0]!;
  // DATA on stream 0 is a connection error.
  await handleMessage(w.env, next, encodeFrame(FrameType.Data, 0, 0, bytes("x")).slice().buffer);
  assert.equal(next.closedWith, CLOSE_BROKEN);

  await startAgent(ctxOf(w));
  const third = w.open[0]!;
  // The edge aborts the control channel.
  await handleMessage(w.env, third, encodeData(1, hex(EDGE.abort)).slice().buffer);
  assert.equal(third.closedWith, CLOSE_BROKEN);

  await startAgent(ctxOf(w));
  const fourth = w.open[0]!;
  // An answer that says the registration failed after all.
  await handleMessage(w.env, fourth, encodeData(1, hex(EDGE.unimplemented)).slice().buffer);
  assert.equal(fourth.closedWith, CLOSE_FAILED);
});

/** The fields of a WebSocket request from the edge. */
const UPGRADE: Array<[string, string]> = [
  ["cf-cloudflared-proxy-connection-upgrade", "websocket"],
  ["sec-websocket-version", "13"],
  ["sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="],
];

test("an upgrade opens a library socket that carries the client's stream both ways", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const agent = w.open[0]!;
  await handleMessage(w.env, agent, request(3, "/api/live", UPGRADE, false));
  assert.deepEqual(w.upgrades, [
    { attachment: { agent: agent.id, stream: 3 }, clientAddress: "203.0.113.9" },
  ]);
  const live = w.open.find((ws) => ws.id.startsWith("live"))!;
  assert.deepEqual(liveOf(live), { agent: agent.id, stream: 3 });
  assert.equal(liveOf(agent), null);
  assert.equal(streamOf(w, agent, 3).heads[0]?.[0]?.[1], "200");

  // A masked "hi" from the client reaches the library.
  await handleMessage(
    w.env,
    agent,
    encodeData(3, new Uint8Array([0x81, 0x82, 1, 2, 3, 4, 0x68 ^ 1, 0x69 ^ 2])).slice().buffer,
  );
  assert.deepEqual(w.liveSent.get(live.id), ["hi"]);

  // The library's message and close reach the client.
  await handleMessage(w.env, live, "news");
  assert.ok(latin1(streamOf(w, agent, 3).body).endsWith("\x81\x04news"));
  await handleClose(ctxOf(w), live, 1000, "done");
  const end = streamOf(w, agent, 3);
  assert.ok(latin1(end.body).endsWith("\x88\x06\x03\xe8done"));
  assert.ok(end.ended);
  assert.equal(w.connects.length, 1, "a library socket's close does not reconnect");

  // A message for a stream that ended closes the library socket.
  await handleMessage(w.env, live, "late");
  assert.equal(live.closedWith, 1001);
});

test("library sockets close with their session, and health sweeps orphans", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const agent = w.open[0]!;
  await handleMessage(w.env, agent, request(3, "/api/live", UPGRADE, false));
  const live = w.open.find((ws) => ws.id.startsWith("live"))!;
  // One whose session this isolate never held (it restarted).
  const orphan = (await w.env.ORIGIN!.connect(new Request("https://x/"), {
    attachment: { agent: "gone", stream: 5 },
  })) as { socket: FakeSocket };
  assert.deepEqual(await agentHealth(ctxOf(w)), { serving: true });
  assert.equal(orphan.socket.closedWith, 1001);
  assert.equal(live.closedWith, null, "the held session's socket is kept");
  // Its messages and close find no session either.
  const stray = (await w.env.ORIGIN!.connect(new Request("https://x/"), {
    attachment: { agent: "gone", stream: 7 },
  })) as { socket: FakeSocket };
  await handleMessage(w.env, stray.socket, "x");
  assert.equal(stray.socket.closedWith, 1001);
  await handleClose(ctxOf(w), stray.socket, 1000, "");

  // The edge ending the connector's stream closes the library sockets it carried.
  await handleClose(ctxOf(w), agent, 1000);
  assert.equal(live.closedWith, 1001);
  assert.equal(w.connects.length, 2);

  // So does a session that breaks the protocol.
  await w.settle();
  const next = w.open.find((ws) => !ws.id.startsWith("live") && ws !== agent)!;
  await handleMessage(w.env, next, request(3, "/api/live", UPGRADE, false));
  const second = w.open.find((ws) => ws.id.startsWith("live"))!;
  await handleMessage(w.env, next, encodeFrame(FrameType.Data, 0, 0, bytes("x")).slice().buffer);
  assert.equal(second.closedWith, 1001);
});

test("writes to the edge that fail", async () => {
  // The first write fails: start reports the edge unavailable.
  const broken = world();
  const connect = broken.binding.connect.bind(broken.binding);
  broken.binding.connect = async (url, options) => {
    const ws = (await connect(url, options)) as FakeSocket;
    ws.send = () => Promise.reject(new Error("broken pipe"));
    return ws;
  };
  await assert.rejects(startAgent(ctxOf(broken)), /could not connect to the Cloudflare edge/);
  assert.ok(broken.connects.length > 0);

  // A ping that cannot be written reports the connection closed.
  const w = world();
  await startAgent(ctxOf(w));
  const agent = w.open[0]!;
  await handleMessage(w.env, agent, request(3, "/api/live", UPGRADE, false));
  const live = w.open.find((ws) => ws.id.startsWith("live"))!;
  const send = agent.send;
  agent.send = () => Promise.reject(new Error("broken pipe"));
  assert.deepEqual(await agentHealth(ctxOf(w)), {
    serving: false,
    detail: "the connection to the Cloudflare edge closed",
  });
  // A library message whose write to the edge fails closes the library socket.
  await handleMessage(w.env, live, "news");
  assert.equal(live.closedWith, 1001);
  agent.send = send;

  // A library message whose connector socket is gone from the host closes it too.
  const x = world();
  await startAgent(ctxOf(x));
  const host = x.open[0]!;
  await handleMessage(x.env, host, request(3, "/api/live", UPGRADE, false));
  const other = x.open.find((ws) => ws.id.startsWith("live"))!;
  x.open.splice(x.open.indexOf(host), 1);
  await handleMessage(x.env, other, "news");
  assert.equal(other.closedWith, 1001);
});

test("without ORIGIN, requests and upgrades are answered 502", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const agent = w.open[0]!;
  delete (w.env as { ORIGIN?: unknown }).ORIGIN;
  await handleMessage(w.env, agent, request(3, "/"));
  await handleMessage(w.env, agent, request(5, "/api/live", UPGRADE, false));
  assert.equal(streamOf(w, agent, 3).heads[0]?.[0]?.[1], "502");
  assert.equal(streamOf(w, agent, 5).heads[0]?.[0]?.[1], "502");
  assert.equal(ioFor(w.env, agent).library.socket("unknown"), null);
  // A stream the edge reset after the answer is closed; the session goes on.
  await handleMessage(
    w.env,
    agent,
    encodeFrame(FrameType.RstStream, 0, 5, new Uint8Array([0, 0, 0, ErrorCode.Cancel])).slice()
      .buffer,
  );
  assert.equal(agent.closedWith, null);
  assert.equal(Flag.EndStream, 0x1);
});

test("the entrypoint classes call the lifecycle", async () => {
  const w = world();
  const ingress = new Ingress({} as never, w.env as never);
  assert.deepEqual(await ingress.start(), { origin: `https://${HOSTNAME}` });
  assert.deepEqual(await ingress.health(), { serving: true });
  const plugin = new Plugin({} as never, w.env as never);
  const ws = w.open[0]!;
  assert.deepEqual(await plugin.webSocketMessage(ws, "ignored"), {});
  await ingress.stop();
  assert.equal(ws.closedWith, CLOSE_STOPPED);
  await plugin.webSocketClose(ws, CLOSE_STOPPED, "ingress stopped");
  assert.equal(w.connects.length, 1);
});

test("VERSION matches plugin.toml and package.json", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  // Compared as text: no regular expression built from the version.
  assert.ok(manifest.split("\n").includes(`version = "${VERSION}"`));
  const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
  assert.equal(pkg.version, VERSION);
});
