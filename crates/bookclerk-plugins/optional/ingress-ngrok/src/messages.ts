/**
 * ngrok's agent messages, sans-IO: the typed-stream prefix, the JSON RPCs
 * (auth, bind) and the header on each proxied connection.
 *
 * Ported from `golang.ngrok.com/ngrok` v1.12.1
 * (`internal/tunnel/proto/msg.go`, `internal/tunnel/client`), MIT,
 * Copyright 2022 ngrok, Inc. Every stream starts with its 32-bit type,
 * big-endian; an RPC then carries one JSON request and one JSON reply.
 */

/** Stream types (`proto.ReqType`), and muxado's heartbeat type. */
export const StreamType = {
  /** Client: authenticate the session. */
  Auth: 0,
  /** Client: bind a tunnel (an endpoint). */
  Bind: 1,
  /** Client: unbind a tunnel. */
  Unbind: 2,
  /** Server: one proxied connection. */
  Proxy: 3,
  /** Server: restart the agent. */
  Restart: 4,
  /** Server: stop the agent. */
  Stop: 5,
  /** Server: update the agent. */
  Update: 6,
  /** Server: an endpoint was stopped. */
  StopTunnel: 9,
  /** Either side: muxado heartbeat; 4 random bytes echoed back. */
  Heartbeat: 0xffffffff,
} as const;

/** Protocol versions offered, in order of preference. */
export const PROTOCOL_VERSIONS = ["3", "2"] as const;

/** Heartbeat interval announced to the server, in milliseconds. */
export const HEARTBEAT_INTERVAL_MS = 10_000;

/** Heartbeat tolerance announced to the server, in milliseconds. */
export const HEARTBEAT_TOLERANCE_MS = 15_000;

/** Largest RPC reply or server request this agent reads, in bytes. */
export const MAX_RPC_BYTES = 64 * 1024;

/** Largest proxy header JSON this agent reads, in bytes. */
export const MAX_PROXY_HEADER_BYTES = 16 * 1024;

/** Nanoseconds per millisecond (Go durations are nanoseconds). */
const NS_PER_MS = 1_000_000;

/**
 * The 4-byte type prefix of a stream.
 *
 * @param type - The stream type.
 * @returns Its big-endian bytes.
 */
export function typePrefix(type: number): Uint8Array {
  const out = new Uint8Array(4);
  new DataView(out.buffer).setUint32(0, type >>> 0, false);
  return out;
}

/**
 * Reads a stream's type prefix.
 *
 * @param bytes - The stream's first bytes, at least 4.
 * @returns The type.
 */
export function readType(bytes: Uint8Array): number {
  return new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, false);
}

/** What the agent tells ngrok about itself when it authenticates. */
export interface AuthOptions {
  /** The account's authtoken. Never logged. */
  authtoken: string;
  /** Agent version. */
  version: string;
  /** Session id to resume; empty for a new session. */
  clientId?: string;
}

/**
 * The `Auth` request body.
 *
 * @param options - Authtoken and version.
 * @returns The JSON text, newline-terminated as Go's encoder writes it.
 */
export function authRequest(options: AuthOptions): string {
  const userAgent = `bookclerk-ingress-ngrok/${options.version}`;
  const unsupported = "not supported by the Bookclerk agent";
  return `${JSON.stringify({
    Version: PROTOCOL_VERSIONS,
    ClientId: options.clientId ?? "",
    Extra: {
      OS: "linux",
      Arch: "wasm",
      Authtoken: options.authtoken,
      Version: options.version,
      Hostname: "",
      UserAgent: userAgent,
      Metadata: "",
      Cookie: "",
      HeartbeatInterval: HEARTBEAT_INTERVAL_MS * NS_PER_MS,
      HeartbeatTolerance: HEARTBEAT_TOLERANCE_MS * NS_PER_MS,
      ClientType: "bookclerk",
      UpdateUnsupportedError: unsupported,
      StopUnsupportedError: unsupported,
      RestartUnsupportedError: unsupported,
    },
  })}\n`;
}

/** What binding an HTTPS endpoint needs. */
export interface BindOptions {
  /** Session-unique bind id the agent picks. */
  bindId: string;
  /** The reserved or free static domain to serve on, e.g. `x.ngrok-free.app`. */
  domain: string;
  /** What the agent forwards to, shown in ngrok's dashboard. */
  forwardsTo: string;
}

/**
 * The `Bind` request body for an HTTPS endpoint on `domain`.
 *
 * @param options - Bind id, domain and forward label.
 * @returns The JSON text.
 */
export function bindRequest(options: BindOptions): string {
  return `${JSON.stringify({
    Id: options.bindId,
    Proto: "https",
    ForwardsTo: options.forwardsTo,
    ForwardsProto: "http1",
    Opts: {
      URL: "",
      Domain: options.domain,
      Hostname: "",
      Subdomain: "",
      Auth: "",
      HostHeaderRewrite: false,
      LocalURLScheme: "http",
    },
    Extra: { Name: "", Token: "", IPPolicyRef: "", Metadata: "", Description: "Bookclerk" },
  })}\n`;
}

