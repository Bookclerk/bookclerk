/**
 * Tests the SDK's `connect()` (framed tunnels, `startTls()`,
 * `secureTransport: "on"`) through the testkit's fake relay, and the frame
 * codec against the layout the launcher's relay
 * (`bookclerk-workerd::egress_starttls`) speaks.
 */
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const {
  RelayFrameDecoder,
  STARTTLS_HOST_SUFFIX,
  createConnect,
  encodeDataFrames,
  encodeStartTlsFrame,
  parseTcpAddress,
  MAX_TCP_FRAME_PAYLOAD,
} = await import(join(dist, "tcp.js"));
const { createTestConnect } = await import(join(dist, "testkit-tcp.js"));

const text = (s) => new TextEncoder().encode(s);
const decode = (b) => new TextDecoder().decode(b);

/**
 * Reads from `reader` until the text read ends with `suffix`.
 *
 * @param {ReadableStreamDefaultReader<Uint8Array>} reader - Where to read.
 * @param {string} suffix - Text that ends the read.
 * @returns {Promise<string>} Everything read.
 */
async function readUntil(reader, suffix) {
  let out = "";
  while (!out.endsWith(suffix)) {
    const { value, done } = await reader.read();
    if (done) throw new Error(`closed after ${JSON.stringify(out)}`);
    out += decode(value);
  }
  return out;
}

/**
 * A minimal SMTP server that upgrades on STARTTLS.
 *
 * @param {import("../dist/testkit-tcp.js").TestTcpConnection} conn - The server side.
 */
async function smtp(conn) {
  await conn.write("220 mail ready\r\n");
  assert.equal(await conn.readLine(), "EHLO test\r\n");
  await conn.write("250-mail\r\n250 STARTTLS\r\n");
  assert.equal(await conn.readLine(), "STARTTLS\r\n");
  assert.equal(conn.tls, false);
  await conn.write("220 go ahead\r\n");
  await conn.waitForTls();
  if (!conn.tls) return;
  assert.equal(await conn.readLine(), "EHLO test\r\n");
  await conn.write("250 secure\r\n");
}

let tests = 0;
async function test(name, fn) {
  await fn();
  tests += 1;
  console.log(`ok - ${name}`);
}

await test("a STARTTLS socket upgrades and carries plaintext both ways", async () => {
  const connect = createTestConnect({ serve: smtp });
  const plain = connect(
    { hostname: "mail.example.com", port: 587 },
    { secureTransport: "starttls" },
  );
  const info = await plain.opened;
  assert.equal(info.remoteAddress, "mail.example.com:587");
  const reader = plain.readable.getReader();
  const writer = plain.writable.getWriter();
  assert.equal(await readUntil(reader, "\r\n"), "220 mail ready\r\n");
  await writer.write(text("EHLO test\r\n"));
  assert.match(await readUntil(reader, "STARTTLS\r\n"), /250 STARTTLS/);
  await writer.write(text("STARTTLS\r\n"));
  assert.equal(await readUntil(reader, "\r\n"), "220 go ahead\r\n");
  reader.releaseLock();
  writer.releaseLock();
  const secure = plain.startTls();
  await secure.opened;
  const sw = secure.writable.getWriter();
  const sr = secure.readable.getReader();
  await sw.write(text("EHLO test\r\n"));
  assert.equal(await readUntil(sr, "\r\n"), "250 secure\r\n");
  assert.deepEqual(connect.dials, [{ hostname: "mail.example.com", port: 587, tunnel: "framed" }]);
  // The plain socket is spent.
  assert.throws(() => plain.startTls(), TypeError);
  assert.throws(() => secure.startTls(), TypeError);
  await assert.rejects(plain.writable.getWriter().write(text("x")), TypeError);
  await secure.close();
});

await test("secureTransport on upgrades before the first byte", async () => {
  const connect = createTestConnect({
    async serve(conn) {
      await conn.waitForTls();
      assert.equal(conn.tls, true);
      assert.equal(await conn.readLine(), "PING\n");
      await conn.write("PONG\n");
    },
  });
  const socket = connect("db.example.com:5432", { secureTransport: "on" });
  await socket.opened;
  await socket.writable.getWriter().write(text("PING\n"));
  assert.equal(await readUntil(socket.readable.getReader(), "\n"), "PONG\n");
  assert.throws(() => socket.startTls(), TypeError);
});

await test("a failed handshake rejects opened and errors the stream", async () => {
  const connect = createTestConnect({
    serve: smtp,
    tlsFailure: () => "invalid peer certificate: NotValidForName",
  });
  const plain = connect(
    { hostname: "mail.example.com", port: 587 },
    { secureTransport: "starttls" },
  );
  const reader = plain.readable.getReader();
  const writer = plain.writable.getWriter();
  await readUntil(reader, "\r\n");
  await writer.write(text("EHLO test\r\n"));
  await readUntil(reader, "STARTTLS\r\n");
  await writer.write(text("STARTTLS\r\n"));
  await readUntil(reader, "\r\n");
  reader.releaseLock();
  writer.releaseLock();
  const secure = plain.startTls();
  await assert.rejects(secure.opened, /TLS handshake failed: invalid peer certificate/);
  await assert.rejects(secure.readable.getReader().read(), /TLS handshake failed/);
});

await test("a refused grant rejects opened with network_not_granted", async () => {
  const connect = createTestConnect({ serve: smtp, allows: () => false });
  const socket = connect(
    { hostname: "mail.example.com", port: 25 },
    { secureTransport: "starttls" },
  );
  await assert.rejects(socket.opened, /network_not_granted/);
});

