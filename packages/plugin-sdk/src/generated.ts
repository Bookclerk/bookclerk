/**
 * GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.
 *
 * Author-facing TypeScript projection of every struct, union, and
 * interface in `crates/bookclerk-plugin-abi/schema/plugin.capnp`. Field
 * names are the schema names (camelCase). Cap'n Proto unions become
 * discriminated `{ kind, value }` unions; `Void` members carry no
 * `value`. `Int64` is `bigint` (exact SQL integers); `UInt64` counters,
 * sizes, and unix-millisecond timestamps are `number`. `Data` is
 * `Uint8Array`. Interfaces are capabilities: their methods return
 * promises and the wire codecs carry them through a transport cap table.
 *
 * @module
 */

import type {
  PluginErrorCode,
  Entrypoint,
  PortalAuthMode,
  CliArgKind,
  CatalogSort,
  CatalogField,
  Abridgement,
  DbType,
  DbStatementKind,
  DbResultSelection,
  IsolationReq,
  ResolvedSqlType,
  IntegerArithKind,
} from "./abi.js";

export type {
  PluginErrorCode,
  Entrypoint,
  PortalAuthMode,
  CliArgKind,
  CatalogSort,
  CatalogField,
  Abridgement,
  DbType,
  DbStatementKind,
  DbResultSelection,
  IsolationReq,
  ResolvedSqlType,
  IntegerArithKind,
};

/** Guest-advertised caps for `rpc.scalarLimits`; never above the file constants. */
export interface ScalarLimits {
  /** Largest scalar the guest accepts; at most `maxScalarBytes`. */
  maxScalarBytes: number;
  /** Largest `ByteSource.pull` window; at most `maxStreamWindowBytes`. */
  maxStreamWindowBytes: number;
  /** Largest list page; at most `maxListPage`. */
  maxListPage: number;
}

/**
 * `code` is a snake_case string (`not_found`, `invalid_cursor`, …). Unknown
 * codes are forwarded as-is; SDKs surface them as PluginErrorCode.unknown
 * while retaining the raw wire code.
 */
export interface PluginError {
  /** Stable snake_case error code (see `PluginErrorCode`). */
  code: string;
  /** Human-readable detail; never contains secrets. */
  message: string;
}

/** Full metadata of one stored object. */
export interface ObjectMetadata {
  /** Object key. */
  key: string;
  /** Object size in bytes. */
  size: number;
  /** MIME type; empty when unknown. */
  contentType: string;
  /** Backend entity tag; empty when unsupported. */
  etag: string;
  /** Raw SHA-256 digest (32 bytes) or empty when unknown. */
  sha256: Uint8Array;
}

/** Compact object entry in a list page. */
export interface ObjectInfo {
  /** Object key. */
  key: string;
  /** Object size in bytes. */
  size: number;
}

/** Paging options for `Destination.list`. */
export interface ListOptions {
  /** Only keys starting with this prefix; empty lists everything. */
  prefix: string;
  /** Opaque cursor from a previous `ListPage.nextCursor`; empty starts over. */
  cursor: string;
  /** Requested page size; clamped to `maxListPage`. `0` means guest default. */
  limit: number;
}

/** One page of `Destination.list` results. */
export interface ListPage {
  /** Objects in this page, in backend order. */
  objects: ObjectInfo[];
  /** Cursor for the next page; empty when exhausted. */
  nextCursor: string;
}

/** Half-open byte window `[offset, offset + length)`. */
export interface ByteRange {
  /** First byte offset. */
  offset: number;
  /** Number of bytes; `0` reads to the end. */
  length: number;
}

/** Options for `Destination.get`. */
export interface ReadOptions {
  /** Byte range to read; an all-zero range reads the whole object. */
  range: ByteRange;
}

/** Options for `Destination.put`. */
export interface WriteOptions {
  /** MIME type to record; empty when unknown. */
  contentType: string;
  /** Expected body length in bytes; `0` when unknown (chunked). */
  contentLength: number;
  /** Expected raw SHA-256 digest; empty skips verification. */
  sha256: Uint8Array;
  /** Destination-side stage-and-publish. Empty means a one-shot put. */
  commitToken: string;
  /** When true, `put` stages remotely and does not publish until `commit`. */
  stageOnly: boolean;
}

/** Summary of a stored (or committed) object. */
export interface PutResult {
  /** Object key written. */
  key: string;
  /** Bytes persisted. */
  bytesWritten: number;
  /** Backend entity tag; empty when unsupported. */
  etag: string;
  /** Raw SHA-256 digest of the stored bytes; empty when not computed. */
  sha256: Uint8Array;
}

/** Summary of a server-side copy. */
export interface CopyResult {
  /** Bytes copied. */
  bytesCopied: number;
}

/** Guest identity and negotiation surface returned by `describe()`. */
export interface PluginDescribe {
  /** ABI version the guest speaks; must equal `apiVersion`. */
  apiVersion: number;
  /** Stable plugin id (`[a-z][a-z0-9_]{0,63}`). */
  id: string;
  /** Human-readable name for UI lists. */
  displayName: string;
  /** Negotiable feature names the guest supports (see `feature*` constants). */
  rpcFeatures: string[];
  /** Guest caps when `rpc.scalarLimits` is advertised. */
  scalarLimits: ScalarLimits;
  /**
   * Exported entrypoints, triggers, and bindings the guest implements. The
   * host rejects anything wider than the manifest and the operator grant.
   */
  capabilities: PluginCapabilities;
  /** Portal Accounts connect mode for storefronts. */
  portalAuthMode: PortalAuthMode;
  /**
   * Env var name operators may set for password helpers; never required for
   * Accounts UI connect. Empty when the guest accepts none.
   * Omitted when absent (wire zero value).
   */
  passwordEnvVar?: string;
  /** Alternate ids accepted for config / CLI targeting. */
  aliases: string[];
  /** UI sort weight among peers of the same family; lower sorts first. */
  sortKey: number;
  /**
   * Portal brand colors and icon URL; `brand.id` is empty when the guest has
   * no brand and the host renders a neutral fallback.
   */
  brand: Brand;
  /** Discoverable config option groups for source UIs. */
  configOptions: ConfigOption[];
  /** Embedded CLI schema (same shape as `PluginCli.describe`); empty when unused. */
  cli: CliSchema;
}

/**
 * Bookclerk-as-IdP relying-party template. Plugins declare callback path and
 * client id; the host materializes `oidc_clients` rows and remains the AS.
 * `originConfigKey` is a dotted config path (e.g. integrations.audiobookshelf.base_url).
 */
export interface OidcClientTemplate {
  /** OIDC client id the host materializes. */
  clientId: string;
  /** Name shown on consent / admin screens. */
  displayName: string;
  /** Redirect URI path relative to the integration origin. */
  callbackPath: string;
  /** When true, the client uses PKCE without a client secret. */
  publicClient: boolean;
  /** Scopes granted by default. */
  defaultScopes: string[];
  /** When true, the AS issues refresh tokens to this client. */
  issueRefreshToken: boolean;
  /** Dotted config path holding the client's origin URL. */
  originConfigKey: string;
}

/** Success payload of `Oidc.clients`. */
export interface OidcClientsOk {
  /** Client templates; empty when the plugin is not a relying party. */
  clients: OidcClientTemplate[];
}

/** Result union of `Oidc.clients`. */
export type OidcClientsReply =
  | { kind: "ok"; value: OidcClientsOk } // Success: OIDC client templates.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Plugin-specific extensible config. Not a substitute for typed ABI fields. */
export interface ExtensibleConfig {
  /** Version of `payload`'s schema, owned by the plugin. */
  schemaVersion: number;
  /** Media type of `payload` (e.g. `application/json`). */
  mediaType: string;
  /** Bounded encoded payload; at most `maxConfigPayloadBytes`. */
  payload: Uint8Array;
}

/**
 * Host-granted bindings for one `PluginWorker.open` (`env` in the Workers
 * idiom). OS paths, FDs, and sockets are transport-private. Capability fields
 * are null when the manifest does not declare (or the operator did not grant)
 * the binding.
 */
export interface Bindings {
  /**
   * `CONFIG`: granted plugin settings (operator `[vars]` plus the family
   * settings table) as `application/json`; at most `maxConfigPayloadBytes`.
   */
  config: ExtensibleConfig;
  /**
   * `SECRETS`: granted secret values as `application/json`; empty payload
   * when the manifest declares no `[secrets]`.
   */
  secrets: ExtensibleConfig;
  /**
   * Database-adapter bootstrap for the `databaseAdapter` entrypoint. First-party
   * host-managed adapters receive host-private connect params in `config`;
   * third-party adapters receive this typed bootstrap (and an empty `config`).
   */
  adapter: DatabaseAdapterConfig;
  /** `EVENTS`: outbox publisher; null unless `[[events.producers]]` is granted. */
  events: EventPublisher;
  /**
   * Named plugin-owned `[[databases]]` bindings: each entry is an isolated
   * database provisioned by the active adapter, separate from the Bookclerk
   * library and from every other plugin. Empty when the manifest declares none.
   */
  databases: NamedDatabase[];
  /** Host cancellation for the whole invocation (fence / lease loss). */
  cancel: Cancellation;
  /**
   * `WORK_FS`: host-granted object storage for durable plugin files (work
   * filesystem); null unless `[work_fs]` is granted. Job input/output travel on
   * `JobRunner.job(controller)`, never here.
   */
  storage: Destination;
}

/**
 * Exported entrypoints returned by `PluginWorker.open`. One capability per
 * `plugin.toml` `entrypoints` entry / trigger; null when not exported. The host
 * refuses an entrypoint the manifest or operator grant did not allow.
 */
export interface Entrypoints {
  /** `[[events.consumers]]` trigger: `event(batch)` handler. */
  eventConsumer: EventConsumer;
  /** `[triggers] jobs` trigger: `job(controller)` handler. */
  jobRunner: JobRunner;
  /** `storefront` entrypoint. */
  storefront: ContentSource;
  /** `storage` entrypoint. */
  storage: Destination;
  /** `databaseAdapter` entrypoint. */
  databaseAdapter: Database;
  /** `remoteLibrary` entrypoint. */
  remoteLibrary: RemoteLibrary;
  /** `cli` entrypoint. */
  cli: PluginCli;
  /** `oidc` entrypoint. */
  oidc: Oidc;
}

