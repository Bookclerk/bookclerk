/**
 * The ingress lifecycle and the socket handlers, with the SDK's isolate
 * module replaced by stand-ins (the real one imports `cloudflare:workers`,
 * which only exists inside workerd), fake sockets and a small ngrok
 * emulator behind them.
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

import { StreamType, readType } from "../src/messages.ts";
import { FrameReader, encodeData, encodeGoAway, type Frame } from "../src/mux.ts";
import { bytes, proxy, text } from "./fake.ts";

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
const {
  CLOSE_BROKEN,
  CLOSE_FAILED,
  CLOSE_REPLACED,
  CLOSE_RESTARTED,
  CLOSE_STOPPED,
  NGROK_AGENT_URL,
  agentHealth,
  handleClose,
  handleMessage,
  sessionOf,
  settingsOf,
  startAgent,
  stopAgent,
} = await import("../src/lifecycle.ts");
const { NGROK_ROOT_CA } = await import("../src/ca.ts");
const { VERSION } = await import("../src/version.ts");

/** The domain these tests bind. */
const DOMAIN = "shelf.ngrok-free.app";

/** How the emulator answers. */
interface Behaviour {
  /** Auth reply error, or empty. */
  authError: string;
  /** Bind reply error, or empty. */
  bindError: string;
  /** Whether heartbeats are echoed. */
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
}

/**
 * Builds the fake world. Each socket feeds what the plugin writes to a
 * small ngrok emulator, whose answers are delivered to `handleMessage`
 * on the next turn, one at a time, as the host does.
 *
 * @param partial - Emulator behaviour overrides.
 * @param refuse - Whether `connect` fails.
 * @returns The world.
 */
