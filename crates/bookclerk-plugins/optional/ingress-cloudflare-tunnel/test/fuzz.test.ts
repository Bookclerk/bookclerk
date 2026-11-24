/**
 * Fuzzing for what the Cloudflare edge sends the connector, as the fuzzing
 * ADR asks of boundaries outside the Rust workspace
 * (docs/adr/untrusted-input-fuzzing.md, decision 6).
 *
 * Every byte on the connector's connection comes from Cloudflare's edge or,
 * inside a proxied stream, from any client on the internet. Each seed in
 * `fuzz/h2/` (hex HTTP/2 bytes from the preface on), `fuzz/hpack/` (hex
 * header blocks), `fuzz/capnp/` (hex framed RPC messages), `fuzz/head/`
 * (a JSON field list), `fuzz/wsframe/` (hex WebSocket frames a client
 * sends) and `fuzz/api/` (API answers) is replayed, then seeded mutations are checked against oracles
 * written from the protocols' rules rather than from the parsers:
 *
 * - HTTP/2 frames: RFC 9113's framing rules restated over the whole input,
 *   and the same answer whatever the chunk boundaries;
 * - header blocks: an independent RFC 7541 decoder, whose Huffman strings
 *   are checked by encoding the decoded octets again;
 * - RPC messages: the same answer whatever the chunk boundaries, only
 *   `CapnpError` for a bad message, and the message's union tag read
 *   straight from the bytes;
 * - request fields: RFC 9113 section 8.3's rules restated;
 * - WebSocket frames: an independent RFC 6455 decoder over the whole
 *   input, and the same answer whatever the chunk boundaries;
 * - Cloudflare API answers to the guided setup (`fuzz/api/`, JSON text):
 *   the envelope, account, tunnel, zone, record and tunnel token rules
 *   restated over `JSON.parse`'s value.
 *
 * `FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run; a failure prints the
 * seed, iteration and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

import {
  SetupError,
  parseAccounts,
  parseEnvelope,
  parseRecords,
  parseTunnel,
  parseTunnelToken,
  parseTunnels,
  parseZone,
} from "../src/cfapi.ts";
import { test } from "node:test";

import { requestHead } from "../src/agent.ts";
import { CapnpError, MessageFramer } from "../src/capnp.ts";
import { FrameReader, H2Error, type Frame } from "../src/h2.ts";
import {
  HUFFMAN_CODES,
  HUFFMAN_LENGTHS,
  HpackDecoder,
  HpackError,
  STATIC_TABLE,
  type Field,
} from "../src/hpack.ts";
import { readIncoming } from "../src/rpc.ts";
import { FrameDecoder, WsError, type Message } from "../src/ws.ts";

/**
 * A whole-number setting from the environment.
 *
 * @param name - Variable name.
 * @param fallback - Value when the variable is unset.
 * @param max - Largest accepted value.
 * @returns The value.
 * @throws {Error} Naming the variable when its value is not a whole number
 *   from 0 to `max`.
 */
function wholeEnv(name: string, fallback: number, max: number): number {
  const raw = process.env[name];
  if (raw === undefined) return fallback;
  const value = /^(?:0x[0-9a-f]+|[0-9]+)$/i.test(raw) ? Number(raw) : Number.NaN;
  if (!Number.isSafeInteger(value) || value > max)
    throw new Error(`${name} must be a whole number from 0 to ${max}, not ${JSON.stringify(raw)}`);
  return value;
}

/** PRNG seed for this run. */
const SEED = wholeEnv("FUZZ_SEED", 0x9e0c, 0xffff_ffff);

/** Mutated inputs checked per corpus. */
const ITERATIONS = wholeEnv("FUZZ_ITERATIONS", 3000, 10_000_000);

/** One seed file. */
interface Seed {
  /** File name, for failures. */
  file: string;
  /** Text without its final newline. */
  text: string;
}

/**
 * Reads one corpus directory.
 *
 * @param name - Directory under `fuzz/`.
 * @returns Each seed, by file name order.
 * @throws {Error} When the directory holds no seed.
 */
function corpus(name: string): Seed[] {
  const dir = new URL(`../fuzz/${name}/`, import.meta.url);
  const seeds = readdirSync(dir)
    .filter((file) => file.endsWith(".txt"))
    .sort()
    .map((file) => ({
      file: `fuzz/${name}/${file}`,
      text: readFileSync(new URL(file, dir), "utf8").replace(/\n$/, ""),
    }));
  if (seeds.length === 0) throw new Error(`fuzz/${name}/ holds no .txt seeds`);
  return seeds;
}

/**
 * A small deterministic PRNG (mulberry32).
 *
 * @param seed - Seed.
 * @returns A function giving floats in [0, 1).
 */
