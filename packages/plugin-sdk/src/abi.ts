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

/**
 * Maximum keys or key pairs in one `Destination.moveMany`, `deleteMany` or
 * `headMany` call, and maximum outcomes in one reply.
 */
export const MAX_BATCH_KEYS = 256 as const;

/** Maximum changes in one `Destination.changes` page. */
export const MAX_CHANGES_PAGE = 256 as const;

/** Maximum bytes of a `Destination.changes` cursor. */
export const MAX_CHANGES_CURSOR_BYTES = 4096 as const;

/** Maximum job / event checkpoint payload size (bytes). */
export const MAX_CHECKPOINT_BYTES = 65536 as const;

/** Maximum titles, and separately removed product ids, in one `ScanPage`. */
export const MAX_SCAN_PAGE_BOOKS = 500 as const;

/** Maximum bytes of a scan cursor or sync token. */
export const MAX_SCAN_CURSOR_BYTES = 4096 as const;

/**
 * Maximum bytes of refreshed credentials a storefront hands back
 * (`ScanPage.refreshedCredentials`, `PlainFetch.refreshedCredentials`).
 */
export const MAX_REFRESHED_CREDENTIAL_BYTES = 65536 as const;

/** Maximum bytes of an `EventReadRequest.after` / `EventReadPage.cursor`. */
export const MAX_EVENT_CURSOR_BYTES = 512 as const;

/** Maximum plugin / account identifier length (bytes). */
export const MAX_IDENTIFIER_BYTES = 64 as const;

/** Maximum `[triggers] crons` source length (bytes), as in `ScheduledController.cron`. */
export const MAX_CRON_BYTES = 128 as const;

/** Maximum granted config payload size (bytes). */
export const MAX_CONFIG_PAYLOAD_BYTES = 65536 as const;

/** Maximum decoded size of a domain-event scalar payload (not a stream). */
export const MAX_EVENT_PAYLOAD_BYTES = 65536 as const;

/**
 * Largest `KvNamespace` value in bytes; a `[[kv_namespaces]]` row may lower
 * its own cap but never raise it past this.
 */
export const MAX_KV_VALUE_BYTES = 1048576 as const;

/** Most URLs in one `CoverFetcher.fetchCovers` call. */
export const MAX_COVER_URLS = 32 as const;

/** Largest image body `CoverFetcher.fetchCovers` may return for one URL. */
export const MAX_COVER_BYTES = 10485760 as const;

/**
 * Longest cache header value, in bytes, `CoverFetcher.fetchCovers` carries in
 * either direction (a condition the host sends, a header the server sent).
 * The host ignores a longer header the fetcher returns.
 */
export const MAX_COVER_HEADER_BYTES = 1024 as const;

/**
 * Largest metadata file `MetadataFiles.parse` reads and `MetadataFiles.render`
 * returns. The host skips a larger sidecar before any plugin sees it.
 */
export const MAX_METADATA_FILE_BYTES = 262144 as const;

/** Longest description a `MetadataFileRecord` carries, in UTF-8 bytes. */
export const MAX_METADATA_FILE_DESCRIPTION_BYTES = 65536 as const;

/**
 * Longest other text value in a `MetadataFileRecord` (a title, a name, a
 * genre, a chapter title), in UTF-8 bytes.
 */
export const MAX_METADATA_FILE_TEXT_BYTES = 1024 as const;

/**
 * Most entries in each list of a `MetadataFileRecord` other than chapters
 * (credits, genres, tags, identifiers, series).
 */
export const MAX_METADATA_FILE_LIST_ENTRIES = 256 as const;

/** Most chapters in a `MetadataFileRecord`. */
export const MAX_METADATA_FILE_CHAPTERS = 2000 as const;

/** Most entries in `MetadataFileRenderParams.audioFiles`. */
export const MAX_METADATA_AUDIO_FILES = 4096 as const;

/** Longest `KvNamespace` key in UTF-8 bytes. */
export const MAX_KV_KEY_BYTES = 512 as const;

/**
 * Largest WebSocket message, text or binary, that `Sockets.send` accepts or
 * `SocketHandler.message` delivers.
 */
export const MAX_SOCKET_FRAME_BYTES = 262144 as const;

/** Largest per-socket attachment the host keeps for a sleeping guest. */
export const MAX_SOCKET_ATTACHMENT_BYTES = 2048 as const;

/** Most host-brokered sockets one plugin instance may hold open at once. */
export const MAX_SOCKETS_PER_INSTANCE = 8 as const;

/** Most heartbeat pairs one `Sockets.connect` may declare. */
export const MAX_SOCKET_AUTO_RESPONSES = 4 as const;

