/**
 * TypeScript typed twin of the workerd runtime (`embed/bookclerk_plugin.js`)
 * for product `apiVersion` 3 — the Workers-idiom author model.
 *
 * - The default export extends {@link BookclerkEntrypoint}; triggers are
 *   handler methods on it: `event(batch)` for `[[events.consumers]]` and
 *   `job(job)` for `[triggers] jobs`.
 * - Capabilities the host calls are named exported classes (`Storefront`,
 *   `Storage`, `RemoteLibrary`, `DatabaseAdapter`, `Cli`, `Oidc`) extending
 *   the matching `*Entrypoint` base with typed methods.
 * - Everything the host provides is a binding on `env` ({@link BookclerkEnv}:
 *   `CONFIG`, `SECRETS`, `EVENTS`, `WORK_FS`, named databases, …);
 *   per-invocation facilities ride on the handler's controller object
 *   ({@link EventMessage}, {@link JobController}), never on `env`.
 *
 * Byte payloads move as `ReadableStream` — never as base64 scalars.
 *
 * `bookclerk-workerd` injects the JS runtime into the isolate under
 * `@bookclerk/plugin-sdk/workerd`; this module provides the same classes with
 * types for `tsc` and for the trusted adapter isolate.
 */

import "./cloudflare-workers.d.ts";
import { WorkerEntrypoint, RpcTarget } from "cloudflare:workers";
import { MAX_CHECKPOINT_BYTES } from "./abi.js";
import type { DatabaseBinding, ExecuteReply, ExecuteRequest } from "./db-execute.js";
import type { BookclerkEnv, EventPublisherBinding, JsonObject } from "./env.js";
import { requirePluginMigrationRegistration } from "./plugin-migrations.js";
import type {
  AuthenticateUserParams,
  CatalogDetailParams,
  CatalogHit,
  CliInvokeParams,
  CliInvokeResult,
  CliSchema,
  DomainEvent,
  ExpandCandidatesParams,
  ExtensibleConfig,
  ExternalUser,
  FetchTitleParams,
  HealthOk,
  Invocation,
  JobInvocation,
  ListDealsParams,
  ListeningProgress,
  LoginCompleteParams,
  LoginParams,
  LoginResult,
  LoginStartResult,
  OidcClientTemplate,
  PlainFetch,
  PluginDescribe as WirePluginDescribe,
  PluginMigration,
  PurchaseHint,
  PurchaseHintParams,
  ScanLibraryParams,
  ScanParams,
  ScanSummary,
  SearchCatalogParams,
  SourceAccount,
} from "./generated.js";

// Product constants come from the generated `abi.ts` projection of
// `schema/plugin.capnp` — re-exported here for guest convenience.
export {
  FEATURE_SCALAR_LIMITS,
  FEATURE_STORAGE_COPY,
  FEATURE_STREAMS,
  MAX_CHECKPOINT_BYTES,
  MAX_EVENT_PAYLOAD_BYTES,
  MAX_LIST_PAGE,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  PRODUCT_API_VERSION,
} from "./abi.js";
export { requirePluginMigrationRegistration } from "./plugin-migrations.js";

// Typed method payloads are the generated projections of
// `schema/plugin.capnp`; re-exported so guests can import them beside the
// entrypoint classes.
export type {
  CliInvokeParams,
  CliInvokeResult,
  CliSchema,
  DomainEvent,
  ExtensibleConfig,
  HealthOk,
  Invocation,
  JobInvocation,
  OidcClientTemplate,
  PluginCapabilities,
  PluginMigration,
  PluginMigrationOp,
  ScalarLimits,
} from "./generated.js";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

const KNOWN_ERROR_CODES = new Set([
  "invalid_params",
  "unauthorized",
  "forbidden",
  "not_found",
  "unavailable",
  "unsupported",
  "internal",
  "payload_too_large",
  "deadline_exceeded",
  "invalid_cursor",
  "cancelled",
  "conflict",
]);

/** Thrown by the SDK when a wire union carries `err`. Unknown codes are kept. */
export class PluginError extends Error {
  /** Known `PluginErrorCode` wire string, or `unknown`. */
  readonly code: string;
  /** Raw wire code, including codes this SDK does not know. */
  readonly wireCode: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "PluginError";
    this.wireCode = code;
    this.code = KNOWN_ERROR_CODES.has(code) ? code : "unknown";
  }

  /**
   * Construct a {@link PluginError} from a Cap'n Proto / JSON wire code.
   *
   * Unknown codes become `unknown` on {@link PluginError.code} while
   * {@link PluginError.wireCode} keeps the raw value.
   *
   * @param code - Wire error code (known or unknown).
   * @param message - Operator-facing error text.
   * @returns Typed plugin error.
   */
  static fromWire(code: string, message: string): PluginError {
    return new PluginError(code, message);
  }
}

function unsupported(method: string): PluginError {
  return PluginError.fromWire("unsupported", `${method} not implemented`);
}