/** Result union of `PluginWorker.open`. */
export type EntrypointsReply =
  | { kind: "ok"; value: Entrypoints } // Success: exported entrypoint capabilities.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Durable command envelope (not a domain event). Command payload schema
 * version and checkpoint schema versions are independent of the plugin ABI.
 * The envelope itself is not persisted as opaque bytes across ABI majors.
 * Idempotency keys are scoped to (account, plugin, commandType) until a
 * terminal fenced outcome is committed.
 */
export interface JobInvocation {
  /** Schema version of `payloadJson`, owned by the command type. */
  payloadSchemaVersion: number;
  /** Unique id of this invocation attempt. */
  invocationId: string;
  /** Command type the handler dispatches on. */
  commandType: string;
  /** Command payload (JSON); at most `maxScalarBytes`. */
  payloadJson: string;
  /** Caller idempotency key scoped to (account, plugin, commandType). */
  idempotencyKey: string;
  /** Failure retry counter, starting at 1. */
  attempt: number;
  /** Trace correlation id; empty when none. */
  correlationId: string;
  /** Id of the event or command that caused this one; empty when none. */
  causationId: string;
  /**
   * UTC Unix milliseconds. Host fence/lease is authoritative; this hint must
   * not outlive the fence (clock skew across VPS nodes).
   */
  deadlineUnixMs: number;
  /** Checkpoint persisted by a prior `SuspendedOutcome`; empty on first run. */
  checkpointJson: string;
  /** Schema version of `checkpointJson`. */
  checkpointSchemaVersion: number;
  /** Resume ordinal; distinct from failure `attempt`. */
  invocationSequence: number;
  /** Optional step identifier for multi-step commands. */
  stepId: string;
}

/** Job finished successfully. */
export interface CompletedOutcome {
  /** Short human summary. */
  message: string;
  /** Bytes produced, when meaningful. */
  bytesCopied: number;
}

/** Job failed transiently; the host reschedules it. */
export interface RetryableOutcome {
  /** Short human reason. */
  message: string;
  /** Earliest retry time; `0` lets the host choose. */
  retryAfterUnixMs: number;
}

/** Job failed permanently; no retry. */
export interface RejectedOutcome {
  /** Short human reason. */
  message: string;
}

/** Job observed cancellation and stopped. */
export interface CancelledOutcome {
  /** Short human note. */
  message: string;
}

/** Job released the process and asks to be resumed later. */
export interface SuspendedOutcome {
  /** Bounded checkpoint to replay on resume; at most `maxCheckpointBytes`. */
  checkpointJson: string;
  /** Schema version of `checkpointJson`. */
  checkpointSchemaVersion: number;
  /** Earliest resume time. */
  wakeAtUnixMs: number;
}

/** Terminal or suspended result of `JobRunner.job`. */
export type JobOutcome =
  | { kind: "completed"; value: CompletedOutcome } // Finished successfully.
  | { kind: "retryable"; value: RetryableOutcome } // Transient failure; retry later.
  | { kind: "rejected"; value: RejectedOutcome } // Permanent failure.
  | { kind: "cancelled"; value: CancelledOutcome } // Stopped on cancellation.
  | { kind: "suspended"; value: SuspendedOutcome }; // Released with a checkpoint.

/** Domain event (not a job). Outbox-produced, at-least-once, idempotent consume. */
export interface DomainEvent {
  /** Unique event id (outbox row identity). */
  eventId: string;
  /** Dotted event type (e.g. `library.title.added`). */
  eventType: string;
  /** Schema version of `payload`, owned by the event type. */
  schemaVersion: number;
  /** When the producer observed the fact. */
  occurredAtUnixMs: number;
  /** Account scope; empty for host-wide events. */
  accountId: string;
  /** Trace correlation id; empty when none. */
  correlationId: string;
  /** Id of the command or event that caused this one; empty when none. */
  causationId: string;
  /** Consumer-side idempotency key; stable across redeliveries. */
  deduplicationKey: string;
  /** Delivery counter, starting at 1. */
  deliveryAttempt: number;
  /** Encoded event payload; at most `maxEventPayloadBytes`. */
  payload: Uint8Array;
  /** Append-only. Resume a prior EventResult.suspended. */
  checkpointJson: string;
  /** Schema version of `checkpointJson`. */
  checkpointSchemaVersion: number;
  /** Resume ordinal; distinct from `deliveryAttempt`. */
  invocationSequence: number;
  /** True when this delivery resumes a prior suspension. */
  resumePending: boolean;
  /** Append-only. Producer plugin id; empty when unknown. */
  source: string;
}

/** Event handled; the host marks it delivered. */
export interface EventAck {
  /** Placeholder; the struct carries no data. */
  dummy: void;
}

/** Redeliver later. */
export interface EventRetry {
  /** Earliest redelivery time; `0` lets the host choose. */
  retryAtUnixMs: number;
  /** Short human reason. */
  reason: string;
}

/** Event rejected; the host records the reason and stops delivering. */
export interface EventReject {
  /** Short human reason. */
  reason: string;
}

/** Event moved to the dead-letter queue for operator review. */
export interface EventDeadLetter {
  /** Short human reason. */
  reason: string;
}

/**
 * Append-only. Mirrors job SuspendedOutcome; event handlers persist a
 * bounded checkpoint and release the process until wakeAtUnixMs. Optional
 * wake-on-matching-event fields (empty = timestamp-only).
 */
export interface EventSuspended {
  /** Bounded checkpoint to replay on resume; at most `maxCheckpointBytes`. */
  checkpointJson: string;
  /** Schema version of `checkpointJson`. */
  checkpointSchemaVersion: number;
  /** Earliest resume time. */
  wakeAtUnixMs: number;
  /** Also wake when an event of this type arrives; empty disables. */
  wakeOnEventType: string;
  /** JSON filter applied to matching wake events; empty matches all. */
  wakeOnFilterJson: string;
}

/** Per-event outcome inside an `EventConsumer.event` batch reply. */
export type EventResult =
  | { kind: "ack"; value: EventAck } // Handled.
  | { kind: "retry"; value: EventRetry } // Redeliver later.
  | { kind: "reject"; value: EventReject } // Stop delivering.
  | { kind: "deadLetter"; value: EventDeadLetter } // Park for operator review.
  | { kind: "suspended"; value: EventSuspended }; // Released with a checkpoint.

/**
 * One `EventConsumer.event` delivery: at most `maxListPage` events, each
 * bounded by `maxEventPayloadBytes` / `maxCheckpointBytes`.
 */
export interface EventBatch {
  /** Events in delivery order; the reply carries one `EventResult` per entry. */
  events: DomainEvent[];
}

/**
 * Result union of `EventConsumer.event`. `ok` has exactly one entry per
 * `EventBatch.events` entry, in order; a short list is a host-side error and
 * the missing tail is retried.
 */
export type EventBatchReply =
  | { kind: "ok"; value: EventResult[] } // Success: per-event outcomes.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Everything one `JobRunner.job` invocation may touch. Capabilities are
 * host-served; the guest never sees OS paths or sockets.
 */
export interface JobController {
  /** Durable command envelope. */
  invocation: JobInvocation;
  /** Job input objects. */
  input: Source;
  /** Job output object store. */
  output: Destination;
  /** Progress reporter (host coalesces frequent updates). */
  progress: ProgressSink;
  /** Host cancellation probe for this job (fence / lease). */
  cancel: Cancellation;
}

/** Success payload of `Destination.head`. */
export interface HeadOk {
  /** Whether the object exists. */
  found: boolean;
  /** Object metadata; meaningful only when `found`. */
  meta: ObjectMetadata;
}

/** Result union of `Destination.head`. */
export type HeadReply =
  | { kind: "ok"; value: HeadOk } // Success: head probe outcome.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.list`. */
export type ListReply =
  | { kind: "ok"; value: ListPage } // Success: one list page.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `Destination.get`. */
export interface GetOk {
  /** Object metadata. */
  meta: ObjectMetadata;
  /** Streamed object bytes. */
  body: ByteSource;
}

/** Result union of `Destination.get`. */
export type GetReply =
  | { kind: "ok"; value: GetOk } // Success: object metadata and body stream.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.put` / `Destination.commit`. */
export type PutReply =
  | { kind: "ok"; value: PutResult } // Success: stored object summary.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.copy`. */
export type CopyReply =
  | { kind: "ok"; value: CopyResult } // Success: copy summary.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `methods without a success payload`. */
export type EmptyReply =
  | { kind: "ok" } // Success: no payload.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `ByteSource.pull`. */
export interface PullOk {
  /** Next bytes; may be shorter than requested. */
  chunk: Uint8Array;
  /** True when the stream is exhausted after `chunk`. */
  done: boolean;
}

/** Result union of `ByteSource.pull`. */
export type PullReply =
  | { kind: "ok"; value: PullOk } // Success: one stream window.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `Source.open`. */
export interface OpenOk {
  /** Object metadata. */
  meta: ObjectMetadata;
  /** Streamed object bytes. */
  body: ByteSource;
}

/** Result union of `Source.open`. */
export type OpenReply =
  | { kind: "ok"; value: OpenOk } // Success: object metadata and body stream.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `PluginWorker.describe`. */
export type DescribeReply =
  | { kind: "ok"; value: PluginDescribe } // Success: plugin identity.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `JobRunner.job`. */
export type HandleReply =
  | { kind: "ok"; value: JobOutcome } // Success: job outcome.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Typed liveness report. */
export interface HealthOk {
  /** True when the guest is healthy enough for traffic. */
  ok: boolean;
  /** Short human status line; empty when none. */
  detail: string;
}

/** Result union of `health`. */
export type HealthReply =
  | { kind: "ok"; value: HealthOk } // Success: health status.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `Database.openSession`. */
export type AdapterSessionReply =
  | { kind: "ok"; value: AdapterDatabaseSession } // Success: opened `AdapterDatabaseSession` capability.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `guest database opens`. */
export type GuestDatabaseReply =
  | { kind: "ok"; value: GuestDatabase } // Success: opened `GuestDatabase` capability.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Transferred readable byte stream. The capability *is* the stream; callers
 * pull bounded windows. Abort is capability drop / RPC cancel. A failed pull
 * MUST set `err` — never a successful empty EOF.
 */
