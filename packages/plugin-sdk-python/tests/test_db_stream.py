"""Streamed execute replies: the shared `db_stream` vectors, the binding and `chunk()`."""

from __future__ import annotations

import unittest
from pathlib import Path
from typing import Any

from bookclerk_plugin_sdk._abi import MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES
from bookclerk_plugin_sdk._wire import _encode_message, _execute_stream_frame_codec
from bookclerk_plugin_sdk.db_value import (
    ExecuteStreamDecoder,
    ExecuteStreamLimits,
    chunk_indexes,
    create_database_binding,
    decode_execute_stream,
    encode_execute_request,
    execute_stream_limits,
    raw_stream_ceiling,
    streams_results,
)
from bookclerk_plugin_sdk.workerd import PluginError

CORPUS = Path(__file__).resolve().parents[3] / "fuzz" / "corpus" / "db_stream"

# The first-party sqlite advertisement as the host reads it (every value
# mirrors `DbCapabilities::advertised_sqlite()`).
SQLITE_CAPS: dict[str, Any] = {
    "sqlContractVersion": 1,
    "atomicBatch": True,
    "returning": True,
    "affectedRows": True,
    "schemaMigrations": True,
    "cancellation": True,
    "timing": True,
    "maxBinds": 999,
    "maxStatements": 100,
    "maxResultRows": 10_000,
    "maxPayloadBytes": MAX_SCALAR_BYTES,
    "maxResultBytes": MAX_SCALAR_BYTES,
    "maxCellBytes": MAX_SCALAR_BYTES,
    "maxRequestBytes": MAX_SCALAR_BYTES,
    "maxAtomicResultBytes": MAX_SCALAR_BYTES,
    "maxStreamedResultBytes": 64 * 1024 * 1024,
    "maxStreamedResultRows": 250_000,
    "pluginDatabases": True,
    "maxFunctionArgs": 127,
    "maxSchemaColumns": 2000,
    "maxPatternBytes": 50_000,
    "maxLoweredStatementBytes": 0,
    "consistentBackupRead": True,
    "atomicUnitRestore": True,
}
INLINE_ONLY_CAPS = {**SQLITE_CAPS, "maxStreamedResultBytes": 0, "maxStreamedResultRows": 0}
TINY_CAPS = {**SQLITE_CAPS, "maxStreamedResultBytes": 4096, "maxStreamedResultRows": 4}
TINY_LIMITS = ExecuteStreamLimits(4096, "maxStreamedResultBytes", 4, "maxStreamedResultRows")
TIMING = {"attemptElapsedUs": 5, "dbExecutionUs": 3, "dbTimingSource": "test"}
COLUMNS = [{"name": "id", "dbType": "int64"}, {"name": "title", "dbType": "text"}]

# Expected outcome per seed; a regression seed only has to hold the invariants.
EXPECTED: dict[str, dict[str, Any]] = {
    "reply_fits.bin": {"rows": 3},
    "reply_fits_tiny_chunks.bin": {"rows": 3},
    "inline_caps.bin": {"rows": 3},
    "adapter_error.bin": {"code": "unavailable"},
    "rows_over_tiny_cap.bin": {"code": "limit_exceeded", "limit": "maxStreamedResultRows"},
    "truncated.bin": {"code": "unavailable"},
    "bytes_after_end.bin": {"code": "invalid_params"},
}


def _limits_for(selector: int) -> ExecuteStreamLimits:
    """The budget the seed's first byte selects (mirrors `fuzz/oracles/db_stream.rs`)."""
    which = selector % 3
    if which == 0:
        return TINY_LIMITS
    if which == 1:
        return execute_stream_limits(SQLITE_CAPS)
    return execute_stream_limits(INLINE_ONLY_CAPS)


def _frame(kind: str, value: Any) -> bytes:
    return _encode_message(_execute_stream_frame_codec, {"kind": kind, "value": value})


def _row(i: int) -> dict[str, Any]:
    return {"values": [{"kind": "int64", "value": i}, {"kind": "text", "value": f"Title {i}"}]}


def _stream(operation_id: str, rows: list[dict[str, Any]], *, rows_per_frame: int = 2) -> bytes:
    frames = [
        _frame("head", {"operationId": operation_id, "statementCount": 1}),
        _frame("statement", {"index": 0, "columns": COLUMNS, "rowsAffected": 0}),
    ]
    for at in range(0, len(rows), rows_per_frame):
        frames.append(_frame("rows", rows[at : at + rows_per_frame]))
    frames.append(_frame("end", TIMING))
    return b"".join(frames)


