"""Isolate adapter batch runner (``db_value.execute_adapter_batch*``).

Runs against a real SQLite engine (``sqlite3``) and the host's receipt-wrap
vector (``crates/bookclerk-library/fixtures/guest_receipt/isolate_adapter.json``).
"""

from __future__ import annotations

import asyncio
import copy
import json
import sqlite3
import unittest
from pathlib import Path
from typing import Any

from bookclerk_plugin_sdk import db_value
from bookclerk_plugin_sdk._abi import MAX_SCALAR_BYTES
from bookclerk_plugin_sdk.db_value import (
    GUEST_READ_ONLY_REPLAY_PAYLOAD,
    GUEST_RECEIPT_RESULT_LOST,
    GUEST_REPLAY_PAYLOAD_MAX_BYTES,
    ExecuteStreamLimits,
    decode_execute_stream,
    encode_execute_stream,
    execute_adapter_batch,
    execute_adapter_batch_streamed,
    guest_batch_is_read_only,
    guest_receipt_finalize_statements,
    guest_replay_payload,
    run_adapter_batch,
    should_skip_remaining_guest_work,
)
from bookclerk_plugin_sdk.workerd import PluginError

FIXTURE = json.loads(
    (
        Path(__file__).resolve().parents[3]
        / "crates/bookclerk-library/fixtures/guest_receipt/isolate_adapter.json"
    ).read_text(encoding="utf-8")
)
SOURCE = "sqlite_txn"
LIMITS = ExecuteStreamLimits(
    max_bytes=64 << 20,
    bytes_limit="maxStreamedResultBytes",
    max_rows=0,
    rows_limit="maxStreamedResultRows",
)
EMPTY: dict[str, Any] = {"rows": [], "columns": [], "rowsAffected": 0}


def _value(raw: dict[str, Any]) -> dict[str, Any]:
    if raw["kind"] == "bytes":
        return {"kind": "bytes", "value": bytes(raw["value"])}
    return dict(raw)


def _result(raw: dict[str, Any]) -> dict[str, Any]:
    return {
        "rows": [{"values": [_value(v) for v in row["values"]]} for row in raw["rows"]],
        "columns": raw["columns"],
        "rowsAffected": raw["rowsAffected"],
    }


def _request(name: str) -> dict[str, Any]:
    raw = copy.deepcopy(FIXTURE[name]["request"])
    return {
        "operationId": raw["operationId"],
        "statements": [
            {**s, "parameters": [_value(p) for p in s["parameters"]]} for s in raw["statements"]
        ],
        "receipt": raw["receipt"],
    }


def _db_type(declared: str | None) -> str:
    return {"INTEGER": "int64", "TEXT": "text", "REAL": "float64", "BLOB": "bytes"}.get(
        (declared or "").upper(), "unspecified"
    )


def _cell(v: Any, db_type: str) -> dict[str, Any]:
    if v is None:
        return {"kind": "null", "value": db_type}
    if isinstance(v, int):
        return {"kind": "int64", "value": v}
    if isinstance(v, str):
        return {"kind": "text", "value": v}
    if isinstance(v, float):
        return {"kind": "float64", "value": v}
    return {"kind": "bytes", "value": bytes(v)}


def _bind(v: dict[str, Any]) -> Any:
    if v["kind"] == "null":
        return None
    if v["kind"] == "boolean":
        return int(v["value"])
    return v["value"]


class _Txn:
    def __init__(self, engine: _Engine) -> None:
        self.engine = engine

    async def run(self, statement: dict[str, Any]) -> dict[str, Any]:
        return self.engine.run(statement)

    async def commit(self) -> None:
        self.engine.db.execute("COMMIT")
        self.engine.commits += 1

    async def rollback(self) -> None:
        self.engine.db.execute("ROLLBACK")
        self.engine.rollbacks += 1
        if self.engine.fail_rollback:
            raise RuntimeError("rollback failed")