function world(partial: Partial<Behaviour> = {}, refuse = false): World {
  const behaviour: Behaviour = {
    authError: "",
    bindError: "",
    echo: true,
    silent: false,
    ...partial,
  };
  const open: FakeSocket[] = [];
  const connects: World["connects"] = [];
  const seen: Request[] = [];
  const written = new Map<string, Uint8Array[]>();
  let minted = 0;
  let queue = Promise.resolve();
  const env = {
    CONFIG: { domain: DOMAIN },
    SECRETS: { authtoken: "tok_secret" },
    SOCKETS: undefined as unknown as SocketsBinding,
    ORIGIN: {
      fetch: (request: Request) => {
        seen.push(request);
        return Promise.resolve(new Response("library says hi"));
      },
      connect: () => Promise.reject(new Error("no upgrades")),
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
   * Makes a socket that answers like ngrok.
   *
   * @param url - Its URL.
   * @param attachment - Its attachment.
   * @returns The socket.
   */
  const socket = (url: string, attachment: unknown): FakeSocket => {
    const reader = new FrameReader();
    const kinds = new Map<number, number>();
    const id = `s${minted++}`;
    written.set(id, []);
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
        for (const frame of reader.push(chunk)) answer(ws, kinds, frame);
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
   * The emulator: answers auth, bind and heartbeats.
   *
   * @param ws - The socket.
   * @param kinds - Stream types by id.
   * @param frame - A frame the plugin wrote.
   */
  const answer = (ws: FakeSocket, kinds: Map<number, number>, frame: Frame): void => {
    if (frame.kind !== "data") return;
    let data = frame.data;
    if (frame.syn) {
      kinds.set(frame.stream, readType(data));
      data = data.subarray(4);
    }
    const kind = kinds.get(frame.stream);
    if (kind === StreamType.Auth && frame.syn) {
      deliver(
        ws,
        encodeData(
          frame.stream,
          bytes(JSON.stringify({ ClientId: "c", Error: behaviour.authError })),
        ),
      );
    } else if (kind === StreamType.Bind && frame.syn) {
      deliver(
        ws,
        encodeData(
          frame.stream,
          bytes(
            JSON.stringify({
              Id: "bookclerk",
              URL: `https://${DOMAIN}`,
              Error: behaviour.bindError,
            }),
          ),
        ),
      );
    } else if (kind === StreamType.Heartbeat && data.length === 4 && behaviour.echo) {
      deliver(ws, encodeData(frame.stream, data));
    }
  };
  const binding: SocketsBinding = {
    connect(url, options) {
      connects.push({ url, options });
      if (refuse) return Promise.reject(new PluginError("forbidden", "no grant"));
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
    settle: async () => {
      for (let i = 0; i < 20; i++) {
        await queue;
        await new Promise((resolve) => setImmediate(resolve));
      }
    },
  };
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
    beat: () => bytes("beat"),
    startWaitMs: waitMs,
    beatWaitMs: waitMs,
  };
}

test("settings: domain is required and must be a host name; the authtoken is required", () => {
  const w = world();
  assert.deepEqual(settingsOf(w.env), { domain: DOMAIN, authtoken: "tok_secret" });
  const env = (config: Record<string, unknown>, secrets: Record<string, string>) =>
    ({ ...w.env, CONFIG: config, SECRETS: secrets }) as typeof w.env;
  assert.equal(
    settingsOf(env({ domain: " Shelf.Ngrok-Free.App. " }, { authtoken: "t" })).domain,
    DOMAIN,
  );
  assert.throws(() => settingsOf(env({ domain: "" }, { authtoken: "t" })), /set domain/);
  assert.throws(
    () => settingsOf(env({ domain: "https://x.ngrok.app" }, { authtoken: "t" })),
    /host name/,
  );
  assert.throws(() => settingsOf(env({ domain: DOMAIN }, {})), /authtoken/);
});

test("start connects over TLS with ngrok's CA, binds and answers the domain's origin", async () => {
  const w = world();
  const started = await startAgent(ctxOf(w));
  assert.deepEqual(started, { origin: `https://${DOMAIN}` });
  assert.equal(w.connects.length, 1);
  assert.equal(w.connects[0]?.url, NGROK_AGENT_URL);
  assert.deepEqual(w.connects[0]?.options?.trustAnchors, [NGROK_ROOT_CA]);
  assert.deepEqual(w.connects[0]?.options?.attachment, { domain: DOMAIN });
  assert.equal(sessionOf(w.open[0]!.id)?.phase, "serving");
  // The authtoken goes to ngrok only inside the TLS stream.
  assert.ok(!JSON.stringify(w.connects).includes("tok_secret"));
});

test("start fails with ngrok's reason and closes the socket without reconnecting", async () => {
  const w = world({ authError: "ERR_NGROK_105 invalid authtoken" });
  await assert.rejects(startAgent(ctxOf(w)), (err: PluginError) => {
    assert.equal(err.code, "unavailable");
    assert.match(err.message, /ERR_NGROK_105/);
    return true;
  });
  await w.settle();
  assert.equal(w.open.length, 0);

  const silent = world({ silent: true });
  await assert.rejects(startAgent(ctxOf(silent, 20)), /did not bring the endpoint online/);

  const refused = world({}, true);
  // The host's own refusal (a missing grant, say) passes through.
  await assert.rejects(startAgent(ctxOf(refused)), /no grant/);
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
  const before = w.written.get(ws.id)!.length;
  await handleMessage(w.env, ws, proxy(2, "GET /hello HTTP/1.1\r\nHost: x\r\n\r\n").slice().buffer);
  assert.equal(w.seen.length, 1);
  assert.equal(new URL(w.seen[0]!.url).pathname, "/hello");
  const reader = new FrameReader();
  const frames = w.written
    .get(ws.id)!
    .slice(before)
    .flatMap((chunk) => reader.push(chunk));
  const out = frames
    .filter((f): f is Extract<Frame, { kind: "data" }> => f.kind === "data" && f.stream === 2)
    .map((f) => text(f.data))
    .join("");
  assert.match(out, /^HTTP\/1\.1 200 OK\r\n[\s\S]*library says hi$/);
  // A string message (never sent on a stream) is ignored.
  await handleMessage(w.env, ws, "text");
});

test("health: a bound session that echoes a heartbeat serves", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  assert.deepEqual(await agentHealth(ctxOf(w)), { serving: true });

  w.behaviour.echo = false;
  assert.deepEqual(await agentHealth(ctxOf(w, 20)), {
    serving: false,
    detail: "ngrok did not answer a heartbeat",
  });
});

test("health reconnects when no session is held, and reports a refused one", async () => {
  const w = world();
  assert.deepEqual(await agentHealth(ctxOf(w)), { serving: true });
  assert.equal(w.connects.length, 1);

  const bad = world({ bindError: "ERR_NGROK_320 reserved" });
  const health = await agentHealth(ctxOf(bad));
  assert.equal(health.serving, false);
  assert.match(health.detail ?? "", /ERR_NGROK_320/);

  const refused = world({}, true);
  assert.deepEqual(await agentHealth(ctxOf(refused)), {
    serving: false,
    detail: "could not connect to ngrok",
  });

  const unset = world();
  (unset.env as { SECRETS: Record<string, string> }).SECRETS = {};
  assert.match((await agentHealth(ctxOf(unset))).detail ?? "", /authtoken/);
});

test("health replaces a socket whose session this isolate lost", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const stale = w.open[0]!;
  // Another domain's socket (settings changed) is not reused either.
  const other = (await w.binding.connect(NGROK_AGENT_URL, {
    attachment: { domain: "old.ngrok.app" },
  })) as FakeSocket;
  assert.deepEqual(await agentHealth(ctxOf(w)), { serving: true });
  assert.equal(stale.closedWith, null, "the live session is kept");
  assert.equal(other.closedWith, CLOSE_REPLACED);
});

test("stop closes the connection for good", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const ws = w.open[0]!;
  await stopAgent(w.binding);
  assert.equal(ws.closedWith, CLOSE_STOPPED);
  assert.equal(sessionOf(ws.id), undefined);
  await handleClose(w.env, ws, CLOSE_STOPPED);
  assert.equal(w.connects.length, 1);
});