function utf8Bytes(value: unknown): number {
  return new TextEncoder().encode(String(value ?? "")).byteLength;
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

// ---------------------------------------------------------------------------
// Describe
// ---------------------------------------------------------------------------

/**
 * Optional identity refinement returned by `BookclerkEntrypoint.describe()`.
 *
 * The launcher derives the full wire `PluginDescribe` from `plugin.toml`
 * (`apiVersion`, `id`, `capabilities`, `cli`, …); an author `describe()` may
 * add or override presentation fields such as `displayName`, `brand`,
 * `rpcFeatures`, or `configOptions`. Identity and capabilities always come
 * from the manifest.
 */
export type PluginDescribe = Partial<WirePluginDescribe>;

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/**
 * Schema DDL convenience for {@link PluginMigration} operations.
 *
 * @param sql - Already-separated BookclerkSQL schema statement.
 * @returns Schema operation tagged for host registration.
 */
export function schemaMigrationOp(sql: string): { schema: string } {
  return { schema: sql };
}

/**
 * Data DML convenience for {@link PluginMigration} operations.
 *
 * @param sql - Already-separated BookclerkSQL data statement.
 * @returns Data operation tagged for host registration.
 */
export function dataMigrationOp(sql: string): { data: string } {
  return { data: sql };
}

// ---------------------------------------------------------------------------
// Storage / stream shapes (author-ergonomic projections of the ABI structs)
// ---------------------------------------------------------------------------

/** Object listing entry. */
export interface ObjectInfo {
  key: string;
  size: number;
}

/** Metadata without a body. */
export interface ObjectMetadata {
  key: string;
  size: number;
  contentType?: string;
  etag?: string;
  sha256?: Uint8Array;
}

/** Paginated list request. */
export interface ListOptions {
  prefix?: string;
  cursor?: string;
  limit?: number;
}

/** One page of keys. */
export interface ListPage {
  objects: ObjectInfo[];
  nextCursor?: string;
}

/** Optional byte range for a streamed read. */
export interface ByteRange {
  offset: number;
  length?: number;
}

/** Read options for {@link StorageEntrypoint.get}. */
export interface ReadOptions {
  range?: ByteRange;
}

/** Write options for {@link StorageEntrypoint.put}. */
export interface WriteOptions {
  contentType?: string;
  contentLength?: number;
  sha256?: Uint8Array;
  commitToken?: string;
  stageOnly?: boolean;
}

/** Streamed read result. `body` is a transferred ReadableStream. */
export interface ReadResult {
  meta: ObjectMetadata;
  body: ReadableStream<Uint8Array>;
}

/** Result of a streamed put. */
export interface PutResult {
  key: string;
  bytesWritten: number;
  etag?: string;
  sha256?: Uint8Array;
}

/** Result of a server-side copy. */
export interface CopyResult {
  bytesCopied: number;
}

// ---------------------------------------------------------------------------
// Capability shapes (host-served objects a handler receives on its controller
// or as a binding). Byte payloads move as `ReadableStream`, never as scalars.
// ---------------------------------------------------------------------------

/**
 * Object store capability: `job.output` and the `WORK_FS` binding. The
 * runtime stub *is* the capability; abort is stream cancel.
 */
export class Destination extends RpcTarget {
  /**
   * Metadata without a body; `null` when the key is missing.
   *
   * @param _key - Object key.
   * @returns Metadata or `null` when the key is missing.
   */
  head(_key: string): Promise<ObjectMetadata | null> {
    return Promise.reject(unsupported("head"));
  }

  /**
   * One page of keys under `options.prefix`.
   *
   * @param _options - Prefix, cursor, and limit.
   * @returns One page of object keys.
   */
  list(_options: ListOptions): Promise<ListPage> {
    return Promise.reject(unsupported("list"));
  }

  /**
   * Streamed read. The body is a transferred stream, not a scalar.
   *
   * @param _key - Object key.
   * @param _options - Optional byte range.
   * @returns Metadata plus a transferred body stream.
   */
  get(_key: string, _options?: ReadOptions): Promise<ReadResult> {
    return Promise.reject(unsupported("get"));
  }

  /**
   * Streamed write. `body` ownership is transferred to the destination.
   *
   * @param _key - Object key.
   * @param _body - Byte stream.
   * @param _options - Optional content type / length.
   * @returns Bytes written and optional etag / sha256.
   */
  put(
    _key: string,
    _body: ReadableStream<Uint8Array>,
    _options?: WriteOptions,
  ): Promise<PutResult> {
    return Promise.reject(unsupported("put"));
  }

  /**
   * Server-side copy when the backend supports it.
   *
   * @param _from - Source key.
   * @param _to - Destination key.
   * @returns Bytes copied.
   */
  copy(_from: string, _to: string): Promise<CopyResult> {
    return Promise.reject(unsupported("copy"));
  }

  /**
   * Delete a key (no-op if missing).
   *
   * @param _key - Object key.
   * @returns Resolves when the delete is complete.
   */
  delete(_key: string): Promise<void> {
    return Promise.reject(unsupported("delete"));
  }

  /**
   * Finalize a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Idempotency / commit token.
   * @returns Published object metadata.
   */
  commit(_key: string, _commitToken: string): Promise<PutResult> {
    return Promise.reject(unsupported("commit"));
  }

  /**
   * Abort a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Staging token to discard.
   * @returns Rejects with typed `unsupported` unless overridden.
   */
  abortStage(_key: string, _commitToken: string): Promise<void> {
    return Promise.reject(unsupported("abortStage"));
  }
}

/** Byte source capability: `job.input`. */
export class Source extends RpcTarget {
  /**
   * Opens `key` for streamed reading.
   *
   * @param _key - Object key.
   * @returns Metadata plus a transferred body stream.
   */
  open(_key: string): Promise<ReadResult> {
    return Promise.reject(unsupported("open"));
  }
}

/** Progress reports for a job invocation (never carries media). */
export class ProgressSink extends RpcTarget {
  /**
   * Reports `percent` in `0..=100` and an operator-facing `message`.
   *
   * @param _percent - Completion percent.
   * @param _message - Operator-facing status.
   * @returns Resolves when the host records progress.
   */
  report(_percent: number, _message: string): Promise<void> {
    return Promise.reject(unsupported("report"));
  }
}

/** Host ↔ database adapter session returned by {@link DatabaseAdapterEntrypoint.openSession}. */
export class AdapterDatabaseSession extends RpcTarget {
  /**
   * Typed SQL-contract advertisement.
   *
   * @returns Guest `DbCapabilities`.
   */
  capabilities(): Promise<unknown> {
    return Promise.reject(unsupported("capabilities"));
  }

  /**
   * Typed atomic batch (`ExecuteRequest` → `ExecuteReply`).
   *
   * @param _request - Cap'n `ExecuteRequest` (structured Workers RPC object).
   * @returns `ExecuteReply`.
   */
  execute(_request: ExecuteRequest): Promise<ExecuteReply> {
    return Promise.reject(unsupported("execute"));
  }

  /**
   * Close the adapter session.
   *
   * @returns Resolves when the session is closed.
   */
  close(): Promise<void> {
    return Promise.resolve();
  }
}

// ---------------------------------------------------------------------------
// Granted context (adapter ↔ author; authors read it through `env`)
// ---------------------------------------------------------------------------

/**
 * Granted `Bindings` for one invocation as the bridge routes carry them:
 * the `context` body field / `x-bookclerk-context` header decoded from
 * `PluginWorker.open(invocation, bindings)`. The adapter merges it onto the
 * author's `env` before dispatching; authors never receive it directly.
 */
export interface GrantedContext {
  /** Invocation identity (`Invocation` struct). */
  invocation?: Partial<Invocation>;
  /** Operator settings for this plugin as `application/json`. */
  config?: ExtensibleConfig;
  /** Granted secret values as `application/json`. */
  secrets?: ExtensibleConfig;
  /** `WORK_FS` object store, when granted. */
  storage?: Destination;
  /** `EVENTS` publisher, when `[[events.producers]]` is granted. */
  events?: EventPublisherBinding;
  /** Named `[[databases]]` bindings. */
  databases?: Array<{ name: string; database: DatabaseBinding }>;
}

/** Job-runner bridge context: the durable job id plus {@link GrantedContext}. */
export interface JobRunnerContext extends GrantedContext {
  /** Durable job id (`Invocation.id` of the job open). */
  jobId?: string;
}

/**
 * Decode an `ExtensibleConfig` (`{ schemaVersion, mediaType, payload }`) into
 * the plain object authors read from `env.CONFIG` / `env.SECRETS`. Non-JSON
 * media types are surfaced verbatim so nothing is lost.
 *
 * @param cfg - Wire config struct, a pre-decoded object, or `undefined`.
 * @returns Plain JSON object (`{}` when empty or malformed).
 */
export function decodeExtensibleConfig(cfg: unknown): JsonObject {
  if (cfg == null) return {};
  if (typeof cfg !== "object") return {};
  const record = cfg as Record<string, unknown>;
  const payload = record.payload;
  let text = "";
  if (payload instanceof Uint8Array) {
    text = new TextDecoder().decode(payload);
  } else if (payload instanceof ArrayBuffer) {
    text = new TextDecoder().decode(new Uint8Array(payload));
  } else if (typeof payload === "string") {
    text = payload;
  } else if (payload && typeof payload === "object" && !("schemaVersion" in record)) {
    return payload as JsonObject;
  }
  const mediaType = String(record.mediaType ?? "");
  if (!text.trim()) return {};
  if (mediaType === "" || /json/i.test(mediaType)) {
    try {
      const parsed: unknown = JSON.parse(text);
      return parsed === null || typeof parsed !== "object" ? {} : (parsed as JsonObject);
    } catch {
      return {};
    }
  }
  return { mediaType, text };
}

/**
 * Wraps a JSON-serializable value as an `application/json` `ExtensibleConfig`
 * (schema version 1) — the shape of `CliInvokeResult.payload` and every other
 * extensible payload on the wire.
 *
 * @param value - JSON-serializable value.
 * @returns Wire-shaped extensible payload.
 */
export function jsonPayload(value: unknown): ExtensibleConfig {
  return {
    schemaVersion: 1,
    mediaType: "application/json",
    payload: new TextEncoder().encode(JSON.stringify(value ?? null)),
  };
}

/**
 * Turns `CliInvokeParams.args` (`[{ name, value }]`) into a plain
 * `{ name: value }` object for CLI handlers.
 *
 * @param params - CLI invocation params from the host.
 * @returns Argument values keyed by name.
 */
export function cliArgs(params: Pick<CliInvokeParams, "args"> | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const arg of params?.args ?? []) {
    if (arg && typeof arg.name === "string") out[arg.name] = String(arg.value ?? "");
  }
  return out;
}

function invocationEnv(rawEnv: unknown, context: GrantedContext | undefined): BookclerkEnv {
  const merged: Record<string, unknown> = { ...((rawEnv as Record<string, unknown>) ?? {}) };
  if (context && typeof context === "object") {
    if (context.config !== undefined) merged.CONFIG = decodeExtensibleConfig(context.config);
    if (context.secrets !== undefined) merged.SECRETS = decodeExtensibleConfig(context.secrets);
    if (context.storage) merged.WORK_FS = context.storage;
    if (context.events) merged.EVENTS = context.events;
    if (Array.isArray(context.databases)) {
      for (const entry of context.databases) {
        if (entry && typeof entry.name === "string" && entry.database) {
          merged[entry.name] = entry.database;
        }
      }
    }
  }
  return Object.freeze(merged) as BookclerkEnv;
}

function applyInvocationEnv(instance: object, context: GrantedContext | undefined): void {
  const target = instance as { env?: unknown };
  const merged = invocationEnv(target.env, context);
  try {
    target.env = merged;
  } catch {
    Object.defineProperty(instance, "env", { value: merged, configurable: true });
  }
}

/**
 * Invocation envelope (`Invocation` struct) with zero values normalized.
 *
 * @param context - Granted context carrying `invocation`.
 * @returns Frozen invocation identity.
 */
export function invocationOf(context: GrantedContext | undefined): Invocation {
  const inv = context && typeof context === "object" ? (context.invocation ?? {}) : {};
  return Object.freeze({
    id: String(inv.id ?? ""),
    accountId: String(inv.accountId ?? ""),
    deadlineUnixMs: Number(inv.deadlineUnixMs ?? 0) || 0,
    correlationId: String(inv.correlationId ?? ""),
    causationId: String(inv.causationId ?? ""),
  });
}

// ---------------------------------------------------------------------------
// Event trigger: `event(batch)` shaped like Workers `queue(batch)`
// ---------------------------------------------------------------------------

/** Wall-clock instant accepted by `retry` / `suspend`: a `Date` or Unix ms. */
export type Instant = Date | number;

function unixMs(value: Instant | undefined): number {
  if (value instanceof Date) return value.getTime();
  const n = Number(value ?? 0);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}

/** JSON-serializable checkpoint accepted by `suspend()`. */
export type Checkpoint = string | JsonObject | unknown[] | number | boolean | null;

function checkpointText(checkpoint: Checkpoint | undefined): string {
  if (checkpoint === undefined || checkpoint === null) return "";
  const text = typeof checkpoint === "string" ? checkpoint : JSON.stringify(checkpoint);
  if (utf8Bytes(text) > MAX_CHECKPOINT_BYTES) {
    throw PluginError.fromWire(
      "payload_too_large",
      `checkpoint is ${utf8Bytes(text)} bytes; exceeds maxCheckpointBytes (${MAX_CHECKPOINT_BYTES})`,
    );
  }
  return text;
}

/** A prior checkpoint delivered on resume. */
export interface DeliveredCheckpoint {
  /** Checkpoint JSON text as recorded by the earlier `suspend()`. */
  json: string;
  /** Schema version the author attached to the checkpoint. */
  schemaVersion: number;
}

/** Options for {@link EventMessage.retry}. */
export interface RetryOptions {
  /** Explicit earliest redelivery instant. */
  retryAt?: Instant;
  /** Delay from now, in seconds; ignored when `retryAt` is set. */
  delaySeconds?: number;
  /** Short operator-facing reason. */
  reason?: string;
}

