/**
 * Workerd runtime binding surfaces available on the guest `env`.
 *
 * These are runtime capability shapes (they carry functions), not wire DTOs,
 * so they live here rather than in the generated `generated.ts` projection.
 * `bookclerk-plugin types` writes a precise `Env` interface for one
 * `plugin.toml` in terms of these types.
 */

import type { DatabaseBinding } from "./db-execute.js";
import type { PluginEvent, PublishOk, QueueSendOk } from "./generated.js";

/** Plain JSON object (string keys, JSON values). */
export type JsonObject = Record<string, unknown>;

/** Event a guest publishes; the host stamps id, `source`, and `accountId`. */
export type PublishEvent = Pick<PluginEvent, "eventType"> &
  Partial<Omit<PluginEvent, "eventType" | "payload">> & {
    /** Payload bytes or a JSON-serializable value encoded as UTF-8 JSON. */
    payload?: Uint8Array | JsonObject | unknown[] | string;
  };

/**
 * `EVENTS` binding: outbox publisher, present when `[[events.producers]]` is
 * declared and granted.
 */
export interface EventPublisherBinding {
  /**
   * Publish one domain event through the host outbox.
   *
   * @param event - Event type, schema version, payload, dedup key.
   * @returns Outbox row identity (`duplicate` when the dedup key matched).
   */
  publish(event: PublishEvent): Promise<PublishOk>;
}

/** Per-send options for a {@link QueueBinding}. */
export interface QueueSendOptions {
  /**
   * Body encoding: `json` (default; the body is JSON-encoded), `text`
   * (UTF-8 string), or `bytes` (raw `Uint8Array`).
   */
  contentType?: "json" | "text" | "bytes";
  /** Seconds before the message may be delivered; `0` means now. */
  delaySeconds?: number;
}

/** One entry of {@link QueueBinding.sendBatch}. */
export interface QueueSendBatchEntry extends QueueSendOptions {
  /** Message body (JSON value, string, or bytes per `contentType`). */
  body: unknown;
}

/**
 * `[[queues.producers]]` binding (`env.<BINDING>`): Workers `Queue`-shaped
 * producer into the queue the row names. The host assigns ids, stamps the
 * sending plugin as `source`, and bounds each body at `maxEventPayloadBytes`.
 */
export interface QueueBinding {
  /**
   * Send one message.
   *
   * @param body - JSON value (default), string, or bytes per `contentType`.
   * @param options - Encoding and delay.
   * @returns The outbox id the host assigned.
   */
  send(body: unknown, options?: QueueSendOptions): Promise<QueueSendOk>;
  /**
   * Send up to `maxListPage` messages in order.
   *
   * @param messages - Bodies with per-message options.
   * @param options - Default delay for entries that carry none.
   * @returns The outbox ids in send order.
   */
  sendBatch(
    messages: ReadonlyArray<QueueSendBatchEntry>,
    options?: Pick<QueueSendOptions, "delaySeconds">,
  ): Promise<QueueSendOk>;
}

/** How {@link KvNamespaceBinding.get} decodes a stored value. */
export type KvValueType = "text" | "json" | "arrayBuffer";

/** Options for {@link KvNamespaceBinding.put} (Workers KV names). */
export interface KvPutOptionsBinding {
  /** Absolute expiry in seconds since the Unix epoch. */
  expiration?: number;
  /** Relative expiry in seconds from the write; ignored when `expiration` is set. */
  expirationTtl?: number;
  /** JSON-serializable metadata, at most `maxKvMetadataBytes` once encoded. */
  metadata?: unknown;
}

/** Page request for {@link KvNamespaceBinding.list}. */
export interface KvListOptionsBinding {
  /** Only keys starting with this prefix. */
  prefix?: string;
  /** Cursor from a previous page. */
  cursor?: string;
  /** Most keys to return (host default when omitted, at most `maxListPage`). */
  limit?: number;
}

/** One key on a {@link KvNamespaceBinding.list} page. */
export interface KvListKeyBinding {
  /** Key name. */
  name: string;
  /** Absolute expiry in seconds since the Unix epoch, when set. */
  expiration?: number;
  /** Stored metadata, when set. */
  metadata?: unknown;
}

/** One page of {@link KvNamespaceBinding.list}. */
export interface KvListResultBinding {
  /** Keys in lexical order. */
  keys: KvListKeyBinding[];
  /** True when no further page exists. */
  list_complete: boolean;
  /** Cursor for the next page; absent when `list_complete`. */
  cursor?: string;
}

/** Value plus metadata from {@link KvNamespaceBinding.getWithMetadata}. */
export interface KvValueWithMetadata<T> {
  /** Decoded value, or `null` when the key is absent or expired. */
  value: T | null;
  /** Stored metadata, or `null` when none. */
  metadata: unknown;
}

/**
 * `[[kv_namespaces]]` binding (`env.<BINDING>`, default `KV`): Workers
 * `KVNamespace`-shaped durable storage the host serves from the library
 * database, scoped to this plugin and the namespace the row names. Values
 * follow the instance across hosts.
 */