class _Engine:
    """An in-memory SQLite engine that lowers through the fixture's table."""

    def __init__(self) -> None:
        self.db = sqlite3.connect(":memory:", isolation_level=None)
        for ddl in FIXTURE["setup"]:
            self.db.execute(ddl)
        self.commits = 0
        self.rollbacks = 0
        self.ran: list[str] = []
        self.fail_on: str | None = None
        self.fail_rollback = False

    async def begin(self) -> _Txn:
        self.db.execute("BEGIN")
        return _Txn(self)

    def run(self, statement: dict[str, Any]) -> dict[str, Any]:
        sql = FIXTURE["sqlite"].get(statement["sql"])
        assert sql is not None, f"no SQLite lowering for {statement['sql']}"
        self.ran.append(statement["sql"])
        if self.fail_on and self.fail_on in statement["sql"]:
            raise RuntimeError(f"fail on {self.fail_on}")
        cur = self.db.execute(sql, [_bind(p) for p in statement["parameters"]])
        if statement["kind"] == "execute":
            return {"rows": [], "columns": [], "rowsAffected": max(cur.rowcount, 0)}
        names = [d[0] for d in cur.description or []]
        rows_raw = cur.fetchall()
        types = self._types(names)
        columns = [{"name": n, "dbType": t} for n, t in zip(names, types, strict=True)]
        rows = [
            {"values": [_cell(v, t) for v, t in zip(row, types, strict=True)]} for row in rows_raw
        ]
        affected = len(rows) if statement["kind"] == "returning" else 0
        return {"rows": rows, "columns": columns, "rowsAffected": affected}

    def _types(self, names: list[str]) -> list[str]:
        declared = {"id": "INTEGER"}
        return [_db_type(declared.get(n, "TEXT")) for n in names]

    def receipt(self, operation_id: str) -> tuple[Any, ...] | None:
        return self.db.execute(
            "SELECT status, payload, request_hash FROM bookclerk_receipts WHERE operation_id = ?",
            (operation_id,),
        ).fetchone()

    def has_notes(self) -> bool:
        row = self.db.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'notes'"
        ).fetchone()
        return row is not None

    def note_count(self) -> int:
        return self.db.execute("SELECT count(*) FROM notes").fetchone()[0]


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


class FinalizeParityTest(unittest.TestCase):
    def test_finalize_matches_the_host(self) -> None:
        f = FIXTURE["finalize"]
        partial = {
            "operationId": f["partial"]["operationId"],
            "statements": [_result(s) for s in f["partial"]["statements"]],
            "timing": f["partial"]["timing"],
        }
        got = guest_receipt_finalize_statements(partial, f["guestLen"], f["guestHash"], False)
        want = [{**s, "parameters": [_value(p) for p in s["parameters"]]} for s in f["statements"]]
        self.assertEqual(got, want)
        read_only = guest_receipt_finalize_statements(partial, f["guestLen"], f["guestHash"], True)
        self.assertEqual(read_only[0]["parameters"][0]["value"], FIXTURE["readOnlyPayload"])
        self.assertEqual(GUEST_READ_ONLY_REPLAY_PAYLOAD, FIXTURE["readOnlyPayload"])
        self.assertEqual(GUEST_REPLAY_PAYLOAD_MAX_BYTES, FIXTURE["replayPayloadMaxBytes"])