export interface ByteSource {
  /**
   * Pull the next window of bytes. `done = true` on the final chunk.
   *
   * @param maxBytes - Upper bound for this window; at most `maxStreamWindowBytes`.
   * @returns {@link PullReply}
   */
  pull(maxBytes: number): Promise<PullReply>;
}

/**
 * Object store the host writes acquired media into (`output.*` plugins).
 * Keys are relative object paths; the plugin owns the physical layout.
 */
export interface Destination {
  /**
   * Metadata probe. `found = false` is a success, not `not_found`.
   *
   * @param key - Object key to probe.
   * @returns {@link HeadReply}
   */
  head(key: string): Promise<HeadReply>;
  /**
   * Page through objects under a prefix; at most `maxListPage` per page.
   *
   * @param options - Prefix, cursor, and page size.
   * @returns {@link ListReply}
   */
  list(options: ListOptions): Promise<ListReply>;
  /**
   * Open an object (or a byte range of it) for reading.
   *
   * @param key - Object key to read.
   * @param options - Optional byte range.
   * @returns {@link GetReply}
   */
  get(key: string, options: ReadOptions): Promise<GetReply>;
  /**
   * Store an object from a transferred byte stream.
   *
   * @param key - Object key to write.
   * @param body - Streamed object bytes.
   * @param options - Content type, length, digest, staging.
   * @returns {@link PutReply}
   */
  put(key: string, body: ByteSource, options: WriteOptions): Promise<PutReply>;
  /**
   * Server-side copy (requires `storage.copy`).
   *
   * @param from - Source object key.
   * @param to - Destination object key.
   * @returns {@link CopyReply}
   */
  copy(from: string, to: string): Promise<CopyReply>;
  /**
   * Remove an object; deleting a missing key is a success.
   *
   * @param key - Object key to remove.
   * @returns {@link EmptyReply}
   */
  delete(key: string): Promise<EmptyReply>;
  /**
   * Finalize a destination-side staged object. Staging itself is `put` with
   * `stageOnly = true`; bytes must stream into destination-managed temp/multipart
   * storage, never a complete local spool on host/adapter/broker/guest.
   *
   * @param key - Object key that was staged.
   * @param commitToken - Token from `WriteOptions.commitToken`.
   * @returns {@link PutReply}
   */
  commit(key: string, commitToken: string): Promise<PutReply>;
  /**
   * Discard a staged object without publishing it.
   *
   * @param key - Object key that was staged.
   * @param commitToken - Token from `WriteOptions.commitToken`.
   * @returns {@link EmptyReply}
   */
  abortStage(key: string, commitToken: string): Promise<EmptyReply>;
}

/** Read-only byte source for job inputs (not a storefront). */
export interface Source {
  /**
   * Open an object for streaming reads.
   *
   * @param key - Object key to open.
   * @returns {@link OpenReply}
   */
  open(key: string): Promise<OpenReply>;
}

/** Host-side progress reporter handed to job handlers. */
export interface ProgressSink {
  /**
   * Report progress; the host coalesces frequent updates.
   *
   * @param percent - Completion in `[0, 100]`.
   * @param message - Short human-readable status line.
   * @returns {@link EmptyReply}
   */
  report(percent: number, message: string): Promise<EmptyReply>;
}

/**
 * Transport cancellation. SDKs project this into a locally created AbortSignal
 * (AbortSignal is not a serializable Workers RPC value).
 */
export interface Cancellation {
  /**
   * Non-blocking check; `true` once the host has fenced the invocation.
   *
   * @returns `cancelled` (boolean)
   */
  poll(): Promise<boolean>;
}

/** `[triggers] jobs` handler (`Entrypoints.jobRunner`); runs one durable command. */
export interface JobRunner {
  /**
   * Run one command invocation to a terminal or suspended outcome. Named
   * database bindings come from `Bindings.databases` at `open`, not per job.
   *
   * @param controller - Envelope plus host-served capabilities.
   * @returns {@link HandleReply}
   */
  job(controller: JobController): Promise<HandleReply>;
}

/**
 * `[[events.consumers]]` handler (`Entrypoints.eventConsumer`). Delivery is
 * at-least-once; consumers must be idempotent on `deduplicationKey`.
 */
export interface EventConsumer {
  /**
   * Deliver one ordered batch of domain events.
   *
   * @param batch - Events in delivery order.
   * @returns {@link EventBatchReply}
   */
  event(batch: EventBatch): Promise<EventBatchReply>;
}

/**
 * `EVENTS` binding: host-served outbox publisher granted through
 * `Bindings.events`. Event types must be listed in `[[events.producers]]`.
 */
export interface EventPublisher {
  /**
   * Append one event to the outbox (idempotent on `deduplicationKey`).
   *
   * @param event - Event to publish.
   * @returns {@link PublishReply}
   */
  publish(event: PluginEvent): Promise<PublishReply>;
}

/** One named plugin-owned database binding delivered on `Bindings.databases`. */
export interface NamedDatabase {
  /** Binding name from `plugin.toml` `[[databases]]`. */
  name: string;
  /** Isolated typed SQL session for this binding (plugin-owned schema). */
  database: GuestDatabase;
}

/**
 * Storefront content source (not byte Source). Every method takes and
 * returns typed structs from the "Typed method payloads" section.
 */
export interface ContentSource {
  /**
   * Connect an account (password or one-shot OAuth).
   *
   * @param params - Credentials, marketplace, callback wiring.
   * @returns {@link LoginReply}
   */
  login(params: LoginParams): Promise<LoginReply>;
  /**
   * Sync library rows for one or more accounts.
   *
   * @param params - Accounts, paging, and host-sealed credentials.
   * @returns {@link ScanReply}
   */
  scan(params: ScanParams): Promise<ScanReply>;
  /**
   * Download and decrypt one title into `cacheDir`.
   *
   * @param params - Title, credentials, and fetch options.
   * @returns {@link FetchTitleReply}
   */
  fetchTitle(params: FetchTitleParams): Promise<FetchTitleReply>;
  /**
   * Enumerate accounts the guest knows about.
   *
   * @returns {@link SourceAccountsReply}
   */
  listAccounts(): Promise<SourceAccountsReply>;
  /**
   * Begin an interactive OAuth login; returns a session id.
   *
   * @param params - Same shape as `login`; host fills callback IPC.
   * @returns {@link LoginStartReply}
   */
  loginStart(params: LoginParams): Promise<LoginStartReply>;
  /**
   * Finish an interactive OAuth login started by `loginStart`.
   *
   * @param params - Session id from `loginStart`.
   * @returns {@link LoginReply}
   */
  loginComplete(params: LoginCompleteParams): Promise<LoginReply>;
  /**
   * Free-text storefront catalog search.
   *
   * @param params - Query, region, paging, sort, facet.
   * @returns {@link CatalogHitsReply}
   */
  searchCatalog(params: SearchCatalogParams): Promise<CatalogHitsReply>;
  /**
   * Related-title expansion from a seed title.
   *
   * @param params - Seed identity fields and limit.
   * @returns {@link CatalogHitsReply}
   */
  expandCandidates(params: ExpandCandidatesParams): Promise<CatalogHitsReply>;
  /**
   * Purchase link / price hint for one title.
   *
   * @param params - Identity fields and price flag.
   * @returns {@link PurchaseHintReply}
   */
  purchaseHint(params: PurchaseHintParams): Promise<PurchaseHintReply>;
  /**
   * Current storefront deals.
   *
   * @param params - Optional result cap.
   * @returns {@link CatalogHitsReply}
   */
  listDeals(params: ListDealsParams): Promise<CatalogHitsReply>;
  /**
   * Liveness / readiness probe.
   *
   * @returns {@link HealthReply}
   */
  health(): Promise<HealthReply>;
  /**
   * Human-readable diagnostic lines.
   *
   * @returns {@link DiagnoseReply}
   */
  diagnose(): Promise<DiagnoseReply>;
  /**
   * Full catalog record for one product.
   *
   * @param params - Product id and optional ISBN.
   * @returns {@link CatalogDetailReply}
   */
  catalogDetail(params: CatalogDetailParams): Promise<CatalogDetailReply>;
}

/**
 * `remoteLibrary` entrypoint: long-running remote-library lifecycle (start /
 * stop, library rescan, listening sync, external-user polling). Event delivery
 * is `EventConsumer`; credential verification is `Oidc`.
 */
export interface RemoteLibrary {
  /**
   * Liveness / readiness probe.
   *
   * @returns {@link HealthReply}
   */
  health(): Promise<HealthReply>;
  /**
   * Start background work after the host has granted bindings.
   *
   * @returns {@link EmptyReply}
   */
  start(): Promise<EmptyReply>;
  /**
   * Stop background work; the host may drop the capability afterwards.
   *
   * @returns {@link EmptyReply}
   */
  stop(): Promise<EmptyReply>;
  /**
   * Human-readable diagnostic lines.
   *
   * @returns {@link DiagnoseReply}
   */
  diagnose(): Promise<DiagnoseReply>;
  /**
   * Re-sync the remote library.
   *
   * @param params - Full-rescan flag.
   * @returns {@link EmptyReply}
   */
  scanLibrary(params: ScanLibraryParams): Promise<EmptyReply>;
  /**
   * Push / pull listening progress.
   *
   * @returns {@link SyncListeningReply}
   */
  syncListening(): Promise<SyncListeningReply>;
  /**
   * Drain external users the remote side observed since the last poll.
   *
   * @returns {@link EventPollReply}
   */
  pollEvents(): Promise<EventPollReply>;
}

/** `cli` entrypoint: guest commands under `bookclerk plugins <id> <command>`. */
export interface PluginCli {
  /**
   * Declared CLI surface.
   *
   * @returns {@link CliSchemaReply}
   */
  describe(): Promise<CliSchemaReply>;
  /**
   * Run one plugin CLI command.
   *
   * @param params - Command name and argument values.
   * @returns {@link CliInvokeReply}
   */
  invoke(params: CliInvokeParams): Promise<CliInvokeReply>;
}

/**
 * `oidc` entrypoint: relying-party client templates and credential
 * verification on behalf of the host authorization server.
 */
export interface Oidc {
  /**
   * Plugin-provided OIDC AS client templates. Empty list when unused.
   *
   * @returns {@link OidcClientsReply}
   */
  clients(): Promise<OidcClientsReply>;
  /**
   * Verify remote credentials on behalf of the host.
   *
   * @param params - Username and password.
   * @returns {@link ExternalUserReply}
   */
  authenticateUser(params: AuthenticateUserParams): Promise<ExternalUserReply>;
}

