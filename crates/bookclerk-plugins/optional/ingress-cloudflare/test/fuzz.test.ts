/**
 * Fuzzing for the relay protocol, as the fuzzing ADR asks of boundaries
 * outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
 * decision 6).
 *
 * Both ends parse what the other sends: the plugin parses the relay's
 * frames, request heads and notices, and the relay parses the plugin's
 * frames and response heads. Each seed in `fuzz/frame/` (hex bytes),
 * `fuzz/head/` (head JSON) and `fuzz/notice/` (notice text and client
 * addresses) is replayed, then seeded mutations are checked against
 * oracles written from the protocol's rules rather than from the parser:
 *
 * - frames: an independent decoder over the whole input, and the same
 *   answer whatever the chunk boundaries;
 * - heads: the RFC 9110 token and field-value grammar and the shared
 *   limits, plus a round trip through `encodeFrame`;
 * - client addresses: Node's `net.isIP`, less IPv6 zones.
 *
 * `FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run; a failure prints the
 * seed, iteration and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { isIP } from "node:net";
import { test } from "node:test";

import {
  FRAME_PREFIX_BYTES,
  FrameReader,
  HOP_BY_HOP,
  MAX_HEADERS,
  MAX_HEADER_BYTES,
  MAX_HEAD_BYTES,
  MAX_METHOD_BYTES,
  MAX_TARGET_BYTES,
  encodeFrame,
  isIpAddress,
  isRequestId,
  parseNotice,
  parseRequestHead,
  parseResponseHead,
  providerHeader,
} from "../src/protocol.ts";

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0x7e1a);

/** Mutated inputs checked per corpus. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 3000);

/**
 * Reads one corpus directory.
 *
 * @param name - Directory under `fuzz/`.
 * @returns Each seed's text without its final newline, by file name order.
 */
