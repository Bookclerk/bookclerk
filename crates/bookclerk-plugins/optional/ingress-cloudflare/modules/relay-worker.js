/**
 * The Bookclerk relay Worker, as the plugin deploys it into the operator's
 * own Cloudflare account.
 *
 * Every request goes to one Durable Object (`Relay`), which holds the
 * plugin's WebSockets and a {@link RelayHub}. The plugin uploads this module
 * with `relay-core.js` and `protocol.js`; the plugin's own isolate never
 * loads it. The sockets use the hibernation API, so the object sleeps while
 * no request is in flight, even with live connections open. Tags find the
 * sockets again after it wakes: `plugin` and `plugin:<key>` on the plugin's,
 * `client:<id>` and `via:<key>` on each public client's; the attachments
 * say which is which.
 *
 * The module exports only the Worker and the `Relay` class, since the
 * runtime treats each named export as an entrypoint.
 */
import { RelayHub } from "./relay-core.js";
import { RELAY_CONNECT_PATH, encodeLiveFrame, messageFrame, sendableCloseCode, sendableCloseReason, } from "./protocol.js";
/**
 * A socket's attachment, when it is one the relay wrote.
 *
 * @param socket - The socket.
 * @returns Its attachment, or `null`.
 */
function attachmentOf(socket) {
    const value = socket.deserializeAttachment();
    if (value?.["kind"] === "plugin" && typeof value["key"] === "string") {
        return { kind: "plugin", key: value["key"] };
    }
    const { id, via, local } = value ?? {};
    if (value?.["kind"] === "client" &&
        typeof id === "string" &&
        typeof via === "string" &&
        typeof local === "string") {
        return { kind: "client", id, via, local };
    }
    return null;
}
/**
 * Closes a socket, ignoring one that is already closed.
 *
 * @param socket - The socket.
 * @param code - A sendable close code.
 * @param reason - The reason.
 */
function closeQuietly(socket, code, reason) {
    try {
        socket.close(code, reason);
    }
    catch {
        // Already closed.
    }
}
/**
 * The relay's client sockets, as the hub finds and makes them.
 *
 * @param state - The object's state.
 * @returns The hub's view of them.
 */
function workerClients(state) {
    return {
        accept(id, via, local, protocol) {
            const viaAttachment = attachmentOf(via);
            const viaKey = viaAttachment?.kind === "plugin" ? viaAttachment.key : "";
            const pair = new WebSocketPair();
            state.acceptWebSocket(pair[1], ["client", `client:${id}`, `via:${viaKey}`]);
            pair[1].serializeAttachment({ kind: "client", id, via: viaKey, local });
            const headers = new Headers();
            if (protocol)
                headers.set("sec-websocket-protocol", protocol);
            const response = new Response(null, {
                status: 101,
                webSocket: pair[0],
                headers,
            });
            return { response, client: pair[1] };
        },
        find(id) {
            return state.getWebSockets(`client:${id}`)[0] ?? null;
        },
    };
}
/** The one Durable Object every request goes to. */
export class Relay {
    /** The object's state. */
    #state;
    /** The hub, holding requests and sockets. */
    #hub;
    /**
     * Creates the object and adopts the plugin sockets it held before it
     * slept.
     *
     * @param state - The object's state.
     * @param env - The Worker's bindings.
     */
    constructor(state, env) {
        this.#state = state;
        this.#hub = new RelayHub(env.RELAY_TOKEN, undefined, workerClients(state));
        for (const socket of state.getWebSockets("plugin"))
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
            const key = crypto.randomUUID();
            this.#state.acceptWebSocket(pair[1], ["plugin", `plugin:${key}`]);
            pair[1].serializeAttachment({ kind: "plugin", key });
            this.#hub.attach(pair[1]);
            return new Response(null, { status: 101, webSocket: pair[0] });
        }
        return this.#hub.fetch(request);
    }
    /**
     * Passes a message on: a plugin's live frame to its client, a client's
     * message to the plugin as a live frame.
     *
     * @param socket - The socket it arrived on.
     * @param message - The message.
     */
    webSocketMessage(socket, message) {
        const attachment = attachmentOf(socket);
        if (attachment?.kind === "plugin") {
            // Text from the plugin is only ever the runtime's ping.
            if (typeof message !== "string")
                this.#hub.pluginFrame(new Uint8Array(message));
            return;
        }
        if (attachment?.kind !== "client")
            return;
        const via = this.#state.getWebSockets(`plugin:${attachment.via}`)[0];
        if (!via) {
            closeQuietly(socket, 1011, "the library's connection to the relay closed");
            return;
        }
        const frame = messageFrame(attachment.local, message);
        if (frame === null) {
            closeQuietly(socket, 1009, "message too large");
            this.#tellPlugin(via, attachment, 1009, "message too large");
            return;
        }
        try {
            via.send(frame);
        }
        catch {
            closeQuietly(socket, 1011, "the library's connection to the relay closed");
        }
    }
    /**
     * A socket closed: a plugin socket's request and connections end, a
     * client's close goes to the plugin.
     *
     * @param socket - The socket.
     * @param code - The close code.
     * @param reason - The close reason.
     */
    webSocketClose(socket, code, reason) {
        this.#closed(socket, code, reason);
    }
    /**
     * A socket failed; handled as an abnormal close.
     *
     * @param socket - The socket.
     */
    webSocketError(socket) {
        this.#closed(socket, 1006, "");
    }
    /**
     * Handles a socket that closed or failed.
     *
     * @param socket - The socket.
     * @param code - The close code.
     * @param reason - The close reason.
     */
    #closed(socket, code, reason) {
        const attachment = attachmentOf(socket);
        if (attachment?.kind === "client") {
            const via = this.#state.getWebSockets(`plugin:${attachment.via}`)[0];
            if (via)
                this.#tellPlugin(via, attachment, code, reason);
            return;
        }
        this.#hub.detach(socket);
        if (attachment?.kind === "plugin") {
            for (const client of this.#state.getWebSockets(`via:${attachment.key}`)) {
                closeQuietly(client, 1011, "the library's connection to the relay closed");
            }
        }
    }
    /**
     * Sends the plugin a client's close.
     *
     * @param via - The plugin socket.
     * @param attachment - The client's attachment.
     * @param code - The client's close code.
     * @param reason - The close reason.
     */
    #tellPlugin(via, attachment, code, reason) {
        try {
            via.send(encodeLiveFrame({
                op: "close",
                id: attachment.local,
                code: sendableCloseCode(code),
                reason: sendableCloseReason(reason),
            }));
        }
        catch {
            // The plugin socket closed too; the plugin closes its side then.
        }
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