/**
 * One declared event consumer: a `[[events.consumers]]` row the default
 * entrypoint's `event(batch)` handler accepts.
 */
export interface EventConsumerSpec {
  /** Versioned event type (snake_case, e.g. `book_acquired`). */
  eventType: string;
  /** Schema versions the guest can consume; never empty. */
  schemaVersions: number[];
  /** Whether `EventResult.suspended` is supported for this type. */
  supportsSuspend: boolean;
}

/**
 * Typed capability declaration returned by `describe()`. The host compares
 * it with `plugin.toml` and the operator grant; widening is rejected at spawn.
 */
export interface PluginCapabilities {
  /** Named entrypoints the guest exports. */
  entrypoints: Entrypoint[];
  /** Event types the default entrypoint consumes (`event(batch)` trigger). */
  consumes: EventConsumerSpec[];
  /** Event types the guest may publish through its `EVENTS` binding. */
  produces: string[];
  /** Command types the default entrypoint runs (`job(controller)` trigger). */
  jobs: string[];
  /** Plugin-owned database binding names (`[[databases]]`). */
  databases: string[];
  /**
   * Other named bindings the guest expects on `env` (`CONFIG`, `SECRETS`,
   * `WORK_FS`, `OAUTH`, `KV`, `EVENTS`, ...).
   */
  bindings: string[];
}

/**
 * Portal brand crossing the RPC boundary. Distinct from `plugin.toml`
 * `logo`: `iconUrl` is the live URL or data URI the SPA renders.
 */
export interface Brand {
  /** Brand id (often matches the plugin id); empty means "no brand". */
  id: string;
  /** Display name shown next to the brand swatch. */
  name: string;
  /** Background CSS color (hex or named). */
  bg: string;
  /** Foreground CSS color for text on `bg`. */
  fg: string;
  /** Accent CSS color for highlights / CTAs. */
  accent: string;
  /**
   * Icon URL or data URI for the portal.
   * Omitted when absent (wire zero value).
   */
  iconUrl?: string;
}

/** One discoverable config option group advertised for sources. */
export interface ConfigOption {
  /** Config key under the plugin's `config.toml` table. */
  key: string;
  /** Operator-facing label for the option group. */
  label: string;
  /** Allowed selectable values for this key. */
  values: ConfigOptionValue[];
}

/** One selectable value under a `ConfigOption`. */
export interface ConfigOptionValue {
  /** Value written to config when selected. */
  id: string;
  /** Operator-facing label for this value. */
  label: string;
}

/** Declared plugin CLI surface (`PluginCli.describe` / `describe().cli`). */
export interface CliSchema {
  /** Commands exposed as `bookclerk plugins <id> <command> ...`. */
  commands: CliCommandSpec[];
}

/** One plugin CLI command under `CliSchema`. */
export interface CliCommandSpec {
  /** Command verb after the plugin id (for example "ping"). */
  name: string;
  /**
   * Short help text for `--help`.
   * Omitted when absent (wire zero value).
   */
  about?: string;
  /** Argument / flag specs for this command. */
  args: CliArgSpec[];
}

/** One CLI argument or flag under a `CliCommandSpec`. */
export interface CliArgSpec {
  /** Internal arg name used as `CliArg.name` on invoke. */
  name: string;
  /**
   * Long flag without leading dashes (e.g. "message" -> `--message`).
   * Omitted when absent (wire zero value).
   */
  long?: string;
  /**
   * Short flag character (e.g. "m" -> `-m`).
   * Omitted when absent (wire zero value).
   */
  short?: string;
  /** Parsed value kind. */
  kind: CliArgKind;
  /** When true, the host rejects invoke if the arg is missing. */
  required: boolean;
  /**
   * Default string form when the operator omits the arg.
   * Omitted when absent (wire zero value).
   */
  default?: string;
  /**
   * Help text for this arg.
   * Omitted when absent (wire zero value).
   */
  about?: string;
  /** When true, the arg is positional rather than a flagged option. */
  positional: boolean;
}

/** One named argument value passed to `PluginCli.invoke`. */
export interface CliArg {
  /** Arg name matching a `CliArgSpec.name`. */
  name: string;
  /** String form of the value (the guest parses per `CliArgSpec.kind`). */
  value: string;
}

/** Params of `PluginCli.invoke`. */
export interface CliInvokeParams {
  /** Command name matching a `CliCommandSpec.name`. */
  command: string;
  /** Named argument values. */
  args: CliArg[];
}

/** Result of `PluginCli.invoke`. */
export interface CliInvokeResult {
  /** Process-style exit code (0 = success). */
  exitCode: number;
  /** Captured standard output text. */
  stdout: string;
  /** Captured standard error text. */
  stderr: string;
  /** Structured payload for machine consumers; empty `mediaType` when absent. */
  payload: ExtensibleConfig;
}

/** Result union of `PluginCli.describe`. */
export type CliSchemaReply =
  | { kind: "ok"; value: CliSchema } // Success: declared CLI surface.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `PluginCli.invoke`. */
export type CliInvokeReply =
  | { kind: "ok"; value: CliInvokeResult } // Success: command output.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Author-facing database adapter configuration carried in
 * `DatabaseContext.adapter`. This is the generic bootstrap mechanism for
 * third-party adapters: the operator's granted `[database.<id>]` table plus
 * the scoped writable data dir. First-party host-managed adapters receive
 * host-private connect params in `DatabaseContext.config` instead.
 */
export interface DatabaseAdapterConfig {
  /** Scoped writable directory for this plugin (`.../plugins/<id>/data`). */
  pluginDataDir: string;
  /**
   * Granted plugin settings (operator `[database.<id>]` table) as
   * `application/json`; `{}` when the operator configured nothing.
   */
  settings: ExtensibleConfig;
  /**
   * Named plugin database binding this open serves; empty for the primary
   * library open. Adapters advertising `DbCapabilities.pluginDatabases` must
   * serve each binding from its own isolated database.
   * Omitted when absent (wire zero value).
   */
  binding?: string;
  /**
   * Host-issued opaque instance id for this (owner plugin, binding) pair.
   * Collision-resistant and stable across re-opens. Empty for the primary
   * library open. Third-party adapters must key isolated databases on this
   * value rather than `binding` alone (two plugins may both declare `DB`).
   * Omitted when absent (wire zero value).
   */
  instanceId?: string;
  /**
   * When true, open an existing binding unit and do not provision a missing
   * one (read-only backup capture). False lets the adapter create the unit.
   */
  openExisting: boolean;
}

/** Operator-facing diagnostic lines printed by `bookclerk plugins diagnose`. */
export interface DiagnoseResult {
  /** Human-readable probe lines. */
  lines: string[];
}

/** Result union of `diagnose`. */
export type DiagnoseReply =
  | { kind: "ok"; value: DiagnoseResult } // Success: diagnostic lines.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Source account metadata returned from login and stored by the host. */
export interface SourceAccount {
  /** Stable account id within this source plugin. */
  accountId: string;
  /** Source plugin id (the host forces this to the guest's install id). */
  source: string;
  /** Storefront marketplace / region code (for example `us`, `uk`). */
  marketplace: string;
  /**
   * Operator-facing label.
   * Omitted when absent (wire zero value).
   */
  label?: string;
  /**
   * When true, bare / scheduled scans include this account. Explicit CLI
   * `--account` bypasses this flag.
   */
  scanEnabled: boolean;
}

/**
 * Params of `ContentSource.login` and `ContentSource.loginStart`. Password
 * sources fill email/password; OAuth sources use callback / external fields.
 * There is no files-dir root or library DB path -- only `pluginDataDir`.
 */
export interface LoginParams {
  /** Scoped writable directory for this plugin only (`.../plugins/<id>/data`). */
  pluginDataDir: string;
  /** Marketplace / locale for the storefront; empty means the guest default. */
  marketplace: string;
  /**
   * Operator label stored on the account row.
   * Omitted when absent (wire zero value).
   */
  label?: string;
  /**
   * Account email / username for password logins; empty for pure OAuth.
   * Omitted when absent (wire zero value).
   */
  email?: string;
  /**
   * Account password for password logins; never logged; empty for OAuth.
   * Omitted when absent (wire zero value).
   */
  password?: string;
  /** When true, overwrite an existing sealed credential for this account. */
  force: boolean;
  /**
   * Bind address for OAuth callback servers (`host:port`). Ignored when
   * `callbackIpc` is set (host owns the TCP listener).
   * Omitted when absent (wire zero value).
   */
  callbackBind?: string;
  /**
   * Host-owned callback IPC endpoint the guest must connect to. When set
   * (with `callbackPublicBase`), the guest must not bind a TCP listener.
   * Omitted when absent (wire zero value).
   */
  callbackIpc?: string;
  /**
   * Public base URL for the host TCP listener, e.g. `http://127.0.0.1:12345`.
   * Omitted when absent (wire zero value).
   */
  callbackPublicBase?: string;
  /**
   * When true, use external / paste-redirect OAuth instead of a local
   * callback server.
   */
  external: boolean;
  /**
   * Pre-supplied OAuth redirect URL (paste flow).
   * Omitted when absent (wire zero value).
   */
  responseUrl?: string;
  /** Prefer QR output when the guest supports it. */
  showQr: boolean;
  /**
   * Seconds to wait for OAuth callback capture; guest default when 0.
   * Omitted when absent (wire zero value).
   */
  timeoutSecs?: number;
  /** Store-specific knobs as `application/json`; guests may ignore unknowns. */
  extra: ExtensibleConfig;
}

/**
 * Result of `ContentSource.login` / `loginComplete`: account metadata plus
 * opaque credentials for the host to seal into `encrypted_secrets`
 * (`provider = plugin id`). Guests never write secrets into the library DB.
 */
export interface LoginResult {
  /** Account row fields for the host to upsert. */
  account: SourceAccount;
  /**
   * Opaque credential blob the host seals; empty when login only refreshed
   * metadata. Guests choose the encoding (typically JSON bytes).
   * Omitted when absent (wire zero value).
   */
  credentials?: Uint8Array;
}

/**
 * Result of `ContentSource.loginStart` (interactive OAuth). The operator
 * opens `url`; `loginComplete` later uses `sessionId`.
 */
