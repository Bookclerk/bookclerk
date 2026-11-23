/**
 * RFC 6455 framing: the handshake's accept value, server frames, and the
 * decoder for client frames with each protocol error it must catch.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  FrameDecoder,
  MAX_CLOSE_REASON_BYTES,
  Opcode,
  WsError,
  acceptKey,
  closeReason,
  encodeClose,
  encodeFrame,
  isSendableCloseCode,
  validKey,
  type Message,
} from "../src/ws.ts";
import { bytes, clientFrame, serverFrames } from "./fake.ts";

test("the accept value is RFC 6455's, and keys must be 16 bytes of base64", async () => {
  assert.equal(await acceptKey("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
  assert.ok(validKey("dGhlIHNhbXBsZSBub25jZQ=="));
  assert.ok(validKey("AAAAAAAAAAAAAAAAAAAAAA=="));
  for (const key of [
    "",
    "dGhlIHNhbXBsZSBub25jZQ",
    "dGhlIHNhbXBsZSBub25jZR==",
    "dGhl IHNhbXBsZSBub25jZQ=",
    "dGhlIHNhbXBsZSBub25jZQ===",
  ]) {
    assert.ok(!validKey(key), key);
  }
});

test("server frames use the shortest length and no mask", () => {
  for (const size of [0, 125, 126, 65535, 65536]) {
    const payload = new Uint8Array(size).map((_, i) => i & 0xff);
    const frame = encodeFrame(Opcode.Binary, payload);
    assert.equal(frame[0], 0x82);
    assert.equal(frame.length, size + (size < 126 ? 2 : size < 65536 ? 4 : 10));
    assert.deepEqual(serverFrames(frame), [{ opcode: Opcode.Binary, payload }]);
  }
  assert.deepEqual(encodeClose(null), new Uint8Array([0x88, 0x00]));
  assert.deepEqual(encodeClose(1000, "ok"), new Uint8Array([0x88, 0x04, 0x03, 0xe8, 0x6f, 0x6b]));
  // A long reason is cut on a character boundary.
  const long = "é".repeat(100);
  assert.equal(new TextEncoder().encode(closeReason(long)).length, MAX_CLOSE_REASON_BYTES - 1);
  assert.equal(closeReason("short"), "short");
  assert.equal(encodeClose(4000, long).length, 2 + 2 + MAX_CLOSE_REASON_BYTES - 1);
});

test("sendable close codes are the wire codes and the registered ranges", () => {
  for (const code of [1000, 1003, 1007, 1011, 1014, 3000, 4999])
    assert.ok(isSendableCloseCode(code));
  for (const code of [999, 1004, 1005, 1006, 1015, 2999, 5000, 1000.5]) {
    assert.ok(!isSendableCloseCode(code), String(code));
  }
});

/**
 * Feeds bytes one at a time.
 *
 * @param decoder - The decoder.
 * @param data - The bytes.
 * @returns Every message.
 */
function byteByByte(decoder: FrameDecoder, data: Uint8Array): Message[] {
  const out: Message[] = [];
  for (let i = 0; i < data.length; i++) out.push(...decoder.push(data.subarray(i, i + 1)));
  return out;
}