function prng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * Bytes of a hex seed.
 *
 * @param seed - The seed, whose text is even-length hex.
 * @returns The bytes.
 * @throws {Error} Naming the file when its text is not whole hex pairs.
 */
function fromHex(seed: Seed): Uint8Array {
  const hex = seed.text;
  if (!/^(?:[0-9a-f]{2})*$/i.test(hex)) throw new Error(`${seed.file} is not whole hex byte pairs`);
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  return out;
}

/**
 * Bytes of a text seed.
 *
 * @param seed - The seed.
 * @returns Its UTF-8 bytes.
 */
function fromText(seed: Seed): Uint8Array {
  return new TextEncoder().encode(seed.text);
}

/**
 * Hex of bytes, for failure messages.
 *
 * @param bytes - The bytes.
 * @returns Lowercase hex.
 */
function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Text a mutation splices into a JSON field list. */
const FRAGMENTS: readonly string[] = [
  '["x","y"],',
  '[":path","/"],',
  '[":method","GET"],',
  '[":authority","h"],',
  '[":protocol","websocket"],',
  '["Upper","v"],',
  '["","v"],',
  "\\r\\n",
  "\\u0000",
  " ",
  ":",
  "/",
  "*",
];

/**
 * Mutates bytes: flips, inserts, deletes, splices fragments, duplicates
 * and truncates.
 *
 * @param input - Seed bytes.
 * @param rand - PRNG.
 * @param fragments - Whether to splice text fragments.
 * @returns A mutated copy.
 */
function mutate(input: Uint8Array, rand: () => number, fragments = false): Uint8Array {
  const bytes = [...input];
  const edits = 1 + Math.floor(rand() * 4);
  for (let e = 0; e < edits; e++) {
    const at = Math.floor(rand() * (bytes.length + 1));
    switch (Math.floor(rand() * (fragments ? 7 : 6))) {
      case 0:
        if (bytes.length) bytes[at % bytes.length] = Math.floor(rand() * 256);
        break;
      case 1:
        bytes.splice(at, 0, Math.floor(rand() * 256));
        break;
      case 2:
        bytes.splice(at, 1 + Math.floor(rand() * 4));
        break;
      case 3: {
        const from = Math.floor(rand() * (bytes.length + 1));
        bytes.splice(at, 0, ...bytes.slice(from, from + Math.floor(rand() * 16)));
        break;
      }
      case 4:
        if (bytes.length)
          bytes[at % bytes.length] = [0, 0xff, 0x7f, 0x80, 10, 13][Math.floor(rand() * 6)]!;
        break;
      case 5:
        bytes.length = Math.floor(rand() * (bytes.length + 1));
        break;
      default: {
        const fragment = FRAGMENTS[Math.floor(rand() * FRAGMENTS.length)]!;
        bytes.splice(at, 0, ...Array.from(fragment, (c) => c.charCodeAt(0)));
      }
    }
  }
  return Uint8Array.from(bytes);
}

/**
 * Splits bytes at random points.
 *
 * @param bytes - The bytes.
 * @param rand - PRNG.
 * @returns The chunks, in order.
 */
function split(bytes: Uint8Array, rand: () => number): Uint8Array[] {
  const out: Uint8Array[] = [];
  let at = 0;
  while (at < bytes.length) {
    const n = 1 + Math.floor(rand() * Math.max(1, Math.min(64, bytes.length - at)));
    out.push(bytes.slice(at, at + n));
    at += n;
  }
  return out;
}

/**
 * Runs `check` over every seed and over seeded mutations of them.
 *
 * @param name - Corpus directory.
 * @param parse - Turns a seed into bytes.
 * @param check - Asserts the oracle for one input.
 * @param fragments - Whether mutations splice text fragments.
 */
function fuzz(
  name: string,
  parse: (seed: Seed) => Uint8Array,
  check: (input: Uint8Array, rand: () => number) => void,
  fragments = false,
): void {
  const seeds = corpus(name).map((seed) => ({ seed, bytes: parse(seed) }));
  const rand = prng(SEED ^ name.length);
  for (const { seed, bytes } of seeds) {
    try {
      check(bytes, rand);
    } catch (err) {
      throw new Error(`${seed.file}: ${(err as Error).message}`, { cause: err });
    }
  }
  for (let i = 0; i < ITERATIONS; i++) {
    const base = seeds[Math.floor(rand() * seeds.length)]!;
    const input = mutate(base.bytes, rand, fragments);
    try {
      check(input, rand);
    } catch (err) {
      throw new Error(
        `${name}: FUZZ_SEED=${SEED} iteration ${i} from ${base.seed.file}, input ${hex(input)}: ${(err as Error).message}`,
        { cause: err },
      );
    }
  }
}

