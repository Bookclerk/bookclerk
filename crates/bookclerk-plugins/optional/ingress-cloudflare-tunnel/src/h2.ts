/**
 * HTTP/2 framing (RFC 9113) for the connector's side of a Cloudflare Tunnel
 * connection, sans-IO.
 *
 * The connector dials the edge, but on the connection it is the HTTP/2
 * server: the edge sends the client preface and opens a stream for the
 * control channel and for each public request. {@link FrameReader} checks
 * the preface, splits frames, enforces each frame type's shape and joins a
 * HEADERS frame with its CONTINUATION frames into one header block. The
 * encoders build the frames the connector sends.
 */

/** The client connection preface (RFC 9113 section 3.4). */
export const PREFACE = new TextEncoder().encode("PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");

/** Frame types. */
export const FrameType = {
  /** Body bytes. */
  Data: 0x0,
  /** A header block's first fragment. */
  Headers: 0x1,
  /** Stream priority (deprecated; ignored). */
  Priority: 0x2,
  /** Ends a stream. */
  RstStream: 0x3,
  /** Connection settings. */
  Settings: 0x4,
  /** A server push (never valid toward a server). */
  PushPromise: 0x5,
  /** A liveness check. */
  Ping: 0x6,
  /** Ends the connection. */
  GoAway: 0x7,
  /** Widens a flow-control window. */
  WindowUpdate: 0x8,
  /** A header block's later fragment. */
  Continuation: 0x9,
} as const;

/** Frame flags. */
export const Flag = {
  /** DATA / HEADERS: the sender's last frame on the stream. */
  EndStream: 0x1,
  /** SETTINGS / PING: an acknowledgement. */
  Ack: 0x1,
  /** HEADERS / CONTINUATION: the block ends here. */
  EndHeaders: 0x4,
  /** DATA / HEADERS: padding follows. */
  Padded: 0x8,
  /** HEADERS: priority fields follow. */
  Priority: 0x20,
} as const;

/** Error codes (RFC 9113 section 7). */
export const ErrorCode = {
  /** Graceful. */
  NoError: 0x0,
  /** The peer broke the protocol. */
  ProtocolError: 0x1,
  /** Something failed on this side. */
  InternalError: 0x2,
  /** A flow-control window was broken. */
  FlowControlError: 0x3,
  /** A frame came on a closed stream. */
  StreamClosed: 0x5,
  /** A frame had the wrong size. */
  FrameSizeError: 0x6,
  /** The stream was refused before any processing. */
  RefusedStream: 0x7,
  /** The stream is no longer needed. */
  Cancel: 0x8,
  /** The header block could not be decoded. */
  CompressionError: 0x9,
  /** The peer is generating too much load. */
  EnhanceYourCalm: 0xb,
} as const;

/** Setting identifiers. */
export const Setting = {
  /** HPACK dynamic table size. */
  HeaderTableSize: 0x1,
  /** Whether push is allowed. */
  EnablePush: 0x2,
  /** Most concurrent streams the sender accepts. */
  MaxConcurrentStreams: 0x3,
  /** Initial stream window. */
  InitialWindowSize: 0x4,
  /** Largest frame payload the sender accepts. */
  MaxFrameSize: 0x5,
  /** Largest field list the sender accepts. */
  MaxHeaderListSize: 0x6,
} as const;

/** Default and smallest `SETTINGS_MAX_FRAME_SIZE`. */
export const DEFAULT_MAX_FRAME = 16_384;

/** Largest `SETTINGS_MAX_FRAME_SIZE` a peer may set. */
export const LARGEST_MAX_FRAME = 16_777_215;

/** Default initial window, for streams and the connection. */
export const DEFAULT_WINDOW = 65_535;

/** Largest window. */
export const MAX_WINDOW = 0x7fffffff;

/** Largest header block the reader joins, in bytes. */
export const MAX_HEADER_BLOCK = 256 * 1024;

/** A connection error: the connection ends with GOAWAY carrying `code`. */
export class H2Error extends Error {
  /** The error code for GOAWAY. */
  readonly code: number;

  /**
   * Creates an error.
   *
   * @param code - The error code.
   * @param message - What was wrong.
   */
  constructor(code: number, message: string) {
    super(message);
    this.name = "H2Error";
    this.code = code;
  }
}

