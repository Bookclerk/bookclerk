/**
 * The relay ingress's lifecycle and socket handling, apart from the
 * entrypoint classes.
 *
 * workerd treats every named export of a Worker's main module as an
 * entrypoint, so `index.ts` exports only the classes and this module holds
 * the logic they call, which the tests drive with fakes.
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
import {
  DeployError,
  deployRelay,
  findRelayHost,
  relayToken,
  type Fetch,
  type RelayTarget,
} from "./cloudflare.ts";
import { RELAY_CONNECT_PATH, RELAY_HEALTH_PATH, RELAY_SOCKETS, parseNotice } from "./protocol.ts";
import { serveRequest } from "./serve.ts";

/** What a socket's attachment holds: the relay it is connected to. */
interface SocketAttachment {
  /** The relay's host name. */
  host: string;
}

/** How long `start` waits for the new relay to answer, in milliseconds. */
export const START_WAIT_MS = 30_000;

/** Pause between checks while `start` waits, in milliseconds. */
const START_POLL_MS = 1_000;

/**
 * Reads the deploy target from the instance settings.
 *
 * @param env - The plugin's bindings.
 * @returns The account, Worker name and API token.
 * @throws {PluginError} `invalid_params` when a setting is missing.
 */
export function targetOf(env: Env): RelayTarget {
  const accountId = String(env.CONFIG.account_id ?? "").trim();
  const workerName = String(env.CONFIG.worker_name ?? "").trim();
  const apiToken = String(env.SECRETS["api_token"] ?? "").trim();
  if (!accountId)
    throw PluginError.fromWire("invalid_params", "set account_id to your Cloudflare account id");
  if (!apiToken)
    throw PluginError.fromWire(
      "invalid_params",
      "set the api_token secret to a Cloudflare API token",
    );
  return { accountId, workerName: workerName || "bookclerk-relay", apiToken };
}

/**
 * The relay host a socket is connected to.
 *
 * @param ws - The socket.
 * @returns Its host, or `null` when the attachment has none.
 */
export function hostOf(ws: HostWebSocket): string | null {
  const attachment = ws.deserializeAttachment() as Partial<SocketAttachment> | null;
  return typeof attachment?.host === "string" ? attachment.host : null;
}

/**
 * Opens sockets to the relay until {@link RELAY_SOCKETS} are open.
 *
 * @param sockets - The socket binding.
 * @param host - The relay's host name.
 * @param token - The relay's bearer token.
 * @returns How many sockets are open to `host` afterwards.
 */
export async function topUpSockets(
  sockets: SocketsBinding,
  host: string,
  token: string,
): Promise<number> {
  const open = await sockets.getWebSockets();
  let current = 0;
  for (const ws of open) {
    if (hostOf(ws) === host) current += 1;
    else await ws.close(1000, "relay moved").catch(() => {});
  }
  while (current < RELAY_SOCKETS) {
    try {
      await sockets.connect(`wss://${host}${RELAY_CONNECT_PATH}`, {
        headers: { authorization: `Bearer ${token}` },
        attachment: { host } satisfies SocketAttachment,
      });
    } catch {
      break;
    }
    current += 1;
  }
  return current;
}

/**
 * Asks the relay how many of the plugin's sockets it holds.
 *
 * @param fetch - The request function.
 * @param host - The relay's host name.
 * @param token - The relay's bearer token.
 * @returns The count, or `null` when the relay did not answer.
 */
