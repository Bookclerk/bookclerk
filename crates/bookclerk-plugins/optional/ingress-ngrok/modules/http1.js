/**
 * HTTP/1.1 on a proxied connection, sans-IO: the request head and body
 * framing the agent reads (RFC 9112) and the response head it writes.
 *
 * ngrok's edge terminates TLS and hands the agent the client's bytes as
 * they are. The agent serves one request per connection and answers with
 * `Connection: close`, so a request ends the stream's reading side.
 */
/** Largest request head (request line and fields) the agent reads, in bytes. */
export const MAX_HEAD_BYTES = 64 * 1024;
/** Most header fields a request may carry. */
export const MAX_HEADERS = 128;
/** Largest chunk-size line (with extensions) the chunked decoder reads, in bytes. */
const MAX_CHUNK_LINE_BYTES = 4 * 1024;
/** Largest trailer section the chunked decoder reads, in bytes. */
const MAX_TRAILER_BYTES = 16 * 1024;
/** `tchar` from RFC 9110: what a method or field name may hold. */
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
/** A field value: visible ASCII, obs-text, spaces and tabs (no CR, LF or NUL). */
const FIELD_VALUE = /^[\t\x20-\x7e\x80-\xff]*$/;
/** An origin-form target: a path and an optional query, no spaces or controls. */
const ORIGIN_FORM = /^\/[\x21-\x7e]*$/;
/**
 * Finds the end of a head: the first CRLF CRLF.
 *
 * @param bytes - Bytes read so far.
 * @returns The offset just past the blank line, or -1.
 */
function headEnd(bytes) {
    for (let i = 3; i < bytes.length; i++) {
        if (bytes[i] === 10 && bytes[i - 1] === 13 && bytes[i - 2] === 10 && bytes[i - 3] === 13) {
            return i + 1;
        }
    }
    return -1;
}
/**
 * Decodes bytes as Latin-1, so every byte maps to one code unit.
 *
 * @param bytes - The bytes.
 * @returns The text.
 */
function latin1(bytes) {
    let out = "";
    for (let i = 0; i < bytes.length; i += 8192) {
        out += String.fromCharCode(...bytes.subarray(i, i + 8192));
    }
    return out;
}
/**
 * Splits a comma-separated field value into lowercased, trimmed members.
 *
 * @param value - The value.
 * @returns Its non-empty members.
 */
function members(value) {
    return value
        .split(",")
        .map((part) => part.trim().toLowerCase())
        .filter((part) => part.length > 0);
}
/**
 * Parses a request head.
 *
 * @param bytes - The connection's bytes so far, starting at the request line.
 * @returns The head, `null` when more bytes are needed, or the error to
 *   answer with.
 */
export function parseRequestHead(bytes) {
    const end = headEnd(bytes.subarray(0, MAX_HEAD_BYTES));
    if (end < 0) {
        if (bytes.length >= MAX_HEAD_BYTES) {
            return { status: 431, message: "The request head is too large." };
        }
        return null;
    }
    const lines = latin1(bytes.subarray(0, end - 4)).split("\r\n");
    const requestLine = lines[0] ?? "";
    const parts = requestLine.split(" ");
    if (parts.length !== 3)
        return { status: 400, message: "Malformed request line." };
    const [method = "", target = "", version = ""] = parts;
    if (!TOKEN.test(method))
        return { status: 400, message: "Malformed method." };
    if (version !== "HTTP/1.1" && version !== "HTTP/1.0") {
        return /^HTTP\/\d\.\d$/.test(version)
            ? { status: 505, message: "Only HTTP/1.1 is supported." }
            : { status: 400, message: "Malformed HTTP version." };
    }
    if (method === "CONNECT" || !ORIGIN_FORM.test(target)) {
        return { status: 400, message: "Only origin-form request targets are supported." };
    }
    const headers = [];
    for (const line of lines.slice(1)) {
        if (line.startsWith(" ") || line.startsWith("\t")) {
            return { status: 400, message: "Folded header fields are not allowed." };
        }
        const colon = line.indexOf(":");
        if (colon <= 0)
            return { status: 400, message: "Malformed header field." };
        const name = line.slice(0, colon);
        const value = line.slice(colon + 1).replace(/^[ \t]+|[ \t]+$/g, "");
        if (!TOKEN.test(name) || !FIELD_VALUE.test(value)) {
            return { status: 400, message: "Malformed header field." };
        }
        headers.push([name.toLowerCase(), value]);
        if (headers.length > MAX_HEADERS) {
            return { status: 431, message: "The request has too many header fields." };
        }
    }
    const body = framingOf(headers);
    if ("status" in body)
        return body;
    const connection = headers.filter(([n]) => n === "connection").flatMap(([, v]) => members(v));
    const expect = headers.filter(([n]) => n === "expect").flatMap(([, v]) => members(v));
    return {
        method,
        target,
        headers,
        body,
        upgrade: connection.includes("upgrade") && headers.some(([n]) => n === "upgrade"),
        expectContinue: version === "HTTP/1.1" && expect.includes("100-continue"),
        headBytes: end,
    };
}
/**
 * Decides how a request's body is framed (RFC 9112 section 6.3).
 *
 * @param headers - The request's fields, names lowercased.
 * @returns The framing, or the error to answer with.
 */
