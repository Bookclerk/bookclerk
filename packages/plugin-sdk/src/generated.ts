/**
 * TypeScript projection of `crates/bookclerk-plugin-abi/schema/abi.json`.
 *
 * Field names are camelCase to match the Workers RPC wire format. Keep in sync
 * with the schema and the Rust types in `bookclerk-plugin-abi`. Run
 * `npm run check-schema` to assert the authoritative JSON still exists.
 *
 * See also `docs/plugins.md` for the guest contract narrative.
 */

/**
 * Wire API version negotiated during handshake for all guests.
 */
export const API_VERSION = 1 as const;

/**
 * Plugin surface kind advertised in handshake and `plugin.toml`.
 */
export type PluginKind = "source" | "integration" | "output" | "database";

/**
 * Stable error codes carried on {@link PluginError.code}.
 */
export type PluginErrorCode =
  | "invalid_params"
  | "unauthorized"
  | "forbidden"
  | "not_found"
  | "unavailable"
  | "unsupported"
  | "internal";

/**
 * RPC or plugin failure payload returned on the wire.
 */
export interface PluginError {
  /** Stable machine-readable failure code. */
  code: PluginErrorCode;
  /** Human-readable error message (secrets must already be redacted). */
  message: string;
  /** Optional structured details for operators or host UIs. */
  details?: Record<string, unknown>;
}

/**
 * Loose JSON object used for config blobs and CLI argument maps.
 */
export type JsonObject = Record<string, unknown>;

/**
 * Parameters for the required `handshake` RPC method.
 */
export interface HandshakeParams {
  /** Host-offered ABI version (must be {@link API_VERSION}). */
  apiVersion: typeof API_VERSION | 1;
  /** Install / operator config object passed into the guest. */
  config: JsonObject;
}

/**
 * Brand colors and icon for UI chrome.
 */
export interface BrandDto {
  /** Stable brand identifier (often matches plugin id). */
  id: string;
  /** Display name shown beside the brand mark in Accounts / Settings chrome. */
  name: string;
  /** Brand panel background as a CSS color (`#rrggbb`, `rgb()`, named color, …). */
  bg: string;
  /** Brand panel foreground / text as a CSS color string. */
  fg: string;
  /** Highlight / CTA accent as a CSS color string. */
  accent: string;
  /** Absolute `https://` URL or relative path for the brand icon asset. */
  iconUrl: string;
}

/**
 * One selectable value under a {@link ConfigOptionDto}.
 */
export interface ConfigOptionValueDto {
  /** Machine id written into config when the operator selects this value. */
  id: string;
  /** Operator-facing label shown in the option picker. */
  label: string;
}

/**
 * Config option descriptor discovered during handshake for sources.
 */
export interface ConfigOptionDto {
  /** Config key written under the plugin's config table when a value is chosen. */
  key: string;
  /** Operator-facing label for the option group in Settings. */
  label: string;
  /** Allowed values the operator may pick for this option. */
  values: ConfigOptionValueDto[];
}

/**
 * CLI argument value kind for plugin-declared commands.
 */
export type CliArgKind = "string" | "bool" | "int" | "path";

/**
 * One CLI argument declared by a plugin command.
 */
export interface CliArgSpec {
  /** Internal argument name (also the default long flag when `long` is omitted). */
  name: string;
  /** Long flag spelling without the leading `--` (for example `message`). */
  long?: string;
  /** Single-character short flag without the leading `-` (for example `m`). */
  short?: string;
  /** Value kind; defaults to `string` when omitted. */
  kind?: CliArgKind;
  /** When true, the host must supply this argument or the invoke fails. */
  required?: boolean;
  /** Default value encoded as a string when the flag is omitted. */
  default?: string;
  /** Short help text rendered next to the flag in `bookclerk plugin` help. */
  about?: string;
  /** When true, the argument is positional rather than a `--flag`. */
  positional?: boolean;
}

/**
 * One plugin CLI command declared in handshake / `cliDescribe`.
 */
export interface CliCommandSpec {
  /** Command name passed as `cliInvoke.command` (for example `ping`). */
  name: string;
  /** Short help text rendered in `bookclerk plugin <id> --help`. */
  about?: string;
  /** Arguments accepted by this command (order preserved for positionals). */
  args?: CliArgSpec[];
}

/**
 * Declared CLI surface returned from handshake or `cliDescribe`.
 */
export interface CliSchema {
  /** Commands the guest exposes to `bookclerk plugin` / host plumbing. */
  commands?: CliCommandSpec[];
}

