"""GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.

Python projection of the product ABI constants and enum ordinal tables
declared in ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.
"""

from __future__ import annotations

PRODUCT_API_VERSION: int = 3
"""Product ABI version (`plugin.toml` `api_version` / `describe().apiVersion`)."""

MAX_SCALAR_BYTES: int = 262144
"""Maximum decoded size of an ordinary RPC scalar value (not a stream window)."""

MAX_STREAM_WINDOW_BYTES: int = 1048576
"""Maximum bytes returned by one `ByteSource.pull` (flow-control window)."""

MAX_LIST_PAGE: int = 256
"""Maximum objects in one `Destination.list` page."""

MAX_BATCH_KEYS: int = 256
"""Maximum keys or key pairs in one `Destination.moveMany`, `deleteMany` or
`headMany` call, and maximum outcomes in one reply.
"""

MAX_CHANGES_PAGE: int = 256
"""Maximum changes in one `Destination.changes` page."""

MAX_CHANGES_CURSOR_BYTES: int = 4096
"""Maximum bytes of a `Destination.changes` cursor."""

MAX_CHECKPOINT_BYTES: int = 65536
"""Maximum job / event checkpoint payload size (bytes)."""

MAX_SCAN_PAGE_BOOKS: int = 500
"""Maximum titles, and separately removed product ids, in one `ScanPage`."""

MAX_SCAN_CURSOR_BYTES: int = 4096
"""Maximum bytes of a scan cursor or sync token."""

MAX_REFRESHED_CREDENTIAL_BYTES: int = 65536
"""Maximum bytes of refreshed credentials a storefront hands back
(`ScanPage.refreshedCredentials`, `PlainFetch.refreshedCredentials`,
`QualityOffersResult.refreshedCredentials`).
"""

MAX_EVENT_CURSOR_BYTES: int = 512
"""Maximum bytes of an `EventReadRequest.after` / `EventReadPage.cursor`."""

MAX_IDENTIFIER_BYTES: int = 64
"""Maximum plugin / account identifier length (bytes)."""

MAX_CRON_BYTES: int = 128
"""Maximum `[triggers] crons` source length (bytes), as in `ScheduledController.cron`."""

MAX_CONFIG_PAYLOAD_BYTES: int = 65536
"""Maximum granted config payload size (bytes)."""

MAX_EVENT_PAYLOAD_BYTES: int = 65536
"""Maximum decoded size of a domain-event scalar payload (not a stream)."""

MAX_KV_VALUE_BYTES: int = 1048576
"""Largest `KvNamespace` value in bytes; a `[[kv_namespaces]]` row may lower
its own cap but never raise it past this.
"""

MAX_COVER_URLS: int = 32
"""Most URLs in one `CoverFetcher.fetchCovers` call."""

MAX_COVER_BYTES: int = 10485760
"""Largest image body `CoverFetcher.fetchCovers` may return for one URL."""

MAX_COVER_HEADER_BYTES: int = 1024
"""Longest cache header value, in bytes, `CoverFetcher.fetchCovers` carries in
either direction (a condition the host sends, a header the server sent).
The host ignores a longer header the fetcher returns.
"""

MAX_METADATA_FILE_BYTES: int = 262144
"""Largest metadata file `MetadataFiles.parse` reads and `MetadataFiles.render`
returns. The host skips a larger sidecar before any plugin sees it.
"""

MAX_METADATA_FILE_DESCRIPTION_BYTES: int = 65536
"""Longest description a `MetadataFileRecord` carries, in UTF-8 bytes."""

MAX_METADATA_FILE_TEXT_BYTES: int = 1024
"""Longest other text value in a `MetadataFileRecord` (a title, a name, a
genre, a chapter title), in UTF-8 bytes.
"""

MAX_METADATA_FILE_LIST_ENTRIES: int = 256
"""Most entries in each list of a `MetadataFileRecord` other than chapters
(credits, genres, tags, identifiers, series).
"""

MAX_METADATA_FILE_CHAPTERS: int = 2000
"""Most chapters in a `MetadataFileRecord`."""