def _err_stream(operation_id: str, code: str, message: str) -> bytes:
    return _frame("head", {"operationId": operation_id, "statementCount": 1}) + _frame(
        "err", {"code": code, "message": message, "details": ""}
    )


async def _chunked(data: bytes, size: int):
    for at in range(0, len(data), size):
        yield data[at : at + size]


def _inline_reply(request: dict[str, Any], rows: list[dict[str, Any]] | None = None) -> Any:
    return {
        "operationId": request["operationId"],
        "statements": [
            {"rows": rows or [], "columns": COLUMNS, "rowsAffected": 0}
            for _ in request["statements"]
        ],
        "timing": TIMING,
    }


class _Transport:
    """Counting fake of the three `GuestDatabase` projections."""

    def __init__(
        self,
        *,
        caps: dict[str, Any] | None = None,
        body: Any = None,
        inline: Any = None,
    ) -> None:
        self.caps = caps
        self.body = body
        self.inline = inline
        self.calls = {"inline": 0, "streamed": 0, "limits": 0}

    async def execute(self, request: dict[str, Any]) -> Any:
        self.calls["inline"] += 1
        if self.inline is None:
            raise AssertionError("inline execute must not be used")
        return self.inline(request)

    async def limits(self) -> Any:
        self.calls["limits"] += 1
        return self.caps

    async def execute_streamed(self, request: dict[str, Any]) -> Any:
        self.calls["streamed"] += 1
        return self.body(request)

    def binding(self) -> Any:
        return create_database_binding(
            self.execute,
            limits=self.limits if self.caps is not None else None,
            execute_streamed=self.execute_streamed if self.body is not None else None,
        )


class StreamLimitsTests(unittest.TestCase):
    def test_limits_follow_the_capabilities(self) -> None:
        self.assertTrue(streams_results(SQLITE_CAPS))
        self.assertFalse(streams_results(INLINE_ONLY_CAPS))
        self.assertEqual(
            execute_stream_limits(SQLITE_CAPS),
            ExecuteStreamLimits(
                64 * 1024 * 1024, "maxStreamedResultBytes", 250_000, "maxStreamedResultRows"
            ),
        )
        self.assertEqual(
            execute_stream_limits(INLINE_ONLY_CAPS),
            ExecuteStreamLimits(MAX_SCALAR_BYTES, "maxAtomicResultBytes", 10_000, "maxResultRows"),
        )
        self.assertEqual(raw_stream_ceiling(TINY_LIMITS), 4096 + 512 + MAX_STREAM_WINDOW_BYTES)


class CorpusVectorTests(unittest.TestCase):
    """The `db_stream` fuzz seeds decode here exactly as the host decodes them."""

    @staticmethod
    def _whole(stream: bytes, limits: ExecuteStreamLimits) -> tuple[Any, PluginError | None]:
        try:
            return decode_execute_stream(stream, limits), None
        except PluginError as err:
            return None, err

    @staticmethod
    def _split(
        stream: bytes, chunk: int, limits: ExecuteStreamLimits
    ) -> tuple[Any, PluginError | None]:
        decoder = ExecuteStreamDecoder(limits)
        try:
            for at in range(0, len(stream), chunk):
                decoder.push(stream[at : at + chunk])
            return decoder.finish(), None
        except PluginError as err:
            return None, err

    def test_seeds_hold_the_oracle_invariants_and_expected_outcomes(self) -> None:
        seeds = sorted(p for p in CORPUS.iterdir() if p.suffix == ".bin")
        self.assertGreaterEqual(len(seeds), len(EXPECTED), "db_stream corpus is incomplete")
        for path in seeds:
            data = path.read_bytes()
            limits = _limits_for(data[0])
            chunk = data[1] + 1
            stream = data[2:]
            whole, whole_err = self._whole(stream, limits)
            split_reply, split_err = self._split(stream, chunk, limits)
            if whole_err is None:
                self.assertIsNone(split_err, f"{path.name}: chunks of {chunk} refused the stream")
                self.assertEqual(split_reply, whole, f"{path.name}: chunked decode differs")
                if limits.max_rows > 0:
                    for stmt in whole["statements"]:
                        self.assertLessEqual(len(stmt["rows"]), limits.max_rows, path.name)
            else:
                self.assertIsNotNone(split_err, f"{path.name}: chunks accepted a refused stream")
                assert split_err is not None
                self.assertEqual(split_err.wire_code, whole_err.wire_code, path.name)

            expected = EXPECTED.get(path.name)
            if expected is None:
                continue
            if "rows" in expected:
                self.assertIsNone(whole_err, f"{path.name}: {whole_err}")
                self.assertEqual(whole["operationId"], "op-stream")
                self.assertEqual(len(whole["statements"]), 2)
                self.assertEqual(len(whole["statements"][0]["rows"]), expected["rows"])
                self.assertEqual(
                    [c["name"] for c in whole["statements"][0]["columns"]], ["id", "title"]
                )
                self.assertEqual(
                    whole["statements"][0]["rows"][2]["values"],
                    [{"kind": "int64", "value": 2}, {"kind": "text", "value": "Title 002"}],
                )
                self.assertEqual(
                    whole["statements"][1], {"rows": [], "columns": [], "rowsAffected": 2}
                )
                self.assertEqual(
                    whole["timing"],
                    {"attemptElapsedUs": 11, "dbExecutionUs": 7, "dbTimingSource": "sqlite_txn"},
                )
            else:
                self.assertIsNotNone(whole_err, f"{path.name}: expected {expected['code']}")
                assert whole_err is not None
                self.assertEqual(whole_err.wire_code, expected["code"], path.name)
                if "limit" in expected:
                    limit = whole_err.limit
                    assert limit is not None
                    self.assertEqual((limit.limit, limit.cap), (expected["limit"], limits.max_rows))


