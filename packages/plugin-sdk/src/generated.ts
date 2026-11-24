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
  ObjectChangeKind,
  SettingNetworkKind,
  SettingGrantState,
  PluginErrorCode,
  Entrypoint,
  PortalAuthMode,
  CliArgKind,
  WorkKind,
  ExpressionKind,
  MediaKind,
  IdentifierScheme,
  CreditRole,
  CreditLevel,
  FetchedFileRole,
  ChapterKind,
  CatalogSort,
  CatalogField,
  PriceLookup,
  Abridgement,
  Explicitness,
  OfferedCodec,
  OfferedAacProfile,
  InvocationPurpose,
  RemoteMedia,
  MetadataField,
  NotificationSeverity,
  NotificationAudience,
  NotificationAlertAction,
  NotifyDestinationKind,
  AiTask,
  AiTextRole,
  ProvisionGrantKind,
  DbType,
  DbStatementKind,
  DbResultSelection,
  IsolationReq,
  ResolvedSqlType,
  IntegerArithKind,
} from "./abi.js";

export type {
  ObjectChangeKind,
  SettingNetworkKind,
  SettingGrantState,
  PluginErrorCode,
  Entrypoint,
  PortalAuthMode,
  CliArgKind,
  WorkKind,
  ExpressionKind,
  MediaKind,
  IdentifierScheme,
  CreditRole,
  CreditLevel,
  FetchedFileRole,
  ChapterKind,
  CatalogSort,
  CatalogField,
  PriceLookup,
  Abridgement,
  Explicitness,
  OfferedCodec,
  OfferedAacProfile,
  InvocationPurpose,
  RemoteMedia,
  MetadataField,
  NotificationSeverity,
  NotificationAudience,
  NotificationAlertAction,
  NotifyDestinationKind,
  AiTask,
  AiTextRole,
  ProvisionGrantKind,
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
  /**
   * Structured extras as one JSON object (`limitExceeded` carries `limit`,
   * `cap` and `used`; engine failures carry `engineCode`). Empty when none.
   */
  details: string;
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
  /** Last modification time, UTC unix milliseconds; zero when unknown. */
  modifiedAtMs: number;
  /** Backend object version id (versioned buckets); empty when none. */
  versionId: string;
  /**
   * Opaque change signature: an unchanged value means the object has not
   * changed. Backend-defined (a local file's inode, size, mtime and ctime;
   * an S3 ETag and version id). Empty when the backend has none.
   */
  signature: string;
}

/** Compact object entry in a list page. */
export interface ObjectInfo {
  /** Object key. */
  key: string;
  /** Object size in bytes. */
  size: number;
  /**
   * Backend entity tag; empty when unsupported or not requested
   * (`ListOptions.withSignatures`).
   */
  etag: string;
  /**
   * Last modification time, UTC unix milliseconds; zero when unknown or not
   * requested.
   */
  modifiedAtMs: number;
  /** Backend object version id; empty when none or not requested. */
  versionId: string;
  /**
   * Opaque change signature, as `ObjectMetadata.signature`; empty when
   * unsupported or not requested.
   */
  signature: string;
}

/** Paging options for `Destination.list`. */
export interface ListOptions {
  /** Only keys starting with this prefix; empty lists everything. */
  prefix: string;
  /** Opaque cursor from a previous `ListPage.nextCursor`; empty starts over. */
  cursor: string;
  /** Requested page size; clamped to `maxListPage`. `0` means guest default. */
  limit: number;
  /**
   * Hint: fill `etag`, `modifiedAtMs`, `versionId` and `signature` on each
   * entry. `ListPage.signaturesIncluded` says whether the guest did.
   */
  withSignatures: boolean;
  /**
   * Hint: only objects modified at or after this time (UTC unix
   * milliseconds); zero lists everything. `ListPage.modifiedSinceApplied`
   * says whether the guest filtered; when it did not, the page is unfiltered.
   */
  modifiedSinceMs: number;
}

/** One page of `Destination.list` results. */
export interface ListPage {
  /** Objects in this page, in backend order. */
  objects: ObjectInfo[];
  /** Cursor for the next page; empty when exhausted. */
  nextCursor: string;
  /**
   * True when every entry carries the signature fields the backend has
   * (`ListOptions.withSignatures` honoured).
   */
  signaturesIncluded: boolean;
  /**
   * True when the guest left out objects modified before
   * `ListOptions.modifiedSinceMs`. False means the page is unfiltered.
   */
  modifiedSinceApplied: boolean;
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
  /**
   * Conditional write (`storage.conditionalWrite`): publish only while the
   * stored object's `etag` equals this value, else fail with `conflict`.
   * Empty means unconditional. A staged put checks it at `commit`.
   */
  ifMatch: string;
  /**
   * Conditional write (`storage.conditionalWrite`): publish only when no
   * object exists at the key, else fail with `conflict`.
   */
  ifNoneMatch: boolean;
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
  /**
   * Modification time of the stored object, UTC unix milliseconds; zero when
   * unknown.
   */
  modifiedAtMs: number;
  /** Version id of the stored object; empty when none. */
  versionId: string;
  /**
   * Change signature of the stored object (`ObjectMetadata.signature`), so
   * the host can recognise its own write in a later listing; empty when none.
   */
  signature: string;
}

/** Summary of a server-side copy. */
export interface CopyResult {
  /** Bytes copied. */
  bytesCopied: number;
}

/**
 * Options for `Destination.move` and `Destination.moveMany`. Fields are only
 * ever appended.
 */
export interface MoveOptions {
  /** Fail with `conflict` instead of replacing an object at the target key. */
  noOverwrite: boolean;
  /**
   * `move` only: move only while the source's `etag` equals this value,
   * else fail with `conflict`. Empty skips the check.
   */
  ifSourceMatch: string;
}

/** Summary of `Destination.move`. */
export interface MoveResult {
  /**
   * True when the backend moved atomically (a rename): no reader saw the
   * object at both keys or at neither.
   */
  atomic: boolean;
  /**
   * True when the source was already gone and the target present, as when
   * a move is repeated after a restart. The move counts as done.
   */
  alreadyMoved: boolean;
}

/** One source and target key. */
export interface KeyPair {
  /** Source object key (or source prefix). */
  from: string;
  /** Target object key (or target prefix). */
  to: string;
}

/** What `Destination.moveMany` moves. */
export type MoveManyTarget =
  | { kind: "pairs"; value: KeyPair[] } // Explicit moves, at most `maxBatchKeys`. Each is independent.
  | { kind: "prefix"; value: KeyPair }; // Every object under `prefix.from` moves under `prefix.to`, keeping the rest of its key. Paged through `MoveManyRequest.cursor`.

/** Argument of `Destination.moveMany`. Fields are only ever appended. */
export interface MoveManyRequest {
  /** Explicit pairs or a prefix. */
  target: MoveManyTarget;
  /**
   * Prefix form: `MoveManyPage.nextCursor` from the previous call; empty
   * starts. Ignored for `pairs`.
   */
  cursor: string;
  /**
   * Prefix form: objects to move in this call, clamped to `maxBatchKeys`;
   * zero means guest default.
   */
  limit: number;
  /** Overwrite rule applied to every move (`ifSourceMatch` is ignored). */
  options: MoveOptions;
}

/** Outcome for one key of a batch call. */
export interface KeyOutcome {
  /** The key this outcome is for (the source key for moves). */
  key: string;
  /** Failure for this key; an empty `code` means it succeeded. */
  error: PluginError;
}

/** One page of `Destination.moveMany` progress. */
export interface MoveManyPage {
  /**
   * Outcomes in request order. A prefix move done in one step (a folder
   * rename) may report a single outcome keyed by the source prefix.
   */
  results: KeyOutcome[];
  /** Prefix form: cursor for the next call; empty when every object moved. */
  nextCursor: string;
  /** True when every move in this page was atomic. */
  atomic: boolean;
  /**
   * Objects moved in this page (a folder rename counts what it carried
   * when known, else the outcomes).
   */
  objectsMoved: number;
}

/** Result of `Destination.deleteMany`. */
export interface DeleteManyResult {
  /** One outcome per requested key, in request order. A missing key succeeds. */
  results: KeyOutcome[];
}

/** Result of `Destination.headMany`. */
export interface HeadManyResult {
  /** One probe per requested key, in request order. */
  objects: HeadOk[];
}

/** Success payload of `Destination.translateKeys`. */
export interface TranslateKeysResult {
  /**
   * One translated key per requested host key, in request order: the key
   * the guest stores that host key under. Each is a relative key with no
   * empty, `.` or `..` segment.
   */
  keys: string[];
}

/** Argument of `Destination.changes`. Fields are only ever appended. */
export interface ChangesRequest {
  /**
   * `ChangesPage.cursor` from the previous call. Empty asks for the current
   * position: the page carries a cursor and no changes.
   */
  cursor: string;
  /**
   * Requested changes per page, clamped to `maxChangesPage`; zero means
   * guest default.
   */
  limit: number;
}

/** One entry of a change feed. */
export interface ObjectChange {
  /** Object key, relative to the destination root. */
  key: string;
  /** What happened to it. */
  kind: ObjectChangeKind;
  /**
   * Listing fields after the change when the feed carries them (signature
   * fields may be empty); empty for `removed`.
   */
  info: ObjectInfo;
}

/** One page of `Destination.changes`. */
export interface ChangesPage {
  /** Changes since the request's cursor, oldest first. A key may repeat. */
  changes: ObjectChange[];
  /** Cursor for the next call; never empty. */
  cursor: string;
  /** True when more changes are ready now; call again with `cursor`. */
  more: boolean;
  /**
   * False when the feed may have missed changes since the request's cursor
   * (an overflowed watcher, expired queue retention, a restart): the host
   * backs it with a full listing. True means these are all the changes.
   */
  complete: boolean;
  /**
   * True when this running guest produces the feed itself (a filesystem
   * watcher) and stopping it would lose changes, so the host keeps it awake
   * while `complete` stays true. Durable feeds (a notification queue) leave
   * it false. The host honours it only from the first-party storage plugins
   * it starts on demand.
   */
  liveWatch: boolean;
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
  /**
   * Catalog behaviours a storefront declares so the host can use any store
   * for catalog search without naming it; all false for other kinds.
   */
  catalog: CatalogCapabilities;
  /**
   * Work kinds and edition carriers the plugin lists and fetches; required
   * for storefronts and metadata providers, empty lists for every other
   * kind (see `MediaSupport`).
   */
  media: MediaSupport;
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
  /**
   * `EVENTS`: outbox publisher; null unless `[[events.producers]]` is granted.
   * Null when the guest does not export this capability.
   */
  events?: EventPublisher | null;
  /**
   * Named plugin-owned `[[databases]]` bindings: each entry is an isolated
   * database provisioned by the active adapter, separate from the Bookclerk
   * library and from every other plugin. Empty when the manifest declares none.
   */
  databases: NamedDatabase[];
  /**
   * Host cancellation for the whole invocation (fence / lease loss).
   * Null when the guest does not export this capability.
   */
  cancel?: Cancellation | null;
  /**
   * `WORK_FS`: host-granted object storage for durable plugin files (work
   * filesystem); null unless `[work_fs]` is granted. Job input/output travel on
   * `JobRunner.job(controller)`, never here.
   * Null when the guest does not export this capability.
   */
  storage?: Destination | null;
  /**
   * Named `[[queues.producers]]` bindings: one host-served producer per
   * binding, each sending into the queue the row names. Empty when the
   * manifest declares none.
   */
  queues: NamedQueue[];
  /**
   * Named `[[kv_namespaces]]` bindings: one host-served durable namespace per
   * binding, scoped to this plugin. Empty when the manifest declares none.
   */
  kv: NamedKv[];
  /**
   * `SOCKETS`: host-brokered outbound WebSockets that stay open while the
   * guest sleeps; null unless `[sockets]` is declared and granted.
   * Null when the guest does not export this capability.
   */
  sockets?: Sockets | null;
  /**
   * `NOTIFY`: raises the notification types the plugin declares in
   * `[[notifications.types]]`; null when it declares none.
   * Null when the guest does not export this capability.
   */
  notify?: NotificationRaiser | null;
  /**
   * `NETWORK`: what the operator granted this plugin's network, including
   * the destination each network setting names and whether it is approved,
   * so a guest can tell an unapproved destination from an unreachable one.
   */
  network: NetworkGrant;
  /**
   * `ORIGIN`: the library itself, for a relay `ingress` plugin to send the
   * public requests it carries to; null unless the plugin is the library's
   * active relay ingress.
   * Null when the guest does not export this capability.
   */
  origin?: Origin | null;
  /**
   * `AI`: runs models other plugins serve; null unless `[ai]` names the
   * tasks the plugin calls and the operator granted them.
   * Null when the guest does not export this capability.
   */
  ai?: AiRunner | null;
  /**
   * `WEBHOOKS`: the instance's inbound webhooks, their URLs and the
   * deliveries the host stored; null unless `[[webhooks]]` is declared and
   * granted.
   * Null when the guest does not export this capability.
   */
  webhooks?: Webhooks | null;
}

/**
 * The network grant in effect for one `PluginWorker.open`. Read-only
 * feedback: the host enforces the grant whatever the guest does with this.
 */