/** Largest heartbeat request or response frame, in bytes. */
export const MAX_SOCKET_AUTO_RESPONSE_BYTES = 256 as const;

/** Longest `SocketConnectRequest.url`, in bytes. */
export const MAX_SOCKET_URL_BYTES = 2048 as const;

/** Most request headers, and separately subprotocols, on one `Sockets.connect`. */
export const MAX_SOCKET_HEADERS = 16 as const;

/** Largest `KvNamespace` metadata JSON text in bytes. */
export const MAX_KV_METADATA_BYTES = 1024 as const;

/**
 * Most entries in each list of a `NetworkGrant` (fetch hosts, TCP
 * destinations, network settings).
 */
export const MAX_NETWORK_GRANT_ENTRIES = 256 as const;

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

/** Guest implements `Destination.move` (`storage.move`). */
export const FEATURE_STORAGE_MOVE = "storage.move" as const;

/** Guest implements `Destination.moveMany` (`storage.moveMany`). */
export const FEATURE_STORAGE_MOVE_MANY = "storage.moveMany" as const;

/** Guest implements `Destination.deleteMany` (`storage.deleteMany`). */
export const FEATURE_STORAGE_DELETE_MANY = "storage.deleteMany" as const;

/** Guest implements `Destination.headMany` (`storage.headMany`). */
export const FEATURE_STORAGE_HEAD_MANY = "storage.headMany" as const;

/** Guest enforces `WriteOptions.ifMatch` / `ifNoneMatch` (`storage.conditionalWrite`). */
export const FEATURE_STORAGE_CONDITIONAL_WRITE = "storage.conditionalWrite" as const;

/** Guest implements `Destination.changes` (`storage.changes`). */
export const FEATURE_STORAGE_CHANGES = "storage.changes" as const;

/**
 * What happened to an object in a change feed. Unknown codes are kept and
 * treated as `changed`.
 *
 * Ordinal-ordered `ObjectChangeKind` wire names (index = Cap'n Proto ordinal).
 */
export const OBJECT_CHANGE_KINDS = ["changed", "upserted", "removed"] as const;

/** Union of `OBJECT_CHANGE_KINDS` wire names. */
export type ObjectChangeKind = (typeof OBJECT_CHANGE_KINDS)[number];

/**
 * How a setting's destination is reached.
 *
 * Ordinal-ordered `SettingNetworkKind` wire names (index = Cap'n Proto ordinal).
 */
export const SETTING_NETWORK_KINDS = ["fetch", "tcp"] as const;

/** Union of `SETTING_NETWORK_KINDS` wire names. */
export type SettingNetworkKind = (typeof SETTING_NETWORK_KINDS)[number];

/**
 * Where a network setting's destination stands.
 *
 * Ordinal-ordered `SettingGrantState` wire names (index = Cap'n Proto ordinal).
 */
export const SETTING_GRANT_STATES = ["approved", "pending", "unset", "invalid"] as const;

/** Union of `SETTING_GRANT_STATES` wire names. */
export type SettingGrantState = (typeof SETTING_GRANT_STATES)[number];

/**
 * Stable `PluginError.code` strings. Unknown future codes are forwarded
 * as-is; SDKs surface them as a local `unknown` while keeping the raw wire
 * code.
 *
 * Wire strings of `PluginErrorCode` in ordinal order (snake_case `Text` codes).
 */
export const PLUGIN_ERROR_CODES = ["invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict", "limit_exceeded", "rate_limited", "network_not_granted"] as const;

/** Union of `PLUGIN_ERROR_CODES` wire names. */
export type PluginErrorCode = (typeof PLUGIN_ERROR_CODES)[number];

/**
 * Named entrypoint a plugin exports (Cloudflare Workers named-entrypoint
 * analogue). Each value is a capability the host calls over RPC; triggers on
 * the default entrypoint (`event`, `job`) are declared separately.
 *
 * Ordinal-ordered `Entrypoint` wire names (index = Cap'n Proto ordinal).
 */