export interface LoginStartResult {
  /** Opaque session id for `loginComplete`. */
  sessionId: string;
  /** Browser URL the operator should open to complete OAuth. */
  url: string;
}

/** Params of `ContentSource.loginComplete`. */
export interface LoginCompleteParams {
  /** Session id previously returned by `loginStart`. */
  sessionId: string;
}

/** Result union of `ContentSource.login` / `loginComplete`. */
export type LoginReply =
  | { kind: "ok"; value: LoginResult } // Success: account plus credentials to seal.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `ContentSource.loginStart`. */
export type LoginStartReply =
  | { kind: "ok"; value: LoginStartResult } // Success: session id and browser URL.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Host-sealed credentials for one account, delivered on `scan`. */
export interface AccountCredential {
  /** Account id the blob belongs to. */
  accountId: string;
  /** Opaque credential bytes exactly as the guest returned them at login. */
  credentials: Uint8Array;
}

/**
 * Params of `ContentSource.scan`. The host injects sealed credentials so the
 * plugin does not need a private credential store under `pluginDataDir`.
 */
export interface ScanParams {
  /** Scoped plugin data directory. */
  pluginDataDir: string;
  /** Account ids to scan; empty means all scan-enabled accounts. */
  accounts: string[];
  /** Storefront page size; the host always sends an explicit value. */
  pageSize: number;
  /** When true, import podcast/episode-style rows. */
  importEpisodes: boolean;
  /** When true, import Plus/catalog entitlement titles. */
  importPlusTitles: boolean;
  /** Host-loaded credential blobs for the requested accounts. */
  credentials: AccountCredential[];
}

/**
 * One library title returned by `ContentSource.scan`. The host upserts these
 * rows and forces `source` to the plugin id.
 */
export interface ScanBook {
  /** Account that owns this library entry. */
  accountId: string;
  /** Storefront product / SKU id. */
  productId: string;
  /** Primary title string. */
  title: string;
  /**
   * Marketplace / region when known.
   * Omitted when absent (wire zero value).
   */
  marketplace?: string;
  /**
   * Amazon ASIN when the storefront exposes one.
   * Omitted when absent (wire zero value).
   */
  asin?: string;
  /**
   * ISBN when the storefront exposes one.
   * Omitted when absent (wire zero value).
   */
  isbn?: string;
  /**
   * Comma- or guest-formatted author list.
   * Omitted when absent (wire zero value).
   */
  authors?: string;
  /**
   * Comma- or guest-formatted narrator list.
   * Omitted when absent (wire zero value).
   */
  narrators?: string;
  /**
   * Series name when applicable.
   * Omitted when absent (wire zero value).
   */
  series?: string;
  /**
   * Series index / sequence label.
   * Omitted when absent (wire zero value).
   */
  seriesIndex?: string;
  /**
   * Content classification (e.g. `book` vs `episode`).
   * Omitted when absent (wire zero value).
   */
  contentKind?: string;
  /**
   * Publisher name when known.
   * Omitted when absent (wire zero value).
   */
  publisher?: string;
  /**
   * Runtime in whole minutes when known.
   * Omitted when absent (wire zero value).
   */
  lengthMinutes?: bigint;
  /**
   * Subtitle when distinct from `title`.
   * Omitted when absent (wire zero value).
   */
  subtitle?: string;
}

/** Summary result of `ContentSource.scan`. */
export interface ScanSummary {
  /** Number of accounts touched during the scan. */
  accounts: number;
  /**
   * Count of titles the guest expects the host to upsert; may mirror
   * `books.length`.
   */
  booksUpserted: number;
  /** Number of storefront pages fetched. */
  pages: number;
  /** Accounts skipped because `scanEnabled` was false. */
  skippedDisabled: number;
  /** Titles for the host to upsert. Prefer this over plugin-side DB writes. */
  books: ScanBook[];
}

/** Result union of `ContentSource.scan`. */
export type ScanReply =
  | { kind: "ok"; value: ScanSummary } // Success: scan summary and titles to upsert.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Fetch-relevant acquire knobs the host forwards so external load matches
 * in-process. Packaging / naming knobs stay host-side.
 */
export interface FetchOptions {
  /** Prefer Widevine/CENC download when the store offers it. */
  widevine: boolean;
  /** Prefer xHE-AAC on the Widevine path when offered. */
  xheAac: boolean;
  /**
   * Local Widevine `.wvd` path granted to the guest.
   * Omitted when absent (wire zero value).
   */
  widevineCdmPath?: string;
  /**
   * Remote L3 CDM provider URL; empty means the classic default, `off`
   * disables remote provisioning.
   * Omitted when absent (wire zero value).
   */
  widevineCdmProvider?: string;
  /** When true, download a cover image alongside audio. */
  downloadCover: boolean;
  /** When true, download a companion PDF when the store exposes one. */
  downloadPdf: boolean;
  /** Cover image size request (`500`, `1215`, or `native`). */
  coverSize: string;
  /** Preferred chapter API layout when fetching (`tree` or `flat`). */
  chapterLayout: string;
  /** When true, trim Audible brand intro/outro from the remux window. */
  stripAudibleBrandAudio: boolean;
  /** When true, download clips/bookmarks sidecars when offered. */
  downloadClipsBookmarks: boolean;
  /** When true, keep the encrypted download in storage. */
  retainAaxFile: boolean;
  /** Fetch speed cap in KB/s (`0` = unlimited). */
  downloadSpeedLimitKbps: number;
  /** When true, persist raw catalog API JSON as `metadata.json`. */
  saveMetadataJson: boolean;
}

/**
 * Params of `ContentSource.fetchTitle`. The plugin writes media under
 * `cacheDir` and returns plain (DRM-free) paths. The host injects
 * credentials; guests must not open `library.db` or `master.key`.
 */
export interface FetchTitleParams {
  /** Scoped plugin data directory. */
  pluginDataDir: string;
  /** Account whose credentials apply. */
  accountId: string;
  /** Library / storefront title id to download. */
  titleId: string;
  /** Absolute path the guest should write media into (jail-granted TMPDIR). */
  cacheDir: string;
  /**
   * Host-loaded credential blob for this account; empty when unavailable.
   * Omitted when absent (wire zero value).
   */
  credentials?: Uint8Array;
  /** Granted `[sources.<id>]` table as `application/json`. */
  sourceConfig: ExtensibleConfig;
  /** Fetch-relevant acquire options. */
  fetch: FetchOptions;
}

/** One plain audio part written under the cache directory. */
export interface PlainPart {
  /** Absolute path to the part file under `cacheDir`. */
  path: string;
  /**
   * Part title (disc / chapter label).
   * Omitted when absent (wire zero value).
   */
  title?: string;
  /**
   * Duration of this part in milliseconds when known.
   * Omitted when absent (wire zero value).
   */
  durationMs?: number;
}

/** One chapter marker of a fetched title. */
export interface ChapterMarker {
  /** Chapter title. */
  title: string;
  /** Chapter start offset in milliseconds from the beginning of the title. */
  startMs: number;
}

/**
 * Plain (DRM-free) fetch result. Sources always return decrypted media; DRM
 * guests decrypt before responding.
 */
export interface PlainFetch {
  /** Ordered audio part files written under the cache directory. */
  parts: PlainPart[];
  /**
   * Single M4B path when the guest assembled one.
   * Omitted when absent (wire zero value).
   */
  m4bPath?: string;
  /**
   * Cover image path under the cache directory.
   * Omitted when absent (wire zero value).
   */
  coverPath?: string;
  /** Chapter markers; empty when unknown. */
  chapters: ChapterMarker[];
  /**
   * Companion PDF download URL when the store exposes one.
   * Omitted when absent (wire zero value).
   */
  pdfUrl?: string;
}

/** Result union of `ContentSource.fetchTitle`. */
export type FetchTitleReply =
  | { kind: "ok"; value: PlainFetch } // Success: plain media paths.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `ContentSource.listAccounts`. */
export interface SourceAccounts {
  /** Accounts the guest knows about. */
  accounts: SourceAccount[];
}

/** Result union of `ContentSource.listAccounts`. */
export type SourceAccountsReply =
  | { kind: "ok"; value: SourceAccounts } // Success: account list.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Params of `ContentSource.searchCatalog`. */
export interface SearchCatalogParams {
  /** Free-text search query. */
  query: string;
  /** Storefront region / marketplace code; empty means the guest default. */
  region: string;
  /** Maximum hits to return; the host always sends an explicit value. */
  limit: number;
  /** 1-based page for storefronts that page. */
  page: number;
  /** Sort order. */
  sort: CatalogSort;
  /** Facet restriction. */
  field: CatalogField;
  /**
   * Preferred content language (soft-prioritize; e.g. `en`).
   * Omitted when absent (wire zero value).
   */
  language?: string;
}

/**
 * Params of `ContentSource.expandCandidates`. Seed fields identify a known
 * title; the guest returns related catalog hits.
 */
export interface ExpandCandidatesParams {
  /** Source plugin id hint when expanding across storefronts. */
  source: string;
  /** Seed storefront product id. */
  productId: string;
  /** Seed title text. */
  title: string;
  /**
   * Seed authors string.
   * Omitted when absent (wire zero value).
   */
  authors?: string;
  /**
   * Seed narrators string.
   * Omitted when absent (wire zero value).
   */
  narrators?: string;
  /**
   * Seed series name.
   * Omitted when absent (wire zero value).
   */
  series?: string;
  /**
   * Seed series ASIN when known.
   * Omitted when absent (wire zero value).
   */
  seriesAsin?: string;
  /**
   * Seed Amazon ASIN.
   * Omitted when absent (wire zero value).
   */
  asin?: string;
  /**
   * Seed ISBN.
   * Omitted when absent (wire zero value).
   */
  isbn?: string;
  /** Storefront region / marketplace code. */
  region: string;
  /** Maximum candidates to return; the host always sends an explicit value. */
  limit: number;
}

/**
 * Params of `ContentSource.purchaseHint`. At least one identity field
 * (`productId` / `asin` / `isbn` / title+authors) should be set; guests may
 * return `invalid_params` when none are usable.
 */
