/**
 * The agent session against a fake ngrok server: auth and bind, serving
 * proxied requests, flow control, large responses in ranged pieces,
 * heartbeats, server requests and protocol errors.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MAX_SESSION_BYTES,
  MAX_SOCKET_BACKLOG,
  MAX_SOCKET_MESSAGE,
  MAX_STREAMS,
  MAX_WRITE_BATCH,
  Session,
} from "../src/agent.ts";
import { StreamType, typePrefix } from "../src/messages.ts";
import {
  ErrorCode,
  FLAG_FIN,
  FLAG_SYN,
  INITIAL_WINDOW,
  MuxError,
  concat,
  encodeData,
  encodeGoAway,
  encodeRst,
  type Frame,
} from "../src/mux.ts";
import { MAX_BUFFERED_RESPONSE, MAX_REQUEST_BODY } from "../src/serve.ts";
import { Opcode } from "../src/ws.ts";
import {
  FakeLibrary,
  FakeNgrok,
  bytes,
  clientFrame,
  serverFrames,
  grant,
  opened,
  proxy,
  rangedResource,
  reply,
  streamBytes,
  text,
} from "./fake.ts";

/** The domain these tests bind. */
const DOMAIN = "shelf.ngrok-free.app";

/**
 * A session that is bound and serving.
 *
 * @param library - The library.
 * @returns The session and the fake server.
 */
async function serving(library: FakeLibrary): Promise<{ session: Session; ngrok: FakeNgrok }> {
  const ngrok = new FakeNgrok(library);
  const session = new Session({ authtoken: "tok_secret", domain: DOMAIN, version: "0.1.0" });
  await session.start(ngrok.io);
  const auth = opened(ngrok.take(), StreamType.Auth);
  assert.ok(auth);
  await session.receive(ngrok.io, reply(auth.stream, { ClientId: "c1", Error: "" }));
  const bind = opened(ngrok.take(), StreamType.Bind);
  assert.ok(bind);
  await session.receive(
    ngrok.io,
    reply(bind.stream, { Id: "bookclerk", URL: `https://${DOMAIN}`, Error: "" }),
  );
  assert.equal(session.phase, "serving");
  ngrok.take();
  return { session, ngrok };
}

/**
 * A library answering every request with `body`.
 *
 * @param body - Response text.
 * @returns The library.
 */
function plain(body = "hello"): FakeLibrary {
  return new FakeLibrary(() => new Response(body, { headers: { "content-type": "text/plain" } }));
}

test("start authenticates, then binds the domain once ngrok accepts", async () => {
  const ngrok = new FakeNgrok(plain());
  const session = new Session({ authtoken: "tok_secret", domain: DOMAIN, version: "0.1.0" });
  await session.start(ngrok.io);
  const first = ngrok.take();
  const auth = opened(first, StreamType.Auth);
  assert.ok(auth);
  assert.equal(auth.stream, 3);
  const request = JSON.parse(auth.body) as { Version: string[]; Extra: Record<string, unknown> };
  assert.deepEqual(request.Version, ["3", "2"]);
  assert.equal(request.Extra["Authtoken"], "tok_secret");
  assert.equal(request.Extra["HeartbeatInterval"], 10_000_000_000);
  assert.equal(session.phase, "authenticating");

  // The reply may arrive in pieces.
  const authReply = reply(3, { ClientId: "c1", Error: "" });
  await session.receive(ngrok.io, authReply.subarray(0, 12));
  assert.equal(session.phase, "authenticating");
  await session.receive(ngrok.io, authReply.subarray(12));
  assert.equal(session.phase, "binding");
  const frames = ngrok.take();
  assert.ok(frames.some((f) => f.kind === "wndinc" && f.stream === 3));
  assert.ok(streamBytes(frames, 3).fin, "the auth stream is closed after its reply");
  const bind = opened(frames, StreamType.Bind);
  assert.ok(bind);
  assert.equal(bind.stream, 5);
  const bindBody = JSON.parse(bind.body) as { Proto: string; Opts: { Domain: string } };
  assert.equal(bindBody.Proto, "https");
  assert.equal(bindBody.Opts.Domain, DOMAIN);

  await session.receive(
    ngrok.io,
    reply(5, { Id: "bookclerk", URL: `https://${DOMAIN}`, Error: "" }),
  );
  assert.equal(session.phase, "serving");
  assert.equal(session.url, `https://${DOMAIN}`);
});

