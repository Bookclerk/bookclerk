/**
 * Backblaze B2 through its native API, as an SDK `ObjectStore`.
 *
 * The destination signs in with an application key (`b2_authorize_account`),
 * keeps the account token until B2 says it expired, and then signs in again
 * once. Every reply is read by `b2-api.ts`, the fuzzed boundary.
 *
 * - Writes of up to one part go up in one `b2_upload_file`. Longer bodies
 *   become large files: `b2_start_large_file`, one `b2_upload_part` per
 *   part, then `b2_finish_large_file`. At most two part buffers are held
 *   (two read ahead to choose between the two paths); a retry resends the
 *   buffer it already holds. A failed upload is cancelled (best effort),
 *   and a finish whose reply was lost is settled by reading the file back
 *   by id.
 * - An isolate may make a bounded number of requests per call, so parts
 *   reuse one upload URL until a failure (as B2 recommends), and a write of
 *   a declared length picks a part size that keeps its parts within
 *   {@link PART_REQUEST_BUDGET}.
 * - Reads and probes use the download URL (`GET` / `HEAD` on
 *   `/file/<bucket>/<name>`), so a probe never lists.
 * - Copies are server-side (`b2_copy_file`, or `b2_copy_part` above 5 GB).
 * - A delete removes every version of the name, oldest first, so nothing
 *   stays billed behind a hide marker and a delete that fails part way
 *   never uncovers an older version.
 * - Requests have a deadline for their reply headers and an idle timeout on
 *   the body, so a long download that keeps moving is never cut off. An
 *   upload's deadline covers sending its body too, so it grows with the
 *   body's size (one second per 64 KiB on top of five minutes).
 *
 * B2 has no conditional writes, so the store declares checked ones: it
 * writes, then reads the name's versions and withdraws its own write when
 * another writer got in first.
 */

import {
  PluginError,
  type ByteRange,
  type ObjectInfo,
  type ObjectPage,
  type ObjectStore,
  type ObjectWrite,
  type StoredObject,
  type WriteCondition,
  type WrittenObject,
} from "@bookclerk/plugin-sdk/workerd";
import {
  B2ReplyError,
  MAX_B2_REPLY_BYTES,
  MAX_PARTS,
  compareNames,
  describeApiError,
  encodeFileName,
  expiredToken,
  fileNameProblem,
  isStored,
  notFound,
  parseApiError,
  parseAuthorization,
  parseB2Url,
  parseBuckets,
  parseDownloadHeaders,
  parseFileVersion,
  parseNamesPage,
  parsePart,
  parseUploadTarget,
  parseVersionsPage,
  retryable,
  type ApiError,
  type Authorization,
  type DownloadHead,
  type FileVersion,
  type UploadTarget,
} from "./b2-api.ts";
import {
  ownRules,
  ownRulesInPlace,
  parseNotificationRules,
  ruleNames,
  rulesWith,
  type B2SigningSecret,
  type NotificationRule,
} from "./b2-events.ts";
import { Sha256Stream, hex, parseSha256Hex, sha1Hex } from "./hash.ts";

/** Where `b2_authorize_account` lives; every other URL comes from its reply. */
export const B2_AUTHORIZE_URL = "https://api.backblazeb2.com";
/** Path of the version 3 API calls under an API URL. */
const API_PATH = "/b2api/v3/";
/** Path of the version 4 API calls: the event notification rules are only there. */
const API_V4_PATH = "/b2api/v4/";
/** Tries per call, counting the first, for retryable failures. */
const ATTEMPTS = 4;
/** Smallest upload part, in bytes (B2's own minimum may be larger). */
export const MULTIPART_PART_SIZE = 8 * 1024 * 1024;
/** Largest part B2 accepts, in bytes. */
const MAX_PART_BYTES = 5_000_000_000;
/** Largest object a write of unknown length may reach, in bytes. */
export const MAX_SUPPORTED_OBJECT_BYTES = MULTIPART_PART_SIZE * MAX_PARTS;
/**
 * Most parts one large upload plans for, below the isolate's 1,000
 * requests per call so the start, finish and URL calls still fit.
 */
export const PART_REQUEST_BUDGET = 900;
/** Largest file `b2_copy_file` copies in one call, in bytes. */
const COPY_FILE_MAX_BYTES = 5_000_000_000;
/** Part size of a large server-side copy, in bytes. */
const COPY_PART_BYTES = 1024 * 1024 * 1024;
/** Versions asked for per `b2_list_file_versions` page. */
const VERSIONS_PAGE = 1000;
/** Most versions of one name a walk reads; a name with more is refused rather than half deleted. */
const MAX_VERSIONS_WALKED = 10_000;
/** Most version pages one walk asks for, so a listing of empty pages cannot loop forever. */
const MAX_VERSION_PAGES = 4 * (MAX_VERSIONS_WALKED / VERSIONS_PAGE) + 4;
/** Longest wait for a request's reply headers, in milliseconds. */
const SEND_DEADLINE_MS = 300_000;
/** Slowest upload rate allowed for: the deadline grows one second per this many bytes. */
const MIN_UPLOAD_BYTES_PER_SEC = 64 * 1024;
/** Longest wait for the next chunk of a reply body, in milliseconds. */
const BODY_IDLE_MS = 60_000;
/** Content type that asks B2 to pick one from the file name. */
const AUTO_CONTENT_TYPE = "b2/x-auto";

/** The `fetch` the store sends with (the isolate's own, or a test double). */
export type Fetcher = (input: string, init: RequestInit) => Promise<Response>;

/** How to reach one bucket. */
export interface B2Settings {
  /** The application key's id. */
  keyId: string;
  /** The application key (secret). */
  applicationKey: string;
  /** The bucket's name. */
  bucket: string;
  /** Prefix under which every key is stored (may be empty). */
  prefix: string;
  /** Base URL of `b2_authorize_account`; {@link B2_AUTHORIZE_URL} when absent. */
  authorizeUrl?: string;
}

/** What the store runs on; tests replace each. */
export interface B2Runtime {
  /** Sends requests. */
  fetch: Fetcher;
  /** Waits between retries; `setTimeout` when absent. */
  sleep?: (ms: number) => Promise<void>;
  /** Smallest part size, in bytes; {@link MULTIPART_PART_SIZE} when absent. */
  partFloor?: number;
  /** Largest single-call copy, in bytes (tests shrink it). */
  copyFileMaxBytes?: number;
  /** Part size of a large copy, in bytes (tests shrink it). */
  copyPartBytes?: number;
}

/** One sign-in: what the key may do, and the bucket it resolved to. */
interface Session {
  /** The authorization reply. */
  auth: Authorization;
  /** The bucket's id, which API calls take. */
  bucketId: string;
}

/** A settled reply: its body, or the API error it carries. */
type Settled = { ok: true; bytes: Uint8Array } | { ok: false; error: ApiError };

