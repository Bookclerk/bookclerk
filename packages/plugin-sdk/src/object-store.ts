/**
 * Object-store destination helpers: everything a `storage` plugin over a
 * bucket-style backend needs beyond its own five calls.
 *
 * A plugin implements {@link ObjectStore} (head, list, get, put, copy and
 * delete against its service) and hands it to {@link ObjectStoreDestination},
 * which serves the whole `storage` entrypoint the way the Rust storage
 * helpers do: staged writes under a commit token with an idempotent commit,
 * conditional writes (atomic, checked after writing, or a probe first),
 * moves and batch calls built from the five calls when the backend has
 * nothing better, object-store key translation, and a change feed when the
 * plugin brings one. The Python SDK's `object_store` module is the same
 * code, and shared test vectors keep the three languages in step.
 */

import { MAX_BATCH_KEYS, MAX_LIST_PAGE } from "./abi.js";
import type {
  FEATURE_STORAGE_CHANGES,
  FEATURE_STORAGE_CHECKED_WRITE,
  FEATURE_STORAGE_CONDITIONAL_WRITE,
  FEATURE_STORAGE_COPY,
  FEATURE_STORAGE_TRANSLATE_KEYS,
} from "./abi.js";
import { PluginError } from "./errors.js";
import type {
  ByteRange,
  ChangesPage,
  ChangesRequest,
  CopyResult,
  KeyOutcome,
  ListOptions,
  ListPage,
  MoveManyPage,
  MoveManyRequest,
  MoveOptions,
  MoveResult,
  ObjectInfo,
  ObjectMetadata,
  PutResult,
  ReadResult,
  WriteOptions,
} from "./plugin.js";

// ---------------------------------------------------------------------------
// Backend contract
// ---------------------------------------------------------------------------

/** One stored object as the backend reports it. */
export interface StoredObject extends ObjectMetadata {
  /** The commit token the object was written under, when it carries one. */
  commitToken?: string;
}

/** What a write stores alongside the bytes. */
export interface ObjectWrite {
  /** MIME type. */
  contentType?: string;
  /** Declared length; the backend refuses a body of another length. */
  contentLength?: number;
  /** Expected SHA-256 of the body; the backend refuses a body that differs. */
  sha256?: Uint8Array;
  /** Commit token to record on the object. */
  commitToken?: string;
}

/** What a write stored. */
export interface WrittenObject {
  /** Bytes stored. */
  bytesWritten: number;
  /** The stored object's entity tag. */
  etag?: string;
  /** SHA-256 of the stored body. */
  sha256?: Uint8Array;
}

/** A write's precondition. */
export type WriteCondition = { kind: "ifMatch"; etag: string } | { kind: "ifNoneMatch" };

/** One page of a backend listing. */
export interface ObjectPage {
  /** Objects on the page, in the backend's order. */
  objects: ObjectInfo[];
  /** Cursor for the next page; absent on the last page. */
  nextCursor?: string;
}

/**
 * The calls a bucket-style backend makes. Keys are the host's object keys;
 * the backend adds its own prefix. Every failure is a {@link PluginError}:
 * `not_found`, `invalid_params` (a key the backend cannot hold),
 * `payload_too_large`, `invalid_cursor`, `conflict`, or `internal`.
 */
