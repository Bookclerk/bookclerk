# Bookclerk plugin ABI — object-capability Workers RPC (`api_version = 3`).
#
# This file is the first supported plugin contract. Discarded development
# compatibility fields were removed and ordinals compacted. From this contract
# forward:
# - `apiVersion` / `plugin.toml` `api_version` is the single incompatible ABI
#   version. Manifest `api_version` and `describe().apiVersion` must match.
# - Named `rpcFeatures` negotiate optional facilities. Required features are
#   rejected at spawn when missing.
# - Do not reuse field, method, or union ordinals after the v1 schema freeze.
#   Before the freeze, renumbering is allowed when every first-party plugin and
#   SDK is updated in the same change (see docs/adr/library-data-model.md).
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
#   the `EVENTS` publisher, named `[[queues.producers]]` producers, and named
#   `[[databases]]` sessions.
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
# Maximum keys or key pairs in one `Destination.moveMany`, `deleteMany` or
# `headMany` call, and maximum outcomes in one reply.
const maxBatchKeys :UInt32 = 256;
# Maximum changes in one `Destination.changes` page.
const maxChangesPage :UInt32 = 256;
# Maximum bytes of a `Destination.changes` cursor.
const maxChangesCursorBytes :UInt32 = 4096;
# Maximum job / event checkpoint payload size (bytes).
const maxCheckpointBytes :UInt32 = 65536;
# Maximum titles, and separately removed product ids, in one `ScanPage`.
const maxScanPageBooks :UInt32 = 500;
# Maximum bytes of a scan cursor or sync token.
const maxScanCursorBytes :UInt32 = 4096;
# Maximum bytes of refreshed credentials a storefront hands back
# (`ScanPage.refreshedCredentials`, `PlainFetch.refreshedCredentials`,
# `QualityOffersResult.refreshedCredentials`).
const maxRefreshedCredentialBytes :UInt32 = 65536;
# Maximum bytes of an `EventReadRequest.after` / `EventReadPage.cursor`.
const maxEventCursorBytes :UInt32 = 512;
# Maximum plugin / account identifier length (bytes).
const maxIdentifierBytes :UInt32 = 64;
# Maximum `[triggers] crons` source length (bytes), as in `ScheduledController.cron`.
const maxCronBytes :UInt32 = 128;
# Maximum granted config payload size (bytes).
const maxConfigPayloadBytes :UInt32 = 65536;
# Maximum decoded size of a domain-event scalar payload (not a stream).
const maxEventPayloadBytes :UInt32 = 65536;
# Largest `KvNamespace` value in bytes; a `[[kv_namespaces]]` row may lower
# its own cap but never raise it past this.
const maxKvValueBytes :UInt32 = 1048576;
# Most URLs in one `CoverFetcher.fetchCovers` call.
const maxCoverUrls :UInt32 = 32;
# Largest image body `CoverFetcher.fetchCovers` may return for one URL.
const maxCoverBytes :UInt32 = 10485760;
# Longest cache header value, in bytes, `CoverFetcher.fetchCovers` carries in
# either direction (a condition the host sends, a header the server sent).
# The host ignores a longer header the fetcher returns.
const maxCoverHeaderBytes :UInt32 = 1024;
# Largest metadata file `MetadataFiles.parse` reads and `MetadataFiles.render`
# returns. The host skips a larger sidecar before any plugin sees it.
const maxMetadataFileBytes :UInt32 = 262144;
# Longest description a `MetadataFileRecord` carries, in UTF-8 bytes.
const maxMetadataFileDescriptionBytes :UInt32 = 65536;
# Longest other text value in a `MetadataFileRecord` (a title, a name, a
# genre, a chapter title), in UTF-8 bytes.
const maxMetadataFileTextBytes :UInt32 = 1024;
# Most entries in each list of a `MetadataFileRecord` other than chapters
# (credits, genres, tags, identifiers, series).
const maxMetadataFileListEntries :UInt32 = 256;
# Most chapters in a `MetadataFileRecord`.
const maxMetadataFileChapters :UInt32 = 2000;
# Longest `NotificationMessage.title` and `NotificationField` name, in UTF-8
# bytes.
const maxNotificationTitleBytes :UInt32 = 256;
# Longest `NotificationMessage.text` or `markdown`, in UTF-8 bytes.
const maxNotificationTextBytes :UInt32 = 16384;
# Longest `NotificationMessage.html`, in UTF-8 bytes.
const maxNotificationHtmlBytes :UInt32 = 65536;
# Most `NotificationField`s on one message or raise.
const maxNotificationFields :UInt32 = 25;
# Longest `NotificationField.value`, link, group key or alert key, in UTF-8
# bytes.
const maxNotificationFieldBytes :UInt32 = 1024;
# Longest destination address document (`addressJson`), in UTF-8 bytes.
const maxNotifierAddressBytes :UInt32 = 4096;
# Most `NotifyDeliverabilityCheck`s one deliverability report lists.
const maxDeliverabilityChecks :UInt32 = 8;
# Most `NotifyDeliverabilityAdvice` entries one deliverability report lists.
const maxDeliverabilityAdvice :UInt32 = 32;
# Longest `Notifier.checkDeliverability` may run, in milliseconds: checkers
# that read a delivered message need time for it to arrive.
const deliverabilityCheckDeadlineMs :UInt32 = 120000;
# Longest `AiModel.name`, in UTF-8 bytes (`@<plugin id>/<model id>`).
const maxAiModelNameBytes :UInt32 = 192;
# Most models one `AiModels.models` reply lists.
const maxAiModels :UInt32 = 64;
# Most texts in one `AiTextEmbeddingsInput`.
const maxAiEmbedTexts :UInt32 = 64;
# Longest text in an `AiTextEmbeddingsInput`, in UTF-8 bytes.
const maxAiEmbedTextBytes :UInt32 = 16384;
# Most numbers in one embedding vector.
const maxAiEmbedDimensions :UInt32 = 4096;
# Most language tags on one `AiModel`.
const maxAiLanguages :UInt32 = 64;
# Most entries in `MetadataFileRenderParams.audioFiles`.
const maxMetadataAudioFiles :UInt32 = 4096;
# Longest `KvNamespace` key in UTF-8 bytes.
const maxKvKeyBytes :UInt32 = 512;
# Largest WebSocket message, text or binary, that `Sockets.send` accepts or
# `SocketHandler.message` delivers.
const maxSocketFrameBytes :UInt32 = 262144;
# Largest per-socket attachment the host keeps for a sleeping guest.
const maxSocketAttachmentBytes :UInt32 = 2048;
# Most host-brokered sockets one plugin instance may hold open at once.
const maxSocketsPerInstance :UInt32 = 8;
# Most sockets into the library (accepted `Origin.connect` upgrades) one
# relay ingress instance may hold at once, apart from `maxSocketsPerInstance`:
# the host holds them in process, not in the socket broker.
const maxOriginSocketsPerInstance :UInt32 = 256;
# Most heartbeat pairs one `Sockets.connect` may declare.
const maxSocketAutoResponses :UInt32 = 4;
# Largest heartbeat request or response frame, in bytes.
const maxSocketAutoResponseBytes :UInt32 = 256;
# Most trust anchors one `tls://` `Sockets.connect` may supply.
const maxSocketTrustAnchors :UInt32 = 4;
# Largest DER certificate in `SocketConnectRequest.trustAnchors`, in bytes.
const maxSocketTrustAnchorBytes :UInt32 = 8192;
# Longest `SocketConnectRequest.serverName`, in bytes (a DNS name's limit).
const maxSocketServerNameBytes :UInt32 = 253;
# Longest `SocketConnectRequest.url`, in bytes.
const maxSocketUrlBytes :UInt32 = 2048;
# Most request headers, and separately subprotocols, on one `Sockets.connect`.
const maxSocketHeaders :UInt32 = 16;
# Most header fields on one `Origin.fetch` request, and separately on its
# response.
const maxOriginHeaders :UInt32 = 128;
# Largest total UTF-8 bytes of header names and values on one `Origin.fetch`
# request, and separately on its response.
const maxOriginHeaderBytes :UInt32 = 65536;
# Longest `OriginRequest.target` (path and query), in bytes.
const maxOriginTargetBytes :UInt32 = 8192;
# Longest `IngressStarted.origin`, in bytes.
const maxIngressOriginBytes :UInt32 = 253;
# Largest body of an `OriginRefusal` (the library's answer to a WebSocket
# upgrade it did not accept), in bytes; a longer body is cut to this.
const maxOriginRefusalBodyBytes :UInt32 = 65536;
# Most deliveries one `Webhooks.pull` returns.
const maxWebhookPullBatch :UInt32 = 100;
# Longest `WebhookDelivery.leaseId`, in bytes.
const maxWebhookLeaseIdBytes :UInt32 = 128;
# Longest `Webhooks.pull` visibility timeout, in milliseconds (12 hours).
const maxWebhookVisibilityMs :UInt32 = 43200000;
# Largest stored webhook request body, in bytes (1 MiB); a `[[webhooks]]`
# row may lower its own cap but never raise it past this.
const maxWebhookBodyBytes :UInt32 = 1048576;
# Most request headers a `[[webhooks]]` row keeps; a delivery also carries
# `content-type`, so it holds at most one more.
const maxWebhookHeaders :UInt32 = 16;
# Longest `[[webhooks]]` name, in bytes.
const maxWebhookNameBytes :UInt32 = 32;
# Longest kept request header value, in bytes.
const maxWebhookHeaderValueBytes :UInt32 = 8192;
# Longest `ProvisionGrant.token`, in bytes.
const maxProvisionTokenBytes :UInt32 = 4096;
# Longest `ProvisionedSecret.value`, in bytes.
const maxProvisionSecretBytes :UInt32 = 1024;
# Most credentials one `Provisioned` returns.
const maxProvisionSecrets :UInt32 = 16;
# Largest `Provisioned.settingsJson` and `ProvisionRequest.inputsJson`, in
# bytes.
const maxProvisionSettingsBytes :UInt32 = 16384;
# Most lines in `Provisioned.summary`.
const maxProvisionSummaryLines :UInt32 = 8;
# Longest line of `Provisioned.summary`, in bytes.
const maxProvisionSummaryLineBytes :UInt32 = 200;
# Largest `KvNamespace` metadata JSON text in bytes.
const maxKvMetadataBytes :UInt32 = 1024;
# Most entries in each list of a `NetworkGrant` (fetch hosts, TCP
# destinations, network settings).
const maxNetworkGrantEntries :UInt32 = 256;
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
# Guest implements `Destination.move` (`storage.move`).
const featureStorageMove :Text = "storage.move";
# Guest implements `Destination.moveMany` (`storage.moveMany`).
const featureStorageMoveMany :Text = "storage.moveMany";
# Guest implements `Destination.deleteMany` (`storage.deleteMany`).
const featureStorageDeleteMany :Text = "storage.deleteMany";
# Guest implements `Destination.headMany` (`storage.headMany`).
const featureStorageHeadMany :Text = "storage.headMany";
# Guest enforces `WriteOptions.ifMatch` / `ifNoneMatch` (`storage.conditionalWrite`).
const featureStorageConditionalWrite :Text = "storage.conditionalWrite";
# Guest enforces `WriteOptions.ifMatch` / `ifNoneMatch` by writing, then
# checking and withdrawing its write when another got in first
# (`storage.checkedWrite`); a reader may briefly see the losing write.
const featureStorageCheckedWrite :Text = "storage.checkedWrite";
# Guest implements `Destination.changes` (`storage.changes`).
const featureStorageChanges :Text = "storage.changes";
# Guest implements `Destination.translateKeys` (`storage.translateKeys`).
const featureStorageTranslateKeys :Text = "storage.translateKeys";

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
  # Structured extras as one JSON object (`limitExceeded` carries `limit`,
  # `cap` and `used`; engine failures carry `engineCode`). Empty when none.
  details @2 :Text;
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
  # Last modification time, UTC unix milliseconds; zero when unknown.
  modifiedAtMs @5 :UInt64;
  # Backend object version id (versioned buckets); empty when none.
  versionId @6 :Text;
  # Opaque change signature: an unchanged value means the object has not
  # changed. Backend-defined (a local file's inode, size, mtime and ctime;
  # an S3 ETag and version id). Empty when the backend has none.
  signature @7 :Text;
}

# Compact object entry in a list page.
struct ObjectInfo {
  # Object key.
  key @0 :Text;
  # Object size in bytes.
  size @1 :UInt64;
  # Backend entity tag; empty when unsupported or not requested
  # (`ListOptions.withSignatures`).
  etag @2 :Text;
  # Last modification time, UTC unix milliseconds; zero when unknown or not
  # requested.
  modifiedAtMs @3 :UInt64;
  # Backend object version id; empty when none or not requested.
  versionId @4 :Text;
  # Opaque change signature, as `ObjectMetadata.signature`; empty when
  # unsupported or not requested.
  signature @5 :Text;
}

# Paging options for `Destination.list`.
struct ListOptions {
  # Only keys starting with this prefix; empty lists everything.
  prefix @0 :Text;
  # Opaque cursor from a previous `ListPage.nextCursor`; empty starts over.
  cursor @1 :Text;
  # Requested page size; clamped to `maxListPage`. `0` means guest default.
  limit @2 :UInt32;
  # Hint: fill `etag`, `modifiedAtMs`, `versionId` and `signature` on each
  # entry. `ListPage.signaturesIncluded` says whether the guest did.
  withSignatures @3 :Bool;
  # Hint: only objects modified at or after this time (UTC unix
  # milliseconds); zero lists everything. `ListPage.modifiedSinceApplied`
  # says whether the guest filtered; when it did not, the page is unfiltered.
  modifiedSinceMs @4 :UInt64;
}

# One page of `Destination.list` results.
struct ListPage {
  # Objects in this page, in backend order.
  objects @0 :List(ObjectInfo);
  # Cursor for the next page; empty when exhausted.
  nextCursor @1 :Text;
  # True when every entry carries the signature fields the backend has
  # (`ListOptions.withSignatures` honoured).
  signaturesIncluded @2 :Bool;
  # True when the guest left out objects modified before
  # `ListOptions.modifiedSinceMs`. False means the page is unfiltered.
  modifiedSinceApplied @3 :Bool;
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
  # Conditional write (`storage.conditionalWrite`): publish only while the
  # stored object's `etag` equals this value, else fail with `conflict`.
  # Empty means unconditional. A staged put checks it at `commit`.
  ifMatch @5 :Text;
  # Conditional write (`storage.conditionalWrite`): publish only when no
  # object exists at the key, else fail with `conflict`.
  ifNoneMatch @6 :Bool;
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
  # Modification time of the stored object, UTC unix milliseconds; zero when
  # unknown.
  modifiedAtMs @4 :UInt64;
  # Version id of the stored object; empty when none.
  versionId @5 :Text;
  # Change signature of the stored object (`ObjectMetadata.signature`), so
  # the host can recognise its own write in a later listing; empty when none.
  signature @6 :Text;
}