/** One frame, after shape checks; a header block is whole. */
export type Frame =
  /** Body bytes; `flow` is the payload length flow control counts. */
  | { kind: "data"; stream: number; data: Uint8Array; endStream: boolean; flow: number }
  /** A whole header block. */
  | { kind: "headers"; stream: number; block: Uint8Array; endStream: boolean }
  /** A stream reset. */
  | { kind: "rst"; stream: number; code: number }
  /** Settings, or their acknowledgement (`params` empty). */
  | { kind: "settings"; ack: boolean; params: Array<[id: number, value: number]> }
  /** A ping or its answer. */
  | { kind: "ping"; ack: boolean; data: Uint8Array }
  /** The peer ends the connection. */
  | { kind: "goaway"; lastStream: number; code: number; debug: string }
  /** A window update; stream 0 is the connection's. */
  | { kind: "window"; stream: number; increment: number };

/**
 * Reads a 31-bit stream identifier or window value.
 *
 * @param bytes - The bytes.
 * @param at - Offset.
 * @returns The value without its reserved top bit.
 */
function u31(bytes: Uint8Array, at: number): number {
  return (
    (((bytes[at] as number) & 0x7f) * 0x1000000 +
      ((bytes[at + 1] as number) << 16) +
      ((bytes[at + 2] as number) << 8) +
      (bytes[at + 3] as number)) >>>
    0
  );
}

/**
 * Reads a 32-bit unsigned integer.
 *
 * @param bytes - The bytes.
 * @param at - Offset.
 * @returns The value.
 */
function u32(bytes: Uint8Array, at: number): number {
  return (
    (bytes[at] as number) * 0x1000000 +
    ((bytes[at + 1] as number) << 16) +
    ((bytes[at + 2] as number) << 8) +
    (bytes[at + 3] as number)
  );
}

/**
 * Strips padding from a DATA or HEADERS payload.
 *
 * @param payload - The payload.
 * @param flags - The frame's flags.
 * @returns The bytes inside the padding.
 * @throws {H2Error} When the padding is longer than the payload.
 */
function unpad(payload: Uint8Array, flags: number): Uint8Array {
  if ((flags & Flag.Padded) === 0) return payload;
  if (payload.length < 1) throw new H2Error(ErrorCode.FrameSizeError, "padded frame too short");
  const pad = payload[0] as number;
  if (pad > payload.length - 1) throw new H2Error(ErrorCode.ProtocolError, "padding too long");
  return payload.subarray(1, payload.length - pad);
}

/** A header block whose CONTINUATION frames are still arriving. */
interface OpenBlock {
  /** The stream. */
  stream: number;
  /** Fragments so far. */
  parts: Uint8Array[];
  /** Their summed length. */
  bytes: number;
  /** Whether the HEADERS frame ended the stream. */
  endStream: boolean;
}

/**
 * Reads the edge's side of the connection: the preface, then frames.
 */
export class FrameReader {
  /** Bytes not yet parsed. */
  #buf: Uint8Array = new Uint8Array();
  /** Whether the preface was read. */
  #prefaced = false;
  /** Whether the first frame (which must be SETTINGS) was read. */
  #settled = false;
  /** Largest payload accepted: this side's `SETTINGS_MAX_FRAME_SIZE`. */
  readonly #maxFrame: number;
  /** The header block being joined, if any. */
  #open: OpenBlock | null = null;

  /**
   * Creates a reader.
   *
   * @param maxFrame - This side's `SETTINGS_MAX_FRAME_SIZE`.
   */
  constructor(maxFrame = DEFAULT_MAX_FRAME) {
    this.#maxFrame = maxFrame;
  }

  /** Bytes held unparsed, for the session's memory cap. */
  get buffered(): number {
    return this.#buf.length + (this.#open?.bytes ?? 0);
  }

