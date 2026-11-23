/**
 * One Cloudflare Tunnel connection, sans-IO apart from the
 * {@link SessionIo} each call is given.
 *
 * The connector dials the edge and then serves HTTP/2 on the connection:
 * the edge sends the client preface, opens a control stream on which the
 * connector registers over Cap'n Proto RPC, and then opens a stream for
 * each public request. A request goes to the library and its answer comes
 * back as a header block and DATA frames; a WebSocket request opens a
 * socket to the library and the stream then carries frames both ways
 * ({@link Session.fromLibrary}, {@link Session.libraryClosed}). The edge's
 * configuration pushes are acknowledged and otherwise ignored: every
 * request goes to the library.
 *
 * Response fields travel the way cloudflared sends them: serialized into
 * one `cf-cloudflared-response-headers` field, so HTTP/2's field rules
 * don't apply to HTTP/1 values, with `cf-cloudflared-response-meta` naming
 * who produced the answer.
 *
 * Flow control follows RFC 9113: the session credits the edge's windows as
 * soon as it has taken the bytes, and never sends DATA past the stream's or
 * the connection's window; a response that doesn't fit waits in memory
 * until a window update arrives.
 */

import { CapnpError, MessageFramer } from "./capnp.ts";
import {
  DEFAULT_MAX_FRAME,
  DEFAULT_WINDOW,
  ErrorCode,
  FrameReader,
  H2Error,
  MAX_WINDOW,
  Setting,
  encodeData,
  encodeGoAway,
  encodeHeaders as encodeHeaderFrames,
  encodePing,
  encodeRst,
  encodeSettings,
  encodeSettingsAck,
  encodeWindowUpdate,
  join,
  type Frame,
} from "./h2.ts";
import { HpackDecoder, HpackError, encodeHeaders, type Field } from "./hpack.ts";
import {
  REGISTER_QUESTION,
  encodeBootstrap,
  encodeFinish,
  encodeRegisterConnection,
  encodeUnimplementedReturn,
  readIncoming,
  type Registration,
} from "./rpc.ts";
import {
  MAX_REQUEST_BODY,
  continueBody,
  errorAnswer,
  serveRequest,
  serveUpgrade,
  type Answer,
  type Continuation,
  type Library,
  type RequestHead,
  type Source,
} from "./serve.ts";
import {
  CloseCode,
  FrameDecoder,
  Opcode,
  WsError,
  encodeClose,
  encodeFrame,
  isSendableCloseCode,
} from "./ws.ts";

/**
 * Largest socket message the session writes, in bytes; the host's limit
 * (`MAX_SOCKET_FRAME_BYTES`).
 */
export const MAX_WRITE_BATCH = 256 * 1024;

/** Most streams a session keeps state for at once. */
export const MAX_STREAMS = 256;

/**
 * Most bytes the session holds for all streams together: request bodies
 * being read and responses waiting for window. One isolate has about
 * 128 MiB, and many slow streams must not exhaust it.
 */
export const MAX_SESSION_BYTES = 64 * 1024 * 1024;

/**
 * Largest WebSocket message passed between a client and the library, in
 * bytes; the host's socket frame limit (`MAX_SOCKET_FRAME_BYTES`).
 */
export const MAX_SOCKET_MESSAGE = 256 * 1024;

/**
 * Most bytes a WebSocket stream may hold waiting for the edge's window.
 * Past it the client is not reading, and the connection is closed.
 */
export const MAX_SOCKET_BACKLOG = 4 * 1024 * 1024;

/** The window this side gives each stream the edge opens, in bytes. */
export const STREAM_WINDOW = 256 * 1024;

/** The window this side gives the whole connection, in bytes. */
export const CONNECTION_WINDOW = 4 * 1024 * 1024;

/** Largest field list a request may carry (`SETTINGS_MAX_HEADER_LIST_SIZE`). */
export const MAX_HEADER_LIST = 64 * 1024;

/** Largest configuration push the session reads, in bytes. */
const MAX_CONFIG_BYTES = 1024 * 1024;

/** The field the edge marks a stream's purpose with. */
const UPGRADE_FIELD = "cf-cloudflared-proxy-connection-upgrade";

/** The field that marks a raw TCP (WARP) stream, which the connector refuses. */
const TCP_SOURCE_FIELD = "cf-cloudflared-proxy-src";

/** The response field holding the serialized response fields. */
const RESPONSE_FIELDS = "cf-cloudflared-response-headers";

/** The response field naming who produced the answer. */
const RESPONSE_META = "cf-cloudflared-response-meta";

/** The field that carries the client's address. */
const CLIENT_ADDRESS_FIELD = "cf-connecting-ip";

/** An IPv4 or IPv6 address, loosely: only what a client address may hold. */
const IP_ADDRESS = /^(?:\d{1,3}(?:\.\d{1,3}){3}|[0-9A-Fa-f:.]{2,45})$/;

/** What a session call reaches outside itself. */
export interface SessionIo {
  /**
   * Writes bytes to the edge connection.
   *
   * @param bytes - Whole HTTP/2 frames.
   */
  write(bytes: Uint8Array): Promise<void>;
  /** The library. */
  library: Library;
}

/** Where a session is. */
export type Phase =
  /** Waiting for the edge to open the control stream. */
  | "connecting"
  /** The registration is out; waiting for its answer. */
  | "registering"
  /** Registered: the tunnel serves. */
  | "serving"
  /** The edge refused the registration. */
  | "failed"
  /** The connection ended. */
  | "closed";

