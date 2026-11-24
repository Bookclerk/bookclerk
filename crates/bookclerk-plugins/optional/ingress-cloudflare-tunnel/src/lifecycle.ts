/**
 * The Cloudflare Tunnel ingress's lifecycle and socket handling, apart from
 * the entrypoint classes.
 *
 * workerd treats every named export of a Worker's main module as an
 * entrypoint, so `index.ts` exports only the classes and this module holds
 * the logic they call, which the tests drive with fakes.
 *
 * The connector's connection is one host-held `tls://` stream to a
 * Cloudflare edge region. Its session (HTTP/2 streams, windows, the HPACK
 * table, half-read requests) lives in this isolate's memory, keyed by the
 * socket's id: it is far larger than a socket attachment may be. When the
 * isolate starts again while the stream is open, the session is gone, so
 * the plugin closes the stream and opens a new one, which registers again
 * and replaces the old registration.
 *
 * A client's WebSocket rides on a socket to the library that
 * `ORIGIN.connect` opened; its attachment names the connector's socket and
 * the stream, so the library's messages and close find the session. When
 * the connector's session ends, those sockets close with it.
 */

import {
  PluginError,
  type HostWebSocket,
  type IngressHealth,
  type IngressStarted,
  type SocketMessageData,
  type SocketsBinding,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { Session, type SessionIo } from "./agent.ts";
import { CLOUDFLARE_ROOT_CAS } from "./ca.ts";
import { H2Error } from "./h2.ts";
import { VERSION } from "./version.ts";

/** The edge regions, in the order they are tried (cloudflared's SRV targets). */
export const EDGE_URLS = [
  "tls://region1.v2.argotunnel.com:7844",
  "tls://region2.v2.argotunnel.com:7844",
] as const;

/** The name the edge answers HTTP/2 tunnel connections for. */
export const EDGE_SERVER_NAME = "h2.cftunnel.com";

/** Close code for a stream the plugin closed because the ingress stopped. */
export const CLOSE_STOPPED = 4001;

/** Close code for a connection Cloudflare refused (token) or stopped. */
export const CLOSE_FAILED = 4002;

/** Close code for a stream whose session this isolate no longer holds. */
export const CLOSE_RESTARTED = 4003;

/** Close code for a session that broke the protocol or that the edge ended. */
export const CLOSE_BROKEN = 4004;

/** Close code for a stream `health` replaced with a new one. */
export const CLOSE_REPLACED = 4005;

/** How long `start` waits for the tunnel to register, in milliseconds. */
export const START_WAIT_MS = 20_000;

/** How long `health` waits for a ping's answer, in milliseconds. */
export const BEAT_WAIT_MS = 5_000;

/** Pause between checks while `start` or `health` waits, in milliseconds. */
const POLL_MS = 100;

/** What the connector's socket keeps as its attachment. */
interface AgentAttachment {
  /** The public hostname the session serves. */
  hostname: string;
}

/** What a socket to the library keeps as its attachment. */
interface LiveAttachment {
  /** The connector's socket whose session carries the client's side. */
  agent: string;
  /** The HTTP/2 stream of the client's side. */
  stream: number;
}

/** Close code for a socket to the library whose client side ended. */
const CLOSE_GOING_AWAY = 1001;

/** Sessions by socket id; lost when the isolate starts again. */
const sessions = new Map<string, Session>();

/** A tunnel token, decoded. */
export interface TunnelToken {
  /** The account the tunnel belongs to. */
  accountTag: string;
  /** The tunnel's secret. */
  secret: Uint8Array;
  /** The tunnel's id, 16 bytes. */
  tunnelId: Uint8Array;
}

/** The plugin's settings. */
export interface Settings {
  /** The public hostname routed to the tunnel. */
  hostname: string;
  /** The tunnel's credentials. */
  token: TunnelToken;
}

/** A DNS name: letters, digits and hyphens in dot-separated labels. */
const DOMAIN =
  /^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/**
 * Whether `name` is a DNS name the plugin can serve.
 *
 * @param name - Candidate, lowercase.
 * @returns `true` for dot-separated LDH labels, 253 characters at most.
 */
export function hostnameOk(name: string): boolean {
  return DOMAIN.test(name);
}

/** A tunnel id: a UUID. */
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * Bytes from standard or URL-safe base64, padded or not.
 *
 * @param text - The base64.
 * @returns The bytes, or `null` when it is not base64.
 */
function fromBase64(text: string): Uint8Array | null {
  const normal = text.replace(/-/g, "+").replace(/_/g, "/").replace(/=+$/, "");
  if (!/^[A-Za-z0-9+/]*$/.test(normal) || normal.length % 4 === 1) return null;
  // The checks above leave only input `atob` accepts.
  const binary = atob(normal + "=".repeat((4 - (normal.length % 4)) % 4));
  return Uint8Array.from(binary, (c) => c.charCodeAt(0));
}

/**
 * Decodes a tunnel token: base64 of `{"a": account, "t": tunnel id,
 * "s": base64 secret}`, as the Cloudflare dashboard shows it. The whole
 * `cloudflared service install <token>` command is accepted too.
 *
 * @param raw - The token as pasted.
 * @returns The credentials, or a message saying what is wrong.
 */
export function parseToken(raw: string): TunnelToken | string {
  const word = raw.trim().split(/\s+/).pop() ?? "";
  const json = fromBase64(word);
  if (json === null || json.length === 0 || json.length > 4096) {
    return "the tunnel token is not the base64 token the Cloudflare dashboard shows";
  }
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(json));
  } catch {
    return "the tunnel token does not decode to the expected JSON";
  }
  if (typeof value !== "object" || value === null) {
    return "the tunnel token does not decode to the expected JSON";
  }
  const { a, t, s, e } = value as Record<string, unknown>;
  if (e !== undefined && e !== "") {
    return "tunnels on a dedicated Cloudflare endpoint (such as FedRAMP) are not supported";
  }
  if (typeof a !== "string" || !/^[0-9a-f]{32}$/i.test(a)) {
    return "the tunnel token has no account tag";
  }
  if (typeof t !== "string" || !UUID.test(t)) return "the tunnel token has no tunnel id";
  const secret = typeof s === "string" ? fromBase64(s) : null;
  if (secret === null || secret.length < 32 || secret.length > 256) {
    return "the tunnel token has no tunnel secret";
  }
  const hex = t.replace(/-/g, "");
  const tunnelId = Uint8Array.from({ length: 16 }, (_, i) =>
    Number.parseInt(hex.slice(2 * i, 2 * i + 2), 16),
  );
  return { accountTag: a.toLowerCase(), secret, tunnelId };
}