export interface ObjectStore {
  /**
   * The object at `key`, or `null` when there is none.
   *
   * @param key - Object key.
   * @returns The object, or `null`.
   */
  head(key: string): Promise<StoredObject | null>;
  /**
   * One page of objects under `prefix`.
   *
   * @param prefix - Key prefix.
   * @param cursor - The previous page's cursor, or `undefined` to start.
   * @param limit - Most objects to return, already within `1..=MAX_LIST_PAGE`.
   * @returns The page.
   */
  list(prefix: string, cursor: string | undefined, limit: number): Promise<ObjectPage>;
  /**
   * Streams the object at `key`.
   *
   * @param key - Object key.
   * @param range - A byte range already checked by
   *   {@link checkByteRange}, or `undefined` for the whole object.
   * @returns The object and its body; `not_found` when it is missing.
   */
  get(
    key: string,
    range: ByteRange | undefined,
  ): Promise<{ object: StoredObject; body: ReadableStream<Uint8Array> }>;
  /**
   * Stores `body` at `key`, replacing what is there.
   *
   * @param key - Object key.
   * @param body - The bytes.
   * @param write - Type, length, checksum and commit token.
   * @returns What was stored.
   */
  put(key: string, body: ReadableStream<Uint8Array>, write: ObjectWrite): Promise<WrittenObject>;
  /**
   * Copies `from` to `to` (on the server when {@link ObjectStore.serverCopy}).
   *
   * The copy keeps the source's content type, checksum and commit token:
   * a commit publishes a stage with this call, and a repeated commit after
   * the stage is gone recognizes the published object by its token.
   *
   * @param from - Source key.
   * @param to - Target key.
   * @returns Bytes copied.
   */
  copy(from: string, to: string): Promise<number>;
  /**
   * Removes the object at `key`; a missing object is not an error.
   *
   * @param key - Object key.
   * @returns Resolves once it is gone.
   */
  delete(key: string): Promise<void>;
  /**
   * A conditional write, when the backend has one. Leave it out and
   * {@link ObjectStoreDestination} probes first (not atomic).
   *
   * @param key - Object key.
   * @param body - The bytes.
   * @param write - Type, length, checksum and commit token.
   * @param condition - The precondition.
   * @returns What was stored; `conflict` when the condition failed.
   */
  putConditional?(
    key: string,
    body: ReadableStream<Uint8Array>,
    write: ObjectWrite,
    condition: WriteCondition,
  ): Promise<WrittenObject>;
  /** Whether {@link ObjectStore.copy} runs on the server (no bytes through the plugin). */
  readonly serverCopy: boolean;
  /**
   * How {@link ObjectStore.putConditional} keeps its condition: `atomic`
   * (the service enforces it), `checked` (it writes, then checks and removes
   * a write that lost), or absent when there is no `putConditional`.
   */
  readonly conditionalWrites?: "atomic" | "checked";
  /** UTF-8 bytes of the backend's own key prefix, for key translation. */
  readonly prefixBytes: number;
}

/** A change feed a destination serves `changes` from. */
export interface ChangeFeed {
  /**
   * One page of changes after `cursor`.
   *
   * @param cursor - The previous page's cursor, or `undefined` to ask for the
   *   current position.
   * @param limit - Requested changes (0 for the feed's default).
   * @returns The page, or `null` when the feed is off (the host lists).
   */
  changes(cursor: string | undefined, limit: number): Promise<ChangesPage | null>;
}

// ---------------------------------------------------------------------------
// Checks shared by every backend
// ---------------------------------------------------------------------------

/** Cursor a destination with no feed (or a feed that is off) hands out. */
export const NO_FEED_CURSOR = "none";

/** Folder under which staged writes wait for their commit. */
export const STAGE_FOLDER = ".bookclerk-stage";

/** Longest host key, in UTF-8 bytes. */
export const MAX_HOST_KEY_BYTES = 1024;

/** Longest object-store key, in UTF-8 bytes (prefix included). */
export const MAX_OBJECT_KEY_BYTES = 1024;

/** Longest file extension key translation keeps when it shortens a name, in bytes. */
const MAX_KEPT_EXTENSION_BYTES = 16;

/** Shared encoder for byte lengths. */
const ENCODER = new TextEncoder();

/**
 * UTF-8 length of `text`.
 *
 * @param text - Any string.
 * @returns Its length in bytes.
 */
function utf8Length(text: string): number {
  return ENCODER.encode(text).length;
}

/**
 * True when `char` is a Unicode control character (category Cc).
 *
 * @param char - One code point.
 * @returns Whether it is a control character.
 */
function isControl(char: string): boolean {
  const code = char.codePointAt(0) ?? 0;
  return code < 0x20 || (code >= 0x7f && code <= 0x9f);
}

/**
 * Checks a host key: 1 to {@link MAX_HOST_KEY_BYTES} bytes of `/`-separated
 * segments, none empty, `.` or `..`, and none holding a backslash or a
 * control character.
 *
 * @param key - The host key.
 * @returns The key.
 * @throws {PluginError} `invalid_params` when it is not one.
 */
export function parseHostKey(key: string): string {
  if (key.length === 0) throw PluginError.fromWire("invalid_params", "empty host key");
  const bytes = utf8Length(key);
  if (bytes > MAX_HOST_KEY_BYTES) {
    throw PluginError.fromWire(
      "invalid_params",
      `host key is ${bytes} bytes, more than ${MAX_HOST_KEY_BYTES}`,
    );
  }
  for (const segment of key.split("/")) {
    if (segment === "" || segment === "." || segment === "..") {
      throw PluginError.fromWire(
        "invalid_params",
        `host key segment ${JSON.stringify(segment)} is empty or a dot segment`,
      );
    }
    for (const char of segment) {
      if (char === "\\" || isControl(char)) {
        throw PluginError.fromWire(
          "invalid_params",
          `host key segment ${JSON.stringify(segment)} holds a backslash or a control character`,
        );
      }
    }
  }
  return key;
}

