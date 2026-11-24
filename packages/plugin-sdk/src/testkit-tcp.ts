/**
 * An in-memory `connect()` for plugin tests: a scripted server behind a fake
 * of the launcher's egress relay, speaking the same frames, so a plugin's
 * `startTls()` and `secureTransport: "on"` code runs as it would on a host.
 *
 * TLS itself is simulated: the fake answers the upgrade with TLS ready (or
 * the failure {@link TestConnectOptions.tlsFailure} names) and the server
 * sees {@link TestTcpConnection.tls} turn true. Certificate checks are the
 * relay's job and are covered by its own tests.
 *
 * ```ts
 * import { createTestConnect } from "@bookclerk/plugin-sdk/testkit";
 *
 * const connect = createTestConnect({
 *   async serve(conn) {
 *     await conn.write("220 ready\r\n");
 *     // ...
 *   },
 * });
 * ```
 *
 * @module
 */

import {
  MAX_TCP_FRAME_PAYLOAD,
  STARTTLS_HOST_SUFFIX,
  createConnect,
  encodeDataFrames,
  type SecureTransport,
  type TcpConnect,
  type TcpSocket,
  type TcpSocketAddress,
} from "./tcp.js";

/** The server's side of one test connection. */
export interface TestTcpConnection {
  /** Host the plugin dialed, without the framing suffix. */
  readonly hostname: string;
  /** Port the plugin dialed. */
  readonly port: number;
  /** Whether the connection has upgraded to (simulated) TLS. */
  readonly tls: boolean;
  /**
   * The next plaintext bytes the plugin wrote.
   *
   * @returns The bytes, or `null` once the plugin closed its side.
   */
  read(): Promise<Uint8Array | null>;
  /**
   * Reads up to and including the next `\n`, decoded as UTF-8.
   *
   * @returns The line, or `null` when the plugin closed first.
   */
  readLine(): Promise<string | null>;
  /**
   * Sends bytes (or UTF-8 text) to the plugin.
   *
   * @param data - What to send.
   */
  write(data: Uint8Array | string): Promise<void>;
  /** Waits until the plugin asks to upgrade and the upgrade is answered. */
  waitForTls(): Promise<void>;
  /** Ends the connection. */
  close(): Promise<void>;
}

/** Options for {@link createTestConnect}. */
export interface TestConnectOptions {
  /** Runs the server for each connection the plugin opens. */
  serve(conn: TestTcpConnection): Promise<void> | void;
  /**
   * Stands in for the plugin's `tcp` grant: return `false` to refuse, as the
   * egress gate does (`network_not_granted`). Default: allow all.
   */
  allows?(hostname: string, port: number): boolean;
  /**
   * The handshake outcome for an upgrade: a reason string fails it, as a
   * certificate that does not verify would. Default: succeed.
   */
  tlsFailure?(hostname: string): string | null | undefined;
}

/** One connection the plugin opened. */
export interface TestDial {
  /** Host without the framing suffix. */
  hostname: string;
  /** Port. */
  port: number;
  /** `"off"` for a plain socket, `"framed"` for one that may use TLS. */
  tunnel: "off" | "framed";
}

/** The fake `connect()`, plus what it saw. */
export interface TestConnect extends TcpConnect {
  /** Every connection opened, in order. */
  readonly dials: readonly TestDial[];
}

/** An async queue of byte chunks, closed with `null`. */
class ByteQueue {
  /** Chunks not yet read. */
  readonly #items: (Uint8Array | null)[] = [];
  /** A reader waiting for the next chunk. */
  #waiting: ((item: Uint8Array | null) => void) | null = null;

  /**
   * Adds a chunk, or `null` to end the queue.
   *
   * @param item - The chunk, or `null`.
   */
  push(item: Uint8Array | null): void {
    if (this.#waiting) {
      const wake = this.#waiting;
      this.#waiting = null;
      wake(item);
    } else {
      this.#items.push(item);
    }
  }

  /** The next chunk, or `null` once ended. */
  async shift(): Promise<Uint8Array | null> {
    if (this.#items.length > 0) {
      const item = this.#items.shift() ?? null;
      if (item === null) this.#items.unshift(null);
      return item;
    }
    return new Promise((resolve) => {
      this.#waiting = resolve;
    });
  }
}

/** UTF-8 encoder shared by the fake. */
const ENCODER = new TextEncoder();

/**
 * Builds the server connection and the raw socket the SDK reads, for one
 * dial.
 *
 * @param dial - What the plugin dialed.
 * @param options - The server script and outcomes.
 * @returns The raw socket and the server's side.
 */