test("a refused authtoken or domain fails the session with ngrok's reason", async () => {
  const ngrok = new FakeNgrok(plain());
  const session = new Session({ authtoken: "bad", domain: DOMAIN, version: "0.1.0" });
  await session.start(ngrok.io);
  await session.receive(ngrok.io, reply(3, { Error: "ERR_NGROK_105 authtoken invalid" }));
  assert.equal(session.phase, "failed");
  assert.match(session.error, /ERR_NGROK_105/);
  assert.doesNotMatch(session.error, /bad/);

  const second = new FakeNgrok(plain());
  const other = new Session({ authtoken: "tok", domain: DOMAIN, version: "0.1.0" });
  await other.start(second.io);
  await other.receive(second.io, reply(3, { ClientId: "c", Error: "" }));
  await other.receive(second.io, reply(5, { Error: "ERR_NGROK_320 domain reserved elsewhere" }));
  assert.equal(other.phase, "failed");
  assert.match(other.error, /did not bind shelf\.ngrok-free\.app: ERR_NGROK_320/);

  const third = new FakeNgrok(plain());
  const broken = new Session({ authtoken: "tok", domain: DOMAIN, version: "0.1.0" });
  await broken.start(third.io);
  await broken.receive(third.io, encodeData(3, bytes("not json\n"), FLAG_FIN));
  assert.equal(broken.phase, "failed");
  assert.match(broken.error, /malformed auth reply/);
});

test("a proxied GET reaches the library and the answer closes the stream", async () => {
  const library = plain("hello");
  const { session, ngrok } = await serving(library);
  await session.receive(
    ngrok.io,
    proxy(
      2,
      "GET /api/status?x=1 HTTP/1.1\r\nHost: shelf.ngrok-free.app\r\nX-Forwarded-For: 1.2.3.4\r\nConnection: keep-alive\r\nCookie: a=b\r\n\r\n",
    ),
  );
  assert.equal(library.seen.length, 1);
  const seen = library.seen[0]!;
  assert.equal(seen.method, "GET");
  assert.equal(seen.url, `https://${DOMAIN}/api/status?x=1`);
  assert.equal(seen.clientAddress, "203.0.113.9");
  assert.equal(seen.headers.get("cookie"), "a=b");
  assert.equal(seen.headers.get("connection"), null);
  const frames = ngrok.take();
  assert.ok(frames.some((f) => f.kind === "wndinc" && f.stream === 2));
  const out = streamBytes(frames, 2);
  assert.ok(out.fin);
  const response = text(out.bytes);
  assert.match(response, /^HTTP\/1\.1 200 OK\r\n/);
  assert.match(response, /\r\ncontent-length: 5\r\n/);
  assert.match(response, /\r\nconnection: close\r\n\r\nhello$/);
  assert.equal(session.streamCount, 0);
});

test("a body arrives in pieces, by length or chunked, and reaches the library whole", async () => {
  const library = new FakeLibrary(async (request) => new Response(`got ${await request.text()}`));
  const { session, ngrok } = await serving(library);
  const head =
    "POST /api/x HTTP/1.1\r\nHost: h\r\nContent-Length: 11\r\nContent-Type: text/plain\r\n\r\nhello";
  await session.receive(ngrok.io, proxy(2, head));
  assert.equal(library.seen.length, 0);
  await session.receive(ngrok.io, encodeData(2, bytes(" world")));
  assert.equal(library.seen[0]?.body, "hello world");
  assert.match(text(streamBytes(ngrok.take(), 2).bytes), /got hello world$/);

  const chunked =
    "PUT /api/y HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n";
  await session.receive(ngrok.io, proxy(4, chunked));
  const cont = text(streamBytes(ngrok.take(), 4).bytes);
  assert.equal(cont, "HTTP/1.1 100 Continue\r\n\r\n");
  await session.receive(
    ngrok.io,
    encodeData(4, bytes("3\r\nabc\r\n4;ext=1\r\ndefg\r\n0\r\nX-T: 1\r\n\r\n")),
  );
  assert.equal(library.seen[1]?.body, "abcdefg");
  assert.equal(library.seen[1]?.headers.get("transfer-encoding"), null);
});

