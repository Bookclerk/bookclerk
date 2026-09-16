/**
 * Workerd runtime binding surfaces available on the guest `env`.
 *
 * These are runtime capability shapes (they carry functions), not wire DTOs,
 * so they live here rather than in the generated `generated.ts` projection.
 * `bookclerk-plugin types` writes a precise `Env` interface for one
 * `plugin.toml` in terms of these types.
 */

import type { DatabaseBinding } from "./db-execute.js";
import type { PluginEvent, PublishOk } from "./generated.js";

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
  head(key: string): Promise<{ key: string; size: number; contentType?: string; etag?: string } | null>;
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
  /** `[[kv_namespaces]]` store (default binding name); surface reserved. */
  KV?: unknown;
  /** `[oauth]` loopback helper; surface reserved. */
  OAUTH?: unknown;
  /** Named `[[databases]]` bindings and any renamed `binding = "..."` entries. */
  [binding: string]: DatabaseBinding | EventPublisherBinding | StorageBinding | JsonObject | unknown;
}
