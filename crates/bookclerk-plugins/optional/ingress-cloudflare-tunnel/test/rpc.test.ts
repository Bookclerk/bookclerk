/**
 * The control channel: Cap'n Proto framing and reading, and the RPC
 * messages, checked byte for byte against what the reference encoder
 * (`capnp encode`, with cloudflared's `tunnelrpc.capnp`) produced.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { Builder, CapnpError, MAX_SEGMENTS, MessageFramer, readRoot } from "../src/capnp.ts";
import {
  encodeBootstrap,
  encodeFinish,
  encodeRegisterConnection,
  encodeUnimplementedReturn,
  readIncoming,
  type Incoming,
} from "../src/rpc.ts";
import { EDGE, REGISTRATION, hex } from "./fake.ts";

/**
 * Reads one framed message.
 *
 * @param bytes - The framed bytes.
 * @returns The message, as the connector reads it.
 */
function read(bytes: Uint8Array | string): Incoming {
  const messages = new MessageFramer().push(typeof bytes === "string" ? hex(bytes) : bytes);
  assert.equal(messages.length, 1);
  return readIncoming(messages[0]!);
}

test("the connector's messages are the reference encoder's bytes", () => {
  assert.deepEqual(
    encodeBootstrap(),
    hex(
      "000000000500000000000000010001000800000000000000000000000100010000000000000000000000000000000000",
    ),
  );
  assert.deepEqual(
    encodeRegisterConnection(REGISTRATION),
    hex(
      "000000002500000000000000010001000200000000000000000000000300030001000000000000009754e87fec9516f700000000000000000800000001000100140000000000020000000000000000000000000001000000000000000100010000000000000000000000000000000000040000000100030000000000000000000200000000000000080000000000020015000000820000001800000001000200050000002a000000050000002a0000006163637400000000010203040500000000112233445566778899aabbccddeeff0100030000000000040000000000040000000000000000000d0000008200000011000000060000000d000000820000001100000042000000ffeeddccbbaa99887766554433221100626f6f6b636c65726b2f302e312e3000776f726b65726400",
    ),
  );
  assert.deepEqual(
    encodeFinish(1),
    hex("00000000040000000000000001000100040000000000000000000000010000000100000000000000"),
  );
  assert.deepEqual(
    encodeUnimplementedReturn(7, "nope"),
    hex(
      "000000000b0000000000000001000100030000000000000000000000020001000700000000000100000000000000000000000000010003000000000003000000090000002a000000000000000000000000000000000000006e6f706500000000",
    ),
  );
});

test("the edge's answers to registerConnection read as the reference encoder wrote them", () => {
  assert.deepEqual(read(EDGE.registered), {
    kind: "return",
    answer: 1,
    result: {
      kind: "registered",
      uuid: hex("0102030405060708090a0b0c0d0e0f10"),
      location: "SJC",
      remotelyManaged: true,
    },
  });
  assert.deepEqual(read(EDGE.refused), {
    kind: "return",
    answer: 1,
    result: {
      kind: "refused",
      cause: "too many connections",
      retryAfterMs: 5000,
      shouldRetry: true,
    },
  });
  assert.deepEqual(read(EDGE.refusedEmpty), {
    kind: "return",
    answer: 1,
    result: { kind: "refused", cause: "", retryAfterMs: 0, shouldRetry: false },
  });
  assert.deepEqual(read(EDGE.exception), {
    kind: "return",
    answer: 1,
    result: { kind: "exception", reason: "no such tunnel" },
  });
  assert.throws(() => read(EDGE.nullResults), /returned nothing/);
  // The bootstrap's own answer needs nothing.
  assert.deepEqual(read(EDGE.return0), { kind: "return", answer: 0, result: null });
});

test("questions, aborts and the rest are told apart", () => {
  assert.deepEqual(read(EDGE.bootstrap4), { kind: "question", question: 4 });
  assert.deepEqual(read(EDGE.call9), { kind: "question", question: 9 });
  assert.deepEqual(read(EDGE.abort), { kind: "abort", reason: "shutting down" });
  assert.deepEqual(read(EDGE.unimplemented), { kind: "unimplemented" });
  assert.deepEqual(read(EDGE.finish), { kind: "other" });
});

test("a ConnectionResponse with an unknown union tag is refused", () => {
  const bytes = hex(EDGE.refusedEmpty);
  // The response struct (one data word holding the tag, one pointer)
  // comes just before the error struct (two data words, one pointer).
  const segment = bytes.subarray(8);
  const at = segment.length - 8 * 5;
  assert.equal(segment[at], 0);
  segment[at] = 5;
  assert.throws(() => read(bytes), /ConnectionResponse tag 5/);
  // A Return with no body is refused.
  const b = new Builder();
  b.root(1, 1).uint(0, 2, 3);
  assert.throws(() => read(b.frame()), /without a body/);
});