/** Options for {@link EventMessage.suspend}. */
export interface EventSuspendOptions {
  /** Bounded checkpoint replayed on resume (at most `maxCheckpointBytes`). */
  checkpoint?: Checkpoint;
  /** Schema version of `checkpoint` (default `1`). */
  checkpointSchemaVersion?: number;
  /** Earliest resume instant. */
  wakeAt?: Instant;
  /** Also wake when an event of this type arrives. */
  wakeOnEventType?: string;
  /** Host-owned payload filter for wake events (object or JSON text). */
  wakeOnFilter?: string | JsonObject;
}

/**
 * Recorded per-event outcome (`EventResult` union, bridge JSON projection).
 */
export type EventOutcome =
  | { kind: "ack" }
  | { kind: "retry"; retryAtUnixMs: number; reason: string }
  | { kind: "reject"; reason: string }
  | { kind: "deadLetter"; reason: string }
  | {
      kind: "suspended";
      checkpointJson: string;
      checkpointSchemaVersion: number;
      wakeAtUnixMs: number;
      wakeOnEventType: string;
      wakeOnFilterJson: string;
    };

/**
 * One delivered domain event. Exactly one outcome is recorded per message;
 * the first of `ack` / `retry` / `reject` / `deadLetter` / `suspend` wins and
 * later calls are ignored (as with Workers queue messages). Messages left
 * untouched are acked when `event()` resolves and retried when it throws.
 */
export class EventMessage {
  #result: EventOutcome | null = null;

  /** Outbox event id. */
  readonly id: string;
  /** Event type (`book_acquired`, …). */
  readonly type: string;
  /** Schema version of {@link EventMessage.body}. */
  readonly schemaVersion: number;
  /** When the producer observed the fact. */
  readonly timestamp: Date;
  /** Delivery counter, starting at 1. */
  readonly attempts: number;
  /** Account scope; empty for host-wide events. */
  readonly accountId: string;
  /** Producer plugin id; empty when unknown. */
  readonly source: string;
  /** Trace correlation id. */
  readonly correlationId: string;
  /** Id of the command or event that caused this one. */
  readonly causationId: string;
  /** Consumer-side idempotency key; stable across redeliveries. */
  readonly deduplicationKey: string;
  /** Encoded payload bytes. */
  readonly body: Uint8Array;
  /** Resume ordinal; distinct from `attempts`. */
  readonly invocationSequence: number;
  /** True when this delivery resumes a prior `suspend()`. */
  readonly resumePending: boolean;
  /** Checkpoint from the prior `suspend()`, or `null`. */
  readonly checkpoint: DeliveredCheckpoint | null;
  /** Raw wire envelope. */
  readonly raw: Partial<DomainEvent>;

  constructor(event: Partial<DomainEvent> | undefined) {
    const e = event && typeof event === "object" ? event : {};
    this.id = String(e.eventId ?? "");
    this.type = String(e.eventType ?? "");
    this.schemaVersion = Number(e.schemaVersion ?? 0) || 0;
    this.timestamp = new Date(Number(e.occurredAtUnixMs ?? 0) || 0);
    this.attempts = Number(e.deliveryAttempt ?? 1) || 1;
    this.accountId = String(e.accountId ?? "");
    this.source = String(e.source ?? "");
    this.correlationId = String(e.correlationId ?? "");
    this.causationId = String(e.causationId ?? "");
    this.deduplicationKey = String(e.deduplicationKey ?? "");
    const payload: unknown = e.payload;
    this.body =
      payload instanceof Uint8Array
        ? payload
        : payload instanceof ArrayBuffer
          ? new Uint8Array(payload)
          : new Uint8Array();
    this.invocationSequence = Number(e.invocationSequence ?? 0) || 0;
    this.resumePending = Boolean(e.resumePending);
    const checkpointJson = String(e.checkpointJson ?? "");
    this.checkpoint = checkpointJson
      ? Object.freeze({
          json: checkpointJson,
          schemaVersion: Number(e.checkpointSchemaVersion ?? 0) || 0,
        })
      : null;
    this.raw = e;
  }

  /**
   * Decode {@link EventMessage.body} as JSON.
   *
   * @returns Parsed payload (`{}` for an empty body).
   */
  json<T = JsonObject>(): T {
    if (this.body.byteLength === 0) return {} as T;
    return JSON.parse(new TextDecoder().decode(this.body)) as T;
  }

  /**
   * Recorded outcome.
   *
   * @returns The outcome, or `null` while undecided.
   */
  get result(): EventOutcome | null {
    return this.#result;
  }

  #record(result: EventOutcome): void {
    if (this.#result === null) this.#result = Object.freeze(result);
  }

  /** Mark handled; the host marks the event delivered. */
  ack(): void {
    this.#record({ kind: "ack" });
  }

  /**
   * Redeliver later: at `retryAt`, after `delaySeconds`, or — when both are
   * omitted — whenever the host's backoff chooses.
   *
   * @param options - Redelivery timing and reason.
   */
  retry(options?: RetryOptions): void {
    const explicit = unixMs(options?.retryAt);
    const delay = Number(options?.delaySeconds ?? 0) || 0;
    this.#record({
      kind: "retry",
      retryAtUnixMs: explicit || (delay > 0 ? Date.now() + Math.floor(delay * 1000) : 0),
      reason: String(options?.reason ?? ""),
    });
  }

  /**
   * Stop delivering; the host records `reason`.
   *
   * @param reason - Short operator-facing reason.
   */
  reject(reason?: string): void {
    this.#record({ kind: "reject", reason: String(reason ?? "") });
  }

  /**
   * Park for operator review.
   *
   * @param reason - Short operator-facing reason.
   */
  deadLetter(reason?: string): void {
    this.#record({ kind: "deadLetter", reason: String(reason ?? "") });
  }

  /**
   * Release with a bounded checkpoint; the host redelivers at `wakeAt` or
   * when a `wakeOnEventType` event arrives.
   *
   * @param options - Checkpoint and wake conditions.
   */
  suspend(options?: EventSuspendOptions): void {
    const opts = options ?? {};
    this.#record({
      kind: "suspended",
      checkpointJson: checkpointText(opts.checkpoint),
      checkpointSchemaVersion: Number(opts.checkpointSchemaVersion ?? 1) || 1,
      wakeAtUnixMs: unixMs(opts.wakeAt),
      wakeOnEventType: String(opts.wakeOnEventType ?? ""),
      wakeOnFilterJson:
        opts.wakeOnFilter === undefined || opts.wakeOnFilter === null
          ? ""
          : typeof opts.wakeOnFilter === "string"
            ? opts.wakeOnFilter
            : JSON.stringify(opts.wakeOnFilter),
    });
  }
}

/** One `event(batch)` delivery (`EventBatch` struct), Workers `MessageBatch`-shaped. */
export class EventBatch {
  /** Messages in delivery order. */
  readonly messages: readonly EventMessage[];
  /** Invocation identity of this delivery. */
  invocation: Invocation;

  constructor(events: ReadonlyArray<Partial<DomainEvent>> | undefined, invocation?: Invocation) {
    this.messages = Object.freeze(
      (Array.isArray(events) ? events : []).map((event) => new EventMessage(event)),
    );
    this.invocation = invocation ?? invocationOf(undefined);
  }

  /**
   * Event type shared by the batch.
   *
   * @returns The shared type, or `""` when mixed or empty.
   */
  get type(): string {
    const first = this.messages[0]?.type ?? "";
    return this.messages.every((m) => m.type === first) ? first : "";
  }

  /** Ack every message. */
  ackAll(): void {
    for (const m of this.messages) m.ack();
  }

  /**
   * Retry every message.
   *
   * @param options - Redelivery timing and reason.
   */
  retryAll(options?: RetryOptions): void {
    for (const m of this.messages) m.retry(options);
  }
}

/**
 * Translate recorded {@link EventMessage} outcomes into the wire list.
 * `failed` is the error thrown by the author's `event()`; undecided messages
 * then retry instead of ack.
 *
 * @param batch - Delivered batch after the handler ran.
 * @param failed - Error thrown by the handler, if any.
 * @returns One outcome per message, in order.
 */
export function eventBatchResults(batch: EventBatch, failed: unknown): EventOutcome[] {
  return batch.messages.map((m) => {
    if (m.result) return m.result;
    if (failed !== undefined) {
      return { kind: "retry", retryAtUnixMs: 0, reason: errorMessage(failed) };
    }
    return { kind: "ack" };
  });
}

// ---------------------------------------------------------------------------
// Job trigger: `job(controller)` shaped like Workers `scheduled(controller)`
// ---------------------------------------------------------------------------

/** Bounded, versioned checkpoint. */
export interface JobCheckpoint {
  schemaVersion: number;
  json: string;
}

/** Recorded terminal / suspended job outcome (`JobOutcome` union, bridge JSON projection). */
export type JobOutcomeRecord =
  | { kind: "completed"; message: string; bytesCopied: number }
  | { kind: "retryable"; message: string; retryAfterUnixMs: number }
  | { kind: "rejected"; message: string }
  | { kind: "cancelled"; message: string }
  | { kind: "suspended"; checkpoint: JobCheckpoint; wakeAtUnixMs: number };

/** Value a `job()` handler may return to annotate a `completed` outcome. */
export interface JobCompletion {
  /** Operator-facing summary. */
  message?: string;
  /** Bytes copied by the job, when meaningful. */
  bytesCopied?: number;
}

