// Fuzzing for the Worker's untrusted-input parsers, as the fuzzing ADR asks
// of boundaries outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
// decision 6): each seed in fuzz/<target>/ is replayed, then seeded mutations
// of the seeds are checked against an independent oracle. Run with `npm test`;
// FUZZ_SEED and FUZZ_ITERATIONS pick another run, and a failure prints the
// seed and input that reproduce it.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import { pickEncoding } from "./worker.ts";
import type { Encoding } from "./worker.ts";

/** This directory. */
const HERE = fileURLToPath(new URL(".", import.meta.url));

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0x5eed);

/** Mutated inputs checked per target. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 20000);

/** Fragments mutations splice in: list and parameter syntax, codings, odd qvalues and whitespace. */
const FRAGMENTS = [",", ";", "=", " ", "\t", "q", "Q", "q=", "br", "BR", "gzip", "*", "identity", "0", "1", ".", "0.5", "1.000", "1.0001", ".5", "0x1", "1e0", "-0", " ", "\u0000", "é"];

/**
 * Seeds a small deterministic PRNG (mulberry32).
 *
 * @param seed - Any 32-bit integer.
 * @returns A generator of floats in [0, 1).
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
 * Applies one to four random edits to an input.
 *
 * @param input - The input to mutate.
 * @param seeds - Every seed, for splicing.
 * @param rand - The PRNG.
 * @returns The mutated input.
 */
function mutate(input: string, seeds: readonly string[], rand: () => number): string {
  let out = input;
  const pick = (list: readonly string[]): string => list[Math.floor(rand() * list.length)] ?? "";
  for (let n = 1 + Math.floor(rand() * 4); n > 0; n--) {
    const at = Math.floor(rand() * (out.length + 1));
    switch (Math.floor(rand() * 5)) {
      case 0:
        out = out.slice(0, at) + pick(FRAGMENTS) + out.slice(at);
        break;
      case 1:
        out = out.slice(0, at) + out.slice(at + 1 + Math.floor(rand() * 4));
        break;
      case 2:
        out = out.slice(0, at) + out.slice(at).toUpperCase();
        break;
      case 3:
        out = `${out}, ${pick(seeds)}`;
        break;
      default:
        out = out.slice(0, at) + out.slice(Math.floor(rand() * (out.length + 1)));
    }
  }
  return out;
}

/**
 * Reads a qvalue the way RFC 9110 §12.4.2 spells it, without a regular
 * expression: "0" or "1", optionally followed by "." and up to three digits,
 * which must be zeros after a "1".
 *
 * @param text - The parameter value.
 * @returns The weight, or 0 for anything else.
 */
function oracleQvalue(text: string): number {
  if (text !== "0" && text !== "1" && !text.startsWith("0.") && !text.startsWith("1.")) return 0;
  const decimals = text.slice(2);
  if (decimals.length > 3) return 0;
  for (const c of decimals) {
    if (c < "0" || c > "9") return 0;
    if (text[0] === "1" && c !== "0") return 0;
  }
  return text[0] === "1" ? 1 : Number(`0.${decimals || "0"}`);
}

/**
 * The coding an Accept-Encoding header should get, computed independently of
 * worker.ts: every member's weight is collected first, then Brotli and gzip
 * are compared with `*` standing in for an unlisted coding.
 *
 * @param header - The header.
 * @returns The expected choice.
 */
function oracleEncoding(header: string): Encoding | null {
  const weights: Record<string, number> = Object.create(null);
  for (const member of header.split(",")) {
    const [name = "", ...params] = member.split(";");
    const coding = name.trim().toLowerCase();
    if (coding === "") continue;
    const qs = params.filter((p) => (p.split("=")[0] ?? "").trim().toLowerCase() === "q");
    const last = qs.at(-1);
    const weight = last === undefined ? 1 : last.includes("=") ? oracleQvalue(last.slice(last.indexOf("=") + 1).trim()) : 0;
    weights[coding] = Math.max(weights[coding] ?? 0, weight);
  }
  const of = (coding: string): number => (coding in weights ? (weights[coding] ?? 0) : (weights["*"] ?? 0));
  const br = of("br");
  const gzip = of("gzip");
  if (br === 0 && gzip === 0) return null;
  return br >= gzip ? "br" : "gzip";
}

/**
 * Reads a target's seed corpus.
 *
 * @param target - The directory under fuzz/.
 * @returns The seeds.
 */
function corpus(target: string): string[] {
  const dir = join(HERE, "fuzz", target);
  return readdirSync(dir)
    .sort()
    .map((name) => readFileSync(join(dir, name), "utf8"));
}

test("pickEncoding agrees with the oracle on its seed corpus", () => {
  const seeds = corpus("accept_encoding");
  assert.ok(seeds.length >= 10);
  for (const seed of seeds) assert.equal(pickEncoding(seed), oracleEncoding(seed), JSON.stringify(seed));
});

test("pickEncoding agrees with the oracle on mutated inputs", () => {
  const seeds = corpus("accept_encoding");
  const rand = prng(SEED);
  for (let i = 0; i < ITERATIONS; i++) {
    const input = mutate(seeds[Math.floor(rand() * seeds.length)] ?? "", seeds, rand);
    const got = pickEncoding(input);
    assert.ok(got === null || got === "br" || got === "gzip", JSON.stringify(input));
    assert.equal(got, oracleEncoding(input), `FUZZ_SEED=${SEED} iteration ${i}: ${JSON.stringify(input)}`);
  }
});
