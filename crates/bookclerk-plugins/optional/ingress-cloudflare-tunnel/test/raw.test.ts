/**
 * The session fed hand-built frames, for the edge behavior Node's HTTP/2
 * client never produces: protocol errors, window overflows, resets at
 * awkward moments and WebSocket limits.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { MAX_SOCKET_BACKLOG, Session, type SessionIo } from "../src/agent.ts";
import {
  ErrorCode,
  Flag,
  FrameType,
  H2Error,
  MAX_WINDOW,
  PREFACE,
  Setting,
  encodeData,
  encodeFrame,
  encodeGoAway,
  encodeHeaders as encodeHeaderFrames,
  encodeRst,
  encodeSettings,
  encodeWindowUpdate,
  join,
} from "../src/h2.ts";
import { HpackDecoder, encodeHeaders } from "../src/hpack.ts";
import { MAX_REQUEST_BODY } from "../src/serve.ts";
import { CloseCode, Opcode } from "../src/ws.ts";
import { EDGE, FakeLibrary, REGISTRATION, clientFrame, hex, serverFrames } from "./fake.ts";

/** One frame the session wrote. */
interface Written {
  /** Frame type. */
  type: number;
  /** Flags. */
  flags: number;
  /** Stream id. */
  stream: number;
  /** Payload. */
  payload: Uint8Array;
}

/** The control stream's request fields. */
const CONTROL: Array<[string, string]> = [
  [":method", "GET"],
  [":scheme", "https"],
  [":authority", "edge"],
  [":path", "/"],
  ["cf-cloudflared-proxy-connection-upgrade", "control-stream"],
];

/**
 * A request's fields.
 *
 * @param extra - Fields after the pseudo-fields.
 * @param method - The method.
 * @returns The fields.
 */
function request(extra: Array<[string, string]> = [], method = "GET"): Array<[string, string]> {
  return [
    [":method", method],
    [":scheme", "https"],
    [":authority", "books.example.com"],
    [":path", "/"],
    ...extra,
  ];
}

/** A WebSocket upgrade's fields. */
const UPGRADE = request([
  ["cf-cloudflared-proxy-connection-upgrade", "websocket"],
  ["sec-websocket-version", "13"],
  ["sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="],
]);

/** A session fed raw frames, recording what it writes. */
class Raw {
  /** The session under test. */
  readonly session = new Session({ registration: REGISTRATION, hostname: "books.example.com" });
  /** The library it serves. */
  readonly library: FakeLibrary;
  /** Bytes written, not yet parsed. */
  #buf: Uint8Array = new Uint8Array();
  /** Every frame written, in order. */
  readonly written: Written[] = [];
  /** Decodes the session's header blocks. */
  readonly #hpack = new HpackDecoder(1 << 20);
  /** The session's IO. */
  readonly io: SessionIo;

  /**
   * Creates the harness.
   *
   * @param library - The library the session serves.
   */
  constructor(library = new FakeLibrary(() => new Response("ok"))) {
    this.library = library;
    this.io = {
      write: async (chunk) => {
        this.#buf = join(this.#buf, chunk);
        while (this.#buf.length >= 9) {
          const b = this.#buf;
          const length = ((b[0] as number) << 16) | ((b[1] as number) << 8) | (b[2] as number);
          if (b.length < 9 + length) break;
          const view = new DataView(b.buffer, b.byteOffset, b.byteLength);
          this.written.push({
            type: b[3] as number,
            flags: b[4] as number,
            stream: view.getUint32(5) & 0x7fffffff,
            payload: b.slice(9, 9 + length),
          });
          this.#buf = b.subarray(9 + length);
        }
      },
      library,
    };
  }

  /**
   * Starts the session and sends the preface.
   *
   * @returns Resolves once read.
   */
  async open(): Promise<void> {
    await this.session.start(this.io);
    await this.feed(PREFACE, encodeSettings([]));
  }

  /**
   * Feeds bytes to the session.
   *
   * @param parts - The bytes.
   * @returns Resolves once read.
   */
  feed(...parts: Uint8Array[]): Promise<void> {
    return this.session.receive(this.io, join(...parts));
  }

