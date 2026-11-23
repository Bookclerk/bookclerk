/**
 * Fuzzing for what the plugin reads from the network and the operator, as
 * the fuzzing ADR asks of boundaries outside the Rust workspace
 * (docs/adr/untrusted-input-fuzzing.md, decision 6).
 *
 * Each seed in `fuzz/address/` (echo answers), `fuzz/api/` (Cloudflare API
 * answers) and `fuzz/hostname/` (host name settings) is replayed, then
 * seeded mutations are checked against oracles written apart from the
 * parsers:
 *
 * - echo answers: Node's `net.isIPv4` and `net.isIPv6` (less zones) for
 *   what is an address, the WHATWG URL serializer for the canonical IPv6
 *   form, and Node's `net.BlockList` over the special-purpose ranges for
 *   what is public;
 * - API answers: the envelope, zone and record rules restated over
 *   `JSON.parse`'s value;
 * - host names: RFC 1035's label rules restated character by character.
 *
 * `FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run; a failure prints the
 * seed, iteration and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { BlockList, isIPv4, isIPv6 } from "node:net";
import { test } from "node:test";

import {
  MAX_HOSTNAME_BYTES,
  isPublicAddress,
  parseEchoAnswer,
  parseHostname,
  zoneCandidates,
  type Family,
} from "../src/address.ts";
import { DnsError, parseEnvelope, parseRecords, parseZone } from "../src/dns.ts";

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
const SEED = wholeEnv("FUZZ_SEED", 0xd115, 0xffff_ffff);

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
 * Mutates text: replaces, inserts or deletes characters and splices in
 * fragments.
 *
 * @param input - Seed text.
 * @param fragments - Pieces worth splicing in.
 * @param rand - PRNG.
 * @returns A mutated copy.
 */
function mutateText(input: string, fragments: readonly string[], rand: () => number): string {
  let text = input;
  const edits = 1 + Math.floor(rand() * 4);
  for (let e = 0; e < edits; e++) {
    const at = Math.floor(rand() * (text.length + 1));
    const piece = fragments[Math.floor(rand() * fragments.length)] as string;
    switch (Math.floor(rand() * 4)) {
      case 0:
        text = text.slice(0, at) + piece + text.slice(at);
        break;
      case 1:
        text = text.slice(0, at) + piece + text.slice(at + 1);
        break;
      case 2:
        text = text.slice(0, at) + text.slice(at + 1 + Math.floor(rand() * 3));
        break;
      default:
        text = text.slice(0, at) + String.fromCharCode(Math.floor(rand() * 128)) + text.slice(at);
    }
  }
  return text;
}

/**
 * Runs a check over each seed and its mutations, naming the input on a
 * failure.
 *
 * @param name - The corpus.
 * @param fragments - Pieces worth splicing in.
 * @param check - The oracle comparison for one input.
 */
function fuzz(name: string, fragments: readonly string[], check: (input: string) => void): void {
  const seeds = corpus(name);
  const rand = prng(SEED);
  const run = (input: string, where: string): void => {
    try {
      check(input);
    } catch (err) {
      throw new Error(
        `${where} (FUZZ_SEED=${SEED}) input ${JSON.stringify(input)}: ${String(err)}`,
        { cause: err },
      );
    }
  };
  for (const seed of seeds) run(seed.text, seed.file);
  for (let i = 0; i < ITERATIONS; i++) {
    const seed = seeds[i % seeds.length] as Seed;
    run(mutateText(seed.text, fragments, rand), `${seed.file} iteration ${i}`);
  }
}

/** The special-purpose ranges, for the public-address oracle. */
const SPECIAL = new BlockList();
for (const [network, prefix] of [
  ["0.0.0.0", 8],
  ["10.0.0.0", 8],
  ["100.64.0.0", 10],
  ["127.0.0.0", 8],
  ["169.254.0.0", 16],
  ["172.16.0.0", 12],
  ["192.0.0.0", 24],
  ["192.0.2.0", 24],
  ["192.88.99.0", 24],
  ["192.168.0.0", 16],
  ["198.18.0.0", 15],
  ["198.51.100.0", 24],
  ["203.0.113.0", 24],
  ["224.0.0.0", 4],
  ["240.0.0.0", 4],
] as const) {
  SPECIAL.addSubnet(network, prefix, "ipv4");
}

/** The IPv6 ranges inside `2000::/3` that are not public. */
const SPECIAL_V6 = new BlockList();
for (const [network, prefix] of [
  ["2001::", 23],
  ["2001:db8::", 32],
  ["2002::", 16],
  ["3fff::", 20],
] as const) {
  SPECIAL_V6.addSubnet(network, prefix, "ipv6");
}

/** Global unicast, the only IPv6 space that may be public. */
const GLOBAL_V6 = new BlockList();
GLOBAL_V6.addSubnet("2000::", 3, "ipv6");