class DecoderTests(unittest.TestCase):
    def test_declared_frame_length_past_the_ceiling_fails_closed(self) -> None:
        decoder = ExecuteStreamDecoder(TINY_LIMITS)
        header = (0).to_bytes(4, "little") + (1 << 24).to_bytes(4, "little")
        with self.assertRaises(PluginError) as ctx:
            decoder.push(header)
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual(limit.limit, "maxStreamedResultBytes")

    def test_multi_segment_frame_is_invalid(self) -> None:
        decoder = ExecuteStreamDecoder(TINY_LIMITS)
        with self.assertRaises(PluginError) as ctx:
            decoder.push((1).to_bytes(4, "little") + (1).to_bytes(4, "little") + bytes(16))
        self.assertEqual(ctx.exception.code, "invalid_params")

    def test_frames_out_of_sequence_are_invalid(self) -> None:
        for frames, text in (
            ([_frame("end", TIMING)], "opens with a end frame"),
            (
                [
                    _frame("head", {"operationId": "op", "statementCount": 1}),
                    _frame("rows", [_row(1)]),
                ],
                "rows before a statement",
            ),
            (
                [
                    _frame("head", {"operationId": "op", "statementCount": 1}),
                    _frame("statement", {"index": 1, "columns": [], "rowsAffected": 0}),
                ],
                "statement frame 1 arrived where 0",
            ),
            (
                [_frame("head", {"operationId": "op", "statementCount": 2}), _frame("end", TIMING)],
                "ended with 0 statements",
            ),
        ):
            with self.assertRaises(PluginError) as ctx:
                decode_execute_stream(b"".join(frames), TINY_LIMITS)
            self.assertEqual(ctx.exception.code, "invalid_params")
            self.assertIn(text, str(ctx.exception))

    def test_retain_rows_false_counts_but_keeps_nothing(self) -> None:
        decoder = ExecuteStreamDecoder(TINY_LIMITS, retain_rows=False)
        frames = decoder.push(_stream("op-iter", [_row(i) for i in range(3)]))
        self.assertEqual([f["kind"] for f in frames], ["head", "statement", "rows", "rows", "end"])
        self.assertEqual(decoder.finish()["statements"][0]["rows"], [])
        over = ExecuteStreamDecoder(TINY_LIMITS, retain_rows=False)
        with self.assertRaises(PluginError) as ctx:
            over.push(_stream("op-iter", [_row(i) for i in range(5)]))
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.cap, limit.used), ("maxStreamedResultRows", 4, 5))

    def test_finish_before_the_end_is_unavailable(self) -> None:
        decoder = ExecuteStreamDecoder(TINY_LIMITS)
        decoder.push(_frame("head", {"operationId": "op", "statementCount": 1}))
        with self.assertRaises(PluginError) as ctx:
            decoder.finish()
        self.assertEqual(ctx.exception.code, "unavailable")