export interface NetworkGrant {
  /** True when the grant allows outbound connections; false for `deny`. */
  outbound: boolean;
  /**
   * Fetch destinations in effect, as `plugin.toml` `domains` entries
   * (`host`, `*.host`, `host:port`, or `*` for any public host), including
   * approved setting destinations; at most `maxNetworkGrantEntries`.
   */
  domains: string[];
  /**
   * Raw TCP destinations in effect, as `host:port` (IPv6 in brackets), one
   * per granted port; at most `maxNetworkGrantEntries`.
   */
  tcp: string[];
  /**
   * One entry per `[[settings]]` entry that declares `network`, in
   * declaration order; at most `maxNetworkGrantEntries`.
   */
  settings: SettingNetworkGrant[];
}

/** One network setting and the destination its value names. */
export interface SettingNetworkGrant {
  /** The setting key. */
  key: string;
  /** Whether the plugin fetches the destination or opens a socket to it. */
  network: SettingNetworkKind;
  /**
   * The destination as the consent form shows it: `scheme://host:port` for
   * a fetch, `host:port` for TCP (IPv6 in brackets); empty when `state` is
   * `unset` or `invalid`.
   * Omitted when absent (wire zero value).
   */
  destination?: string;
  /** Where the destination stands. */
  state: SettingGrantState;
  /**
   * Why the value names no destination, when `state` is `invalid`.
   * Omitted when absent (wire zero value).
   */
  error?: string;
}

/**
 * Exported entrypoints returned by `PluginWorker.open`. One capability per
 * `plugin.toml` `entrypoints` entry / trigger; null when not exported. The host
 * refuses an entrypoint the manifest or operator grant did not allow.
 */