// ---------------------------------------------------------------- HTTP/2

/** What an HTTP/2 decode came to. */
interface H2Outcome {
  /** Frames, in order, up to an error. */
  frames: Frame[];
  /** The error code that stopped it, or `null`. */
  error: number | null;
}

/** The connection preface (RFC 9113 section 3.4). */
const H2_PREFACE = Array.from("PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n", (c) => c.charCodeAt(0));

/**
 * RFC 9113's framing rules restated over the whole input, for what a
 * server reads: the preface, then SETTINGS, frames of at most 16384 bytes,
 * each type's stream and size rules, padding, priority, header blocks
 * joined across CONTINUATION up to 256 KiB, setting values, and no
 * PUSH_PROMISE; unknown types are skipped.
 *
 * @param input - Bytes.
 * @returns The frames and the error code, if any.
 */
function oracleH2(input: Uint8Array): H2Outcome {
  const frames: Frame[] = [];
  const fail = (error: number): H2Outcome => ({ frames, error });
  for (let k = 0; k < Math.min(input.length, 24); k++) {
    if (input[k] !== H2_PREFACE[k]) return fail(1);
  }
  const be = (at: number, n: number): number => {
    let v = 0;
    for (let k = 0; k < n; k++) v = v * 256 + input[at + k]!;
    return v;
  };
  let i = 24;
  let first = true;
  let open: { stream: number; parts: number[]; end: boolean } | null = null;
  while (i + 9 <= input.length) {
    const length = be(i, 3);
    if (length > 16384) return fail(6);
    if (i + 9 + length > input.length) break;
    const type = input[i + 3]!;
    const flags = input[i + 4]!;
    const stream = be(i + 5, 4) % 2 ** 31;
    const payload = input.slice(i + 9, i + 9 + length);
    i += 9 + length;
    if (first) {
      if (type !== 4 || flags & 1) return fail(1);
      first = false;
    }
    if (open !== null && type !== 9) return fail(1);
    let body: Uint8Array = payload;
    const unpad = (): number | null => {
      if (!(flags & 8)) return null;
      if (payload.length < 1) return 6;
      if (payload[0]! > payload.length - 1) return 1;
      body = payload.slice(1, payload.length - payload[0]!);
      return null;
    };
    switch (type) {
      case 0: {
        if (stream === 0) return fail(1);
        const bad = unpad();
        if (bad !== null) return fail(bad);
        frames.push({
          kind: "data",
          stream,
          data: body,
          endStream: (flags & 1) !== 0,
          flow: length,
        });
        break;
      }
      case 1: {
        if (stream === 0) return fail(1);
        const bad = unpad();
        if (bad !== null) return fail(bad);
        if (flags & 0x20) {
          if (body.length < 5) return fail(6);
          const dep = (body[0]! & 0x7f) * 2 ** 24 + body[1]! * 65536 + body[2]! * 256 + body[3]!;
          if (dep === stream) return fail(1);
          body = body.slice(5);
        }
        if (flags & 4)
          frames.push({ kind: "headers", stream, block: body, endStream: (flags & 1) !== 0 });
        else open = { stream, parts: [...body], end: (flags & 1) !== 0 };
        break;
      }
      case 9: {
        if (open === null || open.stream !== stream) return fail(1);
        open.parts.push(...payload);
        if (open.parts.length > 256 * 1024) return fail(0xb);
        if (flags & 4) {
          frames.push({
            kind: "headers",
            stream,
            block: Uint8Array.from(open.parts),
            endStream: open.end,
          });
          open = null;
        }
        break;
      }
      case 2:
        if (stream === 0) return fail(1);
        if (length !== 5) return fail(6);
        break;
      case 3:
        if (stream === 0) return fail(1);
        if (length !== 4) return fail(6);
        frames.push({ kind: "rst", stream, code: be(i - 4, 4) });
        break;
      case 4: {
        if (stream !== 0) return fail(1);
        if (flags & 1) {
          if (length !== 0) return fail(6);
          frames.push({ kind: "settings", ack: true, params: [] });
          break;
        }
        if (length % 6 !== 0) return fail(6);
        const params: Array<[number, number]> = [];
        for (let at = 0; at < length; at += 6) {
          const id = payload[at]! * 256 + payload[at + 1]!;
          const value =
            ((payload[at + 2]! * 256 + payload[at + 3]!) * 256 + payload[at + 4]!) * 256 +
            payload[at + 5]!;
          if (id === 2 && value > 1) return fail(1);
          if (id === 4 && value > 2 ** 31 - 1) return fail(3);
          if (id === 5 && (value < 16384 || value > 2 ** 24 - 1)) return fail(1);
          params.push([id, value]);
        }
        frames.push({ kind: "settings", ack: false, params });
        break;
      }
      case 5:
        return fail(1);
      case 6:
        if (stream !== 0) return fail(1);
        if (length !== 8) return fail(6);
        frames.push({ kind: "ping", ack: (flags & 1) !== 0, data: payload });
        break;
      case 7:
        if (stream !== 0) return fail(1);
        if (length < 8) return fail(6);
        frames.push({
          kind: "goaway",
          lastStream:
            (payload[0]! & 0x7f) * 2 ** 24 + payload[1]! * 65536 + payload[2]! * 256 + payload[3]!,
          code: ((payload[4]! * 256 + payload[5]!) * 256 + payload[6]!) * 256 + payload[7]!,
          debug: new TextDecoder().decode(payload.slice(8, 8 + 256)),
        });
        break;
      case 8: {
        if (length !== 4) return fail(6);
        const increment =
          (payload[0]! & 0x7f) * 2 ** 24 + payload[1]! * 65536 + payload[2]! * 256 + payload[3]!;
        if (increment === 0) return fail(1);
        frames.push({ kind: "window", stream, increment });
        break;
      }
      default:
        break;
    }
  }
  return { frames, error: null };
}