/** Characters object stores and their tools handle badly, and their stand-ins. */
const OBJECT_STORE_AVOIDED: ReadonlyMap<string, string> = new Map([
  ["{", "_"],
  ["}", "_"],
  ["^", "_"],
  ["%", "_"],
  ["`", "_"],
  ['"', "'"],
  ["<", "("],
  [">", ")"],
  ["[", "("],
  ["]", ")"],
  ["#", "_"],
  ["|", "_"],
  ["~", "_"],
]);

/**
 * The extension (from the last `.`, not at the start) when it is at most
 * {@link MAX_KEPT_EXTENSION_BYTES} bytes.
 *
 * @param segment - One key segment.
 * @returns The extension with its dot, or `null`.
 */
function keptExtension(segment: string): string | null {
  const dot = segment.lastIndexOf(".");
  if (dot <= 0) return null;
  const ext = segment.slice(dot);
  return utf8Length(ext) <= MAX_KEPT_EXTENSION_BYTES ? ext : null;
}

/**
 * The longest prefix of `text` within `limit` bytes, without trailing dots
 * or spaces (or its first character, when that leaves nothing).
 *
 * @param text - The text to cut.
 * @param limit - Most UTF-8 bytes.
 * @returns The cut text.
 */
function cut(text: string, limit: number): string {
  let used = 0;
  let out = "";
  for (const char of text) {
    used += utf8Length(char);
    if (used > limit) break;
    out += char;
  }
  const trimmed = out.replace(/[. ]+$/u, "");
  if (trimmed.length > 0) return trimmed;
  for (const char of text) return char;
  return "";
}

/**
 * Shortens `segment` to `limit` bytes, keeping its extension when it fits.
 *
 * @param segment - One key segment.
 * @param limit - Most UTF-8 bytes.
 * @returns The shortened segment.
 */
function shortenKeepingExtension(segment: string, limit: number): string {
  if (utf8Length(segment) <= limit) return segment;
  const ext = keptExtension(segment);
  if (ext !== null && utf8Length(ext) < limit) {
    const stem = segment.slice(0, segment.length - ext.length);
    return `${cut(stem, limit - utf8Length(ext))}${ext}`;
  }
  return cut(segment, limit);
}

/**
 * Shortens `segments` so they join within `budget` bytes: the file name
 * first (keeping its extension), then the longest folder, then the file
 * name again.
 *
 * @param segments - Key segments, changed in place.
 * @param budget - Most UTF-8 bytes of the joined key.
 */
function fitTotal(segments: string[], budget: number): void {
  const total = (): number =>
    segments.reduce((sum, segment) => sum + utf8Length(segment), 0) +
    Math.max(segments.length - 1, 0);
  const lastAt = segments.length - 1;
  let over = Math.max(total() - budget, 0);
  if (over === 0) return;
  const last = segments[lastAt];
  if (last !== undefined) {
    const ext = keptExtension(last);
    const floor = ext === null ? 1 : utf8Length(ext) + 1;
    segments[lastAt] = shortenKeepingExtension(last, Math.max(utf8Length(last) - over, floor));
  }
  while (total() > budget) {
    let index = -1;
    let longest = 0;
    for (let i = 0; i < lastAt; i++) {
      const length = utf8Length(segments[i] ?? "");
      // Ties go to the later folder, as Rust's `max_by_key` does.
      if (length >= longest) {
        longest = length;
        index = i;
      }
    }
    if (index < 0 || longest <= 1) break;
    over = total() - budget;
    segments[index] = cut(segments[index] ?? "", Math.max(longest - over, 1));
  }
  over = Math.max(total() - budget, 0);
  const final = segments[lastAt];
  if (over > 0 && final !== undefined) {
    segments[lastAt] = shortenKeepingExtension(final, Math.max(utf8Length(final) - over, 1));
  }
}

/**
 * Translates a checked host key into a key an object store holds well:
 * awkward characters replaced, and the whole key shortened to fit
 * {@link MAX_OBJECT_KEY_BYTES} after the backend's own prefix.
 *
 * @param key - A key {@link parseHostKey} accepted.
 * @param prefixBytes - UTF-8 bytes of the backend's prefix.
 * @returns The object-store key.
 */
