/**
 * Test doubles: a fake ngrok server that speaks muxado to a session, and a
 * fake library that serves one resource with byte ranges.
 */

import assert from "node:assert/strict";

import { StreamType, encodeProxyHeader, readType, typePrefix } from "../src/messages.ts";
import {
  FLAG_FIN,
  FLAG_SYN,
  FrameReader,
  concat,
  encodeData,
  encodeWndInc,
  type Frame,
} from "../src/mux.ts";
import type { SessionIo } from "../src/agent.ts";
import type { Library, LibrarySocket, LibraryUpgrade } from "../src/serve.ts";

/** One request the fake library saw. */
export interface Seen {
  /** Method. */
  method: string;
  /** Full URL. */
  url: string;
  /** Header fields. */
  headers: Headers;
  /** Body text. */
  body: string;
  /** Client address. */
  clientAddress: string | undefined;
}

/**
 * Text of bytes.
 *
 * @param bytes - UTF-8 bytes.
 * @returns The text.
 */
export function text(bytes: Uint8Array): string {
  return new TextDecoder().decode(bytes);
}

/**
 * Bytes of text.
 *
 * @param value - Text.
 * @returns UTF-8 bytes.
 */
export function bytes(value: string): Uint8Array {
  return new TextEncoder().encode(value);
}

/** A socket to the library the fake hands out, recording what it got. */
export class FakeSocket implements LibrarySocket {
  /** Its id. */
  readonly id: string;
  /** The subprotocol it chose. */
  readonly protocol: string;
  /** The stream it was opened for. */
  readonly stream: number;
  /** Messages sent to it, in order. */
  readonly sent: Array<string | Uint8Array> = [];
  /** Its close, once closed. */
  closed: { code: number; reason: string } | null = null;
  /** Whether `send` fails. */
  failing = false;

  /**
   * Creates a socket.
   *
   * @param id - Its id.
   * @param protocol - The subprotocol it chose.
   * @param stream - The stream it was opened for.
   */
  constructor(id: string, protocol: string, stream: number) {
    this.id = id;
    this.protocol = protocol;
    this.stream = stream;
  }

  /**
   * Records a message.
   *
   * @param message - The message.
   * @returns Resolves, or rejects when closed or failing.
   */
  send(message: string | Uint8Array): Promise<void> {
    if (this.closed !== null || this.failing) return Promise.reject(new Error("closed"));
    this.sent.push(typeof message === "string" ? message : message.slice());
    return Promise.resolve();
  }

  /**
   * Records the close.
   *
   * @param code - Close code.
   * @param reason - Close reason.
   * @returns Resolves.
   */
  close(code: number, reason: string): Promise<void> {
    this.closed ??= { code, reason };
    return Promise.resolve();
  }
}

/** How the fake library answers an upgrade. */
export type UpgradeHandler = (
  request: Request,
  stream: number,
) => { protocol: string } | Response | Promise<{ protocol: string } | Response>;

/** A library made of a handler, recording each request. */
export class FakeLibrary implements Library {
  /** Requests seen, in order. */
  readonly seen: Seen[] = [];
  /** Answers each request. */
  handler: (request: Request) => Response | Promise<Response>;
  /** Answers each upgrade: a subprotocol to accept with, or a refusal. */
  upgrades: UpgradeHandler = () => ({ protocol: "" });
  /** Sockets handed out, by id. */
  readonly sockets = new Map<string, FakeSocket>();
  /** Upgrade requests seen, in order. */
  readonly upgraded: Seen[] = [];

  /**
   * Creates a library.
   *
   * @param handler - Answers each request.
   */
  constructor(handler: (request: Request) => Response | Promise<Response>) {
    this.handler = handler;
  }

  /**
   * Records and answers a request.
   *
   * @param request - The request.
   * @param clientAddress - The client's IP.
   * @returns The handler's answer.
   */
  async fetch(request: Request, clientAddress: string | undefined): Promise<Response> {
    this.seen.push({
      method: request.method,
      url: request.url,
      headers: request.headers,
      body: request.body ? await request.clone().text() : "",
      clientAddress,
    });
    return this.handler(request);
  }