/**
 * Runs the reader over chunks.
 *
 * @param chunks - The input, in pieces.
 * @returns The frames from every call that returned, and the error code.
 */
function readerH2(chunks: Uint8Array[]): H2Outcome {
  const reader = new FrameReader();
  const frames: Frame[] = [];
  try {
    for (const chunk of chunks) frames.push(...reader.push(chunk));
  } catch (err) {
    if (err instanceof H2Error) return { frames, error: err.code };
    throw err;
  }
  return { frames, error: null };
}

test("fuzz: HTTP/2 frames match RFC 9113's rules restated, whatever the chunking", () => {
  fuzz("h2", fromHex, (input, rand) => {
    const expected = oracleH2(input);
    for (const actual of [readerH2([input]), readerH2(split(input, rand))]) {
      assert.equal(actual.error, expected.error);
      assert.deepEqual(actual.frames, expected.frames.slice(0, actual.frames.length));
      if (actual.error === null) assert.deepEqual(actual.frames, expected.frames);
    }
  });
});

// ---------------------------------------------------------------- HPACK

/** Huffman codes by `length:code`, for the oracle's own lookup. */
const HUFFMAN_BY_CODE = new Map<string, number>(
  HUFFMAN_CODES.map((code, symbol) => [`${HUFFMAN_LENGTHS[symbol]}:${code}`, symbol]),
);

/**
 * Decodes a Huffman string by looking each prefix up (RFC 7541 section
 * 5.2): EOS (30 ones) is an error, and so is padding longer than 7 bits or
 * not all ones.
 *
 * @param bytes - The coded bytes.
 * @returns The octets, or `null` when the coding is bad.
 */
function oracleHuffman(bytes: Uint8Array): number[] | null {
  const out: number[] = [];
  let code = 0;
  let length = 0;
  for (const byte of bytes) {
    for (let bit = 7; bit >= 0; bit--) {
      code = code * 2 + ((byte >> bit) & 1);
      length++;
      if (length === 30 && code === 0x3fffffff) return null;
      const symbol = HUFFMAN_BY_CODE.get(`${length}:${code}`);
      if (symbol !== undefined) {
        out.push(symbol);
        code = 0;
        length = 0;
      }
    }
  }
  if (length > 7 || code !== 2 ** length - 1) return null;
  return out;
}

/** What an HPACK decode came to. */
interface HpackOutcome {
  /** The fields, or `null` for a coding error. */
  fields: Field[] | null;
  /** Dynamic table bytes after the block. */
  table: number;
}

/**
 * RFC 7541 restated: indexed fields, literals with and without indexing,
 * table size updates at a block's start up to 4096, eviction, integers up
 * to 2^28 and strings inside the block.
 *
 * @param block - One header block, decoded on a fresh context.
 * @returns The fields and the table's size.
 */
