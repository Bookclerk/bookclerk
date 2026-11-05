/**
 * Fuzzing for the cover URL policy, as the fuzzing ADR asks of boundaries
 * outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
 * decision 6).
 *
 * `checkUrl` decides which URLs the fetcher requests: the host's URLs and
 * every redirect `Location` an upstream server sends. Each seed in
 * `fuzz/url/` and `fuzz/location/` is replayed, then seeded text mutations
 * are checked against an independent oracle written from the policy and the
 * WHATWG URL rules rather than from the fetcher's code. `FUZZ_SEED` and
 * `FUZZ_ITERATIONS` pick another run; a failure prints the seed, iteration
 * and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import { MAX_REDIRECTS, checkUrl, fetchOne, type CoverSink, type Fetch } from "../src/fetcher.ts";
import { VERSION } from "../src/version.ts";

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0xc0de);

/** Mutated inputs checked per corpus. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 3000);

/** Longest URL the policy accepts, in UTF-16 code units. */
const MAX_URL = 4096;

/**
 * Reads one corpus directory.
 *
 * @param name - Directory under `fuzz/`.
 * @returns Each seed's text, by file name order.
 */
function corpus(name: string): string[] {
  const dir = new URL(`../fuzz/${name}/`, import.meta.url);
  return readdirSync(dir)
    .filter((file) => file.endsWith(".txt"))
    .sort()
    .map((file) => readFileSync(new URL(file, dir), "utf8"));
}

/** Seeds for URLs the host asks for. */
const URLS = corpus("url");

/** Seeds for redirect `Location` values, relative or absolute. */
const LOCATIONS = corpus("location");

/** Fragments a mutation splices in: URL syntax, address forms and odd text. */
const FRAGMENTS: readonly string[] = [
  ":",
  "/",
  "//",
  "@",
  ":443",
  ":8443",
  ":0",
  "[",
  "]",
  "[::1]",
  ".",
  "..",
  "%",
  "%2e",
  "%00",
  "#",
  "?",
  "\\",
  " ",
  "\t",
  "\n",
  "\u007f",
  "http",
  "https",
  "HTTPS://",
  "user:pw@",
  "127.0.0.1",
  "0x7f",
  "2130706433",
  "10",
  "a",
  "example",
  "xn--",
  "ü",
  "📚",
  "\uD800",
  "x".repeat(MAX_URL),
];

/**
 * Seeds a small deterministic PRNG (mulberry32).
 *
 * @param seed - Seed value.
 * @returns A function returning floats in `[0, 1)`.
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
 * Picks one element.
 *
 * @param list - Non-empty list.
 * @param rand - The PRNG.
 * @returns A random element.
 */
function pick<T>(list: readonly T[], rand: () => number): T {
  return list[Math.floor(rand() * list.length)] as T;
}

/**
 * Applies one to three random splices or deletions.
 *
 * @param input - Seed text.
 * @param rand - The PRNG.
 * @returns The mutated text.
 */
function mutate(input: string, rand: () => number): string {
  let out = input;
  for (let n = 1 + Math.floor(rand() * 3); n > 0; n--) {
    const at = Math.floor(rand() * (out.length + 1));
    out =
      rand() < 0.6
        ? out.slice(0, at) + pick(FRAGMENTS, rand) + out.slice(at)
        : out.slice(0, at) + out.slice(at + 1 + Math.floor(rand() * 6));
  }
  return out;
}

/**
 * Whether a WHATWG-parsed hostname is an address rather than a name.
 *
 * The URL standard parses a host as IPv4 exactly when its last label (after
 * one trailing dot) is a decimal or `0x` hex number, and as IPv6 when it is
 * bracketed; the parser then serializes either as an address.
 *
 * @param hostname - `URL.hostname`.
 * @returns Whether it names an address.
 */
function isAddress(hostname: string): boolean {
  if (hostname.startsWith("[")) return true;
  const labels = hostname.split(".");
  if (labels.length > 1 && labels[labels.length - 1] === "") labels.pop();
  const last = labels[labels.length - 1] ?? "";
  return /^(?:[0-9]+|0x[0-9a-f]*)$/i.test(last);
}

/**
 * The oracle: what `checkUrl` must answer for `input`.
 *
 * @param input - A URL as text.
 * @returns The canonical URL when the policy allows it, else `undefined`.
 */