class BindingStreamTests(unittest.IsolatedAsyncioTestCase):
    async def test_all_reads_through_the_streamed_transport_once_limits_are_known(self) -> None:
        rows = [_row(i) for i in range(7)]
        transport = _Transport(caps=SQLITE_CAPS, body=lambda req: _stream(req["operationId"], rows))
        db = transport.binding()
        result = await db.prepare("SELECT id, title FROM books").all()
        self.assertEqual(len(result["results"]), 7)
        self.assertEqual(result["results"][6]["title"]["value"], "Title 6")
        await db.prepare("SELECT id, title FROM books").all()
        self.assertEqual(transport.calls, {"inline": 0, "streamed": 2, "limits": 1})

    async def test_chunked_body_is_reassembled(self) -> None:
        rows = [_row(i) for i in range(40)]
        transport = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _chunked(_stream(req["operationId"], rows, rows_per_frame=7), 13),
        )
        raw = await transport.binding().prepare("SELECT id, title FROM books").raw()
        self.assertEqual(len(raw), 40)
        self.assertEqual(raw[39][0]["value"], 39)

    async def test_inline_sessions_and_older_hosts_read_inline(self) -> None:
        for transport in (
            _Transport(
                caps=INLINE_ONLY_CAPS,
                body=lambda _req: b"",
                inline=lambda r: _inline_reply(r, [_row(1)]),
            ),
            _Transport(
                caps=None, body=lambda _req: b"", inline=lambda r: _inline_reply(r, [_row(1)])
            ),
            _Transport(caps=SQLITE_CAPS, inline=lambda r: _inline_reply(r, [_row(1)])),
        ):
            result = await transport.binding().prepare("SELECT id FROM books").all()
            self.assertEqual(len(result["results"]), 1)
            self.assertEqual((transport.calls["streamed"], transport.calls["inline"]), (0, 1))

    async def test_unsupported_falls_back_to_inline_and_other_errors_surface(self) -> None:
        def raise_unsupported(_req: Any) -> Any:
            raise PluginError.from_wire("unsupported", "GuestDatabase.executeStreamed")

        thrown = _Transport(
            caps=SQLITE_CAPS,
            body=raise_unsupported,
            inline=lambda r: _inline_reply(r, [_row(1), _row(2)]),
        )
        result = await thrown.binding().prepare("SELECT 1").all()
        self.assertEqual(len(result["results"]), 2)
        self.assertEqual(thrown.calls, {"inline": 1, "streamed": 1, "limits": 1})

        framed = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _err_stream(req["operationId"], "unsupported", "no stream"),
            inline=lambda r: _inline_reply(r, [_row(1)]),
        )
        result = await framed.binding().prepare("SELECT 1").all()
        self.assertEqual(len(result["results"]), 1)
        self.assertEqual(framed.calls, {"inline": 1, "streamed": 1, "limits": 1})

        failing = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _err_stream(
                req["operationId"],
                "limit_exceeded",
                "limit exceeded: maxStreamedResultBytes is 67108864; used 70000000",
            ),
        )
        with self.assertRaises(PluginError) as ctx:
            await failing.binding().prepare("SELECT 1").all()
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.used), ("maxStreamedResultBytes", 70_000_000))
        self.assertEqual(failing.calls["inline"], 0)

    async def test_the_receivers_budget_applies_to_the_reply(self) -> None:
        rows = [_row(i) for i in range(9)]
        transport = _Transport(caps=TINY_CAPS, body=lambda req: _stream(req["operationId"], rows))
        with self.assertRaises(PluginError) as ctx:
            await transport.binding().prepare("SELECT id FROM books").all()
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.cap), ("maxStreamedResultRows", 4))

    async def test_limits_is_cached_and_still_reports_the_hosts_error(self) -> None:
        transport = _Transport(caps=SQLITE_CAPS, body=lambda req: _stream(req["operationId"], []))
        db = transport.binding()
        self.assertEqual(await db.limits(), SQLITE_CAPS)
        await db.prepare("SELECT 1").all()
        self.assertEqual(transport.calls["limits"], 1)

        async def broken() -> Any:
            raise PluginError.from_wire("unavailable", "limits are down")

        db = create_database_binding(
            lambda r: _inline_reply_async(r),
            limits=broken,
            execute_streamed=transport.execute_streamed,
        )
        result = await db.prepare("SELECT 1").all()
        self.assertEqual(result["results"], [])
        with self.assertRaises(PluginError) as ctx:
            await db.limits()
        self.assertEqual(ctx.exception.code, "unavailable")

    async def test_iterate_yields_rows_as_frames_arrive(self) -> None:
        rows = [_row(i) for i in range(25)]
        transport = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _chunked(_stream(req["operationId"], rows, rows_per_frame=4), 50),
        )
        seen = [
            r["id"]["value"]
            async for r in transport.binding().prepare("SELECT id, title FROM books").iterate()
        ]
        self.assertEqual(seen, list(range(25)))
        self.assertEqual(transport.calls, {"inline": 0, "streamed": 1, "limits": 1})

        over = _Transport(caps=TINY_CAPS, body=lambda req: _stream(req["operationId"], rows))
        count = 0
        with self.assertRaises(PluginError) as ctx:
            async for _ in over.binding().prepare("SELECT id FROM books").iterate():
                count += 1
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual(limit.limit, "maxStreamedResultRows")
        self.assertLessEqual(count, 4)

        # The whole reply in one chunk completes every frame in one push.
        whole = _Transport(caps=SQLITE_CAPS, body=lambda req: _stream(req["operationId"], rows[:3]))
        seen = [
            r["id"]["value"]
            async for r in whole.binding().prepare("SELECT id FROM books").iterate()
        ]
        self.assertEqual(seen, [0, 1, 2])

        inline = _Transport(caps=INLINE_ONLY_CAPS, inline=lambda r: _inline_reply(r, rows[:3]))
        seen = [
            r["id"]["value"]
            async for r in inline.binding().prepare("SELECT id FROM books").iterate()
        ]
        self.assertEqual(seen, [0, 1, 2])

    async def test_chunk_splits_to_the_session_caps(self) -> None:
        transport = _Transport(caps={**SQLITE_CAPS, "maxStatements": 2}, inline=_inline_reply)
        db = transport.binding()
        stmts = [
            db.prepare("INSERT INTO t VALUES (?)").bind({"kind": "int64", "value": i})
            for i in range(5)
        ]
        sizes = []
        async for batch in db.chunk(stmts):
            sizes.append(len(batch))
            await db.batch(batch)
        self.assertEqual(sizes, [2, 2, 1])

        no_limits = create_database_binding(_inline_reply_async)
        sizes = [
            len(b) async for b in no_limits.chunk(stmts, max_statements=3, max_request_bytes=0)
        ]
        self.assertEqual(sizes, [3, 2])
        with self.assertRaises(PluginError) as ctx:
            async for _ in no_limits.chunk(stmts):
                pass
        self.assertEqual(ctx.exception.code, "unsupported")


