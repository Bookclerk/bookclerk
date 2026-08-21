# Bookclerk plugin ABI — object-capability Workers RPC (`api_version = 2`).
#
# Evolution (append-only):
# - Never reuse field, method, or union ordinals.
# - Unknown enum/union members: preserve the wire code and fail closed or
#   return typed `unsupported`. Never collapse unknown codes to `internal`.
# - `describe().abiMajor` must match `apiVersion`. `abiMinor` may increase
#   within a major; hosts ignore unknown optional fields.
# - Feature bits (`rpcFeatures`) negotiate optional facilities inside a major.
#   Required features are rejected at spawn when missing.
# - Every variable-length field is bounded by the constants below.
# - Identifiers are non-empty `[a-z][a-z0-9_]{0,63}`. Timestamps are UTC
#   unix milliseconds (UInt64); zero means omitted.
# - Absent factories/methods return typed `unsupported`.
# - `describe()` advertises `supportedRoles`. The signed manifest is the host
#   allowlist of what may be invoked (kind alone is not sufficient).
#
# Authors never see transport-private capability table indexes. Public types
# are the interfaces below plus TypeScript `BookclerkPlugin`. ByteSource is
# the Cap'n Proto realization of a transferred byte ReadableStream.
#
# Method results are typed success/error unions. SDKs map `err` onto a thrown
# PluginError. Unknown future `code` strings MUST be preserved.
@0x816df58cae22db0c;

const apiVersion :UInt32 = 2;
const abiMajor :UInt32 = 2;
const abiMinor :UInt32 = 6;
const envelopeVersion :UInt32 = 1;
const maxScalarBytes :UInt32 = 262144;
const maxStreamWindowBytes :UInt32 = 1048576;
const maxListPage :UInt32 = 256;
const maxCheckpointBytes :UInt32 = 65536;
const maxIdentifierBytes :UInt32 = 64;
const maxConfigPayloadBytes :UInt32 = 65536;
const maxEventPayloadBytes :UInt32 = 65536;

struct ScalarLimits {
  maxScalarBytes @0 :UInt32;
  maxStreamWindowBytes @1 :UInt32;
  maxListPage @2 :UInt32;
}

# `code` is a snake_case string (`not_found`, `invalid_cursor`, …). Unknown
# codes are forwarded as-is; SDKs surface them as PluginErrorCode.unknown
# while retaining the raw wire code.
struct PluginError {
  code @0 :Text;
  message @1 :Text;
}

struct ObjectMetadata {
  key @0 :Text;
  size @1 :UInt64;
  contentType @2 :Text;
  etag @3 :Text;
  sha256 @4 :Data;
}

struct ObjectInfo {
  key @0 :Text;
  size @1 :UInt64;
}

struct ListOptions {
  prefix @0 :Text;
  cursor @1 :Text;
  limit @2 :UInt32;
}

struct ListPage {
  objects @0 :List(ObjectInfo);
  nextCursor @1 :Text;
}

struct ByteRange {
  offset @0 :UInt64;
  length @1 :UInt64;
}

struct ReadOptions {
  range @0 :ByteRange;
}

struct WriteOptions {
  contentType @0 :Text;
  contentLength @1 :UInt64;
  sha256 @2 :Data;
  # Destination-side stage-and-publish. Empty means a one-shot put.
  commitToken @3 :Text;
  # When true, `put` stages remotely and does not publish until `commit`.
  stageOnly @4 :Bool;
}

struct PutResult {
  key @0 :Text;
  bytesWritten @1 :UInt64;
  etag @2 :Text;
  sha256 @3 :Data;
}

struct CopyResult {
  bytesCopied @0 :UInt64;
}

struct PluginDescribe {
  apiVersion @0 :UInt32;
  id @1 :Text;
  kind @2 :Text;
  displayName @3 :Text;
  rpcFeatures @4 :List(Text);
  scalarLimits @5 :ScalarLimits;
  abiMajor @6 :UInt32;
  abiMinor @7 :UInt32;
  # Advertised factories (`destination`, `source`, `worker`, `contentSource`,
  # `integration`, `database`). Host still intersects with the manifest allowlist.
  supportedRoles @8 :List(Text);
  # Handshake-era identity extras (brand, cli schema, method names, aliases).
  # Versioned JSON escape hatch; not a substitute for typed fields.
  metadataJson @9 :Text;
}