/**
 * Result of a successful `handshake` negotiation.
 */
export interface HandshakeResult {
  /** Negotiated ABI version (must match {@link API_VERSION}). */
  apiVersion: typeof API_VERSION | 1;
  /** Globally unique plugin id (`[a-z0-9_]{2,32}` grammar). */
  id: string;
  /** Plugin kind (`source`, `integration`, `output`, or `database`). */
  kind: PluginKind | string;
  /** Optional operator-facing display name. */
  displayName?: string;
  /** Capability method names this guest implements. */
  capabilities: string[];
  /** Optional UI brand block. */
  brand?: BrandDto;
  /** Optional config options for the Accounts / Settings UI. */
  configOptions?: ConfigOptionDto[];
  /** Optional CLI schema (may also be returned from `cliDescribe`). */
  cli?: CliSchema;
  /** Portal login mode when the guest supports connect-portal auth. */
  portalAuthMode?: "oauth" | "password";
  /** Env var name holding a password for password-mode portal auth. */
  passwordEnvVar?: string;
  /** Alternate ids accepted by the host for this install. */
  aliases?: string[];
  /** Sort key for stable ordering in UI lists (lower first). */
  sortKey?: number;
}

/**
 * Parameters for the `cliInvoke` RPC method.
 */
export interface CliInvokeParams {
  /** Command name matching a {@link CliCommandSpec.name}. */
  command: string;
  /** Named argument map (flag names → values). */
  args?: JsonObject;
}

/**
 * Result of a `cliInvoke` call.
 */
export interface CliInvokeResult {
  /** Process-style exit code (`0` = success; non-zero surfaces as a CLI failure). */
  exitCode?: number;
  /** Captured stdout text shown to the operator. */
  stdout?: string;
  /** Captured stderr text shown to the operator on failure or diagnostics. */
  stderr?: string;
  /** Optional structured JSON payload alongside text output for machine consumers. */
  json?: unknown;
}

/**
 * Result of the `health` RPC method.
 */
export interface HealthResult {
  /** Whether the guest considers itself healthy enough for host scheduling. */
  ok: boolean;
  /** Optional plugin id echo so host adapters can correlate multi-guest probes. */
  id?: string;
  /** Optional enablement flag when the guest mirrors operator config state. */
  enabled?: boolean;
  /** Optional human-readable health detail for Status / doctor UIs. */
  detail?: string;
}

/**
 * Result of the `diagnose` RPC method (`plugins doctor`).
 */
export interface DiagnoseResult {
  /** Operator-facing diagnostic lines (empty when nothing to report). */
  lines: string[];
}

/**
 * Host → plugin payload when a title has been acquired.
 */
export interface BookAcquiredPayload {
  /** Library title id assigned by the host after upsert. */
  titleId: string;
  /** Source plugin / storefront id that produced the title. */
  source: string;
  /** Optional Audible ASIN when the title originated from Audible. */
  asin?: string;
  /** Optional ISBN when the title carries one. */
  isbn?: string;
  /** Destination object keys written for this acquire (one per enabled destination). */
  pathKeys: string[];
}

/**
 * Host → plugin payload when a library scan finishes.
 */
export interface LibraryScanCompletedPayload {
  /** Source plugin / storefront id that was scanned. */
  source: string;
  /** Number of titles upserted into the library during this scan. */
  upserted: number;
}

/**
 * Host → plugin payload when operator config changes.
 */
export interface ConfigChangedPayload {
  /** Updated config object for the guest install. */
  config: JsonObject;
}

/**
 * Host → plugin event envelope delivered via `onEvent`.
 *
 * Discriminated by `type` using snake_case event names from the ABI schema.
 */
export type HostToPluginEvent =
  | { type: "book_acquired"; payload: BookAcquiredPayload }
  | { type: "library_scan_completed"; payload: LibraryScanCompletedPayload }
  | { type: "config_changed"; payload: ConfigChangedPayload };

/**
 * Severity for {@link PluginLogPayload} events pushed to the host.
 */
export type PluginLogLevel = "debug" | "info" | "warn" | "error";

/**
 * Plugin → host payload listing external users for portal sync.
 */
export interface ExternalUsersPayload {
  /** Opaque user records understood by the host integration layer. */
  users: unknown[];
}

/**
 * Plugin → host payload for listening-progress sync.
 */
export interface ListeningProgressPayload {
  /** Opaque progress items understood by the host. */
  items: unknown[];
}

/**
 * Plugin → host structured log line.
 */