/** What a response still has to write on its stream. */
interface Outbound {
  /** Bytes the edge will still take on this stream. */
  window: number;
  /** Bytes ready to write, in order. */
  queue: Uint8Array[];
  /** The rest of a large body, fetched when the window opens. */
  more: Continuation | null;
  /** Whether the response is all queued (END_STREAM goes after the queue). */
  complete: boolean;
}

/** The control stream: Cap'n Proto RPC both ways. */
interface Control {
  /** Kind tag. */
  kind: "control";
  /** Messages from the edge. */
  framer: MessageFramer;
  /** What the session writes on it. */
  out: Outbound;
}

/** A configuration push from the edge. */
interface Config {
  /** Kind tag. */
  kind: "config";
  /** Body bytes so far. */
  body: Uint8Array[];
  /** Their summed length. */
  bytes: number;
  /** The answer. */
  out: Outbound;
  /** Whether this side ended the stream. */
  ended: boolean;
}

/** A proxied request. */
interface Proxy {
  /** Kind tag. */
  kind: "proxy";
  /**
   * What the stream is reading: the body, WebSocket frames once an upgrade
   * was accepted, or nothing more.
   */
  reading: "body" | "socket" | "done";
  /** The request. */
  head: RequestHead;
  /** The client's IP. */
  clientAddress: string | undefined;
  /** Body bytes so far. */
  body: Uint8Array[];
  /** Body bytes so far, summed. */
  bodyBytes: number;
  /** The response being written. */
  out: Outbound;
  /** Whether this side ended the stream. */
  ended: boolean;
  /** Whether the edge ended its side. */
  remoteEnded: boolean;
  /** Whether an upgrade waits for the library's answer. */
  connecting: boolean;
  /** Frames from the library that arrived before its socket was known, encoded. */
  early: Uint8Array[];
  /** The library's socket, while the WebSocket is open. */
  socket: string | null;
  /** The client's WebSocket frames, while the WebSocket is open. */
  frames: FrameDecoder | null;
  /** Whether a close frame went to the client. */
  closeSent: boolean;
}

/** A close to send one side of a WebSocket. */
interface Close {
  /** The code, or `null` for a close frame without one. */
  code: number | null;
  /** The reason. */
  reason: string;
}

/** A stream the session keeps state for. */
type StreamState = Control | Config | Proxy;

/** How a session was configured. */
export interface SessionOptions {
  /** What to register. */
  registration: Registration;
  /** The public hostname requests arrive on. */
  hostname: string;
}

/**
 * Bytes a stream has queued for the edge's window.
 *
 * @param out - The stream's outbound side.
 * @returns The sum.
 */
function backlog(out: Outbound): number {
  let total = 0;
  for (const part of out.queue) total += part.length;
  return total;
}

/**
 * A field name in Go's canonical form (`content-type` becomes
 * `Content-Type`), as cloudflared serializes them.
 *
 * @param name - The name.
 * @returns The canonical name.
 */
export function canonicalName(name: string): string {
  return name
    .toLowerCase()
    .split("-")
    .map((part) => (part ? (part[0] as string).toUpperCase() + part.slice(1) : part))
    .join("-");
}

/**
 * Base64 without padding (Go's `RawStdEncoding`) of a Latin-1 string's
 * octets.
 *
 * @param text - The string.
 * @returns The encoding.
 */
function rawBase64(text: string): string {
  let binary = "";
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    binary += String.fromCharCode(code <= 0xff ? code : 0x3f);
  }
  return btoa(binary).replace(/=+$/, "");
}

/**
 * Serializes response fields the way cloudflared does: each name and
 * value in unpadded base64, `name:value` pairs joined by `;`.
 *
 * @param fields - The fields.
 * @returns The serialized value.
 */
export function serializeFields(fields: ReadonlyArray<readonly [string, string]>): string {
  return fields
    .map(([name, value]) => `${rawBase64(canonicalName(name))}:${rawBase64(value)}`)
    .join(";");
}

/** One tunnel connection. */
export class Session {
  /** Where the session is. */
  phase: Phase = "connecting";
  /** Why it failed or closed, for `health`; never holds the tunnel secret. */
  error = "";
  /** The edge location this connection landed on, once registered. */
  location = "";
  /**
   * The number of this connector's last ping whose answer arrived, or 0.
   * Pings are numbered from 1 as {@link Session.beat} sends them, so a
   * caller waits for its own ping's answer, not for one that arrived
   * earlier.
   */
  echoed = 0;
  /** The session's configuration. */
  readonly options: SessionOptions;
  /** Frames decoded from the connection. */
  readonly #reader = new FrameReader(DEFAULT_MAX_FRAME);
  /** The edge's header compression context. */
  readonly #hpack = new HpackDecoder(MAX_HEADER_LIST);
  /** Streams with state. */
  readonly #streams = new Map<number, StreamState>();
  /** The highest stream id the edge opened. */
  #lastStream = 0;
  /** The control stream's id, once opened. */
  #control = 0;
  /** Bytes the edge will still take on the whole connection. */
  #sendWindow = DEFAULT_WINDOW;
  /** The edge's initial stream window. */
  #peerWindow = DEFAULT_WINDOW;
  /** The edge's largest frame payload. */
  #peerMaxFrame = DEFAULT_MAX_FRAME;
  /** Pings this connector sent. */
  #beats = 0;
  /** The ping waiting for its answer, and its number. */
  #pending: { data: Uint8Array; ticket: number } | null = null;
  /** Frames waiting for the next write. */
  #out: Uint8Array[] = [];