# Bookclerk-as-IdP relying-party template. Plugins declare callback path and
# client id; the host materializes `oidc_clients` rows and remains the AS.
# `originConfigKey` is a dotted config path (e.g. integrations.audiobookshelf.base_url).
struct OidcClientTemplate {
  clientId @0 :Text;
  displayName @1 :Text;
  callbackPath @2 :Text;
  publicClient @3 :Bool;
  defaultScopes @4 :List(Text);
  issueRefreshToken @5 :Bool;
  originConfigKey @6 :Text;
}

struct OidcClientsOk {
  clients @0 :List(OidcClientTemplate);
}

struct OidcClientsReply {
  union {
    ok @0 :OidcClientsOk;
    err @1 :PluginError;
  }
}

# Plugin-specific extensible config. Not a substitute for typed ABI fields.
struct ExtensibleConfig {
  schemaVersion @0 :UInt32;
  mediaType @1 :Text;
  payload @2 :Data;
}

# Opaque JSON knobs only (migration bridge). Prefer `config` for new fields.
# OS paths, FDs, and sockets are transport-private.
struct DestinationContext {
  json @0 :Text;
  config @1 :ExtensibleConfig;
}

struct SourceContext {
  json @0 :Text;
  config @1 :ExtensibleConfig;
}

struct WorkerContext {
  jobId @0 :Text;
  json @1 :Text;
  config @2 :ExtensibleConfig;
}

struct ContentSourceContext {
  json @0 :Text;
  config @1 :ExtensibleConfig;
}

struct IntegrationContext {
  json @0 :Text;
  config @1 :ExtensibleConfig;
}

struct DatabaseContext {
  json @0 :Text;
  config @1 :ExtensibleConfig;
}

# Durable command envelope (not a domain event). Envelope version and command
# payload schema version are independent. Idempotency keys are scoped to
# (account, plugin, commandType) until a terminal fenced outcome is committed.
struct JobInvocation {
  envelopeVersion @0 :UInt32;
  payloadSchemaVersion @1 :UInt32;
  invocationId @2 :Text;
  commandType @3 :Text;
  payloadJson @4 :Text;
  idempotencyKey @5 :Text;
  attempt @6 :UInt32;
  correlationId @7 :Text;
  causationId @8 :Text;
  # UTC Unix milliseconds. Host fence/lease is authoritative; this hint must
  # not outlive the fence (clock skew across VPS nodes).
  deadlineUnixMs @9 :UInt64;
  checkpointJson @10 :Text;
  checkpointSchemaVersion @11 :UInt32;
  # Resume ordinal; distinct from failure `attempt`.
  invocationSequence @12 :UInt32;
  stepId @13 :Text;
}

struct CompletedOutcome {
  message @0 :Text;
  bytesCopied @1 :UInt64;
}

struct RetryableOutcome {
  message @0 :Text;
  retryAfterUnixMs @1 :UInt64;
}

struct RejectedOutcome {
  message @0 :Text;
}

struct CancelledOutcome {
  message @0 :Text;
}

struct SuspendedOutcome {
  checkpointJson @0 :Text;
  checkpointSchemaVersion @1 :UInt32;
  wakeAtUnixMs @2 :UInt64;
}

struct JobOutcome {
  union {
    completed @0 :CompletedOutcome;
    retryable @1 :RetryableOutcome;
    rejected @2 :RejectedOutcome;
    cancelled @3 :CancelledOutcome;
    suspended @4 :SuspendedOutcome;
  }
}

# Domain event (not a job). Outbox-produced, at-least-once, idempotent consume.
struct DomainEvent {
  eventId @0 :Text;
  eventType @1 :Text;
  schemaVersion @2 :UInt32;
  occurredAtUnixMs @3 :UInt64;
  accountId @4 :Text;
  correlationId @5 :Text;
  causationId @6 :Text;
  deduplicationKey @7 :Text;
  deliveryAttempt @8 :UInt32;
  payload @9 :Data;
  # Append-only (abiMinor 5). Resume a prior EventResult.suspended.
  checkpointJson @10 :Text;
  checkpointSchemaVersion @11 :UInt32;
  invocationSequence @12 :UInt32;
  resumePending @13 :Bool;
  # Append-only (abiMinor 6). Producer plugin id; empty when unknown.
  source @14 :Text;
}

struct EventAck {
  dummy @0 :Void;
}

struct EventRetry {
  retryAtUnixMs @0 :UInt64;
  reason @1 :Text;
}

struct EventReject {
  reason @0 :Text;
}

struct EventDeadLetter {
  reason @0 :Text;
}

