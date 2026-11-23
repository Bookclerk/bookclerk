/**
 * RFC 6455 framing for the WebSocket connections clients open through
 * ngrok, sans-IO.
 *
 * ngrok ends TLS at its edge and passes the client's bytes through a
 * proxied stream unchanged. Once the agent answers an upgrade with
 * `101 Switching Protocols`, the stream carries WebSocket frames: masked
 * ones from the client, which {@link FrameDecoder} reads, and unmasked ones
 * from the agent, which {@link encodeFrame} writes. No extension is
 * negotiated (the host drops `sec-websocket-extensions`), so a frame with a
 * reserved bit set is a protocol error.
 */
/** Frame opcodes (RFC 6455 section 5.2). */
export const Opcode = {
    /** The next fragment of a message. */
    Continuation: 0,
    /** The first fragment of a UTF-8 text message. */
    Text: 1,
    /** The first fragment of a binary message. */
    Binary: 2,
    /** A close, with an optional code and reason. */
    Close: 8,
    /** A ping; the peer answers with a pong carrying the same bytes. */
    Ping: 9,
    /** A pong. */
    Pong: 10,
};
/** Close codes the agent sends (RFC 6455 section 7.4.1). */
export const CloseCode = {
    /** The connection did its job. */
    Normal: 1000,
    /** One side is going away. */
    GoingAway: 1001,
    /** A frame broke the protocol. */
    ProtocolError: 1002,
    /** A text message or close reason was not UTF-8. */
    InvalidData: 1007,
    /** The peer broke a policy (here: stopped reading). */
    PolicyViolation: 1008,
    /** A message was larger than the agent passes on. */
    TooBig: 1009,
    /** Something failed on the agent's side. */
    InternalError: 1011,
    /** The agent is out of room for now. */
    TryAgainLater: 1013,
};
/** Largest payload of a control frame, in bytes. */
export const MAX_CONTROL_PAYLOAD = 125;
/** Largest close reason, in UTF-8 bytes (125 less the 2-byte code). */
export const MAX_CLOSE_REASON_BYTES = 123;
/** The GUID the handshake appends to the client's key (RFC 6455 section 1.3). */
const HANDSHAKE_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/** Most header bytes a client frame takes: 2, an 8-byte length and the mask. */
const MAX_HEADER_BYTES = 14;
/** A client frame that breaks the protocol, and the code to close with. */
export class WsError extends Error {
    /** Close code to send: 1002, 1007 or 1009. */
    code;
    /**
     * Creates an error.
     *
     * @param code - Close code to send.
     * @param message - What was wrong, sent as the close reason.
     */
    constructor(code, message) {
        super(message);
        this.name = "WsError";
        this.code = code;
    }
}
/**
 * Whether a close code may travel in a close frame: the codes RFC 6455
 * defines for use on the wire, and the registered and private ranges.
 *
 * @param code - The code.
 * @returns `true` when a peer may send it.
 */
export function isSendableCloseCode(code) {
    return (Number.isInteger(code) &&
        ((code >= 1000 && code <= 1003) ||
            (code >= 1007 && code <= 1014) ||
            (code >= 3000 && code <= 4999)));
}
/**
 * Whether a `Sec-WebSocket-Key` is the base64 of 16 bytes, as RFC 6455
 * section 4.1 requires.
 *
 * @param key - The field value.
 * @returns `true` when it is.
 */
export function validKey(key) {
    // 16 bytes are 22 base64 digits and `==`; the last digit carries 2 bits.
    return /^[A-Za-z0-9+/]{21}[AQgw]==$/.test(key);
}
/**
 * The `Sec-WebSocket-Accept` value for a client's key.
 *
 * @param key - The client's `Sec-WebSocket-Key`.
 * @returns base64 of the SHA-1 of the key and the handshake GUID.
 */
export async function acceptKey(key) {
    const digest = await crypto.subtle.digest("SHA-1", new TextEncoder().encode(`${key}${HANDSHAKE_GUID}`));
    return btoa(String.fromCharCode(...new Uint8Array(digest)));
}
/**
 * Encodes one unmasked, final frame, as a server sends.
 *
 * @param opcode - The opcode.
 * @param payload - The payload.
 * @returns The frame's bytes.
 */
export function encodeFrame(opcode, payload) {
    const length = payload.length;
    const extra = length < 126 ? 0 : length < 65536 ? 2 : 8;
    const out = new Uint8Array(2 + extra + length);
    out[0] = 0x80 | opcode;
    const view = new DataView(out.buffer);
    if (extra === 0) {
        out[1] = length;
    }
    else if (extra === 2) {
        out[1] = 126;
        view.setUint16(2, length);
    }
    else {
        out[1] = 127;
        view.setUint32(2, Math.floor(length / 2 ** 32));
        view.setUint32(6, length >>> 0);
    }
    out.set(payload, 2 + extra);
    return out;
}
/**
 * Cuts a reason to {@link MAX_CLOSE_REASON_BYTES} UTF-8 bytes on a
 * character boundary.
 *
 * @param reason - The reason.
 * @returns It, shortened when needed.
 */