/** What a download request found. */
type Fetched =
  | { kind: "missing" }
  | { kind: "unsatisfiable" }
  | { kind: "found"; head: DownloadHead; reply: Response };

/** Builds a plugin error. */
const fail = (code: string, message: string): PluginError => PluginError.fromWire(code, message);

/**
 * A backend failure, reported as `internal` with what was being done.
 *
 * @param what - The operation.
 * @param detail - What went wrong.
 * @returns The error.
 */
function remote(what: string, detail: unknown): PluginError {
  const text = detail instanceof Error ? detail.message : String(detail);
  return fail("internal", `B2 ${what}: ${text}`);
}

/**
 * Maps an API error reply: a missing file is `not_found`, the rest `internal`.
 *
 * @param what - The operation.
 * @param err - The reply.
 * @returns The error.
 */
function apiError(what: string, err: ApiError): PluginError {
  return notFound(err)
    ? fail("not_found", `${what}: ${describeApiError(err)}`)
    : remote(what, describeApiError(err));
}

/**
 * True when `err` is a plugin error with `code`.
 *
 * @param err - Anything thrown.
 * @param code - The wire code.
 * @returns Whether it matches.
 */
function hasCode(err: unknown, code: string): boolean {
  return err instanceof PluginError && err.code === code;
}

/**
 * The normalized prefix: no leading `/`, and empty or ending in `/`.
 *
 * @param prefix - The setting.
 * @returns The prefix.
 */
export function normalizePrefix(prefix: string): string {
  let trimmed = prefix.trim();
  while (trimmed.startsWith("/")) trimmed = trimmed.slice(1);
  if (trimmed === "") return "";
  return trimmed.endsWith("/") ? trimmed : `${trimmed}/`;
}

/**
 * A content type safe to send as a header, or B2's automatic one.
 *
 * @param kind - The requested type.
 * @returns The header value.
 */
export function contentTypeHeader(kind: string | null | undefined): string {
  if (
    kind !== null &&
    kind !== undefined &&
    kind.length > 0 &&
    kind.length <= 256 &&
    /^[\x20-\x7e]+$/u.test(kind)
  ) {
    return kind;
  }
  return AUTO_CONTENT_TYPE;
}

/**
 * The file info stored with a write: the body's SHA-256 and the commit
 * token, the same names the other object stores use for user metadata.
 *
 * @param sha256Hex - The body's SHA-256, when known.
 * @param commitToken - The commit token, when there is one.
 * @returns The info entries, by name.
 */
export function fileInfo(
  sha256Hex: string | undefined,
  commitToken: string | undefined,
): Map<string, string> {
  const info = new Map<string, string>();
  if (sha256Hex !== undefined) info.set("sha256", sha256Hex);
  if (commitToken !== undefined) info.set("commit-token", commitToken);
  return info;
}

/**
 * The part size for a write: at least the floor and B2's minimum, and for
 * a declared length large enough to keep the parts within
 * {@link PART_REQUEST_BUDGET}, rounded up to a whole MiB.
 *
 * @param floor - Smallest part size.
 * @param minimum - B2's absolute minimum part size.
 * @param declared - The body's declared length, when there is one.
 * @returns The part size, in bytes.
 * @throws {PluginError} `payload_too_large` when no part size fits.
 */
export function partSizeFor(floor: number, minimum: number, declared?: number): number {
  let size = Math.max(floor, minimum);
  if (declared !== undefined && declared > size * PART_REQUEST_BUDGET) {
    const mib = 1024 * 1024;
    size = Math.max(size, Math.ceil(declared / PART_REQUEST_BUDGET / mib) * mib);
  }
  if (size > MAX_PART_BYTES) {
    throw fail("payload_too_large", `B2 part size ${size} exceeds ${MAX_PART_BYTES}`);
  }
  return size;
}

/**
 * Waits for a reply body's next chunk, at most {@link BODY_IDLE_MS}.
 *
 * @param reader - The body reader.
 * @param what - The operation, for errors.
 * @returns The chunk, or the end.
 */
async function nextChunk(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  what: string,
): Promise<ReadableStreamReadResult<Uint8Array>> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const stall = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      reject(remote(what, "the reply stalled"));
    }, BODY_IDLE_MS);
  });
  try {
    return await Promise.race([reader.read(), stall]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Reads a reply body, stopping one byte past {@link MAX_B2_REPLY_BYTES} so
 * an oversized reply never sits whole in memory (the parser refuses it).
 *
 * @param reply - The reply.
 * @returns The body.
 */
async function readReply(reply: Response): Promise<Uint8Array> {
  if (reply.body === null) return new Uint8Array(0);
  const reader = reply.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await nextChunk(reader, "reply");
      if (done) break;
      const room = MAX_B2_REPLY_BYTES + 1 - total;
      const take = value.length > room ? value.subarray(0, room) : value;
      chunks.push(take);
      total += take.length;
      if (total > MAX_B2_REPLY_BYTES) {
        await reader.cancel().catch(() => undefined);
        break;
      }
    }
  } finally {
    reader.releaseLock();
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.length;
  }
  return out;
}

/**
 * Reads a reply into its body, or the API error it carries.
 *
 * @param reply - The reply.
 * @returns The settled reply.
 */
async function settle(reply: Response): Promise<Settled> {
  const bytes = await readReply(reply);
  return reply.ok ? { ok: true, bytes } : { ok: false, error: parseApiError(reply.status, bytes) };
}

/**
 * Reads up to `size` bytes from `reader` into one buffer; fewer only at the
 * end. Bytes past `size` are kept in `carry` for the next part.
 *
 * @param source - The body and what is left of its last chunk.
 * @param size - The part size.
 * @returns The part.
 */
async function readPart(source: PartSource, size: number): Promise<Uint8Array<ArrayBuffer>> {
  const part = new Uint8Array(size);
  let filled = 0;
  while (filled < size) {
    if (source.carry.length === 0) {
      const { done, value } = await source.reader.read();
      if (done) break;
      source.carry = value;
      continue;
    }
    const take = Math.min(size - filled, source.carry.length);
    part.set(source.carry.subarray(0, take), filled);
    source.carry = source.carry.subarray(take);
    filled += take;
  }
  return filled === size ? part : part.slice(0, filled);
}

/** A write's body, read part by part. */
interface PartSource {
  /** The body reader. */
  reader: ReadableStreamDefaultReader<Uint8Array>;
  /** Bytes read but not yet placed in a part. */
  carry: Uint8Array;
}

/** Running length and digest of a write's body, checked against what the caller declared. */
class BodyTally {
  /** Bytes seen. */
  total = 0;
  /** SHA-256 of the bytes seen. */
  readonly #hasher = new Sha256Stream();
  /** Declared length, when there is one. */
  readonly #expected: number | undefined;

