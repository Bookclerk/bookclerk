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
 * (`ScanPage.refreshedCredentials`, `PlainFetch.refreshedCredentials`,
 * `QualityOffersResult.refreshedCredentials`).
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

/**
 * Longest `NotificationMessage.title` and `NotificationField` name, in UTF-8
 * bytes.
 */
export const MAX_NOTIFICATION_TITLE_BYTES = 256 as const;

/** Longest `NotificationMessage.text` or `markdown`, in UTF-8 bytes. */
export const MAX_NOTIFICATION_TEXT_BYTES = 16384 as const;

/** Longest `NotificationMessage.html`, in UTF-8 bytes. */
export const MAX_NOTIFICATION_HTML_BYTES = 65536 as const;

/** Most `NotificationField`s on one message or raise. */
export const MAX_NOTIFICATION_FIELDS = 25 as const;

/**
 * Longest `NotificationField.value`, link, group key or alert key, in UTF-8
 * bytes.
 */
export const MAX_NOTIFICATION_FIELD_BYTES = 1024 as const;

/** Longest destination address document (`addressJson`), in UTF-8 bytes. */
export const MAX_NOTIFIER_ADDRESS_BYTES = 4096 as const;

/** Longest `AiModel.name`, in UTF-8 bytes (`@<plugin id>/<model id>`). */
export const MAX_AI_MODEL_NAME_BYTES = 192 as const;

/** Most models one `AiModels.models` reply lists. */
export const MAX_AI_MODELS = 64 as const;

/** Most texts in one `AiTextEmbeddingsInput`. */
export const MAX_AI_EMBED_TEXTS = 64 as const;

/** Longest text in an `AiTextEmbeddingsInput`, in UTF-8 bytes. */
export const MAX_AI_EMBED_TEXT_BYTES = 16384 as const;

/** Most numbers in one embedding vector. */
export const MAX_AI_EMBED_DIMENSIONS = 4096 as const;

/** Most language tags on one `AiModel`. */
export const MAX_AI_LANGUAGES = 64 as const;

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

/**
 * Most sockets into the library (accepted `Origin.connect` upgrades) one
 * relay ingress instance may hold at once, apart from `maxSocketsPerInstance`:
 * the host holds them in process, not in the socket broker.
 */
export const MAX_ORIGIN_SOCKETS_PER_INSTANCE = 256 as const;

/** Most heartbeat pairs one `Sockets.connect` may declare. */
export const MAX_SOCKET_AUTO_RESPONSES = 4 as const;

/** Largest heartbeat request or response frame, in bytes. */
export const MAX_SOCKET_AUTO_RESPONSE_BYTES = 256 as const;

/** Most trust anchors one `tls://` `Sockets.connect` may supply. */
export const MAX_SOCKET_TRUST_ANCHORS = 4 as const;

/** Largest DER certificate in `SocketConnectRequest.trustAnchors`, in bytes. */
export const MAX_SOCKET_TRUST_ANCHOR_BYTES = 8192 as const;

/** Longest `SocketConnectRequest.url`, in bytes. */
export const MAX_SOCKET_URL_BYTES = 2048 as const;

/** Most request headers, and separately subprotocols, on one `Sockets.connect`. */
export const MAX_SOCKET_HEADERS = 16 as const;

/**
 * Most header fields on one `Origin.fetch` request, and separately on its
 * response.
 */
export const MAX_ORIGIN_HEADERS = 128 as const;

/**
 * Largest total UTF-8 bytes of header names and values on one `Origin.fetch`
 * request, and separately on its response.
 */
export const MAX_ORIGIN_HEADER_BYTES = 65536 as const;

/** Longest `OriginRequest.target` (path and query), in bytes. */
export const MAX_ORIGIN_TARGET_BYTES = 8192 as const;

/** Longest `IngressStarted.origin`, in bytes. */
export const MAX_INGRESS_ORIGIN_BYTES = 253 as const;

/**
 * Largest body of an `OriginRefusal` (the library's answer to a WebSocket
 * upgrade it did not accept), in bytes; a longer body is cut to this.
 */
export const MAX_ORIGIN_REFUSAL_BODY_BYTES = 65536 as const;

/** Most deliveries one `Webhooks.pull` returns. */
export const MAX_WEBHOOK_PULL_BATCH = 100 as const;

/** Longest `WebhookDelivery.leaseId`, in bytes. */
export const MAX_WEBHOOK_LEASE_ID_BYTES = 128 as const;

/** Longest `Webhooks.pull` visibility timeout, in milliseconds (12 hours). */
export const MAX_WEBHOOK_VISIBILITY_MS = 43200000 as const;

/**
 * Largest stored webhook request body, in bytes (1 MiB); a `[[webhooks]]`
 * row may lower its own cap but never raise it past this.
 */
export const MAX_WEBHOOK_BODY_BYTES = 1048576 as const;

/**
 * Most request headers a `[[webhooks]]` row keeps; a delivery also carries
 * `content-type`, so it holds at most one more.
 */
export const MAX_WEBHOOK_HEADERS = 16 as const;

/** Longest `[[webhooks]]` name, in bytes. */
export const MAX_WEBHOOK_NAME_BYTES = 32 as const;

/** Longest kept request header value, in bytes. */
export const MAX_WEBHOOK_HEADER_VALUE_BYTES = 8192 as const;

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

/**
 * Guest enforces `WriteOptions.ifMatch` / `ifNoneMatch` by writing, then
 * checking and withdrawing its write when another got in first
 * (`storage.checkedWrite`); a reader may briefly see the losing write.
 */
export const FEATURE_STORAGE_CHECKED_WRITE = "storage.checkedWrite" as const;

/** Guest implements `Destination.changes` (`storage.changes`). */
export const FEATURE_STORAGE_CHANGES = "storage.changes" as const;