export interface PurchaseHintParams {
  /**
   * Storefront product id when known.
   * Omitted when absent (wire zero value).
   */
  productId?: string;
  /**
   * Title text for fuzzy lookup.
   * Omitted when absent (wire zero value).
   */
  title?: string;
  /**
   * Authors string for fuzzy lookup.
   * Omitted when absent (wire zero value).
   */
  authors?: string;
  /**
   * Amazon ASIN when known.
   * Omitted when absent (wire zero value).
   */
  asin?: string;
  /**
   * ISBN when known.
   * Omitted when absent (wire zero value).
   */
  isbn?: string;
  /** Storefront region / marketplace code. */
  region: string;
  /** When true, guests should include live price fields when available. */
  withPrice: boolean;
}

/** Params of `ContentSource.listDeals`. */
export interface ListDealsParams {
  /**
   * Maximum number of deals to return; guest default when 0.
   * Omitted when absent (wire zero value).
   */
  limit?: number;
}

/** Params of `ContentSource.catalogDetail`. */
export interface CatalogDetailParams {
  /** Store product id (Libro ISBN or ISBN-slug). */
  productId: string;
  /**
   * ISBN when it differs from `productId`.
   * Omitted when absent (wire zero value).
   */
  isbn?: string;
}

/**
 * One catalog / candidate hit returned by `searchCatalog`,
 * `expandCandidates`, `listDeals`, and `catalogDetail`.
 */
export interface CatalogHit {
  /** Storefront product / SKU id. */
  productId: string;
  /** Primary title. */
  title: string;
  /**
   * Authors string when known.
   * Omitted when absent (wire zero value).
   */
  authors?: string;
  /**
   * Narrators string when known.
   * Omitted when absent (wire zero value).
   */
  narrators?: string;
  /**
   * Series name when applicable.
   * Omitted when absent (wire zero value).
   */
  series?: string;
  /**
   * Series index / sequence label.
   * Omitted when absent (wire zero value).
   */
  seriesIndex?: string;
  /**
   * Amazon ASIN when known.
   * Omitted when absent (wire zero value).
   */
  asin?: string;
  /**
   * ISBN when known.
   * Omitted when absent (wire zero value).
   */
  isbn?: string;
  /**
   * Storefront product page URL.
   * Omitted when absent (wire zero value).
   */
  url?: string;
  /**
   * Cover image URL.
   * Omitted when absent (wire zero value).
   */
  coverUrl?: string;
  /** Hit origin label (plugin id or storefront name). */
  origin: string;
  /**
   * Subtitle when distinct from `title`.
   * Omitted when absent (wire zero value).
   */
  subtitle?: string;
  /**
   * Long description / blurb when fetched.
   * Omitted when absent (wire zero value).
   */
  description?: string;
  /**
   * Publisher name when known.
   * Omitted when absent (wire zero value).
   */
  publisher?: string;
  /**
   * Runtime in whole minutes.
   * Omitted when absent (wire zero value).
   */
  lengthMinutes?: bigint;
  /**
   * Publication date string as provided by the storefront.
   * Omitted when absent (wire zero value).
   */
  publishedAt?: string;
  /**
   * Category / genre labels as a single string when known.
   * Omitted when absent (wire zero value).
   */
  categories?: string;
  /**
   * Content language code when known.
   * Omitted when absent (wire zero value).
   */
  language?: string;
  /**
   * Current price in minor units (cents).
   * Omitted when absent (wire zero value).
   */
  priceCents?: bigint;
  /**
   * ISO currency code for `priceCents`.
   * Omitted when absent (wire zero value).
   */
  currency?: string;
  /**
   * Pre-formatted price for display.
   * Omitted when absent (wire zero value).
   */
  priceLabel?: string;
  /**
   * Aggregate rating when known.
   * Omitted when absent (wire zero value).
   */
  ratingOverall?: number;
  /**
   * Number of ratings when known.
   * Omitted when absent (wire zero value).
   */
  ratingCount?: bigint;
  /** Whether the edition is abridged when the storefront says so. */
  abridgement: Abridgement;
}

/** Success payload of the catalog list methods. */
export interface CatalogHits {
  /** Hits in storefront order. */
  hits: CatalogHit[];
}

/** Result union of `searchCatalog` / `expandCandidates` / `listDeals`. */
export type CatalogHitsReply =
  | { kind: "ok"; value: CatalogHits } // Success: catalog hits.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `ContentSource.catalogDetail`. */
export interface CatalogDetail {
  /** False when the product is unknown to the storefront (`hit` is empty). */
  found: boolean;
  /** Full catalog record when `found`. */
  hit: CatalogHit;
}

/** Result union of `ContentSource.catalogDetail`. */
export type CatalogDetailReply =
  | { kind: "ok"; value: CatalogDetail } // Success: detail record or not-found marker.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Purchase hint for SPA / CLI purchase deep-links. */
export interface PurchaseHint {
  /** Storefront product id. */
  productId: string;
  /**
   * Title when resolved.
   * Omitted when absent (wire zero value).
   */
  title?: string;
  /**
   * Purchase or product-page URL.
   * Omitted when absent (wire zero value).
   */
  url?: string;
  /**
   * Current price in minor units.
   * Omitted when absent (wire zero value).
   */
  priceCents?: bigint;
  /**
   * ISO currency code for price fields.
   * Omitted when absent (wire zero value).
   */
  currency?: string;
  /**
   * Pre-formatted current price.
   * Omitted when absent (wire zero value).
   */
  priceLabel?: string;
  /**
   * List / MSRP price in minor units.
   * Omitted when absent (wire zero value).
   */
  listPriceCents?: bigint;
  /**
   * Pre-formatted list price.
   * Omitted when absent (wire zero value).
   */
  listPriceLabel?: string;
  /**
   * Member / Plus price in minor units.
   * Omitted when absent (wire zero value).
   */
  memberPriceCents?: bigint;
  /**
   * Pre-formatted member price.
   * Omitted when absent (wire zero value).
   */
  memberPriceLabel?: string;
}

/** Success payload of `ContentSource.purchaseHint`. */
export interface PurchaseHintResult {
  /** False when the guest could not resolve the title (`hint` is empty). */
  found: boolean;
  /** Purchase hint when `found`. */
  hint: PurchaseHint;
}

/** Result union of `ContentSource.purchaseHint`. */
export type PurchaseHintReply =
  | { kind: "ok"; value: PurchaseHintResult } // Success: hint or not-found marker.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Params of `RemoteLibrary.scanLibrary` (remote library sync). */
export interface ScanLibraryParams {
  /**
   * When true, force a full rescan even if the guest would otherwise
   * incremental-sync.
   */
  force: boolean;
}

/** Params of `Oidc.authenticateUser`. */
export interface AuthenticateUserParams {
  /** Integration username / login id. */
  username: string;
  /** Integration password; never logged by the host. */
  password: string;
}

/**
 * One external user observed by an integration. The host may mint claim
 * tickets without exposing portal details to the guest.
 */
export interface ExternalUser {
  /** Integration provider id (often the plugin id). */
  provider: string;
  /** Provider-scoped user id. */
  externalUserId: string;
  /**
   * Display name for UI.
   * Omitted when absent (wire zero value).
   */
  displayName?: string;
  /**
   * Ephemeral remote token (e.g. ABS JWT). Guest-to-host only; never
   * persisted.
   * Omitted when absent (wire zero value).
   */
  accessToken?: string;
}

/** Result union of `Oidc.authenticateUser`. */
export type ExternalUserReply =
  | { kind: "ok"; value: ExternalUser } // Success: verified external user.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Success payload of `RemoteLibrary.pollEvents`: signals for the host to kick
 * off workflows.
 */
export interface EventPollResult {
  /** Newly observed external users since the last poll. */
  users: ExternalUser[];
}

/** Result union of `RemoteLibrary.pollEvents`. */
export type EventPollReply =
  | { kind: "ok"; value: EventPollResult } // Success: observed users.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * One listening-progress row. The host upserts into `listening_progress`
 * tagged with the plugin id; plugins never open the library DB.
 */
export interface ListeningProgress {
  /** Provider-scoped user id. */
  externalUserId: string;
  /** Provider-scoped item / library id. */
  externalItemId: string;
  /**
   * Bookclerk identity row id when already linked.
   * Omitted when absent (wire zero value).
   */
  identityId?: bigint;
  /**
   * Title text when known.
   * Omitted when absent (wire zero value).
   */
  title?: string;
  /**
   * Authors string when known.
   * Omitted when absent (wire zero value).
   */
  authors?: string;
  /**
   * Amazon ASIN when known.
   * Omitted when absent (wire zero value).
   */
  asin?: string;
  /**
   * ISBN when known.
   * Omitted when absent (wire zero value).
   */
  isbn?: string;
  /**
   * Fractional progress in `0.0..=1.0` when the provider reports it.
   * Omitted when absent (wire zero value).
   */
  progress?: number;
  /**
   * Current playback position in seconds.
   * Omitted when absent (wire zero value).
   */
  currentTimeSeconds?: number;
  /**
   * Total duration in seconds when known.
   * Omitted when absent (wire zero value).
   */
  durationSeconds?: number;
  /** When true, the provider marks the item finished. */
  isFinished: boolean;
  /**
   * Last listen timestamp as unix milliseconds (UTC).
   * Omitted when absent (wire zero value).
   */
  lastListenedAtUnixMs?: number;
}

/** Success payload of `RemoteLibrary.syncListening`. */
export interface SyncListeningResult {
  /** Progress snapshots to upsert. */
  items: ListeningProgress[];
}

/** Result union of `RemoteLibrary.syncListening`. */
export type SyncListeningReply =
  | { kind: "ok"; value: SyncListeningResult } // Success: progress snapshots.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Guest-published domain event (`EventPublisher.publish`). The host stamps
 * `eventId`, `source` (the plugin id), and `accountId` from the invocation;
 * guests cannot forge either.
 */
export interface PluginEvent {
  /** Snake_case event type; must be listed in `[[events.producers]]`. */
  eventType: string;
  /** Schema version of `payload`, owned by the event type. */
  schemaVersion: number;
  /**
   * Producer idempotency key, unique per (account, source, eventType) in the
   * outbox; a repeat returns `PublishOk.duplicate = true` with the earlier id.
   * Empty publishes unconditionally.
   */
  deduplicationKey: string;
  /** Encoded event payload; at most `maxEventPayloadBytes`. */
  payload: Uint8Array;
  /** When the producer observed the fact; zero means "now" on the host clock. */
  occurredAtUnixMs: number;
  /** Trace correlation id; empty inherits `Invocation.correlationId`. */
  correlationId: string;
  /**
   * Id of the event or command that caused this one; empty inherits
   * `Invocation.causationId`.
   */
  causationId: string;
}