export function translateForObjectStore(key: string, prefixBytes: number): string {
  const segments = key.split("/").map((segment) => {
    let out = "";
    for (const char of segment) out += OBJECT_STORE_AVOIDED.get(char) ?? char;
    return out;
  });
  fitTotal(segments, Math.max(MAX_OBJECT_KEY_BYTES - prefixBytes, 0));
  return segments.join("/");
}

/**
 * Checks a read range: a length of 0 is ambiguous (leave it out to read to
 * the end) and an offset plus length must stay a safe integer. With `size`,
 * the range must also lie within the object.
 *
 * @param range - The requested range, or `undefined`.
 * @param size - The object's size, when known.
 * @returns The range.
 * @throws {PluginError} `invalid_params` for a range that cannot be read.
 */
export function checkByteRange(range: ByteRange | undefined, size?: number): ByteRange | undefined {
  if (range === undefined) return undefined;
  const { offset, length } = range;
  if (!Number.isSafeInteger(offset) || offset < 0) {
    throw PluginError.fromWire("invalid_params", "byte range offset is not a whole number");
  }
  if (length !== undefined) {
    if (length === 0) {
      throw PluginError.fromWire(
        "invalid_params",
        "byte range length 0 is ambiguous; omit length to read through EOF",
      );
    }
    if (!Number.isSafeInteger(length) || length < 0 || !Number.isSafeInteger(offset + length)) {
      throw PluginError.fromWire("invalid_params", "byte range offset + length overflows");
    }
    if (size !== undefined && offset + length > size) {
      throw PluginError.fromWire(
        "invalid_params",
        `byte range ${offset + length} extends past object size ${size}`,
      );
    }
  }
  if (size !== undefined && offset > size) {
    throw PluginError.fromWire(
      "invalid_params",
      `byte range offset ${offset} is past object size ${size}`,
    );
  }
  return range;
}

/**
 * Checks a list or batch page size: 0 asks for the most, and anything else
 * is clamped to `1..=MAX_LIST_PAGE`.
 *
 * @param limit - The requested size.
 * @returns The size to use.
 */
export function clampPageLimit(limit: number | undefined): number {
  const requested = limit === undefined || limit === 0 ? MAX_LIST_PAGE : limit;
  return Math.min(Math.max(Math.trunc(requested), 1), MAX_LIST_PAGE);
}

/**
 * The key a staged write waits under: `.bookclerk-stage/<token>/<key>`.
 *
 * @param token - The commit token.
 * @param key - The object key.
 * @returns The stage key.
 * @throws {PluginError} `invalid_params` for an empty token, one holding a
 *   path separator, `..` or NUL, or an empty key.
 */
export function stageKey(token: string | undefined, key: string): string {
  const t = token ?? "";
  if (t === "" || t.includes("/") || t.includes("\\") || t.includes("..") || t.includes("\0")) {
    throw PluginError.fromWire(
      "invalid_params",
      "commit_token must be a non-empty identifier without path separators",
    );
  }
  if (key === "") throw PluginError.fromWire("invalid_params", "object key required");
  return `${STAGE_FOLDER}/${t}/${key}`;
}

/**
 * The precondition `options` asks for.
 *
 * @param options - The write options.
 * @returns The condition, or `null` for a plain write.
 * @throws {PluginError} `invalid_params` for both `ifMatch` and
 *   `ifNoneMatch`, or for a conditional write that is also staged.
 */
export function writeCondition(options: WriteOptions): WriteCondition | null {
  if (options.ifMatch !== undefined && options.ifNoneMatch === true) {
    throw PluginError.fromWire("invalid_params", "a write takes ifMatch or ifNoneMatch, not both");
  }
  const condition: WriteCondition | null =
    options.ifMatch !== undefined
      ? { kind: "ifMatch", etag: options.ifMatch }
      : options.ifNoneMatch === true
        ? { kind: "ifNoneMatch" }
        : null;
  if (condition !== null && options.stageOnly === true) {
    throw PluginError.fromWire("invalid_params", "a conditional write cannot be staged");
  }
  return condition;
}

/**
 * Refuses a batch over {@link MAX_BATCH_KEYS} entries.
 *
 * @param length - Entries in the batch.
 * @param op - The call, for the message.
 * @throws {PluginError} `invalid_params` when the batch is too long.
 */