export interface KvNamespaceBinding {
  /**
   * Read one key.
   *
   * @param key - Key, at most `maxKvKeyBytes` UTF-8 bytes.
   * @param type - Decoding (`text` by default).
   * @returns The value, or `null` when absent or expired.
   */
  get(key: string, type?: "text"): Promise<string | null>;
  /**
   * Read one key and parse it as JSON.
   *
   * @param key - Key, at most `maxKvKeyBytes` UTF-8 bytes.
   * @param type - `json`.
   * @returns The parsed value, or `null` when absent or expired.
   */
  get<T = unknown>(key: string, type: "json"): Promise<T | null>;
  /**
   * Read one key as bytes.
   *
   * @param key - Key, at most `maxKvKeyBytes` UTF-8 bytes.
   * @param type - `arrayBuffer`.
   * @returns The bytes, or `null` when absent or expired.
   */
  get(key: string, type: "arrayBuffer"): Promise<ArrayBuffer | null>;
  /**
   * Read one key with its metadata.
   *
   * @param key - Key to read.
   * @param type - Decoding (`text` by default).
   * @returns Value and metadata; both `null` when absent.
   */
  getWithMetadata(key: string, type?: "text"): Promise<KvValueWithMetadata<string>>;
  /**
   * Read one key as JSON with its metadata.
   *
   * @param key - Key to read.
   * @param type - `json`.
   * @returns Parsed value and metadata; both `null` when absent.
   */
  getWithMetadata<T = unknown>(key: string, type: "json"): Promise<KvValueWithMetadata<T>>;
  /**
   * Read one key as bytes with its metadata.
   *
   * @param key - Key to read.
   * @param type - `arrayBuffer`.
   * @returns Bytes and metadata; both `null` when absent.
   */
  getWithMetadata(key: string, type: "arrayBuffer"): Promise<KvValueWithMetadata<ArrayBuffer>>;
  /**
   * Write one key, replacing any previous value and metadata.
   *
   * @param key - Key to write.
   * @param value - String (stored as UTF-8) or bytes.
   * @param options - Expiration and metadata.
   */
  put(
    key: string,
    value: string | ArrayBuffer | ArrayBufferView,
    options?: KvPutOptionsBinding,
  ): Promise<void>;
  /**
   * Delete one key; a missing key is not an error.
   *
   * @param key - Key to delete.
   */
  delete(key: string): Promise<void>;
  /**
   * List keys in lexical order, one page at a time.
   *
   * @param options - Prefix, cursor, and page size.
   * @returns One page of keys.
   */
  list(options?: KvListOptionsBinding): Promise<KvListResultBinding>;
}

/**
 * `WORK_FS` binding: host-granted object storage for durable plugin files.
 * Same surface as the `storage` entrypoint the host calls on storage plugins.
 */
export interface StorageBinding {
  /**
   * Object metadata without the body.
   *
   * @param key - Object key.
   * @returns Metadata, or `null` when the key does not exist.
   */
  head(
    key: string,
  ): Promise<{ key: string; size: number; contentType?: string; etag?: string } | null>;
  /**
   * One page of keys under `prefix`.
   *
   * @param options - Prefix, continuation cursor, and page size.
   * @returns Objects plus the cursor for the next page, when any.
   */
  list(options: { prefix?: string; cursor?: string; limit?: number }): Promise<{
    objects: Array<{ key: string; size: number }>;
    nextCursor?: string;
  }>;
  /**
   * Stream an object (optionally a byte range).
   *
   * @param key - Object key.
   * @param options - Optional byte range.
   * @returns Metadata and the body stream.
   */
  get(
    key: string,
    options?: { range?: { offset: number; length?: number } },
  ): Promise<{ meta: { key: string; size: number }; body: ReadableStream<Uint8Array> }>;
  /**
   * Write an object from a stream.
   *
   * @param key - Object key.
   * @param body - Body bytes.
   * @param options - Content type and length hints.
   * @returns Key and bytes written.
   */
  put(
    key: string,
    body: ReadableStream<Uint8Array>,
    options?: { contentType?: string; contentLength?: number },
  ): Promise<{ key: string; bytesWritten: number }>;
  /**
   * Remove an object; missing keys are not an error.
   *
   * @param key - Object key.
   * @returns Resolves when the delete is durable.
   */
  delete(key: string): Promise<void>;
}

/**
 * Guest `env` bindings for `api_version = 3` manifests. Every host-provided
 * facility is a binding here; there is no `HOST` object. Bindings appear only
 * when `plugin.toml` declares them and the operator has consented.
 *
 * Run `bookclerk-plugin types` to generate a precise `Env` for one manifest
 * (`bookclerk-configuration.d.ts`), then `extends BookclerkEntrypoint<Env>`.
 */
export interface BookclerkEnv {
  /** `[vars]` plus operator settings, decoded from the granted `CONFIG` payload. */
  CONFIG?: JsonObject;
  /** `[secrets]` values, decoded from the granted `SECRETS` payload. */
  SECRETS?: JsonObject;
  /** `[[events.producers]]` outbox publisher. */
  EVENTS?: EventPublisherBinding;
  /** `[work_fs]` object storage. */
  WORK_FS?: StorageBinding;
  /** `[[kv_namespaces]]` durable KV binding (default name). */
  KV?: KvNamespaceBinding;
  /** `[oauth]` loopback helper; surface reserved. */
  OAUTH?: unknown;
  /**
   * Named `[[databases]]` bindings, `[[queues.producers]]` bindings, and any
   * renamed `binding = "..."` entries.
   */
  [binding: string]:
    DatabaseBinding | EventPublisherBinding | QueueBinding | StorageBinding | JsonObject | unknown;
}