/** Options for {@link JobController.suspend}. */
export interface JobSuspendOptions {
  /** Bounded checkpoint replayed on resume (at most `maxCheckpointBytes`). */
  checkpoint?: Checkpoint;
  /** Schema version of `checkpoint` (default `1`). */
  checkpointSchemaVersion?: number;
  /** Earliest resume instant. */
  wakeAt?: Instant;
}

/** Options for {@link JobController.retryLater}. */
export interface JobRetryOptions {
  /** Earliest retry instant; omitted lets the host choose. */
  retryAt?: Instant;
  /** Short operator-facing reason. */
  reason?: string;
}

/** Host cancellation watch delivered to a job controller (adapter-internal). */
export interface CancelWatchLike {
  /** Resolves once the host cancels the invocation. */
  wait(): Promise<void>;
}

/** Granted capabilities for one `job()` invocation (adapter-internal). */
export interface GrantedJobCapabilities {
  input?: Source | null;
  output?: Destination | null;
  progress?: ProgressSink | null;
  cancel?: CancelWatchLike | null;
}

/**
 * Everything one job invocation may touch: the durable envelope, `input` /
 * `output` streams, `progress()`, `signal`, and the terminal-outcome recorders
 * `suspend()` / `retryLater()`. Returning normally without a recorded outcome
 * completes the job; throwing rejects it (`unavailable` /
 * `deadline_exceeded` errors are retryable, `cancelled` is cancelled).
 */
export class JobController {
  #result: JobOutcomeRecord | null = null;
  #progress: ProgressSink | null;
  #abort: AbortController;

  /** Full durable envelope as delivered. */
  readonly invocation: Readonly<Partial<JobInvocation>>;
  /** Unique id of this invocation attempt. */
  readonly id: string;
  /** Command type the handler dispatches on. */
  readonly type: string;
  /** Command payload JSON text. */
  readonly payloadJson: string;
  /** Schema version of {@link JobController.payloadJson}. */
  readonly payloadSchemaVersion: number;
  /** Caller idempotency key. */
  readonly idempotencyKey: string;
  /** Failure retry counter, starting at 1. */
  readonly attempt: number;
  /** Deadline hint (Unix ms); the host fence is authoritative. */
  readonly deadlineUnixMs: number;
  /** Trace correlation id. */
  readonly correlationId: string;
  /** Id of the event or command that caused this job. */
  readonly causationId: string;
  /** Resume ordinal. */
  readonly invocationSequence: number;
  /** Step id within a multi-step command; empty when single-step. */
  readonly stepId: string;
  /** Checkpoint from the prior `suspend()`, or `null`. */
  readonly checkpoint: DeliveredCheckpoint | null;
  /** Granted byte source, when the job has input. */
  readonly input: Source | null;
  /** Granted object store, when the job has output. */
  readonly output: Destination | null;
  /** Aborts when the host cancels the invocation. */
  readonly signal: AbortSignal;

  constructor(invocation: Partial<JobInvocation> | undefined, granted?: GrantedJobCapabilities) {
    const inv = (invocation && typeof invocation === "object" ? invocation : {}) as Partial<
      JobInvocation & { invocationSequence: number; stepId: string; deadlineUnixMs: number; causationId: string; checkpointJson: string }
    >;
    this.invocation = Object.freeze({ ...inv });
    this.id = String(inv.invocationId ?? "");
    this.type = String(inv.commandType ?? "");
    this.payloadJson = String(inv.payloadJson ?? "");
    this.payloadSchemaVersion = Number(inv.payloadSchemaVersion ?? 0) || 0;
    this.idempotencyKey = String(inv.idempotencyKey ?? "");
    this.attempt = Number(inv.attempt ?? 1) || 1;
    this.deadlineUnixMs = Number(inv.deadlineUnixMs ?? 0) || 0;
    this.correlationId = String(inv.correlationId ?? "");
    this.causationId = String(inv.causationId ?? "");
    this.invocationSequence = Number(inv.invocationSequence ?? 0) || 0;
    this.stepId = String(inv.stepId ?? "");
    const checkpointJson = String(inv.checkpointJson ?? "");
    this.checkpoint = checkpointJson
      ? Object.freeze({
          json: checkpointJson,
          schemaVersion: Number(inv.checkpointSchemaVersion ?? 0) || 0,
        })
      : null;
    this.input = granted?.input ?? null;
    this.output = granted?.output ?? null;
    this.#progress = granted?.progress ?? null;
    this.#abort = new AbortController();
    this.signal = this.#abort.signal;
    const cancel = granted?.cancel;
    if (cancel && typeof cancel.wait === "function") {
      Promise.resolve()
        .then(() => cancel.wait())
        .then(
          () => this.#abort.abort(PluginError.fromWire("cancelled", "job cancelled by host")),
          () => {},
        );
    }
  }

  /**
   * Decode {@link JobController.payloadJson}.
   *
   * @returns Parsed payload (`{}` when empty).
   */
  json<T = JsonObject>(): T {
    return (this.payloadJson ? JSON.parse(this.payloadJson) : {}) as T;
  }

  /**
   * Report `percent` in `0..=100` with an operator-facing `message`.
   *
   * @param percent - Completion percent.
   * @param message - Operator-facing status.
   * @returns Resolves when the host records progress.
   */
  async progress(percent: number, message?: string): Promise<void> {
    if (!this.#progress) return;
    await this.#progress.report(Number(percent) || 0, String(message ?? ""));
  }

  /**
   * Recorded terminal outcome.
   *
   * @returns The outcome, or `null` while the job is still running.
   */
  get result(): JobOutcomeRecord | null {
    return this.#result;
  }

  #record(result: JobOutcomeRecord): void {
    if (this.#result === null) this.#result = Object.freeze(result);
  }

  /**
   * Release with a bounded checkpoint; the host resumes at `wakeAt`.
   *
   * @param options - Checkpoint and wake time.
   */
  suspend(options?: JobSuspendOptions): void {
    const opts = options ?? {};
    this.#record({
      kind: "suspended",
      checkpoint: {
        schemaVersion: Number(opts.checkpointSchemaVersion ?? 1) || 1,
        json: checkpointText(opts.checkpoint),
      },
      wakeAtUnixMs: unixMs(opts.wakeAt),
    });
  }

  /**
   * Give up this attempt and let the host retry at `retryAt` (or its default).
   *
   * @param options - Retry time and reason.
   */
  retryLater(options?: JobRetryOptions): void {
    const opts = options ?? {};
    this.#record({
      kind: "retryable",
      message: String(opts.reason ?? ""),
      retryAfterUnixMs: unixMs(opts.retryAt),
    });
  }

  /** Host-side cancellation observed (adapter-internal). */
  cancel(): void {
    this.#abort.abort(PluginError.fromWire("cancelled", "job cancelled by host"));
  }
}

/**
 * Translate a `job()` handler's return / throw into the wire outcome.
 *
 * @param job - Controller after the handler ran.
 * @param returned - Handler return value.
 * @param failed - Error thrown by the handler, if any.
 * @returns Wire job outcome.
 */
export function jobOutcomeFor(
  job: JobController,
  returned: JobCompletion | void | undefined,
  failed: unknown,
): JobOutcomeRecord {
  if (job.result) return job.result;
  if (failed !== undefined) {
    const code =
      failed && typeof failed === "object"
        ? ((failed as { wireCode?: string; code?: string }).wireCode ??
          (failed as { code?: string }).code)
        : undefined;
    if (job.signal.aborted || code === "cancelled") {
      return { kind: "cancelled", message: errorMessage(failed) };
    }
    if (code === "unavailable" || code === "deadline_exceeded") {
      return { kind: "retryable", message: errorMessage(failed), retryAfterUnixMs: 0 };
    }
    return { kind: "rejected", message: errorMessage(failed) };
  }
  const extra = returned && typeof returned === "object" ? returned : {};
  return {
    kind: "completed",
    message: String(extra.message ?? ""),
    bytesCopied: Number(extra.bytesCopied ?? 0) || 0,
  };
}

// ---------------------------------------------------------------------------
// Author base classes
// ---------------------------------------------------------------------------

/** Wire `EventBatch` as the adapter delivers it to `bookclerkEvent`. */
export interface WireEventBatch {
  events: Array<Partial<DomainEvent>>;
}

/**
 * Default-export base. Optional trigger methods: {@link BookclerkEntrypoint.event}
 * and {@link BookclerkEntrypoint.job}; optional {@link BookclerkEntrypoint.describe}
 * (identity beyond `plugin.toml`), {@link BookclerkEntrypoint.databaseMigrations},
 * and {@link BookclerkEntrypoint.shutdown}.
 *
 * The `bookclerk*` methods are the adapter-facing dispatch surface; authors
 * neither call nor override them.
 *
 * @example
 * ```ts
 * import { BookclerkEntrypoint, type EventBatch } from "@bookclerk/plugin-sdk/workerd";
 * import type { Env } from "./bookclerk-configuration.js";
 *
 * export default class MyPlugin extends BookclerkEntrypoint<Env> {
 *   async event(batch: EventBatch) {
 *     for (const msg of batch.messages) {
 *       await this.env.DB.prepare("INSERT INTO seen (id) VALUES (?)").bind(msg.id).run();
 *       msg.ack();
 *     }
 *   }
 * }
 * ```
 */
export class BookclerkEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends WorkerEntrypoint<Env> {
  /**
   * Rejects HTTP fetch — workerd guests are Workers-RPC only.
   *
   * @param _request - Incoming HTTP request when the entrypoint is fetch-facing.
   * @returns Always a 404 empty response.
   */
  async fetch(_request?: Request): Promise<Response> {
    return new Response(null, { status: 404 });
  }

