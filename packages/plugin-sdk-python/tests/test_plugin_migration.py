"""N/N+1 resource bounds for plugin migration registration."""

import pytest

from bookclerk_plugin_sdk._abi import (
    MAX_LIST_PAGE,
    MAX_PLUGIN_MIGRATION_OPS,
    MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
    MAX_PLUGIN_MIGRATION_TOTAL_OPS,
    MAX_SCALAR_BYTES,
)
from bookclerk_plugin_sdk.plugin_migration import require_plugin_migration_registration
from bookclerk_plugin_sdk.workerd import PluginError


def _mig(ident: str, sql: str = "x", ops: int = 1) -> dict:
    return {
        "id": ident,
        "operations": [{"schema": sql} for _ in range(ops)],
    }


def test_max_migration_count_ok():
    require_plugin_migration_registration(
        [_mig(f"m{i:03}") for i in range(MAX_LIST_PAGE)]
    )


def test_migration_count_plus_one_payload_too_large():
    with pytest.raises(PluginError) as exc:
        require_plugin_migration_registration(
            [_mig(f"m{i:03}") for i in range(MAX_LIST_PAGE + 1)]
        )
    assert exc.value.code == "payload_too_large"
    assert "maxListPage" in str(exc.value)


def test_max_ops_ok():
    require_plugin_migration_registration([_mig("ops", ops=MAX_PLUGIN_MIGRATION_OPS)])


def test_ops_plus_one_payload_too_large():
    with pytest.raises(PluginError) as exc:
        require_plugin_migration_registration(
            [_mig("ops", ops=MAX_PLUGIN_MIGRATION_OPS + 1)]
        )
    assert exc.value.code == "payload_too_large"
    assert "maxPluginMigrationOps" in str(exc.value)


def test_max_total_ops_ok():
    per = MAX_PLUGIN_MIGRATION_OPS // 2
    n = MAX_PLUGIN_MIGRATION_TOTAL_OPS // per
    require_plugin_migration_registration(
        [_mig(f"t{i:03}", ops=per) for i in range(n)]
    )


def test_total_ops_plus_one_payload_too_large():
    per = MAX_PLUGIN_MIGRATION_OPS // 2
    n = MAX_PLUGIN_MIGRATION_TOTAL_OPS // per
    migrations = [_mig(f"t{i:03}", ops=per) for i in range(n)]
    migrations.append(_mig("extra", ops=1))
    with pytest.raises(PluginError) as exc:
        require_plugin_migration_registration(migrations)
    assert exc.value.code == "payload_too_large"
    assert "maxPluginMigrationTotalOps" in str(exc.value)


def test_max_sql_ok():
    require_plugin_migration_registration([_mig("", sql="x" * MAX_SCALAR_BYTES)])


def test_sql_plus_one_payload_too_large():
    with pytest.raises(PluginError) as exc:
        require_plugin_migration_registration(
            [_mig("sql", sql="x" * (MAX_SCALAR_BYTES + 1))]
        )
    assert exc.value.code == "payload_too_large"
    assert "maxScalarBytes" in str(exc.value)


def test_max_aggregate_ok():
    ident = "a"
    sql = "x" * (MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES - len(ident))
    require_plugin_migration_registration([_mig(ident, sql=sql)])


def test_aggregate_plus_one_payload_too_large():
    half = MAX_SCALAR_BYTES // 2 + 1
    sql = "x" * half
    with pytest.raises(PluginError) as exc:
        require_plugin_migration_registration([_mig("a", sql=sql), _mig("b", sql=sql)])
    assert exc.value.code == "payload_too_large"
    assert "maxPluginMigrationRegistrationBytes" in str(exc.value)
