/**
 * TypeScript typed twin of the workerd runtime (`embed/bookclerk_plugin.js`)
 * for product `apiVersion` 3 — the Workers-idiom author model.
 *
 * - The default export extends {@link BookclerkEntrypoint}; triggers are
 *   handler methods on it: `event(batch)` for `[[events.consumers]]` and
 *   `job(job)` for `[triggers] jobs`.
 * - Capabilities the host calls are named exported classes (`Storefront`,
 *   `Storage`, `RemoteLibrary`, `DatabaseAdapter`, `Cli`, `Oidc`,
 *   `Metadata`) extending the matching `*Entrypoint` base with typed methods.
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
import {
  MAX_CHECKPOINT_BYTES,
  MAX_EVENT_PAYLOAD_BYTES,
  MAX_KV_KEY_BYTES,
  MAX_KV_METADATA_BYTES,
  MAX_KV_VALUE_BYTES,
  MAX_LIST_PAGE,
} from "./abi.js";
import {
  createDatabaseBinding,
  decodeDbCapabilitiesReply,
  decodeExecuteResultReply,
  encodeDbCapabilitiesReply,
  encodeExecuteRequest,
  encodeExecuteResultReply,
  type AtomicTransport,
  type DatabaseBinding,
  type ExecuteReply,
  type ExecuteRequest,
} from "./db-execute.js";
import type {
  BookclerkEnv,
  EventPublisherBinding,
  EventReadOptions,
  JsonObject,
  KvListOptionsBinding,
  KvListResultBinding,
  KvNamespaceBinding,
  KvPutOptionsBinding,
  KvValueType,
  KvValueWithMetadata,
  AiBinding,
  AiTextEmbeddingsRequest,
  AiTextEmbeddingsResponse,
  NotifyBinding,
  NotifyRaise,
  PublishEvent,
  QueueBinding,
  QueueSendBatchEntry,
  QueueSendOptions,
  HostWebSocket,
  SocketMessageData,
  WebhookDeliveredEvent,
} from "./env.js";
import { PluginError, parseLimitExceeded } from "./errors.js";
import { ExecuteStreamFrameCodec, encodeMessage } from "./generated-wire.js";
import {
  dispatchInvoke,
  InvokeError,
  type CapDescriptor,
  type InvokeHost,
  type InvokeOutcome,
} from "./invoke.js";
import { requirePluginMigrationRegistration } from "./plugin-migrations.js";
import {
  hostWebSocketOf,
  socketMessageData,
  socketsBinding,
  type SocketOp,
  type SocketsTransport,
  type SocketTransportReply,
} from "./sockets.js";
import {
  grantedOriginConnect,
  grantedOriginFetch,
  originBinding,
  type OriginConnectTransportReply,
  type OriginTransport,
  type OriginTransportReply,
} from "./origin.js";
import {
  webhooksBinding,
  type WebhookOp,
  type WebhooksTransport,
  type WebhookTransportReply,
} from "./webhooks.js";
import type { ObjectChangeKind } from "./abi.js";
import type {
  MoveManyTarget,
  AuthenticateUserParams,
  CatalogDetailParams,
  AdapterExecuteRequest,
  CatalogHit,
  CliInvokeParams,
  CliInvokeResult,
  CliSchema,
  DbBootstrap,
  DbCapabilities,
  DomainEvent,
  EventReadPage,
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
  CoverFetchParams,
  CoverImage,
  MetadataChapters,
  MetadataChaptersParams,
  MetadataFileParseParams,
  MetadataFileRecord,
  MetadataFileRenderParams,
  MetadataLookupParams,
  MetadataRecord,
  MetadataSearchParams,
  NetworkGrant,
  AiModelList,
  AiRunOk,
  AiRunParams,
  NotificationField,
  NotifyRaised,
  NotifySendParams,
  NotifySent,
  NotifyValidateParams,
  NotifyValidated,
  NotifyDeliverabilityParams,
  NotifyDeliverabilityReport,
  OidcClientTemplate,
  OriginRequest,
  IngressHealth,
  IngressStarted,
  DeprovisionRequest,
  ProvisionRequest,
  Provisioned,
  PlainFetch,
  PluginCapabilities as WirePluginCapabilities,
  PluginDescribe as WirePluginDescribe,
  PluginMigration,
  PublishOk,
  PricedRecord,
  PurchaseHint,
  PurchaseHintParams,
  PurchaseHintsParams,
  QualityOffersParams,
  QualityOffersResult,
  QueueMessage as WireQueueMessage,
  QueueSendOk,
  RevokeAccountParams,
  ScanLibraryParams,
  ScanPage,
  ScanParams,
  ScheduledController,
  SocketClosed as WireSocketClosed,
  SocketMessage as WireSocketMessage,
  SocketMessageOk,
  SearchCatalogParams,
  SourceAccount,
} from "./generated.js";

export { PluginError } from "./errors.js";

// Product constants come from the generated `abi.ts` projection of
// `schema/plugin.capnp` — re-exported here for guest convenience.
export {
  FEATURE_SCALAR_LIMITS,
  FEATURE_STORAGE_CHANGES,
  FEATURE_STORAGE_CHECKED_WRITE,
  FEATURE_STORAGE_CONDITIONAL_WRITE,
  FEATURE_STORAGE_COPY,
  FEATURE_STORAGE_DELETE_MANY,
  FEATURE_STORAGE_HEAD_MANY,
  FEATURE_STORAGE_MOVE,
  FEATURE_STORAGE_MOVE_MANY,
  FEATURE_STREAMS,
  MAX_BATCH_KEYS,
  MAX_CHANGES_CURSOR_BYTES,
  MAX_CHANGES_PAGE,
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
  CoverFetchParams,
  CoverImage,
  CoverImages,
  DomainEvent,
  ExtensibleConfig,
  HealthOk,
  Invocation,
  JobInvocation,
  MetadataAudioFile,
  MetadataChapter,
  MetadataChapters,
  MetadataChaptersParams,
  MetadataField,
  MetadataFileParseParams,
  MetadataFileParsed,
  MetadataFileRecord,
  MetadataFileRenderParams,
  MetadataFileRendered,
  MetadataLookupParams,
  MetadataRecord,
  MetadataSearchParams,
  NetworkGrant,
  AiEmbedText,
  AiInput,
  AiModel,
  AiModelList,
  AiOutput,
  AiRunOk,
  AiRunParams,
  AiTask,
  AiTextEmbeddingsInput,
  AiTextEmbeddingsOutput,
  AiTextRole,
  NotificationAlertAction,
  NotificationAudience,
  NotificationField,
  NotificationMessage,
  NotificationSeverity,
  NotifyDestination,
  NotifyDestinationKind,
  NotifyRaiseParams,
  NotifyRaised,
  NotifySendParams,
  NotifySent,
  NotifyValidateParams,
  NotifyValidated,
  NotifyDeliverabilityParams,
  NotifyDeliverabilityReport,
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
 * (`apiVersion`, `id`, `capabilities`, `configOptions`, `cli`, …); an author
 * `describe()` may add or override presentation fields such as
 * `displayName`, `brand`, or `rpcFeatures`. Identity, capabilities, the
 * `[media]` declaration, and the `[[settings]]`-derived `configOptions`
 * always come from the manifest: a guest list that differs from the
 * manifest fails the spawn on the host.
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

/**
 * Object listing entry. The signature fields are set only when the listing
 * was asked for them ({@link ListOptions.withSignatures}).
 */
export interface ObjectInfo {
  key: string;
  size: number;
  /** Opaque backend entity tag. */
  etag?: string;
  /** Last modification time, UTC unix milliseconds. */
  modifiedAtMs?: number;
  /** Backend object version id (versioned buckets). */
  versionId?: string;
  /** Opaque change signature: unchanged means the object did not change. */
  signature?: string;
}

/** Metadata without a body. */
export interface ObjectMetadata {
  key: string;
  size: number;
  contentType?: string;
  etag?: string;
  sha256?: Uint8Array;
  /** Last modification time, UTC unix milliseconds. */
  modifiedAtMs?: number;
  /** Backend object version id (versioned buckets). */
  versionId?: string;
  /** Opaque change signature, as {@link ObjectInfo.signature}. */
  signature?: string;
}

/** Paginated list request. */
export interface ListOptions {
  prefix?: string;
  cursor?: string;
  limit?: number;
  /**
   * Hint: fill the signature fields of each entry. Say whether you did in
   * {@link ListPage.signaturesIncluded}.
   */
  withSignatures?: boolean;
  /**
   * Hint: only objects modified at or after this UTC unix millisecond. Say
   * whether you filtered in {@link ListPage.modifiedSinceApplied}.
   */
  modifiedSinceMs?: number;
}

/** One page of keys. */
export interface ListPage {
  objects: ObjectInfo[];
  nextCursor?: string;
  /** True when every entry carries the signature fields the backend has. */
  signaturesIncluded?: boolean;
  /** True when objects modified before `modifiedSinceMs` were left out. */
  modifiedSinceApplied?: boolean;
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
  /**
   * Conditional write (`storage.conditionalWrite`): publish only while the
   * stored object's etag equals this value, else reject with `conflict`.
   */
  ifMatch?: string;
  /**
   * Conditional write (`storage.conditionalWrite`): publish only when no
   * object exists at the key, else reject with `conflict`.
   */
  ifNoneMatch?: boolean;
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
  /** Modification time of the stored object, UTC unix milliseconds. */
  modifiedAtMs?: number;
  /** Version id of the stored object. */
  versionId?: string;
  /** Change signature of the stored object. */
  signature?: string;
}

/** Result of a server-side copy. */
export interface CopyResult {
  bytesCopied: number;
}

/** Options for {@link StorageEntrypoint.move} and {@link StorageEntrypoint.moveMany}. */
export interface MoveOptions {
  /** Reject with `conflict` instead of replacing an object at the target. */
  noOverwrite?: boolean;
  /** `move` only: move only while the source's etag equals this value. */
  ifSourceMatch?: string;
}

/** How a single move was done. */
export interface MoveResult {
  /** True when the move was atomic (a rename). */
  atomic: boolean;
  /** True when the source was already gone and the target present. */
  alreadyMoved?: boolean;
}

/** Argument of {@link StorageEntrypoint.moveMany}. */
export interface MoveManyRequest {
  /** Explicit pairs, or every object under a prefix. */
  target: MoveManyTarget;
  /** Prefix form: the previous page's `nextCursor`; absent starts. */
  cursor?: string;
  /** Prefix form: objects to move in this call (at most `MAX_BATCH_KEYS`). */
  limit?: number;
  /** Overwrite rule applied to every move. */
  options?: MoveOptions;
}

/** Outcome for one key of a batch call; `error` is absent on success. */
export interface KeyOutcome {
  key: string;
  error?: { code: string; message: string };
}

/** One page of {@link StorageEntrypoint.moveMany} progress. */
export interface MoveManyPage {
  /** Outcomes in request order (one per pair for the pairs form). */
  results: KeyOutcome[];
  /** Prefix form: cursor for the next call; absent when done. */
  nextCursor?: string;
  /** True when every move in this page was atomic. */
  atomic?: boolean;
  /** Objects moved in this page. */
  objectsMoved?: number;
}

/** Argument of {@link StorageEntrypoint.changes}. */
export interface ChangesRequest {
  /** The previous page's `cursor`; absent asks for the current position. */
  cursor?: string;
  /** Requested changes per page (at most `MAX_CHANGES_PAGE`). */
  limit?: number;
}

/** One entry of a change feed. */
export interface ObjectChange {
  key: string;
  kind: ObjectChangeKind;
  /** Listing fields after the change, when the feed carries them. */
  info?: ObjectInfo;
}

/** One page of {@link StorageEntrypoint.changes}. */
export interface ChangesPage {
  changes: ObjectChange[];
  /** Cursor for the next call; never empty. */
  cursor: string;
  /** True when more changes are ready now. */
  more?: boolean;
  /**
   * False when the feed may have missed changes (overflow, expired
   * retention); the host then backs it with a full listing.
   */
  complete: boolean;
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

  /**
   * Move one object (declare `storage.move`). Repeating a finished move
   * resolves with `alreadyMoved`.
   *
   * @param _from - Source key.
   * @param _to - Target key.
   * @param _options - Overwrite and source preconditions.
   * @returns How the move was done.
   */
  move(_from: string, _to: string, _options: MoveOptions): Promise<MoveResult> {
    return Promise.reject(unsupported("move"));
  }

  /**
   * Move a set of objects or a whole prefix, one page per call (declare
   * `storage.moveMany`). Safe to repeat.
   *
   * @param _request - Pairs or prefix, cursor, and options.
   * @returns Per-key outcomes and the continuation.
   */
  moveMany(_request: MoveManyRequest): Promise<MoveManyPage> {
    return Promise.reject(unsupported("moveMany"));
  }

  /**
   * Remove up to `MAX_BATCH_KEYS` objects (declare `storage.deleteMany`).
   *
   * @param _keys - Object keys.
   * @returns One outcome per key, in order; a missing key succeeds.
   */
  deleteMany(_keys: string[]): Promise<KeyOutcome[]> {
    return Promise.reject(unsupported("deleteMany"));
  }