  /** Optional identity refinement merged over the manifest projection. */
  describe?(): Promise<PluginDescribe> | PluginDescribe;

  /**
   * `[[events.consumers]]` trigger: handle one delivered batch. Record an
   * outcome per message; untouched messages ack on return and retry on throw.
   */
  event?(batch: EventBatch): Promise<void> | void;

  /**
   * `[triggers] jobs` trigger: run one durable job. Return to complete,
   * throw to reject, or call `job.suspend()` / `job.retryLater()`.
   */
  job?(job: JobController): Promise<JobCompletion | void> | JobCompletion | void;

  /**
   * Complete ordered plugin-owned migration sequence for one named binding.
   * The host calls this at binding initialization, before ordinary execute.
   */
  databaseMigrations?(binding: string): Promise<PluginMigration[]> | PluginMigration[];

  /** Releases guest resources. */
  shutdown?(): Promise<void> | void;

  /**
   * Adapter dispatch for `describe()`.
   *
   * @returns Author refinement or `null` when not implemented.
   * @internal
   */
  async bookclerkDescribe(): Promise<PluginDescribe | null> {
    if (typeof this.describe !== "function") return null;
    return await this.describe();
  }

  /**
   * Adapter dispatch for the `event(batch)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param wireBatch - Wire `EventBatch`.
   * @returns One outcome per event, in order.
   * @internal
   */
  async bookclerkEvent(context: GrantedContext, wireBatch: WireEventBatch): Promise<EventOutcome[]> {
    if (typeof this.event !== "function") {
      throw unsupported("event");
    }
    applyInvocationEnv(this, context);
    const events = Array.isArray(wireBatch?.events) ? wireBatch.events : [];
    const batch = new EventBatch(events, invocationOf(context));
    try {
      await this.event(batch);
      return eventBatchResults(batch, undefined);
    } catch (err) {
      return eventBatchResults(batch, err ?? new Error("event handler failed"));
    }
  }

  /**
   * Adapter dispatch for the `job(controller)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param invocation - Durable command envelope.
   * @param granted - Granted input / output / progress / cancel stubs.
   * @returns Wire job outcome.
   * @internal
   */
  async bookclerkJob(
    context: GrantedContext,
    invocation: Partial<JobInvocation>,
    granted: GrantedJobCapabilities,
  ): Promise<JobOutcomeRecord> {
    if (typeof this.job !== "function") {
      throw unsupported("job");
    }
    applyInvocationEnv(this, context);
    const job = new JobController(invocation, granted);
    try {
      const returned = await this.job(job);
      return jobOutcomeFor(job, returned, undefined);
    } catch (err) {
      return jobOutcomeFor(job, undefined, err ?? new Error("job handler failed"));
    }
  }

  /**
   * Adapter dispatch for `databaseMigrations(binding)`.
   *
   * @param binding - Binding name from `[[databases]]`.
   * @returns Bounded registration (`[]` when not implemented).
   * @internal
   */
  async bookclerkDatabaseMigrations(binding: string): Promise<PluginMigration[]> {
    if (typeof this.databaseMigrations !== "function") return [];
    const migrations = await this.databaseMigrations(String(binding ?? ""));
    return requirePluginMigrationRegistration(Array.isArray(migrations) ? migrations : []);
  }

  /**
   * Adapter dispatch for `shutdown()`.
   *
   * @returns Resolves when the author hook has run.
   * @internal
   */
  async bookclerkShutdown(): Promise<void> {
    if (typeof this.shutdown === "function") await this.shutdown();
  }
}

/**
 * Base for named entrypoints. Subclasses list their RPC surface on the static
 * `bookclerkMethods`; the adapter reaches them only through `bookclerkInvoke`,
 * which installs the granted bindings on `env` before dispatching.
 */
export class NamedEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends WorkerEntrypoint<Env> {
  /** Methods the adapter may dispatch on this entrypoint. */
  static bookclerkMethods: readonly string[] = [];

  /** Invocation identity of the current call (set by `bookclerkInvoke`). */
  invocation: Invocation = invocationOf(undefined);

  /**
   * Rejects HTTP fetch — workerd guests are Workers-RPC only.
   *
   * @param _request - Incoming HTTP request when the entrypoint is fetch-facing.
   * @returns Always a 404 empty response.
   */
  async fetch(_request?: Request): Promise<Response> {
    return new Response(null, { status: 404 });
  }

  /**
   * Adapter dispatch: install granted bindings, then call `method`.
   *
   * @param context - Granted bindings for this invocation.
   * @param method - Method name from the static allowlist.
   * @param args - Method arguments.
   * @returns Method result.
   * @internal
   */
  async bookclerkInvoke(context: GrantedContext, method: string, ...args: unknown[]): Promise<unknown> {
    const allowed = (this.constructor as typeof NamedEntrypoint).bookclerkMethods ?? [];
    const target = (this as unknown as Record<string, unknown>)[method];
    if (!allowed.includes(method) || typeof target !== "function") {
      throw unsupported(method);
    }
    applyInvocationEnv(this, context);
    this.invocation = invocationOf(context);
    return await (target as (...a: unknown[]) => unknown).apply(this, args);
  }
}

const STOREFRONT_METHODS = Object.freeze([
  "login",
  "scan",
  "fetchTitle",
  "listAccounts",
  "loginStart",
  "loginComplete",
  "searchCatalog",
  "expandCandidates",
  "purchaseHint",
  "listDeals",
  "catalogDetail",
  "diagnose",
  "health",
]);

/**
 * `storefront` entrypoint (`ContentSource` interface). Export it as
 * `export class Storefront extends StorefrontEntrypoint`. Every method takes
 * and returns the typed ABI structs generated from `schema/plugin.capnp`.
 */
export class StorefrontEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = STOREFRONT_METHODS;

  /**
   * Password or one-shot OAuth login. The host seals
   * `LoginResult.credentials` into `encrypted_secrets`.
   *
   * @param _params - Login parameters.
   * @returns Account identity plus opaque credentials.
   */
  login(_params: LoginParams): Promise<LoginResult> {
    return Promise.reject(unsupported("login"));
  }
  /**
   * Library scan; the host upserts `ScanSummary.books`.
   *
   * @param _params - Scan parameters (accounts + credentials).
   * @returns Books discovered plus per-account counters.
   */
  scan(_params: ScanParams): Promise<ScanSummary> {
    return Promise.reject(unsupported("scan"));
  }
  /**
   * Fetches one title into `params.cacheDir` and returns plain media paths.
   *
   * @param _params - Title identifiers, credentials, fetch options.
   * @returns Plain media parts plus metadata.
   */
  fetchTitle(_params: FetchTitleParams): Promise<PlainFetch> {
    return Promise.reject(unsupported("fetchTitle"));
  }
  /**
   * Accounts the guest knows about.
   *
   * @returns Account list.
   */
  listAccounts(): Promise<SourceAccount[]> {
    return Promise.reject(unsupported("listAccounts"));
  }
  /**
   * Begins an interactive OAuth login.
   *
   * @param _params - Login parameters.
   * @returns Continuation for {@link StorefrontEntrypoint.loginComplete}.
   */
  loginStart(_params: LoginParams): Promise<LoginStartResult> {
    return Promise.reject(unsupported("loginStart"));
  }
  /**
   * Finishes a login started by {@link StorefrontEntrypoint.loginStart}.
   *
   * @param _params - Continuation plus user input.
   * @returns Account identity plus opaque credentials.
   */
  loginComplete(_params: LoginCompleteParams): Promise<LoginResult> {
    return Promise.reject(unsupported("loginComplete"));
  }
  /**
   * Free-text storefront catalog search.
   *
   * @param _params - Query parameters.
   * @returns Catalog hits.
   */
  searchCatalog(_params: SearchCatalogParams): Promise<CatalogHit[]> {
    return Promise.reject(unsupported("searchCatalog"));
  }
  /**
   * Related-title expansion from a seed title.
   *
   * @param _params - Seed title plus limits.
   * @returns Candidate hits.
   */
  expandCandidates(_params: ExpandCandidatesParams): Promise<CatalogHit[]> {
    return Promise.reject(unsupported("expandCandidates"));
  }
  /**
   * Purchase link / price hint; `null` when the title is unknown.
   *
   * @param _params - Title identifiers.
   * @returns Hint or `null`.
   */
  purchaseHint(_params: PurchaseHintParams): Promise<PurchaseHint | null> {
    return Promise.reject(unsupported("purchaseHint"));
  }
  /**
   * Current storefront deals.
   *
   * @param _params - Optional filters.
   * @returns Deal hits.
   */
  listDeals(_params: ListDealsParams): Promise<CatalogHit[]> {
    return Promise.reject(unsupported("listDeals"));
  }
  /**
   * Full catalog record for one product; `null` when unknown.
   *
   * @param _params - Product identifiers.
   * @returns Catalog hit or `null`.
   */
  catalogDetail(_params: CatalogDetailParams): Promise<CatalogHit | null> {
    return Promise.reject(unsupported("catalogDetail"));
  }
  /**
   * Operator-facing diagnostic lines.
   *
   * @returns Probe lines.
   */
  diagnose(): Promise<string[]> {
    return Promise.resolve([]);
  }
  /**
   * Reports whether the storefront session is usable.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
}

const STORAGE_METHODS = Object.freeze([
  "head",
  "list",
  "get",
  "put",
  "copy",
  "delete",
  "commit",
  "abortStage",
]);

/**
 * `storage` entrypoint (`Destination` interface): an object store the host
 * writes acquired media into. Export it as
 * `export class Storage extends StorageEntrypoint`.
 */