  /**
   * Sends a header block.
   *
   * @param id - The stream.
   * @param fields - The fields.
   * @param endStream - Whether no body follows.
   * @returns Resolves once read.
   */
  headers(id: number, fields: Array<[string, string]>, endStream: boolean): Promise<void> {
    return this.feed(encodeHeaderFrames(id, encodeHeaders(fields), endStream, 16_384));
  }

  /**
   * Opens the control stream and registers.
   *
   * @returns Resolves once registered.
   */
  async register(): Promise<void> {
    await this.headers(1, CONTROL, false);
    await this.feed(encodeData(1, hex(EDGE.registered)));
    assert.equal(this.session.phase, "serving");
  }

  /**
   * Resets on a stream.
   *
   * @param id - The stream.
   * @returns Each RST_STREAM code written for it.
   */
  resets(id: number): number[] {
    return this.written
      .filter((f) => f.type === FrameType.RstStream && f.stream === id)
      .map((f) => new DataView(f.payload.buffer).getUint32(0));
  }

  /**
   * The `:status` values written on a stream.
   *
   * @param id - The stream.
   * @returns Each status.
   */
  statuses(id: number): string[] {
    const out: string[] = [];
    for (const f of this.written) {
      if (f.type !== FrameType.Headers || f.stream !== id) continue;
      const status = this.#hpack.decode(f.payload).fields.find(([n]) => n === ":status");
      if (status) out.push(status[1]);
    }
    return out;
  }

  /**
   * The DATA written on a stream, joined.
   *
   * @param id - The stream.
   * @returns The bytes.
   */
  data(id: number): Uint8Array {
    return join(
      ...this.written
        .filter((f) => f.type === FrameType.Data && f.stream === id)
        .map((f) => f.payload),
    );
  }
}

/**
 * Splits bytes into DATA frames of at most 16 KiB.
 *
 * @param id - The stream.
 * @param size - How many bytes.
 * @param endStream - Whether the last ends the stream.
 * @returns The frames.
 */
function body(id: number, size: number, endStream = false): Uint8Array {
  const piece = new Uint8Array(16_384);
  const frames: Uint8Array[] = [];
  for (let at = 0; at < size; at += piece.length) {
    const n = Math.min(piece.length, size - at);
    frames.push(encodeData(id, piece.subarray(0, n), endStream && at + n >= size));
  }
  return join(...frames);
}

/**
 * Feeds a large body in pieces, so no single chunk is huge.
 *
 * @param raw - The harness.
 * @param id - The stream.
 * @param size - How many bytes.
 * @returns Resolves once read.
 */
async function bigBody(raw: Raw, id: number, size: number): Promise<void> {
  const step = 4 * 1024 * 1024;
  for (let at = 0; at < size; at += step) await raw.feed(body(id, Math.min(step, size - at)));
}

/**
 * Asserts the session refuses bytes with an HTTP/2 error and a GOAWAY.
 *
 * @param raw - The harness.
 * @param bytes - The bytes.
 * @param code - The expected error code.
 */
async function refuses(raw: Raw, bytes: Uint8Array, code: number): Promise<void> {
  await assert.rejects(
    raw.feed(bytes),
    (err: unknown) => err instanceof H2Error && err.code === code,
  );
  const goaway = raw.written.filter((f) => f.type === FrameType.GoAway).at(-1);
  assert.ok(goaway);
  assert.equal(new DataView(goaway.payload.buffer).getUint32(4), code);
  assert.equal(raw.session.phase, "closed");
}

test("settings change the frame size and every stream's window", async () => {
  const raw = new Raw(new FakeLibrary(() => new Response(new Uint8Array(100_000))));
  await raw.open();
  await raw.register();
  // A response larger than the default window waits for more of it.
  await raw.headers(3, request(), true);
  const before = raw.data(3).length;
  assert.ok(before < 100_000);
  await raw.feed(
    encodeSettings([
      [Setting.MaxFrameSize, 20_000],
      [Setting.InitialWindowSize, 70_000],
    ]),
  );
  await raw.feed(encodeWindowUpdate(0, 10_000));
  assert.ok(raw.data(3).length > before);
});

