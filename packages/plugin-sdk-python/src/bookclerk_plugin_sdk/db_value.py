"""Universal Cap'n database value domain (``DbValue``) and unpacked codec.

Baseline cells are typed null, bool, int64 (``int``), finite float64, UTF-8
text, and bytes (``bytes``). U+0000 is forbidden in TEXT and allowed in
BYTES. Unknown ``kind`` values fail closed. JSON
:func:`parse_db_value` still accepts a ``b64:`` string for bytes; the codec
always uses the domain types.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import json
import math
import time
import uuid
from collections.abc import AsyncIterable, AsyncIterator, Awaitable, Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Literal, Protocol, TypedDict

from ._abi import (
    DB_RESULT_SELECTIONS,
    DB_STATEMENT_KINDS,
    DB_TYPES,
    MAX_SCALAR_BYTES,
    MAX_STREAM_WINDOW_BYTES,
)
from ._capnp import NO_CAPS, _CapnpMessage, _CapnpReader, _CapnpStruct, _StructReader
from ._wire import (
    _db_capabilities_reply_codec,
    _decode_message,
    _encode_message,
    _execute_stream_frame_codec,
)
from .guest_sql import guest_statement_kind, split_exec_queries
from .limits import LimitExceeded

if TYPE_CHECKING:
    from .abi import DbCapabilities, DbType
    from .workerd import PluginError


def _plugin_error() -> type[PluginError]:
    """The SDK's ``PluginError`` class, imported late to avoid an import cycle.

    Returns:
        ``bookclerk_plugin_sdk.workerd.PluginError``.
    """
    from .workerd import PluginError

    return PluginError


KINDS = frozenset({"null", "boolean", "int64", "float64", "text", "bytes"})
TYPES = frozenset(DB_TYPES)

I64_MIN = -(2**63)
I64_MAX = 2**63 - 1


def _require_portable_text(text: str) -> str:
    """Reject U+0000 in BookclerkSQL TEXT (allowed only in BYTES)."""
    if "\x00" in text:
        raise ValueError("BookclerkSQL TEXT cannot contain U+0000 (use BYTES/BLOB for binary)")
    return text


# Ordinal tables come from the generated ``_abi`` projection of
# ``schema/plugin.capnp`` (index = Cap'n Proto ordinal).
_DB_TYPE_FROM = DB_TYPES
_DB_TYPE_ORD = {name: ord_ for ord_, name in enumerate(DB_TYPES)}

_KIND_FROM = DB_STATEMENT_KINDS
_KIND_ORD = {name: ord_ for ord_, name in enumerate(DB_STATEMENT_KINDS)}
_SELECT_FROM = DB_RESULT_SELECTIONS
_SELECT_ORD = {name: ord_ for ord_, name in enumerate(DB_RESULT_SELECTIONS)}


class NullValue(TypedDict):
    """Typed SQL NULL with a declared ``DbType``."""

    kind: Literal["null"]
    value: DbType


class BoolValue(TypedDict):
    """Boolean cell."""

    kind: Literal["boolean"]
    value: bool


class Int64Value(TypedDict):
    """Signed 64-bit integer cell (arbitrary-precision ``int`` in range)."""

    kind: Literal["int64"]
    value: int


class Float64Value(TypedDict):
    """Finite IEEE-754 float64 cell."""

    kind: Literal["float64"]
    value: float


class TextValue(TypedDict):
    """UTF-8 text cell (U+0000 forbidden; allowed only in ``bytes``)."""

    kind: Literal["text"]
    value: str


class BytesValue(TypedDict):
    """Binary cell as raw ``bytes`` (not a ``b64:`` string)."""

    kind: Literal["bytes"]
    value: bytes


DbValue = NullValue | BoolValue | Int64Value | Float64Value | TextValue | BytesValue

DbStatementKind = Literal["execute", "select", "returning"]
DbResultSelection = Literal["discard", "affectedRows", "rows"]


class TypedDbStatement(TypedDict):
    """One statement in a typed atomic batch."""

    sql: str
    parameters: list[DbValue]
    kind: DbStatementKind
    maxRows: int
    resultSelection: DbResultSelection


class ExecuteRequest(TypedDict):
    """Typed ``execute`` request (adapter wire: statements + transport metadata only)."""

    operationId: str
    requestHash: str
    statements: list[TypedDbStatement]
    deadlineUnixMs: int


class StatementResult(TypedDict):
    """Result of one statement in an atomic batch."""

    rows: list[dict[str, Any]]
    columns: list[dict[str, Any]]
    rowsAffected: int


class DbTiming(TypedDict):
    """Handler/engine timing on ``ExecuteReply``."""

    attemptElapsedUs: int
    dbExecutionUs: int
    dbTimingSource: str


class ExecuteReply(TypedDict):
    """Typed ``execute`` reply."""

    operationId: str
    statements: list[StatementResult]
    timing: DbTiming


def parse_db_value(raw: Any) -> DbValue:
    """Parse a JSON ``DbValue``. Unknown union members raise ``ValueError``.

    ``bytes`` accepts raw ``bytes`` or a ``b64:`` JSON string. ``int64`` is a
    Python ``int`` (not a float).
    """
    if not isinstance(raw, dict) or "kind" not in raw:
        raise ValueError("DbValue must be an object with kind")
    kind = raw["kind"]
    if kind not in KINDS:
        raise ValueError(f"unknown DbValue union member: {kind}")
    value = raw.get("value")
    if kind == "null":
        if value not in TYPES:
            raise ValueError("typed null requires a DbType")
        return {"kind": "null", "value": value}
    if kind == "boolean":
        if not isinstance(value, bool):
            raise ValueError("boolean DbValue requires a bool")
        return {"kind": "boolean", "value": value}
    if kind == "int64":
        if not isinstance(value, int) or isinstance(value, bool):
            raise ValueError("int64 DbValue requires an int")
        if value < I64_MIN or value > I64_MAX:
            raise ValueError("int64 DbValue is out of range")
        return {"kind": "int64", "value": value}
    if kind == "float64":
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            raise ValueError("float64 DbValue requires a number")
        number = float(value)
        if number != number or number in (float("inf"), float("-inf")):
            raise ValueError("float64 value is not finite")
        return {"kind": "float64", "value": number}
    if kind == "text":
        if not isinstance(value, str):
            raise ValueError("text DbValue requires a string")
        _require_portable_text(value)
        return {"kind": "text", "value": value}
    if kind == "bytes":
        return {"kind": "bytes", "value": _parse_bytes(value)}
    raise ValueError(f"unknown DbValue union member: {kind}")


def encode_db_value(value: DbValue) -> bytes:
    """Encode a standalone unpacked Cap'n ``DbValue`` message.

    Args:
        value: Domain value (``int`` / ``bytes``, not a JSON ``b64:`` string).

    Returns:
        Unpacked Cap'n stream bytes (same encoding as the Rust SDK).
    """
    msg = _CapnpMessage()
    root = msg.init_root(2, 1)
    _write_db_value(root, value)
    return msg.finish()


def decode_db_value(data: bytes) -> DbValue:
    """Decode a standalone unpacked Cap'n ``DbValue`` message.

    Args:
        data: Unpacked Cap'n stream.

    Returns:
        Domain value.

    Raises:
        ValueError: If the buffer is not a valid ``DbValue``.
    """
    reader = _CapnpReader(data)
    return _read_db_value(reader.root(2, 1))


def encode_execute_request(request: ExecuteRequest) -> bytes:
    """Encode a standalone unpacked Cap'n ``ExecuteRequest`` message.

    Args:
        request: Structured request (non-empty ``statements``).

    Returns:
        Unpacked Cap'n stream bytes (same encoding as the Rust SDK).

    Raises:
        ValueError: If ``statements`` is empty.
    """
    if not request["statements"]:
        raise ValueError("execute statements must be non-empty")
    msg = _CapnpMessage()
    root = msg.init_root(4, 3)
    root.set_text(0, request["operationId"])
    root.set_text(1, request["requestHash"])
    root.set_u64(3, request["deadlineUnixMs"])
    stmts = root.init_struct_list(2, len(request["statements"]), 1, 2)
    for i, stmt in enumerate(request["statements"]):
        _write_statement(stmts[i], stmt)
    return msg.finish()


def decode_execute_request(data: bytes) -> ExecuteRequest:
    """Decode a standalone unpacked Cap'n ``ExecuteRequest`` message.

    Args:
        data: Unpacked Cap'n stream.

    Returns:
        Structured request.

    Raises:
        ValueError: If the buffer is not a valid non-empty ``ExecuteRequest``.
    """
    reader = _CapnpReader(data)
    root = reader.root(4, 3)
    stmt_structs = root.get_struct_list(2, 1, 2)
    if not stmt_structs:
        raise ValueError("execute statements must be non-empty")
    return {
        "operationId": root.get_text(0),
        "requestHash": root.get_text(1),
        "statements": [_read_statement(s) for s in stmt_structs],
        "deadlineUnixMs": root.get_u64(3),
    }


def encode_execute_result_reply(
    outcome: ExecuteReply | tuple[str, str] | tuple[str, str, str],
) -> bytes:
    """Encode ``ExecuteResultReply``.

    Args:
        outcome: The ``ok`` reply, or an ``err`` as ``(code, message)`` or
            ``(code, message, details_json)``.

    Returns:
        Unpacked Cap'n stream bytes.
    """
    msg = _CapnpMessage()
    root = msg.init_root(1, 1)
    if isinstance(outcome, tuple):
        root.set_u16(0, 1)
        err = root.init_struct(0, 0, 3)
        err.set_text(0, outcome[0])
        err.set_text(1, outcome[1])
        err.set_text(2, outcome[2] if len(outcome) > 2 else "")
    else:
        root.set_u16(0, 0)
        _write_execute_reply(root.init_struct(0, 0, 3), outcome)
    return msg.finish()


def decode_execute_result_reply(data: bytes) -> ExecuteReply:
    """Decode ``ExecuteResultReply``. ``err`` is raised as ``PluginError``."""
    root = _CapnpReader(data).root(1, 1)
    disc = root.get_u16(0)
    if disc == 0:
        return _read_execute_reply(root.get_struct(0, 0, 3))
    if disc == 1:
        err = root.get_struct(0, 0, 3)
        from bookclerk_plugin_sdk.workerd import PluginError

        raise PluginError.from_wire(err.get_text(0), err.get_text(1), err.get_text(2))
    raise ValueError("unknown ExecuteResultReply union member")


def encode_db_capabilities_reply(
    outcome: DbCapabilities | tuple[str, str] | tuple[str, str, str],
) -> bytes:
    """Encode ``DbCapabilitiesReply``, the ``GuestDatabase.limits`` answer.

    Args:
        outcome: The ``ok`` capabilities, or an ``err`` as ``(code, message)``
            or ``(code, message, details_json)``.

    Returns:
        Unpacked Cap'n stream bytes (the ``GET /db/limits`` body).
    """
    if isinstance(outcome, tuple):
        value: dict[str, Any] = {
            "kind": "err",
            "value": {
                "code": outcome[0],
                "message": outcome[1],
                "details": outcome[2] if len(outcome) > 2 else "",
            },
        }
    else:
        value = {"kind": "ok", "value": outcome}
    return _encode_message(_db_capabilities_reply_codec, value)


def decode_db_capabilities_reply(data: bytes) -> DbCapabilities:
    """Decode ``DbCapabilitiesReply``. ``err`` is raised as ``PluginError``.

    Args:
        data: Unpacked Cap'n stream.

    Returns:
        The session's ``DbCapabilities``.

    Raises:
        PluginError: When the union is ``err``.
        ValueError: When the buffer is not a ``DbCapabilitiesReply``.
    """
    reply = _decode_message(_db_capabilities_reply_codec, data)
    if reply["kind"] == "ok":
        caps: DbCapabilities = reply["value"]
        return caps
    from bookclerk_plugin_sdk.workerd import PluginError

    err = reply["value"]
    raise PluginError.from_wire(err["code"], err["message"], err.get("details", ""))


def _write_execute_reply(root: _CapnpStruct, reply: ExecuteReply) -> None:
    root.set_text(0, reply["operationId"])
    stmts = root.init_struct_list(1, len(reply["statements"]), 1, 2)
    for i, stmt in enumerate(reply["statements"]):
        _write_statement_result(stmts[i], stmt)
    timing = root.init_struct(2, 2, 1)
    t = reply["timing"]
    timing.set_u64(0, t["attemptElapsedUs"])
    timing.set_u64(1, t["dbExecutionUs"])
    timing.set_text(0, t["dbTimingSource"])


def _read_execute_reply(root: _StructReader) -> ExecuteReply:
    t = root.get_struct(2, 2, 1)
    return {
        "operationId": root.get_text(0),
        "statements": [_read_statement_result(s) for s in root.get_struct_list(1, 1, 2)],
        "timing": {
            "attemptElapsedUs": t.get_u64(0),
            "dbExecutionUs": t.get_u64(1),
            "dbTimingSource": t.get_text(0),
        },
    }


def _write_statement_result(s: _CapnpStruct, stmt: StatementResult) -> None:
    s.set_u64(0, stmt["rowsAffected"])
    rows = s.init_struct_list(0, len(stmt["rows"]), 0, 1)
    for i, row in enumerate(stmt["rows"]):
        cells = rows[i].init_struct_list(0, len(row["values"]), 2, 1)
        for j, cell in enumerate(row["values"]):
            _write_db_value(cells[j], cell)
    cols = s.init_struct_list(1, len(stmt["columns"]), 1, 1)
    for i, col in enumerate(stmt["columns"]):
        cols[i].set_text(0, col["name"])
        cols[i].set_u16(0, _DB_TYPE_ORD[col["dbType"]])


def _read_statement_result(s: _StructReader) -> StatementResult:
    columns = []
    for c in s.get_struct_list(1, 1, 1):
        ty = _DB_TYPE_FROM[c.get_u16(0)]
        columns.append({"name": c.get_text(0), "dbType": ty})
    rows = [
        {"values": [_read_db_value(cell) for cell in row.get_struct_list(0, 2, 1)]}
        for row in s.get_struct_list(0, 0, 1)
    ]
    return {
        "rows": rows,
        "columns": columns,
        "rowsAffected": s.get_u64(0),
    }


class D1Meta(TypedDict):
    """Cloudflare D1Result.meta projection."""

    duration: float
    changes: int
    last_row_id: int
    changed_db: bool
    rows_read: int
    rows_written: int


class D1Result(TypedDict):
    """Cloudflare D1Result projection for plugin guests."""

    success: bool
    results: list[dict[str, Any]] | None
    meta: D1Meta


class D1ExecResult(TypedDict):
    """Cloudflare D1ExecResult projection."""

    count: int
    duration: float


def statement_result_to_d1_result(stmt: StatementResult, timing: DbTiming) -> D1Result:
    """Map one statement result to Cloudflare :class:`D1Result`."""
    changes = int(stmt["rowsAffected"])
    duration_ms = timing["dbExecutionUs"] / 1000.0
    columns = stmt.get("columns") or []
    rows = stmt.get("rows") or []
    results: list[dict[str, Any]] | None
    if columns:
        mapped: list[dict[str, Any]] = []
        for row in rows:
            cells = row["values"] if isinstance(row, dict) else []
            mapped.append({col["name"]: cell for col, cell in zip(columns, cells, strict=False)})
        results = mapped
    else:
        results = None
    return {
        "success": True,
        "results": results,
        "meta": {
            "duration": duration_ms,
            "changes": changes,
            "last_row_id": 0,
            "changed_db": changes > 0,
            "rows_read": len(rows),
            "rows_written": changes,
        },
    }


def execute_reply_to_d1_results(reply: ExecuteReply) -> list[D1Result]:
    """Map an execute reply to one Cloudflare :class:`D1Result` per statement."""
    timing = reply["timing"]
    return [statement_result_to_d1_result(stmt, timing) for stmt in reply["statements"]]


def _row_map_from_statement(result: StatementResult) -> dict[str, Any] | None:
    rows = result.get("rows") or []
    columns = result.get("columns") or []
    if not rows or not columns:
        return None
    cells = rows[0]["values"] if isinstance(rows[0], dict) else []
    return {col["name"]: cell for col, cell in zip(columns, cells, strict=False)}


def _column_value_from_row(row: dict[str, Any], col_name: str) -> Any:
    if col_name in row:
        return row[col_name]
    lower = col_name.lower()
    for name, value in row.items():
        if name.lower() == lower:
            return value
    raise ValueError(f"column {col_name} not found in first() result")


class DatabaseBinding:
    """Host-mediated Cloudflare-style SQL binding for plugin guests.

    Public surface is ``prepare().bind().run()/first()/all()`` and
    ``batch()``. Raw ``execute`` is an internal transport used by those
    methods. Each call without an explicit :class:`RetryToken` mints a fresh
    operation id and leaves ``requestHash`` empty so the trusted host can stamp
    the canonical digest after validation. A :class:`RetryToken` reuses both.
    """

    def __init__(
        self,
        execute: AtomicTransport,
        *,
        limits: LimitsTransport | None = None,
        execute_streamed: StreamedTransport | None = None,
        max_request_bytes: int = 0,
        max_result_rows: int = 0,
        operation_id: str | None = None,
        request_hash: str = "",
        deadline_unix_ms: int = 0,
    ) -> None:
        """Create a binding over a host ``execute`` transport.

        Args:
            execute: Host session projection.
            limits: ``GuestDatabase.limits`` projection; ``None`` on a host
                that predates it (:meth:`limits` then raises ``unsupported``).
            execute_streamed: ``GuestDatabase.executeStreamed`` projection:
                the same batch answered as ``ExecuteStreamFrame`` bytes. When
                set and the session advertises a streamed budget, every
                row-returning read goes through it; ``None`` reads inline.
            max_request_bytes: Negotiated cap (``0`` = unlimited).
            max_result_rows: Default ``maxRows`` for :meth:`PreparedStatement.all`.
            operation_id: Default retry id; omitted calls mint a UUID.
            request_hash: Default retry hash; empty lets the host stamp one.
            deadline_unix_ms: Guest-visible deadline (unix ms).
        """
        self._execute = execute
        self._limits = limits
        self._execute_streamed = execute_streamed
        self._limits_fetch: asyncio.Future[DbCapabilities | None] | None = None
        self._max_request_bytes = max_request_bytes
        self._max_result_rows = max_result_rows
        self._operation_id = operation_id
        self._request_hash = request_hash
        self._deadline_unix_ms = deadline_unix_ms

    def prepare(self, sql: str) -> PreparedStatement:
        """Prepare one canonical-SQL statement (``?`` placeholders).

        Args:
            sql: Host-mediated SQL. Kind and bounds are derived by the host.

        Returns:
            A statement that can be bound and executed.
        """
        return PreparedStatement(self, sql, [], max_result_rows=self._max_result_rows)

    async def limits(self) -> DbCapabilities:
        """The negotiated limits of this binding's session (``DbCapabilities``).

        ``maxRequestBytes``, ``maxStatements``, ``maxResultBytes``,
        ``maxResultRows`` and the rest: use them to size a batch or a read
        before sending.

        Returns:
            The adapter's capability advertisement for this session.

        Raises:
            PluginError: ``unsupported`` on a host that predates
                ``GuestDatabase.limits``.
        """
        if self._limits is None:
            raise _plugin_error().from_wire(
                "unsupported", "GuestDatabase.limits is not available on this host"
            )
        caps = await self._session_limits()
        if caps is None:
            # The cached fetch failed: let the host's own error surface, and
            # keep a later success so reads use the session's budget.
            caps = await self._limits()
            fetched: asyncio.Future[DbCapabilities | None] = (
                asyncio.get_running_loop().create_future()
            )
            fetched.set_result(caps)
            self._limits_fetch = fetched
        return caps

    async def _session_limits(self) -> DbCapabilities | None:
        """The session's limits, fetched once per binding (one invocation).

        Overlapping first reads share one fetch, so a later failure cannot
        replace an earlier success. The fetch is shielded from its callers:
        a cancelled or timed-out read leaves it running for the others, and
        a fetch that was itself cancelled is forgotten so the next read
        retries instead of inheriting the cancellation.

        Returns:
            The capabilities, or ``None`` when the host predates
            ``GuestDatabase.limits`` or the call failed; the binding then
            reads inline and :meth:`limits` reports the error itself.
        """
        fetch = self._limits_fetch
        if fetch is None or fetch.cancelled():
            # A fetch cancelled with no waiter (its loop closed under it) is
            # replaced rather than awaited, which would raise its cancellation.
            fetch = self._limits_fetch = asyncio.ensure_future(self._fetch_limits())
        try:
            return await asyncio.shield(fetch)
        except asyncio.CancelledError:
            if fetch.cancelled() and self._limits_fetch is fetch:
                self._limits_fetch = None
            raise

    async def _fetch_limits(self) -> DbCapabilities | None:
        """One ``GuestDatabase.limits`` call, with any failure read as ``None``."""
        if self._limits is None:
            return None
        try:
            return await self._limits()
        except Exception:  # noqa: BLE001  (any failure means "read inline")
            return None

    def chunk(
        self,
        statements: list[PreparedStatement],
        *,
        max_statements: int | None = None,
        max_request_bytes: int | None = None,
    ) -> AsyncIterator[list[PreparedStatement]]:
        """Split ``statements`` into batches that each fit one request.

        ``async for b in db.chunk(stmts): await db.batch(b)``. Each batch is
        atomic; the loop is not. The caps come from the session's limits
        (``maxStatements``, and the tighter of the session's and the
        binding's ``maxRequestBytes``) unless given here; zero caps given
        explicitly mean one batch. A single statement larger than the
        request cap is yielded alone, so :meth:`batch` reports the typed
        error. Batches are sized with the UUID-shaped retry id the binding
        mints; a caller-chosen retry id that is longer leaves that much less
        room.

        Args:
            statements: Prepared statements (binds and intent already applied).
            max_statements: Statements per batch (``0`` = no cap); the
                session's ``maxStatements`` when ``None``.
            max_request_bytes: Encoded request bytes per batch (``0`` = no
                cap); the tighter of the session's and the binding's cap
                when ``None``.

        Returns:
            Batches in order, together covering every statement once.

        Raises:
            PluginError: ``unsupported`` when neither the session's limits nor
                explicit caps are available.
        """
        return self._chunk(statements, max_statements, max_request_bytes)

    async def _chunk(
        self,
        statements: list[PreparedStatement],
        max_statements: int | None,
        max_request_bytes: int | None,
    ) -> AsyncIterator[list[PreparedStatement]]:
        explicit = max_statements is not None and max_request_bytes is not None
        caps = None if explicit else await self._session_limits()
        statements_cap = (
            max_statements
            if max_statements is not None
            else (caps["maxStatements"] if caps is not None else 0)
        )
        if max_request_bytes is not None:
            bytes_cap = max_request_bytes
        else:
            # The session's request cap and the binding's own both apply to
            # ``batch()``, so a batch is sized to the tighter one.
            bytes_cap = _tighter_cap(
                caps["maxRequestBytes"] if caps is not None else 0, self._max_request_bytes
            )
        # Explicit zero caps mean "one batch"; only missing caps are an error.
        if not explicit and statements_cap <= 0 and bytes_cap <= 0:
            raise _plugin_error().from_wire(
                "unsupported",
                "chunk needs the session limits (GuestDatabase.limits) "
                "or explicit max_statements / max_request_bytes",
            )
        typed = [stmt._as_typed() for stmt in statements]  # noqa: SLF001
        for indexes in chunk_indexes(typed, statements_cap, bytes_cap):
            yield [statements[i] for i in indexes]

    async def write_chunks(
        self,
        statements: list[PreparedStatement],
        *,
        key: str,
        start: int = 0,
        stop: Callable[[int], bool] | None = None,
        max_statements: int | None = None,
        max_request_bytes: int | None = None,
    ) -> WriteProgress:
        """Write ``statements`` in batches that fit one request, resumably.

        The batches are the ones :meth:`chunk` yields for
        ``statements[start:]``, each committed with :meth:`batch` under an
        operation id derived from ``key`` and the batch's offset
        (:func:`write_chunks_operation_id`). Store the returned ``next`` (for
        example in a job checkpoint) and call again with the same list, the
        same ``key`` and ``start=next`` to carry on: a batch whose reply was
        lost is resent under the same id and the host replays its receipt.
        Receipts are kept for 24 hours; a batch resent later runs again, so a
        write that may pause that long uses statements safe to repeat.
        The list and the caps must not change between calls with one ``key``,
        or a resent batch no longer matches its receipt and the host answers
        ``conflict``.

        Args:
            statements: The whole list, the same on every call.
            key: Names this write, stable across resumes (a job id plus a step
                name); at most :data:`MAX_WRITE_KEY_BYTES` UTF-8 bytes.
            start: Statements already committed; ``0`` on the first call.
            stop: Asked before each batch with the statements committed so
                far; returning ``True`` pauses the run there.
            max_statements: As for :meth:`chunk`.
            max_request_bytes: As for :meth:`chunk`.

        Returns:
            ``{"next": n, "done": bool}``: statements committed, and whether
            all of them are.

        Raises:
            WriteChunksError: Carrying ``next`` when the input is invalid or a
                batch fails.
        """
        from .workerd import WriteChunksError

        plugin_error = _plugin_error()

        def fail(next_offset: int, err: Exception) -> WriteChunksError:
            cause = err if isinstance(err, plugin_error) else plugin_error("internal", str(err))
            return WriteChunksError(next_offset, cause)

        key_bytes = len(key.encode("utf-8"))
        if key_bytes == 0 or key_bytes > MAX_WRITE_KEY_BYTES:
            raise fail(
                start,
                plugin_error(
                    "invalid_params",
                    f"write_chunks key must be 1 to {MAX_WRITE_KEY_BYTES} bytes",
                ),
            )
        if start < 0 or start > len(statements):
            raise fail(
                start,
                plugin_error(
                    "invalid_params",
                    f"write_chunks start {start} is not within the {len(statements)} statements",
                ),
            )
        batches: list[list[PreparedStatement]] = []
        try:
            # A cap left as None keeps the session's, as in :meth:`chunk`.
            async for b in self.chunk(
                statements[start:],
                max_statements=max_statements,
                max_request_bytes=max_request_bytes,
            ):
                batches.append(b)
        except Exception as err:
            raise fail(start, err) from err
        next_offset = start
        for b in batches:
            if stop is not None and stop(next_offset):
                return {"next": next_offset, "done": False}
            retry = RetryToken(write_chunks_operation_id(key, next_offset), "")
            try:
                await self.batch(b, retry=retry)
            except Exception as err:
                raise fail(next_offset, err) from err
            next_offset += len(b)
        return {"next": next_offset, "done": True}

    async def batch(
        self,
        statements: list[PreparedStatement],
        *,
        retry: RetryToken | None = None,
    ) -> list[D1Result]:
        """Run ``statements`` as one typed atomic batch.

        Returns one Cloudflare-shaped :class:`D1Result` per statement.
        """
        typed = []
        for stmt in statements:
            typed.append(stmt._as_typed())  # noqa: SLF001
        reply = await self._execute_batch(typed, retry=retry)
        return execute_reply_to_d1_results(reply)

    async def exec(
        self,
        query: str,
        *,
        retry: RetryToken | None = None,
    ) -> D1ExecResult:
        """Execute raw SQL without bind parameters (Cloudflare ``D1Database.exec``)."""
        queries = split_exec_queries(query)
        if not queries:
            raise ValueError("exec query is empty")
        prepared = [self.prepare(sql) for sql in queries]
        results = await self.batch(prepared, retry=retry)
        return {
            "count": len(results),
            "duration": sum(r["meta"]["duration"] for r in results),
        }

    async def execute(
        self,
        batch: list[TypedDbStatement],
        *,
        retry: RetryToken | None = None,
    ) -> ExecuteReply:
        """Internal typed-batch transport. Prefer :meth:`prepare` / :meth:`batch`."""
        return await self._execute_batch(batch, retry=retry)

    async def _request_for(
        self,
        batch: list[TypedDbStatement],
        retry: RetryToken | None,
    ) -> ExecuteRequest:
        if not batch:
            raise ValueError("execute statements must be non-empty")
        if retry is not None:
            operation_id = retry.operation_id
            request_hash = retry.request_hash
        elif self._operation_id is not None:
            operation_id = self._operation_id
            request_hash = self._request_hash
        else:
            operation_id = str(uuid.uuid4())
            request_hash = ""
        request: ExecuteRequest = {
            "operationId": operation_id,
            "requestHash": request_hash,
            "statements": batch,
            "deadlineUnixMs": self._deadline_unix_ms,
        }
        encoded = encode_execute_request(request)
        # The tighter of the binding's configured cap and the session's, so
        # an oversized request fails locally with the typed error the host
        # would raise (the same cap `chunk()` splits to).
        caps = await self._session_limits()
        cap = _tighter_cap(
            self._max_request_bytes, caps["maxRequestBytes"] if caps is not None else 0
        )
        if cap and len(encoded) > cap:
            raise _plugin_error().limit_exceeded(
                LimitExceeded("maxRequestBytes", cap, len(encoded))
            )
        return request

    async def _open_streamed(
        self, request: ExecuteRequest
    ) -> tuple[AsyncIterable[bytes], DbCapabilities] | None:
        """Open the streamed read for ``request`` when the session streams.

        Returns:
            The frame chunks and the session's capabilities, or ``None`` to
            read inline: no streamed transport, no (or non-streaming) limits,
            or an ``unsupported`` answer from an older host.
        """
        if self._execute_streamed is None:
            return None
        caps = await self._session_limits()
        if caps is None or not streams_results(caps):
            return None
        try:
            body = await self._execute_streamed(request)
        except _plugin_error() as err:
            if err.wire_code == "unsupported":
                return None
            raise
        if isinstance(body, (bytes, bytearray, memoryview)):
            return _one_chunk(bytes(body)), caps
        return body, caps

    async def _execute_batch(
        self,
        batch: list[TypedDbStatement],
        *,
        retry: RetryToken | None = None,
    ) -> ExecuteReply:
        request = await self._request_for(batch, retry)
        streamed = await self._open_streamed(request)
        if streamed is None:
            return _reply_for(request, await self._execute(request))
        chunks, caps = streamed
        decoder = ExecuteStreamDecoder(execute_stream_limits(caps))
        try:
            async for chunk in chunks:
                decoder.push(bytes(chunk))
        finally:
            # A refused frame closes the chunk source, which releases the
            # unread transport body instead of leaving it parked.
            await _aclose(chunks)
        try:
            reply = decoder.finish()
        except _plugin_error() as err:
            if err.wire_code == "unsupported":
                return _reply_for(request, await self._execute(request))
            raise
        return _reply_for(request, reply)

    async def _inline_rows(self, request: ExecuteRequest) -> AsyncIterator[dict[str, Any]]:
        """Rows of a single-statement request read inline, from one ``execute``."""
        reply = _reply_for(request, await self._execute(request))
        if reply["statements"]:
            result = reply["statements"][0]
            for row in result.get("rows") or []:
                yield _row_map(result.get("columns") or [], row)

    async def _iterate_rows(
        self,
        batch: list[TypedDbStatement],
        retry: RetryToken | None,
    ) -> AsyncIterator[dict[str, Any]]:
        """Rows of a single-statement request as they arrive, kept by no one.

        A stream that answers ``unsupported`` before any row arrived falls back
        to the inline read like ``all()``; once rows were handed out the error
        is the caller's, since the read cannot restart behind them.
        """
        request = await self._request_for(batch, retry)
        streamed = await self._open_streamed(request)
        if streamed is None:
            async for row in self._inline_rows(request):
                yield row
            return
        chunks, caps = streamed
        decoder = ExecuteStreamDecoder(execute_stream_limits(caps), retain_rows=False)

        def falls_back_inline(err: Any, yielded: bool) -> bool:
            return not yielded and err.wire_code == "unsupported"

        # One push can complete the statement, its rows and the end frame at
        # once, so the columns come from the frames rather than the decoder.
        columns: list[dict[str, Any]] = []
        yielded = False
        try:
            async for chunk in chunks:
                try:
                    frames = decoder.push(bytes(chunk))
                except _plugin_error() as err:
                    if falls_back_inline(err, yielded):
                        async for row in self._inline_rows(request):
                            yield row
                        return
                    raise
                for frame in frames:
                    if frame["kind"] == "head":
                        _check_reply_identity(
                            request,
                            frame["value"]["operationId"],
                            frame["value"]["statementCount"],
                        )
                    elif frame["kind"] == "statement":
                        columns = frame["value"]["columns"]
                    elif frame["kind"] == "rows":
                        for row in frame["value"]:
                            yielded = True
                            yield _row_map(columns, row)
        finally:
            # A consumer that stops early (or a refused frame) closes the chunk
            # source, which releases the unread transport body.
            await _aclose(chunks)
        try:
            decoder.finish()
        except _plugin_error() as err:
            if falls_back_inline(err, yielded):
                async for row in self._inline_rows(request):
                    yield row
                return
            raise


async def _aclose(source: AsyncIterable[bytes]) -> None:
    """Close an async chunk source that offers ``aclose()``; a plain iterable is left alone."""
    aclose = getattr(source, "aclose", None)
    if aclose is not None:
        await aclose()


def _tighter_cap(a: int, b: int) -> int:
    """The smaller of two caps where ``0`` means "no cap"."""
    if a > 0 and b > 0:
        return min(a, b)
    return a if a > 0 else b


def _check_reply_identity(request: ExecuteRequest, operation_id: str, statement_count: int) -> None:
    """Check that a reply answers ``request``: same id, one result per statement.

    A request sent without an id takes the id the host minted.

    Raises:
        PluginError: ``invalid_params`` when the reply is for another request.
    """
    if request["operationId"] and operation_id != request["operationId"]:
        raise _plugin_error().from_wire(
            "invalid_params",
            f"reply answers operation {operation_id}, not {request['operationId']}",
        )
    if statement_count != len(request["statements"]):
        raise _plugin_error().from_wire(
            "invalid_params",
            f"reply carries {statement_count} results for {len(request['statements'])} statements",
        )


def _reply_for(request: ExecuteRequest, reply: ExecuteReply) -> ExecuteReply:
    """``reply`` once it is checked against ``request``.

    Its identity (see :func:`_check_reply_identity`), and no result with more
    rows than its statement's ``maxRows`` asked for.

    Raises:
        PluginError: ``invalid_params`` when the reply is for another request
            or a result carries more rows than its statement allowed.
    """
    _check_reply_identity(request, reply["operationId"], len(reply["statements"]))
    for i, (statement, result) in enumerate(
        zip(request["statements"], reply["statements"], strict=True)
    ):
        max_rows = statement.get("maxRows") or 0
        rows = len(result.get("rows") or [])
        if 0 < max_rows < rows:
            raise _plugin_error().from_wire(
                "invalid_params",
                f"reply carries {rows} rows for statement {i}, which asked for at most {max_rows}",
            )
    return reply


async def _one_chunk(body: bytes) -> AsyncIterator[bytes]:
    """Yield ``body`` as the single chunk of a stream.

    Args:
        body: Every frame, concatenated.

    Yields:
        ``body`` once.
    """
    yield body


def _row_map(columns: list[dict[str, Any]], row: dict[str, Any]) -> dict[str, Any]:
    """Pair one row's cells with its column names.

    Args:
        columns: ``DbColumn`` descriptors of the statement.
        row: ``DbRow`` (``values`` in column order).

    Returns:
        Column name → typed cell.
    """
    cells = row.get("values") or []
    return {col["name"]: cell for col, cell in zip(columns, cells, strict=False)}


AtomicTransport = Callable[[ExecuteRequest], Awaitable[ExecuteReply]]
LimitsTransport = Callable[[], Awaitable["DbCapabilities"]]
StreamedTransport = Callable[[ExecuteRequest], Awaitable["bytes | AsyncIterable[bytes]"]]
"""``GuestDatabase.executeStreamed`` projection: frame bytes, whole or chunked."""


def create_database_binding(
    transport: AtomicTransport,
    *,
    limits: LimitsTransport | None = None,
    execute_streamed: StreamedTransport | None = None,
    max_request_bytes: int = 0,
    max_result_rows: int = 0,
    operation_id: str | None = None,
    request_hash: str = "",
    deadline_unix_ms: int = 0,
) -> DatabaseBinding:
    """Build a host-mediated :class:`DatabaseBinding` over an async transport.

    Args:
        transport: Host ``execute`` projection.
        limits: Host ``limits`` projection, when the host offers one.
        execute_streamed: Host ``executeStreamed`` projection, when the host
            offers one; reads then stream on sessions that advertise a
            streamed budget.
        max_request_bytes: Negotiated cap (``0`` = unlimited).
        max_result_rows: Default ``maxRows`` for ``all()``.
        operation_id: Default retry id; omitted calls mint a UUID.
        request_hash: Default retry hash; empty lets the host stamp one.
        deadline_unix_ms: Guest-visible deadline (unix ms).

    Returns:
        A binding whose terminal methods route through ``transport``.
    """
    return DatabaseBinding(
        transport,
        limits=limits,
        execute_streamed=execute_streamed,
        max_request_bytes=max_request_bytes,
        max_result_rows=max_result_rows,
        operation_id=operation_id,
        request_hash=request_hash,
        deadline_unix_ms=deadline_unix_ms,
    )


MAX_WRITE_KEY_BYTES = 256
"""Longest ``key`` :meth:`DatabaseBinding.write_chunks` accepts, in UTF-8 bytes."""


class WriteProgress(TypedDict):
    """Where a :meth:`DatabaseBinding.write_chunks` run stands."""

    next: int
    """Statements committed from the start of the list; always a batch boundary."""
    done: bool
    """True when every statement is committed; false when ``stop`` paused the run."""


def write_chunks_operation_id(key: str, offset: int) -> str:
    r"""The operation id :meth:`DatabaseBinding.write_chunks` sends for a batch.

    The first 16 bytes of SHA-256 over
    ``"bookclerk-write-chunks\0" + key + "\0" + offset`` (decimal), as
    lowercase hex grouped 8-4-4-4-12. The Rust and TypeScript SDKs derive
    the same id.

    Args:
        key: Name of the write.
        offset: Index of the batch's first statement.

    Returns:
        A UUID-shaped operation id.
    """
    digest = hashlib.sha256(
        b"bookclerk-write-chunks\0" + key.encode("utf-8") + b"\0" + str(offset).encode("ascii")
    ).hexdigest()
    return f"{digest[0:8]}-{digest[8:12]}-{digest[12:16]}-{digest[16:20]}-{digest[20:32]}"


class RetryToken:
    """Explicit retry identity: reuse both ``operationId`` and ``requestHash``."""

    def __init__(self, operation_id: str, request_hash: str) -> None:
        self.operation_id = operation_id
        self.request_hash = request_hash


class PreparedStatement:
    """Cloudflare-style prepared statement over a :class:`DatabaseBinding`."""

    def __init__(
        self,
        binding: DatabaseBinding,
        sql: str,
        parameters: list[DbValue],
        result_selection: DbResultSelection | None = None,
        max_rows: int | None = None,
        *,
        max_result_rows: int = 0,
        intent: tuple[DbResultSelection, int] | None = None,
    ) -> None:
        self._binding = binding
        self.sql = sql
        self.parameters = list(parameters)
        self._max_result_rows = max_result_rows
        if intent is not None:
            self._intent: tuple[DbResultSelection, int] | None = intent
        elif result_selection is not None:
            self._intent = (result_selection, 0 if max_rows is None else max_rows)
        else:
            self._intent = ("rows", max_result_rows)

    def bind(self, *values: DbValue) -> PreparedStatement:
        """Replace bound parameters with ``values`` (universal ``DbValue`` only)."""
        return PreparedStatement(
            self._binding,
            self.sql,
            list(values),
            max_result_rows=self._max_result_rows,
            intent=self._intent,
        )

    def as_run(self) -> PreparedStatement:
        """Mark DML intent for :meth:`DatabaseBinding.batch`."""
        return PreparedStatement(
            self._binding,
            self.sql,
            self.parameters,
            max_result_rows=self._max_result_rows,
            intent=("affectedRows", 0),
        )

    def as_first(self) -> PreparedStatement:
        """Mark ``maxRows = 1`` row intent for :meth:`DatabaseBinding.batch`."""
        return PreparedStatement(
            self._binding,
            self.sql,
            self.parameters,
            max_result_rows=self._max_result_rows,
            intent=("rows", 1),
        )

    def as_all(self) -> PreparedStatement:
        """Mark row-returning intent for :meth:`DatabaseBinding.batch`."""
        return PreparedStatement(
            self._binding,
            self.sql,
            self.parameters,
            max_result_rows=self._max_result_rows,
            intent=("rows", self._max_result_rows),
        )

    async def run(self, *, retry: RetryToken | None = None) -> D1Result:
        """Execute as Cloudflare ``run()`` (functionally equivalent to :meth:`all`)."""
        return await self.all(retry=retry)

    async def first(
        self,
        col_name: str | None = None,
        *,
        retry: RetryToken | None = None,
    ) -> dict[str, Any] | Any | None:
        """Return the first row, or one column when ``col_name`` is set."""
        reply = await self._binding.execute(
            [self.as_first()._as_typed()],  # noqa: SLF001
            retry=retry,
        )
        result = reply["statements"][0] if reply["statements"] else None
        if result is None:
            return None
        row = _row_map_from_statement(result)
        if row is None:
            return None
        if col_name is not None:
            return _column_value_from_row(row, col_name)
        return row

    async def raw(self, *, retry: RetryToken | None = None) -> list[list[Any]]:
        """Return positional cell values per row (Cloudflare ``raw()``)."""
        reply = await self._binding.execute(
            [self.as_all()._as_typed()],  # noqa: SLF001
            retry=retry,
        )
        result = reply["statements"][0] if reply["statements"] else None
        if result is None:
            return []
        return [row["values"] if isinstance(row, dict) else [] for row in result.get("rows") or []]

    async def all(self, *, retry: RetryToken | None = None) -> D1Result:
        """Execute as a row-returning query. Returns a Cloudflare-shaped :class:`D1Result`."""
        reply = await self._binding.execute(
            [self.as_all()._as_typed()],  # noqa: SLF001
            retry=retry,
        )
        return statement_result_to_d1_result(reply["statements"][0], reply["timing"])

    def iterate(self, *, retry: RetryToken | None = None) -> AsyncIterator[dict[str, Any]]:
        """Rows one at a time from the same single-snapshot read as :meth:`all`.

        ``async for row in stmt.iterate(): ...`` decodes the streamed reply
        frame by frame without holding the whole result. The read still fails
        closed (``limit_exceeded``) past the session's streamed budget. On a
        session that does not stream the rows come from one inline ``all()``.

        Args:
            retry: Optional retry token.

        Returns:
            Column name → typed cell, one mapping per row, in order.
        """
        return self._binding._iterate_rows(  # noqa: SLF001
            [self.as_all()._as_typed()],  # noqa: SLF001
            retry,
        )

    def _as_typed(self) -> TypedDbStatement:
        result_selection, max_rows = self._intent or ("rows", self._max_result_rows)
        kind: DbStatementKind = (
            "execute"
            if result_selection in ("affectedRows", "discard")
            else guest_statement_kind(self.sql)
        )
        return {
            "sql": self.sql,
            "parameters": self.parameters,
            "kind": kind,
            "maxRows": max_rows,
            "resultSelection": result_selection,
        }


# ---------------------------------------------------------------------------
# Streamed execute replies (``GuestDatabase.executeStreamed``)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class ExecuteStreamLimits:
    """What a receiver lets a streamed reply grow to.

    Attributes:
        max_bytes: Encoded reply budget in bytes (``0`` = only the framing slack).
        bytes_limit: Capability field ``max_bytes`` came from, named by the
            error it raises (``maxStreamedResultBytes`` or ``maxAtomicResultBytes``).
        max_rows: Rows one statement may return (``0`` = unlimited).
        rows_limit: Capability field ``max_rows`` came from
            (``maxStreamedResultRows`` or ``maxResultRows``).
    """

    max_bytes: int
    bytes_limit: str
    max_rows: int
    rows_limit: str


def streams_results(caps: DbCapabilities) -> bool:
    """Whether ``caps`` advertises a streamed result budget.

    Args:
        caps: Session capabilities (``limits()``).

    Returns:
        True when ``executeStreamed`` is worth calling on this session.
    """
    return caps["maxStreamedResultBytes"] > 0 and caps["maxStreamedResultRows"] > 0


def execute_stream_limits(caps: DbCapabilities) -> ExecuteStreamLimits:
    """The streamed budget of ``caps``, or its inline caps when it does not stream.

    Args:
        caps: Session capabilities (``limits()``).

    Returns:
        Limits for an :class:`ExecuteStreamDecoder`.
    """
    if streams_results(caps):
        return ExecuteStreamLimits(
            caps["maxStreamedResultBytes"],
            "maxStreamedResultBytes",
            caps["maxStreamedResultRows"],
            "maxStreamedResultRows",
        )
    return ExecuteStreamLimits(
        caps["maxAtomicResultBytes"],
        "maxAtomicResultBytes",
        caps["maxResultRows"],
        "maxResultRows",
    )


def raw_stream_ceiling(limits: ExecuteStreamLimits) -> int:
    """Raw frame bytes a receiver accepts before failing closed.

    The reply budget plus one eighth for frame headers and one window of slack.

    Args:
        limits: Budget.

    Returns:
        Byte ceiling on the whole frame sequence and on any one frame.
    """
    return limits.max_bytes + limits.max_bytes // 8 + MAX_STREAM_WINDOW_BYTES


def _validate_cell(cell: dict[str, Any]) -> None:
    """Check a streamed cell the way the inline reader does.

    Raises:
        PluginError: ``invalid_params`` for a non-finite ``float64`` or TEXT
            holding U+0000.
    """
    kind = cell["kind"]
    if kind == "float64" and not math.isfinite(cell["value"]):
        raise _plugin_error().from_wire(
            "invalid_params", "streamed reply float64 cell is not finite"
        )
    if kind == "text" and "\x00" in cell["value"]:
        raise _plugin_error().from_wire(
            "invalid_params", "streamed reply TEXT cell contains U+0000"
        )


_ROWS_FRAME = 2
"""Discriminant of the ``rows`` variant of ``ExecuteStreamFrame``."""


def _decode_frame(data: bytes, ceiling: int, admit_rows: Callable[[int], None]) -> dict[str, Any]:
    """Decode one frame under the traversal budget its stream negotiated.

    A frame carrying one wide row can be larger than the budget that suits
    control messages, so the reader's budget follows the frame's own size,
    clamped to the stream's raw ceiling, as the host's decoder does. A rows
    frame is announced to ``admit_rows`` by its row count before any cell
    decodes, so the decoder can refuse it against its row budget first.

    Args:
        data: One complete single-segment frame.
        ceiling: Raw byte ceiling of the stream (:func:`raw_stream_ceiling`).
        admit_rows: Called with the row count of a rows frame before its
            cells decode; raises to refuse the frame.

    Returns:
        The ``ExecuteStreamFrame`` dict.

    Raises:
        PluginError: ``invalid_params`` when the frame does not decode, or
            whatever ``admit_rows`` raised.
    """
    words = min(len(data) // 8 + 16, max(ceiling, MAX_SCALAR_BYTES) // 8)
    try:
        reader = _CapnpReader(data, words)
        codec = _execute_stream_frame_codec
        root = reader.root(codec.data_words, codec.pointer_count)
        if root.get_u16(0) == _ROWS_FRAME:
            admit_rows(root.get_struct_list_length(0))
        return codec.read(root, NO_CAPS)
    except _plugin_error():
        raise
    except Exception as err:
        raise _plugin_error().from_wire(
            "invalid_params", f"streamed reply frame does not decode: {err}"
        ) from err


class ExecuteStreamDecoder:
    """Sans-IO decoder of a streamed execute reply.

    Feed it bytes in any split with :meth:`push`, then take the reassembled
    ``ExecuteReply`` with :meth:`finish`. It fails closed on more raw bytes
    than the budget allows, on a statement with more rows than the row cap,
    and on any frame out of sequence; fed in any split it yields the same
    reply as one push.
    """

    def __init__(self, limits: ExecuteStreamLimits, *, retain_rows: bool = True) -> None:
        """Create a decoder held to ``limits``.

        Args:
            limits: Budget the stream is held to.
            retain_rows: ``False`` counts rows against the budget but drops
                them from the collected reply (for ``iterate()``).
        """
        self._limits = limits
        self._retain_rows = retain_rows
        self._buf = bytearray()
        self._at = 0
        self._seen = 0
        self._state = "awaitingHead"
        self._operation_id = ""
        self._statement_count = 0
        self._next_index = 0
        self._statements: list[StatementResult] = []
        self._open: StatementResult | None = None
        self._open_rows = 0
        self._columns: list[dict[str, Any]] = []
        self._outcome: ExecuteReply | PluginError | None = None

    @property
    def finished(self) -> bool:
        """Whether the final frame (``end`` or ``err``) has been decoded.

        Returns:
            True once the stream is complete.
        """
        return self._state == "finished"

    @property
    def columns(self) -> list[dict[str, Any]]:
        """Columns of the statement whose rows are arriving.

        Returns:
            The open statement's ``DbColumn`` list, empty between statements.
        """
        return self._columns

    def push(self, data: bytes) -> list[dict[str, Any]]:
        """Feed the next bytes of the stream.

        Args:
            data: Any slice of the frame sequence.

        Returns:
            The ``ExecuteStreamFrame`` dicts completed by these bytes, in order.

        Raises:
            PluginError: ``limit_exceeded`` when the stream exceeds the byte
                or row budget; ``invalid_params`` when a frame is malformed
                or out of sequence or bytes follow the final frame.
        """
        try:
            return self._push(data)
        except _plugin_error() as err:
            # A refused stream stays refused: ``finish()`` after a failed push
            # reports the failure, never a reply the earlier frames described.
            self._columns = []
            self._state = "finished"
            self._outcome = err
            raise

    def _push(self, data: bytes) -> list[dict[str, Any]]:
        """:meth:`push` before its failure is latched."""
        frames: list[dict[str, Any]] = []
        if not data:
            return frames
        if self.finished:
            raise self._sequence("carries bytes after its final frame")
        self._seen += len(data)
        ceiling = raw_stream_ceiling(self._limits)
        if self._seen > ceiling:
            raise self._bytes_exceeded(self._seen)
        self._buf += data
        # Completed frames are consumed by offset and the buffer is compacted
        # once per push, so a reply of many frames is decoded in one pass.
        buf = self._buf
        at = self._at
        while len(buf) - at >= 8:
            if int.from_bytes(buf[at : at + 4], "little") != 0:
                raise _plugin_error().from_wire(
                    "invalid_params",
                    "streamed reply frame is not a single-segment Cap'n message",
                )
            frame_len = int.from_bytes(buf[at + 4 : at + 8], "little") * 8 + 8
            if frame_len > ceiling:
                raise self._bytes_exceeded(frame_len)
            if len(buf) - at < frame_len:
                break
            frame = _decode_frame(bytes(buf[at : at + frame_len]), ceiling, self._admit_rows)
            at += frame_len
            self._apply(frame)
            frames.append(frame)
            if self.finished:
                if len(buf) > at:
                    raise self._sequence("carries bytes after its final frame")
                break
        if at:
            del buf[:at]
        self._at = 0
        return frames

    def finish(self) -> ExecuteReply:
        """Take the reassembled reply once the stream has ended.

        Returns:
            The reply the frames described.

        Raises:
            PluginError: The adapter's error when the stream ended with
                ``err``; ``unavailable`` when it ended before its final frame.
        """
        if not self.finished:
            raise _plugin_error().from_wire(
                "unavailable", "streamed reply ended before its final frame"
            )
        outcome = self._outcome
        if isinstance(outcome, dict):
            return outcome
        assert isinstance(outcome, _plugin_error())  # noqa: S101  (finished implies an outcome)
        raise outcome

    def _bytes_exceeded(self, used: int) -> PluginError:
        return _plugin_error().limit_exceeded(
            LimitExceeded(self._limits.bytes_limit, self._limits.max_bytes, used)
        )

    def _sequence(self, message: str) -> PluginError:
        return _plugin_error().from_wire("invalid_params", f"streamed reply {message}")

    def _admit_rows(self, count: int) -> None:
        """Admit a rows frame by its row count before any of its cells decode.

        Raises:
            PluginError: ``invalid_params`` when no statement is open,
                ``limit_exceeded`` when the rows would pass the row cap.
        """
        if self._open is None:
            raise self._sequence("carries rows before a statement frame")
        used = self._open_rows + count
        if 0 < self._limits.max_rows < used:
            raise _plugin_error().limit_exceeded(
                LimitExceeded(self._limits.rows_limit, self._limits.max_rows, used)
            )

    def _apply(self, frame: dict[str, Any]) -> None:
        kind = frame["kind"]
        value = frame["value"]
        if kind == "err":
            self._columns = []
            self._state = "finished"
            self._outcome = _plugin_error().from_wire(
                value["code"], value["message"], value["details"]
            )
            return
        if self._state == "awaitingHead":
            if kind != "head":
                raise self._sequence(f"opens with a {kind} frame instead of head")
            self._state = "inReply"
            self._operation_id = value["operationId"]
            self._statement_count = value["statementCount"]
            return
        if self._state == "finished":
            raise self._sequence("carries a frame after its final frame")
        if kind == "head":
            raise self._sequence("carries a second head frame")
        if kind == "statement":
            if value["index"] != self._next_index:
                raise self._sequence(
                    f"statement frame {value['index']} arrived where "
                    f"{self._next_index} was expected"
                )
            if self._next_index >= self._statement_count:
                raise self._sequence("carries more statements than its head announced")
            if self._open is not None:
                self._statements.append(self._open)
            self._open = {
                "rows": [],
                "columns": value["columns"],
                "rowsAffected": value["rowsAffected"],
            }
            self._open_rows = 0
            self._next_index += 1
            self._columns = value["columns"]
            return
        if kind == "rows":
            if self._open is None:
                raise self._sequence("carries rows before a statement frame")
            width = len(self._open["columns"])
            for row in value:
                if len(row["values"]) != width:
                    raise self._sequence(
                        f"carries a row of {len(row['values'])} cells for {width} columns"
                    )
                for cell in row["values"]:
                    _validate_cell(cell)
            self._open_rows += len(value)
            if 0 < self._limits.max_rows < self._open_rows:
                raise _plugin_error().limit_exceeded(
                    LimitExceeded(self._limits.rows_limit, self._limits.max_rows, self._open_rows)
                )
            if self._retain_rows:
                self._open["rows"].extend(value)
            return
        if kind == "end":
            if self._open is not None:
                self._statements.append(self._open)
                self._open = None
            if len(self._statements) != self._statement_count:
                raise self._sequence(
                    f"ended with {len(self._statements)} statements; "
                    f"its head announced {self._statement_count}"
                )
            self._columns = []
            self._state = "finished"
            self._outcome = {
                "operationId": self._operation_id,
                "statements": self._statements,
                "timing": value,
            }
            return
        raise self._sequence(f"carries an unknown {kind} frame")


def decode_execute_stream(data: bytes, limits: ExecuteStreamLimits) -> ExecuteReply:
    """Decode a whole frame sequence under ``limits``.

    Args:
        data: Every frame, concatenated.
        limits: Budget the stream is held to.

    Returns:
        The reassembled reply.

    Raises:
        PluginError: See :meth:`ExecuteStreamDecoder.push` and
            :meth:`ExecuteStreamDecoder.finish`.
    """
    decoder = ExecuteStreamDecoder(limits)
    decoder.push(data)
    return decoder.finish()


# ---------------------------------------------------------------------------
# Isolate adapter batch runner.
#
# A guest's ``executeAtomic`` reaches a database adapter wrapped in a durable
# replay receipt: a prune ``DELETE`` and a prior-receipt ``SELECT`` in front of
# the guest statements, every guest write gated on the receipt row being
# absent, and a stub ``INSERT`` that claims the receipt. The adapter persists
# the guest reply on that receipt before ``COMMIT``, so a retry with the same
# ``operationId`` after a lost reply replays the recorded result instead of
# re-running the writes. The helpers below do that once for every isolate
# adapter; they mirror ``bookclerk_db_exec::guest_receipt`` in the host.
# ---------------------------------------------------------------------------

GUEST_RECEIPT_WRAP_PREFIX = 2
"""Prune + prior-receipt ``SELECT`` statements ahead of the guest statements."""

GUEST_RECEIPT_STUB_SUFFIX = 1
"""Receipt stub ``INSERT`` after the guest statements."""

GUEST_REPLAY_PAYLOAD_MAX_BYTES = 65536
"""Largest replay payload, in UTF-8 bytes, a receipt stores for a write batch."""

GUEST_READ_ONLY_REPLAY_PAYLOAD = '{"readOnly":true}'
"""Payload stored for a read-only guest batch instead of its rows (reads run again on replay)."""

GUEST_RECEIPT_RESULT_LOST = "guest receipt committed without payload; original response lost"
"""Error text when a receipt committed but the reply it should hold was lost."""

GUEST_RECEIPT_CLAIM_LOST = "guest receipt claimed by a concurrent attempt; retry to replay it"
"""Error text when a concurrent attempt committed the receipt between this
attempt's prior-receipt ``SELECT`` and its stub ``INSERT``."""

STREAM_FRAME_TARGET_BYTES = 64 * 1024
"""Encoded size an encoder aims for per ``rows`` frame (one wider row may exceed it)."""

_RECEIPT_CLAIMED = "claimed"
"""Receipt status of an in-flight claim."""

_FINALIZE_SQL = (
    "UPDATE bookclerk_receipts SET payload = ?, status = 'ok' "
    "WHERE operation_id = ? AND status IN ('claimed', 'applied')"
)
"""Finalize ``UPDATE`` that stores the replay payload on the claimed receipt."""


class AdapterBatchRequest(TypedDict):
    """The parts of an ``AdapterExecuteRequest`` the batch runner reads.

    Attributes:
        operationId: Host-chosen idempotency key, echoed on the reply and
            keyed on the receipt.
        statements: Wrapped statements in execution order (proofs are not read).
        receipt: ``{"guestLen", "guestHash"}``; ``guestLen`` ``0`` means the
            batch carries no guest receipt.
    """

    operationId: str
    statements: list[TypedDbStatement]
    receipt: dict[str, Any]


class AdapterTransaction(Protocol):
    """One open transaction on the adapter's engine.

    The runner calls :meth:`run` once per statement in order, including the
    finalize statements it adds (canonical SQL with ``?`` placeholders, lowered
    by the adapter like any other statement), then :meth:`commit`, or
    :meth:`rollback` when any step failed.
    """

    async def run(self, statement: TypedDbStatement) -> StatementResult:
        """Run one statement inside the transaction.

        Args:
            statement: Canonical statement to lower and execute.

        Returns:
            The statement's result (rows only when ``resultSelection`` is ``rows``).
        """
        ...

    async def commit(self) -> None:
        """Commit the transaction."""
        ...

    async def rollback(self) -> None:
        """Roll the transaction back."""
        ...


async def execute_adapter_batch(
    request: AdapterBatchRequest,
    begin: Callable[[], Awaitable[AdapterTransaction]],
    timing_source: str,
) -> ExecuteReply:
    """Run one host batch in a transaction and commit it with its replay receipt.

    Args:
        request: The adapter request (``execute``).
        begin: Opens a transaction on the adapter's engine.
        timing_source: Observability source on the reply timing (``sqlite_txn``, …).

    Returns:
        The committed reply, wrap statements included (the host unwraps it).

    Raises:
        PluginError: ``unavailable`` when a prior receipt committed without its
            reply, ``internal`` when the wrap shape is malformed or the payload
            is over :data:`GUEST_REPLAY_PAYLOAD_MAX_BYTES`.
        Exception: Whatever a statement or ``commit`` raised, after the
            transaction was rolled back.
    """

    async def body(txn: AdapterTransaction) -> ExecuteReply:
        return await run_adapter_batch(request, txn, timing_source)

    return await _in_transaction(begin, body)


async def execute_adapter_batch_streamed(
    request: AdapterBatchRequest,
    begin: Callable[[], Awaitable[AdapterTransaction]],
    timing_source: str,
    limits: ExecuteStreamLimits,
) -> bytes:
    """Run one host batch for ``executeStreamed``; commit only when the host can take it.

    An isolate adapter's frames cross the invoke reply as one scalar. The
    frames are encoded inside the transaction: encoding stops with
    ``limit_exceeded`` (``maxScalarBytes``) as soon as they pass
    ``MAX_SCALAR_BYTES``, the finished frames are decoded under ``limits`` the
    way the host decodes them, and the reassembled reply's encoded size is
    held to ``limits.max_bytes`` the way the host validates it (the decoder
    alone allows framing slack past the budget). Any failure rolls the batch
    back, so nothing commits that the host would refuse.

    Args:
        request: The adapter request (``executeStreamed``).
        begin: Opens a transaction on the adapter's engine.
        timing_source: Observability source on the reply timing.
        limits: The streamed budget this adapter advertises
            (``execute_stream_limits(caps)``).

    Returns:
        Every frame of the committed reply, concatenated.

    Raises:
        PluginError: ``limit_exceeded`` when the frames do not fit one scalar
            or ``limits``, or what :func:`execute_adapter_batch` raises.
    """

    async def body(txn: AdapterTransaction) -> bytes:
        reply = await run_adapter_batch(request, txn, timing_source)
        frames = encode_execute_stream(reply, max_bytes=MAX_SCALAR_BYTES)
        decode_execute_stream(frames, limits)
        _require_reply_within_budget(reply, limits)
        return frames

    return await _in_transaction(begin, body)


def _require_reply_within_budget(reply: ExecuteReply, limits: ExecuteStreamLimits) -> None:
    """Hold a reply's encoded ``ExecuteReply`` size to the streamed budget.

    The host checks the reassembled reply this way. Each statement result
    encodes smaller than the whole reply, so this also covers the host's
    per-statement byte check under the same budget.

    Args:
        reply: The reply about to commit.
        limits: The advertised streamed budget (``max_bytes`` ``0`` =
            unlimited, as the host reads it).

    Raises:
        PluginError: ``limit_exceeded`` naming ``limits.bytes_limit`` when the
            encoded reply is larger than ``limits.max_bytes``.
    """
    if limits.max_bytes == 0:
        return
    msg = _CapnpMessage()
    _write_execute_reply(msg.init_root(0, 3), reply)
    used = len(msg.finish())
    if used > limits.max_bytes:
        raise _plugin_error().limit_exceeded(
            LimitExceeded(limits.bytes_limit, limits.max_bytes, used)
        )


async def _in_transaction(
    begin: Callable[[], Awaitable[AdapterTransaction]],
    body: Callable[[AdapterTransaction], Awaitable[Any]],
) -> Any:
    """Open a transaction, run ``body``, commit on success, roll back on failure.

    Args:
        begin: Opens the transaction.
        body: Work to run before ``COMMIT``.

    Returns:
        What ``body`` returned.

    Raises:
        Exception: What ``body`` or ``commit`` raised (a failing or cancelled
            rollback does not replace it).
    """
    txn = await begin()
    try:
        result = await body(txn)
        await txn.commit()
    except BaseException:
        # Shielded and awaited to the end, so a cancellation arriving now
        # cannot leave the transaction open or return before it closes; the
        # original failure is the one to report.
        rollback = asyncio.ensure_future(txn.rollback())
        while not rollback.done():
            with contextlib.suppress(Exception, asyncio.CancelledError):
                await asyncio.shield(rollback)
        raise
    return result


async def run_adapter_batch(
    request: AdapterBatchRequest, txn: AdapterTransaction, timing_source: str
) -> ExecuteReply:
    """Run the wrapped statements and the receipt finalize without committing.

    After the prior-receipt ``SELECT``, a completed receipt for this operation
    skips the remaining guest work (its results are padded so the wrap shape
    stays intact) and the host replays the stored reply.

    Args:
        request: The adapter request.
        txn: The open transaction (only :meth:`AdapterTransaction.run` is used).
        timing_source: Observability source on the reply timing.

    Returns:
        The reply to deliver once the transaction commits.

    Raises:
        PluginError: See :func:`execute_adapter_batch`; ``invalid_params`` when
            a result cell is not a valid ``DbValue``.
    """
    started = time.monotonic()
    statements_in = request["statements"]
    total = len(statements_in)
    guest_len = int(request["receipt"].get("guestLen", 0))
    guest_hash = str(request["receipt"].get("guestHash", ""))
    statements: list[StatementResult] = []
    for statement in statements_in:
        statements.append(await txn.run(statement))
        if guest_len > 0 and should_skip_remaining_guest_work(statements, total, guest_hash):
            while len(statements) < total:
                statements.append({"rows": [], "columns": [], "rowsAffected": 0})
            break
    # The host refuses a non-finite float64 or TEXT with U+0000; refuse it
    # here, before COMMIT, so the write and its receipt roll back.
    for result in statements:
        for row in result["rows"]:
            for cell in row["values"]:
                _validate_cell(cell)

    def timing() -> dict[str, Any]:
        elapsed = max(0, int((time.monotonic() - started) * 1_000_000))
        return {
            "attemptElapsedUs": elapsed,
            "dbExecutionUs": elapsed,
            "dbTimingSource": timing_source,
        }

    if guest_len > 0:
        # No prior receipt, yet the stub claimed nothing: another attempt
        # committed this operation's receipt after the prior-receipt SELECT,
        # and the gated writes saw it. Roll back so the host's retry replays it.
        if (
            len(statements) >= 2
            and not statements[1]["rows"]
            and statements[-1]["rowsAffected"] == 0
        ):
            raise _plugin_error().from_wire("unavailable", GUEST_RECEIPT_CLAIM_LOST)
        # A replay answers with the recorded timing, so record what ran.
        partial: ExecuteReply = {
            "operationId": request["operationId"],
            "statements": statements,
            "timing": timing(),
        }
        read_only = guest_batch_is_read_only(statements_in, guest_len)
        for finalize in guest_receipt_finalize_statements(
            partial, guest_len, guest_hash, read_only
        ):
            await txn.run(finalize)
    return {"operationId": request["operationId"], "statements": statements, "timing": timing()}


def guest_batch_is_read_only(statements: list[TypedDbStatement], guest_len: int) -> bool:
    """True when every guest statement inside the receipt wrap is a ``SELECT``.

    An empty or out-of-range guest slice is not read-only, so a write batch
    never skips its replay payload.

    Args:
        statements: The wrapped statements.
        guest_len: Guest statement count from the receipt hint.

    Returns:
        Whether the batch only reads.
    """
    end = GUEST_RECEIPT_WRAP_PREFIX + guest_len
    if guest_len == 0 or end > len(statements):
        return False
    return all(s["kind"] == "select" for s in statements[GUEST_RECEIPT_WRAP_PREFIX:end])


def guest_receipt_finalize_statements(
    partial: ExecuteReply, guest_len: int, guest_hash: str, read_only: bool
) -> list[TypedDbStatement]:
    """Statements that persist the guest reply on the claimed receipt.

    Empty when the prior receipt already holds a payload, belongs to a
    different request (the host reports the conflict), or is an in-flight
    claim whose stub this attempt did not insert.

    Args:
        partial: The wrapped reply so far (prefix, guest and stub results).
        guest_len: Guest statement count from the receipt hint.
        guest_hash: Guest ``requestHash`` from the receipt hint.
        read_only: Whether the guest slice only reads (:func:`guest_batch_is_read_only`).

    Returns:
        The finalize statements to run before ``COMMIT``.

    Raises:
        PluginError: ``unavailable`` when a prior receipt committed without its
            reply, ``internal`` when the wrap shape is malformed or the payload
            is over :data:`GUEST_REPLAY_PAYLOAD_MAX_BYTES`.
    """
    results = partial["statements"]
    expected = GUEST_RECEIPT_WRAP_PREFIX + guest_len + GUEST_RECEIPT_STUB_SUFFIX
    if len(results) != expected:
        raise _plugin_error().from_wire(
            "internal",
            f"guest atomic receipt wrap returned {len(results)} statements; expected {expected}",
        )
    prior = results[1]
    prior_hash = _receipt_hash(prior)
    if prior_hash is not None:
        if prior_hash != guest_hash:
            return []
        if _receipt_text(prior, "payload", 3) is not None:
            return []
        claimed = _receipt_text(prior, "status", 2) == _RECEIPT_CLAIMED
        stub_affected = results[-1]["rowsAffected"]
        if not claimed and stub_affected == 0:
            raise _plugin_error().from_wire("unavailable", GUEST_RECEIPT_RESULT_LOST)
        if claimed and stub_affected == 0:
            return []
    if read_only:
        payload = GUEST_READ_ONLY_REPLAY_PAYLOAD
    else:
        payload = guest_replay_payload(
            {
                "operationId": partial["operationId"],
                "statements": results[
                    GUEST_RECEIPT_WRAP_PREFIX : GUEST_RECEIPT_WRAP_PREFIX + guest_len
                ],
                "timing": partial["timing"],
            }
        )
    return [
        {
            "sql": _FINALIZE_SQL,
            "parameters": [
                {"kind": "text", "value": payload},
                {"kind": "text", "value": partial["operationId"]},
            ],
            "kind": "execute",
            "maxRows": 0,
            "resultSelection": "affectedRows",
        }
    ]


def should_skip_remaining_guest_work(
    results: list[StatementResult], total: int, guest_hash: str
) -> bool:
    """True when the prefix results show a completed prior receipt.

    An in-flight claim with the same hash and a read-only receipt with the
    same hash do not skip; a receipt with a different hash does (the host
    reports the conflict).

    Args:
        results: Results so far.
        total: Wrapped statement count.
        guest_hash: Guest ``requestHash`` from the receipt hint.

    Returns:
        Whether to pad the remaining results instead of running them.
    """
    if len(results) != GUEST_RECEIPT_WRAP_PREFIX or total <= GUEST_RECEIPT_WRAP_PREFIX:
        return False
    prior = results[1]
    prior_hash = _receipt_hash(prior)
    if prior_hash is None:
        return False
    same = prior_hash == guest_hash
    resumes_claim = same and _receipt_text(prior, "status", 2) == _RECEIPT_CLAIMED
    read_only_replay = same and _receipt_text(prior, "payload", 3) == GUEST_READ_ONLY_REPLAY_PAYLOAD
    return not resumes_claim and not read_only_replay


def guest_replay_payload(reply: ExecuteReply) -> str:
    """The replay payload for a guest reply, in the JSON form the host decodes.

    Args:
        reply: The guest slice of the reply.

    Returns:
        ``{"operation_id", "statements", "timing"}`` with each value in the
        host's serde form.

    Raises:
        PluginError: ``internal`` when a cell does not encode or the payload is
            over :data:`GUEST_REPLAY_PAYLOAD_MAX_BYTES`.
    """
    timing = reply["timing"]
    statements = ",".join(_statement_json(s) for s in reply["statements"])
    text = (
        f'{{"operation_id":{_json_text(reply["operationId"])},"statements":[{statements}],'
        f'"timing":{{"attemptElapsedUs":{_json_uint(timing["attemptElapsedUs"])},'
        f'"dbExecutionUs":{_json_uint(timing["dbExecutionUs"])},'
        f'"dbTimingSource":{_json_text(timing["dbTimingSource"])}}}}}'
    )
    size = len(text.encode("utf-8"))
    if size > GUEST_REPLAY_PAYLOAD_MAX_BYTES:
        raise _plugin_error().from_wire(
            "internal",
            f"guest replay payload is {size} bytes; max is {GUEST_REPLAY_PAYLOAD_MAX_BYTES}",
        )
    return text


def encode_execute_stream(
    reply: ExecuteReply,
    frame_target_bytes: int = STREAM_FRAME_TARGET_BYTES,
    max_bytes: int | None = None,
) -> bytes:
    """Frame ``reply`` as the byte sequence ``executeStreamed`` answers.

    ``head``, each statement with its rows split near ``frame_target_bytes``,
    then ``end``. A slice whose frame still exceeds one scalar is halved until
    each frame fits or holds one row.

    Args:
        reply: The committed reply.
        frame_target_bytes: Estimated encoded size to aim for per ``rows`` frame.
        max_bytes: Stop as soon as the frames pass this many bytes.

    Returns:
        Every frame, concatenated.

    Raises:
        PluginError: ``limit_exceeded`` (``maxScalarBytes``) when the frames
            pass ``max_bytes``.
    """
    statements = reply["statements"]
    out = bytearray()

    def push(frame: bytes) -> None:
        out.extend(frame)
        if max_bytes is not None and len(out) > max_bytes:
            raise _plugin_error().limit_exceeded(
                LimitExceeded("maxScalarBytes", max_bytes, len(out))
            )

    push(
        _stream_frame(
            "head", {"operationId": reply["operationId"], "statementCount": len(statements)}
        )
    )
    for index, stmt in enumerate(statements):
        push(
            _stream_frame(
                "statement",
                {"index": index, "columns": stmt["columns"], "rowsAffected": stmt["rowsAffected"]},
            )
        )
        rows = stmt["rows"]
        start = 0
        size = 0
        for i, row in enumerate(rows):
            estimate = _row_size_estimate(row)
            if size > 0 and size + estimate > frame_target_bytes:
                _push_rows_frames(push, rows[start:i])
                start = i
                size = 0
            size += estimate
        _push_rows_frames(push, rows[start:])
    push(_stream_frame("end", reply["timing"]))
    return bytes(out)


def _push_rows_frames(push: Callable[[bytes], None], rows: list[dict[str, Any]]) -> None:
    """Append ``rows`` as ``rows`` frames, halving a slice past one scalar.

    Args:
        push: Appends one frame.
        rows: Rows of the open statement.
    """
    if not rows:
        return
    encoded = _stream_frame("rows", rows)
    if len(encoded) > MAX_SCALAR_BYTES and len(rows) > 1:
        mid = len(rows) // 2
        _push_rows_frames(push, rows[:mid])
        _push_rows_frames(push, rows[mid:])
        return
    push(encoded)


def _stream_frame(kind: str, value: Any) -> bytes:
    """One encoded ``ExecuteStreamFrame`` message.

    Args:
        kind: Union member (``head``, ``statement``, ``rows``, ``end``).
        value: Its value.

    Returns:
        The single-segment Cap'n Proto bytes.
    """
    return _encode_message(_execute_stream_frame_codec, {"kind": kind, "value": value})


def _row_size_estimate(row: dict[str, Any]) -> int:
    """Rough wire size of one row: cell payloads plus pointer and tag words.

    Args:
        row: ``{"values": [...]}``.

    Returns:
        Estimated bytes.
    """
    size = 16
    for value in row["values"]:
        if value["kind"] == "text":
            size += len(value["value"].encode("utf-8")) + 24
        elif value["kind"] == "bytes":
            size += len(value["value"]) + 24
        else:
            size += 24
    return size


def _receipt_hash(prior: StatementResult) -> str | None:
    """The ``request_hash`` of a prior-receipt row (empty text still counts, as in the host).

    Args:
        prior: The prior-receipt ``SELECT`` result.

    Returns:
        The hash, or ``None`` when there is no row or the cell is not text.
    """
    cell = _receipt_cell(prior, "request_hash", 1)
    if cell is not None and cell.get("kind") == "text":
        return str(cell["value"])
    return None


def _receipt_text(prior: StatementResult, column: str, fallback: int) -> str | None:
    """A non-empty text cell of a prior-receipt row.

    Args:
        prior: The prior-receipt ``SELECT`` result.
        column: Column name (``status``, ``payload``).
        fallback: Position used when no column has that name.

    Returns:
        The text, or ``None`` when there is no row or the cell is not
        non-empty text.
    """
    cell = _receipt_cell(prior, column, fallback)
    if cell is not None and cell.get("kind") == "text" and cell.get("value"):
        return str(cell["value"])
    return None


def _receipt_cell(prior: StatementResult, column: str, fallback: int) -> dict[str, Any] | None:
    """One cell of a prior-receipt row, by column name or else by position.

    Args:
        prior: The prior-receipt ``SELECT`` result.
        column: Column name.
        fallback: Position used when no column has that name.

    Returns:
        The cell, or ``None`` when there is no row or no such cell.
    """
    if not prior["rows"]:
        return None
    values = prior["rows"][0]["values"]
    names = [str(c.get("name", "")).lower() for c in prior["columns"]]
    idx = names.index(column) if column in names else fallback
    return values[idx] if idx < len(values) else None


def _json_text(text: str) -> str:
    """A JSON string literal escaped as the host's serde encoder writes it."""
    return json.dumps(text, ensure_ascii=False)


def _json_uint(n: int) -> str:
    """A non-negative integer counter as JSON.

    Raises:
        PluginError: ``internal`` when ``n`` is not a ``u64``.
    """
    if isinstance(n, bool) or not isinstance(n, int) or n < 0 or n > 2**64 - 1:
        raise _plugin_error().from_wire(
            "internal", f"guest replay payload counter {n!r} is not a u64"
        )
    return str(n)


def _statement_json(stmt: StatementResult) -> str:
    """One statement result in serde form."""
    rows = ",".join(
        '{"values":[' + ",".join(_value_json(v) for v in row["values"]) + "]}"
        for row in stmt["rows"]
    )
    columns = ",".join(
        f'{{"name":{_json_text(c["name"])},"dbType":{_json_text(c["dbType"])}}}'
        for c in stmt["columns"]
    )
    return (
        f'{{"rows":[{rows}],"columns":[{columns}],'
        f'"rowsAffected":{_json_uint(stmt["rowsAffected"])}}}'
    )


def _value_json(value: DbValue) -> str:
    """One cell in serde form (``{"kind", "value"}``, bytes as a number array).

    Raises:
        PluginError: ``internal`` for a non-finite float or an unknown kind.
    """
    kind = value["kind"]
    raw: Any = value["value"]
    if kind == "null":
        return f'{{"kind":"null","value":{_json_text(raw)}}}'
    if kind == "boolean":
        return '{"kind":"boolean","value":' + ("true" if raw else "false") + "}"
    if kind == "int64":
        return f'{{"kind":"int64","value":{int(raw)}}}'
    if kind == "float64":
        if not math.isfinite(raw):
            raise _plugin_error().from_wire(
                "internal", "guest replay payload has a non-finite float"
            )
        return f'{{"kind":"float64","value":{json.dumps(float(raw))}}}'
    if kind == "text":
        return f'{{"kind":"text","value":{_json_text(raw)}}}'
    if kind == "bytes":
        return '{"kind":"bytes","value":[' + ",".join(str(b) for b in bytes(raw)) + "]}"
    raise _plugin_error().from_wire("internal", f"guest replay payload has an unknown {kind} cell")


_PROBE_STATEMENT: TypedDbStatement = {
    "sql": "",
    "parameters": [],
    "kind": "select",
    "maxRows": 0,
    "resultSelection": "rows",
}
"""The smallest statement a request can carry, used to measure the others."""

_RETRY_ID_PLACEHOLDER = "00000000-0000-4000-8000-000000000000"
"""A UUID-shaped ``operationId``, the widest the binding mints."""

_RETRY_HASH_PLACEHOLDER = "0" * 64
"""A SHA-256 hex ``requestHash``, the widest a retry token carries."""


def _request_bytes(operation_id: str, request_hash: str, statements: list[TypedDbStatement]) -> int:
    return len(
        encode_execute_request(
            {
                "operationId": operation_id,
                "requestHash": request_hash,
                "statements": statements,
                "deadlineUnixMs": 0,
            }
        )
    )


def chunk_indexes(
    typed: list[TypedDbStatement],
    max_statements: int,
    max_request_bytes: int,
) -> list[list[int]]:
    """Greedy split of ``typed`` into index runs that each fit one request.

    Sizes are estimated per statement against one probe statement and each
    run is verified by one exact encode; a run the estimate let through is
    halved until it fits, and a lone statement over the cap is yielded on
    its own.

    Args:
        typed: Statements to split.
        max_statements: Statements per request (``0`` = no cap).
        max_request_bytes: Encoded request bytes per request (``0`` = no cap).

    Returns:
        Index runs in order, covering every statement once.
    """
    if not typed:
        return []
    probe_request = _request_bytes("", "", [_PROBE_STATEMENT])
    probe_statement = _request_bytes("", "", [_PROBE_STATEMENT, _PROBE_STATEMENT]) - probe_request
    base = (
        _request_bytes(_RETRY_ID_PLACEHOLDER, _RETRY_HASH_PLACEHOLDER, [_PROBE_STATEMENT])
        - probe_statement
    )
    sizes = [_request_bytes("", "", [_PROBE_STATEMENT, stmt]) - probe_request for stmt in typed]

    def fits(run: list[int]) -> bool:
        if max_request_bytes <= 0:
            return True
        encoded = _request_bytes(
            _RETRY_ID_PLACEHOLDER, _RETRY_HASH_PLACEHOLDER, [typed[i] for i in run]
        )
        return encoded <= max_request_bytes

    out: list[list[int]] = []
    run: list[int] = []
    used = base
    for i, size in enumerate(sizes):
        over_statements = 0 < max_statements <= len(run)
        over_bytes = max_request_bytes > 0 and run and used + size > max_request_bytes
        if over_statements or over_bytes:
            _split_to_fit(run, fits, out)
            run = []
            used = base
        run.append(i)
        used += size
    if run:
        _split_to_fit(run, fits, out)
    return out


def _split_to_fit(run: list[int], fits: Callable[[list[int]], bool], out: list[list[int]]) -> None:
    """Append ``run`` to ``out`` as runs that each satisfy ``fits``, in order.

    A run the per-statement estimate let through is halved until ``fits``
    accepts it; a lone index is appended without asking, since it cannot be
    split further.
    """
    if len(run) <= 1 or fits(run):
        out.append(run)
        return
    half = (len(run) + 1) // 2
    _split_to_fit(run[:half], fits, out)
    _split_to_fit(run[half:], fits, out)


def canonical_execute_request_hash(request: ExecuteRequest) -> str:
    """SHA-256 hex of the Cap'n request with transport metadata cleared.

    ``operationId``, ``requestHash``, and ``deadlineUnixMs`` are omitted so a
    retry can refresh the remaining deadline. Matches the host digest.
    """
    canonical: ExecuteRequest = {
        **request,
        "operationId": "",
        "requestHash": "",
        "deadlineUnixMs": 0,
    }
    return hashlib.sha256(encode_execute_request(canonical)).hexdigest()


def _parse_bytes(value: Any) -> bytes:
    if isinstance(value, (bytes, bytearray)):
        return bytes(value)
    if not isinstance(value, str) or not value.startswith("b64:"):
        raise ValueError("bytes DbValue requires bytes")
    import base64

    return base64.b64decode(value[4:])


def _write_db_value(root: _CapnpStruct, value: DbValue) -> None:
    kind = value["kind"]
    if kind == "null":
        root.set_u16(0, _DB_TYPE_ORD[value["value"]])
        root.set_u16(1, 0)
        return
    if kind == "boolean":
        root.set_bool(0, value["value"])
        root.set_u16(1, 1)
        return
    if kind == "int64":
        n = value["value"]
        if n < I64_MIN or n > I64_MAX:
            raise ValueError("int64 DbValue is out of range")
        root.set_i64(1, n)
        root.set_u16(1, 2)
        return
    if kind == "float64":
        n = value["value"]
        if not math.isfinite(n):
            raise ValueError("float64 value is not finite")
        root.set_f64(1, n)
        root.set_u16(1, 3)
        return
    if kind == "text":
        _require_portable_text(value["value"])
        root.set_u16(1, 4)
        root.set_text(0, value["value"])
        return
    if kind == "bytes":
        root.set_u16(1, 5)
        root.set_data(0, value["value"])
        return
    raise ValueError(f"unknown DbValue union member: {kind}")


def _read_db_value(root: _StructReader) -> DbValue:
    disc = root.get_u16(1)
    if disc == 0:
        ty = _DB_TYPE_FROM[root.get_u16(0)]
        return {"kind": "null", "value": ty}
    if disc == 1:
        return {"kind": "boolean", "value": root.get_bool(0)}
    if disc == 2:
        return {"kind": "int64", "value": root.get_i64(1)}
    if disc == 3:
        n = root.get_f64(1)
        if not math.isfinite(n):
            raise ValueError("float64 value is not finite")
        return {"kind": "float64", "value": n}
    if disc == 4:
        return {"kind": "text", "value": _require_portable_text(root.get_text(0))}
    if disc == 5:
        return {"kind": "bytes", "value": root.get_data(0)}
    raise ValueError(f"unknown DbValue union member: {disc}")


def _write_statement(s: _CapnpStruct, stmt: TypedDbStatement) -> None:
    s.set_text(0, stmt["sql"])
    s.set_u16(0, _KIND_ORD[stmt["kind"]])
    s.set_u16(1, _SELECT_ORD[stmt["resultSelection"]])
    s.set_u32(1, stmt["maxRows"])
    params = s.init_struct_list(1, len(stmt["parameters"]), 2, 1)
    for i, param in enumerate(stmt["parameters"]):
        _write_db_value(params[i], param)


def _read_statement(s: _StructReader) -> TypedDbStatement:
    kind = _KIND_FROM[s.get_u16(0)]
    selection_raw = s.get_u16(1)
    selection = _SELECT_FROM[selection_raw] if selection_raw < len(_SELECT_FROM) else "rows"
    return {
        "sql": s.get_text(0),
        "parameters": [_read_db_value(p) for p in s.get_struct_list(1, 2, 1)],
        "kind": kind,
        "maxRows": s.get_u32(1),
        "resultSelection": selection,
    }
