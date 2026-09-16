"""GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` after changing crates/bookclerk-plugin-abi/schema/plugin.capnp.

Unpacked single-segment Cap'n Proto codecs for every struct and method
envelope in ``plugin.capnp``. Layout numbers come from
``schema/plugin.layout.json`` (Cap'n Proto compiler output), not from
hand-derived tables. Pointer targets are allocated in declaration order,
depth first, matching the ``capnpc-rust`` builder so Rust, TypeScript, and
Python produce byte-identical messages for the same value.
"""

from __future__ import annotations

from typing import Any

from . import _abi as A
from ._capnp import (
    _CapnpMessage,
    _CapnpReader,
    _CapnpStruct,
    _CapTable,
    _StructReader,
    NO_CAPS,
)


class _Codec:
    """Struct codec over the runtime Cap'n builders/readers."""

    def __init__(self, data_words: int, pointer_count: int, write: Any, read: Any) -> None:
        self.data_words = data_words
        self.pointer_count = pointer_count
        self.write = write
        self.read = read


def _encode_message(codec: _Codec, value: Any, caps: _CapTable = NO_CAPS) -> bytes:
    """Encode ``value`` as a standalone unpacked Cap'n message."""
    msg = _CapnpMessage()
    codec.write(msg.init_root(codec.data_words, codec.pointer_count), value, caps)
    return msg.finish()


def _decode_message(codec: _Codec, data: bytes, caps: _CapTable = NO_CAPS) -> Any:
    """Decode a standalone unpacked Cap'n message."""
    return codec.read(_CapnpReader(data).root(codec.data_words, codec.pointer_count), caps)


def _ord(table: tuple[str, ...], name: str, enum_name: str) -> int:
    try:
        return table.index(name)
    except ValueError as exc:
        raise ValueError(f"unknown {enum_name} value: {name}") from exc


def _from_ord(table: tuple[str, ...], ordinal: int, enum_name: str) -> str:
    if ordinal < 0 or ordinal >= len(table):
        raise ValueError(f"unknown {enum_name} ordinal: {ordinal}")
    return table[ordinal]


def _write_scalar_limits(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["maxScalarBytes"])
    s.set_u32(1, v["maxStreamWindowBytes"])
    s.set_u32(2, v["maxListPage"])


def _read_scalar_limits(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "maxScalarBytes": s.get_u32(0),
        "maxStreamWindowBytes": s.get_u32(1),
        "maxListPage": s.get_u32(2),
    }


_scalar_limits_codec = _Codec(2, 0, _write_scalar_limits, _read_scalar_limits)
"""Wire codec for ``ScalarLimits`` (2 data words, 0 pointers)."""


def _write_plugin_error(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["code"])
    s.set_text(1, v["message"])


def _read_plugin_error(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "code": s.get_text(0),
        "message": s.get_text(1),
    }


_plugin_error_codec = _Codec(0, 2, _write_plugin_error, _read_plugin_error)
"""Wire codec for ``PluginError`` (0 data words, 2 pointers)."""


def _write_object_metadata(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    s.set_u64(0, v["size"])
    s.set_text(1, v["contentType"])
    s.set_text(2, v["etag"])
    s.set_data(3, v["sha256"])


def _read_object_metadata(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "size": s.get_u64(0),
        "contentType": s.get_text(1),
        "etag": s.get_text(2),
        "sha256": s.get_data(3),
    }


_object_metadata_codec = _Codec(1, 4, _write_object_metadata, _read_object_metadata)
"""Wire codec for ``ObjectMetadata`` (1 data words, 4 pointers)."""


def _write_object_info(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    s.set_u64(0, v["size"])


def _read_object_info(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "size": s.get_u64(0),
    }


_object_info_codec = _Codec(1, 1, _write_object_info, _read_object_info)
"""Wire codec for ``ObjectInfo`` (1 data words, 1 pointers)."""


def _write_list_options(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["prefix"])
    s.set_text(1, v["cursor"])
    s.set_u32(0, v["limit"])


def _read_list_options(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "prefix": s.get_text(0),
        "cursor": s.get_text(1),
        "limit": s.get_u32(0),
    }


_list_options_codec = _Codec(1, 2, _write_list_options, _read_list_options)
"""Wire codec for ``ListOptions`` (1 data words, 2 pointers)."""


def _write_list_page(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["objects"]), 1, 1)
    for item, elem in zip(items, v["objects"], strict=True):
        _object_info_codec.write(item, elem, caps)
    s.set_text(1, v["nextCursor"])


def _read_list_page(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "objects": [_object_info_codec.read(item, caps) for item in s.get_struct_list(0, 1, 1)],
        "nextCursor": s.get_text(1),
    }


_list_page_codec = _Codec(0, 2, _write_list_page, _read_list_page)
"""Wire codec for ``ListPage`` (0 data words, 2 pointers)."""


def _write_byte_range(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u64(0, v["offset"])
    s.set_u64(1, v["length"])


def _read_byte_range(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "offset": s.get_u64(0),
        "length": s.get_u64(1),
    }


_byte_range_codec = _Codec(2, 0, _write_byte_range, _read_byte_range)
"""Wire codec for ``ByteRange`` (2 data words, 0 pointers)."""


def _write_read_options(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _byte_range_codec.write(s.init_struct(0, 2, 0), v["range"], caps)


def _read_read_options(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "range": _byte_range_codec.read(s.get_struct(0, 2, 0), caps),
    }


_read_options_codec = _Codec(0, 1, _write_read_options, _read_read_options)
"""Wire codec for ``ReadOptions`` (0 data words, 1 pointers)."""


def _write_write_options(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["contentType"])
    s.set_u64(0, v["contentLength"])
    s.set_data(1, v["sha256"])
    s.set_text(2, v["commitToken"])
    s.set_bool(64, v["stageOnly"])


def _read_write_options(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "contentType": s.get_text(0),
        "contentLength": s.get_u64(0),
        "sha256": s.get_data(1),
        "commitToken": s.get_text(2),
        "stageOnly": s.get_bool(64),
    }


_write_options_codec = _Codec(2, 3, _write_write_options, _read_write_options)
"""Wire codec for ``WriteOptions`` (2 data words, 3 pointers)."""


def _write_put_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    s.set_u64(0, v["bytesWritten"])
    s.set_text(1, v["etag"])
    s.set_data(2, v["sha256"])


def _read_put_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "bytesWritten": s.get_u64(0),
        "etag": s.get_text(1),
        "sha256": s.get_data(2),
    }


_put_result_codec = _Codec(1, 3, _write_put_result, _read_put_result)
"""Wire codec for ``PutResult`` (1 data words, 3 pointers)."""


def _write_copy_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u64(0, v["bytesCopied"])


def _read_copy_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "bytesCopied": s.get_u64(0),
    }


_copy_result_codec = _Codec(1, 0, _write_copy_result, _read_copy_result)
"""Wire codec for ``CopyResult`` (1 data words, 0 pointers)."""


def _write_plugin_describe(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["apiVersion"])
    s.set_text(0, v["id"])
    s.set_text(1, v["displayName"])
    s.set_text_list(2, v["rpcFeatures"])
    _scalar_limits_codec.write(s.init_struct(3, 2, 0), v["scalarLimits"], caps)
    _plugin_capabilities_codec.write(s.init_struct(4, 0, 6), v["capabilities"], caps)
    s.set_u16(2, _ord(A.PORTAL_AUTH_MODES, v["portalAuthMode"], "PortalAuthMode"))
    if v.get("passwordEnvVar") is not None:
        s.set_text(5, v["passwordEnvVar"])
    s.set_text_list(6, v["aliases"])
    s.set_u32(2, v["sortKey"])
    _brand_codec.write(s.init_struct(7, 0, 6), v["brand"], caps)
    items = s.init_struct_list(8, len(v["configOptions"]), 0, 3)
    for item, elem in zip(items, v["configOptions"], strict=True):
        _config_option_codec.write(item, elem, caps)
    _cli_schema_codec.write(s.init_struct(9, 0, 1), v["cli"], caps)


def _read_plugin_describe(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "apiVersion": s.get_u32(0),
        "id": s.get_text(0),
        "displayName": s.get_text(1),
        "rpcFeatures": s.get_text_list(2),
        "scalarLimits": _scalar_limits_codec.read(s.get_struct(3, 2, 0), caps),
        "capabilities": _plugin_capabilities_codec.read(s.get_struct(4, 0, 6), caps),
        "portalAuthMode": _from_ord(A.PORTAL_AUTH_MODES, s.get_u16(2), "PortalAuthMode"),
        "aliases": s.get_text_list(6),
        "sortKey": s.get_u32(2),
        "brand": _brand_codec.read(s.get_struct(7, 0, 6), caps),
        "configOptions": [_config_option_codec.read(item, caps) for item in s.get_struct_list(8, 0, 3)],
        "cli": _cli_schema_codec.read(s.get_struct(9, 0, 1), caps),
    }
    password_env_var = s.get_text(5)
    if not (password_env_var == ""):
        out["passwordEnvVar"] = password_env_var
    return out


_plugin_describe_codec = _Codec(2, 10, _write_plugin_describe, _read_plugin_describe)
"""Wire codec for ``PluginDescribe`` (2 data words, 10 pointers)."""


def _write_oidc_client_template(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["clientId"])
    s.set_text(1, v["displayName"])
    s.set_text(2, v["callbackPath"])
    s.set_bool(0, v["publicClient"])
    s.set_text_list(3, v["defaultScopes"])
    s.set_bool(1, v["issueRefreshToken"])
    s.set_text(4, v["originConfigKey"])


def _read_oidc_client_template(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "clientId": s.get_text(0),
        "displayName": s.get_text(1),
        "callbackPath": s.get_text(2),
        "publicClient": s.get_bool(0),
        "defaultScopes": s.get_text_list(3),
        "issueRefreshToken": s.get_bool(1),
        "originConfigKey": s.get_text(4),
    }


_oidc_client_template_codec = _Codec(1, 5, _write_oidc_client_template, _read_oidc_client_template)
"""Wire codec for ``OidcClientTemplate`` (1 data words, 5 pointers)."""


def _write_oidc_clients_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["clients"]), 1, 5)
    for item, elem in zip(items, v["clients"], strict=True):
        _oidc_client_template_codec.write(item, elem, caps)


def _read_oidc_clients_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "clients": [_oidc_client_template_codec.read(item, caps) for item in s.get_struct_list(0, 1, 5)],
    }


_oidc_clients_ok_codec = _Codec(0, 1, _write_oidc_clients_ok, _read_oidc_clients_ok)
"""Wire codec for ``OidcClientsOk`` (0 data words, 1 pointers)."""


def _write_oidc_clients_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _oidc_clients_ok_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown OidcClientsReply union member: {kind}")


def _read_oidc_clients_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _oidc_clients_ok_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown OidcClientsReply union member: {disc}")


_oidc_clients_reply_codec = _Codec(1, 1, _write_oidc_clients_reply, _read_oidc_clients_reply)
"""Wire codec for ``OidcClientsReply`` (1 data words, 1 pointers)."""


def _write_extensible_config(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["schemaVersion"])
    s.set_text(0, v["mediaType"])
    s.set_data(1, v["payload"])


def _read_extensible_config(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "schemaVersion": s.get_u32(0),
        "mediaType": s.get_text(0),
        "payload": s.get_data(1),
    }


_extensible_config_codec = _Codec(1, 2, _write_extensible_config, _read_extensible_config)
"""Wire codec for ``ExtensibleConfig`` (1 data words, 2 pointers)."""


