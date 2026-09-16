/**
 * GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.
 *
 * TypeScript projection of the JSON payload contracts declared in
 * `crates/bookclerk-plugin-abi/schema/plugin.capnp` (the payloads carried
 * inside `Text` fields of the Cap'n Proto ABI: `describe().metadataJson`,
 * role `paramsJson`, and `cliInvoke` params/results). Field names are the
 * literal JSON keys (camelCase).
 */

/** Arbitrary JSON value carried inside a payload field. */
export type JsonValue = unknown;

/** Loose JSON object used for config blobs and structured payloads. */
export type JsonObject = Record<string, unknown>;

/**
 * Stable `PluginError.code` strings. Unknown future codes are forwarded as-is; SDKs surface
 * them as a local `unknown` while keeping the raw wire code.
 */
export const PLUGIN_ERROR_CODES = ["invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict"] as const;

/** Union of known `PluginErrorCode` wire strings. */
export type PluginErrorCode = (typeof PLUGIN_ERROR_CODES)[number];

/**
 * Identity extras carried as JSON in `describe().metadataJson`: portal auth, brand colors,
 * config option discovery, and an embedded CLI schema.
 */
export interface PluginMetadata {
  /** ABI version the guest speaks; must equal `apiVersion`. */
  apiVersion: number;
  /** Stable plugin id matching `plugin.toml` / install directory name. */
  id: string;
  /** Plugin kind: "source", "integration", "output", or "database". */
  kind: string;
  /** Human-readable name for UI lists; omitted when absent. */
  displayName?: string;
  /**
   * Declared capability method names the guest implements (e.g. "health", "login",
   * "fetchTitle").
   */
  capabilities?: string[];
  /** Portal Accounts connect mode: "oauth" or "password". */
  portalAuthMode?: string;
  /**
   * Optional env var name operators may set for password helpers; never required for Accounts
   * UI connect.
   */
  passwordEnvVar?: string;
  /** Alternate ids accepted for config / CLI targeting; omitted when empty. */
  aliases?: string[];
  /** Optional UI sort weight among peers of the same kind. */
  sortKey?: number;
  /** Portal brand colors and icon URL for Accounts / library chrome. */
  brand?: Brand;
  /** Discoverable config option groups for source UIs. */
  configOptions?: ConfigOption[];
  /** Optional embedded CLI schema (same shape as `cliDescribe`). */
  cli?: CliSchema;
}

/**
 * Portal brand crossing the RPC boundary. Distinct from `plugin.toml` `logo`: `iconUrl` is
 * the live URL or data URI the SPA renders.
 */