  /**
   * Starts a tally.
   *
   * @param expected - The declared length.
   */
  constructor(expected: number | undefined) {
    this.#expected = expected;
  }

  /**
   * Adds the next bytes.
   *
   * @param bytes - The bytes.
   * @returns Resolves once hashed.
   * @throws {PluginError} When the body outgrows its declared length or the
   *   largest object.
   */
  async take(bytes: Uint8Array): Promise<void> {
    const next = this.total + bytes.length;
    if (this.#expected !== undefined && next > this.#expected) {
      throw fail("internal", `integrity: B2 upload exceeded declared length ${this.#expected}`);
    }
    if (next > MAX_SUPPORTED_OBJECT_BYTES && this.#expected === undefined) {
      throw fail("payload_too_large", `B2 upload exceeded ${MAX_SUPPORTED_OBJECT_BYTES} bytes`);
    }
    await this.#hasher.update(bytes);
    this.total = next;
  }

  /**
   * Checks the whole body against the declared length and digest.
   *
   * @param expectedSha - The declared SHA-256, as hex.
   * @returns The body's SHA-256, as hex.
   * @throws {PluginError} When the body differs from what was declared.
   */
  async finish(expectedSha: string | undefined): Promise<string> {
    const digest = await this.#hasher.finish();
    if (expectedSha !== undefined && expectedSha !== digest) {
      throw fail("internal", "integrity: B2 upload sha256 does not match the body");
    }
    if (this.#expected !== undefined && this.total !== this.#expected) {
      throw fail(
        "internal",
        `integrity: B2 upload body was ${this.total} bytes, declared ${this.#expected}`,
      );
    }
    return digest;
  }
}

/**
 * The object a stored version describes.
 *
 * @param key - The host key.
 * @param version - The version.
 * @returns The object.
 */
export function storedObject(key: string, version: FileVersion): StoredObject {
  const out: StoredObject = { key, size: version.contentLength };
  if (version.contentType !== null) out.contentType = version.contentType;
  const sha = version.fileInfo.get("sha256");
  const digest = sha === undefined ? null : parseSha256Hex(sha);
  if (digest !== null) out.sha256 = digest;
  const token = version.fileInfo.get("commit-token");
  if (token !== undefined) out.commitToken = token;
  if (version.fileId !== null) {
    out.etag = version.fileId;
    out.versionId = version.fileId;
    out.signature = version.fileId;
  }
  if (version.uploadTimestampMs > 0) out.modifiedAtMs = version.uploadTimestampMs;
  return out;
}

/**
 * Wraps a download body so it holds exactly `expected` bytes, failing on a
 * short body or a stalled one and cutting off any excess.
 *
 * @param body - The reply body.
 * @param expected - The length its headers gave.
 * @returns The checked body.
 */
function sizedBody(
  body: ReadableStream<Uint8Array> | null,
  expected: number,
): ReadableStream<Uint8Array> {
  if (body === null || expected === 0) {
    if (body !== null) void body.cancel().catch(() => undefined);
    if (expected === 0) return new Blob([]).stream();
  }
  const reader = body?.getReader();
  let read = 0;
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      if (reader === undefined) {
        controller.error(remote("download", "the reply has no body"));
        return;
      }
      const { done, value } = await nextChunk(reader, "download");
      if (done) {
        if (read < expected) {
          controller.error(remote("download", "the download ended before its Content-Length"));
        } else {
          controller.close();
        }
        return;
      }
      const room = expected - read;
      const take = value.length > room ? value.subarray(0, room) : value;
      read += take.length;
      if (take.length > 0) controller.enqueue(take);
      if (read >= expected) {
        await reader.cancel().catch(() => undefined);
        controller.close();
      }
    },
    async cancel(reason) {
      await reader?.cancel(reason).catch(() => undefined);
    },
  });
}

/**
 * Cancels a body nobody will read, best effort.
 *
 * @param body - The body.
 * @returns Resolves once cancelled.
 */
async function discard(body: ReadableStream<Uint8Array>): Promise<void> {
  await body.cancel().catch(() => undefined);
}

/** A bucket (and prefix) on Backblaze B2. */
export class B2Store implements ObjectStore {
  /** Copies run on B2. */
  readonly serverCopy = true;
  /** B2 has no conditional upload: writes are checked after the fact. */
  readonly conditionalWrites = "checked" as const;
  /** UTF-8 bytes of the prefix. */
  readonly prefixBytes: number;
  /** The bucket's name. */
  readonly bucket: string;
  /** The normalized prefix (empty or ending in `/`). */
  readonly prefix: string;
  /** The settings, with the prefix normalized. */
  readonly #settings: Required<B2Settings>;
  /** What the store runs on. */
  readonly #runtime: B2Runtime;
  /** The current sign-in, made on first use. */
  #session: Promise<Session> | null = null;

