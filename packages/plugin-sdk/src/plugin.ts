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
import { MAX_CHECKPOINT_BYTES, MAX_EVENT_PAYLOAD_BYTES } from "./abi.js";
import {
  createDatabaseBinding,
  decodeExecuteResultReply,
  encodeExecuteRequest,
  type AtomicTransport,
  type DatabaseBinding,
  type ExecuteReply,
  type ExecuteRequest,
} from "./db-execute.js";
import type { BookclerkEnv, EventPublisherBinding, JsonObject, PublishEvent } from "./env.js";
import { PluginError } from "./errors.js";
import {
  dispatchInvoke,
  InvokeError,
  type CapDescriptor,
  type InvokeHost,
  type InvokeOutcome,
} from "./invoke.js";
import { requirePluginMigrationRegistration } from "./plugin-migrations.js";
import type {
  AuthenticateUserParams,
  CatalogDetailParams,
  CatalogHit,
  CliInvokeParams,
  CliInvokeResult,
  CliSchema,
  DbBootstrap,
  DomainEvent,
  ExpandCandidatesParams,
  ExtensibleConfig,
  ExternalUser,
  FetchTitleParams,
  HealthOk,
  IdentityHighWater,
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
  PluginCapabilities as WirePluginCapabilities,
  PluginDescribe as WirePluginDescribe,
  PluginMigration,
  PublishOk,
  PurchaseHint,
  PurchaseHintParams,
  ScanLibraryParams,
  ScanParams,
  ScanSummary,
  SearchCatalogParams,
  SourceAccount,
} from "./generated.js";

export { PluginError } from "./errors.js";

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
// Errors (`PluginError` lives in `errors.ts`; re-exported above)
// ---------------------------------------------------------------------------

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

  /**
   * Bootstrap-only diagnostic metadata (`DbBootstrap`); optional.
   *
   * @returns Engine identity for diagnostics.
   */
  bootstrap(): Promise<DbBootstrap> {
    return Promise.reject(unsupported("bootstrap"));
  }

  /**
   * Identity high-water marks for a logical export; optional.
   *
   * @returns One row per identity column.
   */
  exportIdentity(): Promise<IdentityHighWater[]> {
    return Promise.reject(unsupported("exportIdentity"));
  }

  /**
   * Restore identity high-water marks after a logical import; optional.
   *
   * @param _rows - Rows from an earlier {@link AdapterDatabaseSession.exportIdentity}.
   * @returns Resolves when the marks are applied.
   */
  importIdentity(_rows: IdentityHighWater[]): Promise<void> {
    return Promise.reject(unsupported("importIdentity"));
  }

  /**
   * Names of user relations the host may drop before a restore; optional.
   *
   * @returns Relation names.
   */
  listUserRelations(): Promise<string[]> {
    return Promise.reject(unsupported("listUserRelations"));
  }

  /**
   * Prepare the engine for a unit restore (disable constraints, …); optional.
   *
   * @returns Resolves when the engine is ready for the restore.
   */
  prepareUnitRestore(): Promise<void> {
    return Promise.reject(unsupported("prepareUnitRestore"));
  }

  /**
   * Drop the named user relations; optional.
   *
   * @param _names - Relation names from {@link AdapterDatabaseSession.listUserRelations}.
   * @returns Resolves when the relations are gone.
   */
  dropUserRelations(_names: string[]): Promise<void> {
    return Promise.reject(unsupported("dropUserRelations"));
  }

  /**
   * Re-check constraints after a unit restore; optional.
   *
   * @returns Resolves when every constraint holds.
   */
  assertRestoreConstraints(): Promise<void> {
    return Promise.reject(unsupported("assertRestoreConstraints"));
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
  databases?: NamedDatabase[];
}

/**
 * Granted-channel SQL transport the adapter hands to the author isolate for
 * one named `[[databases]]` binding. The author-side SDK wraps it as the
 * D1-shaped {@link DatabaseBinding} on `env.<BINDING>`; Python authors call
 * {@link GrantedDatabaseTransport.executeBytes} with Cap'n bytes instead.
 */
