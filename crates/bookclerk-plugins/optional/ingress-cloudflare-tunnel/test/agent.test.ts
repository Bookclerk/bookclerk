/**
 * The session against Node's HTTP/2 client playing the Cloudflare edge:
 * registration, proxied requests, configuration pushes, pings and
 * WebSockets.
 */

import assert from "node:assert/strict";
import http2 from "node:http2";
import { test } from "node:test";

import { MessageFramer, readRoot } from "../src/capnp.ts";
import { encodeBootstrap, encodeRegisterConnection } from "../src/rpc.ts";
import {
  EDGE,
  Edge,
  FakeLibrary,
  REGISTRATION,
  clientFrame,
  deserializeFields,
  hex,
  rangedResource,
  readResponse,
  serverFrames,
  text,
} from "./fake.ts";

test("the session registers on the control stream and serves a request", async () => {
  const library = new FakeLibrary(() => new Response("hello", { headers: { "x-book": "1" } }));
  const edge = new Edge(library);
  try {
    const { messages } = await edge.register();
    assert.deepEqual(messages[0], [encodeBootstrap().subarray(8)]);
    assert.deepEqual(messages[1], [encodeRegisterConnection(REGISTRATION).subarray(8)]);
    assert.equal(edge.session.phase, "serving");
    assert.equal(edge.session.location, "SJC");
    const response = await edge.request({
      ":path": "/book?id=1",
      "cf-connecting-ip": "203.0.113.9",
    });
    assert.equal(response.status, 200);
    assert.equal(text(response.body), "hello");
    assert.ok(
      response.fields.some(([n, v]) => n === "X-Book" && v === "1"),
      JSON.stringify(response.fields),
    );
    assert.equal(library.seen[0]?.url, "https://books.example.com/book?id=1");
    assert.equal(library.seen[0]?.clientAddress, "203.0.113.9");
    assert.deepEqual(edge.errors, []);
  } finally {
    edge.close();
  }
});

test("a large ranged body flows within the edge's windows", async () => {
  const body = new Uint8Array(3 * 1024 * 1024).map((_, i) => i % 251);
  const edge = new Edge(new FakeLibrary(rangedResource(body)));
  try {
    await edge.register();
    const response = await edge.request({ ":path": "/file" });
    assert.equal(response.status, 200);
    assert.equal(response.body.length, body.length);
    assert.deepEqual(response.body, body);
    assert.deepEqual(edge.errors, []);
  } finally {
    edge.close();
  }
});

test("a body larger than the buffer is fetched in ranges as the window opens", async () => {
  const body = new Uint8Array(6 * 1024 * 1024 + 123).map((_, i) => (i * 7) % 253);
  const library = new FakeLibrary(rangedResource(body, { etag: '"v1"' }));
  const edge = new Edge(library);
  try {
    await edge.register();
    const response = await edge.request({ ":path": "/big" });
    assert.equal(response.status, 200);
    assert.deepEqual(response.body, body);
    assert.equal(response.headers["content-length"], String(body.length));
    // The later pieces were ranged requests for the same resource.
    assert.ok(library.seen.length > 1);
    assert.equal(library.seen[1]?.headers.get("if-range"), '"v1"');
    // A ranged request is passed on, and its 206 comes back whole.
    const part = await edge.request({ ":path": "/big", range: "bytes=10-19" });
    assert.equal(part.status, 206);
    assert.deepEqual(part.body, body.subarray(10, 20));
  } finally {
    edge.close();
  }
});

test("request bodies, HEAD, trailers and the library's failures", async () => {
  const library = new FakeLibrary(async (request) => {
    if (request.url.endsWith("/fail")) throw new Error("down");
    if (request.method === "HEAD")
      return new Response(null, { headers: { "content-length": "42" } });
    if (request.url.endsWith("/empty")) return new Response(null, { status: 204 });
    return new Response(`got ${request.method} ${await request.text()}`);
  });
  const edge = new Edge(library);
  try {
    await edge.register();
    const posted = await edge.request({ ":method": "POST", ":path": "/p" }, "a body");
    assert.equal(text(posted.body), "got POST a body");
    const head = await edge.request({ ":method": "HEAD", ":path": "/h" });
    assert.equal(head.headers["content-length"], "42");
    assert.equal(head.body.length, 0);
    const empty = await edge.request({ ":path": "/empty" });
    assert.equal(empty.status, 204);
    const failed = await edge.request({ ":path": "/fail" });
    assert.equal(failed.status, 502);
    assert.match(String(failed.headers["cf-cloudflared-response-meta"]), /cloudflared/);
    // A body ended by trailers.
    const stream = edge.client.request(
      { ":method": "PUT", ":path": "/t" },
      { waitForTrailers: true },
    );
    stream.on("wantTrailers", () => stream.sendTrailers({ "x-sum": "1" }));
    stream.end("trailed");
    const trailed = await readResponse(stream);
    assert.equal(text(trailed.body), "got PUT trailed");
    assert.deepEqual(edge.errors, []);
  } finally {
    edge.close();
  }
});