  /**
   * Probe up to `MAX_BATCH_KEYS` objects (declare `storage.headMany`).
   *
   * @param _keys - Object keys.
   * @returns One entry per key, in order; `null` when missing.
   */
  headMany(_keys: string[]): Promise<(ObjectMetadata | null)[]> {
    return Promise.reject(unsupported("headMany"));
  }

  /**
   * Keys changed since a cursor (declare `storage.changes`).
   *
   * @param _request - Cursor and page size.
   * @returns One page of changes.
   */
  changes(_request: ChangesRequest): Promise<ChangesPage> {
    return Promise.reject(unsupported("changes"));
  }

  /**
   * Translate up to `MAX_BATCH_KEYS` host keys into the keys this
   * destination stores them under (declare `storage.translateKeys`). A host
   * key is `/`-joined segments, none empty, `.`, `..` or holding `\` or a
   * control character; return a relative key with the same properties. The
   * same host key must always translate to the same key. Without the
   * feature the host stores each host key as written.
   *
   * @param _keys - Host keys.
   * @returns One translated key per host key, in order.
   */
  translateKeys(_keys: string[]): Promise<string[]> {
    return Promise.reject(unsupported("translateKeys"));
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
   * The same atomic batch as {@link AdapterDatabaseSession.execute}, answered
   * as concatenated `ExecuteStreamFrame` messages counted against the
   * advertised `maxStreamedResultBytes` / `maxStreamedResultRows`; optional.
   * Adapters that advertise `0` for both leave the default, which answers
   * `unsupported`. An isolate adapter's frames travel inside one invoke
   * reply, so its streamed budget is clamped to `MAX_SCALAR_BYTES` (256 KiB)
   * and larger frames are a `limit_exceeded` (`maxScalarBytes`) error.
   * `executeAdapterBatchStreamed` runs the batch, stores the replay receipt
   * and checks that bound before it commits.
   *
   * @param _request - Proven statements plus isolation and receipt.
   * @returns Every frame of the committed reply, concatenated.
   */
  executeStreamed(_request: AdapterExecuteRequest): Promise<Uint8Array> {
    return Promise.reject(unsupported("executeStreamed"));
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
  events?: SettledTransport;
  /** `NOTIFY` binding, when `[[notifications.types]]` is declared. */
  notify?: SettledTransport;
  /** `AI` binding, when `[ai].calls` is declared and granted. */
  ai?: SettledTransport;
  /** Named `[[databases]]` bindings. */
  databases?: NamedDatabase[];
  /** `[[queues.producers]]` bindings, when granted. */
  queues?: NamedQueue[];
  /** `[[kv_namespaces]]` bindings, when granted. */
  kv?: NamedKv[];
  /** The `[sockets]` binding, when granted (at most one entry). */
  sockets?: NamedSockets[];
  /**
   * `ORIGIN`, the library a relay `ingress` plugin serves, when this
   * instance is the active ingress.
   */
  origin?: OriginTransport;
  /** The `WEBHOOKS` grant, when `[[webhooks]]` is declared. */
  webhooks?: WebhooksTransport;
  /** The network grant in effect, handed to the author as `env.NETWORK`. */
  network?: NetworkGrant;
}

/**
 * Outcome of one granted binding call as it crosses the adapter → author
 * Workers RPC hop: the method's value, or the failure's wire code, message
 * and `details` JSON text.
 *
 * Workers RPC keeps an error's own properties (such as `code`) only under the
 * enhanced error serialization of newer compatibility dates, so a binding
 * that threw across the hop would reach an author on an older date as a
 * plain `Error`. Failures therefore travel as values and the author side
 * rebuilds the {@link PluginError}.
 */
export type GrantedReply =
  { ok: unknown } | { err: { code: string; message: string; details?: string } };

/**
 * A granted binding (`EVENTS`, `NOTIFY`, `AI`, a queue producer or a KV
 * namespace) as the author isolate reaches it. Never throws: every failure
 * comes back as `err`.
 */
export interface SettledTransport {
  /**
   * Run one binding method in the adapter.
   *
   * @param method - Method name on the binding (`publish`, `get`, ...).
   * @param args - That method's arguments, structured-clonable.
   * @returns `{ ok }` with the method's value, or `{ err }`.
   */
  settle(method: string, args: unknown[]): Promise<GrantedReply>;
}

/** Methods of `EVENTS` that cross the RPC hop. */
const EVENTS_METHODS = ["publish", "read"] as const;
/** Methods of `NOTIFY` that cross the RPC hop. */
const NOTIFY_METHODS = ["raise"] as const;
/** Methods of `AI` that cross the RPC hop. */
const AI_METHODS = ["run"] as const;
/** Methods of a queue producer that cross the RPC hop. */
const QUEUE_METHODS = ["send", "sendBatch"] as const;
/** Methods of a KV namespace that cross the RPC hop. */
const KV_METHODS = ["get", "getWithMetadata", "put", "delete", "list"] as const;

/**
 * Unwraps one {@link GrantedReply}, rebuilding the typed error.
 *
 * @param reply - Reply from {@link SettledTransport.settle}.
 * @returns The method's value.
 * @throws {PluginError} The failure the adapter settled, with its code and
 *   `details`; `internal` for a reply that is neither arm.
 */
function unwrapGrantedReply(reply: GrantedReply): unknown {
  if (reply && typeof reply === "object") {
    if ("err" in reply && reply.err && typeof reply.err === "object") {
      return throwGranted(reply.err);
    }
    if ("ok" in reply) return reply.ok;
  }
  throw PluginError.fromWire("internal", "granted binding reply has neither ok nor err");
}

/**
 * Throws the {@link PluginError} an `err` arm names.
 *
 * @param err - The `err` arm.
 * @returns Never.
 */
function throwGranted(err: { code?: unknown; message?: unknown; details?: unknown }): never {
  throw PluginError.fromWire(
    typeof err.code === "string" && err.code ? err.code : "internal",
    typeof err.message === "string" ? err.message : "",
    typeof err.details === "string" ? err.details : "",
  );
}

/**
 * Rebuilds the author-facing binding over a {@link SettledTransport}: each
 * listed method calls `settle` and throws the rebuilt {@link PluginError} on
 * `err`, so the code survives whatever the author's compatibility date.
 *
 * @param transport - Transport stub from the granted context.
 * @param methods - Method names the binding exposes.
 * @returns The binding installed on `env`.
 */
function settledBinding<T>(transport: SettledTransport, methods: readonly string[]): T {
  const binding: Record<string, (...args: unknown[]) => Promise<unknown>> = {};
  for (const method of methods) {
    // Always `transport.settle(...)`: a detached property of a Workers RPC
    // stub is itself a stub (see `transportDatabase`).
    binding[method] = async (...args: unknown[]) =>
      unwrapGrantedReply(await transport.settle(method, args));
  }
  return Object.freeze(binding) as T;
}

/** The granted `[sockets]` binding on the granted context. */
export interface NamedSockets {
  /** Binding name (`env.<name>`, `SOCKETS` unless renamed). */
  name: string;
  /** The adapter's grant (crosses the adapter → author RPC boundary). */
  sockets: SocketsTransport;
}

/** One granted `[[kv_namespaces]]` binding on the granted context. */
export interface NamedKv {
  /** Binding name (`env.<name>`). */
  name: string;
  /** The adapter's namespace grant (crosses the adapter → author RPC boundary). */
  kv: SettledTransport;
}

/** One granted `[[queues.producers]]` binding on the granted context. */
export interface NamedQueue {
  /** Binding name (`env.<name>`). */
  name: string;
  /** The adapter's producer grant (crosses the adapter → author RPC boundary). */
  queue: SettledTransport;
}

/**
 * Granted-channel SQL transport the adapter hands to the author isolate for
 * one named `[[databases]]` binding. The author-side SDK wraps it as the
 * D1-shaped {@link DatabaseBinding} on `env.<BINDING>`; Python authors call
 * {@link GrantedDatabaseTransport.executeBytes} with Cap'n bytes instead.
 *
 * The `*Bytes` methods report every failure inside the reply (the
 * `ExecuteResultReply` or `DbCapabilitiesReply` `err` arm, or one `err`
 * stream frame) rather than by throwing, so the error keeps its code and
 * `details` across the Workers RPC boundary between the two isolates; both
 * author SDKs read through them.
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
  /**
   * Same batch as {@link AtomicTransport.executeStreamed}: the reply as
   * `ExecuteStreamFrame` bytes (`POST /db/execute-streamed`), read under the
   * session's streamed budget. A transport that predates the streamed read
   * leaves it out; the binding then reads inline.
   *
   * @param request - Structured `ExecuteRequest`.
   * @returns The frame sequence as the channel delivers it.
   */
  executeStreamed?(request: ExecuteRequest): Promise<ReadableStream<Uint8Array> | Uint8Array>;
  /**
   * Same frames as {@link GrantedDatabaseTransport.executeStreamed}, whole,
   * from Cap'n request bytes (Python authors decode locally). Optional like
   * {@link GrantedDatabaseTransport.executeStreamed}.
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns Every `ExecuteStreamFrame`, concatenated.
   */
  executeStreamedBytes?(bytes: Uint8Array): Promise<Uint8Array>;
  /**
   * Same frames as {@link GrantedDatabaseTransport.executeStreamedBytes},
   * as the channel delivers them: a `ReadableStream` when it streams, so an
   * author's `iterate()` can decode frames as they arrive instead of holding
   * the reply whole. Optional like
   * {@link GrantedDatabaseTransport.executeStreamed}.
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns The frame sequence, streamed when the channel streams it.
   */
  openStreamedBytes?(bytes: Uint8Array): Promise<ReadableStream<Uint8Array> | Uint8Array>;
  /**
   * Negotiated limits of the granted session (`GuestDatabase.limits`).
   *
   * @returns The adapter's `DbCapabilities` for this binding.
   */
  limits(): Promise<DbCapabilities>;
  /**
   * Same answer as {@link GrantedDatabaseTransport.limits}, as Cap'n Proto
   * bytes (`DbCapabilitiesReply`) for Python authors.
   *
   * @returns Unpacked `DbCapabilitiesReply` message.
   */
  limitsBytes(): Promise<Uint8Array>;
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
export interface BridgeContext extends Omit<
  GrantedContext,
  "databases" | "queues" | "kv" | "sockets" | "origin"
> {
  /** Granted-channel bearer for `env.EVENTS.publish`; stripped before dispatch. */
  eventsToken?: string;
  /** Granted-channel bearer for `env.NOTIFY.raise`; stripped before dispatch. */
  notifyToken?: string;
  /** Granted-channel bearer for `env.AI.run`; stripped before dispatch. */
  aiToken?: string;
  /** Named `[[databases]]` binding → database-only bearer for `/db/execute`. */
  databases?: Record<string, string>;
  /** `[[queues.producers]]` binding → queue-only bearer for `/queues/send`. */
  queues?: Record<string, string>;
  /** `[[kv_namespaces]]` binding → kv-only bearer for `/kv/*`. */
  kv?: Record<string, string>;
  /** `[sockets]` binding → sockets-only bearer for `/sockets/*`. */
  sockets?: Record<string, string>;
  /** Origin-only bearer for `/origin/fetch`; stripped before dispatch. */
  originToken?: string;
  /** Webhooks-only bearer for `/webhooks/*`; stripped before dispatch. */
  webhooksToken?: string;
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
    if (context.events) {
      merged.EVENTS = settledBinding<EventPublisherBinding>(context.events, EVENTS_METHODS);
    }
    if (context.notify) {
      merged.NOTIFY = settledBinding<NotifyBinding>(context.notify, NOTIFY_METHODS);
    }
    if (context.ai) merged.AI = settledBinding<AiBinding>(context.ai, AI_METHODS);
    if (Array.isArray(context.databases)) {
      for (const entry of context.databases) {
        if (!entry || typeof entry.name !== "string" || !entry.name) continue;
        if (entry.database) {
          merged[entry.name] = entry.database;
        } else if (entry.transport) {
          merged[entry.name] = transportDatabase(entry.transport);
        }
      }
    }
    if (Array.isArray(context.queues)) {
      for (const entry of context.queues) {
        if (!entry || typeof entry.name !== "string" || !entry.name || !entry.queue) continue;
        merged[entry.name] = settledBinding<QueueBinding>(entry.queue, QUEUE_METHODS);
      }
    }
    if (Array.isArray(context.kv)) {
      for (const entry of context.kv) {
        if (!entry || typeof entry.name !== "string" || !entry.name || !entry.kv) continue;
        merged[entry.name] = settledBinding<KvNamespaceBinding>(entry.kv, KV_METHODS);
      }
    }
    const sockets = socketsTransportOf(context);
    if (sockets) merged[sockets.name] = socketsBinding(sockets.sockets);
    if (context.origin) merged.ORIGIN = originBinding(context.origin, sockets?.sockets);
    if (context.webhooks) merged.WEBHOOKS = webhooksBinding(context.webhooks);
    if (context.network && typeof context.network === "object") {
      merged.NETWORK = Object.freeze(normalizeNetworkGrant(context.network));
    }
  }
  return Object.freeze(merged) as BookclerkEnv;
}

/**
 * Fills the lists a bridge context leaves out, so `env.NETWORK` always has
 * every field.
 *
 * @param grant - The context's `network` value.
 * @returns The grant with `domains`, `tcp` and `settings` arrays.
 */
function normalizeNetworkGrant(grant: Partial<NetworkGrant>): NetworkGrant {
  return {
    outbound: grant.outbound === true,
    domains: Array.isArray(grant.domains) ? grant.domains : [],
    tcp: Array.isArray(grant.tcp) ? grant.tcp : [],
    settings: Array.isArray(grant.settings) ? grant.settings : [],
  };
}

/**
 * Wraps the adapter's granted transport as the author's D1-shaped binding.
 *
 * The transport lives in the adapter isolate, so its methods are Workers RPC
 * calls, and an error thrown across RPC loses its `PluginError` prototype,
 * code and `details`. The binding therefore reads through the `*Bytes`
 * methods, whose failures arrive inside the Cap'n reply (`err` arm) and are
 * decoded here into a typed `PluginError`; the structured methods are the
 * fallback for a transport that offers no bytes form. A transport that
 * predates `limits` or the streamed read reads inline: the binding only
 * calls what the transport offers.
 *
 * @param entry - Transport stub from the granted context.
 * @returns The binding installed on `env.<BINDING>`.
 */
function transportDatabase(entry: GrantedDatabaseTransport): DatabaseBinding {
  // Methods are always invoked on the transport itself (`transport.m(...)`),
  // never detached and re-bound: on a Workers RPC stub a detached property
  // is itself a stub, so `m.call(transport, ...)` would call a remote method
  // named `call`.
  const transport: Partial<GrantedDatabaseTransport> & Pick<GrantedDatabaseTransport, "execute"> =
    entry;
  const execute =
    typeof transport.executeBytes === "function"
      ? async (request: ExecuteRequest): Promise<ExecuteReply> =>
          decodeExecuteResultReply(
            await (transport as GrantedDatabaseTransport).executeBytes(
              encodeExecuteRequest(request),
            ),
          )
      : (request: ExecuteRequest): Promise<ExecuteReply> => transport.execute(request);
  const readLimits =
    typeof transport.limitsBytes === "function"
      ? async (): Promise<DbCapabilities> =>
          decodeDbCapabilitiesReply(await (transport as GrantedDatabaseTransport).limitsBytes())
      : typeof transport.limits === "function"
        ? (): Promise<DbCapabilities> => (transport as GrantedDatabaseTransport).limits()
        : undefined;
  const streamed =
    typeof transport.openStreamedBytes === "function"
      ? (request: ExecuteRequest): Promise<ReadableStream<Uint8Array> | Uint8Array> =>
          (transport as Required<GrantedDatabaseTransport>).openStreamedBytes(
            encodeExecuteRequest(request),
          )
      : typeof transport.executeStreamed === "function"
        ? (request: ExecuteRequest): Promise<ReadableStream<Uint8Array> | Uint8Array> =>
            (transport as Required<GrantedDatabaseTransport>).executeStreamed(request)
        : undefined;
  return createDatabaseBinding({
    execute,
    ...(readLimits ? { limits: readLimits } : {}),
    ...(streamed ? { executeStreamed: streamed } : {}),
  });
}

/**
 * The granted `[sockets]` entry of a context, if any.
 *
 * @param context - Granted context.
 * @returns The binding name and the adapter's grant, or `undefined`.
 */
function socketsTransportOf(context: GrantedContext | undefined): NamedSockets | undefined {
  const list = context && Array.isArray(context.sockets) ? context.sockets : [];
  return list.find(
    (entry) => entry && typeof entry.name === "string" && entry.name !== "" && entry.sockets,
  );
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
    purpose: inv.purpose === "provision" ? "provision" : "serve",
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
// Queue trigger: `queue(batch)` shaped like Workers `queue(batch)`
// ---------------------------------------------------------------------------

/** Options for {@link QueueMessage.retry}. */
export interface QueueRetryOptions {
  /** Seconds before redelivery; `0` applies the consumer's `retry_delay`. */
  delaySeconds?: number;
  /** Short operator-facing reason. */
  reason?: string;
}

/** Recorded per-message outcome of a `queue()` handler (bridge projection of `QueueResult`). */
export type QueueOutcome =
  { kind: "ack" } | { kind: "retry"; delaySeconds: number; reason: string };

/**
 * One delivered queue message (`QueueMessage` struct), Workers `Message`-shaped.
 *
 * Exactly one outcome is recorded per message; the first of `ack` / `retry`
 * wins and later calls are ignored. Messages left untouched are acked when
 * `queue()` resolves and retried when it throws.
 */
export class QueueMessage {
  #result: QueueOutcome | null = null;

  /** Outbox row id; stable across redeliveries. */
  readonly id: string;
  /** When the producer sent the message. */
  readonly timestamp: Date;
  /** Encoded body bytes. */
  readonly body: Uint8Array;
  /** Body encoding: `json`, `text`, or `bytes`. */
  readonly contentType: string;
  /** Delivery counter, starting at 1. */
  readonly attempts: number;
  /** Producer plugin id; empty when the host sent it. */
  readonly source: string;
  /** Raw wire envelope. */
  readonly raw: Partial<WireQueueMessage>;

  constructor(message: Partial<WireQueueMessage> | undefined) {
    const m = message && typeof message === "object" ? message : {};
    this.id = String(m.id ?? "");
    this.timestamp = new Date(Number(m.timestampUnixMs ?? 0) || 0);
    const body: unknown = m.body;
    this.body =
      body instanceof Uint8Array
        ? body
        : body instanceof ArrayBuffer
          ? new Uint8Array(body)
          : new Uint8Array();
    this.contentType = String(m.contentType ?? "") || "json";
    this.attempts = Number(m.attempts ?? 1) || 1;
    this.source = String(m.source ?? "");
    this.raw = m;
  }

  /**
   * Decode {@link QueueMessage.body} as JSON.
   *
   * @returns Parsed body (`null` for an empty body).
   */
  json<T = unknown>(): T {
    if (this.body.byteLength === 0) return null as T;
    return JSON.parse(new TextDecoder().decode(this.body)) as T;
  }

  /**
   * Decode {@link QueueMessage.body} as UTF-8 text.
   *
   * @returns The body text.
   */
  text(): string {
    return new TextDecoder().decode(this.body);
  }

  /**
   * Recorded outcome.
   *
   * @returns The outcome, or `null` while undecided.
   */
  get result(): QueueOutcome | null {
    return this.#result;
  }

  #record(result: QueueOutcome): void {
    if (this.#result === null) this.#result = Object.freeze(result);
  }

  /** Mark handled; the host marks the message delivered. */
  ack(): void {
    this.#record({ kind: "ack" });
  }

  /**
   * Redeliver later: after `delaySeconds`, or — when omitted — after the
   * consumer's `retry_delay`.
   *
   * @param options - Redelivery timing and reason.
   */
  retry(options?: QueueRetryOptions): void {
    this.#record({
      kind: "retry",
      delaySeconds: Math.max(0, Math.floor(Number(options?.delaySeconds ?? 0) || 0)),
      reason: String(options?.reason ?? ""),
    });
  }
}

