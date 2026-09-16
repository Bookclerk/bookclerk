/**
 * GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.
 *
 * TypeScript projection of the product ABI constants and enum ordinal
 * tables declared in `crates/bookclerk-plugin-abi/schema/plugin.capnp`.
 */

/** Product ABI version (`plugin.toml` `api_version` / `describe().apiVersion`). */
export const PRODUCT_API_VERSION = 3 as const;

/** Maximum decoded size of an ordinary RPC scalar value (not a stream window). */
export const MAX_SCALAR_BYTES = 262144 as const;

/** Maximum bytes returned by one `ByteSource.pull` (flow-control window). */
export const MAX_STREAM_WINDOW_BYTES = 1048576 as const;

/** Maximum objects in one `Destination.list` page. */
export const MAX_LIST_PAGE = 256 as const;

/** Maximum job / event checkpoint payload size (bytes). */
export const MAX_CHECKPOINT_BYTES = 65536 as const;

/** Maximum plugin / account identifier length (bytes). */
export const MAX_IDENTIFIER_BYTES = 64 as const;

/** Maximum granted config payload size (bytes). */
export const MAX_CONFIG_PAYLOAD_BYTES = 65536 as const;

/** Maximum decoded size of a domain-event scalar payload (not a stream). */
export const MAX_EVENT_PAYLOAD_BYTES = 65536 as const;

/** Maximum already-separated operations in one plugin-owned migration. */
export const MAX_PLUGIN_MIGRATION_OPS = 256 as const;

/** Maximum already-separated operations across one `databaseMigrations` registration. */
export const MAX_PLUGIN_MIGRATION_TOTAL_OPS = 2048 as const;

/**
 * Maximum aggregate UTF-8 bytes of plugin migration ids plus SQL in one `databaseMigrations` registration.
 */
export const MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES = 262144 as const;

/** Guest honors scalar / stream-window / list-page caps (`rpc.scalarLimits`). */
export const FEATURE_SCALAR_LIMITS = "rpc.scalarLimits" as const;

/** Media moves through transferred `ByteRange` / `ByteSource` streams (`rpc.streams`). */
export const FEATURE_STREAMS = "rpc.streams" as const;

/** Guest implements server-side `Destination.copy` (`storage.copy`). */
export const FEATURE_STORAGE_COPY = "storage.copy" as const;

/**
 * Stable `PluginError.code` strings. Unknown future codes are forwarded
 * as-is; SDKs surface them as a local `unknown` while keeping the raw wire
 * code.
 *
 * Wire strings of `PluginErrorCode` in ordinal order (snake_case `Text` codes).
 */
export const PLUGIN_ERROR_CODES = ["invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict"] as const;

/** Union of `PLUGIN_ERROR_CODES` wire names. */
export type PluginErrorCode = (typeof PLUGIN_ERROR_CODES)[number];

/**
 * Named entrypoint a plugin exports (Cloudflare Workers named-entrypoint
 * analogue). Each value is a capability the host calls over RPC; triggers on
 * the default entrypoint (`event`, `job`) are declared separately.
 *
 * Ordinal-ordered `Entrypoint` wire names (index = Cap'n Proto ordinal).
 */
export const ENTRYPOINTS = ["storefront", "storage", "databaseAdapter", "remoteLibrary", "cli", "oidc"] as const;

/** Union of `ENTRYPOINTS` wire names. */
export type Entrypoint = (typeof ENTRYPOINTS)[number];

/**
 * Portal Accounts connect mode for storefronts.
 *
 * Ordinal-ordered `PortalAuthMode` wire names (index = Cap'n Proto ordinal).
 */
export const PORTAL_AUTH_MODES = ["unspecified", "password", "oauth"] as const;

/** Union of `PORTAL_AUTH_MODES` wire names. */
export type PortalAuthMode = (typeof PORTAL_AUTH_MODES)[number];

/**
 * Value kind for a `CliArgSpec`.
 *
 * Ordinal-ordered `CliArgKind` wire names (index = Cap'n Proto ordinal).
 */