function oracleHpack(block: Uint8Array): HpackOutcome {
  const table: Field[] = [];
  let size = 0;
  let max = 4096;
  const sizeOf = (f: Field): number => f[0].length + f[1].length + 32;
  const evict = (room: number): void => {
    while (table.length && size + room > max) size -= sizeOf(table.pop()!);
  };
  let at = 0;
  const integer = (prefix: number): number | null => {
    if (at >= block.length) return null;
    const mask = 2 ** prefix - 1;
    let value = block[at++]! & mask;
    if (value < mask) return value;
    let m = 1;
    for (;;) {
      if (at >= block.length) return null;
      const b = block[at++]!;
      value += (b & 0x7f) * m;
      m *= 128;
      if (value > 2 ** 28) return null;
      if (!(b & 0x80)) return value;
    }
  };
  const string = (): string | null => {
    if (at >= block.length) return null;
    const huffman = (block[at]! & 0x80) !== 0;
    const length = integer(7);
    if (length === null || length > block.length - at) return null;
    const raw = Array.from(block.slice(at, at + length));
    at += length;
    const octets = huffman ? oracleHuffman(Uint8Array.from(raw)) : raw;
    return octets === null ? null : String.fromCharCode(...octets);
  };
  const lookup = (index: number): Field | null => {
    if (index === 0) return null;
    if (index <= STATIC_TABLE.length) return [...STATIC_TABLE[index - 1]!] as Field;
    const f = table[index - STATIC_TABLE.length - 1];
    return f ? [f[0], f[1]] : null;
  };
  const fields: Field[] = [];
  const bad: HpackOutcome = { fields: null, table: 0 };
  while (at < block.length) {
    const b = block[at]!;
    if (b >= 0x80) {
      const index = integer(7);
      const f = index === null ? null : lookup(index);
      if (f === null) return bad;
      fields.push(f);
    } else if (b >= 0x40 || b < 0x20) {
      const indexing = b >= 0x40;
      const index = integer(indexing ? 6 : 4);
      if (index === null) return bad;
      let name: string | null;
      if (index === 0) name = string();
      else name = lookup(index)?.[0] ?? null;
      if (name === null) return bad;
      const value = string();
      if (value === null) return bad;
      const f: Field = [name, value];
      fields.push(f);
      if (indexing) {
        evict(sizeOf(f));
        if (sizeOf(f) <= max) {
          table.unshift(f);
          size += sizeOf(f);
        }
      }
    } else {
      if (fields.length > 0) return bad;
      const next = integer(5);
      if (next === null || next > 4096) return bad;
      max = next;
      evict(0);
    }
  }
  return { fields, table: size };
}

/**
 * Huffman-codes octets (RFC 7541 appendix B), padded with ones.
 *
 * @param octets - The octets.
 * @returns The coded bytes.
 */
function huffmanEncode(octets: number[]): Uint8Array {
  const out: number[] = [];
  let acc = 0;
  let bits = 0;
  for (const o of octets) {
    const code = HUFFMAN_CODES[o]!;
    for (let k = HUFFMAN_LENGTHS[o]! - 1; k >= 0; k--) {
      acc = (acc << 1) | (Math.floor(code / 2 ** k) & 1);
      if (++bits === 8) {
        out.push(acc);
        acc = 0;
        bits = 0;
      }
    }
  }
  if (bits > 0) out.push(((acc << (8 - bits)) | (0xff >> bits)) & 0xff);
  return Uint8Array.from(out);
}

test("fuzz: header blocks match an independent RFC 7541 decoder", () => {
  fuzz("hpack", fromHex, (input) => {
    const expected = oracleHpack(input);
    const decoder = new HpackDecoder(1 << 30);
    let actual: Field[] | null;
    try {
      actual = decoder.decode(input).fields;
    } catch (err) {
      if (!(err instanceof HpackError)) throw err;
      actual = null;
    }
    assert.deepEqual(actual, expected.fields);
    if (actual !== null) assert.equal(decoder.tableBytes, expected.table);
  });
});

test("fuzz: Huffman strings round-trip through an encoder written from the code table", () => {
  const rand = prng(SEED ^ 0x48);
  for (let i = 0; i < ITERATIONS; i++) {
    const octets = Array.from({ length: Math.floor(rand() * 40) }, () => Math.floor(rand() * 256));
    const coded = huffmanEncode(octets);
    const string = new Uint8Array([0x80 | coded.length, ...coded]);
    if (coded.length >= 127) continue;
    const block = new Uint8Array([0x00, 0x01, 0x78, ...string]);
    const fields = new HpackDecoder(1 << 20).decode(block).fields;
    assert.deepEqual(fields, [["x", String.fromCharCode(...octets)]], `octets ${octets}`);
  }
});

// ---------------------------------------------------------------- Cap'n Proto

/**
 * The kind a message's union tag names, read straight from the bytes
 * when the root is a plain struct pointer in the first segment.
 *
 * @param segments - The message.
 * @returns The kind, or `null` when the root is not that simple.
 */