def _write_bindings(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _extensible_config_codec.write(s.init_struct(0, 1, 2), v["config"], caps)
    _extensible_config_codec.write(s.init_struct(1, 1, 2), v["secrets"], caps)
    _database_adapter_config_codec.write(s.init_struct(2, 1, 4), v["adapter"], caps)
    s.set_cap(3, caps.export_cap(v["events"]))
    items = s.init_struct_list(4, len(v["databases"]), 0, 2)
    for item, elem in zip(items, v["databases"], strict=True):
        _named_database_codec.write(item, elem, caps)
    s.set_cap(5, caps.export_cap(v["cancel"]))
    s.set_cap(6, caps.export_cap(v["storage"]))


def _read_bindings(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "config": _extensible_config_codec.read(s.get_struct(0, 1, 2), caps),
        "secrets": _extensible_config_codec.read(s.get_struct(1, 1, 2), caps),
        "adapter": _database_adapter_config_codec.read(s.get_struct(2, 1, 4), caps),
        "events": caps.import_cap(s.get_cap_index(3)),
        "databases": [_named_database_codec.read(item, caps) for item in s.get_struct_list(4, 0, 2)],
        "cancel": caps.import_cap(s.get_cap_index(5)),
        "storage": caps.import_cap(s.get_cap_index(6)),
    }


_bindings_codec = _Codec(0, 7, _write_bindings, _read_bindings)
"""Wire codec for ``Bindings`` (0 data words, 7 pointers)."""


def _write_entrypoints(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_cap(0, caps.export_cap(v["eventConsumer"]))
    s.set_cap(1, caps.export_cap(v["jobRunner"]))
    s.set_cap(2, caps.export_cap(v["storefront"]))
    s.set_cap(3, caps.export_cap(v["storage"]))
    s.set_cap(4, caps.export_cap(v["databaseAdapter"]))
    s.set_cap(5, caps.export_cap(v["remoteLibrary"]))
    s.set_cap(6, caps.export_cap(v["cli"]))
    s.set_cap(7, caps.export_cap(v["oidc"]))


def _read_entrypoints(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "eventConsumer": caps.import_cap(s.get_cap_index(0)),
        "jobRunner": caps.import_cap(s.get_cap_index(1)),
        "storefront": caps.import_cap(s.get_cap_index(2)),
        "storage": caps.import_cap(s.get_cap_index(3)),
        "databaseAdapter": caps.import_cap(s.get_cap_index(4)),
        "remoteLibrary": caps.import_cap(s.get_cap_index(5)),
        "cli": caps.import_cap(s.get_cap_index(6)),
        "oidc": caps.import_cap(s.get_cap_index(7)),
    }


_entrypoints_codec = _Codec(0, 8, _write_entrypoints, _read_entrypoints)
"""Wire codec for ``Entrypoints`` (0 data words, 8 pointers)."""


def _write_entrypoints_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _entrypoints_codec.write(s.init_struct(0, 0, 8), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown EntrypointsReply union member: {kind}")


def _read_entrypoints_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _entrypoints_codec.read(s.get_struct(0, 0, 8), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown EntrypointsReply union member: {disc}")


_entrypoints_reply_codec = _Codec(1, 1, _write_entrypoints_reply, _read_entrypoints_reply)
"""Wire codec for ``EntrypointsReply`` (1 data words, 1 pointers)."""


def _write_job_invocation(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["payloadSchemaVersion"])
    s.set_text(0, v["invocationId"])
    s.set_text(1, v["commandType"])
    s.set_text(2, v["payloadJson"])
    s.set_text(3, v["idempotencyKey"])
    s.set_u32(1, v["attempt"])
    s.set_text(4, v["correlationId"])
    s.set_text(5, v["causationId"])
    s.set_u64(1, v["deadlineUnixMs"])
    s.set_text(6, v["checkpointJson"])
    s.set_u32(4, v["checkpointSchemaVersion"])
    s.set_u32(5, v["invocationSequence"])
    s.set_text(7, v["stepId"])


def _read_job_invocation(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "payloadSchemaVersion": s.get_u32(0),
        "invocationId": s.get_text(0),
        "commandType": s.get_text(1),
        "payloadJson": s.get_text(2),
        "idempotencyKey": s.get_text(3),
        "attempt": s.get_u32(1),
        "correlationId": s.get_text(4),
        "causationId": s.get_text(5),
        "deadlineUnixMs": s.get_u64(1),
        "checkpointJson": s.get_text(6),
        "checkpointSchemaVersion": s.get_u32(4),
        "invocationSequence": s.get_u32(5),
        "stepId": s.get_text(7),
    }


_job_invocation_codec = _Codec(3, 8, _write_job_invocation, _read_job_invocation)
"""Wire codec for ``JobInvocation`` (3 data words, 8 pointers)."""


def _write_completed_outcome(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["message"])
    s.set_u64(0, v["bytesCopied"])


def _read_completed_outcome(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "message": s.get_text(0),
        "bytesCopied": s.get_u64(0),
    }


_completed_outcome_codec = _Codec(1, 1, _write_completed_outcome, _read_completed_outcome)
"""Wire codec for ``CompletedOutcome`` (1 data words, 1 pointers)."""


def _write_retryable_outcome(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["message"])
    s.set_u64(0, v["retryAfterUnixMs"])


def _read_retryable_outcome(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "message": s.get_text(0),
        "retryAfterUnixMs": s.get_u64(0),
    }


_retryable_outcome_codec = _Codec(1, 1, _write_retryable_outcome, _read_retryable_outcome)
"""Wire codec for ``RetryableOutcome`` (1 data words, 1 pointers)."""


def _write_rejected_outcome(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["message"])


def _read_rejected_outcome(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "message": s.get_text(0),
    }


_rejected_outcome_codec = _Codec(0, 1, _write_rejected_outcome, _read_rejected_outcome)
"""Wire codec for ``RejectedOutcome`` (0 data words, 1 pointers)."""


def _write_cancelled_outcome(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["message"])


def _read_cancelled_outcome(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "message": s.get_text(0),
    }


_cancelled_outcome_codec = _Codec(0, 1, _write_cancelled_outcome, _read_cancelled_outcome)
"""Wire codec for ``CancelledOutcome`` (0 data words, 1 pointers)."""


def _write_suspended_outcome(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["checkpointJson"])
    s.set_u32(0, v["checkpointSchemaVersion"])
    s.set_u64(1, v["wakeAtUnixMs"])


def _read_suspended_outcome(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "checkpointJson": s.get_text(0),
        "checkpointSchemaVersion": s.get_u32(0),
        "wakeAtUnixMs": s.get_u64(1),
    }


_suspended_outcome_codec = _Codec(2, 1, _write_suspended_outcome, _read_suspended_outcome)
"""Wire codec for ``SuspendedOutcome`` (2 data words, 1 pointers)."""


def _write_job_outcome(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "completed":
        s.set_u16(0, 0)
        _completed_outcome_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "retryable":
        s.set_u16(0, 1)
        _retryable_outcome_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "rejected":
        s.set_u16(0, 2)
        _rejected_outcome_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "cancelled":
        s.set_u16(0, 3)
        _cancelled_outcome_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "suspended":
        s.set_u16(0, 4)
        _suspended_outcome_codec.write(s.init_struct(0, 2, 1), v["value"], caps)
    else:
        raise ValueError(f"unknown JobOutcome union member: {kind}")


def _read_job_outcome(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "completed", "value": _completed_outcome_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "retryable", "value": _retryable_outcome_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 2:
        return {"kind": "rejected", "value": _rejected_outcome_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 3:
        return {"kind": "cancelled", "value": _cancelled_outcome_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 4:
        return {"kind": "suspended", "value": _suspended_outcome_codec.read(s.get_struct(0, 2, 1), caps)}
    raise ValueError(f"unknown JobOutcome union member: {disc}")


_job_outcome_codec = _Codec(1, 1, _write_job_outcome, _read_job_outcome)
"""Wire codec for ``JobOutcome`` (1 data words, 1 pointers)."""


def _write_domain_event(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["eventId"])
    s.set_text(1, v["eventType"])
    s.set_u32(0, v["schemaVersion"])
    s.set_u64(1, v["occurredAtUnixMs"])
    s.set_text(2, v["accountId"])
    s.set_text(3, v["correlationId"])
    s.set_text(4, v["causationId"])
    s.set_text(5, v["deduplicationKey"])
    s.set_u32(1, v["deliveryAttempt"])
    s.set_data(6, v["payload"])
    s.set_text(7, v["checkpointJson"])
    s.set_u32(4, v["checkpointSchemaVersion"])
    s.set_u32(5, v["invocationSequence"])
    s.set_bool(192, v["resumePending"])
    s.set_text(8, v["source"])


def _read_domain_event(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "eventId": s.get_text(0),
        "eventType": s.get_text(1),
        "schemaVersion": s.get_u32(0),
        "occurredAtUnixMs": s.get_u64(1),
        "accountId": s.get_text(2),
        "correlationId": s.get_text(3),
        "causationId": s.get_text(4),
        "deduplicationKey": s.get_text(5),
        "deliveryAttempt": s.get_u32(1),
        "payload": s.get_data(6),
        "checkpointJson": s.get_text(7),
        "checkpointSchemaVersion": s.get_u32(4),
        "invocationSequence": s.get_u32(5),
        "resumePending": s.get_bool(192),
        "source": s.get_text(8),
    }


_domain_event_codec = _Codec(4, 9, _write_domain_event, _read_domain_event)
"""Wire codec for ``DomainEvent`` (4 data words, 9 pointers)."""


def _write_event_ack(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_event_ack(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "dummy": None,
    }


_event_ack_codec = _Codec(0, 0, _write_event_ack, _read_event_ack)
"""Wire codec for ``EventAck`` (0 data words, 0 pointers)."""


def _write_event_retry(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u64(0, v["retryAtUnixMs"])
    s.set_text(0, v["reason"])


def _read_event_retry(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "retryAtUnixMs": s.get_u64(0),
        "reason": s.get_text(0),
    }


_event_retry_codec = _Codec(1, 1, _write_event_retry, _read_event_retry)
"""Wire codec for ``EventRetry`` (1 data words, 1 pointers)."""


def _write_event_reject(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["reason"])


def _read_event_reject(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "reason": s.get_text(0),
    }


_event_reject_codec = _Codec(0, 1, _write_event_reject, _read_event_reject)
"""Wire codec for ``EventReject`` (0 data words, 1 pointers)."""


def _write_event_dead_letter(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["reason"])


def _read_event_dead_letter(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "reason": s.get_text(0),
    }


_event_dead_letter_codec = _Codec(0, 1, _write_event_dead_letter, _read_event_dead_letter)
"""Wire codec for ``EventDeadLetter`` (0 data words, 1 pointers)."""


def _write_event_suspended(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["checkpointJson"])
    s.set_u32(0, v["checkpointSchemaVersion"])
    s.set_u64(1, v["wakeAtUnixMs"])
    s.set_text(1, v["wakeOnEventType"])
    s.set_text(2, v["wakeOnFilterJson"])


def _read_event_suspended(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "checkpointJson": s.get_text(0),
        "checkpointSchemaVersion": s.get_u32(0),
        "wakeAtUnixMs": s.get_u64(1),
        "wakeOnEventType": s.get_text(1),
        "wakeOnFilterJson": s.get_text(2),
    }


_event_suspended_codec = _Codec(2, 3, _write_event_suspended, _read_event_suspended)
"""Wire codec for ``EventSuspended`` (2 data words, 3 pointers)."""


def _write_event_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ack":
        s.set_u16(0, 0)
        _event_ack_codec.write(s.init_struct(0, 0, 0), v["value"], caps)
    elif kind == "retry":
        s.set_u16(0, 1)
        _event_retry_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "reject":
        s.set_u16(0, 2)
        _event_reject_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "deadLetter":
        s.set_u16(0, 3)
        _event_dead_letter_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "suspended":
        s.set_u16(0, 4)
        _event_suspended_codec.write(s.init_struct(0, 2, 3), v["value"], caps)
    else:
        raise ValueError(f"unknown EventResult union member: {kind}")


def _read_event_result(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ack", "value": _event_ack_codec.read(s.get_struct(0, 0, 0), caps)}
    elif disc == 1:
        return {"kind": "retry", "value": _event_retry_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 2:
        return {"kind": "reject", "value": _event_reject_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 3:
        return {"kind": "deadLetter", "value": _event_dead_letter_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 4:
        return {"kind": "suspended", "value": _event_suspended_codec.read(s.get_struct(0, 2, 3), caps)}
    raise ValueError(f"unknown EventResult union member: {disc}")


_event_result_codec = _Codec(1, 1, _write_event_result, _read_event_result)
"""Wire codec for ``EventResult`` (1 data words, 1 pointers)."""


def _write_event_batch(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["events"]), 4, 9)
    for item, elem in zip(items, v["events"], strict=True):
        _domain_event_codec.write(item, elem, caps)


def _read_event_batch(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "events": [_domain_event_codec.read(item, caps) for item in s.get_struct_list(0, 4, 9)],
    }


_event_batch_codec = _Codec(0, 1, _write_event_batch, _read_event_batch)
"""Wire codec for ``EventBatch`` (0 data words, 1 pointers)."""


def _write_event_batch_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        items = s.init_struct_list(0, len(v["value"]), 1, 1)
        for item, elem in zip(items, v["value"], strict=True):
            _event_result_codec.write(item, elem, caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown EventBatchReply union member: {kind}")


def _read_event_batch_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": [_event_result_codec.read(item, caps) for item in s.get_struct_list(0, 1, 1)]}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown EventBatchReply union member: {disc}")


_event_batch_reply_codec = _Codec(1, 1, _write_event_batch_reply, _read_event_batch_reply)
"""Wire codec for ``EventBatchReply`` (1 data words, 1 pointers)."""


def _write_job_controller(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _job_invocation_codec.write(s.init_struct(0, 3, 8), v["invocation"], caps)
    s.set_cap(1, caps.export_cap(v["input"]))
    s.set_cap(2, caps.export_cap(v["output"]))
    s.set_cap(3, caps.export_cap(v["progress"]))
    s.set_cap(4, caps.export_cap(v["cancel"]))


def _read_job_controller(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "invocation": _job_invocation_codec.read(s.get_struct(0, 3, 8), caps),
        "input": caps.import_cap(s.get_cap_index(1)),
        "output": caps.import_cap(s.get_cap_index(2)),
        "progress": caps.import_cap(s.get_cap_index(3)),
        "cancel": caps.import_cap(s.get_cap_index(4)),
    }


_job_controller_codec = _Codec(0, 5, _write_job_controller, _read_job_controller)
"""Wire codec for ``JobController`` (0 data words, 5 pointers)."""


def _write_head_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_bool(0, v["found"])
    _object_metadata_codec.write(s.init_struct(0, 1, 4), v["meta"], caps)


def _read_head_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "found": s.get_bool(0),
        "meta": _object_metadata_codec.read(s.get_struct(0, 1, 4), caps),
    }


_head_ok_codec = _Codec(1, 1, _write_head_ok, _read_head_ok)
"""Wire codec for ``HeadOk`` (1 data words, 1 pointers)."""


def _write_head_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _head_ok_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown HeadReply union member: {kind}")


def _read_head_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _head_ok_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown HeadReply union member: {disc}")


_head_reply_codec = _Codec(1, 1, _write_head_reply, _read_head_reply)
"""Wire codec for ``HeadReply`` (1 data words, 1 pointers)."""


def _write_list_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _list_page_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown ListReply union member: {kind}")


def _read_list_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _list_page_codec.read(s.get_struct(0, 0, 2), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown ListReply union member: {disc}")


_list_reply_codec = _Codec(1, 1, _write_list_reply, _read_list_reply)
"""Wire codec for ``ListReply`` (1 data words, 1 pointers)."""


def _write_get_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _object_metadata_codec.write(s.init_struct(0, 1, 4), v["meta"], caps)
    s.set_cap(1, caps.export_cap(v["body"]))


def _read_get_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "meta": _object_metadata_codec.read(s.get_struct(0, 1, 4), caps),
        "body": caps.import_cap(s.get_cap_index(1)),
    }


_get_ok_codec = _Codec(0, 2, _write_get_ok, _read_get_ok)
"""Wire codec for ``GetOk`` (0 data words, 2 pointers)."""


def _write_get_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _get_ok_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown GetReply union member: {kind}")


def _read_get_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _get_ok_codec.read(s.get_struct(0, 0, 2), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown GetReply union member: {disc}")


_get_reply_codec = _Codec(1, 1, _write_get_reply, _read_get_reply)
"""Wire codec for ``GetReply`` (1 data words, 1 pointers)."""


def _write_put_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _put_result_codec.write(s.init_struct(0, 1, 3), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown PutReply union member: {kind}")


def _read_put_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _put_result_codec.read(s.get_struct(0, 1, 3), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown PutReply union member: {disc}")


_put_reply_codec = _Codec(1, 1, _write_put_reply, _read_put_reply)
"""Wire codec for ``PutReply`` (1 data words, 1 pointers)."""


def _write_copy_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _copy_result_codec.write(s.init_struct(0, 1, 0), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown CopyReply union member: {kind}")


def _read_copy_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _copy_result_codec.read(s.get_struct(0, 1, 0), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown CopyReply union member: {disc}")


_copy_reply_codec = _Codec(1, 1, _write_copy_reply, _read_copy_reply)
"""Wire codec for ``CopyReply`` (1 data words, 1 pointers)."""


def _write_empty_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown EmptyReply union member: {kind}")


def _read_empty_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok"}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown EmptyReply union member: {disc}")


_empty_reply_codec = _Codec(1, 1, _write_empty_reply, _read_empty_reply)
"""Wire codec for ``EmptyReply`` (1 data words, 1 pointers)."""


def _write_pull_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_data(0, v["chunk"])
    s.set_bool(0, v["done"])


def _read_pull_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "chunk": s.get_data(0),
        "done": s.get_bool(0),
    }


_pull_ok_codec = _Codec(1, 1, _write_pull_ok, _read_pull_ok)
"""Wire codec for ``PullOk`` (1 data words, 1 pointers)."""


def _write_pull_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _pull_ok_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown PullReply union member: {kind}")


def _read_pull_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _pull_ok_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown PullReply union member: {disc}")


_pull_reply_codec = _Codec(1, 1, _write_pull_reply, _read_pull_reply)
"""Wire codec for ``PullReply`` (1 data words, 1 pointers)."""


def _write_open_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _object_metadata_codec.write(s.init_struct(0, 1, 4), v["meta"], caps)
    s.set_cap(1, caps.export_cap(v["body"]))


def _read_open_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "meta": _object_metadata_codec.read(s.get_struct(0, 1, 4), caps),
        "body": caps.import_cap(s.get_cap_index(1)),
    }


_open_ok_codec = _Codec(0, 2, _write_open_ok, _read_open_ok)
"""Wire codec for ``OpenOk`` (0 data words, 2 pointers)."""


def _write_open_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _open_ok_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown OpenReply union member: {kind}")


def _read_open_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _open_ok_codec.read(s.get_struct(0, 0, 2), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown OpenReply union member: {disc}")


_open_reply_codec = _Codec(1, 1, _write_open_reply, _read_open_reply)
"""Wire codec for ``OpenReply`` (1 data words, 1 pointers)."""


def _write_describe_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _plugin_describe_codec.write(s.init_struct(0, 2, 10), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown DescribeReply union member: {kind}")


def _read_describe_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _plugin_describe_codec.read(s.get_struct(0, 2, 10), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown DescribeReply union member: {disc}")


_describe_reply_codec = _Codec(1, 1, _write_describe_reply, _read_describe_reply)
"""Wire codec for ``DescribeReply`` (1 data words, 1 pointers)."""


def _write_handle_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _job_outcome_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown HandleReply union member: {kind}")


def _read_handle_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _job_outcome_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown HandleReply union member: {disc}")


_handle_reply_codec = _Codec(1, 1, _write_handle_reply, _read_handle_reply)
"""Wire codec for ``HandleReply`` (1 data words, 1 pointers)."""


def _write_health_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_bool(0, v["ok"])
    s.set_text(0, v["detail"])


def _read_health_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "ok": s.get_bool(0),
        "detail": s.get_text(0),
    }


_health_ok_codec = _Codec(1, 1, _write_health_ok, _read_health_ok)
"""Wire codec for ``HealthOk`` (1 data words, 1 pointers)."""


def _write_health_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _health_ok_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown HealthReply union member: {kind}")


def _read_health_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _health_ok_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown HealthReply union member: {disc}")


_health_reply_codec = _Codec(1, 1, _write_health_reply, _read_health_reply)
"""Wire codec for ``HealthReply`` (1 data words, 1 pointers)."""


def _write_adapter_session_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        s.set_cap(0, caps.export_cap(v["value"]))
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown AdapterSessionReply union member: {kind}")


def _read_adapter_session_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": caps.import_cap(s.get_cap_index(0))}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown AdapterSessionReply union member: {disc}")


_adapter_session_reply_codec = _Codec(1, 1, _write_adapter_session_reply, _read_adapter_session_reply)
"""Wire codec for ``AdapterSessionReply`` (1 data words, 1 pointers)."""


def _write_guest_database_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        s.set_cap(0, caps.export_cap(v["value"]))
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown GuestDatabaseReply union member: {kind}")


def _read_guest_database_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": caps.import_cap(s.get_cap_index(0))}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown GuestDatabaseReply union member: {disc}")


_guest_database_reply_codec = _Codec(1, 1, _write_guest_database_reply, _read_guest_database_reply)
"""Wire codec for ``GuestDatabaseReply`` (1 data words, 1 pointers)."""


def _write_named_database(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["name"])
    s.set_cap(1, caps.export_cap(v["database"]))


def _read_named_database(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "name": s.get_text(0),
        "database": caps.import_cap(s.get_cap_index(1)),
    }


_named_database_codec = _Codec(0, 2, _write_named_database, _read_named_database)
"""Wire codec for ``NamedDatabase`` (0 data words, 2 pointers)."""


def _write_event_consumer_spec(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["eventType"])
    s.set_u32_list(1, v["schemaVersions"])
    s.set_bool(0, v["supportsSuspend"])


def _read_event_consumer_spec(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "eventType": s.get_text(0),
        "schemaVersions": s.get_u32_list(1),
        "supportsSuspend": s.get_bool(0),
    }


_event_consumer_spec_codec = _Codec(1, 2, _write_event_consumer_spec, _read_event_consumer_spec)
"""Wire codec for ``EventConsumerSpec`` (1 data words, 2 pointers)."""


def _write_plugin_capabilities(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u16_list(0, [_ord(A.ENTRYPOINTS, v, "Entrypoint") for v in v["entrypoints"]])
    items = s.init_struct_list(1, len(v["consumes"]), 1, 2)
    for item, elem in zip(items, v["consumes"], strict=True):
        _event_consumer_spec_codec.write(item, elem, caps)
    s.set_text_list(2, v["produces"])
    s.set_text_list(3, v["jobs"])
    s.set_text_list(4, v["databases"])
    s.set_text_list(5, v["bindings"])


def _read_plugin_capabilities(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "entrypoints": [_from_ord(A.ENTRYPOINTS, v, "Entrypoint") for v in s.get_u16_list(0)],
        "consumes": [_event_consumer_spec_codec.read(item, caps) for item in s.get_struct_list(1, 1, 2)],
        "produces": s.get_text_list(2),
        "jobs": s.get_text_list(3),
        "databases": s.get_text_list(4),
        "bindings": s.get_text_list(5),
    }


_plugin_capabilities_codec = _Codec(0, 6, _write_plugin_capabilities, _read_plugin_capabilities)
"""Wire codec for ``PluginCapabilities`` (0 data words, 6 pointers)."""


def _write_brand(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["id"])
    s.set_text(1, v["name"])
    s.set_text(2, v["bg"])
    s.set_text(3, v["fg"])
    s.set_text(4, v["accent"])
    if v.get("iconUrl") is not None:
        s.set_text(5, v["iconUrl"])


def _read_brand(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "id": s.get_text(0),
        "name": s.get_text(1),
        "bg": s.get_text(2),
        "fg": s.get_text(3),
        "accent": s.get_text(4),
    }
    icon_url = s.get_text(5)
    if not (icon_url == ""):
        out["iconUrl"] = icon_url
    return out


_brand_codec = _Codec(0, 6, _write_brand, _read_brand)
"""Wire codec for ``Brand`` (0 data words, 6 pointers)."""


def _write_config_option(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    s.set_text(1, v["label"])
    items = s.init_struct_list(2, len(v["values"]), 0, 2)
    for item, elem in zip(items, v["values"], strict=True):
        _config_option_value_codec.write(item, elem, caps)


def _read_config_option(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "label": s.get_text(1),
        "values": [_config_option_value_codec.read(item, caps) for item in s.get_struct_list(2, 0, 2)],
    }


_config_option_codec = _Codec(0, 3, _write_config_option, _read_config_option)
"""Wire codec for ``ConfigOption`` (0 data words, 3 pointers)."""


def _write_config_option_value(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["id"])
    s.set_text(1, v["label"])


def _read_config_option_value(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "id": s.get_text(0),
        "label": s.get_text(1),
    }


_config_option_value_codec = _Codec(0, 2, _write_config_option_value, _read_config_option_value)
"""Wire codec for ``ConfigOptionValue`` (0 data words, 2 pointers)."""


def _write_cli_schema(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["commands"]), 0, 3)
    for item, elem in zip(items, v["commands"], strict=True):
        _cli_command_spec_codec.write(item, elem, caps)


def _read_cli_schema(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "commands": [_cli_command_spec_codec.read(item, caps) for item in s.get_struct_list(0, 0, 3)],
    }


_cli_schema_codec = _Codec(0, 1, _write_cli_schema, _read_cli_schema)
"""Wire codec for ``CliSchema`` (0 data words, 1 pointers)."""


def _write_cli_command_spec(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["name"])
    if v.get("about") is not None:
        s.set_text(1, v["about"])
    items = s.init_struct_list(2, len(v["args"]), 1, 5)
    for item, elem in zip(items, v["args"], strict=True):
        _cli_arg_spec_codec.write(item, elem, caps)


def _read_cli_command_spec(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "name": s.get_text(0),
        "args": [_cli_arg_spec_codec.read(item, caps) for item in s.get_struct_list(2, 1, 5)],
    }
    about = s.get_text(1)
    if not (about == ""):
        out["about"] = about
    return out


_cli_command_spec_codec = _Codec(0, 3, _write_cli_command_spec, _read_cli_command_spec)
"""Wire codec for ``CliCommandSpec`` (0 data words, 3 pointers)."""


def _write_cli_arg_spec(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["name"])
    if v.get("long") is not None:
        s.set_text(1, v["long"])
    if v.get("short") is not None:
        s.set_text(2, v["short"])
    s.set_u16(0, _ord(A.CLI_ARG_KINDS, v["kind"], "CliArgKind"))
    s.set_bool(16, v["required"])
    if v.get("default") is not None:
        s.set_text(3, v["default"])
    if v.get("about") is not None:
        s.set_text(4, v["about"])
    s.set_bool(17, v["positional"])


def _read_cli_arg_spec(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "name": s.get_text(0),
        "kind": _from_ord(A.CLI_ARG_KINDS, s.get_u16(0), "CliArgKind"),
        "required": s.get_bool(16),
        "positional": s.get_bool(17),
    }
    long = s.get_text(1)
    if not (long == ""):
        out["long"] = long
    short = s.get_text(2)
    if not (short == ""):
        out["short"] = short
    default = s.get_text(3)
    if not (default == ""):
        out["default"] = default
    about = s.get_text(4)
    if not (about == ""):
        out["about"] = about
    return out


_cli_arg_spec_codec = _Codec(1, 5, _write_cli_arg_spec, _read_cli_arg_spec)
"""Wire codec for ``CliArgSpec`` (1 data words, 5 pointers)."""


def _write_cli_arg(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["name"])
    s.set_text(1, v["value"])


def _read_cli_arg(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "name": s.get_text(0),
        "value": s.get_text(1),
    }


_cli_arg_codec = _Codec(0, 2, _write_cli_arg, _read_cli_arg)
"""Wire codec for ``CliArg`` (0 data words, 2 pointers)."""


def _write_cli_invoke_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["command"])
    items = s.init_struct_list(1, len(v["args"]), 0, 2)
    for item, elem in zip(items, v["args"], strict=True):
        _cli_arg_codec.write(item, elem, caps)


def _read_cli_invoke_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "command": s.get_text(0),
        "args": [_cli_arg_codec.read(item, caps) for item in s.get_struct_list(1, 0, 2)],
    }


_cli_invoke_params_codec = _Codec(0, 2, _write_cli_invoke_params, _read_cli_invoke_params)
"""Wire codec for ``CliInvokeParams`` (0 data words, 2 pointers)."""


def _write_cli_invoke_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_i32(0, v["exitCode"])
    s.set_text(0, v["stdout"])
    s.set_text(1, v["stderr"])
    _extensible_config_codec.write(s.init_struct(2, 1, 2), v["payload"], caps)


def _read_cli_invoke_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "exitCode": s.get_i32(0),
        "stdout": s.get_text(0),
        "stderr": s.get_text(1),
        "payload": _extensible_config_codec.read(s.get_struct(2, 1, 2), caps),
    }


_cli_invoke_result_codec = _Codec(1, 3, _write_cli_invoke_result, _read_cli_invoke_result)
"""Wire codec for ``CliInvokeResult`` (1 data words, 3 pointers)."""


def _write_cli_schema_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _cli_schema_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown CliSchemaReply union member: {kind}")


def _read_cli_schema_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _cli_schema_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown CliSchemaReply union member: {disc}")


_cli_schema_reply_codec = _Codec(1, 1, _write_cli_schema_reply, _read_cli_schema_reply)
"""Wire codec for ``CliSchemaReply`` (1 data words, 1 pointers)."""


def _write_cli_invoke_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _cli_invoke_result_codec.write(s.init_struct(0, 1, 3), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown CliInvokeReply union member: {kind}")


def _read_cli_invoke_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _cli_invoke_result_codec.read(s.get_struct(0, 1, 3), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown CliInvokeReply union member: {disc}")


_cli_invoke_reply_codec = _Codec(1, 1, _write_cli_invoke_reply, _read_cli_invoke_reply)
"""Wire codec for ``CliInvokeReply`` (1 data words, 1 pointers)."""


def _write_database_adapter_config(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["pluginDataDir"])
    _extensible_config_codec.write(s.init_struct(1, 1, 2), v["settings"], caps)
    if v.get("binding") is not None:
        s.set_text(2, v["binding"])
    if v.get("instanceId") is not None:
        s.set_text(3, v["instanceId"])
    s.set_bool(0, v["openExisting"])


def _read_database_adapter_config(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "pluginDataDir": s.get_text(0),
        "settings": _extensible_config_codec.read(s.get_struct(1, 1, 2), caps),
        "openExisting": s.get_bool(0),
    }
    binding = s.get_text(2)
    if not (binding == ""):
        out["binding"] = binding
    instance_id = s.get_text(3)
    if not (instance_id == ""):
        out["instanceId"] = instance_id
    return out


_database_adapter_config_codec = _Codec(1, 4, _write_database_adapter_config, _read_database_adapter_config)
"""Wire codec for ``DatabaseAdapterConfig`` (1 data words, 4 pointers)."""


def _write_diagnose_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text_list(0, v["lines"])


def _read_diagnose_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "lines": s.get_text_list(0),
    }


_diagnose_result_codec = _Codec(0, 1, _write_diagnose_result, _read_diagnose_result)
"""Wire codec for ``DiagnoseResult`` (0 data words, 1 pointers)."""


def _write_diagnose_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _diagnose_result_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown DiagnoseReply union member: {kind}")


def _read_diagnose_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _diagnose_result_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown DiagnoseReply union member: {disc}")


_diagnose_reply_codec = _Codec(1, 1, _write_diagnose_reply, _read_diagnose_reply)
"""Wire codec for ``DiagnoseReply`` (1 data words, 1 pointers)."""


def _write_source_account(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["accountId"])
    s.set_text(1, v["source"])
    s.set_text(2, v["marketplace"])
    if v.get("label") is not None:
        s.set_text(3, v["label"])
    s.set_bool(0, v["scanEnabled"])


def _read_source_account(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "accountId": s.get_text(0),
        "source": s.get_text(1),
        "marketplace": s.get_text(2),
        "scanEnabled": s.get_bool(0),
    }
    label = s.get_text(3)
    if not (label == ""):
        out["label"] = label
    return out


_source_account_codec = _Codec(1, 4, _write_source_account, _read_source_account)
"""Wire codec for ``SourceAccount`` (1 data words, 4 pointers)."""


def _write_login_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["pluginDataDir"])
    s.set_text(1, v["marketplace"])
    if v.get("label") is not None:
        s.set_text(2, v["label"])
    if v.get("email") is not None:
        s.set_text(3, v["email"])
    if v.get("password") is not None:
        s.set_text(4, v["password"])
    s.set_bool(0, v["force"])
    if v.get("callbackBind") is not None:
        s.set_text(5, v["callbackBind"])
    if v.get("callbackIpc") is not None:
        s.set_text(6, v["callbackIpc"])
    if v.get("callbackPublicBase") is not None:
        s.set_text(7, v["callbackPublicBase"])
    s.set_bool(1, v["external"])
    if v.get("responseUrl") is not None:
        s.set_text(8, v["responseUrl"])
    s.set_bool(2, v["showQr"])
    if v.get("timeoutSecs") is not None:
        s.set_u64(1, v["timeoutSecs"])
    _extensible_config_codec.write(s.init_struct(9, 1, 2), v["extra"], caps)


def _read_login_params(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "pluginDataDir": s.get_text(0),
        "marketplace": s.get_text(1),
        "force": s.get_bool(0),
        "external": s.get_bool(1),
        "showQr": s.get_bool(2),
        "extra": _extensible_config_codec.read(s.get_struct(9, 1, 2), caps),
    }
    label = s.get_text(2)
    if not (label == ""):
        out["label"] = label
    email = s.get_text(3)
    if not (email == ""):
        out["email"] = email
    password = s.get_text(4)
    if not (password == ""):
        out["password"] = password
    callback_bind = s.get_text(5)
    if not (callback_bind == ""):
        out["callbackBind"] = callback_bind
    callback_ipc = s.get_text(6)
    if not (callback_ipc == ""):
        out["callbackIpc"] = callback_ipc
    callback_public_base = s.get_text(7)
    if not (callback_public_base == ""):
        out["callbackPublicBase"] = callback_public_base
    response_url = s.get_text(8)
    if not (response_url == ""):
        out["responseUrl"] = response_url
    timeout_secs = s.get_u64(1)
    if not (timeout_secs == 0):
        out["timeoutSecs"] = timeout_secs
    return out


_login_params_codec = _Codec(2, 10, _write_login_params, _read_login_params)
"""Wire codec for ``LoginParams`` (2 data words, 10 pointers)."""


def _write_login_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _source_account_codec.write(s.init_struct(0, 1, 4), v["account"], caps)
    if v.get("credentials") is not None:
        s.set_data(1, v["credentials"])


def _read_login_result(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "account": _source_account_codec.read(s.get_struct(0, 1, 4), caps),
    }
    credentials = s.get_data(1)
    if not (len(credentials) == 0):
        out["credentials"] = credentials
    return out


_login_result_codec = _Codec(0, 2, _write_login_result, _read_login_result)
"""Wire codec for ``LoginResult`` (0 data words, 2 pointers)."""


def _write_login_start_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["sessionId"])
    s.set_text(1, v["url"])


def _read_login_start_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "sessionId": s.get_text(0),
        "url": s.get_text(1),
    }


_login_start_result_codec = _Codec(0, 2, _write_login_start_result, _read_login_start_result)
"""Wire codec for ``LoginStartResult`` (0 data words, 2 pointers)."""


def _write_login_complete_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["sessionId"])


def _read_login_complete_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "sessionId": s.get_text(0),
    }


_login_complete_params_codec = _Codec(0, 1, _write_login_complete_params, _read_login_complete_params)
"""Wire codec for ``LoginCompleteParams`` (0 data words, 1 pointers)."""


def _write_login_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _login_result_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown LoginReply union member: {kind}")


def _read_login_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _login_result_codec.read(s.get_struct(0, 0, 2), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown LoginReply union member: {disc}")


_login_reply_codec = _Codec(1, 1, _write_login_reply, _read_login_reply)
"""Wire codec for ``LoginReply`` (1 data words, 1 pointers)."""


def _write_login_start_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _login_start_result_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown LoginStartReply union member: {kind}")


def _read_login_start_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _login_start_result_codec.read(s.get_struct(0, 0, 2), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown LoginStartReply union member: {disc}")


_login_start_reply_codec = _Codec(1, 1, _write_login_start_reply, _read_login_start_reply)
"""Wire codec for ``LoginStartReply`` (1 data words, 1 pointers)."""


def _write_account_credential(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["accountId"])
    s.set_data(1, v["credentials"])


def _read_account_credential(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "accountId": s.get_text(0),
        "credentials": s.get_data(1),
    }


_account_credential_codec = _Codec(0, 2, _write_account_credential, _read_account_credential)
"""Wire codec for ``AccountCredential`` (0 data words, 2 pointers)."""


def _write_scan_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["pluginDataDir"])
    s.set_text_list(1, v["accounts"])
    s.set_u32(0, v["pageSize"])
    s.set_bool(32, v["importEpisodes"])
    s.set_bool(33, v["importPlusTitles"])
    items = s.init_struct_list(2, len(v["credentials"]), 0, 2)
    for item, elem in zip(items, v["credentials"], strict=True):
        _account_credential_codec.write(item, elem, caps)


def _read_scan_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "pluginDataDir": s.get_text(0),
        "accounts": s.get_text_list(1),
        "pageSize": s.get_u32(0),
        "importEpisodes": s.get_bool(32),
        "importPlusTitles": s.get_bool(33),
        "credentials": [_account_credential_codec.read(item, caps) for item in s.get_struct_list(2, 0, 2)],
    }


_scan_params_codec = _Codec(1, 3, _write_scan_params, _read_scan_params)
"""Wire codec for ``ScanParams`` (1 data words, 3 pointers)."""


def _write_scan_book(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["accountId"])
    s.set_text(1, v["productId"])
    s.set_text(2, v["title"])
    if v.get("marketplace") is not None:
        s.set_text(3, v["marketplace"])
    if v.get("asin") is not None:
        s.set_text(4, v["asin"])
    if v.get("isbn") is not None:
        s.set_text(5, v["isbn"])
    if v.get("authors") is not None:
        s.set_text(6, v["authors"])
    if v.get("narrators") is not None:
        s.set_text(7, v["narrators"])
    if v.get("series") is not None:
        s.set_text(8, v["series"])
    if v.get("seriesIndex") is not None:
        s.set_text(9, v["seriesIndex"])
    if v.get("contentKind") is not None:
        s.set_text(10, v["contentKind"])
    if v.get("publisher") is not None:
        s.set_text(11, v["publisher"])
    if v.get("lengthMinutes") is not None:
        s.set_i64(0, v["lengthMinutes"])
    if v.get("subtitle") is not None:
        s.set_text(12, v["subtitle"])


def _read_scan_book(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "accountId": s.get_text(0),
        "productId": s.get_text(1),
        "title": s.get_text(2),
    }
    marketplace = s.get_text(3)
    if not (marketplace == ""):
        out["marketplace"] = marketplace
    asin = s.get_text(4)
    if not (asin == ""):
        out["asin"] = asin
    isbn = s.get_text(5)
    if not (isbn == ""):
        out["isbn"] = isbn
    authors = s.get_text(6)
    if not (authors == ""):
        out["authors"] = authors
    narrators = s.get_text(7)
    if not (narrators == ""):
        out["narrators"] = narrators
    series = s.get_text(8)
    if not (series == ""):
        out["series"] = series
    series_index = s.get_text(9)
    if not (series_index == ""):
        out["seriesIndex"] = series_index
    content_kind = s.get_text(10)
    if not (content_kind == ""):
        out["contentKind"] = content_kind
    publisher = s.get_text(11)
    if not (publisher == ""):
        out["publisher"] = publisher
    length_minutes = s.get_i64(0)
    if not (length_minutes == 0):
        out["lengthMinutes"] = length_minutes
    subtitle = s.get_text(12)
    if not (subtitle == ""):
        out["subtitle"] = subtitle
    return out


_scan_book_codec = _Codec(1, 13, _write_scan_book, _read_scan_book)
"""Wire codec for ``ScanBook`` (1 data words, 13 pointers)."""


def _write_scan_summary(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["accounts"])
    s.set_u32(1, v["booksUpserted"])
    s.set_u32(2, v["pages"])
    s.set_u32(3, v["skippedDisabled"])
    items = s.init_struct_list(0, len(v["books"]), 1, 13)
    for item, elem in zip(items, v["books"], strict=True):
        _scan_book_codec.write(item, elem, caps)


def _read_scan_summary(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "accounts": s.get_u32(0),
        "booksUpserted": s.get_u32(1),
        "pages": s.get_u32(2),
        "skippedDisabled": s.get_u32(3),
        "books": [_scan_book_codec.read(item, caps) for item in s.get_struct_list(0, 1, 13)],
    }


_scan_summary_codec = _Codec(2, 1, _write_scan_summary, _read_scan_summary)
"""Wire codec for ``ScanSummary`` (2 data words, 1 pointers)."""


def _write_scan_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _scan_summary_codec.write(s.init_struct(0, 2, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown ScanReply union member: {kind}")


def _read_scan_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _scan_summary_codec.read(s.get_struct(0, 2, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown ScanReply union member: {disc}")


_scan_reply_codec = _Codec(1, 1, _write_scan_reply, _read_scan_reply)
"""Wire codec for ``ScanReply`` (1 data words, 1 pointers)."""


def _write_fetch_options(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_bool(0, v["widevine"])
    s.set_bool(1, v["xheAac"])
    if v.get("widevineCdmPath") is not None:
        s.set_text(0, v["widevineCdmPath"])
    if v.get("widevineCdmProvider") is not None:
        s.set_text(1, v["widevineCdmProvider"])
    s.set_bool(2, v["downloadCover"])
    s.set_bool(3, v["downloadPdf"])
    s.set_text(2, v["coverSize"])
    s.set_text(3, v["chapterLayout"])
    s.set_bool(4, v["stripAudibleBrandAudio"])
    s.set_bool(5, v["downloadClipsBookmarks"])
    s.set_bool(6, v["retainAaxFile"])
    s.set_u32(1, v["downloadSpeedLimitKbps"])
    s.set_bool(7, v["saveMetadataJson"])


def _read_fetch_options(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "widevine": s.get_bool(0),
        "xheAac": s.get_bool(1),
        "downloadCover": s.get_bool(2),
        "downloadPdf": s.get_bool(3),
        "coverSize": s.get_text(2),
        "chapterLayout": s.get_text(3),
        "stripAudibleBrandAudio": s.get_bool(4),
        "downloadClipsBookmarks": s.get_bool(5),
        "retainAaxFile": s.get_bool(6),
        "downloadSpeedLimitKbps": s.get_u32(1),
        "saveMetadataJson": s.get_bool(7),
    }
    widevine_cdm_path = s.get_text(0)
    if not (widevine_cdm_path == ""):
        out["widevineCdmPath"] = widevine_cdm_path
    widevine_cdm_provider = s.get_text(1)
    if not (widevine_cdm_provider == ""):
        out["widevineCdmProvider"] = widevine_cdm_provider
    return out


_fetch_options_codec = _Codec(1, 4, _write_fetch_options, _read_fetch_options)
"""Wire codec for ``FetchOptions`` (1 data words, 4 pointers)."""


def _write_fetch_title_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["pluginDataDir"])
    s.set_text(1, v["accountId"])
    s.set_text(2, v["titleId"])
    s.set_text(3, v["cacheDir"])
    if v.get("credentials") is not None:
        s.set_data(4, v["credentials"])
    _extensible_config_codec.write(s.init_struct(5, 1, 2), v["sourceConfig"], caps)
    _fetch_options_codec.write(s.init_struct(6, 1, 4), v["fetch"], caps)


def _read_fetch_title_params(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "pluginDataDir": s.get_text(0),
        "accountId": s.get_text(1),
        "titleId": s.get_text(2),
        "cacheDir": s.get_text(3),
        "sourceConfig": _extensible_config_codec.read(s.get_struct(5, 1, 2), caps),
        "fetch": _fetch_options_codec.read(s.get_struct(6, 1, 4), caps),
    }
    credentials = s.get_data(4)
    if not (len(credentials) == 0):
        out["credentials"] = credentials
    return out


_fetch_title_params_codec = _Codec(0, 7, _write_fetch_title_params, _read_fetch_title_params)
"""Wire codec for ``FetchTitleParams`` (0 data words, 7 pointers)."""


def _write_plain_part(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["path"])
    if v.get("title") is not None:
        s.set_text(1, v["title"])
    if v.get("durationMs") is not None:
        s.set_u64(0, v["durationMs"])


def _read_plain_part(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "path": s.get_text(0),
    }
    title = s.get_text(1)
    if not (title == ""):
        out["title"] = title
    duration_ms = s.get_u64(0)
    if not (duration_ms == 0):
        out["durationMs"] = duration_ms
    return out


_plain_part_codec = _Codec(1, 2, _write_plain_part, _read_plain_part)
"""Wire codec for ``PlainPart`` (1 data words, 2 pointers)."""


def _write_chapter_marker(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["title"])
    s.set_u64(0, v["startMs"])


def _read_chapter_marker(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "title": s.get_text(0),
        "startMs": s.get_u64(0),
    }


_chapter_marker_codec = _Codec(1, 1, _write_chapter_marker, _read_chapter_marker)
"""Wire codec for ``ChapterMarker`` (1 data words, 1 pointers)."""


def _write_plain_fetch(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["parts"]), 1, 2)
    for item, elem in zip(items, v["parts"], strict=True):
        _plain_part_codec.write(item, elem, caps)
    if v.get("m4bPath") is not None:
        s.set_text(1, v["m4bPath"])
    if v.get("coverPath") is not None:
        s.set_text(2, v["coverPath"])
    items = s.init_struct_list(3, len(v["chapters"]), 1, 1)
    for item, elem in zip(items, v["chapters"], strict=True):
        _chapter_marker_codec.write(item, elem, caps)
    if v.get("pdfUrl") is not None:
        s.set_text(4, v["pdfUrl"])


def _read_plain_fetch(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "parts": [_plain_part_codec.read(item, caps) for item in s.get_struct_list(0, 1, 2)],
        "chapters": [_chapter_marker_codec.read(item, caps) for item in s.get_struct_list(3, 1, 1)],
    }
    m4b_path = s.get_text(1)
    if not (m4b_path == ""):
        out["m4bPath"] = m4b_path
    cover_path = s.get_text(2)
    if not (cover_path == ""):
        out["coverPath"] = cover_path
    pdf_url = s.get_text(4)
    if not (pdf_url == ""):
        out["pdfUrl"] = pdf_url
    return out


_plain_fetch_codec = _Codec(0, 5, _write_plain_fetch, _read_plain_fetch)
"""Wire codec for ``PlainFetch`` (0 data words, 5 pointers)."""


def _write_fetch_title_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _plain_fetch_codec.write(s.init_struct(0, 0, 5), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown FetchTitleReply union member: {kind}")


def _read_fetch_title_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _plain_fetch_codec.read(s.get_struct(0, 0, 5), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown FetchTitleReply union member: {disc}")


_fetch_title_reply_codec = _Codec(1, 1, _write_fetch_title_reply, _read_fetch_title_reply)
"""Wire codec for ``FetchTitleReply`` (1 data words, 1 pointers)."""


def _write_source_accounts(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["accounts"]), 1, 4)
    for item, elem in zip(items, v["accounts"], strict=True):
        _source_account_codec.write(item, elem, caps)


def _read_source_accounts(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "accounts": [_source_account_codec.read(item, caps) for item in s.get_struct_list(0, 1, 4)],
    }


_source_accounts_codec = _Codec(0, 1, _write_source_accounts, _read_source_accounts)
"""Wire codec for ``SourceAccounts`` (0 data words, 1 pointers)."""


def _write_source_accounts_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _source_accounts_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown SourceAccountsReply union member: {kind}")


def _read_source_accounts_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _source_accounts_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown SourceAccountsReply union member: {disc}")


_source_accounts_reply_codec = _Codec(1, 1, _write_source_accounts_reply, _read_source_accounts_reply)
"""Wire codec for ``SourceAccountsReply`` (1 data words, 1 pointers)."""


def _write_search_catalog_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["query"])
    s.set_text(1, v["region"])
    s.set_u32(0, v["limit"])
    s.set_u32(1, v["page"])
    s.set_u16(4, _ord(A.CATALOG_SORTS, v["sort"], "CatalogSort"))
    s.set_u16(5, _ord(A.CATALOG_FIELDS, v["field"], "CatalogField"))
    if v.get("language") is not None:
        s.set_text(2, v["language"])


def _read_search_catalog_params(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "query": s.get_text(0),
        "region": s.get_text(1),
        "limit": s.get_u32(0),
        "page": s.get_u32(1),
        "sort": _from_ord(A.CATALOG_SORTS, s.get_u16(4), "CatalogSort"),
        "field": _from_ord(A.CATALOG_FIELDS, s.get_u16(5), "CatalogField"),
    }
    language = s.get_text(2)
    if not (language == ""):
        out["language"] = language
    return out


_search_catalog_params_codec = _Codec(2, 3, _write_search_catalog_params, _read_search_catalog_params)
"""Wire codec for ``SearchCatalogParams`` (2 data words, 3 pointers)."""


def _write_expand_candidates_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["source"])
    s.set_text(1, v["productId"])
    s.set_text(2, v["title"])
    if v.get("authors") is not None:
        s.set_text(3, v["authors"])
    if v.get("narrators") is not None:
        s.set_text(4, v["narrators"])
    if v.get("series") is not None:
        s.set_text(5, v["series"])
    if v.get("seriesAsin") is not None:
        s.set_text(6, v["seriesAsin"])
    if v.get("asin") is not None:
        s.set_text(7, v["asin"])
    if v.get("isbn") is not None:
        s.set_text(8, v["isbn"])
    s.set_text(9, v["region"])
    s.set_u32(0, v["limit"])


def _read_expand_candidates_params(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "source": s.get_text(0),
        "productId": s.get_text(1),
        "title": s.get_text(2),
        "region": s.get_text(9),
        "limit": s.get_u32(0),
    }
    authors = s.get_text(3)
    if not (authors == ""):
        out["authors"] = authors
    narrators = s.get_text(4)
    if not (narrators == ""):
        out["narrators"] = narrators
    series = s.get_text(5)
    if not (series == ""):
        out["series"] = series
    series_asin = s.get_text(6)
    if not (series_asin == ""):
        out["seriesAsin"] = series_asin
    asin = s.get_text(7)
    if not (asin == ""):
        out["asin"] = asin
    isbn = s.get_text(8)
    if not (isbn == ""):
        out["isbn"] = isbn
    return out


_expand_candidates_params_codec = _Codec(1, 10, _write_expand_candidates_params, _read_expand_candidates_params)
"""Wire codec for ``ExpandCandidatesParams`` (1 data words, 10 pointers)."""


def _write_purchase_hint_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    if v.get("productId") is not None:
        s.set_text(0, v["productId"])
    if v.get("title") is not None:
        s.set_text(1, v["title"])
    if v.get("authors") is not None:
        s.set_text(2, v["authors"])
    if v.get("asin") is not None:
        s.set_text(3, v["asin"])
    if v.get("isbn") is not None:
        s.set_text(4, v["isbn"])
    s.set_text(5, v["region"])
    s.set_bool(0, v["withPrice"])


def _read_purchase_hint_params(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "region": s.get_text(5),
        "withPrice": s.get_bool(0),
    }
    product_id = s.get_text(0)
    if not (product_id == ""):
        out["productId"] = product_id
    title = s.get_text(1)
    if not (title == ""):
        out["title"] = title
    authors = s.get_text(2)
    if not (authors == ""):
        out["authors"] = authors
    asin = s.get_text(3)
    if not (asin == ""):
        out["asin"] = asin
    isbn = s.get_text(4)
    if not (isbn == ""):
        out["isbn"] = isbn
    return out


_purchase_hint_params_codec = _Codec(1, 6, _write_purchase_hint_params, _read_purchase_hint_params)
"""Wire codec for ``PurchaseHintParams`` (1 data words, 6 pointers)."""


def _write_list_deals_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    if v.get("limit") is not None:
        s.set_u32(0, v["limit"])


def _read_list_deals_params(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
    }
    limit = s.get_u32(0)
    if not (limit == 0):
        out["limit"] = limit
    return out


_list_deals_params_codec = _Codec(1, 0, _write_list_deals_params, _read_list_deals_params)
"""Wire codec for ``ListDealsParams`` (1 data words, 0 pointers)."""


def _write_catalog_detail_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["productId"])
    if v.get("isbn") is not None:
        s.set_text(1, v["isbn"])


def _read_catalog_detail_params(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "productId": s.get_text(0),
    }
    isbn = s.get_text(1)
    if not (isbn == ""):
        out["isbn"] = isbn
    return out


_catalog_detail_params_codec = _Codec(0, 2, _write_catalog_detail_params, _read_catalog_detail_params)
"""Wire codec for ``CatalogDetailParams`` (0 data words, 2 pointers)."""


def _write_catalog_hit(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["productId"])
    s.set_text(1, v["title"])
    if v.get("authors") is not None:
        s.set_text(2, v["authors"])
    if v.get("narrators") is not None:
        s.set_text(3, v["narrators"])
    if v.get("series") is not None:
        s.set_text(4, v["series"])
    if v.get("seriesIndex") is not None:
        s.set_text(5, v["seriesIndex"])
    if v.get("asin") is not None:
        s.set_text(6, v["asin"])
    if v.get("isbn") is not None:
        s.set_text(7, v["isbn"])
    if v.get("url") is not None:
        s.set_text(8, v["url"])
    if v.get("coverUrl") is not None:
        s.set_text(9, v["coverUrl"])
    s.set_text(10, v["origin"])
    if v.get("subtitle") is not None:
        s.set_text(11, v["subtitle"])
    if v.get("description") is not None:
        s.set_text(12, v["description"])
    if v.get("publisher") is not None:
        s.set_text(13, v["publisher"])
    if v.get("lengthMinutes") is not None:
        s.set_i64(0, v["lengthMinutes"])
    if v.get("publishedAt") is not None:
        s.set_text(14, v["publishedAt"])
    if v.get("categories") is not None:
        s.set_text(15, v["categories"])
    if v.get("language") is not None:
        s.set_text(16, v["language"])
    if v.get("priceCents") is not None:
        s.set_i64(1, v["priceCents"])
    if v.get("currency") is not None:
        s.set_text(17, v["currency"])
    if v.get("priceLabel") is not None:
        s.set_text(18, v["priceLabel"])
    if v.get("ratingOverall") is not None:
        s.set_f64(2, v["ratingOverall"])
    if v.get("ratingCount") is not None:
        s.set_i64(3, v["ratingCount"])
    s.set_u16(16, _ord(A.ABRIDGEMENTS, v["abridgement"], "Abridgement"))


def _read_catalog_hit(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "productId": s.get_text(0),
        "title": s.get_text(1),
        "origin": s.get_text(10),
        "abridgement": _from_ord(A.ABRIDGEMENTS, s.get_u16(16), "Abridgement"),
    }
    authors = s.get_text(2)
    if not (authors == ""):
        out["authors"] = authors
    narrators = s.get_text(3)
    if not (narrators == ""):
        out["narrators"] = narrators
    series = s.get_text(4)
    if not (series == ""):
        out["series"] = series
    series_index = s.get_text(5)
    if not (series_index == ""):
        out["seriesIndex"] = series_index
    asin = s.get_text(6)
    if not (asin == ""):
        out["asin"] = asin
    isbn = s.get_text(7)
    if not (isbn == ""):
        out["isbn"] = isbn
    url = s.get_text(8)
    if not (url == ""):
        out["url"] = url
    cover_url = s.get_text(9)
    if not (cover_url == ""):
        out["coverUrl"] = cover_url
    subtitle = s.get_text(11)
    if not (subtitle == ""):
        out["subtitle"] = subtitle
    description = s.get_text(12)
    if not (description == ""):
        out["description"] = description
    publisher = s.get_text(13)
    if not (publisher == ""):
        out["publisher"] = publisher
    length_minutes = s.get_i64(0)
    if not (length_minutes == 0):
        out["lengthMinutes"] = length_minutes
    published_at = s.get_text(14)
    if not (published_at == ""):
        out["publishedAt"] = published_at
    categories = s.get_text(15)
    if not (categories == ""):
        out["categories"] = categories
    language = s.get_text(16)
    if not (language == ""):
        out["language"] = language
    price_cents = s.get_i64(1)
    if not (price_cents == 0):
        out["priceCents"] = price_cents
    currency = s.get_text(17)
    if not (currency == ""):
        out["currency"] = currency
    price_label = s.get_text(18)
    if not (price_label == ""):
        out["priceLabel"] = price_label
    rating_overall = s.get_f64(2)
    if not (rating_overall == 0):
        out["ratingOverall"] = rating_overall
    rating_count = s.get_i64(3)
    if not (rating_count == 0):
        out["ratingCount"] = rating_count
    return out


_catalog_hit_codec = _Codec(5, 19, _write_catalog_hit, _read_catalog_hit)
"""Wire codec for ``CatalogHit`` (5 data words, 19 pointers)."""


def _write_catalog_hits(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["hits"]), 5, 19)
    for item, elem in zip(items, v["hits"], strict=True):
        _catalog_hit_codec.write(item, elem, caps)


def _read_catalog_hits(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "hits": [_catalog_hit_codec.read(item, caps) for item in s.get_struct_list(0, 5, 19)],
    }


_catalog_hits_codec = _Codec(0, 1, _write_catalog_hits, _read_catalog_hits)
"""Wire codec for ``CatalogHits`` (0 data words, 1 pointers)."""


def _write_catalog_hits_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _catalog_hits_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown CatalogHitsReply union member: {kind}")


def _read_catalog_hits_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _catalog_hits_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown CatalogHitsReply union member: {disc}")


_catalog_hits_reply_codec = _Codec(1, 1, _write_catalog_hits_reply, _read_catalog_hits_reply)
"""Wire codec for ``CatalogHitsReply`` (1 data words, 1 pointers)."""


def _write_catalog_detail(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_bool(0, v["found"])
    _catalog_hit_codec.write(s.init_struct(0, 5, 19), v["hit"], caps)


def _read_catalog_detail(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "found": s.get_bool(0),
        "hit": _catalog_hit_codec.read(s.get_struct(0, 5, 19), caps),
    }


_catalog_detail_codec = _Codec(1, 1, _write_catalog_detail, _read_catalog_detail)
"""Wire codec for ``CatalogDetail`` (1 data words, 1 pointers)."""


def _write_catalog_detail_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _catalog_detail_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown CatalogDetailReply union member: {kind}")


def _read_catalog_detail_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _catalog_detail_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown CatalogDetailReply union member: {disc}")


_catalog_detail_reply_codec = _Codec(1, 1, _write_catalog_detail_reply, _read_catalog_detail_reply)
"""Wire codec for ``CatalogDetailReply`` (1 data words, 1 pointers)."""


def _write_purchase_hint(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["productId"])
    if v.get("title") is not None:
        s.set_text(1, v["title"])
    if v.get("url") is not None:
        s.set_text(2, v["url"])
    if v.get("priceCents") is not None:
        s.set_i64(0, v["priceCents"])
    if v.get("currency") is not None:
        s.set_text(3, v["currency"])
    if v.get("priceLabel") is not None:
        s.set_text(4, v["priceLabel"])
    if v.get("listPriceCents") is not None:
        s.set_i64(1, v["listPriceCents"])
    if v.get("listPriceLabel") is not None:
        s.set_text(5, v["listPriceLabel"])
    if v.get("memberPriceCents") is not None:
        s.set_i64(2, v["memberPriceCents"])
    if v.get("memberPriceLabel") is not None:
        s.set_text(6, v["memberPriceLabel"])


def _read_purchase_hint(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "productId": s.get_text(0),
    }
    title = s.get_text(1)
    if not (title == ""):
        out["title"] = title
    url = s.get_text(2)
    if not (url == ""):
        out["url"] = url
    price_cents = s.get_i64(0)
    if not (price_cents == 0):
        out["priceCents"] = price_cents
    currency = s.get_text(3)
    if not (currency == ""):
        out["currency"] = currency
    price_label = s.get_text(4)
    if not (price_label == ""):
        out["priceLabel"] = price_label
    list_price_cents = s.get_i64(1)
    if not (list_price_cents == 0):
        out["listPriceCents"] = list_price_cents
    list_price_label = s.get_text(5)
    if not (list_price_label == ""):
        out["listPriceLabel"] = list_price_label
    member_price_cents = s.get_i64(2)
    if not (member_price_cents == 0):
        out["memberPriceCents"] = member_price_cents
    member_price_label = s.get_text(6)
    if not (member_price_label == ""):
        out["memberPriceLabel"] = member_price_label
    return out


_purchase_hint_codec = _Codec(3, 7, _write_purchase_hint, _read_purchase_hint)
"""Wire codec for ``PurchaseHint`` (3 data words, 7 pointers)."""


def _write_purchase_hint_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_bool(0, v["found"])
    _purchase_hint_codec.write(s.init_struct(0, 3, 7), v["hint"], caps)


def _read_purchase_hint_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "found": s.get_bool(0),
        "hint": _purchase_hint_codec.read(s.get_struct(0, 3, 7), caps),
    }


_purchase_hint_result_codec = _Codec(1, 1, _write_purchase_hint_result, _read_purchase_hint_result)
"""Wire codec for ``PurchaseHintResult`` (1 data words, 1 pointers)."""


def _write_purchase_hint_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _purchase_hint_result_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown PurchaseHintReply union member: {kind}")


def _read_purchase_hint_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _purchase_hint_result_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown PurchaseHintReply union member: {disc}")


_purchase_hint_reply_codec = _Codec(1, 1, _write_purchase_hint_reply, _read_purchase_hint_reply)
"""Wire codec for ``PurchaseHintReply`` (1 data words, 1 pointers)."""


def _write_scan_library_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_bool(0, v["force"])


def _read_scan_library_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "force": s.get_bool(0),
    }


_scan_library_params_codec = _Codec(1, 0, _write_scan_library_params, _read_scan_library_params)
"""Wire codec for ``ScanLibraryParams`` (1 data words, 0 pointers)."""


def _write_authenticate_user_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["username"])
    s.set_text(1, v["password"])


def _read_authenticate_user_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "username": s.get_text(0),
        "password": s.get_text(1),
    }


_authenticate_user_params_codec = _Codec(0, 2, _write_authenticate_user_params, _read_authenticate_user_params)
"""Wire codec for ``AuthenticateUserParams`` (0 data words, 2 pointers)."""


def _write_external_user(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["provider"])
    s.set_text(1, v["externalUserId"])
    if v.get("displayName") is not None:
        s.set_text(2, v["displayName"])
    if v.get("accessToken") is not None:
        s.set_text(3, v["accessToken"])


def _read_external_user(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "provider": s.get_text(0),
        "externalUserId": s.get_text(1),
    }
    display_name = s.get_text(2)
    if not (display_name == ""):
        out["displayName"] = display_name
    access_token = s.get_text(3)
    if not (access_token == ""):
        out["accessToken"] = access_token
    return out


_external_user_codec = _Codec(0, 4, _write_external_user, _read_external_user)
"""Wire codec for ``ExternalUser`` (0 data words, 4 pointers)."""


def _write_external_user_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _external_user_codec.write(s.init_struct(0, 0, 4), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown ExternalUserReply union member: {kind}")


def _read_external_user_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _external_user_codec.read(s.get_struct(0, 0, 4), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown ExternalUserReply union member: {disc}")


_external_user_reply_codec = _Codec(1, 1, _write_external_user_reply, _read_external_user_reply)
"""Wire codec for ``ExternalUserReply`` (1 data words, 1 pointers)."""


def _write_event_poll_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["users"]), 0, 4)
    for item, elem in zip(items, v["users"], strict=True):
        _external_user_codec.write(item, elem, caps)


def _read_event_poll_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "users": [_external_user_codec.read(item, caps) for item in s.get_struct_list(0, 0, 4)],
    }


_event_poll_result_codec = _Codec(0, 1, _write_event_poll_result, _read_event_poll_result)
"""Wire codec for ``EventPollResult`` (0 data words, 1 pointers)."""


def _write_event_poll_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _event_poll_result_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown EventPollReply union member: {kind}")


def _read_event_poll_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _event_poll_result_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown EventPollReply union member: {disc}")


_event_poll_reply_codec = _Codec(1, 1, _write_event_poll_reply, _read_event_poll_reply)
"""Wire codec for ``EventPollReply`` (1 data words, 1 pointers)."""


def _write_listening_progress(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["externalUserId"])
    s.set_text(1, v["externalItemId"])
    if v.get("identityId") is not None:
        s.set_i64(0, v["identityId"])
    if v.get("title") is not None:
        s.set_text(2, v["title"])
    if v.get("authors") is not None:
        s.set_text(3, v["authors"])
    if v.get("asin") is not None:
        s.set_text(4, v["asin"])
    if v.get("isbn") is not None:
        s.set_text(5, v["isbn"])
    if v.get("progress") is not None:
        s.set_f64(1, v["progress"])
    if v.get("currentTimeSeconds") is not None:
        s.set_f64(2, v["currentTimeSeconds"])
    if v.get("durationSeconds") is not None:
        s.set_f64(3, v["durationSeconds"])
    s.set_bool(256, v["isFinished"])
    if v.get("lastListenedAtUnixMs") is not None:
        s.set_u64(5, v["lastListenedAtUnixMs"])


def _read_listening_progress(s: _StructReader, caps: _CapTable) -> Any:
    out: dict[str, Any] = {
        "externalUserId": s.get_text(0),
        "externalItemId": s.get_text(1),
        "isFinished": s.get_bool(256),
    }
    identity_id = s.get_i64(0)
    if not (identity_id == 0):
        out["identityId"] = identity_id
    title = s.get_text(2)
    if not (title == ""):
        out["title"] = title
    authors = s.get_text(3)
    if not (authors == ""):
        out["authors"] = authors
    asin = s.get_text(4)
    if not (asin == ""):
        out["asin"] = asin
    isbn = s.get_text(5)
    if not (isbn == ""):
        out["isbn"] = isbn
    progress = s.get_f64(1)
    if not (progress == 0):
        out["progress"] = progress
    current_time_seconds = s.get_f64(2)
    if not (current_time_seconds == 0):
        out["currentTimeSeconds"] = current_time_seconds
    duration_seconds = s.get_f64(3)
    if not (duration_seconds == 0):
        out["durationSeconds"] = duration_seconds
    last_listened_at_unix_ms = s.get_u64(5)
    if not (last_listened_at_unix_ms == 0):
        out["lastListenedAtUnixMs"] = last_listened_at_unix_ms
    return out


_listening_progress_codec = _Codec(6, 6, _write_listening_progress, _read_listening_progress)
"""Wire codec for ``ListeningProgress`` (6 data words, 6 pointers)."""


def _write_sync_listening_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["items"]), 6, 6)
    for item, elem in zip(items, v["items"], strict=True):
        _listening_progress_codec.write(item, elem, caps)


def _read_sync_listening_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "items": [_listening_progress_codec.read(item, caps) for item in s.get_struct_list(0, 6, 6)],
    }


_sync_listening_result_codec = _Codec(0, 1, _write_sync_listening_result, _read_sync_listening_result)
"""Wire codec for ``SyncListeningResult`` (0 data words, 1 pointers)."""


def _write_sync_listening_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _sync_listening_result_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown SyncListeningReply union member: {kind}")


def _read_sync_listening_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _sync_listening_result_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown SyncListeningReply union member: {disc}")


_sync_listening_reply_codec = _Codec(1, 1, _write_sync_listening_reply, _read_sync_listening_reply)
"""Wire codec for ``SyncListeningReply`` (1 data words, 1 pointers)."""


def _write_plugin_event(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["eventType"])
    s.set_u32(0, v["schemaVersion"])
    s.set_text(1, v["deduplicationKey"])
    s.set_data(2, v["payload"])
    s.set_u64(1, v["occurredAtUnixMs"])
    s.set_text(3, v["correlationId"])
    s.set_text(4, v["causationId"])


def _read_plugin_event(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "eventType": s.get_text(0),
        "schemaVersion": s.get_u32(0),
        "deduplicationKey": s.get_text(1),
        "payload": s.get_data(2),
        "occurredAtUnixMs": s.get_u64(1),
        "correlationId": s.get_text(3),
        "causationId": s.get_text(4),
    }


_plugin_event_codec = _Codec(2, 5, _write_plugin_event, _read_plugin_event)
"""Wire codec for ``PluginEvent`` (2 data words, 5 pointers)."""


def _write_publish_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["eventId"])
    s.set_bool(0, v["duplicate"])


def _read_publish_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "eventId": s.get_text(0),
        "duplicate": s.get_bool(0),
    }