test("before registration requests are refused, and TCP streams always are", async () => {
  const edge = new Edge(new FakeLibrary(() => new Response("never")));
  try {
    const early = await edge.request({ ":path": "/" });
    assert.equal(early.status, 503);
    await edge.register();
    const tcp = await edge.request({ ":path": "/", "cf-cloudflared-proxy-src": "warp" });
    assert.equal(tcp.status, 502);
    assert.equal(edge.library.seen.length, 0);
  } finally {
    edge.close();
  }
});

test("configuration pushes are acknowledged with their version", async () => {
  const edge = new Edge(new FakeLibrary(() => new Response("")));
  try {
    await edge.register();
    const push = await edge.request(
      {
        ":method": "PUT",
        ":path": "/config",
        "cf-cloudflared-proxy-connection-upgrade": "update-configuration",
      },
      JSON.stringify({ version: 7, config: { ingress: [] } }),
    );
    assert.equal(push.status, 200);
    assert.deepEqual(JSON.parse(text(push.body)), { lastAppliedVersion: 7, err: null });
    const bad = await edge.request(
      {
        ":method": "PUT",
        ":path": "/config",
        "cf-cloudflared-proxy-connection-upgrade": "update-configuration",
      },
      "not json",
    );
    assert.deepEqual(JSON.parse(text(bad.body)), { lastAppliedVersion: 0, err: null });
  } finally {
    edge.close();
  }
});

test("pings: the edge's are answered, and the connector's are matched to their answer", async () => {
  const edge = new Edge(new FakeLibrary(() => new Response("")));
  try {
    await edge.register();
    await new Promise<void>((resolve, reject) =>
      edge.client.ping((err) => (err ? reject(err) : resolve())),
    );
    const first = await edge.session.beat(edge.io, new Uint8Array(8).fill(1));
    await edge.settle();
    assert.equal(first, 1);
    assert.equal(edge.session.echoed, 1);
    const second = await edge.session.beat(edge.io, new Uint8Array(8).fill(2));
    await edge.settle();
    assert.equal(edge.session.echoed, second);
  } finally {
    edge.close();
  }
});

test("a refused or failed registration fails the session with the edge's reason", async () => {
  for (const [answer, message] of [
    [EDGE.refused, /too many connections/],
    [EDGE.exception, /no such tunnel/],
    [EDGE.unimplemented, /does not support/],
  ] as const) {
    const edge = new Edge(new FakeLibrary(() => new Response("")));
    try {
      await edge.register(answer);
      assert.equal(edge.session.phase, "failed");
      assert.match(edge.session.error, message);
    } finally {
      edge.close();
    }
  }
});

test("the edge's questions are answered unimplemented, and an abort closes the session", async () => {
  const edge = new Edge(new FakeLibrary(() => new Response("")));
  try {
    const { control, messages } = await edge.register();
    // Bootstrap, registration, then the Finish for the registration.
    const framer = new MessageFramer();
    const more = new Promise<Uint8Array[][]>((resolve) => {
      const got: Uint8Array[][] = [];
      control.on("data", (chunk: Buffer) => {
        got.push(...framer.push(new Uint8Array(chunk)));
        if (got.length >= 1) resolve(got);
      });
    });
    control.write(hex(EDGE.bootstrap4));
    const answers = await more;
    assert.deepEqual(readRoot(answers[0]!).struct(0)?.uint(0, 4), 4);
    assert.equal(messages.length >= 2, true);
    control.write(hex(EDGE.abort));
    await edge.settle();
    assert.equal(edge.session.phase, "closed");
    assert.match(edge.session.error, /shutting down/);
  } finally {
    edge.close();
  }
});