function rootKind(segments: Uint8Array[]): string | null {
  const s = segments[0];
  if (s === undefined || s.length < 8) return null;
  const view = new DataView(s.buffer, s.byteOffset, s.byteLength);
  const lo = view.getInt32(0, true);
  const hi = view.getUint32(4, true);
  if ((lo & 3) !== 0 || (lo === 0 && hi === 0)) return null;
  const word = 1 + (lo >> 2);
  const tag = (hi & 0xffff) === 0 ? 0 : view.getUint16(word * 8, true);
  switch (tag) {
    case 0:
      return "unimplemented";
    case 1:
      return "abort";
    case 2:
    case 8:
      return "question";
    case 3:
      return "return";
    default:
      return "other";
  }
}

test("fuzz: RPC messages read the same whatever the chunking, and only fail as CapnpError", () => {
  fuzz("capnp", fromHex, (input, rand) => {
    const outcome = (chunks: Uint8Array[]): { messages: string[]; framing: string | null } => {
      const framer = new MessageFramer();
      const messages: string[] = [];
      const show = (value: unknown): string =>
        JSON.stringify(value, (_, v) => (v instanceof Uint8Array ? hex(v) : v));
      try {
        for (const chunk of chunks) {
          for (const segments of framer.push(chunk)) {
            try {
              const message = readIncoming(segments);
              const kind = rootKind(segments);
              if (kind !== null) assert.equal(message.kind, kind);
              messages.push(show(message));
            } catch (err) {
              if (!(err instanceof CapnpError)) throw err;
              messages.push(`bad: ${err.message}`);
            }
          }
        }
      } catch (err) {
        if (!(err instanceof CapnpError)) throw err;
        return { messages, framing: err.message };
      }
      return { messages, framing: null };
    };
    const whole = outcome([input]);
    const pieces = outcome(split(input, rand));
    assert.equal(pieces.framing, whole.framing);
    // A push that throws loses the messages it read; the rest must match.
    const [shorter, longer] =
      pieces.messages.length <= whole.messages.length ? [pieces, whole] : [whole, pieces];
    assert.deepEqual(shorter.messages, longer.messages.slice(0, shorter.messages.length));
    if (whole.framing === null) assert.deepEqual(pieces.messages, whole.messages);
  });
});

// ---------------------------------------------------------------- request fields

/**
 * RFC 9113 section 8.3 restated: pseudo-fields first, `:method` and
 * `:path` exactly once, only `:scheme` and `:authority` besides, a token
 * method, an origin-form path of visible ASCII, lowercase non-empty
 * names and values without CR, LF or NUL.
 *
 * @param fields - The decoded fields.
 * @returns Whether the request is well formed.
 */
function oracleHead(fields: Field[]): boolean {
  const pseudo = fields.filter(([n]) => n.startsWith(":"));
  const firstRegular = fields.findIndex(([n]) => !n.startsWith(":"));
  if (firstRegular >= 0 && fields.slice(firstRegular).some(([n]) => n.startsWith(":")))
    return false;
  if (pseudo.some(([n]) => ![":method", ":path", ":scheme", ":authority"].includes(n)))
    return false;
  const methods = pseudo.filter(([n]) => n === ":method");
  const paths = pseudo.filter(([n]) => n === ":path");
  if (methods.length !== 1 || paths.length !== 1) return false;
  if (!/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(methods[0]![1])) return false;
  const path = paths[0]![1];
  if (!path.startsWith("/") || [...path].some((c) => c < "!" || c > "~")) return false;
  return fields
    .filter(([n]) => !n.startsWith(":"))
    .every(([n, v]) => n.length > 0 && n === n.toLowerCase() && !/[\r\n\0]/.test(v));
}

test("fuzz: request fields are accepted exactly when RFC 9113 allows them", () => {
  fuzz(
    "head",
    fromText,
    (input) => {
      let fields: unknown;
      try {
        fields = JSON.parse(new TextDecoder().decode(input));
      } catch {
        return;
      }
      if (
        !Array.isArray(fields) ||
        !fields.every(
          (f) =>
            Array.isArray(f) &&
            f.length === 2 &&
            typeof f[0] === "string" &&
            typeof f[1] === "string",
        )
      ) {
        return;
      }
      const list = fields as Field[];
      const head = requestHead(list);
      assert.equal(head !== null, oracleHead(list));
      if (head !== null) {
        assert.deepEqual(
          head.headers,
          list.filter(([n]) => !n.startsWith(":")),
        );
      }
    },
    true,
  );
});

// ---------------------------------------------------------------- WebSocket

/** Largest message the WebSocket fuzzing passes on, small so 1009 is reached. */
const WS_MAX_MESSAGE = 64;

