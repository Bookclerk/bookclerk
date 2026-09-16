/**
 * GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.
 *
 * TypeScript projection of the product ABI constants and database enum
 * ordinal tables declared in `crates/bookclerk-plugin-abi/schema/plugin.capnp`.
 */

/** Product ABI version (`apiVersion` / `plugin.toml` `api_version`). */
export const PRODUCT_API_VERSION = 2 as const;

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

/** Maximum aggregate UTF-8 bytes of plugin migration ids plus SQL in one `databaseMigrations` registration. */
export const MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES = 262144 as const;

/** Guest honors scalar / stream-window / list-page caps. */
export const FEATURE_SCALAR_LIMITS = "rpc.scalarLimits" as const;

/** Media moves through transferred `ByteRange` / `ByteSource` streams. */
export const FEATURE_STREAMS = "rpc.streams" as const;

/** Guest implements server-side `Destination.copy`. */
export const FEATURE_STORAGE_COPY = "storage.copy" as const;

/** Ordinal-ordered `DbStatementKind` wire names (index = Cap'n Proto ordinal). */
export const DB_STATEMENT_KINDS = ["execute", "select", "returning"] as const;

/** Union of `DB_STATEMENT_KINDS` wire names. */
export type DbStatementKind = (typeof DB_STATEMENT_KINDS)[number];

/** Ordinal-ordered `DbResultSelection` wire names (index = Cap'n Proto ordinal). */
export const DB_RESULT_SELECTIONS = ["discard", "affectedRows", "rows"] as const;

/** Union of `DB_RESULT_SELECTIONS` wire names. */
export type DbResultSelection = (typeof DB_RESULT_SELECTIONS)[number];

/** Ordinal-ordered `DbType` column-type wire names (index = Cap'n Proto ordinal). */
export const DB_COLUMN_TYPES = ["unspecified", "bool", "int64", "float64", "text", "bytes"] as const;

/** Union of `DB_COLUMN_TYPES` wire names. */
export type DbColumnType = (typeof DB_COLUMN_TYPES)[number];