MAX_NOTIFICATION_TITLE_BYTES: int = 256
"""Longest `NotificationMessage.title` and `NotificationField` name, in UTF-8
bytes.
"""

MAX_NOTIFICATION_TEXT_BYTES: int = 16384
"""Longest `NotificationMessage.text` or `markdown`, in UTF-8 bytes."""

MAX_NOTIFICATION_HTML_BYTES: int = 65536
"""Longest `NotificationMessage.html`, in UTF-8 bytes."""

MAX_NOTIFICATION_FIELDS: int = 25
"""Most `NotificationField`s on one message or raise."""

MAX_NOTIFICATION_FIELD_BYTES: int = 1024
"""Longest `NotificationField.value`, link, group key or alert key, in UTF-8
bytes.
"""

MAX_NOTIFIER_ADDRESS_BYTES: int = 4096
"""Longest destination address document (`addressJson`), in UTF-8 bytes."""

MAX_METADATA_AUDIO_FILES: int = 4096
"""Most entries in `MetadataFileRenderParams.audioFiles`."""

MAX_KV_KEY_BYTES: int = 512
"""Longest `KvNamespace` key in UTF-8 bytes."""

MAX_SOCKET_FRAME_BYTES: int = 262144
"""Largest WebSocket message, text or binary, that `Sockets.send` accepts or
`SocketHandler.message` delivers.
"""

MAX_SOCKET_ATTACHMENT_BYTES: int = 2048
"""Largest per-socket attachment the host keeps for a sleeping guest."""

MAX_SOCKETS_PER_INSTANCE: int = 8
"""Most host-brokered sockets one plugin instance may hold open at once."""

MAX_SOCKET_AUTO_RESPONSES: int = 4
"""Most heartbeat pairs one `Sockets.connect` may declare."""

MAX_SOCKET_AUTO_RESPONSE_BYTES: int = 256
"""Largest heartbeat request or response frame, in bytes."""

MAX_SOCKET_URL_BYTES: int = 2048
"""Longest `SocketConnectRequest.url`, in bytes."""

MAX_SOCKET_HEADERS: int = 16
"""Most request headers, and separately subprotocols, on one `Sockets.connect`."""

MAX_ORIGIN_HEADERS: int = 128
"""Most header fields on one `Origin.fetch` request, and separately on its
response.
"""

MAX_ORIGIN_HEADER_BYTES: int = 65536
"""Largest total UTF-8 bytes of header names and values on one `Origin.fetch`
request, and separately on its response.
"""

MAX_ORIGIN_TARGET_BYTES: int = 8192
"""Longest `OriginRequest.target` (path and query), in bytes."""

MAX_INGRESS_ORIGIN_BYTES: int = 253
"""Longest `IngressStarted.origin`, in bytes."""

MAX_KV_METADATA_BYTES: int = 1024
"""Largest `KvNamespace` metadata JSON text in bytes."""

MAX_NETWORK_GRANT_ENTRIES: int = 256
"""Most entries in each list of a `NetworkGrant` (fetch hosts, TCP
destinations, network settings).
"""

MAX_PLUGIN_MIGRATION_OPS: int = 256
"""Maximum already-separated operations in one plugin-owned migration."""

MAX_PLUGIN_MIGRATION_TOTAL_OPS: int = 2048
"""Maximum already-separated operations across one `databaseMigrations` registration."""

MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES: int = 262144
"""Maximum aggregate UTF-8 bytes of plugin migration ids plus SQL in one `databaseMigrations` registration."""

FEATURE_SCALAR_LIMITS: str = "rpc.scalarLimits"
"""Guest honors scalar / stream-window / list-page caps (`rpc.scalarLimits`)."""

FEATURE_STREAMS: str = "rpc.streams"
"""Media moves through transferred `ByteRange` / `ByteSource` streams (`rpc.streams`)."""

FEATURE_STORAGE_COPY: str = "storage.copy"
"""Guest implements server-side `Destination.copy` (`storage.copy`)."""

FEATURE_STORAGE_MOVE: str = "storage.move"
"""Guest implements `Destination.move` (`storage.move`)."""

