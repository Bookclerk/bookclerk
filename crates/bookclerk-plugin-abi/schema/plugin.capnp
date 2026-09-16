# Bookclerk plugin ABI — object-capability Workers RPC (`api_version = 2`).
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
const maxScalarBytes :UInt32 = 262144;
const maxStreamWindowBytes :UInt32 = 1048576;
const maxListPage :UInt32 = 256;
const maxCheckpointBytes :UInt32 = 65536;
const maxIdentifierBytes :UInt32 = 64;
const maxConfigPayloadBytes :UInt32 = 65536;
const maxEventPayloadBytes :UInt32 = 65536;

# Negotiable `rpcFeatures` wire names (see `PluginDescribe.rpcFeatures`).
const featureScalarLimits :Text = "rpc.scalarLimits";
const featureStreams :Text = "rpc.streams";
const featureStorageCopy :Text = "storage.copy";

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
  # Advertised factories (`destination`, `source`, `worker`, `contentSource`,
  # `integration`, `database`). Host still intersects with the manifest allowlist.
  supportedRoles @6 :List(Text);
  # Identity extras (brand, cli schema, method names, aliases).
  # Versioned JSON escape hatch; not a substitute for typed fields.
  metadataJson @7 :Text;
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

# Durable command envelope (not a domain event). Command payload schema
# version and checkpoint schema versions are independent of the plugin ABI.
# The envelope itself is not persisted as opaque bytes across ABI majors.
# Idempotency keys are scoped to (account, plugin, commandType) until a
# terminal fenced outcome is committed.
struct JobInvocation {
  payloadSchemaVersion @0 :UInt32;
  invocationId @1 :Text;
  commandType @2 :Text;
  payloadJson @3 :Text;
  idempotencyKey @4 :Text;
  attempt @5 :UInt32;
  correlationId @6 :Text;
  causationId @7 :Text;
  # UTC Unix milliseconds. Host fence/lease is authoritative; this hint must
  # not outlive the fence (clock skew across VPS nodes).
  deadlineUnixMs @8 :UInt64;
  checkpointJson @9 :Text;
  checkpointSchemaVersion @10 :UInt32;
  # Resume ordinal; distinct from failure `attempt`.
  invocationSequence @11 :UInt32;
  stepId @12 :Text;
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
  # Append-only. Resume a prior EventResult.suspended.
  checkpointJson @10 :Text;
  checkpointSchemaVersion @11 :UInt32;
  invocationSequence @12 :UInt32;
  resumePending @13 :Bool;
  # Append-only. Producer plugin id; empty when unknown.
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

# Append-only. Mirrors job SuspendedOutcome; event handlers persist a
# bounded checkpoint and release the process until wakeAtUnixMs. Optional
# wake-on-matching-event fields (empty = timestamp-only).
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

struct AdapterSessionReply {
  union {
    ok @0 :AdapterDatabaseSession;
    err @1 :PluginError;
  }
}

struct GuestDatabaseReply {
  union {
    ok @0 :GuestDatabase;
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
      cancel :Cancellation,
      # Append-only. Host-mediated typed SQL session.
      database :GuestDatabase,
      # Append-only. Named plugin-owned database bindings
      # (Workers-style): each entry is an isolated database provisioned by
      # the active adapter, separate from the Bookclerk library and from
      # every other plugin. Empty when the manifest declares none.
      databases :List(NamedDatabase))
      -> (result :HandleReply);
}

# One named plugin-owned database binding delivered on `JobHandler.handle`.
struct NamedDatabase {
  # Binding name from `plugin.toml` `capabilities.bindings.databases`.
  name @0 :Text;
  # Isolated typed SQL session for this binding (plugin-owned schema).
  database @1 :GuestDatabase;
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

#############################################################################
# JSON payload contracts
#
# The structs below never travel as Cap'n Proto bytes. They are the schema
# for the JSON payloads carried inside `Text` fields of this ABI
# (`describe().metadataJson`, `ContentSource`/`Integration` `paramsJson`,
# `cliInvoke` params/results). Field names are the literal JSON keys
# (camelCase). SDK projections (TypeScript / Python) and drift checks against
# the Rust serde types are generated from these declarations by
# `scripts/gen-plugin-abi.py`.
#############################################################################

# Marks a JSON payload field that must be present (no default).
annotation required @0xab302cbc0dbdd123 (field) :Void;
# Marks a Text-typed field whose JSON value is an arbitrary JSON value or
# object (projected as a loose JSON type, not a string).
annotation jsonValue @0xe691f0f5a4b30449 (field) :Void;
# Marks an enum whose JSON wire strings are the snake_case form of the
# enumerant names (`payloadTooLarge` -> "payload_too_large").
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

# Identity extras carried as JSON in `describe().metadataJson`: portal auth,
# brand colors, config option discovery, and an embedded CLI schema.
struct PluginMetadata {
  # ABI version the guest speaks; must equal `apiVersion`.
  apiVersion @0 :UInt32 $required;
  # Stable plugin id matching `plugin.toml` / install directory name.
  id @1 :Text $required;
  # Plugin kind: "source", "integration", "output", or "database".
  kind @2 :Text $required;
  # Human-readable name for UI lists; omitted when absent.
  displayName @3 :Text;
  # Declared capability method names the guest implements (e.g. "health",
  # "login", "fetchTitle").
  capabilities @4 :List(Text);
  # Portal Accounts connect mode: "oauth" or "password".
  portalAuthMode @5 :Text;
  # Optional env var name operators may set for password helpers; never
  # required for Accounts UI connect.
  passwordEnvVar @6 :Text;
  # Alternate ids accepted for config / CLI targeting; omitted when empty.
  aliases @7 :List(Text);
  # Optional UI sort weight among peers of the same kind.
  sortKey @8 :UInt32;
  # Portal brand colors and icon URL for Accounts / library chrome.
  brand @9 :Brand;
  # Discoverable config option groups for source UIs.
  configOptions @10 :List(ConfigOption);
  # Optional embedded CLI schema (same shape as `cliDescribe`).
  cli @11 :CliSchema;
}

# Portal brand crossing the RPC boundary. Distinct from `plugin.toml`
# `logo`: `iconUrl` is the live URL or data URI the SPA renders.
struct Brand {
  # Brand id (often matches the plugin id).
  id @0 :Text $required;
  # Display name shown next to the brand swatch.
  name @1 :Text $required;
  # Background CSS color (hex or named).
  bg @2 :Text $required;
  # Foreground CSS color for text on `bg`.
  fg @3 :Text $required;
  # Accent CSS color for highlights / CTAs.
  accent @4 :Text $required;
  # Icon URL or data URI for the portal.
  iconUrl @5 :Text $required;
}

# One discoverable config option group advertised for sources.
struct ConfigOption {
  # Config key under the plugin's `config.toml` table.
  key @0 :Text $required;
  # Operator-facing label for the option group.
  label @1 :Text $required;
  # Allowed selectable values for this key.
  values @2 :List(ConfigOptionValue) $required;
}

# One selectable value under a `ConfigOption`.
struct ConfigOptionValue {
  # Value written to config when selected.
  id @0 :Text $required;
  # Operator-facing label for this value.
  label @1 :Text $required;
}

# Declared plugin CLI surface (`cliDescribe` / metadata `cli` / `plugin.toml`).
struct CliSchema {
  # Commands exposed as `bookclerk plugins <id> <command> ...`.
  commands @0 :List(CliCommandSpec);
}

# One plugin CLI command under `CliSchema`.
struct CliCommandSpec {
  # Command verb after the plugin id (for example "ping").
  name @0 :Text $required;
  # Short help text for `--help`; omitted when absent.
  about @1 :Text;
  # Argument / flag specs for this command (default empty).
  args @2 :List(CliArgSpec);
}

# Value kind for a `CliArgSpec` (wire lowercase: "string" / "bool" / ...).
enum CliArgKind $jsonEnum {
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
  # Internal arg name used as the key in `CliInvokeParams.args`.
  name @0 :Text $required;
  # Long flag without leading dashes (e.g. "message" -> `--message`).
  long @1 :Text;
  # Optional short flag character (e.g. "m" -> `-m`).
  short @2 :Text;
  # Parsed value kind (default "string").
  kind @3 :CliArgKind;
  # When true, the host rejects invoke if the arg is missing.
  required @4 :Bool;
  # Default string form when the operator omits the arg.
  default @5 :Text;
  # Help text for this arg; omitted when absent.
  about @6 :Text;
  # When true, the arg is positional rather than a flagged option.
  positional @7 :Bool;
}

# Params JSON for `cliInvoke`.
struct CliInvokeParams {
  # Command name matching a `CliCommandSpec.name`.
  command @0 :Text $required;
  # Named argument values (keys match `CliArgSpec.name`; default `{}`).
  args @1 :Text $jsonValue;
}

# Result JSON for `cliInvoke`.
struct CliInvokeResult {
  # Process-style exit code (0 = success).
  exitCode @0 :Int32;
  # Captured standard output text.
  stdout @1 :Text;
  # Captured standard error text.
  stderr @2 :Text;
  # Optional structured payload for machine consumers; omitted when absent.
  json @3 :Text $jsonValue;
}

# Author-facing database adapter configuration carried in
# `DatabaseContext.config` (mediaType
# `application/vnd.bookclerk.db-adapter-config+json`). This is the generic
# bootstrap mechanism for third-party adapters: the operator's granted
# `[database.<id>]` table plus the scoped writable data dir. First-party
# host-managed adapters receive host-private connect params instead.
struct DatabaseAdapterConfig {
  # Scoped writable directory for this plugin (`.../plugins/<id>/data`).
  pluginDataDir @0 :Text $required;
  # Granted plugin settings (operator `[database.<id>]` table) as a JSON
  # object; `{}` when the operator configured nothing.
  config @1 :Text $jsonValue;
  # Named plugin database binding this open serves; omitted for the primary
  # library open. Adapters advertising `DbCapabilities.pluginDatabases` must
  # serve each binding from its own isolated database.
  binding @2 :Text;
  # Host-issued opaque instance id for this (owner plugin, binding) pair.
  # Collision-resistant and stable across re-opens. Omitted for the primary
  # library open. Third-party adapters must key isolated databases on this
  # value rather than `binding` alone (two plugins may both declare `DB`).
  instanceId @3 :Text;
}

# JSON health payload for guests that report identity alongside liveness.
# Role-level `health` RPCs return the typed `HealthOk` instead.
struct HealthResult {
  # When true, the guest considers itself healthy enough for traffic.
  ok @0 :Bool;
  # Plugin id echo; omitted when the guest does not duplicate identity.
  id @1 :Text;
  # Whether the guest believes it is enabled in config; omitted when unknown.
  enabled @2 :Bool;
  # Short human detail for CLI / UI status lines; omitted when absent.
  detail @3 :Text;
}

# JSON result of `diagnose`. Each line is printed by
# `bookclerk plugins diagnose` / the control plane.
struct DiagnoseResult {
  # Human-readable probe lines (default empty).
  lines @0 :List(Text);
}

# Params JSON for `ContentSource.login`. Password sources fill
# email/password; OAuth sources use callback / external fields. There is no
# files-dir root or library DB path -- only `pluginDataDir`.
struct LoginParams {
  # Scoped writable directory for this plugin only (`.../plugins/<id>/data`).
  pluginDataDir @0 :Text $required;
  # Marketplace / locale for the storefront (default empty -> guest default).
  marketplace @1 :Text;
  # Optional operator label stored on the account row.
  label @2 :Text;
  # Account email / username for password logins; omitted for pure OAuth.
  email @3 :Text;
  # Account password for password logins; never logged; omitted for OAuth.
  password @4 :Text;
  # When true, overwrite an existing sealed credential for this account.
  force @5 :Bool;
  # Optional bind address for OAuth callback servers (`host:port`). Ignored
  # when `callbackIpc` is set (host owns the TCP listener).
  callbackBind @6 :Text;
  # Host-owned callback IPC endpoint the guest must connect to. When set
  # (with `callbackPublicBase`), the guest must not bind a TCP listener.
  callbackIpc @7 :Text;
  # Public base URL for the host TCP listener, e.g. `http://127.0.0.1:12345`.
  callbackPublicBase @8 :Text;
  # When true, use external / paste-redirect OAuth instead of a local
  # callback server.
  external @9 :Bool;
  # Pre-supplied OAuth redirect URL (paste flow); omitted otherwise.
  responseUrl @10 :Text;
  # Prefer QR output when the guest supports it.
  showQr @11 :Bool;
  # Seconds to wait for OAuth callback capture; guest default when omitted.
  timeoutSecs @12 :UInt64;
  # Store-specific knobs as a JSON object; guests may ignore unknowns.
  extra @13 :Text $jsonValue;
}

# Params JSON for `ContentSource.loginStart` -- same shape as `LoginParams`.
using LoginStartParams = LoginParams;

# Params JSON for `ContentSource.loginComplete`.
struct LoginCompleteParams {
  # Session id previously returned by `loginStart`.
  sessionId @0 :Text $required;
}

# Params JSON for `ContentSource.scan`. Host injects sealed credentials so
# the plugin does not need a private credential store under `pluginDataDir`.
struct ScanParams {
  # Scoped plugin data directory.
  pluginDataDir @0 :Text $required;
  # Account ids to scan; empty means all scan-enabled accounts.
  accounts @1 :List(Text);
  # Storefront page size (default 50).
  pageSize @2 :UInt32;
  # When true, import podcast/episode-style rows (default true).
  importEpisodes @3 :Bool;
  # When true, import Plus/catalog entitlement titles (default true).
  importPlusTitles @4 :Bool;
  # Host-loaded credential blobs keyed by account id (JSON object).
  credentials @5 :Text $jsonValue;
}

# Params JSON for `ContentSource.fetchTitle`. Plugin writes media under
# `cacheDir` and returns plain (DRM-free) paths. Host injects credentials;
# guests must not open `library.db` or `master.key`.
struct FetchTitleParams {
  # Scoped plugin data directory.
  pluginDataDir @0 :Text $required;
  # Account whose credentials apply.
  accountId @1 :Text $required;
  # Library / storefront title id to download.
  titleId @2 :Text $required;
  # Absolute path the guest should write media into (jail-granted TMPDIR).
  cacheDir @3 :Text $required;
  # Host-loaded credential blob for this account; omitted when unavailable.
  credentials @4 :Text $jsonValue;
  # Opaque plugin table from `[sources.<id>]`.
  sourceConfig @5 :Text $jsonValue;
  # Host acquire/download options (JSON object matching host DownloadOptions).
  download @6 :Text $jsonValue;
}

# Params JSON for `ContentSource.searchCatalog`.
struct SearchCatalogParams {
  # Free-text search query.
  query @0 :Text $required;
  # Storefront region / marketplace code (default empty -> guest default).
  region @1 :Text;
  # Maximum hits to return (default 20).
  limit @2 :UInt32;
  # 1-based page for storefronts that page (default 1).
  page @3 :UInt32;
  # Sort key: "relevance" / "popularity" / "rating" / "title" / "author".
  sort @4 :Text;
  # Optional facet ("author" / "narrator" / "series" / "genre").
  field @5 :Text;
  # Preferred content language (soft-prioritize; e.g. "en").
  language @6 :Text;
}

# Params JSON for `ContentSource.expandCandidates`. Seed fields identify a
# known title; the guest returns related catalog hits.
struct ExpandCandidatesParams {
  # Source plugin id hint when expanding across storefronts.
  source @0 :Text;
  # Seed storefront product id.
  productId @1 :Text;
  # Seed title text.
  title @2 :Text;
  # Seed authors string.
  authors @3 :Text;
  # Seed narrators string.
  narrators @4 :Text;
  # Seed series name.
  series @5 :Text;
  # Seed series ASIN when known.
  seriesAsin @6 :Text;
  # Seed Amazon ASIN.
  asin @7 :Text;
  # Seed ISBN.
  isbn @8 :Text;
  # Storefront region / marketplace code.
  region @9 :Text;
  # Maximum candidates to return (default 20).
  limit @10 :UInt32;
}

# Params JSON for `ContentSource.purchaseHint`. At least one identity field
# (`productId` / `asin` / `isbn` / title+authors) should be set; guests may
# return `invalid_params` when none are usable.
struct PurchaseHintParams {
  # Storefront product id when known.
  productId @0 :Text;
  # Title text for fuzzy lookup.
  title @1 :Text;
  # Authors string for fuzzy lookup.
  authors @2 :Text;
  # Amazon ASIN when known.
  asin @3 :Text;
  # ISBN when known.
  isbn @4 :Text;
  # Storefront region / marketplace code.
  region @5 :Text;
  # When true, guests should include live price fields when available.
  withPrice @6 :Bool;
}

# Params JSON for `ContentSource.listDeals`.
struct ListDealsParams {
  # Optional maximum number of deals to return; guest default when omitted.
  limit @0 :UInt32;
}

# Params JSON for `ContentSource.catalogDetail`.
struct CatalogDetailParams {
  # Store product id (Libro ISBN or ISBN-slug).
  productId @0 :Text $required;
  # Optional ISBN when it differs from `productId`.
  isbn @1 :Text;
}

# Params JSON for `Integration.scanLibrary` (remote library sync).
struct ScanLibraryParams {
  # When true, force a full rescan even if the guest would otherwise
  # incremental-sync.
  force @0 :Bool;
}

# Params JSON for `Integration.authenticateUser`.
struct AuthenticateUserParams {
  # Integration username / login id.
  username @0 :Text $required;
  # Integration password; never logged by the host.
  password @1 :Text $required;
}

#############################################################################
# End of JSON payload contracts
#############################################################################

# Universal database cell/parameter domain. Engine-native arrays, enums,
# unsigned integers, and JSON text sentinels are not baseline ABI values.
enum DbType {
  unspecified @0;
  bool @1;
  int64 @2;
  float64 @3;
  text @4;
  bytes @5;
}

struct DbValue {
  union {
    null @0 :DbType;
    boolean @1 :Bool;
    int64 @2 :Int64;
    float64 @3 :Float64;
    text @4 :Text;
    bytes @5 :Data;
  }
}

struct DbColumn {
  name @0 :Text;
  dbType @1 :DbType;
}

struct DbRow {
  values @0 :List(DbValue);
}

enum DbStatementKind {
  execute @0;
  select @1;
  returning @2;
}

enum DbResultSelection {
  discard @0;
  affectedRows @1;
  rows @2;
}

struct DbStatement {
  sql @0 :Text;
  parameters @1 :List(DbValue);
  kind @2 :DbStatementKind;
  maxRows @3 :UInt32;
  resultSelection @4 :DbResultSelection;
}

struct ExecuteRequest {
  operationId @0 :Text;
  requestHash @1 :Text;
  statements @2 :List(DbStatement);
  deadlineUnixMs @3 :UInt64;
}

struct StatementResult {
  rows @0 :List(DbRow);
  columns @1 :List(DbColumn);
  rowsAffected @2 :UInt64;
}

struct DbTiming {
  attemptElapsedUs @0 :UInt64;
  dbExecutionUs @1 :UInt64;
  dbTimingSource @2 :Text;
}

struct ExecuteReply {
  operationId @0 :Text;
  statements @1 :List(StatementResult);
  timing @2 :DbTiming;
}

struct ExecuteResultReply {
  union {
    ok @0 :ExecuteReply;
    err @1 :PluginError;
  }
}

# Semantic SQL-contract advertisement. Bootstrap metadata (`sqlFamily`,
# `dialect`) is not part of the capability plane — see `DbBootstrap`.
struct DbCapabilities {
  sqlContractVersion @0 :UInt32;
  atomicBatch @1 :Bool;
  returning @2 :Bool;
  affectedRows @3 :Bool;
  schemaMigrations @4 :Bool;
  pragmaUserVersion @5 :Bool;
  atomicSchemaBatch @6 :Bool;
  cancellation @7 :Bool;
  timing @8 :Bool;
  maxBinds @9 :UInt32;
  maxStatements @10 :UInt32;
  maxResultRows @11 :UInt32;
  maxPayloadBytes @12 :UInt32;
  maxResultBytes @13 :UInt32;
  maxCellBytes @14 :UInt32;
  maxRequestBytes @15 :UInt32;
  maxAtomicResultBytes @16 :UInt32;
  # Append-only. Adapter can open additional isolated sessions
  # for plugin-owned database bindings (per-binding file / schema / database).
  pluginDatabases @17 :Bool;
  # Maximum arguments in one physical function call after adapter hiding
  # (nested json_object / min / max / coalesce). `0` is unspecified.
  maxFunctionArgs @18 :UInt32;
  # Maximum columns in one CREATE TABLE / result row. `0` is unspecified.
  maxSchemaColumns @19 :UInt32;
  # Maximum UTF-8 bytes of a BookclerkSQL LIKE pattern value (literals and
  # TEXT binds). Adapters that expand LIKE into GLOB must advertise a
  # conservative value that still fits the physical pattern cap. `0` is
  # unspecified.
  maxPatternBytes @20 :UInt32;
  # Maximum UTF-8 bytes of sqlite-family lowered SQL the adapter can realize
  # for one statement (INTEGER overflow wraps, LIKE→GLOB, NULLIF, INSERT OR
  # IGNORE, query LIMIT wrap, bytes-placeholder expansion). `0` is
  # unspecified: the host does not enforce a lowered-size ceiling. First-party
  # D1 advertises 100000. Hosts compare a standardized Bookclerk lowering
  # upper bound against this number and must not branch on engine identity.
  maxLoweredStatementBytes @21 :UInt32;
}

struct DbBootstrapReply {
  union {
    ok @0 :DbBootstrap;
    err @1 :PluginError;
  }
}

struct DbBootstrap {
  sqlFamily @0 :Text;
  dialect @1 :Text;
}

struct DbCapabilitiesReply {
  union {
    ok @0 :DbCapabilities;
    err @1 :PluginError;
  }
}

interface Database {
  openSession @0 () -> (result :AdapterSessionReply);
}

# Host ↔ database adapter plugin. Capability negotiation + typed execute only.
interface AdapterDatabaseSession {
  capabilities @0 () -> (result :DbCapabilitiesReply);
  execute @1 (request :ExecuteRequest) -> (result :ExecuteResultReply);
  close @2 () -> (result :EmptyReply);
  # Bootstrap-only SeaORM proxy metadata (not part of DbCapabilities).
  bootstrap @3 () -> (result :DbBootstrapReply);
}

# Host-granted SQL for job plugin authors. SDK `DatabaseBinding` mirrors the
# Cloudflare Workers D1 surface (`prepare`/`bind`/`run`/`first`/`all`/`raw`,
# `batch`, `exec`) over this typed `execute` transport; wire types stay Cap'n
# `ExecuteRequest`/`ExecuteReply`.
interface GuestDatabase {
  execute @0 (request :ExecuteRequest) -> (result :ExecuteResultReply);
  close @1 () -> (result :EmptyReply);
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
  # Hosts ignore `unsupported` from older guests.
  oidcClients @10 () -> (result :OidcClientsReply);
}