await test("a plain socket is not framed", async () => {
  const connect = createTestConnect({
    async serve(conn) {
      const got = await conn.read();
      await conn.write(got);
    },
  });
  const socket = connect({ hostname: "echo.example.com", port: 7 });
  await socket.writable.getWriter().write(text("raw"));
  assert.equal(await readUntil(socket.readable.getReader(), "raw"), "raw");
  assert.equal(connect.dials[0].tunnel, "off");
  assert.throws(() => socket.startTls(), TypeError);
});

await test("large writes split into bounded frames and arrive whole", async () => {
  const big = new Uint8Array(MAX_TCP_FRAME_PAYLOAD * 3 + 17).map((_, i) => i % 251);
  let received = 0;
  const connect = createTestConnect({
    async serve(conn) {
      while (received < big.length) {
        const chunk = await conn.read();
        assert.ok(chunk.length <= MAX_TCP_FRAME_PAYLOAD);
        received += chunk.length;
      }
      await conn.write(big);
    },
  });
  const socket = connect(
    { hostname: "bulk.example.com", port: 9000 },
    { secureTransport: "starttls" },
  );
  await socket.writable.getWriter().write(big);
  const reader = socket.readable.getReader();
  let back = 0;
  while (back < big.length) {
    const { value, done } = await reader.read();
    assert.ok(!done);
    back += value.length;
  }
  assert.equal(received, big.length);
  assert.equal(back, big.length);
});

await test("bad addresses and IPv6 TLS targets are refused before dialing", async () => {
  const dials = [];
  const connect = createConnect((address) => {
    dials.push(address);
    throw new Error("unreachable");
  });
  assert.throws(() => connect("nohost"), TypeError);
  assert.throws(() => connect({ hostname: "a", port: 0 }), TypeError);
  assert.throws(() => connect({ hostname: "a", port: 70000 }), TypeError);
  assert.throws(
    () => connect({ hostname: "::1", port: 443 }, { secureTransport: "on" }),
    TypeError,
  );
  assert.throws(
    () => connect({ hostname: `x${STARTTLS_HOST_SUFFIX}`, port: 1 }, { secureTransport: "on" }),
    TypeError,
  );
  assert.throws(() => connect({ hostname: "a", port: 1 }, { secureTransport: "maybe" }), TypeError);
  assert.equal(dials.length, 0);
  assert.deepEqual(parseTcpAddress("[::1]:25"), { hostname: "::1", port: 25 });
});

await test("the frame codec matches the relay's layout", async () => {
  assert.deepEqual([...encodeStartTlsFrame()], [1, 0, 0]);
  assert.deepEqual([...encodeDataFrames(text("hi"))], [0, 0, 2, 104, 105]);
  assert.equal(encodeDataFrames(new Uint8Array(0)).length, 0);
  const d = new RelayFrameDecoder();
  d.push(Uint8Array.of(0, 0, 2, 104));
  assert.equal(d.next(), null);
  d.push(Uint8Array.of(105, 1, 0, 0, 2, 0, 2, 110, 111));
  assert.deepEqual(d.next(), { kind: "data", bytes: Uint8Array.of(104, 105) });
  assert.deepEqual(d.next(), { kind: "tlsReady" });
  assert.deepEqual(d.next(), { kind: "tlsFailed", reason: "no" });
  assert.equal(d.next(), null);
  for (const bad of [
    [0, 0, 0],
    [0, 0x40, 0x01],
    [1, 0, 1],
    [2, 0x02, 0x01],
    [3, 0, 0],
  ]) {
    const decoder = new RelayFrameDecoder();
    decoder.push(Uint8Array.from(bad));
    assert.throws(() => decoder.next(), TypeError, JSON.stringify(bad));
  }
});

await test("random frame streams decode like an independent reading", async () => {
  let seed = 7;
  const rand = (n) => {
    seed = (seed * 1103515245 + 12345) % 2 ** 31;
    return seed % n;
  };
  for (let round = 0; round < 3000; round++) {
    const bytes = Uint8Array.from({ length: rand(40) }, () =>
      rand(4) === 0 ? rand(256) : [0, 1, 2, 0, 0, 3][rand(6)],
    );
    // Reference: whole-buffer reading of the documented layout.
    const expected = [];
    let error = false;
    let at = 0;
    while (bytes.length - at >= 3) {
      const kind = bytes[at];
      const len = (bytes[at + 1] << 8) | bytes[at + 2];
      const ok =
        (kind === 0 && len >= 1 && len <= 16384) ||
        (kind === 1 && len === 0) ||
        (kind === 2 && len <= 512);
      if (!ok) {
        error = true;
        break;
      }
      if (bytes.length - at - 3 < len) break;
      expected.push([kind, [...bytes.slice(at + 3, at + 3 + len)]]);
      at += 3 + len;
    }
    const decoder = new RelayFrameDecoder();
    const got = [];
    let threw = false;
    const step = 1 + rand(5);
    outer: for (let i = 0; i < bytes.length; i += step) {
      decoder.push(bytes.slice(i, i + step));
      for (;;) {
        let frame;
        try {
          frame = decoder.next();
        } catch {
          threw = true;
          break outer;
        }
        if (!frame) break;
        if (frame.kind === "data") got.push([0, [...frame.bytes]]);
        else if (frame.kind === "tlsReady") got.push([1, []]);
        else got.push([2, [...text(frame.reason)]]);
      }
    }
    assert.equal(threw, error, JSON.stringify([...bytes]));
    // A failure reason is decoded as text, so compare kinds and data only.
    assert.deepEqual(
      got.map(([k, b]) => (k === 2 ? [2] : [k, b])),
      expected.map(([k, b]) => (k === 2 ? [2] : [k, b])),
    );
  }
});

console.log(`${tests} tcp tests passed`);