class BatchTest(unittest.TestCase):
    def _engine(self) -> _Engine:
        engine = _Engine()
        self.addCleanup(engine.db.close)
        return engine

    def test_write_commits_once_and_retry_replays(self) -> None:
        engine = self._engine()
        first = _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        self.assertEqual(engine.commits, 1)
        self.assertEqual(len(first["statements"]), 5)
        self.assertEqual(first["statements"][3]["rows"][0]["values"][1]["value"], "first")
        self.assertEqual(first["timing"]["dbTimingSource"], SOURCE)
        status, payload, _hash = engine.receipt("fixture-write")
        self.assertEqual(status, "ok")
        decoded = json.loads(payload)
        self.assertEqual(decoded["operation_id"], "fixture-write")
        self.assertEqual(
            decoded["statements"][1]["rows"][0]["values"],
            [{"kind": "int64", "value": 1}, {"kind": "text", "value": "first"}],
        )

        engine.ran = []
        retry = _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        self.assertEqual(engine.commits, 2)
        self.assertEqual(len(engine.ran), 2)
        self.assertEqual(retry["statements"][1]["rows"][0]["values"][3]["value"], payload)
        self.assertEqual(retry["statements"][2:], [EMPTY, EMPTY, EMPTY])
        self.assertEqual(engine.note_count(), 1)

    def test_streamed_reply_decodes_to_the_inline_reply(self) -> None:
        engine = self._engine()
        frames = _run(execute_adapter_batch_streamed(_request("write"), engine.begin, SOURCE))
        reply = decode_execute_stream(frames, LIMITS)
        self.assertEqual(reply["operationId"], "fixture-write")
        self.assertEqual(len(reply["statements"]), 5)
        self.assertEqual(reply["statements"][3]["rows"][0]["values"][1]["value"], "first")

    def test_read_only_stores_the_marker_and_reads_again(self) -> None:
        engine = self._engine()
        _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        self.assertTrue(guest_batch_is_read_only(_request("readOnly")["statements"], 1))
        first = _run(execute_adapter_batch(_request("readOnly"), engine.begin, SOURCE))
        self.assertEqual(engine.receipt("fixture-read")[1], GUEST_READ_ONLY_REPLAY_PAYLOAD)
        self.assertEqual(len(first["statements"][2]["rows"]), 1)
        engine.db.execute("INSERT INTO notes (id, body) VALUES (2, 'second')")
        retry = _run(execute_adapter_batch(_request("readOnly"), engine.begin, SOURCE))
        self.assertEqual(len(retry["statements"][2]["rows"]), 2)

    def test_oversized_streamed_read_rolls_back_before_limit_exceeded(self) -> None:
        engine = self._engine()
        _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        wide = "w" * (150 * 1024)
        engine.db.execute("INSERT INTO notes (id, body) VALUES (2, ?), (3, ?)", (wide, wide))
        with self.assertRaises(PluginError) as caught:
            _run(execute_adapter_batch_streamed(_request("readOnly"), engine.begin, SOURCE))
        self.assertEqual(caught.exception.code, "limit_exceeded")
        self.assertIn("maxScalarBytes", str(caught.exception))
        self.assertIn(str(MAX_SCALAR_BYTES), str(caught.exception))
        self.assertEqual(engine.rollbacks, 1)
        self.assertIsNone(engine.receipt("fixture-read"))

    def test_write_whose_reply_cannot_be_recorded_rolls_back(self) -> None:
        engine = self._engine()
        request = _request("write")
        request["statements"][3]["parameters"][1] = {
            "kind": "text",
            "value": "x" * GUEST_REPLAY_PAYLOAD_MAX_BYTES,
        }
        with self.assertRaises(PluginError) as caught:
            _run(execute_adapter_batch_streamed(request, engine.begin, SOURCE))
        self.assertEqual(caught.exception.code, "internal")
        self.assertRegex(str(caught.exception), r"guest replay payload is \d+ bytes; max is 65536")
        self.assertEqual(engine.commits, 0)
        self.assertFalse(engine.has_notes())
        self.assertIsNone(engine.receipt("fixture-write"))

    def test_receipt_without_its_reply_is_unavailable(self) -> None:
        engine = self._engine()
        _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        engine.db.execute(
            "UPDATE bookclerk_receipts SET payload = '' WHERE operation_id = 'fixture-write'"
        )
        with self.assertRaises(PluginError) as caught:
            _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        self.assertEqual(caught.exception.code, "unavailable")
        self.assertIn(GUEST_RECEIPT_RESULT_LOST, str(caught.exception))

    def _seed_receipt(self, engine: _Engine, request_hash: str, status: str, payload: str) -> None:
        engine.db.execute(
            "INSERT INTO bookclerk_receipts (operation_id, operation_kind, request_hash, status, "
            "payload, created_at, expires_at) VALUES ('fixture-write', 'guestTyped', ?, ?, ?, "
            "'x', '2999')",
            (request_hash, status, payload),
        )

    def test_in_flight_claim_resumes_without_finalize(self) -> None:
        engine = self._engine()
        self._seed_receipt(engine, "a" * 64, "claimed", "")
        reply = _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        self.assertEqual(len(engine.ran), 5)
        self.assertEqual(reply["statements"][3]["rows"], [])
        self.assertEqual(engine.receipt("fixture-write")[0], "claimed")

    def test_receipt_for_another_request_skips_guest_work(self) -> None:
        engine = self._engine()
        self._seed_receipt(engine, "d" * 64, "ok", "{}")
        reply = _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        self.assertEqual(len(engine.ran), 2)
        self.assertEqual(len(reply["statements"]), 5)
        self.assertFalse(engine.has_notes())

    def test_without_a_receipt_the_statements_run_as_given(self) -> None:
        engine = self._engine()
        bare = {
            "operationId": "bare",
            "statements": [_request("write")["statements"][2]],
            "receipt": {"guestLen": 0, "guestHash": ""},
        }
        reply = _run(execute_adapter_batch(bare, engine.begin, SOURCE))
        self.assertEqual(len(reply["statements"]), 1)
        self.assertTrue(engine.has_notes())

    def test_failing_statement_rolls_back_and_a_failing_rollback_does_not_hide_it(self) -> None:
        engine = self._engine()
        engine.fail_on = "INSERT INTO notes"
        engine.fail_rollback = True
        with self.assertRaisesRegex(RuntimeError, "fail on INSERT INTO notes"):
            _run(execute_adapter_batch(_request("write"), engine.begin, SOURCE))
        self.assertEqual(engine.rollbacks, 1)
        self.assertEqual(engine.commits, 0)

    def test_run_adapter_batch_leaves_the_commit_to_the_caller(self) -> None:
        engine = self._engine()
        engine.db.execute("BEGIN")
        reply = _run(run_adapter_batch(_request("write"), _Txn(engine), SOURCE))
        self.assertEqual(len(reply["statements"]), 5)
        engine.db.execute("ROLLBACK")
        self.assertFalse(engine.has_notes())