export const CLI_ARG_KINDS = ["string", "bool", "int", "path"] as const;

/** Union of `CLI_ARG_KINDS` wire names. */
export type CliArgKind = (typeof CLI_ARG_KINDS)[number];

/**
 * Catalog search ordering.
 *
 * Ordinal-ordered `CatalogSort` wire names (index = Cap'n Proto ordinal).
 */
export const CATALOG_SORTS = ["relevance", "popularity", "rating", "title", "author"] as const;

/** Union of `CATALOG_SORTS` wire names. */
export type CatalogSort = (typeof CATALOG_SORTS)[number];

/**
 * Catalog search facet restricting which field the query matches.
 *
 * Ordinal-ordered `CatalogField` wire names (index = Cap'n Proto ordinal).
 */
export const CATALOG_FIELDS = ["any", "author", "narrator", "series", "genre"] as const;

/** Union of `CATALOG_FIELDS` wire names. */
export type CatalogField = (typeof CATALOG_FIELDS)[number];

/**
 * Whether an edition is abridged.
 *
 * Ordinal-ordered `Abridgement` wire names (index = Cap'n Proto ordinal).
 */
export const ABRIDGEMENTS = ["unknown", "unabridged", "abridged"] as const;

/** Union of `ABRIDGEMENTS` wire names. */
export type Abridgement = (typeof ABRIDGEMENTS)[number];

/**
 * Universal database cell/parameter domain. Engine-native arrays, enums,
 * unsigned integers, and JSON text sentinels are not baseline ABI values.
 *
 * Ordinal-ordered `DbType` wire names (index = Cap'n Proto ordinal).
 */
export const DB_TYPES = ["unspecified", "bool", "int64", "float64", "text", "bytes"] as const;

/** Union of `DB_TYPES` wire names. */
export type DbType = (typeof DB_TYPES)[number];

/**
 * How the host classifies a statement for result handling.
 *
 * Ordinal-ordered `DbStatementKind` wire names (index = Cap'n Proto ordinal).
 */
export const DB_STATEMENT_KINDS = ["execute", "select", "returning"] as const;

/** Union of `DB_STATEMENT_KINDS` wire names. */
export type DbStatementKind = (typeof DB_STATEMENT_KINDS)[number];

/**
 * Which part of a statement's outcome the caller wants back.
 *
 * Ordinal-ordered `DbResultSelection` wire names (index = Cap'n Proto ordinal).
 */
export const DB_RESULT_SELECTIONS = ["discard", "affectedRows", "rows"] as const;

/** Union of `DB_RESULT_SELECTIONS` wire names. */
export type DbResultSelection = (typeof DB_RESULT_SELECTIONS)[number];

/**
 * Host → adapter execute. GuestDatabase stays ExecuteRequest-only.
 *
 * Ordinal-ordered `IsolationReq` wire names (index = Cap'n Proto ordinal).
 */
export const ISOLATION_REQS = ["atomicBatch", "nestedSavepoint", "consistentSnapshot"] as const;

/** Union of `ISOLATION_REQS` wire names. */
export type IsolationReq = (typeof ISOLATION_REQS)[number];

/**
 * Resolved BookclerkSQL column type after type checking.
 *
 * Ordinal-ordered `ResolvedSqlType` wire names (index = Cap'n Proto ordinal).
 */
export const RESOLVED_SQL_TYPES = ["integer", "real", "text", "blob", "boolean", "null"] as const;

/** Union of `RESOLVED_SQL_TYPES` wire names. */
export type ResolvedSqlType = (typeof RESOLVED_SQL_TYPES)[number];

/**
 * INTEGER `+` `-` `*` `abs` site lowered to overflow -> NULL.
 *
 * Ordinal-ordered `IntegerArithKind` wire names (index = Cap'n Proto ordinal).
 */
export const INTEGER_ARITH_KINDS = ["add", "sub", "mul", "abs"] as const;

/** Union of `INTEGER_ARITH_KINDS` wire names. */
export type IntegerArithKind = (typeof INTEGER_ARITH_KINDS)[number];