# Append-only (abiMinor 4). Mirrors job SuspendedOutcome; event handlers
# persist a bounded checkpoint and release the process until wakeAtUnixMs.
# abiMinor 6 adds optional wake-on-matching-event fields (empty = timestamp-only).
struct EventSuspended {
  checkpointJson @0 :Text;
  checkpointSchemaVersion @1 :UInt32;
  wakeAtUnixMs @2 :UInt64;
  wakeOnEventType @3 :Text;
  wakeOnFilterJson @4 :Text;
}

struct EventResult {
  union {
    ack @0 :EventAck;
    retry @1 :EventRetry;
    reject @2 :EventReject;
    deadLetter @3 :EventDeadLetter;
    suspended @4 :EventSuspended;
  }
}

struct HeadOk {
  found @0 :Bool;
  meta @1 :ObjectMetadata;
}

struct HeadReply {
  union {
    ok @0 :HeadOk;
    err @1 :PluginError;
  }
}

struct ListReply {
  union {
    ok @0 :ListPage;
    err @1 :PluginError;
  }
}

struct GetOk {
  meta @0 :ObjectMetadata;
  body @1 :ByteSource;
}

struct GetReply {
  union {
    ok @0 :GetOk;
    err @1 :PluginError;
  }
}

struct PutReply {
  union {
    ok @0 :PutResult;
    err @1 :PluginError;
  }
}

struct CopyReply {
  union {
    ok @0 :CopyResult;
    err @1 :PluginError;
  }
}

struct EmptyReply {
  union {
    ok @0 :Void;
    err @1 :PluginError;
  }
}

struct PullOk {
  chunk @0 :Data;
  done @1 :Bool;
}

struct PullReply {
  union {
    ok @0 :PullOk;
    err @1 :PluginError;
  }
}

struct OpenOk {
  meta @0 :ObjectMetadata;
  body @1 :ByteSource;
}

struct OpenReply {
  union {
    ok @0 :OpenOk;
    err @1 :PluginError;
  }
}

struct DescribeReply {
  union {
    ok @0 :PluginDescribe;
    err @1 :PluginError;
  }
}

struct DestinationReply {
  union {
    ok @0 :Destination;
    err @1 :PluginError;
  }
}

struct SourceReply {
  union {
    ok @0 :Source;
    err @1 :PluginError;
  }
}

struct WorkerReply {
  union {
    ok @0 :JobHandler;
    err @1 :PluginError;
  }
}

struct HandleReply {
  union {
    ok @0 :JobOutcome;
    err @1 :PluginError;
  }
}

struct ContentSourceReply {
  union {
    ok @0 :ContentSource;
    err @1 :PluginError;
  }
}

struct IntegrationReply {
  union {
    ok @0 :Integration;
    err @1 :PluginError;
  }
}

struct DatabaseReply {
  union {
    ok @0 :Database;
    err @1 :PluginError;
  }
}

struct EventResultReply {
  union {
    ok @0 :EventResult;
    err @1 :PluginError;
  }
}

# Migration-bridge JSON result. Frozen methods should prefer typed structs;
# plugin-specific DTOs travel as schemaVersion + mediaType + bounded payload
# via ExtensibleConfig, not as unbounded serde dumps.
struct JsonOk {
  json @0 :Text;
}

struct JsonReply {
  union {
    ok @0 :JsonOk;
    err @1 :PluginError;
  }
}

struct HealthOk {
  ok @0 :Bool;
  detail @1 :Text;
}

struct HealthReply {
  union {
    ok @0 :HealthOk;
    err @1 :PluginError;
  }
}

struct Statement {
  sql @0 :Text;
  valuesJson @1 :Text;
}

struct ExecResult {
  lastInsertId @0 :Int64;
  rowsAffected @1 :UInt64;
}

struct QueryPage {
  rowsJson @0 :Text;
  nextCursor @1 :Text;
}

struct ExecReply {
  union {
    ok @0 :ExecResult;
    err @1 :PluginError;
  }
}

struct QueryReply {
  union {
    ok @0 :QueryPage;
    err @1 :PluginError;
  }
}

struct SessionReply {
  union {
    ok @0 :DatabaseSession;
    err @1 :PluginError;
  }
}

struct TransactionReply {
  union {
    ok @0 :Transaction;
    err @1 :PluginError;
  }
}

# Transferred readable byte stream. The capability *is* the stream; callers
# pull bounded windows. Abort is capability drop / RPC cancel. A failed pull
# MUST set `err` — never a successful empty EOF.
interface ByteSource {
  pull @0 (maxBytes :UInt32) -> (result :PullReply);
}