FEATURE_STORAGE_MOVE_MANY: str = "storage.moveMany"
"""Guest implements `Destination.moveMany` (`storage.moveMany`)."""

FEATURE_STORAGE_DELETE_MANY: str = "storage.deleteMany"
"""Guest implements `Destination.deleteMany` (`storage.deleteMany`)."""

FEATURE_STORAGE_HEAD_MANY: str = "storage.headMany"
"""Guest implements `Destination.headMany` (`storage.headMany`)."""

FEATURE_STORAGE_CONDITIONAL_WRITE: str = "storage.conditionalWrite"
"""Guest enforces `WriteOptions.ifMatch` / `ifNoneMatch` (`storage.conditionalWrite`)."""

FEATURE_STORAGE_CHANGES: str = "storage.changes"
"""Guest implements `Destination.changes` (`storage.changes`)."""

FEATURE_STORAGE_TRANSLATE_KEYS: str = "storage.translateKeys"
"""Guest implements `Destination.translateKeys` (`storage.translateKeys`)."""

OBJECT_CHANGE_KINDS: tuple[str, ...] = ("changed", "upserted", "removed")
"""What happened to an object in a change feed. Unknown codes are kept and
treated as `changed`.

Ordinal-ordered ``ObjectChangeKind`` wire names (index = Cap'n Proto ordinal).
"""

SETTING_NETWORK_KINDS: tuple[str, ...] = ("fetch", "tcp")
"""How a setting's destination is reached.

Ordinal-ordered ``SettingNetworkKind`` wire names (index = Cap'n Proto ordinal).
"""

SETTING_GRANT_STATES: tuple[str, ...] = ("approved", "pending", "unset", "invalid")
"""Where a network setting's destination stands.

Ordinal-ordered ``SettingGrantState`` wire names (index = Cap'n Proto ordinal).
"""

PLUGIN_ERROR_CODES: tuple[str, ...] = ("invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict", "limit_exceeded", "rate_limited", "network_not_granted")
"""Stable `PluginError.code` strings. Unknown future codes are forwarded
as-is; SDKs surface them as a local `unknown` while keeping the raw wire
code.

Wire strings of ``PluginErrorCode`` in ordinal order (snake_case ``Text`` codes).
"""

ENTRYPOINTS: tuple[str, ...] = ("storefront", "storage", "databaseAdapter", "remoteLibrary", "cli", "oidc", "metadata", "covers", "metadataFiles", "notifier", "ingress")
"""Named entrypoint a plugin exports (Cloudflare Workers named-entrypoint
analogue). Each value is a capability the host calls over RPC; triggers on
the default entrypoint (`event`, `job`) are declared separately.

Ordinal-ordered ``Entrypoint`` wire names (index = Cap'n Proto ordinal).
"""

PORTAL_AUTH_MODES: tuple[str, ...] = ("unspecified", "password", "oauth")
"""Portal Accounts connect mode for storefronts.

Ordinal-ordered ``PortalAuthMode`` wire names (index = Cap'n Proto ordinal).
"""

CLI_ARG_KINDS: tuple[str, ...] = ("string", "bool", "int", "path")
"""Value kind for a `CliArgSpec`.

Ordinal-ordered ``CliArgKind`` wire names (index = Cap'n Proto ordinal).
"""

WORK_KINDS: tuple[str, ...] = ("unspecified", "book", "podcast")
"""What family of creative work a record describes (FRBR work). Every record
names one: `unspecified` is refused by the host, so a guest that forgets
the field fails instead of silently describing a book. Whether a podcast
work is a show or an episode is structural, not a kind: an episode names
its show through `parentProductId`, a show names no parent.

Ordinal-ordered ``WorkKind`` wire names (index = Cap'n Proto ordinal).
"""

EXPRESSION_KINDS: tuple[str, ...] = ("unspecified", "text", "narration", "dramatisation", "graphic", "originalAudio")
"""How a work is realised (FRBR expression): the kind of content you get,
independent of the carrier. A translation is not a kind: the expression's
language and its translator credits say it. `unspecified` is refused.

Ordinal-ordered ``ExpressionKind`` wire names (index = Cap'n Proto ordinal).
"""