_publish_ok_codec = _Codec(1, 1, _write_publish_ok, _read_publish_ok)
"""Wire codec for ``PublishOk`` (1 data words, 1 pointers)."""


def _write_publish_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _publish_ok_codec.write(s.init_struct(0, 1, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown PublishReply union member: {kind}")


def _read_publish_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _publish_ok_codec.read(s.get_struct(0, 1, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown PublishReply union member: {disc}")


_publish_reply_codec = _Codec(1, 1, _write_publish_reply, _read_publish_reply)
"""Wire codec for ``PublishReply`` (1 data words, 1 pointers)."""


def _write_invocation(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["id"])
    s.set_text(1, v["accountId"])
    s.set_u64(0, v["deadlineUnixMs"])
    s.set_text(2, v["correlationId"])
    s.set_text(3, v["causationId"])


def _read_invocation(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "id": s.get_text(0),
        "accountId": s.get_text(1),
        "deadlineUnixMs": s.get_u64(0),
        "correlationId": s.get_text(2),
        "causationId": s.get_text(3),
    }


_invocation_codec = _Codec(1, 4, _write_invocation, _read_invocation)
"""Wire codec for ``Invocation`` (1 data words, 4 pointers)."""


def _write_db_value(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "null":
        s.set_u16(1, 0)
        s.set_u16(0, _ord(A.DB_TYPES, v["value"], "DbType"))
    elif kind == "boolean":
        s.set_u16(1, 1)
        s.set_bool(0, v["value"])
    elif kind == "int64":
        s.set_u16(1, 2)
        s.set_i64(1, v["value"])
    elif kind == "float64":
        s.set_u16(1, 3)
        s.set_f64(1, v["value"])
    elif kind == "text":
        s.set_u16(1, 4)
        s.set_text(0, v["value"])
    elif kind == "bytes":
        s.set_u16(1, 5)
        s.set_data(0, v["value"])
    else:
        raise ValueError(f"unknown DbValue union member: {kind}")


def _read_db_value(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(1)
    if disc == 0:
        return {"kind": "null", "value": _from_ord(A.DB_TYPES, s.get_u16(0), "DbType")}
    elif disc == 1:
        return {"kind": "boolean", "value": s.get_bool(0)}
    elif disc == 2:
        return {"kind": "int64", "value": s.get_i64(1)}
    elif disc == 3:
        return {"kind": "float64", "value": s.get_f64(1)}
    elif disc == 4:
        return {"kind": "text", "value": s.get_text(0)}
    elif disc == 5:
        return {"kind": "bytes", "value": s.get_data(0)}
    raise ValueError(f"unknown DbValue union member: {disc}")


_db_value_codec = _Codec(2, 1, _write_db_value, _read_db_value)
"""Wire codec for ``DbValue`` (2 data words, 1 pointers)."""


def _write_db_column(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["name"])
    s.set_u16(0, _ord(A.DB_TYPES, v["dbType"], "DbType"))


def _read_db_column(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "name": s.get_text(0),
        "dbType": _from_ord(A.DB_TYPES, s.get_u16(0), "DbType"),
    }


_db_column_codec = _Codec(1, 1, _write_db_column, _read_db_column)
"""Wire codec for ``DbColumn`` (1 data words, 1 pointers)."""


def _write_db_row(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["values"]), 2, 1)
    for item, elem in zip(items, v["values"], strict=True):
        _db_value_codec.write(item, elem, caps)


def _read_db_row(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "values": [_db_value_codec.read(item, caps) for item in s.get_struct_list(0, 2, 1)],
    }


_db_row_codec = _Codec(0, 1, _write_db_row, _read_db_row)
"""Wire codec for ``DbRow`` (0 data words, 1 pointers)."""


def _write_db_statement(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["sql"])
    items = s.init_struct_list(1, len(v["parameters"]), 2, 1)
    for item, elem in zip(items, v["parameters"], strict=True):
        _db_value_codec.write(item, elem, caps)
    s.set_u16(0, _ord(A.DB_STATEMENT_KINDS, v["kind"], "DbStatementKind"))
    s.set_u32(1, v["maxRows"])
    s.set_u16(1, _ord(A.DB_RESULT_SELECTIONS, v["resultSelection"], "DbResultSelection"))


def _read_db_statement(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "sql": s.get_text(0),
        "parameters": [_db_value_codec.read(item, caps) for item in s.get_struct_list(1, 2, 1)],
        "kind": _from_ord(A.DB_STATEMENT_KINDS, s.get_u16(0), "DbStatementKind"),
        "maxRows": s.get_u32(1),
        "resultSelection": _from_ord(A.DB_RESULT_SELECTIONS, s.get_u16(1), "DbResultSelection"),
    }


_db_statement_codec = _Codec(1, 2, _write_db_statement, _read_db_statement)
"""Wire codec for ``DbStatement`` (1 data words, 2 pointers)."""


def _write_execute_request(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["operationId"])
    s.set_text(1, v["requestHash"])
    items = s.init_struct_list(2, len(v["statements"]), 1, 2)
    for item, elem in zip(items, v["statements"], strict=True):
        _db_statement_codec.write(item, elem, caps)
    s.set_u64(0, v["deadlineUnixMs"])


def _read_execute_request(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "operationId": s.get_text(0),
        "requestHash": s.get_text(1),
        "statements": [_db_statement_codec.read(item, caps) for item in s.get_struct_list(2, 1, 2)],
        "deadlineUnixMs": s.get_u64(0),
    }


_execute_request_codec = _Codec(1, 3, _write_execute_request, _read_execute_request)
"""Wire codec for ``ExecuteRequest`` (1 data words, 3 pointers)."""


def _write_sql_span(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["start"])
    s.set_u32(1, v["end"])


def _read_sql_span(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "start": s.get_u32(0),
        "end": s.get_u32(1),
    }


_sql_span_codec = _Codec(1, 0, _write_sql_span, _read_sql_span)
"""Wire codec for ``SqlSpan`` (1 data words, 0 pointers)."""


def _write_text_collate_site(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _sql_span_codec.write(s.init_struct(0, 1, 0), v["span"], caps)


def _read_text_collate_site(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "span": _sql_span_codec.read(s.get_struct(0, 1, 0), caps),
    }


_text_collate_site_codec = _Codec(0, 1, _write_text_collate_site, _read_text_collate_site)
"""Wire codec for ``TextCollateSite`` (0 data words, 1 pointers)."""


def _write_integer_arith_site(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _sql_span_codec.write(s.init_struct(0, 1, 0), v["full"], caps)
    _sql_span_codec.write(s.init_struct(1, 1, 0), v["lhs"], caps)
    _sql_span_codec.write(s.init_struct(2, 1, 0), v["rhs"], caps)
    s.set_u16(0, _ord(A.INTEGER_ARITH_KINDS, v["kind"], "IntegerArithKind"))


def _read_integer_arith_site(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "full": _sql_span_codec.read(s.get_struct(0, 1, 0), caps),
        "lhs": _sql_span_codec.read(s.get_struct(1, 1, 0), caps),
        "rhs": _sql_span_codec.read(s.get_struct(2, 1, 0), caps),
        "kind": _from_ord(A.INTEGER_ARITH_KINDS, s.get_u16(0), "IntegerArithKind"),
    }


_integer_arith_site_codec = _Codec(1, 3, _write_integer_arith_site, _read_integer_arith_site)
"""Wire codec for ``IntegerArithSite`` (1 data words, 3 pointers)."""


def _write_physical_access(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["table"])
    s.set_text(1, v["column"])


def _read_physical_access(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "table": s.get_text(0),
        "column": s.get_text(1),
    }


_physical_access_codec = _Codec(0, 2, _write_physical_access, _read_physical_access)
"""Wire codec for ``PhysicalAccess`` (0 data words, 2 pointers)."""


def _write_resolved_assignment(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["table"])
    s.set_text(1, v["column"])
    s.set_u16(0, _ord(A.RESOLVED_SQL_TYPES, v["dest"], "ResolvedSqlType"))
    s.set_u16(1, _ord(A.RESOLVED_SQL_TYPES, v["source"], "ResolvedSqlType"))


def _read_resolved_assignment(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "table": s.get_text(0),
        "column": s.get_text(1),
        "dest": _from_ord(A.RESOLVED_SQL_TYPES, s.get_u16(0), "ResolvedSqlType"),
        "source": _from_ord(A.RESOLVED_SQL_TYPES, s.get_u16(1), "ResolvedSqlType"),
    }


_resolved_assignment_codec = _Codec(1, 2, _write_resolved_assignment, _read_resolved_assignment)
"""Wire codec for ``ResolvedAssignment`` (1 data words, 2 pointers)."""


def _write_named_sql_type(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["name"])
    s.set_u16(0, _ord(A.RESOLVED_SQL_TYPES, v["sqlType"], "ResolvedSqlType"))


def _read_named_sql_type(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "name": s.get_text(0),
        "sqlType": _from_ord(A.RESOLVED_SQL_TYPES, s.get_u16(0), "ResolvedSqlType"),
    }


_named_sql_type_codec = _Codec(1, 1, _write_named_sql_type, _read_named_sql_type)
"""Wire codec for ``NamedSqlType`` (1 data words, 1 pointers)."""


def _write_column_reference(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["refTable"])
    s.set_text_list(1, v["refColumns"])


def _read_column_reference(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "refTable": s.get_text(0),
        "refColumns": s.get_text_list(1),
    }


_column_reference_codec = _Codec(0, 2, _write_column_reference, _read_column_reference)
"""Wire codec for ``ColumnReference`` (0 data words, 2 pointers)."""


def _write_optional_column_reference(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "none":
        s.set_u16(0, 0)
    elif kind == "some":
        s.set_u16(0, 1)
        _column_reference_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown OptionalColumnReference union member: {kind}")


def _read_optional_column_reference(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "none"}
    elif disc == 1:
        return {"kind": "some", "value": _column_reference_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown OptionalColumnReference union member: {disc}")


_optional_column_reference_codec = _Codec(1, 1, _write_optional_column_reference, _read_optional_column_reference)
"""Wire codec for ``OptionalColumnReference`` (1 data words, 1 pointers)."""


def _write_foreign_key_constraint(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text_list(0, v["columns"])
    s.set_text(1, v["refTable"])
    s.set_text_list(2, v["refColumns"])


def _read_foreign_key_constraint(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "columns": s.get_text_list(0),
        "refTable": s.get_text(1),
        "refColumns": s.get_text_list(2),
    }


_foreign_key_constraint_codec = _Codec(0, 3, _write_foreign_key_constraint, _read_foreign_key_constraint)
"""Wire codec for ``ForeignKeyConstraint`` (0 data words, 3 pointers)."""


def _write_table_constraint(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "primaryKey":
        s.set_u16(0, 0)
        s.set_text_list(0, v["value"])
    elif kind == "unique":
        s.set_u16(0, 1)
        s.set_text_list(0, v["value"])
    elif kind == "check":
        s.set_u16(0, 2)
        s.set_text(0, v["value"])
    elif kind == "foreignKey":
        s.set_u16(0, 3)
        _foreign_key_constraint_codec.write(s.init_struct(0, 0, 3), v["value"], caps)
    else:
        raise ValueError(f"unknown TableConstraint union member: {kind}")


def _read_table_constraint(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "primaryKey", "value": s.get_text_list(0)}
    elif disc == 1:
        return {"kind": "unique", "value": s.get_text_list(0)}
    elif disc == 2:
        return {"kind": "check", "value": s.get_text(0)}
    elif disc == 3:
        return {"kind": "foreignKey", "value": _foreign_key_constraint_codec.read(s.get_struct(0, 0, 3), caps)}
    raise ValueError(f"unknown TableConstraint union member: {disc}")


_table_constraint_codec = _Codec(1, 1, _write_table_constraint, _read_table_constraint)
"""Wire codec for ``TableConstraint`` (1 data words, 1 pointers)."""


def _write_create_table_schema(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["table"])
    items = s.init_struct_list(1, len(v["columns"]), 1, 1)
    for item, elem in zip(items, v["columns"], strict=True):
        _named_sql_type_codec.write(item, elem, caps)
    s.set_text(2, v["identityColumn"])
    s.set_bool_list(3, v["columnNotNull"])
    s.set_bool_list(4, v["columnUnique"])
    s.set_bool_list(5, v["columnPrimaryKey"])
    s.set_text_list(6, v["columnDefaults"])
    s.set_text_list(7, v["columnChecks"])
    items = s.init_struct_list(8, len(v["columnReferences"]), 1, 1)
    for item, elem in zip(items, v["columnReferences"], strict=True):
        _optional_column_reference_codec.write(item, elem, caps)
    items = s.init_struct_list(9, len(v["tableConstraints"]), 1, 1)
    for item, elem in zip(items, v["tableConstraints"], strict=True):
        _table_constraint_codec.write(item, elem, caps)


def _read_create_table_schema(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "table": s.get_text(0),
        "columns": [_named_sql_type_codec.read(item, caps) for item in s.get_struct_list(1, 1, 1)],
        "identityColumn": s.get_text(2),
        "columnNotNull": s.get_bool_list(3),
        "columnUnique": s.get_bool_list(4),
        "columnPrimaryKey": s.get_bool_list(5),
        "columnDefaults": s.get_text_list(6),
        "columnChecks": s.get_text_list(7),
        "columnReferences": [_optional_column_reference_codec.read(item, caps) for item in s.get_struct_list(8, 1, 1)],
        "tableConstraints": [_table_constraint_codec.read(item, caps) for item in s.get_struct_list(9, 1, 1)],
    }


_create_table_schema_codec = _Codec(0, 10, _write_create_table_schema, _read_create_table_schema)
"""Wire codec for ``CreateTableSchema`` (0 data words, 10 pointers)."""


def _write_schema_create(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _create_table_schema_codec.write(s.init_struct(0, 0, 10), v["schema"], caps)
    s.set_text(1, v["fingerprint"])
    s.set_bool(0, v["noop"])


def _read_schema_create(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "schema": _create_table_schema_codec.read(s.get_struct(0, 0, 10), caps),
        "fingerprint": s.get_text(1),
        "noop": s.get_bool(0),
    }


_schema_create_codec = _Codec(1, 2, _write_schema_create, _read_schema_create)
"""Wire codec for ``SchemaCreate`` (1 data words, 2 pointers)."""


def _write_schema_action(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "none":
        s.set_u16(0, 0)
    elif kind == "create":
        s.set_u16(0, 1)
        _schema_create_codec.write(s.init_struct(0, 1, 2), v["value"], caps)
    elif kind == "drop":
        s.set_u16(0, 2)
        s.set_text(0, v["value"])
    else:
        raise ValueError(f"unknown SchemaAction union member: {kind}")


def _read_schema_action(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "none"}
    elif disc == 1:
        return {"kind": "create", "value": _schema_create_codec.read(s.get_struct(0, 1, 2), caps)}
    elif disc == 2:
        return {"kind": "drop", "value": s.get_text(0)}
    raise ValueError(f"unknown SchemaAction union member: {disc}")


_schema_action_codec = _Codec(1, 1, _write_schema_action, _read_schema_action)
"""Wire codec for ``SchemaAction`` (1 data words, 1 pointers)."""


def _write_resolved_statement(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["statementHash"])
    items = s.init_struct_list(1, len(v["outputColumns"]), 1, 1)
    for item, elem in zip(items, v["outputColumns"], strict=True):
        _named_sql_type_codec.write(item, elem, caps)
    items = s.init_struct_list(2, len(v["physicalAccesses"]), 0, 2)
    for item, elem in zip(items, v["physicalAccesses"], strict=True):
        _physical_access_codec.write(item, elem, caps)
    items = s.init_struct_list(3, len(v["assignments"]), 1, 2)
    for item, elem in zip(items, v["assignments"], strict=True):
        _resolved_assignment_codec.write(item, elem, caps)
    items = s.init_struct_list(4, len(v["textCollateSites"]), 0, 1)
    for item, elem in zip(items, v["textCollateSites"], strict=True):
        _text_collate_site_codec.write(item, elem, caps)
    items = s.init_struct_list(5, len(v["integerArithSites"]), 1, 3)
    for item, elem in zip(items, v["integerArithSites"], strict=True):
        _integer_arith_site_codec.write(item, elem, caps)
    s.set_text_list(6, v["functions"])
    _schema_action_codec.write(s.init_struct(7, 1, 1), v["schemaAction"], caps)


def _read_resolved_statement(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "statementHash": s.get_text(0),
        "outputColumns": [_named_sql_type_codec.read(item, caps) for item in s.get_struct_list(1, 1, 1)],
        "physicalAccesses": [_physical_access_codec.read(item, caps) for item in s.get_struct_list(2, 0, 2)],
        "assignments": [_resolved_assignment_codec.read(item, caps) for item in s.get_struct_list(3, 1, 2)],
        "textCollateSites": [_text_collate_site_codec.read(item, caps) for item in s.get_struct_list(4, 0, 1)],
        "integerArithSites": [_integer_arith_site_codec.read(item, caps) for item in s.get_struct_list(5, 1, 3)],
        "functions": s.get_text_list(6),
        "schemaAction": _schema_action_codec.read(s.get_struct(7, 1, 1), caps),
    }


_resolved_statement_codec = _Codec(0, 8, _write_resolved_statement, _read_resolved_statement)
"""Wire codec for ``ResolvedStatement`` (0 data words, 8 pointers)."""


def _write_adapter_receipt(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["guestLen"])
    s.set_text(0, v["guestHash"])


def _read_adapter_receipt(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "guestLen": s.get_u32(0),
        "guestHash": s.get_text(0),
    }


_adapter_receipt_codec = _Codec(1, 1, _write_adapter_receipt, _read_adapter_receipt)
"""Wire codec for ``AdapterReceipt`` (1 data words, 1 pointers)."""


def _write_adapter_statement(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["sql"])
    items = s.init_struct_list(1, len(v["parameters"]), 2, 1)
    for item, elem in zip(items, v["parameters"], strict=True):
        _db_value_codec.write(item, elem, caps)
    s.set_u16(0, _ord(A.DB_STATEMENT_KINDS, v["kind"], "DbStatementKind"))
    s.set_u32(1, v["maxRows"])
    s.set_u16(1, _ord(A.DB_RESULT_SELECTIONS, v["resultSelection"], "DbResultSelection"))
    _resolved_statement_codec.write(s.init_struct(2, 0, 8), v["proof"], caps)


def _read_adapter_statement(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "sql": s.get_text(0),
        "parameters": [_db_value_codec.read(item, caps) for item in s.get_struct_list(1, 2, 1)],
        "kind": _from_ord(A.DB_STATEMENT_KINDS, s.get_u16(0), "DbStatementKind"),
        "maxRows": s.get_u32(1),
        "resultSelection": _from_ord(A.DB_RESULT_SELECTIONS, s.get_u16(1), "DbResultSelection"),
        "proof": _resolved_statement_codec.read(s.get_struct(2, 0, 8), caps),
    }


_adapter_statement_codec = _Codec(1, 3, _write_adapter_statement, _read_adapter_statement)
"""Wire codec for ``AdapterStatement`` (1 data words, 3 pointers)."""


def _write_adapter_execute_request(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["operationId"])
    s.set_text(1, v["requestHash"])
    items = s.init_struct_list(2, len(v["statements"]), 1, 3)
    for item, elem in zip(items, v["statements"], strict=True):
        _adapter_statement_codec.write(item, elem, caps)
    s.set_u64(0, v["deadlineUnixMs"])
    s.set_u16(4, _ord(A.ISOLATION_REQS, v["isolation"], "IsolationReq"))
    _adapter_receipt_codec.write(s.init_struct(3, 1, 1), v["receipt"], caps)


def _read_adapter_execute_request(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "operationId": s.get_text(0),
        "requestHash": s.get_text(1),
        "statements": [_adapter_statement_codec.read(item, caps) for item in s.get_struct_list(2, 1, 3)],
        "deadlineUnixMs": s.get_u64(0),
        "isolation": _from_ord(A.ISOLATION_REQS, s.get_u16(4), "IsolationReq"),
        "receipt": _adapter_receipt_codec.read(s.get_struct(3, 1, 1), caps),
    }


_adapter_execute_request_codec = _Codec(2, 4, _write_adapter_execute_request, _read_adapter_execute_request)
"""Wire codec for ``AdapterExecuteRequest`` (2 data words, 4 pointers)."""


def _write_statement_result(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["rows"]), 0, 1)
    for item, elem in zip(items, v["rows"], strict=True):
        _db_row_codec.write(item, elem, caps)
    items = s.init_struct_list(1, len(v["columns"]), 1, 1)
    for item, elem in zip(items, v["columns"], strict=True):
        _db_column_codec.write(item, elem, caps)
    s.set_u64(0, v["rowsAffected"])


def _read_statement_result(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "rows": [_db_row_codec.read(item, caps) for item in s.get_struct_list(0, 0, 1)],
        "columns": [_db_column_codec.read(item, caps) for item in s.get_struct_list(1, 1, 1)],
        "rowsAffected": s.get_u64(0),
    }


_statement_result_codec = _Codec(1, 2, _write_statement_result, _read_statement_result)
"""Wire codec for ``StatementResult`` (1 data words, 2 pointers)."""


def _write_db_timing(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u64(0, v["attemptElapsedUs"])
    s.set_u64(1, v["dbExecutionUs"])
    s.set_text(0, v["dbTimingSource"])


def _read_db_timing(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "attemptElapsedUs": s.get_u64(0),
        "dbExecutionUs": s.get_u64(1),
        "dbTimingSource": s.get_text(0),
    }


_db_timing_codec = _Codec(2, 1, _write_db_timing, _read_db_timing)
"""Wire codec for ``DbTiming`` (2 data words, 1 pointers)."""


def _write_execute_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["operationId"])
    items = s.init_struct_list(1, len(v["statements"]), 1, 2)
    for item, elem in zip(items, v["statements"], strict=True):
        _statement_result_codec.write(item, elem, caps)
    _db_timing_codec.write(s.init_struct(2, 2, 1), v["timing"], caps)


def _read_execute_reply(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "operationId": s.get_text(0),
        "statements": [_statement_result_codec.read(item, caps) for item in s.get_struct_list(1, 1, 2)],
        "timing": _db_timing_codec.read(s.get_struct(2, 2, 1), caps),
    }


_execute_reply_codec = _Codec(0, 3, _write_execute_reply, _read_execute_reply)
"""Wire codec for ``ExecuteReply`` (0 data words, 3 pointers)."""


def _write_execute_result_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _execute_reply_codec.write(s.init_struct(0, 0, 3), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown ExecuteResultReply union member: {kind}")


def _read_execute_result_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _execute_reply_codec.read(s.get_struct(0, 0, 3), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown ExecuteResultReply union member: {disc}")


_execute_result_reply_codec = _Codec(1, 1, _write_execute_result_reply, _read_execute_result_reply)
"""Wire codec for ``ExecuteResultReply`` (1 data words, 1 pointers)."""


def _write_db_capabilities(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["sqlContractVersion"])
    s.set_bool(32, v["atomicBatch"])
    s.set_bool(33, v["returning"])
    s.set_bool(34, v["affectedRows"])
    s.set_bool(35, v["schemaMigrations"])
    s.set_bool(36, v["cancellation"])
    s.set_bool(37, v["timing"])
    s.set_u32(2, v["maxBinds"])
    s.set_u32(3, v["maxStatements"])
    s.set_u32(4, v["maxResultRows"])
    s.set_u32(5, v["maxPayloadBytes"])
    s.set_u32(6, v["maxResultBytes"])
    s.set_u32(7, v["maxCellBytes"])
    s.set_u32(8, v["maxRequestBytes"])
    s.set_u32(9, v["maxAtomicResultBytes"])
    s.set_bool(38, v["pluginDatabases"])
    s.set_u32(10, v["maxFunctionArgs"])
    s.set_u32(11, v["maxSchemaColumns"])
    s.set_u32(12, v["maxPatternBytes"])
    s.set_u32(13, v["maxLoweredStatementBytes"])
    s.set_bool(39, v["consistentBackupRead"])
    s.set_bool(40, v["atomicUnitRestore"])


def _read_db_capabilities(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "sqlContractVersion": s.get_u32(0),
        "atomicBatch": s.get_bool(32),
        "returning": s.get_bool(33),
        "affectedRows": s.get_bool(34),
        "schemaMigrations": s.get_bool(35),
        "cancellation": s.get_bool(36),
        "timing": s.get_bool(37),
        "maxBinds": s.get_u32(2),
        "maxStatements": s.get_u32(3),
        "maxResultRows": s.get_u32(4),
        "maxPayloadBytes": s.get_u32(5),
        "maxResultBytes": s.get_u32(6),
        "maxCellBytes": s.get_u32(7),
        "maxRequestBytes": s.get_u32(8),
        "maxAtomicResultBytes": s.get_u32(9),
        "pluginDatabases": s.get_bool(38),
        "maxFunctionArgs": s.get_u32(10),
        "maxSchemaColumns": s.get_u32(11),
        "maxPatternBytes": s.get_u32(12),
        "maxLoweredStatementBytes": s.get_u32(13),
        "consistentBackupRead": s.get_bool(39),
        "atomicUnitRestore": s.get_bool(40),
    }


_db_capabilities_codec = _Codec(7, 0, _write_db_capabilities, _read_db_capabilities)
"""Wire codec for ``DbCapabilities`` (7 data words, 0 pointers)."""


def _write_db_bootstrap_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _db_bootstrap_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown DbBootstrapReply union member: {kind}")


def _read_db_bootstrap_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _db_bootstrap_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown DbBootstrapReply union member: {disc}")


_db_bootstrap_reply_codec = _Codec(1, 1, _write_db_bootstrap_reply, _read_db_bootstrap_reply)
"""Wire codec for ``DbBootstrapReply`` (1 data words, 1 pointers)."""


def _write_db_bootstrap(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["engine"])


def _read_db_bootstrap(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "engine": s.get_text(0),
    }


_db_bootstrap_codec = _Codec(0, 1, _write_db_bootstrap, _read_db_bootstrap)
"""Wire codec for ``DbBootstrap`` (0 data words, 1 pointers)."""


def _write_db_capabilities_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _db_capabilities_codec.write(s.init_struct(0, 7, 0), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown DbCapabilitiesReply union member: {kind}")


def _read_db_capabilities_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _db_capabilities_codec.read(s.get_struct(0, 7, 0), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown DbCapabilitiesReply union member: {disc}")


_db_capabilities_reply_codec = _Codec(1, 1, _write_db_capabilities_reply, _read_db_capabilities_reply)
"""Wire codec for ``DbCapabilitiesReply`` (1 data words, 1 pointers)."""


def _write_identity_high_water(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["table"])
    s.set_i64(0, v["last"])


def _read_identity_high_water(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "table": s.get_text(0),
        "last": s.get_i64(0),
    }


_identity_high_water_codec = _Codec(1, 1, _write_identity_high_water, _read_identity_high_water)
"""Wire codec for ``IdentityHighWater`` (1 data words, 1 pointers)."""


def _write_identity_export_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        items = s.init_struct_list(0, len(v["value"]), 1, 1)
        for item, elem in zip(items, v["value"], strict=True):
            _identity_high_water_codec.write(item, elem, caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown IdentityExportReply union member: {kind}")


def _read_identity_export_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": [_identity_high_water_codec.read(item, caps) for item in s.get_struct_list(0, 1, 1)]}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown IdentityExportReply union member: {disc}")


_identity_export_reply_codec = _Codec(1, 1, _write_identity_export_reply, _read_identity_export_reply)
"""Wire codec for ``IdentityExportReply`` (1 data words, 1 pointers)."""


def _write_user_relations_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        s.set_text_list(0, v["value"])
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown UserRelationsReply union member: {kind}")


def _read_user_relations_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": s.get_text_list(0)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown UserRelationsReply union member: {disc}")


_user_relations_reply_codec = _Codec(1, 1, _write_user_relations_reply, _read_user_relations_reply)
"""Wire codec for ``UserRelationsReply`` (1 data words, 1 pointers)."""


def _write_plugin_migration_op(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "schema":
        s.set_u16(0, 0)
        s.set_text(0, v["value"])
    elif kind == "data":
        s.set_u16(0, 1)
        s.set_text(0, v["value"])
    else:
        raise ValueError(f"unknown PluginMigrationOp union member: {kind}")


def _read_plugin_migration_op(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "schema", "value": s.get_text(0)}
    elif disc == 1:
        return {"kind": "data", "value": s.get_text(0)}
    raise ValueError(f"unknown PluginMigrationOp union member: {disc}")


_plugin_migration_op_codec = _Codec(1, 1, _write_plugin_migration_op, _read_plugin_migration_op)
"""Wire codec for ``PluginMigrationOp`` (1 data words, 1 pointers)."""


def _write_plugin_migration(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["id"])
    items = s.init_struct_list(1, len(v["operations"]), 1, 1)
    for item, elem in zip(items, v["operations"], strict=True):
        _plugin_migration_op_codec.write(item, elem, caps)


def _read_plugin_migration(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "id": s.get_text(0),
        "operations": [_plugin_migration_op_codec.read(item, caps) for item in s.get_struct_list(1, 1, 1)],
    }


_plugin_migration_codec = _Codec(0, 2, _write_plugin_migration, _read_plugin_migration)
"""Wire codec for ``PluginMigration`` (0 data words, 2 pointers)."""


def _write_plugin_migrations_ok(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["migrations"]), 0, 2)
    for item, elem in zip(items, v["migrations"], strict=True):
        _plugin_migration_codec.write(item, elem, caps)


def _read_plugin_migrations_ok(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "migrations": [_plugin_migration_codec.read(item, caps) for item in s.get_struct_list(0, 0, 2)],
    }


_plugin_migrations_ok_codec = _Codec(0, 1, _write_plugin_migrations_ok, _read_plugin_migrations_ok)
"""Wire codec for ``PluginMigrationsOk`` (0 data words, 1 pointers)."""


def _write_plugin_migrations_reply(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    kind = v["kind"]
    if kind == "ok":
        s.set_u16(0, 0)
        _plugin_migrations_ok_codec.write(s.init_struct(0, 0, 1), v["value"], caps)
    elif kind == "err":
        s.set_u16(0, 1)
        _plugin_error_codec.write(s.init_struct(0, 0, 2), v["value"], caps)
    else:
        raise ValueError(f"unknown PluginMigrationsReply union member: {kind}")


def _read_plugin_migrations_reply(s: _StructReader, caps: _CapTable) -> Any:
    disc = s.get_u16(0)
    if disc == 0:
        return {"kind": "ok", "value": _plugin_migrations_ok_codec.read(s.get_struct(0, 0, 1), caps)}
    elif disc == 1:
        return {"kind": "err", "value": _plugin_error_codec.read(s.get_struct(0, 0, 2), caps)}
    raise ValueError(f"unknown PluginMigrationsReply union member: {disc}")


_plugin_migrations_reply_codec = _Codec(1, 1, _write_plugin_migrations_reply, _read_plugin_migrations_reply)
"""Wire codec for ``PluginMigrationsReply`` (1 data words, 1 pointers)."""


def _write_byte_source_pull_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_u32(0, v["maxBytes"])


def _read_byte_source_pull_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "maxBytes": s.get_u32(0),
    }


_byte_source_pull_params_codec = _Codec(1, 0, _write_byte_source_pull_params, _read_byte_source_pull_params)
"""Wire codec for ``ByteSourcePullParams`` (1 data words, 0 pointers)."""


def _write_byte_source_pull_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _pull_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_byte_source_pull_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _pull_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_byte_source_pull_results_codec = _Codec(0, 1, _write_byte_source_pull_results, _read_byte_source_pull_results)
"""Wire codec for ``ByteSourcePullResults`` (0 data words, 1 pointers)."""


def _write_destination_head_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])


def _read_destination_head_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
    }


_destination_head_params_codec = _Codec(0, 1, _write_destination_head_params, _read_destination_head_params)
"""Wire codec for ``DestinationHeadParams`` (0 data words, 1 pointers)."""


def _write_destination_head_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _head_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_head_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _head_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_head_results_codec = _Codec(0, 1, _write_destination_head_results, _read_destination_head_results)
"""Wire codec for ``DestinationHeadResults`` (0 data words, 1 pointers)."""


def _write_destination_list_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _list_options_codec.write(s.init_struct(0, 1, 2), v["options"], caps)


def _read_destination_list_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "options": _list_options_codec.read(s.get_struct(0, 1, 2), caps),
    }


_destination_list_params_codec = _Codec(0, 1, _write_destination_list_params, _read_destination_list_params)
"""Wire codec for ``DestinationListParams`` (0 data words, 1 pointers)."""


def _write_destination_list_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _list_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_list_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _list_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_list_results_codec = _Codec(0, 1, _write_destination_list_results, _read_destination_list_results)
"""Wire codec for ``DestinationListResults`` (0 data words, 1 pointers)."""


def _write_destination_get_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    _read_options_codec.write(s.init_struct(1, 0, 1), v["options"], caps)


def _read_destination_get_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "options": _read_options_codec.read(s.get_struct(1, 0, 1), caps),
    }


_destination_get_params_codec = _Codec(0, 2, _write_destination_get_params, _read_destination_get_params)
"""Wire codec for ``DestinationGetParams`` (0 data words, 2 pointers)."""


def _write_destination_get_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _get_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_get_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _get_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_get_results_codec = _Codec(0, 1, _write_destination_get_results, _read_destination_get_results)
"""Wire codec for ``DestinationGetResults`` (0 data words, 1 pointers)."""


def _write_destination_put_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    s.set_cap(1, caps.export_cap(v["body"]))
    _write_options_codec.write(s.init_struct(2, 2, 3), v["options"], caps)


def _read_destination_put_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "body": caps.import_cap(s.get_cap_index(1)),
        "options": _write_options_codec.read(s.get_struct(2, 2, 3), caps),
    }


_destination_put_params_codec = _Codec(0, 3, _write_destination_put_params, _read_destination_put_params)
"""Wire codec for ``DestinationPutParams`` (0 data words, 3 pointers)."""


def _write_destination_put_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _put_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_put_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _put_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_put_results_codec = _Codec(0, 1, _write_destination_put_results, _read_destination_put_results)
"""Wire codec for ``DestinationPutResults`` (0 data words, 1 pointers)."""


def _write_destination_copy_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["from"])
    s.set_text(1, v["to"])


def _read_destination_copy_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "from": s.get_text(0),
        "to": s.get_text(1),
    }


_destination_copy_params_codec = _Codec(0, 2, _write_destination_copy_params, _read_destination_copy_params)
"""Wire codec for ``DestinationCopyParams`` (0 data words, 2 pointers)."""


def _write_destination_copy_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _copy_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_copy_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _copy_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_copy_results_codec = _Codec(0, 1, _write_destination_copy_results, _read_destination_copy_results)
"""Wire codec for ``DestinationCopyResults`` (0 data words, 1 pointers)."""


def _write_destination_delete_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])


def _read_destination_delete_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
    }


_destination_delete_params_codec = _Codec(0, 1, _write_destination_delete_params, _read_destination_delete_params)
"""Wire codec for ``DestinationDeleteParams`` (0 data words, 1 pointers)."""


def _write_destination_delete_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_delete_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_delete_results_codec = _Codec(0, 1, _write_destination_delete_results, _read_destination_delete_results)
"""Wire codec for ``DestinationDeleteResults`` (0 data words, 1 pointers)."""


def _write_destination_commit_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    s.set_text(1, v["commitToken"])


def _read_destination_commit_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "commitToken": s.get_text(1),
    }


_destination_commit_params_codec = _Codec(0, 2, _write_destination_commit_params, _read_destination_commit_params)
"""Wire codec for ``DestinationCommitParams`` (0 data words, 2 pointers)."""


def _write_destination_commit_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _put_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_commit_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _put_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_commit_results_codec = _Codec(0, 1, _write_destination_commit_results, _read_destination_commit_results)
"""Wire codec for ``DestinationCommitResults`` (0 data words, 1 pointers)."""


def _write_destination_abort_stage_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])
    s.set_text(1, v["commitToken"])


def _read_destination_abort_stage_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
        "commitToken": s.get_text(1),
    }