async def _inline_reply_async(request: dict[str, Any]) -> Any:
    return _inline_reply(request)


class ChunkIndexesTests(unittest.TestCase):
    def test_runs_fit_the_byte_cap_and_cover_every_statement_once(self) -> None:
        def stmt(value: str) -> Any:
            return {
                "sql": "INSERT INTO t (v) VALUES (?)",
                "parameters": [{"kind": "text", "value": value}],
                "kind": "execute",
                "maxRows": 0,
                "resultSelection": "affectedRows",
            }

        typed = [stmt("x" * ((i * 37) % 200)) for i in range(30)]
        all_stmts = [*typed[:10], stmt("y" * 2000), *typed[10:]]
        cap = 600
        runs = chunk_indexes(all_stmts, 0, cap)
        self.assertEqual([i for run in runs for i in run], list(range(len(all_stmts))))
        for run in runs:
            if len(run) > 1:
                encoded = encode_execute_request(
                    {
                        "operationId": "00000000-0000-4000-8000-000000000000",
                        "requestHash": "0" * 64,
                        "statements": [all_stmts[i] for i in run],
                        "deadlineUnixMs": 0,
                    }
                )
                self.assertLessEqual(len(encoded), cap, run)
        self.assertEqual(next(run for run in runs if 10 in run), [10])
        self.assertLess(len(runs), len(all_stmts))
        self.assertEqual(chunk_indexes([], 5, 100), [])
        self.assertEqual(chunk_indexes(typed[:3], 0, 0), [[0, 1, 2]])
        self.assertEqual(chunk_indexes(typed[:5], 2, 0), [[0, 1], [2, 3], [4]])


if __name__ == "__main__":
    unittest.main()