export async function relaySockets(
  fetch: Fetch,
  host: string,
  token: string,
): Promise<number | null> {
  try {
    const response = await fetch(`https://${host}${RELAY_HEALTH_PATH}`, {
      headers: { authorization: `Bearer ${token}` },
      redirect: "manual",
    });
    if (!response.ok) {
      await response.body?.cancel().catch(() => {});
      return null;
    }
    const body = (await response.json()) as { sockets?: unknown };
    return typeof body.sockets === "number" && Number.isInteger(body.sockets) ? body.sockets : null;
  } catch {
    return null;
  }
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

/** Everything the ingress methods reach, so tests can pass fakes. */
export interface IngressContext {
  /** The plugin's bindings. */
  env: Env;
  /** Makes requests (the Cloudflare API and the relay). */
  fetch: Fetch;
  /** Waits, between checks. */
  sleep: (ms: number) => Promise<void>;
  /** How long `start` waits for the relay. */
  startWaitMs: number;
}

/**
 * Deploys the relay, connects to it and returns its origin.
 *
 * @param ctx - Bindings, request function and waits.
 * @returns The public origin.
 * @throws {PluginError} `invalid_params` for bad settings, `unavailable`
 *   when the deploy fails or the relay does not take the sockets in time.
 */
export async function startRelay(ctx: IngressContext): Promise<IngressStarted> {
  const target = targetOf(ctx.env);
  const token = await relayToken(target);
  let host: string;
  try {
    host = await deployRelay(ctx.fetch, target, token);
  } catch (err) {
    if (err instanceof DeployError) throw PluginError.fromWire("unavailable", err.message);
    throw err;
  }
  const deadline = Date.now() + ctx.startWaitMs;
  for (;;) {
    // A new workers.dev route can take a few seconds to answer.
    const opened = await topUpSockets(ctx.env.SOCKETS, host, token);
    const held = opened > 0 ? await relaySockets(ctx.fetch, host, token) : null;
    if (held !== null && held > 0) return { origin: `https://${host}` };
    if (Date.now() >= deadline) {
      throw PluginError.fromWire(
        "unavailable",
        `the relay at ${host} did not accept the library's connections`,
      );
    }
    await ctx.sleep(START_POLL_MS);
  }
}

/**
 * Reports whether the relay serves, reopening sockets that closed.
 *
 * @param ctx - Bindings and request function.
 * @returns `serving` with a detail for the operator when it is not.
 */
export async function relayHealth(ctx: IngressContext): Promise<IngressHealth> {
  const sockets = await ctx.env.SOCKETS.getWebSockets();
  const target = targetOf(ctx.env);
  let host = sockets.map(hostOf).find((h): h is string => h !== null);
  if (!host) {
    // Every socket closed (an outage outlasted the reconnects): the host
    // only asks while the ingress is meant to serve, so find the deployed
    // relay again and reconnect.
    try {
      host = await findRelayHost(ctx.fetch, target);
    } catch (err) {
      const detail = err instanceof DeployError ? `: ${err.message}` : "";
      return { serving: false, detail: `not connected to the relay${detail}` };
    }
  }
  const token = await relayToken(target);
  await topUpSockets(ctx.env.SOCKETS, host, token);
  const held = await relaySockets(ctx.fetch, host, token);
  if (held === null) return { serving: false, detail: `the relay at ${host} does not answer` };
  if (held === 0)
    return {
      serving: false,
      detail: `the relay at ${host} holds none of the library's connections`,
    };
  return { serving: true };
}

/**
 * Closes every socket to the relay.
 *
 * @param sockets - The socket binding.
 * @returns Resolves once each close was sent.
 */
export async function stopRelay(sockets: SocketsBinding): Promise<void> {
  for (const ws of await sockets.getWebSockets())
    await ws.close(1000, "ingress stopped").catch(() => {});
}

/**
 * The context for the live bindings.
 *
 * @param env - The plugin's bindings.
 * @returns The context with the global `fetch`.
 */
export function liveContext(env: Env): IngressContext {
  return { env, fetch: (input, init) => fetch(input, init), sleep, startWaitMs: START_WAIT_MS };
}

/**
 * Handles one message on a relay socket.
 *
 * @param env - The plugin's bindings.
 * @param fetch - The request function.
 * @param ws - The socket.
 * @param message - The message.
 * @returns Resolves once the announced request has been served.
 */
export async function handleMessage(
  env: Env,
  fetch: Fetch,
  ws: HostWebSocket,
  message: SocketMessageData,
): Promise<void> {
  if (typeof message !== "string") return;
  const notice = parseNotice(message);
  const host = hostOf(ws);
  if (typeof notice === "string" || host === null || !env.ORIGIN) return;
  const token = await relayToken(targetOf(env));
  await serveRequest({ fetch, origin: env.ORIGIN, host, token }, notice.id);
}
