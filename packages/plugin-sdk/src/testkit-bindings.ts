/**
 * In-memory `KV` and `WEBHOOKS` bindings for plugin tests.
 *
 * They hold the limits and error codes the host applies, so code that
 * passes against them does not first meet a cap on a deployed host: keys
 * are 1 to `MAX_KV_KEY_BYTES` UTF-8 bytes, values at most the namespace's
 * `max_value_bytes`, metadata at most `MAX_KV_METADATA_BYTES` of JSON, and a
 * webhook pull leases 1 to `MAX_WEBHOOK_PULL_BATCH` requests for at most
 * `MAX_WEBHOOK_VISIBILITY_MS`.
 *
 * ```ts
 * import { createTestKv, createTestWebhooks } from "@bookclerk/plugin-sdk/testkit";
 *
 * const env = { KV: createTestKv(), WEBHOOKS: createTestWebhooks() };
 * env.WEBHOOKS.deliver("changes", JSON.stringify({ events: [] }));
 * ```
 *
 * @module
 */

import type {
  KvListOptionsBinding,
  KvListResultBinding,
  KvNamespaceBinding,
  KvPutOptionsBinding,
  KvValueWithMetadata,
  WebhookMessage,
  WebhookPullOptions,
  WebhookPullResult,
  WebhooksBinding,
} from "./env.js";
import { PluginError } from "./errors.js";
import {
  MAX_KV_KEY_BYTES,
  MAX_KV_METADATA_BYTES,
  MAX_KV_VALUE_BYTES,
  MAX_LIST_PAGE,
  MAX_WEBHOOK_PULL_BATCH,
  MAX_WEBHOOK_VISIBILITY_MS,
} from "./abi.js";

/** Default webhook pull batch, as on the host. */
const DEFAULT_PULL_BATCH = 10;

/** One stored KV entry. */
interface KvEntry {
  /** The value bytes. */
  value: Uint8Array;
  /** Metadata as written, or `null`. */
  metadata: unknown;
  /** Expiry in milliseconds since the Unix epoch, or `undefined` for none. */
  expiresAtMs?: number;
}

/** Options for {@link createTestKv}. */
export interface TestKvOptions {
  /** The namespace's `max_value_bytes` (default and cap `MAX_KV_VALUE_BYTES`). */
  maxValueBytes?: number;
  /** The namespace's key-count limit; a new key past it is `forbidden`. */
  maxKeys?: number;
  /** Clock in milliseconds since the Unix epoch (default `Date.now`). */
  now?: () => number;
}

/** Operations a test can make fail once with {@link TestKv.failNext}. */
export type TestKvOperation = "get" | "put" | "delete" | "list";

/** A {@link KvNamespaceBinding} held in memory, with test controls. */
export interface TestKv extends KvNamespaceBinding {
  /**
   * Make the next call of `op` reject with `error` instead of running.
   *
   * @param op - The operation.
   * @param error - What it rejects with.
   */
  failNext(op: TestKvOperation, error: PluginError): void;
  /**
   * Live keys and their values as text, for assertions.
   *
   * @returns Key to UTF-8 value, in key order.
   */
  snapshot(): Map<string, string>;
}

/**
 * Checks a key the way the host does.
 *
 * @param key - The key.
 * @throws {PluginError} `invalid_params` when empty, `payload_too_large`
 *   when longer than `MAX_KV_KEY_BYTES`.
 */
function checkKey(key: string): void {
  if (typeof key !== "string" || key.length === 0) {
    throw PluginError.fromWire("invalid_params", "kv key must not be empty");
  }
  const bytes = new TextEncoder().encode(key).length;
  if (bytes > MAX_KV_KEY_BYTES) {
    throw PluginError.fromWire(
      "payload_too_large",
      `kv key of ${bytes} bytes exceeds ${MAX_KV_KEY_BYTES}`,
    );
  }
}

