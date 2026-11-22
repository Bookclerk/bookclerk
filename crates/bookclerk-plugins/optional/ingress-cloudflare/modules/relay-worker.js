/**
 * The Bookclerk relay Worker, as the plugin deploys it into the operator's
 * own Cloudflare account.
 *
 * Every request goes to one Durable Object (`Relay`), which holds the
 * plugin's WebSockets and a {@link RelayHub}. The plugin uploads this module
 * with `relay-core.js` and `protocol.js`; the plugin's own isolate never
 * loads it. The sockets use the hibernation API, so the object sleeps while
 * no request is in flight.
 */
import { RelayHub } from "./relay-core.js";
import { RELAY_CONNECT_PATH } from "./protocol.js";
/** The one Durable Object every request goes to. */
export class Relay {
    /** The object's state. */
    #state;
    /** The hub, holding requests and sockets. */
    #hub;
    /**
     * Creates the object and adopts the sockets it held before it slept.
     *
     * @param state - The object's state.
     * @param env - The Worker's bindings.
     */
    constructor(state, env) {
        this.#state = state;
        this.#hub = new RelayHub(env.RELAY_TOKEN);
        for (const socket of state.getWebSockets())
            this.#hub.attach(socket);
    }
    /**
     * Answers a request: a plugin socket, a relay path or a public request.
     *
     * @param request - The request.
     * @returns Its answer.
     */
    async fetch(request) {
        const url = new URL(request.url);
        if (url.pathname === RELAY_CONNECT_PATH) {
            if (!this.#hub.authorized(request))
                return new Response("unauthorized", { status: 401 });
            if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
                return new Response("expected a WebSocket", { status: 426 });
            }
            const pair = new WebSocketPair();
            this.#state.acceptWebSocket(pair[1]);
            this.#hub.attach(pair[1]);
            return new Response(null, { status: 101, webSocket: pair[0] });
        }
        return this.#hub.fetch(request);
    }
    /**
     * The plugin sends nothing on its sockets but the runtime's pings, so a
     * message is ignored.
     */
    webSocketMessage() { }
    /**
     * A plugin socket closed.
     *
     * @param socket - The socket.
     */
    webSocketClose(socket) {
        this.#hub.detach(socket);
    }
    /**
     * A plugin socket failed.
     *
     * @param socket - The socket.
     */
    webSocketError(socket) {
        this.#hub.detach(socket);
    }
}
/** The Worker's entry: everything goes to the one `Relay` object. */
export default {
    /**
     * Sends a request to the relay object.
     *
     * @param request - The request.
     * @param env - The Worker's bindings.
     * @returns The object's answer.
     */
    fetch(request, env) {
        return env.RELAY.get(env.RELAY.idFromName("relay")).fetch(request);
    },
};