export interface PluginLogPayload {
  /** Log severity forwarded to the host diagnostics ring (`debug`…`error`). */
  level: PluginLogLevel;
  /** Message text (secrets must already be redacted before notify). */
  message: string;
}

/**
 * Plugin → host event envelope sent through `env.HOST.notify`.
 *
 * Discriminated by `type` using snake_case event names from the ABI schema.
 */
export type PluginToHostEvent =
  | { type: "external_users"; payload: ExternalUsersPayload }
  | { type: "listening_progress"; payload: ListeningProgressPayload }
  | { type: "plugin_log"; payload: PluginLogPayload };

/**
 * Host binding used by guests to push {@link PluginToHostEvent} notifications.
 */
export interface HostBinding {
  /**
   * Delivers a plugin → host event on the reverse notify channel.
   *
   * @param event - Event envelope to send.
   * @returns Resolves when the host acknowledges the notify.
   */
  notify(event: PluginToHostEvent): Promise<void>;
}

/**
 * Guest `env` bindings declared by the ABI / `capabilities.bindings`.
 *
 * Only `HOST` is required for event push; other bindings appear when the
 * operator has consented to them in `plugin.toml`.
 */
export interface BookclerkEnv {
  /** Reverse channel for plugin → host events. */
  HOST: HostBinding;
  /** Operator config object when the `config` binding is enabled. */
  CONFIG?: JsonObject;
  /** Sealed secrets binding when the `secrets` capability is enabled. */
  SECRETS?: unknown;
  /** Per-plugin KV store when the `plugin_kv` binding is enabled. */
  PLUGIN_KV?: unknown;
  /** Work filesystem binding when the `work_fs` capability is enabled. */
  WORK_FS?: unknown;
  /** OAuth helper binding when the `oauth` capability is enabled. */
  OAUTH?: unknown;
}

/**
 * Optional object metadata attached to destination writes.
 *
 * Wire field names are camelCase (`contentType`, `contentLength`, …).
 */
export interface ObjectMetaDto {
  /** MIME type stored with the object when the destination supports it. */
  contentType?: string;
  /** Declared content length in bytes when known up front. */
  contentLength?: number;
  /** Optional Audible ASIN associated with this object. */
  asin?: string;
  /** Optional human title associated with this object. */
  title?: string;
  /** Creation timestamp as an ISO-8601 or destination-native string. */
  creationTime?: string;
  /** Last-write timestamp as an ISO-8601 or destination-native string. */
  lastWriteTime?: string;
}

/**
 * Shared S3 / object-store context flattened into destination method params.
 *
 * Wire field names are camelCase (`pluginDataDir`, `forcePathStyle`, …).
 */
export interface OutputS3Context {
  /** Scoped writable directory for this plugin (`…/plugins/<id>/data`). */
  pluginDataDir: string;
  /** Destination bucket name (S3-compatible stores). */
  bucket: string;
  /** Key prefix applied before object keys. */
  prefix: string;
  /** AWS / S3-compatible region identifier. */
  region: string;
  /** Optional custom endpoint (MinIO, R2, …); may be host-only without a scheme. */
  endpoint?: string;
  /** When true, use path-style URLs instead of virtual-hosted-style. */
  forcePathStyle?: boolean;
  /** Host-injected credential blob; guests must not read env for these secrets. */
  credentials?: JsonObject;
}

/**
 * Parameters for integration `scanLibrary`.
 *
 * Wire: `force` (boolean). Additional kind-specific keys may appear.
 */
export interface ScanLibraryParams {
  /** When true, re-scan even if the host believes the library is current. */
  force?: boolean;
  /** Additional kind-specific options forwarded by the host. */
  [key: string]: unknown;
}

/**
 * Parameters for `authenticateUser` (connect-portal / OIDC helpers).
 *
 * Wire field names are camelCase (`username`, `password`).
 */
export interface AuthenticateUserParams {
  /** External username supplied by the connect portal. */
  username?: string;
  /** External password or secret supplied by the connect portal. */
  password?: string;
  /** Additional portal fields forwarded by the host. */
  [key: string]: unknown;
}

/**
 * Parameters for one-shot `login` and interactive `loginStart`.
 *
 * Wire field names are camelCase (`pluginDataDir`, `callbackBind`, `timeoutSecs`, …).
 */
