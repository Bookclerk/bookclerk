/**
 * Test doubles: a fake ngrok server that speaks muxado to a session, and a
 * fake library that serves one resource with byte ranges.
 */

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
import type { Library } from "../src/serve.ts";

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

/** A library made of a handler, recording each request. */
export class FakeLibrary implements Library {
  /** Requests seen, in order. */
  readonly seen: Seen[] = [];
  /** Answers each request. */
  handler: (request: Request) => Response | Promise<Response>;

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
