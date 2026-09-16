# Bookclerk plugin ABI — object-capability Workers RPC (`api_version = 3`).
#
# This file is the first supported plugin contract. Discarded development
# compatibility fields were removed and ordinals compacted. From this contract
# forward:
# - `apiVersion` / `plugin.toml` `api_version` is the single incompatible ABI
#   version. Manifest `api_version` and `describe().apiVersion` must match.
# - Named `rpcFeatures` negotiate optional facilities. Required features are
#   rejected at spawn when missing.
# - Do not reuse field, method, or union ordinals.
# - Unknown enum/union members: preserve the wire code and fail closed or
#   return typed `unsupported`. Never collapse unknown codes to `internal`.
# - Every variable-length field is bounded by the constants below.
# - Identifiers are non-empty `[a-z][a-z0-9_]{0,63}`. Timestamps are UTC
#   unix milliseconds (UInt64); zero means omitted.
# - Absent entrypoints are null capabilities in `Entrypoints`; absent methods
#   return typed `unsupported`.
# - `describe()` advertises typed `PluginCapabilities` (exported entrypoints,
#   triggers, bindings). The signed manifest plus the operator grant is the
#   host allowlist; a guest that widens beyond either is refused.
# - The guest root is `PluginWorker`: `describe()`, then
#   `open(invocation, bindings)` returns the exported `Entrypoints` for one
#   invocation. Bindings (`env` in the Workers idiom) carry config, secrets,
#   the `EVENTS` publisher, and named `[[databases]]` sessions.
#
# Authors never see transport-private capability table indexes. Public types
# are the interfaces below plus TypeScript `BookclerkEntrypoint`. ByteSource is
# the Cap'n Proto realization of a transferred byte ReadableStream.
#
# Method results are typed success/error unions. SDKs map `err` onto a thrown
# PluginError. Unknown future `code` strings MUST be preserved.
@0x816df58cae22db0c;

# Product ABI version (`plugin.toml` `api_version` / `describe().apiVersion`).
const apiVersion :UInt32 = 3;
# Maximum decoded size of an ordinary RPC scalar value (not a stream window).
const maxScalarBytes :UInt32 = 262144;
# Maximum bytes returned by one `ByteSource.pull` (flow-control window).
const maxStreamWindowBytes :UInt32 = 1048576;
# Maximum objects in one `Destination.list` page.
const maxListPage :UInt32 = 256;
# Maximum job / event checkpoint payload size (bytes).
const maxCheckpointBytes :UInt32 = 65536;
# Maximum plugin / account identifier length (bytes).
const maxIdentifierBytes :UInt32 = 64;
# Maximum granted config payload size (bytes).
const maxConfigPayloadBytes :UInt32 = 65536;
# Maximum decoded size of a domain-event scalar payload (not a stream).
const maxEventPayloadBytes :UInt32 = 65536;
# Plugin `databaseMigrations` is a startup-time scalar (not a stream). Count is
# `maxListPage`. Each SQL text is `maxScalarBytes`. Ops-per-migration, total
# operations across the registration, and the aggregate UTF-8 bytes of ids +
# SQL have dedicated caps so a jailed guest cannot force unbounded host
# allocation before semantic proof. `maxPluginMigrationRegistrationBytes` is
# id+SQL text only; `maxPluginMigrationTotalOps` bounds the structural object
# graph (2048: enough for realistic histories, including 256 migrations of ~8
# ops or 8 migrations at the per-migration cap, and far below 256×256).

# Maximum already-separated operations in one plugin-owned migration.
const maxPluginMigrationOps :UInt32 = 256;
# Maximum already-separated operations across one `databaseMigrations` registration.
const maxPluginMigrationTotalOps :UInt32 = 2048;
# Maximum aggregate UTF-8 bytes of plugin migration ids plus SQL in one `databaseMigrations` registration.
const maxPluginMigrationRegistrationBytes :UInt32 = 262144;

# Negotiable `rpcFeatures` wire names (see `PluginDescribe.rpcFeatures`).

# Guest honors scalar / stream-window / list-page caps (`rpc.scalarLimits`).
const featureScalarLimits :Text = "rpc.scalarLimits";
# Media moves through transferred `ByteRange` / `ByteSource` streams (`rpc.streams`).
const featureStreams :Text = "rpc.streams";
# Guest implements server-side `Destination.copy` (`storage.copy`).
const featureStorageCopy :Text = "storage.copy";

# Guest-advertised caps for `rpc.scalarLimits`; never above the file constants.
struct ScalarLimits {
  # Largest scalar the guest accepts; at most `maxScalarBytes`.
  maxScalarBytes @0 :UInt32;
  # Largest `ByteSource.pull` window; at most `maxStreamWindowBytes`.
  maxStreamWindowBytes @1 :UInt32;
  # Largest list page; at most `maxListPage`.
  maxListPage @2 :UInt32;
}

# `code` is a snake_case string (`not_found`, `invalid_cursor`, …). Unknown
# codes are forwarded as-is; SDKs surface them as PluginErrorCode.unknown
# while retaining the raw wire code.
struct PluginError {
  # Stable snake_case error code (see `PluginErrorCode`).
  code @0 :Text;
  # Human-readable detail; never contains secrets.
  message @1 :Text;
}

# Full metadata of one stored object.
struct ObjectMetadata {
  # Object key.
  key @0 :Text;
  # Object size in bytes.
  size @1 :UInt64;
  # MIME type; empty when unknown.
  contentType @2 :Text;
  # Backend entity tag; empty when unsupported.
  etag @3 :Text;
  # Raw SHA-256 digest (32 bytes) or empty when unknown.
  sha256 @4 :Data;
}

# Compact object entry in a list page.
struct ObjectInfo {
  # Object key.
  key @0 :Text;
  # Object size in bytes.
  size @1 :UInt64;
}

# Paging options for `Destination.list`.
struct ListOptions {
  # Only keys starting with this prefix; empty lists everything.
  prefix @0 :Text;
  # Opaque cursor from a previous `ListPage.nextCursor`; empty starts over.
  cursor @1 :Text;
  # Requested page size; clamped to `maxListPage`. `0` means guest default.
  limit @2 :UInt32;
}

# One page of `Destination.list` results.
struct ListPage {
  # Objects in this page, in backend order.
  objects @0 :List(ObjectInfo);
  # Cursor for the next page; empty when exhausted.
  nextCursor @1 :Text;
}

# Half-open byte window `[offset, offset + length)`.
struct ByteRange {
  # First byte offset.
  offset @0 :UInt64;
  # Number of bytes; `0` reads to the end.
  length @1 :UInt64;
}

# Options for `Destination.get`.
struct ReadOptions {
  # Byte range to read; an all-zero range reads the whole object.
  range @0 :ByteRange;
}

# Options for `Destination.put`.
struct WriteOptions {
  # MIME type to record; empty when unknown.
  contentType @0 :Text;
  # Expected body length in bytes; `0` when unknown (chunked).
  contentLength @1 :UInt64;
  # Expected raw SHA-256 digest; empty skips verification.
  sha256 @2 :Data;
  # Destination-side stage-and-publish. Empty means a one-shot put.
  commitToken @3 :Text;
  # When true, `put` stages remotely and does not publish until `commit`.
  stageOnly @4 :Bool;
}

# Summary of a stored (or committed) object.
struct PutResult {
  # Object key written.
  key @0 :Text;
  # Bytes persisted.
  bytesWritten @1 :UInt64;
  # Backend entity tag; empty when unsupported.
  etag @2 :Text;
  # Raw SHA-256 digest of the stored bytes; empty when not computed.
  sha256 @3 :Data;
}

# Summary of a server-side copy.
struct CopyResult {
  # Bytes copied.
  bytesCopied @0 :UInt64;
}

# Guest identity and negotiation surface returned by `describe()`.
struct PluginDescribe {
  # ABI version the guest speaks; must equal `apiVersion`.
  apiVersion @0 :UInt32;
  # Stable plugin id (`[a-z][a-z0-9_]{0,63}`).
  id @1 :Text;
  # Human-readable name for UI lists.
  displayName @2 :Text;
  # Negotiable feature names the guest supports (see `feature*` constants).
  rpcFeatures @3 :List(Text);
  # Guest caps when `rpc.scalarLimits` is advertised.
  scalarLimits @4 :ScalarLimits;
  # Exported entrypoints, triggers, and bindings the guest implements. The
  # host rejects anything wider than the manifest and the operator grant.
  capabilities @5 :PluginCapabilities;
  # Portal Accounts connect mode for storefronts.
  portalAuthMode @6 :PortalAuthMode;
  # Env var name operators may set for password helpers; never required for
  # Accounts UI connect. Empty when the guest accepts none.
  passwordEnvVar @7 :Text $optional;
  # Alternate ids accepted for config / CLI targeting.
  aliases @8 :List(Text);
  # UI sort weight among peers of the same family; lower sorts first.
  sortKey @9 :UInt32;
  # Portal brand colors and icon URL; `brand.id` is empty when the guest has
  # no brand and the host renders a neutral fallback.
  brand @10 :Brand;
  # Discoverable config option groups for source UIs.
  configOptions @11 :List(ConfigOption);
  # Embedded CLI schema (same shape as `PluginCli.describe`); empty when unused.
  cli @12 :CliSchema;
}

# Marks a scalar field whose zero value (empty `Text` / `Data`, numeric `0`)
# means "absent". SDK projections surface it as optional (`?` / `NotRequired`
# / `Option`) and codecs map the zero value both ways.
annotation optional @0xc4d1e2f3a5b60718 (field) :Void;

# Bookclerk-as-IdP relying-party template. Plugins declare callback path and
# client id; the host materializes `oidc_clients` rows and remains the AS.
# `originConfigKey` is a dotted config path (e.g. integrations.audiobookshelf.base_url).
struct OidcClientTemplate {
  # OIDC client id the host materializes.
  clientId @0 :Text;
  # Name shown on consent / admin screens.
  displayName @1 :Text;
  # Redirect URI path relative to the integration origin.
  callbackPath @2 :Text;
  # When true, the client uses PKCE without a client secret.
  publicClient @3 :Bool;
  # Scopes granted by default.
  defaultScopes @4 :List(Text);
  # When true, the AS issues refresh tokens to this client.
  issueRefreshToken @5 :Bool;
  # Dotted config path holding the client's origin URL.
  originConfigKey @6 :Text;
}

# Success payload of `Oidc.clients`.
struct OidcClientsOk {
  # Client templates; empty when the plugin is not a relying party.
  clients @0 :List(OidcClientTemplate);
}