MEDIA_KINDS: tuple[str, ...] = ("unspecified", "audio", "ebook", "print")
"""Carrier of an edition (FRBR manifestation): what form you receive.
Whether the host acquires an edition depends on the storefront's
`MediaSupport.fetches` and what the host supports, not on this value
alone. `unspecified` is refused.

Ordinal-ordered ``MediaKind`` wire names (index = Cap'n Proto ordinal).
"""

IDENTIFIER_SCHEMES: tuple[str, ...] = ("isbn", "asin", "openLibraryEdition", "openLibraryWork")
"""Scheme of an external identifier. The scheme fixes whether the id names a
work or an edition, and whether it is strong (unique) or weak (shared).

Ordinal-ordered ``IdentifierScheme`` wire names (index = Cap'n Proto ordinal).
"""

CREDIT_ROLES: tuple[str, ...] = ("author", "narrator", "translator", "editor", "illustrator", "publisher", "host", "cast", "adaptor")
"""A contributor's responsibility for a work, expression or edition.

Ordinal-ordered ``CreditRole`` wire names (index = Cap'n Proto ordinal).
"""

CREDIT_LEVELS: tuple[str, ...] = ("roleDefault", "work", "expression", "edition")
"""Which level a credit attaches to. Ordinal 0 is the role's usual level, so
a guest sets this only when a credit departs from it.

Ordinal-ordered ``CreditLevel`` wire names (index = Cap'n Proto ordinal).
"""

FETCHED_FILE_ROLES: tuple[str, ...] = ("unspecified", "part", "whole", "cover")
"""What one fetched file is to the edition it belongs to.

Ordinal-ordered ``FetchedFileRole`` wire names (index = Cap'n Proto ordinal).
"""

CHAPTER_KINDS: tuple[str, ...] = ("unknown", "chapters", "tracks")
"""What a storefront's chapter markers mark (`PlainFetch.chapterKind`).

Ordinal-ordered ``ChapterKind`` wire names (index = Cap'n Proto ordinal).
"""

CATALOG_SORTS: tuple[str, ...] = ("relevance", "popularity", "rating", "title", "author")
"""Catalog search ordering.

Ordinal-ordered ``CatalogSort`` wire names (index = Cap'n Proto ordinal).
"""

CATALOG_FIELDS: tuple[str, ...] = ("any", "author", "narrator", "series", "genre")
"""Catalog search facet restricting which field the query matches.

Ordinal-ordered ``CatalogField`` wire names (index = Cap'n Proto ordinal).
"""

PRICE_LOOKUPS: tuple[str, ...] = ("unspecified", "product", "isbn", "asin", "title")
"""How a purchase-hint query names the title to price.

Ordinal-ordered ``PriceLookup`` wire names (index = Cap'n Proto ordinal).
"""

ABRIDGEMENTS: tuple[str, ...] = ("unknown", "unabridged", "abridged")
"""Whether an edition is abridged.

Ordinal-ordered ``Abridgement`` wire names (index = Cap'n Proto ordinal).
"""

EXPLICITNESSS: tuple[str, ...] = ("unknown", "clean", "explicit")
"""Whether an edition is marked as explicit (adult) content. A patron scope
can hide explicit editions; `unknown` and `clean` editions stay visible.

Ordinal-ordered ``Explicitness`` wire names (index = Cap'n Proto ordinal).
"""

OFFERED_CODECS: tuple[str, ...] = ("unknown", "aac", "alac", "flac", "mp3", "opus", "vorbis", "eac3", "ac4")
"""Audio codec of a stream a store offers (`QualityOffer.codec`). Spelled as
the host's probed codecs are, so an offer compares with a held file.

Ordinal-ordered ``OfferedCodec`` wire names (index = Cap'n Proto ordinal).
"""

OFFERED_AAC_PROFILES: tuple[str, ...] = ("unknown", "lc", "he", "heV2", "xhe")
"""AAC profile of an offered AAC stream (`QualityOffer.aacProfile`).

Ordinal-ordered ``OfferedAacProfile`` wire names (index = Cap'n Proto ordinal).
"""

