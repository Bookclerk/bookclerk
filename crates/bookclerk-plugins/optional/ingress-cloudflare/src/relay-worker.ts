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

import { RelayHub, type RelaySocket } from "./relay-core.ts";
import { RELAY_CONNECT_PATH } from "./protocol.ts";

/** The parts of a Workers WebSocket the relay uses. */
interface WorkerSocket extends RelaySocket {
  /**
   * Closes the socket.
   *
   * @param code - Close code.
   * @param reason - Close reason.
   */
  close(code?: number, reason?: string): void;
}

/** The parts of `DurableObjectState` the relay uses. */
interface RelayState {
  /**
   * Accepts a socket under the hibernation API.
   *
   * @param socket - The server end of a `WebSocketPair`.
   */
  acceptWebSocket(socket: WorkerSocket): void;
  /**
   * The sockets the object holds, after it woke.
   *
   * @returns Every accepted socket that is still open.
   */
  getWebSockets(): WorkerSocket[];
}

/** A Durable Object stub. */
interface RelayStub {
  /**
   * Sends a request to the object.
   *
   * @param request - The request.
   * @returns Its answer.
   */
  fetch(request: Request): Promise<Response>;
}

/** The Durable Object namespace binding. */
interface RelayNamespace {
  /**
   * The id of a named object.
   *
   * @param name - Its name.
   * @returns The id.
   */
  idFromName(name: string): unknown;
  /**
   * A stub for an object.
   *
   * @param id - Its id.
   * @returns The stub.
   */
  get(id: unknown): RelayStub;
}

/** Bindings the plugin deploys the Worker with. */
export interface RelayEnv {
  /** The `Relay` Durable Object namespace. */
  RELAY: RelayNamespace;
  /** The bearer token the plugin derived (a secret binding). */
  RELAY_TOKEN: string;
}

/** `WebSocketPair`, a Workers global. */
declare const WebSocketPair: {
  new (): { 0: WorkerSocket; 1: WorkerSocket };
};

/** The one Durable Object every request goes to. */
export class Relay {
  /** The object's state. */
  readonly #state: RelayState;
  /** The hub, holding requests and sockets. */
  readonly #hub: RelayHub;

  /**
   * Creates the object and adopts the sockets it held before it slept.
   *
   * @param state - The object's state.
   * @param env - The Worker's bindings.
   */
  constructor(state: RelayState, env: RelayEnv) {
    this.#state = state;
    this.#hub = new RelayHub(env.RELAY_TOKEN);
    for (const socket of state.getWebSockets()) this.#hub.attach(socket);
  }

  /**
   * Answers a request: a plugin socket, a relay path or a public request.
   *
   * @param request - The request.
   * @returns Its answer.
   */
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === RELAY_CONNECT_PATH) {
      if (!this.#hub.authorized(request)) return new Response("unauthorized", { status: 401 });
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return new Response("expected a WebSocket", { status: 426 });
      }
      const pair = new WebSocketPair();
      this.#state.acceptWebSocket(pair[1]);
      this.#hub.attach(pair[1]);
      return new Response(null, { status: 101, webSocket: pair[0] } as ResponseInit);
    }
    return this.#hub.fetch(request);
  }

  /**
   * The plugin sends nothing on its sockets but the runtime's pings, so a
   * message is ignored.
   */
  webSocketMessage(): void {}

  /**
   * A plugin socket closed.
   *
   * @param socket - The socket.
   */
  webSocketClose(socket: WorkerSocket): void {
    this.#hub.detach(socket);
  }

  /**
   * A plugin socket failed.
   *
   * @param socket - The socket.
   */
  webSocketError(socket: WorkerSocket): void {
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
  fetch(request: Request, env: RelayEnv): Promise<Response> {
    return env.RELAY.get(env.RELAY.idFromName("relay")).fetch(request);
  },
};