/** What a WebSocket decode came to. */
interface WsOutcome {
  /** Messages, in order, up to a close or an error. */
  messages: Message[];
  /** The close code of the error that stopped it, or `null`. */
  error: number | null;
}

/**
 * RFC 6455 section 5 restated over the whole input, for client frames:
 * masked, no extensions, minimal lengths, control frames final and at most
 * 125 bytes, continuation only inside a message, text and close reasons
 * UTF-8, close codes from the wire set, data messages at most
 * {@link WS_MAX_MESSAGE} bytes (checked as each header arrives).
 *
 * @param input - Bytes.
 * @returns The messages and the error, if any.
 */
function oracleWs(input: Uint8Array): WsOutcome {
  const messages: Message[] = [];
  const decode = (b: Uint8Array): string | null => {
    try {
      return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(b);
    } catch {
      return null;
    }
  };
  let i = 0;
  let open: { text: boolean; parts: number[] } | null = null;
  const fail = (code: number): WsOutcome => ({ messages, error: code });
  while (i + 2 <= input.length) {
    const b0 = input[i]!;
    const b1 = input[i + 1]!;
    if (b0 & 0x40 || b0 & 0x20 || b0 & 0x10) return fail(1002);
    const op = b0 & 0xf;
    if (![0, 1, 2, 8, 9, 10].includes(op)) return fail(1002);
    if (!(b1 & 0x80)) return fail(1002);
    let n = BigInt(b1 & 0x7f);
    let p = i + 2;
    const ext = n === 126n ? 2 : n === 127n ? 8 : 0;
    if (p + ext + 4 > input.length) break;
    if (ext > 0) {
      n = 0n;
      for (let k = 0; k < ext; k++) n = (n << 8n) | BigInt(input[p + k]!);
      if (ext === 2 && n < 126n) return fail(1002);
      if (ext === 8 && n >= 1n << 63n) return fail(1002);
      if (ext === 8 && n < 65536n) return fail(1002);
    }
    p += ext;
    const fin = (b0 & 0x80) !== 0;
    if (op >= 8 && (!fin || n > 125n)) return fail(1002);
    if (op === 0 && open === null) return fail(1002);
    if ((op === 1 || op === 2) && open !== null) return fail(1002);
    if (op < 8 && BigInt(open?.parts.length ?? 0) + n > BigInt(WS_MAX_MESSAGE)) return fail(1009);
    const key = input.subarray(p, p + 4);
    p += 4;
    const len = Number(n);
    if (p + len > input.length) break;
    const payload = Array.from(input.subarray(p, p + len), (b, k) => b ^ key[k % 4]!);
    i = p + len;
    if (op === 9) messages.push({ kind: "ping", data: Uint8Array.from(payload) });
    else if (op === 10) messages.push({ kind: "pong", data: Uint8Array.from(payload) });
    else if (op === 8) {
      if (payload.length === 0) {
        messages.push({ kind: "close", code: null, reason: "" });
        return { messages, error: null };
      }
      if (payload.length === 1) return fail(1002);
      const code = payload[0]! * 256 + payload[1]!;
      const allowed =
        (code >= 1000 && code <= 1003) ||
        (code >= 1007 && code <= 1014) ||
        (code >= 3000 && code <= 4999);
      if (!allowed) return fail(1002);
      const reason = decode(Uint8Array.from(payload.slice(2)));
      if (reason === null) return fail(1007);
      messages.push({ kind: "close", code, reason });
      return { messages, error: null };
    } else {
      open ??= { text: op === 1, parts: [] };
      open.parts.push(...payload);
      if (fin) {
        const data = Uint8Array.from(open.parts);
        const isText = open.text;
        open = null;
        if (isText) {
          const text = decode(data);
          if (text === null) return fail(1007);
          messages.push({ kind: "text", text });
        } else {
          messages.push({ kind: "binary", data });
        }
      }
    }
  }
  return { messages, error: null };
}

/**
 * Runs the decoder over chunks.
 *
 * @param chunks - The input, in pieces.
 * @returns The messages from every call that returned, and the error code.
 */
function decoderWs(chunks: Uint8Array[]): WsOutcome {
  const decoder = new FrameDecoder(WS_MAX_MESSAGE);
  const messages: Message[] = [];
  try {
    for (const chunk of chunks) messages.push(...decoder.push(chunk));
  } catch (err) {
    if (err instanceof WsError) return { messages, error: err.code };
    throw err;
  }
  return { messages, error: null };
}