_destination_abort_stage_params_codec = _Codec(0, 2, _write_destination_abort_stage_params, _read_destination_abort_stage_params)
"""Wire codec for ``DestinationAbortStageParams`` (0 data words, 2 pointers)."""


def _write_destination_abort_stage_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_destination_abort_stage_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_destination_abort_stage_results_codec = _Codec(0, 1, _write_destination_abort_stage_results, _read_destination_abort_stage_results)
"""Wire codec for ``DestinationAbortStageResults`` (0 data words, 1 pointers)."""


def _write_source_open_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["key"])


def _read_source_open_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "key": s.get_text(0),
    }


_source_open_params_codec = _Codec(0, 1, _write_source_open_params, _read_source_open_params)
"""Wire codec for ``SourceOpenParams`` (0 data words, 1 pointers)."""


def _write_source_open_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _open_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_source_open_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _open_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_source_open_results_codec = _Codec(0, 1, _write_source_open_results, _read_source_open_results)
"""Wire codec for ``SourceOpenResults`` (0 data words, 1 pointers)."""


def _write_progress_sink_report_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_f32(0, v["percent"])
    s.set_text(0, v["message"])


def _read_progress_sink_report_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "percent": s.get_f32(0),
        "message": s.get_text(0),
    }


_progress_sink_report_params_codec = _Codec(1, 1, _write_progress_sink_report_params, _read_progress_sink_report_params)
"""Wire codec for ``ProgressSinkReportParams`` (1 data words, 1 pointers)."""


