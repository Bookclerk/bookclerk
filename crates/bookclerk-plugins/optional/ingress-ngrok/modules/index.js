/**
 * ngrok ingress (workerd guest).
 *
 * `start` opens one host-held TLS stream to ngrok's agent endpoint,
 * authenticates with the operator's authtoken, binds the configured domain
 * and returns `https://<domain>`. ngrok then opens a stream inside that
 * connection for each public request; the plugin reads the HTTP/1.1
 * request, hands it to the library through `env.ORIGIN` and writes the
 * answer back (see `agent.ts`); an upgrade becomes a WebSocket to the
 * library through `env.ORIGIN.connect`. `health` checks the session with a
 * heartbeat and reconnects a session that ended.
 */
import { BookclerkEntrypoint, IngressEntrypoint, } from "@bookclerk/plugin-sdk/workerd";
import { agentHealth, handleClose, handleMessage, liveContext, startAgent, stopAgent, } from "./lifecycle.js";
// workerd treats every named export of this module as an entrypoint, so it
// exports only the entrypoint classes; the logic lives in `lifecycle.ts`.
/** `ingress` entrypoint: the agent's lifecycle. */
export class Ingress extends IngressEntrypoint {
    /**
     * Connects to ngrok and brings the endpoint online.
     *
     * @returns The public origin.
     */
    start() {
        return startAgent(liveContext(this.env));
    }
    /**
     * Reports whether the endpoint serves.
     *
     * @returns The session's state.
     */
    health() {
        return agentHealth(liveContext(this.env));
    }
    /**
     * Disconnects from ngrok, which takes the endpoint offline.
     *
     * @returns Resolves once the connection is closed.
     */
    stop() {
        return stopAgent(this.env.SOCKETS);
    }
}
/** Default entrypoint: the agent connection's handlers. */
export default class NgrokAgent extends BookclerkEntrypoint {
    /**
     * Handles bytes ngrok sent (session replies, heartbeats and proxied
     * requests) and messages from the library's WebSockets.
     *
     * @param ws - The agent's socket, or a socket to the library.
     * @param message - The bytes.
     * @returns Nothing for the host to do.
     */
    async webSocketMessage(ws, message) {
        await handleMessage(this.env, ws, message);
        return {};
    }
    /**
     * Passes a library socket's close to its client, or reconnects when the
     * connection to ngrok ended unless the plugin closed it.
     *
     * @param ws - The closed socket.
     * @param code - The close code.
     * @param reason - The close reason.
     * @returns Resolves once handled.
     */
    async webSocketClose(ws, code, reason) {
        await handleClose(this.env, ws, code, reason);
    }
}