export function closeReason(reason) {
    const encoder = new TextEncoder();
    if (encoder.encode(reason).length <= MAX_CLOSE_REASON_BYTES)
        return reason;
    let out = "";
    let bytes = 0;
    for (const ch of reason) {
        const size = encoder.encode(ch).length;
        if (bytes + size > MAX_CLOSE_REASON_BYTES)
            break;
        out += ch;
        bytes += size;
    }
    return out;
}
/**
 * Encodes a close frame.
 *
 * @param code - The code, or `null` for a close without one.
 * @param reason - The reason (cut to {@link MAX_CLOSE_REASON_BYTES}); only
 *   sent with a code.
 * @returns The frame's bytes.
 */
export function encodeClose(code, reason = "") {
    if (code === null)
        return encodeFrame(Opcode.Close, new Uint8Array());
    const text = new TextEncoder().encode(closeReason(reason));
    const payload = new Uint8Array(2 + text.length);
    new DataView(payload.buffer).setUint16(0, code);
    payload.set(text, 2);
    return encodeFrame(Opcode.Close, payload);
}
/**
 * Joins byte arrays.
 *
 * @param parts - The arrays.
 * @param total - Their summed length.
 * @returns One array.
 */
function join(parts, total) {
    if (parts.length === 1)
        return parts[0];
    const out = new Uint8Array(total);
    let at = 0;
    for (const part of parts) {
        out.set(part, at);
        at += part.length;
    }
    return out;
}
/**
 * Decodes UTF-8 strictly.
 *
 * @param bytes - The bytes.
 * @param what - What they are, for the error.
 * @returns The text.
 * @throws {WsError} 1007 when they are not UTF-8.
 */
function utf8(bytes, what) {
    try {
        return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
    }
    catch {
        throw new WsError(CloseCode.InvalidData, `${what} is not UTF-8`);
    }
}
/**
 * Reads the frames a client sends, as they arrive in pieces, into whole
 * messages. Holds at most one header, one frame and one message of up to
 * `maxMessage` bytes.
 */