test("the framer splits messages across chunks and refuses oversized tables", () => {
  const framer = new MessageFramer();
  const two = new Uint8Array([...encodeBootstrap(), ...encodeFinish(3)]);
  assert.deepEqual(framer.push(two.subarray(0, 3)), []);
  assert.deepEqual(framer.push(two.subarray(3, 10)), []);
  assert.equal(framer.buffered, 10);
  const out = framer.push(two.subarray(10));
  assert.equal(out.length, 2);
  assert.deepEqual(readIncoming(out[1]!), { kind: "other" });
  assert.equal(framer.buffered, 0);
  assert.deepEqual(framer.push(new Uint8Array()), []);

  const many = new Uint8Array(8);
  new DataView(many.buffer).setUint32(0, MAX_SEGMENTS, true);
  assert.throws(() => new MessageFramer().push(many), /segments/);
  const huge = new Uint8Array(8);
  new DataView(huge.buffer).setUint32(4, 0x200000, true);
  assert.throws(() => new MessageFramer().push(huge), /too large/);
  // Two segments: the table is padded to a word.
  const padded = hex("01000000 01000000 01000000 00000000" + "00".repeat(16));
  assert.equal(new MessageFramer().push(padded)[0]?.length, 2);
});

test("far and double-far pointers resolve; broken ones are refused", () => {
  const word = (lo: number, hi: number): string =>
    Buffer.from(new Uint32Array([lo, hi]).buffer).toString("hex");
  const data = "2a00000000000000";
  // Root: a far pointer to a landing pad at segment 1, word 0.
  const far = [hex(word(2, 1)), hex(word(0, 1) + data)];
  assert.equal(readRoot(far).uint(0, 4), 42);
  // Root: a double far pointer; the pad points at segment 2, word 0, then a tag.
  const double = [hex(word(6, 1)), hex(word(2, 2) + word(0, 1)), hex(data)];
  assert.equal(readRoot(double).uint(0, 4), 42);

  const refused: Array<[Uint8Array[], RegExp]> = [
    [[], /no segments/],
    [[new Uint8Array(4)], /pointer past its segment/],
    [[hex(word(2, 9))], /no segment/],
    [[hex(word(2 | (5 << 3), 1)), hex(word(0, 1))], /landing pad past/],
    [[hex(word(2, 1)), hex(word(2, 0))], /far pointer to a far pointer/],
    [[hex(word(6, 1)), hex(word(0, 0) + word(0, 1))], /bad double-far/],
    [[hex(word(6, 1)), hex(word(2, 7) + word(0, 1))], /no segment/],
    [[hex(word(6, 1)), hex(word(2, 2) + word(2, 1)), hex(data)], /tag is a far pointer/],
    [[hex(word(1, 0))], /expected a struct/],
    [[hex(word(0, 1))], /content past/],
  ];
  for (const [segments, message] of refused) {
    assert.throws(
      () => readRoot(segments),
      (err: unknown) => {
        assert.ok(err instanceof CapnpError);
        assert.match(err.message, message);
        return true;
      },
    );
  }
});

test("lists, texts and limits: wrong kinds, missing NULs and loops are refused", () => {
  const word = (lo: number, hi: number): string =>
    Buffer.from(new Uint32Array([lo, hi]).buffer).toString("hex");
  // Root struct (0 data, 1 pointer) whose pointer is a byte list of 3.
  const listOf = (bytes: string, kind = 2) =>
    readRoot([hex(word(0, 1 << 16) + word(1, (3 << 3) | kind) + bytes)]);
  assert.deepEqual(listOf("6869210000000000").data(0), hex("686921"));
  assert.throws(() => listOf("6869210000000000").text(0), /without its NUL/);
  assert.equal(listOf("6869000000000000").text(0), "hi");
  assert.throws(() => listOf("6869210000000000", 3).data(0), /byte list/);
  assert.throws(() => listOf("").data(0), /content past/);
  // Reads past the sections are zero, and pointers past them null.
  const root = listOf("6869000000000000");
  assert.equal(root.uint(0, 4), 0);
  assert.equal(root.int64(0), 0n);
  assert.equal(root.bool(3), false);
  assert.equal(root.struct(4), null);
  assert.deepEqual(root.data(4), new Uint8Array());
  assert.equal(root.text(4), "");
  // A capability pointer reads as null.
  assert.equal(readRoot([hex(word(0, 1 << 16) + word(3, 0))]).struct(0), null);
  // A struct whose pointer points at itself: the nesting limit stops it.
  const loop = readRoot([hex(word(0, 1 << 16) + word(-4 & 0xffffffff, 1 << 16))]);
  assert.throws(() => {
    let s = loop.struct(0);
    for (let i = 0; i < 100 && s !== null; i++) s = s.struct(0);
  }, /nesting limit|traversal/);
});

test("the builder writes lists of texts and refuses what it cannot encode", () => {
  const b = new Builder();
  const root = b.root(1, 2);
  root.textList(0, ["a", "bc"]).textList(1, []).uint(0, 1, 9).bool(9, true);
  const back = readRoot([b.segment()]);
  assert.equal(back.uint(0, 1), 9);
  assert.equal(back.bool(9), true);
  assert.throws(() => root.struct(2, 0, 0), /pointer 2 of 2/);
});
