/**
 * Test doubles: a fake library that serves one resource with byte ranges,
 * and a Cloudflare edge played by Node's own HTTP/2 client (nghttp2), so
 * the session's framing and header compression meet an independent
 * implementation.
 */

import assert from "node:assert/strict";
import http2 from "node:http2";
import { duplexPair } from "node:stream";

import { Session, type SessionIo } from "../src/agent.ts";
import { MessageFramer } from "../src/capnp.ts";
import type { Registration } from "../src/rpc.ts";
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
/**
 * Bytes of a hex string.
 *
 * @param value - Hex digits, whitespace ignored.
 * @returns The bytes.
 */
export function hex(value: string): Uint8Array {
  return new Uint8Array(Buffer.from(value.replace(/\s+/g, ""), "hex"));
}

/** What the reference Cap'n Proto encoder produced for the edge's messages. */
export const EDGE = {
  /** `registerConnection` answered: uuid 01..10, location SJC, remotely managed. */
  registered:
    "0000000011000000000000000100010003000000000000000000000002000100010000000000000000000000000000000000000000000200040000000000010000000000000000000000000001000100010000000000000000000000010002000100000000000000050000008200000009000000220000000102030405060708090a0b0c0d0e0f10534a430000000000",
  /** `registerConnection` refused: "too many connections", retry after 5 s. */
  refused:
    "00000000110000000000000001000100030000000000000000000000020001000100000000000000000000000000000000000000000002000400000000000100000000000000000000000000010001000000000000000000000000000200010000f2052a01000000010000000000000001000000aa000000746f6f206d616e7920636f6e6e656374696f6e7300000000",
  /** `registerConnection` refused with an empty error. */
  refusedEmpty:
    "000000000e00000000000000010001000300000000000000000000000200010001000000000000000000000000000000000000000000020004000000000001000000000000000000000000000100010000000000000000000000000002000100000000000000000000000000000000000000000000000000",
  /** A bootstrap request, question 4. */
  bootstrap4:
    "000000000500000000000000010001000800000000000000000000000100010004000000000000000000000000000000",
  /** A call on an imported capability, question 9. */
  call9:
    "000000000b00000000000000010001000200000000000000000000000300030009000000020000000100000000000000000000000000000008000000010001000000000000000000000000000000000000000000000000000000000000000000",
  /** An abort: "shutting down". */
  abort:
    "000000000900000000000000010001000100000000000000000000000100030000000000000000000900000072000000000000000000000000000000000000007368757474696e6720646f776e000000",
  /** `registerConnection` answered with an exception: "no such tunnel". */
  exception:
    "000000000c0000000000000001000100030000000000000000000000020001000100000000000100000000000000000000000000010003000000000000000000090000007a000000000000000000000000000000000000006e6f20737563682074756e6e656c0000",
  /** `registerConnection` answered with empty results. */
  nullResults:
    "000000000800000000000000010001000300000000000000000000000200010001000000000000000000000000000000000000000000020000000000000000000000000000000000",
  /** A finish for question 0. */
  finish: "00000000040000000000000001000100040000000000000000000000010000000000000000000000",
  /** The bootstrap's answer (question 0), with empty results. */
  return0:
    "000000000800000000000000010001000300000000000000000000000200010000000000000000000000000000000000000000000000020000000000000000000000000000000000",
  /** The edge did not understand a bootstrap request. */
  unimplemented:
    "00000000070000000000000001000100000000000000000000000000010001000800000000000000000000000100010000000000000000000000000000000000",
} as const;

/** The registration the tests send; the reference encoder's `call.txt`. */
export const REGISTRATION: Registration = {
  accountTag: "acct",
  tunnelSecret: new Uint8Array([1, 2, 3, 4, 5]),
  tunnelId: hex("00112233445566778899aabbccddeeff"),
  connIndex: 2,
  clientId: hex("ffeeddccbbaa99887766554433221100"),
  version: "bookclerk/0.1.0",
  arch: "workerd",
  replaceExisting: true,
  previousAttempts: 3,
};

/** A response the edge read. */
export interface EdgeResponse {
  /** HTTP/2 status. */
  status: number;
  /** The HTTP/2 fields. */
  headers: http2.IncomingHttpHeaders;
  /** The fields `cf-cloudflared-response-headers` carries, decoded. */
  fields: Array<[string, string]>;
  /** The body. */
  body: Uint8Array;
}

/**
 * Decodes `cf-cloudflared-response-headers` the way the edge does
 * (cloudflared's `DeserializeHeaders`).
 *
 * @param value - The serialized value.
 * @returns The fields.
 */
export function deserializeFields(value: string): Array<[string, string]> {
  if (value === "") return [];
  return value.split(";").map((pair) => {
    const [name, field] = pair.split(":");
    return [
      Buffer.from(name ?? "", "base64").toString("latin1"),
      Buffer.from(field ?? "", "base64").toString("latin1"),
    ];
  });
}