export const ENTRYPOINTS = ["storefront", "storage", "databaseAdapter", "remoteLibrary", "cli", "oidc", "metadata", "covers", "metadataFiles"] as const;

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
 * What family of creative work a record describes (FRBR work). Every record
 * names one: `unspecified` is refused by the host, so a guest that forgets
 * the field fails instead of silently describing a book. Whether a podcast
 * work is a show or an episode is structural, not a kind: an episode names
 * its show through `parentProductId`, a show names no parent.
 *
 * Ordinal-ordered `WorkKind` wire names (index = Cap'n Proto ordinal).
 */
export const WORK_KINDS = ["unspecified", "book", "podcast"] as const;

/** Union of `WORK_KINDS` wire names. */
export type WorkKind = (typeof WORK_KINDS)[number];

/**
 * How a work is realised (FRBR expression): the kind of content you get,
 * independent of the carrier. A translation is not a kind: the expression's
 * language and its translator credits say it. `unspecified` is refused.
 *
 * Ordinal-ordered `ExpressionKind` wire names (index = Cap'n Proto ordinal).
 */
export const EXPRESSION_KINDS = ["unspecified", "text", "narration", "dramatisation", "graphic", "originalAudio"] as const;

/** Union of `EXPRESSION_KINDS` wire names. */
export type ExpressionKind = (typeof EXPRESSION_KINDS)[number];

/**
 * Carrier of an edition (FRBR manifestation): what form you receive.
 * Whether the host acquires an edition depends on the storefront's
 * `MediaSupport.fetches` and what the host supports, not on this value
 * alone. `unspecified` is refused.
 *
 * Ordinal-ordered `MediaKind` wire names (index = Cap'n Proto ordinal).
 */
export const MEDIA_KINDS = ["unspecified", "audio", "ebook", "print"] as const;

/** Union of `MEDIA_KINDS` wire names. */
export type MediaKind = (typeof MEDIA_KINDS)[number];

/**
 * Scheme of an external identifier. The scheme fixes whether the id names a
 * work or an edition, and whether it is strong (unique) or weak (shared).
 *
 * Ordinal-ordered `IdentifierScheme` wire names (index = Cap'n Proto ordinal).
 */
export const IDENTIFIER_SCHEMES = ["isbn", "asin", "openLibraryEdition", "openLibraryWork"] as const;

/** Union of `IDENTIFIER_SCHEMES` wire names. */
export type IdentifierScheme = (typeof IDENTIFIER_SCHEMES)[number];

/**
 * A contributor's responsibility for a work, expression or edition.
 *
 * Ordinal-ordered `CreditRole` wire names (index = Cap'n Proto ordinal).
 */
export const CREDIT_ROLES = ["author", "narrator", "translator", "editor", "illustrator", "publisher", "host", "cast", "adaptor"] as const;

/** Union of `CREDIT_ROLES` wire names. */
export type CreditRole = (typeof CREDIT_ROLES)[number];

/**
 * Which level a credit attaches to. Ordinal 0 is the role's usual level, so
 * a guest sets this only when a credit departs from it.
 *
 * Ordinal-ordered `CreditLevel` wire names (index = Cap'n Proto ordinal).
 */
export const CREDIT_LEVELS = ["roleDefault", "work", "expression", "edition"] as const;

/** Union of `CREDIT_LEVELS` wire names. */
export type CreditLevel = (typeof CREDIT_LEVELS)[number];

/**
 * What one fetched file is to the edition it belongs to.
 *
 * Ordinal-ordered `FetchedFileRole` wire names (index = Cap'n Proto ordinal).
 */
export const FETCHED_FILE_ROLES = ["unspecified", "part", "whole", "cover"] as const;

/** Union of `FETCHED_FILE_ROLES` wire names. */
export type FetchedFileRole = (typeof FETCHED_FILE_ROLES)[number];

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
 * How a purchase-hint query names the title to price.
 *
 * Ordinal-ordered `PriceLookup` wire names (index = Cap'n Proto ordinal).
 */
export const PRICE_LOOKUPS = ["unspecified", "product", "isbn", "asin", "title"] as const;

/** Union of `PRICE_LOOKUPS` wire names. */
export type PriceLookup = (typeof PRICE_LOOKUPS)[number];

/**
 * Whether an edition is abridged.
 *
 * Ordinal-ordered `Abridgement` wire names (index = Cap'n Proto ordinal).
 */
export const ABRIDGEMENTS = ["unknown", "unabridged", "abridged"] as const;

/** Union of `ABRIDGEMENTS` wire names. */
export type Abridgement = (typeof ABRIDGEMENTS)[number];

/**
 * A field of a `MetadataFileRecord`. Dialects declare in the manifest which
 * fields their format carries; a parsed record lists the ones its file
 * states, and a render request lists the ones to write.
 *
 * Ordinal-ordered `MetadataField` wire names (index = Cap'n Proto ordinal).
 */
export const METADATA_FIELDS = ["title", "subtitle", "description", "publisher", "publishedAt", "language", "genres", "tags", "abridgement", "expressionKind", "identifiers", "credits", "series", "chapters", "duration"] as const;

/** Union of `METADATA_FIELDS` wire names. */
export type MetadataField = (typeof METADATA_FIELDS)[number];

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
