/**
 * Fuzzing for the B2 reply and event readers, as the fuzzing ADR asks of
 * boundaries outside the Rust workspace (docs/adr/untrusted-input-fuzzing.md,
 * decision 6).
 *
 * Two targets, each with a seed corpus under `fuzz/` (carried over from the
 * Rust targets the TypeScript guest replaces, same framing):
 *
 * - `b2_reply`: byte 0 picks a reader (`% 9`: authorization, buckets,
 *   upload target, file version, names page, versions page, part, error
 *   with a two-byte status, download headers as `name: value` lines), the
 *   rest is the reply.
 * - `b2_event`: an even byte 0 reads a webhook body (bit 1 picks the
 *   `library/` prefix), an odd one a notification rule listing.
 *
 * Each seed is replayed, then seeded mutations of the seeds (byte edits and
 * edits to the parsed JSON tree) are checked against independent oracles.
 * `FUZZ_SEED` and `FUZZ_ITERATIONS` pick another run; a failure prints the
 * seed, iteration and input that reproduce it.
 */

import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import {
  B2ReplyError,
  MAX_ERROR_TEXT_BYTES,
  MAX_LIST_FILES,
  MAX_PARTS,
  compareNames,
  decodeFileName,
  encodeFileName,
  parseApiError,
  parseAuthorization,
  parseBuckets,
  parseDownloadHeaders,
  parseFileVersion,
  parseNamesPage,
  parsePart,
  parseUploadTarget,
  parseVersionsPage,
  type FileVersion,
} from "../src/b2-api.ts";
import {
  EventParseError,
  MAX_B2_EVENTS,
  MAX_EVENT_MESSAGE_BYTES,
  MAX_NOTIFICATION_RULES,
  parseB2Events,
  parseNotificationRules,
  ruleNames,
  rulesWith,
} from "../src/b2-events.ts";

/** PRNG seed for this run. */
const SEED = Number(process.env["FUZZ_SEED"] ?? 0xb2b2);
/** Mutated inputs checked per target. */
const ITERATIONS = Number(process.env["FUZZ_ITERATIONS"] ?? 3000);
/** The bucket event bodies are read for. */
const BUCKET = "books";

/**
 * Every seed of one target, by name.
 *
 * @param target - Directory name under `fuzz/`.
 * @returns Name and bytes, sorted by name.
 */
function seeds(target: string): [string, Uint8Array][] {
  const dir = new URL(`../fuzz/${target}/`, import.meta.url);
  return readdirSync(dir)
    .sort()
    .map((name) => [name, new Uint8Array(readFileSync(new URL(name, dir)))]);
}

/**
 * A small deterministic PRNG (mulberry32).
 *
 * @param seed - The seed.
 * @returns A function giving floats in `[0, 1)`.
 */