function openPair(
  dial: TestDial,
  options: TestConnectOptions,
): { raw: TcpSocket; conn: TestTcpConnection } {
  const inbox = new ByteQueue();
  let lineBuf = new Uint8Array(0);
  let tls = false;
  let tlsDone: () => void = () => undefined;
  const tlsSettled = new Promise<void>((resolve) => {
    tlsDone = resolve;
  });
  let toPlugin!: ReadableStreamDefaultController<Uint8Array>;
  let closedResolve!: () => void;
  const closed = new Promise<void>((resolve) => {
    closedResolve = resolve;
  });
  let ended = false;
  const end = (): void => {
    if (ended) return;
    ended = true;
    try {
      toPlugin.close();
    } catch {
      /* already closed */
    }
    inbox.push(null);
    closedResolve();
  };
  const send = (bytes: Uint8Array): void => {
    if (ended || bytes.length === 0) return;
    toPlugin.enqueue(dial.tunnel === "framed" ? encodeDataFrames(bytes) : bytes);
  };
  // Frames from the plugin, decoded by an independent reading of the layout.
  let pending = new Uint8Array(0);
  const fromPlugin = (chunk: Uint8Array): void => {
    if (dial.tunnel === "off") {
      inbox.push(chunk);
      return;
    }
    const joined = new Uint8Array(pending.length + chunk.length);
    joined.set(pending, 0);
    joined.set(chunk, pending.length);
    let at = 0;
    while (joined.length - at >= 3) {
      const kind = joined[at];
      const len = (joined[at + 1] << 8) | joined[at + 2];
      const valid =
        (kind === 0 && len >= 1 && len <= MAX_TCP_FRAME_PAYLOAD) || (kind === 1 && len === 0);
      if (!valid || (kind === 1 && tls)) {
        end();
        throw new TypeError(`test relay: malformed frame kind ${kind} length ${len}`);
      }
      if (joined.length - at - 3 < len) break;
      if (kind === 0) {
        inbox.push(joined.slice(at + 3, at + 3 + len));
      } else {
        const reason = options.tlsFailure?.(dial.hostname);
        if (reason) {
          const text = ENCODER.encode(reason).slice(0, 512);
          const frame = new Uint8Array(3 + text.length);
          frame[0] = 2;
          frame[1] = text.length >> 8;
          frame[2] = text.length & 0xff;
          frame.set(text, 3);
          toPlugin.enqueue(frame);
          tlsDone();
          end();
          return;
        }
        tls = true;
        toPlugin.enqueue(Uint8Array.of(1, 0, 0));
        tlsDone();
      }
      at += 3 + len;
    }
    pending = joined.slice(at);
  };
  const readable = new ReadableStream<Uint8Array>({
    start(controller) {
      toPlugin = controller;
    },
  });
  const writable = new WritableStream<Uint8Array>({
    write(chunk) {
      if (!ended) fromPlugin(chunk);
    },
    close() {
      inbox.push(null);
    },
  });
  const raw: TcpSocket = {
    readable,
    writable,
    opened: Promise.resolve({ remoteAddress: `${dial.hostname}:${dial.port}` }),
    closed,
    close: async () => end(),
    startTls: () => {
      throw new TypeError("the test relay's raw socket does not upgrade");
    },
  };
  const conn: TestTcpConnection = {
    hostname: dial.hostname,
    port: dial.port,
    get tls() {
      return tls;
    },
    read: () => inbox.shift(),
    readLine: async () => {
      for (;;) {
        const nl = lineBuf.indexOf(0x0a);
        if (nl !== -1) {
          const line = lineBuf.slice(0, nl + 1);
          lineBuf = lineBuf.slice(nl + 1);
          return new TextDecoder().decode(line);
        }
        const next = await inbox.shift();
        if (next === null) return null;
        const joined = new Uint8Array(lineBuf.length + next.length);
        joined.set(lineBuf, 0);
        joined.set(next, lineBuf.length);
        lineBuf = joined;
      }
    },
    write: async (data) => send(typeof data === "string" ? ENCODER.encode(data) : data),
    waitForTls: () => tlsSettled,
    close: async () => end(),
  };
  return { raw, conn };
}

/**
 * Creates the fake `connect()`.
 *
 * @param options - The server script and optional grant and TLS outcomes.
 * @returns A `connect()` to use where the plugin uses the SDK's.
 */
export function createTestConnect(options: TestConnectOptions): TestConnect {
  const dials: TestDial[] = [];
  const dial = (
    address: TcpSocketAddress,
    _opts: { secureTransport: SecureTransport; allowHalfOpen: boolean },
  ): TcpSocket => {
    const lower = address.hostname.toLowerCase();
    const framed = lower.endsWith(STARTTLS_HOST_SUFFIX);
    const hostname = framed
      ? address.hostname.slice(0, address.hostname.length - STARTTLS_HOST_SUFFIX.length)
      : address.hostname;
    const record: TestDial = { hostname, port: address.port, tunnel: framed ? "framed" : "off" };
    dials.push(record);
    if (options.allows && !options.allows(hostname, address.port)) {
      const err = new Error(
        `network_not_granted: tcp connect \`${hostname}:${address.port}\` is not in capabilities.network.tcp`,
      );
      const refused = Promise.reject(err);
      refused.catch(() => undefined);
      return {
        readable: new ReadableStream({ start: (c) => c.error(err) }),
        writable: new WritableStream({
          write: () => {
            throw err;
          },
        }),
        opened: refused,
        closed: refused,
        close: async () => undefined,
        startTls: () => {
          throw err;
        },
      };
    }
    const { raw, conn } = openPair(record, options);
    void Promise.resolve()
      .then(() => options.serve(conn))
      .catch(() => undefined)
      .finally(() => conn.close());
    return raw;
  };
  const connect = createConnect(dial) as TestConnect;
  Object.defineProperty(connect, "dials", { value: dials });
  return connect;
}
