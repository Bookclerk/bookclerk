/**
 * Fuzzing for the relay protocol, as the fuzzing ADR asks of boundaries
 * outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
 * decision 6).
 *
 * Both ends parse what the other sends: the plugin parses the relay's
 * frames, request heads and notices, and the relay parses the plugin's
 * frames and response heads, and each parses the other's live frames. Each
 * seed in `fuzz/frame/` (hex bytes), `fuzz/head/` (head JSON),
 * `fuzz/notice/` (notice text and client addresses) and `fuzz/live/` (hex
 * live frames) is replayed, then seeded mutations are checked against
 * oracles written from the protocol's rules rather than from the parser:
 *
 * - frames: an independent decoder over the whole input, and the same
 *   answer whatever the chunk boundaries;
 * - heads: the RFC 9110 token and field-value grammar and the shared
 *   limits, plus a round trip through `encodeFrame`;
 * - client addresses: Node's `net.isIP`, less IPv6 zones;
 * - live frames: an independent decoder restating the layout and the
 *   sendable close codes, plus a byte-exact round trip through
 *   `encodeLiveFrame`.
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
  MAX_HEADERS,
  MAX_HEADER_BYTES,
  MAX_HEAD_BYTES,
  MAX_METHOD_BYTES,
  MAX_TARGET_BYTES,
  MAX_LIVE_FRAME_BYTES,
  encodeFrame,
  encodeLiveFrame,
  isIpAddress,
  isRequestId,
  parseLiveFrame,
  parseNotice,
  parseRequestHead,
  parseResponseHead,
} from "../src/protocol.ts";

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
const SEED = wholeEnv("FUZZ_SEED", 0x7e1a, 0xffff_ffff);

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
  const step = new FrameReader().push(bytes);
  const whole: FrameVerdict =
    step.kind === "head" ? { ok: true, json: step.json, rest: [...step.rest] } : { ok: false };
  assert.deepEqual(whole, expected, `${label} whole`);
}

/**
 * Fields that describe one connection (RFC 9110 section 7.6.1, plus the
 * legacy and proxy ones): neither side may pass them on. Written out here
 * rather than imported, so the oracle does not share the parser's list.
 */
const CONNECTION_FIELDS = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

/**
 * Whether a request head may not carry `name`: the provider's own fields,
 * forwarding fields and the authority, which only the relay and the library
 * set.
 *
 * @param name - Lowercase field name.
 * @returns `true` when a request head must not carry it.
 */
