/**
 * Author-side `SOCKETS` binding: host-brokered WebSockets shaped like
 * hibernatable Workers WebSockets.
 *
 * The adapter isolate holds the sockets-only grant and exposes one
 * {@link SocketsTransport} call per `POST /sockets/<op>`; this module turns
 * it into the {@link SocketsBinding} on the author's `env` and into the
 * {@link HostWebSocket} handles the `webSocketMessage` / `webSocketClose`
 * handlers receive. Transport replies carry failures as values, not throws,
 * so a `PluginError` keeps its code across the Workers RPC boundary between
 * the two isolates.
 */

import { MAX_SOCKET_ATTACHMENT_BYTES, MAX_SOCKET_FRAME_BYTES } from "./abi.js";
import type {
  HostWebSocket,
  SocketConnectOptions,
  SocketMessageData,
  SocketSendData,
  SocketsBinding,
} from "./env.js";
import { PluginError } from "./errors.js";
import type { SocketFrame } from "./generated.js";

/** Operations of the `/sockets/<op>` route. */
export type SocketOp = "connect" | "send" | "close" | "attachment" | "list";

/** One transport answer: the route's JSON body, or a typed failure. */
export type SocketTransportReply =
  { ok: Record<string, unknown> } | { err: { code: string; message: string } };

/**
 * The adapter's sockets grant, as the author isolate reaches it. Never
 * throws: every failure comes back as `err`.
 */
export interface SocketsTransport {
  /**
   * Make one `POST /sockets/<op>` call.
   *
   * @param op - Operation.
   * @param body - That operation's JSON fields (bytes already base64).
   * @returns The reply body or a typed failure.
   */
  call(op: SocketOp, body: Record<string, unknown>): Promise<SocketTransportReply>;
}

/**
 * Bytes to standard base64.
 *
 * @param bytes - Bytes to encode.
 * @returns Base64 text.
 */
function toBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/**
 * Standard base64 to bytes; anything that is not a string decodes as empty.
 *
 * @param text - Base64 text.
 * @returns Decoded bytes.
 */
function fromBase64(text: unknown): Uint8Array {
  if (typeof text !== "string" || text === "") return new Uint8Array();
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/**
 * Bytes of a binary message, without copying a view.
 *
 * @param data - Message the author passed.
 * @returns The bytes, or `null` for text.
 */
function sendBytes(data: SocketSendData): Uint8Array | null {
  if (typeof data === "string") return null;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data))
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  throw PluginError.fromWire("invalid_params", "a socket message must be a string or bytes");
}

/**
 * Route JSON form of one message, checked against `MAX_SOCKET_FRAME_BYTES`.
 *
 * @param data - Message the author passed.
 * @returns `{ text }` or `{ binary }` (base64).
 * @throws PluginError `payload_too_large` for an oversized message.
 */
export function routeFrame(data: SocketSendData): { text: string } | { binary: string } {
  const bytes = sendBytes(data);
  const size = bytes ? bytes.byteLength : new TextEncoder().encode(data as string).byteLength;
  if (size > MAX_SOCKET_FRAME_BYTES) {
    throw PluginError.fromWire(
      "payload_too_large",
      `socket message of ${size} bytes exceeds ${MAX_SOCKET_FRAME_BYTES}`,
    );
  }
  return bytes ? { binary: toBase64(bytes) } : { text: data as string };
}

/**
 * The message a handler receives for one wire frame: text as a string,
 * binary as an `ArrayBuffer`.
 *
 * @param frame - Decoded wire frame.
 * @returns The message.
 */
export function socketMessageData(frame: SocketFrame | null | undefined): SocketMessageData {
  if (frame && frame.kind === "binary") {
    const bytes = frame.value instanceof Uint8Array ? frame.value : new Uint8Array();
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
  }
  return frame && typeof frame.value === "string" ? frame.value : "";
}

/**
 * Encodes an attachment value as the host stores it: UTF-8 JSON, or no
 * bytes for `null` / `undefined`.
 *
 * @param value - JSON-serializable value.
 * @returns Stored bytes, at most `MAX_SOCKET_ATTACHMENT_BYTES`.
 * @throws PluginError `invalid_params` for a value JSON cannot encode,
 *   `payload_too_large` when it does not fit.
 */
export function encodeAttachment(value: unknown): Uint8Array {
  if (value === null || value === undefined) return new Uint8Array();
  let text: string | undefined;
  try {
    text = JSON.stringify(value);
  } catch (err) {
    throw PluginError.fromWire("invalid_params", `socket attachment is not JSON: ${String(err)}`);
  }
  if (text === undefined) {
    throw PluginError.fromWire("invalid_params", "socket attachment is not JSON");
  }
  const bytes = new TextEncoder().encode(text);
  if (bytes.byteLength > MAX_SOCKET_ATTACHMENT_BYTES) {
    throw PluginError.fromWire(
      "payload_too_large",
      `socket attachment of ${bytes.byteLength} bytes exceeds ${MAX_SOCKET_ATTACHMENT_BYTES}`,
    );
  }
  return bytes;
}

/**
 * Decodes stored attachment bytes.
 *
 * @param bytes - Bytes the host kept.
 * @returns The JSON value, `null` when empty, or the text itself when the
 *   bytes are not JSON (an attachment another SDK stored).
 */