def _prior(hash_: str, status: str, payload: str) -> dict[str, Any]:
    values = [{"kind": "text", "value": v} for v in ("op", hash_, status, payload)]
    return {"rows": [{"values": values}], "columns": [], "rowsAffected": 0}


class RulesTest(unittest.TestCase):
    def test_wrap_shape_is_checked(self) -> None:
        partial = {"operationId": "op", "statements": [EMPTY], "timing": {}}
        with self.assertRaisesRegex(PluginError, "wrap returned 1 statements; expected 4"):
            guest_receipt_finalize_statements(partial, 1, "h", False)

    def test_skip_rules(self) -> None:
        self.assertTrue(should_skip_remaining_guest_work([EMPTY, _prior("h", "ok", "{}")], 4, "h"))
        self.assertFalse(
            should_skip_remaining_guest_work([EMPTY, _prior("h", "claimed", "")], 4, "h")
        )
        self.assertFalse(
            should_skip_remaining_guest_work(
                [EMPTY, _prior("h", "ok", GUEST_READ_ONLY_REPLAY_PAYLOAD)], 4, "h"
            )
        )
        self.assertFalse(should_skip_remaining_guest_work([EMPTY, EMPTY], 4, "h"))
        # An empty hash is still a receipt, for another request.
        self.assertTrue(should_skip_remaining_guest_work([EMPTY, _prior("", "ok", "")], 4, "h"))
        self.assertFalse(should_skip_remaining_guest_work([EMPTY], 4, "h"))
        self.assertFalse(should_skip_remaining_guest_work([EMPTY, _prior("h", "ok", "{}")], 2, "h"))
        self.assertFalse(guest_batch_is_read_only([{"kind": "select"}], 1))  # type: ignore[list-item]
        self.assertFalse(guest_batch_is_read_only([], 0))
        short = {"rows": [{"values": [{"kind": "text", "value": "op"}]}], "columns": []}
        self.assertFalse(should_skip_remaining_guest_work([EMPTY, short], 4, "h"))  # type: ignore[list-item]
        odd = {
            "rows": [{"values": [{"kind": "text", "value": "op"}, {"kind": "int64", "value": 1}]}],
            "columns": [],
            "rowsAffected": 0,
        }
        partial = {
            "operationId": "op",
            "statements": [EMPTY, odd, EMPTY, {**EMPTY, "rowsAffected": 1}],
            "timing": {"attemptElapsedUs": 0, "dbExecutionUs": 0, "dbTimingSource": "t"},
        }
        self.assertEqual(len(guest_receipt_finalize_statements(partial, 1, "h", False)), 1)

    def test_payload_refuses_what_serde_cannot_decode(self) -> None:
        def reply(values: list[dict[str, Any]], **extra: Any) -> dict[str, Any]:
            stmt = {"rows": [{"values": values}], "columns": [], "rowsAffected": 0, **extra}
            return {
                "operationId": "op",
                "statements": [stmt],
                "timing": {"attemptElapsedUs": 0, "dbExecutionUs": 0, "dbTimingSource": "t"},
            }

        with self.assertRaisesRegex(PluginError, "non-finite"):
            guest_replay_payload(reply([{"kind": "float64", "value": float("nan")}]))
        with self.assertRaisesRegex(PluginError, "not a u64"):
            guest_replay_payload(reply([], rowsAffected=-1))
        with self.assertRaisesRegex(PluginError, "not a u64"):
            guest_replay_payload(reply([], rowsAffected=True))
        with self.assertRaisesRegex(PluginError, "unknown odd cell"):
            guest_replay_payload(reply([{"kind": "odd", "value": 1}]))
        self.assertEqual(
            guest_replay_payload(
                reply(
                    [
                        {"kind": "int64", "value": 9007199254740993},
                        {"kind": "boolean", "value": False},
                    ]
                )
            ),
            '{"operation_id":"op","statements":[{"rows":[{"values":[{"kind":"int64",'
            '"value":9007199254740993},{"kind":"boolean","value":false}]}],"columns":[],'
            '"rowsAffected":0}],"timing":{"attemptElapsedUs":0,"dbExecutionUs":0,'
            '"dbTimingSource":"t"}}',
        )

    def test_wide_rows_split_into_frames(self) -> None:
        def row(n: int) -> dict[str, Any]:
            return {
                "values": [
                    {"kind": "text", "value": "r" * n},
                    {"kind": "bytes", "value": bytes(8)},
                    {"kind": "int64", "value": 1},
                ]
            }

        reply = {
            "operationId": "wide",
            "statements": [
                {
                    "rows": [row(100 * 1024), row(100 * 1024), row(300 * 1024)]
                    + [row(10) for _ in range(50)],
                    "columns": [
                        {"name": "a", "dbType": "text"},
                        {"name": "b", "dbType": "bytes"},
                        {"name": "c", "dbType": "int64"},
                    ],
                    "rowsAffected": 0,
                }
            ],
            "timing": {"attemptElapsedUs": 1, "dbExecutionUs": 1, "dbTimingSource": "t"},
        }
        decoded = decode_execute_stream(encode_execute_stream(reply), LIMITS)
        self.assertEqual(len(decoded["statements"][0]["rows"]), 53)
        self.assertEqual(len(decoded["statements"][0]["rows"][2]["values"][0]["value"]), 300 * 1024)
        # With no target, one rows frame would pass a scalar and is halved.
        frames = encode_execute_stream(reply, frame_target_bytes=1 << 40)
        self.assertEqual(decode_execute_stream(frames, LIMITS), decoded)

    def test_helpers_live_in_the_injected_module(self) -> None:
        self.assertIs(db_value.execute_adapter_batch_streamed, execute_adapter_batch_streamed)