/** A Cloudflare edge: Node's HTTP/2 client on one end of a pipe, a session on the other. */
export class Edge {
  /** The session under test. */
  readonly session: Session;
  /** The library it serves. */
  readonly library: FakeLibrary;
  /** The edge's HTTP/2 client. */
  readonly client: http2.ClientHttp2Session;
  /** The session's IO. */
  readonly io: SessionIo;
  /** Errors the session threw while reading. */
  readonly errors: unknown[] = [];
  /** Raw writes, for size checks. */
  readonly writes: Uint8Array[] = [];
  /** Reads in order, so the session sees bytes as the client wrote them. */
  #chain: Promise<void> = Promise.resolve();
  /** The session's end of the pipe. */
  readonly #pipe: ReturnType<typeof duplexPair>[1];

  /**
   * Starts a session and connects the client to it.
   *
   * @param library - The library the session serves.
   * @param registration - What the session registers.
   */
  constructor(library: FakeLibrary, registration: Registration = REGISTRATION) {
    const [client, server] = duplexPair();
    this.#pipe = server;
    this.library = library;
    this.session = new Session({ registration, hostname: "books.example.com" });
    this.io = {
      write: async (chunk) => {
        this.writes.push(chunk);
        server.write(chunk);
      },
      library,
    };
    server.on("data", (chunk: Buffer) => {
      this.#chain = this.#chain
        .then(() => this.session.receive(this.io, new Uint8Array(chunk)))
        .catch((err: unknown) => {
          this.errors.push(err);
        });
    });
    this.#chain = this.session.start(this.io);
    this.client = http2.connect("http://edge.test", { createConnection: () => client });
    this.client.on("error", () => {});
  }

  /**
   * Feeds raw bytes to the session as if the edge sent them.
   *
   * @param bytes - The bytes.
   * @returns Resolves once read.
   */
  inject(bytes: Uint8Array): Promise<void> {
    this.#chain = this.#chain.then(() => this.session.receive(this.io, bytes));
    return this.#chain;
  }

  /**
   * Waits until the session has read everything the client wrote.
   *
   * @returns Resolves once idle.
   */
  async settle(): Promise<void> {
    for (let i = 0; i < 20; i++) {
      await new Promise((resolve) => setImmediate(resolve));
      await this.#chain;
    }
  }

  /**
   * Opens the control stream and answers the registration.
   *
   * @param answer - The edge's `Return`, as hex.
   * @returns The control stream and the messages the session sent on it.
   */
  async register(
    answer: string = EDGE.registered,
  ): Promise<{ control: http2.ClientHttp2Stream; messages: Uint8Array[][] }> {
    const control = this.client.request(
      {
        ":method": "GET",
        ":path": "/",
        "cf-cloudflared-proxy-connection-upgrade": "control-stream",
      },
      { endStream: false },
    );
    const framer = new MessageFramer();
    const messages: Uint8Array[][] = [];
    const two = new Promise<void>((resolve) => {
      control.on("data", (chunk: Buffer) => {
        messages.push(...framer.push(new Uint8Array(chunk)));
        if (messages.length >= 2) resolve();
      });
    });
    const status = await new Promise<number>((resolve) => {
      control.on("response", (headers) => resolve(Number(headers[":status"])));
    });
    assert.equal(status, 200);
    await two;
    control.write(hex(answer));
    await this.settle();
    return { control, messages };
  }

  /**
   * Sends a request and reads its whole response.
   *
   * @param headers - Request fields (`:path` defaults to `/`).
   * @param body - The body, if any.
   * @returns The response.
   */
  request(headers: http2.OutgoingHttpHeaders, body?: Uint8Array | string): Promise<EdgeResponse> {
    const stream = this.client.request(
      { ":method": "GET", ":path": "/", ...headers },
      { endStream: body === undefined },
    );
    if (body !== undefined) stream.end(body);
    return readResponse(stream);
  }

  /** Ends the pipe. */
  close(): void {
    this.client.destroy();
    this.#pipe.destroy();
  }
}

/**
 * Reads a whole response from a client stream.
 *
 * @param stream - The stream.
 * @returns The response.
 */
export function readResponse(stream: http2.ClientHttp2Stream): Promise<EdgeResponse> {
  return new Promise((resolve, reject) => {
    let head: http2.IncomingHttpHeaders = {};
    const parts: Buffer[] = [];
    stream.on("response", (headers) => {
      head = headers;
    });
    stream.on("data", (chunk: Buffer) => parts.push(chunk));
    stream.on("error", reject);
    stream.on("close", () => {
      if (head[":status"] === undefined) {
        reject(new Error(`stream closed with code ${stream.rstCode}`));
        return;
      }
      resolve({
        status: Number(head[":status"]),
        headers: head,
        fields: deserializeFields(String(head["cf-cloudflared-response-headers"] ?? "")),
        body: new Uint8Array(Buffer.concat(parts)),
      });
    });
  });
}