function oracle(input: string): string | undefined {
  if (input.length > MAX_URL) return undefined;
  for (const ch of input) {
    const code = ch.codePointAt(0) ?? 0;
    if (code <= 0x20 || code === 0x7f) return undefined;
  }
  let url: URL;
  try {
    url = new URL(input);
  } catch {
    return undefined;
  }
  if (url.protocol !== "https:") return undefined;
  if (url.port !== "" || url.username !== "" || url.password !== "") return undefined;
  // A real hostname: labels separated by single dots, at most one trailing.
  if (!/^(?:[^.]+\.)*[^.]+\.?$/.test(url.hostname) || isAddress(url.hostname)) return undefined;
  return url.href;
}

/**
 * Checks `checkUrl` against the oracle and its own invariants for one input.
 *
 * @param input - A URL as text.
 * @param where - How to reproduce it, for the failure message.
 */
function checkOne(input: string, where: string): void {
  const got = checkUrl(input);
  const want = oracle(input);
  assert.equal(got, want, `${where}: ${JSON.stringify(input)}`);
  if (got === undefined) return;
  const url = new URL(got);
  assert.equal(url.href, got, `${where}: not canonical`);
  assert.equal(checkUrl(got), got, `${where}: not idempotent`);
  assert.equal(url.protocol, "https:", where);
  assert.ok(!isAddress(url.hostname), `${where}: address ${url.hostname}`);
}

test("checkUrl replays its corpora against the oracle", () => {
  for (const [i, url] of URLS.entries()) checkOne(url, `url seed ${i}`);
  const base = "https://a.example/dir/start";
  for (const [i, location] of LOCATIONS.entries()) {
    let resolved: string | undefined;
    try {
      resolved = new URL(location, base).href;
    } catch {
      resolved = undefined;
    }
    if (resolved !== undefined) checkOne(resolved, `location seed ${i}`);
  }
});

test("checkUrl agrees with the oracle on mutated URLs", () => {
  const rand = prng(SEED);
  for (let i = 0; i < ITERATIONS; i++) {
    checkOne(mutate(pick(URLS, rand), rand), `FUZZ_SEED=${SEED} iteration ${i}`);
  }
});

/** A sink that drains and keeps every body. */
function sink(): { sink: CoverSink; keys: string[] } {
  const keys: string[] = [];
  return {
    keys,
    sink: {
      async put(key, body) {
        await new Response(body).arrayBuffer();
        keys.push(key);
        return { key };
      },
    },
  };
}

test("fetchOne requests only policy URLs across mutated redirect chains", async () => {
  const rand = prng(SEED ^ 0x9e3779b9);
  const rounds = Math.max(1, Math.floor(ITERATIONS / 10));
  for (let i = 0; i < rounds; i++) {
    const where = `FUZZ_SEED=${SEED} round ${i}`;
    const hops = Math.floor(rand() * (MAX_REDIRECTS + 3));
    const locations = Array.from({ length: hops }, () =>
      rand() < 0.5 ? pick(LOCATIONS, rand) : mutate(pick(LOCATIONS, rand), rand),
    );
    const requested: string[] = [];
    const fetch: Fetch = (url, init) => {
      requested.push(url);
      assert.equal(init.redirect, "manual", where);
      const location = locations[requested.length - 1];
      if (location !== undefined) {
        return Promise.resolve(
          new Response(null, {
            status: pick([301, 302, 303, 307, 308], rand),
            headers: { location },
          }),
        );
      }
      return Promise.resolve(
        new Response(new Uint8Array([1]), { headers: { "content-type": "image/png" } }),
      );
    };
    const { sink: into, keys } = sink();
    const answer = await fetchOne(
      { fetch, version: VERSION },
      "https://a.example/start",
      0,
      8,
      into,
    );
    assert.ok(requested.length <= MAX_REDIRECTS + 1, `${where}: ${requested.length} requests`);
    for (const url of requested) {
      assert.equal(oracle(url), url, `${where}: requested ${JSON.stringify(url)}`);
    }
    // Every hop the fetcher followed was a policy URL, resolved against the
    // URL that redirected to it.
    for (let hop = 1; hop < requested.length; hop++) {
      const resolved = new URL(locations[hop - 1] as string, requested[hop - 1]).href;
      assert.equal(requested[hop], resolved, `${where}: hop ${hop}`);
    }
    assert.equal(answer.stored, keys.length === 1, where);
    if (answer.stored) assert.ok(requested.length === hops + 1, where);
  }
});