test("a WebSocket request opens a socket to the library that carries frames both ways", async () => {
  const library = new FakeLibrary(() => new Response(""));
  library.upgrades = () => ({ protocol: "chat" });
  const edge = new Edge(library);
  try {
    await edge.register();
    const stream = edge.client.request(
      {
        ":method": "GET",
        ":path": "/api/live",
        "cf-cloudflared-proxy-connection-upgrade": "websocket",
        "sec-websocket-version": "13",
        "sec-websocket-key": "dGhlIHNhbXBsZSBub25jZQ==",
        "sec-websocket-protocol": "chat",
      },
      { endStream: false },
    );
    const head = await new Promise<http2.IncomingHttpHeaders>((resolve) =>
      stream.on("response", resolve),
    );
    assert.equal(head[":status"], 200);
    const fields = deserializeFields(String(head["cf-cloudflared-response-headers"]));
    assert.deepEqual(fields, [
      ["Upgrade", "websocket"],
      ["Connection", "Upgrade"],
      ["Sec-Websocket-Accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="],
      ["Sec-Websocket-Protocol", "chat"],
    ]);
    const received: Buffer[] = [];
    stream.on("data", (chunk: Buffer) => received.push(chunk));
    stream.write(clientFrame(0x1, "hello"));
    await edge.settle();
    const socket = [...library.sockets.values()][0]!;
    assert.deepEqual(socket.sent, ["hello"]);
    assert.equal(await edge.session.fromLibrary(edge.io, socket.stream, "news"), true);
    await edge.session.fromLibrary(edge.io, socket.stream, new Uint8Array([1, 2]).buffer);
    await edge.settle();
    const frames = serverFrames(new Uint8Array(Buffer.concat(received)));
    assert.deepEqual(
      frames.map((f) => f.opcode),
      [0x1, 0x2],
    );
    // A ping is answered with a pong; the client's close reaches the library.
    stream.write(clientFrame(0x9, "p"));
    stream.write(clientFrame(0x8, new Uint8Array([0x03, 0xe8])));
    await edge.settle();
    assert.deepEqual(socket.closed, { code: 1000, reason: "" });
    const all = serverFrames(new Uint8Array(Buffer.concat(received)));
    assert.deepEqual(
      all.map((f) => f.opcode),
      [0x1, 0x2, 0xa, 0x8],
    );
    assert.equal(await edge.session.fromLibrary(edge.io, socket.stream, "late"), false);
  } finally {
    edge.close();
  }
});

test("the library's close reaches the client, and a client reset closes the library's socket", async () => {
  const library = new FakeLibrary(() => new Response(""));
  const edge = new Edge(library);
  const open = () =>
    edge.client.request(
      {
        ":method": "GET",
        ":path": "/api/live",
        "cf-cloudflared-proxy-connection-upgrade": "websocket",
        "sec-websocket-version": "13",
        "sec-websocket-key": "dGhlIHNhbXBsZSBub25jZQ==",
      },
      { endStream: false },
    );
  try {
    await edge.register();
    const first = open();
    const received: Buffer[] = [];
    first.on("data", (chunk: Buffer) => received.push(chunk));
    const ended = new Promise<void>((resolve) => first.on("end", resolve));
    await edge.settle();
    const socket = [...library.sockets.values()][0]!;
    await edge.session.libraryClosed(edge.io, socket.stream, 4000, "bye");
    await ended;
    const frames = serverFrames(new Uint8Array(Buffer.concat(received)));
    assert.equal(frames.at(-1)?.opcode, 0x8);

    const second = open();
    await edge.settle();
    const other = [...library.sockets.values()][1]!;
    second.close(http2.constants.NGHTTP2_CANCEL);
    await edge.settle();
    assert.deepEqual(other.closed, { code: 1001, reason: "the client went away" });

    // A refused upgrade passes the library's answer on.
    library.upgrades = () => new Response("no", { status: 403 });
    const refused = await readResponse(open());
    assert.equal(refused.status, 403);
    // A malformed handshake never reaches the library.
    const bad = await edge.request({
      "cf-cloudflared-proxy-connection-upgrade": "websocket",
      "sec-websocket-version": "8",
    });
    assert.equal(bad.status, 426);
  } finally {
    edge.close();
  }
});

test("the edge's GOAWAY closes the session", async () => {
  const edge = new Edge(new FakeLibrary(() => new Response("")));
  try {
    await edge.register();
    edge.client.goaway(http2.constants.NGHTTP2_NO_ERROR, 0, Buffer.from("maintenance"));
    await edge.settle();
    assert.equal(edge.session.phase, "closed");
    assert.match(edge.session.error, /maintenance/);
  } finally {
    edge.close();
  }
});