/** One `queue(batch)` delivery (`QueueBatch` struct), Workers `MessageBatch`-shaped. */
export class QueueBatch {
  /** Queue the messages were sent to. */
  readonly queue: string;
  /** Messages in delivery order. */
  readonly messages: readonly QueueMessage[];
  /** Invocation identity of this delivery. */
  invocation: Invocation;

  constructor(
    queue: string | undefined,
    messages: ReadonlyArray<Partial<WireQueueMessage>> | undefined,
    invocation?: Invocation,
  ) {
    this.queue = String(queue ?? "");
    this.messages = Object.freeze(
      (Array.isArray(messages) ? messages : []).map((message) => new QueueMessage(message)),
    );
    this.invocation = invocation ?? invocationOf(undefined);
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
  retryAll(options?: QueueRetryOptions): void {
    for (const m of this.messages) m.retry(options);
  }
}

/**
 * Translate recorded {@link QueueMessage} outcomes into the wire list.
 * `failed` is the error thrown by the author's `queue()`; undecided messages
 * then retry instead of ack.
 *
 * @param batch - Delivered batch after the handler ran.
 * @param failed - Error thrown by the handler, if any.
 * @returns One outcome per message, in order.
 */
export function queueBatchResults(batch: QueueBatch, failed: unknown): QueueOutcome[] {
  return batch.messages.map((m) => {
    if (m.result) return m.result;
    if (failed !== undefined) {
      return { kind: "retry", delaySeconds: 0, reason: errorMessage(failed) };
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
      JobInvocation & {
        invocationSequence: number;
        stepId: string;
        deadlineUnixMs: number;
        causationId: string;
        checkpointJson: string;
      }
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

/** Wire `QueueBatch` as the adapter delivers it to `bookclerkQueue`. */
export interface WireQueueBatch {
  queue: string;
  messages: Array<Partial<WireQueueMessage>>;
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
export class BookclerkEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends WorkerEntrypoint<Env> {
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
   * `[[queues.consumers]]` trigger: handle one delivered batch from one
   * queue. Record an outcome per message; untouched messages ack on return
   * and retry on throw.
   */
  queue?(batch: QueueBatch): Promise<void> | void;

  /**
   * `[triggers] crons` trigger, the Workers `scheduled(controller)`: run one
   * cron fire. Return to complete; throw to have the host retry the same
   * fire (same `scheduledTime` and `fireId`) with backoff. Fires are
   * delivered at least once, so dedupe on `controller.fireId`.
   */
  scheduled?(controller: Readonly<ScheduledController>): Promise<void> | void;

  /**
   * `[triggers] jobs` trigger: run one durable job. Return to complete,
   * throw to reject, or call `job.suspend()` / `job.retryLater()`.
   */
  job?(job: JobController): Promise<JobCompletion | void> | JobCompletion | void;

  /**
   * `[sockets]` handler, the Workers `webSocketMessage(ws, message)`: one
   * message arrived on a host-brokered socket while the plugin may have been
   * asleep. `ws` carries the socket's id and its attachment as it was when
   * the message arrived. Throwing reports the failure to the host; the
   * socket stays open. Return `{ pollEvents: true }` when the message
   * announced something `pollEvents` hands over (a new user), and the host
   * polls now instead of at its next scheduled poll.
   */
  webSocketMessage?(
    ws: HostWebSocket,
    message: SocketMessageData,
  ): Promise<Partial<SocketMessageOk> | void> | Partial<SocketMessageOk> | void;

  /**
   * `[sockets]` handler, the Workers `webSocketClose(ws, code, reason,
   * wasClean)`: a host-brokered socket closed, by the server, the network,
   * or the host (1006 when the connection dropped or the host stopped the
   * socket's broker). The socket's id is never reused.
   */
  webSocketClose?(
    ws: HostWebSocket,
    code: number,
    reason: string,
    wasClean: boolean,
  ): Promise<void> | void;

  /**
   * `[[webhooks]]` handler: the host stored requests for one webhook. Pull
   * them with `env.WEBHOOKS.pull(event.name)`. The host coalesces calls, so
   * one call may stand for several requests; read until a pull comes back
   * empty. Without this handler the plugin reads requests on its own
   * schedule.
   */
  webhookDelivered?(event: WebhookDeliveredEvent): Promise<void> | void;

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
  async bookclerkEvent(
    context: GrantedContext,
    wireBatch: WireEventBatch,
  ): Promise<EventOutcome[]> {
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
   * Adapter dispatch for the `queue(batch)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param wireBatch - Wire `QueueBatch`.
   * @returns One outcome per message, in order.
   * @internal
   */
  async bookclerkQueue(
    context: GrantedContext,
    wireBatch: WireQueueBatch,
  ): Promise<QueueOutcome[]> {
    if (typeof this.queue !== "function") {
      throw unsupported("queue");
    }
    applyInvocationEnv(this, context);
    const messages = Array.isArray(wireBatch?.messages) ? wireBatch.messages : [];
    const batch = new QueueBatch(wireBatch?.queue, messages, invocationOf(context));
    try {
      await this.queue(batch);
      return queueBatchResults(batch, undefined);
    } catch (err) {
      return queueBatchResults(batch, err ?? new Error("queue handler failed"));
    }
  }

  /**
   * Adapter dispatch for the `scheduled(controller)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `ScheduledController`.
   * @returns Resolves when the author's handler completes.
   * @internal
   */
  async bookclerkScheduled(
    context: GrantedContext,
    wire: Partial<ScheduledController>,
  ): Promise<void> {
    if (typeof this.scheduled !== "function") {
      throw unsupported("scheduled");
    }
    applyInvocationEnv(this, context);
    await this.scheduled(scheduledControllerOf(wire));
  }

  /**
   * Adapter dispatch for `SocketHandler.message`.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `SocketMessage`.
   * @returns What the handler asks of the host (`pollEvents`).
   * @internal
   */
  async bookclerkSocketMessage(
    context: GrantedContext,
    wire: Partial<WireSocketMessage>,
  ): Promise<SocketMessageOk> {
    if (typeof this.webSocketMessage !== "function") {
      throw unsupported("webSocketMessage");
    }
    applyInvocationEnv(this, context);
    const ws = hostWebSocketOf(socketsTransportOf(context)?.sockets, {
      socket: wire?.socket,
      attachment: wire?.attachment,
    });
    const asked = await this.webSocketMessage(ws, socketMessageData(wire?.frame));
    return { pollEvents: asked?.pollEvents === true };
  }

  /**
   * Adapter dispatch for `SocketHandler.closed`. Without a
   * `webSocketClose` handler the close is acknowledged and ignored.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `SocketClosed`.
   * @returns Resolves when the author's handler completes.
   * @internal
   */
  async bookclerkSocketClosed(
    context: GrantedContext,
    wire: Partial<WireSocketClosed>,
  ): Promise<void> {
    if (typeof this.webSocketClose !== "function") return;
    applyInvocationEnv(this, context);
    const ws = hostWebSocketOf(socketsTransportOf(context)?.sockets, {
      socket: wire?.socket,
      attachment: wire?.attachment,
    });
    await this.webSocketClose(
      ws,
      Number(wire?.code ?? 0) || 0,
      String(wire?.reason ?? ""),
      wire?.clean === true,
    );
  }

  /**
   * Adapter dispatch for `WebhookHandler.delivered`. Without a
   * `webhookDelivered` handler the call is acknowledged and ignored.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `WebhookDelivered`.
   * @returns Resolves when the author's handler completes.
   * @internal
   */
  async bookclerkWebhookDelivered(
    context: GrantedContext,
    wire: Partial<WebhookDeliveredEvent>,
  ): Promise<void> {
    if (typeof this.webhookDelivered !== "function") return;
    applyInvocationEnv(this, context);
    await this.webhookDelivered(
      Object.freeze({
        name: String(wire?.name ?? ""),
        waiting: Number(wire?.waiting ?? 0) || 0,
      }),
    );
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
export class NamedEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends WorkerEntrypoint<Env> {
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
  async bookclerkInvoke(
    context: GrantedContext,
    method: string,
    ...args: unknown[]
  ): Promise<unknown> {
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

/**
 * How long the default `purchaseHints` keeps starting one-title lookups
 * before answering with what it has, in milliseconds. Well under the
 * host's 10 s batch deadline, so even a slow last lookup lands before it
 * and a store without a bulk endpoint still returns the titles it priced.
 */
export const PURCHASE_HINTS_BUDGET_MS = 4_000;

/**
 * How many one-title lookups the default `purchaseHints` keeps in flight
 * at once. A store without a bulk endpoint answers a page of cards in
 * about a quarter of the time one lookup after another takes, while
 * staying a polite handful of requests rather than a burst.
 */
export const PURCHASE_HINTS_CONCURRENCY = 4;

/**
 * A purchase-hint answer that carries the priced listings a title search
 * matched, for a host that asked with `lookup = "title"`.
 *
 * Return it from {@link StorefrontEntrypoint.purchaseHint} (or as an entry
 * of {@link StorefrontEntrypoint.purchaseHints}) instead of a bare
 * {@link PurchaseHint} when the store's search returned more than one
 * listing: the host places each record in its library model by
 * identifiers, credits and kinds, and keeps only the recording it asked
 * for. `hint` stays the store's own pick for hosts that ignore records.
 */
export interface PurchaseHintAnswer {
  /** The store's own pick, or `null` when it found nothing. */
  hint: PurchaseHint | null;
  /** Every priced listing the search matched, with its full catalog record. */
  records: PricedRecord[];
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
  "purchaseHints",
  "listDeals",
  "catalogDetail",
  "revokeAccount",
  "qualityOffers",
  "diagnose",
  "health",
]);

/**
 * `storefront` entrypoint (`ContentSource` interface). Export it as
 * `export class Storefront extends StorefrontEntrypoint`. Every method takes
 * and returns the typed ABI structs generated from `schema/plugin.capnp`.
 */
export class StorefrontEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
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
   * One page of one account's library scan.
   *
   * The host calls this repeatedly, passing back `nextCursor` as
   * `params.cursor` until a page returns an empty `nextCursor`, which ends
   * the pass. An empty `params.syncToken` asks for a full pass; a non-empty
   * one asks only for changes since that token (list gone titles in
   * `removed`). Set `complete` only on the last page of a full pass that
   * listed the whole library. Only the last page's `syncToken` is stored. Reject a stale cursor or token with `invalid_cursor` and the
   * host restarts as a full pass.
   *
   * @param _params - The account, its credential, the cursor and the sync token.
   * @returns At most `MAX_SCAN_PAGE_BOOKS` books plus the paging fields.
   */
  scan(_params: ScanParams): Promise<ScanPage> {
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
   * `params.lookup` says how the host names the title: `"isbn"` or
   * `"asin"` (sent only when `describe().catalog` declares `priceByIsbn` /
   * `priceByAsin`) want that exact edition or `null`, never a title match;
   * `"product"` names the store's own product; `"title"` asks for a search
   * by title and authors, answered best with a {@link PurchaseHintAnswer}
   * listing every priced result; `"unspecified"` (older hosts) leaves the
   * choice to the store.
   *
   * @param _params - Title identifiers and how to look the title up.
   * @returns Hint, an answer with title-search records, or `null`.
   */
  purchaseHint(_params: PurchaseHintParams): Promise<PurchaseHint | PurchaseHintAnswer | null> {
    return Promise.reject(unsupported("purchaseHint"));
  }
  /**
   * Purchase links / price hints for several titles in one call, one
   * answer per query in order. The default asks {@link purchaseHint} for
   * each query, {@link PURCHASE_HINTS_CONCURRENCY} at a time, those with a
   * product id first, and stops starting lookups after
   * {@link PURCHASE_HINTS_BUDGET_MS}; a store whose catalog prices
   * many products per request overrides this to make one request. Reject with a
   * `rate_limited` {@link PluginError} (its `details.retryAfterMs` says how
   * long) when the store asks to slow down, so the host leaves it alone.
   *
   * @param params - The titles to price, in order.
   * @returns One hint, answer with records, or `null` per query.
   */
  async purchaseHints(
    params: PurchaseHintsParams,
  ): Promise<Array<PurchaseHint | PurchaseHintAnswer | null>> {
    const started = Date.now();
    const out: Array<PurchaseHint | PurchaseHintAnswer | null> = params.queries.map(() => null);
    // Queries that already name a product id are one lookup each; title
    // searches come after, and nothing starts past the budget.
    const order = params.queries
      .map((query, i) => ({ i, direct: Boolean(query.productId?.trim()) }))
      .sort((a, b) => Number(b.direct) - Number(a.direct))
      .map((entry) => entry.i);
    // A few lanes take the next query in order; the first failure (a rate
    // limit included) rejects the batch.
    let next = 0;
    // Set by the first failure, so the other lanes take on no new query
    // (a rate limit must not be followed by more requests).
    let failed = false;
    const lane = async (): Promise<void> => {
      while (next < order.length && !failed) {
        if (Date.now() - started >= PURCHASE_HINTS_BUDGET_MS) return;
        const i = order[next++];
        try {
          out[i] = await this.purchaseHint(params.queries[i]);
        } catch (err) {
          failed = true;
          throw err;
        }
      }
    };
    const lanes = Math.min(PURCHASE_HINTS_CONCURRENCY, order.length);
    await Promise.all(Array.from({ length: lanes }, lane));
    return out;
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
   * Undo store-side state for one account before the host deletes its
   * credentials, such as deregistering the device registered at login.
   * Best effort: the host revokes the account whatever this returns. The
   * default does nothing.
   *
   * @param _params - Account id and its stored credentials.
   * @returns Resolves when the store sign-out finished.
   */
  revokeAccount(_params: RevokeAccountParams): Promise<void> {
    return Promise.resolve();
  }
  /**
   * The audio quality one account would download for each title today, one
   * answer per title id in order (`found: false` when the store cannot
   * say). It must not license or download anything. The host asks only a
   * store that declares `catalog.qualityOffers` in `describe()`; the
   * default rejects as unsupported.
   *
   * @param _params - Account, its credentials, settings and title ids.
   * @returns One answer per title, plus any refreshed credentials.
   */
  qualityOffers(_params: QualityOffersParams): Promise<QualityOffersResult> {
    return Promise.reject(unsupported("qualityOffers"));
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
  "move",
  "moveMany",
  "deleteMany",
  "headMany",
  "changes",
  "translateKeys",
]);

/**
 * `storage` entrypoint (`Destination` interface): an object store the host
 * writes acquired media into. Export it as
 * `export class Storage extends StorageEntrypoint`.
 */
export class StorageEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
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

  /**
   * Move one object (declare `storage.move`). Repeating a finished move
   * resolves with `alreadyMoved`.
   *
   * @param _from - Source key.
   * @param _to - Target key.
   * @param _options - Overwrite and source preconditions.
   * @returns How the move was done.
   */
  move(_from: string, _to: string, _options: MoveOptions): Promise<MoveResult> {
    return Promise.reject(unsupported("move"));
  }

  /**
   * Move a set of objects or a whole prefix, one page per call (declare
   * `storage.moveMany`). Safe to repeat.
   *
   * @param _request - Pairs or prefix, cursor, and options.
   * @returns Per-key outcomes and the continuation.
   */
  moveMany(_request: MoveManyRequest): Promise<MoveManyPage> {
    return Promise.reject(unsupported("moveMany"));
  }

  /**
   * Remove up to `MAX_BATCH_KEYS` objects (declare `storage.deleteMany`).
   *
   * @param _keys - Object keys.
   * @returns One outcome per key, in order; a missing key succeeds.
   */
  deleteMany(_keys: string[]): Promise<KeyOutcome[]> {
    return Promise.reject(unsupported("deleteMany"));
  }

  /**
   * Probe up to `MAX_BATCH_KEYS` objects (declare `storage.headMany`).
   *
   * @param _keys - Object keys.
   * @returns One entry per key, in order; `null` when missing.
   */
  headMany(_keys: string[]): Promise<(ObjectMetadata | null)[]> {
    return Promise.reject(unsupported("headMany"));
  }

  /**
   * Keys changed since a cursor (declare `storage.changes`).
   *
   * @param _request - Cursor and page size.
   * @returns One page of changes.
   */
  changes(_request: ChangesRequest): Promise<ChangesPage> {
    return Promise.reject(unsupported("changes"));
  }

  /**
   * Translate up to `MAX_BATCH_KEYS` host keys into the keys this
   * destination stores them under (declare `storage.translateKeys`). A host
   * key is `/`-joined segments, none empty, `.`, `..` or holding `\` or a
   * control character; return a relative key with the same properties. The
   * same host key must always translate to the same key. Without the
   * feature the host stores each host key as written.
   *
   * @param _keys - Host keys.
   * @returns One translated key per host key, in order.
   */
  translateKeys(_keys: string[]): Promise<string[]> {
    return Promise.reject(unsupported("translateKeys"));
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
export class RemoteLibraryEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
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
  "executeStreamed",
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
export class DatabaseAdapterEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["openSession", "dropUnit"]);

  /**
   * Opens a session. Sessions cannot survive suspension.
   *
   * @returns Adapter session capability.
   */
  openSession(): Promise<AdapterDatabaseSession> {
    return Promise.reject(unsupported("openSession"));
  }

  /**
   * Physically drops one provisioned binding unit.
   *
   * @param _unitRef - Adapter-native unit identity.
   * @returns Resolves when the unit is dropped.
   */
  dropUnit(_unitRef: string): Promise<void> {
    return Promise.reject(unsupported("dropUnit"));
  }

  /**
   * Adapter dispatch: `openSession` → session id; `dropUnit`; `session` → call
   * on a retained session (`args` = `[id, method, ...params]`).
   *
   * @param context - Granted bindings for this invocation.
   * @param method - `openSession`, `dropUnit`, or `session`.
   * @param args - Method arguments.
   * @returns Session id for `openSession`, else the session method result.
   * @internal
   */
  override async bookclerkInvoke(
    context: GrantedContext,
    method: string,
    ...args: unknown[]
  ): Promise<unknown> {
    if (method === "openSession") {
      const session = (await super.bookclerkInvoke(
        context,
        "openSession",
      )) as AdapterDatabaseSession;
      if (!session || typeof session !== "object") {
        throw PluginError.fromWire("internal", "openSession returned no session");
      }
      const id = freshSessionId();
      ADAPTER_SESSIONS.set(id, session);
      return id;
    }
    if (method === "dropUnit") {
      applyInvocationEnv(this, context);
      this.invocation = invocationOf(context);
      await this.dropUnit(String(args[0] ?? ""));
      return undefined;
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

/**
 * `metadata` entrypoint (`MetadataProvider`): bibliographic lookups against a
 * public catalog. Records describe editions; the host decides identity.
 * Export it as `export class Metadata extends MetadataEntrypoint`.
 */
export class MetadataEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["health", "lookup", "search", "chapters"]);

  /**
   * Reports whether the provider can answer.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Records for the editions the identifiers name.
   *
   * @param _params - Identifiers, marketplace and result cap.
   * @returns Records in relevance order (`[]` when nothing matched).
   */
  lookup(_params: MetadataLookupParams): Promise<MetadataRecord[]> {
    return Promise.reject(unsupported("lookup"));
  }
  /**
   * Records matching a title and, optionally, an author.
   *
   * @param _params - Title, author, language, marketplace and result cap.
   * @returns Records in relevance order (`[]` when nothing matched).
   */
  search(_params: MetadataSearchParams): Promise<MetadataRecord[]> {
    return Promise.reject(unsupported("search"));
  }
  /**
   * Chapter layout of one edition.
   *
   * @param _params - Identifiers of the edition.
   * @returns The layout, or `{ found: false }` when the provider has none.
   */
  chapters(_params: MetadataChaptersParams): Promise<MetadataChapters> {
    return Promise.reject(unsupported("chapters"));
  }
}

/**
 * `covers` entrypoint (`CoverFetcher`): downloads cover images for the host.
 * The host passes a `sink` {@link Destination}; put the body for `urls[i]`
 * under the key `String(i)` and return one {@link CoverImage} per URL, in
 * request order. The host sniffs, decodes and re-encodes every body, so the
 * bytes stay untrusted. Export it as `export class Covers extends CoverEntrypoint`.
 */
export class CoverEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["health", "fetchCovers"]);

  /**
   * Reports whether the fetcher can answer.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Fetches each URL and streams its body into `sink` under its index.
   *
   * @param _params - URLs (absolute `https://`) and the per-body byte cap.
   * @param _sink - Host destination; accepts `put` for keys `0..urls.length`.
   * @returns One entry per URL, in request order.
   */
  fetchCovers(_params: CoverFetchParams, _sink: Destination): Promise<CoverImage[]> {
    return Promise.reject(unsupported("fetchCovers"));
  }
}

/**
 * `metadataFiles` entrypoint (`MetadataFiles`): reads and writes metadata
 * files (sidecars such as `metadata.json`, OPF, NFO or CUE) for the host. The
 * host reads every file, chooses every path and writes every result, so the
 * entrypoint only converts bytes. Each dialect is a
 * `[[metadata_files.dialects]]` row in `plugin.toml`; `params.dialect` names
 * the row. Export it as `export class MetadataFiles extends MetadataFilesEntrypoint`.
 */
export class MetadataFilesEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["health", "parse", "render"]);

  /**
   * Reports whether the dialects can answer.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * What one file says about its edition. List every field the file states
   * in `present`, including ones it states as empty; leave out fields it
   * does not mention. Throw `invalid_params` for a file the dialect cannot
   * read.
   *
   * @param _params - Dialect id, file name and the whole file.
   * @returns The parsed record.
   */
  parse(_params: MetadataFileParseParams): Promise<MetadataFileRecord> {
    return Promise.reject(unsupported("parse"));
  }
  /**
   * The new file: `params.existing` with each field in `params.fields` set to
   * the record's value (removed when the value is empty), everything else
   * kept as it was. `existing` is empty when the host creates the file.
   *
   * @param _params - Dialect id, values, fields to set and the current file.
   * @returns The new file's bytes; a string is written as UTF-8.
   */
  render(_params: MetadataFileRenderParams): Promise<Uint8Array | string> {
    return Promise.reject(unsupported("render"));
  }
}

/**
 * `ai` entrypoint (`AiModels`): serves models to the host and, through the
 * `AI` binding, to every plugin with the grant. `[ai].serves` in
 * `plugin.toml` declares the tasks. The host checks every listing and every
 * reply, so a wrong vector count or length fails the run. Export it as
 * `export class Ai extends AiEntrypoint`.
 */
export class AiEntrypoint<Env extends BookclerkEnv = BookclerkEnv> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["health", "models", "run"]);

  /**
   * Reports whether the models can be loaded with the current settings.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Lists the models served now. Must be cheap: do not load a model here.
   * Change a model's `revision` whenever its weights, tokenizer or loaded
   * language packs change, because outputs are comparable only within one
   * revision.
   *
   * @returns The served models, each `modelId` once.
   */
  models(): Promise<AiModelList> {
    return Promise.resolve({ models: [] });
  }
  /**
   * Runs one model on one input. Throw `not_found` for a model this plugin
   * does not serve and `invalid_params` for an input of another task. For
   * text embeddings return one vector per text, in input order
   * ({@link packEmbeddingVectors}), with the model's listed `dimensions` and
   * `revision`.
   *
   * @param _params - The plugin's own model id and the input.
   * @returns The output; leave `model` unset.
   */
  run(_params: AiRunParams): Promise<AiRunOk> {
    return Promise.reject(unsupported("run"));
  }
}

/**
 * `provisioning` entrypoint (`Provisioning`): guided setup at a cloud
 * provider. The host collects a short-lived grant from the operator (a pasted
 * API token), hands it to `provision`, and stores what it returns: settings
 * listed in `[provisioning] outputs` and credentials it seals. The grant is
 * dropped after the call, so return any lasting credential you create. On a
 * re-run `env.CONFIG` and `env.SECRETS` hold the earlier outputs: update or
 * rotate rather than create again. Export it as
 * `export class Provisioning extends ProvisioningEntrypoint`.
 */
export class ProvisioningEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["provision", "deprovision"]);

  /**
   * Creates (or updates) what the instance needs.
   *
   * @param request - The operator's grant and choices (`inputsJson`, a JSON
   *   object keyed by `[provisioning] inputs`).
   * @returns Settings to store (`settingsJson`), credentials to seal and
   *   summary lines for the setup screen; see {@link provisioned}.
   */
  provision(request: ProvisionRequest): Promise<Provisioned> {
    void request;
    return Promise.reject(unsupported("provision"));
  }
  /**
   * Removes what `provision` created. Removing what is already gone is not
   * an error.
   *
   * @param request - The operator's grant, or kind `none` to use the stored
   *   secrets.
   * @returns Resolves once the resources are gone.
   */
  deprovision(request: DeprovisionRequest): Promise<void> {
    void request;
    return Promise.reject(unsupported("deprovision"));
  }
}

/**
 * Builds a {@link Provisioned} answer from plain objects.
 *
 * @param answer - Settings to store, credentials to seal (by `secret`
 *   setting key) and summary lines.
 * @returns The wire shape `provision` returns.
 */
export function provisioned(answer: {
  settings?: Record<string, string | number | boolean>;
  secrets?: Record<string, string>;
  summary?: string[];
}): Provisioned {
  return {
    settingsJson: JSON.stringify(answer.settings ?? {}),
    secrets: Object.entries(answer.secrets ?? {}).map(([key, value]) => ({ key, value })),
    summary: answer.summary ?? [],
  };
}

/**
 * `notifier` entrypoint (`Notifier`): delivers messages the host already
 * rendered to one outside service (a chat webhook, a mail relay, a paging
 * service). The host decides what to send and where; the entrypoint only
 * sends. `[notifier]` in `plugin.toml` declares the destination kinds,
 * formats and personal address fields. Export it as
 * `export class Notifier extends NotifierEntrypoint`.
 */
export class NotifierEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze([
    "health",
    "send",
    "validateDestination",
    "checkDeliverability",
  ]);

  /**
   * Reports whether the service can be reached with the current settings.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Delivers one message to one destination. Throw `rate_limited` (with
   * `details.retryAfterMs` when the service says when) or `unavailable` for
   * a failure worth retrying; any other error fails this destination. Never
   * log `params.message` when `sensitive` is set, nor `addressJson`.
   *
   * @param _params - The rendered message and its destination.
   * @returns The service's message id, when it gives one.
   */
  send(_params: NotifySendParams): Promise<NotifySent> {
    return Promise.reject(unsupported("send"));
  }
  /**
   * Checks and normalises a personal address a patron entered. Throw
   * `invalid_params` for an address the service would refuse. The default
   * accepts the address as given.
   *
   * @param params - The address document as entered.
   * @returns The address to save and an optional label that is never a secret.
   */
  validateDestination(params: NotifyValidateParams): Promise<NotifyValidated> {
    return Promise.resolve({ addressJson: params.addressJson });
  }
  /**
   * Runs the operator's deliverability check: sends `params.message` the
   * way real messages go, asks checker services how it scores and merges
   * their findings into advice, most urgent first. Only plugins that declare
   * `[notifier] deliverability_check = true` implement it; the default
   * throws `unsupported`.
   *
   * @param _params - The host's rendered `test` notification.
   * @returns One entry per checker and the merged advice.
   */
  checkDeliverability(_params: NotifyDeliverabilityParams): Promise<NotifyDeliverabilityReport> {
    return Promise.reject(unsupported("checkDeliverability"));
  }
}

/**
 * `ingress` entrypoint (`Ingress`): gives the library a public `https`
 * address. A relay plugin (`[ingress] kind = "relay"`) receives requests from
 * its provider and hands each one to `env.ORIGIN.fetch`; the host starts at
 * most one ingress instance at a time. Export it as
 * `export class Ingress extends IngressEntrypoint`.
 */
export class IngressEntrypoint<
  Env extends BookclerkEnv = BookclerkEnv,
> extends NamedEntrypoint<Env> {
  static override bookclerkMethods = Object.freeze(["start", "health", "stop"]);

  /**
   * Brings the public address up and returns it once it serves.
   *
   * @returns The public origin: `https://` and a host name, no port, path
   *   or trailing slash.
   */
  start(): Promise<IngressStarted> {
    return Promise.reject(unsupported("start"));
  }
  /**
   * Reports whether the address is serving.
   *
   * @returns `serving`, plus a detail for the operator when it is not.
   */
  health(): Promise<IngressHealth> {
    return Promise.resolve({ serving: true });
  }
  /**
   * Takes the public address down. The host stops sending requests after
   * this returns.
   *
   * @returns Resolves once the address no longer serves.
   */
  stop(): Promise<void> {
    return Promise.resolve();
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
  | "oidc"
  | "metadata"
  | "covers"
  | "metadataFiles"
  | "notifier"
  | "ingress"
  | "ai"
  | "provisioning";

/** Adapter binding name for each named entrypoint. */
export const ENTRYPOINT_BINDINGS: Readonly<Record<EntrypointName, string>> = Object.freeze({
  storefront: "PLUGIN_STOREFRONT",
  storage: "PLUGIN_STORAGE",
  databaseAdapter: "PLUGIN_DATABASE_ADAPTER",
  remoteLibrary: "PLUGIN_REMOTE_LIBRARY",
  cli: "PLUGIN_CLI",
  oidc: "PLUGIN_OIDC",
  metadata: "PLUGIN_METADATA",
  covers: "PLUGIN_COVERS",
  metadataFiles: "PLUGIN_METADATA_FILES",
  notifier: "PLUGIN_NOTIFIER",
  ingress: "PLUGIN_INGRESS",
  ai: "PLUGIN_AI",
  provisioning: "PLUGIN_PROVISIONING",
});

/** Exported class name the launcher binds for each named entrypoint. */
export const ENTRYPOINT_CLASSES: Readonly<Record<EntrypointName, string>> = Object.freeze({
  storefront: "Storefront",
  storage: "Storage",
  databaseAdapter: "DatabaseAdapter",
  remoteLibrary: "RemoteLibrary",
  cli: "Cli",
  oidc: "Oidc",
  metadata: "Metadata",
  covers: "Covers",
  metadataFiles: "MetadataFiles",
  notifier: "Notifier",
  ingress: "Ingress",
  ai: "Ai",
  provisioning: "Provisioning",
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
  | "bookclerkDescribe"
  | "bookclerkEvent"
  | "bookclerkQueue"
  | "bookclerkScheduled"
  | "bookclerkSocketMessage"
  | "bookclerkSocketClosed"
  | "bookclerkWebhookDelivered"
  | "bookclerkJob"
  | "bookclerkDatabaseMigrations"
  | "bookclerkShutdown"
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

  async put(
    key: string,
    body: ReadableStream<Uint8Array>,
    options?: WriteOptions,
  ): Promise<PutResult> {
    // The granted channel carries no preconditions yet; refuse a conditional
    // write rather than silently overwrite.
    if (options?.ifMatch || options?.ifNoneMatch) {
      throw PluginError.fromWire(
        "unsupported",
        "conditional writes are not supported on granted destinations",
      );
    }
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
 * Packs embedding vectors into the wire form of
 * `AiTextEmbeddingsOutput.vectors`: little-endian 32-bit floats, concatenated.
 *
 * @param vectors - One vector per input text, all of one length.
 * @returns The bytes.
 */
export function packEmbeddingVectors(vectors: readonly ArrayLike<number>[]): Uint8Array {
  const width = vectors.length > 0 ? vectors[0].length : 0;
  const out = new Uint8Array(vectors.length * width * 4);
  const view = new DataView(out.buffer);
  vectors.forEach((vector, row) => {
    if (vector.length !== width) {
      throw PluginError.fromWire("internal", "embedding vectors differ in length");
    }
    for (let i = 0; i < width; i++) {
      view.setFloat32((row * width + i) * 4, vector[i], true);
    }
  });
  return out;
}

/**
 * Splits the wire form of `AiTextEmbeddingsOutput.vectors` into vectors.
 *
 * @param bytes - Little-endian 32-bit floats, concatenated.
 * @param dimensions - Numbers per vector.
 * @returns One vector per `dimensions` numbers.
 * @throws {PluginError} `internal` when the bytes are not whole vectors.
 */
export function unpackEmbeddingVectors(bytes: Uint8Array, dimensions: number): number[][] {
  const width = dimensions * 4;
  if (!Number.isInteger(dimensions) || dimensions < 1 || bytes.length % width !== 0) {
    throw PluginError.fromWire("internal", "embedding bytes are not whole vectors");
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const out: number[][] = [];
  for (let at = 0; at < bytes.length; at += width) {
    const vector = new Array<number>(dimensions);
    for (let i = 0; i < dimensions; i++) vector[i] = view.getFloat32(at + i * 4, true);
    out.push(vector);
  }
  return out;
}

/** `error` object of a granted-channel reply. */
interface GrantedErrorBody {
  /** Wire code (`forbidden`, `invalid_cursor`, ...). */
  code?: string;
  /** Operator-facing text. */
  message?: string;
  /** Structured detail object as JSON text, when the host sent one. */
  details?: string;
}

/**
 * The {@link PluginError} a granted-channel `error` object names.
 *
 * @param error - The reply's `error` object.
 * @returns Typed error carrying the code, message and `details`.
 */
function grantedError(error: GrantedErrorBody): PluginError {
  return PluginError.fromWire(
    error.code || "internal",
    error.message || "",
    typeof error.details === "string" ? error.details : "",
  );
}

/**
 * Adapter-side {@link SettledTransport}: runs one allow-listed method of a
 * granted binding and settles its outcome into a {@link GrantedReply}, so no
 * failure is thrown across the Workers RPC hop to the author isolate.
 */
class SettledTarget extends RpcTarget implements SettledTransport {
  /** Binding the methods run on. */
  #inner: object;
  /** Method names `settle` may run. */
  #methods: ReadonlySet<string>;

  /**
   * @param inner - Granted binding implementation.
   * @param methods - Method names the author may call.
   */
  constructor(inner: object, methods: readonly string[]) {
    super();
    this.#inner = inner;
    this.#methods = new Set(methods);
  }

  /**
   * Run one binding method.
   *
   * @param method - Allow-listed method name.
   * @param args - Its arguments.
   * @returns `{ ok }` with the value, or `{ err }`; never throws.
   */
  async settle(method: string, args: unknown[]): Promise<GrantedReply> {
    if (typeof method !== "string" || !this.#methods.has(method)) {
      return {
        err: { code: "unsupported", message: `binding has no method \`${String(method)}\`` },
      };
    }
    try {
      const fn = (this.#inner as Record<string, unknown>)[method] as (
        ...a: unknown[]
      ) => Promise<unknown>;
      const value = await fn.apply(this.#inner, Array.isArray(args) ? args : []);
      return { ok: value };
    } catch (err) {
      if (err instanceof PluginError) {
        const details = err.detailsText;
        return {
          err: { code: err.wireCode, message: err.message, ...(details ? { details } : {}) },
        };
      }
      return { err: { code: "internal", message: errorMessage(err) } };
    }
  }
}

/**
 * `AI` binding handed to the author: the adapter exchanges the host's ai
 * grant token on the granted channel, so the author isolate never holds the
 * bearer itself. The host checks every run, the tasks granted and the
 * plugin's budget.
 */
class GrantedAi implements AiBinding {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the ai grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Run one model on one input.
   *
   * @param model - Model name or task alias.
   * @param input - The input of the model's task.
   * @returns The model's output.
   */
  async run(model: string, input: AiTextEmbeddingsRequest): Promise<AiTextEmbeddingsResponse> {
    if (typeof model !== "string" || !model) {
      throw PluginError.fromWire("invalid_params", "run requires a model name");
    }
    const text = input?.text;
    const list = typeof text === "string" ? [text] : text;
    if (!Array.isArray(list) || list.some((t) => typeof t !== "string")) {
      throw PluginError.fromWire(
        "invalid_params",
        "run requires input.text as a string or strings",
      );
    }
    const role = input.role ?? "document";
    const wire = {
      model,
      input: { textEmbeddings: { texts: list.map((t) => ({ text: t, role })) } },
    };
    const resp = await this.#granted.fetch("http://granted/ai/run", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire),
    });
    const value = (await resp.json().catch(() => ({}))) as {
      error?: GrantedErrorBody;
      model?: unknown;
      output?: {
        textEmbeddings?: {
          dimensions?: unknown;
          vectors?: unknown;
          revision?: unknown;
          fallbacks?: unknown;
        };
      };
    };
    if (value && value.error) {
      throw grantedError(value.error);
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `ai run HTTP ${resp.status}`);
    }
    const out = value.output?.textEmbeddings ?? {};
    const dimensions = Number(out.dimensions ?? 0);
    const bytes = typeof out.vectors === "string" ? base64ToBytes(out.vectors) : new Uint8Array();
    const data = unpackEmbeddingVectors(bytes, dimensions);
    return {
      shape: [data.length, dimensions],
      data,
      model: String(value.model ?? model),
      revision: String(out.revision ?? ""),
      fallbacks: Array.isArray(out.fallbacks) ? out.fallbacks.map(Number) : [],
    };
  }
}

/**
 * Normalises {@link NotifyRaise.fields} to `{ name, value }` rows.
 *
 * @param fields - Rows or a plain `name → value` object.
 * @returns The rows, with every value stringified.
 * @throws {PluginError} `invalid_params` when `fields` is neither shape.
 */
function notifyFields(fields: NotifyRaise["fields"]): NotificationField[] {
  if (fields === undefined) return [];
  if (Array.isArray(fields)) {
    return fields.map((f) => ({ name: String(f?.name ?? ""), value: String(f?.value ?? "") }));
  }
  if (fields === null || typeof fields !== "object") {
    throw PluginError.fromWire("invalid_params", "fields must be a list or an object");
  }
  return Object.entries(fields).map(([name, value]) => ({ name, value: String(value) }));
}

/**
 * `NOTIFY` binding handed to the author: the adapter exchanges the host's
 * notify grant token on the granted channel, so the author isolate never
 * holds the bearer itself. The host checks every raise against the
 * plugin's declared types.
 */
class GrantedNotify implements NotifyBinding {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the notify grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Raise one declared notification type.
   *
   * @param raise - Type, fields, optional patron, link path and alert.
   * @returns The host's notification id.
   */
  async raise(raise: NotifyRaise): Promise<NotifyRaised> {
    if (!raise || typeof raise.notificationType !== "string" || !raise.notificationType) {
      throw PluginError.fromWire("invalid_params", "raise requires notificationType");
    }
    const wire = {
      notificationType: raise.notificationType,
      fields: notifyFields(raise.fields),
      patronId: raise.patronId || undefined,
      linkPath: raise.linkPath || undefined,
      groupKey: raise.groupKey || undefined,
      alertAction: raise.alertAction ?? "none",
      alertKey: raise.alertKey || undefined,
    };
    const resp = await this.#granted.fetch("http://granted/notify/raise", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire),
    });
    const value = (await resp.json().catch(() => ({}))) as {
      error?: GrantedErrorBody;
      notificationId?: unknown;
    };
    if (value && value.error) {
      throw grantedError(value.error);
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `notify raise HTTP ${resp.status}`);
    }
    return { notificationId: String(value.notificationId ?? "") };
  }
}

/**
 * `EVENTS` binding handed to the author: the adapter exchanges the host's
 * events grant token on the granted channel, so the author isolate never
 * holds the bearer itself.
 */
class GrantedEvents implements EventPublisherBinding {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the events grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
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
      error?: GrantedErrorBody;
      eventId?: unknown;
      duplicate?: unknown;
    };
    if (value && value.error) {
      throw grantedError(value.error);
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `events publish HTTP ${resp.status}`);
    }
    return { eventId: String(value.eventId ?? ""), duplicate: Boolean(value.duplicate) };
  }

