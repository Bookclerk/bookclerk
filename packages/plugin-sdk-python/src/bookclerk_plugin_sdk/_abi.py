"""GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.

Python projection of the product ABI constants and database enum ordinal
tables declared in ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.
"""

from __future__ import annotations

PRODUCT_API_VERSION: int = 2
"""Product ABI version (`apiVersion` / `plugin.toml` `api_version`)."""

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
"""Guest honors scalar / stream-window / list-page caps."""

FEATURE_STREAMS: str = "rpc.streams"
"""Media moves through transferred `ByteRange` / `ByteSource` streams."""

FEATURE_STORAGE_COPY: str = "storage.copy"
"""Guest implements server-side `Destination.copy`."""

DB_STATEMENT_KINDS: tuple[str, ...] = ("execute", "select", "returning")
"""Ordinal-ordered `DbStatementKind` wire names (index = Cap'n Proto ordinal)."""

DB_RESULT_SELECTIONS: tuple[str, ...] = ("discard", "affectedRows", "rows")
"""Ordinal-ordered `DbResultSelection` wire names (index = Cap'n Proto ordinal)."""

DB_COLUMN_TYPES: tuple[str, ...] = ("unspecified", "bool", "int64", "float64", "text", "bytes")
"""Ordinal-ordered `DbType` column-type wire names (index = Cap'n Proto ordinal)."""

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
    "DB_STATEMENT_KINDS",
    "DB_RESULT_SELECTIONS",
    "DB_COLUMN_TYPES",
]