function requireBatch(length: number, op: string): void {
  if (length > MAX_BATCH_KEYS) {
    throw PluginError.fromWire("invalid_params", `${op} takes at most ${MAX_BATCH_KEYS} keys`);
  }
}

/**
 * `source` and `target` prefixes in normal form: empty, `.` and backslash
 * separators dropped, a trailing `/` kept.
 *
 * @param prefix - A key prefix.
 * @returns The normal form.
 */
function normalizedPrefix(prefix: string): string {
  const parts = prefix.split(/[/\\]/u).filter((part) => part !== "" && part !== ".");
  let out = parts.join("/");
  if ((prefix.endsWith("/") || prefix.endsWith("\\")) && out !== "") out += "/";
  return out;
}

/**
 * Refuses a prefix move whose source is empty or whose prefixes overlap.
 *
 * @param from - Source prefix.
 * @param to - Target prefix.
 * @throws {PluginError} `invalid_params` when they cannot be moved apart.
 */
export function requireDisjointPrefixes(from: string, to: string): void {
  const a = normalizedPrefix(from);
  const b = normalizedPrefix(to);
  if (a === "")
    throw PluginError.fromWire("invalid_params", "a prefix move needs a non-empty source prefix");
  if (b.startsWith(a) || a.startsWith(b)) {
    throw PluginError.fromWire(
      "invalid_params",
      `target prefix ${to} overlaps source prefix ${from}`,
    );
  }
}

/**
 * The wire error for a failure, as a key outcome carries it.
 *
 * @param err - What was thrown.
 * @returns The code and message.
 */
function outcomeError(err: unknown): { code: string; message: string } {
  if (err instanceof PluginError) return { code: err.code, message: err.message };
  return { code: "internal", message: err instanceof Error ? err.message : String(err) };
}

/**
 * Whether two probes of one key show the same object: equal entity tags,
 * or, without them, equal size and a known, equal modification time.
 *
 * @param before - The first probe.
 * @param after - The second probe.
 * @returns Whether nothing changed in between.
 */
function sameObject(before: StoredObject, after: StoredObject): boolean {
  if (before.etag !== undefined && after.etag !== undefined) return before.etag === after.etag;
  return (
    before.size === after.size &&
    before.modifiedAtMs !== undefined &&
    before.modifiedAtMs === after.modifiedAtMs
  );
}

/**
 * The listing fields of a stored object.
 *
 * @param object - The object.
 * @returns Its metadata without the commit token.
 */
function metadataOf(object: StoredObject): ObjectMetadata {
  const { commitToken: _commitToken, ...metadata } = object;
  return metadata;
}

// ---------------------------------------------------------------------------
// The destination
// ---------------------------------------------------------------------------

/**
 * Serves the `storage` entrypoint over an {@link ObjectStore}. Use it from
 * a `StorageEntrypoint` subclass:
 *
 * ```ts
 * export class Storage extends StorageEntrypoint<Env> {
 *   #dest = new ObjectStoreDestination(myStore(this.env));
 *   head(key: string) { return this.#dest.head(key); }
 *   // ...one line per storage method
 * }
 * ```
 */
export class ObjectStoreDestination {
  /** The backend. */
  readonly store: ObjectStore;
  /** The change feed, when the plugin has one. */
  readonly feed: ChangeFeed | null;

  /**
   * Wraps a backend.
   *
   * @param store - The backend.
   * @param feed - The change feed, or `null` for none (the host lists).
   */
  constructor(store: ObjectStore, feed: ChangeFeed | null = null) {
    this.store = store;
    this.feed = feed;
  }

  /**
   * The `rpcFeatures` this destination supports natively, for `describe()`.
   *
   * @param store - The backend (only how it keeps write conditions is read).
   * @param options - `changes: true` when the plugin serves a feed (even one
   *   that may be off; a page from a feed that is off says so).
   * @returns Feature names.
   */
  static features(
    store: Pick<ObjectStore, "conditionalWrites">,
    options: { changes?: boolean } = {},
  ): (
    | typeof FEATURE_STORAGE_COPY
    | typeof FEATURE_STORAGE_TRANSLATE_KEYS
    | typeof FEATURE_STORAGE_CONDITIONAL_WRITE
    | typeof FEATURE_STORAGE_CHECKED_WRITE
    | typeof FEATURE_STORAGE_CHANGES
  )[] {
    const out: (
      | typeof FEATURE_STORAGE_COPY
      | typeof FEATURE_STORAGE_TRANSLATE_KEYS
      | typeof FEATURE_STORAGE_CONDITIONAL_WRITE
      | typeof FEATURE_STORAGE_CHECKED_WRITE
      | typeof FEATURE_STORAGE_CHANGES
    )[] = ["storage.copy", "storage.translateKeys"];
    if (store.conditionalWrites === "atomic") out.push("storage.conditionalWrite");
    if (store.conditionalWrites === "checked") out.push("storage.checkedWrite");
    if (options.changes === true) out.push("storage.changes");
    return out;
  }

