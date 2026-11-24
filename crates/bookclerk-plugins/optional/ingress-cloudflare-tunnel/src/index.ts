/**
 * Cloudflare Tunnel ingress (workerd guest).
 *
 * `start` opens one host-held TLS stream to a Cloudflare edge region and
 * serves HTTP/2 on it, as cloudflared does: the edge opens a control
 * stream on which the connector registers with the operator's tunnel
 * token, then a stream for each request to the tunnel's public hostname.
 * The plugin hands each request to the library through `env.ORIGIN` and
 * writes the answer back (see `agent.ts`); a WebSocket request becomes a
 * socket to the library through `env.ORIGIN.connect`. `health` checks the
 * connection with a ping and reconnects one that ended.
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
  agentHealth,
  handleClose,
  handleMessage,
  liveContext,
  startAgent,
  stopAgent,
} from "./lifecycle.ts";
import { deprovisionTunnel, provisionTunnel } from "./provision.ts";

// workerd treats every named export of this module as an entrypoint, so it
// exports only the entrypoint classes; the logic lives in `lifecycle.ts`.

/** `ingress` entrypoint: the connector's lifecycle. */
export class Ingress extends IngressEntrypoint<Env> {
  /**
   * Connects to Cloudflare and registers the tunnel connection.
   *
   * @returns The public origin.
   */
  override start(): Promise<IngressStarted> {
    return startAgent(liveContext(this.env));
  }

  /**
   * Reports whether the tunnel serves.
   *
   * @returns The connection's state.
   */
  override health(): Promise<IngressHealth> {
    return agentHealth(liveContext(this.env));
  }

  /**
   * Disconnects from Cloudflare.
   *
   * @returns Resolves once the connection is closed.
   */
  override stop(): Promise<void> {
    return stopAgent(this.env.SOCKETS);
  }
}

/** `provisioning` entrypoint: the guided setup (see `provision.ts`). */
export class Provisioning extends ProvisioningEntrypoint<Env> {
  /**
   * Creates the tunnel, its route and its DNS record from the pasted token.
   *
   * @param request - The operator's grant.
   * @returns The account, the host name and the tunnel token to keep.
   */
  override provision(request: ProvisionRequest): Promise<Provisioned> {
    return provisionTunnel({ env: this.env, fetch: (input, init) => fetch(input, init) }, request);
  }

  /**
   * Deletes the DNS record and the tunnel.
   *
   * @param request - The operator's grant, or kind `none`.
   * @returns Resolves once they are gone.
   */
  override deprovision(request: DeprovisionRequest): Promise<void> {
    return deprovisionTunnel(
      { env: this.env, fetch: (input, init) => fetch(input, init) },
      request,
    );
  }
}

/** Default entrypoint: the connection's handlers. */
export default class CloudflareTunnelConnector extends BookclerkEntrypoint<Env> {
  /**
   * Handles bytes the edge sent (control messages, pings and requests) and
   * messages from the library's WebSockets.
   *
   * @param ws - The connector's socket, or a socket to the library.
   * @param message - The bytes.
   * @returns Nothing for the host to do.
   */
  override async webSocketMessage(
    ws: HostWebSocket,
    message: SocketMessageData,
  ): Promise<Partial<SocketMessageOk>> {
    await handleMessage(this.env, ws, message);
    return {};
  }

  /**
   * Passes a library socket's close to its client, or reconnects when the
   * connection to Cloudflare ended unless the plugin closed it.
   *
   * @param ws - The closed socket.
   * @param code - The close code.
   * @param reason - The close reason.
   * @returns Resolves once handled.
   */
  override async webSocketClose(ws: HostWebSocket, code: number, reason: string): Promise<void> {
    await handleClose(liveContext(this.env), ws, code, reason);
  }
}
