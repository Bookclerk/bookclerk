/**
 * One ngrok agent session over a muxado connection, sans-IO apart from the
 * {@link SessionIo} each call is given.
 *
 * The session authenticates on stream 3, binds one HTTPS endpoint on
 * stream 5 and then serves the streams ngrok opens: a proxied connection
 * carries one HTTP/1.1 request, which goes to the library; a heartbeat
 * stream is echoed; the server's restart, stop and update requests are
 * declined; a stop-tunnel notice ends the session's endpoint. A WebSocket
 * upgrade opens a socket to the library, and the stream then carries
 * frames both ways ({@link Session.fromLibrary},
 * {@link Session.libraryClosed}).
 *
 * Flow control follows muxado: each stream starts with a 256 KiB window
 * each way. The session widens ngrok's window as soon as it has taken the
 * bytes, and never writes past the window ngrok granted; a response that
 * doesn't fit waits in memory until a window update arrives.
 */

import {
  ChunkedDecoder,
  ChunkedError,
  errorResponse,
  parseRequestHead,
  type RequestHead,
} from "./http1.ts";
import {
  StreamType,
  authReply,
  authRequest,
  bindReply,
  bindRequest,
  clientIp,
  parseProxyHeader,
  parseRpcObject,
  readType,
  typePrefix,
  type ProxyHeader,
} from "./messages.ts";
import {
  ErrorCode,
  FLAG_FIN,
  FLAG_SYN,
  FrameReader,
  INITIAL_WINDOW,
  MuxError,
  concat,
  encodeData,
  encodeGoAway,
  encodeRst,
  encodeWndInc,
  type Frame,
} from "./mux.ts";
import {
  MAX_REQUEST_BODY,
  continueBody,
  serveRequest,
  serveUpgrade,
  type Continuation,
  type Library,
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

/** Largest DATA frame body the session writes, in bytes. */
export const MAX_WRITE_FRAME = 64 * 1024;

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
 * Most bytes a WebSocket stream may hold waiting for ngrok's window. Past
 * it the client is not reading, and the connection is closed.
 */
export const MAX_SOCKET_BACKLOG = 4 * 1024 * 1024;

/** The bind id this agent uses for its one endpoint. */
const BIND_ID = "bookclerk";

/** What a session call reaches outside itself. */
export interface SessionIo {
  /**
   * Writes bytes to the ngrok connection.
   *
   * @param bytes - Whole muxado frames.
   */
  write(bytes: Uint8Array): Promise<void>;
  /** The library. */
  library: Library;
}

/** Where a session is. */
export type Phase =
  /** The auth request is out; waiting for its reply. */
  | "authenticating"
  /** Authenticated; the bind request is out. */
  | "binding"
  /** The endpoint is online. */
  | "serving"
  /** Authentication or the bind failed, or ngrok stopped the endpoint. */
  | "failed"
  /** The connection ended. */
  | "closed";

/** What a response still has to write on its stream. */
interface Outbound {
  /** Bytes ngrok will still take on this stream. */
  window: number;
  /** Bytes ready to write, in order. */
  queue: Uint8Array[];
  /** The rest of a large body, fetched when the window opens. */
  more: Continuation | null;
  /** Whether the response is all queued (FIN goes after the queue). */
  complete: boolean;
}

/** A stream whose type has not arrived yet. */
interface Typing {
  /** Kind tag. */
  kind: "typing";
  /** Bytes so far (fewer than 4). */
  buf: Uint8Array;
}

/** A client RPC (auth or bind) waiting for its reply. */
interface Rpc {
  /** Kind tag. */
  kind: "rpc";
  /** Which request. */
  rpc: "auth" | "bind";
  /** Reply bytes so far. */
  buf: Uint8Array;
}

/** A server request this agent declines (restart, stop, update) or notes (stop tunnel). */
interface ServerRpc {
  /** Kind tag. */
  kind: "server-rpc";
  /** The stream type. */
  type: number;
  /** Request bytes so far. */
  buf: Uint8Array;
}

/** A heartbeat stream ngrok opened: every 4 bytes are echoed. */
interface Echo {
  /** Kind tag. */
  kind: "echo";
  /** Bytes of a partial beat. */
  buf: Uint8Array;
}

/** The heartbeat stream this agent opened. */
interface OwnBeat {
  /** Kind tag. */
  kind: "own-beat";
  /** The beat waiting for its echo, or `null`. */
  pending: Uint8Array | null;
  /** Echo bytes so far. */
  buf: Uint8Array;
}

/** A proxied connection. */
interface Proxy {
  /** Kind tag. */
  kind: "proxy";
  /**
   * What the stream is reading: the proxy header, the request head, its
   * body, WebSocket frames once an upgrade was accepted, or nothing more.
   */
  reading: "header" | "head" | "body" | "socket" | "done";
  /** Bytes not yet parsed. */
  buf: Uint8Array;
  /** The proxy header, once read. */
  header: ProxyHeader | null;
  /** The request head, once read. */
  head: RequestHead | null;
  /** Body bytes so far. */
  body: Uint8Array[];
  /** Body bytes so far, summed. */
  bodyBytes: number;
  /** The chunked decoder, for a chunked body. */
  chunked: ChunkedDecoder | null;
  /** The response being written. */
  out: Outbound;
  /** Whether this side sent FIN. */
  finSent: boolean;
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
type StreamState = Typing | Rpc | ServerRpc | Echo | OwnBeat | Proxy;

/** How a session was configured. */
export interface SessionOptions {
  /** The account's authtoken. */
  authtoken: string;
  /** The domain to bind. */
  domain: string;
  /** Agent version, sent to ngrok. */
  version: string;
}

/**
 * Concatenates two byte arrays, skipping the copy when one is empty.
 *
 * @param a - First.
 * @param b - Second.
 * @returns Both.
 */
function append(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (a.length === 0) return b;
  if (b.length === 0) return a;
  return concat(a, b);
}

/**
 * Bytes a proxied stream has queued for ngrok's window.
 *
 * @param state - The stream's state.
 * @returns The sum.
 */
function backlog(state: Proxy): number {
  let total = 0;
  for (const part of state.out.queue) total += part.length;
  return total;
}

/** One agent session. */
export class Session {
  /** Where the session is. */
  phase: Phase = "authenticating";
  /** Why it failed or closed, for `health`; never holds the authtoken. */
  error = "";
  /** The endpoint's public URL, once bound. */
  url = "";
  /** When the last echo of this agent's own heartbeat arrived (ms since epoch), or 0. */
  lastEcho = 0;
  /** The session's configuration. */
  readonly options: SessionOptions;
  /** Frames decoded from the connection. */
  readonly #reader = new FrameReader();
  /** Streams with state. */
  readonly #streams = new Map<number, StreamState>();
  /** Next stream id this side opens (odd). */
  #nextId = 3;
  /** This agent's heartbeat stream, once opened. */
  #beatStream = 0;
  /** Frames waiting for the next write. */
  #out: Uint8Array[] = [];
  /** Bytes in {@link Session.#out}. */
  #outBytes = 0;

  /**
   * Creates a session; nothing is sent until {@link Session.start}.
   *
   * @param options - Authtoken, domain and version.
   */
  constructor(options: SessionOptions) {
    this.options = options;
  }

  /** Streams with state, for tests. */
  get streamCount(): number {
    return this.#streams.size;
  }

  /**
   * Opens a stream this side starts.
   *
   * @param state - Its state.
   * @returns Its id.
   */
  #open(state: StreamState): number {
    const id = this.#nextId;
    this.#nextId += 2;
    this.#streams.set(id, state);
    return id;
  }

  /**
   * Queues frames for the next write.
   *
   * @param frame - Encoded frames.
   */
  #emit(frame: Uint8Array): void {
    this.#out.push(frame);
    this.#outBytes += frame.length;
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
      this.#outBytes -= size;
      await io.write(batch.length === 1 ? (batch[0] as Uint8Array) : concat(...batch));
    }
  }

  /**
   * Sends the auth request.
   *
   * @param io - The connection.
   * @returns Resolves once written.
   */
  async start(io: SessionIo): Promise<void> {
    const id = this.#open({ kind: "rpc", rpc: "auth", buf: new Uint8Array() });
    const body = new TextEncoder().encode(
      authRequest({ authtoken: this.options.authtoken, version: this.options.version }),
    );
    this.#emit(encodeData(id, concat(typePrefix(StreamType.Auth), body), FLAG_SYN));
    await this.#flush(io);
  }

  /**
   * Sends one heartbeat on this agent's own heartbeat stream; its echo sets
   * {@link Session.lastEcho}.
   *
   * @param io - The connection.
   * @param beat - 4 bytes to send (random outside tests).
   * @returns When the beat was sent (ms since epoch).
   */
  async beat(io: SessionIo, beat: Uint8Array): Promise<number> {
    const sentAt = Date.now();
    if (this.#beatStream === 0) {
      this.#beatStream = this.#open({ kind: "own-beat", pending: beat, buf: new Uint8Array() });
      await io.write(
        encodeData(this.#beatStream, concat(typePrefix(StreamType.Heartbeat), beat), FLAG_SYN),
      );
      return sentAt;
    }
    const state = this.#streams.get(this.#beatStream);
    if (state?.kind === "own-beat") state.pending = beat;
    await io.write(encodeData(this.#beatStream, beat));
    return sentAt;
  }

  /**
   * Handles bytes read from the connection.
   *
   * @param io - The connection and the library.
   * @param chunk - The bytes.
   * @returns Resolves once every frame they complete was handled and the
   *   answers written.
   * @throws {MuxError} When the peer broke the protocol; the session sent
   *   GOAWAY and the connection must close.
   */
  async receive(io: SessionIo, chunk: Uint8Array): Promise<void> {
    let frames: Frame[];
    try {
      frames = this.#reader.push(chunk);
    } catch (err) {
      if (err instanceof MuxError) await this.#fail(io, err);
      throw err;
    }
    try {
      for (const frame of frames) await this.#frame(io, frame);
    } catch (err) {
      if (err instanceof MuxError) await this.#fail(io, err);
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
  async #fail(io: SessionIo, err: MuxError): Promise<void> {
    this.phase = "closed";
    this.error = `ngrok broke the session protocol: ${err.message}`;
    this.#out = [];
    this.#outBytes = 0;
    await io.write(encodeGoAway(0, err.code, err.message)).catch(() => {});
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
      case "goaway":
        this.phase = "closed";
        this.error = frame.debug
          ? `ngrok ended the session: ${frame.debug}`
          : `ngrok ended the session (code ${frame.code})`;
        return;
      case "rst": {
        const state = this.#streams.get(frame.stream);
        this.#streams.delete(frame.stream);
        if (frame.stream === this.#beatStream) this.#beatStream = 0;
        if (state?.kind === "proxy" && state.socket !== null) {
          await this.#closeLibrary(io, state, CloseCode.GoingAway, "the client went away");
        }
        return;
      }
      case "wndinc": {
        const state = this.#streams.get(frame.stream);
        if (state?.kind !== "proxy") return;
        state.out.window += frame.increment;
        if (state.out.window > 0x7fffffff) {
          throw new MuxError(ErrorCode.FlowControlError, "window past 2^31-1");
        }
        await this.#drain(io, frame.stream, state);
        return;
      }
      case "data":
        await this.#data(io, frame.stream, frame.data, frame.fin, frame.syn);
        return;
    }
  }

  /**
   * Handles one DATA frame.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param data - The bytes.
   * @param fin - Whether the peer finished writing.
   * @param syn - Whether this frame opens the stream.
   * @returns Resolves once handled.
   */
  async #data(
    io: SessionIo,
    id: number,
    data: Uint8Array,
    fin: boolean,
    syn: boolean,
  ): Promise<void> {
    if (syn) {
      if (id % 2 === 1 || this.#streams.has(id)) {
        throw new MuxError(ErrorCode.ProtocolError, `SYN on stream ${id}`);
      }
      if (this.#streams.size >= MAX_STREAMS) {
        this.#emit(encodeRst(id, ErrorCode.StreamRefused));
        return;
      }
      this.#streams.set(id, { kind: "typing", buf: new Uint8Array() });
    }
    const state = this.#streams.get(id);
    if (state === undefined) return;
    if (data.length > 0) this.#emit(encodeWndInc(id, data.length));
    await this.#bytes(io, id, state, data, fin);
  }

  /**
   * Dispatches a stream's bytes by its kind.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param data - The bytes.
   * @param fin - Whether the peer finished writing.
   * @returns Resolves once handled.
   */
  async #bytes(
    io: SessionIo,
    id: number,
    state: StreamState,
    data: Uint8Array,
    fin: boolean,
  ): Promise<void> {
    switch (state.kind) {
      case "typing": {
        const buf = append(state.buf, data);
        if (buf.length < 4) {
          state.buf = buf;
          if (fin) this.#streams.delete(id);
          return;
        }
        const next = this.#typed(readType(buf));
        this.#streams.set(id, next);
        await this.#bytes(io, id, next, buf.slice(4), fin);
        return;
      }
      case "rpc":
        state.buf = append(state.buf, data);
        this.#rpcReply(id, state, fin);
        return;
      case "server-rpc":
        state.buf = append(state.buf, data);
        this.#serverRpc(id, state, fin);
        return;
      case "echo": {
        const buf = append(state.buf, data);
        const whole = buf.length - (buf.length % 4);
        if (whole > 0) this.#emit(encodeData(id, buf.slice(0, whole)));
        state.buf = buf.slice(whole);
        if (fin) {
          this.#emit(encodeData(id, new Uint8Array(), FLAG_FIN));
          this.#streams.delete(id);
        }
        return;
      }
      case "own-beat": {
        state.buf = append(state.buf, data);
        while (state.buf.length >= 4) {
          const echo = state.buf.subarray(0, 4);
          if (state.pending !== null && echo.every((b, i) => b === state.pending?.[i])) {
            this.lastEcho = Date.now();
            state.pending = null;
          }
          state.buf = state.buf.slice(4);
        }
        if (fin) {
          this.#streams.delete(id);
          this.#beatStream = 0;
        }
        return;
      }
      case "proxy":
        await this.#proxy(io, id, state, data, fin);
        return;
    }
  }

  /**
   * The state for a stream ngrok opened, by its type.
   *
   * @param type - The stream type.
   * @returns The state.
   */
  #typed(type: number): StreamState {
    switch (type) {
      case StreamType.Proxy:
        return {
          kind: "proxy",
          reading: "header",
          buf: new Uint8Array(),
          header: null,
          head: null,
          body: [],
          bodyBytes: 0,
          chunked: null,
          out: { window: INITIAL_WINDOW, queue: [], more: null, complete: false },
          finSent: false,
          connecting: false,
          early: [],
          socket: null,
          frames: null,
          closeSent: false,
        };
      case StreamType.Heartbeat:
        return { kind: "echo", buf: new Uint8Array() };
      default:
        return { kind: "server-rpc", type, buf: new Uint8Array() };
    }
  }

  /**
   * Reads an RPC reply and moves the session on.
   *
   * @param id - The stream.
   * @param state - Its state.
   * @param fin - Whether ngrok finished writing.
   */
  #rpcReply(id: number, state: Rpc, fin: boolean): void {
    const value = parseRpcObject(state.buf);
    if (value === null && !fin) return;
    this.#streams.delete(id);
    this.#emit(encodeData(id, new Uint8Array(), FLAG_FIN));
    if (value === null || typeof value === "string") {
      this.phase = "failed";
      this.error = `ngrok sent a malformed ${state.rpc} reply${value ? `: ${value}` : ""}`;
      return;
    }
    if (state.rpc === "auth") {
      const reply = authReply(value);
      if (reply.error) {
        this.phase = "failed";
        this.error = `ngrok refused the authtoken: ${reply.error}`;
        return;
      }
      this.phase = "binding";
      const bind = this.#open({ kind: "rpc", rpc: "bind", buf: new Uint8Array() });
      const body = new TextEncoder().encode(
        bindRequest({
          bindId: BIND_ID,
          domain: this.options.domain,
          forwardsTo: "Bookclerk library",
        }),
      );
      this.#emit(encodeData(bind, concat(typePrefix(StreamType.Bind), body), FLAG_SYN));
      return;
    }
    const reply = bindReply(value);
    if (reply.error) {
      this.phase = "failed";
      this.error = `ngrok did not bind ${this.options.domain}: ${reply.error}`;
      return;
    }
    this.phase = "serving";
    this.url = reply.url || `https://${this.options.domain}`;
  }

  /**
   * Answers a server request once it has arrived.
   *
   * @param id - The stream.
   * @param state - Its state.
   * @param fin - Whether ngrok finished writing.
   */
  #serverRpc(id: number, state: ServerRpc, fin: boolean): void {
    const value = parseRpcObject(state.buf);
    if (value === null && !fin) return;
    this.#streams.delete(id);
    if (state.type === StreamType.StopTunnel) {
      // ngrok stopped the endpoint (from its dashboard or API); no reply.
      const message = value !== null && typeof value === "object" ? value["Message"] : undefined;
      this.phase = "failed";
      this.error = `ngrok stopped the endpoint${typeof message === "string" && message ? `: ${message}` : ""}`;
      this.#emit(encodeData(id, new Uint8Array(), FLAG_FIN));
      return;
    }
    const known =
      state.type === StreamType.Restart ||
      state.type === StreamType.Stop ||
      state.type === StreamType.Update;
    if (!known) {
      this.#emit(encodeRst(id, ErrorCode.StreamRefused));
      return;
    }
    const reply = new TextEncoder().encode(
      JSON.stringify({ Error: "not supported by the Bookclerk agent" }),
    );
    this.#emit(encodeData(id, reply, FLAG_FIN));
  }

  /**
   * Reads a proxied connection's bytes and serves its request once whole.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param data - The bytes.
   * @param fin - Whether ngrok finished writing.
   * @returns Resolves once handled.
   */
  async #proxy(
    io: SessionIo,
    id: number,
    state: Proxy,
    data: Uint8Array,
    fin: boolean,
  ): Promise<void> {
    if (state.reading === "socket") {
      await this.#socketBytes(io, id, state, data, fin);
      return;
    }
    // Bytes after the request (pipelining) are dropped: the answer closes the connection.
    if (state.reading === "done") return;
    state.buf = append(state.buf, data);
    if (state.reading === "header") {
      const parsed = parseProxyHeader(state.buf);
      if (parsed === null) {
        if (fin) this.#reset(id);
        return;
      }
      if (typeof parsed === "string" || parsed.header.passthroughTls) {
        this.#reset(id);
        return;
      }
      state.header = parsed.header;
      state.buf = parsed.rest;
      state.reading = "head";
    }
    if (state.reading === "head") {
      const head = parseRequestHead(state.buf);
      if (head === null) {
        if (fin) this.#reset(id);
        return;
      }
      if ("status" in head) {
        await this.#answer(io, id, state, errorResponse(head.status, head.message), null);
        return;
      }
      state.head = head;
      state.buf = state.buf.slice(head.headBytes);
      state.reading = "body";
      if (head.body.kind === "length" && head.body.length > MAX_REQUEST_BODY) {
        await this.#answer(
          io,
          id,
          state,
          errorResponse(413, "The request body is too large for the ngrok tunnel."),
          null,
        );
        return;
      }
      if (head.body.kind === "chunked") state.chunked = new ChunkedDecoder();
      if (head.expectContinue && head.body.kind !== "none") {
        this.#queueRaw(id, state, new TextEncoder().encode("HTTP/1.1 100 Continue\r\n\r\n"));
      }
    }
    const head = state.head as RequestHead;
    const pending = state.buf;
    state.buf = new Uint8Array();
    if (head.upgrade) {
      await this.#upgrade(io, id, state, head, pending, fin);
      return;
    }
    let done = false;
    switch (head.body.kind) {
      case "none":
        done = true;
        break;
      case "length": {
        const take = Math.min(pending.length, head.body.length - state.bodyBytes);
        if (take > 0) state.body.push(pending.subarray(0, take));
        state.bodyBytes += take;
        done = state.bodyBytes === head.body.length;
        break;
      }
      case "chunked": {
        let progress;
        try {
          progress = (state.chunked as ChunkedDecoder).push(pending);
        } catch (err) {
          if (!(err instanceof ChunkedError)) throw err;
          await this.#answer(
            io,
            id,
            state,
            errorResponse(400, "Malformed chunked request body."),
            null,
          );
          return;
        }
        for (const part of progress.data) {
          state.body.push(part);
          state.bodyBytes += part.length;
        }
        if (state.bodyBytes > MAX_REQUEST_BODY) {
          await this.#answer(
            io,
            id,
            state,
            errorResponse(413, "The request body is too large for the ngrok tunnel."),
            null,
          );
          return;
        }
        done = progress.done;
        break;
      }
    }
    if (this.#held() > MAX_SESSION_BYTES) {
      await this.#answer(io, id, state, errorResponse(503, "The ngrok tunnel is busy."), null);
      return;
    }
    if (!done) {
      if (fin) this.#reset(id);
      return;
    }
    state.reading = "done";
    const body = state.body.length === 0 ? null : concat(...state.body);
    state.body = [];
    const header = state.header as ProxyHeader;
    const answer = await serveRequest(
      io.library,
      this.options.domain,
      head,
      body,
      clientIp(header.clientAddr),
    );
    await this.#answer(io, id, state, answer.bytes, answer.more);
  }

  /**
   * Serves an upgrade: on the library's accept, writes the `101` and
   * switches the stream to WebSocket frames.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param head - The upgrade's head.
   * @param pending - Bytes after the head: the client's first frames.
   * @param fin - Whether ngrok finished writing.
   * @returns Resolves once answered.
   */
  async #upgrade(
    io: SessionIo,
    id: number,
    state: Proxy,
    head: RequestHead,
    pending: Uint8Array,
    fin: boolean,
  ): Promise<void> {
    state.reading = "done";
    if (fin) {
      // The client finished sending before its socket opened.
      this.#reset(id);
      return;
    }
    state.connecting = true;
    const header = state.header as ProxyHeader;
    const answer = await serveUpgrade(
      io.library,
      this.options.domain,
      head,
      clientIp(header.clientAddr),
      id,
    );
    state.connecting = false;
    if (answer.kind === "answer") {
      state.early = [];
      await this.#answer(io, id, state, answer.answer.bytes, answer.answer.more);
      return;
    }
    if (this.#streams.get(id) !== state || this.phase === "closed") {
      // ngrok reset the stream, or the session ended, while the library answered.
      await io.library
        .socket(answer.id)
        ?.close(CloseCode.GoingAway, "the client went away")
        .catch(() => {});
      return;
    }
    state.reading = "socket";
    state.socket = answer.id;
    state.frames = new FrameDecoder(MAX_SOCKET_MESSAGE);
    state.out.queue.push(answer.head, ...state.early);
    state.early = [];
    this.#send(id, state);
    await this.#socketBytes(io, id, state, pending, false);
  }

  /**
   * Reads a client's WebSocket frames and passes its messages to the
   * library.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param data - The bytes.
   * @param fin - Whether ngrok finished writing (the client went away).
   * @returns Resolves once the messages were passed on.
   */
  async #socketBytes(
    io: SessionIo,
    id: number,
    state: Proxy,
    data: Uint8Array,
    fin: boolean,
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
    if (fin) {
      await this.#endSocket(io, id, state, null, {
        code: CloseCode.GoingAway,
        reason: "the client went away",
      });
      return;
    }
    if (backlog(state) > MAX_SOCKET_BACKLOG) {
      // Pongs pile up when the client sends pings but doesn't read.
      const close = { code: CloseCode.PolicyViolation, reason: "the client is not reading" };
      await this.#endSocket(io, id, state, close, close);
    } else if (this.#held() > MAX_SESSION_BYTES) {
      const close = { code: CloseCode.TryAgainLater, reason: "the ngrok tunnel is busy" };
      await this.#endSocket(io, id, state, close, close);
    }
  }

  /**
   * Ends a stream's WebSocket: a close frame to the client, then FIN once
   * the queue drains, and a close to the library.
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
    if (backlog(state) > MAX_SOCKET_BACKLOG || this.#held() > MAX_SESSION_BYTES) {
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
   * Bytes held for all proxied streams: bodies being read, partial
   * WebSocket messages and queued response bytes.
   *
   * @returns The sum.
   */
  #held(): number {
    let total = 0;
    for (const state of this.#streams.values()) {
      if (state.kind !== "proxy") continue;
      if (state.reading === "body") total += state.bodyBytes;
      total += state.frames?.buffered ?? 0;
      for (const part of state.out.queue) total += part.length;
      for (const part of state.early) total += part.length;
    }
    return total;
  }

  /**
   * Queues bytes ahead of the response without completing it.
   *
   * @param id - The stream.
   * @param state - Its state.
   * @param bytes - The bytes.
   */
  #queueRaw(id: number, state: Proxy, bytes: Uint8Array): void {
    state.out.queue.push(bytes);
    this.#send(id, state);
  }

  /**
   * Queues a response and writes what the window allows.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @param bytes - Head and body, or the first piece of a large body.
   * @param more - The rest of a large body.
   * @returns Resolves once written as far as the window allows.
   */
  async #answer(
    io: SessionIo,
    id: number,
    state: Proxy,
    bytes: Uint8Array,
    more: Continuation | null,
  ): Promise<void> {
    state.reading = "done";
    state.body = [];
    state.out.queue.push(bytes);
    state.out.more = more;
    state.out.complete = true;
    await this.#drain(io, id, state);
  }

  /**
   * Writes queued response bytes up to the window, as DATA frames.
   *
   * @param id - The stream.
   * @param state - Its state.
   */
  #send(id: number, state: Proxy): void {
    const out = state.out;
    while (out.window > 0 && out.queue.length > 0) {
      const head = out.queue[0] as Uint8Array;
      const take = Math.min(head.length, out.window, MAX_WRITE_FRAME);
      this.#emit(encodeData(id, head.subarray(0, take)));
      out.window -= take;
      if (take === head.length) out.queue.shift();
      else out.queue[0] = head.subarray(take);
    }
  }

  /**
   * Writes a response as far as the window allows, fetching the next
   * piece of a large body while it fits, and FIN once all of it went out.
   *
   * @param io - The connection and the library.
   * @param id - The stream.
   * @param state - Its state.
   * @returns Resolves once the window is used or the response is out.
   */
  async #drain(io: SessionIo, id: number, state: Proxy): Promise<void> {
    const out = state.out;
    for (;;) {
      this.#send(id, state);
      if (out.queue.length > 0 || !out.complete || state.finSent) return;
      if (out.more !== null) {
        if (out.window <= 0) return;
        // Write what is queued before waiting on the library.
        await this.#flush(io);
        const piece = await continueBody(io.library, this.options.domain, out.more);
        if (piece === null) {
          this.#reset(id);
          return;
        }
        out.queue.push(piece.bytes);
        out.more = piece.more;
        continue;
      }
      this.#emit(encodeData(id, new Uint8Array(), FLAG_FIN));
      state.finSent = true;
      // Later bytes from ngrok for this stream are dropped unread.
      this.#streams.delete(id);
      return;
    }
  }

  /**
   * Resets a stream and forgets it.
   *
   * @param id - The stream.
   */
  #reset(id: number): void {
    this.#streams.delete(id);
    this.#emit(encodeRst(id, ErrorCode.StreamCancelled));
  }
}