/** The server's answer to `Auth`. */
export interface AuthReply {
  /** Session id, for resuming. */
  clientId: string;
  /** Why authentication failed; empty on success. */
  error: string;
}

/** The server's answer to `Bind`. */
export interface BindReply {
  /** The bind id. */
  bindId: string;
  /** The endpoint's public URL. */
  url: string;
  /** Why the bind failed; empty on success. */
  error: string;
}

/**
 * Reads a string field of a JSON object.
 *
 * @param value - The parsed object.
 * @param key - The field.
 * @returns The string, or `""`.
 */
function text(value: Record<string, unknown>, key: string): string {
  const field = value[key];
  return typeof field === "string" ? field : "";
}

/**
 * Parses one JSON object from an RPC stream's bytes.
 *
 * @param bytes - Everything read so far.
 * @returns The object, `null` when more bytes may complete it, or an error
 *   message when they can't.
 */
export function parseRpcObject(bytes: Uint8Array): Record<string, unknown> | null | string {
  if (bytes.length > MAX_RPC_BYTES) return `reply over ${MAX_RPC_BYTES} bytes`;
  let body: string;
  try {
    body = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return "reply is not UTF-8";
  }
  // JSON's own whitespace only: `trim` would also drop \v, \f and U+00A0.
  const trimmed = body.replace(/^[ \t\r\n]+|[ \t\r\n]+$/g, "");
  if (trimmed.length === 0) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    // Go's encoder ends each value with a newline; until then it may be partial.
    return body.endsWith("\n") ? "reply is not JSON" : null;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return "reply is not a JSON object";
  }
  return parsed as Record<string, unknown>;
}

/**
 * The fields of an `AuthResp`.
 *
 * @param value - The parsed reply.
 * @returns Session id and error.
 */
export function authReply(value: Record<string, unknown>): AuthReply {
  return { clientId: text(value, "ClientId"), error: text(value, "Error") };
}

/**
 * The fields of a `BindResp`.
 *
 * @param value - The parsed reply.
 * @returns Bind id, URL and error.
 */
export function bindReply(value: Record<string, unknown>): BindReply {
  return { bindId: text(value, "Id"), url: text(value, "URL"), error: text(value, "Error") };
}

/** The header ngrok sends before a proxied connection's bytes. */
export interface ProxyHeader {
  /** The bind the connection is for. */
  bindId: string;
  /** The client's address, `ip:port`. */
  clientAddr: string;
  /** The endpoint protocol, e.g. `https`. */
  proto: string;
  /** Whether the bytes are still TLS (passthrough); this agent refuses those. */
  passthroughTls: boolean;
}

/**
 * Reads a proxy header: a 64-bit little-endian length, then that much JSON.
 *
 * @param bytes - The stream's bytes after its type prefix.
 * @returns The header and the bytes after it, `null` when more are
 *   needed, or an error message.
 */
export function parseProxyHeader(
  bytes: Uint8Array,
): { header: ProxyHeader; rest: Uint8Array } | null | string {
  if (bytes.length < 8) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, 8);
  const high = view.getUint32(4, true);
  const length = view.getUint32(0, true);
  if (high !== 0 || length > MAX_PROXY_HEADER_BYTES) return "proxy header too large";
  if (bytes.length < 8 + length) return null;
  const value = parseRpcObject(bytes.subarray(8, 8 + length));
  if (value === null) return "proxy header is empty";
  if (typeof value === "string") return `proxy header: ${value}`;
  return {
    header: {
      bindId: text(value, "Id"),
      clientAddr: text(value, "ClientAddr"),
      proto: text(value, "Proto"),
      passthroughTls: value["PassthroughTLS"] === true,
    },
    rest: bytes.slice(8 + length),
  };
}

/**
 * Encodes a proxy header, as ngrok's server does (for tests and fuzzing).
 *
 * @param header - The header.
 * @returns Length-prefixed JSON.
 */
export function encodeProxyHeader(header: ProxyHeader): Uint8Array {
  const json = new TextEncoder().encode(
    JSON.stringify({
      Id: header.bindId,
      ClientAddr: header.clientAddr,
      Proto: header.proto,
      EdgeType: "",
      PassthroughTLS: header.passthroughTls,
    }),
  );
  const out = new Uint8Array(8 + json.length);
  new DataView(out.buffer).setUint32(0, json.length, true);
  out.set(json, 8);
  return out;
}

/**
 * The client IP of a proxied connection.
 *
 * @param clientAddr - `ip:port` or `[ipv6]:port`.
 * @returns The IP, or `undefined` when it does not parse.
 */
export function clientIp(clientAddr: string): string | undefined {
  const bracketed = /^\[([0-9A-Fa-f:.]+)\]:\d+$/.exec(clientAddr);
  if (bracketed) return bracketed[1];
  const plain = /^(\d{1,3}(?:\.\d{1,3}){3}):\d+$/.exec(clientAddr);
  return plain ? plain[1] : undefined;
}