  /**
   * Metadata without a body.
   *
   * @param key - Object key.
   * @returns The metadata, or `null` when missing.
   */
  async head(key: string): Promise<ObjectMetadata | null> {
    const found = await this.store.head(key);
    return found === null ? null : metadataOf(found);
  }

  /**
   * One page of keys.
   *
   * @param options - Prefix, cursor and limit.
   * @returns The page (the listing hints are not applied).
   */
  async list(options: ListOptions): Promise<ListPage> {
    const page = await this.store.list(
      options.prefix ?? "",
      options.cursor,
      clampPageLimit(options.limit),
    );
    const out: ListPage = {
      objects: page.objects,
      signaturesIncluded: false,
      modifiedSinceApplied: false,
    };
    if (page.nextCursor !== undefined) out.nextCursor = page.nextCursor;
    return out;
  }

  /**
   * Streamed read.
   *
   * @param key - Object key.
   * @param options - Optional byte range.
   * @returns Metadata and body.
   */
  async get(key: string, options?: { range?: ByteRange }): Promise<ReadResult> {
    const { object, body } = await this.store.get(key, checkByteRange(options?.range));
    return { meta: metadataOf(object), body };
  }

  /**
   * Streamed write: plain, conditional, or staged under a commit token.
   *
   * @param key - Object key.
   * @param body - The bytes.
   * @param options - Type, length, checksum, token, stage and condition.
   * @returns What was written; the etag is the commit token when one is set.
   */
  async put(
    key: string,
    body: ReadableStream<Uint8Array>,
    options: WriteOptions = {},
  ): Promise<PutResult> {
    let condition: WriteCondition | null;
    let target: string;
    try {
      condition = writeCondition(options);
      target = options.stageOnly === true ? stageKey(options.commitToken, key) : key;
      if (
        options.sha256 !== undefined &&
        options.sha256.length !== 0 &&
        options.sha256.length !== 32
      ) {
        throw PluginError.fromWire(
          "internal",
          `integrity: sha256 must be 32 bytes, got ${options.sha256.length}`,
        );
      }
    } catch (err) {
      // A refused write never reads its body; release it for the host.
      await body.cancel().catch(() => undefined);
      throw err;
    }
    const write: ObjectWrite = {};
    if (options.contentType !== undefined) write.contentType = options.contentType;
    if (options.contentLength !== undefined) write.contentLength = options.contentLength;
    if (options.sha256 !== undefined && options.sha256.length === 32) write.sha256 = options.sha256;
    if (options.commitToken !== undefined) write.commitToken = options.commitToken;
    const written =
      condition === null
        ? await this.store.put(target, body, write)
        : await this.putConditional(target, body, write, condition);
    const out: PutResult = { key, bytesWritten: written.bytesWritten };
    const etag = options.commitToken ?? written.etag;
    if (etag !== undefined) out.etag = etag;
    if (written.sha256 !== undefined) out.sha256 = written.sha256;
    return out;
  }

  /**
   * A conditional write through the backend's own, or a probe first.
   *
   * @param key - Object key.
   * @param body - The bytes.
   * @param write - Type, length, checksum and token.
   * @param condition - The precondition.
   * @returns What was written.
   */
  private async putConditional(
    key: string,
    body: ReadableStream<Uint8Array>,
    write: ObjectWrite,
    condition: WriteCondition,
  ): Promise<WrittenObject> {
    if (this.store.conditionalWrites !== undefined && this.store.putConditional !== undefined) {
      return this.store.putConditional(key, body, write, condition);
    }
    const current = await this.store.head(key);
    if (condition.kind === "ifNoneMatch" && current !== null) {
      await body.cancel();
      throw PluginError.fromWire("conflict", `${key} already exists`);
    }
    if (condition.kind === "ifMatch" && current?.etag !== condition.etag) {
      await body.cancel();
      throw PluginError.fromWire("conflict", `${key} changed since it was read`);
    }
    return this.store.put(key, body, write);
  }