def _write_progress_sink_report_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_progress_sink_report_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_progress_sink_report_results_codec = _Codec(0, 1, _write_progress_sink_report_results, _read_progress_sink_report_results)
"""Wire codec for ``ProgressSinkReportResults`` (0 data words, 1 pointers)."""


def _write_cancellation_poll_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_cancellation_poll_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_cancellation_poll_params_codec = _Codec(0, 0, _write_cancellation_poll_params, _read_cancellation_poll_params)
"""Wire codec for ``CancellationPollParams`` (0 data words, 0 pointers)."""


def _write_cancellation_poll_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_bool(0, v["cancelled"])


def _read_cancellation_poll_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "cancelled": s.get_bool(0),
    }


_cancellation_poll_results_codec = _Codec(1, 0, _write_cancellation_poll_results, _read_cancellation_poll_results)
"""Wire codec for ``CancellationPollResults`` (1 data words, 0 pointers)."""


def _write_job_runner_job_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _job_controller_codec.write(s.init_struct(0, 0, 5), v["controller"], caps)


def _read_job_runner_job_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "controller": _job_controller_codec.read(s.get_struct(0, 0, 5), caps),
    }


_job_runner_job_params_codec = _Codec(0, 1, _write_job_runner_job_params, _read_job_runner_job_params)
"""Wire codec for ``JobRunnerJobParams`` (0 data words, 1 pointers)."""


