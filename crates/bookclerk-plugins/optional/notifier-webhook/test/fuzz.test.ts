/**
 * Fuzzing for what the webhook notifier reads from outside, as the fuzzing
 * ADR asks of boundaries outside the Rust workspace
 * (docs/adr/untrusted-input-fuzzing.md, decision 6).
 *
 * - A patron's address document (`parseAddress`), corpus `fuzz/address/`.
 *   The oracle restates the rules with other tools: the byte cap through
 *   `Buffer`, the secret through Node's base64 decoder and a canonical
 *   re-encoding, and the URL rules from the WHATWG parser's parts. An
 *   accepted address re-parses to the trimmed values, its label is the
 *   URL's host, and its key is the decoded secret.
 * - A receiver's `Retry-After` (`parseRetryAfter`), corpus
 *   `fuzz/retry_after/`. The oracle reads delta seconds as digits and an
 *   HTTP date exactly when `Date.prototype.toUTCString` prints it back
 *   unchanged (it prints IMF-fixdate), and checks the cap.
 *
 * Each seed is replayed, then seeded mutations are checked. `FUZZ_SEED` and
 * `FUZZ_ITERATIONS` pick another run; a failure prints the seed, iteration
 * and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import {
  MAX_ADDRESS_BYTES,
  MAX_RETRY_AFTER_MS,
  MAX_SECRET_BYTES,
  MAX_URL_CHARS,
  MIN_SECRET_BYTES,
  WebhookFailure,
  parseAddress,
  parseRetryAfter,
} from "../src/webhook.ts";

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0x5eed);

/** Mutated inputs checked per corpus. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 3000);

/** A fixed "now" for `Retry-After` dates, Unix milliseconds. */
const NOW = Date.UTC(2026, 10, 23, 5, 0, 0);

/**
 * Every seed of one corpus, by file name.
 *
 * @param dir - Corpus directory under `fuzz/`.
 * @returns File name and text, sorted by name.
 */
function corpus(dir: string): [string, string][] {
  const root = new URL(`../fuzz/${dir}/`, import.meta.url);
  return readdirSync(root)
    .sort()
    .map((name) => [name, readFileSync(new URL(name, root), "utf8")]);
}

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
 * Applies one random text edit: a splice of a fragment, a deletion, a
 * duplication or a character swap.
 *
 * @param text - The input.
 * @param fragments - Snippets worth splicing in.
 * @param rand - The PRNG.
 * @returns The edited text.
 */
function mutate(text: string, fragments: readonly string[], rand: () => number): string {
  const at = Math.floor(rand() * (text.length + 1));
  const span = Math.floor(rand() * 8);
  switch (Math.floor(rand() * 4)) {
    case 0:
      return (
        text.slice(0, at) +
        (fragments[Math.floor(rand() * fragments.length)] ?? "") +
        text.slice(at)
      );
    case 1:
      return text.slice(0, at) + text.slice(at + span);
    case 2:
      return text.slice(0, at) + text.slice(at, at + span).repeat(2) + text.slice(at + span);
    default:
      return (
        text.slice(0, at) + String.fromCharCode(Math.floor(rand() * 0x80)) + text.slice(at + 1)
      );
  }
}

/**
 * The key a secret stands for, by the oracle's rules, or `null` when the
 * secret is refused.
 *
 * @param raw - The secret as written.
 * @returns The key bytes, or `null`.
 */
function oracleKey(raw: string): Buffer | null {
  const text = raw.trim();
  if (!text.startsWith("whsec_")) return null;
  const body = text.slice("whsec_".length);
  const bare = body.replace(/=+$/, "");
  const padding = body.length - bare.length;
  if (bare === "" || /[^A-Za-z0-9+/]/.test(bare)) return null;
  // Padding, when written, completes the last group of four exactly.
  if (padding > 0 && (bare.length + padding) % 4 !== 0) return null;
  const key = Buffer.from(bare, "base64");
  // Node decodes leniently; the text must be the canonical encoding of the
  // bytes it gave.
  if (key.toString("base64").replace(/=+$/, "") !== bare) return null;
  if (key.length < MIN_SECRET_BYTES || key.length > MAX_SECRET_BYTES) return null;
  return key;
}

/**
 * Whether a URL passes the oracle's rules.
 *
 * @param raw - The URL as written.
 * @returns The parsed URL, or `null` when refused.
 */
function oracleUrl(raw: string): URL | null {
  const text = raw.trim();
  if (text === "" || text.length > MAX_URL_CHARS || text.includes("#")) return null;
  const url = URL.parse(text);
  if (url === null) return null;
  if (!["http:", "https:"].includes(url.protocol)) return null;
  if (url.username || url.password || !url.hostname) return null;
  return url;
}

/**
 * Checks `parseAddress` on one input against the oracle.
 *
 * @param input - An address document.
 */