/** Success payload of `EventPublisher.publish`. */
export interface PublishOk {
  /** Outbox event id (new or the earlier row when `duplicate`). */
  eventId: string;
  /** True when `deduplicationKey` matched an existing outbox row. */
  duplicate: boolean;
}

/** Result union of `EventPublisher.publish`. */
export type PublishReply =
  | { kind: "ok"; value: PublishOk } // Success: outbox row identity.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Identity of one `PluginWorker.open` invocation. The host issues ids; guests
 * echo `correlationId` / `causationId` onto published events.
 */
export interface Invocation {
  /** Unique host-issued invocation id. */
  id: string;
  /** Account scope; empty for operator / host-wide invocations. */
  accountId: string;
  /**
   * UTC Unix milliseconds; zero when the invocation has no deadline. The host
   * fence is authoritative.
   */
  deadlineUnixMs: number;
  /** Trace correlation id; empty when none. */
  correlationId: string;
  /** Id of the event or command that caused this invocation; empty when none. */
  causationId: string;
}

/** One typed SQL cell or bind parameter. */
export type DbValue =
  | { kind: "null"; value: DbType } // SQL NULL with its declared type.
  | { kind: "boolean"; value: boolean } // Boolean.
  | { kind: "int64"; value: bigint } // Signed 64-bit integer.
  | { kind: "float64"; value: number } // IEEE-754 double.
  | { kind: "text"; value: string } // UTF-8 text.
  | { kind: "bytes"; value: Uint8Array }; // Raw bytes.

/** Result column descriptor. */
export interface DbColumn {
  /** Column name as projected. */
  name: string;
  /** Declared or inferred column type. */
  dbType: DbType;
}

/** One result row. */
export interface DbRow {
  /** Cells in `columns` order. */
  values: DbValue[];
}

/** One guest statement in an `ExecuteRequest`. */
export interface DbStatement {
  /** BookclerkSQL text; at most `maxScalarBytes`. */
  sql: string;
  /** Positional bind values. */
  parameters: DbValue[];
  /** Statement classification. */
  kind: DbStatementKind;
  /** Row cap for queries; `0` means adapter default. */
  maxRows: number;
  /** Which outcome parts to return. */
  resultSelection: DbResultSelection;
}

/** Guest statement batch for `GuestDatabase.execute`; runs atomically. */
export interface ExecuteRequest {
  /** Caller-chosen idempotency key. */
  operationId: string;
  /** SHA-256 hex of the idempotency-relevant request; empty when omitted. */
  requestHash: string;
  /** Statements in execution order; at most `maxStatements`. */
  statements: DbStatement[];
  /** Deadline hint; `0` means none. */
  deadlineUnixMs: number;
}

/** Byte span in the exact canonical SQL string a proof is bound to. */
export interface SqlSpan {
  /** Inclusive start byte offset. */
  start: number;
  /** Exclusive end byte offset. */
  end: number;
}

/** One TEXT expression the adapter must collate bytewise (`COLLATE "C"`). */
export interface TextCollateSite {
  /** Identifier or string-literal span in canonical SQL. */
  span: SqlSpan;
}

/** One INTEGER arithmetic expression that must not wrap or error on overflow. */
export interface IntegerArithSite {
  /** Full expression span (`a + b` or `abs(n)`). */
  full: SqlSpan;
  /** Left operand (or `abs` argument). */
  lhs: SqlSpan;
  /** Right operand (`abs` repeats `lhs`). */
  rhs: SqlSpan;
  /** Operator. */
  kind: IntegerArithKind;
}

/** Physical table/column access used for authorization. */
export interface PhysicalAccess {
  /** Physical table name. */
  table: string;
  /** Empty = table presence only; "*" = projection wildcard. */
  column: string;
}

/** Destination assignment `lhs = rhs` (INSERT, UPDATE, ...). */
export interface ResolvedAssignment {
  /** Target table. */
  table: string;
  /** Target column. */
  column: string;
  /** Declared column type. */
  dest: ResolvedSqlType;
  /** Resolved type of the assigned expression. */
  source: ResolvedSqlType;
}

/** Column name paired with its resolved type. */
export interface NamedSqlType {
  /** Column name. */
  name: string;
  /** Resolved type. */
  sqlType: ResolvedSqlType;
}

/** `REFERENCES` target of one column. */
export interface ColumnReference {
  /** Referenced table. */
  refTable: string;
  /** Referenced columns. */
  refColumns: string[];
}

/** Per-column `REFERENCES` slot in `CreateTableSchema`. */
export type OptionalColumnReference =
  | { kind: "none" } // Column has no reference.
  | { kind: "some"; value: ColumnReference }; // Column references another table.

/** Table-level `FOREIGN KEY` constraint. */
export interface ForeignKeyConstraint {
  /** Local columns. */
  columns: string[];
  /** Referenced table. */
  refTable: string;
  /** Referenced columns. */
  refColumns: string[];
}

/** One table-level constraint. */
export type TableConstraint =
  | { kind: "primaryKey"; value: string[] } // `PRIMARY KEY (...)` columns.
  | { kind: "unique"; value: string[] } // `UNIQUE (...)` columns.
  | { kind: "check"; value: string } // `CHECK (...)` expression text.
  | { kind: "foreignKey"; value: ForeignKeyConstraint }; // `FOREIGN KEY (...) REFERENCES ...`.

/** Parsed `CREATE TABLE` (canonical SQL v1). Per-column lists align with `columns`. */
export interface CreateTableSchema {
  /** Table name. */
  table: string;
  /** Columns with resolved types, in order. */
  columns: NamedSqlType[];
  /** `INTEGER PRIMARY KEY AUTOINCREMENT` column; empty when none. */
  identityColumn: string;
  /** Per-column NOT NULL flags. */
  columnNotNull: boolean[];
  /** Per-column UNIQUE flags. */
  columnUnique: boolean[];
  /** Per-column PRIMARY KEY flags. */
  columnPrimaryKey: boolean[];
  /** Per-column DEFAULT expression text; empty when none. */
  columnDefaults: string[];
  /** Per-column CHECK expression text; empty when none. */
  columnChecks: string[];
  /** Per-column REFERENCES targets. */
  columnReferences: OptionalColumnReference[];
  /** Table-level constraints, in order. */
  tableConstraints: TableConstraint[];
}

/** `CREATE TABLE` action with its durable fingerprint. */
export interface SchemaCreate {
  /** Parsed table schema. */
  schema: CreateTableSchema;
  /** Structured schema fingerprint (hex SHA-256). */
  fingerprint: string;
  /** True when the catalog already holds this exact fingerprint. */
  noop: boolean;
}

/** CREATE/DROP action recorded on a proof. */
export type SchemaAction =
  | { kind: "none" } // Not DDL.
  | { kind: "create"; value: SchemaCreate } // `CREATE TABLE`.
  | { kind: "drop"; value: string }; // `DROP TABLE` of the named table.

/** Host-produced typed proof bound to one exact canonical statement. */
export interface ResolvedStatement {
  /** SHA-256 hex of the exact canonical SQL this proof claims. */
  statementHash: string;
  /** SELECT / RETURNING / VALUES output columns in order. */
  outputColumns: NamedSqlType[];
  /** Physical tables/columns referenced (authorization). */
  physicalAccesses: PhysicalAccess[];
  /** Mutation destination assignments. */
  assignments: ResolvedAssignment[];
  /** TEXT expression spans needing bytewise collation. */
  textCollateSites: TextCollateSite[];
  /** INTEGER overflow sites (`+` `-` `*` `abs`). */
  integerArithSites: IntegerArithSite[];
  /** Function names invoked (folded), for authorization. */
  functions: string[];
  /** DDL action + fingerprint. */
  schemaAction: SchemaAction;
}

/** Replay receipt the host asks the adapter to persist with the batch. */
export interface AdapterReceipt {
  /** Guest statement count inside the receipt wrap (excluding host prune/select). */
  guestLen: number;
  /** Guest `requestHash` compared on replay. */
  guestHash: string;
}

/** One host-lowered statement with its proof. */
export interface AdapterStatement {
  /** Canonical SQL text. */
  sql: string;
  /** Positional bind values. */
  parameters: DbValue[];
  /** Statement classification. */
  kind: DbStatementKind;
  /** Row cap for queries; `0` means adapter default. */
  maxRows: number;
  /** Which outcome parts to return. */
  resultSelection: DbResultSelection;
  /** Typed proof bound to `sql`. */
  proof: ResolvedStatement;
}

/** Host -> adapter batch: proven statements plus isolation and receipt. */
export interface AdapterExecuteRequest {
  /** Host-chosen idempotency key. */
  operationId: string;
  /** SHA-256 hex of the idempotency-relevant request. */
  requestHash: string;
  /** Statements in execution order. */
  statements: AdapterStatement[];
  /** Deadline hint; `0` means none. */
  deadlineUnixMs: number;
  /** Transaction isolation the adapter must realize. */
  isolation: IsolationReq;
  /** Replay receipt to persist; zero/empty when not required. */
  receipt: AdapterReceipt;
}

/** Outcome of one statement. */
export interface StatementResult {
  /** Result rows (empty unless `rows` selected). */
  rows: DbRow[];
  /** Result column descriptors. */
  columns: DbColumn[];
  /** Rows changed by a mutation. */
  rowsAffected: number;
}

/** Engine timing on `ExecuteReply`. */
export interface DbTiming {
  /** Monotonic duration of this handler attempt (microseconds). */
  attemptElapsedUs: number;
  /** Engine-reported SQL/transaction time when available (`0` = omitted). */
  dbExecutionUs: number;
  /** How `dbExecutionUs` was measured. */
  dbTimingSource: string;
}

/** Success payload of `execute`. */
export interface ExecuteReply {
  /** Echo of the request `operationId`. */
  operationId: string;
  /** Per-statement results in request order. */
  statements: StatementResult[];
  /** Engine timing. */
  timing: DbTiming;
}

/** Result union of `execute`. */
export type ExecuteResultReply =
  | { kind: "ok"; value: ExecuteReply } // Success: statement results.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Semantic SQL-contract advertisement. Diagnostic engine identity is not
 * part of the capability plane — see `DbBootstrap`.
 */