function framingOf(headers) {
    const codings = headers.filter(([n]) => n === "transfer-encoding").flatMap(([, v]) => members(v));
    const lengths = headers.filter(([n]) => n === "content-length").flatMap(([, v]) => v.split(","));
    if (codings.length > 0) {
        // Both fields at once is how request smuggling starts: refuse it.
        if (lengths.length > 0) {
            return { status: 400, message: "Both Transfer-Encoding and Content-Length were sent." };
        }
        if (codings.length !== 1 || codings[0] !== "chunked") {
            return { status: 501, message: "Only the chunked transfer coding is supported." };
        }
        return { kind: "chunked" };
    }
    if (lengths.length === 0)
        return { kind: "none" };
    const trimmed = lengths.map((v) => v.trim());
    const first = trimmed[0] ?? "";
    if (!/^\d{1,15}$/.test(first) || trimmed.some((v) => v !== first)) {
        return { status: 400, message: "Malformed Content-Length." };
    }
    const length = Number(first);
    return length === 0 ? { kind: "none" } : { kind: "length", length };
}
/** A malformed chunked body. */
export class ChunkedError extends Error {
}
/** Decodes a `Transfer-Encoding: chunked` body as its bytes arrive. */
export class ChunkedDecoder {
    /** Where the decoder is. */
    state = { at: "size", line: [] };
    /**
     * Feeds more bytes.
     *
     * @param chunk - The next bytes of the body.
     * @returns The body bytes they completed, and whether the body ended.
     * @throws {ChunkedError} For a malformed body.
     */
    push(chunk) {
        const data = [];
        let i = 0;
        while (i < chunk.length) {
            const state = this.state;
            switch (state.at) {
                case "size": {
                    const byte = chunk[i++];
                    if (byte !== 10) {
                        state.line.push(byte);
                        if (state.line.length > MAX_CHUNK_LINE_BYTES) {
                            throw new ChunkedError("chunk size line too long");
                        }
                        break;
                    }
                    if (state.line[state.line.length - 1] !== 13)
                        throw new ChunkedError("bare LF");
                    const line = String.fromCharCode(...state.line.slice(0, -1));
                    const size = /^([0-9A-Fa-f]{1,8})(?:[ \t]*;.*)?$/.exec(line);
                    if (!size)
                        throw new ChunkedError("malformed chunk size");
                    const left = parseInt(size[1], 16);
                    this.state = left === 0 ? { at: "trailer", line: [], total: 0 } : { at: "data", left };
                    break;
                }
                case "data": {
                    const take = Math.min(state.left, chunk.length - i);
                    data.push(chunk.slice(i, i + take));
                    i += take;
                    state.left -= take;
                    if (state.left === 0)
                        this.state = { at: "data-end", seen: 0 };
                    break;
                }
                case "data-end": {
                    const byte = chunk[i++];
                    if (byte !== (state.seen === 0 ? 13 : 10))
                        throw new ChunkedError("missing CRLF after chunk");
                    state.seen += 1;
                    if (state.seen === 2)
                        this.state = { at: "size", line: [] };
                    break;
                }
                case "trailer": {
                    const byte = chunk[i++];
                    state.total += 1;
                    if (state.total > MAX_TRAILER_BYTES)
                        throw new ChunkedError("trailers too large");
                    if (byte !== 10) {
                        state.line.push(byte);
                        break;
                    }
                    if (state.line[state.line.length - 1] !== 13)
                        throw new ChunkedError("bare LF");
                    if (state.line.length === 1) {
                        this.state = { at: "done" };
                        return { data, done: true, rest: chunk.slice(i) };
                    }
                    // Trailer fields are read and dropped.
                    state.line = [];
                    break;
                }
                case "done":
                    return { data, done: true, rest: chunk.slice(i) };
            }
        }
        return { data, done: this.state.at === "done", rest: new Uint8Array() };
    }
}
/** Reason phrases for the statuses the agent writes most; others get none. */
const REASONS = {
    100: "Continue",
    101: "Switching Protocols",
    200: "OK",
    201: "Created",
    204: "No Content",
    206: "Partial Content",
    301: "Moved Permanently",
    302: "Found",
    303: "See Other",
    304: "Not Modified",
    307: "Temporary Redirect",
    308: "Permanent Redirect",
    400: "Bad Request",
    401: "Unauthorized",
    403: "Forbidden",
    404: "Not Found",
    405: "Method Not Allowed",
    409: "Conflict",
    413: "Content Too Large",
    414: "URI Too Long",
    416: "Range Not Satisfiable",
    426: "Upgrade Required",
    429: "Too Many Requests",
    431: "Request Header Fields Too Large",
    500: "Internal Server Error",
    501: "Not Implemented",
    502: "Bad Gateway",
    503: "Service Unavailable",
    504: "Gateway Timeout",
    505: "HTTP Version Not Supported",
};
/** Response fields that describe the connection; the agent writes its own. */
export const RESPONSE_HOP_BY_HOP = new Set([
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "trailer",
    "content-length",
]);
/**
 * Serializes a response head that closes the connection after the body.
 *
 * @param status - Status code, 100 to 599.
 * @param headers - Fields to send (connection fields and `content-length`
 *   are dropped; fields with a CR, LF or NUL are dropped).
 * @param contentLength - Body length to announce, or `null` for a status
 *   or method whose response has no body.
 * @returns The head's bytes.
 */
