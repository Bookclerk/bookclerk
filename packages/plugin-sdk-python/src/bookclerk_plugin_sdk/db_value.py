"""Universal Cap'n database value domain (``DbValue``) and unpacked codec.

Baseline cells are typed null, bool, int64 (``int``), finite float64, UTF-8
text, and bytes (``bytes``). U+0000 is forbidden in TEXT and allowed in
BYTES. Unknown ``kind`` values fail closed. JSON
:func:`parse_db_value` still accepts a ``b64:`` string for bytes; the codec
always uses the domain types.
"""

from __future__ import annotations

import hashlib
import math
import uuid
from collections.abc import AsyncIterable, AsyncIterator, Awaitable, Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Literal, TypedDict

from ._abi import DB_RESULT_SELECTIONS, DB_STATEMENT_KINDS, DB_TYPES, MAX_STREAM_WINDOW_BYTES
from ._capnp import _CapnpMessage, _CapnpReader, _CapnpStruct, _StructReader
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
        self._limits_fetched = False
        self._cached_limits: DbCapabilities | None = None
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
            # The cached fetch failed: let the host's own error surface.
            return await self._limits()
        return caps

    async def _session_limits(self) -> DbCapabilities | None:
        """The session's limits, fetched once per binding (one invocation).

        Returns:
            The capabilities, or ``None`` when the host predates
            ``GuestDatabase.limits`` or the call failed; the binding then
            reads inline and :meth:`limits` reports the error itself.
        """
        if not self._limits_fetched:
            caps: DbCapabilities | None = None
            if self._limits is not None:
                try:
                    caps = await self._limits()
                except Exception:  # noqa: BLE001  (any failure means "read inline")
                    caps = None
            self._cached_limits = caps
            self._limits_fetched = True
        return self._cached_limits

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
        (``maxStatements`` / ``maxRequestBytes``) unless given here. A
        single statement larger than the request cap is yielded alone, so
        :meth:`batch` reports the typed error.

        Args:
            statements: Prepared statements (binds and intent already applied).
            max_statements: Statements per batch (``0`` = no cap); the
                session's ``maxStatements`` when ``None``.
            max_request_bytes: Encoded request bytes per batch (``0`` = no
                cap); the session's ``maxRequestBytes`` when ``None``.

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
        caps = (
            await self._session_limits()
            if max_statements is None or max_request_bytes is None
            else None
        )
        statements_cap = (
            max_statements
            if max_statements is not None
            else (caps["maxStatements"] if caps is not None else 0)
        )
        if max_request_bytes is not None:
            bytes_cap = max_request_bytes
        elif caps is not None:
            bytes_cap = caps["maxRequestBytes"]
        else:
            bytes_cap = self._max_request_bytes
        if statements_cap <= 0 and bytes_cap <= 0:
            raise _plugin_error().from_wire(
                "unsupported",
                "chunk needs the session limits (GuestDatabase.limits) "
                "or explicit max_statements / max_request_bytes",
            )
        typed = [stmt._as_typed() for stmt in statements]  # noqa: SLF001
        for indexes in chunk_indexes(typed, statements_cap, bytes_cap):
            yield [statements[i] for i in indexes]

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

    def _request_for(
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
        if self._max_request_bytes and len(encoded) > self._max_request_bytes:
            raise _plugin_error().limit_exceeded(
                LimitExceeded("maxRequestBytes", self._max_request_bytes, len(encoded))
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
        request = self._request_for(batch, retry)
        streamed = await self._open_streamed(request)
        if streamed is None:
            return await self._execute(request)
        chunks, caps = streamed
        decoder = ExecuteStreamDecoder(execute_stream_limits(caps))
        async for chunk in chunks:
            decoder.push(bytes(chunk))
        try:
            return decoder.finish()
        except _plugin_error() as err:
            if err.wire_code == "unsupported":
                return await self._execute(request)
            raise

    async def _inline_rows(self, request: ExecuteRequest) -> AsyncIterator[dict[str, Any]]:
        """Rows of a single-statement request read inline, from one ``execute``."""
        reply = await self._execute(request)
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
        request = self._request_for(batch, retry)
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
                if frame["kind"] == "statement":
                    columns = frame["value"]["columns"]
                elif frame["kind"] == "rows":
                    for row in frame["value"]:
                        yielded = True
                        yield _row_map(columns, row)
        try:
            decoder.finish()
        except _plugin_error() as err:
            if falls_back_inline(err, yielded):
                async for row in self._inline_rows(request):
                    yield row
                return
            raise


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
        while len(self._buf) >= 8:
            if int.from_bytes(self._buf[0:4], "little") != 0:
                raise _plugin_error().from_wire(
                    "invalid_params",
                    "streamed reply frame is not a single-segment Cap'n message",
                )
            frame_len = int.from_bytes(self._buf[4:8], "little") * 8 + 8
            if frame_len > ceiling:
                raise self._bytes_exceeded(frame_len)
            if len(self._buf) < frame_len:
                break
            try:
                frame = _decode_message(_execute_stream_frame_codec, bytes(self._buf[:frame_len]))
            except Exception as err:
                raise _plugin_error().from_wire(
                    "invalid_params", f"streamed reply frame does not decode: {err}"
                ) from err
            del self._buf[:frame_len]
            self._apply(frame)
            frames.append(frame)
            if self.finished:
                if self._buf:
                    raise self._sequence("carries bytes after its final frame")
                break
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

    def emit(run: list[int]) -> None:
        if len(run) <= 1 or fits(run):
            out.append(run)
            return
        half = (len(run) + 1) // 2
        emit(run[:half])
        emit(run[half:])

    run: list[int] = []
    used = base
    for i, size in enumerate(sizes):
        over_statements = 0 < max_statements <= len(run)
        over_bytes = max_request_bytes > 0 and run and used + size > max_request_bytes
        if over_statements or over_bytes:
            emit(run)
            run = []
            used = base
        run.append(i)
        used += size
    if run:
        emit(run)
    return out


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