  /**
   * Copy within the destination.
   *
   * @param from - Source key.
   * @param to - Target key.
   * @returns Bytes copied.
   */
  async copy(from: string, to: string): Promise<CopyResult> {
    return { bytesCopied: await this.store.copy(from, to) };
  }

  /**
   * Remove an object.
   *
   * @param key - Object key.
   * @returns Resolves once it is gone.
   */
  async delete(key: string): Promise<void> {
    await this.store.delete(key);
  }

  /**
   * Publish a staged write. Committing again after the stage is gone
   * succeeds when the object carries the same commit token.
   *
   * @param key - Object key.
   * @param commitToken - The stage's token.
   * @returns The published object.
   */
  async commit(key: string, commitToken: string): Promise<PutResult> {
    const staged = stageKey(commitToken, key);
    const stage = await this.store.head(staged);
    let published: StoredObject;
    if (stage !== null) {
      await this.store.copy(staged, key);
      try {
        await this.store.delete(staged);
      } catch {
        // A leftover stage costs space only; the object is published.
      }
      published = stage;
    } else {
      const current = await this.store.head(key);
      if (current === null) throw PluginError.fromWire("not_found", key);
      if (current.commitToken !== commitToken) {
        throw PluginError.fromWire(
          "not_found",
          `staged object missing and \`${key}\` is not commit ${commitToken}`,
        );
      }
      published = current;
    }
    const out: PutResult = { key, bytesWritten: published.size, etag: commitToken };
    if (published.sha256 !== undefined) out.sha256 = published.sha256;
    return out;
  }

  /**
   * Discard a staged write; a missing stage is not an error.
   *
   * @param key - Object key.
   * @param commitToken - The stage's token.
   * @returns Resolves once it is gone.
   */
  async abortStage(key: string, commitToken: string): Promise<void> {
    const staged = stageKey(commitToken, key);
    try {
      await this.store.delete(staged);
    } catch (err) {
      if (!(err instanceof PluginError && err.code === "not_found")) throw err;
    }
  }

  /**
   * Copies `source` to `to` through the plugin.
   *
   * @param source - The source object.
   * @param to - Target key.
   * @param condition - A precondition on the target.
   */
  private async copyStreamed(
    source: StoredObject,
    to: string,
    condition: WriteCondition | null,
  ): Promise<void> {
    const { object, body } = await this.store.get(source.key, undefined);
    const write: ObjectWrite = { contentLength: object.size };
    if (object.contentType !== undefined) write.contentType = object.contentType;
    if (object.sha256 !== undefined) write.sha256 = object.sha256;
    if (condition === null) await this.store.put(to, body, write);
    else await this.putConditional(to, body, write, condition);
  }

  /**
   * Moves one object: a copy, a check that it landed whole, then removing
   * the source only while it is unchanged.
   *
   * @param from - Source key.
   * @param to - Target key.
   * @param options - Overwrite rule and source precondition.
   * @returns How the move was done.
   */
  async move(from: string, to: string, options: MoveOptions = {}): Promise<MoveResult> {
    const source = await this.store.head(from);
    if (source === null) {
      if (from !== to && (await this.store.head(to)) !== null)
        return { atomic: false, alreadyMoved: true };
      throw PluginError.fromWire("not_found", from);
    }
    if (options.ifSourceMatch !== undefined && source.etag !== options.ifSourceMatch) {
      throw PluginError.fromWire("conflict", `${from} changed since it was read`);
    }
    if (from === to) return { atomic: true, alreadyMoved: false };
    if (options.noOverwrite === true && this.store.conditionalWrites !== undefined) {
      await this.copyStreamed(source, to, { kind: "ifNoneMatch" });
    } else {
      if (options.noOverwrite === true && (await this.store.head(to)) !== null) {
        throw PluginError.fromWire("conflict", `${to} already exists`);
      }
      if (this.store.serverCopy) await this.store.copy(from, to);
      else await this.copyStreamed(source, to, null);
    }
    // Check the copy landed whole before touching the source.
    const copied = await this.store.head(to);
    if (copied === null || copied.size !== source.size) {
      throw PluginError.fromWire(
        "internal",
        `integrity: ${to} does not match ${from} after the copy; the source was kept`,
      );
    }
    const now = await this.store.head(from);
    if (now !== null) {
      if (!sameObject(source, now)) {
        throw PluginError.fromWire(
          "conflict",
          `${from} changed while it was copied to ${to}; the source was kept`,
        );
      }
      await this.store.delete(from);
    }
    return { atomic: false, alreadyMoved: false };
  }