export class FrameDecoder {
    /** Largest data message passed on, in bytes. */
    maxMessage;
    /** Header bytes of the next frame, fewer than a whole header. */
    #header = new Uint8Array();
    /** The frame being read. */
    #current = null;
    /** The data message being assembled. */
    #partial = null;
    /** Whether a close arrived; later bytes are ignored. */
    #closed = false;
    /**
     * Creates a decoder.
     *
     * @param maxMessage - Largest data message passed on, in bytes.
     */
    constructor(maxMessage) {
        this.maxMessage = maxMessage;
    }
    /** Bytes held: a partial header, the frame and the message so far. */
    get buffered() {
        return this.#header.length + (this.#current?.payload.length ?? 0) + (this.#partial?.bytes ?? 0);
    }
    /** Whether a close arrived. */
    get closed() {
        return this.#closed;
    }
    /**
     * Reads more bytes.
     *
     * @param chunk - The bytes, as they arrived.
     * @returns The messages they complete, in order; a close ends the list
     *   and every later call returns none.
     * @throws {WsError} When a frame breaks the protocol, a text message or
     *   close reason is not UTF-8, or a message passes `maxMessage`.
     */
    push(chunk) {
        const out = [];
        let at = 0;
        while (!this.#closed && at < chunk.length) {
            if (this.#current === null) {
                const need = MAX_HEADER_BYTES - this.#header.length;
                const header = new Uint8Array(this.#header.length + Math.min(need, chunk.length - at));
                header.set(this.#header);
                header.set(chunk.subarray(at, at + header.length - this.#header.length), this.#header.length);
                const parsed = this.#parseHeader(header);
                if (parsed === 0) {
                    this.#header = header;
                    return out;
                }
                at += parsed - this.#header.length;
                this.#header = new Uint8Array();
            }
            const current = this.#current;
            if (current === null)
                continue;
            const take = Math.min(current.remaining, chunk.length - at);
            for (let i = 0; i < take; i++) {
                current.payload[current.read] =
                    chunk[at + i] ^ current.mask[current.read % 4];
                current.read++;
            }
            current.remaining -= take;
            at += take;
            if (current.remaining === 0) {
                this.#current = null;
                const message = this.#complete(current);
                if (message !== null)
                    out.push(message);
            }
        }
        return out;
    }
    /**
     * Parses a frame header and starts the frame.
     *
     * @param header - Bytes from the frame's start (up to 14).
     * @returns Header bytes taken, or 0 when more are needed.
     * @throws {WsError} When the header breaks the protocol or the message
     *   would pass `maxMessage`.
     */
    #parseHeader(header) {
        if (header.length < 2)
            return 0;
        const b0 = header[0];
        const b1 = header[1];
        if ((b0 & 0x70) !== 0) {
            throw new WsError(CloseCode.ProtocolError, "reserved bits set without an extension");
        }
        const fin = (b0 & 0x80) !== 0;
        const opcode = b0 & 0x0f;
        const control = opcode >= 8;
        if (opcode !== Opcode.Continuation &&
            opcode !== Opcode.Text &&
            opcode !== Opcode.Binary &&
            opcode !== Opcode.Close &&
            opcode !== Opcode.Ping &&
            opcode !== Opcode.Pong) {
            throw new WsError(CloseCode.ProtocolError, `unknown opcode ${opcode}`);
        }
        if ((b1 & 0x80) === 0) {
            throw new WsError(CloseCode.ProtocolError, "client frames must be masked");
        }
        const short = b1 & 0x7f;
        const lengthBytes = short === 126 ? 2 : short === 127 ? 8 : 0;
        const size = 2 + lengthBytes + 4;
        if (header.length < size)
            return 0;
        const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
        let length = short;
        if (short === 126) {
            length = view.getUint16(2);
            if (length < 126)
                throw new WsError(CloseCode.ProtocolError, "length not minimally encoded");
        }
        else if (short === 127) {
            const high = view.getUint32(2);
            const low = view.getUint32(6);
            if (high >= 0x8000_0000) {
                throw new WsError(CloseCode.ProtocolError, "length has its top bit set");
            }
            if (high === 0 && low < 65536) {
                throw new WsError(CloseCode.ProtocolError, "length not minimally encoded");
            }
            length = high * 2 ** 32 + low;
        }
        if (control) {
            if (!fin)
                throw new WsError(CloseCode.ProtocolError, "fragmented control frame");
            if (length > MAX_CONTROL_PAYLOAD) {
                throw new WsError(CloseCode.ProtocolError, "control frame payload over 125 bytes");
            }
        }
        else if (opcode === Opcode.Continuation) {
            if (this.#partial === null) {
                throw new WsError(CloseCode.ProtocolError, "continuation without a message");
            }
        }
        else if (this.#partial !== null) {
            throw new WsError(CloseCode.ProtocolError, "new message before the last one ended");
        }
        if (!control && (this.#partial?.bytes ?? 0) + length > this.maxMessage) {
            throw new WsError(CloseCode.TooBig, "message too large");
        }
        this.#current = {
            fin,
            opcode,
            mask: header.slice(2 + lengthBytes, size),
            remaining: length,
            read: 0,
            payload: new Uint8Array(length),
        };
        return size;
    }
    /**
     * Finishes a frame.
     *
     * @param frame - The frame, its payload whole.
     * @returns The message it completes, or `null` for a fragment.
     * @throws {WsError} When a close payload is malformed or a text message
     *   is not UTF-8.
     */
    #complete(frame) {
        switch (frame.opcode) {
            case Opcode.Ping:
                return { kind: "ping", data: frame.payload };
            case Opcode.Pong:
                return { kind: "pong", data: frame.payload };
            case Opcode.Close: {
                this.#closed = true;
                this.#partial = null;
                if (frame.payload.length === 0)
                    return { kind: "close", code: null, reason: "" };
                if (frame.payload.length === 1) {
                    throw new WsError(CloseCode.ProtocolError, "close payload of one byte");
                }
                const code = (frame.payload[0] << 8) | frame.payload[1];
                if (!isSendableCloseCode(code)) {
                    throw new WsError(CloseCode.ProtocolError, `close code ${code} may not be sent`);
                }
                return { kind: "close", code, reason: utf8(frame.payload.subarray(2), "close reason") };
            }
            default: {
                const partial = frame.opcode === Opcode.Continuation
                    ? this.#partial
                    : { opcode: frame.opcode, parts: [], bytes: 0 };
                partial.parts.push(frame.payload);
                partial.bytes += frame.payload.length;
                if (!frame.fin) {
                    this.#partial = partial;
                    return null;
                }
                this.#partial = null;
                const data = join(partial.parts, partial.bytes);
                if (partial.opcode === Opcode.Text)
                    return { kind: "text", text: utf8(data, "text message") };
                return { kind: "binary", data };
            }
        }
    }
}