test("echo answers match Node's address parsers and the URL serializer", () => {
  fuzz(
    "address",
    [
      "0",
      "1",
      "9",
      "f",
      "F",
      "255",
      "256",
      "00",
      ".",
      ":",
      "::",
      "%",
      " ",
      "\n",
      "\r",
      "1.2.3.4",
      "ffff",
    ],
    (input) => {
      // A seed is the family digit, a space, then the body with `\n` as `\\n`.
      const family = (input.startsWith("4") ? 4 : 6) as Family;
      const body = input.slice(2).replaceAll("\\n", "\n");
      const text = body.endsWith("\n") ? body.slice(0, -1) : body;
      let want: string | null = null;
      if (family === 4 && isIPv4(text)) want = text;
      if (family === 6 && isIPv6(text) && !text.includes("%")) {
        want = new URL(`http://[${text}]/`).hostname.slice(1, -1);
      }
      assert.equal(parseEchoAnswer(body, family), want);
      if (want === null) return;
      const special =
        family === 4
          ? SPECIAL.check(want, "ipv4")
          : !GLOBAL_V6.check(want, "ipv6") || SPECIAL_V6.check(want, "ipv6");
      assert.equal(isPublicAddress(want, family), !special);
    },
  );
});

/**
 * Whether `id` is 32 lowercase hex digits, by character.
 *
 * @param id - Candidate.
 * @returns The answer.
 */
function hexId(id: unknown): boolean {
  return (
    typeof id === "string" &&
    id.length === 32 &&
    [...id].every((c) => "0123456789abcdef".includes(c))
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

test("API answers follow the envelope, zone and record rules", () => {
  fuzz(
    "api",
    [
      '"',
      "{",
      "}",
      "[",
      "]",
      ",",
      ":",
      "null",
      "true",
      "false",
      "1",
      '"A"',
      '"CNAME"',
      '"active"',
      "\\u0000",
    ],
    (input) => {
      let value: unknown;
      let json = true;
      try {
        value = JSON.parse(input);
      } catch {
        json = false;
      }
      for (const status of [200, 403]) {
        if (!json || !isObject(value)) {
          assert.throws(() => parseEnvelope(status, input), DnsError);
          continue;
        }
        const envelope = parseEnvelope(status, input);
        if (status === 200 && value.success === true) {
          assert.deepEqual(envelope, { ok: true, result: value.result });
        } else {
          const errors = value.errors;
          const first: unknown = Array.isArray(errors) ? errors[0] : undefined;
          const message = isObject(first) && typeof first.message === "string" ? first.message : "";
          assert.deepEqual(envelope, { ok: false, message: message.slice(0, 200) });
        }
      }
      if (!json || !isObject(value)) return;
      const result = value.result;
      for (const name of ["example.com", "books.example.com"]) {
        if (!Array.isArray(result)) {
          assert.throws(() => parseZone(result, name), DnsError);
          assert.throws(() => parseRecords(result, name), DnsError);
          continue;
        }
        const zone = result.find(
          (z: unknown) => isObject(z) && hexId(z.id) && z.name === name && z.status === "active",
        ) as Record<string, unknown> | undefined;
        assert.equal(parseZone(result, name), zone === undefined ? null : zone.id);
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
            ttl: typeof r.ttl === "number" ? r.ttl : 0,
            comment: typeof r.comment === "string" ? r.comment : "",
          }));
        assert.deepEqual(parseRecords(result, name), records);
      }
    },
  );
});

/**
 * Whether a label is LDH, 1 to 63 long, without an edge dash, by character.
 *
 * @param label - Candidate.
 * @returns The answer.
 */
function ldhLabel(label: string): boolean {
  if (label.length < 1 || label.length > 63) return false;
  if (label[0] === "-" || label[label.length - 1] === "-") return false;
  return [...label].every((c) => (c >= "a" && c <= "z") || (c >= "0" && c <= "9") || c === "-");
}

test("host names follow RFC 1035's label rules", () => {
  fuzz(
    "hostname",
    [".", "-", "..", "xn--", "A", "Z", "0", "9", " ", "_", "é", "a".repeat(63), "com", "1"],
    (input) => {
      let name = input.trim().toLowerCase();
      if (name.endsWith(".")) name = name.slice(0, -1);
      const labels = name.split(".");
      const top = labels[labels.length - 1] as string;
      const valid =
        name.length <= MAX_HOSTNAME_BYTES &&
        labels.length >= 2 &&
        labels.every((label) => ldhLabel(label) && !label.startsWith("xn--")) &&
        top[0] !== undefined &&
        top[0] >= "a" &&
        top[0] <= "z";
      const parsed = parseHostname(input);
      assert.equal(parsed, valid ? name : null);
      if (parsed === null) return;
      const zones = zoneCandidates(parsed);
      assert.equal(zones[0], parsed);
      assert.equal(zones.length, labels.length - 1);
      for (const zone of zones) assert.ok(parsed === zone || parsed.endsWith(`.${zone}`));
    },
  );
});