export function decodeAttachment(bytes: Uint8Array): unknown {
  if (bytes.byteLength === 0) return null;
  const text = new TextDecoder().decode(bytes);
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

/**
 * Makes one call and turns a failure back into a `PluginError`.
 *
 * @param transport - The adapter's grant, or `undefined` when not granted.
 * @param op - Operation.
 * @param body - Fields.
 * @returns The reply body.
 * @throws PluginError carrying the host's code.
 */
async function call(
  transport: SocketsTransport | undefined,
  op: SocketOp,
  body: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  if (!transport) {
    throw PluginError.fromWire("unavailable", "SOCKETS is not granted to this invocation");
  }
  const reply = await transport.call(op, body);
  if ("err" in reply) {
    throw PluginError.fromWire(reply.err.code || "internal", reply.err.message || "");
  }
  return reply.ok ?? {};
}

/** Wire fields of one socket, as `connect` and `list` answer them. */
interface SocketFields {
  /** Socket id. */
  socket?: unknown;
  /** URL without credentials. */
  url?: unknown;
  /** Chosen subprotocol. */
  protocol?: unknown;
  /** Attachment, base64 on the route or bytes from a wire event. */
  attachment?: unknown;
}

/** {@link HostWebSocket} over one transport. */
class HostSocket implements HostWebSocket {
  readonly id: string;
  readonly url: string;
  readonly protocol: string;
  #transport: SocketsTransport | undefined;
  #attachment: Uint8Array;

  /**
   * @param transport - The adapter's grant.
   * @param fields - Socket id, URL, subprotocol and attachment.
   */
  constructor(transport: SocketsTransport | undefined, fields: SocketFields) {
    this.id = String(fields.socket ?? "");
    this.url = typeof fields.url === "string" ? fields.url : "";
    this.protocol = typeof fields.protocol === "string" ? fields.protocol : "";
    this.#transport = transport;
    this.#attachment =
      fields.attachment instanceof Uint8Array ? fields.attachment : fromBase64(fields.attachment);
  }

  async send(message: SocketSendData): Promise<void> {
    await call(this.#transport, "send", { socket: this.id, frame: routeFrame(message) });
  }

  async close(code?: number, reason?: string): Promise<void> {
    await call(this.#transport, "close", {
      socket: this.id,
      code: Math.max(0, Math.floor(Number(code ?? 0) || 0)),
      reason: String(reason ?? ""),
    });
  }

  async serializeAttachment(value: unknown): Promise<void> {
    const bytes = encodeAttachment(value);
    await call(this.#transport, "attachment", { socket: this.id, attachment: toBase64(bytes) });
    this.#attachment = bytes;
  }

  deserializeAttachment(): unknown {
    return decodeAttachment(this.#attachment);
  }
}

/**
 * A handle for one socket, as a handler receives it.
 *
 * @param transport - The adapter's grant, when this invocation has one.
 * @param fields - Socket id, URL, subprotocol and attachment.
 * @returns The handle.
 */
export function hostWebSocketOf(
  transport: SocketsTransport | undefined,
  fields: SocketFields,
): HostWebSocket {
  return new HostSocket(transport, fields);
}

/**
 * Route JSON of the connect options.
 *
 * @param url - Target URL.
 * @param options - Author options.
 * @returns `connect` body.
 */
function connectBody(
  url: string,
  options: SocketConnectOptions | undefined,
): Record<string, unknown> {
  const headers = options?.headers;
  const pairs: Array<[string, string]> = Array.isArray(headers)
    ? headers
    : Object.entries(headers ?? {});
  return {
    url: String(url ?? ""),
    protocols: (options?.protocols ?? []).map(String),
    headers: pairs.map(([name, value]) => ({ name: String(name), value: String(value) })),
    autoResponses: (options?.autoResponses ?? []).map((pair) => ({
      request: routeFrame(pair.request),
      response: routeFrame(pair.response),
    })),
    attachment: toBase64(encodeAttachment(options?.attachment)),
    trustAnchors: (options?.trustAnchors ?? []).map((anchor) =>
      typeof anchor === "string" ? pemBody(anchor) : toBase64(anchor),
    ),
  };
}

/**
 * The base64 body of one PEM `CERTIFICATE` block, which is the DER's base64.
 *
 * @param pem - The PEM text.
 * @returns The base64 DER.
 * @throws PluginError `invalid_params` when it holds no certificate block.
 */
function pemBody(pem: string): string {
  const match = /-----BEGIN CERTIFICATE-----([A-Za-z0-9+/=\s]+)-----END CERTIFICATE-----/.exec(pem);
  if (!match) {
    throw new PluginError("invalid_params", "a trust anchor PEM holds no CERTIFICATE block");
  }
  return match[1].replace(/\s+/g, "");
}

/**
 * The author's `SOCKETS` binding over the adapter's grant.
 *
 * @param transport - The adapter's grant.
 * @returns The binding installed on `env`.
 */
export function socketsBinding(transport: SocketsTransport): SocketsBinding {
  return Object.freeze({
    async connect(url: string, options?: SocketConnectOptions): Promise<HostWebSocket> {
      const info = await call(transport, "connect", connectBody(url, options));
      return new HostSocket(transport, info);
    },
    async getWebSockets(): Promise<HostWebSocket[]> {
      const reply = await call(transport, "list", {});
      const sockets = Array.isArray(reply.sockets) ? (reply.sockets as SocketFields[]) : [];
      return sockets.map((info) => new HostSocket(transport, info));
    },
    getWebSocket(id: string): HostWebSocket {
      return new HostSocket(transport, { socket: String(id ?? "") });
    },
  });
}
