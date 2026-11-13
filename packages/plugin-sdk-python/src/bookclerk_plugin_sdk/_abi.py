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

MAX_CHECKPOINT_BYTES: int = 65536
"""Maximum job / event checkpoint payload size (bytes)."""

MAX_SCAN_PAGE_BOOKS: int = 500
"""Maximum titles, and separately removed product ids, in one `ScanPage`."""

MAX_SCAN_CURSOR_BYTES: int = 4096
"""Maximum bytes of a scan cursor or sync token."""

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

MAX_KV_KEY_BYTES: int = 512
"""Longest `KvNamespace` key in UTF-8 bytes."""

MAX_KV_METADATA_BYTES: int = 1024
"""Largest `KvNamespace` metadata JSON text in bytes."""

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

PLUGIN_ERROR_CODES: tuple[str, ...] = ("invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict", "limit_exceeded", "rate_limited")
"""Stable `PluginError.code` strings. Unknown future codes are forwarded
as-is; SDKs surface them as a local `unknown` while keeping the raw wire
code.

Wire strings of ``PluginErrorCode`` in ordinal order (snake_case ``Text`` codes).
"""

ENTRYPOINTS: tuple[str, ...] = ("storefront", "storage", "databaseAdapter", "remoteLibrary", "cli", "oidc", "metadata", "covers")
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

CATALOG_SORTS: tuple[str, ...] = ("relevance", "popularity", "rating", "title", "author")
"""Catalog search ordering.

Ordinal-ordered ``CatalogSort`` wire names (index = Cap'n Proto ordinal).
"""

CATALOG_FIELDS: tuple[str, ...] = ("any", "author", "narrator", "series", "genre")
"""Catalog search facet restricting which field the query matches.

Ordinal-ordered ``CatalogField`` wire names (index = Cap'n Proto ordinal).
"""

ABRIDGEMENTS: tuple[str, ...] = ("unknown", "unabridged", "abridged")
"""Whether an edition is abridged.

Ordinal-ordered ``Abridgement`` wire names (index = Cap'n Proto ordinal).
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
    "MAX_CHECKPOINT_BYTES",
    "MAX_SCAN_PAGE_BOOKS",
    "MAX_SCAN_CURSOR_BYTES",
    "MAX_EVENT_CURSOR_BYTES",
    "MAX_IDENTIFIER_BYTES",
    "MAX_CRON_BYTES",
    "MAX_CONFIG_PAYLOAD_BYTES",
    "MAX_EVENT_PAYLOAD_BYTES",
    "MAX_KV_VALUE_BYTES",
    "MAX_COVER_URLS",
    "MAX_COVER_BYTES",
    "MAX_COVER_HEADER_BYTES",
    "MAX_KV_KEY_BYTES",
    "MAX_KV_METADATA_BYTES",
    "MAX_PLUGIN_MIGRATION_OPS",
    "MAX_PLUGIN_MIGRATION_TOTAL_OPS",
    "MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES",
    "FEATURE_SCALAR_LIMITS",
    "FEATURE_STREAMS",
    "FEATURE_STORAGE_COPY",
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
    "CATALOG_SORTS",
    "CATALOG_FIELDS",
    "ABRIDGEMENTS",
    "DB_TYPES",
    "DB_STATEMENT_KINDS",
    "DB_RESULT_SELECTIONS",
    "ISOLATION_REQS",
    "RESOLVED_SQL_TYPES",
    "INTEGER_ARITH_KINDS",
]