export interface LoginParams {
  /** Scoped writable directory for this plugin (`…/plugins/<id>/data`). */
  pluginDataDir: string;
  /** Marketplace / locale code the storefront expects (for example `us`, `uk`). */
  marketplace?: string;
  /** Operator-facing account label shown in the Accounts UI. */
  label?: string;
  /** Account email for password-mode storefronts. */
  email?: string;
  /** Account password for password-mode storefronts (never log or persist plainly). */
  password?: string;
  /** When true, overwrite an existing sealed credential blob for this account. */
  force?: boolean;
  /**
   * Optional bind address for a guest-owned OAuth callback server (`host:port`).
   * Ignored when {@link LoginParams.callbackIpc} is set (host owns the TCP listener).
   */
  callbackBind?: string;
  /**
   * Host-owned callback IPC endpoint the guest must connect to (Unix socket path
   * or Windows pipe name). When set with {@link LoginParams.callbackPublicBase},
   * the guest must not bind a TCP listener.
   */
  callbackIpc?: string;
  /**
   * Public base URL for the host TCP listener (for example `http://127.0.0.1:12345`).
   * Combined with the guest landing path to form the browser authorize URL.
   */
  callbackPublicBase?: string;
  /** When true, use external / paste-redirect OAuth instead of a local callback. */
  external?: boolean;
  /** Pre-supplied OAuth redirect URL when the operator pastes a callback. */
  responseUrl?: string;
  /** Prefer QR output when the guest supports presenting an authorize URL as QR. */
  showQr?: boolean;
  /** Seconds to wait for OAuth callback capture before timing out. */
  timeoutSecs?: number;
  /** Store-specific knobs; guests may ignore unknown keys. */
  extra?: JsonObject;
}

/**
 * Parameters for interactive `loginStart` (same shape as {@link LoginParams}).
 */
export type LoginStartParams = LoginParams;

/**
 * Parameters for interactive `loginComplete`.
 *
 * Wire field: `sessionId`.
 */
export interface LoginCompleteParams {
  /** Opaque session id returned by {@link LoginStartParams} / `loginStart`. */
  sessionId: string;
}

/**
 * Parameters for `credentialsUpdate` — guest-requested credential write-back.
 *
 * Wire field names are camelCase (`accountId`, `credentials`).
 */
export interface CredentialsUpdateParams {
  /** Account id whose sealed credential blob should be replaced. */
  accountId: string;
  /** Replacement opaque credential JSON for the host to re-seal. */
  credentials: JsonObject;
}

/**
 * Parameters for source `scan`.
 *
 * Host injects sealed credentials so the guest does not need a private store.
 * Wire field names are camelCase (`pluginDataDir`, `pageSize`, `importEpisodes`, …).
 */
export interface ScanParams {
  /** Scoped writable directory for this plugin (`…/plugins/<id>/data`). */
  pluginDataDir: string;
  /** Account ids to include; empty means all accounts known to the host. */
  accounts?: string[];
  /** Page size for storefront pagination (host default applies when omitted). */
  pageSize?: number;
  /** When true, import podcast / series episodes into the library. */
  importEpisodes?: boolean;
  /** When true, import Plus / catalog-included titles where the store supports it. */
  importPlusTitles?: boolean;
  /**
   * Host-loaded credential blobs keyed by `accountId`.
   * Values are the same opaque JSON sealed at `login`.
   */
  credentials?: Record<string, JsonObject>;
}

/**
 * Parameters for source `fetchTitle`.
 *
 * The guest writes media under `cacheDir` and returns plain paths. Wire field
 * names are camelCase (`pluginDataDir`, `accountId`, `titleId`, `cacheDir`, …).
 */
export interface FetchTitleParams {
  /** Scoped writable directory for this plugin (`…/plugins/<id>/data`). */
  pluginDataDir: string;
  /** Account that owns the title. */
  accountId: string;
  /** Library / storefront title identifier (ASIN, ISBN, UUID, …). */
  titleId: string;
  /** Absolute path to the guest download cache for this fetch. */
  cacheDir: string;
  /** Host-loaded credential blob for this account (sealed in DB). */
  credentials?: JsonObject;
  /** Opaque plugin table from `[sources.<id>]` in operator config. */
  sourceConfig?: JsonObject;
  /**
   * Host acquire/download options (JSON matching host `DownloadOptions`).
   * Guests should honor fetch-relevant knobs (`widevine`, cover download, …).
   */
  download?: JsonObject;
}

/**
 * Parameters for `searchCatalog`.
 *
 * Wire field names are camelCase (`query`, `region`, `limit`, …).
 */
