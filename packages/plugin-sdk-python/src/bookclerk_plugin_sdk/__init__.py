"""Bookclerk Python plugin guest SDK.

Provides the workerd guest surface via :mod:`bookclerk_plugin_sdk.workerd`.
Authors define a module-level ``Default`` class extending
:class:`bookclerk_plugin_sdk.workerd.BookclerkEntrypoint` (``event(batch)`` /
``job(job)`` triggers) plus one class per ``plugin.toml`` entrypoint
(``class Cli(CliEntrypoint)``, ``class Storage(StorageEntrypoint)``, …).
Native guests use the Rust SDK (`serve` / `PluginWorker`). See ``docs/plugins/README.md``.

Typical import:

- Workerd: ``from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, js``
- Tools: ``python -m bookclerk_plugin_sdk check|fmt|types|package|smoke``

Every ABI struct, union, enum, and interface in :mod:`bookclerk_plugin_sdk.abi`
and the product constants in :mod:`bookclerk_plugin_sdk._abi` are generated
from ``crates/bookclerk-plugin-abi/schema/plugin.capnp`` — the single ABI
source of truth.
"""

from ._abi import PLUGIN_ERROR_CODES, PRODUCT_API_VERSION
from .abi import (
    Brand,
    CliInvokeParams,
    CliInvokeResult,
    CliSchema,
    DiagnoseResult,
    Entrypoint,
    FetchTitleParams,
    HealthOk,
    Invocation,
    LoginParams,
    PluginDescribe,
    PluginErrorCode,
    ScanParams,
)
from .db_value import (
    MAX_WRITE_KEY_BYTES,
    D1ExecResult,
    D1Meta,
    D1Result,
    DatabaseBinding,
    ExecuteStreamDecoder,
    ExecuteStreamLimits,
    RetryToken,
    WriteProgress,
    canonical_execute_request_hash,
    chunk_indexes,
    create_database_binding,
    decode_db_capabilities_reply,
    decode_db_value,
    decode_execute_request,
    decode_execute_result_reply,
    decode_execute_stream,
    encode_db_capabilities_reply,
    encode_db_value,
    encode_execute_request,
    encode_execute_result_reply,
    execute_reply_to_d1_results,
    execute_stream_limits,
    parse_db_value,
    raw_stream_ceiling,
    statement_result_to_d1_result,
    streams_results,
    write_chunks_operation_id,
)
from .limits import LimitExceeded, limit_exceeded_message, parse_limit_exceeded
from .workerd import WriteChunksError

__all__ = [
    "PLUGIN_ERROR_CODES",
    "PRODUCT_API_VERSION",
    "Brand",
    "CliInvokeParams",
    "CliInvokeResult",
    "CliSchema",
    "D1ExecResult",
    "Entrypoint",
    "D1Meta",
    "D1Result",
    "DatabaseBinding",
    "DiagnoseResult",
    "ExecuteStreamDecoder",
    "ExecuteStreamLimits",
    "FetchTitleParams",
    "HealthOk",
    "Invocation",
    "LoginParams",
    "PluginDescribe",
    "PluginErrorCode",
    "MAX_WRITE_KEY_BYTES",
    "RetryToken",
    "WriteChunksError",
    "WriteProgress",
    "write_chunks_operation_id",
    "ScanParams",
    "canonical_execute_request_hash",
    "chunk_indexes",
    "create_database_binding",
    "decode_db_value",
    "decode_execute_request",
    "decode_db_capabilities_reply",
    "decode_execute_result_reply",
    "decode_execute_stream",
    "encode_db_capabilities_reply",
    "LimitExceeded",
    "limit_exceeded_message",
    "parse_limit_exceeded",
    "encode_db_value",
    "encode_execute_request",
    "encode_execute_result_reply",
    "execute_reply_to_d1_results",
    "execute_stream_limits",
    "raw_stream_ceiling",
    "streams_results",
    "parse_db_value",
    "statement_result_to_d1_result",
]