test("the decoder reads messages whatever the chunking, and stops at a close", () => {
  const stream = new Uint8Array([
    ...clientFrame(Opcode.Text, "héllo"),
    ...clientFrame(Opcode.Binary, new Uint8Array(300).fill(7), false),
    ...clientFrame(Opcode.Ping, "p"),
    ...clientFrame(Opcode.Continuation, new Uint8Array(70_000).fill(8)),
    ...clientFrame(Opcode.Pong, ""),
    ...clientFrame(Opcode.Text, ""),
    ...clientFrame(Opcode.Close, new Uint8Array([0x03, 0xe8, ...bytes("bye")])),
    ...clientFrame(Opcode.Text, "after the close"),
  ]);
  const whole = new FrameDecoder(1 << 20).push(stream);
  const pieces = byteByByte(new FrameDecoder(1 << 20), stream);
  assert.deepEqual(pieces, whole);
  assert.deepEqual(
    whole.map((m) => m.kind),
    ["text", "ping", "binary", "pong", "text", "close"],
  );
  assert.deepEqual(whole[0], { kind: "text", text: "héllo" });
  assert.equal((whole[2] as { data: Uint8Array }).data.length, 70_300);
  assert.deepEqual(whole[5], { kind: "close", code: 1000, reason: "bye" });

  const decoder = new FrameDecoder(1024);
  const frame = clientFrame(Opcode.Binary, new Uint8Array(100));
  decoder.push(frame.subarray(0, 50));
  assert.equal(decoder.buffered, 100);
  assert.ok(!decoder.closed);
  decoder.push(frame.subarray(50));
  assert.equal(decoder.buffered, 0);
  decoder.push(new Uint8Array([0x81]));
  assert.equal(decoder.buffered, 1);
  const closing = new FrameDecoder(1024);
  assert.deepEqual(closing.push(clientFrame(Opcode.Close, new Uint8Array())), [
    { kind: "close", code: null, reason: "" },
  ]);
  assert.ok(closing.closed);
  assert.deepEqual(closing.push(clientFrame(Opcode.Text, "x")), []);
});

test("the decoder refuses what RFC 6455 forbids, with the code to close with", () => {
  const masked = (b0: number, b1: number, ...rest: number[]): Uint8Array =>
    new Uint8Array([b0, b1, ...rest]);
  const cases: Array<[string, Uint8Array, number, number?]> = [
    ["reserved bit", masked(0xc1, 0x80, 0, 0, 0, 0), 1002],
    ["unknown opcode", masked(0x83, 0x80, 0, 0, 0, 0), 1002],
    ["unmasked", masked(0x81, 0x00), 1002],
    ["16-bit length under 126", masked(0x82, 0xfe, 0x00, 0x7d, 0, 0, 0, 0), 1002],
    [
      "64-bit length with its top bit",
      masked(0x82, 0xff, 0x80, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0),
      1002,
    ],
    [
      "64-bit length under 65536",
      masked(0x82, 0xff, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0),
      1002,
    ],
    ["64-bit length past the limit", masked(0x82, 0xff, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0), 1009],
    ["fragmented ping", clientFrame(Opcode.Ping, "x", false), 1002],
    ["long ping", clientFrame(Opcode.Ping, new Uint8Array(126)), 1002],
    ["continuation first", clientFrame(Opcode.Continuation, "x"), 1002],
    [
      "text inside a fragmented message",
      new Uint8Array([...clientFrame(Opcode.Text, "a", false), ...clientFrame(Opcode.Text, "b")]),
      1002,
    ],
    ["message past the limit", clientFrame(Opcode.Binary, new Uint8Array(1025)), 1009],
    [
      "fragments past the limit",
      new Uint8Array([
        ...clientFrame(Opcode.Binary, new Uint8Array(600), false),
        ...clientFrame(Opcode.Continuation, new Uint8Array(600)),
      ]),
      1009,
    ],
    ["text not UTF-8", clientFrame(Opcode.Text, new Uint8Array([0xc3, 0x28])), 1007],
    ["close of one byte", clientFrame(Opcode.Close, new Uint8Array([3])), 1002],
    ["close code 1005", clientFrame(Opcode.Close, new Uint8Array([0x03, 0xed])), 1002],
    ["close reason not UTF-8", clientFrame(Opcode.Close, new Uint8Array([0x03, 0xe8, 0xff])), 1007],
  ];
  for (const [name, data, code] of cases) {
    for (const feed of [
      (d: FrameDecoder) => d.push(data),
      (d: FrameDecoder) => byteByByte(d, data),
    ]) {
      assert.throws(
        () => feed(new FrameDecoder(1024)),
        (err: unknown) => err instanceof WsError && err.code === code,
        name,
      );
    }
  }
});