/**
 * Encodes a put value as bytes.
 *
 * @param value - Text or bytes.
 * @returns The bytes.
 */
function valueBytes(value: string | ArrayBuffer | ArrayBufferView): Uint8Array {
  if (typeof value === "string") return new TextEncoder().encode(value);
  if (value instanceof ArrayBuffer) return new Uint8Array(value.slice(0));
  return new Uint8Array(value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength));
}

/**
 * Decodes stored bytes as the caller asked.
 *
 * @param bytes - The stored value.
 * @param type - `text`, `json` or `arrayBuffer`.
 * @returns The decoded value.
 */
function decode(bytes: Uint8Array, type: string): unknown {
  if (type === "arrayBuffer") return bytes.slice().buffer;
  const text = new TextDecoder().decode(bytes);
  return type === "json" ? (JSON.parse(text) as unknown) : text;
}

/**
 * Creates an in-memory KV namespace that applies the host's limits.
 *
 * @param options - Namespace limits and clock.
 * @returns The binding.
 */
export function createTestKv(options: TestKvOptions = {}): TestKv {
  const maxValueBytes = Math.min(options.maxValueBytes ?? MAX_KV_VALUE_BYTES, MAX_KV_VALUE_BYTES);
  const now = options.now ?? Date.now;
  const entries = new Map<string, KvEntry>();
  const failures = new Map<TestKvOperation, PluginError>();

  const failure = (op: TestKvOperation): void => {
    const err = failures.get(op);
    if (err !== undefined) {
      failures.delete(op);
      throw err;
    }
  };
  const live = (key: string): KvEntry | undefined => {
    const entry = entries.get(key);
    if (entry?.expiresAtMs !== undefined && entry.expiresAtMs <= now()) {
      entries.delete(key);
      return undefined;
    }
    return entry;
  };
  const read = async (
    key: string,
    type: string = "text",
  ): Promise<KvValueWithMetadata<unknown>> => {
    failure("get");
    checkKey(key);
    const entry = live(key);
    if (entry === undefined) return { value: null, metadata: null };
    return { value: decode(entry.value, type), metadata: entry.metadata };
  };

  const kv = {
    async get(key: string, type?: string): Promise<unknown> {
      return (await read(key, type)).value;
    },
    async getWithMetadata(key: string, type?: string): Promise<KvValueWithMetadata<unknown>> {
      return read(key, type);
    },
    async put(
      key: string,
      value: string | ArrayBuffer | ArrayBufferView,
      putOptions: KvPutOptionsBinding = {},
    ): Promise<void> {
      failure("put");
      checkKey(key);
      const bytes = valueBytes(value);
      if (bytes.length > maxValueBytes) {
        throw PluginError.fromWire(
          "payload_too_large",
          `kv value of ${bytes.length} bytes exceeds ${maxValueBytes}`,
        );
      }
      const metadata = putOptions.metadata ?? null;
      if (metadata !== null) {
        const encoded = new TextEncoder().encode(JSON.stringify(metadata)).length;
        if (encoded > MAX_KV_METADATA_BYTES) {
          throw PluginError.fromWire(
            "payload_too_large",
            `kv metadata of ${encoded} bytes exceeds ${MAX_KV_METADATA_BYTES}`,
          );
        }
      }
      let expiresAtMs: number | undefined;
      if (putOptions.expiration !== undefined) {
        if (!Number.isSafeInteger(putOptions.expiration) || putOptions.expiration < 0) {
          throw PluginError.fromWire("invalid_params", "expiration is out of range");
        }
        expiresAtMs = putOptions.expiration * 1000;
      } else if (putOptions.expirationTtl !== undefined) {
        if (!Number.isSafeInteger(putOptions.expirationTtl) || putOptions.expirationTtl < 0) {
          throw PluginError.fromWire("invalid_params", "expirationTtl is out of range");
        }
        expiresAtMs = now() + putOptions.expirationTtl * 1000;
      }
      if (live(key) === undefined && options.maxKeys !== undefined) {
        const count = [...entries.keys()].filter((name) => live(name) !== undefined).length;
        if (count >= options.maxKeys) {
          throw PluginError.fromWire(
            "forbidden",
            `kv namespace holds at most ${options.maxKeys} keys`,
          );
        }
      }
      const entry: KvEntry = { value: bytes, metadata };
      if (expiresAtMs !== undefined) entry.expiresAtMs = expiresAtMs;
      entries.set(key, entry);
    },
    async delete(key: string): Promise<void> {
      failure("delete");
      checkKey(key);
      entries.delete(key);
    },
    async list(listOptions: KvListOptionsBinding = {}): Promise<KvListResultBinding> {
      failure("list");
      const limit = listOptions.limit ?? MAX_LIST_PAGE;
      if (!Number.isSafeInteger(limit) || limit < 0 || limit > MAX_LIST_PAGE) {
        throw PluginError.fromWire(
          "invalid_params",
          `kv list limit must be at most ${MAX_LIST_PAGE}`,
        );
      }
      const prefix = listOptions.prefix ?? "";
      const after = listOptions.cursor ?? "";
      const names = [...entries.keys()]
        .filter((name) => name.startsWith(prefix) && name > after && live(name) !== undefined)
        .sort();
      const page = names.slice(0, limit === 0 ? MAX_LIST_PAGE : limit);
      const keys = page.map((name) => {
        const entry = entries.get(name) as KvEntry;
        const key: KvListResultBinding["keys"][number] = { name };
        if (entry.expiresAtMs !== undefined) key.expiration = Math.floor(entry.expiresAtMs / 1000);
        if (entry.metadata !== null) key.metadata = entry.metadata;
        return key;
      });
      const result: KvListResultBinding = { keys, list_complete: page.length === names.length };
      if (!result.list_complete) result.cursor = page[page.length - 1];
      return result;
    },
    failNext(op: TestKvOperation, error: PluginError): void {
      failures.set(op, error);
    },
    snapshot(): Map<string, string> {
      const out = new Map<string, string>();
      for (const name of [...entries.keys()].sort()) {
        const entry = live(name);
        if (entry !== undefined) out.set(name, new TextDecoder().decode(entry.value));
      }
      return out;
    },
  };
  return kv as TestKv;
}