export function encodeResponseHead(status, headers, contentLength) {
    const reason = REASONS[status] ?? "";
    let head = `HTTP/1.1 ${status} ${reason}\r\n`;
    for (const [name, value] of headers) {
        const lower = name.toLowerCase();
        if (RESPONSE_HOP_BY_HOP.has(lower))
            continue;
        if (!TOKEN.test(name) || !FIELD_VALUE.test(value))
            continue;
        head += `${lower}: ${value}\r\n`;
    }
    if (contentLength !== null)
        head += `content-length: ${contentLength}\r\n`;
    if (status >= 200)
        head += "connection: close\r\n";
    head += "\r\n";
    const out = new Uint8Array(head.length);
    for (let i = 0; i < head.length; i++)
        out[i] = head.charCodeAt(i) & 0xff;
    return out;
}
/**
 * Serializes the head that accepts a WebSocket upgrade.
 *
 * @param accept - The `Sec-WebSocket-Accept` value.
 * @param protocol - The subprotocol the library chose, or `""`; one that
 *   is not a token is left out.
 * @returns The head's bytes.
 */
export function encodeSwitchingHead(accept, protocol) {
    let head = "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n" +
        `sec-websocket-accept: ${accept}\r\n`;
    if (protocol && TOKEN.test(protocol))
        head += `sec-websocket-protocol: ${protocol}\r\n`;
    head += "\r\n";
    return new TextEncoder().encode(head);
}
/**
 * A plain error response, whole.
 *
 * @param status - HTTP status.
 * @param message - What happened, shown to the client.
 * @param extra - More response fields.
 * @returns Head and body bytes.
 */
export function errorResponse(status, message, extra = []) {
    const body = new TextEncoder().encode(JSON.stringify({ error: "library_unavailable", message, status }));
    const head = encodeResponseHead(status, [["content-type", "application/json"], ...extra], body.length);
    const out = new Uint8Array(head.length + body.length);
    out.set(head);
    out.set(body, head.length);
    return out;
}
