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
from collections.abc import Awaitable, Callable
from typing import Any, Literal, TypedDict, Union

from ._abi import DB_TYPES, DB_RESULT_SELECTIONS, DB_STATEMENT_KINDS
from ._capnp import _CapnpMessage, _CapnpReader, _CapnpStruct, _StructReader
from .guest_sql import guest_statement_kind, split_exec_queries

KINDS = frozenset({"null", "boolean", "int64", "float64", "text", "bytes"})
TYPES = frozenset(DB_TYPES)

I64_MIN = -(2**63)
I64_MAX = 2**63 - 1


def _require_portable_text(text: str) -> str:
    """Reject U+0000 in BookclerkSQL TEXT (allowed only in BYTES)."""
    if "\x00" in text:
        raise ValueError(
            "BookclerkSQL TEXT cannot contain U+0000 (use BYTES/BLOB for binary)"
        )
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


DbValue = Union[NullValue, BoolValue, Int64Value, Float64Value, TextValue, BytesValue]

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
    outcome: ExecuteReply | tuple[str, str],
) -> bytes:
    """Encode ``ExecuteResultReply`` (``ok`` reply or ``(code, message)`` err)."""
    msg = _CapnpMessage()
    root = msg.init_root(1, 1)
    if isinstance(outcome, tuple):
        root.set_u16(0, 1)
        err = root.init_struct(0, 0, 2)
        err.set_text(0, outcome[0])
        err.set_text(1, outcome[1])
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
        err = root.get_struct(0, 0, 2)
        from bookclerk_plugin_sdk.workerd import PluginError

        raise PluginError.from_wire(err.get_text(0), err.get_text(1))
    raise ValueError("unknown ExecuteResultReply union member")


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
            mapped.append(
                {col["name"]: cell for col, cell in zip(columns, cells, strict=False)}
            )
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
        max_request_bytes: int = 0,
        max_result_rows: int = 0,
        operation_id: str | None = None,
        request_hash: str = "",
        deadline_unix_ms: int = 0,
    ) -> None:
        """Create a binding over a host ``execute`` transport.

        Args:
            execute: Host session projection.
            max_request_bytes: Negotiated cap (``0`` = unlimited).
            max_result_rows: Default ``maxRows`` for :meth:`PreparedStatement.all`.
            operation_id: Default retry id; omitted calls mint a UUID.
            request_hash: Default retry hash; empty lets the host stamp one.
            deadline_unix_ms: Guest-visible deadline (unix ms).
        """
        self._execute = execute
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

    async def _execute_batch(
        self,
        batch: list[TypedDbStatement],
        *,
        retry: RetryToken | None = None,
    ) -> ExecuteReply:
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
            raise ValueError(
                f"atomic request is {len(encoded)} bytes; guest maxRequestBytes is "
                f"{self._max_request_bytes}"
            )
        return await self._execute(request)


AtomicTransport = Callable[[ExecuteRequest], Awaitable[ExecuteReply]]


def create_database_binding(
    transport: AtomicTransport,
    *,
    max_request_bytes: int = 0,
    max_result_rows: int = 0,
    operation_id: str | None = None,
    request_hash: str = "",
    deadline_unix_ms: int = 0,
) -> DatabaseBinding:
    """Build a host-mediated :class:`DatabaseBinding` over an async transport."""
    return DatabaseBinding(
        transport,
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
