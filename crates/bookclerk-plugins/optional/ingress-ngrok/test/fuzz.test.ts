/**
 * Fuzzing for what ngrok sends the agent, as the fuzzing ADR asks of
 * boundaries outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
 * decision 6).
 *
 * Every byte on the agent's connection comes from ngrok's servers or, inside
 * a proxied stream, from any client on the internet. Each seed in
 * `fuzz/frame/` (hex muxado bytes), `fuzz/head/` (a JSON string holding a
 * request head), `fuzz/chunked/` (hex chunked bodies), `fuzz/proxy/` (hex
 * proxy headers) and `fuzz/rpc/` (hex RPC replies) is replayed, then seeded
 * mutations are checked against oracles written from the protocols' rules
 * rather than from the parsers:
 *
 * - frames: an independent decoder over the whole input, and the same
 *   answer whatever the chunk boundaries;
 * - heads: the RFC 9110 / 9112 grammar for what an accepted head may hold,
 *   an independent restatement of the body-framing rules, and the same
 *   answer when more bytes follow the head;
 * - chunked bodies: an independent decoder, and the same answer whatever
 *   the chunk boundaries;
 * - proxy headers: an independent little-endian length reader and
 *   `JSON.parse`, plus a round trip through `encodeProxyHeader`;
 * - RPC replies: `JSON.parse` of the same text.
 *
 * `FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run; a failure prints the
 * seed, iteration and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import {
  ChunkedDecoder,
  ChunkedError,
  MAX_HEADERS,
  MAX_HEAD_BYTES,
  parseRequestHead,
} from "../src/http1.ts";
import {
  MAX_PROXY_HEADER_BYTES,
  MAX_RPC_BYTES,
  encodeProxyHeader,
  parseProxyHeader,
  parseRpcObject,
} from "../src/messages.ts";
import { FrameReader, MAX_FRAME_BODY, MuxError, type Frame } from "../src/mux.ts";

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
 * Bytes of a seed holding one JSON string (Latin-1 code units).
 *
 * @param seed - The seed.
 * @returns The bytes.
 * @throws {Error} Naming the file when it is not a JSON string of bytes.
 */
