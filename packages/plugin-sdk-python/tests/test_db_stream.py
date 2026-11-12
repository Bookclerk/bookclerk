"""Streamed execute replies: the shared `db_stream` vectors, the binding and `chunk()`."""

from __future__ import annotations

import asyncio
import math
import random
import struct
import unittest
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Any
from unittest import mock

from bookclerk_plugin_sdk import db_value
from bookclerk_plugin_sdk._abi import MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES
from bookclerk_plugin_sdk._capnp import _CapnpReader
from bookclerk_plugin_sdk._wire import (
    _decode_message,
    _encode_message,
    _execute_stream_frame_codec,
)
from bookclerk_plugin_sdk.db_value import (
    MAX_WRITE_KEY_BYTES,
    ExecuteStreamDecoder,
    ExecuteStreamLimits,
    _split_to_fit,
    chunk_indexes,
    create_database_binding,
    decode_execute_stream,
    encode_execute_request,
    execute_stream_limits,
    raw_stream_ceiling,
    streams_results,
    write_chunks_operation_id,
)
from bookclerk_plugin_sdk.workerd import PluginError, WriteChunksError

# The repository root: the nearest folder above holding the ABI crate. A
# fixed depth would break when mutation testing runs these tests from a copy
# under `mutants/` (docs/contributing/ci.md#python-sdk).
REPO_ROOT = next(
    p for p in Path(__file__).resolve().parents if (p / "crates" / "bookclerk-plugin-abi").is_dir()
)

CORPUS = REPO_ROOT / "fuzz" / "corpus" / "db_stream"

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


def _rows_with_unknown_cell(count: int) -> bytes:
    """A rows frame of ``count`` single-cell rows whose first cell has an unknown discriminant.

    The codec refuses the frame if it decodes the cells. The discriminant is
    located by encoding the first cell as ``int64 0`` and as ``boolean
    False``, which differ only in that field.
    """
    rest = [{"values": [{"kind": "int64", "value": 0}]} for _ in range(count - 1)]
    as_int = _frame("rows", [{"values": [{"kind": "int64", "value": 0}]}, *rest])
    as_bool = _frame("rows", [{"values": [{"kind": "boolean", "value": False}]}, *rest])
    assert len(as_int) == len(as_bool)
    at = next(i for i, (a, b) in enumerate(zip(as_int, as_bool, strict=True)) if a != b)
    out = bytearray(as_int)
    out[at] = 0xFF
    out[at + 1] = 0xFF
    return bytes(out)


def _zero_width_rows(count: int) -> bytes:
    """A rows frame whose list is ``count`` zero-width rows.

    A composite list of zero words that still carries its tag (the count, no
    data words, no pointer words).
    """
    words = [1 << 32 | 1 << 48, 2, 1 | (7 << 32), count << 2]
    return struct.pack("<II", 0, len(words)) + b"".join(struct.pack("<Q", w) for w in words)


async def _chunked(data: bytes, size: int) -> AsyncIterator[bytes]:
    for at in range(0, len(data), size):
        yield data[at : at + size]