export interface SearchCatalogParams {
  /** Free-text catalog query. */
  query: string;
  /** Marketplace / region code when the storefront is multi-market. */
  region?: string;
  /** Maximum number of hits to return. */
  limit?: number;
  /** 1-based page index for paginated catalogs. */
  page?: number;
  /** Store-specific sort key (for example `relevance`, `price`). */
  sort?: string;
  /** Optional field to search within when the storefront supports it. */
  field?: string;
  /** Preferred content language filter. */
  language?: string;
}

/**
 * Parameters for `expandCandidates`.
 *
 * Wire field names are camelCase (`productId`, `seriesAsin`, …).
 */
export interface ExpandCandidatesParams {
  /** Source plugin / storefront id that produced the seed. */
  source?: string;
  /** Seed product id in the storefront namespace. */
  productId?: string;
  /** Seed title text used when ids are unavailable. */
  title?: string;
  /** Seed author names. */
  authors?: string[];
  /** Seed narrator names. */
  narrators?: string[];
  /** Seed series name. */
  series?: string;
  /** Seed series ASIN when the storefront uses ASINs for series. */
  seriesAsin?: string;
  /** Seed Audible ASIN. */
  asin?: string;
  /** Seed ISBN. */
  isbn?: string;
  /** Marketplace / region for the expansion. */
  region?: string;
  /** Maximum number of related candidates to return. */
  limit?: number;
}

/**
 * Parameters for `purchaseHint`.
 *
 * Wire field names are camelCase (`productId`, `withPrice`, …).
 */
export interface PurchaseHintParams {
  /** Storefront product id to price / deep-link. */
  productId?: string;
  /** Title text used when the product id is unknown. */
  title?: string;
  /** Author names used for fuzzy purchase lookup. */
  authors?: string[];
  /** Audible ASIN when known. */
  asin?: string;
  /** ISBN when known. */
  isbn?: string;
  /** Marketplace / region for pricing. */
  region?: string;
  /** When true, include a price quote when the storefront exposes one. */
  withPrice?: boolean;
}

/**
 * Parameters for `listDeals`.
 *
 * Wire field: `limit`.
 */
export interface ListDealsParams {
  /** Maximum number of deal hits to return. */
  limit?: number;
}

/**
 * Parameters for `listAccounts` (typically an empty object today).
 */
export type ListAccountsParams = JsonObject;

/**
 * Parameters for `catalogDetail`.
 *
 * Wire field names are camelCase (`productId`, `isbn`).
 */
export interface CatalogDetailParams {
  /** Store product id (for example Libro ISBN or ISBN-slug). */
  productId: string;
  /** Optional ISBN when it differs from {@link CatalogDetailParams.productId}. */
  isbn?: string;
}

/**
 * Parameters for destination `put` (inline Base64 body).
 *
 * Extends {@link OutputS3Context}. Wire field names are camelCase
 * (`dataBase64`, `pluginDataDir`, …).
 */
export interface PutParams extends OutputS3Context {
  /** Destination object key (relative to `prefix`). */
  key: string;
  /** Base64-encoded object body (sidecars and small objects). */
  dataBase64: string;
  /** Optional object metadata to store with the bytes. */
  meta?: ObjectMetaDto;
}

/**
 * Parameters for destination `putFile`.
 *
 * Jailed guests often receive the local file over a side-channel FD; when no
 * side channel is wired, the host sets {@link PutFileParams.localPath}.
 */
export interface PutFileParams extends OutputS3Context {
  /** Destination object key (relative to `prefix`). */
  key: string;
  /** Optional object metadata to store with the upload. */
  meta?: ObjectMetaDto;
  /**
   * Absolute path to the local file when no SCM_RIGHTS side channel is wired
   * (unconfined / best-effort hosts).
   */
  localPath?: string;
}

/**
 * Parameters for key-scoped destination methods (`get`, `exists`, `probe`, `delete`).
 *
 * Extends {@link OutputS3Context}. Wire field: `key`.
 */
export interface KeyParams extends OutputS3Context {
  /** Destination object key (relative to `prefix`). */
  key: string;
}

/**
 * Parameters for destination `list`.
 *
 * Extends {@link OutputS3Context}; `prefix` is already part of the context and
 * also acts as the listing prefix filter.
 */
export type ListParams = OutputS3Context;

/**
 * Parameters for destination `copy`.
 *
 * Wire field names are camelCase (`from`, `to`, …).
 */
export interface CopyParams extends OutputS3Context {
  /** Source object key within the destination. */
  from: string;
  /** Destination object key within the same store. */
  to: string;
}