export interface GrantedDatabaseTransport extends AtomicTransport {
  /**
   * Same batch as {@link AtomicTransport.execute}, as Cap'n Proto bytes
   * (`ExecuteRequest` in, `ExecuteResultReply` out).
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns Unpacked `ExecuteResultReply` message.
   */
  executeBytes(bytes: Uint8Array): Promise<Uint8Array>;
}

/**
 * One named `[[databases]]` binding on the granted context: either an
 * in-isolate {@link DatabaseBinding} or the adapter's granted-channel
 * {@link GrantedDatabaseTransport} the author-side SDK wraps.
 */
export interface NamedDatabase {
  /** Binding name (`env.<name>`). */
  name: string;
  /** Ready binding (same isolate). */
  database?: DatabaseBinding;
  /** Granted transport (crosses the adapter → author RPC boundary). */
  transport?: GrantedDatabaseTransport;
}

/**
 * Context as the launcher sends it on the bridge: {@link GrantedContext}
 * plus the grant tokens the adapter exchanges for the author's `EVENTS` and
 * `env.<BINDING>` stubs. Authors never receive this shape.
 */
export interface BridgeContext extends Omit<GrantedContext, "databases"> {
  /** Granted-channel bearer for `env.EVENTS.publish`; stripped before dispatch. */
  eventsToken?: string;
  /** Named `[[databases]]` binding → database-only bearer for `/db/execute`. */
  databases?: Record<string, string>;
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
        if (!entry || typeof entry.name !== "string" || !entry.name) continue;
        if (entry.database) {
          merged[entry.name] = entry.database;
        } else if (entry.transport) {
          const transport = entry.transport;
          merged[entry.name] = createDatabaseBinding({ execute: (request) => transport.execute(request) });
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
 * Open adapter sessions by id. Workers RPC stubs cannot be retained across
 * bridge requests, so the session object stays in the author isolate and the
 * launcher names it through `X-Bookclerk-Target`.
 */
const ADAPTER_SESSIONS = new Map<string, AdapterDatabaseSession>();

const ADAPTER_SESSION_METHODS: ReadonlySet<string> = new Set([
  "capabilities",
  "execute",
  "close",
  "bootstrap",
  "exportIdentity",
  "importIdentity",
  "listUserRelations",
  "prepareUnitRestore",
  "dropUserRelations",
  "assertRestoreConstraints",
]);

function freshSessionId(): string {
  const c = (
    globalThis as {
      crypto?: { randomUUID?: () => string; getRandomValues?: (out: Uint8Array) => Uint8Array };
    }
  ).crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  const bytes = new Uint8Array(16);
  if (c && typeof c.getRandomValues === "function") c.getRandomValues(bytes);
  else for (let i = 0; i < bytes.length; i++) bytes[i] = Math.floor(Math.random() * 256);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * `databaseAdapter` entrypoint: opens typed SQL sessions for the host
 * library. Export it as `export class DatabaseAdapter extends DatabaseAdapterEntrypoint`.
 *
 * The adapter isolate drives sessions through two dispatch verbs on
 * {@link DatabaseAdapterEntrypoint.bookclerkInvoke}: `openSession` returns a
 * fresh session id, and `session(id, method, ...params)` calls `method` on
 * the retained {@link AdapterDatabaseSession}; `close` releases the entry.
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

  /**
   * Adapter dispatch: `openSession` → session id; `session` → call on a
   * retained session (`args` = `[id, method, ...params]`).
   *
   * @param context - Granted bindings for this invocation.
   * @param method - `openSession` or `session`.
   * @param args - Method arguments.
   * @returns Session id for `openSession`, else the session method result.
   * @internal
   */
  override async bookclerkInvoke(context: GrantedContext, method: string, ...args: unknown[]): Promise<unknown> {
    if (method === "openSession") {
      const session = (await super.bookclerkInvoke(context, "openSession")) as AdapterDatabaseSession;
      if (!session || typeof session !== "object") {
        throw PluginError.fromWire("internal", "openSession returned no session");
      }
      const id = freshSessionId();
      ADAPTER_SESSIONS.set(id, session);
      return id;
    }
    if (method === "session") {
      const [id, sessionMethod, ...params] = args;
      const session = typeof id === "string" ? ADAPTER_SESSIONS.get(id) : undefined;
      if (!session) {
        throw PluginError.fromWire("invalid_params", `unknown adapter session ${String(id)}`);
      }
      const name = String(sessionMethod ?? "");
      const target = (session as unknown as Record<string, unknown>)[name];
      if (!ADAPTER_SESSION_METHODS.has(name) || typeof target !== "function") {
        throw unsupported(name);
      }
      applyInvocationEnv(this, context);
      this.invocation = invocationOf(context);
      try {
        return await (target as (...a: unknown[]) => unknown).apply(session, params);
      } finally {
        if (name === "close") ADAPTER_SESSIONS.delete(id as string);
      }
    }
    return super.bookclerkInvoke(context, method, ...args);
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
// Adapter isolate (trusted; owns GRANTED / BRIDGE_TOKEN / PLUGIN_DESCRIBE)
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

/**
 * Encode a `PublishEvent.payload` (bytes, string, or JSON value) as bytes.
 *
 * @param payload - Author-supplied payload.
 * @returns UTF-8 / raw payload bytes.
 */
function publishPayloadBytes(payload: PublishEvent["payload"]): Uint8Array {
  if (payload === undefined || payload === null) return new Uint8Array(0);
  if (payload instanceof Uint8Array) return payload;
  if (payload instanceof ArrayBuffer) return new Uint8Array(payload);
  if (typeof payload === "string") return new TextEncoder().encode(payload);
  return new TextEncoder().encode(JSON.stringify(payload));
}

/**
 * `EVENTS` binding handed to the author: the adapter exchanges the host's
 * events grant token on the granted channel, so the author isolate never
 * holds the bearer itself.
 */
class GrantedEvents extends RpcTarget implements EventPublisherBinding {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the events grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Publish one domain event through the host outbox.
   *
   * @param event - Event type, schema version, payload, dedup key.
   * @returns Outbox row identity.
   */
  async publish(event: PublishEvent): Promise<PublishOk> {
    if (!event || typeof event.eventType !== "string" || !event.eventType) {
      throw PluginError.fromWire("invalid_params", "publish requires eventType");
    }
    const payload = publishPayloadBytes(event.payload);
    if (payload.byteLength > MAX_EVENT_PAYLOAD_BYTES) {
      throw PluginError.fromWire(
        "payload_too_large",
        `event payload of ${payload.byteLength} bytes exceeds ${MAX_EVENT_PAYLOAD_BYTES}`,
      );
    }
    const wire = toBridgeJson({
      eventType: event.eventType,
      schemaVersion: Number(event.schemaVersion ?? 1) || 1,
      deduplicationKey: String(event.deduplicationKey ?? ""),
      payload,
      occurredAtUnixMs: Number(event.occurredAtUnixMs ?? 0) || 0,
      correlationId: String(event.correlationId ?? ""),
      causationId: String(event.causationId ?? ""),
    });
    const resp = await this.#granted.fetch("http://granted/events/publish", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire),
    });
    const value = (await resp.json().catch(() => ({}))) as {
      error?: { code?: string; message?: string };
      eventId?: unknown;
      duplicate?: unknown;
    };
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `events publish HTTP ${resp.status}`);
    }
    return { eventId: String(value.eventId ?? ""), duplicate: Boolean(value.duplicate) };
  }
}

