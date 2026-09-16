"""Bookclerk Python plugin guest SDK.

Provides the workerd guest surface via :mod:`bookclerk_plugin_sdk.workerd`.
Authors subclass :class:`bookclerk_plugin_sdk.workerd.BookclerkPlugin` and
export the raw class. Native guests use the Rust SDK (`serve` / `PluginRoot`).
See ``docs/plugins.md``.

Typical import:

- Workerd: ``from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js``

The JSON payload contracts in :mod:`bookclerk_plugin_sdk.abi` and the product
constants in :mod:`bookclerk_plugin_sdk._abi` are generated from
``crates/bookclerk-plugin-abi/schema/plugin.capnp`` — the single ABI source of
truth.
"""

from ._abi import PRODUCT_API_VERSION
from .abi import (
    PLUGIN_ERROR_CODES,
    Brand,
    CliInvokeParams,
    CliInvokeResult,
    CliSchema,
    DiagnoseResult,
    FetchTitleParams,
    HealthResult,
    LoginParams,
    PluginErrorCode,
    PluginMetadata,
    ScanParams,
)
from .db_value import (
    D1ExecResult,
    D1Meta,
    D1Result,
    DatabaseBinding,
    RetryToken,
    canonical_execute_request_hash,
    create_database_binding,
    decode_db_value,
    decode_execute_request,
    decode_execute_result_reply,
    encode_db_value,
    encode_execute_request,
    encode_execute_result_reply,
    execute_reply_to_d1_results,
    parse_db_value,
    statement_result_to_d1_result,
)

__all__ = [
    "PLUGIN_ERROR_CODES",
    "PRODUCT_API_VERSION",
    "Brand",
    "CliInvokeParams",
    "CliInvokeResult",
    "CliSchema",
    "D1ExecResult",
    "D1Meta",
    "D1Result",
    "DatabaseBinding",
    "DiagnoseResult",
    "FetchTitleParams",
    "HealthResult",
    "LoginParams",
    "PluginErrorCode",
    "PluginMetadata",
    "RetryToken",
    "ScanParams",
    "canonical_execute_request_hash",
    "create_database_binding",
    "decode_db_value",
    "decode_execute_request",
    "decode_execute_result_reply",
    "encode_db_value",
    "encode_execute_request",
    "encode_execute_result_reply",
    "execute_reply_to_d1_results",
    "parse_db_value",
    "statement_result_to_d1_result",
]