  /**
   * Read one page of past events the granted subscriptions match.
   *
   * @param options - Start position, types, and page size.
   * @returns One page of events and the cursor after it.
   */
  async read(options: EventReadOptions = {}): Promise<EventReadPage> {
    const since =
      options.since instanceof Date
        ? options.since.getTime()
        : (options.since ?? options.sinceUnixMs);
    if (since !== undefined && (!Number.isSafeInteger(since) || since < 0)) {
      throw PluginError.fromWire("invalid_params", "since must be a non-negative instant");
    }
    const limit = options.limit ?? 0;
    if (!Number.isSafeInteger(limit) || limit < 0) {
      throw PluginError.fromWire("invalid_params", "limit must be a non-negative integer");
    }
    const resp = await this.#granted.fetch("http://granted/events/read", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify({
        after: String(options.after ?? ""),
        sinceUnixMs: since ?? 0,
        types: (options.types ?? []).map(String),
        limit,
      }),
    });
    // A successful reply that does not decode is an error, never an empty
    // page: a catch-up loop would stop early and miss retained events.
    const value = (await resp.json().catch((error: unknown) => {
      if (resp.ok) {
        throw PluginError.fromWire("internal", `events read reply is not JSON: ${String(error)}`);
      }
      return {};
    })) as {
      error?: GrantedErrorBody;
    } & Partial<EventReadPage>;
    if (value && value.error) {
      throw grantedError(value.error);
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `events read HTTP ${resp.status}`);
    }
    const events = Array.isArray(value.events) ? value.events : [];
    return {
      events: events.map((event) => fromBridgeJson(event) as DomainEvent),
      cursor: String(value.cursor ?? ""),
      more: Boolean(value.more),
      expiredEventIds: Array.isArray(value.expiredEventIds)
        ? value.expiredEventIds.map(String)
        : [],
      truncated: Boolean(value.truncated),
    };
  }
}

