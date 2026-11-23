/**
 * The relay protocol between the Bookclerk relay Worker and this plugin.
 *
 * Both ends load this module: the relay Worker runs it in the operator's
 * Cloudflare account, and the plugin runs it in its workerd isolate. It
 * imports nothing, so the plugin can upload the same file as part of the
 * relay Worker.
 *
 * The relay holds each public request until the plugin asks for it:
 *
 * 1. The plugin keeps up to {@link RELAY_SOCKETS} WebSockets open to
 *    {@link RELAY_CONNECT_PATH}. When a public request arrives, the relay
 *    picks an idle socket and sends one text notice on it,
 *    `{"t":"request","id":"<id>"}` ({@link encodeNotice}).
 * 2. The plugin fetches {@link RELAY_PULL_PATH}`<id>`. The answer is a frame:
 *    a 4-byte big-endian length, the request head as JSON
 *    ({@link RelayRequestHead}), then the request body.
 * 3. The plugin serves the request through the library and posts the answer
 *    to {@link RELAY_PUSH_PATH}`<id>` in the same framing: the response head
 *    ({@link RelayResponseHead}), then the response body.
 *
 * A WebSocket upgrade takes the same steps with `upgrade: true` on the
 * request head. The plugin opens a socket to the library and, when the
 * library accepts, pushes a `101` head whose {@link RELAY_SOCKET_HEADER}
 * names the plugin's socket. From then on the connection's messages travel
 * as binary live frames ({@link encodeLiveFrame}) on the plugin socket that
 * served the upgrade: the relay addresses the plugin's socket id, the
 * plugin addresses the request id.
 *
 * Every relay path needs `Authorization: Bearer <token>`, the token the
 * plugin derived and deployed with the Worker. Each end parses what the
 * other sends with the functions here, sans IO, so the fuzz tests drive the
 * same code both ends run.
 */
/** Path prefix the relay keeps for itself; public requests never reach it. */
export const RELAY_PREFIX = "/__bookclerk/relay/";
/** WebSocket path the plugin connects its sockets to. */
export const RELAY_CONNECT_PATH = `${RELAY_PREFIX}connect`;
/** Path that reports how many plugin sockets the relay holds. */
export const RELAY_HEALTH_PATH = `${RELAY_PREFIX}health`;
/** Path prefix the plugin pulls a request from, followed by its id. */
export const RELAY_PULL_PATH = `${RELAY_PREFIX}pull/`;
/** Path prefix the plugin pushes a response to, followed by its id. */
export const RELAY_PUSH_PATH = `${RELAY_PREFIX}push/`;
/**
 * Sockets the plugin keeps open, and so the requests it serves at once. The
 * host lets an instance hold 8 outbound sockets; two stay free for
 * reconnecting. Live connections ride on these sockets rather than taking
 * their own.
 */
export const RELAY_SOCKETS = 6;
/** Bytes of the big-endian length that opens a frame. */
export const FRAME_PREFIX_BYTES = 4;
/** Most header fields a head may carry (the host's `maxOriginHeaders`). */
export const MAX_HEADERS = 128;
/** Most bytes of header names and values together (`maxOriginHeaderBytes`). */
export const MAX_HEADER_BYTES = 65536;
/** Longest request target (`maxOriginTargetBytes`). */
export const MAX_TARGET_BYTES = 8192;
/** Longest method token the host accepts. */
export const MAX_METHOD_BYTES = 32;
/**
 * Largest head JSON a frame may announce: the header and target limits with
 * room for JSON escaping.
 */
export const MAX_HEAD_BYTES = 2 * (MAX_HEADER_BYTES + MAX_TARGET_BYTES) + 16 * 1024;
/** Longest request id the relay mints (a UUID is 36 characters). */
export const MAX_ID_LENGTH = 64;
/**
 * Reads a frame's head off a byte stream fed in chunks of any size.
 *
 * Feed each chunk to {@link FrameReader.push} until it returns `head` or
 * `error`; the reader keeps nothing after that.
 */