test("a stream window past 2^31-1 from new settings is a protocol error", async () => {
  const raw = new Raw(new FakeLibrary(() => new Response(new Uint8Array(100_000))));
  await raw.open();
  await raw.register();
  await raw.headers(3, request(), true);
  await raw.feed(encodeWindowUpdate(3, MAX_WINDOW), encodeWindowUpdate(3, 1));
  // Opening the stream's window past the limit resets just that stream.
  assert.deepEqual(raw.resets(3), [ErrorCode.FlowControlError]);
  await raw.headers(5, request(), true);
  await raw.feed(encodeWindowUpdate(5, MAX_WINDOW - 65_535));
  await refuses(
    raw,
    encodeSettings([[Setting.InitialWindowSize, 65_535 + 100]]),
    ErrorCode.FlowControlError,
  );
});

test("a connection window past 2^31-1 is a protocol error", async () => {
  const raw = new Raw();
  await raw.open();
  await refuses(raw, encodeWindowUpdate(0, MAX_WINDOW), ErrorCode.FlowControlError);
});

test("headers on even, old and refused streams", async () => {
  const raw = new Raw();
  await raw.open();
  await raw.register();
  // A second control stream is refused; so is a stream that already ended.
  await raw.headers(3, CONTROL, false);
  assert.deepEqual(raw.resets(3), [ErrorCode.RefusedStream]);
  await raw.headers(3, request(), true);
  assert.deepEqual(raw.resets(3), [ErrorCode.RefusedStream, ErrorCode.StreamClosed]);
  // Fields without a method are malformed.
  await raw.headers(5, [[":path", "/"]], true);
  assert.deepEqual(raw.resets(5), [ErrorCode.ProtocolError]);
  // Window updates and data for unknown streams are ignored.
  await raw.feed(encodeWindowUpdate(41, 10), encodeData(41, new Uint8Array(3)));
  // After GOAWAY, new streams are refused.
  await raw.feed(encodeGoAway(5, ErrorCode.NoError));
  await raw.headers(7, request(), true);
  assert.deepEqual(raw.resets(7), [ErrorCode.RefusedStream]);
  await refuses(
    raw,
    encodeHeaderFrames(8, encodeHeaders(request()), true, 16_384),
    ErrorCode.ProtocolError,
  );
});

test("trailers must end the stream", async () => {
  const raw = new Raw();
  await raw.open();
  await raw.register();
  await raw.headers(3, request([], "POST"), false);
  await refuses(
    raw,
    encodeHeaderFrames(3, encodeHeaders([["x-t", "1"]]), false, 16_384),
    ErrorCode.ProtocolError,
  );
});

test("fields past the header list limit get a 431", async () => {
  const raw = new Raw();
  await raw.open();
  await raw.register();
  await raw.headers(3, request([["x-big", "a".repeat(70_000)]]), true);
  assert.deepEqual(raw.statuses(3), ["431"]);
});

test("the control stream: malformed messages, its end, its reset and ignored messages", async () => {
  {
    const raw = new Raw();
    await raw.open();
    await raw.headers(1, CONTROL, false);
    await raw.feed(encodeData(1, hex(EDGE.finish)));
    assert.equal(raw.session.phase, "registering");
    await raw.feed(encodeData(1, hex("00000000ffffff7f")));
    assert.equal(raw.session.phase, "closed");
    assert.match(raw.session.error, /malformed control message/);
    assert.deepEqual(raw.resets(1), [ErrorCode.ProtocolError]);
  }
  {
    const raw = new Raw();
    await raw.open();
    await raw.register();
    await raw.feed(encodeData(1, new Uint8Array(), true));
    assert.equal(raw.session.phase, "closed");
    assert.equal(raw.session.error, "the Cloudflare edge closed the control stream");
  }
  {
    const raw = new Raw();
    await raw.open();
    await raw.register();
    await raw.feed(encodeRst(1, ErrorCode.Cancel));
    assert.equal(raw.session.phase, "closed");
    assert.equal(raw.session.error, "the Cloudflare edge reset the control stream");
  }
});