/**
 * Encode one {@link QueueBinding} body per its content type.
 *
 * @param body - Author-supplied body.
 * @param contentType - `json` (default), `text`, or `bytes`.
 * @returns Body bytes and the normalized content type.
 */
function queueBodyBytes(
  body: unknown,
  contentType: string | undefined,
): { body: Uint8Array; contentType: string } {
  const kind = contentType || "json";
  if (kind === "bytes") {
    if (body instanceof Uint8Array) return { body, contentType: kind };
    if (body instanceof ArrayBuffer) return { body: new Uint8Array(body), contentType: kind };
    throw PluginError.fromWire("invalid_params", "bytes queue message requires a Uint8Array body");
  }
  if (kind === "text") {
    if (typeof body !== "string") {
      throw PluginError.fromWire("invalid_params", "text queue message requires a string body");
    }
    return { body: new TextEncoder().encode(body), contentType: kind };
  }
  if (kind !== "json") {
    throw PluginError.fromWire("invalid_params", `unknown queue content type \`${kind}\``);
  }
  return { body: new TextEncoder().encode(JSON.stringify(body ?? null)), contentType: kind };
}

/**
 * `[[queues.producers]]` binding handed to the author: the adapter exchanges
 * the binding's queue-only grant token on `POST /queues/send`, so the author
 * isolate never holds the bearer itself.
 */
