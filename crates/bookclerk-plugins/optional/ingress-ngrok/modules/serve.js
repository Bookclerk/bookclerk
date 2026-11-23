/**
 * Serves one proxied request through the library and shapes the answer for
 * a muxado stream.
 *
 * workerd delivers a socket's messages one at a time, and a stream from
 * one invocation can't be read in the next, so the agent can't hold a
 * response body open while it waits for ngrok to widen the stream's
 * window. A response is therefore read into memory up to
 * {@link MAX_BUFFERED_RESPONSE} bytes. A larger `GET` answer of known
 * length that allows byte ranges is sent in pieces instead: the head and
 * the first bytes now, then each later piece fetched from the library with
 * a `Range` request when the window opens ({@link Continuation}).
 *
 * An upgrade ({@link serveUpgrade}) opens a WebSocket to the library
 * through `ORIGIN.connect`; the agent then carries frames between it and
 * the client's stream.
 */
import { encodeResponseHead, encodeSwitchingHead, errorResponse, } from "./http1.js";
import { acceptKey, validKey } from "./ws.js";
/** Largest request body the agent buffers before handing it on, in bytes. */
export const MAX_REQUEST_BODY = 32 * 1024 * 1024;
/** Largest response the agent buffers whole, in bytes. */
export const MAX_BUFFERED_RESPONSE = 4 * 1024 * 1024;
/** Bytes each later piece of a large response asks the library for. */
export const CONTINUATION_BYTES = 1024 * 1024;
/** Statuses whose responses carry no body. */
const NULL_BODY = new Set([204, 205, 304]);
/** Request fields that describe the connection, never passed on. */
const REQUEST_HOP_BY_HOP = new Set([
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
    "content-length",
]);
/** WebSocket handshake fields the agent answers itself, never passed on. */
const HANDSHAKE_FIELDS = new Set([
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
]);
/**
 * Reads a body until it ends or passes `limit` bytes.
 *
 * @param body - The body.
 * @param limit - Bytes past which reading stops.
 * @returns The bytes read and whether the body ended; a body that did not
 *   end is cancelled.
 */
