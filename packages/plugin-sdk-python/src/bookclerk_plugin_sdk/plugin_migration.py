"""Resource bounds for plugin-owned ``databaseMigrations`` registration.

Semantic BookclerkSQL proof stays on the host. This module only rejects
pathological list / SQL sizes before JSON encoding.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from ._abi import (
    MAX_LIST_PAGE,
    MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
    MAX_PLUGIN_MIGRATION_TOTAL_OPS,
    MAX_SCALAR_BYTES,
)

if TYPE_CHECKING:
    from .workerd import PluginError

__all__ = ["require_plugin_migration_registration"]


def _too_large(message: str) -> PluginError:
    """Build a typed ``payload_too_large`` plugin error.

    Args:
        message: Operator-facing limit text.

    Returns:
        A ``PluginError`` with ``code="payload_too_large"``.
    """
    from .workerd import PluginError

    return PluginError.from_wire("payload_too_large", message)


def _utf8_len(value: object) -> int:
    """UTF-8 byte length of a string-like value (``None`` is empty)."""
    return len(str(value or "").encode("utf-8"))


def _op_sql(op: Any) -> str:
    """Return the schema or data SQL string from one operation object."""
    if isinstance(op, dict):
        schema = op.get("schema")
        if isinstance(schema, str):
            return schema
        data = op.get("data")
        if isinstance(data, str):
            return data
    return ""


def require_plugin_migration_registration(migrations: Any) -> list[Any]:
    """Reject a registration that exceeds ABI resource limits.

    Args:
        migrations: Ordered ``databaseMigrations`` list.

    Returns:
        The same list when within limits, or ``[]`` when ``migrations`` is not a list.

    Raises:
        PluginError: With ``code="payload_too_large"`` when a limit is exceeded.
    """
    if not isinstance(migrations, list):
        return []
    if len(migrations) > MAX_LIST_PAGE:
        raise _too_large(
            f"plugin migration count {len(migrations)} exceeds maxListPage ({MAX_LIST_PAGE})",
        )
    total = 0
    total_ops = 0
    for migration in migrations:
        ops = migration.get("operations") if isinstance(migration, dict) else None
        if not isinstance(ops, list):
            ops = getattr(migration, "operations", None)
        if not isinstance(ops, list):
            ops = []
        ident = (
            migration.get("id")
            if isinstance(migration, dict)
            else getattr(migration, "id", "")
        )
        if len(ops) > MAX_PLUGIN_MIGRATION_OPS:
            raise _too_large(
                f"plugin migration `{ident}` has {len(ops)} operations; "
                f"exceeds maxPluginMigrationOps ({MAX_PLUGIN_MIGRATION_OPS})",
            )
        total_ops += len(ops)
        if total_ops > MAX_PLUGIN_MIGRATION_TOTAL_OPS:
            raise _too_large(
                f"plugin migration registration has {total_ops} operations; exceeds "
                f"maxPluginMigrationTotalOps ({MAX_PLUGIN_MIGRATION_TOTAL_OPS})",
            )
        total += _utf8_len(ident)
        if total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES:
            raise _too_large(
                f"plugin migration registration is {total} bytes; exceeds "
                f"maxPluginMigrationRegistrationBytes ({MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})",
            )
        for op in ops:
            n = _utf8_len(_op_sql(op))
            if n > MAX_SCALAR_BYTES:
                raise _too_large(
                    f"plugin migration `{ident}` SQL is {n} bytes; "
                    f"exceeds maxScalarBytes ({MAX_SCALAR_BYTES})",
                )
            total += n
            if total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES:
                raise _too_large(
                    f"plugin migration registration is {total} bytes; exceeds "
                    f"maxPluginMigrationRegistrationBytes ({MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})",
                )
    return migrations