def _write_job_runner_job_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _handle_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_job_runner_job_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _handle_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_job_runner_job_results_codec = _Codec(0, 1, _write_job_runner_job_results, _read_job_runner_job_results)
"""Wire codec for ``JobRunnerJobResults`` (0 data words, 1 pointers)."""


def _write_event_consumer_event_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _event_batch_codec.write(s.init_struct(0, 0, 1), v["batch"], caps)


def _read_event_consumer_event_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "batch": _event_batch_codec.read(s.get_struct(0, 0, 1), caps),
    }


_event_consumer_event_params_codec = _Codec(0, 1, _write_event_consumer_event_params, _read_event_consumer_event_params)
"""Wire codec for ``EventConsumerEventParams`` (0 data words, 1 pointers)."""


def _write_event_consumer_event_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _event_batch_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_event_consumer_event_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _event_batch_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_event_consumer_event_results_codec = _Codec(0, 1, _write_event_consumer_event_results, _read_event_consumer_event_results)
"""Wire codec for ``EventConsumerEventResults`` (0 data words, 1 pointers)."""


def _write_event_publisher_publish_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _plugin_event_codec.write(s.init_struct(0, 2, 5), v["event"], caps)


def _read_event_publisher_publish_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "event": _plugin_event_codec.read(s.get_struct(0, 2, 5), caps),
    }