function corpus(name: string): string[] {
  const dir = new URL(`../fuzz/${name}/`, import.meta.url);
  return readdirSync(dir)
    .filter((file) => file.endsWith(".txt"))
    .sort()
    .map((file) => readFileSync(new URL(file, dir), "utf8").replace(/\n$/, ""));
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
 * @param hex - Even-length hex.
 * @returns The bytes.
 */
function fromHex(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  return out;
}

/**
 * Mutates bytes: flips, inserts, deletes, length rewrites and truncation.
 *
 * @param input - Seed bytes.
 * @param rand - PRNG.
 * @returns A mutated copy.
 */
function mutateBytes(input: Uint8Array, rand: () => number): Uint8Array {
  const bytes = [...input];
  const edits = 1 + Math.floor(rand() * 4);
  for (let e = 0; e < edits; e++) {
    const at = Math.floor(rand() * (bytes.length + 1));
    switch (Math.floor(rand() * 5)) {
      case 0:
        if (bytes.length) bytes[at % bytes.length] = Math.floor(rand() * 256);
        break;
      case 1:
        bytes.splice(at, 0, Math.floor(rand() * 256));
        break;
      case 2:
        bytes.splice(at, 1);
        break;
      case 3: {
        const n = Math.floor(rand() * (bytes.length + 8));
        bytes.splice(
          0,
          Math.min(4, bytes.length),
          (n >>> 24) & 255,
          (n >>> 16) & 255,
          (n >>> 8) & 255,
          n & 255,
        );
        break;
      }
      default:
        bytes.length = Math.floor(rand() * (bytes.length + 1));
    }
  }
  return Uint8Array.from(bytes);
}

/** Text a mutation splices into a head or notice. */
const FRAGMENTS: readonly string[] = [
  '"',
  "\\u0000",
  "\\r\\n",
  "\\u00e9",
  "{",
  "}",
  "[",
  "]",
  ",",
  ":",
  "null",
  "1e309",
  "-1",
  "/",
  "//",
  "#",
  "?",
  " ",
  "Host",
  "cf-",
  "x-forwarded-for",
  "upgrade",
  "::",
  ".",
  "%",
  "999",
  "0",
];

/**
 * Mutates text by splicing fragments and characters.
 *
 * @param input - Seed text.
 * @param rand - PRNG.
 * @returns A mutated copy.
 */
function mutateText(input: string, rand: () => number): string {
  let text = input;
  const edits = 1 + Math.floor(rand() * 3);
  for (let e = 0; e < edits; e++) {
    const at = Math.floor(rand() * (text.length + 1));
    const r = rand();
    if (r < 0.5) {
      text =
        text.slice(0, at) +
        (FRAGMENTS[Math.floor(rand() * FRAGMENTS.length)] ?? "") +
        text.slice(at);
    } else if (r < 0.75) {
      text = text.slice(0, at) + text.slice(at + 1 + Math.floor(rand() * 3));
    } else {
      text = text.slice(0, at) + String.fromCharCode(Math.floor(rand() * 0x80)) + text.slice(at);
    }
  }
  return text;
}

/** What the oracle says a whole frame decodes to. */
type FrameVerdict = { ok: true; json: string; rest: number[] } | { ok: false };

/**
 * The independent frame decoder: reads the whole input at once.
 *
 * @param bytes - The complete stream.
 * @returns The head's JSON and the remaining bytes, or a refusal.
 */
function frameOracle(bytes: Uint8Array): FrameVerdict {
  if (bytes.length < 4) return { ok: false };
  const length =
    (bytes[0] ?? 0) * 2 ** 24 + ((bytes[1] ?? 0) << 16) + ((bytes[2] ?? 0) << 8) + (bytes[3] ?? 0);
  if (length > MAX_HEAD_BYTES || bytes.length < 4 + length) return { ok: false };
  const head = Buffer.from(bytes.subarray(4, 4 + length));
  const json = head.toString("utf8");
  // Valid UTF-8 survives a round trip byte for byte.
  if (!Buffer.from(json, "utf8").equals(head)) return { ok: false };
  return { ok: true, json, rest: [...bytes.subarray(4 + length)] };
}

/**
 * Feeds `bytes` to a {@link FrameReader} in random chunks.
 *
 * @param bytes - The complete stream.
 * @param rand - PRNG for chunk sizes.
 * @returns What the reader decoded.
 */
function readInChunks(bytes: Uint8Array, rand: () => number): FrameVerdict {
  const reader = new FrameReader();
  let at = 0;
  while (at < bytes.length) {
    const size = 1 + Math.floor(rand() * Math.max(1, bytes.length - at));
    const step = reader.push(bytes.subarray(at, at + size));
    at += size;
    if (step.kind === "error") return { ok: false };
    if (step.kind === "head") {
      return { ok: true, json: step.json, rest: [...step.rest, ...bytes.subarray(at)] };
    }
  }
  const last = reader.end();
  assert.equal(last.kind, "error");
  return { ok: false };
}

/**
 * Checks one frame input against the oracle.
 *
 * @param bytes - The input.
 * @param rand - PRNG.
 * @param label - Reproduction note for failures.
 */
function checkFrame(bytes: Uint8Array, rand: () => number, label: string): void {
  const expected = frameOracle(bytes);
  assert.deepEqual(
    readInChunks(bytes, rand),
    expected,
    `${label} chunked ${Buffer.from(bytes).toString("hex")}`,
  );
  assert.deepEqual(
    readInChunks(bytes, () => 0.999),
    expected,
    `${label} whole`,
  );
}

/** RFC 9110 token. */
const TCHAR = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/;

/**
 * The oracle for header lists: the shared limits and the field grammar.
 *
 * @param value - The `headers` member.
 * @param request - Whether provider headers are refused too.
 * @returns Whether the list is acceptable.
 */
function headersOk(value: unknown, request: boolean): boolean {
  if (!Array.isArray(value) || value.length > MAX_HEADERS) return false;
  let bytes = 0;
  for (const field of value) {
    if (typeof field !== "object" || field === null) return false;
    const { name, value: text } = field as Record<string, unknown>;
    if (typeof name !== "string" || typeof text !== "string") return false;
    if (!TCHAR.test(name) || /[A-Z]/.test(name)) return false;
    for (const ch of text) {
      const code = ch.codePointAt(0) ?? 0;
      if (!(code === 9 || (code >= 0x20 && code <= 0x7e))) return false;
    }
    if (HOP_BY_HOP.has(name)) return false;
    if (request && providerHeader(name)) return false;
    bytes += name.length + text.length;
    if (bytes > MAX_HEADER_BYTES) return false;
  }
  return true;
}

/**
 * The oracle for request heads.
 *
 * @param json - Head JSON.
 * @returns Whether the head is acceptable.
 */
function requestOk(json: string): boolean {
  let value: unknown;
  try {
    value = JSON.parse(json);
  } catch {
    return false;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const { method, target, headers, client } = value as Record<string, unknown>;
  if (typeof method !== "string" || method.length > MAX_METHOD_BYTES || !TCHAR.test(method))
    return false;
  if (typeof target !== "string" || target.length > MAX_TARGET_BYTES) return false;
  if (!target.startsWith("/") || target.startsWith("//")) return false;
  for (const ch of target) {
    const code = ch.codePointAt(0) ?? 0;
    if (code < 0x21 || code > 0x7e || ch === "#") return false;
  }
  if (!headersOk(headers, true)) return false;
  if (
    client !== undefined &&
    (typeof client !== "string" || isIP(client) === 0 || client.includes("%"))
  )
    return false;
  return true;
}

/**
 * The oracle for response heads.
 *
 * @param json - Head JSON.
 * @returns Whether the head is acceptable.
 */
function responseOk(json: string): boolean {
  let value: unknown;
  try {
    value = JSON.parse(json);
  } catch {
    return false;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const { status, headers } = value as Record<string, unknown>;
  if (typeof status !== "number" || !Number.isInteger(status) || status < 200 || status > 599)
    return false;
  return headersOk(headers, false);
}

/**
 * Checks one head input against the oracles and the round trip.
 *
 * @param json - The input.
 * @param label - Reproduction note for failures.
 */
function checkHead(json: string, label: string): void {
  const request = parseRequestHead(json);
  assert.equal(
    typeof request !== "string",
    requestOk(json),
    `${label} request ${JSON.stringify(json)}`,
  );
  if (typeof request !== "string") {
    const step = new FrameReader().push(encodeFrame(request));
    assert.equal(step.kind, "head");
    if (step.kind === "head") assert.deepEqual(parseRequestHead(step.json), request, label);
    // The parsed head carries only the members the protocol defines.
    assert.deepEqual(
      Object.keys(request).filter((k) => !["method", "target", "headers", "client"].includes(k)),
      [],
    );
  }
  const response = parseResponseHead(json);
  assert.equal(
    typeof response !== "string",
    responseOk(json),
    `${label} response ${JSON.stringify(json)}`,
  );
  if (typeof response !== "string") {
    const step = new FrameReader().push(encodeFrame(response));
    assert.equal(step.kind, "head");
    if (step.kind === "head") assert.deepEqual(parseResponseHead(step.json), response, label);
  }
}

/**
 * Checks one notice or address input.
 *
 * @param text - The input.
 * @param label - Reproduction note for failures.
 */
function checkNotice(text: string, label: string): void {
  // Node accepts an IPv6 zone (`%eth0`); the host's address parser, like
  // the relay's, does not.
  assert.equal(
    isIpAddress(text),
    isIP(text) !== 0 && !text.includes("%"),
    `${label} address ${JSON.stringify(text)}`,
  );
  const notice = parseNotice(text);
  let expected = false;
  if (text.length <= 256) {
    try {
      const value = JSON.parse(text) as unknown;
      expected =
        typeof value === "object" &&
        value !== null &&
        !Array.isArray(value) &&
        (value as Record<string, unknown>)["t"] === "request" &&
        typeof (value as Record<string, unknown>)["id"] === "string" &&
        /^[0-9A-Za-z-]{1,64}$/.test((value as Record<string, string>)["id"] ?? "");
    } catch {
      expected = false;
    }
  }
  assert.equal(typeof notice !== "string", expected, `${label} notice ${JSON.stringify(text)}`);
  if (typeof notice !== "string") assert.equal(isRequestId(notice.id), true);
}

test("frame seeds replay and mutations match the independent decoder", () => {
  const seeds = corpus("frame").map(fromHex);
  const rand = prng(SEED);
  seeds.forEach((seed, i) => checkFrame(seed, rand, `seed ${i}`));
  for (let i = 0; i < ITERATIONS; i++) {
    const seed = seeds[Math.floor(rand() * seeds.length)] ?? new Uint8Array();
    checkFrame(mutateBytes(seed, rand), rand, `FUZZ_SEED=${SEED} iteration ${i}`);
  }
  // A frame announcing more than the limit fails without waiting for it.
  const huge = new Uint8Array(FRAME_PREFIX_BYTES);
  new DataView(huge.buffer).setUint32(0, MAX_HEAD_BYTES + 1, false);
  assert.equal(new FrameReader().push(huge).kind, "error");
});

test("head seeds replay and mutations match the grammar oracle", () => {
  const seeds = corpus("head");
  const rand = prng(SEED + 1);
  seeds.forEach((seed, i) => checkHead(seed, `seed ${i}`));
  for (let i = 0; i < ITERATIONS; i++) {
    const seed = seeds[Math.floor(rand() * seeds.length)] ?? "";
    checkHead(mutateText(seed, rand), `FUZZ_SEED=${SEED} iteration ${i}`);
  }
});

test("notice and address seeds replay and mutations match the oracles", () => {
  const seeds = corpus("notice");
  const rand = prng(SEED + 2);
  seeds.forEach((seed, i) => checkNotice(seed, `seed ${i}`));
  for (let i = 0; i < ITERATIONS; i++) {
    const seed = seeds[Math.floor(rand() * seeds.length)] ?? "";
    checkNotice(mutateText(seed, rand), `FUZZ_SEED=${SEED} iteration ${i}`);
  }
});