  /**
   * Reads bytes.
   *
   * @param chunk - The next bytes from the connection.
   * @returns The frames they complete.
   * @throws {H2Error} For a bad preface or a frame that breaks RFC 9113.
   */
  push(chunk: Uint8Array): Frame[] {
    this.#buf = this.#buf.length === 0 ? chunk : join(this.#buf, chunk);
    const frames: Frame[] = [];
    if (!this.#prefaced) {
      const have = Math.min(this.#buf.length, PREFACE.length);
      for (let i = 0; i < have; i++) {
        if (this.#buf[i] !== PREFACE[i]) {
          throw new H2Error(ErrorCode.ProtocolError, "bad connection preface");
        }
      }
      if (this.#buf.length < PREFACE.length) return frames;
      this.#buf = this.#buf.subarray(PREFACE.length);
      this.#prefaced = true;
    }
    while (this.#buf.length >= 9) {
      const length =
        ((this.#buf[0] as number) << 16) |
        ((this.#buf[1] as number) << 8) |
        (this.#buf[2] as number);
      if (length > this.#maxFrame) {
        throw new H2Error(ErrorCode.FrameSizeError, `frame of ${length} bytes`);
      }
      if (this.#buf.length < 9 + length) break;
      const type = this.#buf[3] as number;
      const flags = this.#buf[4] as number;
      const stream = u31(this.#buf, 5);
      const payload = this.#buf.slice(9, 9 + length);
      this.#buf = this.#buf.subarray(9 + length);
      const frame = this.#frame(type, flags, stream, payload);
      if (frame !== null) frames.push(frame);
    }
    if (this.#buf.length === 0) this.#buf = new Uint8Array();
    return frames;
  }

  /**
   * Checks one frame's shape.
   *
   * @param type - Frame type.
   * @param flags - Flags.
   * @param stream - Stream id.
   * @param payload - Payload.
   * @returns The frame, or `null` for one with nothing to act on.
   * @throws {H2Error} When the frame breaks RFC 9113.
   */
  #frame(type: number, flags: number, stream: number, payload: Uint8Array): Frame | null {
    if (!this.#settled) {
      if (type !== FrameType.Settings || (flags & Flag.Ack) !== 0) {
        throw new H2Error(ErrorCode.ProtocolError, "the preface must end with SETTINGS");
      }
      this.#settled = true;
    }
    if (this.#open !== null && type !== FrameType.Continuation) {
      throw new H2Error(ErrorCode.ProtocolError, "a header block was interrupted");
    }
    switch (type) {
      case FrameType.Data: {
        if (stream === 0) throw new H2Error(ErrorCode.ProtocolError, "DATA on stream 0");
        return {
          kind: "data",
          stream,
          data: unpad(payload, flags),
          endStream: (flags & Flag.EndStream) !== 0,
          flow: payload.length,
        };
      }
      case FrameType.Headers: {
        if (stream === 0) throw new H2Error(ErrorCode.ProtocolError, "HEADERS on stream 0");
        let block = unpad(payload, flags);
        if (flags & Flag.Priority) {
          if (block.length < 5) {
            throw new H2Error(ErrorCode.FrameSizeError, "HEADERS priority too short");
          }
          if (u31(block, 0) === stream) {
            throw new H2Error(ErrorCode.ProtocolError, "a stream depends on itself");
          }
          block = block.subarray(5);
        }
        const endStream = (flags & Flag.EndStream) !== 0;
        if (flags & Flag.EndHeaders) return { kind: "headers", stream, block, endStream };
        this.#open = { stream, parts: [block], bytes: block.length, endStream };
        return null;
      }
      case FrameType.Continuation: {
        const open = this.#open;
        if (open === null || open.stream !== stream) {
          throw new H2Error(ErrorCode.ProtocolError, "CONTINUATION without its HEADERS");
        }
        open.parts.push(payload);
        open.bytes += payload.length;
        if (open.bytes > MAX_HEADER_BLOCK) {
          throw new H2Error(ErrorCode.EnhanceYourCalm, "header block too large");
        }
        if ((flags & Flag.EndHeaders) === 0) return null;
        this.#open = null;
        return { kind: "headers", stream, block: join(...open.parts), endStream: open.endStream };
      }
      case FrameType.Priority:
        if (stream === 0) throw new H2Error(ErrorCode.ProtocolError, "PRIORITY on stream 0");
        if (payload.length !== 5) throw new H2Error(ErrorCode.FrameSizeError, "PRIORITY size");
        return null;
      case FrameType.RstStream:
        if (stream === 0) throw new H2Error(ErrorCode.ProtocolError, "RST_STREAM on stream 0");
        if (payload.length !== 4) throw new H2Error(ErrorCode.FrameSizeError, "RST_STREAM size");
        return { kind: "rst", stream, code: u32(payload, 0) };
      case FrameType.Settings: {
        if (stream !== 0) throw new H2Error(ErrorCode.ProtocolError, "SETTINGS on a stream");
        const ack = (flags & Flag.Ack) !== 0;
        if (ack) {
          if (payload.length !== 0)
            throw new H2Error(ErrorCode.FrameSizeError, "SETTINGS ack size");
          return { kind: "settings", ack, params: [] };
        }
        if (payload.length % 6 !== 0) throw new H2Error(ErrorCode.FrameSizeError, "SETTINGS size");
        const params: Array<[number, number]> = [];
        for (let at = 0; at < payload.length; at += 6) {
          const id = ((payload[at] as number) << 8) | (payload[at + 1] as number);
          const value = u32(payload, at + 2);
          checkSetting(id, value);
          params.push([id, value]);
        }
        return { kind: "settings", ack, params };
      }
      case FrameType.PushPromise:
        throw new H2Error(ErrorCode.ProtocolError, "PUSH_PROMISE toward a server");
      case FrameType.Ping:
        if (stream !== 0) throw new H2Error(ErrorCode.ProtocolError, "PING on a stream");
        if (payload.length !== 8) throw new H2Error(ErrorCode.FrameSizeError, "PING size");
        return { kind: "ping", ack: (flags & Flag.Ack) !== 0, data: payload };
      case FrameType.GoAway:
        if (stream !== 0) throw new H2Error(ErrorCode.ProtocolError, "GOAWAY on a stream");
        if (payload.length < 8) throw new H2Error(ErrorCode.FrameSizeError, "GOAWAY size");
        return {
          kind: "goaway",
          lastStream: u31(payload, 0),
          code: u32(payload, 4),
          debug: new TextDecoder("utf-8", { fatal: false }).decode(payload.subarray(8, 8 + 256)),
        };
      case FrameType.WindowUpdate: {
        if (payload.length !== 4) throw new H2Error(ErrorCode.FrameSizeError, "WINDOW_UPDATE size");
        const increment = u31(payload, 0);
        if (increment === 0) throw new H2Error(ErrorCode.ProtocolError, "window increment of 0");
        return { kind: "window", stream, increment };
      }
      default:
        // Unknown frame types are ignored (RFC 9113 section 4.1).
        return null;
    }
  }
}

/**
 * Checks a setting's value.
 *
 * @param id - Setting id.
 * @param value - Its value.
 * @throws {H2Error} For a value RFC 9113 section 6.5.2 forbids.
 */
function checkSetting(id: number, value: number): void {
  switch (id) {
    case Setting.EnablePush:
      if (value > 1) throw new H2Error(ErrorCode.ProtocolError, "ENABLE_PUSH past 1");
      return;
    case Setting.InitialWindowSize:
      if (value > MAX_WINDOW) {
        throw new H2Error(ErrorCode.FlowControlError, "INITIAL_WINDOW_SIZE past 2^31-1");
      }
      return;
    case Setting.MaxFrameSize:
      if (value < DEFAULT_MAX_FRAME || value > LARGEST_MAX_FRAME) {
        throw new H2Error(ErrorCode.ProtocolError, "MAX_FRAME_SIZE out of range");
      }
      return;
    default:
      return;
  }
}

/**
 * Joins byte arrays.
 *
 * @param parts - The arrays.
 * @returns One array.
 */
export function join(...parts: Uint8Array[]): Uint8Array {
  if (parts.length === 1) return parts[0] as Uint8Array;
  let total = 0;
  for (const part of parts) total += part.length;
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/**
 * Encodes one frame.
 *
 * @param type - Frame type.
 * @param flags - Flags.
 * @param stream - Stream id.
 * @param payload - Payload (at most the peer's frame size).
 * @returns The frame.
 */
export function encodeFrame(
  type: number,
  flags: number,
  stream: number,
  payload: Uint8Array = new Uint8Array(),
): Uint8Array {
  const out = new Uint8Array(9 + payload.length);
  out[0] = (payload.length >>> 16) & 0xff;
  out[1] = (payload.length >>> 8) & 0xff;
  out[2] = payload.length & 0xff;
  out[3] = type;
  out[4] = flags;
  new DataView(out.buffer).setUint32(5, stream & MAX_WINDOW);
  out.set(payload, 9);
  return out;
}

/**
 * Encodes a SETTINGS frame.
 *
 * @param params - Setting ids and values.
 * @returns The frame.
 */
export function encodeSettings(params: ReadonlyArray<readonly [number, number]>): Uint8Array {
  const payload = new Uint8Array(params.length * 6);
  const view = new DataView(payload.buffer);
  params.forEach(([id, value], i) => {
    view.setUint16(i * 6, id);
    view.setUint32(i * 6 + 2, value);
  });
  return encodeFrame(FrameType.Settings, 0, 0, payload);
}

/**
 * Encodes a SETTINGS acknowledgement.
 *
 * @returns The frame.
 */
export function encodeSettingsAck(): Uint8Array {
  return encodeFrame(FrameType.Settings, Flag.Ack, 0);
}

/**
 * Encodes a PING or its answer.
 *
 * @param data - 8 bytes.
 * @param ack - Whether it answers the peer's ping.
 * @returns The frame.
 */
export function encodePing(data: Uint8Array, ack: boolean): Uint8Array {
  return encodeFrame(FrameType.Ping, ack ? Flag.Ack : 0, 0, data);
}

/**
 * Encodes a GOAWAY.
 *
 * @param lastStream - The last stream this side processed.
 * @param code - The error code.
 * @param debug - A short reason.
 * @returns The frame.
 */
export function encodeGoAway(lastStream: number, code: number, debug = ""): Uint8Array {
  const text = new TextEncoder().encode(debug.slice(0, 200));
  const payload = new Uint8Array(8 + text.length);
  const view = new DataView(payload.buffer);
  view.setUint32(0, lastStream & MAX_WINDOW);
  view.setUint32(4, code);
  payload.set(text, 8);
  return encodeFrame(FrameType.GoAway, 0, 0, payload);
}

/**
 * Encodes a RST_STREAM.
 *
 * @param stream - The stream.
 * @param code - The error code.
 * @returns The frame.
 */
export function encodeRst(stream: number, code: number): Uint8Array {
  const payload = new Uint8Array(4);
  new DataView(payload.buffer).setUint32(0, code);
  return encodeFrame(FrameType.RstStream, 0, stream, payload);
}

/**
 * Encodes a WINDOW_UPDATE.
 *
 * @param stream - The stream, or 0 for the connection.
 * @param increment - 1 to 2^31-1.
 * @returns The frame.
 */
export function encodeWindowUpdate(stream: number, increment: number): Uint8Array {
  const payload = new Uint8Array(4);
  new DataView(payload.buffer).setUint32(0, increment & MAX_WINDOW);
  return encodeFrame(FrameType.WindowUpdate, 0, stream, payload);
}

/**
 * Encodes a header block as HEADERS and as many CONTINUATION frames as
 * the peer's frame size needs.
 *
 * @param stream - The stream.
 * @param block - The HPACK block.
 * @param endStream - Whether no body follows.
 * @param maxFrame - The peer's `SETTINGS_MAX_FRAME_SIZE`.
 * @returns The frames, joined.
 */
export function encodeHeaders(
  stream: number,
  block: Uint8Array,
  endStream: boolean,
  maxFrame: number,
): Uint8Array {
  const frames: Uint8Array[] = [];
  let at = 0;
  do {
    const piece = block.subarray(at, at + maxFrame);
    at += piece.length;
    const last = at >= block.length;
    const first = frames.length === 0;
    const flags = (last ? Flag.EndHeaders : 0) | (first && endStream ? Flag.EndStream : 0);
    frames.push(
      encodeFrame(first ? FrameType.Headers : FrameType.Continuation, flags, stream, piece),
    );
  } while (at < block.length);
  return join(...frames);
}

/**
 * Encodes one DATA frame.
 *
 * @param stream - The stream.
 * @param data - At most the peer's frame size.
 * @param endStream - Whether it is the last on the stream.
 * @returns The frame.
 */
export function encodeData(stream: number, data: Uint8Array, endStream = false): Uint8Array {
  return encodeFrame(FrameType.Data, endStream ? Flag.EndStream : 0, stream, data);
}