  /**
   * Records an upgrade and accepts or refuses it.
   *
   * @param request - The client's `GET`.
   * @param clientAddress - The client's IP.
   * @param stream - The stream it rides on.
   * @returns The socket, or the refusal.
   */
  async connect(
    request: Request,
    clientAddress: string | undefined,
    stream: number,
  ): Promise<LibraryUpgrade> {
    this.upgraded.push({
      method: request.method,
      url: request.url,
      headers: request.headers,
      body: "",
      clientAddress,
    });
    const answer = await this.upgrades(request, stream);
    if (answer instanceof Response) return { response: answer };
    const socket = new FakeSocket(`live${this.sockets.size}`, answer.protocol, stream);
    this.sockets.set(socket.id, socket);
    return { socket };
  }

  /**
   * A socket by id.
   *
   * @param id - Its id.
   * @returns It, or `null` once closed or unknown.
   */
  socket(id: string): LibrarySocket | null {
    const socket = this.sockets.get(id);
    return socket && socket.closed === null ? socket : null;
  }
}

/**
 * A masked client frame.
 *
 * @param opcode - Opcode.
 * @param payload - Payload.
 * @param fin - Whether it ends its message.
 * @param mask - Masking key.
 * @returns The frame.
 */
export function clientFrame(
  opcode: number,
  payload: Uint8Array | string,
  fin = true,
  mask = new Uint8Array([0x37, 0xfa, 0x21, 0x3d]),
): Uint8Array {
  const data = typeof payload === "string" ? bytes(payload) : payload;
  const extra = data.length < 126 ? 0 : data.length < 65536 ? 2 : 8;
  const out = new Uint8Array(2 + extra + 4 + data.length);
  out[0] = (fin ? 0x80 : 0) | opcode;
  const view = new DataView(out.buffer);
  if (extra === 0) out[1] = 0x80 | data.length;
  else if (extra === 2) {
    out[1] = 0x80 | 126;
    view.setUint16(2, data.length);
  } else {
    out[1] = 0x80 | 127;
    view.setUint32(6, data.length);
  }
  out.set(mask, 2 + extra);
  for (let i = 0; i < data.length; i++) {
    out[6 + extra + i] = (data[i] as number) ^ (mask[i % 4] as number);
  }
  return out;
}

/** A server frame, decoded. */
export interface ServerFrame {
  /** Opcode. */
  opcode: number;
  /** Payload. */
  payload: Uint8Array;
}

/**
 * Decodes unmasked server frames, whole.
 *
 * @param data - Frame bytes.
 * @returns The frames.
 */
export function serverFrames(data: Uint8Array): ServerFrame[] {
  const out: ServerFrame[] = [];
  let at = 0;
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  while (at < data.length) {
    const opcode = (data[at] as number) & 0x0f;
    assert(((data[at + 1] as number) & 0x80) === 0, "server frames are not masked");
    let length = (data[at + 1] as number) & 0x7f;
    at += 2;
    if (length === 126) {
      length = view.getUint16(at);
      at += 2;
    } else if (length === 127) {
      length = view.getUint32(at + 4);
      at += 8;
    }
    out.push({ opcode, payload: data.slice(at, at + length) });
    at += length;
  }
  return out;
}

/**
 * A body that streams in 64 KiB chunks, as workerd's bodies do.
 *
 * @param body - The bytes.
 * @returns The stream.
 */
export function chunkedBody(body: Uint8Array): ReadableStream<Uint8Array> {
  let at = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (at >= body.length) {
        controller.close();
        return;
      }
      controller.enqueue(body.slice(at, at + 65536));
      at += 65536;
    },
  });
}

/**
 * A handler that serves `body` with byte-range support, as the library's
 * playback routes do.
 *
 * @param body - The resource.
 * @param extra - Extra response fields.
 * @returns The handler.
 */