interface Destination {
  head @0 (key :Text) -> (result :HeadReply);
  list @1 (options :ListOptions) -> (result :ListReply);
  get @2 (key :Text, options :ReadOptions) -> (result :GetReply);
  put @3 (key :Text, body :ByteSource, options :WriteOptions) -> (result :PutReply);
  copy @4 (from :Text, to :Text) -> (result :CopyReply);
  delete @5 (key :Text) -> (result :EmptyReply);
  # Finalize a destination-side staged object. Staging itself is `put` with
  # `stageOnly = true`; bytes must stream into destination-managed temp/multipart
  # storage, never a complete local spool on host/adapter/broker/guest.
  commit @6 (key :Text, commitToken :Text) -> (result :PutReply);
  abortStage @7 (key :Text, commitToken :Text) -> (result :EmptyReply);
}

interface Source {
  open @0 (key :Text) -> (result :OpenReply);
}

interface ProgressSink {
  report @0 (percent :Float32, message :Text) -> (result :EmptyReply);
}

# Transport cancellation. SDKs project this into a locally created AbortSignal
# (AbortSignal is not a serializable Workers RPC value).
interface Cancellation {
  poll @0 () -> (cancelled :Bool);
}

interface JobHandler {
  handle @0 (
      invocation :JobInvocation,
      input :Source,
      output :Destination,
      progress :ProgressSink,
      cancel :Cancellation)
      -> (result :HandleReply);
}

# Storefront content source (not byte Source). JSON params/results are a
# migration bridge for existing storefront DTOs.
interface ContentSource {
  login @0 (paramsJson :Text) -> (result :JsonReply);
  scan @1 (paramsJson :Text) -> (result :JsonReply);
  fetchTitle @2 (paramsJson :Text) -> (result :JsonReply);
  listAccounts @3 () -> (result :JsonReply);
  loginStart @4 (paramsJson :Text) -> (result :JsonReply);
  loginComplete @5 (paramsJson :Text) -> (result :JsonReply);
  searchCatalog @6 (paramsJson :Text) -> (result :JsonReply);
  expandCandidates @7 (paramsJson :Text) -> (result :JsonReply);
  purchaseHint @8 (paramsJson :Text) -> (result :JsonReply);
  listDeals @9 (paramsJson :Text) -> (result :JsonReply);
  health @10 () -> (result :HealthReply);
  diagnose @11 () -> (result :JsonReply);
  catalogDetail @12 (paramsJson :Text) -> (result :JsonReply);
}

interface Integration {
  health @0 () -> (result :HealthReply);
  onEvent @1 (event :DomainEvent) -> (result :EventResultReply);
  start @2 () -> (result :EmptyReply);
  stop @3 () -> (result :EmptyReply);
  diagnose @4 () -> (result :JsonReply);
  scanLibrary @5 (paramsJson :Text) -> (result :EmptyReply);
  syncListening @6 () -> (result :JsonReply);
  authenticateUser @7 (paramsJson :Text) -> (result :JsonReply);
  pollEvents @8 () -> (result :JsonReply);
}

interface Database {
  openSession @0 () -> (result :SessionReply);
}

# Invocation-scoped. Must not survive suspension.
interface DatabaseSession {
  execute @0 (statement :Statement) -> (result :ExecReply);
  query @1 (statement :Statement, cursor :Text, limit :UInt32) -> (result :QueryReply);
  begin @2 () -> (result :TransactionReply);
  close @3 () -> (result :EmptyReply);
}

interface Transaction {
  execute @0 (statement :Statement) -> (result :ExecReply);
  query @1 (statement :Statement, cursor :Text, limit :UInt32) -> (result :QueryReply);
  commit @2 () -> (result :EmptyReply);
  rollback @3 () -> (result :EmptyReply);
}

interface BookclerkPlugin {
  describe @0 () -> (result :DescribeReply);
  destination @1 (context :DestinationContext) -> (result :DestinationReply);
  source @2 (context :SourceContext) -> (result :SourceReply);
  worker @3 (context :WorkerContext) -> (result :WorkerReply);
  shutdown @4 () -> (result :EmptyReply);
  contentSource @5 (context :ContentSourceContext) -> (result :ContentSourceReply);
  integration @6 (context :IntegrationContext) -> (result :IntegrationReply);
  database @7 (context :DatabaseContext) -> (result :DatabaseReply);
  cliDescribe @8 () -> (result :JsonReply);
  cliInvoke @9 (paramsJson :Text) -> (result :JsonReply);
  # Plugin-provided OIDC AS client templates. Empty list when unused.
  # Hosts ignore `unsupported` from older abiMinor guests.
  oidcClients @10 () -> (result :OidcClientsReply);
}