# Summary of a server-side copy.
struct CopyResult {
  # Bytes copied.
  bytesCopied @0 :UInt64;
}

# Options for `Destination.move` and `Destination.moveMany`. Fields are only
# ever appended.
struct MoveOptions {
  # Fail with `conflict` instead of replacing an object at the target key.
  noOverwrite @0 :Bool;
  # `move` only: move only while the source's `etag` equals this value,
  # else fail with `conflict`. Empty skips the check.
  ifSourceMatch @1 :Text;
}

# Summary of `Destination.move`.
struct MoveResult {
  # True when the backend moved atomically (a rename): no reader saw the
  # object at both keys or at neither.
  atomic @0 :Bool;
  # True when the source was already gone and the target present, as when
  # a move is repeated after a restart. The move counts as done.
  alreadyMoved @1 :Bool;
}

# One source and target key.
struct KeyPair {
  # Source object key (or source prefix).
  from @0 :Text;
  # Target object key (or target prefix).
  to @1 :Text;
}

# What `Destination.moveMany` moves.
struct MoveManyTarget {
  union {
    # Explicit moves, at most `maxBatchKeys`. Each is independent.
    pairs @0 :List(KeyPair);
    # Every object under `prefix.from` moves under `prefix.to`, keeping the
    # rest of its key. Paged through `MoveManyRequest.cursor`.
    prefix @1 :KeyPair;
  }
}

# Argument of `Destination.moveMany`. Fields are only ever appended.
struct MoveManyRequest {
  # Explicit pairs or a prefix.
  target @0 :MoveManyTarget;
  # Prefix form: `MoveManyPage.nextCursor` from the previous call; empty
  # starts. Ignored for `pairs`.
  cursor @1 :Text;
  # Prefix form: objects to move in this call, clamped to `maxBatchKeys`;
  # zero means guest default.
  limit @2 :UInt32;
  # Overwrite rule applied to every move (`ifSourceMatch` is ignored).
  options @3 :MoveOptions;
}

# Outcome for one key of a batch call.
struct KeyOutcome {
  # The key this outcome is for (the source key for moves).
  key @0 :Text;
  # Failure for this key; an empty `code` means it succeeded.
  error @1 :PluginError;
}

# One page of `Destination.moveMany` progress.
struct MoveManyPage {
  # Outcomes in request order. A prefix move done in one step (a folder
  # rename) may report a single outcome keyed by the source prefix.
  results @0 :List(KeyOutcome);
  # Prefix form: cursor for the next call; empty when every object moved.
  nextCursor @1 :Text;
  # True when every move in this page was atomic.
  atomic @2 :Bool;
  # Objects moved in this page (a folder rename counts what it carried
  # when known, else the outcomes).
  objectsMoved @3 :UInt64;
}

# Result of `Destination.deleteMany`.
struct DeleteManyResult {
  # One outcome per requested key, in request order. A missing key succeeds.
  results @0 :List(KeyOutcome);
}

# Result of `Destination.headMany`.
struct HeadManyResult {
  # One probe per requested key, in request order.
  objects @0 :List(HeadOk);
}

# Success payload of `Destination.translateKeys`.
struct TranslateKeysResult {
  # One translated key per requested host key, in request order: the key
  # the guest stores that host key under. Each is a relative key with no
  # empty, `.` or `..` segment.
  keys @0 :List(Text);
}

# Argument of `Destination.changes`. Fields are only ever appended.
struct ChangesRequest {
  # `ChangesPage.cursor` from the previous call. Empty asks for the current
  # position: the page carries a cursor and no changes.
  cursor @0 :Text;
  # Requested changes per page, clamped to `maxChangesPage`; zero means
  # guest default.
  limit @1 :UInt32;
}

# What happened to an object in a change feed. Unknown codes are kept and
# treated as `changed`.
enum ObjectChangeKind {
  # Something changed; probe the key to learn what.
  changed @0;
  # The object was created or rewritten.
  upserted @1;
  # The object was removed.
  removed @2;
}

# One entry of a change feed.
struct ObjectChange {
  # Object key, relative to the destination root.
  key @0 :Text;
  # What happened to it.
  kind @1 :ObjectChangeKind;
  # Listing fields after the change when the feed carries them (signature
  # fields may be empty); empty for `removed`.
  info @2 :ObjectInfo;
}

# One page of `Destination.changes`.
struct ChangesPage {
  # Changes since the request's cursor, oldest first. A key may repeat.
  changes @0 :List(ObjectChange);
  # Cursor for the next call; never empty.
  cursor @1 :Text;
  # True when more changes are ready now; call again with `cursor`.
  more @2 :Bool;
  # False when the feed may have missed changes since the request's cursor
  # (an overflowed watcher, expired queue retention, a restart): the host
  # backs it with a full listing. True means these are all the changes.
  complete @3 :Bool;
  # True when this running guest produces the feed itself (a filesystem
  # watcher) and stopping it would lose changes, so the host keeps it awake
  # while `complete` stays true. Durable feeds (a notification queue) leave
  # it false. The host honours it only from the first-party storage plugins
  # it starts on demand.
  liveWatch @4 :Bool;
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
  # Catalog behaviours a storefront declares so the host can use any store
  # for catalog search without naming it; all false for other kinds.
  catalog @13 :CatalogCapabilities;
  # Work kinds and edition carriers the plugin lists and fetches; required
  # for storefronts and metadata providers, empty lists for every other
  # kind (see `MediaSupport`).
  media @14 :MediaSupport;
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
  # Named `[[queues.producers]]` bindings: one host-served producer per
  # binding, each sending into the queue the row names. Empty when the
  # manifest declares none.
  queues @7 :List(NamedQueue);
  # Named `[[kv_namespaces]]` bindings: one host-served durable namespace per
  # binding, scoped to this plugin. Empty when the manifest declares none.
  kv @8 :List(NamedKv);
  # `SOCKETS`: host-brokered outbound WebSockets that stay open while the
  # guest sleeps; null unless `[sockets]` is declared and granted.
  sockets @9 :Sockets;
  # `NOTIFY`: raises the notification types the plugin declares in
  # `[[notifications.types]]`; null when it declares none.
  notify @10 :NotificationRaiser;
  # `NETWORK`: what the operator granted this plugin's network, including
  # the destination each network setting names and whether it is approved,
  # so a guest can tell an unapproved destination from an unreachable one.
  network @11 :NetworkGrant;
  # `ORIGIN`: the library itself, for a relay `ingress` plugin to send the
  # public requests it carries to; null unless the plugin is the library's
  # active relay ingress.
  origin @12 :Origin;
  # `AI`: runs models other plugins serve; null unless `[ai]` names the
  # tasks the plugin calls and the operator granted them.
  ai @13 :AiRunner;
  # `WEBHOOKS`: the instance's inbound webhooks, their URLs and the
  # deliveries the host stored; null unless `[[webhooks]]` is declared and
  # granted.
  webhooks @14 :Webhooks;
}

# The network grant in effect for one `PluginWorker.open`. Read-only
# feedback: the host enforces the grant whatever the guest does with this.
struct NetworkGrant {
  # True when the grant allows outbound connections; false for `deny`.
  outbound @0 :Bool;
  # Fetch destinations in effect, as `plugin.toml` `domains` entries
  # (`host`, `*.host`, `host:port`, or `*` for any public host), including
  # approved setting destinations; at most `maxNetworkGrantEntries`.
  domains @1 :List(Text);
  # Raw TCP destinations in effect, as `host:port` (IPv6 in brackets), one
  # per granted port; at most `maxNetworkGrantEntries`.
  tcp @2 :List(Text);
  # One entry per `[[settings]]` entry that declares `network`, in
  # declaration order; at most `maxNetworkGrantEntries`.
  settings @3 :List(SettingNetworkGrant);
}

# How a setting's destination is reached.
enum SettingNetworkKind {
  # HTTP requests through `fetch`.
  fetch @0;
  # A raw TCP socket (`connect()` or the native socket proxy).
  tcp @1;
}

# Where a network setting's destination stands.
enum SettingGrantState {
  # The operator approved the destination the current value names.
  approved @0;
  # The current value names a destination the operator has not approved
  # yet; connections to it are refused with `networkNotGranted`.
  pending @1;
  # The setting is empty and names no destination.
  unset @2;
  # The value names no valid destination; see `SettingNetworkGrant.error`.
  invalid @3;
}