export function rangedResource(
  body: Uint8Array,
  extra: Record<string, string> = {},
): (request: Request) => Response {
  return (request) => {
    const range = request.headers.get("range");
    const match = range ? /^bytes=(\d+)-(\d*)$/.exec(range) : null;
    if (!match) {
      return new Response(chunkedBody(body), {
        headers: {
          "content-type": "audio/mp4",
          "accept-ranges": "bytes",
          "content-length": String(body.length),
          ...extra,
        },
      });
    }
    const first = Number(match[1]);
    const last = match[2] ? Math.min(Number(match[2]), body.length - 1) : body.length - 1;
    return new Response(chunkedBody(body.subarray(first, last + 1)), {
      status: 206,
      headers: {
        "content-type": "audio/mp4",
        "accept-ranges": "bytes",
        "content-range": `bytes ${first}-${last}/${body.length}`,
        "content-length": String(last - first + 1),
        ...extra,
      },
    });
  };
}

/** A fake ngrok server: decodes what the session writes. */
export class FakeNgrok {
  /** Decodes the session's writes. */
  readonly #reader = new FrameReader();
  /** Frames the session wrote and the test has not taken. */
  #frames: Frame[] = [];
  /** Raw writes, for size checks. */
  readonly writes: Uint8Array[] = [];
  /** The session's IO. */
  readonly io: SessionIo;

  /**
   * Creates a server.
   *
   * @param library - The library the session serves.
   */
  constructor(library: Library) {
    this.io = {
      write: async (chunk) => {
        this.writes.push(chunk);
        this.#frames.push(...this.#reader.push(chunk));
      },
      library,
    };
  }

  /**
   * Takes the frames written since the last call.
   *
   * @returns The frames.
   */
  take(): Frame[] {
    const out = this.#frames;
    this.#frames = [];
    return out;
  }
}

/**
 * The DATA bytes of one stream, joined.
 *
 * @param frames - Frames.
 * @param stream - The stream.
 * @returns Its bytes and whether a FIN was among them.
 */
export function streamBytes(frames: Frame[], stream: number): { bytes: Uint8Array; fin: boolean } {
  const parts: Uint8Array[] = [];
  let fin = false;
  for (const frame of frames) {
    if (frame.kind === "data" && frame.stream === stream) {
      parts.push(frame.data);
      fin ||= frame.fin;
    }
  }
  return { bytes: concat(...parts), fin };
}

/**
 * The first SYN DATA frame of a stream type, and its body after the type.
 *
 * @param frames - Frames.
 * @param type - Stream type.
 * @returns The stream id and body, or `null`.
 */
export function opened(frames: Frame[], type: number): { stream: number; body: string } | null {
  for (const frame of frames) {
    if (
      frame.kind === "data" &&
      frame.syn &&
      frame.data.length >= 4 &&
      readType(frame.data) === type
    ) {
      return { stream: frame.stream, body: text(frame.data.subarray(4)) };
    }
  }
  return null;
}

/**
 * A reply on a client stream.
 *
 * @param stream - The stream.
 * @param value - The JSON reply.
 * @returns The frame.
 */
export function reply(stream: number, value: unknown): Uint8Array {
  return encodeData(stream, bytes(JSON.stringify(value)));
}

/**
 * Opens a proxied connection carrying `request`.
 *
 * @param stream - Server stream id (even).
 * @param request - Raw HTTP/1.1 bytes.
 * @param clientAddr - The client's address.
 * @param fin - Whether to end ngrok's side too.
 * @returns The frame.
 */
export function proxy(
  stream: number,
  request: string | Uint8Array,
  clientAddr = "203.0.113.9:5555",
  fin = false,
): Uint8Array {
  const header = encodeProxyHeader({
    bindId: "bookclerk",
    clientAddr,
    proto: "https",
    passthroughTls: false,
  });
  const body = typeof request === "string" ? bytes(request) : request;
  return encodeData(
    stream,
    concat(typePrefix(StreamType.Proxy), header, body),
    FLAG_SYN | (fin ? FLAG_FIN : 0),
  );
}

/**
 * The window update a test grants.
 *
 * @param stream - The stream.
 * @param increment - Bytes.
 * @returns The frame.
 */
export function grant(stream: number, increment: number): Uint8Array {
  return encodeWndInc(stream, increment);
}
