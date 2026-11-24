/**
 * HPACK (RFC 7541): the decoder against the RFC's own examples, Huffman
 * coding against Node's encoder output, and each coding error it must
 * refuse.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { HpackDecoder, HpackError, encodeHeaders, huffmanDecode } from "../src/hpack.ts";
import { hex } from "./fake.ts";

test("RFC 7541 C.3: requests without Huffman coding, sharing a dynamic table", () => {
  const decoder = new HpackDecoder(1 << 16);
  assert.deepEqual(decoder.decode(hex("828684410f7777772e6578616d706c652e636f6d")).fields, [
    [":method", "GET"],
    [":scheme", "http"],
    [":path", "/"],
    [":authority", "www.example.com"],
  ]);
  assert.equal(decoder.tableBytes, 57);
  assert.deepEqual(decoder.decode(hex("828684be58086e6f2d6361636865")).fields, [
    [":method", "GET"],
    [":scheme", "http"],
    [":path", "/"],
    [":authority", "www.example.com"],
    ["cache-control", "no-cache"],
  ]);
  assert.equal(decoder.tableBytes, 110);
  assert.deepEqual(
    decoder.decode(hex("828785bf400a637573746f6d2d6b65790c637573746f6d2d76616c7565")).fields,
    [
      [":method", "GET"],
      [":scheme", "https"],
      [":path", "/index.html"],
      [":authority", "www.example.com"],
      ["custom-key", "custom-value"],
    ],
  );
  assert.equal(decoder.tableBytes, 164);
});

test("RFC 7541 C.4: the same requests with Huffman coding", () => {
  const decoder = new HpackDecoder(1 << 16);
  assert.deepEqual(decoder.decode(hex("828684418cf1e3c2e5f23a6ba0ab90f4ff")).fields[3], [
    ":authority",
    "www.example.com",
  ]);
  assert.deepEqual(decoder.decode(hex("828684be5886a8eb10649cbf")).fields[4], [
    "cache-control",
    "no-cache",
  ]);
  assert.deepEqual(
    decoder.decode(hex("828785bf408825a849e95ba97d7f8925a849e95bb8e8b4bf")).fields[4],
    ["custom-key", "custom-value"],
  );
  assert.equal(decoder.tableBytes, 164);
});

test("RFC 7541 C.5: responses evict the oldest entries from a 256-byte table", () => {
  const decoder = new HpackDecoder(1 << 16, 256);
  decoder.decode(
    hex(
      "4803333032580770726976617465611d4d6f6e2c203231204f637420323031332032303a31333a323120474d546e1768747470733a2f2f7777772e6578616d706c652e636f6d",
    ),
  );
  assert.equal(decoder.tableBytes, 222);
  assert.deepEqual(decoder.decode(hex("4803333037c1c0bf")).fields, [
    [":status", "307"],
    ["cache-control", "private"],
    ["date", "Mon, 21 Oct 2013 20:13:21 GMT"],
    ["location", "https://www.example.com"],
  ]);
  assert.equal(decoder.tableBytes, 222);
  const third = decoder.decode(
    hex(
      "88c1611d4d6f6e2c203231204f637420323031332032303a31333a323220474d54c05a04677a69707738666f6f3d4153444a4b48514b425a584f5157454f50495541585157454f49553b206d61782d6167653d333630303b2076657273696f6e3d31",
    ),
  );
  assert.deepEqual(third.fields.at(-1), [
    "set-cookie",
    "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1",
  ]);
  assert.equal(decoder.tableBytes, 215);
});

test("a table size update shrinks the table, only at a block's start and within the setting", () => {
  const decoder = new HpackDecoder(1 << 16);
  decoder.decode(hex("400a637573746f6d2d6b65790c637573746f6d2d76616c7565"));
  assert.equal(decoder.tableBytes, 54);
  // Size 0 empties it; the entry is gone.
  decoder.decode(hex("20"));
  assert.equal(decoder.tableBytes, 0);
  assert.throws(() => decoder.decode(hex("be")), /past the table/);
  // An entry larger than the table empties it and is not added.
  decoder.decode(hex("3f11"));
  decoder.decode(hex("400a637573746f6d2d6b65790c637573746f6d2d76616c7565"));
  assert.equal(decoder.tableBytes, 0);
  assert.throws(() => new HpackDecoder(1 << 16).decode(hex("82 20")), /after a field/);
  assert.throws(() => new HpackDecoder(1 << 16).decode(hex("3fe21f")), /past the setting/);
});

test("coding errors are refused", () => {
  const cases: Array<[string, RegExp]> = [
    ["80", /index 0/],
    ["ff", /truncated integer/],
    ["ff80", /truncated integer/],
    ["ffffffffff0f", /too large/],
    ["40", /truncated string/],
    ["4005ab", /past the block/],
    ["c0", /past the table/],
    ["0f", /truncated integer/],
    // A Huffman string padded with a zero bit.
    ["008100", /padding/],
  ];
  for (const [block, message] of cases) {
    assert.throws(
      () => new HpackDecoder(1 << 16).decode(hex(block)),
      (err: unknown) => {
        assert.ok(err instanceof HpackError, block);
        assert.match(err.message, message, block);
        return true;
      },
    );
  }
});

test("Huffman coding: EOS and long padding are refused; RFC strings decode", () => {
  assert.equal(
    new TextDecoder().decode(huffmanDecode(hex("f1e3c2e5f23a6ba0ab90f4ff"))),
    "www.example.com",
  );
  assert.deepEqual(huffmanDecode(new Uint8Array()), new Uint8Array());
  // 30 one bits (EOS) then padding.
  assert.throws(() => huffmanDecode(hex("ffffffff")), /EOS/);
  // A full byte of padding.
  assert.throws(() => huffmanDecode(hex("1fff")), /padding/);
  // Every octet value round-trips through a raw literal.
  const all = String.fromCharCode(...Array.from({ length: 256 }, (_, i) => i));
  const decoded = new HpackDecoder(1 << 16).decode(encodeHeaders([["x", all]])).fields;
  assert.deepEqual(decoded, [["x", all]]);
});

test("a list past the limit decodes with `oversized`, keeping the table in step", () => {
  const decoder = new HpackDecoder(64);
  const block = hex("400a637573746f6d2d6b65790c637573746f6d2d76616c7565be");
  const result = decoder.decode(block);
  assert.equal(result.oversized, true);
  assert.deepEqual(result.fields, [["custom-key", "custom-value"]]);
  assert.equal(decoder.tableBytes, 54);
});

test("the encoder writes literals that leave the table alone, with long lengths", () => {
  const long = "v".repeat(300);
  const block = encodeHeaders([
    [":status", "200"],
    ["x-long", long],
    ["x-wide", "☃"],
  ]);
  const decoder = new HpackDecoder(1 << 16);
  assert.deepEqual(decoder.decode(block).fields, [
    [":status", "200"],
    ["x-long", long],
    ["x-wide", "?"],
  ]);
  assert.equal(decoder.tableBytes, 0);
});