test("a configuration push past its limit is reset", async () => {
  const raw = new Raw();
  await raw.open();
  await raw.register();
  await raw.headers(
    3,
    request([["cf-cloudflared-proxy-connection-upgrade", "update-configuration"]], "POST"),
    false,
  );
  await raw.feed(body(3, 1024 * 1024 + 1));
  assert.deepEqual(raw.resets(3), [ErrorCode.EnhanceYourCalm]);
});

test("request bodies past the limits get a 413 or a 503", async () => {
  const raw = new Raw();
  await raw.open();
  await raw.register();
  await raw.headers(3, request([], "POST"), false);
  await bigBody(raw, 3, MAX_REQUEST_BODY + 1);
  assert.deepEqual(raw.statuses(3), ["413"]);
  // Data after the answer is dropped.
  await raw.feed(encodeData(3, new Uint8Array(10), true));
  // Two bodies under the limit each still fill the session.
  await raw.headers(5, request([], "POST"), false);
  await raw.headers(7, request([], "POST"), false);
  await bigBody(raw, 5, MAX_REQUEST_BODY - 1024 * 1024);
  await bigBody(raw, 7, MAX_REQUEST_BODY - 1024 * 1024);
  assert.deepEqual(raw.statuses(5), []);
  assert.deepEqual(raw.statuses(7), []);
  await raw.headers(9, request([], "POST"), false);
  await bigBody(raw, 9, 3 * 1024 * 1024);
  assert.deepEqual(raw.statuses(9), ["503"]);
});

/**
 * Opens a WebSocket on stream 3.
 *
 * @param raw - The harness.
 * @returns The library's socket.
 */
async function socket(raw: Raw): Promise<import("./fake.ts").FakeSocket> {
  await raw.open();
  await raw.register();
  await raw.headers(3, UPGRADE, false);
  assert.deepEqual(raw.statuses(3), ["200"]);
  const live = raw.library.sockets.get("live0");
  assert.ok(live);
  return live;
}

/**
 * A library whose upgrade waits until released.
 *
 * @returns The library, a promise that resolves once it is asked, and the
 *   release.
 */
function gated(): { library: FakeLibrary; called: Promise<void>; release: () => void } {
  const library = new FakeLibrary(() => new Response("ok"));
  let release: () => void = () => {};
  let asked: () => void = () => {};
  const called = new Promise<void>((resolve) => (asked = resolve));
  const accepted = new Promise<void>((resolve) => (release = resolve));
  library.upgrades = async () => {
    asked();
    await accepted;
    return { protocol: "" };
  };
  return { library, called, release: () => release() };
}

test("client bytes sent while the library accepts are read after it", async () => {
  const { library, called, release } = gated();
  const raw = new Raw(library);
  await raw.open();
  await raw.register();
  const opening = raw.headers(3, UPGRADE, false);
  await called;
  // The library speaks before its accept reaches the session.
  assert.equal(await raw.session.fromLibrary(raw.io, 3, "early"), true);
  assert.equal(
    await raw.session.fromLibrary(raw.io, 3, new ArrayBuffer(MAX_SOCKET_BACKLOG + 1)),
    false,
  );
  const pending = raw.feed(encodeData(3, clientFrame(Opcode.Text, "hi")));
  release();
  await opening;
  await pending;
  assert.deepEqual(library.sockets.get("live0")?.sent, ["hi"]);
  assert.deepEqual(
    serverFrames(raw.data(3)).map((f) => f.opcode),
    [Opcode.Text],
  );
});

test("a stream reset or ended while the library accepts closes the socket", async () => {
  for (const end of [encodeRst(3, ErrorCode.Cancel), encodeData(3, new Uint8Array(), true)]) {
    const { library, called, release } = gated();
    const raw = new Raw(library);
    await raw.open();
    await raw.register();
    const opening = raw.headers(3, UPGRADE, false);
    await called;
    const ending = raw.feed(end);
    release();
    await opening;
    await ending;
    assert.equal(library.sockets.get("live0")?.closed?.code, CloseCode.GoingAway);
  }
});