  /**
   * Moves pairs, or one page of objects under a prefix.
   *
   * @param request - Pairs or prefix, cursor, limit and options.
   * @returns Per-key outcomes and the continuation.
   */
  async moveMany(request: MoveManyRequest): Promise<MoveManyPage> {
    const options: MoveOptions = {};
    if (request.options?.noOverwrite !== undefined)
      options.noOverwrite = request.options.noOverwrite;
    const target = request.target;
    if (target.kind === "pairs") {
      if (target.value.length > MAX_BATCH_KEYS) {
        throw PluginError.fromWire(
          "invalid_params",
          `moveMany takes at most ${MAX_BATCH_KEYS} pairs`,
        );
      }
      return this.movePairs(target.value, options);
    }
    const { from, to } = target.value;
    requireDisjointPrefixes(from, to);
    const limit = Math.min(Math.max(request.limit ?? 1, 1), MAX_BATCH_KEYS);
    const page = await this.store.list(from, request.cursor, limit);
    const pairs = page.objects
      .filter((object) => object.key.startsWith(from))
      .map((object) => ({ from: object.key, to: `${to}${object.key.slice(from.length)}` }));
    const moved = await this.movePairs(pairs, options);
    const out: MoveManyPage = {
      results: moved.results,
      atomic: false,
      objectsMoved: moved.objectsMoved ?? 0,
    };
    if (page.nextCursor !== undefined) out.nextCursor = page.nextCursor;
    return out;
  }

  /**
   * Moves each pair in order, recording each outcome.
   *
   * @param pairs - Source and target keys.
   * @param options - The overwrite rule.
   * @returns The outcomes.
   */
  private async movePairs(
    pairs: { from: string; to: string }[],
    options: MoveOptions,
  ): Promise<MoveManyPage> {
    const page: MoveManyPage = { results: [], atomic: true, objectsMoved: 0 };
    for (const pair of pairs) {
      try {
        const moved = await this.move(pair.from, pair.to, options);
        page.atomic = page.atomic === true && moved.atomic;
        page.objectsMoved = (page.objectsMoved ?? 0) + 1;
        page.results.push({ key: pair.from });
      } catch (err) {
        page.results.push({ key: pair.from, error: outcomeError(err) });
      }
    }
    return page;
  }

  /**
   * Removes up to {@link MAX_BATCH_KEYS} objects.
   *
   * @param keys - Object keys.
   * @returns One outcome per key.
   */
  async deleteMany(keys: string[]): Promise<KeyOutcome[]> {
    requireBatch(keys.length, "deleteMany");
    const out: KeyOutcome[] = [];
    for (const key of keys) {
      try {
        await this.store.delete(key);
        out.push({ key });
      } catch (err) {
        out.push({ key, error: outcomeError(err) });
      }
    }
    return out;
  }

  /**
   * Probes up to {@link MAX_BATCH_KEYS} objects.
   *
   * @param keys - Object keys.
   * @returns One entry per key; `null` when missing.
   */
  async headMany(keys: string[]): Promise<(ObjectMetadata | null)[]> {
    requireBatch(keys.length, "headMany");
    const out: (ObjectMetadata | null)[] = [];
    for (const key of keys) out.push(await this.head(key));
    return out;
  }

  /**
   * Translates host keys into object-store keys.
   *
   * @param keys - Host keys.
   * @returns One translated key per host key.
   */
  translateKeys(keys: string[]): Promise<string[]> {
    try {
      requireBatch(keys.length, "translateKeys");
      return Promise.resolve(
        keys.map((key) => translateForObjectStore(parseHostKey(key), this.store.prefixBytes)),
      );
    } catch (err) {
      return Promise.reject(err);
    }
  }

  /**
   * One page of the change feed, or the "no feed" page when there is none
   * or it is off.
   *
   * @param request - Cursor and limit.
   * @returns The page.
   */
  async changes(request: ChangesRequest): Promise<ChangesPage> {
    // A page from the listing fallback carries the "no feed" cursor; to a
    // feed it is a new start.
    const cursor = request.cursor === NO_FEED_CURSOR ? undefined : request.cursor;
    const page = this.feed === null ? null : await this.feed.changes(cursor, request.limit ?? 0);
    return page ?? { changes: [], cursor: NO_FEED_CURSOR, more: false, complete: false };
  }
}