/**
 * Reads the settings.
 *
 * @param env - The plugin's bindings.
 * @returns Hostname and token.
 * @throws {PluginError} `invalid_params` when one is missing or malformed.
 */
export function settingsOf(env: Env): Settings {
  const hostname = String(env.CONFIG.hostname ?? "")
    .trim()
    .toLowerCase()
    .replace(/^https:\/\//, "")
    .replace(/\/$/, "")
    .replace(/\.$/, "");
  const raw = String(env.SECRETS["token"] ?? "").trim();
  if (!hostname) {
    throw PluginError.fromWire(
      "invalid_params",
      "set hostname to the public hostname your tunnel routes to the library",
    );
  }
  if (!hostnameOk(hostname)) {
    throw PluginError.fromWire(
      "invalid_params",
      "hostname must be a host name such as library.example.com",
    );
  }
  if (!raw) {
    throw PluginError.fromWire("invalid_params", "set the token secret to your tunnel token");
  }
  const token = parseToken(raw);
  if (typeof token === "string") throw PluginError.fromWire("invalid_params", token);
  return { hostname, token };
}

/**
 * The hostname a socket's session serves.
 *
 * @param ws - The socket.
 * @returns The hostname, or `null` for a socket that is not the connector's.
 */
export function hostnameOf(ws: HostWebSocket): string | null {
  const attachment = ws.deserializeAttachment() as Partial<AgentAttachment> | null;
  return typeof attachment?.hostname === "string" ? attachment.hostname : null;
}

/**
 * The client stream a socket to the library carries.
 *
 * @param ws - The socket.
 * @returns Its attachment, or `null` for the connector's own socket.
 */
export function liveOf(ws: HostWebSocket): LiveAttachment | null {
  const attachment = ws.deserializeAttachment() as Partial<LiveAttachment> | null;
  return typeof attachment?.agent === "string" && Number.isSafeInteger(attachment.stream)
    ? { agent: attachment.agent, stream: attachment.stream as number }
    : null;
}

/**
 * Closes the sockets to the library that a connector socket's session
 * carried.
 *
 * @param sockets - The socket binding.
 * @param agent - The connector socket's id.
 * @returns Resolves once each close was sent.
 */
async function closeLive(sockets: SocketsBinding, agent: string): Promise<void> {
  for (const socket of await sockets.getWebSockets().catch(() => [])) {
    if (liveOf(socket)?.agent === agent) {
      await socket.close(CLOSE_GOING_AWAY, "the tunnel connection ended").catch(() => {});
    }
  }
}

/**
 * What a session call reaches through this invocation's bindings.
 *
 * @param env - The plugin's bindings.
 * @param ws - The connector's socket.
 * @returns The connection and the library.
 */
export function ioFor(env: Env, ws: HostWebSocket): SessionIo {
  return {
    write: (bytes) => ws.send(bytes),
    library: {
      fetch: (request, clientAddress) => {
        if (!env.ORIGIN) {
          return Promise.reject(new Error("the library is not reachable through this ingress"));
        }
        return env.ORIGIN.fetch(request, clientAddress ? { clientAddress } : undefined);
      },
      connect: (request, clientAddress, stream) => {
        if (!env.ORIGIN) {
          return Promise.reject(new Error("the library is not reachable through this ingress"));
        }
        return env.ORIGIN.connect(request, {
          attachment: { agent: ws.id, stream } satisfies LiveAttachment,
          ...(clientAddress ? { clientAddress } : {}),
        });
      },
      socket: (id) => {
        try {
          return env.SOCKETS.getWebSocket(id);
        } catch {
          return null;
        }
      },
    },
  };
}

/**
 * The session of a socket, for tests.
 *
 * @param id - The socket's id.
 * @returns Its session, if this isolate holds one.
 */
export function sessionOf(id: string): Session | undefined {
  return sessions.get(id);
}

/** Everything the ingress methods reach, so tests can pass fakes. */
export interface IngressContext {
  /** The plugin's bindings. */
  env: Env;
  /** Waits, between checks. */
  sleep: (ms: number) => Promise<void>;
  /** `n` random bytes (ping payloads and the connector id). */
  random: (n: number) => Uint8Array;
  /** How long `start` waits. */
  startWaitMs: number;
  /** How long `health` waits for a ping's answer. */
  beatWaitMs: number;
}

/**
 * Opens a socket to the first edge region that answers: with Cloudflare's
 * Origin CA roots first, as cloudflared trusts them, then with the public
 * roots.
 *
 * @param env - The plugin's bindings.
 * @param attachment - The socket's attachment.
 * @returns The socket.
 * @throws {PluginError} The last refusal when no region answers.
 */
async function dialEdge(env: Env, attachment: AgentAttachment): Promise<HostWebSocket> {
  let last: unknown = null;
  for (const url of EDGE_URLS) {
    for (const trustAnchors of [CLOUDFLARE_ROOT_CAS, []]) {
      try {
        return await env.SOCKETS.connect(url, {
          serverName: EDGE_SERVER_NAME,
          trustAnchors: [...trustAnchors],
          attachment,
        });
      } catch (err) {
        last = err;
        // A refusal of the settings or the grant won't change with roots or region.
        if (err instanceof PluginError && err.wireCode !== "unavailable") throw err;
      }
    }
  }
  throw last instanceof PluginError
    ? last
    : PluginError.fromWire("unavailable", "could not reach the Cloudflare edge");
}

/**
 * Opens the connector's connection and sends the HTTP/2 preface; the edge
 * then opens the control stream and the session registers.
 *
 * @param ctx - Bindings and randomness.
 * @param settings - Hostname and token.
 * @param attempts - Registrations that failed before this one.
 * @returns The socket and its session.
 */
export async function connectAgent(
  ctx: IngressContext,
  settings: Settings,
  attempts = 0,
): Promise<{ ws: HostWebSocket; session: Session }> {
  const ws = await dialEdge(ctx.env, { hostname: settings.hostname });
  const session = new Session({
    hostname: settings.hostname,
    registration: {
      accountTag: settings.token.accountTag,
      tunnelSecret: settings.token.secret,
      tunnelId: settings.token.tunnelId,
      connIndex: 0,
      clientId: ctx.random(16),
      version: `bookclerk-${VERSION}`,
      arch: "workerd",
      replaceExisting: true,
      previousAttempts: Math.min(attempts, 255),
    },
  });
  sessions.set(ws.id, session);
  try {
    await session.start(ioFor(ctx.env, ws));
  } catch (err) {
    sessions.delete(ws.id);
    await ws.close(CLOSE_BROKEN, "could not write to the edge").catch(() => {});
    throw err;
  }
  return { ws, session };
}

/**
 * Closes every connector socket.
 *
 * @param sockets - The socket binding.
 * @param code - Close code.
 * @param reason - Close reason.
 * @returns Resolves once each close was sent.
 */
async function closeAll(sockets: SocketsBinding, code: number, reason: string): Promise<void> {
  for (const ws of await sockets.getWebSockets().catch(() => [])) {
    sessions.delete(ws.id);
    await ws.close(code, reason).catch(() => {});
  }
}

/**
 * Waits until a session is registered or has failed.
 *
 * @param ctx - Waits.
 * @param session - The session.
 * @param waitMs - How long to wait.
 * @returns Resolves when it serves, failed or closed, or the time is up.
 */
async function settled(ctx: IngressContext, session: Session, waitMs: number): Promise<void> {
  const deadline = Date.now() + waitMs;
  while (
    (session.phase === "connecting" || session.phase === "registering") &&
    Date.now() < deadline
  ) {
    await ctx.sleep(POLL_MS);
  }
}

/**
 * Connects to Cloudflare, registers the tunnel connection and returns the
 * public origin.
 *
 * @param ctx - Bindings and waits.
 * @returns The public origin.
 * @throws {PluginError} `invalid_params` for bad settings; `unavailable`
 *   when the edge can't be reached, refuses the token or does not answer
 *   in time.
 */
export async function startAgent(ctx: IngressContext): Promise<IngressStarted> {
  const settings = settingsOf(ctx.env);
  await closeAll(ctx.env.SOCKETS, CLOSE_STOPPED, "restarting");
  let connected: { ws: HostWebSocket; session: Session };
  try {
    connected = await connectAgent(ctx, settings);
  } catch (err) {
    if (err instanceof PluginError) throw err;
    throw PluginError.fromWire("unavailable", "could not connect to the Cloudflare edge");
  }
  const { ws, session } = connected;
  await settled(ctx, session, ctx.startWaitMs);
  if (session.phase === "serving") return { origin: `https://${settings.hostname}` };
  const detail = session.error || "Cloudflare did not register the tunnel connection in time";
  sessions.delete(ws.id);
  await ws.close(CLOSE_FAILED, "tunnel not registered").catch(() => {});
  throw PluginError.fromWire("unavailable", detail);
}

/**
 * Reports whether the tunnel serves: the session is registered and the
 * edge answers a ping. A missing or failed session is replaced first.
 *
 * @param ctx - Bindings and waits.
 * @returns `serving`, with a detail for the operator when it is not.
 */
export async function agentHealth(ctx: IngressContext): Promise<IngressHealth> {
  let settings: Settings;
  try {
    settings = settingsOf(ctx.env);
  } catch (err) {
    return { serving: false, detail: err instanceof Error ? err.message : "bad settings" };
  }
  let ws: HostWebSocket | undefined;
  let session: Session | undefined;
  const all = await ctx.env.SOCKETS.getWebSockets();
  for (const socket of all) {
    if (liveOf(socket) !== null) continue;
    const held = sessions.get(socket.id);
    if (held && held.phase === "serving" && hostnameOf(socket) === settings.hostname && !session) {
      ws = socket;
      session = held;
      continue;
    }
    sessions.delete(socket.id);
    await socket.close(CLOSE_REPLACED, "replaced").catch(() => {});
  }
  for (const socket of all) {
    // A socket to the library whose session is gone carries nothing now.
    const live = liveOf(socket);
    if (live !== null && live.agent !== ws?.id) {
      await socket.close(CLOSE_GOING_AWAY, "the tunnel connection ended").catch(() => {});
    }
  }
  if (!ws || !session) {
    // The host asks only while the ingress is meant to serve: reconnect.
    try {
      ({ ws, session } = await connectAgent(ctx, settings));
    } catch {
      return { serving: false, detail: "could not connect to the Cloudflare edge" };
    }
    await settled(ctx, session, ctx.startWaitMs);
    if (session.phase !== "serving") {
      const detail = session.error || "Cloudflare did not register the tunnel connection in time";
      sessions.delete(ws.id);
      await ws.close(CLOSE_FAILED, "tunnel not registered").catch(() => {});
      return { serving: false, detail };
    }
  }
  let ticket: number;
  try {
    ticket = await session.beat(ioFor(ctx.env, ws), ctx.random(8));
  } catch {
    return { serving: false, detail: "the connection to the Cloudflare edge closed" };
  }
  const deadline = Date.now() + ctx.beatWaitMs;
  while (session.echoed < ticket && Date.now() < deadline) await ctx.sleep(POLL_MS);
  if (session.echoed >= ticket) return { serving: true };
  return { serving: false, detail: "the Cloudflare edge did not answer a ping" };
}

/**
 * Closes the connector's connection; the edge takes the tunnel connection
 * offline.
 *
 * @param sockets - The socket binding.
 * @returns Resolves once each close was sent.
 */
export async function stopAgent(sockets: SocketsBinding): Promise<void> {
  await closeAll(sockets, CLOSE_STOPPED, "ingress stopped");
}

/**
 * Waits for `ms` milliseconds.
 *
 * @param ms - The pause.
 * @returns Resolves after it.
 */
function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * The context for the live bindings.
 *
 * @param env - The plugin's bindings.
 * @returns The context with real waits and randomness.
 */
export function liveContext(env: Env): IngressContext {
  return {
    env,
    sleep,
    random: (n) => crypto.getRandomValues(new Uint8Array(n)),
    startWaitMs: START_WAIT_MS,
    beatWaitMs: BEAT_WAIT_MS,
  };
}

/**
 * Handles bytes the edge sent.
 *
 * @param env - The plugin's bindings.
 * @param ws - The connector's socket.
 * @param message - The bytes (a stream delivers only binary messages).
 * @returns Resolves once every frame they complete was handled.
 */
export async function handleMessage(
  env: Env,
  ws: HostWebSocket,
  message: SocketMessageData,
): Promise<void> {
  const live = liveOf(ws);
  if (live !== null) {
    await fromLibrary(env, ws, live, message);
    return;
  }
  if (typeof message === "string") return;
  const session = sessions.get(ws.id);
  if (!session) {
    // This isolate started after the session did: its state is gone.
    await ws.close(CLOSE_RESTARTED, "connector restarted").catch(() => {});
    return;
  }
  try {
    await session.receive(ioFor(env, ws), new Uint8Array(message));
  } catch (err) {
    sessions.delete(ws.id);
    const reason = err instanceof H2Error ? "protocol error" : "connector error";
    await ws.close(CLOSE_BROKEN, reason).catch(() => {});
    await closeLive(env.SOCKETS, ws.id);
    return;
  }
  if (session.phase === "failed") {
    sessions.delete(ws.id);
    await ws.close(CLOSE_FAILED, "tunnel not registered").catch(() => {});
    await closeLive(env.SOCKETS, ws.id);
  } else if (session.phase === "closed") {
    sessions.delete(ws.id);
    await ws.close(CLOSE_BROKEN, "connection ended").catch(() => {});
    await closeLive(env.SOCKETS, ws.id);
  }
}

/**
 * The connector socket a socket to the library rides on, with its session.
 *
 * @param env - The plugin's bindings.
 * @param live - The library socket's attachment.
 * @returns The connector's socket and session, or `null` when this isolate
 *   holds no session for it.
 */
function agentOf(env: Env, live: LiveAttachment): { ws: HostWebSocket; session: Session } | null {
  const session = sessions.get(live.agent);
  if (!session) return null;
  try {
    return { ws: env.SOCKETS.getWebSocket(live.agent), session };
  } catch {
    return null;
  }
}

/**
 * Passes a library message to the client stream its socket carries, or
 * closes the socket when that stream or its session is gone.
 *
 * @param env - The plugin's bindings.
 * @param ws - The socket to the library.
 * @param live - Its attachment.
 * @param message - The message.
 * @returns Resolves once it was passed on or the socket closed.
 */
async function fromLibrary(
  env: Env,
  ws: HostWebSocket,
  live: LiveAttachment,
  message: SocketMessageData,
): Promise<void> {
  const agent = agentOf(env, live);
  let carried = false;
  if (agent !== null) {
    try {
      carried = await agent.session.fromLibrary(ioFor(env, agent.ws), live.stream, message);
    } catch {
      // The write to the edge failed; the connector socket's own close follows.
    }
  }
  if (!carried) await ws.close(CLOSE_GOING_AWAY, "the client went away").catch(() => {});
}

/**
 * Handles a socket closing. A socket to the library passes its close to
 * the client. The connector's socket closes the library sockets it carried
 * and reconnects unless the plugin stopped it or Cloudflare refused the
 * registration.
 *
 * @param ctx - Bindings and randomness.
 * @param ws - The closed socket.
 * @param code - The close code (1000 when the edge ended the stream).
 * @param reason - The close reason.
 * @returns Resolves once handled.
 */
export async function handleClose(
  ctx: IngressContext,
  ws: HostWebSocket,
  code: number,
  reason = "",
): Promise<void> {
  const env = ctx.env;
  const live = liveOf(ws);
  if (live !== null) {
    const agent = agentOf(env, live);
    await agent?.session
      .libraryClosed(ioFor(env, agent.ws), live.stream, code, reason)
      .catch(() => {});
    return;
  }
  sessions.delete(ws.id);
  await closeLive(env.SOCKETS, ws.id);
  if (code === CLOSE_STOPPED || code === CLOSE_FAILED || code === CLOSE_REPLACED) return;
  if (hostnameOf(ws) === null) return;
  try {
    await connectAgent(ctx, settingsOf(env), 1);
  } catch {
    // `health` connects again.
  }
}