/** One waiting or leased webhook request. */
interface TestRequest {
  /** Request headers. */
  headers: Record<string, string>;
  /** Request body. */
  body: Uint8Array;
  /** When the request arrived. */
  timestamp: Date;
  /** How many pulls have leased it. */
  attempts: number;
}

/** A {@link WebhooksBinding} held in memory, with test controls. */
export interface TestWebhooks extends WebhooksBinding {
  /**
   * Queue one inbound request, as if the outside service sent it.
   *
   * @param name - `[[webhooks]]` name.
   * @param body - Body text or bytes.
   * @param headers - Request headers.
   */
  deliver(name: string, body: string | Uint8Array, headers?: Record<string, string>): void;
  /**
   * Make the next pull of `name` report `missed`, as when requests expired.
   *
   * @param name - `[[webhooks]]` name.
   */
  markMissed(name: string): void;
  /**
   * Requests waiting (not leased) for `name`.
   *
   * @param name - `[[webhooks]]` name.
   * @returns The count.
   */
  waiting(name: string): number;
  /**
   * Lease ids acknowledged for `name`, in order.
   *
   * @param name - `[[webhooks]]` name.
   * @returns The ids.
   */
  acked(name: string): string[];
  /**
   * Lease ids released for retry for `name`, in order.
   *
   * @param name - `[[webhooks]]` name.
   * @returns The ids.
   */
  retried(name: string): string[];
}

/** Options for {@link createTestWebhooks}. */
export interface TestWebhooksOptions {
  /**
   * What `url(name)` returns: a base the name is appended to (default
   * `https://library.example/hooks/`), or `null` for a library the Internet
   * cannot reach.
   */
  baseUrl?: string | null;
}