def _mutate(stream: bytes, rand: random.Random) -> tuple[str, bytes]:
    at = rand.randrange(len(stream)) if stream else 0
    choice = rand.randrange(4)
    if choice == 0:
        out = bytearray(stream)
        if out:
            out[at] ^= 1 << rand.randrange(8)
        return "flip", bytes(out)
    if choice == 1:
        return "truncate", stream[:at]
    if choice == 2:
        return "insert", stream[:at] + bytes([rand.randrange(256)]) + stream[at:]
    end = at + rand.randrange(max(1, len(stream) - at) + 1)
    return "duplicate", stream[:end] + stream[at:end] + stream[end:]


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

    def test_derived_mutants_hold_the_oracle_invariants(self) -> None:
        """A fixed PRNG derives mutants of every seed so this decoder meets the
        shapes the fuzzer finds, on every run: a flipped byte, a truncation, an
        inserted byte and a duplicated slice."""
        rand = random.Random(0x5EEDD8)
        seeds = sorted(p for p in CORPUS.iterdir() if p.suffix == ".bin")
        mutants = 0
        accepted = 0
        for path in seeds:
            data = path.read_bytes()
            limits = _limits_for(data[0])
            for i in range(24):
                kind, stream = _mutate(data[2:], rand)
                chunk = rand.randint(1, 64)
                label = f"{path.name} {kind} #{i} (chunks of {chunk})"
                whole, whole_err = self._whole(stream, limits)
                split_reply, split_err = self._split(stream, chunk, limits)
                mutants += 1
                if whole_err is None:
                    accepted += 1
                    self.assertIsNone(split_err, f"{label}: chunked decode refused the stream")
                    self.assertEqual(split_reply, whole, f"{label}: chunked decode differs")
                    if limits.max_rows > 0:
                        for stmt in whole["statements"]:
                            self.assertLessEqual(len(stmt["rows"]), limits.max_rows, label)
                else:
                    self.assertIsNotNone(split_err, f"{label}: chunks accepted a refused stream")
                    assert split_err is not None
                    self.assertEqual(split_err.wire_code, whole_err.wire_code, label)
        self.assertGreaterEqual(mutants, 24 * len(EXPECTED))
        self.assertGreater(accepted, 0, "some mutants still decode")


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

    def test_empty_pushes_finished_streams_and_undecodable_frames(self) -> None:
        decoder = ExecuteStreamDecoder(TINY_LIMITS)
        self.assertEqual(decoder.push(b""), [])
        decoder.push(_frame("head", {"operationId": "op", "statementCount": 1}))
        decoder.push(_frame("statement", {"index": 0, "columns": COLUMNS, "rowsAffected": 0}))
        self.assertEqual(decoder.columns, COLUMNS)
        decoder.push(_frame("end", TIMING))
        self.assertTrue(decoder.finished)
        self.assertEqual(decoder.columns, [])
        with self.assertRaises(PluginError) as ctx:
            decoder.push(b"\x00")
        self.assertEqual(ctx.exception.code, "invalid_params")
        self.assertIn("bytes after its final frame", str(ctx.exception))

        garbage = ExecuteStreamDecoder(TINY_LIMITS)
        framed = (0).to_bytes(4, "little") + (2).to_bytes(4, "little") + b"\xff" * 16
        with self.assertRaises(PluginError) as ctx:
            garbage.push(framed)
        self.assertEqual(ctx.exception.code, "invalid_params")
        self.assertIn("does not decode", str(ctx.exception))

    def test_total_bytes_past_the_ceiling_fail_before_parsing(self) -> None:
        from bookclerk_plugin_sdk.db_value import raw_stream_ceiling

        decoder = ExecuteStreamDecoder(TINY_LIMITS)
        ceiling = raw_stream_ceiling(TINY_LIMITS)
        with self.assertRaises(PluginError) as ctx:
            decoder.push(bytes(ceiling + 1))
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.used), ("maxStreamedResultBytes", ceiling + 1))

    def test_repeated_heads_and_extra_statements_are_invalid(self) -> None:
        head = _frame("head", {"operationId": "op", "statementCount": 1})
        for frames, text in (
            ([head, head], "second head frame"),
            (
                [
                    head,
                    _frame("statement", {"index": 0, "columns": COLUMNS, "rowsAffected": 0}),
                    _frame("statement", {"index": 1, "columns": COLUMNS, "rowsAffected": 0}),
                ],
                "more statements than its head announced",
            ),
        ):
            with self.assertRaises(PluginError) as ctx:
                decode_execute_stream(b"".join(frames), TINY_LIMITS)
            self.assertEqual(ctx.exception.code, "invalid_params")
            self.assertIn(text, str(ctx.exception))

    def test_a_wide_row_frame_decodes_under_the_streams_budget(self) -> None:
        wide = [{"name": name, "dbType": "text"} for name in "abc"]
        cells = [{"kind": "text", "value": c["name"] * (400 * 1024)} for c in wide]
        stream = b"".join(
            [
                _frame("head", {"operationId": "op-wide", "statementCount": 1}),
                _frame("statement", {"index": 0, "columns": wide, "rowsAffected": 0}),
                _frame("rows", [{"values": cells}]),
                _frame("end", TIMING),
            ]
        )
        self.assertGreater(len(stream), 1024 * 1024)
        limits = execute_stream_limits(SQLITE_CAPS)
        reply = decode_execute_stream(stream, limits)
        self.assertEqual(len(reply["statements"][0]["rows"][0]["values"][2]["value"]), 400 * 1024)
        decoder = ExecuteStreamDecoder(limits)
        for at in range(0, len(stream), 4096):
            decoder.push(stream[at : at + 4096])
        self.assertEqual(decoder.finish(), reply)
        # The generic reader keeps its budget for control messages.
        with self.assertRaisesRegex(ValueError, "traversal budget"):
            _decode_message(_execute_stream_frame_codec, _frame("rows", [{"values": cells}]))
        # Under a budget the frame does not fit, the declared length fails closed.
        with self.assertRaises(PluginError) as ctx:
            decode_execute_stream(stream, TINY_LIMITS)
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual(limit.limit, "maxStreamedResultBytes")

        many = _stream("op-many", [_row(i) for i in range(3000)], rows_per_frame=1)
        decoder = ExecuteStreamDecoder(limits, retain_rows=False)
        frames = decoder.push(many)
        self.assertEqual(sum(1 for f in frames if f["kind"] == "rows"), 3000)
        self.assertTrue(decoder.finished)

    def test_the_traversal_budget_is_cumulative(self) -> None:
        # Two pointers alias one 1000-word text: reading it twice exceeds a
        # budget that holds the message once, so aliasing cannot amplify a frame.
        text_bytes = 8000
        words = 3 + text_bytes // 8 + 1
        list_ptr = lambda offset: 1 | (offset << 2) | (2 << 32) | ((text_bytes + 1) << 35)  # noqa: E731
        data = struct.pack("<II", 0, words)
        data += struct.pack("<QQQ", 2 << 48, list_ptr(1), list_ptr(0))
        data += b"a" * text_bytes + bytes(8)
        self.assertEqual(len(data), 8 + words * 8)
        once = _CapnpReader(data, words + 16).root(0, 2)
        self.assertEqual(len(once.get_text(0)), text_bytes)
        twice = _CapnpReader(data, words + 16).root(0, 2)
        twice.get_text(0)
        with self.assertRaisesRegex(ValueError, "traversal budget"):
            twice.get_text(1)
        roomy = _CapnpReader(data, words * 2 + 16).root(0, 2)
        self.assertEqual(len(roomy.get_text(1)), text_bytes)

    def test_apply_keeps_the_guards_the_admission_check_runs_first(self) -> None:
        # `_push` admits rows frames by count before their cells decode, so
        # these guards in `_apply` are reached only by a frame that bypassed
        # admission; they still hold, as does the order of the frames.
        decoder = ExecuteStreamDecoder(TINY_LIMITS)
        decoder._apply({"kind": "head", "value": {"operationId": "op", "statementCount": 1}})
        with self.assertRaises(PluginError) as ctx:
            decoder._apply({"kind": "rows", "value": [_row(0)]})
        self.assertIn("rows before a statement frame", str(ctx.exception))
        decoder._apply(
            {"kind": "statement", "value": {"index": 0, "columns": COLUMNS, "rowsAffected": 0}}
        )
        with self.assertRaises(PluginError) as ctx:
            decoder._apply({"kind": "rows", "value": [_row(i) for i in range(5)]})
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.cap, limit.used), ("maxStreamedResultRows", 4, 5))
        with self.assertRaises(PluginError) as ctx:
            decoder._apply({"kind": "stats", "value": None})
        self.assertIn("unknown stats frame", str(ctx.exception))
        decoder._apply({"kind": "end", "value": TIMING})
        self.assertTrue(decoder.finished)
        with self.assertRaises(PluginError) as ctx:
            decoder._apply({"kind": "rows", "value": [_row(0)]})
        self.assertIn("after its final frame", str(ctx.exception))

    def test_list_readers_charge_their_elements_to_the_budget(self) -> None:
        # Hand built: word 0 points at two text pointers (words 1-2, texts at
        # words 3-4), word 5 at a List(UInt16) in word 6, word 7 at a
        # List(Bool) in word 8.
        list_ptr = lambda offset, size, length: 1 | (offset << 2) | (size << 32) | (length << 35)  # noqa: E731
        text = b"ab\x00" + bytes(5)
        words = [
            struct.pack("<Q", list_ptr(0, 6, 2)),
            struct.pack("<Q", list_ptr(1, 2, 3)),
            struct.pack("<Q", list_ptr(1, 2, 3)),
            text,
            text,
            struct.pack("<Q", list_ptr(0, 3, 3)),
            struct.pack("<HHHH", 1, 2, 3, 0),
            struct.pack("<Q", list_ptr(0, 1, 10)),
            struct.pack("<Q", 0b1010101011),
        ]
        data = struct.pack("<II", 0, len(words)) + b"".join(words)
        budget = len(words) + 4
        reader = _CapnpReader(data, budget)
        self.assertEqual(reader.read_text_list(0), ["ab", "ab"])
        self.assertEqual(reader.read_data_list(0), [b"ab\x00", b"ab\x00"])
        self.assertEqual(reader.read_uint_list(5, 2), [1, 2, 3])
        self.assertEqual(
            reader.read_bool_list(7),
            [True, True, False, True, False, True, False, True, False, True],
        )
        # The budget is cumulative: the lists read so far charged 10 of the 13
        # words, so reading the two texts again (4 more) is refused.
        with self.assertRaisesRegex(ValueError, "traversal budget"):
            reader.read_text_list(0)
        # A pointer list longer than the budget fails before any element is visited.
        words[0] = struct.pack("<Q", list_ptr(0, 6, len(words) + 1))
        over = struct.pack("<II", 0, len(words)) + b"".join(words)
        with self.assertRaisesRegex(ValueError, "pointer list exceeds traversal budget"):
            _CapnpReader(over, len(words)).read_text_list(0)

    def test_a_failed_push_is_latched(self) -> None:
        decoder = ExecuteStreamDecoder(execute_stream_limits(SQLITE_CAPS))
        with self.assertRaises(PluginError) as ctx:
            decoder.push(_stream("op-latch", [_row(0)]) + b"\x00")
        self.assertEqual(ctx.exception.code, "invalid_params")
        self.assertTrue(decoder.finished)
        with self.assertRaises(PluginError) as ctx:
            decoder.finish()
        self.assertEqual(ctx.exception.code, "invalid_params")

    def test_row_cap_is_checked_before_cells_decode(self) -> None:
        # A frame past the cap whose cells would not even validate is refused
        # as `limit_exceeded`, and a rows frame ahead of any statement as out
        # of sequence: neither materializes its cells.
        bad = {"kind": "text", "value": "a\x00b"}
        head = _frame("head", {"operationId": "op-order", "statementCount": 1})
        statement = _frame(
            "statement",
            {"index": 0, "columns": [{"name": "v", "dbType": "int64"}], "rowsAffected": 0},
        )
        # The frame's first cell does not even decode, so refusing it as
        # `limit_exceeded` shows the count was checked before any cell was read.
        with self.assertRaisesRegex(ValueError, "union"):
            _decode_message(_execute_stream_frame_codec, _rows_with_unknown_cell(5))
        with self.assertRaises(PluginError) as ctx:
            ExecuteStreamDecoder(TINY_LIMITS).push(head + statement + _rows_with_unknown_cell(5))
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.cap, limit.used), ("maxStreamedResultRows", 4, 5))
        # Under the cap the same frame is refused for what it is.
        with self.assertRaisesRegex(PluginError, "does not decode"):
            ExecuteStreamDecoder(TINY_LIMITS).push(head + statement + _rows_with_unknown_cell(3))
        # Zero-width rows occupy no words, yet their count is read from the
        # tag: five of them pass the cap of four, three are rows of no cells.
        with self.assertRaises(PluginError) as ctx:
            ExecuteStreamDecoder(TINY_LIMITS).push(head + statement + _zero_width_rows(5))
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.used), ("maxStreamedResultRows", 5))
        with self.assertRaisesRegex(PluginError, "row of 0 cells for 1 columns"):
            ExecuteStreamDecoder(TINY_LIMITS).push(head + statement + _zero_width_rows(3))
        self.assertEqual(_CapnpReader(_zero_width_rows(3)).root(1, 1).get_struct_list_length(0), 3)
        self.assertEqual(
            len(_CapnpReader(_zero_width_rows(3)).root(1, 1).get_struct_list(0, 0, 0)), 3
        )
        # Zero-width elements are charged to the traversal budget by count.
        with self.assertRaisesRegex(ValueError, "traversal budget"):
            _CapnpReader(_zero_width_rows(1000), 16).root(1, 1).get_struct_list_length(0)
        with self.assertRaisesRegex(ValueError, "traversal budget"):
            _CapnpReader(_zero_width_rows(1000), 1000).root(1, 1).get_struct_list(0, 0, 0)
        # A row of the wrong width for its statement's columns is out of sequence.
        with self.assertRaisesRegex(PluginError, "row of 2 cells for 1 columns"):
            ExecuteStreamDecoder(TINY_LIMITS).push(
                head + statement + _frame("rows", [{"values": [bad, bad]}])
            )
        with self.assertRaisesRegex(PluginError, "before a statement frame"):
            ExecuteStreamDecoder(TINY_LIMITS).push(head + _frame("rows", [{"values": [bad]}]))
        # The count comes from the list tag alone, and a tag that announces
        # more elements than the list holds is refused.
        rows = _frame("rows", [_row(0), _row(1), _row(2)])
        self.assertEqual(_CapnpReader(rows).root(1, 1).get_struct_list_length(0), 3)
        self.assertEqual(_CapnpReader(_frame("rows", [])).root(1, 1).get_struct_list_length(0), 0)
        words = [
            1 << 48,  # root: struct, 0 data words, 1 pointer
            1 | (7 << 32) | (1 << 35),  # composite list of one word (its tag)
            (3 << 2) | (1 << 32),  # tag: 3 elements of one word each
        ]
        lying = struct.pack("<II", 0, len(words)) + b"".join(struct.pack("<Q", w) for w in words)
        with self.assertRaisesRegex(ValueError, "exceeds payload"):
            _CapnpReader(lying).root(0, 1).get_struct_list_length(0)
        with self.assertRaisesRegex(PluginError, "does not decode"):
            ExecuteStreamDecoder(TINY_LIMITS).push(head + lying)

    def test_streamed_cells_are_validated_like_inline_ones(self) -> None:
        for cell, text in [
            ({"kind": "float64", "value": math.nan}, "not finite"),
            ({"kind": "text", "value": "a\x00b"}, "U\\+0000"),
        ]:
            stream = b"".join(
                [
                    _frame("head", {"operationId": "op-cell", "statementCount": 1}),
                    _frame(
                        "statement",
                        {
                            "index": 0,
                            "columns": [{"name": "v", "dbType": "text"}],
                            "rowsAffected": 0,
                        },
                    ),
                    _frame("rows", [{"values": [cell]}]),
                    _frame("end", TIMING),
                ]
            )
            with self.assertRaisesRegex(PluginError, text) as ctx:
                decode_execute_stream(stream, execute_stream_limits(SQLITE_CAPS))
            self.assertEqual(ctx.exception.code, "invalid_params")

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

    async def test_iterate_closes_the_chunk_source_when_the_consumer_stops(self) -> None:
        rows = [_row(i) for i in range(25)]
        closed: list[bool] = []

        async def source(data: bytes) -> AsyncIterator[bytes]:
            try:
                async for c in _chunked(data, 50):
                    yield c
            finally:
                closed.append(True)

        transport = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: source(_stream(req["operationId"], rows, rows_per_frame=4)),
        )
        it = transport.binding().prepare("SELECT id FROM books").iterate()
        self.assertEqual((await it.__anext__())["id"]["value"], 0)
        self.assertEqual(closed, [])
        await it.aclose()
        self.assertEqual(closed, [True])

        # A refused frame closes the source too.
        closed.clear()
        over = _Transport(
            caps=TINY_CAPS, body=lambda req: source(_stream(req["operationId"], rows))
        )
        with self.assertRaises(PluginError):
            async for _ in over.binding().prepare("SELECT id FROM books").iterate():
                pass
        self.assertEqual(closed, [True])

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

    async def test_iterate_falls_back_inline_only_before_the_first_row(self) -> None:
        rows = [_row(i) for i in range(3)]
        fell = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _err_stream(req["operationId"], "unsupported", "no stream"),
            inline=lambda r: _inline_reply(r, rows),
        )
        seen = [r["id"]["value"] async for r in fell.binding().prepare("SELECT id").iterate()]
        self.assertEqual(seen, [0, 1, 2])
        self.assertEqual(fell.calls, {"inline": 1, "streamed": 1, "limits": 1})

        # Chunked delivery takes the same path through `finish()`.
        late = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _chunked(
                _err_stream(req["operationId"], "unsupported", "no stream"), 3
            ),
            inline=lambda r: _inline_reply(r, rows[:1]),
        )
        seen = [r["id"]["value"] async for r in late.binding().prepare("SELECT id").iterate()]
        self.assertEqual(seen, [0])
        self.assertEqual(late.calls, {"inline": 1, "streamed": 1, "limits": 1})

        after_rows = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: b"".join(
                [
                    _frame("head", {"operationId": req["operationId"], "statementCount": 1}),
                    _frame("statement", {"index": 0, "columns": COLUMNS, "rowsAffected": 0}),
                    _frame("rows", rows),
                    _frame("err", {"code": "unsupported", "message": "gave up", "details": ""}),
                ]
            ),
        )
        count = 0
        with self.assertRaises(PluginError) as ctx:
            async for _ in after_rows.binding().prepare("SELECT id").iterate():
                count += 1
        self.assertEqual(ctx.exception.code, "unsupported")
        self.assertEqual(count, 3)
        self.assertEqual(after_rows.calls["inline"], 0)

        unavailable = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _err_stream(req["operationId"], "unavailable", "down"),
        )
        with self.assertRaises(PluginError) as ctx:
            async for _ in unavailable.binding().prepare("SELECT id").iterate():
                pass
        self.assertEqual(ctx.exception.code, "unavailable")
        self.assertEqual(unavailable.calls["inline"], 0)

    async def test_iterate_falls_back_inline_when_a_push_reports_unsupported(self) -> None:
        # `unsupported` raised while frames decode, before any row was handed
        # out, takes the same inline fallback as an `unsupported` err frame.
        class Refusing(ExecuteStreamDecoder):
            def push(self, data: bytes) -> list[dict[str, Any]]:
                raise PluginError.from_wire("unsupported", "no stream")

        rows = [_row(i) for i in range(2)]
        transport = _Transport(
            caps=SQLITE_CAPS,
            body=lambda req: _stream(req["operationId"], rows),
            inline=lambda r: _inline_reply(r, rows),
        )
        with mock.patch.object(db_value, "ExecuteStreamDecoder", Refusing):
            seen = [
                r["id"]["value"] async for r in transport.binding().prepare("SELECT id").iterate()
            ]
        self.assertEqual(seen, [0, 1])
        self.assertEqual(transport.calls, {"inline": 1, "streamed": 1, "limits": 1})

    async def test_replies_must_answer_the_request(self) -> None:
        other = create_database_binding(
            lambda r: _reply_async({**_inline_reply(r), "operationId": "someone-else"})
        )
        with self.assertRaises(PluginError) as ctx:
            await other.prepare("SELECT 1").all()
        self.assertEqual(ctx.exception.code, "invalid_params")
        short = create_database_binding(
            lambda r: _reply_async({**_inline_reply(r), "statements": []})
        )
        with self.assertRaises(PluginError) as ctx:
            await short.prepare("SELECT 1").all()
        self.assertEqual(ctx.exception.code, "invalid_params")
        streamed = _Transport(caps=SQLITE_CAPS, body=lambda _r: _stream("someone-else", [_row(0)]))
        with self.assertRaises(PluginError) as ctx:
            await streamed.binding().prepare("SELECT 1").all()
        self.assertEqual(ctx.exception.code, "invalid_params")
        with self.assertRaises(PluginError) as ctx:
            async for _ in streamed.binding().prepare("SELECT 1").iterate():
                pass
        self.assertEqual(ctx.exception.code, "invalid_params")

    async def test_a_later_limits_success_replaces_the_cached_failure(self) -> None:
        attempts = {"n": 0}
        rows = [_row(0), _row(1)]

        async def limits() -> Any:
            attempts["n"] += 1
            if attempts["n"] == 1:
                raise PluginError.from_wire("unavailable", "limits are down")
            return SQLITE_CAPS

        async def streamed(request: dict[str, Any]) -> bytes:
            return _stream(request["operationId"], rows[:1])

        db = create_database_binding(
            lambda r: _reply_async(_inline_reply(r, rows)), limits=limits, execute_streamed=streamed
        )
        self.assertEqual(len((await db.prepare("SELECT 1").all())["results"]), 2)
        self.assertEqual(await db.limits(), SQLITE_CAPS)
        self.assertEqual(len((await db.prepare("SELECT 1").all())["results"]), 1)
        self.assertEqual(attempts["n"], 2)

    async def test_overlapping_first_reads_share_one_limits_fetch(self) -> None:
        transport = _Transport(caps=SQLITE_CAPS, body=lambda r: _stream(r["operationId"], []))
        db = transport.binding()
        await asyncio.gather(db.prepare("SELECT 1").all(), db.prepare("SELECT 2").all())
        self.assertEqual(transport.calls["limits"], 1)

    async def test_a_cancelled_read_leaves_the_shared_limits_fetch_usable(self) -> None:
        gate = asyncio.Event()
        transport = _Transport(caps=SQLITE_CAPS, body=lambda r: _stream(r["operationId"], []))

        async def slow_limits() -> Any:
            await gate.wait()
            return await transport.limits()

        db = create_database_binding(
            transport.execute, limits=slow_limits, execute_streamed=transport.execute_streamed
        )
        first = asyncio.ensure_future(db.prepare("SELECT 1").all())
        await asyncio.sleep(0)
        first.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await first
        # The shared fetch survived the caller's cancellation: the next read
        # waits on the same fetch and then streams with the session's limits.
        second = asyncio.ensure_future(db.prepare("SELECT 2").all())
        await asyncio.sleep(0)
        gate.set()
        self.assertEqual((await second)["results"], [])
        self.assertEqual(transport.calls, {"inline": 0, "streamed": 1, "limits": 1})
        self.assertEqual(await db.limits(), SQLITE_CAPS)

        # A fetch that was itself cancelled (the loop tearing the task down)
        # is forgotten, so the following read retries instead of failing.
        fetch = db._limits_fetch  # noqa: SLF001
        assert fetch is not None
        db._limits_fetch = asyncio.ensure_future(slow_limits())  # noqa: SLF001
        gate.clear()
        third = asyncio.ensure_future(db.prepare("SELECT 3").all())
        await asyncio.sleep(0)
        db._limits_fetch.cancel()  # noqa: SLF001
        with self.assertRaises(asyncio.CancelledError):
            await third
        self.assertIsNone(db._limits_fetch)  # noqa: SLF001
        gate.set()
        self.assertEqual((await db.prepare("SELECT 4").all())["results"], [])
        self.assertEqual(transport.calls["limits"], 2)

    async def test_the_sessions_request_cap_applies_locally_and_chunk_caps_compose(self) -> None:
        transport = _Transport(caps={**SQLITE_CAPS, "maxRequestBytes": 200}, inline=_inline_reply)
        db = transport.binding()
        with self.assertRaises(PluginError) as ctx:
            await (
                db.prepare("INSERT INTO t VALUES (?)")
                .bind({"kind": "text", "value": "x" * 400})
                .run()
            )
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.cap), ("maxRequestBytes", 200))
        self.assertEqual(transport.calls["inline"], 0, "refused before it was sent")

        stmts = [
            db.prepare("INSERT INTO t VALUES (?)").bind({"kind": "int64", "value": i})
            for i in range(5)
        ]
        one = [len(b) async for b in db.chunk(stmts, max_statements=0, max_request_bytes=0)]
        self.assertEqual(one, [5], "explicit zero caps mean one batch")

        bound = create_database_binding(
            _inline_reply_async,
            limits=_Transport(caps=SQLITE_CAPS).limits,
            max_request_bytes=220,
        )
        sizes = []
        async for batch in bound.chunk(stmts):
            sizes.append(len(batch))
            await bound.batch(batch)
        self.assertGreater(len(sizes), 1, "the binding's cap tightens the session's")

    async def test_iterate_yields_before_the_body_is_read_to_its_end(self) -> None:
        rows = [_row(i) for i in range(40)]
        pulls = {"n": 0}

        async def body(request: dict[str, Any]) -> AsyncIterator[bytes]:
            data = _stream(request["operationId"], rows, rows_per_frame=1)
            for at in range(0, len(data), 64):
                pulls["n"] += 1
                yield data[at : at + 64]

        total = -(-len(_stream("x", rows, rows_per_frame=1)) // 64)
        transport = _Transport(caps=SQLITE_CAPS, body=body)
        first = True
        async for _ in transport.binding().prepare("SELECT id").iterate():
            if first:
                self.assertLess(pulls["n"], total / 2, "the first row arrives early")
                first = False
        self.assertFalse(first, "rows were yielded")
        self.assertEqual(pulls["n"], total)

    async def test_the_binding_checks_replies_and_caps(self) -> None:
        # The tighter of the binding's request cap and the session's applies,
        # as it does to `chunk()`: a request between the two fails locally.
        transport = _Transport(
            caps={**SQLITE_CAPS, "maxRequestBytes": 200},
            body=lambda req: _stream(req["operationId"], [_row(0)]),
        )
        roomy = create_database_binding(
            transport.execute,
            limits=transport.limits,
            execute_streamed=transport.execute_streamed,
            max_request_bytes=1_000_000,
        )
        with self.assertRaises(PluginError) as ctx:
            await roomy.prepare("SELECT ?").bind({"kind": "text", "value": "x" * 300}).all()
        limit = ctx.exception.limit
        assert limit is not None
        self.assertEqual((limit.limit, limit.cap), ("maxRequestBytes", 200))
        self.assertEqual(transport.calls["streamed"], 0, "refused before it was sent")

        # A result with more rows than its statement asked for is refused, on
        # the inline and the streamed path alike.
        two = [_row(0), _row(1)]
        for over in (
            _Transport(inline=lambda r: _inline_reply(r, two)),
            _Transport(caps=SQLITE_CAPS, body=lambda req: _stream(req["operationId"], two)),
        ):
            db = over.binding()
            with self.assertRaisesRegex(PluginError, "at most 1"):
                await db.prepare("SELECT id FROM books").first()
            self.assertEqual(len((await db.prepare("SELECT id FROM books").all())["results"]), 2)

        # A request sent without an operation id takes the one the host minted.
        minted = create_database_binding(
            lambda r: _reply_async({**_inline_reply(r, [_row(0)]), "operationId": "minted"}),
            operation_id="",
        )
        self.assertEqual(len((await minted.prepare("SELECT 1").all())["results"]), 1)
        streamed = _Transport(caps=SQLITE_CAPS, body=lambda _r: _stream("minted", [_row(0)]))
        minted_stream = create_database_binding(
            streamed.execute,
            limits=streamed.limits,
            execute_streamed=streamed.execute_streamed,
            operation_id="",
        )
        self.assertEqual(len((await minted_stream.prepare("SELECT 1").all())["results"]), 1)

    async def test_a_cancelled_cached_limits_fetch_is_replaced(self) -> None:
        # A fetch cancelled with no waiter (its loop closed under it) must not
        # hand its cancellation to the next read on the binding.
        transport = _Transport(
            caps=SQLITE_CAPS, body=lambda req: _stream(req["operationId"], [_row(0)])
        )
        binding = transport.binding()
        abandoned: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        abandoned.cancel()
        binding._limits_fetch = abandoned
        result = await binding.prepare("SELECT 1").all()
        self.assertEqual(len(result["results"]), 1)
        self.assertEqual(transport.calls, {"inline": 0, "streamed": 1, "limits": 1})

    async def test_a_refused_frame_closes_the_chunk_source_of_a_whole_read(self) -> None:
        closed = {"n": 0}

        async def body(request: dict[str, Any]) -> AsyncIterator[bytes]:
            try:
                yield _frame("head", {"operationId": request["operationId"], "statementCount": 1})
                yield _frame("rows", [_row(0)])
                yield _frame("end", TIMING)
            finally:
                closed["n"] += 1

        transport = _Transport(caps=SQLITE_CAPS, body=body)
        with self.assertRaisesRegex(PluginError, "before a statement frame"):
            await transport.binding().prepare("SELECT id").all()
        self.assertEqual(closed["n"], 1, "the unread body was released")

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


class WriteChunksTests(unittest.IsolatedAsyncioTestCase):
    """``write_chunks`` resumes at batch boundaries under offset ids."""

    def setUp(self) -> None:
        self.committed: list[tuple[str, int]] = []
        self.calls = 0
        self.fail_at = 0
        self.lose_reply_at = 0

    async def _execute(self, request: dict[str, Any]) -> Any:
        self.calls += 1
        if self.calls == self.fail_at:
            raise PluginError.from_wire("unavailable", "adapter down")
        # A resent operation id replays its receipt instead of writing again.
        if not any(op == request["operationId"] for op, _ in self.committed):
            self.committed.append((request["operationId"], len(request["statements"])))
        if self.calls == self.lose_reply_at:
            raise PluginError.from_wire("unavailable", "reply lost")
        return _inline_reply(request)

    def _statements(self, db: Any) -> list[Any]:
        return [
            db.prepare("INSERT INTO t VALUES (?)").bind({"kind": "int64", "value": i})
            for i in range(5)
        ]

    def test_operation_id_matches_the_other_sdks(self) -> None:
        self.assertEqual(
            write_chunks_operation_id("job-1:titles", 2), "cb3e8a68-d67e-3739-ee20-30bcaca70026"
        )

    async def test_pauses_and_resumes_without_writing_twice(self) -> None:
        db = create_database_binding(self._execute)
        stmts = self._statements(db)
        paused = await db.write_chunks(
            stmts, key="k", stop=lambda n: n >= 2, max_statements=2, max_request_bytes=0
        )
        self.assertEqual(paused, {"next": 2, "done": False})
        done = await db.write_chunks(
            stmts, key="k", start=paused["next"], max_statements=2, max_request_bytes=0
        )
        self.assertEqual(done, {"next": 5, "done": True})
        self.assertEqual([n for _, n in self.committed], [2, 2, 1])
        self.assertEqual(self.committed[1][0], write_chunks_operation_id("k", 2))

    async def test_stop_that_raises_reports_progress(self) -> None:
        db = create_database_binding(self._execute)
        stmts = self._statements(db)

        def stop(n: int) -> bool:
            if n >= 2:
                raise RuntimeError("deadline clock failed")
            return False

        with self.assertRaises(WriteChunksError) as ctx:
            await db.write_chunks(stmts, key="k", stop=stop, max_statements=2, max_request_bytes=0)
        self.assertEqual(ctx.exception.next, 2)
        self.assertEqual([n for _, n in self.committed], [2])

    async def test_lost_reply_is_replayed_on_resume(self) -> None:
        self.lose_reply_at = 2
        db = create_database_binding(self._execute)
        stmts = self._statements(db)
        with self.assertRaises(WriteChunksError) as ctx:
            await db.write_chunks(stmts, key="k", max_statements=2)
        self.assertEqual((ctx.exception.next, ctx.exception.code), (2, "unavailable"))
        self.lose_reply_at = 0
        done = await db.write_chunks(stmts, key="k", start=2, max_statements=2)
        self.assertTrue(done["done"])
        self.assertEqual(len(self.committed), 3)

    async def test_failure_reports_progress_and_bad_input_is_refused(self) -> None:
        self.fail_at = 3
        db = create_database_binding(self._execute)
        stmts = self._statements(db)
        with self.assertRaises(WriteChunksError) as ctx:
            await db.write_chunks(stmts, key="k", max_statements=2)
        self.assertEqual(ctx.exception.next, 4)
        self.fail_at = 0
        for kwargs in (
            {"key": ""},
            {"key": "k" * (MAX_WRITE_KEY_BYTES + 1)},
            {"key": "k", "start": 6},
        ):
            with self.assertRaises(WriteChunksError) as bad:
                await db.write_chunks(stmts, max_statements=2, **kwargs)
            self.assertEqual(bad.exception.code, "invalid_params")
        self.assertEqual(
            await db.write_chunks([], key="k", max_statements=2), {"next": 0, "done": True}
        )

    async def test_a_cap_left_out_keeps_the_sessions(self) -> None:
        async def limits() -> Any:
            return {**SQLITE_CAPS, "maxRequestBytes": 1024}

        db = create_database_binding(self._execute, limits=limits)
        wide = [
            db.prepare("INSERT INTO t VALUES (?)").bind({"kind": "text", "value": "x" * 300})
            for _ in range(4)
        ]
        # Only max_statements is given; the session's byte cap still splits.
        done = await db.write_chunks(wide, key="wide", max_statements=4)
        self.assertEqual(done, {"next": 4, "done": True})
        self.assertGreater(len(self.committed), 1)
        self.assertEqual(sum(n for _, n in self.committed), 4)


async def _reply_async(reply: Any) -> Any:
    return reply


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

    def test_a_run_the_estimate_let_through_is_halved_until_it_fits(self) -> None:
        # Runs of three or more do not fit, so eight indexes come out as
        # ordered pairs and an odd run splits with the larger half first.
        pairs = lambda run: len(run) <= 2  # noqa: E731
        out: list[list[int]] = []
        _split_to_fit(list(range(8)), pairs, out)
        self.assertEqual(out, [[0, 1], [2, 3], [4, 5], [6, 7]])
        out = []
        _split_to_fit(list(range(5)), pairs, out)
        self.assertEqual(out, [[0, 1], [2], [3, 4]])
        # A lone index is never asked whether it fits.
        out = []
        _split_to_fit([7], lambda _run: self.fail("asked about a lone index"), out)
        self.assertEqual(out, [[7]])


if __name__ == "__main__":
    unittest.main()
