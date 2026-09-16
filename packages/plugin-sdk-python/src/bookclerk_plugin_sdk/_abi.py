"""GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.

Python projection of the product ABI constants and enum ordinal tables
declared in ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.
"""

from __future__ import annotations

PRODUCT_API_VERSION: int = 2
"""Product ABI version (`plugin.toml` `api_version` / `describe().apiVersion`)."""

MAX_SCALAR_BYTES: int = 262144
"""Maximum decoded size of an ordinary RPC scalar value (not a stream window)."""

MAX_STREAM_WINDOW_BYTES: int = 1048576
"""Maximum bytes returned by one `ByteSource.pull` (flow-control window)."""

MAX_LIST_PAGE: int = 256
"""Maximum objects in one `Destination.list` page."""

MAX_CHECKPOINT_BYTES: int = 65536
"""Maximum job / event checkpoint payload size (bytes)."""

MAX_IDENTIFIER_BYTES: int = 64
"""Maximum plugin / account identifier length (bytes)."""

MAX_CONFIG_PAYLOAD_BYTES: int = 65536
"""Maximum granted config payload size (bytes)."""

MAX_EVENT_PAYLOAD_BYTES: int = 65536
"""Maximum decoded size of a domain-event scalar payload (not a stream)."""

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

PLUGIN_ERROR_CODES: tuple[str, ...] = ("invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict")
"""Stable `PluginError.code` strings. Unknown future codes are forwarded
as-is; SDKs surface them as a local `unknown` while keeping the raw wire
code.

Wire strings of ``PluginErrorCode`` in ordinal order (snake_case ``Text`` codes).
"""

PORTAL_AUTH_MODES: tuple[str, ...] = ("unspecified", "password", "oauth")
"""Portal Accounts connect mode for storefronts.

Ordinal-ordered ``PortalAuthMode`` wire names (index = Cap'n Proto ordinal).
"""

CLI_ARG_KINDS: tuple[str, ...] = ("string", "bool", "int", "path")
"""Value kind for a `CliArgSpec`.

Ordinal-ordered ``CliArgKind`` wire names (index = Cap'n Proto ordinal).
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
    "MAX_IDENTIFIER_BYTES",
    "MAX_CONFIG_PAYLOAD_BYTES",
    "MAX_EVENT_PAYLOAD_BYTES",
    "MAX_PLUGIN_MIGRATION_OPS",
    "MAX_PLUGIN_MIGRATION_TOTAL_OPS",
    "MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES",
    "FEATURE_SCALAR_LIMITS",
    "FEATURE_STREAMS",
    "FEATURE_STORAGE_COPY",
    "PLUGIN_ERROR_CODES",
    "PORTAL_AUTH_MODES",
    "CLI_ARG_KINDS",
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