function relayOnlyField(name: string): boolean {
  return (
    name.startsWith("cf-") ||
    name.startsWith("x-forwarded-") ||
    ["cdn-loop", "forwarded", "host", "x-real-ip"].includes(name)
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
    if (CONNECTION_FIELDS.has(name)) return false;
    if (request && relayOnlyField(name)) return false;
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
  const { method, target, headers, client, upgrade } = value as Record<string, unknown>;
  if (typeof method !== "string" || method.length > MAX_METHOD_BYTES || !TCHAR.test(method))
    return false;
  // An upgrade is only ever a GET.
  if (upgrade !== undefined && (upgrade !== true || method !== "GET")) return false;
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
  if (typeof status !== "number" || !Number.isInteger(status)) return false;
  // 101 is the one informational answer: the library accepted an upgrade.
  if (status !== 101 && (status < 200 || status > 599)) return false;
  return headersOk(headers, false);
}

/**
 * The protocol's view of a header list the oracle accepted: each field's
 * name and value, in order, without other members.
 *
 * @param value - The `headers` member.
 * @returns The fields.
 */
function fieldsOf(value: unknown): Array<{ name: string; value: string }> {
  return (value as Array<{ name: string; value: string }>).map(({ name, value: text }) => ({
    name,
    value: text,
  }));
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
    // An accepted head keeps exactly the protocol's members of the input.
    const input = JSON.parse(json) as Record<string, unknown>;
    const projected: Record<string, unknown> = {
      method: input["method"],
      target: input["target"],
      headers: fieldsOf(input["headers"]),
    };
    if (input["client"] !== undefined) projected["client"] = input["client"];
    if (input["upgrade"] !== undefined) projected["upgrade"] = input["upgrade"];
    assert.deepEqual(request, projected, `${label} request projection`);
    const step = new FrameReader().push(encodeFrame(request));
    assert.equal(step.kind, "head");
    if (step.kind === "head") assert.deepEqual(parseRequestHead(step.json), request, label);
    // The parsed head carries only the members the protocol defines.
    assert.deepEqual(
      Object.keys(request).filter(
        (k) => !["method", "target", "headers", "client", "upgrade"].includes(k),
      ),
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
    const input = JSON.parse(json) as Record<string, unknown>;
    assert.deepEqual(
      response,
      { status: input["status"], headers: fieldsOf(input["headers"]) },
      `${label} response projection`,
    );
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
  let id: unknown;
  if (text.length <= 256) {
    try {
      const value = JSON.parse(text) as unknown;
      id = (value as Record<string, unknown> | null)?.["id"];
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
  if (typeof notice !== "string") {
    assert.deepEqual(notice, { t: "request", id }, label);
    assert.equal(isRequestId(notice.id), true);
  }
}

/** What the oracle says a live frame decodes to. */
type LiveVerdict =
  | { ok: true; op: "text"; id: string; text: string }
  | { ok: true; op: "binary"; id: string; data: number[] }
  | { ok: true; op: "close"; id: string; code: number; reason: string }
  | { ok: false };

/**
 * Decodes UTF-8 the way the oracle trusts: a byte-exact round trip.
 *
 * @param bytes - Candidate UTF-8.
 * @returns The text, or `null` when the bytes are not UTF-8.
 */
function utf8(bytes: Uint8Array): string | null {
  const text = Buffer.from(bytes).toString("utf8");
  return Buffer.from(text, "utf8").equals(Buffer.from(bytes)) ? text : null;
}

/**
 * The independent live-frame decoder: op byte, id length, ASCII id, then
 * the payload; close codes a WebSocket endpoint may send (RFC 6455
 * section 7.4, less the reserved and local-only ones).
 *
 * @param bytes - The message.
 * @returns What it decodes to.
 */
function liveOracle(bytes: Uint8Array): LiveVerdict {
  if (bytes.length < 2 || bytes.length > MAX_LIVE_FRAME_BYTES) return { ok: false };
  const n = bytes[1] ?? 0;
  if (n < 1 || n > 64 || bytes.length < 2 + n) return { ok: false };
  const id = Buffer.from(bytes.subarray(2, 2 + n)).toString("latin1");
  if (!/^[0-9A-Za-z-]+$/.test(id)) return { ok: false };
  const payload = bytes.subarray(2 + n);
  switch (bytes[0]) {
    case 1: {
      const text = utf8(payload);
      return text === null ? { ok: false } : { ok: true, op: "text", id, text };
    }
    case 2:
      return { ok: true, op: "binary", id, data: [...payload] };
    case 3: {
      if (payload.length < 2 || payload.length > 125) return { ok: false };
      const code = (payload[0] ?? 0) * 256 + (payload[1] ?? 0);
      const sendable =
        (code >= 1000 && code <= 1003) ||
        (code >= 1007 && code <= 1014) ||
        (code >= 3000 && code <= 4999);
      const reason = utf8(payload.subarray(2));
      if (!sendable || reason === null) return { ok: false };
      return { ok: true, op: "close", id, code, reason };
    }
    default:
      return { ok: false };
  }
}

/**
 * Checks one live frame against the oracle and the round trip.
 *
 * @param bytes - The input.
 * @param label - Reproduction note for failures.
 */
function checkLive(bytes: Uint8Array, label: string): void {
  const expected = liveOracle(bytes);
  const frame = parseLiveFrame(bytes);
  const got: LiveVerdict =
    typeof frame === "string"
      ? { ok: false }
      : frame.op === "binary"
        ? { ok: true, op: "binary", id: frame.id, data: [...frame.data] }
        : { ok: true, ...frame };
  assert.deepEqual(got, expected, `${label} live ${Buffer.from(bytes).toString("hex")}`);
  if (typeof frame !== "string") {
    assert.deepEqual(
      Buffer.from(encodeLiveFrame(frame)).toString("hex"),
      Buffer.from(bytes).toString("hex"),
      `${label} live round trip`,
    );
  }
}

test("live frame seeds replay and mutations match the independent decoder", () => {
  const seeds = corpus("live").map(fromHex);
  const rand = prng(SEED + 3);
  seeds.forEach((seed, i) => checkLive(seed, `seed ${i}`));
  for (let i = 0; i < ITERATIONS; i++) {
    const seed = seeds[Math.floor(rand() * seeds.length)] ?? new Uint8Array();
    checkLive(mutateBytes(seed, rand), `FUZZ_SEED=${SEED} iteration ${i}`);
  }
  // A frame past the host's socket frame limit is refused unread.
  const huge = new Uint8Array(MAX_LIVE_FRAME_BYTES + 1);
  huge.set([2, 2, 0x6f, 0x31]);
  assert.equal(typeof parseLiveFrame(huge), "string");
});

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
  const seeds = corpus("head").map((seed) => seed.text);
  const rand = prng(SEED + 1);
  seeds.forEach((seed, i) => checkHead(seed, `seed ${i}`));
  for (let i = 0; i < ITERATIONS; i++) {
    const seed = seeds[Math.floor(rand() * seeds.length)] ?? "";
    checkHead(mutateText(seed, rand), `FUZZ_SEED=${SEED} iteration ${i}`);
  }
});

test("notice and address seeds replay and mutations match the oracles", () => {
  const seeds = corpus("notice").map((seed) => seed.text);
  const rand = prng(SEED + 2);
  seeds.forEach((seed, i) => checkNotice(seed, `seed ${i}`));
  for (let i = 0; i < ITERATIONS; i++) {
    const seed = seeds[Math.floor(rand() * seeds.length)] ?? "";
    checkNotice(mutateText(seed, rand), `FUZZ_SEED=${SEED} iteration ${i}`);
  }
});