class GrantedQueue implements QueueBinding {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the queue grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Send one message.
   *
   * @param body - JSON value, string, or bytes per `options.contentType`.
   * @param options - Encoding and delay.
   * @returns The outbox id the host assigned.
   */
  async send(body: unknown, options?: QueueSendOptions): Promise<QueueSendOk> {
    return this.sendBatch([{ body, ...(options ?? {}) }]);
  }

  /**
   * Send up to `maxListPage` messages in order.
   *
   * @param messages - Bodies with per-message options.
   * @param options - Default delay for entries that carry none.
   * @returns The outbox ids in send order.
   */
  async sendBatch(
    messages: ReadonlyArray<QueueSendBatchEntry>,
    options?: Pick<QueueSendOptions, "delaySeconds">,
  ): Promise<QueueSendOk> {
    if (!Array.isArray(messages) || messages.length === 0) {
      throw PluginError.fromWire("invalid_params", "sendBatch requires at least one message");
    }
    if (messages.length > MAX_LIST_PAGE) {
      throw PluginError.fromWire(
        "payload_too_large",
        `queue batch of ${messages.length} exceeds ${MAX_LIST_PAGE}`,
      );
    }
    const wireMessages = messages.map((entry) => {
      const encoded = queueBodyBytes(entry?.body, entry?.contentType);
      if (encoded.body.byteLength > MAX_EVENT_PAYLOAD_BYTES) {
        throw PluginError.fromWire(
          "payload_too_large",
          `queue message body of ${encoded.body.byteLength} bytes exceeds ${MAX_EVENT_PAYLOAD_BYTES}`,
        );
      }
      return {
        body: encoded.body,
        contentType: encoded.contentType,
        delaySeconds: Math.max(0, Math.floor(Number(entry?.delaySeconds ?? 0) || 0)),
      };
    });
    const wire = toBridgeJson({
      messages: wireMessages,
      delaySeconds: Math.max(0, Math.floor(Number(options?.delaySeconds ?? 0) || 0)),
    });
    const resp = await this.#granted.fetch("http://granted/queues/send", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire),
    });
    const value = (await resp.json().catch(() => ({}))) as {
      error?: GrantedErrorBody;
      messageIds?: unknown;
    };
    if (value && value.error) {
      throw grantedError(value.error);
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `queue send HTTP ${resp.status}`);
    }
    return {
      messageIds: Array.isArray(value.messageIds) ? value.messageIds.map((id) => String(id)) : [],
    };
  }
}