  /**
   * A store for a bucket. Nothing is sent until the first call, which signs
   * in, finds the bucket, and checks the key may reach the prefix.
   *
   * @param settings - Key, bucket and prefix.
   * @param runtime - `fetch` and the test knobs.
   * @throws {PluginError} `invalid_params` for an empty key id or bucket, a
   *   prefix B2 cannot store, or a sign-in URL that is not plain https.
   */
  constructor(settings: B2Settings, runtime: B2Runtime) {
    if (settings.keyId.trim() === "") {
      throw fail("invalid_params", "B2 application key id is empty");
    }
    if (settings.bucket.trim() === "") throw fail("invalid_params", "B2 bucket is empty");
    const prefix = normalizePrefix(settings.prefix);
    if (prefix !== "") {
      const problem = fileNameProblem(prefix.slice(0, -1));
      if (problem !== null) throw fail("invalid_params", `B2 prefix: ${problem}`);
    }
    let authorizeUrl = (settings.authorizeUrl ?? B2_AUTHORIZE_URL).trim();
    while (authorizeUrl.endsWith("/")) authorizeUrl = authorizeUrl.slice(0, -1);
    // The application key goes to this URL, so it must be plain https.
    try {
      parseB2Url(authorizeUrl);
    } catch (err) {
      throw fail(
        "invalid_params",
        `B2 sign-in URL: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
    this.#settings = { ...settings, prefix, authorizeUrl };
    this.#runtime = runtime;
    this.bucket = settings.bucket;
    this.prefix = prefix;
    this.prefixBytes = new TextEncoder().encode(prefix).length;
  }

  // -------------------------------------------------------------------------
  // Plumbing
  // -------------------------------------------------------------------------

  /**
   * Waits before try `attempt + 1`: 250 ms, 500 ms, 1 s.
   *
   * @param attempt - The try that failed, from 1.
   * @returns Resolves after the wait.
   */
  async #backoff(attempt: number): Promise<void> {
    const ms = 250 * 2 ** Math.min(Math.max(attempt - 1, 0), 4);
    const sleep =
      this.#runtime.sleep ??
      ((wait: number) =>
        new Promise<void>((resolve) => {
          setTimeout(resolve, wait);
        }));
    await sleep(ms);
  }

  /**
   * Sends a request, waiting at most `deadlineMs` for the reply headers.
   *
   * @param url - Where to.
   * @param init - Method, headers and body.
   * @param what - The operation, for errors.
   * @param deadlineMs - The deadline.
   * @returns The reply.
   */
  async #send(
    url: string,
    init: RequestInit,
    what: string,
    deadlineMs = SEND_DEADLINE_MS,
  ): Promise<Response> {
    const abort = new AbortController();
    const timer = setTimeout(() => {
      abort.abort();
    }, deadlineMs);
    try {
      return await this.#runtime.fetch(url, { ...init, signal: abort.signal, redirect: "manual" });
    } catch (err) {
      throw remote(what, abort.signal.aborted ? "no reply before the deadline" : err);
    } finally {
      clearTimeout(timer);
    }
  }

  /**
   * Parses a reply with a `b2-api` reader, mapping its error.
   *
   * @param what - The operation.
   * @param parse - The reader.
   * @param input - The reply body (or headers).
   * @returns What the reader returns.
   */
  #parse<A, T>(what: string, parse: (input: A) => T, input: A): T {
    try {
      return parse(input);
    } catch (err) {
      if (err instanceof B2ReplyError) throw remote(what, err);
      throw err;
    }
  }

  /**
   * The stored name of `key`.
   *
   * @param key - The host key.
   * @returns The name.
   * @throws {PluginError} `invalid_params` when B2 cannot store it.
   */
  #nameFor(key: string): string {
    const name = `${this.prefix}${key}`;
    const problem = fileNameProblem(name);
    if (problem !== null) throw fail("invalid_params", `${key}: ${problem}`);
    return name;
  }

  /**
   * Signs in and resolves the bucket.
   *
   * @returns The session.
   */
  async #authorize(): Promise<Session> {
    const s = this.#settings;
    const basic = btoa(
      String.fromCharCode(...new TextEncoder().encode(`${s.keyId}:${s.applicationKey}`)),
    );
    const reply = await this.#send(
      `${s.authorizeUrl}${API_PATH}b2_authorize_account`,
      { method: "GET", headers: { Authorization: `Basic ${basic}` } },
      "sign-in",
    );
    const settled = await settle(reply);
    if (!settled.ok) throw remote("sign-in", describeApiError(settled.error));
    const auth = this.#parse("sign-in", (bytes) => parseAuthorization(bytes), settled.bytes);
    let bucketId: string;
    if (auth.bucket !== null) {
      if (auth.bucket.name !== s.bucket) {
        throw fail(
          "invalid_params",
          `the B2 application key only reaches bucket ${auth.bucket.name}`,
        );
      }
      bucketId = auth.bucket.id;
    } else {
      bucketId = await this.#findBucket(auth);
    }
    if (auth.namePrefix !== null && !s.prefix.startsWith(auth.namePrefix)) {
      throw fail(
        "invalid_params",
        `the B2 application key only reaches names under ${auth.namePrefix}; set the prefix under it`,
      );
    }
    return { auth, bucketId };
  }

  /**
   * Looks the bucket up by name with an unrestricted key.
   *
   * @param auth - The sign-in.
   * @returns The bucket's id.
   */
  async #findBucket(auth: Authorization): Promise<string> {
    const bucket = this.#settings.bucket;
    const reply = await this.#send(
      `${auth.apiUrl}${API_PATH}b2_list_buckets`,
      {
        method: "POST",
        headers: { Authorization: auth.token, "Content-Type": "application/json" },
        body: JSON.stringify({ accountId: auth.accountId, bucketName: bucket }),
      },
      "bucket lookup",
    );
    const settled = await settle(reply);
    if (!settled.ok) throw remote("bucket lookup", describeApiError(settled.error));
    const found = this.#parse("bucket lookup", parseBuckets, settled.bytes).find(
      (entry) => entry.name === bucket,
    );
    if (found === undefined) {
      throw fail("invalid_params", `B2 bucket ${bucket} does not exist or the key cannot see it`);
    }
    return found.id;
  }

  /**
   * The current sign-in, made on first use.
   *
   * @returns The session.
   */
  #currentSession(): Promise<Session> {
    if (this.#session === null) {
      const fresh = this.#authorize();
      this.#session = fresh;
      // A failed sign-in is not kept: the next call tries again.
      fresh.catch(() => {
        if (this.#session === fresh) this.#session = null;
      });
    }
    return this.#session;
  }

  /**
   * Signs in again after `stale` expired, unless another call already did.
   *
   * @param stale - The expired session.
   * @returns The new session.
   */
  async #refresh(stale: Session): Promise<Session> {
    const current = this.#session;
    if (current !== null) {
      const live = await current.catch(() => null);
      if (live !== null && live !== stale) return live;
    }
    // Another call may have started a new sign-in while this one waited.
    if (this.#session === current) this.#session = null;
    return this.#currentSession();
  }

  /**
   * Calls an API operation, signing in again once when the token expired
   * and retrying busy or failed requests.
   *
   * @param op - The operation name.
   * @param body - The JSON arguments (a `POST`), or query parameters for a `GET`.
   * @param parse - Reads the reply.
   * @param options - API path and method.
   * @returns What `parse` returns.
   */
  async #call<T>(
    op: string,
    body: Record<string, unknown>,
    parse: (bytes: Uint8Array) => T,
    options: { path?: string; get?: boolean } = {},
  ): Promise<T> {
    let session = await this.#currentSession();
    let refreshed = false;
    for (let attempt = 1; ; attempt++) {
      const base = `${session.auth.apiUrl}${options.path ?? API_PATH}${op}`;
      let settled: Settled | PluginError;
      try {
        const reply =
          options.get === true
            ? await this.#send(
                `${base}?${new URLSearchParams(body as Record<string, string>).toString()}`,
                { method: "GET", headers: { Authorization: session.auth.token } },
                op,
              )
            : await this.#send(
                base,
                {
                  method: "POST",
                  headers: {
                    Authorization: session.auth.token,
                    "Content-Type": "application/json",
                  },
                  body: JSON.stringify(body),
                },
                op,
              );
        settled = await settle(reply);
      } catch (err) {
        if (!(err instanceof PluginError)) throw err;
        settled = err;
      }
      if (settled instanceof PluginError) {
        if (attempt < ATTEMPTS) {
          await this.#backoff(attempt);
          continue;
        }
        throw settled;
      }
      if (settled.ok) return this.#parse(op, parse, settled.bytes);
      const err = settled.error;
      if (expiredToken(err) && !refreshed) {
        refreshed = true;
        session = await this.#refresh(session);
      } else if (retryable(err) && attempt < ATTEMPTS) {
        await this.#backoff(attempt);
      } else {
        throw apiError(op, err);
      }
    }
  }

  // -------------------------------------------------------------------------
  // Uploads
  // -------------------------------------------------------------------------

  /**
   * Posts `data` to an upload URL, fetching a new URL after any failure as
   * B2 asks. `target` holds the URL in use, so the parts of one large file
   * share it.
   *
   * @param target - The URL in use (refilled by `fetchTarget` when empty).
   * @param fetchTarget - Asks B2 for a new URL.
   * @param data - The bytes.
   * @param headers - Upload headers.
   * @returns The reply body.
   */
  async #upload(
    target: { current: UploadTarget | null },
    fetchTarget: () => Promise<UploadTarget>,
    data: Uint8Array<ArrayBuffer>,
    headers: Record<string, string>,
  ): Promise<Uint8Array> {
    const sha1 = await sha1Hex(data);
    const deadline = SEND_DEADLINE_MS + Math.floor(data.length / MIN_UPLOAD_BYTES_PER_SEC) * 1000;
    for (let attempt = 1; ; attempt++) {
      target.current ??= await fetchTarget();
      const { url, token } = target.current;
      let settled: Settled | PluginError;
      try {
        const reply = await this.#send(
          url,
          {
            method: "POST",
            headers: { ...headers, Authorization: token, "X-Bz-Content-Sha1": sha1 },
            body: data,
          },
          "upload",
          deadline,
        );
        settled = await settle(reply);
      } catch (err) {
        if (!(err instanceof PluginError)) throw err;
        settled = err;
      }
      if (settled instanceof PluginError) {
        target.current = null;
        if (attempt < ATTEMPTS) {
          await this.#backoff(attempt);
          continue;
        }
        throw settled;
      }
      if (settled.ok) return settled.bytes;
      target.current = null;
      if (attempt < ATTEMPTS && (retryable(settled.error) || settled.error.status === 401)) {
        await this.#backoff(attempt);
        continue;
      }
      throw apiError("upload", settled.error);
    }
  }

  /**
   * Uploads a whole small file.
   *
   * @param session - The sign-in.
   * @param name - The stored name.
   * @param data - The bytes.
   * @param contentType - The content type header.
   * @param info - File info.
   * @returns The stored version.
   */
  async #uploadSmall(
    session: Session,
    name: string,
    data: Uint8Array<ArrayBuffer>,
    contentType: string,
    info: Map<string, string>,
  ): Promise<FileVersion> {
    const headers: Record<string, string> = {
      "X-Bz-File-Name": encodeFileName(name),
      "Content-Type": contentType,
    };
    for (const [key, value] of info) headers[`X-Bz-Info-${key}`] = encodeFileName(value);
    const reply = await this.#upload(
      { current: null },
      () =>
        this.#call("b2_get_upload_url", { bucketId: session.bucketId }, (bytes) =>
          parseUploadTarget(bytes),
        ),
      data,
      headers,
    );
    const version = this.#parse("upload", parseFileVersion, reply);
    if (
      version.fileName !== name ||
      version.contentLength !== data.length ||
      version.contentSha1 !== (await sha1Hex(data))
    ) {
      throw remote("upload", "the stored file does not match the upload");
    }
    return version;
  }

  /**
   * Uploads one part of a large file.
   *
   * @param target - The part URL in use.
   * @param fileId - The large file.
   * @param number - The part number, from 1.
   * @param data - The bytes.
   * @returns The part's SHA-1.
   */
  async #uploadPart(
    target: { current: UploadTarget | null },
    fileId: string,
    number: number,
    data: Uint8Array<ArrayBuffer>,
  ): Promise<string> {
    const reply = await this.#upload(
      target,
      () => this.#call("b2_get_upload_part_url", { fileId }, (bytes) => parseUploadTarget(bytes)),
      data,
      { "X-Bz-Part-Number": String(number) },
    );
    const part = this.#parse("upload part", parsePart, reply);
    const sha1 = await sha1Hex(data);
    if (part.partNumber !== number || part.contentSha1 !== sha1) {
      throw remote("upload part", "the stored part does not match");
    }
    return sha1;
  }

  /**
   * Cancels an unfinished large file, best effort.
   *
   * @param fileId - The large file.
   * @returns Resolves once tried.
   */
  async #cancelLargeFile(fileId: string): Promise<void> {
    try {
      await this.#call("b2_cancel_large_file", { fileId }, () => undefined);
    } catch {
      // Best effort: an unfinished file B2 keeps is removed by the next delete.
    }
  }

  /**
   * Starts a large file.
   *
   * @param session - The sign-in.
   * @param name - The stored name.
   * @param contentType - The content type.
   * @param info - File info.
   * @returns The file id.
   */
  async #startLargeFile(
    session: Session,
    name: string,
    contentType: string,
    info: Map<string, string>,
  ): Promise<string> {
    const started = await this.#call(
      "b2_start_large_file",
      {
        bucketId: session.bucketId,
        fileName: name,
        contentType,
        fileInfo: Object.fromEntries(info),
      },
      parseFileVersion,
    );
    if (started.fileName !== name || started.action !== "start") {
      throw remote("start large file", "the reply names another file");
    }
    if (started.fileId === null) throw remote("start large file", "no file id");
    return started.fileId;
  }

  /**
   * Finishes a large file from its parts' SHA-1s. When the finish fails
   * (for example a retry after a lost reply), the file is read back by id:
   * a finished file under that id is the result, and anything else throws
   * the finish's own error so the caller still cancels.
   *
   * @param fileId - The large file.
   * @param sha1s - The parts' SHA-1s, in order.
   * @returns The finished version.
   */
  async #finishLargeFile(fileId: string, sha1s: string[]): Promise<FileVersion> {
    const finishedOk = (version: FileVersion): boolean =>
      version.action === "upload" && version.fileId === fileId;
    let failure: unknown;
    try {
      const version = await this.#call(
        "b2_finish_large_file",
        { fileId, partSha1Array: sha1s },
        parseFileVersion,
      );
      if (finishedOk(version)) return version;
      return Promise.reject(remote("finish large file", "the reply names another file"));
    } catch (err) {
      failure = err;
    }
    try {
      const version = await this.#call("b2_get_file_info", { fileId }, parseFileVersion);
      if (finishedOk(version)) return version;
    } catch {
      // Fall through to the finish's own error.
    }
    throw failure;
  }

  /**
   * Streams `body` into `key`.
   *
   * @param key - The host key.
   * @param body - The bytes.
   * @param write - Declared length, digest, type and commit token.
   * @returns What was stored and the stored version.
   */
  async #write(
    key: string,
    body: ReadableStream<Uint8Array>,
    write: ObjectWrite,
  ): Promise<{ written: WrittenObject; version: FileVersion }> {
    let reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
    try {
      const name = this.#nameFor(key);
      const expectedSha = write.sha256 === undefined ? undefined : hex(write.sha256);
      const declared = write.contentLength;
      if (declared !== undefined && declared > MAX_PART_BYTES * MAX_PARTS) {
        throw fail("payload_too_large", `object length ${declared} is too large for B2`);
      }
      const session = await this.#currentSession();
      const partSize = partSizeFor(
        this.#runtime.partFloor ?? MULTIPART_PART_SIZE,
        session.auth.absoluteMinimumPartSize,
        declared,
      );
      const contentType = contentTypeHeader(write.contentType);
      const tally = new BodyTally(declared);
      reader = body.getReader();
      const source: PartSource = { reader, carry: new Uint8Array(0) };
      const first = await readPart(source, partSize);
      await tally.take(first);
      const second =
        first.length === partSize ? await readPart(source, partSize) : new Uint8Array(0);
      if (second.length === 0) {
        const digest = await tally.finish(expectedSha);
        const info = fileInfo(digest, write.commitToken);
        const version = await this.#uploadSmall(session, name, first, contentType, info);
        return { written: this.#written(tally.total, digest, version), version };
      }
      await tally.take(second);
      const info = fileInfo(expectedSha, write.commitToken);
      const fileId = await this.#startLargeFile(session, name, contentType, info);
      try {
        const target = { current: null as UploadTarget | null };
        const sha1s = [await this.#uploadPart(target, fileId, 1, first)];
        sha1s.push(await this.#uploadPart(target, fileId, 2, second));
        for (let number = 3; ; number++) {
          const part = await readPart(source, partSize);
          if (part.length === 0) break;
          if (number > MAX_PARTS) {
            throw fail("payload_too_large", `B2 upload exceeded ${MAX_PARTS} parts`);
          }
          await tally.take(part);
          sha1s.push(await this.#uploadPart(target, fileId, number, part));
        }
        const digest = await tally.finish(expectedSha);
        const version = await this.#finishLargeFile(fileId, sha1s);
        if (version.fileName !== name || version.contentLength !== tally.total) {
          throw remote("finish large file", "the stored file does not match");
        }
        return { written: this.#written(tally.total, digest, version), version };
      } catch (err) {
        await this.#cancelLargeFile(fileId);
        throw err;
      }
    } catch (err) {
      if (reader === null) await discard(body);
      else await reader.cancel().catch(() => undefined);
      throw err;
    }
  }

  /**
   * What a write reports.
   *
   * @param total - Bytes stored.
   * @param digest - The body's SHA-256, as hex.
   * @param version - The stored version.
   * @returns The report.
   */
  #written(total: number, digest: string, version: FileVersion): WrittenObject {
    const out: WrittenObject = { bytesWritten: total };
    if (version.fileId !== null) out.etag = version.fileId;
    const sha = parseSha256Hex(digest);
    if (sha !== null) out.sha256 = sha;
    return out;
  }

  // -------------------------------------------------------------------------
  // Downloads and listings
  // -------------------------------------------------------------------------

  /**
   * Requests `name` from the download URL.
   *
   * @param method - `GET` or `HEAD`.
   * @param name - The stored name.
   * @param range - A `Range` header value, when reading part of the file.
   * @returns What the request found.
   */
  async #fetch(method: "GET" | "HEAD", name: string, range?: string): Promise<Fetched> {
    let session = await this.#currentSession();
    let refreshed = false;
    for (let attempt = 1; ; attempt++) {
      const url = `${session.auth.downloadUrl}/file/${encodeURIComponent(this.bucket)}/${encodeFileName(name)}`;
      const headers: Record<string, string> = { Authorization: session.auth.token };
      if (range !== undefined) headers["Range"] = range;
      let reply: Response;
      try {
        reply = await this.#send(url, { method, headers }, "download");
      } catch (err) {
        if (err instanceof PluginError && attempt < ATTEMPTS) {
          await this.#backoff(attempt);
          continue;
        }
        throw err;
      }
      const status = reply.status;
      if (status === 200 || status === 206) {
        let head: DownloadHead;
        try {
          head = this.#parse("download", parseDownloadHeaders, reply.headers.entries());
        } catch (err) {
          if (reply.body !== null) await discard(reply.body);
          throw err;
        }
        if (head.version.fileName !== name) {
          if (reply.body !== null) await discard(reply.body);
          throw remote("download", "the reply names another file");
        }
        return { kind: "found", head, reply };
      }
      const err =
        method === "HEAD"
          ? parseApiError(status, new Uint8Array(0))
          : parseApiError(status, await readReply(reply));
      if (method === "HEAD" && reply.body !== null) await discard(reply.body);
      if (status === 404) return { kind: "missing" };
      if (status === 416) return { kind: "unsatisfiable" };
      // A HEAD reply has no body, so an expired token reads as a bare 401:
      // sign in again once for either.
      if (status === 401 && !refreshed) {
        refreshed = true;
        session = await this.#refresh(session);
      } else if (retryable(err) && attempt < ATTEMPTS) {
        await this.#backoff(attempt);
      } else {
        throw apiError("download", err);
      }
    }
  }

  /**
   * The stored version of `key`, or `null`.
   *
   * @param key - The host key.
   * @returns The version.
   */
  async #stat(key: string): Promise<FileVersion | null> {
    const found = await this.#fetch("HEAD", this.#nameFor(key));
    switch (found.kind) {
      case "found":
        return found.head.version;
      case "missing":
        return null;
      case "unsatisfiable":
        throw remote("probe", "HEAD answered 416");
    }
  }

  /**
   * Every version of `name`, newest first.
   *
   * @param name - The stored name.
   * @returns The versions.
   */
  async #versionsOf(name: string): Promise<FileVersion[]> {
    const session = await this.#currentSession();
    const versions: FileVersion[] = [];
    let start: { name: string; fileId: string } | null = null;
    const tooMany = (): PluginError =>
      remote("version listing", `${name} has more than ${MAX_VERSIONS_WALKED} versions`);
    for (let page = 0; page < MAX_VERSION_PAGES; page++) {
      const body: Record<string, unknown> = {
        bucketId: session.bucketId,
        prefix: name,
        maxFileCount: VERSIONS_PAGE,
        startFileName: start === null ? name : start.name,
      };
      if (start !== null) body["startFileId"] = start.fileId;
      const listed = await this.#call("b2_list_file_versions", body, parseVersionsPage);
      let past = false;
      for (const file of listed.files) {
        if (file.fileName === name) versions.push(file);
        else past = true;
      }
      // A page may hold fewer rows than asked for, so the cap is checked on
      // every page, not only when a continuation follows.
      if (versions.length > MAX_VERSIONS_WALKED) throw tooMany();
      if (past) return versions;
      const next = listed.next;
      if (next === null || next.name !== name) return versions;
      if (versions.length >= MAX_VERSIONS_WALKED) throw tooMany();
      if (start !== null && start.name === next.name && start.fileId === next.fileId) {
        throw remote("version listing", "the listing did not move");
      }
      start = next;
    }
    throw remote("version listing", "the listing did not end");
  }

  /**
   * Whether the version `ours` of `name` may stand under `condition`,
   * judged by the version listed just before it (the one it replaced).
   *
   * Every writer reads the same listing order, so of two writes racing
   * under one condition at most the one listed first can see the state it
   * expected; a later one finds the earlier write in its place and loses.
   * A losing write still being withdrawn counts as present, so a race can
   * also make both lose, never both win.
   *
   * @param name - The stored name.
   * @param ours - Our version's id.
   * @param condition - The condition.
   * @returns Whether the write stands.
   */
  async #writeStands(name: string, ours: string, condition: WriteCondition): Promise<boolean> {
    const versions = await this.#versionsOf(name);
    const at = versions.findIndex((version) => version.fileId === ours);
    if (at < 0) throw remote("conditional write", "the write is not in the version listing");
    // An unfinished large file is not readable, so it replaced nothing.
    const replaced = versions.slice(at + 1).find((version) => version.action !== "start");
    switch (condition.kind) {
      case "ifNoneMatch":
        return replaced === undefined || !isStored(replaced.action);
      case "ifMatch":
        return (
          replaced !== undefined && isStored(replaced.action) && replaced.fileId === condition.etag
        );
    }
  }

  /**
   * Deletes one version of `name`; a version already gone is not an error.
   *
   * @param name - The stored name.
   * @param fileId - The version.
   * @returns Resolves once gone.
   */
  async #deleteVersion(name: string, fileId: string): Promise<void> {
    try {
      await this.#call("b2_delete_file_version", { fileName: name, fileId }, () => undefined);
    } catch (err) {
      if (!hasCode(err, "not_found")) throw err;
    }
  }

  // -------------------------------------------------------------------------
  // ObjectStore
  // -------------------------------------------------------------------------

  /**
   * The object at `key`, or `null`.
   *
   * @param key - The host key.
   * @returns The object.
   */
  async head(key: string): Promise<StoredObject | null> {
    const version = await this.#stat(key);
    return version === null ? null : storedObject(key, version);
  }

  /**
   * One page of objects under `prefix` (`b2_list_file_names`). The cursor
   * is the next stored name, checked to belong to this listing.
   *
   * @param prefix - Key prefix.
   * @param cursor - The previous page's cursor.
   * @param limit - Most objects.
   * @returns The page.
   */
  async list(prefix: string, cursor: string | undefined, limit: number): Promise<ObjectPage> {
    const full = `${this.prefix}${prefix}`;
    let start = full;
    if (cursor !== undefined) {
      if (!cursor.startsWith(full) || cursor === full || fileNameProblem(cursor) !== null) {
        throw fail("invalid_cursor", "B2 listing cursor is not from this listing");
      }
      start = cursor;
    }
    const session = await this.#currentSession();
    const body: Record<string, unknown> = { bucketId: session.bucketId, maxFileCount: limit };
    if (full !== "") body["prefix"] = full;
    if (start !== "") body["startFileName"] = start;
    const page = await this.#call("b2_list_file_names", body, parseNamesPage);
    const objects: ObjectInfo[] = [];
    for (const file of page.files) {
      if (!file.fileName.startsWith(full) || compareNames(file.fileName, start) < 0) {
        throw remote("listing", "the reply lists a name outside the page");
      }
      if (!isStored(file.action) || !file.fileName.startsWith(this.prefix)) continue;
      const info: ObjectInfo = {
        key: file.fileName.slice(this.prefix.length),
        size: file.contentLength,
      };
      if (file.fileId !== null) {
        info.etag = file.fileId;
        info.versionId = file.fileId;
        info.signature = file.fileId;
      }
      if (file.uploadTimestampMs > 0) info.modifiedAtMs = file.uploadTimestampMs;
      objects.push(info);
    }
    const out: ObjectPage = { objects };
    const next = page.nextFileName;
    if (next !== null && next.startsWith(full)) {
      if (compareNames(next, start) <= 0) {
        throw remote("listing", "the continuation does not advance");
      }
      out.nextCursor = next;
    }
    return out;
  }

  /**
   * Streams the object at `key`.
   *
   * @param key - The host key.
   * @param range - A checked byte range, or `undefined` for the whole object.
   * @returns The object and its body.
   */
  async get(
    key: string,
    range: ByteRange | undefined,
  ): Promise<{ object: StoredObject; body: ReadableStream<Uint8Array> }> {
    const name = this.#nameFor(key);
    const header =
      range === undefined
        ? undefined
        : range.length === undefined
          ? `bytes=${range.offset}-`
          : `bytes=${range.offset}-${range.offset + range.length - 1}`;
    const found = await this.#fetch("GET", name, header);
    switch (found.kind) {
      case "missing":
        throw fail("not_found", key);
      case "unsatisfiable": {
        // A read that starts exactly at the end is empty, as on the other
        // backends; anything past the end is refused.
        const version = await this.#stat(key);
        if (version === null) throw fail("not_found", key);
        if (
          range !== undefined &&
          range.length === undefined &&
          range.offset === version.contentLength
        ) {
          return { object: storedObject(key, version), body: new Blob([]).stream() };
        }
        if (range !== undefined && range.offset > version.contentLength) {
          throw fail(
            "invalid_params",
            `byte range offset ${range.offset} is past object size ${version.contentLength}`,
          );
        }
        throw remote("download", "the file changed while it was read");
      }
      case "found": {
        const { head, reply } = found;
        const size = head.version.contentLength;
        let offset = 0;
        let expected = size;
        if (range !== undefined) {
          if (
            range.offset > size ||
            (range.length !== undefined && range.offset + range.length > size)
          ) {
            if (reply.body !== null) await discard(reply.body);
            throw fail("invalid_params", `byte range extends past object size ${size}`);
          }
          offset = range.offset;
          expected = range.length ?? size - range.offset;
        }
        if (head.bodyOffset !== offset || head.bodyLength !== expected) {
          if (reply.body !== null) await discard(reply.body);
          throw remote("download", "the reply is not the range asked for");
        }
        return { object: storedObject(key, head.version), body: sizedBody(reply.body, expected) };
      }
    }
  }

  /**
   * Stores `body` at `key`.
   *
   * @param key - The host key.
   * @param body - The bytes.
   * @param write - Type, length, checksum and commit token.
   * @returns What was stored.
   */
  async put(
    key: string,
    body: ReadableStream<Uint8Array>,
    write: ObjectWrite,
  ): Promise<WrittenObject> {
    return (await this.#write(key, body, write)).written;
  }

  /**
   * Writes, then reads the name's versions and withdraws the write when the
   * version it replaced is not the one the condition expects. Until then a
   * reader may see the write, so the result is never atomic.
   *
   * @param key - The host key.
   * @param body - The bytes.
   * @param write - Type, length, checksum and commit token.
   * @param condition - The precondition.
   * @returns What was stored; `conflict` when the condition failed.
   */
  async putConditional(
    key: string,
    body: ReadableStream<Uint8Array>,
    write: ObjectWrite,
    condition: WriteCondition,
  ): Promise<WrittenObject> {
    const name = this.#nameFor(key);
    const { written, version } = await this.#write(key, body, write);
    const ours = version.fileId;
    if (ours === null) throw remote("conditional write", "the stored file has no id");
    let stands: boolean;
    try {
      stands = await this.#writeStands(name, ours, condition);
    } catch (err) {
      // A check that could not finish: the write must not stay, since
      // nothing proves it may.
      await this.#deleteVersion(name, ours);
      throw err;
    }
    if (stands) return written;
    await this.#deleteVersion(name, ours);
    throw fail(
      "conflict",
      condition.kind === "ifNoneMatch"
        ? `${key} already exists`
        : `${key} changed since it was read`,
    );
  }

  /**
   * Server-side copy of `from` to `to`. `b2_copy_file` keeps the file info
   * (digest and commit token) and content type.
   *
   * @param from - Source key.
   * @param to - Target key.
   * @returns Bytes copied.
   */
  async copy(from: string, to: string): Promise<number> {
    const source = await this.#stat(from);
    if (source === null) throw fail("not_found", from);
    const sourceId = source.fileId;
    if (sourceId === null) throw remote("copy", "the source has no file id");
    const toName = this.#nameFor(to);
    if (source.contentLength <= (this.#runtime.copyFileMaxBytes ?? COPY_FILE_MAX_BYTES)) {
      const copied = await this.#call(
        "b2_copy_file",
        { sourceFileId: sourceId, fileName: toName },
        parseFileVersion,
      );
      if (copied.fileName !== toName || copied.contentLength !== source.contentLength) {
        throw remote("copy", "the copy does not match its source");
      }
      return copied.contentLength;
    }
    const session = await this.#currentSession();
    const fileId = await this.#startLargeFile(
      session,
      toName,
      contentTypeHeader(source.contentType),
      source.fileInfo,
    );
    try {
      const partBytes = this.#runtime.copyPartBytes ?? COPY_PART_BYTES;
      const sha1s: string[] = [];
      let offset = 0;
      for (let number = 1; offset < source.contentLength; number++) {
        const end = Math.min(offset + partBytes, source.contentLength) - 1;
        const part = await this.#call(
          "b2_copy_part",
          {
            sourceFileId: sourceId,
            largeFileId: fileId,
            partNumber: number,
            range: `bytes=${offset}-${end}`,
          },
          parsePart,
        );
        if (part.partNumber !== number) throw remote("copy part", "the reply names another part");
        sha1s.push(part.contentSha1);
        offset = end + 1;
      }
      const version = await this.#finishLargeFile(fileId, sha1s);
      if (version.contentLength !== source.contentLength) {
        throw remote("copy", "the copy does not match its source");
      }
      return version.contentLength;
    } catch (err) {
      await this.#cancelLargeFile(fileId);
      throw err;
    }
  }

  /**
   * Removes every version of `key`, oldest first, so the current version
   * goes last and a delete that fails part way never uncovers an older one.
   *
   * @param key - The host key.
   * @returns Resolves once gone.
   */
  async delete(key: string): Promise<void> {
    const name = this.#nameFor(key);
    const versions = await this.#versionsOf(name);
    for (const version of versions.reverse()) {
      const fileId = version.fileId;
      if (fileId === null) continue;
      try {
        if (version.action === "start") {
          await this.#call("b2_cancel_large_file", { fileId }, () => undefined);
        } else {
          await this.#call("b2_delete_file_version", { fileName: name, fileId }, () => undefined);
        }
      } catch (err) {
        if (!hasCode(err, "not_found")) throw err;
      }
    }
  }

  // -------------------------------------------------------------------------
  // Event notification rules
  // -------------------------------------------------------------------------

  /**
   * The bucket's event notification rules (the key needs
   * `readBucketNotifications`).
   *
   * @returns The rules.
   */
  async notificationRules(): Promise<NotificationRule[]> {
    const { bucketId } = await this.#currentSession();
    return this.#call("b2_get_bucket_notification_rules", { bucketId }, parseNotificationRules, {
      path: API_V4_PATH,
      get: true,
    });
  }

  /**
   * Replaces the bucket's rules (the key needs `writeBucketNotifications`).
   *
   * @param rules - The full rule list.
   * @returns The rules B2 kept.
   */
  async #setNotificationRules(rules: Record<string, unknown>[]): Promise<NotificationRule[]> {
    const { bucketId } = await this.#currentSession();
    return this.#call(
      "b2_set_bucket_notification_rules",
      { bucketId, eventNotificationRules: rules },
      parseNotificationRules,
      { path: API_V4_PATH },
    );
  }

  /**
   * Makes sure the destination's rules for its prefix post to `url`, signed
   * with `secret`, keeping every other rule of the bucket. Setting rules
   * replaces them all, so the others are read first and sent back
   * unchanged. Out-of-date rules of ours are removed in one call and added
   * again in the next: a suspended rule can only be deleted, and an edited
   * rule may keep its old signing secret.
   *
   * @param url - The webhook URL.
   * @param secret - The signing secret.
   * @returns `true` when the rules were (re)written, `false` when they were
   *   already in place.
   */
  async keepNotificationRules(url: string, secret: B2SigningSecret): Promise<boolean> {
    let existing = await this.notificationRules();
    if (await ownRulesInPlace(existing, this.prefix, url, secret)) return false;
    const names = await ruleNames(this.prefix);
    if (existing.some((rule) => names.includes(rule.name))) {
      existing = await this.#setNotificationRules(await rulesWith(existing, this.prefix, []));
    }
    const own = await ownRules(this.prefix, url, secret);
    const kept = await this.#setNotificationRules(await rulesWith(existing, this.prefix, own));
    if (await ownRulesInPlace(kept, this.prefix, url, secret)) return true;
    throw remote("notification rules", "B2 did not keep the rules as written");
  }

  /**
   * Removes the destination's rules for its prefix, keeping the others.
   *
   * @returns `true` when rules were removed, `false` when there were none.
   */
  async dropNotificationRules(): Promise<boolean> {
    const existing = await this.notificationRules();
    const names = await ruleNames(this.prefix);
    if (!existing.some((rule) => names.includes(rule.name))) return false;
    await this.#setNotificationRules(await rulesWith(existing, this.prefix, []));
    return true;
  }
}