/**
 * Parameters for destination `touchFile`.
 *
 * Updates timestamps / metadata without rewriting object bytes.
 */
export interface TouchFileParams extends OutputS3Context {
  /** Destination object key (relative to `prefix`). */
  key: string;
  /** Optional creation timestamp to apply. */
  created?: string;
  /** Optional last-modified timestamp to apply. */
  modified?: string;
}

/**
 * Parameters for database `dbConnect`.
 *
 * Discriminated by wire field `backend` (`sqlite` | `d1` | `postgres`). Extra
 * fields depend on the backend (paths, account ids, connection URLs).
 */
export type DbConnectParams = JsonObject;

/**
 * SQL statement DTO for `dbQuery` / `dbExecute`.
 *
 * Wire field names are camelCase (`sql`, `values`).
 */
export interface StatementDto {
  /**
   * SQL text with positional or named placeholders as understood by the guest
   * dialect (SQLite `?`, Postgres `$1`, …).
   */
  sql: string;
  /**
   * Ordered bind values for the statement (null, bool, number, string, or nested
   * arrays matching the host RPC encoding). Defaults to an empty list.
   */
  values?: unknown[];
  /**
   * Guest transaction id from `dbBegin`. Omitted for autocommit statements.
   */
  txnId?: string;
}

/**
 * Parameters for database `dbBegin`.
 */
export interface DbBeginParams {
  /**
   * Existing transaction to nest a savepoint under. Omitted to start a
   * top-level transaction.
   */
  parentTxnId?: string;
}

/**
 * Result of a successful `dbBegin`.
 */
export interface DbBeginResult {
  /**
   * Opaque id the host must send on subsequent statements and commit/rollback.
   */
  txnId: string;
}

/**
 * Parameters for `dbCommit` / `dbRollback`.
 */
export interface DbTxnParams {
  /**
   * Transaction id returned by `dbBegin`.
   */
  txnId: string;
}

/**
 * Result of `dbConnect`. D1 sets `interactiveTxn` to false and implements
 * `dbAtomic` instead of interactive `dbBegin`.
 */
export interface DbConnectResult {
  /** SeaORM dialect (`sqlite` or `postgres`). D1 reports `sqlite`. */
  dialect: string;
  /**
   * When false, the host must not use SeaORM `begin()` / `dbBegin`.
   * Omitted by older guests (treated as true).
   */
  interactiveTxn?: boolean;
}

/**
 * Named atomic library operation for `dbAtomic`.
 * Tagged `op`: `deleteUser`, `redeemClaimTicket`, `takeOidcRpState`,
 * `takeWebauthnChallenge`, …
 */
export type DbAtomicParams = JsonObject;

/**
 * Idempotency envelope for `dbAtomic`.
 */
export interface DbAtomicRequest {
  /** Caller-chosen idempotency key; retries must reuse it. */
  operationId: string;
  operation: DbAtomicParams;
}

/**
 * Application result of `dbAtomic`.
 */
export interface DbAtomicResult {
  /** `ok`, `empty`, `notFound`, `lastOwner`, `claimInvalid`, `passwordRequired`, `idempotencyConflict`. */
  status: string;
  /** Library record JSON when `status` is `ok`. */
  payload?: unknown;
  operationId?: string;
  replayed?: boolean;
  receiptCreatedAt?: string;
  timing?: {
    attemptElapsedUs: number;
    dbExecutionUs?: number;
    dbTimingSource?: string;
  };
}

/**
 * Known core method names on the Workers RPC wire.
 */
export const METHOD_NAMES = [
  "handshake",
  "shutdown",
  "health",
  "diagnose",
  "start",
  "onEvent",
  "pollEvents",
  "scanLibrary",
  "syncListening",
  "authenticateUser",
  "cliDescribe",
  "cliInvoke",
  "login",
  "loginStart",
  "loginComplete",
  "credentialsUpdate",
  "scan",
  "fetchTitle",
  "searchCatalog",
  "expandCandidates",
  "purchaseHint",
  "listDeals",
  "listAccounts",
  "catalogDetail",
  "put",
  "putFile",
  "get",
  "exists",
  "list",
  "probe",
  "copy",
  "delete",
  "touchFile",
  "dbConnect",
  "dbPing",
  "dbQuery",
  "dbExecute",
  "dbBegin",
  "dbCommit",
  "dbRollback",
  "dbAtomic",
] as const;

/**
 * Union of known core RPC method names from {@link METHOD_NAMES}.
 */
export type MethodName = (typeof METHOD_NAMES)[number];
