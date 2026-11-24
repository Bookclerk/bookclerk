/**
 * Cloudflare relay ingress (workerd guest).
 *
 * `start` deploys the Bookclerk relay Worker into the operator's own
 * Cloudflare account (see `cloudflare.ts`), opens {@link RELAY_SOCKETS}
 * host-brokered WebSockets to it and returns its `workers.dev` origin. Each
 * public request the relay announces on a socket is served in that socket's
 * message handler (see `serve.ts`); a WebSocket upgrade becomes a socket
 * to the library whose messages travel as live frames on the relay socket
 * that served it. `health` reopens sockets that closed.
 */

import {
  BookclerkEntrypoint,
  IngressEntrypoint,
  ProvisioningEntrypoint,
  type DeprovisionRequest,
  type HostWebSocket,
  type IngressHealth,
  type IngressStarted,
  type Provisioned,
  type ProvisionRequest,
  type SocketMessageData,
  type SocketMessageOk,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import {
  handleClose,
  handleMessage,
  liveContext,
  relayHealth,
  startRelay,
  stopRelay,
} from "./lifecycle.ts";
import { deprovisionRelay, provisionRelay } from "./provision.ts";

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

/** `provisioning` entrypoint: the guided setup (see `provision.ts`). */
export class Provisioning extends ProvisioningEntrypoint<Env> {
  /**
   * Mints the Workers token from the pasted one.
   *
   * @param request - The operator's grant.
   * @returns The account and the token to keep.
   */
  override provision(request: ProvisionRequest): Promise<Provisioned> {
    return provisionRelay({ env: this.env, fetch: (input, init) => fetch(input, init) }, request);
  }

  /**
   * Deletes the relay Worker and the minted token.
   *
   * @param request - The operator's grant, or kind `none`.
   * @returns Resolves once they are gone.
   */
  override deprovision(request: DeprovisionRequest): Promise<void> {
    return deprovisionRelay({ env: this.env, fetch: (input, init) => fetch(input, init) }, request);
  }
}

/** Default entrypoint: the relay sockets' handlers. */
export default class CloudflareRelay extends BookclerkEntrypoint<Env> {
  /**
   * Serves the request a relay notice announces, or passes a live
   * connection's message on.
   *
   * @param ws - The socket.
   * @param message - The notice, live frame or library message.
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
   * Passes a live connection's close to the relay, or ends the
   * connections of a relay socket that closed and reopens it unless the
   * plugin closed it itself.
   *
   * @param ws - The closed socket.
   * @param code - The close code.
   * @param reason - The close reason.
   * @returns Resolves once the close was handled.
   */
  override async webSocketClose(ws: HostWebSocket, code: number, reason: string): Promise<void> {
    await handleClose(this.env, ws, code, reason);
  }
}