export class FrameReader {
    /** Bytes read so far, before the head is complete. */
    #buffered = new Uint8Array(0);
    /** Set once a step returned `head` or `error`. */
    #finished = false;
    /**
     * Adds bytes and reports whether the head is complete.
     *
     * @param chunk - The next bytes of the stream.
     * @returns `need`, `head` with the JSON and the bytes after it, or `error`.
     */
    push(chunk) {
        if (this.#finished)
            return { kind: "error", reason: "frame reader already finished" };
        const joined = new Uint8Array(this.#buffered.length + chunk.length);
        joined.set(this.#buffered);
        joined.set(chunk, this.#buffered.length);
        this.#buffered = joined;
        if (joined.length < FRAME_PREFIX_BYTES)
            return { kind: "need" };
        const length = new DataView(joined.buffer, joined.byteOffset, joined.length).getUint32(0, false);
        if (length > MAX_HEAD_BYTES)
            return this.#fail("frame head is too large");
        if (joined.length < FRAME_PREFIX_BYTES + length)
            return { kind: "need" };
        const end = FRAME_PREFIX_BYTES + length;
        let json;
        try {
            // `ignoreBOM` keeps a leading byte-order mark, which JSON then refuses.
            json = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(joined.subarray(FRAME_PREFIX_BYTES, end));
        }
        catch {
            return this.#fail("frame head is not UTF-8");
        }
        const rest = joined.slice(end);
        this.#finished = true;
        this.#buffered = new Uint8Array(0);
        return { kind: "head", json, rest };
    }
    /**
     * Reports the end of the stream.
     *
     * @returns `error` when the stream ended inside the head; `need` never.
     */
    end() {
        if (this.#finished)
            return { kind: "error", reason: "frame reader already finished" };
        return this.#fail("stream ended inside the frame head");
    }
    /**
     * Fails the reader.
     *
     * @param reason - Why.
     * @returns The error step.
     */
    #fail(reason) {
        this.#finished = true;
        this.#buffered = new Uint8Array(0);
        return { kind: "error", reason };
    }
}
/**
 * Frames a head: its JSON's length as 4 big-endian bytes, then the JSON.
 *
 * @param head - The head to send.
 * @returns The frame's opening bytes.
 * @throws {RangeError} When the JSON is longer than {@link MAX_HEAD_BYTES}.
 */
export function encodeFrame(head) {
    const json = new TextEncoder().encode(JSON.stringify(head));
    if (json.length > MAX_HEAD_BYTES)
        throw new RangeError("relay frame head is too large");
    const out = new Uint8Array(FRAME_PREFIX_BYTES + json.length);
    new DataView(out.buffer).setUint32(0, json.length, false);
    out.set(json, FRAME_PREFIX_BYTES);
    return out;
}
/** Characters RFC 9110 allows in a token (`tchar`). */
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
/** A field value: visible ASCII, spaces and tabs (no obs-text). */
const FIELD_VALUE = /^[\t\x20-\x7e]*$/;
/** A request target: one `/`, then visible ASCII without `#`. */
const TARGET = /^\/[\x21-\x22\x24-\x7e]*$/;
/**
 * Header fields that describe one connection, never passed on.
 */
export const HOP_BY_HOP = new Set([
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
]);
/**
 * Whether a header name is one the relay drops from a public request: the
 * provider's own fields (`cf-*`, `cdn-loop`), forwarding fields, and the
 * authority, which the library sets itself.
 *
 * @param name - Lowercase field name.
 * @returns `true` when the relay must not pass it to the library.
 */
export function providerHeader(name) {
    return (name.startsWith("cf-") ||
        name.startsWith("x-forwarded-") ||
        name === "cdn-loop" ||
        name === "forwarded" ||
        name === "host" ||
        name === "x-real-ip");
}
/**
 * Checks header fields against the shared limits.
 *
 * @param value - The parsed `headers` member.
 * @returns The fields, or why they are refused.
 */
function parseHeaders(value) {
    if (!Array.isArray(value))
        return "headers is not a list";
    if (value.length > MAX_HEADERS)
        return "too many header fields";
    const out = [];
    let bytes = 0;
    for (const field of value) {
        if (typeof field !== "object" || field === null)
            return "malformed header field";
        const { name, value: text } = field;
        if (typeof name !== "string" || typeof text !== "string")
            return "malformed header field";
        if (!TOKEN.test(name) || name !== name.toLowerCase())
            return "header name is not a lowercase token";
        if (!FIELD_VALUE.test(text))
            return "header value holds a control or non-ASCII character";
        if (HOP_BY_HOP.has(name))
            return "header field describes the connection";
        bytes += name.length + text.length;
        if (bytes > MAX_HEADER_BYTES)
            return "header fields are too large";
        out.push({ name, value: text });
    }
    return out;
}
/**
 * Parses the JSON object of a head.
 *
 * @param json - The head's JSON text.
 * @returns The object, or why it is refused.
 */
function parseObject(json) {
    let value;
    try {
        value = JSON.parse(json);
    }
    catch {
        return "head is not JSON";
    }
    if (typeof value !== "object" || value === null || Array.isArray(value))
        return "head is not an object";
    return value;
}
/**
 * Whether `text` is an IPv4 address in dotted-decimal form.
 *
 * @param text - Candidate address.
 * @returns `true` for four decimal octets, each 0 to 255 without leading zeros.
 */
function isIpv4(text) {
    const parts = text.split(".");
    return (parts.length === 4 &&
        parts.every((part) => /^(0|[1-9][0-9]{0,2})$/.test(part) && Number(part) <= 255));
}
/**
 * Whether `text` is an IPv6 address (RFC 4291 text forms, no zone).
 *
 * @param text - Candidate address.
 * @returns `true` for a valid IPv6 address.
 */
function isIpv6(text) {
    if (text.length > 45 || !/^[0-9A-Fa-f:.]+$/.test(text))
        return false;
    let body = text;
    const lastColon = body.lastIndexOf(":");
    if (body.includes(".")) {
        if (!isIpv4(body.slice(lastColon + 1)))
            return false;
        body = `${body.slice(0, lastColon + 1)}0:0`;
    }
    const halves = body.split("::");
    if (halves.length > 2)
        return false;
    const groups = (half) => {
        if (half === "")
            return [];
        const list = half.split(":");
        return list.every((g) => /^[0-9A-Fa-f]{1,4}$/.test(g)) ? list : null;
    };
    const head = groups(halves[0] ?? "");
    if (head === null)
        return false;
    if (halves.length === 1)
        return head.length === 8;
    const tail = groups(halves[1] ?? "");
    if (tail === null)
        return false;
    return head.length + tail.length < 8;
}
/**
 * Whether `text` is an IP address the relay may report as the client.
 *
 * @param text - Candidate address.
 * @returns `true` for an IPv4 or IPv6 address.
 */
export function isIpAddress(text) {
    return isIpv4(text) || isIpv6(text);
}
/**
 * Parses a request head the relay sent.
 *
 * @param json - The head's JSON text, as {@link FrameReader} returned it.
 * @returns The head, or why it is refused.
 */
export function parseRequestHead(json) {
    const record = parseObject(json);
    if (typeof record === "string")
        return record;
    const { method, target, headers, client } = record;
    if (typeof method !== "string" || method.length > MAX_METHOD_BYTES || !TOKEN.test(method)) {
        return "method is not a token";
    }
    if (typeof target !== "string" ||
        target.length > MAX_TARGET_BYTES ||
        !TARGET.test(target) ||
        target.startsWith("//")) {
        return "target is not a path and query";
    }
    const fields = parseHeaders(headers);
    if (typeof fields === "string")
        return fields;
    for (const field of fields) {
        if (providerHeader(field.name))
            return "head carries a provider or forwarding header";
    }
    const head = { method, target, headers: fields };
    if (client !== undefined) {
        if (typeof client !== "string" || !isIpAddress(client))
            return "client is not an IP address";
        head.client = client;
    }
    const { upgrade } = record;
    if (upgrade !== undefined) {
        if (upgrade !== true || method !== "GET")
            return "upgrade is not true on a GET";
        head.upgrade = true;
    }
    return head;
}
/**
 * Parses a response head the plugin sent.
 *
 * @param json - The head's JSON text, as {@link FrameReader} returned it.
 * @returns The head, or why it is refused.
 */
export function parseResponseHead(json) {
    const record = parseObject(json);
    if (typeof record === "string")
        return record;
    const { status, headers } = record;
    if (typeof status !== "number" ||
        !Number.isInteger(status) ||
        (status !== 101 && (status < 200 || status > 599))) {
        return "status is not 101 or 200 to 599";
    }
    const fields = parseHeaders(headers);
    if (typeof fields === "string")
        return fields;
    return { status, headers: fields };
}
/** An id: letters, digits and dashes. */
const ID = /^[0-9A-Za-z-]+$/;
/**
 * Whether `id` is a request id the paths can carry.
 *
 * @param id - Candidate id.
 * @returns `true` for 1 to {@link MAX_ID_LENGTH} letters, digits and dashes.
 */
export function isRequestId(id) {
    return id.length > 0 && id.length <= MAX_ID_LENGTH && ID.test(id);
}
/**
 * Encodes a notice.
 *
 * @param notice - The notice.
 * @returns Its text frame.
 */
export function encodeNotice(notice) {
    return JSON.stringify(notice);
}
/**
 * Parses a notice the relay sent.
 *
 * @param text - The socket message's text.
 * @returns The notice, or why it is refused.
 */
export function parseNotice(text) {
    if (text.length > 256)
        return "notice is too long";
    const record = parseObject(text);
    if (typeof record === "string")
        return record;
    if (record["t"] !== "request")
        return "notice is not a request notice";
    const id = record["id"];
    if (typeof id !== "string" || !isRequestId(id))
        return "notice has no valid id";
    return { t: "request", id };
}
/**
 * Compares two strings in time that depends only on their lengths.
 *
 * @param a - First string.
 * @param b - Second string.
 * @returns `true` when they are equal.
 */
export function constantTimeEqual(a, b) {
    if (a.length !== b.length)
        return false;
    let diff = 0;
    for (let i = 0; i < a.length; i++)
        diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
    return diff === 0;
}
/**
 * Reads the bearer token of an `Authorization` value.
 *
 * @param header - The header value, or `null`.
 * @returns The token, or `null` when the value is not `Bearer <token>`.
 */
export function bearerToken(header) {
    if (header === null)
        return null;
    const match = /^Bearer ([A-Za-z0-9_-]{16,128})$/.exec(header);
    return match?.[1] ?? null;
}
/**
 * Response header on a `101` push naming the plugin's socket for the
 * connection; the relay keeps it and never passes it to the client.
 */
export const RELAY_SOCKET_HEADER = "x-bookclerk-relay-socket";
/**
 * Largest live frame either end sends: the host's `maxSocketFrameBytes`,
 * since each frame is one message on a host-brokered socket.
 */
export const MAX_LIVE_FRAME_BYTES = 262144;
/** Bytes before a live frame's payload at most: op, id length and id. */
export const LIVE_FRAME_OVERHEAD = 2 + MAX_ID_LENGTH;
/**
 * Largest message a live connection carries in either direction; a client
 * or library message past it closes the connection with 1009.
 */
export const MAX_LIVE_MESSAGE_BYTES = MAX_LIVE_FRAME_BYTES - LIVE_FRAME_OVERHEAD;
/** Longest close reason in bytes (a close frame's 125 less the code). */
export const MAX_CLOSE_REASON_BYTES = 123;
/** Op byte of a text message. */
const OP_TEXT = 1;
/** Op byte of a binary message. */
const OP_BINARY = 2;
/** Op byte of a close. */
const OP_CLOSE = 3;
/**
 * Whether a close code may be sent on to the other side. Both the Workers
 * runtime and the host accept these.
 *
 * @param code - Candidate code.
 * @returns `true` for 1000 to 1003, 1007 to 1014 and 3000 to 4999.
 */
export function isSendableCloseCode(code) {
    return (Number.isInteger(code) &&
        ((code >= 1000 && code <= 1003) ||
            (code >= 1007 && code <= 1014) ||
            (code >= 3000 && code <= 4999)));
}
/**
 * The code to pass on for a close the other side reported.
 *
 * @param code - The reported code.
 * @returns It when sendable; 1011 for an abnormal close (1006); else 1000.
 */
export function sendableCloseCode(code) {
    if (isSendableCloseCode(code))
        return code;
    return code === 1006 ? 1011 : 1000;
}
/**
 * A close reason cut to {@link MAX_CLOSE_REASON_BYTES} bytes on a character
 * boundary.
 *
 * @param reason - The reported reason.
 * @returns The reason, shortened when needed.
 */
export function sendableCloseReason(reason) {
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
 * Encodes a live frame: an op byte (1 text, 2 binary, 3 close), the id's
 * length in one byte, the id in ASCII, then the payload (the message, or a
 * big-endian close code and the reason in UTF-8).
 *
 * @param frame - The frame.
 * @returns Its bytes.
 * @throws {RangeError} When the id is not a request id, the close code or
 *   reason may not be sent, or the frame is over
 *   {@link MAX_LIVE_FRAME_BYTES}.
 */
export function encodeLiveFrame(frame) {
    if (!isRequestId(frame.id))
        throw new RangeError("live frame id is not a request id");
    const encoder = new TextEncoder();
    let op;
    let payload;
    switch (frame.op) {
        case "text":
            op = OP_TEXT;
            payload = encoder.encode(frame.text);
            break;
        case "binary":
            op = OP_BINARY;
            payload = frame.data;
            break;
        case "close": {
            op = OP_CLOSE;
            if (!isSendableCloseCode(frame.code))
                throw new RangeError("close code may not be sent");
            const reason = encoder.encode(frame.reason);
            if (reason.length > MAX_CLOSE_REASON_BYTES)
                throw new RangeError("close reason is too long");
            payload = new Uint8Array(2 + reason.length);
            new DataView(payload.buffer).setUint16(0, frame.code, false);
            payload.set(reason, 2);
            break;
        }
    }
    const size = 2 + frame.id.length + payload.length;
    if (size > MAX_LIVE_FRAME_BYTES)
        throw new RangeError("live frame is too large");
    const out = new Uint8Array(size);
    out[0] = op;
    out[1] = frame.id.length;
    for (let i = 0; i < frame.id.length; i++)
        out[2 + i] = frame.id.charCodeAt(i);
    out.set(payload, 2 + frame.id.length);
    return out;
}
/**
 * Parses a live frame the other end sent.
 *
 * @param bytes - The binary socket message.
 * @returns The frame, or why it is refused.
 */
export function parseLiveFrame(bytes) {
    if (bytes.length > MAX_LIVE_FRAME_BYTES)
        return "live frame is too large";
    if (bytes.length < 2)
        return "live frame is too short";
    const op = bytes[0];
    const idLength = bytes[1] ?? 0;
    if (bytes.length < 2 + idLength)
        return "live frame ends inside its id";
    let id = "";
    for (let i = 0; i < idLength; i++)
        id += String.fromCharCode(bytes[2 + i] ?? 0);
    if (!isRequestId(id))
        return "live frame id is not a request id";
    const payload = bytes.slice(2 + idLength);
    if (op === OP_TEXT) {
        try {
            const text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(payload);
            return { op: "text", id, text };
        }
        catch {
            return "live text is not UTF-8";
        }
    }
    if (op === OP_BINARY)
        return { op: "binary", id, data: payload };
    if (op === OP_CLOSE) {
        if (payload.length < 2)
            return "live close has no code";
        if (payload.length > 2 + MAX_CLOSE_REASON_BYTES)
            return "live close reason is too long";
        const code = ((payload[0] ?? 0) << 8) | (payload[1] ?? 0);
        if (!isSendableCloseCode(code))
            return "live close code may not be sent";
        try {
            const reason = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(payload.subarray(2));
            return { op: "close", id, code, reason };
        }
        catch {
            return "live close reason is not UTF-8";
        }
    }
    return "live frame op is unknown";
}
/**
 * The live frame carrying one message of a live connection.
 *
 * @param id - The connection's id on the receiving end.
 * @param message - A socket message: text, or binary bytes.
 * @returns The frame, or `null` when the message is over
 *   {@link MAX_LIVE_MESSAGE_BYTES} (the sender then closes with 1009).
 * @throws {RangeError} When `id` is not a request id.
 */
export function messageFrame(id, message) {
    if (typeof message === "string") {
        const bytes = new TextEncoder().encode(message);
        if (bytes.length > MAX_LIVE_MESSAGE_BYTES)
            return null;
        return encodeLiveFrame({ op: "text", id, text: message });
    }
    const data = message instanceof Uint8Array ? message : new Uint8Array(message);
    if (data.length > MAX_LIVE_MESSAGE_BYTES)
        return null;
    return encodeLiveFrame({ op: "binary", id, data });
}
