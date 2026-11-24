/**
 * TCP sockets for workerd plugins, shaped like Workers `connect()` from
 * `cloudflare:sockets`, with `startTls()` and `secureTransport: "on"` that
 * work under Bookclerk.
 *
 * The pinned workerd cannot upgrade a socket relayed through the egress
 * worker, so a socket that may use TLS is a framed tunnel: the SDK dials the
 * host with {@link STARTTLS_HOST_SUFFIX} appended, the egress gate checks the
 * real host against the plugin's `[capabilities.network] tcp` grant, and the
 * launcher's egress relay originates TLS when asked, verifying the server's
 * certificate for the dialed name against Mozilla's roots. Between the
 * plugin and the relay every byte is a frame: a kind byte, a big-endian
 * 16-bit payload length, then the payload (see the `egress_starttls` module
 * in `bookclerk-workerd`). This module hides the frames: plugins read and
 * write plaintext on {@link TcpSocket.readable} and {@link TcpSocket.writable}
 * as they would on Cloudflare.
 *
 * `secureTransport: "off"` (the default) is a plain socket with no framing.
 *
 * @example
 * ```ts
 * import { connect } from "@bookclerk/plugin-sdk/workerd";
 *
 * const plain = connect({ hostname: "smtp.example.com", port: 587 }, { secureTransport: "starttls" });
 * // ...read the greeting, send EHLO and STARTTLS, read "220"...
 * const secure = plain.startTls();
 * await secure.opened; // rejects when the certificate does not verify
 * ```
 */

/** Host-name suffix that asks the egress gate and relay for a framed tunnel. */
export const STARTTLS_HOST_SUFFIX = ".starttls.bookclerk.invalid";

/** Largest payload one frame carries, in bytes. */
export const MAX_TCP_FRAME_PAYLOAD = 16 * 1024;

/** Largest reason a TLS-failed frame carries, in bytes. */
export const MAX_TLS_FAILURE_REASON_BYTES = 512;

/** Frame kind for data, in either direction. */
const KIND_DATA = 0;
/** Frame kind for start TLS (to the relay) and TLS ready (from it). */
const KIND_TLS = 1;
/** Frame kind for TLS failed (from the relay). */
const KIND_TLS_FAILED = 2;
/** Bytes in a frame header. */
const HEADER_BYTES = 3;

/** Where to connect, as Workers `connect()` takes it. */
export interface TcpSocketAddress {
  /** Host name or IP address; IPv6 without brackets. */
  hostname: string;
  /** TCP port, 1 to 65535. */
  port: number;
}

/** How the socket uses TLS, as Workers `SocketOptions.secureTransport`. */
export type SecureTransport = "off" | "on" | "starttls";

/** Options for `connect()`, as Workers `SocketOptions`. */
export interface TcpSocketOptions {
  /**
   * `"off"` (default): plain TCP. `"starttls"`: plain until
   * {@link TcpSocket.startTls}. `"on"`: TLS from the start.
   */
  secureTransport?: SecureTransport;
  /** Keep reading after the writable side closes (default `false`). */
  allowHalfOpen?: boolean;
}

/** What {@link TcpSocket.opened} resolves to, as Workers `SocketInfo`. */
export interface TcpSocketInfo {
  /** The peer address workerd reports, when it reports one. */
  remoteAddress?: string;
  /** The local address workerd reports, when it reports one. */
  localAddress?: string;
}

/** A TCP socket, as Workers `Socket`. */
export interface TcpSocket {
  /** Bytes from the server; plaintext after an upgrade. */
  readonly readable: ReadableStream<Uint8Array>;
  /** Bytes to the server; plaintext after an upgrade. */
  readonly writable: WritableStream<Uint8Array>;
  /** Resolves once connected (after the handshake for TLS). */
  readonly opened: Promise<TcpSocketInfo>;
  /** Resolves when the socket has closed. */
  readonly closed: Promise<void>;
  /** Closes both directions. */
  close(): Promise<void>;
  /**
   * Upgrades to TLS and returns the secure socket; this socket stops being
   * usable. Only on a socket opened with `secureTransport: "starttls"`, and
   * only once. Call it after reading the server's go-ahead.
   *
   * @throws TypeError when the socket was not opened for STARTTLS or has
   * already upgraded.
   */
  startTls(): TcpSocket;
}

/**
 * The Workers `connect()` this module builds on: in an isolate,
 * `connect` from `cloudflare:sockets`; in tests, a fake.
 */
