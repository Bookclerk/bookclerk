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
import { BookclerkEntrypoint, IngressEntrypoint, } from "@bookclerk/plugin-sdk/workerd";
import { agentHealth, handleClose, handleMessage, liveContext, startAgent, stopAgent, } from "./lifecycle.js";
// workerd treats every named export of this module as an entrypoint, so it
// exports only the entrypoint classes; the logic lives in `lifecycle.ts`.
/** `ingress` entrypoint: the connector's lifecycle. */
export class Ingress extends IngressEntrypoint {
    /**
     * Connects to Cloudflare and registers the tunnel connection.
     *
     * @returns The public origin.
     */
    start() {
        return startAgent(liveContext(this.env));
    }
    /**
     * Reports whether the tunnel serves.
     *
     * @returns The connection's state.
     */
    health() {
        return agentHealth(liveContext(this.env));
    }
    /**
     * Disconnects from Cloudflare.
     *
     * @returns Resolves once the connection is closed.
     */
    stop() {
        return stopAgent(this.env.SOCKETS);
    }
}
/** Default entrypoint: the connection's handlers. */
export default class CloudflareTunnelConnector extends BookclerkEntrypoint {
    /**
     * Handles bytes the edge sent (control messages, pings and requests) and
     * messages from the library's WebSockets.
     *
     * @param ws - The connector's socket, or a socket to the library.
     * @param message - The bytes.
     * @returns Nothing for the host to do.
     */
    async webSocketMessage(ws, message) {
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
    async webSocketClose(ws, code, reason) {
        await handleClose(liveContext(this.env), ws, code, reason);
    }
}