export interface Brand {
  /** Brand id (often matches the plugin id). */
  id: string;
  /** Display name shown next to the brand swatch. */
  name: string;
  /** Background CSS color (hex or named). */
  bg: string;
  /** Foreground CSS color for text on `bg`. */
  fg: string;
  /** Accent CSS color for highlights / CTAs. */
  accent: string;
  /** Icon URL or data URI for the portal. */
  iconUrl: string;
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

/** Declared plugin CLI surface (`cliDescribe` / metadata `cli` / `plugin.toml`). */
export interface CliSchema {
  /** Commands exposed as `bookclerk plugins <id> <command> ...`. */
  commands?: CliCommandSpec[];
}

/** One plugin CLI command under `CliSchema`. */
export interface CliCommandSpec {
  /** Command verb after the plugin id (for example "ping"). */
  name: string;
  /** Short help text for `--help`; omitted when absent. */
  about?: string;
  /** Argument / flag specs for this command (default empty). */
  args?: CliArgSpec[];
}

/** Value kind for a `CliArgSpec` (wire lowercase: "string" / "bool" / ...). */
export const CLI_ARG_KINDS = ["string", "bool", "int", "path"] as const;

/** Union of known `CliArgKind` wire strings. */
export type CliArgKind = (typeof CLI_ARG_KINDS)[number];

/** One CLI argument or flag under a `CliCommandSpec`. */
export interface CliArgSpec {
  /** Internal arg name used as the key in `CliInvokeParams.args`. */
  name: string;
  /** Long flag without leading dashes (e.g. "message" -> `--message`). */
  long?: string;
  /** Optional short flag character (e.g. "m" -> `-m`). */
  short?: string;
  /** Parsed value kind (default "string"). */
  kind?: CliArgKind;
  /** When true, the host rejects invoke if the arg is missing. */
  required?: boolean;
  /** Default string form when the operator omits the arg. */
  default?: string;
  /** Help text for this arg; omitted when absent. */
  about?: string;
  /** When true, the arg is positional rather than a flagged option. */
  positional?: boolean;
}

/** Params JSON for `cliInvoke`. */
export interface CliInvokeParams {
  /** Command name matching a `CliCommandSpec.name`. */
  command: string;
  /** Named argument values (keys match `CliArgSpec.name`; default `{}`). */
  args?: JsonValue;
}

/** Result JSON for `cliInvoke`. */
export interface CliInvokeResult {
  /** Process-style exit code (0 = success). */
  exitCode?: number;
  /** Captured standard output text. */
  stdout?: string;
  /** Captured standard error text. */
  stderr?: string;
  /** Optional structured payload for machine consumers; omitted when absent. */
  json?: JsonValue;
}

/**
 * Author-facing database adapter configuration carried in `DatabaseContext.config` (mediaType
 * `application/vnd.bookclerk.db-adapter-config+json`). This is the generic bootstrap
 * mechanism for third-party adapters: the operator's granted `[database.<id>]` table plus the
 * scoped writable data dir. First-party host-managed adapters receive host-private connect
 * params instead.
 */
export interface DatabaseAdapterConfig {
  /** Scoped writable directory for this plugin (`.../plugins/<id>/data`). */
  pluginDataDir: string;
  /**
   * Granted plugin settings (operator `[database.<id>]` table) as a JSON object; `{}` when
   * the operator configured nothing.
   */
  config?: JsonValue;
  /**
   * Named plugin database binding this open serves; omitted for the primary library open.
   * Adapters advertising `DbCapabilities.pluginDatabases` must serve each binding from its
   * own isolated database.
   */
  binding?: string;
  /**
   * Host-issued opaque instance id for this (owner plugin, binding) pair. Collision-resistant
   * and stable across re-opens. Omitted for the primary library open. Third-party adapters
   * must key isolated databases on this value rather than `binding` alone (two plugins may
   * both declare `DB`).
   */
  instanceId?: string;
}

/**
 * JSON health payload for guests that report identity alongside liveness. Role-level `health`
 * RPCs return the typed `HealthOk` instead.
 */
export interface HealthResult {
  /** When true, the guest considers itself healthy enough for traffic. */
  ok?: boolean;
  /** Plugin id echo; omitted when the guest does not duplicate identity. */
  id?: string;
  /** Whether the guest believes it is enabled in config; omitted when unknown. */
  enabled?: boolean;
  /** Short human detail for CLI / UI status lines; omitted when absent. */
  detail?: string;
}

/**
 * JSON result of `diagnose`. Each line is printed by `bookclerk plugins diagnose` / the
 * control plane.
 */
export interface DiagnoseResult {
  /** Human-readable probe lines (default empty). */
  lines?: string[];
}

/**
 * Params JSON for `ContentSource.login`. Password sources fill email/password; OAuth sources
 * use callback / external fields. There is no files-dir root or library DB path -- only
 * `pluginDataDir`.
 */
export interface LoginParams {
  /** Scoped writable directory for this plugin only (`.../plugins/<id>/data`). */
  pluginDataDir: string;
  /** Marketplace / locale for the storefront (default empty -> guest default). */
  marketplace?: string;
  /** Optional operator label stored on the account row. */
  label?: string;
  /** Account email / username for password logins; omitted for pure OAuth. */
  email?: string;
  /** Account password for password logins; never logged; omitted for OAuth. */
  password?: string;
  /** When true, overwrite an existing sealed credential for this account. */
  force?: boolean;
  /**
   * Optional bind address for OAuth callback servers (`host:port`). Ignored when
   * `callbackIpc` is set (host owns the TCP listener).
   */
  callbackBind?: string;
  /**
   * Host-owned callback IPC endpoint the guest must connect to. When set (with
   * `callbackPublicBase`), the guest must not bind a TCP listener.
   */
  callbackIpc?: string;
  /** Public base URL for the host TCP listener, e.g. `http://127.0.0.1:12345`. */
  callbackPublicBase?: string;
  /** When true, use external / paste-redirect OAuth instead of a local callback server. */
  external?: boolean;
  /** Pre-supplied OAuth redirect URL (paste flow); omitted otherwise. */
  responseUrl?: string;
  /** Prefer QR output when the guest supports it. */
  showQr?: boolean;
  /** Seconds to wait for OAuth callback capture; guest default when omitted. */
  timeoutSecs?: number;
  /** Store-specific knobs as a JSON object; guests may ignore unknowns. */
  extra?: JsonValue;
}

/** Params JSON for `ContentSource.loginStart` -- same shape as `LoginParams`. */
export type LoginStartParams = LoginParams;

/** Params JSON for `ContentSource.loginComplete`. */
export interface LoginCompleteParams {
  /** Session id previously returned by `loginStart`. */
  sessionId: string;
}

/**
 * Params JSON for `ContentSource.scan`. Host injects sealed credentials so the plugin does
 * not need a private credential store under `pluginDataDir`.
 */
export interface ScanParams {
  /** Scoped plugin data directory. */
  pluginDataDir: string;
  /** Account ids to scan; empty means all scan-enabled accounts. */
  accounts?: string[];
  /** Storefront page size (default 50). */
  pageSize?: number;
  /** When true, import podcast/episode-style rows (default true). */
  importEpisodes?: boolean;
  /** When true, import Plus/catalog entitlement titles (default true). */
  importPlusTitles?: boolean;
  /** Host-loaded credential blobs keyed by account id (JSON object). */
  credentials?: JsonValue;
}

/**
 * Params JSON for `ContentSource.fetchTitle`. Plugin writes media under `cacheDir` and
 * returns plain (DRM-free) paths. Host injects credentials; guests must not open `library.db`
 * or `master.key`.
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
  /** Host-loaded credential blob for this account; omitted when unavailable. */
  credentials?: JsonValue;
  /** Opaque plugin table from `[sources.<id>]`. */
  sourceConfig?: JsonValue;
  /** Host acquire/download options (JSON object matching host DownloadOptions). */
  download?: JsonValue;
}

