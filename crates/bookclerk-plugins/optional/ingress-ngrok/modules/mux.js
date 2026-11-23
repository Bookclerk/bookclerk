/**
 * muxado framing, sans-IO: the stream multiplexing ngrok's agent protocol
 * runs over its one TLS connection.
 *
 * Ported from `golang.ngrok.com/muxado/v2` v2.0.2 (`frame/`), MIT,
 * Copyright 2023 ngrok, Inc. Every frame is an 8-byte header, big-endian:
 * a 24-bit body length, a byte of type (high nibble) and flags (low
 * nibble), and a 31-bit stream id; then the body.
 */
/** Frame types. */
export const FrameType = {
    /** Stream reset; body is a 32-bit error code. */
    Rst: 0x0,
    /** Stream data; the body is the bytes. */
    Data: 0x1,
    /** Send-window increment; body is a 31-bit increment. */
    WndInc: 0x2,
    /** Session end; body is the last stream id, an error code and debug text. */
    GoAway: 0x3,
};
/** DATA flag: the sender is done writing this stream. */
export const FLAG_FIN = 0x1;
/** DATA flag: the first frame of a new stream. */
export const FLAG_SYN = 0x2;
/** Bytes in a frame header. */
export const HEADER_BYTES = 8;
/** Largest stream id (31 bits). */
export const MAX_STREAM_ID = 0x7fffffff;
/** Per-stream receive window both sides start with (256 KiB). */
export const INITIAL_WINDOW = 0x40000;
/**
 * Largest frame body this reader accepts. A DATA frame never needs more
 * than a full window, and the fixed frames are 8 bytes plus GOAWAY's
 * debug text.
 */
export const MAX_FRAME_BODY = INITIAL_WINDOW;
/** muxado error codes. */
export const ErrorCode = {
    NoError: 0,
    ProtocolError: 1,
    InternalError: 2,
    FlowControlError: 3,
    StreamClosed: 4,
    StreamRefused: 5,
    StreamCancelled: 6,
    StreamReset: 7,
    FrameSizeError: 8,
    AcceptQueueFull: 9,
    EnhanceYourCalm: 10,
    RemoteGoneAway: 11,
    StreamsExhausted: 12,
    WriteTimeout: 13,
    SessionClosed: 14,
    PeerEof: 15,
};
/** A protocol error that ends the session. */
export class MuxError extends Error {
    /** The muxado code to send in GOAWAY. */
    code;
    /**
     * @param code - The muxado error code.
     * @param message - What went wrong.
     */
    constructor(code, message) {
        super(message);
        this.name = "MuxError";
        this.code = code;
    }
}
/**
 * Encodes one frame header.
 *
 * @param type - Frame type.
 * @param flags - Flags (low nibble).
 * @param stream - Stream id.
 * @param length - Body length.
 * @returns The 8 header bytes.
 */
function header(type, flags, stream, length) {
    if (!Number.isInteger(stream) || stream < 0 || stream > MAX_STREAM_ID) {
        throw new RangeError(`stream id ${stream} out of range`);
    }
    if (!Number.isInteger(length) || length < 0 || length > 0xffffff) {
        throw new RangeError(`frame length ${length} out of range`);
    }
    const out = new Uint8Array(HEADER_BYTES);
    out[0] = (length >>> 16) & 0xff;
    out[1] = (length >>> 8) & 0xff;
    out[2] = length & 0xff;
    out[3] = ((type & 0x0f) << 4) | (flags & 0x0f);
    new DataView(out.buffer).setUint32(4, stream, false);
    return out;
}
/**
 * Joins byte arrays.
 *
 * @param parts - The arrays.
 * @returns One array holding them in order.
 */
export function concat(...parts) {
    let total = 0;
    for (const part of parts)
        total += part.length;
    const out = new Uint8Array(total);
    let at = 0;
    for (const part of parts) {
        out.set(part, at);
        at += part.length;
    }
    return out;
}
/**
 * Encodes a DATA frame.
 *
 * @param stream - Stream id (not 0).
 * @param data - The bytes.
 * @param flags - {@link FLAG_SYN} and/or {@link FLAG_FIN}.
 * @returns The frame.
 */
export function encodeData(stream, data, flags = 0) {
    if (stream === 0)
        throw new RangeError("DATA on stream 0");
    return concat(header(FrameType.Data, flags, stream, data.length), data);
}
/**
 * Encodes a frame with one 32-bit body word.
 *
 * @param type - RST or WNDINC.
 * @param stream - Stream id.
 * @param word - The body.
 * @returns The frame.
 */
function wordFrame(type, stream, word) {
    const out = concat(header(type, 0, stream, 4), new Uint8Array(4));
    new DataView(out.buffer).setUint32(HEADER_BYTES, word >>> 0, false);
    return out;
}
/**
 * Encodes an RST frame.
 *
 * @param stream - Stream id.
 * @param code - Error code.
 * @returns The frame.
 */