test("bad requests get an error answer without reaching the library", async () => {
  const library = plain();
  const { session, ngrok } = await serving(library);
  const cases: Array<[string, number]> = [
    ["GET http://evil/ HTTP/1.1\r\n\r\n", 400],
    ["GET / HTTP/2.0\r\n\r\n", 505],
    ["POST / HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n", 400],
    ["POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n", 501],
    [`POST / HTTP/1.1\r\nContent-Length: ${MAX_REQUEST_BODY + 1}\r\n\r\n`, 413],
    ["POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 400],
  ];
  let stream = 2;
  for (const [request, status] of cases) {
    await session.receive(ngrok.io, proxy(stream, request));
    const out = streamBytes(ngrok.take(), stream);
    assert.ok(out.fin, request);
    assert.match(text(out.bytes), new RegExp(`^HTTP/1\\.1 ${status} `), request);
    stream += 2;
  }
  assert.equal(library.seen.length, 0);
});

/** The RFC 6455 sample key. */
const KEY = "dGhlIHNhbXBsZSBub25jZQ==";

/** The accept value for {@link KEY}. */
const ACCEPT = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/**
 * A WebSocket upgrade request.
 *
 * @param extra - More header lines, each ending in CRLF.
 * @param fields - Handshake lines to use instead of the valid ones.
 * @returns The request text.
 */
function upgradeRequest(
  extra = "",
  fields = `Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: ${KEY}\r\n`,
): string {
  return `GET /api/live?x=1 HTTP/1.1\r\nHost: shelf.ngrok-free.app\r\nConnection: keep-alive, Upgrade\r\n${fields}Sec-WebSocket-Extensions: permessage-deflate\r\nSec-WebSocket-Protocol: bookclerk.v1, other\r\nCookie: s=1\r\n${extra}\r\n`;
}

/**
 * Splits a stream's bytes into the response head and the frames after it.
 *
 * @param data - The stream's bytes.
 * @returns The head text and the decoded frames.
 */
function headAndFrames(data: Uint8Array): {
  head: string;
  frames: ReturnType<typeof serverFrames>;
} {
  const raw = text(data);
  const end = raw.indexOf("\r\n\r\n");
  const headBytes = bytes(raw.slice(0, end + 4)).length;
  return { head: raw.slice(0, end + 4), frames: serverFrames(data.subarray(headBytes)) };
}

/**
 * A session with one WebSocket open on stream 2.
 *
 * @returns The session, the fake server and the library.
 */
async function socketOpen(): Promise<{
  session: Session;
  ngrok: FakeNgrok;
  library: FakeLibrary;
}> {
  const library = plain();
  library.upgrades = () => ({ protocol: "bookclerk.v1" });
  const { session, ngrok } = await serving(library);
  await session.receive(ngrok.io, proxy(2, upgradeRequest()));
  const { head } = headAndFrames(streamBytes(ngrok.take(), 2).bytes);
  assert.match(head, /^HTTP\/1\.1 101 /);
  return { session, ngrok, library };
}

test("an accepted upgrade answers 101 and carries frames both ways", async () => {
  const library = plain();
  library.upgrades = () => ({ protocol: "bookclerk.v1" });
  const { session, ngrok } = await serving(library);
  // The client's first frame may ride with the handshake.
  await session.receive(
    ngrok.io,
    proxy(2, concat(bytes(upgradeRequest()), clientFrame(Opcode.Text, "hello"))),
  );
  assert.equal(library.upgraded.length, 1);
  const seen = library.upgraded[0]!;
  assert.equal(seen.method, "GET");
  assert.equal(seen.url, `https://${DOMAIN}/api/live?x=1`);
  assert.equal(seen.clientAddress, "203.0.113.9");
  assert.equal(seen.headers.get("sec-websocket-protocol"), "bookclerk.v1, other");
  assert.equal(seen.headers.get("cookie"), "s=1");
  for (const name of [
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "upgrade",
    "connection",
  ]) {
    assert.equal(seen.headers.get(name), null, name);
  }
  const socket = library.sockets.get("live0")!;
  assert.equal(socket.stream, 2);
  assert.deepEqual(socket.sent, ["hello"]);
  const out = streamBytes(ngrok.take(), 2);
  assert.ok(!out.fin);
  const { head, frames } = headAndFrames(out.bytes);
  assert.equal(
    head,
    `HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: ${ACCEPT}\r\nsec-websocket-protocol: bookclerk.v1\r\n\r\n`,
  );
  assert.equal(frames.length, 0);

  // Binary, fragmented text and a ping in pieces; the ping is answered here.
  const ping = clientFrame(Opcode.Ping, "p1");
  await session.receive(
    ngrok.io,
    encodeData(
      2,
      concat(
        clientFrame(Opcode.Binary, new Uint8Array([1, 2, 3])),
        clientFrame(Opcode.Text, "fra", false),
        ping.subarray(0, 3),
      ),
    ),
  );
  await session.receive(
    ngrok.io,
    encodeData(2, concat(ping.subarray(3), clientFrame(Opcode.Continuation, "gment"))),
  );
  assert.deepEqual(socket.sent, ["hello", new Uint8Array([1, 2, 3]), "fragment"]);
  let reply = serverFrames(streamBytes(ngrok.take(), 2).bytes);
  assert.deepEqual(reply, [{ opcode: Opcode.Pong, payload: bytes("p1") }]);
  await session.receive(ngrok.io, encodeData(2, clientFrame(Opcode.Pong, "")));
  assert.equal(socket.sent.length, 3);

  // The library's messages go out as unmasked frames.
  assert.ok(await session.fromLibrary(ngrok.io, 2, "hi there"));
  assert.ok(await session.fromLibrary(ngrok.io, 2, new Uint8Array([9, 8]).buffer));
  const big = "x".repeat(70_000);
  assert.ok(await session.fromLibrary(ngrok.io, 2, big));
  reply = serverFrames(streamBytes(ngrok.take(), 2).bytes);
  assert.deepEqual(
    reply.map((f) => f.opcode),
    [Opcode.Text, Opcode.Binary, Opcode.Text],
  );
  assert.equal(text(reply[0]!.payload), "hi there");
  assert.deepEqual(reply[1]!.payload, new Uint8Array([9, 8]));
  assert.equal(text(reply[2]!.payload), big);

  // The client's close is echoed, passed to the library and ends the stream.
  const close = new Uint8Array([0x0f, 0xa0, ...bytes("bye")]);
  await session.receive(ngrok.io, encodeData(2, clientFrame(Opcode.Close, close)));
  assert.deepEqual(socket.closed, { code: 4000, reason: "bye" });
  const end = streamBytes(ngrok.take(), 2);
  assert.ok(end.fin);
  assert.deepEqual(serverFrames(end.bytes), [{ opcode: Opcode.Close, payload: close }]);
  assert.equal(session.streamCount, 0);
  assert.equal(await session.fromLibrary(ngrok.io, 2, "late"), false);
});

test("the library's close reaches the client; a close without a code stays without one", async () => {
  const { session, ngrok, library } = await socketOpen();
  await session.libraryClosed(ngrok.io, 2, 1001, "restarting");
  const out = streamBytes(ngrok.take(), 2);
  assert.ok(out.fin);
  const [frame] = serverFrames(out.bytes);
  assert.equal(frame!.opcode, Opcode.Close);
  assert.deepEqual(frame!.payload, new Uint8Array([0x03, 0xe9, ...bytes("restarting")]));
  assert.equal(library.sockets.get("live0")!.closed, null, "a closed socket is not closed again");
  // A later close or message for the stream does nothing.
  await session.libraryClosed(ngrok.io, 2, 1000, "");
  assert.equal(ngrok.take().length, 0);

  for (const [code, payload] of [
    [1005, new Uint8Array()],
    [1006, new Uint8Array([0x03, 0xf3, ...bytes("the library connection failed")])],
  ] as const) {
    const again = await socketOpen();
    await again.session.libraryClosed(again.ngrok.io, 2, code, "");
    const [sent] = serverFrames(streamBytes(again.ngrok.take(), 2).bytes);
    assert.deepEqual(sent!.payload, payload, String(code));
  }

  // A client close without a code closes the library with 1000.
  const plainClose = await socketOpen();
  await plainClose.session.receive(
    plainClose.ngrok.io,
    encodeData(2, clientFrame(Opcode.Close, new Uint8Array())),
  );
  assert.deepEqual(plainClose.library.sockets.get("live0")!.closed, { code: 1000, reason: "" });
});

test("the library's refusal and bad handshakes are answered without a socket", async () => {
  const library = plain();
  library.upgrades = (request) =>
    request.url.includes("/denied")
      ? new Response("sign in first", { status: 401, headers: { "www-authenticate": "Bearer" } })
      : { protocol: "" };
  const { session, ngrok } = await serving(library);
  await session.receive(ngrok.io, proxy(2, upgradeRequest().replace("/api/live", "/denied")));
  let out = streamBytes(ngrok.take(), 2);
  assert.ok(out.fin);
  assert.match(
    text(out.bytes),
    /^HTTP\/1\.1 401 [^]*www-authenticate: Bearer\r\n[^]*\r\n\r\nsign in first$/,
  );
  assert.equal(library.sockets.size, 0);

  const cases: Array<[string, number]> = [
    [upgradeRequest("", "Upgrade: h2c\r\n"), 501],
    [
      upgradeRequest(
        "",
        `Upgrade: websocket\r\nSec-WebSocket-Version: 8\r\nSec-WebSocket-Key: ${KEY}\r\n`,
      ),
      426,
    ],
    [
      upgradeRequest(
        "",
        "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: short\r\n",
      ),
      400,
    ],
    [upgradeRequest("", "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n"), 400],
    [upgradeRequest().replace("GET ", "POST "), 400],
    [upgradeRequest("Content-Length: 2\r\n") + "xy", 400],
  ];
  let stream = 4;
  for (const [request, status] of cases) {
    await session.receive(ngrok.io, proxy(stream, request));
    out = streamBytes(ngrok.take(), stream);
    assert.ok(out.fin, request);
    assert.match(text(out.bytes), new RegExp(`^HTTP/1\\.1 ${status} `), request);
    if (status === 426) assert.match(text(out.bytes), /sec-websocket-version: 13\r\n/);
    stream += 2;
  }
  assert.equal(library.upgraded.length, 1);

  library.upgrades = () => {
    throw new Error("down");
  };
  await session.receive(ngrok.io, proxy(stream, upgradeRequest()));
  assert.match(text(streamBytes(ngrok.take(), stream).bytes), /^HTTP\/1\.1 502 /);
  // A client that ends its side with the handshake gets no socket.
  await session.receive(ngrok.io, proxy(stream + 2, upgradeRequest(), undefined, true));
  assert.ok(ngrok.take().some((f) => f.kind === "rst" && f.stream === stream + 2));
  assert.equal(library.upgraded.length, 2);
});

test("broken client frames close both sides with the protocol's code", async () => {
  const cases: Array<[Uint8Array, number]> = [
    [new Uint8Array([0x81, 0x02, 0x68, 0x69]), 1002], // unmasked
    [clientFrame(Opcode.Text, new Uint8Array([0xff])), 1007],
    [clientFrame(Opcode.Binary, new Uint8Array(MAX_SOCKET_MESSAGE + 1)).subarray(0, 14), 1009],
    [clientFrame(3, "x"), 1002],
  ];
  for (const [frame, code] of cases) {
    const { session, ngrok, library } = await socketOpen();
    await session.receive(ngrok.io, encodeData(2, frame));
    const socket = library.sockets.get("live0")!;
    assert.equal(socket.closed?.code, code);
    const out = streamBytes(ngrok.take(), 2);
    assert.ok(out.fin);
    const [close] = serverFrames(out.bytes);
    assert.equal(close!.opcode, Opcode.Close);
    assert.equal((close!.payload[0]! << 8) | close!.payload[1]!, code);
    assert.equal(socket.sent.length, 0);
  }
});

test("a client that goes away, or a stream ngrok resets, closes the library's socket", async () => {
  const gone = await socketOpen();
  await gone.session.receive(gone.ngrok.io, encodeData(2, new Uint8Array(), FLAG_FIN));
  assert.deepEqual(gone.library.sockets.get("live0")!.closed, {
    code: 1001,
    reason: "the client went away",
  });
  assert.ok(streamBytes(gone.ngrok.take(), 2).fin);
  assert.equal(gone.session.streamCount, 0);

  const reset = await socketOpen();
  await reset.session.receive(reset.ngrok.io, encodeRst(2, ErrorCode.StreamCancelled));
  assert.equal(reset.library.sockets.get("live0")!.closed?.code, 1001);
  assert.equal(reset.session.streamCount, 0);

  // A library socket that can no longer be written ends the client's side.
  const failing = await socketOpen();
  failing.library.sockets.get("live0")!.failing = true;
  await failing.session.receive(failing.ngrok.io, encodeData(2, clientFrame(Opcode.Text, "x")));
  const [close] = serverFrames(streamBytes(failing.ngrok.take(), 2).bytes);
  assert.equal((close!.payload[0]! << 8) | close!.payload[1]!, 1001);
});

test("a client that stops reading is closed once its backlog passes the cap", async () => {
  const { session, ngrok, library } = await socketOpen();
  const message = new Uint8Array(MAX_SOCKET_MESSAGE - 16);
  let sent = 0;
  while (library.sockets.get("live0")!.closed === null) {
    assert.ok(await session.fromLibrary(ngrok.io, 2, message.slice().buffer));
    sent += message.length;
    assert.ok(sent < MAX_SOCKET_BACKLOG + INITIAL_WINDOW + 2 * message.length);
  }
  assert.deepEqual(library.sockets.get("live0")!.closed, {
    code: 1008,
    reason: "the client is not reading",
  });
  // The backlog and the close go out as the window opens, then FIN.
  await session.receive(ngrok.io, grant(2, 64 * 1024 * 1024));
  const out = streamBytes(ngrok.take(), 2);
  assert.ok(out.fin);
  const frames = serverFrames(out.bytes);
  assert.equal(frames.at(-1)!.opcode, Opcode.Close);
  assert.equal(frames.length, sent / message.length + 1);
});

test("a client that sends pings but never reads is closed", async () => {
  const { session, ngrok, library } = await socketOpen();
  const ping = clientFrame(Opcode.Ping, new Uint8Array(125));
  const batch = concat(...Array.from({ length: 1000 }, () => ping));
  for (let i = 0; i < 60 && library.sockets.get("live0")!.closed === null; i++) {
    await session.receive(ngrok.io, encodeData(2, batch));
  }
  assert.deepEqual(library.sockets.get("live0")!.closed, {
    code: 1008,
    reason: "the client is not reading",
  });
});

test("library messages that arrive before its accept are held for the client", async () => {
  const library = plain();
  let session!: Session;
  let ngrok!: FakeNgrok;
  library.upgrades = async (_request, stream) => {
    assert.ok(await session.fromLibrary(ngrok.io, stream, "early"));
    assert.equal(
      await session.fromLibrary(ngrok.io, stream, new Uint8Array(MAX_SOCKET_BACKLOG + 1).buffer),
      false,
    );
    return { protocol: "" };
  };
  ({ session, ngrok } = await serving(library));
  await session.receive(ngrok.io, proxy(2, upgradeRequest()));
  const { head, frames } = headAndFrames(streamBytes(ngrok.take(), 2).bytes);
  assert.match(head, /^HTTP\/1\.1 101 /);
  assert.doesNotMatch(head, /sec-websocket-protocol/);
  assert.deepEqual(frames, [{ opcode: Opcode.Text, payload: bytes("early") }]);
});

test("a socket the library accepts after its stream was reset is closed", async () => {
  const library = plain();
  let session!: Session;
  let ngrok!: FakeNgrok;
  library.upgrades = async () => {
    // The session ends while the library answers.
    await session.receive(ngrok.io, encodeGoAway(0, ErrorCode.NoError, "bye"));
    return { protocol: "" };
  };
  ({ session, ngrok } = await serving(library));
  await session.receive(ngrok.io, proxy(2, upgradeRequest()));
  assert.equal(library.sockets.get("live0")!.closed?.code, 1001);
});

test("a library failure is a 502, HEAD and 304 carry no body", async () => {
  const library = new FakeLibrary((request) => {
    if (request.url.endsWith("/fail")) throw new Error("down");
    if (request.url.endsWith("/cached"))
      return new Response(null, { status: 304, headers: { etag: '"1"' } });
    return new Response("abcdef", { headers: { "content-length": "6", "set-cookie": "a=1" } });
  });
  const { session, ngrok } = await serving(library);
  await session.receive(ngrok.io, proxy(2, "GET /fail HTTP/1.1\r\n\r\n"));
  assert.match(text(streamBytes(ngrok.take(), 2).bytes), /^HTTP\/1\.1 502 /);
  await session.receive(ngrok.io, proxy(4, "HEAD /x HTTP/1.1\r\n\r\n"));
  const head = text(streamBytes(ngrok.take(), 4).bytes);
  assert.match(head, /content-length: 6\r\n/);
  assert.match(head, /set-cookie: a=1\r\n/);
  assert.ok(head.endsWith("\r\n\r\n"));
  await session.receive(ngrok.io, proxy(6, "GET /cached HTTP/1.1\r\n\r\n"));
  const cached = text(streamBytes(ngrok.take(), 6).bytes);
  assert.match(cached, /^HTTP\/1\.1 304 /);
  assert.doesNotMatch(cached, /content-length/);
});

test("a response never passes the window ngrok granted", async () => {
  const body = new Uint8Array(INITIAL_WINDOW + 100_000).map((_, i) => i % 251);
  const library = new FakeLibrary(() => new Response(body.slice()));
  const { session, ngrok } = await serving(library);
  await session.receive(ngrok.io, proxy(2, "GET /big HTTP/1.1\r\n\r\n"));
  let frames = ngrok.take();
  let out = streamBytes(frames, 2);
  assert.equal(out.bytes.length, INITIAL_WINDOW);
  assert.ok(!out.fin);
  for (const write of ngrok.writes) assert.ok(write.length <= MAX_WRITE_BATCH);
  await session.receive(ngrok.io, grant(2, 50_000));
  frames = ngrok.take();
  assert.equal(streamBytes(frames, 2).bytes.length, 50_000);
  await session.receive(ngrok.io, grant(2, 1_000_000));
  frames = ngrok.take();
  out = streamBytes(frames, 2);
  assert.ok(out.fin);
  assert.equal(session.streamCount, 0);
});

test("a large ranged response is sent in pieces fetched as the window opens", async () => {
  const resource = new Uint8Array(MAX_BUFFERED_RESPONSE + 3 * 1024 * 1024 + 17).map(
    (_, i) => (i * 7) % 256,
  );
  const library = new FakeLibrary(rangedResource(resource, { etag: '"v1"' }));
  const { session, ngrok } = await serving(library);
  await session.receive(
    ngrok.io,
    proxy(2, "GET /api/playback/t/0 HTTP/1.1\r\nAuthorization: Bearer x\r\n\r\n"),
  );
  const received: Uint8Array[] = [];
  let fin = false;
  let guard = 0;
  while (!fin && guard++ < 1000) {
    const out = streamBytes(ngrok.take(), 2);
    received.push(out.bytes);
    fin = out.fin;
    if (!fin) await session.receive(ngrok.io, grant(2, out.bytes.length || INITIAL_WINDOW));
  }
  assert.ok(fin);
  const all = concat(...received);
  const split = text(all.subarray(0, 400)).indexOf("\r\n\r\n") + 4;
  const head = text(all.subarray(0, split));
  assert.match(head, /^HTTP\/1\.1 200 OK\r\n/);
  assert.match(head, new RegExp(`content-length: ${resource.length}\\r\\n`));
  assert.deepEqual(all.subarray(split), resource);
  const ranged = library.seen.slice(1);
  assert.ok(ranged.length >= 3);
  for (const seen of ranged) {
    assert.match(seen.headers.get("range") ?? "", /^bytes=\d+-\d+$/);
    assert.equal(seen.headers.get("if-range"), '"v1"');
    assert.equal(seen.headers.get("authorization"), "Bearer x");
  }
});

test("a client range is continued within its own span", async () => {
  const resource = new Uint8Array(MAX_BUFFERED_RESPONSE * 2).map((_, i) => i % 256);
  const library = new FakeLibrary(rangedResource(resource));
  const { session, ngrok } = await serving(library);
  const start = 1000;
  await session.receive(ngrok.io, proxy(2, `GET /t HTTP/1.1\r\nRange: bytes=${start}-\r\n\r\n`));
  const received: Uint8Array[] = [];
  let fin = false;
  while (!fin) {
    const out = streamBytes(ngrok.take(), 2);
    received.push(out.bytes);
    fin = out.fin;
    if (!fin) await session.receive(ngrok.io, grant(2, INITIAL_WINDOW));
  }
  const all = concat(...received);
  const split = text(all.subarray(0, 400)).indexOf("\r\n\r\n") + 4;
  assert.match(text(all.subarray(0, split)), /^HTTP\/1\.1 206 /);
  assert.deepEqual(all.subarray(split), resource.subarray(start));
});

test("a large response that can't be continued is a 502; a changed resource resets the stream", async () => {
  const huge = new Uint8Array(MAX_BUFFERED_RESPONSE + 10);
  const library = new FakeLibrary(() => new Response(huge.slice()));
  const { session, ngrok } = await serving(library);
  await session.receive(ngrok.io, proxy(2, "GET /x HTTP/1.1\r\n\r\n"));
  assert.match(text(streamBytes(ngrok.take(), 2).bytes), /^HTTP\/1\.1 502 /);

  const resource = new Uint8Array(MAX_BUFFERED_RESPONSE + 2 * 1024 * 1024);
  let calls = 0;
  const changing = new FakeLibrary((request) => {
    calls += 1;
    if (calls === 1) return rangedResource(resource)(request);
    return new Response("changed", { status: 200 });
  });
  const second = await serving(changing);
  await second.session.receive(second.ngrok.io, proxy(2, "GET /t HTTP/1.1\r\n\r\n"));
  let frames: Frame[] = second.ngrok.take();
  let guard = 0;
  while (!frames.some((f) => f.kind === "rst" && f.stream === 2) && guard++ < 100) {
    await second.session.receive(second.ngrok.io, grant(2, INITIAL_WINDOW));
    frames = second.ngrok.take();
  }
  assert.ok(
    frames.some((f) => f.kind === "rst" && f.stream === 2 && f.code === ErrorCode.StreamCancelled),
  );
  assert.equal(second.session.streamCount, 0);
});

test("heartbeats: ngrok's are echoed, the agent's own are matched", async () => {
  const { session, ngrok } = await serving(plain());
  await session.receive(
    ngrok.io,
    encodeData(2, concat(typePrefix(StreamType.Heartbeat), bytes("abcdef")), FLAG_SYN),
  );
  assert.equal(text(streamBytes(ngrok.take(), 2).bytes), "abcd");
  await session.receive(ngrok.io, encodeData(2, bytes("gh")));
  assert.equal(text(streamBytes(ngrok.take(), 2).bytes), "efgh");
  await session.receive(ngrok.io, encodeData(2, new Uint8Array(), FLAG_FIN));
  assert.ok(streamBytes(ngrok.take(), 2).fin);

  const sentAt = await session.beat(ngrok.io, bytes("1234"));
  const beat = opened(ngrok.take(), StreamType.Heartbeat);
  assert.ok(beat);
  assert.equal(beat.body, "1234");
  assert.equal(session.lastEcho, 0);
  await session.receive(ngrok.io, encodeData(beat.stream, bytes("9999")));
  assert.equal(session.lastEcho, 0, "a wrong echo does not count");
  await session.receive(ngrok.io, encodeData(beat.stream, bytes("1234")));
  assert.ok(session.lastEcho >= sentAt);
  await session.beat(ngrok.io, bytes("5678"));
  const again = streamBytes(ngrok.take(), beat.stream);
  assert.equal(text(again.bytes), "5678");
  await session.receive(ngrok.io, encodeRst(beat.stream, ErrorCode.StreamClosed));
  await session.beat(ngrok.io, bytes("0000"));
  assert.ok(opened(ngrok.take(), StreamType.Heartbeat), "a reset beat stream is reopened");
});

test("restart, stop and update are declined; stop-tunnel ends the endpoint", async () => {
  const { session, ngrok } = await serving(plain());
  for (const [stream, type] of [
    [2, StreamType.Restart],
    [4, StreamType.Stop],
    [6, StreamType.Update],
  ] as const) {
    await session.receive(
      ngrok.io,
      encodeData(stream, concat(typePrefix(type), bytes("{}")), FLAG_SYN),
    );
    const out = streamBytes(ngrok.take(), stream);
    assert.ok(out.fin);
    assert.deepEqual(JSON.parse(text(out.bytes)), {
      Error: "not supported by the Bookclerk agent",
    });
  }
  await session.receive(ngrok.io, encodeData(8, concat(typePrefix(42), bytes("{}")), FLAG_SYN));
  assert.ok(ngrok.take().some((f) => f.kind === "rst" && f.stream === 8));
  assert.equal(session.phase, "serving");
  await session.receive(
    ngrok.io,
    encodeData(
      10,
      concat(
        typePrefix(StreamType.StopTunnel),
        bytes('{"Id":"bookclerk","Message":"stopped from the dashboard"}'),
      ),
      FLAG_SYN,
    ),
  );
  assert.equal(session.phase, "failed");
  assert.match(session.error, /stopped from the dashboard/);
});

test("GOAWAY closes the session; protocol errors send GOAWAY and throw", async () => {
  const { session, ngrok } = await serving(plain());
  await session.receive(ngrok.io, encodeGoAway(0, ErrorCode.NoError, "maintenance"));
  assert.equal(session.phase, "closed");
  assert.match(session.error, /maintenance/);

  const second = await serving(plain());
  await assert.rejects(
    second.session.receive(second.ngrok.io, encodeData(3, bytes("x"), FLAG_SYN)),
    MuxError,
  );
  assert.equal(second.session.phase, "closed");
  assert.ok(
    second.ngrok.take().some((f) => f.kind === "goaway" && f.code === ErrorCode.ProtocolError),
  );

  const third = await serving(plain());
  const oversized = new Uint8Array([0xff, 0xff, 0xff, 0x10, 0, 0, 0, 2]);
  await assert.rejects(third.session.receive(third.ngrok.io, oversized), MuxError);
});

test("passthrough TLS and malformed proxy headers are reset; streams past the cap are refused", async () => {
  const { session, ngrok } = await serving(plain());
  const header = new Uint8Array(8);
  new DataView(header.buffer).setUint32(0, 2, true);
  await session.receive(
    ngrok.io,
    encodeData(2, concat(typePrefix(StreamType.Proxy), header, bytes("[]")), FLAG_SYN),
  );
  assert.ok(ngrok.take().some((f) => f.kind === "rst" && f.stream === 2));

  const json = bytes('{"Id":"b","ClientAddr":"1.2.3.4:1","Proto":"tls","PassthroughTLS":true}');
  const tls = new Uint8Array(8 + json.length);
  new DataView(tls.buffer).setUint32(0, json.length, true);
  tls.set(json, 8);
  await session.receive(
    ngrok.io,
    encodeData(4, concat(typePrefix(StreamType.Proxy), tls), FLAG_SYN),
  );
  assert.ok(ngrok.take().some((f) => f.kind === "rst" && f.stream === 4));

  // A request cut short by ngrok's FIN is reset.
  await session.receive(ngrok.io, proxy(6, "GET / HTTP/1.1\r\nHost: x", undefined, true));
  assert.ok(ngrok.take().some((f) => f.kind === "rst" && f.stream === 6));

  for (let i = 0; i < MAX_STREAMS; i++) {
    await session.receive(
      ngrok.io,
      encodeData(100 + 2 * i, typePrefix(StreamType.Proxy).subarray(0, 2), FLAG_SYN),
    );
  }
  ngrok.take();
  await session.receive(ngrok.io, proxy(100 + 2 * MAX_STREAMS, "GET / HTTP/1.1\r\n\r\n"));
  assert.ok(
    ngrok
      .take()
      .some(
        (f) =>
          f.kind === "rst" &&
          f.stream === 100 + 2 * MAX_STREAMS &&
          f.code === ErrorCode.StreamRefused,
      ),
  );
});

test("bytes held across streams are capped: past the cap a request is answered 503", async () => {
  const resource = new Uint8Array(MAX_BUFFERED_RESPONSE);
  const library = new FakeLibrary(() => new Response(resource.slice()));
  const { session, ngrok } = await serving(library);
  // Each answer waits for window it never gets, so its bytes stay held.
  let stream = 2;
  let busy = false;
  for (let i = 0; i < 2 * (MAX_SESSION_BYTES / MAX_BUFFERED_RESPONSE) && !busy; i++) {
    await session.receive(ngrok.io, proxy(stream, "POST /x HTTP/1.1\r\nContent-Length: 1\r\n\r\n"));
    await session.receive(ngrok.io, encodeData(stream, bytes("y")));
    busy = /^HTTP\/1\.1 503 /.test(text(streamBytes(ngrok.take(), stream).bytes));
    stream += 2;
  }
  assert.ok(busy);
});