_event_publisher_publish_params_codec = _Codec(0, 1, _write_event_publisher_publish_params, _read_event_publisher_publish_params)
"""Wire codec for ``EventPublisherPublishParams`` (0 data words, 1 pointers)."""


def _write_event_publisher_publish_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _publish_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_event_publisher_publish_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _publish_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_event_publisher_publish_results_codec = _Codec(0, 1, _write_event_publisher_publish_results, _read_event_publisher_publish_results)
"""Wire codec for ``EventPublisherPublishResults`` (0 data words, 1 pointers)."""


def _write_content_source_login_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _login_params_codec.write(s.init_struct(0, 2, 10), v["params"], caps)


def _read_content_source_login_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _login_params_codec.read(s.get_struct(0, 2, 10), caps),
    }


_content_source_login_params_codec = _Codec(0, 1, _write_content_source_login_params, _read_content_source_login_params)
"""Wire codec for ``ContentSourceLoginParams`` (0 data words, 1 pointers)."""


def _write_content_source_login_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _login_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_login_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _login_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_login_results_codec = _Codec(0, 1, _write_content_source_login_results, _read_content_source_login_results)
"""Wire codec for ``ContentSourceLoginResults`` (0 data words, 1 pointers)."""


def _write_content_source_scan_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _scan_params_codec.write(s.init_struct(0, 1, 3), v["params"], caps)


def _read_content_source_scan_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _scan_params_codec.read(s.get_struct(0, 1, 3), caps),
    }


_content_source_scan_params_codec = _Codec(0, 1, _write_content_source_scan_params, _read_content_source_scan_params)
"""Wire codec for ``ContentSourceScanParams`` (0 data words, 1 pointers)."""


def _write_content_source_scan_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _scan_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_scan_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _scan_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_scan_results_codec = _Codec(0, 1, _write_content_source_scan_results, _read_content_source_scan_results)
"""Wire codec for ``ContentSourceScanResults`` (0 data words, 1 pointers)."""


def _write_content_source_fetch_title_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _fetch_title_params_codec.write(s.init_struct(0, 0, 7), v["params"], caps)


def _read_content_source_fetch_title_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _fetch_title_params_codec.read(s.get_struct(0, 0, 7), caps),
    }


_content_source_fetch_title_params_codec = _Codec(0, 1, _write_content_source_fetch_title_params, _read_content_source_fetch_title_params)
"""Wire codec for ``ContentSourceFetchTitleParams`` (0 data words, 1 pointers)."""


def _write_content_source_fetch_title_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _fetch_title_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_fetch_title_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _fetch_title_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_fetch_title_results_codec = _Codec(0, 1, _write_content_source_fetch_title_results, _read_content_source_fetch_title_results)
"""Wire codec for ``ContentSourceFetchTitleResults`` (0 data words, 1 pointers)."""


def _write_content_source_list_accounts_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_content_source_list_accounts_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_content_source_list_accounts_params_codec = _Codec(0, 0, _write_content_source_list_accounts_params, _read_content_source_list_accounts_params)
"""Wire codec for ``ContentSourceListAccountsParams`` (0 data words, 0 pointers)."""


def _write_content_source_list_accounts_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _source_accounts_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_list_accounts_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _source_accounts_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_list_accounts_results_codec = _Codec(0, 1, _write_content_source_list_accounts_results, _read_content_source_list_accounts_results)
"""Wire codec for ``ContentSourceListAccountsResults`` (0 data words, 1 pointers)."""


def _write_content_source_login_start_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _login_params_codec.write(s.init_struct(0, 2, 10), v["params"], caps)


def _read_content_source_login_start_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _login_params_codec.read(s.get_struct(0, 2, 10), caps),
    }


_content_source_login_start_params_codec = _Codec(0, 1, _write_content_source_login_start_params, _read_content_source_login_start_params)
"""Wire codec for ``ContentSourceLoginStartParams`` (0 data words, 1 pointers)."""


def _write_content_source_login_start_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _login_start_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_login_start_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _login_start_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_login_start_results_codec = _Codec(0, 1, _write_content_source_login_start_results, _read_content_source_login_start_results)
"""Wire codec for ``ContentSourceLoginStartResults`` (0 data words, 1 pointers)."""


def _write_content_source_login_complete_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _login_complete_params_codec.write(s.init_struct(0, 0, 1), v["params"], caps)


def _read_content_source_login_complete_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _login_complete_params_codec.read(s.get_struct(0, 0, 1), caps),
    }


_content_source_login_complete_params_codec = _Codec(0, 1, _write_content_source_login_complete_params, _read_content_source_login_complete_params)
"""Wire codec for ``ContentSourceLoginCompleteParams`` (0 data words, 1 pointers)."""


def _write_content_source_login_complete_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _login_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_login_complete_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _login_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_login_complete_results_codec = _Codec(0, 1, _write_content_source_login_complete_results, _read_content_source_login_complete_results)
"""Wire codec for ``ContentSourceLoginCompleteResults`` (0 data words, 1 pointers)."""


def _write_content_source_search_catalog_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _search_catalog_params_codec.write(s.init_struct(0, 2, 3), v["params"], caps)


def _read_content_source_search_catalog_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _search_catalog_params_codec.read(s.get_struct(0, 2, 3), caps),
    }


_content_source_search_catalog_params_codec = _Codec(0, 1, _write_content_source_search_catalog_params, _read_content_source_search_catalog_params)
"""Wire codec for ``ContentSourceSearchCatalogParams`` (0 data words, 1 pointers)."""


def _write_content_source_search_catalog_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _catalog_hits_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_search_catalog_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _catalog_hits_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_search_catalog_results_codec = _Codec(0, 1, _write_content_source_search_catalog_results, _read_content_source_search_catalog_results)
"""Wire codec for ``ContentSourceSearchCatalogResults`` (0 data words, 1 pointers)."""


def _write_content_source_expand_candidates_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _expand_candidates_params_codec.write(s.init_struct(0, 1, 10), v["params"], caps)


def _read_content_source_expand_candidates_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _expand_candidates_params_codec.read(s.get_struct(0, 1, 10), caps),
    }


_content_source_expand_candidates_params_codec = _Codec(0, 1, _write_content_source_expand_candidates_params, _read_content_source_expand_candidates_params)
"""Wire codec for ``ContentSourceExpandCandidatesParams`` (0 data words, 1 pointers)."""


def _write_content_source_expand_candidates_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _catalog_hits_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_expand_candidates_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _catalog_hits_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_expand_candidates_results_codec = _Codec(0, 1, _write_content_source_expand_candidates_results, _read_content_source_expand_candidates_results)
"""Wire codec for ``ContentSourceExpandCandidatesResults`` (0 data words, 1 pointers)."""


def _write_content_source_purchase_hint_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _purchase_hint_params_codec.write(s.init_struct(0, 1, 6), v["params"], caps)


def _read_content_source_purchase_hint_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _purchase_hint_params_codec.read(s.get_struct(0, 1, 6), caps),
    }


_content_source_purchase_hint_params_codec = _Codec(0, 1, _write_content_source_purchase_hint_params, _read_content_source_purchase_hint_params)
"""Wire codec for ``ContentSourcePurchaseHintParams`` (0 data words, 1 pointers)."""


def _write_content_source_purchase_hint_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _purchase_hint_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_purchase_hint_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _purchase_hint_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_purchase_hint_results_codec = _Codec(0, 1, _write_content_source_purchase_hint_results, _read_content_source_purchase_hint_results)
"""Wire codec for ``ContentSourcePurchaseHintResults`` (0 data words, 1 pointers)."""


def _write_content_source_list_deals_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _list_deals_params_codec.write(s.init_struct(0, 1, 0), v["params"], caps)


def _read_content_source_list_deals_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _list_deals_params_codec.read(s.get_struct(0, 1, 0), caps),
    }


_content_source_list_deals_params_codec = _Codec(0, 1, _write_content_source_list_deals_params, _read_content_source_list_deals_params)
"""Wire codec for ``ContentSourceListDealsParams`` (0 data words, 1 pointers)."""


def _write_content_source_list_deals_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _catalog_hits_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_list_deals_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _catalog_hits_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_list_deals_results_codec = _Codec(0, 1, _write_content_source_list_deals_results, _read_content_source_list_deals_results)
"""Wire codec for ``ContentSourceListDealsResults`` (0 data words, 1 pointers)."""


def _write_content_source_health_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_content_source_health_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_content_source_health_params_codec = _Codec(0, 0, _write_content_source_health_params, _read_content_source_health_params)
"""Wire codec for ``ContentSourceHealthParams`` (0 data words, 0 pointers)."""


def _write_content_source_health_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _health_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_health_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _health_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_health_results_codec = _Codec(0, 1, _write_content_source_health_results, _read_content_source_health_results)
"""Wire codec for ``ContentSourceHealthResults`` (0 data words, 1 pointers)."""


def _write_content_source_diagnose_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_content_source_diagnose_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_content_source_diagnose_params_codec = _Codec(0, 0, _write_content_source_diagnose_params, _read_content_source_diagnose_params)
"""Wire codec for ``ContentSourceDiagnoseParams`` (0 data words, 0 pointers)."""


def _write_content_source_diagnose_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _diagnose_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_diagnose_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _diagnose_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_diagnose_results_codec = _Codec(0, 1, _write_content_source_diagnose_results, _read_content_source_diagnose_results)
"""Wire codec for ``ContentSourceDiagnoseResults`` (0 data words, 1 pointers)."""


def _write_content_source_catalog_detail_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _catalog_detail_params_codec.write(s.init_struct(0, 0, 2), v["params"], caps)


def _read_content_source_catalog_detail_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _catalog_detail_params_codec.read(s.get_struct(0, 0, 2), caps),
    }


_content_source_catalog_detail_params_codec = _Codec(0, 1, _write_content_source_catalog_detail_params, _read_content_source_catalog_detail_params)
"""Wire codec for ``ContentSourceCatalogDetailParams`` (0 data words, 1 pointers)."""


def _write_content_source_catalog_detail_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _catalog_detail_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_content_source_catalog_detail_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _catalog_detail_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_content_source_catalog_detail_results_codec = _Codec(0, 1, _write_content_source_catalog_detail_results, _read_content_source_catalog_detail_results)
"""Wire codec for ``ContentSourceCatalogDetailResults`` (0 data words, 1 pointers)."""


def _write_remote_library_health_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_remote_library_health_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_remote_library_health_params_codec = _Codec(0, 0, _write_remote_library_health_params, _read_remote_library_health_params)
"""Wire codec for ``RemoteLibraryHealthParams`` (0 data words, 0 pointers)."""


def _write_remote_library_health_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _health_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_remote_library_health_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _health_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_remote_library_health_results_codec = _Codec(0, 1, _write_remote_library_health_results, _read_remote_library_health_results)
"""Wire codec for ``RemoteLibraryHealthResults`` (0 data words, 1 pointers)."""


def _write_remote_library_start_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_remote_library_start_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_remote_library_start_params_codec = _Codec(0, 0, _write_remote_library_start_params, _read_remote_library_start_params)
"""Wire codec for ``RemoteLibraryStartParams`` (0 data words, 0 pointers)."""