function fromJsonString(seed: Seed): Uint8Array {
  const value: unknown = JSON.parse(seed.text);
  if (typeof value !== "string" || /[^\x00-\xff]/.test(value)) {
    throw new Error(`${seed.file} is not a JSON string of bytes`);
  }
  return Uint8Array.from(value, (c) => c.charCodeAt(0));
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

/** Text a mutation splices into a head. */
const FRAGMENTS: readonly string[] = [
  "\r\n",
  "\n",
  "\r",
  " ",
  "\t",
  ":",
  ",",
  "\0",
  "\x7f",
  "\xff",
  "Transfer-Encoding: chunked\r\n",
  "Content-Length: 5\r\n",
  "Content-Length: 5, 6\r\n",
  "Content-Length: -1\r\n",
  "Transfer-Encoding: gzip, chunked\r\n",
  "Connection: upgrade\r\nUpgrade: websocket\r\n",
  "Expect: 100-continue\r\n",
  " folded\r\n",
  "HTTP/1.0",
  "HTTP/2.0",
  "http://x/",
  "*",
  "CONNECT",
  "%00",
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

// ---------------------------------------------------------------- frames

/** What the oracle decoder makes of a byte stream. */
type FrameOutcome = { frames: Frame[]; rest: number } | { error: string };

/**
 * Decodes muxado frames, written from the framing rules: a 24-bit length,
 * a type nibble and a flag nibble, a 31-bit stream id; RST and WNDINC
 * carry exactly one 32-bit word, GOAWAY at least two; DATA never on stream
 * 0; unknown types are skipped; bodies past the window are refused.
 *
 * @param bytes - The stream.
 * @returns The frames and the bytes left over, or an error.
 */
function oracleFrames(bytes: Uint8Array): FrameOutcome {
  const frames: Frame[] = [];
  let at = 0;
  const u32 = (o: number): number =>
    ((bytes[o]! << 24) | (bytes[o + 1]! << 16) | (bytes[o + 2]! << 8) | bytes[o + 3]!) >>> 0;
  while (bytes.length - at >= 8) {
    const length = bytes[at]! * 65536 + bytes[at + 1]! * 256 + bytes[at + 2]!;
    if (length > MAX_FRAME_BODY) return { error: "size" };
    if (bytes.length - at - 8 < length) break;
    const type = bytes[at + 3]! >> 4;
    const flags = bytes[at + 3]! & 15;
    const stream = u32(at + 4) & 0x7fffffff;
    const body = at + 8;
    at += 8 + length;
    if (type === 1) {
      if (stream === 0) return { error: "data0" };
      frames.push({
        kind: "data",
        stream,
        data: bytes.slice(body, body + length),
        fin: (flags & 1) === 1,
        syn: (flags & 2) === 2,
      });
    } else if (type === 0 || type === 2) {
      if (length !== 4) return { error: "word" };
      frames.push(
        type === 0
          ? { kind: "rst", stream, code: u32(body) }
          : { kind: "wndinc", stream, increment: u32(body) & 0x7fffffff },
      );
    } else if (type === 3) {
      if (length < 8) return { error: "goaway" };
      frames.push({
        kind: "goaway",
        lastStream: u32(body) & 0x7fffffff,
        code: u32(body + 4),
        debug: new TextDecoder().decode(bytes.slice(body + 8, body + length)),
      });
    }
  }
  return { frames, rest: bytes.length - at };
}

/**
 * The reader's outcome over chunks.
 *
 * @param chunks - The stream, in pieces.
 * @returns Frames and leftover bytes, or that it threw.
 */
function readerFrames(chunks: Uint8Array[]): FrameOutcome {
  const reader = new FrameReader();
  const frames: Frame[] = [];
  try {
    for (const chunk of chunks) frames.push(...reader.push(chunk));
  } catch (err) {
    if (err instanceof MuxError) return { error: "mux" };
    throw err;
  }
  return { frames, rest: reader.buffered };
}

test("fuzz: muxado frames match an independent decoder, whatever the chunking", () => {
  fuzz("frame", fromHex, (input, rand) => {
    const expected = oracleFrames(input);
    const whole = readerFrames([input]);
    const pieces = readerFrames(split(input, rand));
    if ("error" in expected) {
      assert.ok("error" in whole, "whole input should be refused");
      assert.ok("error" in pieces, "chunked input should be refused");
      return;
    }
    assert.deepEqual(whole, expected);
    // A refusal may come later when chunked (after earlier frames); a success must match.
    if (!("error" in pieces)) assert.deepEqual(pieces, expected);
  });
});

// ---------------------------------------------------------------- heads

/** `tchar` (RFC 9110 section 5.6.2). */
const TCHAR = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

/**
 * The body framing RFC 9112 section 6 gives a request, as this agent
 * applies it: Transfer-Encoding with Content-Length is refused; only a
 * lone `chunked` coding is served; Content-Length must be one decimal
 * value (repeated identically at most).
 *
 * @param headers - Fields, names lowercased.
 * @returns The framing kind and length, or the status refusing it.
 */
function oracleFraming(headers: Array<[string, string]>): string {
  const te: string[] = [];
  const cl: string[] = [];
  for (const [name, value] of headers) {
    if (name === "transfer-encoding") {
      for (const part of value.split(",")) if (part.trim()) te.push(part.trim().toLowerCase());
    }
    if (name === "content-length") for (const part of value.split(",")) cl.push(part.trim());
  }
  if (te.length > 0) {
    if (cl.length > 0) return "400";
    return te.length === 1 && te[0] === "chunked" ? "chunked" : "501";
  }
  if (cl.length === 0) return "none";
  if (!cl.every((v) => v === cl[0]) || !/^[0-9]{1,15}$/.test(cl[0]!)) return "400";
  return Number(cl[0]) === 0 ? "none" : `length:${Number(cl[0])}`;
}

test("fuzz: request heads hold only what the grammar allows, framed by RFC 9112", () => {
  fuzz(
    "head",
    fromJsonString,
    (input) => {
      const result = parseRequestHead(input);
      const text = String.fromCharCode(...input.subarray(0, MAX_HEAD_BYTES));
      const end = text.indexOf("\r\n\r\n");
      if (result === null) {
        assert.ok(
          end < 0 && input.length < MAX_HEAD_BYTES,
          "null only while the head is incomplete",
        );
        return;
      }
      if ("status" in result) {
        assert.ok([400, 431, 501, 505].includes(result.status), `status ${result.status}`);
        return;
      }
      assert.equal(result.headBytes, end + 4);
      const lines = text.slice(0, end).split("\r\n");
      const [method, target, version, ...extra] = (lines[0] ?? "").split(" ");
      assert.equal(extra.length, 0);
      assert.equal(result.method, method);
      assert.equal(result.target, target);
      assert.ok(TCHAR.test(result.method) && result.method !== "CONNECT");
      assert.ok(/^\/[\x21-\x7e]*$/.test(result.target));
      assert.ok(version === "HTTP/1.1" || version === "HTTP/1.0");
      assert.equal(result.headers.length, lines.length - 1);
      assert.ok(result.headers.length <= MAX_HEADERS);
      for (const [name, value] of result.headers) {
        assert.ok(TCHAR.test(name) && name === name.toLowerCase());
        assert.ok(!/[\r\n\0]/.test(value) && !/^[ \t]|[ \t]$/.test(value));
      }
      const framing = oracleFraming(result.headers);
      const got = result.body.kind === "length" ? `length:${result.body.length}` : result.body.kind;
      assert.equal(got, framing);
      // Bytes after the head never change what the head says.
      const longer = parseRequestHead(Uint8Array.from([...input, 0x47, 0x0d, 0x0a]));
      assert.deepEqual(longer, result);
    },
    true,
  );
});

// ---------------------------------------------------------------- chunked

/**
 * Decodes a chunked body, written from RFC 9112 section 7.1: hex size
 * (here at most 8 digits) with optional extensions, CRLF, data, CRLF; a
 * zero size, then trailer lines until an empty one.
 *
 * @param bytes - The bytes.
 * @returns The body and what follows it, `null` when incomplete, or
 *   `"error"`.
 */
function oracleChunked(bytes: Uint8Array): { body: Uint8Array; rest: Uint8Array } | null | "error" {
  const parts: number[] = [];
  let at = 0;
  const line = (): string | null | "error" => {
    const lf = bytes.indexOf(10, at);
    if (lf < 0) return bytes.length - at > 16 * 1024 ? "error" : null;
    if (lf === at || bytes[lf - 1] !== 13) return "error";
    const out = String.fromCharCode(...bytes.subarray(at, lf - 1));
    at = lf + 1;
    return out;
  };
  for (;;) {
    const sizeLine = line();
    if (sizeLine === null || sizeLine === "error") return sizeLine;
    const m = /^([0-9a-fA-F]{1,8})(?:[ \t]*;.*)?$/.exec(sizeLine);
    if (!m) return "error";
    const size = parseInt(m[1]!, 16);
    if (size === 0) break;
    if (bytes.length - at < size) return null;
    parts.push(...bytes.subarray(at, at + size));
    at += size;
    if (bytes.length - at < 2) return bytes.length - at === 1 && bytes[at] !== 13 ? "error" : null;
    if (bytes[at] !== 13 || bytes[at + 1] !== 10) return "error";
    at += 2;
  }
  for (;;) {
    if (at < bytes.length && bytes[at] === 10) return "error";
    const trailer = line();
    if (trailer === null || trailer === "error") return trailer;
    if (trailer === "") return { body: Uint8Array.from(parts), rest: bytes.slice(at) };
  }
}

/**
 * The decoder's outcome over chunks.
 *
 * @param chunks - The body, in pieces.
 * @returns Body and rest, `null` when incomplete, or `"error"`.
 */
function decoderChunked(
  chunks: Uint8Array[],
): { body: Uint8Array; rest: Uint8Array } | null | "error" {
  const decoder = new ChunkedDecoder();
  const body: number[] = [];
  try {
    for (let i = 0; i < chunks.length; i++) {
      const progress = decoder.push(chunks[i]!);
      for (const part of progress.data) body.push(...part);
      if (progress.done) {
        const rest = [...progress.rest];
        for (const later of chunks.slice(i + 1)) rest.push(...later);
        return { body: Uint8Array.from(body), rest: Uint8Array.from(rest) };
      }
    }
  } catch (err) {
    if (err instanceof ChunkedError) return "error";
    throw err;
  }
  return null;
}

test("fuzz: chunked bodies match an independent decoder, whatever the chunking", () => {
  fuzz(
    "chunked",
    fromHex,
    (input, rand) => {
      const expected = oracleChunked(input);
      const whole = decoderChunked([input]);
      const pieces = decoderChunked(split(input, rand));
      if (expected === "error") {
        // The oracle reads a line before judging it; the decoder may refuse
        // a too-long one sooner, but never accepts what the oracle refuses.
        assert.notEqual(typeof whole === "object" && whole !== null, true);
        assert.notEqual(typeof pieces === "object" && pieces !== null, true);
        return;
      }
      if (expected === null) {
        assert.ok(whole === null || whole === "error");
        return;
      }
      assert.deepEqual(whole, expected);
      assert.deepEqual(pieces, expected);
    },
    true,
  );
});

// ---------------------------------------------------------------- proxy headers

test("fuzz: proxy headers match an independent reader and round-trip", () => {
  fuzz("proxy", fromHex, (input) => {
    const result = parseProxyHeader(input);
    if (input.length < 8) {
      assert.equal(result, null);
      return;
    }
    const low = input[0]! | (input[1]! << 8) | (input[2]! << 16) | (input[3]! * 2 ** 24);
    const high = input[4]! | input[5]! | input[6]! | input[7]!;
    if (high !== 0 || low > MAX_PROXY_HEADER_BYTES) {
      assert.equal(typeof result, "string");
      return;
    }
    if (input.length < 8 + low) {
      assert.equal(result, null);
      return;
    }
    let value: unknown;
    try {
      value = JSON.parse(
        new TextDecoder("utf-8", { fatal: true }).decode(input.subarray(8, 8 + low)),
      );
    } catch {
      assert.equal(typeof result, "string");
      return;
    }
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      assert.equal(typeof result, "string");
      return;
    }
    assert.ok(result !== null && typeof result === "object");
    const fields = value as Record<string, unknown>;
    const str = (k: string): string => (typeof fields[k] === "string" ? (fields[k] as string) : "");
    assert.deepEqual(result.header, {
      bindId: str("Id"),
      clientAddr: str("ClientAddr"),
      proto: str("Proto"),
      passthroughTls: fields["PassthroughTLS"] === true,
    });
    assert.deepEqual(result.rest, input.slice(8 + low));
    const again = parseProxyHeader(encodeProxyHeader(result.header));
    assert.ok(again !== null && typeof again === "object");
    assert.deepEqual(again.header, result.header);
  });
});

// ---------------------------------------------------------------- RPC replies

test("fuzz: RPC replies are JSON objects of the same text", () => {
  fuzz("rpc", fromHex, (input) => {
    const result = parseRpcObject(input);
    if (input.length > MAX_RPC_BYTES) {
      assert.equal(typeof result, "string");
      return;
    }
    let text: string;
    try {
      text = new TextDecoder("utf-8", { fatal: true }).decode(input);
    } catch {
      assert.equal(typeof result, "string");
      return;
    }
    let value: unknown;
    try {
      value = JSON.parse(text);
    } catch {
      // Not JSON yet: incomplete unless the writer ended the value.
      assert.ok(result === null || (typeof result === "string" && text.endsWith("\n")));
      return;
    }
    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      assert.deepEqual(result, value);
    } else {
      assert.equal(typeof result, "string");
    }
  });
});