export function encodeRst(stream, code) {
    return wordFrame(FrameType.Rst, stream, code);
}
/**
 * Encodes a WNDINC frame.
 *
 * @param stream - Stream id.
 * @param increment - Bytes, 1 to 2^31 - 1.
 * @returns The frame.
 */
export function encodeWndInc(stream, increment) {
    if (!Number.isInteger(increment) || increment < 1 || increment > MAX_STREAM_ID) {
        throw new RangeError(`window increment ${increment} out of range`);
    }
    return wordFrame(FrameType.WndInc, stream, increment);
}
/**
 * Encodes a GOAWAY frame.
 *
 * @param lastStream - The last peer stream handled.
 * @param code - Error code.
 * @param debug - Debug text.
 * @returns The frame.
 */
export function encodeGoAway(lastStream, code, debug = "") {
    const text = new TextEncoder().encode(debug);
    const body = new Uint8Array(8 + text.length);
    const view = new DataView(body.buffer);
    view.setUint32(0, lastStream & MAX_STREAM_ID, false);
    view.setUint32(4, code >>> 0, false);
    body.set(text, 8);
    return concat(header(FrameType.GoAway, 0, 0, body.length), body);
}
/**
 * Reassembles frames from the chunks a byte stream arrives in.
 *
 * Feed each chunk to {@link FrameReader.push}; it returns every frame the
 * bytes so far complete and keeps a partial one for the next chunk.
 */
export class FrameReader {
    /** Bytes not yet part of a whole frame. */
    #pending = new Uint8Array(0);
    /** Bytes held for a partial frame. */
    get buffered() {
        return this.#pending.length;
    }
    /**
     * Adds bytes and decodes the frames they complete.
     *
     * @param chunk - The next bytes of the stream.
     * @returns The complete frames, in order.
     * @throws {MuxError} for a frame no peer may send; the session must end.
     */
    push(chunk) {
        const bytes = this.#pending.length === 0 ? chunk : concat(this.#pending, chunk);
        const frames = [];
        let at = 0;
        while (bytes.length - at >= HEADER_BYTES) {
            const length = (bytes[at] << 16) | (bytes[at + 1] << 8) | bytes[at + 2];
            if (length > MAX_FRAME_BODY) {
                throw new MuxError(ErrorCode.FrameSizeError, `frame of ${length} bytes`);
            }
            if (bytes.length - at < HEADER_BYTES + length)
                break;
            const typeFlags = bytes[at + 3];
            const view = new DataView(bytes.buffer, bytes.byteOffset + at + 4, 4);
            const stream = view.getUint32(0, false) & MAX_STREAM_ID;
            const body = bytes.slice(at + HEADER_BYTES, at + HEADER_BYTES + length);
            at += HEADER_BYTES + length;
            const frame = decode(typeFlags >> 4, typeFlags & 0x0f, stream, body);
            if (frame !== null)
                frames.push(frame);
        }
        this.#pending = bytes.slice(at);
        return frames;
    }
}
/**
 * Decodes one frame body.
 *
 * @param type - Frame type.
 * @param flags - Flags.
 * @param stream - Stream id.
 * @param body - The body.
 * @returns The frame, or `null` for a type this reader skips.
 * @throws {MuxError} for a malformed fixed-size body or DATA on stream 0.
 */
function decode(type, flags, stream, body) {
    const word = (offset) => new DataView(body.buffer, body.byteOffset + offset, 4).getUint32(0, false);
    switch (type) {
        case FrameType.Data:
            if (stream === 0)
                throw new MuxError(ErrorCode.ProtocolError, "DATA on stream 0");
            return {
                kind: "data",
                stream,
                data: body,
                fin: (flags & FLAG_FIN) !== 0,
                syn: (flags & FLAG_SYN) !== 0,
            };
        case FrameType.Rst:
            if (body.length !== 4)
                throw new MuxError(ErrorCode.FrameSizeError, "RST body");
            return { kind: "rst", stream, code: word(0) };
        case FrameType.WndInc: {
            if (body.length !== 4)
                throw new MuxError(ErrorCode.FrameSizeError, "WNDINC body");
            return { kind: "wndinc", stream, increment: word(0) & MAX_STREAM_ID };
        }
        case FrameType.GoAway:
            if (body.length < 8)
                throw new MuxError(ErrorCode.FrameSizeError, "GOAWAY body");
            return {
                kind: "goaway",
                lastStream: word(0) & MAX_STREAM_ID,
                code: word(4),
                debug: new TextDecoder("utf-8", { fatal: false }).decode(body.subarray(8)),
            };
        default:
            // muxado skips unknown frame types.
            return null;
    }
}