def _write_remote_library_start_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_remote_library_start_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_remote_library_start_results_codec = _Codec(0, 1, _write_remote_library_start_results, _read_remote_library_start_results)
"""Wire codec for ``RemoteLibraryStartResults`` (0 data words, 1 pointers)."""


def _write_remote_library_stop_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_remote_library_stop_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_remote_library_stop_params_codec = _Codec(0, 0, _write_remote_library_stop_params, _read_remote_library_stop_params)
"""Wire codec for ``RemoteLibraryStopParams`` (0 data words, 0 pointers)."""


def _write_remote_library_stop_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_remote_library_stop_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_remote_library_stop_results_codec = _Codec(0, 1, _write_remote_library_stop_results, _read_remote_library_stop_results)
"""Wire codec for ``RemoteLibraryStopResults`` (0 data words, 1 pointers)."""


def _write_remote_library_diagnose_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_remote_library_diagnose_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_remote_library_diagnose_params_codec = _Codec(0, 0, _write_remote_library_diagnose_params, _read_remote_library_diagnose_params)
"""Wire codec for ``RemoteLibraryDiagnoseParams`` (0 data words, 0 pointers)."""


def _write_remote_library_diagnose_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _diagnose_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_remote_library_diagnose_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _diagnose_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_remote_library_diagnose_results_codec = _Codec(0, 1, _write_remote_library_diagnose_results, _read_remote_library_diagnose_results)
"""Wire codec for ``RemoteLibraryDiagnoseResults`` (0 data words, 1 pointers)."""


def _write_remote_library_scan_library_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _scan_library_params_codec.write(s.init_struct(0, 1, 0), v["params"], caps)


def _read_remote_library_scan_library_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _scan_library_params_codec.read(s.get_struct(0, 1, 0), caps),
    }


_remote_library_scan_library_params_codec = _Codec(0, 1, _write_remote_library_scan_library_params, _read_remote_library_scan_library_params)
"""Wire codec for ``RemoteLibraryScanLibraryParams`` (0 data words, 1 pointers)."""


def _write_remote_library_scan_library_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_remote_library_scan_library_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_remote_library_scan_library_results_codec = _Codec(0, 1, _write_remote_library_scan_library_results, _read_remote_library_scan_library_results)
"""Wire codec for ``RemoteLibraryScanLibraryResults`` (0 data words, 1 pointers)."""


def _write_remote_library_sync_listening_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_remote_library_sync_listening_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_remote_library_sync_listening_params_codec = _Codec(0, 0, _write_remote_library_sync_listening_params, _read_remote_library_sync_listening_params)
"""Wire codec for ``RemoteLibrarySyncListeningParams`` (0 data words, 0 pointers)."""


def _write_remote_library_sync_listening_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _sync_listening_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_remote_library_sync_listening_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _sync_listening_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_remote_library_sync_listening_results_codec = _Codec(0, 1, _write_remote_library_sync_listening_results, _read_remote_library_sync_listening_results)
"""Wire codec for ``RemoteLibrarySyncListeningResults`` (0 data words, 1 pointers)."""


def _write_remote_library_poll_events_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_remote_library_poll_events_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_remote_library_poll_events_params_codec = _Codec(0, 0, _write_remote_library_poll_events_params, _read_remote_library_poll_events_params)
"""Wire codec for ``RemoteLibraryPollEventsParams`` (0 data words, 0 pointers)."""


def _write_remote_library_poll_events_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _event_poll_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_remote_library_poll_events_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _event_poll_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_remote_library_poll_events_results_codec = _Codec(0, 1, _write_remote_library_poll_events_results, _read_remote_library_poll_events_results)
"""Wire codec for ``RemoteLibraryPollEventsResults`` (0 data words, 1 pointers)."""


def _write_plugin_cli_describe_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_plugin_cli_describe_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_plugin_cli_describe_params_codec = _Codec(0, 0, _write_plugin_cli_describe_params, _read_plugin_cli_describe_params)
"""Wire codec for ``PluginCliDescribeParams`` (0 data words, 0 pointers)."""


def _write_plugin_cli_describe_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _cli_schema_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_plugin_cli_describe_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _cli_schema_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_plugin_cli_describe_results_codec = _Codec(0, 1, _write_plugin_cli_describe_results, _read_plugin_cli_describe_results)
"""Wire codec for ``PluginCliDescribeResults`` (0 data words, 1 pointers)."""


def _write_plugin_cli_invoke_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _cli_invoke_params_codec.write(s.init_struct(0, 0, 2), v["params"], caps)


def _read_plugin_cli_invoke_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _cli_invoke_params_codec.read(s.get_struct(0, 0, 2), caps),
    }


_plugin_cli_invoke_params_codec = _Codec(0, 1, _write_plugin_cli_invoke_params, _read_plugin_cli_invoke_params)
"""Wire codec for ``PluginCliInvokeParams`` (0 data words, 1 pointers)."""


def _write_plugin_cli_invoke_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _cli_invoke_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_plugin_cli_invoke_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _cli_invoke_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_plugin_cli_invoke_results_codec = _Codec(0, 1, _write_plugin_cli_invoke_results, _read_plugin_cli_invoke_results)
"""Wire codec for ``PluginCliInvokeResults`` (0 data words, 1 pointers)."""


def _write_oidc_clients_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_oidc_clients_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_oidc_clients_params_codec = _Codec(0, 0, _write_oidc_clients_params, _read_oidc_clients_params)
"""Wire codec for ``OidcClientsParams`` (0 data words, 0 pointers)."""


def _write_oidc_clients_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _oidc_clients_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_oidc_clients_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _oidc_clients_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_oidc_clients_results_codec = _Codec(0, 1, _write_oidc_clients_results, _read_oidc_clients_results)
"""Wire codec for ``OidcClientsResults`` (0 data words, 1 pointers)."""


def _write_oidc_authenticate_user_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _authenticate_user_params_codec.write(s.init_struct(0, 0, 2), v["params"], caps)


def _read_oidc_authenticate_user_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "params": _authenticate_user_params_codec.read(s.get_struct(0, 0, 2), caps),
    }


_oidc_authenticate_user_params_codec = _Codec(0, 1, _write_oidc_authenticate_user_params, _read_oidc_authenticate_user_params)
"""Wire codec for ``OidcAuthenticateUserParams`` (0 data words, 1 pointers)."""


def _write_oidc_authenticate_user_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _external_user_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_oidc_authenticate_user_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _external_user_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_oidc_authenticate_user_results_codec = _Codec(0, 1, _write_oidc_authenticate_user_results, _read_oidc_authenticate_user_results)
"""Wire codec for ``OidcAuthenticateUserResults`` (0 data words, 1 pointers)."""


def _write_database_open_session_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_database_open_session_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_database_open_session_params_codec = _Codec(0, 0, _write_database_open_session_params, _read_database_open_session_params)
"""Wire codec for ``DatabaseOpenSessionParams`` (0 data words, 0 pointers)."""


def _write_database_open_session_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _adapter_session_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_database_open_session_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _adapter_session_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_database_open_session_results_codec = _Codec(0, 1, _write_database_open_session_results, _read_database_open_session_results)
"""Wire codec for ``DatabaseOpenSessionResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_capabilities_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_adapter_database_session_capabilities_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_adapter_database_session_capabilities_params_codec = _Codec(0, 0, _write_adapter_database_session_capabilities_params, _read_adapter_database_session_capabilities_params)
"""Wire codec for ``AdapterDatabaseSessionCapabilitiesParams`` (0 data words, 0 pointers)."""


def _write_adapter_database_session_capabilities_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _db_capabilities_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_capabilities_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _db_capabilities_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_capabilities_results_codec = _Codec(0, 1, _write_adapter_database_session_capabilities_results, _read_adapter_database_session_capabilities_results)
"""Wire codec for ``AdapterDatabaseSessionCapabilitiesResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_execute_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _adapter_execute_request_codec.write(s.init_struct(0, 2, 4), v["request"], caps)


def _read_adapter_database_session_execute_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "request": _adapter_execute_request_codec.read(s.get_struct(0, 2, 4), caps),
    }


_adapter_database_session_execute_params_codec = _Codec(0, 1, _write_adapter_database_session_execute_params, _read_adapter_database_session_execute_params)
"""Wire codec for ``AdapterDatabaseSessionExecuteParams`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_execute_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _execute_result_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_execute_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _execute_result_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_execute_results_codec = _Codec(0, 1, _write_adapter_database_session_execute_results, _read_adapter_database_session_execute_results)
"""Wire codec for ``AdapterDatabaseSessionExecuteResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_close_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_adapter_database_session_close_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_adapter_database_session_close_params_codec = _Codec(0, 0, _write_adapter_database_session_close_params, _read_adapter_database_session_close_params)
"""Wire codec for ``AdapterDatabaseSessionCloseParams`` (0 data words, 0 pointers)."""


def _write_adapter_database_session_close_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_close_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_close_results_codec = _Codec(0, 1, _write_adapter_database_session_close_results, _read_adapter_database_session_close_results)
"""Wire codec for ``AdapterDatabaseSessionCloseResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_bootstrap_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_adapter_database_session_bootstrap_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_adapter_database_session_bootstrap_params_codec = _Codec(0, 0, _write_adapter_database_session_bootstrap_params, _read_adapter_database_session_bootstrap_params)
"""Wire codec for ``AdapterDatabaseSessionBootstrapParams`` (0 data words, 0 pointers)."""


def _write_adapter_database_session_bootstrap_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _db_bootstrap_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_bootstrap_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _db_bootstrap_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_bootstrap_results_codec = _Codec(0, 1, _write_adapter_database_session_bootstrap_results, _read_adapter_database_session_bootstrap_results)
"""Wire codec for ``AdapterDatabaseSessionBootstrapResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_export_identity_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_adapter_database_session_export_identity_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_adapter_database_session_export_identity_params_codec = _Codec(0, 0, _write_adapter_database_session_export_identity_params, _read_adapter_database_session_export_identity_params)
"""Wire codec for ``AdapterDatabaseSessionExportIdentityParams`` (0 data words, 0 pointers)."""


def _write_adapter_database_session_export_identity_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _identity_export_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_export_identity_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _identity_export_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_export_identity_results_codec = _Codec(0, 1, _write_adapter_database_session_export_identity_results, _read_adapter_database_session_export_identity_results)
"""Wire codec for ``AdapterDatabaseSessionExportIdentityResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_import_identity_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    items = s.init_struct_list(0, len(v["rows"]), 1, 1)
    for item, elem in zip(items, v["rows"], strict=True):
        _identity_high_water_codec.write(item, elem, caps)


def _read_adapter_database_session_import_identity_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "rows": [_identity_high_water_codec.read(item, caps) for item in s.get_struct_list(0, 1, 1)],
    }


_adapter_database_session_import_identity_params_codec = _Codec(0, 1, _write_adapter_database_session_import_identity_params, _read_adapter_database_session_import_identity_params)
"""Wire codec for ``AdapterDatabaseSessionImportIdentityParams`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_import_identity_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_import_identity_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_import_identity_results_codec = _Codec(0, 1, _write_adapter_database_session_import_identity_results, _read_adapter_database_session_import_identity_results)
"""Wire codec for ``AdapterDatabaseSessionImportIdentityResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_list_user_relations_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_adapter_database_session_list_user_relations_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_adapter_database_session_list_user_relations_params_codec = _Codec(0, 0, _write_adapter_database_session_list_user_relations_params, _read_adapter_database_session_list_user_relations_params)
"""Wire codec for ``AdapterDatabaseSessionListUserRelationsParams`` (0 data words, 0 pointers)."""


def _write_adapter_database_session_list_user_relations_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _user_relations_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_list_user_relations_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _user_relations_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_list_user_relations_results_codec = _Codec(0, 1, _write_adapter_database_session_list_user_relations_results, _read_adapter_database_session_list_user_relations_results)
"""Wire codec for ``AdapterDatabaseSessionListUserRelationsResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_prepare_unit_restore_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_adapter_database_session_prepare_unit_restore_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_adapter_database_session_prepare_unit_restore_params_codec = _Codec(0, 0, _write_adapter_database_session_prepare_unit_restore_params, _read_adapter_database_session_prepare_unit_restore_params)
"""Wire codec for ``AdapterDatabaseSessionPrepareUnitRestoreParams`` (0 data words, 0 pointers)."""


def _write_adapter_database_session_prepare_unit_restore_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_prepare_unit_restore_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_prepare_unit_restore_results_codec = _Codec(0, 1, _write_adapter_database_session_prepare_unit_restore_results, _read_adapter_database_session_prepare_unit_restore_results)
"""Wire codec for ``AdapterDatabaseSessionPrepareUnitRestoreResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_drop_user_relations_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text_list(0, v["names"])


def _read_adapter_database_session_drop_user_relations_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "names": s.get_text_list(0),
    }


_adapter_database_session_drop_user_relations_params_codec = _Codec(0, 1, _write_adapter_database_session_drop_user_relations_params, _read_adapter_database_session_drop_user_relations_params)
"""Wire codec for ``AdapterDatabaseSessionDropUserRelationsParams`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_drop_user_relations_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_drop_user_relations_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_drop_user_relations_results_codec = _Codec(0, 1, _write_adapter_database_session_drop_user_relations_results, _read_adapter_database_session_drop_user_relations_results)
"""Wire codec for ``AdapterDatabaseSessionDropUserRelationsResults`` (0 data words, 1 pointers)."""


def _write_adapter_database_session_assert_restore_constraints_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_adapter_database_session_assert_restore_constraints_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_adapter_database_session_assert_restore_constraints_params_codec = _Codec(0, 0, _write_adapter_database_session_assert_restore_constraints_params, _read_adapter_database_session_assert_restore_constraints_params)
"""Wire codec for ``AdapterDatabaseSessionAssertRestoreConstraintsParams`` (0 data words, 0 pointers)."""


def _write_adapter_database_session_assert_restore_constraints_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_adapter_database_session_assert_restore_constraints_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_adapter_database_session_assert_restore_constraints_results_codec = _Codec(0, 1, _write_adapter_database_session_assert_restore_constraints_results, _read_adapter_database_session_assert_restore_constraints_results)
"""Wire codec for ``AdapterDatabaseSessionAssertRestoreConstraintsResults`` (0 data words, 1 pointers)."""


def _write_guest_database_execute_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _execute_request_codec.write(s.init_struct(0, 1, 3), v["request"], caps)


def _read_guest_database_execute_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "request": _execute_request_codec.read(s.get_struct(0, 1, 3), caps),
    }


_guest_database_execute_params_codec = _Codec(0, 1, _write_guest_database_execute_params, _read_guest_database_execute_params)
"""Wire codec for ``GuestDatabaseExecuteParams`` (0 data words, 1 pointers)."""


def _write_guest_database_execute_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _execute_result_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_guest_database_execute_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _execute_result_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_guest_database_execute_results_codec = _Codec(0, 1, _write_guest_database_execute_results, _read_guest_database_execute_results)
"""Wire codec for ``GuestDatabaseExecuteResults`` (0 data words, 1 pointers)."""


def _write_guest_database_close_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_guest_database_close_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_guest_database_close_params_codec = _Codec(0, 0, _write_guest_database_close_params, _read_guest_database_close_params)
"""Wire codec for ``GuestDatabaseCloseParams`` (0 data words, 0 pointers)."""


def _write_guest_database_close_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_guest_database_close_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_guest_database_close_results_codec = _Codec(0, 1, _write_guest_database_close_results, _read_guest_database_close_results)
"""Wire codec for ``GuestDatabaseCloseResults`` (0 data words, 1 pointers)."""


def _write_plugin_worker_describe_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_plugin_worker_describe_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_plugin_worker_describe_params_codec = _Codec(0, 0, _write_plugin_worker_describe_params, _read_plugin_worker_describe_params)
"""Wire codec for ``PluginWorkerDescribeParams`` (0 data words, 0 pointers)."""


def _write_plugin_worker_describe_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _describe_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_plugin_worker_describe_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _describe_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_plugin_worker_describe_results_codec = _Codec(0, 1, _write_plugin_worker_describe_results, _read_plugin_worker_describe_results)
"""Wire codec for ``PluginWorkerDescribeResults`` (0 data words, 1 pointers)."""


def _write_plugin_worker_open_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _invocation_codec.write(s.init_struct(0, 1, 4), v["invocation"], caps)
    _bindings_codec.write(s.init_struct(1, 0, 7), v["bindings"], caps)


def _read_plugin_worker_open_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "invocation": _invocation_codec.read(s.get_struct(0, 1, 4), caps),
        "bindings": _bindings_codec.read(s.get_struct(1, 0, 7), caps),
    }


_plugin_worker_open_params_codec = _Codec(0, 2, _write_plugin_worker_open_params, _read_plugin_worker_open_params)
"""Wire codec for ``PluginWorkerOpenParams`` (0 data words, 2 pointers)."""


def _write_plugin_worker_open_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _entrypoints_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_plugin_worker_open_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _entrypoints_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_plugin_worker_open_results_codec = _Codec(0, 1, _write_plugin_worker_open_results, _read_plugin_worker_open_results)
"""Wire codec for ``PluginWorkerOpenResults`` (0 data words, 1 pointers)."""


def _write_plugin_worker_shutdown_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    pass


def _read_plugin_worker_shutdown_params(s: _StructReader, caps: _CapTable) -> Any:
    return {}


_plugin_worker_shutdown_params_codec = _Codec(0, 0, _write_plugin_worker_shutdown_params, _read_plugin_worker_shutdown_params)
"""Wire codec for ``PluginWorkerShutdownParams`` (0 data words, 0 pointers)."""


def _write_plugin_worker_shutdown_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _empty_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_plugin_worker_shutdown_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _empty_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_plugin_worker_shutdown_results_codec = _Codec(0, 1, _write_plugin_worker_shutdown_results, _read_plugin_worker_shutdown_results)
"""Wire codec for ``PluginWorkerShutdownResults`` (0 data words, 1 pointers)."""


def _write_plugin_worker_database_migrations_params(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    s.set_text(0, v["binding"])


def _read_plugin_worker_database_migrations_params(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "binding": s.get_text(0),
    }


_plugin_worker_database_migrations_params_codec = _Codec(0, 1, _write_plugin_worker_database_migrations_params, _read_plugin_worker_database_migrations_params)
"""Wire codec for ``PluginWorkerDatabaseMigrationsParams`` (0 data words, 1 pointers)."""


def _write_plugin_worker_database_migrations_results(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:
    _plugin_migrations_reply_codec.write(s.init_struct(0, 1, 1), v["result"], caps)


def _read_plugin_worker_database_migrations_results(s: _StructReader, caps: _CapTable) -> Any:
    return {
        "result": _plugin_migrations_reply_codec.read(s.get_struct(0, 1, 1), caps),
    }


_plugin_worker_database_migrations_results_codec = _Codec(0, 1, _write_plugin_worker_database_migrations_results, _read_plugin_worker_database_migrations_results)
"""Wire codec for ``PluginWorkerDatabaseMigrationsResults`` (0 data words, 1 pointers)."""