# One network setting and the destination its value names.
struct SettingNetworkGrant {
  # The setting key.
  key @0 :Text;
  # Whether the plugin fetches the destination or opens a socket to it.
  network @1 :SettingNetworkKind;
  # The destination as the consent form shows it: `scheme://host:port` for
  # a fetch, `host:port` for TCP (IPv6 in brackets); empty when `state` is
  # `unset` or `invalid`.
  destination @2 :Text $optional;
  # Where the destination stands.
  state @3 :SettingGrantState;
  # Why the value names no destination, when `state` is `invalid`.
  error @4 :Text $optional;
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
  # `[[queues.consumers]]` trigger: `queue(batch)` handler.
  queueConsumer @8 :QueueConsumer;
  # `[triggers] crons` trigger: `scheduled(controller)` handler.
  scheduledHandler @9 :ScheduledHandler;
  # `metadata` entrypoint.
  metadata @10 :MetadataProvider;
  # `covers` entrypoint.
  covers @11 :CoverFetcher;
  # `[sockets]` handler: messages and closes on host-brokered sockets.
  socketHandler @12 :SocketHandler;
  # `metadataFiles` entrypoint.
  metadataFiles @13 :MetadataFiles;
  # `notifier` entrypoint.
  notifier @14 :Notifier;
  # `ingress` entrypoint.
  ingress @15 :Ingress;
  # `ai` entrypoint.
  ai @16 :AiModels;
  # `[[webhooks]]` handler: called after the host stores deliveries.
  webhookHandler @17 :WebhookHandler;
  # `provisioning` entrypoint: guided setup.
  provisioning @18 :Provisioning;
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

# Request of `EventPublisher.read`: one page of past outbox events, oldest
# first. Only types the instance's consented `[[events.consumers]]` rows
# subscribe to are returned, through the same schema-version and filter match
# as delivery, and only back to `[events].retention_days`.
struct EventReadRequest {
  # Opaque cursor from an earlier `EventReadPage.cursor`; empty starts at
  # `sinceUnixMs`. At most `maxEventCursorBytes`. Set at most one of `after`
  # and `sinceUnixMs`.
  after @0 :Text;
  # UTC Unix milliseconds to start from when `after` is empty; zero starts at
  # the oldest retained event.
  sinceUnixMs @1 :UInt64;
  # Event types to return; empty means every consented subscription. Each
  # entry must be a consented subscription type; at most `maxListPage`.
  types @2 :List(Text);
  # Most events to return; zero means `maxListPage`. At most `maxListPage`.
  limit @3 :UInt32;
}

# Success payload of `EventPublisher.read`.
struct EventReadPage {
  # Matching events in outbox order. `deliveryAttempt` is zero and
  # `deduplicationKey` is the event id: a read is not a delivery, so dedupe
  # on `eventId`.
  events @0 :List(DomainEvent);
  # Opaque position after the last event scanned; pass it as `after` to
  # continue. Empty only when nothing was scanned and the request had no
  # cursor. At most `maxEventCursorBytes`.
  cursor @1 :Text;
  # True when more events may follow: the page filled or the host's scan
  # budget ran out before the newest event.
  more @2 :Bool;
  # Ids of the events on this page whose delivery to this plugin expired
  # unleased: the events the instance missed.
  expiredEventIds @3 :List(Text);
  # True when the start position is older than `[events].retention_days`, so
  # events before the retained window may be gone.
  truncated @4 :Bool;
}

# Result union of `EventPublisher.read`.
struct EventReadReply {
  union {
    # Success: one page.
    ok @0 :EventReadPage;
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

# Result union of `Destination.move`.
struct MoveReply {
  union {
    # Success: how the move was done.
    ok @0 :MoveResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.moveMany`.
struct MoveManyReply {
  union {
    # Success: per-key outcomes and the continuation.
    ok @0 :MoveManyPage;
    # Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.deleteMany`.
struct DeleteManyReply {
  union {
    # Success: per-key outcomes.
    ok @0 :DeleteManyResult;
    # Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.headMany`.
struct HeadManyReply {
  union {
    # Success: one probe per key.
    ok @0 :HeadManyResult;
    # Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.translateKeys`.
struct TranslateKeysReply {
  union {
    # Success: one translated key per host key.
    ok @0 :TranslateKeysResult;
    # Typed failure of the whole call; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `Destination.changes`.
struct ChangesReply {
  union {
    # Success: one page of changes.
    ok @0 :ChangesPage;
    # Typed failure; `invalid_cursor` means the cursor expired and the host
    # starts over with a full listing.
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

# Success payload of `Origin.fetch`: the library's response head and its
# streamed body.
struct OriginResponse {
  # HTTP status code, 100 to 599.
  status @0 :UInt16;
  # Response header fields in order, at most `maxOriginHeaders` and
  # `maxOriginHeaderBytes`; repeated names (`set-cookie`) stay separate.
  headers @1 :List(OriginHeader);
  # Response body; read to `done`. Null when the response has no body.
  body @2 :ByteSource;
}

# Result union of `Origin.fetch`.
struct OriginFetchReply {
  union {
    # Success: the library answered (any HTTP status).
    ok @0 :OriginResponse;
    # Typed failure: the request was refused before the library saw it
    # (`invalid_params`) or the library is unavailable.
    err @1 :PluginError;
  }
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
  # Move one object (requires `storage.move`). Repeating a finished move
  # succeeds with `alreadyMoved`.
  move @8 (
      from :Text,  # Source object key.
      to :Text,  # Target object key.
      options :MoveOptions  # Overwrite and source preconditions.
  ) -> (result :MoveReply);
  # Move a set of objects or a whole prefix (requires `storage.moveMany`).
  # Paged and safe to repeat.
  moveMany @9 (
      request :MoveManyRequest  # Pairs or prefix, cursor, and options.
  ) -> (result :MoveManyReply);
  # Remove up to `maxBatchKeys` objects (requires `storage.deleteMany`).
  deleteMany @10 (
      keys :List(Text)  # Object keys to remove.
  ) -> (result :DeleteManyReply);
  # Probe up to `maxBatchKeys` objects (requires `storage.headMany`).
  headMany @11 (
      keys :List(Text)  # Object keys to probe.
  ) -> (result :HeadManyReply);
  # Keys changed since a cursor (requires `storage.changes`). A hint for
  # what to read; a scheduled listing stays the backstop.
  changes @12 (
      request :ChangesRequest  # Cursor and page size.
  ) -> (result :ChangesReply);
  # Translate up to `maxBatchKeys` host keys into the keys the guest stores
  # them under (requires `storage.translateKeys`). A host key is `/`-joined
  # segments, none empty, `.`, `..` or holding `\` or a control character.
  # The same host key must always translate to the same key. Without the
  # feature the host stores each host key as written.
  translateKeys @13 (
      keys :List(Text)  # Host keys to translate.
  ) -> (result :TranslateKeysReply);
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
  # Read one page of past outbox events this instance subscribes to, oldest
  # first, to catch up after a gap (`env.EVENTS.read`).
  read @1 (
      request :EventReadRequest  # Where to start, which types, how many.
  ) -> (result :EventReadReply);
}

# One named plugin-owned database binding delivered on `Bindings.databases`.
struct NamedDatabase {
  # Binding name from `plugin.toml` `[[databases]]`.
  name @0 :Text;
  # Isolated typed SQL session for this binding (plugin-owned schema).
  database @1 :GuestDatabase;
}

# `[triggers] crons` handler (`Entrypoints.scheduledHandler`), the Workers
# `scheduled(controller)` trigger. Each fire is delivered at least once;
# a fire that throws is retried with backoff, keeping `scheduledTime` and
# `fireId`, so handlers dedupe on `fireId`.
interface ScheduledHandler {
  # Run one cron fire to completion.
  scheduled @0 (
      controller :ScheduledController  # The fire being delivered.
  ) -> (result :EmptyReply);
}

# `[[queues.consumers]]` handler (`Entrypoints.queueConsumer`). Delivery is
# at-least-once; consumers must be idempotent on `QueueMessage.id`.
interface QueueConsumer {
  # Deliver one ordered batch of messages from one queue.
  queue @0 (
      batch :QueueBatch  # Messages in delivery order.
  ) -> (result :QueueBatchReply);
}

# Queue producer binding: host-served send into the queue a
# `[[queues.producers]]` row names, granted through `Bindings.queues`.
interface QueueProducer {
  # Send one message.
  send @0 (
      message :QueueSendMessage  # Body, encoding, and delay.
  ) -> (result :QueueSendReply);
  # Send up to `maxListPage` messages in order; `delaySeconds` applies to
  # every message that carries no delay of its own.
  sendBatch @1 (
      messages :List(QueueSendMessage),  # Messages in send order.
      delaySeconds :UInt32  # Default delay for the batch.
  ) -> (result :QueueSendReply);
}

# One named queue producer delivered on `Bindings.queues`.
struct NamedQueue {
  # Binding name from `plugin.toml` `[[queues.producers]]`.
  name @0 :Text;
  # Queue the binding sends to.
  queue @1 :Text;
  # Host-served producer for that queue.
  producer @2 :QueueProducer;
}

# Durable key/value namespace: a Workers `KVNamespace`-shaped binding the host
# serves from the library database, scoped to the plugin and the namespace a
# `[[kv_namespaces]]` row names, granted through `Bindings.kv`. Values live in
# the library, so an instance moved to another host reads its own keys.
interface KvNamespace {
  # Read one key; `found = false` when absent or expired.
  get @0 (
      key :Text  # Key, at most `maxKvKeyBytes` UTF-8 bytes.
  ) -> (result :KvGetReply);
  # Write one key, replacing any previous value and metadata.
  put @1 (
      key :Text,  # Key, at most `maxKvKeyBytes` UTF-8 bytes.
      value :Data,  # Value, at most the namespace's `max_value_bytes`.
      options :KvPutOptions  # Expiration and metadata.
  ) -> (result :KvPutReply);
  # Delete one key; a missing key is not an error.
  delete @2 (
      key :Text  # Key to delete.
  ) -> (result :KvDeleteReply);
  # List keys in lexical order, one page at a time.
  list @3 (
      options :KvListOptions  # Prefix, cursor, and page size.
  ) -> (result :KvListReply);
}

# One named durable namespace delivered on `Bindings.kv`.
struct NamedKv {
  # Binding name from `plugin.toml` `[[kv_namespaces]]`.
  name @0 :Text;
  # Namespace the binding is scoped to.
  namespace @1 :Text;
  # Host-served namespace.
  kv @2 :KvNamespace;
}

# Host-brokered outbound WebSockets, granted through `Bindings.sockets`.
#
# The guest asks for a socket; a broker the host runs for the plugin
# instance dials it under the plugin's network grant and keeps it open while
# the guest sleeps, in the manner of hibernatable Workers WebSockets. Each
# incoming message wakes the guest and arrives on
# `Entrypoints.socketHandler`. Heartbeat pairs declared at connect are
# answered by the broker without waking the guest.
interface Sockets {
  # Open a socket; it is owned by the plugin instance, not this invocation.
  connect @0 (
      request :SocketConnectRequest  # Target, subprotocols, headers, heartbeats.
  ) -> (result :SocketConnectReply);
  # Send one message on an open socket.
  send @1 (
      socket :Text,  # Socket id from `connect` or `list`.
      frame :SocketFrame  # Message, at most `maxSocketFrameBytes`.
  ) -> (result :EmptyReply);
  # Close a socket; closing an unknown or closed socket is not an error.
  close @2 (
      socket :Text,  # Socket id.
      code :UInt16,  # WebSocket close code; zero sends 1000.
      reason :Text  # Close reason, at most 123 UTF-8 bytes.
  ) -> (result :EmptyReply);
  # Replace the attachment the host keeps for a socket.
  setAttachment @3 (
      socket :Text,  # Socket id.
      attachment :Data  # Opaque bytes, at most `maxSocketAttachmentBytes`.
  ) -> (result :EmptyReply);
  # The instance's open sockets, for a guest that just woke.
  list @4 () -> (result :SocketListReply);
}

# Guest handler for host-brokered sockets (`Entrypoints.socketHandler`).
interface SocketHandler {
  # One message arrived on an open socket.
  message @0 (
      event :SocketMessage  # Socket, frame, and attachment.
  ) -> (result :SocketMessageReply);
  # A socket closed, by the remote side, the network, or the host.
  closed @1 (
      event :SocketClosed  # Socket, close code, and attachment.
  ) -> (result :EmptyReply);
}

# The library, as a relay `ingress` plugin reaches it (`Bindings.origin`,
# `ORIGIN` in the Workers idiom). The plugin turns each request its provider
# delivers into a `fetch` and streams the response back. The host treats
# every request as the `tunnel` transport: public `https`, never loopback,
# with the client address the plugin reports.
interface Origin {
  # Serve one request; the response body streams as the library writes it.
  fetch @0 (
      request :OriginRequest,  # Method, target, headers, client address.
      body :ByteSource  # Request body; null when the request has none.
  ) -> (result :OriginFetchReply);
  # Open a WebSocket to the library for one upgrade request the provider
  # delivers. An accepted upgrade becomes a socket in the instance's
  # `Bindings.sockets`: its messages arrive on `Entrypoints.socketHandler`
  # and the plugin answers with `Sockets.send` and `Sockets.close`, as on
  # any socket it dialed. Needs `[sockets]`. These sockets count toward
  # `maxOriginSocketsPerInstance`, not `maxSocketsPerInstance`.
  connect @1 (
      request :OriginRequest,  # A `GET` with `upgrade: websocket`.
      attachment :Data  # Initial attachment, at most `maxSocketAttachmentBytes`.
  ) -> (result :OriginConnectReply);
}

# A public address provider (`Entrypoints.ingress`): the one `ingress`
# family instance the library runs. A relay provider carries traffic to
# `Bindings.origin`; a direct provider keeps a DNS name pointing at this
# install.
interface Ingress {
  # Bring the public address up; answers the origin it serves.
  start @0 () -> (result :IngressStartReply);
  # Whether the public address is serving now.
  health @1 () -> (result :IngressHealthReply);
  # Take the public address down; stopping a stopped one is not an error.
  stop @2 () -> (result :EmptyReply);
}

# Guided setup (`Entrypoints.provisioning`). The host collects a
# short-lived grant from the operator (a pasted API token, or an OAuth access
# token), opens the instance with `Invocation.purpose = provision` and hands
# the grant over. The plugin creates what it needs at the provider and
# returns settings and narrowly scoped credentials; the host checks them
# against `[provisioning] outputs` and the pinned settings schema, writes the
# settings, seals the credentials and drops the grant.
interface Provisioning {
  # Create (or, when `CONFIG` holds earlier outputs, update or rotate) what
  # the instance needs.
  provision @0 (
      request :ProvisionRequest  # The grant and the operator's choices.
  ) -> (result :ProvisionReply);
  # Remove what `provision` created. Removing what is already gone is not an
  # error.
  deprovision @1 (
      request :DeprovisionRequest  # A grant, or none to use stored secrets.
  ) -> (result :EmptyReply);
}

# Host-served inbound webhooks, granted through `Bindings.webhooks`.
#
# The host serves each `[[webhooks]]` row at a secret URL under the
# library's public origin, checks every request against the row's body cap,
# rate limit and signature rule, stores it and answers at once. The guest
# takes stored deliveries with `pull` and settles them with `ack`, in the
# manner of a Cloudflare Queues HTTP pull consumer: a pulled delivery stays
# leased until acknowledged, retried, or its visibility timeout ends.
interface Webhooks {
  # The URL an outside service should call for one webhook.
  url @0 (
      name :Text  # `[[webhooks]]` name.
  ) -> (result :WebhookUrlReply);
  # Lease up to `batchSize` stored deliveries, oldest first.
  pull @1 (
      request :WebhookPullRequest  # Webhook, batch size, visibility timeout.
  ) -> (result :WebhookPullReply);
  # Acknowledge finished deliveries and offer retried ones again at once.
  ack @2 (
      request :WebhookAckRequest  # Webhook and lease ids.
  ) -> (result :EmptyReply);
}

# Guest handler for inbound webhooks (`Entrypoints.webhookHandler`).
interface WebhookHandler {
  # The host stored deliveries for one webhook; the guest pulls them.
  delivered @0 (
      event :WebhookDelivered  # Webhook and how many wait.
  ) -> (result :EmptyReply);
}

# Storefront content source (not byte Source). Every method takes and
# returns typed structs from the "Typed method payloads" section.
interface ContentSource {
  # Connect an account (password or one-shot OAuth).
  login @0 (
      params :LoginParams  # Credentials, marketplace, callback wiring.
  ) -> (result :LoginReply);
  # One page of one account's library scan pass (see `ScanPage`).
  scan @1 (
      params :ScanParams  # Account, cursor, sync token, credential.
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
  # Undo store-side state for one account before the host deletes its
  # stored credentials (for example, deregister the device registered at
  # login). Best effort: the host revokes the account whatever this returns.
  revokeAccount @13 (
      params :RevokeAccountParams  # Account id and its stored credentials.
  ) -> (result :EmptyReply);
  # Purchase links / price hints for several titles in one call, so a guest
  # whose backend prices many products per request makes one request for a
  # page of cards. A guest that cannot batch answers each query as
  # `purchaseHint` would.
  purchaseHints @14 (
      params :PurchaseHintsParams  # The titles to price, in order.
  ) -> (result :PurchaseHintsReply);
  # The audio quality one account would download for each of several
  # titles today, so the host can tell when a better file than the one it
  # holds is on offer. Asked only of guests that declare
  # `CatalogCapabilities.qualityOffers`; it must not license or download.
  qualityOffers @15 (
      params :QualityOffersParams  # Account, credentials, and title ids.
  ) -> (result :QualityOffersReply);
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

# `metadata` entrypoint: bibliographic lookups against a public catalog
# (OpenLibrary, Audnexus, ...). A provider describes what it finds as
# edition records; the host decides identity and never trusts a record to
# name an existing library row. Network access is limited to the domains the
# manifest's `[capabilities.network]` declares.
interface MetadataProvider {
  # Liveness / readiness probe.
  health @0 () -> (result :HealthReply);
  # Records for the editions the given identifiers name.
  lookup @1 (
      params :MetadataLookupParams  # Identifiers to resolve and a result cap.
  ) -> (result :MetadataRecordsReply);
  # Records matching a title and, optionally, an author.
  search @2 (
      params :MetadataSearchParams  # Title, author, language and a result cap.
  ) -> (result :MetadataRecordsReply);
  # Chapter layout of the edition the given identifiers name.
  chapters @3 (
      params :MetadataChaptersParams  # Identifiers of one edition.
  ) -> (result :MetadataChaptersReply);
}

# `covers` entrypoint: downloads cover images on the host's behalf, so the
# host itself never makes an outbound request to a URL a plugin or a store
# supplied. The fetcher only transports bytes: the host sniffs, decodes and
# re-encodes every image in its media worker before anything stores or
# serves it. Network access is limited to what the manifest's
# `[capabilities.network]` declares.
interface CoverFetcher {
  # Liveness / readiness probe.
  health @0 () -> (result :HealthReply);
  # Fetches every URL concurrently and streams each body into `sink` under
  # its index in `params.urls` (`"0"`, `"1"`, ...). The reply says, per URL,
  # whether its body was stored.
  fetchCovers @1 (
      params :CoverFetchParams,  # URLs and the per-image byte cap.
      sink :Destination  # Host-served store that accepts only those keys.
  ) -> (result :CoverImagesReply);
}

# `metadataFiles` entrypoint: reads and writes metadata files that sit next
# to an edition's media (sidecars such as `metadata.json`, OPF, NFO or CUE),
# one dialect per format. The plugin only converts bytes: the host reads the
# file, chooses every path and writes the result, so a dialect plugin needs
# no network or file grants. The manifest's `[metadata_files]` table names
# each dialect, the file names it reads and the fields it carries, so the
# host routes a file to a sleeping plugin without starting it.
interface MetadataFiles {
  # Liveness / readiness probe.
  health @0 () -> (result :HealthReply);
  # What one metadata file says about its edition.
  parse @1 (
      params :MetadataFileParseParams  # Dialect, file name and bytes.
  ) -> (result :MetadataFileParseReply);
  # The bytes of a metadata file with the given fields set to the record's
  # values and everything else kept from the existing file.
  render @2 (
      params :MetadataFileRenderParams  # Dialect, record, fields and existing bytes.
  ) -> (result :MetadataFileRenderReply);
}

# `notifier` entrypoint: delivers one host-rendered message to one
# destination (an email address, a chat webhook, a push subscription). The
# host decides what is worth a notification, who gets it and which instance
# carries it; the plugin only sends. Each send is one claimed `notify.send`
# job, so the host owns retries: a reply of `rate_limited`, `unavailable`,
# `deadline_exceeded` or `cancelled` is retried (`rate_limited` honouring
# `details.retryAfterMs`), any other error is a permanent failure for that
# destination.
interface Notifier {
  # Liveness / readiness probe: credentials and reachability.
  health @0 () -> (result :HealthReply);
  # Deliver one message to one destination.
  send @1 (
      params :NotifySendParams  # Message and destination.
  ) -> (result :NotifySendReply);
  # Normalise and check a personal destination's address before the host
  # saves it.
  validateDestination @2 (
      params :NotifyValidateParams  # Address document to check.
  ) -> (result :NotifyValidateReply);
  # Operator's deliverability check: send a sample the way real messages go,
  # ask checker services how it scores, and merge their findings into
  # advice. Only plugins that declare `[notifier].deliverability_check`
  # implement it (others answer `unsupported`); it may run up to
  # `deliverabilityCheckDeadlineMs`.
  checkDeliverability @3 (
      params :NotifyDeliverabilityParams  # Sample message to check.
  ) -> (result :NotifyDeliverabilityReply);
}

# `NOTIFY` binding: raises a notification type the plugin declared in
# `[[notifications.types]]`. The host router applies the administrators'
# routing and each patron's preferences; the raising plugin never learns
# where the notification went.
interface NotificationRaiser {
  # Raise one notification.
  raise @0 (
      params :NotifyRaiseParams  # Declared type, fields and optional patron.
  ) -> (result :NotifyRaiseReply);
}

# `ai` entrypoint: serves models. The plugin lists the models it serves,
# each with one task, and runs one model on one input. The host routes
# every call (its own and other plugins' through the `AI` binding) to the
# deployed plugin that serves the named model; a model plugin never learns
# who asked. Bulk work arrives as many `run` calls from a host job, each
# within the per-call caps.
interface AiModels {
  # Liveness / readiness probe: model files present and loadable.
  health @0 () -> (result :HealthReply);
  # The models this plugin serves right now, under its current settings.
  models @1 () -> (result :AiModelsReply);
  # Run one model on one input.
  run @2 (
      params :AiRunParams  # Model name and the task's input.
  ) -> (result :AiRunReply);
}

# `AI` binding: runs a model by name, as `env.AI.run(model, input)` does on
# Cloudflare Workers. The host checks the caller's `[ai]` grant for the
# model's task, applies the call's deadline and the caller's budget, and
# forwards to the serving plugin's `AiModels.run`.
interface AiRunner {
  # Run one model on one input.
  run @0 (
      params :AiRunParams  # Model name or task alias, and the task's input.
  ) -> (result :AiRunReply);
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
  # A negotiated database limit was exceeded (`maxResultBytes`, `maxResultRows`,
  # `maxRequestBytes`, ...). `details` names the limit, its cap and the value
  # that exceeded it.
  limitExceeded @12;
  # The backend asked the guest to slow down (HTTP 429 or its equivalent).
  # `details.retryAfterMs` says how long the host should leave this
  # backend alone, when the backend said so.
  rateLimited @13;
  # The network grant does not cover the destination. `details.destination`
  # names it, and `details.setting` names the network setting whose value
  # names it, when one does (its destination awaits the operator's approval).
  networkNotGranted @14;
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
  # Metadata provider: `lookup`, `search` and `chapters`
  # (`MetadataProvider`).
  metadata @6;
  # Cover fetcher: `fetchCovers` (`CoverFetcher`).
  covers @7;
  # Metadata file dialects: `parse` and `render` (`MetadataFiles`).
  metadataFiles @8;
  # Notification delivery: `send` and `validateDestination` (`Notifier`).
  notifier @9;
  # Public address provider: `start`, `health` and `stop` (`Ingress`).
  ingress @10;
  # Model serving: `models` and `run` (`AiModels`).
  ai @11;
  # Guided setup: `provision` and `deprovision` (`Provisioning`).
  provisioning @12;
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
  # `WORK_FS`, `OAUTH`, `KV`, `EVENTS`, `[[queues.producers]]` names, ...).
  bindings @5 :List(Text);
  # Queues the default entrypoint consumes (`queue(batch)` trigger).
  queueConsumers @6 :List(Text);
  # Queues the guest may send to through its `[[queues.producers]]` bindings.
  queueProducers @7 :List(Text);
  # Five-field UTC cron schedules the default entrypoint handles
  # (`scheduled(controller)` trigger, `[triggers] crons`), normalized to
  # single spaces.
  crons @8 :List(Text);
  # True when the guest declared `[sockets]`: it receives the host-brokered
  # `Sockets` binding and may export a `SocketHandler` for pushed messages.
  sockets @9 :Bool;
  # True when the guest declared `[[webhooks]]`: it receives the host-served
  # `Webhooks` binding and may export a `WebhookHandler` the host wakes when
  # it stores deliveries.
  webhooks @10 :Bool;
}

# Catalog behaviours a storefront declares in `describe()`. Catalog search
# reads them instead of special-casing plugin ids; every flag defaults to
# false, so a guest that says nothing keeps the plain behaviour.
struct CatalogCapabilities {
  # True when `catalogDetail` resolves a bare ISBN (sent in both `productId`
  # and `isbn`) to a full bibliographic record. Catalog search then asks this
  # store to fill description, narrators, runtime and genres on lean result
  # rows from any store that carry an ISBN.
  detailByIsbn @0 :Bool;
  # True when `purchaseHint` / `purchaseHints` price a query by its exact
  # ISBN (`lookup = isbn`): the answer is the store's listing of that ISBN
  # or not found, never a title match. The host then prices a card at this
  # store by the ISBNs of the card's recording before searching by title.
  priceByIsbn @1 :Bool;
  # True when `purchaseHint` / `purchaseHints` price a query by its exact
  # ASIN (`lookup = asin`), as `priceByIsbn` does for ISBNs.
  priceByAsin @2 :Bool;
  # True when `catalogDetail` resolves this store's own `productId` (an id
  # its search, expand or deals hits gave) to a full bibliographic record.
  # Catalog search then asks this store to fill lean result rows it listed
  # itself, and title dialogs ask it for the editions it listed. A store
  # that leaves it false is never woken for a detail lookup by product id.
  detailByProductId @3 :Bool;
  # True when `qualityOffers` answers which audio quality an account would
  # download for a title. The host's quality-upgrade check asks only stores
  # that declare it.
  qualityOffers @4 :Bool;
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

# Params of `ContentSource.scan`: one page of one account's scan pass. The
# host injects that account's sealed credentials so the plugin does not need a
# private credential store under `pluginDataDir`, and drives the pass page by
# page: the first call of a pass sends an empty `cursor`, every later call
# echoes the previous page's `nextCursor`.
struct ScanParams {
  # Scoped plugin data directory.
  pluginDataDir @0 :Text;
  # Account this pass scans; always one of the plugin's connected accounts.
  accountId @1 :Text;
  # Storefront page size; the host always sends an explicit value.
  pageSize @2 :UInt32;
  # When true, import podcast/episode-style rows.
  importEpisodes @3 :Bool;
  # When true, import Plus/catalog entitlement titles.
  importPlusTitles @4 :Bool;
  # Host-loaded credential blob for `accountId`.
  credential @5 :AccountCredential;
  # Opaque position the guest returned as `ScanPage.nextCursor`; empty on the
  # first page of a pass. At most `maxScanCursorBytes`.
  cursor @6 :Text;
  # Opaque token the guest returned as `ScanPage.syncToken` at the end of the
  # account's last pass; empty asks for a full pass. At most
  # `maxScanCursorBytes`.
  syncToken @7 :Text;
}

# What family of creative work a record describes (FRBR work). Every record
# names one: `unspecified` is refused by the host, so a guest that forgets
# the field fails instead of silently describing a book. Whether a podcast
# work is a show or an episode is structural, not a kind: an episode names
# its show through `parentProductId`, a show names no parent.
enum WorkKind {
  # Not set; the host refuses the record.
  unspecified @0;
  # A book: fiction or non-fiction, in any expression (read, narrated,
  # dramatised, illustrated) and on any carrier.
  book @1;
  # A podcast: the show, or one episode of it when the record names a
  # parent.
  podcast @2;
}

# How a work is realised (FRBR expression): the kind of content you get,
# independent of the carrier. A translation is not a kind: the expression's
# language and its translator credits say it. `unspecified` is refused.
enum ExpressionKind {
  # Not set; the host refuses the record.
  unspecified @0;
  # The written text, as sold in print and ebook editions.
  text @1;
  # A reading of the text by one or more narrators.
  narration @2;
  # A full-cast dramatisation or performance of the work.
  dramatisation @3;
  # Sequential art: comics, graphic novels, manga.
  graphic @4;
  # Audio that is itself the first realisation rather than a reading of a
  # text: podcast episodes, lectures, single-voice originals.
  originalAudio @5;
}

# Carrier of an edition (FRBR manifestation): what form you receive.
# Whether the host acquires an edition depends on the storefront's
# `MediaSupport.fetches` and what the host supports, not on this value
# alone. `unspecified` is refused.
enum MediaKind {
  # Not set; the host refuses the record.
  unspecified @0;
  # Downloadable audio: audiobooks and podcast episodes.
  audio @1;
  # A downloadable document: EPUB, PDF, CBZ and similar.
  ebook @2;
  # A printed edition.
  print @3;
}

# Media a plugin works with, declared in `describe()` and in the manifest's
# `[media]` table. Required for plugins that export the storefront or
# metadata entrypoint: the host refuses to spawn one whose `workKinds` or
# `lists` is empty, holds `unspecified`, or is wider than its manifest, and
# refuses any record whose kinds fall outside the declaration.
struct MediaSupport {
  # Work kinds the plugin reports.
  workKinds @0 :List(WorkKind);
  # Edition carriers the plugin reports in scans, catalog search and
  # metadata records.
  lists @1 :List(MediaKind);
  # Edition carriers `ContentSource.fetchTitle` can deliver; a subset of
  # `lists`. Empty for metadata providers and for stores that only list.
  fetches @2 :List(MediaKind);
}

# Scheme of an external identifier. The scheme fixes whether the id names a
# work or an edition, and whether it is strong (unique) or weak (shared).
enum IdentifierScheme {
  # ISBN-10 or ISBN-13 of an edition; weak, because one ISBN can sit on
  # several editions. The host normalises it to ISBN-13.
  isbn @0;
  # Amazon ASIN of an edition in one marketplace; strong.
  asin @1;
  # OpenLibrary edition id (`OL…M`); strong.
  openLibraryEdition @2;
  # OpenLibrary work id (`OL…W`); names the work, not an edition.
  openLibraryWork @3;
}

# One external identifier a storefront or metadata provider reports.
struct Identifier {
  # Scheme the value belongs to; it decides the identifier's level.
  scheme @0 :IdentifierScheme;
  # The identifier as the source printed it; the host trims and normalises.
  value @1 :Text;
  # Marketplace the id is scoped to (ASIN); absent for global schemes.
  marketplace @2 :Text $optional;
}

# A contributor's responsibility for a work, expression or edition.
enum CreditRole {
  # Wrote the work.
  author @0;
  # Reads the text in a narration.
  narrator @1;
  # Translated the text.
  translator @2;
  # Edited the work or the edition.
  editor @3;
  # Illustrated the work or the edition.
  illustrator @4;
  # Published the edition.
  publisher @5;
  # Hosts a podcast show or episode.
  host @6;
  # Performs a part in a dramatisation.
  cast @7;
  # Adapted the work into a dramatisation.
  adaptor @8;
}

# Which level a credit attaches to. Ordinal 0 is the role's usual level, so
# a guest sets this only when a credit departs from it.
enum CreditLevel {
  # The role's usual level: author and adaptor on the work; narrator,
  # translator, host and cast on the expression; editor, illustrator and
  # publisher on the edition.
  roleDefault @0;
  # The work.
  work @1;
  # The expression.
  expression @2;
  # The edition.
  edition @3;
}

# One credited contributor. Credits are listed in billing order.
struct Credit {
  # Contributor display name as the source prints it, one person or body.
  name @0 :Text;
  # What the contributor did.
  role @1 :CreditRole;
  # Level the credit attaches to.
  level @2 :CreditLevel;
}

# A work's place in one series.
struct SeriesEntry {
  # Series name as the source prints it.
  name @0 :Text;
  # Position label as the source prints it (`1`, `2.5`, `1-3`).
  position @1 :Text $optional;
  # The store's own id for the series (an Audible series ASIN) when known.
  storeSeriesId @2 :Text $optional;
}

# One library title returned by `ContentSource.scan`. The host forces `source`
# to the plugin id and, until the library stores works, expressions and
# editions, maps the record onto one flat library row.
struct ScanBook {
  # Account that owns this library entry.
  accountId @0 :Text;
  # Storefront product / SKU id.
  productId @1 :Text;
  # Primary title string.
  title @2 :Text;
  # Marketplace / region when known.
  marketplace @3 :Text $optional;
  # Publisher name when known.
  publisher @4 :Text $optional;
  # Runtime in whole minutes when known.
  lengthMinutes @5 :Int64 $optional;
  # Subtitle when distinct from `title`.
  subtitle @6 :Text $optional;
  # Kind of work the title is.
  workKind @7 :WorkKind;
  # How the work is realised in this edition.
  expressionKind @8 :ExpressionKind;
  # Carrier of this edition.
  mediaKind @9 :MediaKind;
  # External identifiers (ISBN, ASIN, OpenLibrary ids).
  identifiers @10 :List(Identifier);
  # Credited contributors in billing order.
  credits @11 :List(Credit);
  # Series the work belongs to.
  seriesEntries @12 :List(SeriesEntry);
  # Product id of the parent podcast show, for a podcast episode.
  parentProductId @13 :Text $optional;
  # Content language code when known.
  language @14 :Text $optional;
  # Whether the edition is abridged when the storefront says so.
  abridgement @15 :Abridgement;
  # Publication date as RFC 3339 when known.
  publishedAt @16 :Text $optional;
  # When the account bought or added the title, as RFC 3339, when known.
  purchasedAt @17 :Text $optional;
  # Category / genre labels as a single string when known.
  categories @18 :Text $optional;
  # Whether the storefront marks the edition explicit (adult) content.
  explicitness @19 :Explicitness;
}

# One page of an account's scan pass, the result of `ContentSource.scan`.
# A pass is the run of pages from an empty `cursor` to the page whose
# `nextCursor` is empty; `syncToken` and `complete` are read from that last
# page only. A guest that does not page answers the first call with every
# title, an empty `nextCursor` and `complete = true`.
struct ScanPage {
  # Titles for the host to upsert; at most `maxScanPageBooks`.
  books @0 :List(ScanBook);
  # Product ids the storefront reports as gone from the account since the
  # pass's `syncToken` (delta passes only); at most `maxScanPageBooks`.
  removed @1 :List(Text);
  # Opaque position of the next page; empty ends the pass. At most
  # `maxScanCursorBytes`; never equal to the cursor this page answered.
  nextCursor @2 :Text;
  # On the last page: the opaque token the host sends as
  # `ScanParams.syncToken` on the account's next pass; empty means the next
  # pass is a full one. At most `maxScanCursorBytes`.
  syncToken @3 :Text;
  # On the last page: true when the pass listed every title the account
  # holds (a full pass), false for a delta pass that listed only changes.
  complete @4 :Bool;
  # Storefront requests this page took.
  storePages @5 :UInt32;
  # The account's credentials as the guest holds them after refreshing them
  # while producing this page (for example a renewed access token): the
  # same encoding login returned, a JSON object of at most
  # `maxRefreshedCredentialBytes`. The host stores them in place of the
  # credentials it sent, unless the account was signed in again meanwhile,
  # and sends them on the pass's later pages. Empty when nothing changed.
  refreshedCredentials @6 :Data $optional;
}

# Result union of `ContentSource.scan`.
struct ScanReply {
  union {
    # Success: one page of the pass.
    ok @0 :ScanPage;
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

# What one fetched file is to the edition it belongs to.
enum FetchedFileRole {
  # Not set; the host refuses a file without a role.
  unspecified @0;
  # One ordered piece of the edition (an audio part or chapter file, one
  # volume of a set). The host joins parts in list order.
  part @1;
  # The whole edition in one file (an assembled M4B, an EPUB, a PDF).
  whole @2;
  # The edition's cover image.
  cover @3;
}

# One file `fetchTitle` wrote under the cache directory.
struct FetchedFile {
  # Absolute path to the file under `cacheDir`.
  path @0 :Text;
  # What the file is to the edition.
  role @1 :FetchedFileRole;
  # MIME type when the guest knows it (`audio/mp4`, `application/epub+zip`,
  # `image/jpeg`); the host sniffs the file either way.
  contentType @2 :Text $optional;
  # Part title (disc or chapter label), for a `part`.
  title @3 :Text $optional;
  # Playing time of the file in milliseconds, for audio, when known.
  durationMs @4 :UInt64 $optional;
}

# What a storefront's chapter markers mark (`PlainFetch.chapterKind`).
enum ChapterKind {
  # The storefront does not say.
  unknown @0;
  # Literary chapters: what a listener would call the book's chapters.
  chapters @1;
  # Track or part boundaries: where the store cut its files, which need
  # not fall on a chapter.
  tracks @2;
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
  # Carrier of what was fetched. Must be one of the storefront's declared
  # `MediaSupport.fetches`; `unspecified` is refused.
  mediaKind @0 :MediaKind;
  # The files the guest wrote under the cache directory: the edition's
  # ordered `part`s or one `whole` file, plus an optional `cover`.
  files @1 :List(FetchedFile);
  # Chapter markers; empty when unknown.
  chapters @2 :List(ChapterMarker);
  # Companion PDF download URL when the store exposes one; the host
  # downloads it itself.
  pdfUrl @3 :Text $optional;
  # The account's credentials as the guest holds them after refreshing them
  # during the fetch, as in `ScanPage.refreshedCredentials`. Empty when
  # nothing changed.
  refreshedCredentials @4 :Data $optional;
  # What `chapters` marks. The host ranks literary chapters above track
  # boundaries when it decides an edition's chapters.
  chapterKind @5 :ChapterKind;
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
  # Maximum hits to return; 0 means the storefront default (20 hits).
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
  # How the host asks the store to find the title. `unspecified` (an older
  # host) leaves the guest free to use any field, as before.
  lookup @7 :PriceLookup;
}

# How a purchase-hint query names the title to price.
enum PriceLookup {
  # Any field may be used, strongest first (older hosts).
  unspecified @0;
  # `productId` names the store's own product; answer for it alone.
  product @1;
  # `isbn` names the edition; answer for that exact ISBN or not found.
  # Sent only to a store that declared `CatalogCapabilities.priceByIsbn`.
  isbn @2;
  # `asin` names the edition; answer for that exact ASIN or not found.
  # Sent only to a store that declared `CatalogCapabilities.priceByAsin`.
  asin @3;
  # Search by `title` and `authors`, and return every priced listing the
  # search matched in `PurchaseHintResult.records`, so the host can place
  # each one in the library model and keep only the recording it asked for.
  title @4;
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

# Params of `ContentSource.revokeAccount`. The host sends the credentials it
# is about to delete so the guest can sign the device out at the store.
struct RevokeAccountParams {
  # Account being revoked.
  accountId @0 :Text;
  # Decrypted credential blob for this account, exactly as login returned
  # it; never log it. Empty when none is stored.
  credentials @1 :Data $optional;
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

# Whether an edition is marked as explicit (adult) content. A patron scope
# can hide explicit editions; `unknown` and `clean` editions stay visible.
enum Explicitness {
  # The storefront or file did not say.
  unknown @0;
  # Marked as not explicit.
  clean @1;
  # Marked as explicit (adult) content.
  explicit @2;
}

# One catalog / candidate hit returned by `searchCatalog`,
# `expandCandidates`, `listDeals`, and `catalogDetail`.
struct CatalogHit {
  # Storefront product / SKU id.
  productId @0 :Text;
  # Primary title.
  title @1 :Text;
  # Storefront product page URL.
  url @2 :Text $optional;
  # Cover image URL.
  coverUrl @3 :Text $optional;
  # Hit origin label (plugin id or storefront name).
  origin @4 :Text;
  # Subtitle when distinct from `title`.
  subtitle @5 :Text $optional;
  # Long description / blurb when fetched.
  description @6 :Text $optional;
  # Publisher name when known.
  publisher @7 :Text $optional;
  # Runtime in whole minutes.
  lengthMinutes @8 :Int64 $optional;
  # Publication date string as provided by the storefront.
  publishedAt @9 :Text $optional;
  # Category / genre labels as a single string when known.
  categories @10 :Text $optional;
  # Content language code when known.
  language @11 :Text $optional;
  # Current price in minor units (cents).
  priceCents @12 :Int64 $optional;
  # ISO currency code for `priceCents`.
  currency @13 :Text $optional;
  # Pre-formatted price for display.
  priceLabel @14 :Text $optional;
  # Aggregate rating when known.
  ratingOverall @15 :Float64 $optional;
  # Number of ratings when known.
  ratingCount @16 :Int64 $optional;
  # Whether the edition is abridged when the storefront says so.
  abridgement @17 :Abridgement;
  # Kind of work the hit is.
  workKind @18 :WorkKind;
  # How the work is realised in this edition.
  expressionKind @19 :ExpressionKind;
  # Carrier of this edition.
  mediaKind @20 :MediaKind;
  # External identifiers (ISBN, ASIN, OpenLibrary ids).
  identifiers @21 :List(Identifier);
  # Credited contributors in billing order.
  credits @22 :List(Credit);
  # Series the work belongs to.
  seriesEntries @23 :List(SeriesEntry);
  # Product id of the parent podcast show, for a podcast episode.
  parentProductId @24 :Text $optional;
  # The storefront's own rank for this hit in `searchCatalog` results:
  # 1-based position in the store's ordering for the requested sort across
  # all pages (page 2 of 20-hit pages starts at 21), or a store-wide
  # popularity rank when the store has one. 0 means unranked. Catalog search
  # merges stores by it for relevance and popularity sorts and puts unranked
  # hits after ranked ones. Other catalog methods leave it 0.
  rankHint @25 :UInt32 $optional;
  # Whether the storefront marks the edition explicit (adult) content.
  explicitness @26 :Explicitness;
  # Absolute `https://` URL of a short audio sample of this edition that
  # the storefront offers publicly (a few minutes, in any format the media
  # worker decodes). The host never hands it to a client: a book's details
  # play it through a same-origin path, fetched by a `covers` plugin with
  # `media = audio` and re-encoded in the media worker. Empty when the store
  # has none.
  sampleUrl @27 :Text $optional;
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
  # Every priced listing a title search matched (`lookup = title`), each
  # with its full catalog record, so the host can tell recordings apart by
  # identifiers, credits and kinds rather than by title. Empty for other
  # lookups and for guests that do not return records; `hint` then stays
  # the answer.
  records @2 :List(PricedRecord);
}

# One priced listing returned by a title search.
struct PricedRecord {
  # The listing's catalog record: identifiers, credits, kinds, language.
  record @0 :CatalogHit;
  # Its purchase link and prices, as `PurchaseHint` carries them.
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

# Params of `ContentSource.purchaseHints`: the titles to price together.
struct PurchaseHintsParams {
  # One query per title, each as for `purchaseHint`; at most `maxListPage`.
  queries @0 :List(PurchaseHintParams);
}

# Success payload of `ContentSource.purchaseHints`.
struct PurchaseHintsResult {
  # One answer per query, in the order the queries were sent.
  results @0 :List(PurchaseHintResult);
}

# Result union of `ContentSource.purchaseHints`. A whole-batch `err` (for
# example `rateLimited`) applies to every query in it.
struct PurchaseHintsReply {
  union {
    # Success: an answer per query.
    ok @0 :PurchaseHintsResult;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Audio codec of a stream a store offers (`QualityOffer.codec`). Spelled as
# the host's probed codecs are, so an offer compares with a held file.
enum OfferedCodec {
  # The store did not say; the host never upgrades to an unknown codec.
  unknown @0;
  # MPEG-4 AAC, lossy.
  aac @1;
  # Apple Lossless.
  alac @2;
  # FLAC, lossless.
  flac @3;
  # MPEG audio layer III (or I / II), lossy.
  mp3 @4;
  # Opus, lossy.
  opus @5;
  # Vorbis, lossy.
  vorbis @6;
  # Dolby Digital Plus (E-AC-3), lossy.
  eac3 @7;
  # Dolby AC-4, lossy.
  ac4 @8;
}

# AAC profile of an offered AAC stream (`QualityOffer.aacProfile`).
enum OfferedAacProfile {
  # Not AAC, or the store did not say.
  unknown @0;
  # AAC-LC.
  lc @1;
  # HE-AAC (AAC-LC plus SBR).
  he @2;
  # HE-AAC v2 (SBR plus parametric stereo).
  heV2 @3;
  # xHE-AAC (USAC).
  xhe @4;
}

# The stream one account would download for a title today.
struct QualityOffer {
  # Codec of the offered stream.
  codec @0 :OfferedCodec;
  # AAC profile when `codec` is `aac`; `unknown` otherwise.
  aacProfile @1 :OfferedAacProfile;
  # Average bitrate in kbit/s; zero when unknown.
  bitrateKbps @2 :UInt32 $optional;
  # Sample rate in Hz; zero when unknown.
  sampleRateHz @3 :UInt32 $optional;
  # Channel count; zero when unknown.
  channels @4 :UInt32 $optional;
}

# Params of `ContentSource.qualityOffers`: the titles one account holds.
struct QualityOffersParams {
  # Account whose download would be made.
  accountId @0 :Text;
  # Decrypted credential blob for this account; never log it. Empty when
  # none is stored.
  credentials @1 :Data $optional;
  # The instance's settings as `application/json` (bitrate tier and the
  # like, which change what a download gets).
  sourceConfig @2 :ExtensibleConfig;
  # Store product ids, as the account's scan listed them; at most
  # `maxListPage`.
  titleIds @3 :List(Text);
  # The fetch options a download would carry (DRM path, xHE-AAC), so the
  # answer names what that download would get.
  fetch @4 :FetchOptions;
}

# One title's answer in `QualityOffersResult`.
struct TitleQualityOffer {
  # False when the store could not say for this title (`offer` is empty).
  found @0 :Bool;
  # What a download would get, when `found`.
  offer @1 :QualityOffer;
}

# Success payload of `ContentSource.qualityOffers`.
struct QualityOffersResult {
  # One answer per title id, in the order they were sent. A short list
  # leaves the rest unanswered.
  offers @0 :List(TitleQualityOffer);
  # The account's credentials as the guest holds them after refreshing them
  # during the call, as in `ScanPage.refreshedCredentials`. Empty when
  # nothing changed.
  refreshedCredentials @1 :Data $optional;
}

# Result union of `ContentSource.qualityOffers`. A whole-batch `err` (for
# example `rateLimited`) applies to every title in it.
struct QualityOffersReply {
  union {
    # Success: an answer per title.
    ok @0 :QualityOffersResult;
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
  # What the host opens the entrypoints for. `provision` opens an instance
  # that may not be configured yet: `CONFIG` and `SECRETS` hold what is
  # stored so far, required settings may be missing, and the host calls
  # only `Entrypoints.provisioning`.
  purpose @5 :InvocationPurpose;
}

# Why the host opens a plugin's entrypoints (`Invocation.purpose`).
enum InvocationPurpose {
  # Ordinary work: every required setting is set.
  serve @0;
  # Guided setup (`Entrypoints.provisioning`): settings may be incomplete.
  provision @1;
}

# One message on a Workers-style queue, delivered by `QueueConsumer.queue`.
struct QueueMessage {
  # Outbox row id; stable across redeliveries.
  id @0 :Text;
  # When the producer sent the message (UTC Unix milliseconds).
  timestampUnixMs @1 :UInt64;
  # Encoded body; at most `maxEventPayloadBytes`.
  body @2 :Data;
  # Body encoding: `json`, `text`, or `bytes`.
  contentType @3 :Text;
  # Delivery counter, starting at 1.
  attempts @4 :UInt32;
  # Producer plugin id; empty when the host sent it.
  source @5 :Text;
}

# One `QueueConsumer.queue` delivery: messages from one queue in send order.
struct QueueBatch {
  # Queue the messages were sent to.
  queue @0 :Text;
  # Messages in delivery order; the reply carries one `QueueResult` per entry.
  messages @1 :List(QueueMessage);
}

# Redeliver a queue message later.
struct QueueRetry {
  # Seconds before redelivery; zero applies the consumer's `retry_delay`.
  delaySeconds @0 :UInt32;
  # Short human reason.
  reason @1 :Text;
}

# Per-message outcome inside a `QueueConsumer.queue` batch reply.
struct QueueResult {
  union {
    # Handled; the host marks the message delivered.
    ack @0 :Void;
    # Redeliver later.
    retry @1 :QueueRetry;
  }
}

# Success payload of `QueueConsumer.queue`: exactly one entry per
# `QueueBatch.messages` entry, in order. A short list is a host-side error
# and the missing tail is retried.
struct QueueBatchOk {
  # Per-message outcomes in delivery order.
  results @0 :List(QueueResult);
}

# Result union of `QueueConsumer.queue`.
struct QueueBatchReply {
  union {
    # Success: per-message outcomes.
    ok @0 :QueueBatchOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# One message to send through a `[[queues.producers]]` binding.
struct QueueSendMessage {
  # Encoded body; at most `maxEventPayloadBytes`.
  body @0 :Data;
  # Body encoding: `json` (default when empty), `text`, or `bytes`.
  contentType @1 :Text;
  # Seconds before the message may be delivered; zero means now.
  delaySeconds @2 :UInt32;
}

# Success payload of `QueueProducer.send` / `sendBatch`.
struct QueueSendOk {
  # Outbox row ids in send order.
  messageIds @0 :List(Text);
}

# Result union of `QueueProducer.send` / `sendBatch`.
struct QueueSendReply {
  union {
    # Success: the ids the host assigned.
    ok @0 :QueueSendOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Expiration and metadata for `KvNamespace.put`.
struct KvPutOptions {
  # Absolute expiry (UTC Unix milliseconds); zero means none.
  expirationUnixMs @0 :UInt64;
  # Relative expiry in seconds from the write; zero means none. Ignored when
  # `expirationUnixMs` is set.
  expirationTtlSeconds @1 :UInt64;
  # Opaque JSON metadata text, at most `maxKvMetadataBytes`; empty means none.
  metadata @2 :Text;
}

# Success payload of `KvNamespace.get`.
struct KvGetOk {
  # True when the key exists and has not expired.
  found @0 :Bool;
  # Stored value; empty when not found.
  value @1 :Data;
  # Stored metadata JSON text; empty when none.
  metadata @2 :Text;
  # Absolute expiry (UTC Unix milliseconds); zero means none.
  expirationUnixMs @3 :UInt64;
}

# Result union of `KvNamespace.get`.
struct KvGetReply {
  union {
    # Success: the value, when found.
    ok @0 :KvGetOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `KvNamespace.put`.
struct KvPutOk {
  # Effective absolute expiry (UTC Unix milliseconds); zero means none.
  expirationUnixMs @0 :UInt64;
}

# Success payload of `KvNamespace.delete`.
struct KvDeleteOk {
  # True when a live value existed before the delete.
  existed @0 :Bool;
}

# Result union of `KvNamespace.put`.
struct KvPutReply {
  union {
    # Success: the write is durable.
    ok @0 :KvPutOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Result union of `KvNamespace.delete`.
struct KvDeleteReply {
  union {
    # Success: the key is gone (or never existed).
    ok @0 :KvDeleteOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Page request for `KvNamespace.list`.
struct KvListOptions {
  # Only keys starting with this prefix; empty means all.
  prefix @0 :Text;
  # Opaque cursor from a previous page; empty means the first page.
  cursor @1 :Text;
  # Most keys to return; zero means the host default, at most `maxListPage`.
  limit @2 :UInt32;
}

# One key on a `KvNamespace.list` page.
struct KvListKey {
  # Key name.
  key @0 :Text;
  # Stored metadata JSON text; empty when none.
  metadata @1 :Text;
  # Absolute expiry (UTC Unix milliseconds); zero means none.
  expirationUnixMs @2 :UInt64;
}

# Success payload of `KvNamespace.list`.
struct KvListOk {
  # Keys in lexical order.
  keys @0 :List(KvListKey);
  # True when no further page exists.
  listComplete @1 :Bool;
  # Cursor for the next page; empty when `listComplete`.
  cursor @2 :Text;
}

# Result union of `KvNamespace.list`.
struct KvListReply {
  union {
    # Success: one page of keys.
    ok @0 :KvListOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# One fire of a `[triggers] crons` schedule (`scheduled(controller)`).
struct ScheduledController {
  # The schedule that fired, normalized to single spaces (`0 3 * * MON`).
  cron @0 :Text;
  # Fire time, UTC unix milliseconds (always a whole minute). Retries of
  # one fire keep it.
  scheduledTime @1 :UInt64;
  # Durable id of this fire, stable across retries; dedupe on it.
  fireId @2 :Text;
  # Failure attempt, starting at 1.
  attempt @3 :UInt32;
  # UTC unix milliseconds. Host fence/lease is authoritative; this hint must
  # not outlive the fence.
  deadlineUnixMs @4 :UInt64;
}

# Params of `MetadataProvider.lookup`.
struct MetadataLookupParams {
  # Identifiers naming the wanted edition or work; never empty. A provider
  # ignores schemes it does not serve.
  identifiers @0 :List(Identifier);
  # Marketplace to answer for (`us`, `uk`, ...) when the provider is
  # regional.
  marketplace @1 :Text $optional;
  # Most records to return; the provider picks a small default when absent.
  limit @2 :UInt32 $optional;
}

# Params of `MetadataProvider.search`.
struct MetadataSearchParams {
  # Title to match; never empty.
  title @0 :Text;
  # Author display name to narrow the match when known.
  author @1 :Text $optional;
  # Preferred content language code when known.
  language @2 :Text $optional;
  # Marketplace to answer for when the provider is regional.
  marketplace @3 :Text $optional;
  # Most records to return; the provider picks a small default when absent.
  limit @4 :UInt32 $optional;
}

# Params of `MetadataProvider.chapters`.
struct MetadataChaptersParams {
  # Identifiers of one edition (an ASIN for Audnexus); never empty.
  identifiers @0 :List(Identifier);
  # Marketplace to answer for when the provider is regional.
  marketplace @1 :Text $optional;
}

# One edition as a metadata provider describes it: the same structured
# fields as `CatalogHit`, without store pricing.
struct MetadataRecord {
  # Primary title.
  title @0 :Text;
  # Subtitle when distinct from `title`.
  subtitle @1 :Text $optional;
  # Long description / blurb when known.
  description @2 :Text $optional;
  # Cover image URL.
  coverUrl @3 :Text $optional;
  # The provider's page for this record.
  url @4 :Text $optional;
  # Publisher name when known.
  publisher @5 :Text $optional;
  # Runtime in whole minutes when known.
  lengthMinutes @6 :UInt32 $optional;
  # Publication date string as the provider prints it.
  publishedAt @7 :Text $optional;
  # Category / genre labels as a single string when known.
  categories @8 :Text $optional;
  # Content language code when known.
  language @9 :Text $optional;
  # Whether the edition is abridged when the provider says so.
  abridgement @10 :Abridgement;
  # Kind of work the record describes.
  workKind @11 :WorkKind;
  # How the work is realised in this edition.
  expressionKind @12 :ExpressionKind;
  # Carrier of this edition.
  mediaKind @13 :MediaKind;
  # External identifiers (ISBN, ASIN, OpenLibrary ids).
  identifiers @14 :List(Identifier);
  # Credited contributors in billing order.
  credits @15 :List(Credit);
  # Series the work belongs to.
  seriesEntries @16 :List(SeriesEntry);
  # Aggregate rating when the provider publishes one.
  ratingOverall @17 :Float64 $optional;
  # Number of ratings behind `ratingOverall` when known.
  ratingCount @18 :UInt32 $optional;
}

# Success payload of `MetadataProvider.lookup` / `search`.
struct MetadataRecords {
  # Records in the provider's relevance order; empty when nothing matched.
  records @0 :List(MetadataRecord);
}

# Result union of `MetadataProvider.lookup` / `search`.
struct MetadataRecordsReply {
  union {
    # Success: matching records.
    ok @0 :MetadataRecords;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# One chapter in a provider's chapter layout. Chapters are listed in
# pre-order: a chapter with `depth` n + 1 is nested under the nearest
# earlier chapter with `depth` n.
struct MetadataChapter {
  # Chapter title.
  title @0 :Text;
  # Start offset in milliseconds from the beginning of the title.
  startMs @1 :UInt64;
  # Length in milliseconds when known.
  lengthMs @2 :UInt64 $optional;
  # Nesting depth; 0 for a top-level chapter.
  depth @3 :UInt32;
}

# Success payload of `MetadataProvider.chapters`.
struct MetadataChapters {
  # False when the provider has no chapter layout for the edition.
  found @0 :Bool;
  # Chapters in pre-order; empty when not `found`.
  chapters @1 :List(MetadataChapter);
  # Length of the store's spoken brand intro in milliseconds when known.
  brandIntroMs @2 :UInt64 $optional;
  # Length of the store's spoken brand outro in milliseconds when known.
  brandOutroMs @3 :UInt64 $optional;
  # Total runtime the layout was measured against, in milliseconds.
  runtimeMs @4 :UInt64 $optional;
  # Whether the provider marks the offsets as exact rather than estimated.
  accurate @5 :Bool;
}

# Result union of `MetadataProvider.chapters`.
struct MetadataChaptersReply {
  union {
    # Success: chapter layout or not-found marker.
    ok @0 :MetadataChapters;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Params of `CoverFetcher.fetchCovers`.
struct CoverFetchParams {
  # Absolute `https://` URLs to fetch, at most `maxCoverUrls`; never empty.
  # The host has already checked each one against its URL policy.
  urls @0 :List(Text);
  # Largest body, in bytes, the host accepts for one image; at most
  # `maxCoverBytes`. The sink refuses a larger body, which fails that URL
  # only.
  maxBytes @1 :UInt32;
  # Conditional request headers per URL, in the same order as `urls`, from
  # the host's cached copy. Empty, or shorter than `urls`, means the
  # remaining URLs are fetched unconditionally.
  conditions @2 :List(CoverCondition);
  # What the URLs name: cover art (the default) or a storefront's audio
  # sample (`CatalogHit.sampleUrl`). A fetcher sends a matching `Accept`
  # header and refuses a body whose `Content-Type` names something else;
  # the host checks and re-encodes either kind regardless.
  media @3 :RemoteMedia;
}

# The kind of public media `CoverFetcher.fetchCovers` downloads.
enum RemoteMedia {
  # Cover art or another image.
  image @0;
  # A short audio sample of a book.
  audio @1;
}

# Validators the host already holds for one URL, sent as conditional request
# headers so an unchanged image costs a `304` instead of its body.
struct CoverCondition {
  # `If-None-Match` value: the `ETag` the host stored, verbatim; empty for
  # none.
  ifNoneMatch @0 :Text $optional;
  # `If-Modified-Since` value: the `Last-Modified` the host stored,
  # verbatim; empty for none.
  ifModifiedSince @1 :Text $optional;
}

# What happened to one requested URL.
struct CoverImage {
  # The requested URL, verbatim.
  url @0 :Text;
  # Whether the body was put into the sink under this URL's index.
  stored @1 :Bool;
  # `Content-Type` the server sent. Advisory: the host sniffs the bytes.
  contentType @2 :Text $optional;
  # Why nothing was stored (HTTP status, size cap, timeout, refused host).
  error @3 :Text $optional;
  # The server answered `304 Not Modified` to the URL's conditions: nothing
  # was put into the sink and the host's copy is still current.
  notModified @4 :Bool;
  # `ETag` the server sent, verbatim. Advisory: the host parses and caps it.
  etag @5 :Text $optional;
  # `Last-Modified` the server sent, verbatim.
  lastModified @6 :Text $optional;
  # `Cache-Control` the server sent, verbatim (several lines joined with
  # `, `).
  cacheControl @7 :Text $optional;
  # `Expires` the server sent, verbatim.
  expires @8 :Text $optional;
  # `Date` the server sent, verbatim.
  date @9 :Text $optional;
  # `Age` the server (or a cache in front of it) sent, verbatim.
  age @10 :Text $optional;
}

# Success payload of `CoverFetcher.fetchCovers`.
struct CoverImages {
  # One entry per requested URL, in request order.
  images @0 :List(CoverImage);
}

# Result union of `CoverFetcher.fetchCovers`.
struct CoverImagesReply {
  union {
    # Success: one entry per requested URL.
    ok @0 :CoverImages;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# A field of a `MetadataFileRecord`. Dialects declare in the manifest which
# fields their format carries; a parsed record lists the ones its file
# states, and a render request lists the ones to write.
enum MetadataField {
  # `MetadataFileRecord.title`.
  title @0;
  # `MetadataFileRecord.subtitle`.
  subtitle @1;
  # `MetadataFileRecord.description`.
  description @2;
  # `MetadataFileRecord.publisher`.
  publisher @3;
  # `MetadataFileRecord.publishedAt`.
  publishedAt @4;
  # `MetadataFileRecord.language`.
  language @5;
  # `MetadataFileRecord.genres`.
  genres @6;
  # `MetadataFileRecord.tags`.
  tags @7;
  # `MetadataFileRecord.abridgement`.
  abridgement @8;
  # `MetadataFileRecord.expressionKind`.
  expressionKind @9;
  # `MetadataFileRecord.identifiers`.
  identifiers @10;
  # `MetadataFileRecord.credits` in the roles `creditRoles` names.
  credits @11;
  # `MetadataFileRecord.seriesEntries`.
  series @12;
  # `MetadataFileRecord.chapters`.
  chapters @13;
  # `MetadataFileRecord.durationMs`.
  duration @14;
  # `MetadataFileRecord.explicitness`.
  explicitness @15;
}

# What one metadata file says about the edition it sits beside. A field the
# file does not state is left out of `present` and means "unknown", never
# "cleared"; a field in `present` with an empty value means the file states
# that it is empty. The host checks every value against the `maxMetadataFile*`
# caps and refuses a record that states a field its dialect does not carry.
struct MetadataFileRecord {
  # Fields the file states, each at most once; at most the fields the
  # dialect declares.
  present @0 :List(MetadataField);
  # Primary title.
  title @1 :Text $optional;
  # Subtitle when distinct from `title`.
  subtitle @2 :Text $optional;
  # Long description / blurb; at most `maxMetadataFileDescriptionBytes`.
  description @3 :Text $optional;
  # Publisher name.
  publisher @4 :Text $optional;
  # Publication date as `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
  publishedAt @5 :Text $optional;
  # Content language as the file prints it (a code or a name).
  language @6 :Text $optional;
  # Genre labels in file order.
  genres @7 :List(Text);
  # Free tags in file order.
  tags @8 :List(Text);
  # Whether the edition is abridged; `unknown` when the file does not say.
  abridgement @9 :Abridgement;
  # How the work is realised; `unspecified` when the file does not say.
  expressionKind @10 :ExpressionKind;
  # External identifiers the file carries.
  identifiers @11 :List(Identifier);
  # Credited contributors in billing order, only in the roles `creditRoles`
  # names.
  credits @12 :List(Credit);
  # Credit roles the file states, each at most once. A role listed here
  # with no credit means the file states nobody in that role; a role left
  # out is unknown. Empty unless `present` holds `credits`.
  creditRoles @13 :List(CreditRole);
  # Series the work belongs to.
  seriesEntries @14 :List(SeriesEntry);
  # Chapter layout in pre-order (see `MetadataChapter`); at most
  # `maxMetadataFileChapters`, starts never decreasing.
  chapters @15 :List(MetadataChapter);
  # Running time in milliseconds.
  durationMs @16 :UInt64 $optional;
  # Whether the edition is explicit (adult) content; `unknown` when the
  # file does not say.
  explicitness @17 :Explicitness;
}

# Params of `MetadataFiles.parse`.
struct MetadataFileParseParams {
  # Dialect id from the manifest's `[[metadata_files.dialects]]` that the
  # host routed the file to.
  dialect @0 :Text;
  # The file's name without any folder (`metadata.json`, `Book.opf`).
  fileName @1 :Text;
  # The whole file; at most `maxMetadataFileBytes`.
  bytes @2 :Data;
}

# Success payload of `MetadataFiles.parse`.
struct MetadataFileParsed {
  # What the file says.
  record @0 :MetadataFileRecord;
}

# Result union of `MetadataFiles.parse`. A file the dialect cannot read is
# `invalid_params`, so the host records it as unreadable and moves on.
struct MetadataFileParseReply {
  union {
    # Success: the parsed record.
    ok @0 :MetadataFileParsed;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# One audio file of the edition a sidecar describes, for formats that name
# their audio (a CUE sheet's `FILE` lines).
struct MetadataAudioFile {
  # The file's name without any folder, as it sits next to the sidecar.
  name @0 :Text;
  # Running time in milliseconds; 0 when the host does not know it.
  durationMs @1 :UInt64;
}

# Params of `MetadataFiles.render`.
struct MetadataFileRenderParams {
  # Dialect id from the manifest; one that declares `writes = true`.
  dialect @0 :Text;
  # Values to write. Only the fields in `fields` are read from it.
  record @1 :MetadataFileRecord;
  # Fields to set, each at most once and all declared by the dialect. A
  # listed field with an empty value removes it from the file.
  fields @2 :List(MetadataField);
  # The file as it is now; empty when the host is creating it. Everything
  # the dialect does not change (unknown keys, comments, other fields) is
  # kept. At most `maxMetadataFileBytes`.
  existing @3 :Data;
  # The edition's audio files in playback order, at most
  # `maxMetadataAudioFiles`; empty when the host has none or does not know
  # them. Chapter starts in `record` count from the start of the first file.
  audioFiles @4 :List(MetadataAudioFile);
}

# Success payload of `MetadataFiles.render`.
struct MetadataFileRendered {
  # The new file; at most `maxMetadataFileBytes`. Empty when the dialect has
  # nothing left to write; the host decides whether to remove the file.
  bytes @0 :Data;
}

# Result union of `MetadataFiles.render`.
struct MetadataFileRenderReply {
  union {
    # Success: the new file.
    ok @0 :MetadataFileRendered;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# How urgent a notification is. Routing rules carry a minimum severity.
enum NotificationSeverity {
  # Informational.
  info @0;
  # Needs attention soon.
  warning @1;
  # Needs attention now; ignores quiet hours for administrators.
  critical @2;
}

# Who a notification type is for. The account audience is reserved to the
# host; a plugin may declare only operational and patron types.
enum NotificationAudience {
  # Administrators and library channels.
  operational @0;
  # One patron, under their own preferences.
  patron @1;
  # Account mail and security notices; host-only.
  account @2;
}

# Lifecycle step of an alert, for services such as PagerDuty that open and
# close incidents.
enum NotificationAlertAction {
  # Not an alert: a one-off message.
  none @0;
  # Open (or update) the alert named by `alertKey`.
  trigger @1;
  # Close the alert named by `alertKey`.
  resolve @2;
}

# Kind of destination a `Notifier` sends to.
enum NotifyDestinationKind {
  # A library channel the instance itself is configured for (its settings
  # hold the webhook or service key); `addressJson` is empty.
  channel @0;
  # One patron's own address on this instance (`addressJson`).
  personal @1;
}

# One labelled value shown with a notification.
struct NotificationField {
  # Label; at most `maxNotificationTitleBytes`.
  name @0 :Text;
  # Value; at most `maxNotificationFieldBytes`.
  value @1 :Text;
}

# A message the host rendered for one notification. Every link is built
# from the library's public origin, never from a request.
struct NotificationMessage {
  # Host-assigned id, the same on every destination's copy; plugins use it
  # as an idempotency key where the service takes one.
  notificationId @0 :Text;
  # Notification type id (`job_failed`, `<plugin>:sync_failed`).
  notificationType @1 :Text;
  # How urgent it is.
  severity @2 :NotificationSeverity;
  # Short title; at most `maxNotificationTitleBytes`.
  title @3 :Text;
  # Plain-text body; at most `maxNotificationTextBytes`.
  text @4 :Text;
  # Markdown body for chat services; at most `maxNotificationTextBytes`.
  markdown @5 :Text $optional;
  # Minimal HTML body for email; at most `maxNotificationHtmlBytes`.
  html @6 :Text $optional;
  # Absolute `https` (or loopback `http`) link into the library.
  link @7 :Text $optional;
  # Labelled values, at most `maxNotificationFields`.
  fields @8 :List(NotificationField);
  # Messages with one group key may be threaded or collapsed together.
  groupKey @9 :Text $optional;
  # Alert lifecycle step; `none` for a one-off message.
  alertAction @10 :NotificationAlertAction;
  # Alert this message opens or closes; required unless `alertAction` is
  # `none`.
  alertKey @11 :Text $optional;
  # True when the body carries a secret (an account-mail link): the plugin
  # and SDKs must never log it.
  sensitive @12 :Bool;
  # The library's home page (`https://library.example.com/`), built from its
  # public origin like `link`; absent when no public origin is set. Email
  # footers link to it.
  libraryOrigin @13 :Text $optional;
}

# Where a `Notifier.send` goes.
struct NotifyDestination {
  # Channel or personal destination.
  kind @0 :NotifyDestinationKind;
  # Host id of the destination, stable across sends.
  destinationId @1 :Text;
  # Personal address as a JSON object of the fields the manifest's
  # `[[notifier.address_fields]]` declare; empty for a channel. At most
  # `maxNotifierAddressBytes`. May hold secrets; never logged.
  addressJson @2 :Text;
}

# Params of `Notifier.send`.
struct NotifySendParams {
  # What to send.
  message @0 :NotificationMessage;
  # Where to send it.
  destination @1 :NotifyDestination;
}

# Success payload of `Notifier.send`.
struct NotifySent {
  # The service's id for the delivered message, when it gives one.
  providerId @0 :Text $optional;
}

# Result union of `Notifier.send`.
struct NotifySendReply {
  union {
    # Delivered.
    ok @0 :NotifySent;
    # Typed failure; `rate_limited`, `unavailable`, `deadline_exceeded` and `cancelled` are retried.
    err @1 :PluginError;
  }
}

# Params of `Notifier.validateDestination`.
struct NotifyValidateParams {
  # Address document as the patron entered it; at most
  # `maxNotifierAddressBytes`.
  addressJson @0 :Text;
}

# Success payload of `Notifier.validateDestination`.
struct NotifyValidated {
  # Normalised address document the host saves; at most
  # `maxNotifierAddressBytes`.
  addressJson @0 :Text;
  # Short label shown to the patron (`a***@example.com`); never a secret.
  label @1 :Text $optional;
}

# Result union of `Notifier.validateDestination`. An address the plugin
# refuses is `invalid_params`.
struct NotifyValidateReply {
  union {
    # Accepted, normalised address.
    ok @0 :NotifyValidated;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Params of `Notifier.checkDeliverability`.
struct NotifyDeliverabilityParams {
  # The host's `test` notification, rendered as a real one is, so the
  # checkers score the plugin's real output.
  message @0 :NotificationMessage;
}

# How one deliverability check or piece of advice came out.
enum NotifyCheckOutcome {
  # Passed; nothing to change.
  passed @0;
  # Works, but could be better.
  warning @1;
  # Likely to land in spam or be refused; fix it first.
  failed @2;
  # Not run: not set up, not approved, or the service did not answer.
  skipped @3;
}

# One checker's result.
struct NotifyDeliverabilityCheck {
  # Service name (`Postmark spam check`); at most `maxNotificationTitleBytes`.
  service @0 :Text;
  # How it came out.
  outcome @1 :NotifyCheckOutcome;
  # One-line result (`SpamAssassin score 0.8; under 5 passes`); at most
  # `maxNotificationFieldBytes`.
  summary @2 :Text;
  # Absolute `https` link to the service's full report, when it has one.
  reportUrl @3 :Text $optional;
}

# One thing the operator can do, merged from every check.
struct NotifyDeliverabilityAdvice {
  # `failed` is a fix, `warning` an improvement, `passed` what is already
  # right; never `skipped`.
  outcome @0 :NotifyCheckOutcome;
  # Short heading; at most `maxNotificationTitleBytes`.
  title @1 :Text;
  # What to change and where; at most `maxNotificationFieldBytes`.
  detail @2 :Text;
  # The `service` names of the checks that raised it.
  sources @3 :List(Text);
}

# Success payload of `Notifier.checkDeliverability`.
struct NotifyDeliverabilityReport {
  # One entry per checker, at most `maxDeliverabilityChecks`.
  checks @0 :List(NotifyDeliverabilityCheck);
  # Merged advice, most urgent first; at most `maxDeliverabilityAdvice`.
  advice @1 :List(NotifyDeliverabilityAdvice);
}

# Result union of `Notifier.checkDeliverability`.
struct NotifyDeliverabilityReply {
  union {
    # The report.
    ok @0 :NotifyDeliverabilityReport;
    # Typed failure; `unsupported` when the plugin has no check.
    err @1 :PluginError;
  }
}

# Params of `NotificationRaiser.raise`.
struct NotifyRaiseParams {
  # Type id from the plugin's `[[notifications.types]]` (without the plugin
  # prefix the host adds).
  notificationType @0 :Text;
  # Labelled values, at most `maxNotificationFields`.
  fields @1 :List(NotificationField);
  # Patron the notification is for; required for a patron-audience type
  # and must be inside the invocation's account scope; empty otherwise.
  patronId @2 :Text $optional;
  # Path inside the library the notification links to (starts with `/`);
  # the host prefixes the public origin.
  linkPath @3 :Text $optional;
  # Messages with one group key may be threaded or collapsed together.
  groupKey @4 :Text $optional;
  # Alert lifecycle step; `none` for a one-off notification.
  alertAction @5 :NotificationAlertAction;
  # Alert this raise opens or closes; required unless `alertAction` is
  # `none`.
  alertKey @6 :Text $optional;
}

# Success payload of `NotificationRaiser.raise`.
struct NotifyRaised {
  # Host-assigned notification id.
  notificationId @0 :Text;
}

# Result union of `NotificationRaiser.raise`.
struct NotifyRaiseReply {
  union {
    # Accepted by the router.
    ok @0 :NotifyRaised;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# What a model does. A model has exactly one task, and the task fixes the
# shape of `AiRunParams.input` and of the reply.
enum AiTask {
  # Turns each text into one vector; similar texts get nearby vectors.
  textEmbeddings @0;
}

# What a text is to an embedding model. Some models embed a search query
# differently from the documents it is matched against.
enum AiTextRole {
  # Stored content: a description, a passage.
  document @0;
  # A search query matched against documents.
  query @1;
}

# One model an `ai` plugin serves.
struct AiModel {
  # Model id within the plugin (`bekko-a8m`): lowercase letters, digits,
  # `.`, `_` and `-`. Callers name it `@<plugin id>/<model id>`; the whole
  # name is at most `maxAiModelNameBytes`.
  modelId @0 :Text;
  # The model's one task.
  task @1 :AiTask;
  # Digest of everything that decides the output: weights, tokenizer and
  # the language packs loaded. Outputs are comparable only when it matches.
  revision @2 :Text;
  # Text embeddings: numbers per vector, at most `maxAiEmbedDimensions`.
  dimensions @3 :UInt32 $optional;
  # Text embeddings: tokens read from one text; the rest is cut off.
  maxTokens @4 :UInt32 $optional;
  # BCP 47 tags of the languages the model is loaded for, at most
  # `maxAiLanguages`; empty when it has no language packs.
  languages @5 :List(Text);
}

# Success payload of `AiModels.models`.
struct AiModelList {
  # Served models, at most `maxAiModels`, each `modelId` once.
  models @0 :List(AiModel);
}

# Result union of `AiModels.models`.
struct AiModelsReply {
  union {
    # The served models.
    ok @0 :AiModelList;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# One text to embed.
struct AiEmbedText {
  # The text, at most `maxAiEmbedTextBytes`; never empty.
  text @0 :Text;
  # Document or query.
  role @1 :AiTextRole;
}

# Input of the `textEmbeddings` task.
struct AiTextEmbeddingsInput {
  # Texts to embed, 1 to `maxAiEmbedTexts`.
  texts @0 :List(AiEmbedText);
}

# Output of the `textEmbeddings` task.
struct AiTextEmbeddingsOutput {
  # Numbers per vector; equals the model's `AiModel.dimensions`.
  dimensions @0 :UInt32;
  # One vector per input text, in input order: `dimensions` little-endian
  # IEEE 754 32-bit floats each, concatenated. Every number is finite.
  vectors @1 :Data;
  # `AiModel.revision` of the model that made the vectors.
  revision @2 :Text;
  # Indexes (into the input texts, ascending, each once) of the texts
  # where the tokenizer met text outside the loaded language packs and
  # fell back to smaller pieces; such a vector should be recomputed after
  # the missing pack is added.
  fallbacks @3 :List(UInt32);
}

# Input to one model run: one field per `AiTask`, and exactly one is set
# (the others are null). A new task appends a field.
struct AiInput {
  # `textEmbeddings` task.
  textEmbeddings @0 :AiTextEmbeddingsInput;
}

# Output of one model run: one field per `AiTask`, and exactly one is set
# (the others are null). A new task appends a field.
struct AiOutput {
  # `textEmbeddings` task.
  textEmbeddings @0 :AiTextEmbeddingsOutput;
}

# Params of `AiModels.run` and `AiRunner.run`.
struct AiRunParams {
  # On `AiModels.run`, the plugin's own `AiModel.modelId`. On
  # `AiRunner.run`, `@<plugin id>/<model id>` or a task alias
  # (`@library/text-embeddings`: the model the library chose for the task).
  model @0 :Text;
  # The input; the field set must match the model's task.
  input @1 :AiInput;
}

# Success payload of a model run.
struct AiRunOk {
  # The output; the field set matches the model's task.
  output @0 :AiOutput;
  # Full name of the model that ran (`@<plugin id>/<model id>`), so a
  # caller that used a task alias knows what answered. Set by the host on
  # `AiRunner.run`; empty on `AiModels.run`.
  model @1 :Text $optional;
}

# Result union of `AiModels.run` and `AiRunner.run`. A model the plugin
# does not serve is `not_found`; an input for another task is
# `invalid_params`; a caller without the grant gets `forbidden`.
struct AiRunReply {
  union {
    # The model ran.
    ok @0 :AiRunOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# One WebSocket message, or one chunk of a `tls://` stream (always
# `binary`).
struct SocketFrame {
  union {
    # Text message, UTF-8.
    text @0 :Text;
    # Binary message.
    binary @1 :Data;
  }
}

# One request header on `Sockets.connect`.
struct SocketHeader {
  # Header name; the broker refuses hop-by-hop and WebSocket handshake names.
  name @0 :Text;
  # Header value. May carry a credential: the host never logs it.
  value @1 :Text;
}

# A heartbeat the broker answers itself: when `request` arrives byte for byte,
# it sends `response` and does not wake the guest.
struct SocketAutoResponse {
  # Frame to match, at most `maxSocketAutoResponseBytes`.
  request @0 :SocketFrame;
  # Frame to send back, at most `maxSocketAutoResponseBytes`.
  response @1 :SocketFrame;
}

# Arguments of `Sockets.connect`.
struct SocketConnectRequest {
  # `wss://` or `ws://` URL, at most `maxSocketUrlBytes`, or
  # `tls://host:port` for a raw TLS byte stream (port required, nothing
  # after it). The host must be on the plugin's network grant; a `tls://`
  # stream needs a `tcp` grant for that host and port.
  url @0 :Text;
  # Requested subprotocols, at most `maxSocketHeaders`; on a `tls://`
  # stream, the ALPN protocol ids to offer (visible ASCII, 1 to 255 bytes).
  protocols @1 :List(Text);
  # Extra request headers, at most `maxSocketHeaders`; none on `tls://`.
  headers @2 :List(SocketHeader);
  # Heartbeats the broker answers, at most `maxSocketAutoResponses`; none on
  # `tls://`.
  autoResponses @3 :List(SocketAutoResponse);
  # Initial attachment, at most `maxSocketAttachmentBytes`.
  attachment @4 :Data;
  # `tls://` only: DER root certificates, at most `maxSocketTrustAnchors` of
  # at most `maxSocketTrustAnchorBytes` each, that the server's chain must
  # end in instead of Mozilla's roots, for a service that signs its own
  # (an ngrok or Cloudflare Tunnel agent endpoint). Empty uses Mozilla's.
  trustAnchors @5 :List(Data);
  # `tls://` only: the DNS name to send as SNI and to check the server's
  # certificate against, for an endpoint reached at one name that answers
  # for another (a Cloudflare Tunnel edge, dialed at `region1.v2.argotunnel.com`
  # and serving `h2.cftunnel.com`). It must be on a `tcp` grant for the
  # URL's port too, so it can't route past the grant by name. At most
  # `maxSocketServerNameBytes`; empty uses the URL's host.
  serverName @6 :Text;
}

# One open host-brokered socket.
struct SocketInfo {
  # Host-minted socket id, stable for the socket's life.
  socket @0 :Text;
  # URL the socket was opened to, without credentials.
  url @1 :Text;
  # Subprotocol the server chose (on `tls://`, the negotiated ALPN id);
  # empty when none.
  protocol @2 :Text $optional;
  # Current attachment.
  attachment @3 :Data;
}

# Result union of `Sockets.connect`.
struct SocketConnectReply {
  union {
    # Success: the handshake finished and the socket is open.
    ok @0 :SocketInfo;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `Sockets.list`.
struct SocketList {
  # Open sockets, oldest first.
  sockets @0 :List(SocketInfo);
}

# Result union of `Sockets.list`.
struct SocketListReply {
  union {
    # Success: the open sockets.
    ok @0 :SocketList;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Reply of `SocketHandler.message`.
struct SocketMessageReply {
  union {
    # Success: what the guest asks of the host after the message.
    ok @0 :SocketMessageOk;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# What a guest asks of the host after handling one pushed message.
struct SocketMessageOk {
  # True asks the host to call `Integration.pollEvents` now, while the
  # guest is awake, instead of at the next scheduled poll: the message
  # announced something the poll hands over (a new user). The host ignores
  # it for a plugin whose host schedules no poll.
  pollEvents @0 :Bool;
}

# Argument of `SocketHandler.message`.
struct SocketMessage {
  # Socket the message arrived on.
  socket @0 :Text;
  # The message.
  frame @1 :SocketFrame;
  # The socket's attachment when the message arrived.
  attachment @2 :Data;
}

# Argument of `SocketHandler.closed`.
struct SocketClosed {
  # Socket that closed; its id is not reused.
  socket @0 :Text;
  # WebSocket close code; 1006 when the connection dropped without one.
  code @1 :UInt16;
  # Close reason the peer sent; empty when none.
  reason @2 :Text $optional;
  # True when the close handshake completed.
  clean @3 :Bool;
  # The socket's last attachment.
  attachment @4 :Data;
}

# One header field on an `Origin.fetch` request or response.
struct OriginHeader {
  # Field name, an HTTP token; the host refuses hop-by-hop names.
  name @0 :Text;
  # Field value, visible ASCII, spaces and tabs. May carry a credential (a
  # cookie, an authorization): the host never logs it.
  value @1 :Text;
}

# The head of one public request a relay `ingress` plugin carries
# (`Origin.fetch`). Its scheme is `https` and its authority is the library's
# public origin; neither is the plugin's to choose.
struct OriginRequest {
  # Request method, an HTTP token (`GET`, `PUT`, ...).
  method @0 :Text;
  # Path and query in origin form (`/api/status?x=1`), at most
  # `maxOriginTargetBytes`.
  target @1 :Text;
  # Request header fields in order, at most `maxOriginHeaders` and
  # `maxOriginHeaderBytes`. `host` and forwarding headers are ignored.
  headers @2 :List(OriginHeader);
  # IP address of the client, from the provider's own connection metadata;
  # empty when the provider gives none.
  clientAddress @3 :Text $optional;
}

# The library's answer to an `Origin.connect` it did not upgrade: an
# ordinary response (`401`, `403`, `404`, ...) for the plugin to hand back
# to the client.
struct OriginRefusal {
  # HTTP status code, 200 to 599 and never `101`.
  status @0 :UInt16;
  # Response header fields in order, at most `maxOriginHeaders` and
  # `maxOriginHeaderBytes`.
  headers @1 :List(OriginHeader);
  # Response body, at most `maxOriginRefusalBodyBytes`.
  body @2 :Data;
}

# Success payload of `Origin.connect`.
struct OriginConnected {
  union {
    # The library accepted the upgrade; the socket is open in
    # `Bindings.sockets`. Its `protocol` is the subprotocol the library
    # chose.
    socket @0 :SocketInfo;
    # The library answered without upgrading.
    refused @1 :OriginRefusal;
  }
}

# Result union of `Origin.connect`.
struct OriginConnectReply {
  union {
    # Success: an open socket or the library's refusal.
    ok @0 :OriginConnected;
    # Typed failure: the request was refused before the library saw it
    # (`invalid_params`, `forbidden` without `[sockets]`, `limit_exceeded`
    # when the instance holds `maxOriginSocketsPerInstance`) or the library
    # is unavailable.
    err @1 :PluginError;
  }
}

# Success payload of `Ingress.start`.
struct IngressStarted {
  # The public origin now serving: `https://` and a host name, no port, path
  # or trailing slash, at most `maxIngressOriginBytes` after the scheme.
  origin @0 :Text;
}

# Result union of `Ingress.start`.
struct IngressStartReply {
  union {
    # Success: the address is up.
    ok @0 :IngressStarted;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `Webhooks.url`.
struct WebhookUrl {
  # Absolute `https://` URL; empty when the library cannot be reached from
  # the Internet or webhooks are turned off, and the plugin should use its
  # fallback.
  url @0 :Text $optional;
}

# Result union of `Webhooks.url`.
struct WebhookUrlReply {
  union {
    # Success: the URL, or none.
    ok @0 :WebhookUrl;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Arguments of `Webhooks.pull`.
struct WebhookPullRequest {
  # `[[webhooks]]` name.
  name @0 :Text;
  # Most deliveries to lease, 1 to `maxWebhookPullBatch`.
  batchSize @1 :UInt32;
  # How long leased deliveries stay hidden from the next pull, in
  # milliseconds, at most `maxWebhookVisibilityMs`; zero means one minute.
  visibilityTimeoutMs @2 :UInt32;
}

# One request header kept with a delivery.
struct WebhookHeader {
  # Lowercase header name, one the row's `headers` names or `content-type`.
  name @0 :Text;
  # Header value as received.
  value @1 :Text;
}

# One stored request, leased to the guest.
struct WebhookDelivery {
  # Lease id for `ack`, at most `maxWebhookLeaseIdBytes`; a new one each
  # time the delivery is pulled.
  leaseId @0 :Text;
  # When the host stored the request, in Unix milliseconds.
  receivedAtUnixMs @1 :UInt64;
  # How many times this delivery has been pulled, counting this one.
  attempt @2 :UInt32;
  # Kept request headers, in the order the row names them.
  headers @3 :List(WebhookHeader);
  # Request body as received, at most the row's `max_body_bytes`.
  body @4 :Data;
}

# Success payload of `Webhooks.pull`.
struct WebhookPullPage {
  # Leased deliveries, oldest first.
  deliveries @0 :List(WebhookDelivery);
  # True when the host dropped deliveries for this webhook since the last
  # pull (the waiting cap was full or a delivery expired unacknowledged):
  # the guest should treat what it follows as incomplete.
  missed @1 :Bool;
}

# Result union of `Webhooks.pull`.
struct WebhookPullReply {
  union {
    # Success: the leased deliveries.
    ok @0 :WebhookPullPage;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Success payload of `Ingress.health`.
struct IngressHealth {
  # True when the address is serving.
  serving @0 :Bool;
  # Why it is not serving, for the operator; empty when serving.
  detail @1 :Text $optional;
}

# Result union of `Ingress.health`.
struct IngressHealthReply {
  union {
    # Success: the provider's view of the address.
    ok @0 :IngressHealth;
    # Typed failure; `code` is a `PluginErrorCode` wire string.
    err @1 :PluginError;
  }
}

# Arguments of `Webhooks.ack`.
struct WebhookAckRequest {
  # `[[webhooks]]` name.
  name @0 :Text;
  # Leases whose deliveries are done and are deleted.
  acks @1 :List(Text);
  # Leases whose deliveries the next pull offers again.
  retries @2 :List(Text);
}

# Argument of `WebhookHandler.delivered`.
struct WebhookDelivered {
  # `[[webhooks]]` name.
  name @0 :Text;
  # Deliveries waiting for this webhook when the host called.
  waiting @1 :UInt32;
}

# What kind of grant the operator gave a guided setup (`ProvisionGrant`).
enum ProvisionGrantKind {
  # No grant: `deprovision` uses the instance's stored secrets.
  none @0;
  # An API token the operator created at the provider and pasted.
  apiToken @1;
  # An OAuth access token from the provider's sign-in.
  oauthAccessToken @2;
}

# A short-lived credential for one guided setup run. The host never stores
# or logs it; the plugin uses it only during the call.
struct ProvisionGrant {
  # Kind of grant.
  kind @0 :ProvisionGrantKind;
  # The token; empty for `none`. At most `maxProvisionTokenBytes`.
  token @1 :Text $optional;
  # When the token expires (UTC Unix milliseconds); zero when unknown or it
  # does not expire.
  expiresAtUnixMs @2 :UInt64 $optional;
}

# Arguments of `Provisioning.provision`.
struct ProvisionRequest {
  # The operator's grant; never `none`.
  grant @0 :ProvisionGrant;
  # The operator's choices as a JSON object keyed by `[provisioning] inputs`
  # setting keys (`{"bucket": "books"}`); `{}` when none were asked for.
  inputsJson @1 :Text;
}

# One credential a guided setup returns for the host to seal.
struct ProvisionedSecret {
  # A `secret` setting key listed in `[provisioning] outputs`.
  key @0 :Text;
  # The credential, at most `maxProvisionSecretBytes`.
  value @1 :Text;
}

# Success payload of `Provisioning.provision`.
struct Provisioned {
  # Non-secret settings to store, as a JSON object keyed by setting keys
  # listed in `[provisioning] outputs`; at most `maxProvisionSettingsBytes`.
  settingsJson @0 :Text;
  # Credentials to seal; each key at most once.
  secrets @1 :List(ProvisionedSecret);
  # Short lines for the setup screen ("Created bucket books"), at most
  # `maxProvisionSummaryLines` of `maxProvisionSummaryLineBytes`. Never a
  # credential.
  summary @2 :List(Text);
}

# Result union of `Provisioning.provision`.
struct ProvisionReply {
  union {
    # Success: what to store.
    ok @0 :Provisioned;
    # Typed failure: `invalid_params` for a refused choice, `unauthorized`
    # or `forbidden` when the grant cannot do what is needed.
    err @1 :PluginError;
  }
}

# Arguments of `Provisioning.deprovision`.
struct DeprovisionRequest {
  # The operator's grant, or kind `none` to use the stored secrets.
  grant @0 :ProvisionGrant;
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

# First frame of a streamed execute reply: what the reply is about.
struct ExecuteStreamHead {
  # Echo of the request `operationId`.
  operationId @0 :Text;
  # Number of `statement` frames that follow, one per request statement.
  statementCount @1 :UInt32;
}

# Opens one statement result inside a streamed execute reply. Its rows, if
# any, follow in `rows` frames until the next `statement`, `end` or `err`.
struct ExecuteStreamStatement {
  # Request statement index, `0`-based and increasing by one per frame.
  index @0 :UInt32;
  # Result column descriptors.
  columns @1 :List(DbColumn);
  # Rows changed by a mutation.
  rowsAffected @2 :UInt64;
}

# One frame of a streamed execute reply (`executeStreamed`). Frames are
# consecutive unpacked single-segment Cap'n Proto messages, each at most
# `maxScalarBytes` unless it carries one row that alone exceeds it. The
# sequence is `head`, then for each statement `statement` followed by its
# `rows` frames in order, then `end`; `err` ends the sequence early.
struct ExecuteStreamFrame {
  union {
    # First frame.
    head @0 :ExecuteStreamHead;
    # Opens the next statement result.
    statement @1 :ExecuteStreamStatement;
    # Rows of the open statement, in order.
    rows @2 :List(DbRow);
    # Last frame: the batch committed; engine timing.
    end @3 :DbTiming;
    # Last frame: the batch failed; `code` is a `PluginErrorCode` wire string.
    err @4 :PluginError;
  }
}

# Result union of `executeStreamed`. The frames (`ExecuteStreamFrame`) travel
# inline when they fit one scalar, otherwise as a transferred byte stream in
# `maxStreamWindowBytes` windows. The adapter counts the reply against
# `maxStreamedResultBytes` and `maxStreamedResultRows` before it commits, so
# results still fail closed and are never truncated.
struct ExecuteStreamReply {
  union {
    # Success: every frame, concatenated, at most `maxScalarBytes`.
    inline @0 :Data;
    # Success: the frames as a byte stream.
    stream @1 :ByteSource;
    # Typed failure before any frame; `code` is a `PluginErrorCode` wire string.
    err @2 :PluginError;
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
  # Maximum encoded bytes of one `ExecuteReply` answered through
  # `executeStreamed` (the sum of its frames' statement results). `0` means
  # the adapter does not stream and callers stay under the inline caps.
  # When set it is at least `maxAtomicResultBytes`.
  maxStreamedResultBytes @22 :UInt32;
  # Maximum rows one statement may return through `executeStreamed`. `0`
  # means the adapter does not stream. When set it is at least
  # `maxResultRows`.
  maxStreamedResultRows @23 :UInt32;
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
  # Physically drop one provisioned binding unit (`unitRef` is adapter-owned).
  dropUnit @1 (
      unitRef :Text  # Adapter-native unit (sqlite path, postgres database, D1 name).
  ) -> (result :EmptyReply);
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
  # `execute` whose reply is framed (`ExecuteStreamFrame`) and counted against
  # `maxStreamedResultBytes` / `maxStreamedResultRows` instead of the inline
  # caps. One transaction, one snapshot; `unsupported` when the adapter
  # advertises no streamed budget.
  executeStreamed @10 (
      request :AdapterExecuteRequest  # Proven statements plus isolation and receipt.
  ) -> (result :ExecuteStreamReply);
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
  # Negotiated limits of this session, so guests size requests and reads
  # before sending instead of discovering caps from errors.
  limits @2 () -> (result :DbCapabilitiesReply);
  # `execute` whose reply is framed (`ExecuteStreamFrame`) and counted against
  # the session's `maxStreamedResultBytes` / `maxStreamedResultRows`, so a
  # result past the inline caps still arrives whole. `unsupported` when the
  # session advertises no streamed budget.
  executeStreamed @3 (
      request :ExecuteRequest  # Statements, binds, and deadline.
  ) -> (result :ExecuteStreamReply);
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