  /**
   * Creates a session; nothing is sent until {@link Session.start}.
   *
   * @param options - Registration and hostname.
   */
  constructor(options: SessionOptions) {
    this.options = options;
  }

  /**
   * Queues frames for the next write.
   *
   * @param frame - Encoded frames.
   */
  #emit(frame: Uint8Array): void {
    this.#out.push(frame);
  }

  /**
   * Writes the queued frames, in batches no larger than the host takes.
   *
   * @param io - The connection.
   * @returns Resolves once written.
   */
  async #flush(io: SessionIo): Promise<void> {
    while (this.#out.length > 0) {
      const batch: Uint8Array[] = [];
      let size = 0;
      while (this.#out.length > 0) {
        const next = this.#out[0] as Uint8Array;
        if (batch.length > 0 && size + next.length > MAX_WRITE_BATCH) break;
        batch.push(next);
        size += next.length;
        this.#out.shift();
      }
      await io.write(join(...batch));
    }
  }

  /**
   * Sends the server preface: this side's settings and a wider connection
   * window.
   *
   * @param io - The connection.
   * @returns Resolves once written.
   */
  async start(io: SessionIo): Promise<void> {
    this.#emit(
      encodeSettings([
        [Setting.MaxConcurrentStreams, MAX_STREAMS],
        [Setting.InitialWindowSize, STREAM_WINDOW],
        [Setting.MaxHeaderListSize, MAX_HEADER_LIST],
      ]),
    );
    this.#emit(encodeWindowUpdate(0, CONNECTION_WINDOW - DEFAULT_WINDOW));
    await this.#flush(io);
  }

  /**
   * Sends one ping; its answer sets {@link Session.echoed} to the ping's
   * number.
   *
   * @param io - The connection.
   * @param data - 8 bytes to send (random outside tests).
   * @returns The ping's number.
   */
  async beat(io: SessionIo, data: Uint8Array): Promise<number> {
    const ticket = ++this.#beats;
    this.#pending = { data: data.slice(0, 8), ticket };
    await io.write(encodePing(this.#pending.data, false));
    return ticket;
  }

  /**
   * Handles bytes read from the connection.
   *
   * @param io - The connection and the library.
   * @param chunk - The bytes.
   * @returns Resolves once every frame they complete was handled and the
   *   answers written.
   * @throws {H2Error} When the edge broke the protocol; the session sent
   *   GOAWAY and the connection must close.
   */
  async receive(io: SessionIo, chunk: Uint8Array): Promise<void> {
    try {
      const frames = this.#reader.push(chunk);
      for (const frame of frames) await this.#frame(io, frame);
    } catch (err) {
      const h2 =
        err instanceof H2Error
          ? err
          : err instanceof HpackError
            ? new H2Error(ErrorCode.CompressionError, err.message)
            : null;
      if (h2 !== null) {
        await this.#fail(io, h2);
        throw h2;
      }
      throw err;
    }
    await this.#flush(io);
  }

  /**
   * Ends the session after a protocol error.
   *
   * @param io - The connection.
   * @param err - The error.
   * @returns Resolves once GOAWAY was written (or could not be).
   */
  async #fail(io: SessionIo, err: H2Error): Promise<void> {
    this.phase = "closed";
    this.error = `the Cloudflare edge broke the HTTP/2 protocol: ${err.message}`;
    this.#out = [];
    await io.write(encodeGoAway(this.#lastStream, err.code, err.message)).catch(() => {});
  }

  /**
   * Handles one frame.
   *
   * @param io - The connection and the library.
   * @param frame - The frame.
   * @returns Resolves once handled.
   */
  async #frame(io: SessionIo, frame: Frame): Promise<void> {
    switch (frame.kind) {
      case "settings":
        if (!frame.ack) this.#settings(frame.params);
        return;
      case "ping":
        if (!frame.ack) {
          this.#emit(encodePing(frame.data, true));
        } else if (
          this.#pending !== null &&
          frame.data.every((b, i) => b === this.#pending?.data[i])
        ) {
          this.echoed = this.#pending.ticket;
          this.#pending = null;
        }
        return;
      case "goaway":
        this.phase = "closed";
        this.error = frame.debug
          ? `the Cloudflare edge ended the connection: ${frame.debug}`
          : `the Cloudflare edge ended the connection (code ${frame.code})`;
        return;
      case "rst": {
        const state = this.#streams.get(frame.stream);
        this.#streams.delete(frame.stream);
        if (frame.stream === this.#control && this.phase !== "failed") {
          this.phase = "closed";
          this.error = "the Cloudflare edge reset the control stream";
        }
        if (state?.kind === "proxy" && state.socket !== null) {
          await this.#closeLibrary(io, state, CloseCode.GoingAway, "the client went away");
        }
        return;
      }
      case "window":
        await this.#window(io, frame.stream, frame.increment);
        return;
      case "headers":
        await this.#headers(io, frame.stream, frame.block, frame.endStream);
        return;
      case "data":
        await this.#data(io, frame.stream, frame.data, frame.endStream, frame.flow);
        return;
    }
  }

  /**
   * Applies the edge's settings and acknowledges them.
   *
   * @param params - Setting ids and values.
   * @throws {H2Error} When a new initial window overflows a stream's.
   */
  #settings(params: Array<[number, number]>): void {
    for (const [id, value] of params) {
      if (id === Setting.MaxFrameSize) {
        this.#peerMaxFrame = value;
      } else if (id === Setting.InitialWindowSize) {
        const delta = value - this.#peerWindow;
        this.#peerWindow = value;
        for (const state of this.#streams.values()) {
          state.out.window += delta;
          if (state.out.window > MAX_WINDOW) {
            throw new H2Error(ErrorCode.FlowControlError, "a stream window past 2^31-1");
          }
        }
      }
    }
    this.#emit(encodeSettingsAck());
    for (const [id, state] of this.#streams) this.#send(id, state);
  }

  /**
   * Widens a window and writes what now fits.
   *
   * @param io - The connection and the library.
   * @param stream - The stream, or 0 for the connection.
   * @param increment - The increment.
   * @returns Resolves once written as far as the windows allow.
   * @throws {H2Error} When the connection's window passes 2^31-1.
   */
  async #window(io: SessionIo, stream: number, increment: number): Promise<void> {
    if (stream === 0) {
      this.#sendWindow += increment;
      if (this.#sendWindow > MAX_WINDOW) {
        throw new H2Error(ErrorCode.FlowControlError, "connection window past 2^31-1");
      }
      for (const [id, state] of [...this.#streams]) await this.#drain(io, id, state);
      return;
    }
    const state = this.#streams.get(stream);
    if (state === undefined) return;
    state.out.window += increment;
    if (state.out.window > MAX_WINDOW) {
      this.#reset(stream, ErrorCode.FlowControlError);
      if (state.kind === "proxy" && state.socket !== null) {
        await this.#closeLibrary(io, state, CloseCode.GoingAway, "the client went away");
      }
      return;
    }
    await this.#drain(io, stream, state);
  }

  /**
   * Handles a header block: a new stream's request, or a request's
   * trailers.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param block - The HPACK block.
   * @param endStream - Whether the edge ended its side.
   * @returns Resolves once handled.
   * @throws {H2Error} For an even stream id or one that goes backwards.
   */
  async #headers(io: SessionIo, id: number, block: Uint8Array, endStream: boolean): Promise<void> {
    // Every block passes through the decoder, to keep its table in step.
    const decoded = this.#hpack.decode(block);
    const existing = this.#streams.get(id);
    if (existing !== undefined) {
      // Trailers: they end the body; their fields are not passed on.
      if (!endStream) throw new H2Error(ErrorCode.ProtocolError, "trailers without END_STREAM");
      if (existing.kind === "proxy") await this.#bodyEnd(io, id, existing);
      else if (existing.kind === "config") await this.#configEnd(io, id, existing);
      return;
    }
    if (id % 2 === 0) throw new H2Error(ErrorCode.ProtocolError, `HEADERS on stream ${id}`);
    if (id <= this.#lastStream) {
      // A stream that already ended: RFC 9113 section 5.1, closed.
      this.#emit(encodeRst(id, ErrorCode.StreamClosed));
      return;
    }
    this.#lastStream = id;
    if (this.#streams.size >= MAX_STREAMS || this.phase === "closed") {
      this.#emit(encodeRst(id, ErrorCode.RefusedStream));
      return;
    }
    const out = (): Outbound => ({
      window: this.#peerWindow,
      queue: [],
      more: null,
      complete: false,
    });
    const head = requestHead(decoded.fields);
    const purpose = head?.headers.find(([name]) => name === UPGRADE_FIELD)?.[1] ?? "";
    if (purpose === "control-stream") {
      await this.#openControl(io, id, out());
      return;
    }
    if (purpose === "update-configuration") {
      const state: Config = { kind: "config", body: [], bytes: 0, out: out(), ended: false };
      this.#streams.set(id, state);
      if (endStream) await this.#configEnd(io, id, state);
      return;
    }
    const proxy: Proxy = {
      kind: "proxy",
      reading: "body",
      head: head ?? { method: "GET", target: "/", headers: [] },
      clientAddress: clientAddressOf(head?.headers ?? []),
      body: [],
      bodyBytes: 0,
      out: out(),
      ended: false,
      remoteEnded: endStream,
      connecting: false,
      early: [],
      socket: null,
      frames: null,
      closeSent: false,
    };
    this.#streams.set(id, proxy);
    if (decoded.oversized) {
      await this.#answer(io, id, proxy, errorAnswer(431, "The request's fields are too large."));
      return;
    }
    if (head === null) {
      this.#streams.delete(id);
      this.#emit(encodeRst(id, ErrorCode.ProtocolError));
      return;
    }
    if (head.headers.some(([name]) => name === TCP_SOURCE_FIELD)) {
      await this.#answer(
        io,
        id,
        proxy,
        errorAnswer(502, "TCP streams are not served through this tunnel."),
      );
      return;
    }
    if (this.phase !== "serving") {
      await this.#answer(io, id, proxy, errorAnswer(503, "The tunnel is not registered yet."));
      return;
    }
    if (purpose === "websocket") {
      await this.#upgrade(io, id, proxy);
      return;
    }
    if (endStream) await this.#bodyEnd(io, id, proxy);
  }

  /**
   * Opens the control stream: answers its request and sends the bootstrap
   * request and the registration.
   *
   * @param io - The connection.
   * @param id - The stream.
   * @param out - Its outbound side.
   * @returns Resolves once queued as far as the windows allow.
   */
  async #openControl(io: SessionIo, id: number, out: Outbound): Promise<void> {
    if (this.#control !== 0) {
      this.#emit(encodeRst(id, ErrorCode.RefusedStream));
      return;
    }
    this.#control = id;
    const state: Control = { kind: "control", framer: new MessageFramer(), out };
    this.#streams.set(id, state);
    this.#emit(
      encodeHeaderFrames(id, encodeHeaders([[":status", "200"]]), false, this.#peerMaxFrame),
    );
    out.queue.push(encodeBootstrap(), encodeRegisterConnection(this.options.registration));
    this.phase = "registering";
    await this.#drain(io, id, state);
  }

  /**
   * Handles one DATA frame.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param data - The bytes, without padding.
   * @param endStream - Whether the edge ended its side.
   * @param flow - The bytes flow control counts.
   * @returns Resolves once handled.
   */
  async #data(
    io: SessionIo,
    id: number,
    data: Uint8Array,
    endStream: boolean,
    flow: number,
  ): Promise<void> {
    if (flow > 0) this.#emit(encodeWindowUpdate(0, flow));
    const state = this.#streams.get(id);
    if (state === undefined) return;
    if (flow > 0 && !endStream) this.#emit(encodeWindowUpdate(id, flow));
    switch (state.kind) {
      case "control":
        await this.#controlBytes(io, id, state, data, endStream);
        return;
      case "config":
        state.body.push(data);
        state.bytes += data.length;
        if (state.bytes > MAX_CONFIG_BYTES) {
          this.#reset(id, ErrorCode.EnhanceYourCalm);
          return;
        }
        if (endStream) await this.#configEnd(io, id, state);
        return;
      case "proxy":
        if (endStream) state.remoteEnded = true;
        await this.#proxyBytes(io, id, state, data, endStream);
        return;
    }
  }

  /**
   * Reads RPC messages on the control stream.
   *
   * @param io - The connection.
   * @param id - The stream.
   * @param state - Its state.
   * @param data - The bytes.
   * @param endStream - Whether the edge ended its side.
   * @returns Resolves once answered.
   */
  async #controlBytes(
    io: SessionIo,
    id: number,
    state: Control,
    data: Uint8Array,
    endStream: boolean,
  ): Promise<void> {
    let messages: Uint8Array[][];
    try {
      messages = state.framer.push(data);
      for (const segments of messages) this.#rpc(state, segments);
    } catch (err) {
      if (!(err instanceof CapnpError)) throw err;
      this.phase = "closed";
      this.error = `the Cloudflare edge sent a malformed control message: ${err.message}`;
      this.#reset(id, ErrorCode.ProtocolError);
      return;
    }
    if (endStream && this.phase !== "failed") {
      this.phase = "closed";
      this.error ||= "the Cloudflare edge closed the control stream";
    }
    await this.#drain(io, id, state);
  }

  /**
   * Acts on one RPC message from the edge.
   *
   * @param state - The control stream's state.
   * @param segments - The message.
   * @throws {CapnpError} When it is malformed.
   */
  #rpc(state: Control, segments: Uint8Array[]): void {
    const message = readIncoming(segments);
    switch (message.kind) {
      case "return": {
        if (message.answer !== REGISTER_QUESTION) return;
        state.out.queue.push(encodeFinish(REGISTER_QUESTION));
        const result = message.result;
        if (result?.kind === "registered") {
          this.phase = "serving";
          this.location = result.location;
          this.error = "";
        } else if (result?.kind === "refused") {
          this.phase = "failed";
          this.error = `Cloudflare refused the tunnel connection: ${result.cause || "no reason given"}`;
        } else {
          this.phase = "failed";
          this.error = `Cloudflare could not register the tunnel connection${
            result?.kind === "exception" && result.reason ? `: ${result.reason}` : ""
          }`;
        }
        return;
      }
      case "question":
        state.out.queue.push(
          encodeUnimplementedReturn(message.question, "the Bookclerk connector serves no calls"),
        );
        return;
      case "abort":
        this.phase = "closed";
        this.error = `the Cloudflare edge ended the control channel${message.reason ? `: ${message.reason}` : ""}`;
        return;
      case "unimplemented":
        this.phase = "failed";
        this.error = "the Cloudflare edge does not support this connector's registration";
        return;
      case "other":
        return;
    }
  }

  /**
   * Answers a configuration push: the connector takes the version and
   * ignores the rules, as every request goes to the library.
   *
   * @param io - The connection.
   * @param id - The stream.
   * @param state - Its state.
   * @returns Resolves once the answer went out as far as the windows allow.
   */
  async #configEnd(io: SessionIo, id: number, state: Config): Promise<void> {
    // Answered once: trailers after a whole body need nothing more.
    if (state.out.complete) return;
    let version = 0;
    try {
      const parsed = JSON.parse(new TextDecoder().decode(join(...state.body, new Uint8Array())));
      if (Number.isSafeInteger(parsed?.version)) version = parsed.version as number;
    } catch {
      // A malformed push still gets an answer, naming version 0.
    }
    state.body = [];
    const body = new TextEncoder().encode(
      JSON.stringify({ lastAppliedVersion: version, err: null }),
    );
    this.#emit(
      encodeHeaderFrames(
        id,
        encodeHeaders([
          [":status", "200"],
          ["content-type", "application/json"],
          ["content-length", String(body.length)],
        ]),
        false,
        this.#peerMaxFrame,
      ),
    );
    state.out.queue.push(body);
    state.out.complete = true;
    await this.#drain(io, id, state);
  }

  /**
   * Reads a proxied stream's bytes: body bytes until the request is
   * whole, then WebSocket frames once an upgrade was accepted.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param data - The bytes.
   * @param endStream - Whether the edge ended its side.
   * @returns Resolves once handled.
   */
  async #proxyBytes(
    io: SessionIo,
    id: number,
    state: Proxy,
    data: Uint8Array,
    endStream: boolean,
  ): Promise<void> {
    if (state.reading === "socket") {
      await this.#socketBytes(io, id, state, data, endStream);
      return;
    }
    if (state.connecting) {
      // Client bytes while the library answers the upgrade: read after it.
      if (data.length > 0) state.body.push(data);
      return;
    }
    if (state.reading !== "body") return;
    if (data.length > 0) {
      state.body.push(data);
      state.bodyBytes += data.length;
    }
    if (state.bodyBytes > MAX_REQUEST_BODY) {
      await this.#answer(
        io,
        id,
        state,
        errorAnswer(413, "The request body is too large for the Cloudflare Tunnel."),
      );
      return;
    }
    if (this.#held() > MAX_SESSION_BYTES) {
      await this.#answer(io, id, state, errorAnswer(503, "The Cloudflare Tunnel is busy."));
      return;
    }
    if (endStream) await this.#bodyEnd(io, id, state);
  }

  /**
   * Serves a request whose body is whole.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @returns Resolves once answered.
   */
  async #bodyEnd(io: SessionIo, id: number, state: Proxy): Promise<void> {
    state.remoteEnded = true;
    if (state.reading !== "body") {
      if (state.reading === "socket") {
        await this.#socketBytes(io, id, state, new Uint8Array(), true);
      }
      return;
    }
    state.reading = "done";
    const body =
      state.body.length === 0
        ? null
        : (join(...state.body, new Uint8Array()) as Uint8Array<ArrayBuffer>);
    state.body = [];
    const answer = await serveRequest(
      io.library,
      this.options.hostname,
      state.head,
      body,
      state.clientAddress,
    );
    await this.#answer(io, id, state, answer);
  }

  /**
   * Serves a WebSocket request: on the library's accept, answers 200 and
   * switches the stream to WebSocket frames.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @returns Resolves once answered.
   */
  async #upgrade(io: SessionIo, id: number, state: Proxy): Promise<void> {
    state.reading = "done";
    state.connecting = true;
    const answer = await serveUpgrade(
      io.library,
      this.options.hostname,
      state.head,
      state.clientAddress,
      id,
    );
    state.connecting = false;
    if (answer.kind === "answer") {
      state.early = [];
      await this.#answer(io, id, state, answer.answer);
      return;
    }
    if (this.#streams.get(id) !== state || this.phase === "closed" || state.remoteEnded) {
      // The edge reset or ended the stream, or the session ended, while the
      // library answered.
      await io.library
        .socket(answer.id)
        ?.close(CloseCode.GoingAway, "the client went away")
        .catch(() => {});
      if (this.#streams.get(id) === state) this.#reset(id, ErrorCode.Cancel);
      return;
    }
    state.reading = "socket";
    state.socket = answer.id;
    state.frames = new FrameDecoder(MAX_SOCKET_MESSAGE);
    this.#head(id, 200, answer.fields, null, "origin", false);
    state.out.queue.push(...state.early);
    state.early = [];
    this.#send(id, state);
    const pending = join(...state.body, new Uint8Array());
    state.body = [];
    if (pending.length > 0) await this.#socketBytes(io, id, state, pending, false);
  }

  /**
   * Reads a client's WebSocket frames and passes its messages to the
   * library.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param data - The bytes.
   * @param endStream - Whether the edge ended its side (the client went
   *   away).
   * @returns Resolves once the messages were passed on.
   */
  async #socketBytes(
    io: SessionIo,
    id: number,
    state: Proxy,
    data: Uint8Array,
    endStream: boolean,
  ): Promise<void> {
    let messages;
    try {
      messages = (state.frames as FrameDecoder).push(data);
    } catch (err) {
      if (!(err instanceof WsError)) throw err;
      const close = { code: err.code, reason: err.message };
      await this.#endSocket(io, id, state, close, close);
      return;
    }
    for (const message of messages) {
      switch (message.kind) {
        case "text":
        case "binary": {
          const socket = io.library.socket(state.socket as string);
          try {
            if (socket === null) throw new Error("gone");
            await socket.send(message.kind === "text" ? message.text : message.data);
          } catch {
            await this.#endSocket(
              io,
              id,
              state,
              { code: CloseCode.GoingAway, reason: "the library closed the connection" },
              null,
            );
            return;
          }
          break;
        }
        case "ping":
          state.out.queue.push(encodeFrame(Opcode.Pong, message.data));
          this.#send(id, state);
          break;
        case "pong":
          break;
        case "close":
          await this.#endSocket(
            io,
            id,
            state,
            { code: message.code, reason: message.reason },
            { code: message.code ?? CloseCode.Normal, reason: message.reason },
          );
          return;
      }
    }
    if (endStream) {
      await this.#endSocket(io, id, state, null, {
        code: CloseCode.GoingAway,
        reason: "the client went away",
      });
      return;
    }
    if (backlog(state.out) > MAX_SOCKET_BACKLOG) {
      // Pongs pile up when the client sends pings but doesn't read.
      const close = { code: CloseCode.PolicyViolation, reason: "the client is not reading" };
      await this.#endSocket(io, id, state, close, close);
    } else if (this.#held() > MAX_SESSION_BYTES) {
      const close = { code: CloseCode.TryAgainLater, reason: "the Cloudflare Tunnel is busy" };
      await this.#endSocket(io, id, state, close, close);
    }
  }

  /**
   * Ends a stream's WebSocket: a close frame to the client, then
   * END_STREAM once the queue drains, and a close to the library.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param client - The close frame for the client, or `null` for none.
   * @param library - The close for the library's socket, or `null` when it
   *   closed already.
   * @returns Resolves once both were sent as far as the window allows.
   */
  async #endSocket(
    io: SessionIo,
    id: number,
    state: Proxy,
    client: Close | null,
    library: Close | null,
  ): Promise<void> {
    state.reading = "done";
    state.frames = null;
    if (client !== null && !state.closeSent) {
      state.out.queue.push(encodeClose(client.code, client.reason));
      state.closeSent = true;
    }
    state.out.complete = true;
    if (library !== null) {
      await this.#closeLibrary(io, state, library.code ?? CloseCode.Normal, library.reason);
    }
    state.socket = null;
    await this.#drain(io, id, state);
  }

  /**
   * Closes a stream's socket to the library, once.
   *
   * @param io - The library.
   * @param state - The stream's state.
   * @param code - Close code.
   * @param reason - Close reason.
   * @returns Resolves once the close was sent (or the socket was gone).
   */
  async #closeLibrary(io: SessionIo, state: Proxy, code: number, reason: string): Promise<void> {
    const id = state.socket;
    state.socket = null;
    if (id === null) return;
    await io.library
      .socket(id)
      ?.close(isSendableCloseCode(code) ? code : CloseCode.Normal, reason)
      .catch(() => {});
  }

  /**
   * Passes a message from the library's socket to the client.
   *
   * @param io - The connection.
   * @param stream - The stream the socket belongs to.
   * @param message - The message.
   * @returns `false` when the stream no longer carries the WebSocket (the
   *   caller closes the library's socket).
   */
  async fromLibrary(
    io: SessionIo,
    stream: number,
    message: string | ArrayBuffer,
  ): Promise<boolean> {
    const state = this.#streams.get(stream);
    if (state?.kind !== "proxy") return false;
    const frame =
      typeof message === "string"
        ? encodeFrame(Opcode.Text, new TextEncoder().encode(message))
        : encodeFrame(Opcode.Binary, new Uint8Array(message));
    if (state.connecting) {
      // The library spoke before its accept reached this session.
      let early = frame.length;
      for (const part of state.early) early += part.length;
      if (early > MAX_SOCKET_BACKLOG) return false;
      state.early.push(frame);
      return true;
    }
    if (state.reading !== "socket") return false;
    state.out.queue.push(frame);
    if (backlog(state.out) > MAX_SOCKET_BACKLOG || this.#held() > MAX_SESSION_BYTES) {
      const close = { code: CloseCode.PolicyViolation, reason: "the client is not reading" };
      await this.#endSocket(io, stream, state, close, close);
    } else {
      this.#send(stream, state);
    }
    await this.#flush(io);
    return true;
  }

  /**
   * Passes the library's close to the client.
   *
   * @param io - The connection.
   * @param stream - The stream the socket belonged to.
   * @param code - The library's close code (1005 when it sent none).
   * @param reason - Its reason.
   * @returns Resolves once the close frame went out as far as the window
   *   allows.
   */
  async libraryClosed(io: SessionIo, stream: number, code: number, reason: string): Promise<void> {
    const state = this.#streams.get(stream);
    if (state?.kind !== "proxy" || state.reading !== "socket") return;
    state.socket = null;
    const sent = isSendableCloseCode(code)
      ? { code, reason }
      : code === 1005
        ? { code: null, reason: "" }
        : { code: CloseCode.InternalError, reason: "the library connection failed" };
    await this.#endSocket(io, stream, state, sent, null);
    await this.#flush(io);
  }

  /**
   * Bytes held for all streams: unparsed input, bodies being read, partial
   * WebSocket messages and queued response bytes.
   *
   * @returns The sum.
   */
  #held(): number {
    let total = this.#reader.buffered;
    for (const state of this.#streams.values()) {
      for (const part of state.out.queue) total += part.length;
      if (state.kind !== "proxy") continue;
      if (state.reading === "body") total += state.bodyBytes;
      total += state.frames?.buffered ?? 0;
      for (const part of state.early) total += part.length;
    }
    return total;
  }

  /**
   * Emits a response's header block.
   *
   * @param id - The stream.
   * @param status - HTTP status.
   * @param fields - Response fields.
   * @param length - The `content-length` to announce, or `null`.
   * @param source - Who produced the answer.
   * @param endStream - Whether no body follows.
   */
  #head(
    id: number,
    status: number,
    fields: Array<[string, string]>,
    length: number | null,
    source: Source,
    endStream: boolean,
  ): void {
    const user: Array<[string, string]> =
      length === null ? fields : [...fields, ["content-length", String(length)]];
    const out: Field[] = [[":status", String(status)]];
    if (length !== null) out.push(["content-length", String(length)]);
    out.push([RESPONSE_FIELDS, serializeFields(user)]);
    out.push([RESPONSE_META, JSON.stringify({ src: source })]);
    this.#emit(encodeHeaderFrames(id, encodeHeaders(out), endStream, this.#peerMaxFrame));
  }

  /**
   * Sends an answer and writes what the windows allow.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param answer - The answer.
   * @returns Resolves once written as far as the windows allow.
   */
  async #answer(io: SessionIo, id: number, state: Proxy, answer: Answer): Promise<void> {
    state.reading = "done";
    state.body = [];
    const empty = answer.body.length === 0 && answer.more === null;
    this.#head(id, answer.status, answer.fields, answer.length, answer.source, empty);
    state.out.complete = true;
    if (empty) {
      this.#finish(id, state);
      return;
    }
    state.out.queue.push(answer.body);
    state.out.more = answer.more;
    await this.#drain(io, id, state);
  }

  /**
   * Writes queued bytes up to the windows, as DATA frames.
   *
   * @param id - The stream.
   * @param state - Its state.
   */
  #send(id: number, state: StreamState): void {
    const out = state.out;
    while (out.window > 0 && this.#sendWindow > 0 && out.queue.length > 0) {
      const head = out.queue[0] as Uint8Array;
      const take = Math.min(head.length, out.window, this.#sendWindow, this.#peerMaxFrame);
      this.#emit(encodeData(id, head.subarray(0, take)));
      out.window -= take;
      this.#sendWindow -= take;
      if (take === head.length) out.queue.shift();
      else out.queue[0] = head.subarray(take);
    }
  }

  /**
   * Writes a response as far as the windows allow, fetching the next
   * piece of a large body while it fits, and END_STREAM once all of it
   * went out.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @returns Resolves once the windows are used or the response is out.
   */
  async #drain(io: SessionIo, id: number, state: StreamState): Promise<void> {
    const out = state.out;
    for (;;) {
      this.#send(id, state);
      if (state.kind === "control") return;
      if (out.queue.length > 0 || !out.complete || state.ended) return;
      if (out.more !== null && state.kind === "proxy") {
        if (out.window <= 0 || this.#sendWindow <= 0) return;
        // Write what is queued before waiting on the library.
        await this.#flush(io);
        const piece = await continueBody(io.library, this.options.hostname, out.more);
        if (piece === null) {
          this.#reset(id, ErrorCode.InternalError);
          return;
        }
        out.queue.push(piece.bytes);
        out.more = piece.more;
        continue;
      }
      this.#emit(encodeData(id, new Uint8Array(), true));
      this.#finish(id, state);
      return;
    }
  }

  /**
   * Marks this side's END_STREAM sent and forgets the stream, telling the
   * edge to stop sending when it had not finished.
   *
   * @param id - The stream.
   * @param state - Its state.
   */
  #finish(id: number, state: Config | Proxy): void {
    state.ended = true;
    this.#streams.delete(id);
    if (state.kind === "proxy" && !state.remoteEnded) {
      // RFC 9113 section 8.1: a complete answer before the request ended.
      this.#emit(encodeRst(id, ErrorCode.NoError));
    }
  }

  /**
   * Resets a stream and forgets it.
   *
   * @param id - The stream.
   * @param code - The error code.
   */
  #reset(id: number, code: number): void {
    this.#streams.delete(id);
    this.#emit(encodeRst(id, code));
  }
}