# Result union of `Oidc.clients`.
struct OidcClientsReply {
  union {
    # Success: OIDC client templates.
    ok @0 :OidcClientsOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Plugin-specific extensible config. Not a substitute for typed ABI fields.
struct ExtensibleConfig {
  # Version of `payload`'s schema, owned by the plugin.
  schemaVersion @0 :UInt32;
  # Media type of `payload` (e.g. `application/json`).
  mediaType @1 :Text;
  # Bounded encoded payload; at most `maxConfigPayloadBytes`.
  payload @2 :Data;
}

# Host-granted bindings for one `PluginWorker.open` (`env` in the Workers
# idiom). OS paths, FDs, and sockets are transport-private. Capability fields
# are null when the manifest does not declare (or the operator did not grant)
# the binding.
struct Bindings {
  # `CONFIG`: granted plugin settings (operator `[vars]` plus the family
  # settings table) as `application/json`; at most `maxConfigPayloadBytes`.
  config @0 :ExtensibleConfig;
  # `SECRETS`: granted secret values as `application/json`; empty payload
  # when the manifest declares no `[secrets]`.
  secrets @1 :ExtensibleConfig;
  # Database-adapter bootstrap for the `databaseAdapter` entrypoint. First-party
  # host-managed adapters receive host-private connect params in `config`;
  # third-party adapters receive this typed bootstrap (and an empty `config`).
  adapter @2 :DatabaseAdapterConfig;
  # `EVENTS`: outbox publisher; null unless `[[events.producers]]` is granted.
  events @3 :EventPublisher;
  # Named plugin-owned `[[databases]]` bindings: each entry is an isolated
  # database provisioned by the active adapter, separate from the Bookclerk
  # library and from every other plugin. Empty when the manifest declares none.
  databases @4 :List(NamedDatabase);
  # Host cancellation for the whole invocation (fence / lease loss).
  cancel @5 :Cancellation;
  # `WORK_FS`: host-granted object storage for durable plugin files (work
  # filesystem); null unless `[work_fs]` is granted. Job input/output travel on
  # `JobRunner.job(controller)`, never here.
  storage @6 :Destination;
}

# Exported entrypoints returned by `PluginWorker.open`. One capability per
# `plugin.toml` `entrypoints` entry / trigger; null when not exported. The host
# refuses an entrypoint the manifest or operator grant did not allow.
struct Entrypoints {
  # `[[events.consumers]]` trigger: `event(batch)` handler.
  eventConsumer @0 :EventConsumer;
  # `[triggers] jobs` trigger: `job(controller)` handler.
  jobRunner @1 :JobRunner;
  # `storefront` entrypoint.
  storefront @2 :ContentSource;
  # `storage` entrypoint.
  storage @3 :Destination;
  # `databaseAdapter` entrypoint.
  databaseAdapter @4 :Database;
  # `remoteLibrary` entrypoint.
  remoteLibrary @5 :RemoteLibrary;
  # `cli` entrypoint.
  cli @6 :PluginCli;
  # `oidc` entrypoint.
  oidc @7 :Oidc;
}

# Result union of `PluginWorker.open`.
struct EntrypointsReply {
  union {
    # Success: exported entrypoint capabilities.
    ok @0 :Entrypoints;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Durable command envelope (not a domain event). Command payload schema
# version and checkpoint schema versions are independent of the plugin ABI.
# The envelope itself is not persisted as opaque bytes across ABI majors.
# Idempotency keys are scoped to (account, plugin, commandType) until a
# terminal fenced outcome is committed.
struct JobInvocation {
  # Schema version of `payloadJson`, owned by the command type.
  payloadSchemaVersion @0 :UInt32;
  # Unique id of this invocation attempt.
  invocationId @1 :Text;
  # Command type the handler dispatches on.
  commandType @2 :Text;
  # Command payload (JSON); at most `maxScalarBytes`.
  payloadJson @3 :Text;
  # Caller idempotency key scoped to (account, plugin, commandType).
  idempotencyKey @4 :Text;
  # Failure retry counter, starting at 1.
  attempt @5 :UInt32;
  # Trace correlation id; empty when none.
  correlationId @6 :Text;
  # Id of the event or command that caused this one; empty when none.
  causationId @7 :Text;
  # UTC Unix milliseconds. Host fence/lease is authoritative; this hint must
  # not outlive the fence (clock skew across VPS nodes).
  deadlineUnixMs @8 :UInt64;
  # Checkpoint persisted by a prior `SuspendedOutcome`; empty on first run.
  checkpointJson @9 :Text;
  # Schema version of `checkpointJson`.
  checkpointSchemaVersion @10 :UInt32;
  # Resume ordinal; distinct from failure `attempt`.
  invocationSequence @11 :UInt32;
  # Optional step identifier for multi-step commands.
  stepId @12 :Text;
}

# Job finished successfully.
struct CompletedOutcome {
  # Short human summary.
  message @0 :Text;
  # Bytes produced, when meaningful.
  bytesCopied @1 :UInt64;
}

# Job failed transiently; the host reschedules it.
struct RetryableOutcome {
  # Short human reason.
  message @0 :Text;
  # Earliest retry time; `0` lets the host choose.
  retryAfterUnixMs @1 :UInt64;
}

# Job failed permanently; no retry.
struct RejectedOutcome {
  # Short human reason.
  message @0 :Text;
}

# Job observed cancellation and stopped.
struct CancelledOutcome {
  # Short human note.
  message @0 :Text;
}

# Job released the process and asks to be resumed later.
struct SuspendedOutcome {
  # Bounded checkpoint to replay on resume; at most `maxCheckpointBytes`.
  checkpointJson @0 :Text;
  # Schema version of `checkpointJson`.
  checkpointSchemaVersion @1 :UInt32;
  # Earliest resume time.
  wakeAtUnixMs @2 :UInt64;
}

# Terminal or suspended result of `JobRunner.job`.
struct JobOutcome {
  union {
    # Finished successfully.
    completed @0 :CompletedOutcome;
    # Transient failure; retry later.
    retryable @1 :RetryableOutcome;
    # Permanent failure.
    rejected @2 :RejectedOutcome;
    # Stopped on cancellation.
    cancelled @3 :CancelledOutcome;
    # Released with a checkpoint.
    suspended @4 :SuspendedOutcome;
  }
}

# Domain event (not a job). Outbox-produced, at-least-once, idempotent consume.
struct DomainEvent {
  # Unique event id (outbox row identity).
  eventId @0 :Text;
  # Dotted event type (e.g. `library.title.added`).
  eventType @1 :Text;
  # Schema version of `payload`, owned by the event type.
  schemaVersion @2 :UInt32;
  # When the producer observed the fact.
  occurredAtUnixMs @3 :UInt64;
  # Account scope; empty for host-wide events.
  accountId @4 :Text;
  # Trace correlation id; empty when none.
  correlationId @5 :Text;
  # Id of the command or event that caused this one; empty when none.
  causationId @6 :Text;
  # Consumer-side idempotency key; stable across redeliveries.
  deduplicationKey @7 :Text;
  # Delivery counter, starting at 1.
  deliveryAttempt @8 :UInt32;
  # Encoded event payload; at most `maxEventPayloadBytes`.
  payload @9 :Data;
  # Append-only. Resume a prior EventResult.suspended.
  checkpointJson @10 :Text;
  # Schema version of `checkpointJson`.
  checkpointSchemaVersion @11 :UInt32;
  # Resume ordinal; distinct from `deliveryAttempt`.
  invocationSequence @12 :UInt32;
  # True when this delivery resumes a prior suspension.
  resumePending @13 :Bool;
  # Append-only. Producer plugin id; empty when unknown.
  source @14 :Text;
}

# Event handled; the host marks it delivered.
struct EventAck {
  # Placeholder; the struct carries no data.
  dummy @0 :Void;
}

# Redeliver later.
struct EventRetry {
  # Earliest redelivery time; `0` lets the host choose.
  retryAtUnixMs @0 :UInt64;
  # Short human reason.
  reason @1 :Text;
}

# Event rejected; the host records the reason and stops delivering.
struct EventReject {
  # Short human reason.
  reason @0 :Text;
}

# Event moved to the dead-letter queue for operator review.
struct EventDeadLetter {
  # Short human reason.
  reason @0 :Text;
}

# Append-only. Mirrors job SuspendedOutcome; event handlers persist a
# bounded checkpoint and release the process until wakeAtUnixMs. Optional
# wake-on-matching-event fields (empty = timestamp-only).
struct EventSuspended {
  # Bounded checkpoint to replay on resume; at most `maxCheckpointBytes`.
  checkpointJson @0 :Text;
  # Schema version of `checkpointJson`.
  checkpointSchemaVersion @1 :UInt32;
  # Earliest resume time.
  wakeAtUnixMs @2 :UInt64;
  # Also wake when an event of this type arrives; empty disables.
  wakeOnEventType @3 :Text;
  # JSON filter applied to matching wake events; empty matches all.
  wakeOnFilterJson @4 :Text;
}

# Per-event outcome inside an `EventConsumer.event` batch reply.
struct EventResult {
  union {
    # Handled.
    ack @0 :EventAck;
    # Redeliver later.
    retry @1 :EventRetry;
    # Stop delivering.
    reject @2 :EventReject;
    # Park for operator review.
    deadLetter @3 :EventDeadLetter;
    # Released with a checkpoint.
    suspended @4 :EventSuspended;
  }
}

# One `EventConsumer.event` delivery: at most `maxListPage` events, each
# bounded by `maxEventPayloadBytes` / `maxCheckpointBytes`.
struct EventBatch {
  # Events in delivery order; the reply carries one `EventResult` per entry.
  events @0 :List(DomainEvent);
}

# Result union of `EventConsumer.event`. `ok` has exactly one entry per
# `EventBatch.events` entry, in order; a short list is a host-side error and
# the missing tail is retried.
struct EventBatchReply {
  union {
    # Success: per-event outcomes.
    ok @0 :List(EventResult);
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Everything one `JobRunner.job` invocation may touch. Capabilities are
# host-served; the guest never sees OS paths or sockets.
struct JobController {
  # Durable command envelope.
  invocation @0 :JobInvocation;
  # Job input objects.
  input @1 :Source;
  # Job output object store.
  output @2 :Destination;
  # Progress reporter (host coalesces frequent updates).
  progress @3 :ProgressSink;
  # Host cancellation probe for this job (fence / lease).
  cancel @4 :Cancellation;
}

# Success payload of `Destination.head`.
struct HeadOk {
  # Whether the object exists.
  found @0 :Bool;
  # Object metadata; meaningful only when `found`.
  meta @1 :ObjectMetadata;
}

# Result union of `Destination.head`.
struct HeadReply {
  union {
    # Success: head probe outcome.
    ok @0 :HeadOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.list`.
struct ListReply {
  union {
    # Success: one list page.
    ok @0 :ListPage;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `Destination.get`.
struct GetOk {
  # Object metadata.
  meta @0 :ObjectMetadata;
  # Streamed object bytes.
  body @1 :ByteSource;
}

# Result union of `Destination.get`.
struct GetReply {
  union {
    # Success: object metadata and body stream.
    ok @0 :GetOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.put` / `Destination.commit`.
struct PutReply {
  union {
    # Success: stored object summary.
    ok @0 :PutResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.copy`.
struct CopyReply {
  union {
    # Success: copy summary.
    ok @0 :CopyResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `methods without a success payload`.
struct EmptyReply {
  union {
    # Success: no payload.
    ok @0 :Void;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `ByteSource.pull`.
struct PullOk {
  # Next bytes; may be shorter than requested.
  chunk @0 :Data;
  # True when the stream is exhausted after `chunk`.
  done @1 :Bool;
}

# Result union of `ByteSource.pull`.
struct PullReply {
  union {
    # Success: one stream window.
    ok @0 :PullOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `Source.open`.
struct OpenOk {
  # Object metadata.
  meta @0 :ObjectMetadata;
  # Streamed object bytes.
  body @1 :ByteSource;
}

# Result union of `Source.open`.
struct OpenReply {
  union {
    # Success: object metadata and body stream.
    ok @0 :OpenOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `PluginWorker.describe`.
struct DescribeReply {
  union {
    # Success: plugin identity.
    ok @0 :PluginDescribe;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `JobRunner.job`.
struct HandleReply {
  union {
    # Success: job outcome.
    ok @0 :JobOutcome;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Typed liveness report.
struct HealthOk {
  # True when the guest is healthy enough for traffic.
  ok @0 :Bool;
  # Short human status line; empty when none.
  detail @1 :Text;
}

# Result union of `health`.
struct HealthReply {
  union {
    # Success: health status.
    ok @0 :HealthOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Database.openSession`.
struct AdapterSessionReply {
  union {
    # Success: opened `AdapterDatabaseSession` capability.
    ok @0 :AdapterDatabaseSession;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `guest database opens`.
struct GuestDatabaseReply {
  union {
    # Success: opened `GuestDatabase` capability.
    ok @0 :GuestDatabase;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Transferred readable byte stream. The capability *is* the stream; callers
# pull bounded windows. Abort is capability drop / RPC cancel. A failed pull
# MUST set `err` — never a successful empty EOF.
interface ByteSource {
  # Pull the next window of bytes. `done = true` on the final chunk.
  pull @0 (
      maxBytes :UInt32  # Upper bound for this window; at most `maxStreamWindowBytes`.
  ) -> (result :PullReply);
}

# Object store the host writes acquired media into (`output.*` plugins).
# Keys are relative object paths; the plugin owns the physical layout.
interface Destination {
  # Metadata probe. `found = false` is a success, not `not_found`.
  head @0 (
      key :Text  # Object key to probe.
  ) -> (result :HeadReply);
  # Page through objects under a prefix; at most `maxListPage` per page.
  list @1 (
      options :ListOptions  # Prefix, cursor, and page size.
  ) -> (result :ListReply);
  # Open an object (or a byte range of it) for reading.
  get @2 (
      key :Text,  # Object key to read.
      options :ReadOptions  # Optional byte range.
  ) -> (result :GetReply);
  # Store an object from a transferred byte stream.
  put @3 (
      key :Text,  # Object key to write.
      body :ByteSource,  # Streamed object bytes.
      options :WriteOptions  # Content type, length, digest, staging.
  ) -> (result :PutReply);
  # Server-side copy (requires `storage.copy`).
  copy @4 (
      from :Text,  # Source object key.
      to :Text  # Destination object key.
  ) -> (result :CopyReply);
  # Remove an object; deleting a missing key is a success.
  delete @5 (
      key :Text  # Object key to remove.
  ) -> (result :EmptyReply);
  # Finalize a destination-side staged object. Staging itself is `put` with
  # `stageOnly = true`; bytes must stream into destination-managed temp/multipart
  # storage, never a complete local spool on host/adapter/broker/guest.
  commit @6 (
      key :Text,  # Object key that was staged.
      commitToken :Text  # Token from `WriteOptions.commitToken`.
  ) -> (result :PutReply);
  # Discard a staged object without publishing it.
  abortStage @7 (
      key :Text,  # Object key that was staged.
      commitToken :Text  # Token from `WriteOptions.commitToken`.
  ) -> (result :EmptyReply);
}

# Read-only byte source for job inputs (not a storefront).
interface Source {
  # Open an object for streaming reads.
  open @0 (
      key :Text  # Object key to open.
  ) -> (result :OpenReply);
}

# Host-side progress reporter handed to job handlers.
interface ProgressSink {
  # Report progress; the host coalesces frequent updates.
  report @0 (
      percent :Float32,  # Completion in `[0, 100]`.
      message :Text  # Short human-readable status line.
  ) -> (result :EmptyReply);
}

# Transport cancellation. SDKs project this into a locally created AbortSignal
# (AbortSignal is not a serializable Workers RPC value).
interface Cancellation {
  # Non-blocking check; `true` once the host has fenced the invocation.
  poll @0 () -> (cancelled :Bool);
}

# `[triggers] jobs` handler (`Entrypoints.jobRunner`); runs one durable command.
interface JobRunner {
  # Run one command invocation to a terminal or suspended outcome. Named
  # database bindings come from `Bindings.databases` at `open`, not per job.
  job @0 (
      controller :JobController  # Envelope plus host-served capabilities.
  ) -> (result :HandleReply);
}

# `[[events.consumers]]` handler (`Entrypoints.eventConsumer`). Delivery is
# at-least-once; consumers must be idempotent on `deduplicationKey`.
interface EventConsumer {
  # Deliver one ordered batch of domain events.
  event @0 (
      batch :EventBatch  # Events in delivery order.
  ) -> (result :EventBatchReply);
}

# `EVENTS` binding: host-served outbox publisher granted through
# `Bindings.events`. Event types must be listed in `[[events.producers]]`.
interface EventPublisher {
  # Append one event to the outbox (idempotent on `deduplicationKey`).
  publish @0 (
      event :PluginEvent  # Event to publish.
  ) -> (result :PublishReply);
}

# One named plugin-owned database binding delivered on `Bindings.databases`.
struct NamedDatabase {
  # Binding name from `plugin.toml` `[[databases]]`.
  name @0 :Text;
  # Isolated typed SQL session for this binding (plugin-owned schema).
  database @1 :GuestDatabase;
}

# Storefront content source (not byte Source). Every method takes and
# returns typed structs from the "Typed method payloads" section.
interface ContentSource {
  # Connect an account (password or one-shot OAuth).
  login @0 (
      params :LoginParams  # Credentials, marketplace, callback wiring.
  ) -> (result :LoginReply);
  # Sync library rows for one or more accounts.
  scan @1 (
      params :ScanParams  # Accounts, paging, and host-sealed credentials.
  ) -> (result :ScanReply);
  # Download and decrypt one title into `cacheDir`.
  fetchTitle @2 (
      params :FetchTitleParams  # Title, credentials, and fetch options.
  ) -> (result :FetchTitleReply);
  # Enumerate accounts the guest knows about.
  listAccounts @3 () -> (result :SourceAccountsReply);
  # Begin an interactive OAuth login; returns a session id.
  loginStart @4 (
      params :LoginParams  # Same shape as `login`; host fills callback IPC.
  ) -> (result :LoginStartReply);
  # Finish an interactive OAuth login started by `loginStart`.
  loginComplete @5 (
      params :LoginCompleteParams  # Session id from `loginStart`.
  ) -> (result :LoginReply);
  # Free-text storefront catalog search.
  searchCatalog @6 (
      params :SearchCatalogParams  # Query, region, paging, sort, facet.
  ) -> (result :CatalogHitsReply);
  # Related-title expansion from a seed title.
  expandCandidates @7 (
      params :ExpandCandidatesParams  # Seed identity fields and limit.
  ) -> (result :CatalogHitsReply);
  # Purchase link / price hint for one title.
  purchaseHint @8 (
      params :PurchaseHintParams  # Identity fields and price flag.
  ) -> (result :PurchaseHintReply);
  # Current storefront deals.
  listDeals @9 (
      params :ListDealsParams  # Optional result cap.
  ) -> (result :CatalogHitsReply);
  # Liveness / readiness probe.
  health @10 () -> (result :HealthReply);
  # Human-readable diagnostic lines.
  diagnose @11 () -> (result :DiagnoseReply);
  # Full catalog record for one product.
  catalogDetail @12 (
      params :CatalogDetailParams  # Product id and optional ISBN.
  ) -> (result :CatalogDetailReply);
}

# `remoteLibrary` entrypoint: long-running remote-library lifecycle (start /
# stop, library rescan, listening sync, external-user polling). Event delivery
# is `EventConsumer`; credential verification is `Oidc`.
interface RemoteLibrary {
  # Liveness / readiness probe.
  health @0 () -> (result :HealthReply);
  # Start background work after the host has granted bindings.
  start @1 () -> (result :EmptyReply);
  # Stop background work; the host may drop the capability afterwards.
  stop @2 () -> (result :EmptyReply);
  # Human-readable diagnostic lines.
  diagnose @3 () -> (result :DiagnoseReply);
  # Re-sync the remote library.
  scanLibrary @4 (
      params :ScanLibraryParams  # Full-rescan flag.
  ) -> (result :EmptyReply);
  # Push / pull listening progress.
  syncListening @5 () -> (result :SyncListeningReply);
  # Drain external users the remote side observed since the last poll.
  pollEvents @6 () -> (result :EventPollReply);
}

# `cli` entrypoint: guest commands under `bookclerk plugins <id> <command>`.
interface PluginCli {
  # Declared CLI surface.
  describe @0 () -> (result :CliSchemaReply);
  # Run one plugin CLI command.
  invoke @1 (
      params :CliInvokeParams  # Command name and argument values.
  ) -> (result :CliInvokeReply);
}

# `oidc` entrypoint: relying-party client templates and credential
# verification on behalf of the host authorization server.
interface Oidc {
  # Plugin-provided OIDC AS client templates. Empty list when unused.
  clients @0 () -> (result :OidcClientsReply);
  # Verify remote credentials on behalf of the host.
  authenticateUser @1 (
      params :AuthenticateUserParams  # Username and password.
  ) -> (result :ExternalUserReply);
}

# Marks an enum whose wire strings (where an enum travels as `Text`, e.g.
# `PluginError.code`) are the snake_case form of the enumerant names
# (`payloadTooLarge` -> "payload_too_large").
annotation jsonEnum @0xba425910028861ab (enum) :Void;

# Stable `PluginError.code` strings. Unknown future codes are forwarded
# as-is; SDKs surface them as a local `unknown` while keeping the raw wire
# code.
enum PluginErrorCode $jsonEnum {
  # Request params failed validation or are missing required fields.
  invalidParams @0;
  # Caller is not authenticated for this method (credentials / token).
  unauthorized @1;
  # Caller is authenticated but not allowed to perform this operation.
  forbidden @2;
  # Requested account, object key, session, or row does not exist.
  notFound @3;
  # Backend or dependency is temporarily unreachable (store API, DB, ...).
  unavailable @4;
  # Method or capability is not implemented by this guest.
  unsupported @5;
  # Unexpected guest or host failure; see `PluginError.message`.
  internal @6;
  # A scalar RPC value exceeded `maxScalarBytes`.
  payloadTooLarge @7;
  # The invocation deadline elapsed before the call completed.
  deadlineExceeded @8;
  # List cursor is missing, stale, or not from this backend.
  invalidCursor @9;
  # The invocation was cancelled (host fence / guest abort).
  cancelled @10;
  # The operation conflicts with current state (conditional put, ...).
  conflict @11;
}

#############################################################################
# Typed method payloads
#
# Real Cap'n Proto wire structs for `describe()` identity extras, every
# `ContentSource` / `RemoteLibrary` / `Oidc` method, and the plugin CLI. Rust DTOs and
# codecs for this section are generated into `src/generated.rs`; the
# TypeScript and Python SDK types and codecs come from the same emitter.
# Scalars marked `$optional` use the zero value as "absent". Credentials are
# opaque `Data` the host seals into `encrypted_secrets`; plugin-specific
# knobs travel as `ExtensibleConfig` (`application/json`).
#############################################################################

# --- rust-generated: begin ---

# Named entrypoint a plugin exports (Cloudflare Workers named-entrypoint
# analogue). Each value is a capability the host calls over RPC; triggers on
# the default entrypoint (`event`, `job`) are declared separately.
enum Entrypoint {
  # Storefront: login, scan, fetch, catalog search (`ContentSource`).
  storefront @0;
  # Object storage destination (`Destination`).
  storage @1;
  # Library database adapter (`Database`).
  databaseAdapter @2;
  # Remote-library lifecycle: start/stop, scanLibrary, syncListening,
  # pollEvents (`RemoteLibrary`).
  remoteLibrary @3;
  # Guest CLI (`PluginCli.describe` / `PluginCli.invoke`).
  cli @4;
  # OIDC bridge: relying-party client templates and `authenticateUser`.
  oidc @5;
}

# One declared event consumer: a `[[events.consumers]]` row the default
# entrypoint's `event(batch)` handler accepts.
struct EventConsumerSpec {
  # Versioned event type (snake_case, e.g. `book_acquired`).
  eventType @0 :Text;
  # Schema versions the guest can consume; never empty.
  schemaVersions @1 :List(UInt32);
  # Whether `EventResult.suspended` is supported for this type.
  supportsSuspend @2 :Bool;
}

# Typed capability declaration returned by `describe()`. The host compares
# it with `plugin.toml` and the operator grant; widening is rejected at spawn.
struct PluginCapabilities {
  # Named entrypoints the guest exports.
  entrypoints @0 :List(Entrypoint);
  # Event types the default entrypoint consumes (`event(batch)` trigger).
  consumes @1 :List(EventConsumerSpec);
  # Event types the guest may publish through its `EVENTS` binding.
  produces @2 :List(Text);
  # Command types the default entrypoint runs (`job(controller)` trigger).
  jobs @3 :List(Text);
  # Plugin-owned database binding names (`[[databases]]`).
  databases @4 :List(Text);
  # Other named bindings the guest expects on `env` (`CONFIG`, `SECRETS`,
  # `WORK_FS`, `OAUTH`, `KV`, `EVENTS`, ...).
  bindings @5 :List(Text);
}

# Portal Accounts connect mode for storefronts.
enum PortalAuthMode {
  # Guest did not declare a mode; the host assumes password login.
  unspecified @0;
  # Email / password login through the Accounts UI.
  password @1;
  # Browser OAuth through `loginStart` / `loginComplete`.
  oauth @2;
}

# Portal brand crossing the RPC boundary. Distinct from `plugin.toml`
# `logo`: `iconUrl` is the live URL or data URI the SPA renders.
struct Brand {
  # Brand id (often matches the plugin id); empty means "no brand".
  id @0 :Text;
  # Display name shown next to the brand swatch.
  name @1 :Text;
  # Background CSS color (hex or named).
  bg @2 :Text;
  # Foreground CSS color for text on `bg`.
  fg @3 :Text;
  # Accent CSS color for highlights / CTAs.
  accent @4 :Text;
  # Icon URL or data URI for the portal.
  iconUrl @5 :Text $optional;
}

# One discoverable config option group advertised for sources.
struct ConfigOption {
  # Config key under the plugin's `config.toml` table.
  key @0 :Text;
  # Operator-facing label for the option group.
  label @1 :Text;
  # Allowed selectable values for this key.
  values @2 :List(ConfigOptionValue);
}

# One selectable value under a `ConfigOption`.
struct ConfigOptionValue {
  # Value written to config when selected.
  id @0 :Text;
  # Operator-facing label for this value.
  label @1 :Text;
}

# Declared plugin CLI surface (`PluginCli.describe` / `describe().cli`).
struct CliSchema {
  # Commands exposed as `bookclerk plugins <id> <command> ...`.
  commands @0 :List(CliCommandSpec);
}

# One plugin CLI command under `CliSchema`.
struct CliCommandSpec {
  # Command verb after the plugin id (for example "ping").
  name @0 :Text;
  # Short help text for `--help`.
  about @1 :Text $optional;
  # Argument / flag specs for this command.
  args @2 :List(CliArgSpec);
}

# Value kind for a `CliArgSpec`.
enum CliArgKind {
  # Free-form string argument (default).
  string @0;
  # Boolean flag ("true" / "false").
  bool @1;
  # Integer argument.
  int @2;
  # Filesystem path argument.
  path @3;
}

# One CLI argument or flag under a `CliCommandSpec`.
struct CliArgSpec {
  # Internal arg name used as `CliArg.name` on invoke.
  name @0 :Text;
  # Long flag without leading dashes (e.g. "message" -> `--message`).
  long @1 :Text $optional;
  # Short flag character (e.g. "m" -> `-m`).
  short @2 :Text $optional;
  # Parsed value kind.
  kind @3 :CliArgKind;
  # When true, the host rejects invoke if the arg is missing.
  required @4 :Bool;
  # Default string form when the operator omits the arg.
  default @5 :Text $optional;
  # Help text for this arg.
  about @6 :Text $optional;
  # When true, the arg is positional rather than a flagged option.
  positional @7 :Bool;
}

# One named argument value passed to `PluginCli.invoke`.
struct CliArg {
  # Arg name matching a `CliArgSpec.name`.
  name @0 :Text;
  # String form of the value (the guest parses per `CliArgSpec.kind`).
  value @1 :Text;
}

# Params of `PluginCli.invoke`.
struct CliInvokeParams {
  # Command name matching a `CliCommandSpec.name`.
  command @0 :Text;
  # Named argument values.
  args @1 :List(CliArg);
}

# Result of `PluginCli.invoke`.
struct CliInvokeResult {
  # Process-style exit code (0 = success).
  exitCode @0 :Int32;
  # Captured standard output text.
  stdout @1 :Text;
  # Captured standard error text.
  stderr @2 :Text;
  # Structured payload for machine consumers; empty `mediaType` when absent.
  payload @3 :ExtensibleConfig;
}

# Result union of `PluginCli.describe`.
struct CliSchemaReply {
  union {
    # Success: declared CLI surface.
    ok @0 :CliSchema;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `PluginCli.invoke`.
struct CliInvokeReply {
  union {
    # Success: command output.
    ok @0 :CliInvokeResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Author-facing database adapter configuration carried in
# `DatabaseContext.adapter`. This is the generic bootstrap mechanism for
# third-party adapters: the operator's granted `[database.<id>]` table plus
# the scoped writable data dir. First-party host-managed adapters receive
# host-private connect params in `DatabaseContext.config` instead.
struct DatabaseAdapterConfig {
  # Scoped writable directory for this plugin (`.../plugins/<id>/data`).
  pluginDataDir @0 :Text;
  # Granted plugin settings (operator `[database.<id>]` table) as
  # `application/json`; `{}` when the operator configured nothing.
  settings @1 :ExtensibleConfig;
  # Named plugin database binding this open serves; empty for the primary
  # library open. Adapters advertising `DbCapabilities.pluginDatabases` must
  # serve each binding from its own isolated database.
  binding @2 :Text $optional;
  # Host-issued opaque instance id for this (owner plugin, binding) pair.
  # Collision-resistant and stable across re-opens. Empty for the primary
  # library open. Third-party adapters must key isolated databases on this
  # value rather than `binding` alone (two plugins may both declare `DB`).
  instanceId @3 :Text $optional;
  # When true, open an existing binding unit and do not provision a missing
  # one (read-only backup capture). False lets the adapter create the unit.
  openExisting @4 :Bool;
}

# Operator-facing diagnostic lines printed by `bookclerk plugins diagnose`.
struct DiagnoseResult {
  # Human-readable probe lines.
  lines @0 :List(Text);
}

# Result union of `diagnose`.
struct DiagnoseReply {
  union {
    # Success: diagnostic lines.
    ok @0 :DiagnoseResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Source account metadata returned from login and stored by the host.
struct SourceAccount {
  # Stable account id within this source plugin.
  accountId @0 :Text;
  # Source plugin id (the host forces this to the guest's install id).
  source @1 :Text;
  # Storefront marketplace / region code (for example `us`, `uk`).
  marketplace @2 :Text;
  # Operator-facing label.
  label @3 :Text $optional;
  # When true, bare / scheduled scans include this account. Explicit CLI
  # `--account` bypasses this flag.
  scanEnabled @4 :Bool;
}

# Params of `ContentSource.login` and `ContentSource.loginStart`. Password
# sources fill email/password; OAuth sources use callback / external fields.
# There is no files-dir root or library DB path -- only `pluginDataDir`.
struct LoginParams {
  # Scoped writable directory for this plugin only (`.../plugins/<id>/data`).
  pluginDataDir @0 :Text;
  # Marketplace / locale for the storefront; empty means the guest default.
  marketplace @1 :Text;
  # Operator label stored on the account row.
  label @2 :Text $optional;
  # Account email / username for password logins; empty for pure OAuth.
  email @3 :Text $optional;
  # Account password for password logins; never logged; empty for OAuth.
  password @4 :Text $optional;
  # When true, overwrite an existing sealed credential for this account.
  force @5 :Bool;
  # Bind address for OAuth callback servers (`host:port`). Ignored when
  # `callbackIpc` is set (host owns the TCP listener).
  callbackBind @6 :Text $optional;
  # Host-owned callback IPC endpoint the guest must connect to. When set
  # (with `callbackPublicBase`), the guest must not bind a TCP listener.
  callbackIpc @7 :Text $optional;
  # Public base URL for the host TCP listener, e.g. `http://127.0.0.1:12345`.
  callbackPublicBase @8 :Text $optional;
  # When true, use external / paste-redirect OAuth instead of a local
  # callback server.
  external @9 :Bool;
  # Pre-supplied OAuth redirect URL (paste flow).
  responseUrl @10 :Text $optional;
  # Prefer QR output when the guest supports it.
  showQr @11 :Bool;
  # Seconds to wait for OAuth callback capture; guest default when 0.
  timeoutSecs @12 :UInt64 $optional;
  # Store-specific knobs as `application/json`; guests may ignore unknowns.
  extra @13 :ExtensibleConfig;
}

# Result of `ContentSource.login` / `loginComplete`: account metadata plus
# opaque credentials for the host to seal into `encrypted_secrets`
# (`provider = plugin id`). Guests never write secrets into the library DB.
struct LoginResult {
  # Account row fields for the host to upsert.
  account @0 :SourceAccount;
  # Opaque credential blob the host seals; empty when login only refreshed
  # metadata. Guests choose the encoding (typically JSON bytes).
  credentials @1 :Data $optional;
}

# Result of `ContentSource.loginStart` (interactive OAuth). The operator
# opens `url`; `loginComplete` later uses `sessionId`.
struct LoginStartResult {
  # Opaque session id for `loginComplete`.
  sessionId @0 :Text;
  # Browser URL the operator should open to complete OAuth.
  url @1 :Text;
}

# Params of `ContentSource.loginComplete`.
struct LoginCompleteParams {
  # Session id previously returned by `loginStart`.
  sessionId @0 :Text;
}

# Result union of `ContentSource.login` / `loginComplete`.
struct LoginReply {
  union {
    # Success: account plus credentials to seal.
    ok @0 :LoginResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `ContentSource.loginStart`.
struct LoginStartReply {
  union {
    # Success: session id and browser URL.
    ok @0 :LoginStartResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Host-sealed credentials for one account, delivered on `scan`.
struct AccountCredential {
  # Account id the blob belongs to.
  accountId @0 :Text;
  # Opaque credential bytes exactly as the guest returned them at login.
  credentials @1 :Data;
}

# Params of `ContentSource.scan`. The host injects sealed credentials so the
# plugin does not need a private credential store under `pluginDataDir`.
struct ScanParams {
  # Scoped plugin data directory.
  pluginDataDir @0 :Text;
  # Account ids to scan; empty means all scan-enabled accounts.
  accounts @1 :List(Text);
  # Storefront page size; the host always sends an explicit value.
  pageSize @2 :UInt32;
  # When true, import podcast/episode-style rows.
  importEpisodes @3 :Bool;
  # When true, import Plus/catalog entitlement titles.
  importPlusTitles @4 :Bool;
  # Host-loaded credential blobs for the requested accounts.
  credentials @5 :List(AccountCredential);
}

# One library title returned by `ContentSource.scan`. The host upserts these
# rows and forces `source` to the plugin id.
struct ScanBook {
  # Account that owns this library entry.
  accountId @0 :Text;
  # Storefront product / SKU id.
  productId @1 :Text;
  # Primary title string.
  title @2 :Text;
  # Marketplace / region when known.
  marketplace @3 :Text $optional;
  # Amazon ASIN when the storefront exposes one.
  asin @4 :Text $optional;
  # ISBN when the storefront exposes one.
  isbn @5 :Text $optional;
  # Comma- or guest-formatted author list.
  authors @6 :Text $optional;
  # Comma- or guest-formatted narrator list.
  narrators @7 :Text $optional;
  # Series name when applicable.
  series @8 :Text $optional;
  # Series index / sequence label.
  seriesIndex @9 :Text $optional;
  # Content classification (e.g. `book` vs `episode`).
  contentKind @10 :Text $optional;
  # Publisher name when known.
  publisher @11 :Text $optional;
  # Runtime in whole minutes when known.
  lengthMinutes @12 :Int64 $optional;
  # Subtitle when distinct from `title`.
  subtitle @13 :Text $optional;
}

# Summary result of `ContentSource.scan`.
struct ScanSummary {
  # Number of accounts touched during the scan.
  accounts @0 :UInt32;
  # Count of titles the guest expects the host to upsert; may mirror
  # `books.length`.
  booksUpserted @1 :UInt32;
  # Number of storefront pages fetched.
  pages @2 :UInt32;
  # Accounts skipped because `scanEnabled` was false.
  skippedDisabled @3 :UInt32;
  # Titles for the host to upsert. Prefer this over plugin-side DB writes.
  books @4 :List(ScanBook);
}

# Result union of `ContentSource.scan`.
struct ScanReply {
  union {
    # Success: scan summary and titles to upsert.
    ok @0 :ScanSummary;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Fetch-relevant acquire knobs the host forwards so external load matches
# in-process. Packaging / naming knobs stay host-side.
struct FetchOptions {
  # Prefer Widevine/CENC download when the store offers it.
  widevine @0 :Bool;
  # Prefer xHE-AAC on the Widevine path when offered.
  xheAac @1 :Bool;
  # Local Widevine `.wvd` path granted to the guest.
  widevineCdmPath @2 :Text $optional;
  # Remote L3 CDM provider URL; empty means the classic default, `off`
  # disables remote provisioning.
  widevineCdmProvider @3 :Text $optional;
  # When true, download a cover image alongside audio.
  downloadCover @4 :Bool;
  # When true, download a companion PDF when the store exposes one.
  downloadPdf @5 :Bool;
  # Cover image size request (`500`, `1215`, or `native`).
  coverSize @6 :Text;
  # Preferred chapter API layout when fetching (`tree` or `flat`).
  chapterLayout @7 :Text;
  # When true, trim Audible brand intro/outro from the remux window.
  stripAudibleBrandAudio @8 :Bool;
  # When true, download clips/bookmarks sidecars when offered.
  downloadClipsBookmarks @9 :Bool;
  # When true, keep the encrypted download in storage.
  retainAaxFile @10 :Bool;
  # Fetch speed cap in KB/s (`0` = unlimited).
  downloadSpeedLimitKbps @11 :UInt32;
  # When true, persist raw catalog API JSON as `metadata.json`.
  saveMetadataJson @12 :Bool;
}

# Params of `ContentSource.fetchTitle`. The plugin writes media under
# `cacheDir` and returns plain (DRM-free) paths. The host injects
# credentials; guests must not open `library.db` or `master.key`.
struct FetchTitleParams {
  # Scoped plugin data directory.
  pluginDataDir @0 :Text;
  # Account whose credentials apply.
  accountId @1 :Text;
  # Library / storefront title id to download.
  titleId @2 :Text;
  # Absolute path the guest should write media into (jail-granted TMPDIR).
  cacheDir @3 :Text;
  # Host-loaded credential blob for this account; empty when unavailable.
  credentials @4 :Data $optional;
  # Granted `[sources.<id>]` table as `application/json`.
  sourceConfig @5 :ExtensibleConfig;
  # Fetch-relevant acquire options.
  fetch @6 :FetchOptions;
}

# One plain audio part written under the cache directory.
struct PlainPart {
  # Absolute path to the part file under `cacheDir`.
  path @0 :Text;
  # Part title (disc / chapter label).
  title @1 :Text $optional;
  # Duration of this part in milliseconds when known.
  durationMs @2 :UInt64 $optional;
}

# One chapter marker of a fetched title.
struct ChapterMarker {
  # Chapter title.
  title @0 :Text;
  # Chapter start offset in milliseconds from the beginning of the title.
  startMs @1 :UInt64;
}

# Plain (DRM-free) fetch result. Sources always return decrypted media; DRM
# guests decrypt before responding.
struct PlainFetch {
  # Ordered audio part files written under the cache directory.
  parts @0 :List(PlainPart);
  # Single M4B path when the guest assembled one.
  m4bPath @1 :Text $optional;
  # Cover image path under the cache directory.
  coverPath @2 :Text $optional;
  # Chapter markers; empty when unknown.
  chapters @3 :List(ChapterMarker);
  # Companion PDF download URL when the store exposes one.
  pdfUrl @4 :Text $optional;
}

# Result union of `ContentSource.fetchTitle`.
struct FetchTitleReply {
  union {
    # Success: plain media paths.
    ok @0 :PlainFetch;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `ContentSource.listAccounts`.
struct SourceAccounts {
  # Accounts the guest knows about.
  accounts @0 :List(SourceAccount);
}

# Result union of `ContentSource.listAccounts`.
struct SourceAccountsReply {
  union {
    # Success: account list.
    ok @0 :SourceAccounts;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Catalog search ordering.
enum CatalogSort {
  # Storefront relevance ranking (default).
  relevance @0;
  # Most popular first.
  popularity @1;
  # Highest rated first.
  rating @2;
  # Alphabetical by title.
  title @3;
  # Alphabetical by author.
  author @4;
}

# Catalog search facet restricting which field the query matches.
enum CatalogField {
  # Match any field.
  any @0;
  # Match author names.
  author @1;
  # Match narrator names.
  narrator @2;
  # Match series names.
  series @3;
  # Match genre / category labels.
  genre @4;
}

# Params of `ContentSource.searchCatalog`.
struct SearchCatalogParams {
  # Free-text search query.
  query @0 :Text;
  # Storefront region / marketplace code; empty means the guest default.
  region @1 :Text;
  # Maximum hits to return; the host always sends an explicit value.
  limit @2 :UInt32;
  # 1-based page for storefronts that page.
  page @3 :UInt32;
  # Sort order.
  sort @4 :CatalogSort;
  # Facet restriction.
  field @5 :CatalogField;
  # Preferred content language (soft-prioritize; e.g. `en`).
  language @6 :Text $optional;
}

# Params of `ContentSource.expandCandidates`. Seed fields identify a known
# title; the guest returns related catalog hits.
struct ExpandCandidatesParams {
  # Source plugin id hint when expanding across storefronts.
  source @0 :Text;
  # Seed storefront product id.
  productId @1 :Text;
  # Seed title text.
  title @2 :Text;
  # Seed authors string.
  authors @3 :Text $optional;
  # Seed narrators string.
  narrators @4 :Text $optional;
  # Seed series name.
  series @5 :Text $optional;
  # Seed series ASIN when known.
  seriesAsin @6 :Text $optional;
  # Seed Amazon ASIN.
  asin @7 :Text $optional;
  # Seed ISBN.
  isbn @8 :Text $optional;
  # Storefront region / marketplace code.
  region @9 :Text;
  # Maximum candidates to return; the host always sends an explicit value.
  limit @10 :UInt32;
}

# Params of `ContentSource.purchaseHint`. At least one identity field
# (`productId` / `asin` / `isbn` / title+authors) should be set; guests may
# return `invalid_params` when none are usable.
struct PurchaseHintParams {
  # Storefront product id when known.
  productId @0 :Text $optional;
  # Title text for fuzzy lookup.
  title @1 :Text $optional;
  # Authors string for fuzzy lookup.
  authors @2 :Text $optional;
  # Amazon ASIN when known.
  asin @3 :Text $optional;
  # ISBN when known.
  isbn @4 :Text $optional;
  # Storefront region / marketplace code.
  region @5 :Text;
  # When true, guests should include live price fields when available.
  withPrice @6 :Bool;
}

# Params of `ContentSource.listDeals`.
struct ListDealsParams {
  # Maximum number of deals to return; guest default when 0.
  limit @0 :UInt32 $optional;
}

# Params of `ContentSource.catalogDetail`.
struct CatalogDetailParams {
  # Store product id (Libro ISBN or ISBN-slug).
  productId @0 :Text;
  # ISBN when it differs from `productId`.
  isbn @1 :Text $optional;
}

# Whether an edition is abridged.
enum Abridgement {
  # The storefront did not say.
  unknown @0;
  # Unabridged edition.
  unabridged @1;
  # Abridged edition.
  abridged @2;
}

# One catalog / candidate hit returned by `searchCatalog`,
# `expandCandidates`, `listDeals`, and `catalogDetail`.
struct CatalogHit {
  # Storefront product / SKU id.
  productId @0 :Text;
  # Primary title.
  title @1 :Text;
  # Authors string when known.
  authors @2 :Text $optional;
  # Narrators string when known.
  narrators @3 :Text $optional;
  # Series name when applicable.
  series @4 :Text $optional;
  # Series index / sequence label.
  seriesIndex @5 :Text $optional;
  # Amazon ASIN when known.
  asin @6 :Text $optional;
  # ISBN when known.
  isbn @7 :Text $optional;
  # Storefront product page URL.
  url @8 :Text $optional;
  # Cover image URL.
  coverUrl @9 :Text $optional;
  # Hit origin label (plugin id or storefront name).
  origin @10 :Text;
  # Subtitle when distinct from `title`.
  subtitle @11 :Text $optional;
  # Long description / blurb when fetched.
  description @12 :Text $optional;
  # Publisher name when known.
  publisher @13 :Text $optional;
  # Runtime in whole minutes.
  lengthMinutes @14 :Int64 $optional;
  # Publication date string as provided by the storefront.
  publishedAt @15 :Text $optional;
  # Category / genre labels as a single string when known.
  categories @16 :Text $optional;
  # Content language code when known.
  language @17 :Text $optional;
  # Current price in minor units (cents).
  priceCents @18 :Int64 $optional;
  # ISO currency code for `priceCents`.
  currency @19 :Text $optional;
  # Pre-formatted price for display.
  priceLabel @20 :Text $optional;
  # Aggregate rating when known.
  ratingOverall @21 :Float64 $optional;
  # Number of ratings when known.
  ratingCount @22 :Int64 $optional;
  # Whether the edition is abridged when the storefront says so.
  abridgement @23 :Abridgement;
}

# Success payload of the catalog list methods.
struct CatalogHits {
  # Hits in storefront order.
  hits @0 :List(CatalogHit);
}

# Result union of `searchCatalog` / `expandCandidates` / `listDeals`.
struct CatalogHitsReply {
  union {
    # Success: catalog hits.
    ok @0 :CatalogHits;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `ContentSource.catalogDetail`.
struct CatalogDetail {
  # False when the product is unknown to the storefront (`hit` is empty).
  found @0 :Bool;
  # Full catalog record when `found`.
  hit @1 :CatalogHit;
}

# Result union of `ContentSource.catalogDetail`.
struct CatalogDetailReply {
  union {
    # Success: detail record or not-found marker.
    ok @0 :CatalogDetail;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Purchase hint for SPA / CLI purchase deep-links.
struct PurchaseHint {
  # Storefront product id.
  productId @0 :Text;
  # Title when resolved.
  title @1 :Text $optional;
  # Purchase or product-page URL.
  url @2 :Text $optional;
  # Current price in minor units.
  priceCents @3 :Int64 $optional;
  # ISO currency code for price fields.
  currency @4 :Text $optional;
  # Pre-formatted current price.
  priceLabel @5 :Text $optional;
  # List / MSRP price in minor units.
  listPriceCents @6 :Int64 $optional;
  # Pre-formatted list price.
  listPriceLabel @7 :Text $optional;
  # Member / Plus price in minor units.
  memberPriceCents @8 :Int64 $optional;
  # Pre-formatted member price.
  memberPriceLabel @9 :Text $optional;
}

# Success payload of `ContentSource.purchaseHint`.
struct PurchaseHintResult {
  # False when the guest could not resolve the title (`hint` is empty).
  found @0 :Bool;
  # Purchase hint when `found`.
  hint @1 :PurchaseHint;
}

# Result union of `ContentSource.purchaseHint`.
struct PurchaseHintReply {
  union {
    # Success: hint or not-found marker.
    ok @0 :PurchaseHintResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Params of `RemoteLibrary.scanLibrary` (remote library sync).
struct ScanLibraryParams {
  # When true, force a full rescan even if the guest would otherwise
  # incremental-sync.
  force @0 :Bool;
}

# Params of `Oidc.authenticateUser`.
struct AuthenticateUserParams {
  # Integration username / login id.
  username @0 :Text;
  # Integration password; never logged by the host.
  password @1 :Text;
}

# One external user observed by an integration. The host may mint claim
# tickets without exposing portal details to the guest.
struct ExternalUser {
  # Integration provider id (often the plugin id).
  provider @0 :Text;
  # Provider-scoped user id.
  externalUserId @1 :Text;
  # Display name for UI.
  displayName @2 :Text $optional;
  # Ephemeral remote token (e.g. ABS JWT). Guest-to-host only; never
  # persisted.
  accessToken @3 :Text $optional;
}

# Result union of `Oidc.authenticateUser`.
struct ExternalUserReply {
  union {
    # Success: verified external user.
    ok @0 :ExternalUser;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `RemoteLibrary.pollEvents`: signals for the host to kick
# off workflows.
struct EventPollResult {
  # Newly observed external users since the last poll.
  users @0 :List(ExternalUser);
}

# Result union of `RemoteLibrary.pollEvents`.
struct EventPollReply {
  union {
    # Success: observed users.
    ok @0 :EventPollResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# One listening-progress row. The host upserts into `listening_progress`
# tagged with the plugin id; plugins never open the library DB.
struct ListeningProgress {
  # Provider-scoped user id.
  externalUserId @0 :Text;
  # Provider-scoped item / library id.
  externalItemId @1 :Text;
  # Bookclerk identity row id when already linked.
  identityId @2 :Int64 $optional;
  # Title text when known.
  title @3 :Text $optional;
  # Authors string when known.
  authors @4 :Text $optional;
  # Amazon ASIN when known.
  asin @5 :Text $optional;
  # ISBN when known.
  isbn @6 :Text $optional;
  # Fractional progress in `0.0..=1.0` when the provider reports it.
  progress @7 :Float64 $optional;
  # Current playback position in seconds.
  currentTimeSeconds @8 :Float64 $optional;
  # Total duration in seconds when known.
  durationSeconds @9 :Float64 $optional;
  # When true, the provider marks the item finished.
  isFinished @10 :Bool;
  # Last listen timestamp as unix milliseconds (UTC).
  lastListenedAtUnixMs @11 :UInt64 $optional;
}

# Success payload of `RemoteLibrary.syncListening`.
struct SyncListeningResult {
  # Progress snapshots to upsert.
  items @0 :List(ListeningProgress);
}

# Result union of `RemoteLibrary.syncListening`.
struct SyncListeningReply {
  union {
    # Success: progress snapshots.
    ok @0 :SyncListeningResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Guest-published domain event (`EventPublisher.publish`). The host stamps
# `eventId`, `source` (the plugin id), and `accountId` from the invocation;
# guests cannot forge either.
struct PluginEvent {
  # Snake_case event type; must be listed in `[[events.producers]]`.
  eventType @0 :Text;
  # Schema version of `payload`, owned by the event type.
  schemaVersion @1 :UInt32;
  # Producer idempotency key, unique per (account, source, eventType) in the
  # outbox; a repeat returns `PublishOk.duplicate = true` with the earlier id.
  # Empty publishes unconditionally.
  deduplicationKey @2 :Text;
  # Encoded event payload; at most `maxEventPayloadBytes`.
  payload @3 :Data;
  # When the producer observed the fact; zero means "now" on the host clock.
  occurredAtUnixMs @4 :UInt64;
  # Trace correlation id; empty inherits `Invocation.correlationId`.
  correlationId @5 :Text;
  # Id of the event or command that caused this one; empty inherits
  # `Invocation.causationId`.
  causationId @6 :Text;
}

# Success payload of `EventPublisher.publish`.
struct PublishOk {
  # Outbox event id (new or the earlier row when `duplicate`).
  eventId @0 :Text;
  # True when `deduplicationKey` matched an existing outbox row.
  duplicate @1 :Bool;
}

# Result union of `EventPublisher.publish`.
struct PublishReply {
  union {
    # Success: outbox row identity.
    ok @0 :PublishOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Identity of one `PluginWorker.open` invocation. The host issues ids; guests
# echo `correlationId` / `causationId` onto published events.
struct Invocation {
  # Unique host-issued invocation id.
  id @0 :Text;
  # Account scope; empty for operator / host-wide invocations.
  accountId @1 :Text;
  # UTC Unix milliseconds; zero when the invocation has no deadline. The host
  # fence is authoritative.
  deadlineUnixMs @2 :UInt64;
  # Trace correlation id; empty when none.
  correlationId @3 :Text;
  # Id of the event or command that caused this invocation; empty when none.
  causationId @4 :Text;
}

# --- rust-generated: end ---

# Universal database cell/parameter domain. Engine-native arrays, enums,
# unsigned integers, and JSON text sentinels are not baseline ABI values.
enum DbType {
  # Type not known (typed NULL of unknown domain).
  unspecified @0;
  # Boolean.
  bool @1;
  # Signed 64-bit integer.
  int64 @2;
  # IEEE-754 double.
  float64 @3;
  # UTF-8 text.
  text @4;
  # Raw bytes.
  bytes @5;
}

# One typed SQL cell or bind parameter.
struct DbValue {
  union {
    # SQL NULL with its declared type.
    null @0 :DbType;
    # Boolean.
    boolean @1 :Bool;
    # Signed 64-bit integer.
    int64 @2 :Int64;
    # IEEE-754 double.
    float64 @3 :Float64;
    # UTF-8 text.
    text @4 :Text;
    # Raw bytes.
    bytes @5 :Data;
  }
}

# Result column descriptor.
struct DbColumn {
  # Column name as projected.
  name @0 :Text;
  # Declared or inferred column type.
  dbType @1 :DbType;
}

# One result row.
struct DbRow {
  # Cells in `columns` order.
  values @0 :List(DbValue);
}

# How the host classifies a statement for result handling.
enum DbStatementKind {
  # Mutation without result rows.
  execute @0;
  # Query producing rows.
  select @1;
  # Mutation with `RETURNING` rows.
  returning @2;
}

# Which part of a statement's outcome the caller wants back.
enum DbResultSelection {
  # Return nothing.
  discard @0;
  # Return only `rowsAffected`.
  affectedRows @1;
  # Return rows and columns.
  rows @2;
}

# One guest statement in an `ExecuteRequest`.
struct DbStatement {
  # BookclerkSQL text; at most `maxScalarBytes`.
  sql @0 :Text;
  # Positional bind values.
  parameters @1 :List(DbValue);
  # Statement classification.
  kind @2 :DbStatementKind;
  # Row cap for queries; `0` means adapter default.
  maxRows @3 :UInt32;
  # Which outcome parts to return.
  resultSelection @4 :DbResultSelection;
}

# Guest statement batch for `GuestDatabase.execute`; runs atomically.
struct ExecuteRequest {
  # Caller-chosen idempotency key.
  operationId @0 :Text;
  # SHA-256 hex of the idempotency-relevant request; empty when omitted.
  requestHash @1 :Text;
  # Statements in execution order; at most `maxStatements`.
  statements @2 :List(DbStatement);
  # Deadline hint; `0` means none.
  deadlineUnixMs @3 :UInt64;
}

# Host → adapter execute. GuestDatabase stays ExecuteRequest-only.
enum IsolationReq {
  # All statements in one transaction.
  atomicBatch @0;
  # Run inside a savepoint of an enclosing transaction.
  nestedSavepoint @1;
  # One stable read snapshot for the whole batch.
  consistentSnapshot @2;
}

# Resolved BookclerkSQL column type after type checking.
enum ResolvedSqlType {
  # INTEGER.
  integer @0;
  # REAL.
  real @1;
  # TEXT.
  text @2;
  # BLOB.
  blob @3;
  # BOOLEAN.
  boolean @4;
  # NULL literal type.
  null @5;
}

# Byte span in the exact canonical SQL string a proof is bound to.
struct SqlSpan {
  # Inclusive start byte offset.
  start @0 :UInt32;
  # Exclusive end byte offset.
  end @1 :UInt32;
}

# One TEXT expression the adapter must collate bytewise (`COLLATE "C"`).
struct TextCollateSite {
  # Identifier or string-literal span in canonical SQL.
  span @0 :SqlSpan;
}

# INTEGER `+` `-` `*` `abs` site lowered to overflow -> NULL.
enum IntegerArithKind {
  # `lhs + rhs`.
  add @0;
  # `lhs - rhs`.
  sub @1;
  # `lhs * rhs`.
  mul @2;
  # `abs(arg)` (`lhs` is the argument span).
  abs @3;
}

# One INTEGER arithmetic expression that must not wrap or error on overflow.
struct IntegerArithSite {
  # Full expression span (`a + b` or `abs(n)`).
  full @0 :SqlSpan;
  # Left operand (or `abs` argument).
  lhs @1 :SqlSpan;
  # Right operand (`abs` repeats `lhs`).
  rhs @2 :SqlSpan;
  # Operator.
  kind @3 :IntegerArithKind;
}

# Physical table/column access used for authorization.
struct PhysicalAccess {
  # Physical table name.
  table @0 :Text;
  # Empty = table presence only; "*" = projection wildcard.
  column @1 :Text;
}

# Destination assignment `lhs = rhs` (INSERT, UPDATE, ...).
struct ResolvedAssignment {
  # Target table.
  table @0 :Text;
  # Target column.
  column @1 :Text;
  # Declared column type.
  dest @2 :ResolvedSqlType;
  # Resolved type of the assigned expression.
  source @3 :ResolvedSqlType;
}

# Column name paired with its resolved type.
struct NamedSqlType {
  # Column name.
  name @0 :Text;
  # Resolved type.
  sqlType @1 :ResolvedSqlType;
}

# `REFERENCES` target of one column.
struct ColumnReference {
  # Referenced table.
  refTable @0 :Text;
  # Referenced columns.
  refColumns @1 :List(Text);
}

# Per-column `REFERENCES` slot in `CreateTableSchema`.
struct OptionalColumnReference {
  union {
    # Column has no reference.
    none @0 :Void;
    # Column references another table.
    some @1 :ColumnReference;
  }
}

# Table-level `FOREIGN KEY` constraint.
struct ForeignKeyConstraint {
  # Local columns.
  columns @0 :List(Text);
  # Referenced table.
  refTable @1 :Text;
  # Referenced columns.
  refColumns @2 :List(Text);
}

# One table-level constraint.
struct TableConstraint {
  union {
    # `PRIMARY KEY (...)` columns.
    primaryKey @0 :List(Text);
    # `UNIQUE (...)` columns.
    unique @1 :List(Text);
    # `CHECK (...)` expression text.
    check @2 :Text;
    # `FOREIGN KEY (...) REFERENCES ...`.
    foreignKey @3 :ForeignKeyConstraint;
  }
}

# Parsed `CREATE TABLE` (canonical SQL v1). Per-column lists align with `columns`.
struct CreateTableSchema {
  # Table name.
  table @0 :Text;
  # Columns with resolved types, in order.
  columns @1 :List(NamedSqlType);
  # `INTEGER PRIMARY KEY AUTOINCREMENT` column; empty when none.
  identityColumn @2 :Text;
  # Per-column NOT NULL flags.
  columnNotNull @3 :List(Bool);
  # Per-column UNIQUE flags.
  columnUnique @4 :List(Bool);
  # Per-column PRIMARY KEY flags.
  columnPrimaryKey @5 :List(Bool);
  # Per-column DEFAULT expression text; empty when none.
  columnDefaults @6 :List(Text);
  # Per-column CHECK expression text; empty when none.
  columnChecks @7 :List(Text);
  # Per-column REFERENCES targets.
  columnReferences @8 :List(OptionalColumnReference);
  # Table-level constraints, in order.
  tableConstraints @9 :List(TableConstraint);
}

# `CREATE TABLE` action with its durable fingerprint.
struct SchemaCreate {
  # Parsed table schema.
  schema @0 :CreateTableSchema;
  # Structured schema fingerprint (hex SHA-256).
  fingerprint @1 :Text;
  # True when the catalog already holds this exact fingerprint.
  noop @2 :Bool;
}

# CREATE/DROP action recorded on a proof.
struct SchemaAction {
  union {
    # Not DDL.
    none @0 :Void;
    # `CREATE TABLE`.
    create @1 :SchemaCreate;
    # `DROP TABLE` of the named table.
    drop @2 :Text;
  }
}

# Host-produced typed proof bound to one exact canonical statement.
struct ResolvedStatement {
  # SHA-256 hex of the exact canonical SQL this proof claims.
  statementHash @0 :Text;
  # SELECT / RETURNING / VALUES output columns in order.
  outputColumns @1 :List(NamedSqlType);
  # Physical tables/columns referenced (authorization).
  physicalAccesses @2 :List(PhysicalAccess);
  # Mutation destination assignments.
  assignments @3 :List(ResolvedAssignment);
  # TEXT expression spans needing bytewise collation.
  textCollateSites @4 :List(TextCollateSite);
  # INTEGER overflow sites (`+` `-` `*` `abs`).
  integerArithSites @5 :List(IntegerArithSite);
  # Function names invoked (folded), for authorization.
  functions @6 :List(Text);
  # DDL action + fingerprint.
  schemaAction @7 :SchemaAction;
}

# Replay receipt the host asks the adapter to persist with the batch.
struct AdapterReceipt {
  # Guest statement count inside the receipt wrap (excluding host prune/select).
  guestLen @0 :UInt32;
  # Guest `requestHash` compared on replay.
  guestHash @1 :Text;
}

# One host-lowered statement with its proof.
struct AdapterStatement {
  # Canonical SQL text.
  sql @0 :Text;
  # Positional bind values.
  parameters @1 :List(DbValue);
  # Statement classification.
  kind @2 :DbStatementKind;
  # Row cap for queries; `0` means adapter default.
  maxRows @3 :UInt32;
  # Which outcome parts to return.
  resultSelection @4 :DbResultSelection;
  # Typed proof bound to `sql`.
  proof @5 :ResolvedStatement;
}

# Host -> adapter batch: proven statements plus isolation and receipt.
struct AdapterExecuteRequest {
  # Host-chosen idempotency key.
  operationId @0 :Text;
  # SHA-256 hex of the idempotency-relevant request.
  requestHash @1 :Text;
  # Statements in execution order.
  statements @2 :List(AdapterStatement);
  # Deadline hint; `0` means none.
  deadlineUnixMs @3 :UInt64;
  # Transaction isolation the adapter must realize.
  isolation @4 :IsolationReq;
  # Replay receipt to persist; zero/empty when not required.
  receipt @5 :AdapterReceipt;
}

# Outcome of one statement.
struct StatementResult {
  # Result rows (empty unless `rows` selected).
  rows @0 :List(DbRow);
  # Result column descriptors.
  columns @1 :List(DbColumn);
  # Rows changed by a mutation.
  rowsAffected @2 :UInt64;
}

# Engine timing on `ExecuteReply`.
struct DbTiming {
  # Monotonic duration of this handler attempt (microseconds).
  attemptElapsedUs @0 :UInt64;
  # Engine-reported SQL/transaction time when available (`0` = omitted).
  dbExecutionUs @1 :UInt64;
  # How `dbExecutionUs` was measured.
  dbTimingSource @2 :Text;
}

# Success payload of `execute`.
struct ExecuteReply {
  # Echo of the request `operationId`.
  operationId @0 :Text;
  # Per-statement results in request order.
  statements @1 :List(StatementResult);
  # Engine timing.
  timing @2 :DbTiming;
}

# Result union of `execute`.
struct ExecuteResultReply {
  union {
    # Success: statement results.
    ok @0 :ExecuteReply;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Semantic SQL-contract advertisement. Diagnostic engine identity is not
# part of the capability plane — see `DbBootstrap`.
struct DbCapabilities {
  # Bookclerk SQL contract version.
  sqlContractVersion @0 :UInt32;
  # Guest can run a bounded statement list as one SQL transaction.
  atomicBatch @1 :Bool;
  # Guest SQL supports `RETURNING`.
  returning @2 :Bool;
  # Guest reports `rowsAffected`.
  affectedRows @3 :Bool;
  # Guest versions schema with a `bookclerk_schema_migrations` table.
  schemaMigrations @4 :Bool;
  # Guest honors RPC/session cancellation.
  cancellation @5 :Bool;
  # Guest can fill `DbTiming.dbExecutionUs`. Not a host connect minimum.
  timing @6 :Bool;
  # Maximum bound parameters per statement.
  maxBinds @7 :UInt32;
  # Maximum statements in one atomic batch.
  maxStatements @8 :UInt32;
  # Maximum rows a query statement may return.
  maxResultRows @9 :UInt32;
  # Maximum UTF-8 bytes of SQL plus binds per statement.
  maxPayloadBytes @10 :UInt32;
  # Maximum encoded bytes of one statement's result rows.
  maxResultBytes @11 :UInt32;
  # Maximum UTF-8 / blob bytes of one result cell.
  maxCellBytes @12 :UInt32;
  # Maximum encoded bytes of one `ExecuteRequest`.
  maxRequestBytes @13 :UInt32;
  # Maximum encoded bytes of one `ExecuteReply`.
  maxAtomicResultBytes @14 :UInt32;
  # Append-only. Adapter can open additional isolated sessions
  # for plugin-owned database bindings (per-binding file / schema / database).
  pluginDatabases @15 :Bool;
  # Maximum arguments in one physical function call after adapter hiding
  # (nested min / max / coalesce). Portable json_object is already ≤ 32
  # arguments (16 pairs), matching D1. `0` is unspecified.
  maxFunctionArgs @16 :UInt32;
  # Maximum columns in one CREATE TABLE / result row. `0` is unspecified.
  maxSchemaColumns @17 :UInt32;
  # Maximum UTF-8 bytes of a BookclerkSQL LIKE pattern value (literals and
  # TEXT binds). Adapters that expand LIKE into GLOB must advertise a
  # conservative value that still fits the physical pattern cap. `0` is
  # unspecified.
  maxPatternBytes @18 :UInt32;
  # Maximum UTF-8 bytes of sqlite-family lowered SQL the adapter can realize
  # for one statement (INTEGER overflow wraps, LIKE→GLOB, NULLIF, INSERT OR
  # IGNORE, query LIMIT wrap, bytes-placeholder expansion). `0` is
  # unspecified: the host does not enforce a lowered-size ceiling. First-party
  # D1 advertises 100000. Hosts compare a standardized Bookclerk lowering
  # upper bound against this number and must not branch on engine identity.
  maxLoweredStatementBytes @19 :UInt32;
  # Adapter can expose one stable logical database state while the host
  # reads schema, rows, and identity.
  consistentBackupRead @20 :Bool;
  # Adapter can destructively replace one logical database unit so an
  # ordinary restore failure does not leave that unit partially replaced.
  atomicUnitRestore @21 :Bool;
}

# Result union of `AdapterDatabaseSession.bootstrap`.
struct DbBootstrapReply {
  union {
    # Success: bootstrap metadata.
    ok @0 :DbBootstrap;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Bootstrap-only diagnostic metadata (not a capability).
struct DbBootstrap {
  # Diagnostic physical engine name. Hosts must not admit or generate SQL
  # from this value. Any string is valid.
  engine @0 :Text;
}

# Result union of `AdapterDatabaseSession.capabilities`.
struct DbCapabilitiesReply {
  union {
    # Success: capability advertisement.
    ok @0 :DbCapabilities;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# `databaseAdapter` entrypoint (`Entrypoints.databaseAdapter`).
interface Database {
  # Open one adapter session (capability negotiation + typed execute).
  openSession @0 () -> (result :AdapterSessionReply);
}

# Adapter-private identity high-water (sqlite_sequence / bookclerk_identity).
# Column names live in the canonical backup schema, not this catalog.
struct IdentityHighWater {
  # Table whose identity column the mark belongs to.
  table @0 :Text;
  # Highest generated or stored value that must not be reused.
  last @1 :Int64;
}

# Result union of `AdapterDatabaseSession.exportIdentity`.
struct IdentityExportReply {
  union {
    # Success: identity high-water rows.
    ok @0 :List(IdentityHighWater);
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `AdapterDatabaseSession.listUserRelations`.
struct UserRelationsReply {
  union {
    # Success: user relation names.
    ok @0 :List(Text);
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Host ↔ database adapter plugin. Capability negotiation + typed execute only.
interface AdapterDatabaseSession {
  # Semantic SQL-contract advertisement for this session.
  capabilities @0 () -> (result :DbCapabilitiesReply);
  # Canonical SQL + required structured proofs (not JSON).
  execute @1 (
      request :AdapterExecuteRequest  # Proven statements plus isolation and receipt.
  ) -> (result :ExecuteResultReply);
  # Release the session; further calls fail.
  close @2 () -> (result :EmptyReply);
  # Bootstrap-only SeaORM proxy metadata (not part of DbCapabilities).
  bootstrap @3 () -> (result :DbBootstrapReply);
  # Snapshot/identity/restore primitives (not a SQL dialect API).
  exportIdentity @4 () -> (result :IdentityExportReply);
  # Restore identity high-water marks captured by `exportIdentity`.
  importIdentity @5 (
      rows :List(IdentityHighWater)  # High-water rows to apply.
  ) -> (result :EmptyReply);
  # Names of user relations present in the logical database unit.
  listUserRelations @6 () -> (result :UserRelationsReply);
  # Enter restore mode for one logical unit (`atomicUnitRestore`).
  prepareUnitRestore @7 () -> (result :EmptyReply);
  # Drop the named user relations during restore.
  dropUserRelations @8 (
      names :List(Text)  # Relation names from `listUserRelations`.
  ) -> (result :EmptyReply);
  # Verify constraints hold after restore rows were written.
  assertRestoreConstraints @9 () -> (result :EmptyReply);
}

# Host-granted SQL for job plugin authors. SDK `DatabaseBinding` mirrors the
# Cloudflare Workers D1 surface (`prepare`/`bind`/`run`/`first`/`all`/`raw`,
# `batch`, `exec`) over this typed `execute` transport; wire types stay Cap'n
# `ExecuteRequest`/`ExecuteReply`.
interface GuestDatabase {
  # Run one typed statement batch.
  execute @0 (
      request :ExecuteRequest  # Statements, binds, and deadline.
  ) -> (result :ExecuteResultReply);
  # Release the session; further calls fail.
  close @1 () -> (result :EmptyReply);
}

# One already-separated BookclerkSQL operation in a plugin-owned migration.
# `schema` is admitted DDL; `data` is admitted DML. There is no native SQL
# escape hatch.
struct PluginMigrationOp {
  union {
    # Admitted DDL statement text.
    schema @0 :Text;
    # Admitted DML statement text.
    data @1 :Text;
  }
}

# One plugin-owned migration application. `id` is an opaque plugin-chosen
# stable identity (name, UUID, timestamp-like string, or digits-as-text).
# Bookclerk assigns no order, version, or predecessor meaning to `id`.
# Registration order is the forward sequence.
struct PluginMigration {
  # Opaque plugin-chosen stable identity.
  id @0 :Text;
  # Operations in forward order; at most `maxPluginMigrationOps`.
  operations @1 :List(PluginMigrationOp);
}

# Success payload of `PluginWorker.databaseMigrations`.
struct PluginMigrationsOk {
  # At most `maxListPage` entries; aggregate id+SQL bytes at most
  # `maxPluginMigrationRegistrationBytes`; total operations at most
  # `maxPluginMigrationTotalOps`.
  migrations @0 :List(PluginMigration);
}

# Result union of `PluginWorker.databaseMigrations`.
struct PluginMigrationsReply {
  union {
    # Success: ordered migration sequence.
    ok @0 :PluginMigrationsOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Plugin bootstrap capability: the guest's root object (Workers `default`
# export analogue). `describe()` first; `open()` once per invocation.
interface PluginWorker {
  # Identity, ABI version, negotiated features, and typed capabilities.
  describe @0 () -> (result :DescribeReply);
  # Open the exported entrypoints for one invocation with host-granted
  # bindings. Entrypoints the manifest does not export are null.
  open @1 (
      invocation :Invocation,  # Host-issued invocation identity.
      bindings :Bindings  # Granted `env` bindings.
  ) -> (result :EntrypointsReply);
  # Flush and release resources before the process exits.
  shutdown @2 () -> (result :EmptyReply);
  # Complete ordered plugin-owned migration sequence for one named binding.
  # The host calls this before `open` while provisioning `[[databases]]`
  # (a binding must be migrated before any `Bindings.databases` session is
  # handed out). Empty list means the binding has no plugin-owned migrations.
  # Bounded by `maxListPage` / `maxPluginMigrationOps` /
  # `maxPluginMigrationTotalOps` / `maxScalarBytes` /
  # `maxPluginMigrationRegistrationBytes`.
  databaseMigrations @3 (
      binding :Text  # Binding name from `plugin.toml`.
  ) -> (result :PluginMigrationsReply);
}