export class StorageEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = STORAGE_METHODS;

  /**
   * Metadata without a body; `null` when the key is missing.
   *
   * @param _key - Object key.
   * @returns Metadata or `null` when the key is missing.
   */
  head(_key: string): Promise<ObjectMetadata | null> {
    return Promise.reject(unsupported("head"));
  }
  /**
   * One page of keys under `options.prefix`.
   *
   * @param _options - Prefix, cursor, and limit.
   * @returns One page of object keys.
   */
  list(_options: ListOptions): Promise<ListPage> {
    return Promise.reject(unsupported("list"));
  }
  /**
   * Streamed read. The body is a transferred stream, not a scalar.
   *
   * @param _key - Object key.
   * @param _options - Optional byte range.
   * @returns Metadata plus a transferred body stream.
   */
  get(_key: string, _options?: ReadOptions): Promise<ReadResult> {
    return Promise.reject(unsupported("get"));
  }
  /**
   * Streamed write. `body` ownership is transferred to the destination.
   *
   * @param _key - Object key.
   * @param _body - Byte stream.
   * @param _options - Optional content type / length.
   * @returns Bytes written and optional etag / sha256.
   */
  put(_key: string, _body: ReadableStream<Uint8Array>, _options?: WriteOptions): Promise<PutResult> {
    return Promise.reject(unsupported("put"));
  }
  /**
   * Server-side copy when the backend supports it.
   *
   * @param _from - Source key.
   * @param _to - Destination key.
   * @returns Bytes copied.
   */
  copy(_from: string, _to: string): Promise<CopyResult> {
    return Promise.reject(unsupported("copy"));
  }
  /**
   * Delete a key (no-op if missing).
   *
   * @param _key - Object key.
   * @returns Resolves when the delete is complete.
   */
  delete(_key: string): Promise<void> {
    return Promise.reject(unsupported("delete"));
  }
  /**
   * Finalize a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Idempotency / commit token.
   * @returns Published object metadata.
   */
  commit(_key: string, _commitToken: string): Promise<PutResult> {
    return Promise.reject(unsupported("commit"));
  }
  /**
   * Abort a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Staging token to discard.
   * @returns Rejects with typed `unsupported` unless overridden.
   */
  abortStage(_key: string, _commitToken: string): Promise<void> {
    return Promise.reject(unsupported("abortStage"));
  }
}

const REMOTE_LIBRARY_METHODS = Object.freeze([
  "health",
  "diagnose",
  "start",
  "stop",
  "scanLibrary",
  "syncListening",
  "pollEvents",
]);

/**
 * `remoteLibrary` entrypoint: lifecycle, rescan, listening sync, and user
 * polling for a remote library server. Export it as
 * `export class RemoteLibrary extends RemoteLibraryEntrypoint`.
 */
export class RemoteLibraryEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = REMOTE_LIBRARY_METHODS;

  /**
   * Reports whether the remote library session is usable.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Operator-facing diagnostic lines.
   *
   * @returns Probe lines.
   */
  diagnose(): Promise<string[]> {
    return Promise.resolve([]);
  }
  /**
   * Starts long-running work for this invocation.
   *
   * @returns Resolves when running.
   */
  start(): Promise<void> {
    return Promise.resolve();
  }
  /**
   * Stops long-running work for this invocation.
   *
   * @returns Resolves when stopped.
   */
  stop(): Promise<void> {
    return Promise.resolve();
  }
  /**
   * Re-syncs the remote library.
   *
   * @param _params - Scan scope.
   * @returns Resolves when the scan has been accepted.
   */
  scanLibrary(_params: ScanLibraryParams): Promise<void> {
    return Promise.reject(unsupported("scanLibrary"));
  }
  /**
   * Push / pull listening progress; the host upserts the rows.
   *
   * @returns Progress rows.
   */
  syncListening(): Promise<ListeningProgress[]> {
    return Promise.reject(unsupported("syncListening"));
  }
  /**
   * Drains external users observed since the last poll.
   *
   * @returns Newly observed users.
   */
  pollEvents(): Promise<ExternalUser[]> {
    return Promise.reject(unsupported("pollEvents"));
  }
}

/**
 * `databaseAdapter` entrypoint: opens typed SQL sessions for the host
 * library. Export it as `export class DatabaseAdapter extends DatabaseAdapterEntrypoint`.
 */
export class DatabaseAdapterEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["openSession"]);

  /**
   * Opens a session. Sessions cannot survive suspension.
   *
   * @returns Adapter session capability.
   */
  openSession(): Promise<AdapterDatabaseSession> {
    return Promise.reject(unsupported("openSession"));
  }
}

/**
 * `cli` entrypoint (`PluginCli`): `describe()` → `CliSchema`,
 * `invoke(params)` → `CliInvokeResult`. Export it as
 * `export class Cli extends CliEntrypoint`.
 */
export class CliEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["describe", "invoke"]);

  /**
   * Guest CLI schema.
   *
   * @returns Declared commands (`{ commands: [] }` when the guest has no CLI).
   */
  describe(): Promise<CliSchema> {
    return Promise.resolve({ commands: [] });
  }
  /**
   * Invokes a guest CLI command.
   *
   * @param _params - Command name plus named argument values.
   * @returns Exit code, captured output, optional structured payload.
   */
  invoke(_params: CliInvokeParams): Promise<CliInvokeResult> {
    return Promise.reject(unsupported("invoke"));
  }
}

/**
 * `oidc` entrypoint: relying-party client templates and credential
 * verification. Export it as `export class Oidc extends OidcEntrypoint`.
 */
export class OidcEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["clients", "authenticateUser"]);

  /**
   * Plugin-provided OIDC authorization-server client templates. The host
   * materializes `oidc_clients` rows; plugins never mint tokens.
   *
   * @returns Templates (`[]` when unused).
   */
  clients(): Promise<OidcClientTemplate[]> {
    return Promise.resolve([]);
  }
  /**
   * Verifies remote credentials on behalf of the host.
   *
   * @param _params - Username / password pair.
   * @returns External user identity.
   */
  authenticateUser(_params: AuthenticateUserParams): Promise<ExternalUser> {
    return Promise.reject(unsupported("authenticateUser"));
  }
}

// ---------------------------------------------------------------------------
// Adapter isolate (trusted; owns GRANTED / BRIDGE_TOKEN / PLUGIN_BACKEND)
// ---------------------------------------------------------------------------

/** Wire name of each named entrypoint (`plugin.toml` `entrypoints`). */
export type EntrypointName =
  | "storefront"
  | "storage"
  | "databaseAdapter"
  | "remoteLibrary"
  | "cli"
  | "oidc";

/** Adapter binding name for each named entrypoint. */
export const ENTRYPOINT_BINDINGS: Readonly<Record<EntrypointName, string>> = Object.freeze({
  storefront: "PLUGIN_STOREFRONT",
  storage: "PLUGIN_STORAGE",
  databaseAdapter: "PLUGIN_DATABASE_ADAPTER",
  remoteLibrary: "PLUGIN_REMOTE_LIBRARY",
  cli: "PLUGIN_CLI",
  oidc: "PLUGIN_OIDC",
});

/** Exported class name the launcher binds for each named entrypoint. */
export const ENTRYPOINT_CLASSES: Readonly<Record<EntrypointName, string>> = Object.freeze({
  storefront: "Storefront",
  storage: "Storage",
  databaseAdapter: "DatabaseAdapter",
  remoteLibrary: "RemoteLibrary",
  cli: "Cli",
  oidc: "Oidc",
});

/** Reverse-channel fetcher bound as `GRANTED` on the adapter isolate. */
export interface GrantedFetcher {
  /**
   * Call the host grant broker.
   *
   * @param input - Request URL.
   * @param init - Optional fetch init.
   * @returns Host response.
   */
  fetch(input: string, init?: RequestInit): Promise<Response>;
}

/** Author default entrypoint as seen through the `PLUGIN` service binding. */
export type AuthorStub = Pick<
  BookclerkEntrypoint,
  "bookclerkDescribe" | "bookclerkEvent" | "bookclerkJob" | "bookclerkDatabaseMigrations" | "bookclerkShutdown"
>;

/** Named author entrypoint as seen through a `PLUGIN_<ENTRYPOINT>` binding. */
export type NamedStub = Pick<NamedEntrypoint, "bookclerkInvoke">;

/**
 * Trusted adapter env. Authors never see this type on their class.
 */
export interface AdapterEnv {
  /** Author default entrypoint. */
  PLUGIN?: AuthorStub;
  /** Named author entrypoints (`PLUGIN_STOREFRONT`, `PLUGIN_STORAGE`, …). */
  PLUGIN_STOREFRONT?: NamedStub;
  PLUGIN_STORAGE?: NamedStub;
  PLUGIN_DATABASE_ADAPTER?: NamedStub;
  PLUGIN_REMOTE_LIBRARY?: NamedStub;
  PLUGIN_CLI?: NamedStub;
  PLUGIN_OIDC?: NamedStub;
  /** Manifest `PluginDescribe` projection (JSON binding). */
  PLUGIN_DESCRIBE?: string | WirePluginDescribe;
  /** Native jail / workerd backend handle (native-behind-workerd). */
  PLUGIN_BACKEND?: {
    /** Broker HTTP surface reached through the service binding. */
    fetch: typeof fetch;
  };
  /** Per-invocation grant reverse channel. */
  GRANTED?: GrantedFetcher;
  /** Isolate-to-host bridge bearer. */
  BRIDGE_TOKEN?: string;
}