test("socket bytes: protocol errors, failed sends, pings and pongs, and the client's end", async () => {
  {
    const raw = new Raw();
    const live = await socket(raw);
    // An unmasked frame breaks RFC 6455.
    await raw.feed(encodeData(3, new Uint8Array([0x81, 0x01, 0x61])));
    assert.equal(live.closed?.code, CloseCode.ProtocolError);
  }
  {
    const raw = new Raw();
    const live = await socket(raw);
    await raw.feed(
      encodeData(
        3,
        join(
          clientFrame(Opcode.Ping, "p"),
          clientFrame(Opcode.Pong, "q"),
          clientFrame(Opcode.Text, "a"),
        ),
      ),
    );
    assert.deepEqual(live.sent, ["a"]);
    live.failing = true;
    await raw.feed(encodeData(3, clientFrame(Opcode.Binary, "b")));
    const frames = serverFrames(raw.data(3));
    assert.deepEqual(
      frames.map((f) => f.opcode),
      [Opcode.Pong, Opcode.Close],
    );
  }
  {
    const raw = new Raw();
    const live = await socket(raw);
    await raw.feed(encodeData(3, new Uint8Array(), true));
    assert.equal(live.closed?.code, CloseCode.GoingAway);
  }
  {
    const raw = new Raw();
    const live = await socket(raw);
    // Trailers end a socket as the end of its stream does.
    await raw.feed(encodeHeaderFrames(3, encodeHeaders([["x", "1"]]), true, 16_384));
    assert.equal(live.closed?.code, CloseCode.GoingAway);
  }
  {
    const raw = new Raw();
    const live = await socket(raw);
    // The edge resets the stream: the library hears the client left.
    await raw.feed(encodeRst(3, ErrorCode.Cancel));
    assert.equal(live.closed?.code, CloseCode.GoingAway);
  }
  {
    const raw = new Raw();
    const live = await socket(raw);
    await raw.feed(encodeWindowUpdate(3, MAX_WINDOW));
    assert.deepEqual(raw.resets(3), [ErrorCode.FlowControlError]);
    assert.equal(live.closed?.code, CloseCode.GoingAway);
  }
});

test("a client that pings but never reads is closed", async () => {
  const raw = new Raw();
  const live = await socket(raw);
  const ping = clientFrame(Opcode.Ping, new Uint8Array(125));
  const frame = join(...Array.from({ length: 120 }, () => ping));
  for (let i = 0; i * frame.length * 2 < MAX_SOCKET_BACKLOG * 3 && live.closed === null; i++) {
    await raw.feed(encodeData(3, frame));
  }
  assert.equal(live.closed?.code, CloseCode.PolicyViolation);
});

test("the library's messages and closes on a socket", async () => {
  {
    const raw = new Raw();
    await socket(raw);
    // A library that outruns the client's window is cut off.
    assert.equal(
      await raw.session.fromLibrary(raw.io, 3, new ArrayBuffer(MAX_SOCKET_BACKLOG)),
      true,
    );
    assert.equal(await raw.session.fromLibrary(raw.io, 3, "late"), false);
  }
  for (const [code, sent] of [
    [1005, null],
    [1006, CloseCode.InternalError],
  ] as const) {
    const raw = new Raw();
    await socket(raw);
    await raw.session.libraryClosed(raw.io, 3, code, "x");
    await raw.feed(encodeWindowUpdate(3, 1 << 20), encodeWindowUpdate(0, 1 << 20));
    const close = serverFrames(raw.data(3)).at(-1);
    assert.equal(close?.opcode, Opcode.Close);
    assert.equal(
      close.payload.length === 0
        ? null
        : new DataView(close.payload.buffer, close.payload.byteOffset).getUint16(0),
      sent,
    );
  }
});

test("frames the session ignores", async () => {
  const raw = new Raw();
  await raw.open();
  await raw.feed(encodeFrame(FrameType.Settings, Flag.Ack, 0, new Uint8Array()));
  assert.equal(raw.session.phase, "connecting");
});