export type TcpDial = (
  address: TcpSocketAddress,
  options: { secureTransport: "off"; allowHalfOpen: boolean },
) => TcpSocket;

/** `connect()` as this module exports it. */
export type TcpConnect = (
  address: string | TcpSocketAddress,
  options?: TcpSocketOptions,
) => TcpSocket;

/** One frame the relay sent. */
export type RelayFrame =
  | { kind: "data"; bytes: Uint8Array }
  | { kind: "tlsReady" }
  | { kind: "tlsFailed"; reason: string };

/**
 * Splits the relay's byte stream into {@link RelayFrame}s. Sans-IO, so the
 * tests and fuzzing drive the same code the socket runs.
 */
export class RelayFrameDecoder {
  /** Bytes received and not yet consumed as a frame. */
  #buf: Uint8Array = new Uint8Array(0);

  /**
   * Appends bytes read from the relay.
   *
   * @param bytes - The next bytes, in order.
   */
  push(bytes: Uint8Array): void {
    const next = new Uint8Array(this.#buf.length + bytes.length);
    next.set(this.#buf, 0);
    next.set(bytes, this.#buf.length);
    this.#buf = next;
  }

  /**
   * Bytes buffered that do not yet make a whole frame.
   *
   * @returns The byte count.
   */
  get buffered(): number {
    return this.#buf.length;
  }

  /**
   * Takes the next complete frame, if one is buffered.
   *
   * @returns The frame, or `null` when more bytes are needed.
   * @throws TypeError when the header shows a malformed frame.
   */
  next(): RelayFrame | null {
    if (this.#buf.length < HEADER_BYTES) return null;
    const kind = this.#buf[0];
    const len = (this.#buf[1] << 8) | this.#buf[2];
    if (kind === KIND_DATA) {
      if (len === 0 || len > MAX_TCP_FRAME_PAYLOAD) {
        throw new TypeError(`relay sent a data frame of ${len} bytes`);
      }
    } else if (kind === KIND_TLS) {
      if (len !== 0) throw new TypeError(`relay sent a TLS-ready frame with ${len} bytes`);
    } else if (kind === KIND_TLS_FAILED) {
      if (len > MAX_TLS_FAILURE_REASON_BYTES) {
        throw new TypeError(`relay sent a TLS-failed reason of ${len} bytes`);
      }
    } else {
      throw new TypeError(`relay sent unknown frame kind ${kind}`);
    }
    if (this.#buf.length < HEADER_BYTES + len) return null;
    const payload = this.#buf.slice(HEADER_BYTES, HEADER_BYTES + len);
    this.#buf = this.#buf.slice(HEADER_BYTES + len);
    if (kind === KIND_DATA) return { kind: "data", bytes: payload };
    if (kind === KIND_TLS) return { kind: "tlsReady" };
    return { kind: "tlsFailed", reason: new TextDecoder().decode(payload) };
  }
}

/**
 * Encodes plaintext for the server as data frames.
 *
 * @param bytes - Bytes to send; empty encodes nothing.
 * @returns One frame per {@link MAX_TCP_FRAME_PAYLOAD} bytes.
 */
export function encodeDataFrames(bytes: Uint8Array): Uint8Array {
  const count = Math.ceil(bytes.length / MAX_TCP_FRAME_PAYLOAD);
  const out = new Uint8Array(bytes.length + count * HEADER_BYTES);
  let at = 0;
  for (let start = 0; start < bytes.length; start += MAX_TCP_FRAME_PAYLOAD) {
    const chunk = bytes.subarray(start, start + MAX_TCP_FRAME_PAYLOAD);
    out[at] = KIND_DATA;
    out[at + 1] = chunk.length >> 8;
    out[at + 2] = chunk.length & 0xff;
    out.set(chunk, at + HEADER_BYTES);
    at += HEADER_BYTES + chunk.length;
  }
  return out;
}

/**
 * The start-TLS frame.
 *
 * @returns The three-byte frame.
 */
export function encodeStartTlsFrame(): Uint8Array {
  return Uint8Array.of(KIND_TLS, 0, 0);
}

/**
 * Parses a `"host:port"` address as Workers `connect()` does.
 *
 * @param address - `host:port`, IPv6 in brackets, or an address object.
 * @returns The validated address.
 * @throws TypeError for a missing host or a port outside 1 to 65535.
 */
export function parseTcpAddress(address: string | TcpSocketAddress): TcpSocketAddress {
  let hostname: string;
  let port: number;
  if (typeof address === "string") {
    const idx = address.lastIndexOf(":");
    if (idx <= 0) throw new TypeError(`connect address ${address} has no port`);
    hostname = address.slice(0, idx);
    if (hostname.startsWith("[") && hostname.endsWith("]")) hostname = hostname.slice(1, -1);
    port = Number(address.slice(idx + 1));
  } else {
    hostname = address.hostname;
    port = address.port;
  }
  if (typeof hostname !== "string" || hostname.length === 0) {
    throw new TypeError("connect address has no host");
  }
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new TypeError(`connect port ${String(port)} is not 1 to 65535`);
  }
  return { hostname, port };
}

/**
 * Removes the framing suffix from an address workerd reports.
 *
 * @param text - The reported address, if any.
 * @returns The address without the suffix.
 */
function withoutSuffix(text: string | undefined): string | undefined {
  if (text === undefined) return undefined;
  const at = text.toLowerCase().indexOf(STARTTLS_HOST_SUFFIX);
  return at === -1 ? text : text.slice(0, at) + text.slice(at + STARTTLS_HOST_SUFFIX.length);
}

/** A promise with its settle functions. */
interface Deferred<T> {
  /** The promise. */
  promise: Promise<T>;
  /** Fulfils it. */
  resolve: (value: T) => void;
  /** Rejects it. */
  reject: (reason: unknown) => void;
}

/**
 * A new {@link Deferred} whose rejection is never reported as unhandled.
 *
 * @returns The deferred.
 */
function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  promise.catch(() => undefined);
  return { promise, resolve, reject };
}

/**
 * One framed tunnel and the socket views on it: the plain view, then, after
 * an upgrade, the secure one.
 */
class FramedTunnel {
  /** The raw socket to the relay. */
  readonly #raw: TcpSocket;
  /** Writer on the raw socket, shared by every view in order. */
  readonly #writer: WritableStreamDefaultWriter<Uint8Array>;
  /** Frames from the relay. */
  readonly #decoder = new RelayFrameDecoder();
  /** Info the raw socket reported, with the suffix removed. */
  readonly #info: Promise<TcpSocketInfo>;
  /** Writes queued so far, so frames go out in order. */
  #writes: Promise<void> = Promise.resolve();
  /** Controller of the view that receives server data now. */
  #sink: ReadableStreamDefaultController<Uint8Array> | null = null;
  /** The upgrade awaiting the relay's answer. */
  #upgrade: Deferred<void> | null = null;
  /** Whether the tunnel has upgraded or is upgrading. */
  #upgraded = false;
  /** Set once the tunnel failed; later reads and writes report it. */
  #failure: unknown = null;
  /** Resolves when the raw socket closes. */
  readonly closed: Promise<void>;

  /**
   * @param raw - The raw socket to the relay.
   */
  constructor(raw: TcpSocket) {
    this.#raw = raw;
    this.#writer = raw.writable.getWriter();
    this.#info = raw.opened.then((info) => ({
      remoteAddress: withoutSuffix(info?.remoteAddress),
      localAddress: withoutSuffix(info?.localAddress),
    }));
    this.#info.catch(() => undefined);
    this.closed = raw.closed;
    this.closed.catch(() => undefined);
    void this.#pump();
  }

  /** Reads relay frames and routes them until the raw socket ends. */
  async #pump(): Promise<void> {
    const reader = this.#raw.readable.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        this.#decoder.push(value);
        for (let frame = this.#decoder.next(); frame; frame = this.#decoder.next()) {
          this.#route(frame);
        }
      }
      if (this.#decoder.buffered > 0) throw new TypeError("relay ended inside a frame");
      this.#upgrade?.reject(new Error("the connection closed before TLS was ready"));
      this.#upgrade = null;
      try {
        this.#sink?.close();
      } catch {
        /* the reader cancelled */
      }
    } catch (err) {
      this.#fail(err);
    } finally {
      reader.releaseLock();
    }
  }

  /**
   * Handles one relay frame.
   *
   * @param frame - The frame the relay sent.
   */
  #route(frame: RelayFrame): void {
    switch (frame.kind) {
      case "data":
        try {
          this.#sink?.enqueue(frame.bytes);
        } catch {
          /* the reader cancelled; drop */
        }
        return;
      case "tlsReady":
        if (!this.#upgrade) throw new TypeError("relay sent TLS ready unasked");
        this.#upgrade.resolve();
        this.#upgrade = null;
        return;
      case "tlsFailed": {
        if (!this.#upgrade) throw new TypeError("relay sent TLS failed unasked");
        const err = new Error(`TLS handshake failed: ${frame.reason}`);
        this.#upgrade.reject(err);
        this.#upgrade = null;
        this.#fail(err);
        return;
      }
    }
  }

  /**
   * Ends the tunnel with `err`.
   *
   * @param err - Why the tunnel ended.
   */
  #fail(err: unknown): void {
    if (this.#failure === null) this.#failure = err;
    this.#upgrade?.reject(err);
    this.#upgrade = null;
    try {
      this.#sink?.error(err);
    } catch {
      /* already closed */
    }
    void this.#raw.close().catch(() => undefined);
  }

  /**
   * Queues `bytes` (already framed) behind earlier writes.
   *
   * @param bytes - Framed bytes for the relay.
   * @returns Settles when the bytes are written.
   */
  #write(bytes: Uint8Array): Promise<void> {
    const next = this.#writes.then(() => {
      if (this.#failure !== null) throw this.#failure;
      return this.#writer.write(bytes);
    });
    this.#writes = next.catch(() => undefined);
    return next;
  }

  /**
   * A socket view that receives server data from now on.
   *
   * @param opened - What the view's `opened` resolves to.
   * @param canUpgrade - Whether the view offers `startTls()`.
   * @returns The view.
   */
  view(opened: Promise<TcpSocketInfo>, canUpgrade: boolean): TcpSocket {
    let usable = true;
    const readable = new ReadableStream<Uint8Array>({
      start: (controller) => {
        this.#sink = controller;
      },
    });
    const writable = new WritableStream<Uint8Array>({
      write: async (chunk) => {
        if (!usable) throw new TypeError("the socket upgraded; write to the secure socket");
        await opened;
        if (chunk.length > 0) await this.#write(encodeDataFrames(chunk));
      },
      close: async () => {
        await this.#writes;
      },
    });
    const socket: TcpSocket = {
      readable,
      writable,
      opened,
      closed: this.closed,
      close: async () => {
        usable = false;
        await this.#raw.close();
      },
      startTls: () => {
        if (!canUpgrade || !usable || this.#upgraded) {
          throw new TypeError('startTls() needs a socket opened with secureTransport: "starttls"');
        }
        usable = false;
        return this.startTls();
      },
    };
    return socket;
  }

  /**
   * Sends the start-TLS frame and returns the secure view.
   *
   * @returns The view whose `opened` settles with the relay's answer.
   */
  startTls(): TcpSocket {
    this.#upgraded = true;
    const previous = this.#sink;
    const upgrade = deferred<void>();
    this.#upgrade = upgrade;
    const secure = this.view(
      Promise.all([this.#info, upgrade.promise]).then(([info]) => info),
      false,
    );
    try {
      previous?.close();
    } catch {
      /* the reader cancelled */
    }
    this.#write(encodeStartTlsFrame()).catch((err: unknown) => this.#fail(err));
    return secure;
  }

  /**
   * What the plain view's `opened` resolves to.
   *
   * @returns The connection info promise.
   */
  get info(): Promise<TcpSocketInfo> {
    return this.#info;
  }
}

/**
 * Builds `connect()` over a Workers-style `dial`.
 *
 * @param dial - `connect` from `cloudflare:sockets`, or a test fake.
 * @returns A `connect()` with working `startTls()` and `secureTransport: "on"`.
 */
export function createConnect(dial: TcpDial): TcpConnect {
  return (address, options = {}) => {
    const target = parseTcpAddress(address);
    const mode = options.secureTransport ?? "off";
    const allowHalfOpen = options.allowHalfOpen ?? false;
    if (mode === "off") {
      return dial(target, { secureTransport: "off", allowHalfOpen });
    }
    if (mode !== "on" && mode !== "starttls") {
      throw new TypeError(`secureTransport ${String(mode)} is not off, on or starttls`);
    }
    if (target.hostname.includes(":")) {
      throw new TypeError("TLS sockets need a host name or IPv4 address, not IPv6");
    }
    if (target.hostname.toLowerCase().endsWith(STARTTLS_HOST_SUFFIX)) {
      throw new TypeError(`connect host may not end in ${STARTTLS_HOST_SUFFIX}`);
    }
    const raw = dial(
      { hostname: `${target.hostname}${STARTTLS_HOST_SUFFIX}`, port: target.port },
      { secureTransport: "off", allowHalfOpen },
    );
    const tunnel = new FramedTunnel(raw);
    return mode === "on" ? tunnel.startTls() : tunnel.view(tunnel.info, true);
  };
}