/**
 * The request a header block describes.
 *
 * @param fields - The decoded fields.
 * @returns The request, or `null` when it is malformed (RFC 9113 section
 *   8.3: missing or repeated pseudo-fields, uppercase names, a pseudo-field
 *   after a regular one).
 */
export function requestHead(fields: Field[]): RequestHead | null {
  let method: string | null = null;
  let path: string | null = null;
  const headers: Array<[string, string]> = [];
  let regular = false;
  for (const [name, value] of fields) {
    if (name.startsWith(":")) {
      if (regular) return null;
      if (name === ":method") {
        if (method !== null) return null;
        method = value;
      } else if (name === ":path") {
        if (path !== null) return null;
        path = value;
      } else if (name !== ":scheme" && name !== ":authority") {
        return null;
      }
      continue;
    }
    regular = true;
    if (name !== name.toLowerCase() || name.length === 0) return null;
    if (/[\r\n\0]/.test(value)) return null;
    headers.push([name, value]);
  }
  if (method === null || !/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(method)) return null;
  if (path === null || !/^\/[\x21-\x7e]*$/.test(path)) return null;
  return { method, target: path, headers };
}

/**
 * The client's IP, from the edge's field.
 *
 * @param headers - The request fields.
 * @returns The IP, or `undefined` when absent or malformed.
 */
function clientAddressOf(headers: Array<[string, string]>): string | undefined {
  const value = headers.find(([name]) => name === CLIENT_ADDRESS_FIELD)?.[1]?.trim();
  return value !== undefined && IP_ADDRESS.test(value) ? value : undefined;
}