/**
 * Named `[[databases]]` binding transport handed to the author: the adapter
 * exchanges the binding's database-only grant token on `POST /db/execute`,
 * so the author isolate never holds the bearer itself. ABI failures come
 * back inside the `ExecuteResultReply` union and surface as `PluginError`.
 */
class GrantedDatabase extends RpcTarget implements GrantedDatabaseTransport {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the database grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Run one typed atomic batch through the host broker.
   *
   * @param request - Structured `ExecuteRequest`.
   * @returns Structured `ExecuteReply`.
   */
  async execute(request: ExecuteRequest): Promise<ExecuteReply> {
    return decodeExecuteResultReply(await this.executeBytes(encodeExecuteRequest(request)));
  }

  /**
   * Same batch as Cap'n Proto bytes (Python authors encode locally).
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns Unpacked `ExecuteResultReply` message.
   */
  async executeBytes(bytes: Uint8Array): Promise<Uint8Array> {
    const resp = await this.#granted.fetch("http://granted/db/execute", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/octet-stream" },
      body: bytes,
    });
    if (!resp.ok) {
      const text = await resp.text().catch(() => "");
      throw PluginError.fromWire(
        resp.status === 401 ? "unauthorized" : resp.status === 413 ? "payload_too_large" : "unavailable",
        `database grant: ${resp.status} ${text}`.trim(),
      );
    }
    return new Uint8Array(await resp.arrayBuffer());
  }
}

