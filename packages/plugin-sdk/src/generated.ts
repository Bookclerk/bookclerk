/**
 * TypeScript projection of `crates/bookclerk-plugin-abi/schema/abi.json`.
 * Field names are camelCase to match the Workers RPC wire format.
 *
 * Keep in sync with the schema (and the Rust types in `bookclerk-plugin-abi`).
 * Run `npm run check-schema` to assert the authoritative JSON still exists.
 */

/** Wire API version for all guests. */
export const API_VERSION = 1 as const;

/** Plugin surface kind (handshake + plugin.toml). */
export type PluginKind = "source" | "integration" | "output" | "database";

/** Stable error codes (`PluginError.code`). */
export type PluginErrorCode =
  | "invalid_params"
  | "unauthorized"
  | "forbidden"
  | "not_found"
  | "unavailable"
  | "unsupported"
  | "internal";

/** RPC/plugin failure payload. */
export interface PluginError {
  code: PluginErrorCode;
  message: string;
  details?: Record<string, unknown>;
}

/** Loose JSON object used for config / CLI args. */
export type JsonObject = Record<string, unknown>;

/** `handshake` params. */
export interface HandshakeParams {
  apiVersion: typeof API_VERSION | 1;
  config: JsonObject;
}

/** Brand block for UI. */
export interface BrandDto {
  id: string;
  name: string;
  bg: string;
  fg: string;
  accent: string;
  iconUrl: string;
}

/** Config option value. */
export interface ConfigOptionValueDto {
  id: string;
  label: string;
}

/** Config option descriptor. */
export interface ConfigOptionDto {
  key: string;
  label: string;
  values: ConfigOptionValueDto[];
}

/** CLI argument kind. */
export type CliArgKind = "string" | "bool" | "int" | "path";

/** One CLI argument. */
export interface CliArgSpec {
  name: string;
  long?: string;
  short?: string;
  kind?: CliArgKind;
  required?: boolean;
  default?: string;
  about?: string;
  positional?: boolean;
}

/** One plugin CLI command. */
export interface CliCommandSpec {
  name: string;
  about?: string;
  args?: CliArgSpec[];
}

/** Declared CLI surface. */
export interface CliSchema {
  commands?: CliCommandSpec[];
}

/** `handshake` result. */
export interface HandshakeResult {
  apiVersion: typeof API_VERSION | 1;
  id: string;
  kind: PluginKind | string;
  displayName?: string;
  capabilities: string[];
  brand?: BrandDto;
  configOptions?: ConfigOptionDto[];
  cli?: CliSchema;
  portalAuthMode?: "oauth" | "password";
  passwordEnvVar?: string;
  aliases?: string[];
  sortKey?: number;
}

/** `cliInvoke` params. */
export interface CliInvokeParams {
  command: string;
  args?: JsonObject;
}

/** `cliInvoke` result. */
export interface CliInvokeResult {
  exitCode?: number;
  stdout?: string;
  stderr?: string;
  json?: unknown;
}

/** `health` result. */
export interface HealthResult {
  ok: boolean;
  id?: string;
  enabled?: boolean;
  detail?: string;
}

/** `diagnose` result. */
export interface DiagnoseResult {
  lines: string[];
}

/** Host → plugin: book acquired. */
export interface BookAcquiredPayload {
  titleId: string;
  source: string;
  asin?: string;
  isbn?: string;
  pathKeys: string[];
}

/** Host → plugin: library scan completed. */
export interface LibraryScanCompletedPayload {
  source: string;
  upserted: number;
}

/** Host → plugin: config changed. */
export interface ConfigChangedPayload {
  config: JsonObject;
}

/**
 * Host → plugin (`onEvent`).
 * Discriminated by `type` with snake_case event names (schema).
 */
export type HostToPluginEvent =
  | { type: "book_acquired"; payload: BookAcquiredPayload }
  | { type: "library_scan_completed"; payload: LibraryScanCompletedPayload }
  | { type: "config_changed"; payload: ConfigChangedPayload };

export type PluginLogLevel = "debug" | "info" | "warn" | "error";

export interface ExternalUsersPayload {
  users: unknown[];
}

export interface ListeningProgressPayload {
  items: unknown[];
}

export interface PluginLogPayload {
  level: PluginLogLevel;
  message: string;
}

/**
 * Plugin → host (`env.HOST.notify`).
 * Discriminated by `type` with snake_case event names (schema).
 */
export type PluginToHostEvent =
  | { type: "external_users"; payload: ExternalUsersPayload }
  | { type: "listening_progress"; payload: ListeningProgressPayload }
  | { type: "plugin_log"; payload: PluginLogPayload };

/** Host binding for plugin → host notifications. */
export interface HostBinding {
  notify(event: PluginToHostEvent): Promise<void>;
}

/**
 * Guest `env` bindings declared by the ABI / capabilities.bindings.
 * Only `HOST` is required for event push; others appear when consented.
 */
export interface BookclerkEnv {
  HOST: HostBinding;
  CONFIG?: JsonObject;
  SECRETS?: unknown;
  PLUGIN_KV?: unknown;
  WORK_FS?: unknown;
  OAUTH?: unknown;
}

/** Kind-specific / stub params (schema placeholders — widen as schema grows). */
export type ScanLibraryParams = JsonObject;
export type AuthenticateUserParams = JsonObject;
export type LoginParams = JsonObject;
export type LoginStartParams = JsonObject;
export type LoginCompleteParams = JsonObject;
export type ScanParams = JsonObject;
export type FetchTitleParams = JsonObject;
export type SearchCatalogParams = JsonObject;
export type ExpandCandidatesParams = JsonObject;
export type PurchaseHintParams = JsonObject;
export type ListDealsParams = JsonObject;
export type PutParams = JsonObject;
export type PutFileParams = JsonObject;
export type KeyParams = JsonObject;
export type ListParams = JsonObject;
export type CopyParams = JsonObject;
export type TouchFileParams = JsonObject;
export type DbConnectParams = JsonObject;
export type StatementDto = JsonObject;

/** Known core method names on the wire. */
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
] as const;

export type MethodName = (typeof METHOD_NAMES)[number];
