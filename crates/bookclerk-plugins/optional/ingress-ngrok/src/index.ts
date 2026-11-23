/**
 * ngrok ingress (workerd guest).
 *
 * `start` opens one host-held TLS stream to ngrok's agent endpoint,
 * authenticates with the operator's authtoken, binds the configured domain
 * and returns `https://<domain>`. ngrok then opens a stream inside that
 * connection for each public request; the plugin reads the HTTP/1.1
 * request, hands it to the library through `env.ORIGIN` and writes the
 * answer back (see `agent.ts`). `health` checks the session with a
 * heartbeat and reconnects a session that ended.
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
import {
  agentHealth,
  handleClose,
  handleMessage,
  liveContext,
  startAgent,
  stopAgent,
} from "./lifecycle.ts";

// workerd treats every named export of this module as an entrypoint, so it
// exports only the entrypoint classes; the logic lives in `lifecycle.ts`.

/** `ingress` entrypoint: the agent's lifecycle. */
export class Ingress extends IngressEntrypoint<Env> {
  /**
   * Connects to ngrok and brings the endpoint online.
   *
   * @returns The public origin.
   */
  override start(): Promise<IngressStarted> {
    return startAgent(liveContext(this.env));
  }

  /**
   * Reports whether the endpoint serves.
   *
   * @returns The session's state.
   */
  override health(): Promise<IngressHealth> {
    return agentHealth(liveContext(this.env));
  }

  /**
   * Disconnects from ngrok, which takes the endpoint offline.
   *
   * @returns Resolves once the connection is closed.
   */
  override stop(): Promise<void> {
    return stopAgent(this.env.SOCKETS);
  }
}

/** Default entrypoint: the agent connection's handlers. */
export default class NgrokAgent extends BookclerkEntrypoint<Env> {
  /**
   * Handles bytes ngrok sent: session replies, heartbeats and proxied
   * requests.
   *
   * @param ws - The agent's socket.
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
   * Reconnects when the connection ended unless the plugin closed it.
   *
   * @param ws - The closed socket.
   * @param code - The close code.
   * @returns Resolves once handled.
   */
  override async webSocketClose(ws: HostWebSocket, code: number): Promise<void> {
    await handleClose(this.env, ws, code);
  }
}