function prng(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Strict UTF-8 decoding: invalid bytes throw. */
const UTF8 = new TextDecoder("utf-8", { fatal: true });
/** UTF-8 encoding. */
const ENCODER = new TextEncoder();

/**
 * Decodes JSON the way an independent reader would, or `undefined`.
 *
 * @param bytes - The bytes.
 * @returns The value.
 */
function json(bytes: Uint8Array): unknown {
  try {
    return JSON.parse(UTF8.decode(bytes)) as unknown;
  } catch {
    return undefined;
  }
}

/**
 * True for a JSON object.
 *
 * @param value - Any value.
 * @returns Whether it is a plain object.
 */
function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Runs `read` twice, checking it is deterministic, and returns its value or
 * the error it threw.
 *
 * @param read - The reader.
 * @param allowed - Error classes the reader may throw.
 * @returns The value, or `{ error }`.
 */
function attempt<T>(
  read: () => T,
  allowed: (new (...args: never[]) => Error)[],
): T | { error: Error } {
  const once = (): T | { error: Error } => {
    try {
      return read();
    } catch (err) {
      assert.ok(
        allowed.some((kind) => err instanceof kind),
        `unexpected error: ${err instanceof Error ? err.stack : String(err)}`,
      );
      return { error: err as Error };
    }
  };
  const first = once();
  assert.deepEqual(once(), first, "the reader is deterministic");
  return first;
}

/**
 * True when `value` is an error marker from {@link attempt}.
 *
 * @param value - The result.
 * @returns Whether it failed.
 */
function failed(value: unknown): value is { error: Error } {
  return isObject(value) && value["error"] instanceof Error && Object.keys(value).length === 1;
}

/**
 * The oracle's rule for a B2 file name.
 *
 * @param name - The name.
 * @returns Whether B2 can store it.
 */
function nameOk(name: string): boolean {
  return (
    name.length > 0 &&
    ENCODER.encode(name).length <= 1024 &&
    ![...name].some((c) => c.charCodeAt(0) < 0x20 || c.charCodeAt(0) === 0x7f || c === "\\") &&
    !name.startsWith("/") &&
    !name.endsWith("/") &&
    !name.includes("//")
  );
}

/**
 * The oracle's rule for a bucket name.
 *
 * @param name - The name.
 * @returns Whether it is a valid bucket name.
 */
function bucketNameOk(name: string): boolean {
  const labels = name.split(".");
  const ipv4 =
    labels.length === 4 &&
    labels.every((label) => /^[0-9]+$/.test(label) && Number(label) <= 0xffff_ffff);
  return (
    name.length >= 1 &&
    name.length <= 63 &&
    /^[A-Za-z0-9.-]+$/.test(name) &&
    labels.every((label) => label.length > 0) &&
    !ipv4
  );
}

/**
 * The oracle's rule for a SHA-1.
 *
 * @param text - The digest.
 * @returns Whether it is 40 lower-case hex digits.
 */
function sha1Ok(text: string): boolean {
  return /^[0-9a-f]{40}$/.test(text);
}

/**
 * The oracle's rule for a URL B2 handed out.
 *
 * @param url - The URL.
 * @returns Whether it is plain https with a host and no query or fragment.
 */
function urlOk(url: string): boolean {
  if (!url.startsWith("https://")) return false;
  const host = url.slice("https://".length).split("/")[0] ?? "";
  return host.length > 0 && !host.includes("@") && !url.includes("?") && !url.includes("#");
}

/**
 * The oracle's percent-encoding of a file name: unreserved bytes and `/`
 * stay, everything else is `%XX` of its UTF-8 bytes.
 *
 * @param text - The name.
 * @returns The encoding.
 */
function encode(text: string): string {
  let out = "";
  for (const byte of ENCODER.encode(text)) {
    const char = String.fromCharCode(byte);
    out += /[A-Za-z0-9\-._~/]/.test(char)
      ? char
      : `%${byte.toString(16).toUpperCase().padStart(2, "0")}`;
  }
  return out;
}

/**
 * Checks one accepted file version.
 *
 * @param file - The version.
 * @param wire - Its JSON entry, when it came from JSON.
 */
function checkVersion(file: FileVersion, wire: unknown): void {
  const name =
    file.action === "folder" && file.fileName.endsWith("/")
      ? file.fileName.slice(0, -1)
      : file.fileName;
  assert.ok(nameOk(name), JSON.stringify(name));
  assert.equal(file.fileId === null, file.action === "folder");
  if (file.contentSha1 !== null) assert.ok(sha1Ok(file.contentSha1), file.contentSha1);
  if (wire !== undefined) {
    assert.ok(isObject(wire), "an accepted file entry is an object");
    assert.equal(wire["fileName"], file.fileName);
    if (file.fileId !== null) assert.equal(wire["fileId"], file.fileId);
  }
}

/**
 * Checks an accepted listing's files against its JSON.
 *
 * @param files - The versions.
 * @param value - The reply's JSON.
 */
function checkFiles(files: FileVersion[], value: unknown): void {
  assert.ok(isObject(value));
  const listed = value["files"];
  assert.ok(Array.isArray(listed), "an accepted listing has files");
  assert.equal(listed.length, files.length);
  assert.ok(files.length <= MAX_LIST_FILES);
  files.forEach((file, at) => {
    checkVersion(file, listed[at]);
  });
}

/**
 * Splits download-header text into name and value pairs.
 *
 * @param rest - The seed's text.
 * @returns The pairs.
 */
function headerLines(rest: Uint8Array): [string, string][] {
  let text: string;
  try {
    text = UTF8.decode(rest);
  } catch {
    return [];
  }
  const out: [string, string][] = [];
  for (const line of text.split("\n")) {
    const colon = line.indexOf(":");
    if (colon < 0) continue;
    out.push([line.slice(0, colon).trim(), line.slice(colon + 1).trim()]);
  }
  return out;
}

/**
 * The `b2_reply` oracle.
 *
 * @param data - Selector byte, then the reply.
 */
function checkReply(data: Uint8Array): void {
  if (data.length === 0) return;
  const selector = data[0] ?? 0;
  const rest = data.subarray(1);
  let text: string | null = null;
  try {
    text = UTF8.decode(rest);
  } catch {
    text = null;
  }
  if (text !== null) {
    const encoded = encodeFileName(text);
    assert.equal(encoded, encode(text));
    assert.equal(decodeFileName(encoded), text);
  }
  const value = json(rest);
  const object = isObject(value);
  const errors = [B2ReplyError];
  switch (selector % 9) {
    case 0: {
      const auth = attempt(() => parseAuthorization(rest), errors);
      if (failed(auth)) return;
      assert.ok(object);
      const api = (value["apiInfo"] as Record<string, unknown> | undefined)?.["storageApi"];
      assert.ok(isObject(api));
      assert.equal(value["authorizationToken"], auth.token);
      assert.ok(urlOk(auth.apiUrl) && urlOk(auth.downloadUrl));
      assert.ok(auth.absoluteMinimumPartSize > 0);
      assert.ok(auth.recommendedPartSize >= auth.absoluteMinimumPartSize);
      if (auth.bucket !== null) assert.ok(bucketNameOk(auth.bucket.name), auth.bucket.name);
      return;
    }
    case 1: {
      const buckets = attempt(() => parseBuckets(rest), errors);
      if (failed(buckets)) return;
      assert.ok(object);
      const listed = value["buckets"];
      assert.ok(Array.isArray(listed));
      assert.equal(listed.length, buckets.length);
      assert.ok(listed.every(isObject));
      assert.ok(buckets.every((bucket) => bucketNameOk(bucket.name)));
      return;
    }
    case 2: {
      const target = attempt(() => parseUploadTarget(rest), errors);
      if (failed(target)) return;
      assert.ok(object);
      assert.ok(urlOk(target.url));
      assert.ok(/^[\x21-\x7e]+$/.test(target.token));
      return;
    }
    case 3: {
      const file = attempt(() => parseFileVersion(rest), errors);
      if (failed(file)) return;
      assert.ok(object);
      checkVersion(file, value);
      return;
    }
    case 4: {
      const page = attempt(() => parseNamesPage(rest), errors);
      if (failed(page)) return;
      checkFiles(page.files, value);
      const next = page.nextFileName;
      if (next !== null) {
        assert.ok(nameOk(next));
        assert.ok(page.files.every((file) => compareNames(file.fileName, next) < 0));
      }
      return;
    }
    case 5: {
      const page = attempt(() => parseVersionsPage(rest), errors);
      if (failed(page)) return;
      checkFiles(page.files, value);
      if (page.next !== null) {
        const next = page.next.name;
        assert.ok(nameOk(next));
        assert.ok(page.files.every((file) => compareNames(file.fileName, next) <= 0));
      }
      page.files.forEach((earlier, at) => {
        for (const later of page.files.slice(at + 1)) {
          if (earlier.fileName === later.fileName) {
            assert.ok(earlier.uploadTimestampMs >= later.uploadTimestampMs);
          }
        }
      });
      return;
    }
    case 6: {
      const part = attempt(() => parsePart(rest), errors);
      if (failed(part)) return;
      assert.ok(object);
      assert.ok(part.partNumber >= 1 && part.partNumber <= MAX_PARTS);
      assert.equal(value["partNumber"], part.partNumber);
      assert.ok(sha1Ok(part.contentSha1));
      return;
    }
    case 7: {
      const status = rest.length >= 2 ? ((rest[0] ?? 0) << 8) | (rest[1] ?? 0) : 500;
      const body = rest.subarray(2);
      const err = parseApiError(status, body);
      assert.deepEqual(parseApiError(status, body), err);
      assert.equal(err.status, status);
      assert.ok(err.code.length > 0 && ENCODER.encode(err.code).length <= MAX_ERROR_TEXT_BYTES);
      assert.ok(ENCODER.encode(err.message).length <= MAX_ERROR_TEXT_BYTES);
      // eslint-disable-next-line no-control-regex
      assert.ok(!/[\u0000-\u001f\u007f-\u009f]/u.test(err.code + err.message));
      if (err.code !== "unknown") {
        const coded = json(body);
        assert.ok(isObject(coded), "a coded error is a JSON object");
        assert.equal(coded["code"], err.code);
        assert.ok(/^[a-z0-9_]+$/.test(err.code));
      }
      return;
    }
    default: {
      const lines = headerLines(rest);
      const head = attempt(() => parseDownloadHeaders(lines), errors);
      if (failed(head)) return;
      assert.ok(head.bodyOffset + head.bodyLength <= head.version.contentLength);
      assert.ok(head.version.fileId !== null);
      checkVersion(head.version, undefined);
    }
  }
}

/**
 * What the oracle expects a webhook body to yield, or `null` for a refusal.
 *
 * @param body - The body.
 * @param prefix - The destination prefix.
 * @returns Key and time per change.
 */
function expectedEvents(body: Uint8Array, prefix: string): [string, string | null][] | null {
  const value = json(body);
  if (!isObject(value)) return null;
  const events = value["events"];
  if (!Array.isArray(events) || events.length > MAX_B2_EVENTS) return null;
  const out: [string, string | null][] = [];
  for (const event of events as unknown[]) {
    if (!isObject(event)) return null;
    const type = event["eventType"];
    if (typeof type !== "string") return null;
    const parts = type.split(":");
    const head = parts.slice(0, 2);
    const detail = parts.slice(2).join(":");
    const reported =
      parts.length >= 3 &&
      head[0] === "b2" &&
      detail.length > 0 &&
      ["ObjectCreated", "ObjectDeleted", "HideMarkerCreated"].includes(head[1] ?? "");
    if (!reported) continue;
    const bucket = event["bucketName"];
    if (typeof bucket !== "string") return null;
    if (bucket !== BUCKET) continue;
    const name = event["objectName"];
    if (typeof name !== "string") return null;
    if (!name.startsWith(prefix)) continue;
    const key = name.slice(prefix.length);
    if (key === "") continue;
    if (!nameOk(name)) return null;
    const stamp = event["eventTimestamp"];
    let time: string | null = null;
    if (stamp !== undefined && stamp !== null) {
      if (typeof stamp !== "number" || !Number.isSafeInteger(stamp) || stamp < 0) return null;
      time = String(stamp).padStart(20, "0");
    }
    out.push([key, time]);
  }
  return out;
}

/**
 * The `b2_event` oracle.
 *
 * @param data - Selector byte, then the body or rule listing.
 */
async function checkEvent(data: Uint8Array): Promise<void> {
  if (data.length === 0) return;
  const selector = data[0] ?? 0;
  const rest = data.subarray(1);
  if (selector % 2 === 0) {
    const prefix = (selector & 2) === 0 ? "" : "library/";
    const scope = { bucket: BUCKET, prefix };
    const events = attempt(() => parseB2Events(rest, scope), [EventParseError]);
    if (rest.length > MAX_EVENT_MESSAGE_BYTES) {
      assert.ok(failed(events) && (events.error as EventParseError).kind === "too_large");
      return;
    }
    const expected = expectedEvents(rest, prefix);
    if (failed(events)) {
      assert.equal(expected, null, "a well-formed body was refused");
      return;
    }
    const found = events.map((event): [string, string | null] => {
      assert.equal(event.change.kind, "changed");
      assert.ok(event.change.key.length > 0 && !event.change.key.includes("\0"));
      return [event.change.key, event.time];
    });
    assert.deepEqual(found, expected, "the parser and the oracle disagree");
    return;
  }
  const rules = attempt(() => parseNotificationRules(rest), [B2ReplyError]);
  if (failed(rules)) return;
  const value = json(rest);
  assert.ok(isObject(value));
  const listed = value["eventNotificationRules"];
  assert.ok(Array.isArray(listed), "an accepted reply lists rules");
  assert.ok(rules.length <= MAX_NOTIFICATION_RULES);
  assert.equal(rules.length, listed.length);
  rules.forEach((rule, at) => {
    const raw = listed[at] as Record<string, unknown>;
    assert.equal(rule.name, raw["name"]);
    assert.ok(rule.name.length > 0 && ENCODER.encode(rule.name).length <= 63);
    if (rule.signingSecret !== null)
      assert.ok(/^[A-Za-z0-9]{32}$/.test(rule.signingSecret.expose()));
    assert.ok(rules.slice(0, at).every((seen) => seen.name !== rule.name));
  });
  const own = await ruleNames("library/");
  const kept = await rulesWith(rules, "library/", []);
  const others = (listed as Record<string, unknown>[]).filter(
    (raw) => !own.includes(raw["name"] as string),
  );
  assert.equal(kept.length, others.length);
  kept.forEach((sent, at) => {
    const raw: Record<string, unknown> = { ...others[at] };
    delete raw["isSuspended"];
    delete raw["suspensionReason"];
    assert.deepEqual(sent, raw);
  });
}

/** Values a tree mutation puts in place of a node. */
const VALUES: readonly unknown[] = [
  null,
  true,
  0,
  -1,
  1.5,
  2 ** 53,
  "",
  "a\u0000b",
  "a\\b",
  "/lead",
  "trail/",
  "a//b",
  "x".repeat(1100),
  "http://insecure.example",
  "https://user@host.example",
  "https://ok.example/path?q",
  "b2:ObjectCreated:Upload",
  "b2:TestEvent:Test",
  "books",
  "library/x",
  "0123456789abcdef0123456789abcdef01234567",
  [],
  {},
];

/**
 * Mutates one seed: a byte edit, a cut, or an edit to its JSON tree.
 *
 * @param seed - Selector byte and body.
 * @param random - The PRNG.
 * @returns The mutated input.
 */
function mutate(seed: Uint8Array, random: () => number): Uint8Array {
  const pick = <T>(items: readonly T[]): T => items[Math.floor(random() * items.length)] as T;
  const head = seed.subarray(0, 1);
  const body = seed.subarray(1);
  const value = json(body);
  if (value !== undefined && random() < 0.6) {
    const nodes: [Record<string, unknown> | unknown[], string | number][] = [];
    const walk = (node: unknown): void => {
      if (Array.isArray(node)) {
        node.forEach((child, at) => {
          nodes.push([node, at]);
          walk(child);
        });
      } else if (isObject(node)) {
        for (const key of Object.keys(node)) {
          nodes.push([node, key]);
          walk(node[key]);
        }
      }
    };
    walk(value);
    if (nodes.length > 0) {
      const [parent, slot] = pick(nodes);
      const roll = random();
      if (roll < 0.15) {
        if (Array.isArray(parent)) parent.splice(slot as number, 1);
        else delete parent[slot as string];
      } else if (roll < 0.3 && Array.isArray(parent)) {
        parent.push(parent[slot as number]);
      } else {
        (parent as Record<string | number, unknown>)[slot] = structuredClone(pick(VALUES));
      }
    }
    const text = ENCODER.encode(JSON.stringify(value));
    const out = new Uint8Array(1 + text.length);
    out.set(head);
    out.set(text, 1);
    return out;
  }
  const out = new Uint8Array(seed);
  const roll = random();
  if (roll < 0.4 && out.length > 1) {
    const at = 1 + Math.floor(random() * (out.length - 1));
    out[at] = Math.floor(random() * 256);
    return out;
  }
  if (roll < 0.6) return out.subarray(0, Math.floor(random() * out.length) + 1);
  if (roll < 0.8) {
    out[0] = Math.floor(random() * 256);
    return out;
  }
  const at = 1 + Math.floor(random() * Math.max(out.length - 1, 1));
  const fill = Uint8Array.from({ length: 1 + Math.floor(random() * 8) }, () =>
    pick([0, 0x22, 0x5c, 0x7b, 0x7d, 0xc3, 0xff, 0x3a, 0x0a]),
  );
  const spliced = new Uint8Array(out.length + fill.length);
  spliced.set(out.subarray(0, at));
  spliced.set(fill, at);
  spliced.set(out.subarray(at), at + fill.length);
  return spliced;
}

/**
 * Hex of bytes, for reproduction messages.
 *
 * @param bytes - The bytes.
 * @returns The hex.
 */
function hexOf(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Replays the seeds and their mutations through an oracle.
 *
 * @param target - The corpus.
 * @param check - The oracle.
 */
async function fuzz(
  target: string,
  check: (data: Uint8Array) => void | Promise<void>,
): Promise<void> {
  const corpus = seeds(target);
  assert.ok(corpus.length > 0, `${target} has seeds`);
  for (const [name, data] of corpus) {
    try {
      await check(data);
    } catch (err) {
      assert.fail(`${target}/${name}: ${err instanceof Error ? err.message : String(err)}`);
    }
  }
  const random = prng(SEED);
  for (let iteration = 0; iteration < ITERATIONS; iteration++) {
    const [, seed] = corpus[Math.floor(random() * corpus.length)] as [string, Uint8Array];
    const input = mutate(seed, random);
    try {
      await check(input);
    } catch (err) {
      assert.fail(
        `${target}: FUZZ_SEED=${SEED} iteration ${iteration} input ${hexOf(input)}: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }
}

/**
 * Reads one `b2_reply` seed with the reader its byte 0 picks.
 *
 * @param name - The seed.
 * @returns Whether it was accepted.
 */
function replySeedReads(name: string): boolean {
  const data = new Uint8Array(readFileSync(new URL(`../fuzz/b2_reply/${name}`, import.meta.url)));
  const rest = data.subarray(1);
  const ok = (read: () => unknown): boolean => {
    try {
      read();
      return true;
    } catch {
      return false;
    }
  };
  switch ((data[0] ?? 0) % 9) {
    case 0:
      return ok(() => parseAuthorization(rest));
    case 1:
      return ok(() => parseBuckets(rest));
    case 2:
      return ok(() => parseUploadTarget(rest));
    case 3:
      return ok(() => parseFileVersion(rest));
    case 4:
      return ok(() => parseNamesPage(rest));
    case 5:
      return ok(() => parseVersionsPage(rest));
    case 6:
      return ok(() => parsePart(rest));
    case 7:
      return parseApiError(500, rest).code !== "";
    default:
      return ok(() => parseDownloadHeaders(headerLines(rest)));
  }
}

/**
 * Reads one `b2_event` seed: the keys of a body, or the names of a rule
 * listing, or `null` when refused.
 *
 * @param name - The seed.
 * @returns What it yields.
 */
function eventSeed(name: string): string[] | null {
  const data = new Uint8Array(readFileSync(new URL(`../fuzz/b2_event/${name}`, import.meta.url)));
  const selector = data[0] ?? 0;
  const rest = data.subarray(1);
  try {
    if (selector % 2 === 0) {
      const prefix = (selector & 2) === 0 ? "" : "library/";
      return parseB2Events(rest, { bucket: BUCKET, prefix }).map((event) => event.change.key);
    }
    return parseNotificationRules(rest).map((rule) => rule.name);
  } catch {
    return null;
  }
}

test("b2_reply seeds and their mutations hold the oracle", async () => {
  await fuzz("b2_reply", checkReply);
});

test("b2_event seeds and their mutations hold the oracle", async () => {
  await fuzz("b2_event", checkEvent);
  const reached = seeds("b2_event").filter(([name]) => (eventSeed(name) ?? []).length > 0).length;
  assert.ok(reached >= 5, `the seeds must reach the oracle's comparison, ${reached} did`);
});

test("named b2_reply seeds keep their meaning", () => {
  for (const accepted of [
    "authorize_restricted",
    "authorize_unrestricted",
    "buckets",
    "buckets_dotted_name",
    "upload_target",
    "file_upload",
    "file_large_sha1",
    "file_start",
    "file_folder",
    "names_page",
    "names_page_last",
    "versions_page",
    "part",
    "error_html",
    "download_whole",
    "download_range",
  ]) {
    assert.ok(replySeedReads(accepted), `${accepted} should read`);
  }
  for (const refused of [
    "authorize_http_url",
    "authorize_not_utf8_in_skipped_field",
    "authorize_deleted_bucket",
    "authorize_positional_array",
    "buckets_positional_array",
    "authorize_positional_storage_api",
    "names_page_positional_file",
    "versions_page_positional_file",
    "versions_page_old_first",
    "versions_page_interleaved",
    "file_no_length",
    "buckets_bad_name",
    "buckets_ipv4_name",
    "buckets_double_dot",
    "buckets_edge_dot",
    "upload_target_userinfo",
    "file_backslash",
    "file_unknown_action",
    "names_page_backwards",
    "versions_half_continuation",
    "part_zero",
    "part_too_many",
    "download_range_mismatch",
    "download_no_id",
    "download_repeated",
  ]) {
    assert.ok(!replySeedReads(refused), `${refused} should be refused`);
  }
  for (const [name, status, code, messageEmpty] of [
    ["error_html", 503, "unknown", true],
    ["error_expired", 401, "expired_auth_token", false],
    ["error_control", 400, "unknown", false],
  ] as const) {
    const data = new Uint8Array(readFileSync(new URL(`../fuzz/b2_reply/${name}`, import.meta.url)));
    assert.equal((data[0] ?? 0) % 9, 7, `${name} is an error seed`);
    const err = parseApiError(((data[1] ?? 0) << 8) | (data[2] ?? 0), data.subarray(3));
    assert.equal(err.status, status, name);
    assert.equal(err.code, code, name);
    assert.equal(err.message === "", messageEmpty, name);
  }
});

test("named b2_event seeds keep their meaning", () => {
  const keys = (name: string): string[] => {
    const found = eventSeed(name);
    assert.ok(found !== null, name);
    return found;
  };
  assert.deepEqual(keys("upload"), ["a b.m4b"]);
  assert.deepEqual(keys("upload_under_prefix"), ["x/y.m4b"]);
  assert.deepEqual(keys("batch_mixed"), ["a", "b", "c", "d", "e"]);
  assert.deepEqual(keys("future_type"), ["a"]);
  assert.deepEqual(keys("no_time"), ["a"]);
  for (const skipped of ["other_bucket", "outside_prefix", "test_event", "live_read"]) {
    assert.deepEqual(keys(skipped), [], skipped);
  }
  for (const refused of [
    "bad_name_backslash",
    "bad_time",
    "missing_name",
    "events_as_object",
    "body_as_array",
    "not_json",
    "rules_duplicate",
    "rules_as_array",
  ]) {
    assert.equal(eventSeed(refused), null, refused);
  }
  assert.equal(keys("rules").length, 3);
  assert.deepEqual(keys("rules_bad_secret"), ["a"]);
  assert.deepEqual(keys("rules_empty"), []);
});