test("fuzz: WebSocket frames match an independent decoder, whatever the chunking", () => {
  fuzz("wsframe", fromHex, (input, rand) => {
    const expected = oracleWs(input);
    for (const actual of [decoderWs([input]), decoderWs(split(input, rand))]) {
      assert.equal(actual.error, expected.error);
      // A call that throws loses the messages it read; the rest must match.
      assert.deepEqual(actual.messages, expected.messages.slice(0, actual.messages.length));
      if (actual.error === null) assert.deepEqual(actual.messages, expected.messages);
    }
  });
});

// ---------------------------------------------------------- API answers

/**
 * Whether a value is one of Cloudflare's ids, by character.
 *
 * @param id - Candidate.
 * @returns The answer.
 */
function hexId(id: unknown): id is string {
  return (
    typeof id === "string" &&
    id.length === 32 &&
    [...id].every((c) => "0123456789abcdef".includes(c))
  );
}

/**
 * Whether a value is a lowercase UUID, by character.
 *
 * @param id - Candidate.
 * @returns The answer.
 */
function uuid(id: unknown): id is string {
  if (typeof id !== "string" || id.length !== 36) return false;
  return [...id].every((c, i) =>
    [8, 13, 18, 23].includes(i) ? c === "-" : "0123456789abcdef".includes(c),
  );
}

/**
 * Whether a value is a JSON object (not null or an array).
 *
 * @param value - Candidate.
 * @returns The answer.
 */
function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The tunnel rule restated: a live tunnel with a UUID id and a string name.
 *
 * @param value - Candidate.
 * @returns The tunnel, or `null`.
 */
function tunnelOracle(value: unknown): { id: string; name: string } | null {
  if (!isObject(value) || !uuid(value.id) || typeof value.name !== "string") return null;
  if (value.deleted_at !== undefined && value.deleted_at !== null) return null;
  return { id: value.id, name: value.name };
}

test("fuzz: API answers match the envelope, account, tunnel, zone, record and token rules", () => {
  fuzz(
    "api",
    fromText,
    (input) => {
      const text = new TextDecoder().decode(input);
      let value: unknown;
      let json = true;
      try {
        value = JSON.parse(text);
      } catch {
        json = false;
      }
      for (const status of [200, 404]) {
        if (!json || !isObject(value)) {
          assert.throws(() => parseEnvelope(status, text), SetupError);
          continue;
        }
        const envelope = parseEnvelope(status, text);
        if (status === 200 && value.success === true) {
          assert.deepEqual(envelope, { ok: true, result: value.result });
        } else {
          const first: unknown = Array.isArray(value.errors) ? value.errors[0] : undefined;
          const message = isObject(first) && typeof first.message === "string" ? first.message : "";
          assert.deepEqual(envelope, { ok: false, message: message.slice(0, 200) });
        }
      }
      if (!json || !isObject(value)) return;
      const result = value.result;
      assert.deepEqual(parseTunnel(result), tunnelOracle(result));
      const usable =
        typeof result === "string" &&
        result.length >= 1 &&
        result.length <= 1024 &&
        [...result].every((c) => c >= "!" && c <= "~");
      if (usable) assert.equal(parseTunnelToken(result), result);
      else assert.throws(() => parseTunnelToken(result), SetupError);
      const name = "books.example.com";
      if (!Array.isArray(result)) {
        assert.throws(() => parseAccounts(result), SetupError);
        assert.throws(() => parseTunnels(result, name), SetupError);
        assert.throws(() => parseZone(result, name), SetupError);
        assert.throws(() => parseRecords(result, name), SetupError);
        return;
      }
      assert.deepEqual(
        parseAccounts(result),
        result
          .filter((a: unknown) => isObject(a) && hexId(a.id))
          .map((a) => (a as { id: string }).id),
      );
      const tunnel = result
        .map(tunnelOracle)
        .find((t) => t !== null && t.name === `bookclerk-${name}`);
      assert.deepEqual(parseTunnels(result, `bookclerk-${name}`), tunnel ?? null);
      for (const zoneName of ["example.com", name]) {
        const zone = result.find(
          (z: unknown) =>
            isObject(z) && hexId(z.id) && z.name === zoneName && z.status === "active",
        ) as Record<string, unknown> | undefined;
        assert.equal(parseZone(result, zoneName), zone === undefined ? null : zone.id);
      }
      const records = result
        .filter(
          (r: unknown): r is Record<string, unknown> =>
            isObject(r) &&
            hexId(r.id) &&
            typeof r.type === "string" &&
            r.name === name &&
            typeof r.content === "string",
        )
        .map((r) => ({
          id: r.id,
          type: r.type,
          content: r.content,
          proxied: r.proxied === true,
          comment: typeof r.comment === "string" ? r.comment : "",
        }));
      assert.deepEqual(parseRecords(result, name), records);
    },
    true,
  );
});