METADATA_FIELDS: tuple[str, ...] = ("title", "subtitle", "description", "publisher", "publishedAt", "language", "genres", "tags", "abridgement", "expressionKind", "identifiers", "credits", "series", "chapters", "duration", "explicitness")
"""A field of a `MetadataFileRecord`. Dialects declare in the manifest which
fields their format carries; a parsed record lists the ones its file
states, and a render request lists the ones to write.

Ordinal-ordered ``MetadataField`` wire names (index = Cap'n Proto ordinal).
"""

NOTIFICATION_SEVERITYS: tuple[str, ...] = ("info", "warning", "critical")
"""How urgent a notification is. Routing rules carry a minimum severity.

Ordinal-ordered ``NotificationSeverity`` wire names (index = Cap'n Proto ordinal).
"""

NOTIFICATION_AUDIENCES: tuple[str, ...] = ("operational", "patron", "account")
"""Who a notification type is for. The account audience is reserved to the
host; a plugin may declare only operational and patron types.

Ordinal-ordered ``NotificationAudience`` wire names (index = Cap'n Proto ordinal).
"""

NOTIFICATION_ALERT_ACTIONS: tuple[str, ...] = ("none", "trigger", "resolve")
"""Lifecycle step of an alert, for services such as PagerDuty that open and
close incidents.

Ordinal-ordered ``NotificationAlertAction`` wire names (index = Cap'n Proto ordinal).
"""

NOTIFY_DESTINATION_KINDS: tuple[str, ...] = ("channel", "personal")
"""Kind of destination a `Notifier` sends to.

Ordinal-ordered ``NotifyDestinationKind`` wire names (index = Cap'n Proto ordinal).
"""

DB_TYPES: tuple[str, ...] = ("unspecified", "bool", "int64", "float64", "text", "bytes")
"""Universal database cell/parameter domain. Engine-native arrays, enums,
unsigned integers, and JSON text sentinels are not baseline ABI values.

Ordinal-ordered ``DbType`` wire names (index = Cap'n Proto ordinal).
"""

DB_STATEMENT_KINDS: tuple[str, ...] = ("execute", "select", "returning")
"""How the host classifies a statement for result handling.

Ordinal-ordered ``DbStatementKind`` wire names (index = Cap'n Proto ordinal).
"""

DB_RESULT_SELECTIONS: tuple[str, ...] = ("discard", "affectedRows", "rows")
"""Which part of a statement's outcome the caller wants back.

Ordinal-ordered ``DbResultSelection`` wire names (index = Cap'n Proto ordinal).
"""

ISOLATION_REQS: tuple[str, ...] = ("atomicBatch", "nestedSavepoint", "consistentSnapshot")
"""Host → adapter execute. GuestDatabase stays ExecuteRequest-only.

Ordinal-ordered ``IsolationReq`` wire names (index = Cap'n Proto ordinal).
"""

RESOLVED_SQL_TYPES: tuple[str, ...] = ("integer", "real", "text", "blob", "boolean", "null")
"""Resolved BookclerkSQL column type after type checking.

Ordinal-ordered ``ResolvedSqlType`` wire names (index = Cap'n Proto ordinal).
"""

INTEGER_ARITH_KINDS: tuple[str, ...] = ("add", "sub", "mul", "abs")
"""INTEGER `+` `-` `*` `abs` site lowered to overflow -> NULL.

Ordinal-ordered ``IntegerArithKind`` wire names (index = Cap'n Proto ordinal).
"""