test("a message for a lost session closes the socket, and that close reconnects", async () => {
  const w = world();
  const ws = (await w.binding.connect(NGROK_AGENT_URL, {
    attachment: { domain: DOMAIN },
  })) as FakeSocket;
  await handleMessage(w.env, ws, new Uint8Array([1, 2, 3]).buffer);
  assert.equal(ws.closedWith, CLOSE_RESTARTED);
  await handleClose(w.env, ws, CLOSE_RESTARTED);
  await w.settle();
  assert.equal(w.connects.length, 2);
  assert.equal(sessionOf(w.open[0]!.id)?.phase, "serving");
});

test("closes: ngrok ending the stream reconnects; failed, replaced and foreign ones don't", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const ws = w.open[0]!;
  await handleClose(w.env, ws, 1000);
  assert.equal(w.connects.length, 2);
  for (const code of [CLOSE_FAILED, CLOSE_REPLACED, CLOSE_STOPPED]) {
    await handleClose(w.env, ws, code);
  }
  assert.equal(w.connects.length, 2);
  const foreign = { ...ws, deserializeAttachment: () => null } as HostWebSocket;
  await handleClose(w.env, foreign, 1006);
  assert.equal(w.connects.length, 2);
  const refused = world({}, true);
  await handleClose(refused.env, ws, 1006);
  assert.equal(refused.connects.length, 1);
});

test("a session that ngrok ends or that breaks the protocol is closed to reconnect", async () => {
  const w = world();
  await startAgent(ctxOf(w));
  const ws = w.open[0]!;
  await handleMessage(w.env, ws, encodeGoAway(0, 0, "bye").slice().buffer);
  assert.equal(ws.closedWith, CLOSE_BROKEN);

  await startAgent(ctxOf(w));
  const next = w.open[0]!;
  await handleMessage(w.env, next, encodeData(3, bytes("x"), 0x2).slice().buffer);
  assert.equal(next.closedWith, CLOSE_BROKEN);

  await startAgent(ctxOf(w));
  const third = w.open[0]!;
  await handleMessage(
    w.env,
    third,
    encodeData(2, new Uint8Array([0, 0, 0, StreamType.StopTunnel, ...bytes("{}")]), 0x2).slice()
      .buffer,
  );
  assert.equal(third.closedWith, CLOSE_FAILED);
});

test("the entrypoint classes call the lifecycle", async () => {
  const w = world();
  const ingress = new Ingress({} as never, w.env as never);
  assert.deepEqual(await ingress.start(), { origin: `https://${DOMAIN}` });
  assert.deepEqual(await ingress.health(), { serving: true });
  const plugin = new Plugin({} as never, w.env as never);
  const ws = w.open[0]!;
  assert.deepEqual(await plugin.webSocketMessage(ws, "ignored"), {});
  await ingress.stop();
  assert.equal(ws.closedWith, CLOSE_STOPPED);
  await plugin.webSocketClose(ws, CLOSE_STOPPED);
  assert.equal(w.connects.length, 1);
});

test("VERSION matches plugin.toml", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  assert.match(manifest, new RegExp(`^version = "${VERSION.replace(/\./g, "\\.")}"$`, "m"));
});
