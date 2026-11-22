/**
 * Cloudflare relay ingress (workerd guest).
 *
 * `start` deploys the Bookclerk relay Worker into the operator's own
 * Cloudflare account (see `cloudflare.ts`), opens {@link RELAY_SOCKETS}
 * host-brokered WebSockets to it and returns its `workers.dev` origin. Each
 * public request the relay announces on a socket is served in that socket's
 * message handler (see `serve.ts`). `health` reopens sockets that closed.
 */

import {
  BookclerkEntrypoint,
  IngressEntrypoint,
  type HostWebSocket,
  type IngressHealth,
  type IngressStarted,
  type SocketMessageData,
  type SocketMessageOk,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { relayToken } from "./cloudflare.ts";
import {
  handleMessage,
  hostOf,
  liveContext,
  relayHealth,
  startRelay,
  stopRelay,
  targetOf,
  topUpSockets,
} from "./lifecycle.ts";

// workerd treats every named export of this module as an entrypoint, so it
// exports only the entrypoint classes; the logic lives in `lifecycle.ts`.

/** `ingress` entrypoint: the relay's lifecycle. */
export class Ingress extends IngressEntrypoint<Env> {
  /**
   * Deploys the relay and connects to it.
   *
   * @returns The public origin.
   */
  override start(): Promise<IngressStarted> {
    return startRelay(liveContext(this.env));
  }

  /**
   * Reports whether the relay serves.
   *
   * @returns The relay's state.
   */
  override health(): Promise<IngressHealth> {
    return relayHealth(liveContext(this.env));
  }

  /**
   * Disconnects from the relay. The Worker stays deployed, so starting
   * again serves under the same name.
   *
   * @returns Resolves once the sockets are closed.
   */
  override stop(): Promise<void> {
    return stopRelay(this.env.SOCKETS);
  }
}

/** Default entrypoint: the relay sockets' handlers. */
export default class CloudflareRelay extends BookclerkEntrypoint<Env> {
  /**
   * Serves the request a relay notice announces.
   *
   * @param ws - The socket.
   * @param message - The notice.
   * @returns Nothing for the host to do.
   */
  override async webSocketMessage(
    ws: HostWebSocket,
    message: SocketMessageData,
  ): Promise<Partial<SocketMessageOk>> {
    await handleMessage(this.env, (input, init) => fetch(input, init), ws, message);
    return {};
  }

  /**
   * Reopens a socket the relay or the network closed, unless the plugin
   * closed it itself.
   *
   * @param ws - The closed socket.
   * @param code - The close code.
   * @returns Resolves once a replacement was attempted.
   */
  override async webSocketClose(ws: HostWebSocket, code: number): Promise<void> {
    const host = hostOf(ws);
    if (host === null || code === 1000) return;
    try {
      await topUpSockets(this.env.SOCKETS, host, await relayToken(targetOf(this.env)));
    } catch {
      // `health` tops the sockets up again.
    }
  }
}