function exactLengthBody(
  body: ReadableStream<Uint8Array> | null | undefined,
  expected: number | undefined,
): ReadableStream<Uint8Array> | null | undefined {
  if (body == null || expected == null || !Number.isFinite(expected)) {
    return body;
  }
  const reader = body.getReader();
  let n = 0;
  return new ReadableStream({
    async pull(controller) {
      const { done, value } = await reader.read();
      if (done) {
        if (n !== expected) {
          controller.error(
            PluginError.fromWire("invalid_params", `content-length ${expected} got ${n}`),
          );
          return;
        }
        controller.close();
        return;
      }
      n += value.byteLength;
      if (n > expected) {
        controller.error(
          PluginError.fromWire("payload_too_large", `body exceeded Content-Length ${expected}`),
        );
        return;
      }
      controller.enqueue(value);
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
}

function streamedRead(resp: Response, key: string): ReadResult {
  return {
    meta: {
      key: resp.headers.get("x-bookclerk-key") || key,
      size: Number(resp.headers.get("x-bookclerk-size") || "0"),
      contentType: resp.headers.get("x-bookclerk-content-type") || undefined,
      etag: resp.headers.get("x-bookclerk-etag") || undefined,
    },
    body: resp.body as ReadableStream<Uint8Array>,
  };
}

class GrantedSource extends Source {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;
  #signal: AbortSignal;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, signal: AbortSignal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }

  async open(key: string): Promise<ReadResult> {
    const resp = await this.#granted.fetch(`http://granted/open?key=${encodeURIComponent(key)}`, {
      headers: this.#auth,
      signal: this.#signal,
    });
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return streamedRead(resp, key);
  }
}

class GrantedDestination extends Destination {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;
  #signal: AbortSignal;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, signal: AbortSignal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }

  async put(key: string, body: ReadableStream<Uint8Array>, options?: WriteOptions): Promise<PutResult> {
    const headers: Record<string, string> = { ...this.#auth };
    if (options?.contentType) headers["content-type"] = options.contentType;
    if (options?.contentLength != null) {
      headers["content-length"] = String(options.contentLength);
    }
    const resp = await this.#granted.fetch(`http://granted/put?key=${encodeURIComponent(key)}`, {
      method: "PUT",
      headers,
      body,
      signal: this.#signal,
    });
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return (await resp.json()) as PutResult;
  }
}

class GrantedProgress extends ProgressSink {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;
  #signal: AbortSignal;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, signal: AbortSignal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }

  async report(percent: number, message?: string): Promise<void> {
    await this.#granted.fetch(`http://granted/progress`, {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify({ percent, message: message || "" }),
      signal: this.#signal,
    });
  }
}

/** Resolves `wait()` once the adapter observes host cancellation. */
class CancelWatch extends RpcTarget implements CancelWatchLike {
  #promise: Promise<void>;

  constructor(signal: AbortSignal) {
    super();
    this.#promise = new Promise((resolve) => {
      if (signal.aborted) resolve();
      else signal.addEventListener("abort", () => resolve(), { once: true });
    });
  }

  wait(): Promise<void> {
    return this.#promise;
  }
}

function grantedJobCapabilities(
  env: AdapterEnv,
  grantToken: string,
  controller: AbortController,
): GrantedJobCapabilities {
  const granted = env.GRANTED;
  if (!granted || typeof grantToken !== "string" || !grantToken) {
    throw PluginError.fromWire("internal", "granted reverse channel missing");
  }
  const auth = { Authorization: `Bearer ${grantToken}` };
  return {
    input: new GrantedSource(granted, auth, controller.signal),
    output: new GrantedDestination(granted, auth, controller.signal),
    progress: new GrantedProgress(granted, auth, controller.signal),
    cancel: new CancelWatch(controller.signal),
  };
}

type NativeErrorEnvelope = { error?: { code: string; message: string } };

async function readNativeJson<T>(resp: Response): Promise<T> {
  const value = (await resp.json().catch(() => ({}))) as T & NativeErrorEnvelope;
  if (value && value.error) {
    throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
  }
  if (!resp.ok) {
    throw PluginError.fromWire("internal", `native broker HTTP ${resp.status}`);
  }
  return value;
}

function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/**
 * Typed ABI value → native-broker JSON: Cap'n `Data` fields (`Uint8Array`)
 * travel as base64 text; `bookclerk-workerd` decodes with the same convention.
 *
 * @param value - Plain typed struct value.
 * @returns JSON-safe projection.
 */
export function toBridgeJson(value: unknown): unknown {
  if (value instanceof Uint8Array) return bytesToBase64(value);
  if (value instanceof ArrayBuffer) return bytesToBase64(new Uint8Array(value));
  if (Array.isArray(value)) return value.map(toBridgeJson);
  if (value === null || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
    if (inner === undefined) continue;
    out[key] = toBridgeJson(inner);
  }
  return out;
}

/**
 * Native-behind-workerd backend (`PLUGIN_BACKEND` is the trusted broker's
 * HTTP surface). Entrypoint methods map onto the broker's role routes.
 */
class HttpNativeRoot {
  #fetcher: { fetch: typeof fetch };

  constructor(fetcher: { fetch: typeof fetch }) {
    this.#fetcher = fetcher;
  }

  #headers(ctx: GrantedContext | undefined): Record<string, string> {
    return {
      "content-type": "application/json",
      "x-bookclerk-context": JSON.stringify(toBridgeJson(ctx ?? {})),
    };
  }

  async #json<T>(path: string, ctx: GrantedContext | undefined, body?: Record<string, unknown>): Promise<T> {
    const resp = await this.#fetcher.fetch(`http://backend${path}`, {
      method: "POST",
      headers: this.#headers(ctx),
      body: JSON.stringify(toBridgeJson({ ...(body ?? {}), context: ctx ?? {} })),
    });
    return readNativeJson<T>(resp);
  }

  async describe(): Promise<WirePluginDescribe> {
    const resp = await this.#fetcher.fetch("http://backend/describe", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    });
    return readNativeJson<WirePluginDescribe>(resp);
  }

  async storage(ctx: GrantedContext | undefined, method: string, args: unknown[]): Promise<unknown> {
    const [a, b, c] = args as [unknown, unknown, WriteOptions | undefined];
    switch (method) {
      case "head": {
        const v = await this.#json<{ found?: boolean; meta?: ObjectMetadata }>(
          "/destination/head",
          ctx,
          { key: String(a ?? "") },
        );
        return v.found ? (v.meta ?? null) : null;
      }
      case "list":
        return this.#json<ListPage>("/destination/list", ctx, { options: a ?? {} });
      case "get": {
        let path = `/destination/get?key=${encodeURIComponent(String(a ?? ""))}`;
        const range = (b as ReadOptions | undefined)?.range;
        if (range) {
          path += `&offset=${range.offset}`;
          if (range.length != null) path += `&length=${range.length}`;
        }
        const resp = await this.#fetcher.fetch(`http://backend${path}`, {
          headers: this.#headers(ctx),
        });
        if (!resp.ok) {
          throw PluginError.fromWire("internal", await resp.text());
        }
        return streamedRead(resp, String(a ?? ""));
      }
      case "put": {
        const headers = this.#headers(ctx);
        if (c?.contentType) headers["content-type"] = c.contentType;
        if (c?.contentLength != null) headers["content-length"] = String(c.contentLength);
        if (c?.commitToken) headers["x-bookclerk-commit-token"] = c.commitToken;
        if (c?.stageOnly) headers["x-bookclerk-stage-only"] = "1";
        const resp = await this.#fetcher.fetch(
          `http://backend/destination/put?key=${encodeURIComponent(String(a ?? ""))}`,
          { method: "PUT", headers, body: b as ReadableStream<Uint8Array> },
        );
        return readNativeJson<PutResult>(resp);
      }
      case "copy":
        return this.#json<CopyResult>("/destination/copy", ctx, { from: a, to: b });
      case "delete":
        await this.#json<unknown>("/destination/delete", ctx, { key: String(a ?? "") });
        return undefined;
      case "commit":
        return this.#json<PutResult>("/destination/commit", ctx, { key: a, commitToken: b });
      case "abortStage":
        await this.#json<unknown>("/destination/abortStage", ctx, { key: a, commitToken: b });
        return undefined;
      default:
        throw unsupported(`storage.${method}`);
    }
  }

  async remoteLibrary(ctx: GrantedContext | undefined, method: string, args: unknown[]): Promise<unknown> {
    if (!REMOTE_LIBRARY_METHODS.includes(method)) {
      throw unsupported(`remoteLibrary.${method}`);
    }
    const v = await this.#json<unknown>(
      `/integration/${method}`,
      ctx,
      args[0] != null ? { params: args[0] } : {},
    );
    if (method === "diagnose") {
      const lines = (v as { lines?: string[] } | string[] | undefined);
      return Array.isArray(lines) ? lines : Array.isArray(lines?.lines) ? lines.lines : [];
    }
    if (method === "pollEvents") {
      const users = (v as { users?: ExternalUser[] } | ExternalUser[] | undefined);
      return Array.isArray(users) ? users : Array.isArray(users?.users) ? users.users : [];
    }
    return v;
  }

  async event(ctx: GrantedContext | undefined, event: Partial<DomainEvent>): Promise<EventOutcome> {
    return this.#json<EventOutcome>("/integration/onEvent", ctx, { event });
  }

  async invoke(name: EntrypointName, ctx: GrantedContext | undefined, method: string, args: unknown[]): Promise<unknown> {
    switch (name) {
      case "storage":
        return this.storage(ctx, method, args);
      case "remoteLibrary":
        return this.remoteLibrary(ctx, method, args);
      case "oidc":
        if (method === "authenticateUser") {
          return this.#json<ExternalUser>("/integration/authenticateUser", ctx, { params: args[0] });
        }
        throw unsupported(`oidc.${method}`);
      default:
        throw unsupported(`${name}.${method} via native broker`);
    }
  }
}