/**
 * Fails closed on an empty key or one longer than `maxKvKeyBytes`.
 *
 * @param key - Key the guest passed.
 * @returns The key, once checked.
 */
function checkKvKey(key: unknown): string {
  if (typeof key !== "string" || key.length === 0) {
    throw PluginError.fromWire("invalid_params", "kv key must be a non-empty string");
  }
  const bytes = new TextEncoder().encode(key).byteLength;
  if (bytes > MAX_KV_KEY_BYTES) {
    throw PluginError.fromWire(
      "payload_too_large",
      `kv key of ${bytes} bytes exceeds ${MAX_KV_KEY_BYTES}`,
    );
  }
  return key;
}

/**
 * Stored metadata text → value (`null` when none or unparsable).
 *
 * @param text - Metadata as stored.
 * @returns The parsed value, or `null`.
 */
function parseKvMetadata(text: unknown): unknown {
  if (typeof text !== "string" || text === "") return null;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return null;
  }
}

/**
 * Decode a stored value per {@link KvValueType}.
 *
 * @param bytes - Stored bytes.
 * @param type - Requested decoding.
 * @returns Decoded value.
 */
function decodeKvValue(bytes: Uint8Array, type: KvValueType): unknown {
  if (type === "arrayBuffer") {
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  }
  const text = new TextDecoder().decode(bytes);
  if (type === "json") return JSON.parse(text) as unknown;
  return text;
}

/**
 * `[[kv_namespaces]]` binding handed to the author: the adapter exchanges the
 * binding's kv-only grant token on `POST /kv/<op>`, so the author isolate
 * never holds the bearer itself. Key, value, and metadata caps are checked
 * here before the request leaves; the host applies the namespace's own
 * `max_value_bytes` and `max_keys`.
 */
class GrantedKv implements KvNamespaceBinding {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the kv grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    this.#granted = granted;
    this.#auth = auth;
  }

  async #call(op: string, body: unknown): Promise<Record<string, unknown>> {
    const resp = await this.#granted.fetch(`http://granted/kv/${op}`, {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(toBridgeJson(body)),
    });
    const value = (await resp.json().catch(() => ({}))) as Record<string, unknown> & {
      error?: GrantedErrorBody;
    };
    if (value && value.error) {
      throw grantedError(value.error);
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `kv ${op} HTTP ${resp.status}`);
    }
    return value ?? {};
  }

  get(key: string, type?: "text"): Promise<string | null>;
  get<T = unknown>(key: string, type: "json"): Promise<T | null>;
  get(key: string, type: "arrayBuffer"): Promise<ArrayBuffer | null>;
  async get(key: string, type: KvValueType = "text"): Promise<unknown> {
    const found = await this.#read(key, type);
    return found.value;
  }

  getWithMetadata(key: string, type?: "text"): Promise<KvValueWithMetadata<string>>;
  getWithMetadata<T = unknown>(key: string, type: "json"): Promise<KvValueWithMetadata<T>>;
  getWithMetadata(key: string, type: "arrayBuffer"): Promise<KvValueWithMetadata<ArrayBuffer>>;
  async getWithMetadata(
    key: string,
    type: KvValueType = "text",
  ): Promise<KvValueWithMetadata<unknown>> {
    return this.#read(key, type);
  }

  async #read(key: string, type: KvValueType): Promise<KvValueWithMetadata<unknown>> {
    const reply = await this.#call("get", { key: checkKvKey(key) });
    if (!reply.found) return { value: null, metadata: null };
    const bytes = typeof reply.value === "string" ? base64ToBytes(reply.value) : new Uint8Array();
    return { value: decodeKvValue(bytes, type), metadata: parseKvMetadata(reply.metadata) };
  }

  async put(
    key: string,
    value: string | ArrayBuffer | ArrayBufferView,
    options?: KvPutOptionsBinding,
  ): Promise<void> {
    let bytes: Uint8Array;
    if (typeof value === "string") {
      bytes = new TextEncoder().encode(value);
    } else if (value instanceof ArrayBuffer) {
      bytes = new Uint8Array(value);
    } else if (ArrayBuffer.isView(value)) {
      bytes = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    } else {
      throw PluginError.fromWire(
        "invalid_params",
        "kv value must be a string, ArrayBuffer, or view",
      );
    }
    if (bytes.byteLength > MAX_KV_VALUE_BYTES) {
      throw PluginError.fromWire(
        "payload_too_large",
        `kv value of ${bytes.byteLength} bytes exceeds ${MAX_KV_VALUE_BYTES}`,
      );
    }
    const metadata = options?.metadata === undefined ? "" : JSON.stringify(options.metadata);
    const metadataBytes = new TextEncoder().encode(metadata).byteLength;
    if (metadataBytes > MAX_KV_METADATA_BYTES) {
      throw PluginError.fromWire(
        "payload_too_large",
        `kv metadata of ${metadataBytes} bytes exceeds ${MAX_KV_METADATA_BYTES}`,
      );
    }
    const expiration = Math.max(0, Math.floor(Number(options?.expiration ?? 0) || 0));
    const ttl = Math.max(0, Math.floor(Number(options?.expirationTtl ?? 0) || 0));
    await this.#call("put", {
      key: checkKvKey(key),
      value: bytes,
      options: { expirationUnixMs: expiration * 1000, expirationTtlSeconds: ttl, metadata },
    });
  }

  async delete(key: string): Promise<void> {
    await this.#call("delete", { key: checkKvKey(key) });
  }

  async list(options?: KvListOptionsBinding): Promise<KvListResultBinding> {
    const limit = Math.max(0, Math.floor(Number(options?.limit ?? 0) || 0));
    if (limit > MAX_LIST_PAGE) {
      throw PluginError.fromWire(
        "invalid_params",
        `kv list limit of ${limit} exceeds ${MAX_LIST_PAGE}`,
      );
    }
    const reply = await this.#call("list", {
      options: { prefix: options?.prefix ?? "", cursor: options?.cursor ?? "", limit },
    });
    const keys = Array.isArray(reply.keys) ? (reply.keys as Array<Record<string, unknown>>) : [];
    const cursor = typeof reply.cursor === "string" && reply.cursor ? reply.cursor : undefined;
    return {
      keys: keys.map((entry) => {
        const ms = Number(entry.expirationUnixMs ?? 0) || 0;
        const metadata = parseKvMetadata(entry.metadata);
        return {
          name: String(entry.key ?? ""),
          ...(ms > 0 ? { expiration: Math.floor(ms / 1000) } : {}),
          ...(metadata !== null ? { metadata } : {}),
        };
      }),
      list_complete: Boolean(reply.listComplete),
      ...(cursor ? { cursor } : {}),
    };
  }
}

/**
 * `[sockets]` grant handed to the author: the adapter exchanges the
 * sockets-only grant token on `POST /sockets/<op>`, so the author isolate
 * never holds the bearer. Every failure, including a channel that cannot be
 * reached, comes back as an `err` value rather than a throw, so its code
 * survives the Workers RPC hop to the author isolate.
 */
class GrantedSockets extends RpcTarget implements SocketsTransport {
  /** Granted reverse channel that serves `POST /sockets/<op>`. */
  #granted: GrantedFetcher;
  /** `Authorization` header carrying the sockets-only grant token. */
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the sockets grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Makes one `POST /sockets/<op>` call on the granted channel.
   *
   * @param op - Operation, the path segment after `/sockets/`.
   * @param body - That operation's JSON fields, bytes already base64.
   * @returns `{ ok }` with the route's reply body, or `{ err }` with the
   *   host's code (`unavailable` when the channel cannot be reached,
   *   `internal` for a reply that is not JSON); never throws.
   */
  async call(op: SocketOp, body: Record<string, unknown>): Promise<SocketTransportReply> {
    let resp: Response;
    try {
      resp = await this.#granted.fetch(`http://granted/sockets/${op}`, {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    } catch (err) {
      return { err: { code: "unavailable", message: `sockets ${op}: ${errorMessage(err)}` } };
    }
    const value = (await resp.json().catch(() => null)) as
      (Record<string, unknown> & { error?: GrantedErrorBody }) | null;
    if (value && value.error) {
      return {
        err: { code: value.error.code || "internal", message: value.error.message || "" },
      };
    }
    if (!resp.ok || !value || typeof value !== "object") {
      return { err: { code: "internal", message: `sockets ${op} HTTP ${resp.status}` } };
    }
    return { ok: value };
  }
}

/**
 * `ORIGIN` grant handed to a relay `ingress` author: the adapter exchanges
 * the origin-only grant token on `POST /origin/fetch`, so the author isolate
 * never holds the bearer. Failures come back as `err` values so their code
 * survives the Workers RPC hop.
 */
class GrantedOrigin extends RpcTarget implements OriginTransport {
  /** Granted reverse channel that serves `POST /origin/fetch`. */
  #granted: GrantedFetcher;
  /** `Authorization` header carrying the origin-only grant token. */
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the origin grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Hands one request to the library.
   *
   * @param head - Method, target, header fields and client address.
   * @param body - Request body, or `null`.
   * @returns The library's response, or a typed failure; never throws.
   */
  fetch(
    head: OriginRequest,
    body: ReadableStream<Uint8Array> | null,
  ): Promise<OriginTransportReply> {
    return grantedOriginFetch(this.#granted, this.#auth, head, body);
  }

  /**
   * Hands one WebSocket upgrade to the library.
   *
   * @param head - The upgrade's head.
   * @param attachment - The socket's initial attachment, base64.
   * @returns The route's answer, or a typed failure; never throws.
   */
  connect(head: OriginRequest, attachment: string): Promise<OriginConnectTransportReply> {
    return grantedOriginConnect(this.#granted, this.#auth, head, attachment);
  }
}

/**
 * `WEBHOOKS` grant handed to the author: the adapter exchanges the
 * webhooks-only grant token on `POST /webhooks/<op>`, so the author isolate
 * never holds the bearer. Every failure, including a channel that cannot be
 * reached, comes back as an `err` value rather than a throw, so its code
 * survives the Workers RPC hop to the author isolate.
 */
class GrantedWebhooks extends RpcTarget implements WebhooksTransport {
  /** Granted reverse channel that serves `POST /webhooks/<op>`. */
  #granted: GrantedFetcher;
  /** `Authorization` header carrying the webhooks-only grant token. */
  #auth: Record<string, string>;

  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the webhooks grant.
   */
  constructor(granted: GrantedFetcher, auth: Record<string, string>) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }

  /**
   * Makes one `POST /webhooks/<op>` call on the granted channel.
   *
   * @param op - Operation, the path segment after `/webhooks/`.
   * @param body - That operation's JSON fields, bytes already base64.
   * @returns `{ ok }` with the route's reply body, or `{ err }` with the
   *   host's code (`unavailable` when the channel cannot be reached,
   *   `internal` for a reply that is not JSON); never throws.
   */
  async call(op: WebhookOp, body: Record<string, unknown>): Promise<WebhookTransportReply> {
    let resp: Response;
    try {
      resp = await this.#granted.fetch(`http://granted/webhooks/${op}`, {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    } catch (err) {
      return { err: { code: "unavailable", message: `webhooks ${op}: ${errorMessage(err)}` } };
    }
    const value = (await resp.json().catch(() => null)) as
      (Record<string, unknown> & { error?: GrantedErrorBody }) | null;
    if (value && value.error) {
      return {
        err: { code: value.error.code || "internal", message: value.error.message || "" },
      };
    }
    if (!resp.ok || !value || typeof value !== "object") {
      return { err: { code: "internal", message: `webhooks ${op} HTTP ${resp.status}` } };
    }
    return { ok: value };
  }
}

/**
 * Named `[[databases]]` binding transport handed to the author: the adapter
 * exchanges the binding's database-only grant token on `POST /db/execute`,
 * so the author isolate never holds the bearer itself. ABI failures come
 * back inside the `ExecuteResultReply` union and surface as `PluginError`.
 *
 * The `*Bytes` methods never throw: a channel failure (a non-2xx status or
 * a failed fetch) is encoded into the reply's `err` arm instead, because the
 * author isolate calls them over Workers RPC and a thrown `PluginError` would
 * arrive there as a plain error without its code or `details`.
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
    try {
      const resp = await this.#granted.fetch("http://granted/db/execute", {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/octet-stream" },
        body: bytes,
      });
      return await grantedDatabaseBody(resp);
    } catch (err) {
      return encodeExecuteResultReply({ err: wirePluginError(err) });
    }
  }

  /**
   * Run one typed atomic batch answered as stream frames
   * (`POST /db/execute-streamed`). A non-2xx status is the channel's error;
   * an adapter or authorization failure travels inside the frames.
   *
   * @param request - Structured `ExecuteRequest`.
   * @returns The response body: the frame sequence.
   */
  async executeStreamed(request: ExecuteRequest): Promise<ReadableStream<Uint8Array> | Uint8Array> {
    const resp = await this.#granted.fetch("http://granted/db/execute-streamed", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/octet-stream" },
      body: encodeExecuteRequest(request),
    });
    if (!resp.ok) {
      await grantedDatabaseBody(resp, "executeStreamed");
    }
    return resp.body ?? new Uint8Array(0);
  }

  /**
   * Same frames, whole, from Cap'n request bytes (Python authors decode
   * locally).
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns Every `ExecuteStreamFrame`, concatenated; a channel failure is
   *   one `err` frame.
   */
  async executeStreamedBytes(bytes: Uint8Array): Promise<Uint8Array> {
    try {
      const resp = await this.#granted.fetch("http://granted/db/execute-streamed", {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/octet-stream" },
        body: bytes,
      });
      return await grantedDatabaseBody(resp, "executeStreamed");
    } catch (err) {
      return errFrame(err);
    }
  }

  /**
   * Same frames, as the channel delivers them (a `ReadableStream` when it
   * streams), from Cap'n request bytes, so a Python `iterate()` decodes
   * frames as they arrive.
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns The response body: the frame sequence; a channel failure is
   *   one `err` frame.
   */
  async openStreamedBytes(bytes: Uint8Array): Promise<ReadableStream<Uint8Array> | Uint8Array> {
    try {
      const resp = await this.#granted.fetch("http://granted/db/execute-streamed", {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/octet-stream" },
        body: bytes,
      });
      if (!resp.ok) {
        await grantedDatabaseBody(resp, "executeStreamed");
      }
      return resp.body ?? new Uint8Array(0);
    } catch (err) {
      return errFrame(err);
    }
  }

  /**
   * Negotiated limits of the granted session (`GET /db/limits`).
   *
   * @returns The adapter's `DbCapabilities` for this binding.
   */
  async limits(): Promise<DbCapabilities> {
    return decodeDbCapabilitiesReply(await this.limitsBytes());
  }

  /**
   * Same answer as Cap'n Proto bytes (Python authors decode locally).
   *
   * @returns Unpacked `DbCapabilitiesReply` message; a channel failure is
   *   its `err` arm.
   */
  async limitsBytes(): Promise<Uint8Array> {
    try {
      const resp = await this.#granted.fetch("http://granted/db/limits", {
        method: "GET",
        headers: { ...this.#auth },
      });
      return await grantedDatabaseBody(resp, "limits");
    } catch (err) {
      return encodeDbCapabilitiesReply({ err: wirePluginError(err) });
    }
  }
}