/**
 * Host cancellation as a granted long poll: `GET /cancel` answers `200` once
 * the host fences the invocation and `204` when the poll window lapses, in
 * which case the watch re-arms. Polling stops without resolving on any error
 * or once `stop` aborts (the adapter aborts it when the job call returns).
 */
class CancelWatch extends RpcTarget implements CancelWatchLike {
  #promise: Promise<void>;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, stop: AbortSignal) {
    super();
    this.#promise = new Promise<void>((resolve) => {
      const poll = async (): Promise<void> => {
        while (!stop.aborted) {
          const resp = await granted.fetch("http://granted/cancel", { headers: auth, signal: stop });
          if (resp.status === 200) {
            resolve();
            return;
          }
          if (resp.status !== 204) return;
        }
      };
      poll().catch(() => {});
    });
  }

  wait(): Promise<void> {
    return this.#promise;
  }
}

function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

function base64ToBytes(text: string): Uint8Array {
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Cap'n `Data` field names; bridge JSON carries them as base64 text. */
const BRIDGE_BYTES_FIELDS: ReadonlySet<string> = new Set(["payload", "sha256", "credentials"]);

/**
 * Bridge JSON → typed ABI value: base64 `Data` fields (`payload`, `sha256`,
 * `credentials`) become `Uint8Array`; everything else is structurally
 * identical, so no per-struct codec is needed.
 *
 * @param value - Bridge JSON value.
 * @returns Typed struct value.
 */
export function fromBridgeJson(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(fromBridgeJson);
  if (value === null || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
    if (BRIDGE_BYTES_FIELDS.has(key) && typeof inner === "string") {
      out[key] = base64ToBytes(inner);
    } else if (
      BRIDGE_BYTES_FIELDS.has(key) &&
      Array.isArray(inner) &&
      inner.every((n) => typeof n === "number")
    ) {
      out[key] = Uint8Array.from(inner as number[]);
    } else {
      out[key] = fromBridgeJson(inner);
    }
  }
  return out;
}

/**
 * Typed ABI value → bridge JSON: Cap'n `Data` fields (`Uint8Array`) travel as
 * base64 text; `bookclerk-workerd` decodes with the same convention.
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

function parseHeaderJson(text: string | null | undefined, name: string): unknown {
  if (text === null || text === undefined || text === "") return undefined;
  try {
    return JSON.parse(String(text)) as unknown;
  } catch (err) {
    throw new InvokeError(400, "invalid_params", `${name} header is not JSON: ${errorMessage(err)}`);
  }
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
 * and `env.PLUGIN_<ENTRYPOINT>` the named ones. Every `POST /invoke` call
 * (Cap'n Proto bytes) and every stream route becomes one `bookclerk*`
 * dispatch on the author class; authors cannot replace adapter behavior with
 * adapter-internal names.
 *
 * @returns Adapter entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapPluginFromBinding() {
  return createInvocationAdapter();
}

/**
 * Native-behind-workerd generated adapter: control plane only. The launcher
 * forwards every entrypoint call to the native guest as typed Cap'n Proto;
 * this isolate decides `describe` (merged against `PLUGIN_DESCRIBE`) and
 * `open` policy (`openInvocation`) and receives `shutdown`. There is no
 * author `PLUGIN` binding.
 *
 * @returns Adapter entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapPluginFromNative() {
  return createInvocationAdapter();
}

/** Wire names of the trigger families the launcher may ask `openInvocation` about. */
export const TRIGGER_FAMILIES: Readonly<Record<"eventConsumer" | "jobRunner", string>> =
  Object.freeze({
    eventConsumer: "eventConsumer",
    jobRunner: "jobRunner",
  });

/**
 * Filters requested entrypoint / trigger family names against the manifest
 * capabilities: named entrypoints must be declared in `entrypoints`,
 * `eventConsumer` needs a non-empty `consumes`, and `jobRunner` needs
 * `jobs` or the `storage` entrypoint (storage guests run the host
 * `stream_copy` job). Unknown names and a missing manifest fail closed.
 *
 * @param capabilities - Manifest capabilities from `PLUGIN_DESCRIBE`.
 * @param requested - Family names the launcher wants to export.
 * @returns The authorized subset in request order.
 */
export function allowedEntrypointFamilies(
  capabilities: Partial<WirePluginCapabilities> | null | undefined,
  requested: readonly string[],
): string[] {
  if (!capabilities || typeof capabilities !== "object") return [];
  const entrypoints = Array.isArray(capabilities.entrypoints) ? capabilities.entrypoints : [];
  const consumes = Array.isArray(capabilities.consumes) ? capabilities.consumes : [];
  const jobs = Array.isArray(capabilities.jobs) ? capabilities.jobs : [];
  const exportsStorage = entrypoints.includes("storage");
  const out: string[] = [];
  for (const name of requested) {
    if (typeof name !== "string" || out.includes(name)) continue;
    if (name === TRIGGER_FAMILIES.eventConsumer) {
      if (consumes.length > 0) out.push(name);
    } else if (name === TRIGGER_FAMILIES.jobRunner) {
      if (jobs.length > 0 || exportsStorage) out.push(name);
    } else if (name in ENTRYPOINT_BINDINGS && entrypoints.includes(name as EntrypointName)) {
      out.push(name);
    }
  }
  return out;
}

/**
 * Merges a guest's `describe()` under the manifest projection: identity,
 * `apiVersion`, and `capabilities` always come from the manifest; the guest's
 * presentation fields, features, and limits are kept. The guest's `cli`
 * schema wins only when it declares commands.
 *
 * @param base - Manifest projection (`PLUGIN_DESCRIBE`).
 * @param guest - Guest-reported describe (author refinement or native).
 * @returns Wire describe.
 */
function mergeDescribe(
  base: Partial<WirePluginDescribe>,
  guest: Partial<WirePluginDescribe>,
): WirePluginDescribe {
  const guestCli = guest.cli;
  const cli =
    guestCli && Array.isArray(guestCli.commands) && guestCli.commands.length > 0
      ? guestCli
      : base.cli ?? guestCli;
  return {
    ...base,
    ...guest,
    apiVersion: base.apiVersion ?? guest.apiVersion ?? 3,
    id: base.id ?? guest.id,
    capabilities: base.capabilities ?? guest.capabilities,
    ...(cli === undefined ? {} : { cli }),
  } as WirePluginDescribe;
}

function createInvocationAdapter() {
  return class InvocationAdapter extends WorkerEntrypoint<AdapterEnv> {
    #manifest(): Partial<WirePluginDescribe> {
      return parseJsonBinding(this.env.PLUGIN_DESCRIBE) ?? ({} as Partial<WirePluginDescribe>);
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

    /**
     * Turn the bridge context into the author-facing one: the host's events
     * grant token becomes an `EVENTS` stub and never reaches the author.
     *
     * @param ctx - Bridge context from the launcher.
     * @returns Author-facing granted context.
     */
    #bindContext(ctx: BridgeContext | undefined): GrantedContext {
      const source = ctx && typeof ctx === "object" ? ctx : {};
      const { eventsToken, databases, ...rest } = source;
      const bound: GrantedContext = rest;
      const granted = this.env.GRANTED;
      if (typeof eventsToken === "string" && eventsToken && granted) {
        bound.events = new GrantedEvents(granted, {
          Authorization: `Bearer ${eventsToken}`,
        });
      }
      if (databases && typeof databases === "object" && !Array.isArray(databases) && granted) {
        const named: NamedDatabase[] = [];
        for (const [name, token] of Object.entries(databases)) {
          if (!name || typeof token !== "string" || !token) continue;
          named.push({
            name,
            transport: new GrantedDatabase(granted, { Authorization: `Bearer ${token}` }),
          });
        }
        if (named.length > 0) bound.databases = named;
      }
      return bound;
    }

    async fetch(_request?: Request): Promise<Response> {
      return new Response(null, { status: 404 });
    }

    /**
     * `PluginDescribe`: the manifest projection the launcher binds as
     * `PLUGIN_DESCRIBE`, refined by the guest's describe. Author mode calls
     * the author's optional `describe()`; native-behind-workerd passes the
     * native guest's typed describe as `native`. Identity and capabilities
     * always come from the manifest.
     *
     * @param native - Native guest describe (native-behind-workerd only).
     * @returns Wire describe.
     */
    async describe(native?: Partial<WirePluginDescribe>): Promise<WirePluginDescribe> {
      const base = this.#manifest();
      if (native !== undefined) {
        if (native === null || typeof native !== "object") {
          throw PluginError.fromWire("invalid_params", "native describe must be an object");
        }
        return mergeDescribe(base, native);
      }
      const author = (await this.#author().bookclerkDescribe()) ?? {};
      return mergeDescribe(base, author);
    }

    /**
     * `/open` policy for native-behind-workerd: validates the invocation
     * envelope and returns the requested entrypoint / trigger families the
     * manifest (`PLUGIN_DESCRIBE`) lets this plugin export. The launcher
     * nulls every family that is not returned.
     *
     * @param ctx - Open context (`invocation`, `config`, `secrets`).
     * @param requested - Family names the launcher wants to export.
     * @returns Authorized subset of `requested`.
     */
    async openInvocation(
      ctx: GrantedContext | undefined,
      requested: readonly string[] = [],
    ): Promise<string[]> {
      const invocation = ctx && typeof ctx === "object" ? ctx.invocation : undefined;
      if (!invocation || typeof invocation.id !== "string" || !invocation.id) {
        throw PluginError.fromWire("invalid_params", "open requires a non-empty invocation id");
      }
      const list = Array.isArray(requested) ? requested : [];
      return allowedEntrypointFamilies(this.#manifest().capabilities, list);
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
      ctx: BridgeContext | undefined,
      method: string,
      args: unknown[] = [],
    ): Promise<unknown> {
      const list = Array.isArray(args) ? args : [];
      return this.#named(name).bookclerkInvoke(this.#bindContext(ctx), method, ...list);
    }

    /**
     * Stream routes (`GET /destination/get`, `PUT /destination/put`) →
     * `storage` entrypoint. Every other `Destination` method travels on
     * `POST /invoke` ({@link InvocationAdapter.invoke}).
     *
     * @param op - `get` or `put`.
     * @param ctx - Granted context.
     * @param args - Route arguments.
     * @param body - Stream body for `put`.
     * @returns Method result.
     */
    async invokeDestination(
      op: string,
      ctx: BridgeContext | undefined,
      args: Record<string, unknown> = {},
      body?: ReadableStream<Uint8Array>,
    ): Promise<unknown> {
      switch (op) {
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
        default:
          throw PluginError.fromWire("unsupported", `destination.${op} is not a stream route`);
      }
    }

    /**
     * `/source/open` route → `storage.get`.
     *
     * @param ctx - Granted context.
     * @param key - Object key.
     * @returns Opened byte source result.
     */
    async invokeSourceOpen(ctx: BridgeContext | undefined, key: string): Promise<unknown> {
      return this.invokeEntrypoint("storage", ctx, "get", [String(key ?? "")]);
    }

    /**
     * Resolve one request capability descriptor (`X-Bookclerk-Caps` entry)
     * into the granted-channel stub handed to the author.
     *
     * @param descriptor - Capability descriptor.
     * @param stop - Aborts when the call returns.
     * @returns Granted stub, or `undefined` for an unknown kind.
     */
    #importCap(descriptor: CapDescriptor, stop: AbortSignal): unknown {
      const granted = this.env.GRANTED;
      if (!granted) {
        throw new InvokeError(500, "unavailable", "GRANTED reverse channel missing");
      }
      const auth = { Authorization: `Bearer ${String(descriptor.token ?? "")}` };
      switch (descriptor.kind) {
        case "source":
          return new GrantedSource(granted, auth, stop);
        case "destination":
          return new GrantedDestination(granted, auth, stop);
        case "progress":
          return new GrantedProgress(granted, auth, stop);
        case "cancellation":
          return new CancelWatch(granted, auth, stop);
        default:
          return undefined;
      }
    }

    /**
     * `POST /invoke`: one ABI method call as Cap'n Proto bytes. The bridge
     * passes the request headers through verbatim; the reply carries the
     * `$Results` bytes plus the descriptors of capabilities it exported.
     * Transport failures (unknown method, malformed message, missing
     * binding) come back as an {@link InvokeFailure} with the HTTP status the
     * bridge should answer with — never as a thrown error, because Workers
     * RPC drops own properties of thrown errors.
     *
     * @param iface - `X-Bookclerk-Interface` (Cap'n interface name).
     * @param method - `X-Bookclerk-Method`.
     * @param contextJson - `X-Bookclerk-Context` bridge JSON, or `null`.
     * @param capsJson - `X-Bookclerk-Caps` JSON descriptor array, or `null`.
     * @param target - `X-Bookclerk-Target` isolate object id, or `null`.
     * @param body - `$Params` message bytes.
     * @returns Reply bytes plus capability descriptors, or a transport failure.
     */
    async invoke(
      iface: string,
      method: string,
      contextJson: string | null,
      capsJson: string | null,
      target: string | null,
      body: Uint8Array,
    ): Promise<InvokeOutcome> {
      const host: InvokeHost = {
        author: () => this.env.PLUGIN,
        named: (name) => {
          const binding = ENTRYPOINT_BINDINGS[name];
          return (this.env as Record<string, unknown>)[binding] as NamedStub | undefined;
        },
        bindContext: (ctx) => this.#bindContext(ctx),
        importCap: (descriptor, stop) => this.#importCap(descriptor, stop),
      };
      try {
        const context = parseHeaderJson(contextJson, "context");
        if (context !== undefined && (context === null || typeof context !== "object" || Array.isArray(context))) {
          throw new InvokeError(400, "invalid_params", "context header must be a JSON object");
        }
        const caps = parseHeaderJson(capsJson, "caps") ?? [];
        if (!Array.isArray(caps)) {
          throw new InvokeError(400, "invalid_params", "caps header must be a JSON array");
        }
        const ctx = context === undefined ? undefined : (fromBridgeJson(context) as BridgeContext);
        return await dispatchInvoke(
          String(iface ?? ""),
          String(method ?? ""),
          ctx,
          caps as CapDescriptor[],
          typeof target === "string" && target ? target : null,
          body instanceof Uint8Array ? body : new Uint8Array(body ?? []),
          host,
        );
      } catch (err) {
        if (err instanceof InvokeError) {
          return { status: err.status, error: { code: err.wireCode, message: err.message } };
        }
        return { status: 500, error: { code: "internal", message: errorMessage(err) } };
      }
    }

    /**
     * `/shutdown` route → default entrypoint `shutdown`. Without an author
     * `PLUGIN` binding (native-behind-workerd) there is nothing to run here:
     * the launcher shuts the native guest down over Cap'n Proto.
     *
     * @returns Resolves when the author hook has run.
     */
    async shutdown(): Promise<void> {
      if (!this.env.PLUGIN) return;
      await this.#author().bookclerkShutdown();
    }
  };
}