/**
 * Creates an in-memory `WEBHOOKS` binding that applies the host's limits.
 *
 * Leased requests stay leased until acknowledged or retried; lease expiry
 * is not modelled, so release a lease with `ack(name, { retries })`.
 *
 * @param options - The URL base.
 * @returns The binding.
 */
export function createTestWebhooks(options: TestWebhooksOptions = {}): TestWebhooks {
  const base = options.baseUrl === undefined ? "https://library.example/hooks/" : options.baseUrl;
  const queues = new Map<string, TestRequest[]>();
  const leases = new Map<string, { name: string; request: TestRequest }>();
  const acks = new Map<string, string[]>();
  const retries = new Map<string, string[]>();
  const missed = new Set<string>();
  let nextLease = 0;
  const queue = (name: string): TestRequest[] => {
    let found = queues.get(name);
    if (found === undefined) {
      found = [];
      queues.set(name, found);
    }
    return found;
  };
  const log = (map: Map<string, string[]>, name: string): string[] => {
    let found = map.get(name);
    if (found === undefined) {
      found = [];
      map.set(name, found);
    }
    return found;
  };

  return {
    async url(name: string): Promise<string | null> {
      return base === null ? null : `${base}${encodeURIComponent(name)}`;
    },
    async pull(name: string, pullOptions: WebhookPullOptions = {}): Promise<WebhookPullResult> {
      const size = pullOptions.batchSize ?? DEFAULT_PULL_BATCH;
      if (!Number.isSafeInteger(size) || size < 1 || size > MAX_WEBHOOK_PULL_BATCH) {
        throw PluginError.fromWire(
          "invalid_params",
          `batchSize must be 1 to ${MAX_WEBHOOK_PULL_BATCH}`,
        );
      }
      const visibility = pullOptions.visibilityTimeoutMs ?? 0;
      if (
        !Number.isSafeInteger(visibility) ||
        visibility < 0 ||
        visibility > MAX_WEBHOOK_VISIBILITY_MS
      ) {
        throw PluginError.fromWire(
          "invalid_params",
          `visibilityTimeoutMs must be 0 to ${MAX_WEBHOOK_VISIBILITY_MS}`,
        );
      }
      const messages: WebhookMessage[] = queue(name)
        .splice(0, size)
        .map((request) => {
          nextLease += 1;
          const leaseId = `lease-${nextLease}`;
          request.attempts += 1;
          leases.set(leaseId, { name, request });
          return {
            leaseId,
            timestamp: request.timestamp,
            attempts: request.attempts,
            headers: { ...request.headers },
            body: request.body.slice(),
          };
        });
      const wasMissed = missed.delete(name);
      return { messages, missed: wasMissed };
    },
    async ack(name: string, settled: { acks?: string[]; retries?: string[] }): Promise<void> {
      for (const id of settled.acks ?? []) {
        if (leases.get(id)?.name === name) {
          leases.delete(id);
          log(acks, name).push(id);
        }
      }
      const back: TestRequest[] = [];
      for (const id of settled.retries ?? []) {
        const lease = leases.get(id);
        if (lease?.name === name) {
          leases.delete(id);
          back.push(lease.request);
          log(retries, name).push(id);
        }
      }
      queue(name).unshift(...back);
    },
    deliver(name: string, body: string | Uint8Array, headers: Record<string, string> = {}): void {
      const bytes = typeof body === "string" ? new TextEncoder().encode(body) : body.slice();
      queue(name).push({
        headers: { ...headers },
        body: bytes,
        timestamp: new Date(),
        attempts: 0,
      });
    },
    markMissed(name: string): void {
      missed.add(name);
    },
    waiting(name: string): number {
      return queue(name).length;
    },
    acked(name: string): string[] {
      return [...log(acks, name)];
    },
    retried(name: string): string[] {
      return [...log(retries, name)];
    },
  };
}