/**
 * The wire form of a granted-channel failure: a `PluginError` keeps its code,
 * message and `details`; anything else (a fetch that threw) is `unavailable`.
 *
 * @param err - What the channel call threw.
 * @returns `PluginError` fields as the Cap'n `err` arms carry them.
 */
function wirePluginError(err: unknown): { code: string; message: string; details: string } {
  if (err instanceof PluginError) {
    return { code: err.wireCode, message: err.message, details: err.detailsText };
  }
  const message = err instanceof Error ? err.message : String(err);
  return { code: "unavailable", message: `database grant: ${message}`, details: "" };
}

/**
 * A streamed reply that is one `err` frame: how the bytes transport reports a
 * channel failure, which both SDKs' stream decoders raise as `PluginError`.
 *
 * @param err - What the channel call threw.
 * @returns One encoded `ExecuteStreamFrame`.
 */
function errFrame(err: unknown): Uint8Array {
  return encodeMessage(ExecuteStreamFrameCodec, { kind: "err", value: wirePluginError(err) });
}

/**
 * Reads a granted `/db/*` response body, mapping a non-2xx status onto the
 * `PluginError` the channel implies: `401` is `unauthorized`, `413` is
 * `limit_exceeded` (the request was over the grant's `maxRequestBytes`; the
 * body is the canonical limit message, parsed back into `details`), `404`
 * and `405` are `unsupported` (an older host without the route), anything
 * else is `unavailable`.
 *
 * @param resp - Granted channel response.
 * @param method - `GuestDatabase` method name for the error text.
 * @returns Response body bytes.
 */
async function grantedDatabaseBody(resp: Response, method = "execute"): Promise<Uint8Array> {
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    const message = `database grant: ${resp.status} ${text}`.trim();
    if (resp.status === 413) {
      const limit = parseLimitExceeded(text);
      throw limit === undefined
        ? PluginError.fromWire("limit_exceeded", message)
        : new PluginError("limit_exceeded", message, { ...limit });
    }
    if (resp.status === 404 || resp.status === 405) {
      throw PluginError.fromWire(
        "unsupported",
        `GuestDatabase.${method} is not served by this host (${message})`,
      );
    }
    throw PluginError.fromWire(resp.status === 401 ? "unauthorized" : "unavailable", message);
  }
  return new Uint8Array(await resp.arrayBuffer());
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
          const resp = await granted.fetch("http://granted/cancel", {
            headers: auth,
            signal: stop,
          });
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
    throw new InvokeError(
      400,
      "invalid_params",
      `${name} header is not JSON: ${errorMessage(err)}`,
    );
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
export const TRIGGER_FAMILIES: Readonly<
  Record<
    | "eventConsumer"
    | "queueConsumer"
    | "scheduledHandler"
    | "jobRunner"
    | "socketHandler"
    | "webhookHandler",
    string
  >
> = Object.freeze({
  eventConsumer: "eventConsumer",
  queueConsumer: "queueConsumer",
  scheduledHandler: "scheduledHandler",
  jobRunner: "jobRunner",
  socketHandler: "socketHandler",
  webhookHandler: "webhookHandler",
});

/**
 * Frozen author view of one wire `ScheduledController`, with every field
 * present and typed.
 *
 * @param wire - Decoded wire struct (fields may be missing).
 * @returns The controller the author's `scheduled()` receives.
 */
export function scheduledControllerOf(
  wire: Partial<ScheduledController> | null | undefined,
): Readonly<ScheduledController> {
  const w = wire && typeof wire === "object" ? wire : {};
  return Object.freeze({
    cron: String(w.cron ?? ""),
    scheduledTime: Number(w.scheduledTime ?? 0) || 0,
    fireId: String(w.fireId ?? ""),
    attempt: Number(w.attempt ?? 0) || 0,
    deadlineUnixMs: Number(w.deadlineUnixMs ?? 0) || 0,
  });
}

/**
 * Filters requested entrypoint / trigger family names against the manifest
 * capabilities: named entrypoints must be declared in `entrypoints`,
 * `eventConsumer` needs a non-empty `consumes`, `queueConsumer` needs a
 * non-empty `queueConsumers`, `scheduledHandler` needs a non-empty `crons`,
 * `socketHandler` needs `sockets` (`[sockets]` in the manifest),
 * `webhookHandler` needs `webhooks` (`[[webhooks]]` in the manifest),
 * and `jobRunner` needs `jobs` or the `storage`
 * entrypoint (storage guests run the host `stream_copy` job). Unknown names
 * and a missing manifest fail closed.
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
  const queueConsumers = Array.isArray(capabilities.queueConsumers)
    ? capabilities.queueConsumers
    : [];
  const jobs = Array.isArray(capabilities.jobs) ? capabilities.jobs : [];
  const crons = Array.isArray(capabilities.crons) ? capabilities.crons : [];
  const exportsStorage = entrypoints.includes("storage");
  const out: string[] = [];
  for (const name of requested) {
    if (typeof name !== "string" || out.includes(name)) continue;
    if (name === TRIGGER_FAMILIES.eventConsumer) {
      if (consumes.length > 0) out.push(name);
    } else if (name === TRIGGER_FAMILIES.queueConsumer) {
      if (queueConsumers.length > 0) out.push(name);
    } else if (name === TRIGGER_FAMILIES.scheduledHandler) {
      if (crons.length > 0) out.push(name);
    } else if (name === TRIGGER_FAMILIES.jobRunner) {
      if (jobs.length > 0 || exportsStorage) out.push(name);
    } else if (name === TRIGGER_FAMILIES.socketHandler) {
      if (capabilities.sockets === true) out.push(name);
    } else if (name === TRIGGER_FAMILIES.webhookHandler) {
      if (capabilities.webhooks === true) out.push(name);
    } else if (name in ENTRYPOINT_BINDINGS && entrypoints.includes(name as EntrypointName)) {
      out.push(name);
    }
  }
  return out;
}

/**
 * Merges a guest's `describe()` under the manifest projection: identity,
 * `apiVersion`, `capabilities`, and `media` always come from the manifest; the guest's
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
      : (base.cli ?? guestCli);
  // `[[settings]]` is the authority for configOptions; an empty guest list
  // defers to the manifest projection and the host rejects a differing one.
  const guestOptions = guest.configOptions;
  const configOptions =
    Array.isArray(guestOptions) && guestOptions.length > 0
      ? guestOptions
      : (base.configOptions ?? guestOptions);
  return {
    ...base,
    ...guest,
    apiVersion: base.apiVersion ?? guest.apiVersion ?? 3,
    id: base.id ?? guest.id,
    capabilities: base.capabilities ?? guest.capabilities,
    // `[media]` is the authority for the declared media, like capabilities.
    media: base.media ?? guest.media,
    ...(cli === undefined ? {} : { cli }),
    ...(configOptions === undefined ? {} : { configOptions }),
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
     * Turn the bridge context into the author-facing one: the host's events,
     * database, and queue grant tokens become `EVENTS` / `env.<BINDING>`
     * stubs and never reach the author.
     *
     * @param ctx - Bridge context from the launcher.
     * @returns Author-facing granted context.
     */
    #bindContext(ctx: BridgeContext | undefined): GrantedContext {
      const source = ctx && typeof ctx === "object" ? ctx : {};
      const {
        eventsToken,
        notifyToken,
        aiToken,
        databases,
        queues,
        kv,
        sockets,
        originToken,
        webhooksToken,
        ...rest
      } = source;
      const bound: GrantedContext = rest;
      const granted = this.env.GRANTED;
      if (typeof eventsToken === "string" && eventsToken && granted) {
        bound.events = new SettledTarget(
          new GrantedEvents(granted, { Authorization: `Bearer ${eventsToken}` }),
          EVENTS_METHODS,
        );
      }
      if (typeof notifyToken === "string" && notifyToken && granted) {
        bound.notify = new SettledTarget(
          new GrantedNotify(granted, { Authorization: `Bearer ${notifyToken}` }),
          NOTIFY_METHODS,
        );
      }
      if (typeof aiToken === "string" && aiToken && granted) {
        bound.ai = new SettledTarget(
          new GrantedAi(granted, { Authorization: `Bearer ${aiToken}` }),
          AI_METHODS,
        );
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
      if (queues && typeof queues === "object" && !Array.isArray(queues) && granted) {
        const named: NamedQueue[] = [];
        for (const [name, token] of Object.entries(queues)) {
          if (!name || typeof token !== "string" || !token) continue;
          named.push({
            name,
            queue: new SettledTarget(
              new GrantedQueue(granted, { Authorization: `Bearer ${token}` }),
              QUEUE_METHODS,
            ),
          });
        }
        if (named.length > 0) bound.queues = named;
      }
      if (kv && typeof kv === "object" && !Array.isArray(kv) && granted) {
        const named: NamedKv[] = [];
        for (const [name, token] of Object.entries(kv)) {
          if (!name || typeof token !== "string" || !token) continue;
          named.push({
            name,
            kv: new SettledTarget(
              new GrantedKv(granted, { Authorization: `Bearer ${token}` }),
              KV_METHODS,
            ),
          });
        }
        if (named.length > 0) bound.kv = named;
      }
      if (sockets && typeof sockets === "object" && !Array.isArray(sockets) && granted) {
        const named: NamedSockets[] = [];
        for (const [name, token] of Object.entries(sockets)) {
          if (!name || typeof token !== "string" || !token) continue;
          named.push({
            name,
            sockets: new GrantedSockets(granted, { Authorization: `Bearer ${token}` }),
          });
        }
        if (named.length > 0) bound.sockets = named.slice(0, 1);
      }
      if (typeof originToken === "string" && originToken && granted) {
        bound.origin = new GrantedOrigin(granted, { Authorization: `Bearer ${originToken}` });
      }
      if (typeof webhooksToken === "string" && webhooksToken && granted) {
        bound.webhooks = new GrantedWebhooks(granted, {
          Authorization: `Bearer ${webhooksToken}`,
        });
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
          return this.invokeEntrypoint("storage", ctx, "get", [
            String(args.key ?? ""),
            args.options,
          ]);
        case "put": {
          if (!body) {
            throw PluginError.fromWire("invalid_params", "put missing body stream");
          }
          const options = args.options as WriteOptions | undefined;
          const bounded = exactLengthBody(body, options?.contentLength);
          return this.invokeEntrypoint("storage", ctx, "put", [
            String(args.key ?? ""),
            bounded,
            options,
          ]);
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
        if (
          context !== undefined &&
          (context === null || typeof context !== "object" || Array.isArray(context))
        ) {
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
