/**
 * The agent session against a fake ngrok server: auth and bind, serving
 * proxied requests, flow control, large responses in ranged pieces,
 * heartbeats, server requests and protocol errors.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { MAX_SESSION_BYTES, MAX_STREAMS, MAX_WRITE_BATCH, Session } from "../src/agent.ts";
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
import {
  FakeLibrary,
  FakeNgrok,
  bytes,
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

test("a WebSocket upgrade is answered 501 for now", async () => {
  const library = plain();
  const { session, ngrok } = await serving(library);
  await session.receive(
    ngrok.io,
    proxy(2, "GET /api/live HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"),
  );
  assert.match(text(streamBytes(ngrok.take(), 2).bytes), /^HTTP\/1\.1 501 /);
  assert.equal(library.seen.length, 0);
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