function checkAddress(input: string): void {
  let doc: unknown;
  let parsed = false;
  try {
    doc = JSON.parse(input);
    parsed = true;
  } catch {
    // Not JSON: refused below.
  }
  const fields =
    parsed && typeof doc === "object" && doc !== null && !Array.isArray(doc)
      ? (doc as Record<string, unknown>)
      : null;
  const onlyKnown =
    fields !== null && Object.keys(fields).every((k) => k === "url" || k === "secret");
  const url =
    fields !== null && typeof fields["url"] === "string" ? oracleUrl(fields["url"]) : null;
  const key =
    fields !== null && typeof fields["secret"] === "string" ? oracleKey(fields["secret"]) : null;
  const accept =
    Buffer.byteLength(input, "utf8") <= MAX_ADDRESS_BYTES &&
    onlyKnown &&
    url !== null &&
    key !== null;

  let got: ReturnType<typeof parseAddress> | undefined;
  try {
    got = parseAddress(input);
  } catch (err) {
    assert.ok(err instanceof WebhookFailure, `unexpected ${String(err)}`);
    assert.equal(err.code, "invalid_params");
    assert.ok(!accept, "refused an address the oracle accepts");
    return;
  }
  assert.ok(accept, "accepted an address the oracle refuses");
  assert.ok(fields !== null && url !== null && key !== null);
  assert.equal(got.label, url.host);
  assert.equal(got.endpoint.url.href, url.href);
  assert.deepEqual(Buffer.from(got.endpoint.key), key);
  assert.deepEqual(JSON.parse(got.addressJson), {
    url: String(fields["url"]).trim(),
    secret: String(fields["secret"]).trim(),
  });
  // Normalising twice changes nothing.
  assert.equal(parseAddress(got.addressJson).addressJson, got.addressJson);
}

/**
 * Checks `parseRetryAfter` on one input against the oracle.
 *
 * @param input - A `Retry-After` value.
 */
function checkRetryAfter(input: string): void {
  const text = input.trim();
  let expected: number | undefined;
  if (/^[0-9]{1,10}$/.test(text)) {
    expected = Math.min(Number(text) * 1000, MAX_RETRY_AFTER_MS);
  } else if (/^[A-Z][a-z]{2}, \d{2} [A-Z][a-z]{2} \d{4} \d{2}:\d{2}:\d{2} GMT$/.test(text)) {
    const [, dd, mon, yyyy, hh, mm, ss] =
      /^\w{3}, (\d{2}) (\w{3}) (\d{4}) (\d{2}):(\d{2}):(\d{2}) GMT$/.exec(text) ?? [];
    const month = "JanFebMarAprMayJunJulAugSepOctNovDec".indexOf(mon ?? "?") / 3;
    if (Number.isInteger(month)) {
      const when = new Date(0);
      when.setUTCFullYear(Number(yyyy), month, Number(dd));
      when.setUTCHours(Number(hh), Number(mm), Number(ss), 0);
      if (when.toUTCString() === text) {
        expected = Math.min(Math.max(when.getTime() - NOW, 0), MAX_RETRY_AFTER_MS);
      }
    }
  }
  const got = parseRetryAfter(input, NOW);
  assert.equal(got, expected);
  if (got !== undefined) assert.ok(got >= 0 && got <= MAX_RETRY_AFTER_MS);
}

/**
 * Replays a corpus, then checks seeded mutations of it.
 *
 * @param name - Corpus directory.
 * @param fragments - Snippets the mutator splices in.
 * @param check - The oracle check.
 */
function fuzz(name: string, fragments: readonly string[], check: (input: string) => void): void {
  const seeds = corpus(name);
  assert.ok(seeds.length > 0, `fuzz/${name}/ is empty`);
  for (const [file, text] of seeds) {
    try {
      check(text);
    } catch (err) {
      throw new Error(`fuzz/${name}/${file}: ${String(err)}`, { cause: err });
    }
  }
  const rand = prng(SEED);
  for (let i = 0; i < ITERATIONS; i++) {
    let input = seeds[Math.floor(rand() * seeds.length)]?.[1] ?? "";
    const edits = 1 + Math.floor(rand() * 3);
    for (let e = 0; e < edits; e++) input = mutate(input, fragments, rand);
    try {
      check(input);
    } catch (err) {
      throw new Error(
        `${name}: FUZZ_SEED=${SEED} iteration ${i} input ${JSON.stringify(input)}: ${String(err)}`,
        { cause: err },
      );
    }
  }
}

test("address documents match the oracle", () => {
  fuzz(
    "address",
    [
      '"',
      ",",
      "{",
      "}",
      ":",
      "=",
      "==",
      "/",
      "+",
      "#",
      "@",
      " ",
      "\\u0000",
      "http://",
      "whsec_",
      '"x":1',
    ],
    checkAddress,
  );
});

test("Retry-After values match the oracle", () => {
  fuzz(
    "retry_after",
    ["0", "9", "60", " ", ",", ":", "GMT", "Mon", "Sun", "Feb", "29", "31", "99", "-", "."],
    checkRetryAfter,
  );
});