__all__ = [
    "PRODUCT_API_VERSION",
    "MAX_SCALAR_BYTES",
    "MAX_STREAM_WINDOW_BYTES",
    "MAX_LIST_PAGE",
    "MAX_BATCH_KEYS",
    "MAX_CHANGES_PAGE",
    "MAX_CHANGES_CURSOR_BYTES",
    "MAX_CHECKPOINT_BYTES",
    "MAX_SCAN_PAGE_BOOKS",
    "MAX_SCAN_CURSOR_BYTES",
    "MAX_REFRESHED_CREDENTIAL_BYTES",
    "MAX_EVENT_CURSOR_BYTES",
    "MAX_IDENTIFIER_BYTES",
    "MAX_CRON_BYTES",
    "MAX_CONFIG_PAYLOAD_BYTES",
    "MAX_EVENT_PAYLOAD_BYTES",
    "MAX_KV_VALUE_BYTES",
    "MAX_COVER_URLS",
    "MAX_COVER_BYTES",
    "MAX_COVER_HEADER_BYTES",
    "MAX_METADATA_FILE_BYTES",
    "MAX_METADATA_FILE_DESCRIPTION_BYTES",
    "MAX_METADATA_FILE_TEXT_BYTES",
    "MAX_METADATA_FILE_LIST_ENTRIES",
    "MAX_METADATA_FILE_CHAPTERS",
    "MAX_NOTIFICATION_TITLE_BYTES",
    "MAX_NOTIFICATION_TEXT_BYTES",
    "MAX_NOTIFICATION_HTML_BYTES",
    "MAX_NOTIFICATION_FIELDS",
    "MAX_NOTIFICATION_FIELD_BYTES",
    "MAX_NOTIFIER_ADDRESS_BYTES",
    "MAX_METADATA_AUDIO_FILES",
    "MAX_KV_KEY_BYTES",
    "MAX_SOCKET_FRAME_BYTES",
    "MAX_SOCKET_ATTACHMENT_BYTES",
    "MAX_SOCKETS_PER_INSTANCE",
    "MAX_SOCKET_AUTO_RESPONSES",
    "MAX_SOCKET_AUTO_RESPONSE_BYTES",
    "MAX_SOCKET_URL_BYTES",
    "MAX_SOCKET_HEADERS",
    "MAX_ORIGIN_HEADERS",
    "MAX_ORIGIN_HEADER_BYTES",
    "MAX_ORIGIN_TARGET_BYTES",
    "MAX_INGRESS_ORIGIN_BYTES",
    "MAX_KV_METADATA_BYTES",
    "MAX_NETWORK_GRANT_ENTRIES",
    "MAX_PLUGIN_MIGRATION_OPS",
    "MAX_PLUGIN_MIGRATION_TOTAL_OPS",
    "MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES",
    "FEATURE_SCALAR_LIMITS",
    "FEATURE_STREAMS",
    "FEATURE_STORAGE_COPY",
    "FEATURE_STORAGE_MOVE",
    "FEATURE_STORAGE_MOVE_MANY",
    "FEATURE_STORAGE_DELETE_MANY",
    "FEATURE_STORAGE_HEAD_MANY",
    "FEATURE_STORAGE_CONDITIONAL_WRITE",
    "FEATURE_STORAGE_CHANGES",
    "FEATURE_STORAGE_TRANSLATE_KEYS",
    "OBJECT_CHANGE_KINDS",
    "SETTING_NETWORK_KINDS",
    "SETTING_GRANT_STATES",
    "PLUGIN_ERROR_CODES",
    "ENTRYPOINTS",
    "PORTAL_AUTH_MODES",
    "CLI_ARG_KINDS",
    "WORK_KINDS",
    "EXPRESSION_KINDS",
    "MEDIA_KINDS",
    "IDENTIFIER_SCHEMES",
    "CREDIT_ROLES",
    "CREDIT_LEVELS",
    "FETCHED_FILE_ROLES",
    "CHAPTER_KINDS",
    "CATALOG_SORTS",
    "CATALOG_FIELDS",
    "PRICE_LOOKUPS",
    "ABRIDGEMENTS",
    "EXPLICITNESSS",
    "OFFERED_CODECS",
    "OFFERED_AAC_PROFILES",
    "METADATA_FIELDS",
    "NOTIFICATION_SEVERITYS",
    "NOTIFICATION_AUDIENCES",
    "NOTIFICATION_ALERT_ACTIONS",
    "NOTIFY_DESTINATION_KINDS",
    "DB_TYPES",
    "DB_STATEMENT_KINDS",
    "DB_RESULT_SELECTIONS",
    "ISOLATION_REQS",
    "RESOLVED_SQL_TYPES",
    "INTEGER_ARITH_KINDS",
]