/** Guest implements `Destination.translateKeys` (`storage.translateKeys`). */
export const FEATURE_STORAGE_TRANSLATE_KEYS = "storage.translateKeys" as const;

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
export const ENTRYPOINTS = ["storefront", "storage", "databaseAdapter", "remoteLibrary", "cli", "oidc", "metadata", "covers", "metadataFiles", "notifier", "ingress", "ai"] as const;

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
 * What a storefront's chapter markers mark (`PlainFetch.chapterKind`).
 *
 * Ordinal-ordered `ChapterKind` wire names (index = Cap'n Proto ordinal).
 */
export const CHAPTER_KINDS = ["unknown", "chapters", "tracks"] as const;

/** Union of `CHAPTER_KINDS` wire names. */
export type ChapterKind = (typeof CHAPTER_KINDS)[number];

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
 * Whether an edition is marked as explicit (adult) content. A patron scope
 * can hide explicit editions; `unknown` and `clean` editions stay visible.
 *
 * Ordinal-ordered `Explicitness` wire names (index = Cap'n Proto ordinal).
 */
export const EXPLICITNESSS = ["unknown", "clean", "explicit"] as const;

/** Union of `EXPLICITNESSS` wire names. */
export type Explicitness = (typeof EXPLICITNESSS)[number];

/**
 * Audio codec of a stream a store offers (`QualityOffer.codec`). Spelled as
 * the host's probed codecs are, so an offer compares with a held file.
 *
 * Ordinal-ordered `OfferedCodec` wire names (index = Cap'n Proto ordinal).
 */
export const OFFERED_CODECS = ["unknown", "aac", "alac", "flac", "mp3", "opus", "vorbis", "eac3", "ac4"] as const;

/** Union of `OFFERED_CODECS` wire names. */
export type OfferedCodec = (typeof OFFERED_CODECS)[number];

/**
 * AAC profile of an offered AAC stream (`QualityOffer.aacProfile`).
 *
 * Ordinal-ordered `OfferedAacProfile` wire names (index = Cap'n Proto ordinal).
 */
export const OFFERED_AAC_PROFILES = ["unknown", "lc", "he", "heV2", "xhe"] as const;

/** Union of `OFFERED_AAC_PROFILES` wire names. */
export type OfferedAacProfile = (typeof OFFERED_AAC_PROFILES)[number];

/**
 * A field of a `MetadataFileRecord`. Dialects declare in the manifest which
 * fields their format carries; a parsed record lists the ones its file
 * states, and a render request lists the ones to write.
 *
 * Ordinal-ordered `MetadataField` wire names (index = Cap'n Proto ordinal).
 */
export const METADATA_FIELDS = ["title", "subtitle", "description", "publisher", "publishedAt", "language", "genres", "tags", "abridgement", "expressionKind", "identifiers", "credits", "series", "chapters", "duration", "explicitness"] as const;

/** Union of `METADATA_FIELDS` wire names. */
export type MetadataField = (typeof METADATA_FIELDS)[number];

/**
 * How urgent a notification is. Routing rules carry a minimum severity.
 *
 * Ordinal-ordered `NotificationSeverity` wire names (index = Cap'n Proto ordinal).
 */
export const NOTIFICATION_SEVERITYS = ["info", "warning", "critical"] as const;

/** Union of `NOTIFICATION_SEVERITYS` wire names. */
export type NotificationSeverity = (typeof NOTIFICATION_SEVERITYS)[number];

/**
 * Who a notification type is for. The account audience is reserved to the
 * host; a plugin may declare only operational and patron types.
 *
 * Ordinal-ordered `NotificationAudience` wire names (index = Cap'n Proto ordinal).
 */
export const NOTIFICATION_AUDIENCES = ["operational", "patron", "account"] as const;

/** Union of `NOTIFICATION_AUDIENCES` wire names. */
export type NotificationAudience = (typeof NOTIFICATION_AUDIENCES)[number];

/**
 * Lifecycle step of an alert, for services such as PagerDuty that open and
 * close incidents.
 *
 * Ordinal-ordered `NotificationAlertAction` wire names (index = Cap'n Proto ordinal).
 */
export const NOTIFICATION_ALERT_ACTIONS = ["none", "trigger", "resolve"] as const;

/** Union of `NOTIFICATION_ALERT_ACTIONS` wire names. */
export type NotificationAlertAction = (typeof NOTIFICATION_ALERT_ACTIONS)[number];

/**
 * Kind of destination a `Notifier` sends to.
 *
 * Ordinal-ordered `NotifyDestinationKind` wire names (index = Cap'n Proto ordinal).
 */
export const NOTIFY_DESTINATION_KINDS = ["channel", "personal"] as const;

/** Union of `NOTIFY_DESTINATION_KINDS` wire names. */
export type NotifyDestinationKind = (typeof NOTIFY_DESTINATION_KINDS)[number];

/**
 * What a model does. A model has exactly one task, and the task fixes the
 * shape of `AiRunParams.input` and of the reply.
 *
 * Ordinal-ordered `AiTask` wire names (index = Cap'n Proto ordinal).
 */
export const AI_TASKS = ["textEmbeddings"] as const;

/** Union of `AI_TASKS` wire names. */
export type AiTask = (typeof AI_TASKS)[number];

/**
 * What a text is to an embedding model. Some models embed a search query
 * differently from the documents it is matched against.
 *
 * Ordinal-ordered `AiTextRole` wire names (index = Cap'n Proto ordinal).
 */
export const AI_TEXT_ROLES = ["document", "query"] as const;

/** Union of `AI_TEXT_ROLES` wire names. */
export type AiTextRole = (typeof AI_TEXT_ROLES)[number];

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