/** Params JSON for `ContentSource.searchCatalog`. */
export interface SearchCatalogParams {
  /** Free-text search query. */
  query: string;
  /** Storefront region / marketplace code (default empty -> guest default). */
  region?: string;
  /** Maximum hits to return (default 20). */
  limit?: number;
  /** 1-based page for storefronts that page (default 1). */
  page?: number;
  /** Sort key: "relevance" / "popularity" / "rating" / "title" / "author". */
  sort?: string;
  /** Optional facet ("author" / "narrator" / "series" / "genre"). */
  field?: string;
  /** Preferred content language (soft-prioritize; e.g. "en"). */
  language?: string;
}

/**
 * Params JSON for `ContentSource.expandCandidates`. Seed fields identify a known title; the
 * guest returns related catalog hits.
 */
export interface ExpandCandidatesParams {
  /** Source plugin id hint when expanding across storefronts. */
  source?: string;
  /** Seed storefront product id. */
  productId?: string;
  /** Seed title text. */
  title?: string;
  /** Seed authors string. */
  authors?: string;
  /** Seed narrators string. */
  narrators?: string;
  /** Seed series name. */
  series?: string;
  /** Seed series ASIN when known. */
  seriesAsin?: string;
  /** Seed Amazon ASIN. */
  asin?: string;
  /** Seed ISBN. */
  isbn?: string;
  /** Storefront region / marketplace code. */
  region?: string;
  /** Maximum candidates to return (default 20). */
  limit?: number;
}

/**
 * Params JSON for `ContentSource.purchaseHint`. At least one identity field (`productId` /
 * `asin` / `isbn` / title+authors) should be set; guests may return `invalid_params` when
 * none are usable.
 */
export interface PurchaseHintParams {
  /** Storefront product id when known. */
  productId?: string;
  /** Title text for fuzzy lookup. */
  title?: string;
  /** Authors string for fuzzy lookup. */
  authors?: string;
  /** Amazon ASIN when known. */
  asin?: string;
  /** ISBN when known. */
  isbn?: string;
  /** Storefront region / marketplace code. */
  region?: string;
  /** When true, guests should include live price fields when available. */
  withPrice?: boolean;
}

/** Params JSON for `ContentSource.listDeals`. */
export interface ListDealsParams {
  /** Optional maximum number of deals to return; guest default when omitted. */
  limit?: number;
}

/** Params JSON for `ContentSource.catalogDetail`. */
export interface CatalogDetailParams {
  /** Store product id (Libro ISBN or ISBN-slug). */
  productId: string;
  /** Optional ISBN when it differs from `productId`. */
  isbn?: string;
}

/** Params JSON for `Integration.scanLibrary` (remote library sync). */
export interface ScanLibraryParams {
  /** When true, force a full rescan even if the guest would otherwise incremental-sync. */
  force?: boolean;
}

/** Params JSON for `Integration.authenticateUser`. */
export interface AuthenticateUserParams {
  /** Integration username / login id. */
  username: string;
  /** Integration password; never logged by the host. */
  password: string;
}