export interface Entrypoints {
  /**
   * `[[events.consumers]]` trigger: `event(batch)` handler.
   * Null when the guest does not export this capability.
   */
  eventConsumer?: EventConsumer | null;
  /**
   * `[triggers] jobs` trigger: `job(controller)` handler.
   * Null when the guest does not export this capability.
   */
  jobRunner?: JobRunner | null;
  /**
   * `storefront` entrypoint.
   * Null when the guest does not export this capability.
   */
  storefront?: ContentSource | null;
  /**
   * `storage` entrypoint.
   * Null when the guest does not export this capability.
   */
  storage?: Destination | null;
  /**
   * `databaseAdapter` entrypoint.
   * Null when the guest does not export this capability.
   */
  databaseAdapter?: Database | null;
  /**
   * `remoteLibrary` entrypoint.
   * Null when the guest does not export this capability.
   */
  remoteLibrary?: RemoteLibrary | null;
  /**
   * `cli` entrypoint.
   * Null when the guest does not export this capability.
   */
  cli?: PluginCli | null;
  /**
   * `oidc` entrypoint.
   * Null when the guest does not export this capability.
   */
  oidc?: Oidc | null;
  /**
   * `[[queues.consumers]]` trigger: `queue(batch)` handler.
   * Null when the guest does not export this capability.
   */
  queueConsumer?: QueueConsumer | null;
  /**
   * `[triggers] crons` trigger: `scheduled(controller)` handler.
   * Null when the guest does not export this capability.
   */
  scheduledHandler?: ScheduledHandler | null;
  /**
   * `metadata` entrypoint.
   * Null when the guest does not export this capability.
   */
  metadata?: MetadataProvider | null;
  /**
   * `covers` entrypoint.
   * Null when the guest does not export this capability.
   */
  covers?: CoverFetcher | null;
  /**
   * `[sockets]` handler: messages and closes on host-brokered sockets.
   * Null when the guest does not export this capability.
   */
  socketHandler?: SocketHandler | null;
  /**
   * `metadataFiles` entrypoint.
   * Null when the guest does not export this capability.
   */
  metadataFiles?: MetadataFiles | null;
  /**
   * `notifier` entrypoint.
   * Null when the guest does not export this capability.
   */
  notifier?: Notifier | null;
  /**
   * `ingress` entrypoint.
   * Null when the guest does not export this capability.
   */
  ingress?: Ingress | null;
  /**
   * `ai` entrypoint.
   * Null when the guest does not export this capability.
   */
  ai?: AiModels | null;
  /**
   * `[[webhooks]]` handler: called after the host stores deliveries.
   * Null when the guest does not export this capability.
   */
  webhookHandler?: WebhookHandler | null;
  /**
   * `provisioning` entrypoint: guided setup.
   * Null when the guest does not export this capability.
   */
  provisioning?: Provisioning | null;
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
 * Request of `EventPublisher.read`: one page of past outbox events, oldest
 * first. Only types the instance's consented `[[events.consumers]]` rows
 * subscribe to are returned, through the same schema-version and filter match
 * as delivery, and only back to `[events].retention_days`.
 */
export interface EventReadRequest {
  /**
   * Opaque cursor from an earlier `EventReadPage.cursor`; empty starts at
   * `sinceUnixMs`. At most `maxEventCursorBytes`. Set at most one of `after`
   * and `sinceUnixMs`.
   */
  after: string;
  /**
   * UTC Unix milliseconds to start from when `after` is empty; zero starts at
   * the oldest retained event.
   */
  sinceUnixMs: number;
  /**
   * Event types to return; empty means every consented subscription. Each
   * entry must be a consented subscription type; at most `maxListPage`.
   */
  types: string[];
  /** Most events to return; zero means `maxListPage`. At most `maxListPage`. */
  limit: number;
}

/** Success payload of `EventPublisher.read`. */
export interface EventReadPage {
  /**
   * Matching events in outbox order. `deliveryAttempt` is zero and
   * `deduplicationKey` is the event id: a read is not a delivery, so dedupe
   * on `eventId`.
   */
  events: DomainEvent[];
  /**
   * Opaque position after the last event scanned; pass it as `after` to
   * continue. Empty only when nothing was scanned and the request had no
   * cursor. At most `maxEventCursorBytes`.
   */
  cursor: string;
  /**
   * True when more events may follow: the page filled or the host's scan
   * budget ran out before the newest event.
   */
  more: boolean;
  /**
   * Ids of the events on this page whose delivery to this plugin expired
   * unleased: the events the instance missed.
   */
  expiredEventIds: string[];
  /**
   * True when the start position is older than `[events].retention_days`, so
   * events before the retained window may be gone.
   */
  truncated: boolean;
}

/** Result union of `EventPublisher.read`. */
export type EventReadReply =
  | { kind: "ok"; value: EventReadPage } // Success: one page.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * Everything one `JobRunner.job` invocation may touch. Capabilities are
 * host-served; the guest never sees OS paths or sockets.
 */
export interface JobController {
  /** Durable command envelope. */
  invocation: JobInvocation;
  /**
   * Job input objects.
   * Null when the guest does not export this capability.
   */
  input?: Source | null;
  /**
   * Job output object store.
   * Null when the guest does not export this capability.
   */
  output?: Destination | null;
  /**
   * Progress reporter (host coalesces frequent updates).
   * Null when the guest does not export this capability.
   */
  progress?: ProgressSink | null;
  /**
   * Host cancellation probe for this job (fence / lease).
   * Null when the guest does not export this capability.
   */
  cancel?: Cancellation | null;
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
  /**
   * Streamed object bytes.
   * Null when the guest does not export this capability.
   */
  body?: ByteSource | null;
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

/** Result union of `Destination.move`. */
export type MoveReply =
  | { kind: "ok"; value: MoveResult } // Success: how the move was done.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.moveMany`. */
export type MoveManyReply =
  | { kind: "ok"; value: MoveManyPage } // Success: per-key outcomes and the continuation.
  | { kind: "err"; value: PluginError }; // Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.deleteMany`. */
export type DeleteManyReply =
  | { kind: "ok"; value: DeleteManyResult } // Success: per-key outcomes.
  | { kind: "err"; value: PluginError }; // Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.headMany`. */
export type HeadManyReply =
  | { kind: "ok"; value: HeadManyResult } // Success: one probe per key.
  | { kind: "err"; value: PluginError }; // Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.translateKeys`. */
export type TranslateKeysReply =
  | { kind: "ok"; value: TranslateKeysResult } // Success: one translated key per host key.
  | { kind: "err"; value: PluginError }; // Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.

/** Result union of `Destination.changes`. */
export type ChangesReply =
  | { kind: "ok"; value: ChangesPage } // Success: one page of changes.
  | { kind: "err"; value: PluginError }; // Typed failure; `invalid_cursor` means the cursor expired and the host starts over with a full listing.

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
  /**
   * Streamed object bytes.
   * Null when the guest does not export this capability.
   */
  body?: ByteSource | null;
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
 * Success payload of `Origin.fetch`: the library's response head and its
 * streamed body.
 */
export interface OriginResponse {
  /** HTTP status code, 100 to 599. */
  status: number;
  /**
   * Response header fields in order, at most `maxOriginHeaders` and
   * `maxOriginHeaderBytes`; repeated names (`set-cookie`) stay separate.
   */
  headers: OriginHeader[];
  /**
   * Response body; read to `done`. Null when the response has no body.
   * Null when the guest does not export this capability.
   */
  body?: ByteSource | null;
}

/** Result union of `Origin.fetch`. */
export type OriginFetchReply =
  | { kind: "ok"; value: OriginResponse } // Success: the library answered (any HTTP status).
  | { kind: "err"; value: PluginError }; // Typed failure: the request was refused before the library saw it (`invalid_params`) or the library is unavailable.

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
  /**
   * Move one object (requires `storage.move`). Repeating a finished move
   * succeeds with `alreadyMoved`.
   *
   * @param from - Source object key.
   * @param to - Target object key.
   * @param options - Overwrite and source preconditions.
   * @returns {@link MoveReply}
   */
  move(from: string, to: string, options: MoveOptions): Promise<MoveReply>;
  /**
   * Move a set of objects or a whole prefix (requires `storage.moveMany`).
   * Paged and safe to repeat.
   *
   * @param request - Pairs or prefix, cursor, and options.
   * @returns {@link MoveManyReply}
   */
  moveMany(request: MoveManyRequest): Promise<MoveManyReply>;
  /**
   * Remove up to `maxBatchKeys` objects (requires `storage.deleteMany`).
   *
   * @param keys - Object keys to remove.
   * @returns {@link DeleteManyReply}
   */
  deleteMany(keys: string[]): Promise<DeleteManyReply>;
  /**
   * Probe up to `maxBatchKeys` objects (requires `storage.headMany`).
   *
   * @param keys - Object keys to probe.
   * @returns {@link HeadManyReply}
   */
  headMany(keys: string[]): Promise<HeadManyReply>;
  /**
   * Keys changed since a cursor (requires `storage.changes`). A hint for
   * what to read; a scheduled listing stays the backstop.
   *
   * @param request - Cursor and page size.
   * @returns {@link ChangesReply}
   */
  changes(request: ChangesRequest): Promise<ChangesReply>;
  /**
   * Translate up to `maxBatchKeys` host keys into the keys the guest stores
   * them under (requires `storage.translateKeys`). A host key is `/`-joined
   * segments, none empty, `.`, `..` or holding `\` or a control character.
   * The same host key must always translate to the same key. Without the
   * feature the host stores each host key as written.
   *
   * @param keys - Host keys to translate.
   * @returns {@link TranslateKeysReply}
   */
  translateKeys(keys: string[]): Promise<TranslateKeysReply>;
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
  /**
   * Read one page of past outbox events this instance subscribes to, oldest
   * first, to catch up after a gap (`env.EVENTS.read`).
   *
   * @param request - Where to start, which types, how many.
   * @returns {@link EventReadReply}
   */
  read(request: EventReadRequest): Promise<EventReadReply>;
}

/** One named plugin-owned database binding delivered on `Bindings.databases`. */
export interface NamedDatabase {
  /** Binding name from `plugin.toml` `[[databases]]`. */
  name: string;
  /**
   * Isolated typed SQL session for this binding (plugin-owned schema).
   * Null when the guest does not export this capability.
   */
  database?: GuestDatabase | null;
}

/**
 * `[triggers] crons` handler (`Entrypoints.scheduledHandler`), the Workers
 * `scheduled(controller)` trigger. Each fire is delivered at least once;
 * a fire that throws is retried with backoff, keeping `scheduledTime` and
 * `fireId`, so handlers dedupe on `fireId`.
 */
export interface ScheduledHandler {
  /**
   * Run one cron fire to completion.
   *
   * @param controller - The fire being delivered.
   * @param cancel - Fires once the host cancels the fire (fence loss or operator cancel); null from hosts that predate it.
   * @returns {@link EmptyReply}
   */
  scheduled(controller: ScheduledController, cancel: Cancellation): Promise<EmptyReply>;
}

/**
 * `[[queues.consumers]]` handler (`Entrypoints.queueConsumer`). Delivery is
 * at-least-once; consumers must be idempotent on `QueueMessage.id`.
 */
export interface QueueConsumer {
  /**
   * Deliver one ordered batch of messages from one queue.
   *
   * @param batch - Messages in delivery order.
   * @returns {@link QueueBatchReply}
   */
  queue(batch: QueueBatch): Promise<QueueBatchReply>;
}

/**
 * Queue producer binding: host-served send into the queue a
 * `[[queues.producers]]` row names, granted through `Bindings.queues`.
 */
export interface QueueProducer {
  /**
   * Send one message.
   *
   * @param message - Body, encoding, and delay.
   * @returns {@link QueueSendReply}
   */
  send(message: QueueSendMessage): Promise<QueueSendReply>;
  /**
   * Send up to `maxListPage` messages in order; `delaySeconds` applies to
   * every message that carries no delay of its own.
   *
   * @param messages - Messages in send order.
   * @param delaySeconds - Default delay for the batch.
   * @returns {@link QueueSendReply}
   */
  sendBatch(messages: QueueSendMessage[], delaySeconds: number): Promise<QueueSendReply>;
}

/** One named queue producer delivered on `Bindings.queues`. */
export interface NamedQueue {
  /** Binding name from `plugin.toml` `[[queues.producers]]`. */
  name: string;
  /** Queue the binding sends to. */
  queue: string;
  /**
   * Host-served producer for that queue.
   * Null when the guest does not export this capability.
   */
  producer?: QueueProducer | null;
}

/**
 * Durable key/value namespace: a Workers `KVNamespace`-shaped binding the host
 * serves from the library database, scoped to the plugin and the namespace a
 * `[[kv_namespaces]]` row names, granted through `Bindings.kv`. Values live in
 * the library, so an instance moved to another host reads its own keys.
 */
export interface KvNamespace {
  /**
   * Read one key; `found = false` when absent or expired.
   *
   * @param key - Key, at most `maxKvKeyBytes` UTF-8 bytes.
   * @returns {@link KvGetReply}
   */
  get(key: string): Promise<KvGetReply>;
  /**
   * Write one key, replacing any previous value and metadata.
   *
   * @param key - Key, at most `maxKvKeyBytes` UTF-8 bytes.
   * @param value - Value, at most the namespace's `max_value_bytes`.
   * @param options - Expiration and metadata.
   * @returns {@link KvPutReply}
   */
  put(key: string, value: Uint8Array, options: KvPutOptions): Promise<KvPutReply>;
  /**
   * Delete one key; a missing key is not an error.
   *
   * @param key - Key to delete.
   * @returns {@link KvDeleteReply}
   */
  delete(key: string): Promise<KvDeleteReply>;
  /**
   * List keys in lexical order, one page at a time.
   *
   * @param options - Prefix, cursor, and page size.
   * @returns {@link KvListReply}
   */
  list(options: KvListOptions): Promise<KvListReply>;
}

/** One named durable namespace delivered on `Bindings.kv`. */
export interface NamedKv {
  /** Binding name from `plugin.toml` `[[kv_namespaces]]`. */
  name: string;
  /** Namespace the binding is scoped to. */
  namespace: string;
  /**
   * Host-served namespace.
   * Null when the guest does not export this capability.
   */
  kv?: KvNamespace | null;
}

/**
 * Host-brokered outbound WebSockets, granted through `Bindings.sockets`.
 *
 * The guest asks for a socket; a broker the host runs for the plugin
 * instance dials it under the plugin's network grant and keeps it open while
 * the guest sleeps, in the manner of hibernatable Workers WebSockets. Each
 * incoming message wakes the guest and arrives on
 * `Entrypoints.socketHandler`. Heartbeat pairs declared at connect are
 * answered by the broker without waking the guest.
 */
export interface Sockets {
  /**
   * Open a socket; it is owned by the plugin instance, not this invocation.
   *
   * @param request - Target, subprotocols, headers, heartbeats.
   * @returns {@link SocketConnectReply}
   */
  connect(request: SocketConnectRequest): Promise<SocketConnectReply>;
  /**
   * Send one message on an open socket.
   *
   * @param socket - Socket id from `connect` or `list`.
   * @param frame - Message, at most `maxSocketFrameBytes`.
   * @returns {@link EmptyReply}
   */
  send(socket: string, frame: SocketFrame): Promise<EmptyReply>;
  /**
   * Close a socket; closing an unknown or closed socket is not an error.
   *
   * @param socket - Socket id.
   * @param code - WebSocket close code; zero sends 1000.
   * @param reason - Close reason, at most 123 UTF-8 bytes.
   * @returns {@link EmptyReply}
   */
  close(socket: string, code: number, reason: string): Promise<EmptyReply>;
  /**
   * Replace the attachment the host keeps for a socket.
   *
   * @param socket - Socket id.
   * @param attachment - Opaque bytes, at most `maxSocketAttachmentBytes`.
   * @returns {@link EmptyReply}
   */
  setAttachment(socket: string, attachment: Uint8Array): Promise<EmptyReply>;
  /**
   * The instance's open sockets, for a guest that just woke.
   *
   * @returns {@link SocketListReply}
   */
  list(): Promise<SocketListReply>;
}

/** Guest handler for host-brokered sockets (`Entrypoints.socketHandler`). */
export interface SocketHandler {
  /**
   * One message arrived on an open socket.
   *
   * @param event - Socket, frame, and attachment.
   * @returns {@link SocketMessageReply}
   */
  message(event: SocketMessage): Promise<SocketMessageReply>;
  /**
   * A socket closed, by the remote side, the network, or the host.
   *
   * @param event - Socket, close code, and attachment.
   * @returns {@link EmptyReply}
   */
  closed(event: SocketClosed): Promise<EmptyReply>;
}

/**
 * The library, as a relay `ingress` plugin reaches it (`Bindings.origin`,
 * `ORIGIN` in the Workers idiom). The plugin turns each request its provider
 * delivers into a `fetch` and streams the response back. The host treats
 * every request as the `tunnel` transport: public `https`, never loopback,
 * with the client address the plugin reports.
 */
export interface Origin {
  /**
   * Serve one request; the response body streams as the library writes it.
   *
   * @param request - Method, target, headers, client address.
   * @param body - Request body; null when the request has none.
   * @returns {@link OriginFetchReply}
   */
  fetch(request: OriginRequest, body: ByteSource): Promise<OriginFetchReply>;
  /**
   * Open a WebSocket to the library for one upgrade request the provider
   * delivers. An accepted upgrade becomes a socket in the instance's
   * `Bindings.sockets`: its messages arrive on `Entrypoints.socketHandler`
   * and the plugin answers with `Sockets.send` and `Sockets.close`, as on
   * any socket it dialed. Needs `[sockets]`. These sockets count toward
   * `maxOriginSocketsPerInstance`, not `maxSocketsPerInstance`.
   *
   * @param request - A `GET` with `upgrade: websocket`.
   * @param attachment - Initial attachment, at most `maxSocketAttachmentBytes`.
   * @returns {@link OriginConnectReply}
   */
  connect(request: OriginRequest, attachment: Uint8Array): Promise<OriginConnectReply>;
}

/**
 * A public address provider (`Entrypoints.ingress`): the one `ingress`
 * family instance the library runs. A relay provider carries traffic to
 * `Bindings.origin`; a direct provider keeps a DNS name pointing at this
 * install.
 */
export interface Ingress {
  /**
   * Bring the public address up; answers the origin it serves.
   *
   * @returns {@link IngressStartReply}
   */
  start(): Promise<IngressStartReply>;
  /**
   * Whether the public address is serving now.
   *
   * @returns {@link IngressHealthReply}
   */
  health(): Promise<IngressHealthReply>;
  /**
   * Take the public address down; stopping a stopped one is not an error.
   *
   * @returns {@link EmptyReply}
   */
  stop(): Promise<EmptyReply>;
}

/**
 * Guided setup (`Entrypoints.provisioning`). The host collects a
 * short-lived grant from the operator (a pasted API token, or an OAuth access
 * token), opens the instance with `Invocation.purpose = provision` and hands
 * the grant over. The plugin creates what it needs at the provider and
 * returns settings and narrowly scoped credentials; the host checks them
 * against `[provisioning] outputs` and the pinned settings schema, writes the
 * settings, seals the credentials and drops the grant.
 */
export interface Provisioning {
  /**
   * Create (or, when `CONFIG` holds earlier outputs, update or rotate) what
   * the instance needs.
   *
   * @param request - The grant and the operator's choices.
   * @returns {@link ProvisionReply}
   */
  provision(request: ProvisionRequest): Promise<ProvisionReply>;
  /**
   * Remove what `provision` created. Removing what is already gone is not an
   * error.
   *
   * @param request - A grant, or none to use stored secrets.
   * @returns {@link EmptyReply}
   */
  deprovision(request: DeprovisionRequest): Promise<EmptyReply>;
}

/**
 * Host-served inbound webhooks, granted through `Bindings.webhooks`.
 *
 * The host serves each `[[webhooks]]` row at a secret URL under the
 * library's public origin, checks every request against the row's body cap,
 * rate limit and signature rule, stores it and answers at once. The guest
 * takes stored deliveries with `pull` and settles them with `ack`, in the
 * manner of a Cloudflare Queues HTTP pull consumer: a pulled delivery stays
 * leased until acknowledged, retried, or its visibility timeout ends.
 */
export interface Webhooks {
  /**
   * The URL an outside service should call for one webhook.
   *
   * @param name - `[[webhooks]]` name.
   * @returns {@link WebhookUrlReply}
   */
  url(name: string): Promise<WebhookUrlReply>;
  /**
   * Lease up to `batchSize` stored deliveries, oldest first.
   *
   * @param request - Webhook, batch size, visibility timeout.
   * @returns {@link WebhookPullReply}
   */
  pull(request: WebhookPullRequest): Promise<WebhookPullReply>;
  /**
   * Acknowledge finished deliveries and offer retried ones again at once.
   *
   * @param request - Webhook and lease ids.
   * @returns {@link EmptyReply}
   */
  ack(request: WebhookAckRequest): Promise<EmptyReply>;
}

/** Guest handler for inbound webhooks (`Entrypoints.webhookHandler`). */
export interface WebhookHandler {
  /**
   * The host stored deliveries for one webhook; the guest pulls them.
   *
   * @param event - Webhook and how many wait.
   * @returns {@link EmptyReply}
   */
  delivered(event: WebhookDelivered): Promise<EmptyReply>;
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
   * One page of one account's library scan pass (see `ScanPage`).
   *
   * @param params - Account, cursor, sync token, credential.
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
  /**
   * Undo store-side state for one account before the host deletes its
   * stored credentials (for example, deregister the device registered at
   * login). Best effort: the host revokes the account whatever this returns.
   *
   * @param params - Account id and its stored credentials.
   * @returns {@link EmptyReply}
   */
  revokeAccount(params: RevokeAccountParams): Promise<EmptyReply>;
  /**
   * Purchase links / price hints for several titles in one call, so a guest
   * whose backend prices many products per request makes one request for a
   * page of cards. A guest that cannot batch answers each query as
   * `purchaseHint` would.
   *
   * @param params - The titles to price, in order.
   * @returns {@link PurchaseHintsReply}
   */
  purchaseHints(params: PurchaseHintsParams): Promise<PurchaseHintsReply>;
  /**
   * The audio quality one account would download for each of several
   * titles today, so the host can tell when a better file than the one it
   * holds is on offer. Asked only of guests that declare
   * `CatalogCapabilities.qualityOffers`; it must not license or download.
   *
   * @param params - Account, credentials, and title ids.
   * @returns {@link QualityOffersReply}
   */
  qualityOffers(params: QualityOffersParams): Promise<QualityOffersReply>;
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
   * External users the remote side observed and the host has not
   * acknowledged. Without `params.ackFollows` (hosts that predate
   * `ackEvents`) a returned user counts as delivered.
   *
   * @param params - Whether the host acknowledges what it receives.
   * @returns {@link EventPollReply}
   */
  pollEvents(params: EventPollParams): Promise<EventPollReply>;
  /**
   * Acknowledge users a `pollEvents` reply handed over: the host has passed
   * them on, so the guest stops returning them.
   *
   * @param params - The acknowledged users.
   * @returns {@link EmptyReply}
   */
  ackEvents(params: EventAckParams): Promise<EmptyReply>;
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
 * `metadata` entrypoint: bibliographic lookups against a public catalog
 * (OpenLibrary, Audnexus, ...). A provider describes what it finds as
 * edition records; the host decides identity and never trusts a record to
 * name an existing library row. Network access is limited to the domains the
 * manifest's `[capabilities.network]` declares.
 */
export interface MetadataProvider {
  /**
   * Liveness / readiness probe.
   *
   * @returns {@link HealthReply}
   */
  health(): Promise<HealthReply>;
  /**
   * Records for the editions the given identifiers name.
   *
   * @param params - Identifiers to resolve and a result cap.
   * @returns {@link MetadataRecordsReply}
   */
  lookup(params: MetadataLookupParams): Promise<MetadataRecordsReply>;
  /**
   * Records matching a title and, optionally, an author.
   *
   * @param params - Title, author, language and a result cap.
   * @returns {@link MetadataRecordsReply}
   */
  search(params: MetadataSearchParams): Promise<MetadataRecordsReply>;
  /**
   * Chapter layout of the edition the given identifiers name.
   *
   * @param params - Identifiers of one edition.
   * @returns {@link MetadataChaptersReply}
   */
  chapters(params: MetadataChaptersParams): Promise<MetadataChaptersReply>;
}

/**
 * `covers` entrypoint: downloads cover images on the host's behalf, so the
 * host itself never makes an outbound request to a URL a plugin or a store
 * supplied. The fetcher only transports bytes: the host sniffs, decodes and
 * re-encodes every image in its media worker before anything stores or
 * serves it. Network access is limited to what the manifest's
 * `[capabilities.network]` declares.
 */
export interface CoverFetcher {
  /**
   * Liveness / readiness probe.
   *
   * @returns {@link HealthReply}
   */
  health(): Promise<HealthReply>;
  /**
   * Fetches every URL concurrently and streams each body into `sink` under
   * its index in `params.urls` (`"0"`, `"1"`, ...). The reply says, per URL,
   * whether its body was stored.
   *
   * @param params - URLs and the per-image byte cap.
   * @param sink - Host-served store that accepts only those keys.
   * @returns {@link CoverImagesReply}
   */
  fetchCovers(params: CoverFetchParams, sink: Destination): Promise<CoverImagesReply>;
}

/**
 * `metadataFiles` entrypoint: reads and writes metadata files that sit next
 * to an edition's media (sidecars such as `metadata.json`, OPF, NFO or CUE),
 * one dialect per format. The plugin only converts bytes: the host reads the
 * file, chooses every path and writes the result, so a dialect plugin needs
 * no network or file grants. The manifest's `[metadata_files]` table names
 * each dialect, the file names it reads and the fields it carries, so the
 * host routes a file to a sleeping plugin without starting it.
 */
export interface MetadataFiles {
  /**
   * Liveness / readiness probe.
   *
   * @returns {@link HealthReply}
   */
  health(): Promise<HealthReply>;
  /**
   * What one metadata file says about its edition.
   *
   * @param params - Dialect, file name and bytes.
   * @returns {@link MetadataFileParseReply}
   */
  parse(params: MetadataFileParseParams): Promise<MetadataFileParseReply>;
  /**
   * The bytes of a metadata file with the given fields set to the record's
   * values and everything else kept from the existing file.
   *
   * @param params - Dialect, record, fields and existing bytes.
   * @returns {@link MetadataFileRenderReply}
   */
  render(params: MetadataFileRenderParams): Promise<MetadataFileRenderReply>;
}

/**
 * `notifier` entrypoint: delivers one host-rendered message to one
 * destination (an email address, a chat webhook, a push subscription). The
 * host decides what is worth a notification, who gets it and which instance
 * carries it; the plugin only sends. Each send is one claimed `notify.send`
 * job, so the host owns retries: a reply of `rate_limited`, `unavailable`,
 * `deadline_exceeded` or `cancelled` is retried (`rate_limited` honouring
 * `details.retryAfterMs`), any other error is a permanent failure for that
 * destination.
 */
export interface Notifier {
  /**
   * Liveness / readiness probe: credentials and reachability.
   *
   * @returns {@link HealthReply}
   */
  health(): Promise<HealthReply>;
  /**
   * Deliver one message to one destination.
   *
   * @param params - Message and destination.
   * @returns {@link NotifySendReply}
   */
  send(params: NotifySendParams): Promise<NotifySendReply>;
  /**
   * Normalise and check a personal destination's address before the host
   * saves it.
   *
   * @param params - Address document to check.
   * @returns {@link NotifyValidateReply}
   */
  validateDestination(params: NotifyValidateParams): Promise<NotifyValidateReply>;
}

/**
 * `NOTIFY` binding: raises a notification type the plugin declared in
 * `[[notifications.types]]`. The host router applies the administrators'
 * routing and each patron's preferences; the raising plugin never learns
 * where the notification went.
 */
export interface NotificationRaiser {
  /**
   * Raise one notification.
   *
   * @param params - Declared type, fields and optional patron.
   * @returns {@link NotifyRaiseReply}
   */
  raise(params: NotifyRaiseParams): Promise<NotifyRaiseReply>;
}

/**
 * `ai` entrypoint: serves models. The plugin lists the models it serves,
 * each with one task, and runs one model on one input. The host routes
 * every call (its own and other plugins' through the `AI` binding) to the
 * deployed plugin that serves the named model; a model plugin never learns
 * who asked. Bulk work arrives as many `run` calls from a host job, each
 * within the per-call caps.
 */
export interface AiModels {
  /**
   * Liveness / readiness probe: model files present and loadable.
   *
   * @returns {@link HealthReply}
   */
  health(): Promise<HealthReply>;
  /**
   * The models this plugin serves right now, under its current settings.
   *
   * @returns {@link AiModelsReply}
   */
  models(): Promise<AiModelsReply>;
  /**
   * Run one model on one input.
   *
   * @param params - Model name and the task's input.
   * @returns {@link AiRunReply}
   */
  run(params: AiRunParams): Promise<AiRunReply>;
}

/**
 * `AI` binding: runs a model by name, as `env.AI.run(model, input)` does on
 * Cloudflare Workers. The host checks the caller's `[ai]` grant for the
 * model's task, applies the call's deadline and the caller's budget, and
 * forwards to the serving plugin's `AiModels.run`.
 */
export interface AiRunner {
  /**
   * Run one model on one input.
   *
   * @param params - Model name or task alias, and the task's input.
   * @returns {@link AiRunReply}
   */
  run(params: AiRunParams): Promise<AiRunReply>;
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
   * `WORK_FS`, `OAUTH`, `KV`, `EVENTS`, `[[queues.producers]]` names, ...).
   */
  bindings: string[];
  /** Queues the default entrypoint consumes (`queue(batch)` trigger). */
  queueConsumers: string[];
  /** Queues the guest may send to through its `[[queues.producers]]` bindings. */
  queueProducers: string[];
  /**
   * Five-field UTC cron schedules the default entrypoint handles
   * (`scheduled(controller)` trigger, `[triggers] crons`), normalized to
   * single spaces.
   */
  crons: string[];
  /**
   * True when the guest declared `[sockets]`: it receives the host-brokered
   * `Sockets` binding and may export a `SocketHandler` for pushed messages.
   */
  sockets: boolean;
  /**
   * True when the guest declared `[[webhooks]]`: it receives the host-served
   * `Webhooks` binding and may export a `WebhookHandler` the host wakes when
   * it stores deliveries.
   */
  webhooks: boolean;
}

/**
 * Catalog behaviours a storefront declares in `describe()`. Catalog search
 * reads them instead of special-casing plugin ids; every flag defaults to
 * false, so a guest that says nothing keeps the plain behaviour.
 */
export interface CatalogCapabilities {
  /**
   * True when `catalogDetail` resolves a bare ISBN (sent in both `productId`
   * and `isbn`) to a full bibliographic record. Catalog search then asks this
   * store to fill description, narrators, runtime and genres on lean result
   * rows from any store that carry an ISBN.
   */
  detailByIsbn: boolean;
  /**
   * True when `purchaseHint` / `purchaseHints` price a query by its exact
   * ISBN (`lookup = isbn`): the answer is the store's listing of that ISBN
   * or not found, never a title match. The host then prices a card at this
   * store by the ISBNs of the card's recording before searching by title.
   */
  priceByIsbn: boolean;
  /**
   * True when `purchaseHint` / `purchaseHints` price a query by its exact
   * ASIN (`lookup = asin`), as `priceByIsbn` does for ISBNs.
   */
  priceByAsin: boolean;
  /**
   * True when `catalogDetail` resolves this store's own `productId` (an id
   * its search, expand or deals hits gave) to a full bibliographic record.
   * Catalog search then asks this store to fill lean result rows it listed
   * itself, and title dialogs ask it for the editions it listed. A store
   * that leaves it false is never woken for a detail lookup by product id.
   */
  detailByProductId: boolean;
  /**
   * True when `qualityOffers` answers which audio quality an account would
   * download for a title. The host's quality-upgrade check asks only stores
   * that declare it.
   */
  qualityOffers: boolean;
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
 * Params of `ContentSource.scan`: one page of one account's scan pass. The
 * host injects that account's sealed credentials so the plugin does not need a
 * private credential store under `pluginDataDir`, and drives the pass page by
 * page: the first call of a pass sends an empty `cursor`, every later call
 * echoes the previous page's `nextCursor`.
 */
export interface ScanParams {
  /** Scoped plugin data directory. */
  pluginDataDir: string;
  /** Account this pass scans; always one of the plugin's connected accounts. */
  accountId: string;
  /** Storefront page size; the host always sends an explicit value. */
  pageSize: number;
  /** When true, import podcast/episode-style rows. */
  importEpisodes: boolean;
  /** When true, import Plus/catalog entitlement titles. */
  importPlusTitles: boolean;
  /** Host-loaded credential blob for `accountId`. */
  credential: AccountCredential;
  /**
   * Opaque position the guest returned as `ScanPage.nextCursor`; empty on the
   * first page of a pass. At most `maxScanCursorBytes`.
   */
  cursor: string;
  /**
   * Opaque token the guest returned as `ScanPage.syncToken` at the end of the
   * account's last pass; empty asks for a full pass. At most
   * `maxScanCursorBytes`.
   */
  syncToken: string;
}

/**
 * Media a plugin works with, declared in `describe()` and in the manifest's
 * `[media]` table. Required for plugins that export the storefront or
 * metadata entrypoint: the host refuses to spawn one whose `workKinds` or
 * `lists` is empty, holds `unspecified`, or is wider than its manifest, and
 * refuses any record whose kinds fall outside the declaration.
 */
export interface MediaSupport {
  /** Work kinds the plugin reports. */
  workKinds: WorkKind[];
  /**
   * Edition carriers the plugin reports in scans, catalog search and
   * metadata records.
   */
  lists: MediaKind[];
  /**
   * Edition carriers `ContentSource.fetchTitle` can deliver; a subset of
   * `lists`. Empty for metadata providers and for stores that only list.
   */
  fetches: MediaKind[];
}

/** One external identifier a storefront or metadata provider reports. */
export interface Identifier {
  /** Scheme the value belongs to; it decides the identifier's level. */
  scheme: IdentifierScheme;
  /** The identifier as the source printed it; the host trims and normalises. */
  value: string;
  /**
   * Marketplace the id is scoped to (ASIN); absent for global schemes.
   * Omitted when absent (wire zero value).
   */
  marketplace?: string;
}

/** One credited contributor. Credits are listed in billing order. */
export interface Credit {
  /** Contributor display name as the source prints it, one person or body. */
  name: string;
  /** What the contributor did. */
  role: CreditRole;
  /** Level the credit attaches to. */
  level: CreditLevel;
}

/** A work's place in one series. */
export interface SeriesEntry {
  /** Series name as the source prints it. */
  name: string;
  /**
   * Position label as the source prints it (`1`, `2.5`, `1-3`).
   * Omitted when absent (wire zero value).
   */
  position?: string;
  /**
   * The store's own id for the series (an Audible series ASIN) when known.
   * Omitted when absent (wire zero value).
   */
  storeSeriesId?: string;
}

/**
 * One library title returned by `ContentSource.scan`. The host forces `source`
 * to the plugin id and, until the library stores works, expressions and
 * editions, maps the record onto one flat library row.
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
  /** Kind of work the title is. */
  workKind: WorkKind;
  /** How the work is realised in this edition. */
  expressionKind: ExpressionKind;
  /** Carrier of this edition. */
  mediaKind: MediaKind;
  /** External identifiers (ISBN, ASIN, OpenLibrary ids). */
  identifiers: Identifier[];
  /** Credited contributors in billing order. */
  credits: Credit[];
  /** Series the work belongs to. */
  seriesEntries: SeriesEntry[];
  /**
   * Product id of the parent podcast show, for a podcast episode.
   * Omitted when absent (wire zero value).
   */
  parentProductId?: string;
  /**
   * Content language code when known.
   * Omitted when absent (wire zero value).
   */
  language?: string;
  /** Whether the edition is abridged when the storefront says so. */
  abridgement: Abridgement;
  /**
   * Publication date as RFC 3339 when known.
   * Omitted when absent (wire zero value).
   */
  publishedAt?: string;
  /**
   * When the account bought or added the title, as RFC 3339, when known.
   * Omitted when absent (wire zero value).
   */
  purchasedAt?: string;
  /**
   * Category / genre labels as a single string when known.
   * Omitted when absent (wire zero value).
   */
  categories?: string;
  /** Whether the storefront marks the edition explicit (adult) content. */
  explicitness: Explicitness;
}

/**
 * One page of an account's scan pass, the result of `ContentSource.scan`.
 * A pass is the run of pages from an empty `cursor` to the page whose
 * `nextCursor` is empty; `syncToken` and `complete` are read from that last
 * page only. A guest that does not page answers the first call with every
 * title, an empty `nextCursor` and `complete = true`.
 */
export interface ScanPage {
  /** Titles for the host to upsert; at most `maxScanPageBooks`. */
  books: ScanBook[];
  /**
   * Product ids the storefront reports as gone from the account since the
   * pass's `syncToken` (delta passes only); at most `maxScanPageBooks`.
   */
  removed: string[];
  /**
   * Opaque position of the next page; empty ends the pass. At most
   * `maxScanCursorBytes`; never equal to the cursor this page answered.
   */
  nextCursor: string;
  /**
   * On the last page: the opaque token the host sends as
   * `ScanParams.syncToken` on the account's next pass; empty means the next
   * pass is a full one. At most `maxScanCursorBytes`.
   */
  syncToken: string;
  /**
   * On the last page: true when the pass listed every title the account
   * holds (a full pass), false for a delta pass that listed only changes.
   */
  complete: boolean;
  /** Storefront requests this page took. */
  storePages: number;
  /**
   * The account's credentials as the guest holds them after refreshing them
   * while producing this page (for example a renewed access token): the
   * same encoding login returned, a JSON object of at most
   * `maxRefreshedCredentialBytes`. The host stores them in place of the
   * credentials it sent, unless the account was signed in again meanwhile,
   * and sends them on the pass's later pages. Empty when nothing changed.
   * Omitted when absent (wire zero value).
   */
  refreshedCredentials?: Uint8Array;
}

/** Result union of `ContentSource.scan`. */
export type ScanReply =
  | { kind: "ok"; value: ScanPage } // Success: one page of the pass.
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

/** One file `fetchTitle` wrote under the cache directory. */
export interface FetchedFile {
  /** Absolute path to the file under `cacheDir`. */
  path: string;
  /** What the file is to the edition. */
  role: FetchedFileRole;
  /**
   * MIME type when the guest knows it (`audio/mp4`, `application/epub+zip`,
   * `image/jpeg`); the host sniffs the file either way.
   * Omitted when absent (wire zero value).
   */
  contentType?: string;
  /**
   * Part title (disc or chapter label), for a `part`.
   * Omitted when absent (wire zero value).
   */
  title?: string;
  /**
   * Playing time of the file in milliseconds, for audio, when known.
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
  /**
   * Carrier of what was fetched. Must be one of the storefront's declared
   * `MediaSupport.fetches`; `unspecified` is refused.
   */
  mediaKind: MediaKind;
  /**
   * The files the guest wrote under the cache directory: the edition's
   * ordered `part`s or one `whole` file, plus an optional `cover`.
   */
  files: FetchedFile[];
  /** Chapter markers; empty when unknown. */
  chapters: ChapterMarker[];
  /**
   * Companion PDF download URL when the store exposes one; the host
   * downloads it itself.
   * Omitted when absent (wire zero value).
   */
  pdfUrl?: string;
  /**
   * The account's credentials as the guest holds them after refreshing them
   * during the fetch, as in `ScanPage.refreshedCredentials`. Empty when
   * nothing changed.
   * Omitted when absent (wire zero value).
   */
  refreshedCredentials?: Uint8Array;
  /**
   * What `chapters` marks. The host ranks literary chapters above track
   * boundaries when it decides an edition's chapters.
   */
  chapterKind: ChapterKind;
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
  /** Maximum hits to return; 0 means the storefront default (20 hits). */
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
  /**
   * How the host asks the store to find the title. `unspecified` (an older
   * host) leaves the guest free to use any field, as before.
   */
  lookup: PriceLookup;
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
 * Params of `ContentSource.revokeAccount`. The host sends the credentials it
 * is about to delete so the guest can sign the device out at the store.
 */
export interface RevokeAccountParams {
  /** Account being revoked. */
  accountId: string;
  /**
   * Decrypted credential blob for this account, exactly as login returned
   * it; never log it. Empty when none is stored.
   * Omitted when absent (wire zero value).
   */
  credentials?: Uint8Array;
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
  /** Kind of work the hit is. */
  workKind: WorkKind;
  /** How the work is realised in this edition. */
  expressionKind: ExpressionKind;
  /** Carrier of this edition. */
  mediaKind: MediaKind;
  /** External identifiers (ISBN, ASIN, OpenLibrary ids). */
  identifiers: Identifier[];
  /** Credited contributors in billing order. */
  credits: Credit[];
  /** Series the work belongs to. */
  seriesEntries: SeriesEntry[];
  /**
   * Product id of the parent podcast show, for a podcast episode.
   * Omitted when absent (wire zero value).
   */
  parentProductId?: string;
  /**
   * The storefront's own rank for this hit in `searchCatalog` results:
   * 1-based position in the store's ordering for the requested sort across
   * all pages (page 2 of 20-hit pages starts at 21), or a store-wide
   * popularity rank when the store has one. 0 means unranked. Catalog search
   * merges stores by it for relevance and popularity sorts and puts unranked
   * hits after ranked ones. Other catalog methods leave it 0.
   * Omitted when absent (wire zero value).
   */
  rankHint?: number;
  /** Whether the storefront marks the edition explicit (adult) content. */
  explicitness: Explicitness;
  /**
   * Absolute `https://` URL of a short audio sample of this edition that
   * the storefront offers publicly (a few minutes, in any format the media
   * worker decodes). The host never hands it to a client: a book's details
   * play it through a same-origin path, fetched by a `covers` plugin with
   * `media = audio` and re-encoded in the media worker. Empty when the store
   * has none.
   * Omitted when absent (wire zero value).
   */
  sampleUrl?: string;
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
  /**
   * Every priced listing a title search matched (`lookup = title`), each
   * with its full catalog record, so the host can tell recordings apart by
   * identifiers, credits and kinds rather than by title. Empty for other
   * lookups and for guests that do not return records; `hint` then stays
   * the answer.
   */
  records: PricedRecord[];
}

/** One priced listing returned by a title search. */
export interface PricedRecord {
  /** The listing's catalog record: identifiers, credits, kinds, language. */
  record: CatalogHit;
  /** Its purchase link and prices, as `PurchaseHint` carries them. */
  hint: PurchaseHint;
}

/** Result union of `ContentSource.purchaseHint`. */
export type PurchaseHintReply =
  | { kind: "ok"; value: PurchaseHintResult } // Success: hint or not-found marker.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Params of `ContentSource.purchaseHints`: the titles to price together. */
export interface PurchaseHintsParams {
  /** One query per title, each as for `purchaseHint`; at most `maxListPage`. */
  queries: PurchaseHintParams[];
}

/** Success payload of `ContentSource.purchaseHints`. */
export interface PurchaseHintsResult {
  /** One answer per query, in the order the queries were sent. */
  results: PurchaseHintResult[];
}

/**
 * Result union of `ContentSource.purchaseHints`. A whole-batch `err` (for
 * example `rateLimited`) applies to every query in it.
 */
export type PurchaseHintsReply =
  | { kind: "ok"; value: PurchaseHintsResult } // Success: an answer per query.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** The stream one account would download for a title today. */
export interface QualityOffer {
  /** Codec of the offered stream. */
  codec: OfferedCodec;
  /** AAC profile when `codec` is `aac`; `unknown` otherwise. */
  aacProfile: OfferedAacProfile;
  /**
   * Average bitrate in kbit/s; zero when unknown.
   * Omitted when absent (wire zero value).
   */
  bitrateKbps?: number;
  /**
   * Sample rate in Hz; zero when unknown.
   * Omitted when absent (wire zero value).
   */
  sampleRateHz?: number;
  /**
   * Channel count; zero when unknown.
   * Omitted when absent (wire zero value).
   */
  channels?: number;
}

/** Params of `ContentSource.qualityOffers`: the titles one account holds. */
export interface QualityOffersParams {
  /** Account whose download would be made. */
  accountId: string;
  /**
   * Decrypted credential blob for this account; never log it. Empty when
   * none is stored.
   * Omitted when absent (wire zero value).
   */
  credentials?: Uint8Array;
  /**
   * The instance's settings as `application/json` (bitrate tier and the
   * like, which change what a download gets).
   */
  sourceConfig: ExtensibleConfig;
  /**
   * Store product ids, as the account's scan listed them; at most
   * `maxListPage`.
   */
  titleIds: string[];
  /**
   * The fetch options a download would carry (DRM path, xHE-AAC), so the
   * answer names what that download would get.
   */
  fetch: FetchOptions;
}

/** One title's answer in `QualityOffersResult`. */
export interface TitleQualityOffer {
  /** False when the store could not say for this title (`offer` is empty). */
  found: boolean;
  /** What a download would get, when `found`. */
  offer: QualityOffer;
}

/** Success payload of `ContentSource.qualityOffers`. */
export interface QualityOffersResult {
  /**
   * One answer per title id, in the order they were sent. A short list
   * leaves the rest unanswered.
   */
  offers: TitleQualityOffer[];
  /**
   * The account's credentials as the guest holds them after refreshing them
   * during the call, as in `ScanPage.refreshedCredentials`. Empty when
   * nothing changed.
   * Omitted when absent (wire zero value).
   */
  refreshedCredentials?: Uint8Array;
}

/**
 * Result union of `ContentSource.qualityOffers`. A whole-batch `err` (for
 * example `rateLimited`) applies to every title in it.
 */
export type QualityOffersReply =
  | { kind: "ok"; value: QualityOffersResult } // Success: an answer per title.
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
  /**
   * What the host opens the entrypoints for. `provision` opens an instance
   * that may not be configured yet: `CONFIG` and `SECRETS` hold what is
   * stored so far, required settings may be missing, and the host calls
   * only `Entrypoints.provisioning`.
   */
  purpose: InvocationPurpose;
}

/** One message on a Workers-style queue, delivered by `QueueConsumer.queue`. */
export interface QueueMessage {
  /** Outbox row id; stable across redeliveries. */
  id: string;
  /** When the producer sent the message (UTC Unix milliseconds). */
  timestampUnixMs: number;
  /** Encoded body; at most `maxEventPayloadBytes`. */
  body: Uint8Array;
  /** Body encoding: `json`, `text`, or `bytes`. */
  contentType: string;
  /** Delivery counter, starting at 1. */
  attempts: number;
  /** Producer plugin id; empty when the host sent it. */
  source: string;
}

/** One `QueueConsumer.queue` delivery: messages from one queue in send order. */
export interface QueueBatch {
  /** Queue the messages were sent to. */
  queue: string;
  /** Messages in delivery order; the reply carries one `QueueResult` per entry. */
  messages: QueueMessage[];
}

/** Redeliver a queue message later. */
export interface QueueRetry {
  /** Seconds before redelivery; zero applies the consumer's `retry_delay`. */
  delaySeconds: number;
  /** Short human reason. */
  reason: string;
}

/** Per-message outcome inside a `QueueConsumer.queue` batch reply. */
export type QueueResult =
  | { kind: "ack" } // Handled; the host marks the message delivered.
  | { kind: "retry"; value: QueueRetry }; // Redeliver later.

/**
 * Success payload of `QueueConsumer.queue`: exactly one entry per
 * `QueueBatch.messages` entry, in order. A short list is a host-side error
 * and the missing tail is retried.
 */
export interface QueueBatchOk {
  /** Per-message outcomes in delivery order. */
  results: QueueResult[];
}

/** Result union of `QueueConsumer.queue`. */
export type QueueBatchReply =
  | { kind: "ok"; value: QueueBatchOk } // Success: per-message outcomes.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** One message to send through a `[[queues.producers]]` binding. */
export interface QueueSendMessage {
  /** Encoded body; at most `maxEventPayloadBytes`. */
  body: Uint8Array;
  /** Body encoding: `json` (default when empty), `text`, or `bytes`. */
  contentType: string;
  /** Seconds before the message may be delivered; zero means now. */
  delaySeconds: number;
}

/** Success payload of `QueueProducer.send` / `sendBatch`. */
export interface QueueSendOk {
  /** Outbox row ids in send order. */
  messageIds: string[];
}

/** Result union of `QueueProducer.send` / `sendBatch`. */
export type QueueSendReply =
  | { kind: "ok"; value: QueueSendOk } // Success: the ids the host assigned.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Expiration and metadata for `KvNamespace.put`. */
export interface KvPutOptions {
  /** Absolute expiry (UTC Unix milliseconds); zero means none. */
  expirationUnixMs: number;
  /**
   * Relative expiry in seconds from the write; zero means none. Ignored when
   * `expirationUnixMs` is set.
   */
  expirationTtlSeconds: number;
  /** Opaque JSON metadata text, at most `maxKvMetadataBytes`; empty means none. */
  metadata: string;
}

/** Success payload of `KvNamespace.get`. */
export interface KvGetOk {
  /** True when the key exists and has not expired. */
  found: boolean;
  /** Stored value; empty when not found. */
  value: Uint8Array;
  /** Stored metadata JSON text; empty when none. */
  metadata: string;
  /** Absolute expiry (UTC Unix milliseconds); zero means none. */
  expirationUnixMs: number;
}

/** Result union of `KvNamespace.get`. */
export type KvGetReply =
  | { kind: "ok"; value: KvGetOk } // Success: the value, when found.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `KvNamespace.put`. */
export interface KvPutOk {
  /** Effective absolute expiry (UTC Unix milliseconds); zero means none. */
  expirationUnixMs: number;
}

/** Success payload of `KvNamespace.delete`. */
export interface KvDeleteOk {
  /** True when a live value existed before the delete. */
  existed: boolean;
}

/** Result union of `KvNamespace.put`. */
export type KvPutReply =
  | { kind: "ok"; value: KvPutOk } // Success: the write is durable.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Result union of `KvNamespace.delete`. */
export type KvDeleteReply =
  | { kind: "ok"; value: KvDeleteOk } // Success: the key is gone (or never existed).
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Page request for `KvNamespace.list`. */
export interface KvListOptions {
  /** Only keys starting with this prefix; empty means all. */
  prefix: string;
  /** Opaque cursor from a previous page; empty means the first page. */
  cursor: string;
  /** Most keys to return; zero means the host default, at most `maxListPage`. */
  limit: number;
}

/** One key on a `KvNamespace.list` page. */
export interface KvListKey {
  /** Key name. */
  key: string;
  /** Stored metadata JSON text; empty when none. */
  metadata: string;
  /** Absolute expiry (UTC Unix milliseconds); zero means none. */
  expirationUnixMs: number;
}

/** Success payload of `KvNamespace.list`. */
export interface KvListOk {
  /** Keys in lexical order. */
  keys: KvListKey[];
  /** True when no further page exists. */
  listComplete: boolean;
  /** Cursor for the next page; empty when `listComplete`. */
  cursor: string;
}

/** Result union of `KvNamespace.list`. */
export type KvListReply =
  | { kind: "ok"; value: KvListOk } // Success: one page of keys.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** One fire of a `[triggers] crons` schedule (`scheduled(controller)`). */
export interface ScheduledController {
  /** The schedule that fired, normalized to single spaces (`0 3 * * MON`). */
  cron: string;
  /**
   * Fire time, UTC unix milliseconds (always a whole minute). Retries of
   * one fire keep it.
   */
  scheduledTime: number;
  /** Durable id of this fire, stable across retries; dedupe on it. */
  fireId: string;
  /** Failure attempt, starting at 1. */
  attempt: number;
  /**
   * UTC unix milliseconds. Host fence/lease is authoritative; this hint must
   * not outlive the fence.
   */
  deadlineUnixMs: number;
}

/** Params of `MetadataProvider.lookup`. */
export interface MetadataLookupParams {
  /**
   * Identifiers naming the wanted edition or work; never empty. A provider
   * ignores schemes it does not serve.
   */
  identifiers: Identifier[];
  /**
   * Marketplace to answer for (`us`, `uk`, ...) when the provider is
   * regional.
   * Omitted when absent (wire zero value).
   */
  marketplace?: string;
  /**
   * Most records to return; the provider picks a small default when absent.
   * Omitted when absent (wire zero value).
   */
  limit?: number;
}

/** Params of `MetadataProvider.search`. */
export interface MetadataSearchParams {
  /** Title to match; never empty. */
  title: string;
  /**
   * Author display name to narrow the match when known.
   * Omitted when absent (wire zero value).
   */
  author?: string;
  /**
   * Preferred content language code when known.
   * Omitted when absent (wire zero value).
   */
  language?: string;
  /**
   * Marketplace to answer for when the provider is regional.
   * Omitted when absent (wire zero value).
   */
  marketplace?: string;
  /**
   * Most records to return; the provider picks a small default when absent.
   * Omitted when absent (wire zero value).
   */
  limit?: number;
}

/** Params of `MetadataProvider.chapters`. */
export interface MetadataChaptersParams {
  /** Identifiers of one edition (an ASIN for Audnexus); never empty. */
  identifiers: Identifier[];
  /**
   * Marketplace to answer for when the provider is regional.
   * Omitted when absent (wire zero value).
   */
  marketplace?: string;
}

/**
 * One edition as a metadata provider describes it: the same structured
 * fields as `CatalogHit`, without store pricing.
 */
export interface MetadataRecord {
  /** Primary title. */
  title: string;
  /**
   * Subtitle when distinct from `title`.
   * Omitted when absent (wire zero value).
   */
  subtitle?: string;
  /**
   * Long description / blurb when known.
   * Omitted when absent (wire zero value).
   */
  description?: string;
  /**
   * Cover image URL.
   * Omitted when absent (wire zero value).
   */
  coverUrl?: string;
  /**
   * The provider's page for this record.
   * Omitted when absent (wire zero value).
   */
  url?: string;
  /**
   * Publisher name when known.
   * Omitted when absent (wire zero value).
   */
  publisher?: string;
  /**
   * Runtime in whole minutes when known.
   * Omitted when absent (wire zero value).
   */
  lengthMinutes?: number;
  /**
   * Publication date string as the provider prints it.
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
  /** Whether the edition is abridged when the provider says so. */
  abridgement: Abridgement;
  /** Kind of work the record describes. */
  workKind: WorkKind;
  /** How the work is realised in this edition. */
  expressionKind: ExpressionKind;
  /** Carrier of this edition. */
  mediaKind: MediaKind;
  /** External identifiers (ISBN, ASIN, OpenLibrary ids). */
  identifiers: Identifier[];
  /** Credited contributors in billing order. */
  credits: Credit[];
  /** Series the work belongs to. */
  seriesEntries: SeriesEntry[];
  /**
   * Aggregate rating when the provider publishes one.
   * Omitted when absent (wire zero value).
   */
  ratingOverall?: number;
  /**
   * Number of ratings behind `ratingOverall` when known.
   * Omitted when absent (wire zero value).
   */
  ratingCount?: number;
}

/** Success payload of `MetadataProvider.lookup` / `search`. */
export interface MetadataRecords {
  /** Records in the provider's relevance order; empty when nothing matched. */
  records: MetadataRecord[];
}

/** Result union of `MetadataProvider.lookup` / `search`. */
export type MetadataRecordsReply =
  | { kind: "ok"; value: MetadataRecords } // Success: matching records.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * One chapter in a provider's chapter layout. Chapters are listed in
 * pre-order: a chapter with `depth` n + 1 is nested under the nearest
 * earlier chapter with `depth` n.
 */
export interface MetadataChapter {
  /** Chapter title. */
  title: string;
  /** Start offset in milliseconds from the beginning of the title. */
  startMs: number;
  /**
   * Length in milliseconds when known.
   * Omitted when absent (wire zero value).
   */
  lengthMs?: number;
  /** Nesting depth; 0 for a top-level chapter. */
  depth: number;
}

/** Success payload of `MetadataProvider.chapters`. */
export interface MetadataChapters {
  /** False when the provider has no chapter layout for the edition. */
  found: boolean;
  /** Chapters in pre-order; empty when not `found`. */
  chapters: MetadataChapter[];
  /**
   * Length of the store's spoken brand intro in milliseconds when known.
   * Omitted when absent (wire zero value).
   */
  brandIntroMs?: number;
  /**
   * Length of the store's spoken brand outro in milliseconds when known.
   * Omitted when absent (wire zero value).
   */
  brandOutroMs?: number;
  /**
   * Total runtime the layout was measured against, in milliseconds.
   * Omitted when absent (wire zero value).
   */
  runtimeMs?: number;
  /** Whether the provider marks the offsets as exact rather than estimated. */
  accurate: boolean;
}

/** Result union of `MetadataProvider.chapters`. */
export type MetadataChaptersReply =
  | { kind: "ok"; value: MetadataChapters } // Success: chapter layout or not-found marker.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Params of `CoverFetcher.fetchCovers`. */
export interface CoverFetchParams {
  /**
   * Absolute `https://` URLs to fetch, at most `maxCoverUrls`; never empty.
   * The host has already checked each one against its URL policy.
   */
  urls: string[];
  /**
   * Largest body, in bytes, the host accepts for one image; at most
   * `maxCoverBytes`. The sink refuses a larger body, which fails that URL
   * only.
   */
  maxBytes: number;
  /**
   * Conditional request headers per URL, in the same order as `urls`, from
   * the host's cached copy. Empty, or shorter than `urls`, means the
   * remaining URLs are fetched unconditionally.
   */
  conditions: CoverCondition[];
  /**
   * What the URLs name: cover art (the default) or a storefront's audio
   * sample (`CatalogHit.sampleUrl`). A fetcher sends a matching `Accept`
   * header and refuses a body whose `Content-Type` names something else;
   * the host checks and re-encodes either kind regardless.
   */
  media: RemoteMedia;
}

/**
 * Validators the host already holds for one URL, sent as conditional request
 * headers so an unchanged image costs a `304` instead of its body.
 */
export interface CoverCondition {
  /**
   * `If-None-Match` value: the `ETag` the host stored, verbatim; empty for
   * none.
   * Omitted when absent (wire zero value).
   */
  ifNoneMatch?: string;
  /**
   * `If-Modified-Since` value: the `Last-Modified` the host stored,
   * verbatim; empty for none.
   * Omitted when absent (wire zero value).
   */
  ifModifiedSince?: string;
}

/** What happened to one requested URL. */
export interface CoverImage {
  /** The requested URL, verbatim. */
  url: string;
  /** Whether the body was put into the sink under this URL's index. */
  stored: boolean;
  /**
   * `Content-Type` the server sent. Advisory: the host sniffs the bytes.
   * Omitted when absent (wire zero value).
   */
  contentType?: string;
  /**
   * Why nothing was stored (HTTP status, size cap, timeout, refused host).
   * Omitted when absent (wire zero value).
   */
  error?: string;
  /**
   * The server answered `304 Not Modified` to the URL's conditions: nothing
   * was put into the sink and the host's copy is still current.
   */
  notModified: boolean;
  /**
   * `ETag` the server sent, verbatim. Advisory: the host parses and caps it.
   * Omitted when absent (wire zero value).
   */
  etag?: string;
  /**
   * `Last-Modified` the server sent, verbatim.
   * Omitted when absent (wire zero value).
   */
  lastModified?: string;
  /**
   * `Cache-Control` the server sent, verbatim (several lines joined with
   * `, `).
   * Omitted when absent (wire zero value).
   */
  cacheControl?: string;
  /**
   * `Expires` the server sent, verbatim.
   * Omitted when absent (wire zero value).
   */
  expires?: string;
  /**
   * `Date` the server sent, verbatim.
   * Omitted when absent (wire zero value).
   */
  date?: string;
  /**
   * `Age` the server (or a cache in front of it) sent, verbatim.
   * Omitted when absent (wire zero value).
   */
  age?: string;
}

/** Success payload of `CoverFetcher.fetchCovers`. */
export interface CoverImages {
  /** One entry per requested URL, in request order. */
  images: CoverImage[];
}

/** Result union of `CoverFetcher.fetchCovers`. */
export type CoverImagesReply =
  | { kind: "ok"; value: CoverImages } // Success: one entry per requested URL.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * What one metadata file says about the edition it sits beside. A field the
 * file does not state is left out of `present` and means "unknown", never
 * "cleared"; a field in `present` with an empty value means the file states
 * that it is empty. The host checks every value against the `maxMetadataFile*`
 * caps and refuses a record that states a field its dialect does not carry.
 */
export interface MetadataFileRecord {
  /**
   * Fields the file states, each at most once; at most the fields the
   * dialect declares.
   */
  present: MetadataField[];
  /**
   * Primary title.
   * Omitted when absent (wire zero value).
   */
  title?: string;
  /**
   * Subtitle when distinct from `title`.
   * Omitted when absent (wire zero value).
   */
  subtitle?: string;
  /**
   * Long description / blurb; at most `maxMetadataFileDescriptionBytes`.
   * Omitted when absent (wire zero value).
   */
  description?: string;
  /**
   * Publisher name.
   * Omitted when absent (wire zero value).
   */
  publisher?: string;
  /**
   * Publication date as `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
   * Omitted when absent (wire zero value).
   */
  publishedAt?: string;
  /**
   * Content language as the file prints it (a code or a name).
   * Omitted when absent (wire zero value).
   */
  language?: string;
  /** Genre labels in file order. */
  genres: string[];
  /** Free tags in file order. */
  tags: string[];
  /** Whether the edition is abridged; `unknown` when the file does not say. */
  abridgement: Abridgement;
  /** How the work is realised; `unspecified` when the file does not say. */
  expressionKind: ExpressionKind;
  /** External identifiers the file carries. */
  identifiers: Identifier[];
  /**
   * Credited contributors in billing order, only in the roles `creditRoles`
   * names.
   */
  credits: Credit[];
  /**
   * Credit roles the file states, each at most once. A role listed here
   * with no credit means the file states nobody in that role; a role left
   * out is unknown. Empty unless `present` holds `credits`.
   */
  creditRoles: CreditRole[];
  /** Series the work belongs to. */
  seriesEntries: SeriesEntry[];
  /**
   * Chapter layout in pre-order (see `MetadataChapter`); at most
   * `maxMetadataFileChapters`, starts never decreasing.
   */
  chapters: MetadataChapter[];
  /**
   * Running time in milliseconds.
   * Omitted when absent (wire zero value).
   */
  durationMs?: number;
  /**
   * Whether the edition is explicit (adult) content; `unknown` when the
   * file does not say.
   */
  explicitness: Explicitness;
}

/** Params of `MetadataFiles.parse`. */
export interface MetadataFileParseParams {
  /**
   * Dialect id from the manifest's `[[metadata_files.dialects]]` that the
   * host routed the file to.
   */
  dialect: string;
  /** The file's name without any folder (`metadata.json`, `Book.opf`). */
  fileName: string;
  /** The whole file; at most `maxMetadataFileBytes`. */
  bytes: Uint8Array;
}

/** Success payload of `MetadataFiles.parse`. */
export interface MetadataFileParsed {
  /** What the file says. */
  record: MetadataFileRecord;
}

/**
 * Result union of `MetadataFiles.parse`. A file the dialect cannot read is
 * `invalid_params`, so the host records it as unreadable and moves on.
 */
export type MetadataFileParseReply =
  | { kind: "ok"; value: MetadataFileParsed } // Success: the parsed record.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * One audio file of the edition a sidecar describes, for formats that name
 * their audio (a CUE sheet's `FILE` lines).
 */
export interface MetadataAudioFile {
  /** The file's name without any folder, as it sits next to the sidecar. */
  name: string;
  /** Running time in milliseconds; 0 when the host does not know it. */
  durationMs: number;
}

/** Params of `MetadataFiles.render`. */
export interface MetadataFileRenderParams {
  /** Dialect id from the manifest; one that declares `writes = true`. */
  dialect: string;
  /** Values to write. Only the fields in `fields` are read from it. */
  record: MetadataFileRecord;
  /**
   * Fields to set, each at most once and all declared by the dialect. A
   * listed field with an empty value removes it from the file.
   */
  fields: MetadataField[];
  /**
   * The file as it is now; empty when the host is creating it. Everything
   * the dialect does not change (unknown keys, comments, other fields) is
   * kept. At most `maxMetadataFileBytes`.
   */
  existing: Uint8Array;
  /**
   * The edition's audio files in playback order, at most
   * `maxMetadataAudioFiles`; empty when the host has none or does not know
   * them. Chapter starts in `record` count from the start of the first file.
   */
  audioFiles: MetadataAudioFile[];
}

/** Success payload of `MetadataFiles.render`. */
export interface MetadataFileRendered {
  /**
   * The new file; at most `maxMetadataFileBytes`. Empty when the dialect has
   * nothing left to write; the host decides whether to remove the file.
   */
  bytes: Uint8Array;
}

/** Result union of `MetadataFiles.render`. */
export type MetadataFileRenderReply =
  | { kind: "ok"; value: MetadataFileRendered } // Success: the new file.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** One labelled value shown with a notification. */
export interface NotificationField {
  /** Label; at most `maxNotificationTitleBytes`. */
  name: string;
  /** Value; at most `maxNotificationFieldBytes`. */
  value: string;
}

/**
 * A message the host rendered for one notification. Every link is built
 * from the library's public origin, never from a request.
 */
export interface NotificationMessage {
  /**
   * Host-assigned id, the same on every destination's copy; plugins use it
   * as an idempotency key where the service takes one.
   */
  notificationId: string;
  /** Notification type id (`job_failed`, `<plugin>:sync_failed`). */
  notificationType: string;
  /** How urgent it is. */
  severity: NotificationSeverity;
  /** Short title; at most `maxNotificationTitleBytes`. */
  title: string;
  /** Plain-text body; at most `maxNotificationTextBytes`. */
  text: string;
  /**
   * Markdown body for chat services; at most `maxNotificationTextBytes`.
   * Omitted when absent (wire zero value).
   */
  markdown?: string;
  /**
   * Minimal HTML body for email; at most `maxNotificationHtmlBytes`.
   * Omitted when absent (wire zero value).
   */
  html?: string;
  /**
   * Absolute `https` (or loopback `http`) link into the library.
   * Omitted when absent (wire zero value).
   */
  link?: string;
  /** Labelled values, at most `maxNotificationFields`. */
  fields: NotificationField[];
  /**
   * Messages with one group key may be threaded or collapsed together.
   * Omitted when absent (wire zero value).
   */
  groupKey?: string;
  /** Alert lifecycle step; `none` for a one-off message. */
  alertAction: NotificationAlertAction;
  /**
   * Alert this message opens or closes; required unless `alertAction` is
   * `none`.
   * Omitted when absent (wire zero value).
   */
  alertKey?: string;
  /**
   * True when the body carries a secret (an account-mail link): the plugin
   * and SDKs must never log it.
   */
  sensitive: boolean;
}

/** Where a `Notifier.send` goes. */
export interface NotifyDestination {
  /** Channel or personal destination. */
  kind: NotifyDestinationKind;
  /** Host id of the destination, stable across sends. */
  destinationId: string;
  /**
   * Personal address as a JSON object of the fields the manifest's
   * `[[notifier.address_fields]]` declare; empty for a channel. At most
   * `maxNotifierAddressBytes`. May hold secrets; never logged.
   */
  addressJson: string;
}

/** Params of `Notifier.send`. */
export interface NotifySendParams {
  /** What to send. */
  message: NotificationMessage;
  /** Where to send it. */
  destination: NotifyDestination;
}

/** Success payload of `Notifier.send`. */
export interface NotifySent {
  /**
   * The service's id for the delivered message, when it gives one.
   * Omitted when absent (wire zero value).
   */
  providerId?: string;
}

/** Result union of `Notifier.send`. */
export type NotifySendReply =
  | { kind: "ok"; value: NotifySent } // Delivered.
  | { kind: "err"; value: PluginError }; // Typed failure; `rate_limited`, `unavailable`, `deadline_exceeded` and `cancelled` are retried.

/** Params of `Notifier.validateDestination`. */
export interface NotifyValidateParams {
  /**
   * Address document as the patron entered it; at most
   * `maxNotifierAddressBytes`.
   */
  addressJson: string;
}

/** Success payload of `Notifier.validateDestination`. */
export interface NotifyValidated {
  /**
   * Normalised address document the host saves; at most
   * `maxNotifierAddressBytes`.
   */
  addressJson: string;
  /**
   * Short label shown to the patron (`a***@example.com`); never a secret.
   * Omitted when absent (wire zero value).
   */
  label?: string;
}

/**
 * Result union of `Notifier.validateDestination`. An address the plugin
 * refuses is `invalid_params`.
 */
export type NotifyValidateReply =
  | { kind: "ok"; value: NotifyValidated } // Accepted, normalised address.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Params of `NotificationRaiser.raise`. */
export interface NotifyRaiseParams {
  /**
   * Type id from the plugin's `[[notifications.types]]` (without the plugin
   * prefix the host adds).
   */
  notificationType: string;
  /** Labelled values, at most `maxNotificationFields`. */
  fields: NotificationField[];
  /**
   * Patron the notification is for; required for a patron-audience type
   * and must be inside the invocation's account scope; empty otherwise.
   * Omitted when absent (wire zero value).
   */
  patronId?: string;
  /**
   * Path inside the library the notification links to (starts with `/`);
   * the host prefixes the public origin.
   * Omitted when absent (wire zero value).
   */
  linkPath?: string;
  /**
   * Messages with one group key may be threaded or collapsed together.
   * Omitted when absent (wire zero value).
   */
  groupKey?: string;
  /** Alert lifecycle step; `none` for a one-off notification. */
  alertAction: NotificationAlertAction;
  /**
   * Alert this raise opens or closes; required unless `alertAction` is
   * `none`.
   * Omitted when absent (wire zero value).
   */
  alertKey?: string;
}

/** Success payload of `NotificationRaiser.raise`. */
export interface NotifyRaised {
  /** Host-assigned notification id. */
  notificationId: string;
}

/** Result union of `NotificationRaiser.raise`. */
export type NotifyRaiseReply =
  | { kind: "ok"; value: NotifyRaised } // Accepted by the router.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** One model an `ai` plugin serves. */
export interface AiModel {
  /**
   * Model id within the plugin (`bekko-a8m`): lowercase letters, digits,
   * `.`, `_` and `-`. Callers name it `@<plugin id>/<model id>`; the whole
   * name is at most `maxAiModelNameBytes`.
   */
  modelId: string;
  /** The model's one task. */
  task: AiTask;
  /**
   * Digest of everything that decides the output: weights, tokenizer and
   * the language packs loaded. Outputs are comparable only when it matches.
   */
  revision: string;
  /**
   * Text embeddings: numbers per vector, at most `maxAiEmbedDimensions`.
   * Omitted when absent (wire zero value).
   */
  dimensions?: number;
  /**
   * Text embeddings: tokens read from one text; the rest is cut off.
   * Omitted when absent (wire zero value).
   */
  maxTokens?: number;
  /**
   * BCP 47 tags of the languages the model is loaded for, at most
   * `maxAiLanguages`; empty when it has no language packs.
   */
  languages: string[];
}

/** Success payload of `AiModels.models`. */
export interface AiModelList {
  /** Served models, at most `maxAiModels`, each `modelId` once. */
  models: AiModel[];
}

/** Result union of `AiModels.models`. */
export type AiModelsReply =
  | { kind: "ok"; value: AiModelList } // The served models.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** One text to embed. */
export interface AiEmbedText {
  /** The text, at most `maxAiEmbedTextBytes`; never empty. */
  text: string;
  /** Document or query. */
  role: AiTextRole;
}

/** Input of the `textEmbeddings` task. */
export interface AiTextEmbeddingsInput {
  /** Texts to embed, 1 to `maxAiEmbedTexts`. */
  texts: AiEmbedText[];
}

/** Output of the `textEmbeddings` task. */
export interface AiTextEmbeddingsOutput {
  /** Numbers per vector; equals the model's `AiModel.dimensions`. */
  dimensions: number;
  /**
   * One vector per input text, in input order: `dimensions` little-endian
   * IEEE 754 32-bit floats each, concatenated. Every number is finite.
   */
  vectors: Uint8Array;
  /** `AiModel.revision` of the model that made the vectors. */
  revision: string;
  /**
   * Indexes (into the input texts, ascending, each once) of the texts
   * where the tokenizer met text outside the loaded language packs and
   * fell back to smaller pieces; such a vector should be recomputed after
   * the missing pack is added.
   */
  fallbacks: number[];
}

/**
 * Input to one model run: one field per `AiTask`, and exactly one is set
 * (the others are null). A new task appends a field.
 */
export interface AiInput {
  /** `textEmbeddings` task. */
  textEmbeddings: AiTextEmbeddingsInput;
}

/**
 * Output of one model run: one field per `AiTask`, and exactly one is set
 * (the others are null). A new task appends a field.
 */
export interface AiOutput {
  /** `textEmbeddings` task. */
  textEmbeddings: AiTextEmbeddingsOutput;
}

/** Params of `AiModels.run` and `AiRunner.run`. */
export interface AiRunParams {
  /**
   * On `AiModels.run`, the plugin's own `AiModel.modelId`. On
   * `AiRunner.run`, `@<plugin id>/<model id>` or a task alias
   * (`@library/text-embeddings`: the model the library chose for the task).
   */
  model: string;
  /** The input; the field set must match the model's task. */
  input: AiInput;
}

/** Success payload of a model run. */
export interface AiRunOk {
  /** The output; the field set matches the model's task. */
  output: AiOutput;
  /**
   * Full name of the model that ran (`@<plugin id>/<model id>`), so a
   * caller that used a task alias knows what answered. Set by the host on
   * `AiRunner.run`; empty on `AiModels.run`.
   * Omitted when absent (wire zero value).
   */
  model?: string;
}

/**
 * Result union of `AiModels.run` and `AiRunner.run`. A model the plugin
 * does not serve is `not_found`; an input for another task is
 * `invalid_params`; a caller without the grant gets `forbidden`.
 */
export type AiRunReply =
  | { kind: "ok"; value: AiRunOk } // The model ran.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/**
 * One WebSocket message, or one chunk of a `tls://` stream (always
 * `binary`).
 */
export type SocketFrame =
  | { kind: "text"; value: string } // Text message, UTF-8.
  | { kind: "binary"; value: Uint8Array }; // Binary message.

/** One request header on `Sockets.connect`. */
export interface SocketHeader {
  /** Header name; the broker refuses hop-by-hop and WebSocket handshake names. */
  name: string;
  /** Header value. May carry a credential: the host never logs it. */
  value: string;
}

/**
 * A heartbeat the broker answers itself: when `request` arrives byte for byte,
 * it sends `response` and does not wake the guest.
 */
export interface SocketAutoResponse {
  /** Frame to match, at most `maxSocketAutoResponseBytes`. */
  request: SocketFrame;
  /** Frame to send back, at most `maxSocketAutoResponseBytes`. */
  response: SocketFrame;
}

/** Arguments of `Sockets.connect`. */
export interface SocketConnectRequest {
  /**
   * `wss://` or `ws://` URL, at most `maxSocketUrlBytes`, or
   * `tls://host:port` for a raw TLS byte stream (port required, nothing
   * after it). The host must be on the plugin's network grant; a `tls://`
   * stream needs a `tcp` grant for that host and port.
   */
  url: string;
  /**
   * Requested subprotocols, at most `maxSocketHeaders`; on a `tls://`
   * stream, the ALPN protocol ids to offer (visible ASCII, 1 to 255 bytes).
   */
  protocols: string[];
  /** Extra request headers, at most `maxSocketHeaders`; none on `tls://`. */
  headers: SocketHeader[];
  /**
   * Heartbeats the broker answers, at most `maxSocketAutoResponses`; none on
   * `tls://`.
   */
  autoResponses: SocketAutoResponse[];
  /** Initial attachment, at most `maxSocketAttachmentBytes`. */
  attachment: Uint8Array;
  /**
   * `tls://` only: DER root certificates, at most `maxSocketTrustAnchors` of
   * at most `maxSocketTrustAnchorBytes` each, that the server's chain must
   * end in instead of Mozilla's roots, for a service that signs its own
   * (an ngrok or Cloudflare Tunnel agent endpoint). Empty uses Mozilla's.
   */
  trustAnchors: Uint8Array[];
  /**
   * `tls://` only: the DNS name to send as SNI and to check the server's
   * certificate against, for an endpoint reached at one name that answers
   * for another (a Cloudflare Tunnel edge, dialed at `region1.v2.argotunnel.com`
   * and serving `h2.cftunnel.com`). It must be on a `tcp` grant for the
   * URL's port too, so it can't route past the grant by name. At most
   * `maxSocketServerNameBytes`; empty uses the URL's host.
   */
  serverName: string;
}

/** One open host-brokered socket. */
export interface SocketInfo {
  /** Host-minted socket id, stable for the socket's life. */
  socket: string;
  /** URL the socket was opened to, without credentials. */
  url: string;
  /**
   * Subprotocol the server chose (on `tls://`, the negotiated ALPN id);
   * empty when none.
   * Omitted when absent (wire zero value).
   */
  protocol?: string;
  /** Current attachment. */
  attachment: Uint8Array;
}

/** Result union of `Sockets.connect`. */
export type SocketConnectReply =
  | { kind: "ok"; value: SocketInfo } // Success: the handshake finished and the socket is open.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `Sockets.list`. */
export interface SocketList {
  /** Open sockets, oldest first. */
  sockets: SocketInfo[];
}

/** Result union of `Sockets.list`. */
export type SocketListReply =
  | { kind: "ok"; value: SocketList } // Success: the open sockets.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Reply of `SocketHandler.message`. */
export type SocketMessageReply =
  | { kind: "ok"; value: SocketMessageOk } // Success: what the guest asks of the host after the message.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** What a guest asks of the host after handling one pushed message. */
export interface SocketMessageOk {
  /**
   * True asks the host to call `Integration.pollEvents` now, while the
   * guest is awake, instead of at the next scheduled poll: the message
   * announced something the poll hands over (a new user). The host ignores
   * it for a plugin whose host schedules no poll.
   */
  pollEvents: boolean;
}

/** Argument of `SocketHandler.message`. */
export interface SocketMessage {
  /** Socket the message arrived on. */
  socket: string;
  /** The message. */
  frame: SocketFrame;
  /** The socket's attachment when the message arrived. */
  attachment: Uint8Array;
}

/** Argument of `SocketHandler.closed`. */
export interface SocketClosed {
  /** Socket that closed; its id is not reused. */
  socket: string;
  /** WebSocket close code; 1006 when the connection dropped without one. */
  code: number;
  /**
   * Close reason the peer sent; empty when none.
   * Omitted when absent (wire zero value).
   */
  reason?: string;
  /** True when the close handshake completed. */
  clean: boolean;
  /** The socket's last attachment. */
  attachment: Uint8Array;
}

/** One header field on an `Origin.fetch` request or response. */
export interface OriginHeader {
  /** Field name, an HTTP token; the host refuses hop-by-hop names. */
  name: string;
  /**
   * Field value, visible ASCII, spaces and tabs. May carry a credential (a
   * cookie, an authorization): the host never logs it.
   */
  value: string;
}

/**
 * The head of one public request a relay `ingress` plugin carries
 * (`Origin.fetch`). Its scheme is `https` and its authority is the library's
 * public origin; neither is the plugin's to choose.
 */
export interface OriginRequest {
  /** Request method, an HTTP token (`GET`, `PUT`, ...). */
  method: string;
  /**
   * Path and query in origin form (`/api/status?x=1`), at most
   * `maxOriginTargetBytes`.
   */
  target: string;
  /**
   * Request header fields in order, at most `maxOriginHeaders` and
   * `maxOriginHeaderBytes`. `host` and forwarding headers are ignored.
   */
  headers: OriginHeader[];
  /**
   * IP address of the client, from the provider's own connection metadata;
   * empty when the provider gives none.
   * Omitted when absent (wire zero value).
   */
  clientAddress?: string;
}

/**
 * The library's answer to an `Origin.connect` it did not upgrade: an
 * ordinary response (`401`, `403`, `404`, ...) for the plugin to hand back
 * to the client.
 */
export interface OriginRefusal {
  /** HTTP status code, 200 to 599 and never `101`. */
  status: number;
  /**
   * Response header fields in order, at most `maxOriginHeaders` and
   * `maxOriginHeaderBytes`.
   */
  headers: OriginHeader[];
  /** Response body, at most `maxOriginRefusalBodyBytes`. */
  body: Uint8Array;
}

/** Success payload of `Origin.connect`. */
export type OriginConnected =
  | { kind: "socket"; value: SocketInfo } // The library accepted the upgrade; the socket is open in `Bindings.sockets`. Its `protocol` is the subprotocol the library chose.
  | { kind: "refused"; value: OriginRefusal }; // The library answered without upgrading.

/** Result union of `Origin.connect`. */
export type OriginConnectReply =
  | { kind: "ok"; value: OriginConnected } // Success: an open socket or the library's refusal.
  | { kind: "err"; value: PluginError }; // Typed failure: the request was refused before the library saw it (`invalid_params`, `forbidden` without `[sockets]`, `limit_exceeded` when the instance holds `maxOriginSocketsPerInstance`) or the library is unavailable.

/** Success payload of `Ingress.start`. */
export interface IngressStarted {
  /**
   * The public origin now serving: `https://` and a host name, no port, path
   * or trailing slash, at most `maxIngressOriginBytes` after the scheme.
   */
  origin: string;
}

/** Result union of `Ingress.start`. */
export type IngressStartReply =
  | { kind: "ok"; value: IngressStarted } // Success: the address is up.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `Webhooks.url`. */
export interface WebhookUrl {
  /**
   * Absolute `https://` URL; empty when the library cannot be reached from
   * the Internet or webhooks are turned off, and the plugin should use its
   * fallback.
   * Omitted when absent (wire zero value).
   */
  url?: string;
}

/** Result union of `Webhooks.url`. */
export type WebhookUrlReply =
  | { kind: "ok"; value: WebhookUrl } // Success: the URL, or none.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Arguments of `Webhooks.pull`. */
export interface WebhookPullRequest {
  /** `[[webhooks]]` name. */
  name: string;
  /** Most deliveries to lease, 1 to `maxWebhookPullBatch`. */
  batchSize: number;
  /**
   * How long leased deliveries stay hidden from the next pull, in
   * milliseconds, at most `maxWebhookVisibilityMs`; zero means one minute.
   */
  visibilityTimeoutMs: number;
}

/** One request header kept with a delivery. */
export interface WebhookHeader {
  /** Lowercase header name, one the row's `headers` names or `content-type`. */
  name: string;
  /** Header value as received. */
  value: string;
}

/** One stored request, leased to the guest. */
export interface WebhookDelivery {
  /**
   * Lease id for `ack`, at most `maxWebhookLeaseIdBytes`; a new one each
   * time the delivery is pulled.
   */
  leaseId: string;
  /** When the host stored the request, in Unix milliseconds. */
  receivedAtUnixMs: number;
  /** How many times this delivery has been pulled, counting this one. */
  attempt: number;
  /** Kept request headers, in the order the row names them. */
  headers: WebhookHeader[];
  /** Request body as received, at most the row's `max_body_bytes`. */
  body: Uint8Array;
}

/** Success payload of `Webhooks.pull`. */
export interface WebhookPullPage {
  /** Leased deliveries, oldest first. */
  deliveries: WebhookDelivery[];
  /**
   * True when the host dropped deliveries for this webhook since the last
   * pull (the waiting cap was full or a delivery expired unacknowledged):
   * the guest should treat what it follows as incomplete.
   */
  missed: boolean;
}

/** Result union of `Webhooks.pull`. */
export type WebhookPullReply =
  | { kind: "ok"; value: WebhookPullPage } // Success: the leased deliveries.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Success payload of `Ingress.health`. */
export interface IngressHealth {
  /** True when the address is serving. */
  serving: boolean;
  /**
   * Why it is not serving, for the operator; empty when serving.
   * Omitted when absent (wire zero value).
   */
  detail?: string;
}

/** Result union of `Ingress.health`. */
export type IngressHealthReply =
  | { kind: "ok"; value: IngressHealth } // Success: the provider's view of the address.
  | { kind: "err"; value: PluginError }; // Typed failure; `code` is a `PluginErrorCode` wire string.

/** Arguments of `Webhooks.ack`. */
export interface WebhookAckRequest {
  /** `[[webhooks]]` name. */
  name: string;
  /** Leases whose deliveries are done and are deleted. */
  acks: string[];
  /** Leases whose deliveries the next pull offers again. */
  retries: string[];
}

/** Argument of `WebhookHandler.delivered`. */
export interface WebhookDelivered {
  /** `[[webhooks]]` name. */
  name: string;
  /** Deliveries waiting for this webhook when the host called. */
  waiting: number;
}

/**
 * A short-lived credential for one guided setup run. The host never stores
 * or logs it; the plugin uses it only during the call.
 */
export interface ProvisionGrant {
  /** Kind of grant. */
  kind: ProvisionGrantKind;
  /**
   * The token; empty for `none`. At most `maxProvisionTokenBytes`.
   * Omitted when absent (wire zero value).
   */
  token?: string;
  /**
   * When the token expires (UTC Unix milliseconds); zero when unknown or it
   * does not expire.
   * Omitted when absent (wire zero value).
   */
  expiresAtUnixMs?: number;
}

/** Arguments of `Provisioning.provision`. */
export interface ProvisionRequest {
  /** The operator's grant; never `none`. */
  grant: ProvisionGrant;
  /**
   * The operator's choices as a JSON object keyed by `[provisioning] inputs`
   * setting keys (`{"bucket": "books"}`); `{}` when none were asked for.
   */
  inputsJson: string;
}

/** One credential a guided setup returns for the host to seal. */
export interface ProvisionedSecret {
  /** A `secret` setting key listed in `[provisioning] outputs`. */
  key: string;
  /** The credential, at most `maxProvisionSecretBytes`. */
  value: string;
}

/** Success payload of `Provisioning.provision`. */
export interface Provisioned {
  /**
   * Non-secret settings to store, as a JSON object keyed by setting keys
   * listed in `[provisioning] outputs`; at most `maxProvisionSettingsBytes`.
   */
  settingsJson: string;
  /** Credentials to seal; each key at most once. */
  secrets: ProvisionedSecret[];
  /**
   * Short lines for the setup screen ("Created bucket books"), at most
   * `maxProvisionSummaryLines` of `maxProvisionSummaryLineBytes`. Never a
   * credential.
   */
  summary: string[];
}

/** Result union of `Provisioning.provision`. */
export type ProvisionReply =
  | { kind: "ok"; value: Provisioned } // Success: what to store.
  | { kind: "err"; value: PluginError }; // Typed failure: `invalid_params` for a refused choice, `unauthorized` or `forbidden` when the grant cannot do what is needed.

/** Arguments of `Provisioning.deprovision`. */
export interface DeprovisionRequest {
  /** The operator's grant, or kind `none` to use the stored secrets. */
  grant: ProvisionGrant;
}

/** Params of `RemoteLibrary.pollEvents`. */
export interface EventPollParams {
  /**
   * The host calls `ackEvents` with every user it passes on, so the guest
   * keeps returning a user until it is acknowledged. False from hosts that
   * predate `ackEvents`: the guest then treats a returned user as delivered.
   */
  ackFollows: boolean;
}

/** Params of `RemoteLibrary.ackEvents`. */
export interface EventAckParams {
  /** `ExternalUser.externalUserId` of each user the host passed on. */
  externalUserIds: string[];
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

/** First frame of a streamed execute reply: what the reply is about. */
export interface ExecuteStreamHead {
  /** Echo of the request `operationId`. */
  operationId: string;
  /** Number of `statement` frames that follow, one per request statement. */
  statementCount: number;
}

/**
 * Opens one statement result inside a streamed execute reply. Its rows, if
 * any, follow in `rows` frames until the next `statement`, `end` or `err`.
 */
export interface ExecuteStreamStatement {
  /** Request statement index, `0`-based and increasing by one per frame. */
  index: number;
  /** Result column descriptors. */
  columns: DbColumn[];
  /** Rows changed by a mutation. */
  rowsAffected: number;
}

/**
 * One frame of a streamed execute reply (`executeStreamed`). Frames are
 * consecutive unpacked single-segment Cap'n Proto messages, each at most
 * `maxScalarBytes` unless it carries one row that alone exceeds it. The
 * sequence is `head`, then for each statement `statement` followed by its
 * `rows` frames in order, then `end`; `err` ends the sequence early.
 */
export type ExecuteStreamFrame =
  | { kind: "head"; value: ExecuteStreamHead } // First frame.
  | { kind: "statement"; value: ExecuteStreamStatement } // Opens the next statement result.
  | { kind: "rows"; value: DbRow[] } // Rows of the open statement, in order.
  | { kind: "end"; value: DbTiming } // Last frame: the batch committed; engine timing.
  | { kind: "err"; value: PluginError }; // Last frame: the batch failed; `code` is a `PluginErrorCode` wire string.

/**
 * Result union of `executeStreamed`. The frames (`ExecuteStreamFrame`) travel
 * inline when they fit one scalar, otherwise as a transferred byte stream in
 * `maxStreamWindowBytes` windows. The adapter counts the reply against
 * `maxStreamedResultBytes` and `maxStreamedResultRows` before it commits, so
 * results still fail closed and are never truncated.
 */
export type ExecuteStreamReply =
  | { kind: "inline"; value: Uint8Array } // Success: every frame, concatenated, at most `maxScalarBytes`.
  | { kind: "stream"; value: ByteSource } // Success: the frames as a byte stream.
  | { kind: "err"; value: PluginError }; // Typed failure before any frame; `code` is a `PluginErrorCode` wire string.

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
  /**
   * Maximum encoded bytes of one `ExecuteReply` answered through
   * `executeStreamed` (the sum of its frames' statement results). `0` means
   * the adapter does not stream and callers stay under the inline caps.
   * When set it is at least `maxAtomicResultBytes`.
   */
  maxStreamedResultBytes: number;
  /**
   * Maximum rows one statement may return through `executeStreamed`. `0`
   * means the adapter does not stream. When set it is at least
   * `maxResultRows`.
   */
  maxStreamedResultRows: number;
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
  /**
   * Physically drop one provisioned binding unit (`unitRef` is adapter-owned).
   *
   * @param unitRef - Adapter-native unit (sqlite path, postgres database, D1 name).
   * @returns {@link EmptyReply}
   */
  dropUnit(unitRef: string): Promise<EmptyReply>;
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
  /**
   * `execute` whose reply is framed (`ExecuteStreamFrame`) and counted against
   * `maxStreamedResultBytes` / `maxStreamedResultRows` instead of the inline
   * caps. One transaction, one snapshot; `unsupported` when the adapter
   * advertises no streamed budget.
   *
   * @param request - Proven statements plus isolation and receipt.
   * @returns {@link ExecuteStreamReply}
   */
  executeStreamed(request: AdapterExecuteRequest): Promise<ExecuteStreamReply>;
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
  /**
   * Negotiated limits of this session, so guests size requests and reads
   * before sending instead of discovering caps from errors.
   *
   * @returns {@link DbCapabilitiesReply}
   */
  limits(): Promise<DbCapabilitiesReply>;
  /**
   * `execute` whose reply is framed (`ExecuteStreamFrame`) and counted against
   * the session's `maxStreamedResultBytes` / `maxStreamedResultRows`, so a
   * result past the inline caps still arrives whole. `unsupported` when the
   * session advertises no streamed budget.
   *
   * @param request - Statements, binds, and deadline.
   * @returns {@link ExecuteStreamReply}
   */
  executeStreamed(request: ExecuteRequest): Promise<ExecuteStreamReply>;
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