function nativeRoot(env: AdapterEnv): HttpNativeRoot {
  const backend = env.PLUGIN_BACKEND;
  if (!backend) {
    throw PluginError.fromWire("unavailable", "PLUGIN_BACKEND binding missing");
  }
  return new HttpNativeRoot(backend);
}

function parseJsonBinding(value: unknown): WirePluginDescribe | null {
  if (value == null) return null;
  if (typeof value === "string") {
    try {
      return JSON.parse(value) as WirePluginDescribe;
    } catch {
      return null;
    }
  }
  return typeof value === "object" ? (value as WirePluginDescribe) : null;
}

/**
 * Generated adapter isolate: `env.PLUGIN` is the author's default entrypoint
 * and `env.PLUGIN_<ENTRYPOINT>` the named ones. Each bridge route becomes one
 * `bookclerk*` dispatch on the author class; authors cannot replace adapter
 * behavior with adapter-internal names.
 *
 * @returns Adapter entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapPluginFromBinding() {
  return createInvocationAdapter();
}

/**
 * Native-behind-workerd generated adapter. Forwards through `PLUGIN_BACKEND`,
 * which stays private workerd config.
 *
 * @returns Adapter entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapPluginFromNative() {
  return createInvocationAdapter();
}

function createInvocationAdapter() {
  return class InvocationAdapter extends WorkerEntrypoint<AdapterEnv> {
    #native(): HttpNativeRoot | null {
      return this.env.PLUGIN_BACKEND && !this.env.PLUGIN ? nativeRoot(this.env) : null;
    }

    #author(): AuthorStub {
      if (this.env.PLUGIN) return this.env.PLUGIN;
      throw PluginError.fromWire("unavailable", "PLUGIN binding missing");
    }

    #named(name: EntrypointName): NamedStub {
      const binding = ENTRYPOINT_BINDINGS[name];
      const stub = binding ? (this.env as Record<string, unknown>)[binding] : undefined;
      if (!stub) {
        throw PluginError.fromWire("unsupported", `${name} entrypoint not exported`);
      }
      return stub as NamedStub;
    }

    async fetch(_request?: Request): Promise<Response> {
      return new Response(null, { status: 404 });
    }

    /**
     * `PluginDescribe`: the manifest projection the launcher binds as
     * `PLUGIN_DESCRIBE`, refined by the author's optional `describe()`.
     * Identity and capabilities always come from the manifest.
     *
     * @returns Wire describe.
     */
    async describe(): Promise<WirePluginDescribe> {
      const native = this.#native();
      if (native) return native.describe();
      const base = parseJsonBinding(this.env.PLUGIN_DESCRIBE) ?? ({} as Partial<WirePluginDescribe>);
      const author = (await this.#author().bookclerkDescribe()) ?? {};
      return {
        ...base,
        ...author,
        apiVersion: base.apiVersion ?? author.apiVersion ?? 3,
        id: base.id ?? author.id,
        capabilities: base.capabilities ?? author.capabilities,
      } as WirePluginDescribe;
    }

    /**
     * Call `method` on the named entrypoint `name` with the granted context.
     *
     * @param name - Entrypoint wire name.
     * @param ctx - Granted context.
     * @param method - Method name.
     * @param args - Method arguments.
     * @returns Method result.
     */
    async invokeEntrypoint(
      name: EntrypointName,
      ctx: GrantedContext | undefined,
      method: string,
      args: unknown[] = [],
    ): Promise<unknown> {
      const list = Array.isArray(args) ? args : [];
      const native = this.#native();
      if (native) return native.invoke(name, ctx ?? {}, method, list);
      return this.#named(name).bookclerkInvoke(ctx ?? {}, method, ...list);
    }

    /**
     * Deliver one domain event to the default entrypoint's `event(batch)`.
     *
     * @param ctx - Granted context.
     * @param event - Wire domain event.
     * @returns Recorded outcome.
     */
    async invokeEvent(ctx: GrantedContext | undefined, event: Partial<DomainEvent>): Promise<EventOutcome> {
      const native = this.#native();
      if (native) return native.event(ctx ?? {}, event);
      const results = await this.#author().bookclerkEvent(ctx ?? {}, { events: [event] });
      const first = Array.isArray(results) ? results[0] : undefined;
      if (!first || typeof first.kind !== "string") {
        throw PluginError.fromWire("internal", "event handler returned no result");
      }
      return first;
    }

    /**
     * `/destination/<op>` route → `storage` entrypoint.
     *
     * @param op - Destination method name.
     * @param ctx - Granted context.
     * @param args - Route arguments.
     * @param body - Stream body for `put`.
     * @returns Method result.
     */
    async invokeDestination(
      op: string,
      ctx: GrantedContext | undefined,
      args: Record<string, unknown> = {},
      body?: ReadableStream<Uint8Array>,
    ): Promise<unknown> {
      switch (op) {
        case "head":
          return this.invokeEntrypoint("storage", ctx, "head", [String(args.key ?? "")]);
        case "list":
          return this.invokeEntrypoint("storage", ctx, "list", [args.options ?? {}]);
        case "get":
          return this.invokeEntrypoint("storage", ctx, "get", [String(args.key ?? ""), args.options]);
        case "put": {
          if (!body) {
            throw PluginError.fromWire("invalid_params", "put missing body stream");
          }
          const options = args.options as WriteOptions | undefined;
          const bounded = exactLengthBody(body, options?.contentLength);
          return this.invokeEntrypoint("storage", ctx, "put", [String(args.key ?? ""), bounded, options]);
        }
        case "copy":
          return this.invokeEntrypoint("storage", ctx, "copy", [String(args.from ?? ""), String(args.to ?? "")]);
        case "delete":
          await this.invokeEntrypoint("storage", ctx, "delete", [String(args.key ?? "")]);
          return { ok: true };
        case "commit":
          return this.invokeEntrypoint("storage", ctx, "commit", [
            String(args.key ?? ""),
            String(args.commitToken ?? ""),
          ]);
        case "abortStage":
          await this.invokeEntrypoint("storage", ctx, "abortStage", [
            String(args.key ?? ""),
            String(args.commitToken ?? ""),
          ]);
          return { ok: true };
        default:
          throw PluginError.fromWire("unsupported", `destination.${op}`);
      }
    }

    /**
     * `/source/open` route → `storage.get`.
     *
     * @param ctx - Granted context.
     * @param key - Object key.
     * @returns Opened byte source result.
     */
    async invokeSourceOpen(ctx: GrantedContext | undefined, key: string): Promise<unknown> {
      return this.invokeEntrypoint("storage", ctx, "get", [String(key ?? "")]);
    }

    /**
     * `/worker/handle` route → default entrypoint `job(controller)`.
     *
     * @param ctx - Granted context.
     * @param invocation - Durable command envelope.
     * @param grantToken - Per-invocation grant token.
     * @param _databases - Per-binding grant tokens (bound in a later ABI step).
     * @returns Wire job outcome.
     */
    async invokeHandle(
      ctx: JobRunnerContext | undefined,
      invocation: Partial<JobInvocation>,
      grantToken: string,
      _databases?: Record<string, string>,
    ): Promise<JobOutcomeRecord> {
      if (this.#native()) {
        throw PluginError.fromWire("unsupported", "native job runner via broker not bound");
      }
      const controller = new AbortController();
      try {
        const granted = grantedJobCapabilities(this.env, grantToken, controller);
        return await this.#author().bookclerkJob(ctx ?? {}, invocation ?? {}, granted);
      } finally {
        controller.abort();
      }
    }

    /**
     * `/cliDescribe` route → `cli.describe`.
     *
     * @returns CLI schema.
     */
    async cliDescribe(): Promise<CliSchema> {
      return (await this.invokeEntrypoint("cli", {}, "describe", [])) as CliSchema;
    }

    /**
     * `/cliInvoke` route → `cli.invoke`.
     *
     * @param params - Command and arguments.
     * @returns Invocation result.
     */
    async cliInvoke(params: CliInvokeParams): Promise<CliInvokeResult> {
      return (await this.invokeEntrypoint("cli", {}, "invoke", [params ?? {}])) as CliInvokeResult;
    }

    /**
     * `/oidcClients` route → `oidc.clients`.
     *
     * @returns Client templates.
     */
    async oidcClients(): Promise<OidcClientTemplate[]> {
      const clients = await this.invokeEntrypoint("oidc", {}, "clients", []);
      return Array.isArray(clients) ? (clients as OidcClientTemplate[]) : [];
    }

    /**
     * `/databaseMigrations` route → default entrypoint `databaseMigrations`.
     *
     * @param binding - Binding name.
     * @returns Bounded registration.
     */
    async databaseMigrations(binding: string): Promise<PluginMigration[]> {
      if (this.#native()) return [];
      return this.#author().bookclerkDatabaseMigrations(String(binding ?? ""));
    }

    /**
     * Shutdown hook → default entrypoint `shutdown`.
     *
     * @returns Resolves when the author hook has run.
     */
    async shutdown(): Promise<void> {
      if (this.#native()) return;
      await this.#author().bookclerkShutdown();
    }
  };
}