export interface DbCapabilities {
  /** Bookclerk SQL contract version. */
  sqlContractVersion: number;
  /** Guest can run a bounded statement list as one SQL transaction. */
  atomicBatch: boolean;
  /** Guest SQL supports `RETURNING`. */
  returning: boolean;
  /** Guest reports `rowsAffected`. */
  affectedRows: boolean;
  /** Guest versions schema with a `bookclerk_schema_migrations` table. */
  schemaMigrations: boolean;
  /** Guest honors RPC/session cancellation. */
  cancellation: boolean;
  /** Guest can fill `DbTiming.dbExecutionUs`. Not a host connect minimum. */
  timing: boolean;
  /** Maximum bound parameters per statement. */
  maxBinds: number;
  /** Maximum statements in one atomic batch. */
  maxStatements: number;
  /** Maximum rows a query statement may return. */
  maxResultRows: number;
  /** Maximum UTF-8 bytes of SQL plus binds per statement. */
  maxPayloadBytes: number;
  /** Maximum encoded bytes of one statement's result rows. */
  maxResultBytes: number;
  /** Maximum UTF-8 / blob bytes of one result cell. */
  maxCellBytes: number;
  /** Maximum encoded bytes of one `ExecuteRequest`. */
  maxRequestBytes: number;
  /** Maximum encoded bytes of one `ExecuteReply`. */
  maxAtomicResultBytes: number;
  /**
   * Append-only. Adapter can open additional isolated sessions
   * for plugin-owned database bindings (per-binding file / schema / database).
   */
  pluginDatabases: boolean;
  /**
   * Maximum arguments in one physical function call after adapter hiding
   * (nested min / max / coalesce). Portable json_object is already ≤ 32
   * arguments (16 pairs), matching D1. `0` is unspecified.
   */
  maxFunctionArgs: number;
  /** Maximum columns in one CREATE TABLE / result row. `0` is unspecified. */
  maxSchemaColumns: number;
  /**
   * Maximum UTF-8 bytes of a BookclerkSQL LIKE pattern value (literals and
   * TEXT binds). Adapters that expand LIKE into GLOB must advertise a
   * conservative value that still fits the physical pattern cap. `0` is
   * unspecified.
   */
  maxPatternBytes: number;
  /**
   * Maximum UTF-8 bytes of sqlite-family lowered SQL the adapter can realize
   * for one statement (INTEGER overflow wraps, LIKE→GLOB, NULLIF, INSERT OR
   * IGNORE, query LIMIT wrap, bytes-placeholder expansion). `0` is
   * unspecified: the host does not enforce a lowered-size ceiling. First-party
   * D1 advertises 100000. Hosts compare a standardized Bookclerk lowering
   * upper bound against this number and must not branch on engine identity.
   */
  maxLoweredStatementBytes: number;
  /**
   * Adapter can expose one stable logical database state while the host
   * reads schema, rows, and identity.
   */
  consistentBackupRead: boolean;
  /**
   * Adapter can destructively replace one logical database unit so an
   * ordinary restore failure does not leave that unit partially replaced.
   */
  atomicUnitRestore: boolean;
}

/** Result union of `AdapterDatabaseSession.bootstrap`. */
export type DbBootstrapReply =
  | { kind: "ok"; value: DbBootstrap } // Success: bootstrap metadata.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Bootstrap-only diagnostic metadata (not a capability). */
export interface DbBootstrap {
  /**
   * Diagnostic physical engine name. Hosts must not admit or generate SQL
   * from this value. Any string is valid.
   */
  engine: string;
}

/** Result union of `AdapterDatabaseSession.capabilities`. */
export type DbCapabilitiesReply =
  | { kind: "ok"; value: DbCapabilities } // Success: capability advertisement.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** `databaseAdapter` entrypoint (`Entrypoints.databaseAdapter`). */
export interface Database {
  /**
   * Open one adapter session (capability negotiation + typed execute).
   *
   * @returns {@link AdapterSessionReply}
   */
  openSession(): Promise<AdapterSessionReply>;
}

/**
 * Adapter-private identity high-water (sqlite_sequence / bookclerk_identity).
 * Column names live in the canonical backup schema, not this catalog.
 */
export interface IdentityHighWater {
  /** Table whose identity column the mark belongs to. */
  table: string;
  /** Highest generated or stored value that must not be reused. */
  last: bigint;
}

/** Result union of `AdapterDatabaseSession.exportIdentity`. */
export type IdentityExportReply =
  | { kind: "ok"; value: IdentityHighWater[] } // Success: identity high-water rows.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `AdapterDatabaseSession.listUserRelations`. */
export type UserRelationsReply =
  | { kind: "ok"; value: string[] } // Success: user relation names.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Host ↔ database adapter plugin. Capability negotiation + typed execute only. */
export interface AdapterDatabaseSession {
  /**
   * Semantic SQL-contract advertisement for this session.
   *
   * @returns {@link DbCapabilitiesReply}
   */
  capabilities(): Promise<DbCapabilitiesReply>;
  /**
   * Canonical SQL + required structured proofs (not JSON).
   *
   * @param request - Proven statements plus isolation and receipt.
   * @returns {@link ExecuteResultReply}
   */
  execute(request: AdapterExecuteRequest): Promise<ExecuteResultReply>;
  /**
   * Release the session; further calls fail.
   *
   * @returns {@link EmptyReply}
   */
  close(): Promise<EmptyReply>;
  /**
   * Bootstrap-only SeaORM proxy metadata (not part of DbCapabilities).
   *
   * @returns {@link DbBootstrapReply}
   */
  bootstrap(): Promise<DbBootstrapReply>;
  /**
   * Snapshot/identity/restore primitives (not a SQL dialect API).
   *
   * @returns {@link IdentityExportReply}
   */
  exportIdentity(): Promise<IdentityExportReply>;
  /**
   * Restore identity high-water marks captured by `exportIdentity`.
   *
   * @param rows - High-water rows to apply.
   * @returns {@link EmptyReply}
   */
  importIdentity(rows: IdentityHighWater[]): Promise<EmptyReply>;
  /**
   * Names of user relations present in the logical database unit.
   *
   * @returns {@link UserRelationsReply}
   */
  listUserRelations(): Promise<UserRelationsReply>;
  /**
   * Enter restore mode for one logical unit (`atomicUnitRestore`).
   *
   * @returns {@link EmptyReply}
   */
  prepareUnitRestore(): Promise<EmptyReply>;
  /**
   * Drop the named user relations during restore.
   *
   * @param names - Relation names from `listUserRelations`.
   * @returns {@link EmptyReply}
   */
  dropUserRelations(names: string[]): Promise<EmptyReply>;
  /**
   * Verify constraints hold after restore rows were written.
   *
   * @returns {@link EmptyReply}
   */
  assertRestoreConstraints(): Promise<EmptyReply>;
}

/**
 * Host-granted SQL for job plugin authors. SDK `DatabaseBinding` mirrors the
 * Cloudflare Workers D1 surface (`prepare`/`bind`/`run`/`first`/`all`/`raw`,
 * `batch`, `exec`) over this typed `execute` transport; wire types stay Cap'n
 * `ExecuteRequest`/`ExecuteReply`.
 */
export interface GuestDatabase {
  /**
   * Run one typed statement batch.
   *
   * @param request - Statements, binds, and deadline.
   * @returns {@link ExecuteResultReply}
   */
  execute(request: ExecuteRequest): Promise<ExecuteResultReply>;
  /**
   * Release the session; further calls fail.
   *
   * @returns {@link EmptyReply}
   */
  close(): Promise<EmptyReply>;
}

/**
 * One already-separated BookclerkSQL operation in a plugin-owned migration.
 * `schema` is admitted DDL; `data` is admitted DML. There is no native SQL
 * escape hatch.
 */
export type PluginMigrationOp =
  | { kind: "schema"; value: string } // Admitted DDL statement text.
  | { kind: "data"; value: string }; // Admitted DML statement text.

/**
 * One plugin-owned migration application. `id` is an opaque plugin-chosen
 * stable identity (name, UUID, timestamp-like string, or digits-as-text).
 * Bookclerk assigns no order, version, or predecessor meaning to `id`.
 * Registration order is the forward sequence.
 */
export interface PluginMigration {
  /** Opaque plugin-chosen stable identity. */
  id: string;
  /** Operations in forward order; at most `maxPluginMigrationOps`. */
  operations: PluginMigrationOp[];
}

/** Success payload of `PluginWorker.databaseMigrations`. */
export interface PluginMigrationsOk {
  /**
   * At most `maxListPage` entries; aggregate id+SQL bytes at most
   * `maxPluginMigrationRegistrationBytes`; total operations at most
   * `maxPluginMigrationTotalOps`.
   */
  migrations: PluginMigration[];
}

/** Result union of `PluginWorker.databaseMigrations`. */
export type PluginMigrationsReply =
  | { kind: "ok"; value: PluginMigrationsOk } // Success: ordered migration sequence.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Plugin bootstrap capability: the guest's root object (Workers `default`
 * export analogue). `describe()` first; `open()` once per invocation.
 */
export interface PluginWorker {
  /**
   * Identity, ABI version, negotiated features, and typed capabilities.
   *
   * @returns {@link DescribeReply}
   */
  describe(): Promise<DescribeReply>;
  /**
   * Open the exported entrypoints for one invocation with host-granted
   * bindings. Entrypoints the manifest does not export are null.
   *
   * @param invocation - Host-issued invocation identity.
   * @param bindings - Granted `env` bindings.
   * @returns {@link EntrypointsReply}
   */
  open(invocation: Invocation, bindings: Bindings): Promise<EntrypointsReply>;
  /**
   * Flush and release resources before the process exits.
   *
   * @returns {@link EmptyReply}
   */
  shutdown(): Promise<EmptyReply>;
  /**
   * Complete ordered plugin-owned migration sequence for one named binding.
   * The host calls this before `open` while provisioning `[[databases]]`
   * (a binding must be migrated before any `Bindings.databases` session is
   * handed out). Empty list means the binding has no plugin-owned migrations.
   * Bounded by `maxListPage` / `maxPluginMigrationOps` /
   * `maxPluginMigrationTotalOps` / `maxScalarBytes` /
   * `maxPluginMigrationRegistrationBytes`.
   *
   * @param binding - Binding name from `plugin.toml`.
   * @returns {@link PluginMigrationsReply}
   */
  databaseMigrations(binding: string): Promise<PluginMigrationsReply>;
}