export async function readUpTo(body, limit) {
    const reader = body.getReader();
    const parts = [];
    let total = 0;
    for (;;) {
        const { value, done } = await reader.read();
        if (done)
            return { bytes: join(parts, total), done: true };
        parts.push(value);
        total += value.length;
        if (total > limit) {
            await reader.cancel().catch(() => { });
            return { bytes: join(parts, total), done: false };
        }
    }
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
 * The response fields to send, `set-cookie` kept apart.
 *
 * @param headers - The library's fields.
 * @returns Name and value pairs.
 */
export function responseFields(headers) {
    const out = [];
    headers.forEach((value, name) => {
        if (name.toLowerCase() !== "set-cookie")
            out.push([name, value]);
    });
    const cookies = typeof headers.getSetCookie === "function" ? headers.getSetCookie() : [];
    for (const cookie of cookies)
        out.push(["set-cookie", cookie]);
    return out;
}
/**
 * The request fields to pass to the library.
 *
 * @param head - The parsed request head.
 * @returns Fields without connection fields.
 */
function requestFields(head) {
    return head.headers.filter(([name]) => !REQUEST_HOP_BY_HOP.has(name));
}
/**
 * Builds the request the library sees.
 *
 * @param domain - The public domain.
 * @param method - Method.
 * @param target - Origin-form target.
 * @param fields - Header fields.
 * @param body - Body bytes, or `null`.
 * @returns The request.
 */
function libraryRequest(domain, method, target, fields, body) {
    const headers = new Headers();
    for (const [name, value] of fields)
        headers.append(name, value);
    return new Request(`https://${domain}${target}`, {
        method,
        headers,
        ...(body !== null && body.length > 0 && method !== "GET" && method !== "HEAD" ? { body } : {}),
    });
}
/**
 * Concatenates a head and body.
 *
 * @param head - Head bytes.
 * @param body - Body bytes.
 * @returns Both, in one array.
 */
function withBody(head, body) {
    const out = new Uint8Array(head.length + body.length);
    out.set(head);
    out.set(body, head.length);
    return out;
}
/**
 * Parses `Content-Range: bytes first-last/total`.
 *
 * @param value - The field value.
 * @returns The span, or `null` when it does not parse.
 */
export function parseContentRange(value) {
    const match = value === null ? null : /^bytes (\d{1,15})-(\d{1,15})\/(\d{1,15}|\*)$/.exec(value.trim());
    if (!match)
        return null;
    const first = Number(match[1]);
    const last = Number(match[2]);
    const total = match[3] === "*" ? null : Number(match[3]);
    if (last < first || (total !== null && last >= total))
        return null;
    return { first, last, total };
}
/**
 * The span a large response covers, when the rest may be fetched by range.
 *
 * @param method - The request method.
 * @param response - The library's response.
 * @returns First and last offsets, total and validator, or `null` when the
 *   response can't be continued.
 */
function continuable(method, response) {
    if (method !== "GET" || response.headers.has("content-encoding"))
        return null;
    const lengthText = response.headers.get("content-length");
    if (lengthText === null || !/^\d{1,15}$/.test(lengthText))
        return null;
    const length = Number(lengthText);
    const etag = response.headers.get("etag");
    const validator = etag !== null && !etag.startsWith("W/") ? etag : response.headers.get("last-modified");
    if (response.status === 200) {
        if (response.headers.get("accept-ranges")?.toLowerCase() !== "bytes" || length === 0) {
            return null;
        }
        return { first: 0, end: length - 1, total: length, validator };
    }
    if (response.status === 206) {
        const span = parseContentRange(response.headers.get("content-range"));
        if (span === null || span.last - span.first + 1 !== length)
            return null;
        return { first: span.first, end: span.last, total: span.total, validator };
    }
    return null;
}
/**
 * Serves one request through the library.
 *
 * @param library - The library.
 * @param domain - The public domain the request arrived on.
 * @param head - The parsed request head.
 * @param body - The request body, or `null`.
 * @param clientAddress - The client's IP.
 * @returns The answer to write.
 */
export async function serveRequest(library, domain, head, body, clientAddress) {
    const fields = requestFields(head);
    let response;
    try {
        response = await library.fetch(libraryRequest(domain, head.method, head.target, fields, body), clientAddress);
    }
    catch {
        return { bytes: errorResponse(502, "The library could not serve the request."), more: null };
    }
    return shapeResponse(head, fields, clientAddress, response);
}
/**
 * Shapes the library's response for the stream: buffered whole, or its
 * first piece and how to fetch the rest.
 *
 * @param head - The request head.
 * @param fields - The request fields the library saw.
 * @param clientAddress - The client's IP.
 * @param response - The library's response.
 * @returns The answer to write.
 */
async function shapeResponse(head, fields, clientAddress, response) {
    const status = response.status >= 200 && response.status <= 599 ? response.status : 502;
    const out = responseFields(response.headers);
    if (head.method === "HEAD" || NULL_BODY.has(status) || !response.body) {
        await response.body?.cancel().catch(() => { });
        const length = response.headers.get("content-length");
        const announced = head.method === "HEAD" && length !== null && /^\d{1,15}$/.test(length)
            ? Number(length)
            : null;
        const contentLength = NULL_BODY.has(status) ? null : (announced ?? 0);
        return { bytes: encodeResponseHead(status, out, contentLength), more: null };
    }
    const read = await readUpTo(response.body, MAX_BUFFERED_RESPONSE);
    if (read.done) {
        return {
            bytes: withBody(encodeResponseHead(status, out, read.bytes.length), read.bytes),
            more: null,
        };
    }
    const span = continuable(head.method, response);
    if (span === null) {
        return {
            bytes: errorResponse(502, "The response is too large to pass through the ngrok tunnel."),
            more: null,
        };
    }
    const length = span.end - span.first + 1;
    const first = read.bytes.subarray(0, Math.min(read.bytes.length, length));
    const next = span.first + first.length;
    return {
        bytes: withBody(encodeResponseHead(status, out, length), first),
        more: next > span.end
            ? null
            : {
                target: head.target,
                headers: fields.filter(([name]) => name !== "range" && name !== "if-range"),
                clientAddress,
                next,
                end: span.end,
                total: span.total,
                validator: span.validator,
            },
    };
}
/**
 * An answer that closes the stream, as an {@link UpgradeAnswer}.
 *
 * @param status - HTTP status.
 * @param message - What happened.
 * @param extra - More response fields.
 * @returns The answer.
 */
function refuse(status, message, extra = []) {
    return { kind: "answer", answer: { bytes: errorResponse(status, message, extra), more: null } };
}
/**
 * Serves one upgrade: checks the client's WebSocket handshake, opens a
 * socket to the library and builds the `101`, or passes on the library's
 * refusal.
 *
 * @param library - The library.
 * @param domain - The public domain the request arrived on.
 * @param head - The parsed request head (`upgrade` set).
 * @param clientAddress - The client's IP.
 * @param stream - The stream that carries the client's side.
 * @returns What to write.
 */
export async function serveUpgrade(library, domain, head, clientAddress, stream) {
    const field = (name) => head.headers.filter(([n]) => n === name).map(([, value]) => value);
    const protocols = field("upgrade").flatMap((value) => value.split(",").map((part) => part.trim().toLowerCase()));
    if (!protocols.includes("websocket")) {
        return refuse(501, "Only WebSocket upgrades are supported through ngrok.");
    }
    if (head.method !== "GET" || head.body.kind !== "none") {
        return refuse(400, "A WebSocket upgrade is a GET without a body.");
    }
    const versions = field("sec-websocket-version");
    if (versions.length !== 1 || versions[0] !== "13") {
        return refuse(426, "Only WebSocket version 13 is supported.", [
            ["sec-websocket-version", "13"],
        ]);
    }
    const keys = field("sec-websocket-key");
    if (keys.length !== 1 || !validKey(keys[0])) {
        return refuse(400, "Malformed Sec-WebSocket-Key.");
    }
    // Worked out first, so nothing waits between the library's accept and the 101.
    const accept = await acceptKey(keys[0]);
    const fields = requestFields(head).filter(([name]) => !HANDSHAKE_FIELDS.has(name));
    let result;
    try {
        result = await library.connect(libraryRequest(domain, "GET", head.target, fields, null), clientAddress, stream);
    }
    catch {
        return refuse(502, "The library could not open the connection.");
    }
    if ("response" in result) {
        return {
            kind: "answer",
            answer: await shapeResponse(head, fields, clientAddress, result.response),
        };
    }
    return {
        kind: "socket",
        id: result.socket.id,
        head: encodeSwitchingHead(accept, result.socket.protocol),
    };
}
/**
 * Fetches the next piece of a large response.
 *
 * @param library - The library.
 * @param domain - The public domain.
 * @param more - What is left.
 * @returns The piece and what is left after it, or `null` when the library
 *   no longer answers with the same resource (the stream must be reset).
 */
export async function continueBody(library, domain, more) {
    const last = Math.min(more.next + CONTINUATION_BYTES - 1, more.end);
    const fields = [
        ...more.headers,
        ["range", `bytes=${more.next}-${last}`],
    ];
    if (more.validator !== null)
        fields.push(["if-range", more.validator]);
    let response;
    try {
        response = await library.fetch(libraryRequest(domain, "GET", more.target, fields, null), more.clientAddress);
    }
    catch {
        return null;
    }
    const span = parseContentRange(response.headers.get("content-range"));
    if (response.status !== 206 ||
        !response.body ||
        span === null ||
        span.first !== more.next ||
        span.last > last ||
        (more.total !== null && span.total !== more.total)) {
        await response.body?.cancel().catch(() => { });
        return null;
    }
    const want = span.last - span.first + 1;
    const read = await readUpTo(response.body, want);
    if (read.bytes.length < want)
        return null;
    const bytes = read.bytes.subarray(0, want);
    const next = more.next + want;
    return { bytes, more: next > more.end ? null : { ...more, next } };
}
