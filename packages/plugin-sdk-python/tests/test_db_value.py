"""Golden vectors for typed DbValue (Cap'n wire, i64 bounds, UTF-8, unknown unions)."""

from __future__ import annotations

import asyncio
import unittest

from bookclerk_plugin_sdk.db_value import (
    DatabaseBinding,
    RetryToken,
    canonical_execute_request_hash,
    decode_db_value,
    decode_execute_result_reply,
    decode_execute_request,
    encode_db_value,
    encode_execute_result_reply,
    encode_execute_request,
    parse_db_value,
)

I64_MIN = -(2**63)
I64_MAX = 2**63 - 1

GOLDEN_I64_MIN = (
    "00000000040000000000000002000100000002000000000000000000000000800000000000000000"
)
GOLDEN_I64_MAX = (
    "000000000400000000000000020001000000020000000000ffffffffffffff7f0000000000000000"
)
GOLDEN_TEXT_B64 = (
    "0000000006000000000000000200010000000400000000000000000000000000010000004a000000"
    "6236343a414141410000000000000000"
)
GOLDEN_BYTES_012 = (
    "0000000005000000000000000200010000000500000000000000000000000000010000001a000000"
    "0001020000000000"
)
GOLDEN_BOOL_TRUE = (
    "00000000040000000000000002000100010001000000000000000000000000000000000000000000"
)
GOLDEN_NULL_BYTES = (
    "00000000040000000000000002000100050000000000000000000000000000000000000000000000"
)
GOLDEN_EXECUTE_REQUEST = (
    "000000001d0000000000000004000300000000000000000000000000000000000000000000000000"
    "0000000000000000090000001a0000000900000022000000090000001f0000006f70000000000000"
    "616263000000000004000000010002000100020001000000050000004a000000090000004f000000"
    "53454c454354203f00000000000000000c0000000200010000000200000000000000000000000080"
    "0000000000000000000004000000000000000000000000000d000000720000000000050000000000"
    "0000000000000000090000000a0000006236343a6e6f742d6279746573000000ff00000000000000"
)


class DbValueGoldens(unittest.TestCase):
    def test_typed_null_bytes(self) -> None:
        self.assertEqual(
            parse_db_value({"kind": "null", "value": "bytes"}),
            {"kind": "null", "value": "bytes"},
        )

    def test_i64_min_max(self) -> None:
        for n in (I64_MIN, -1, 0, 1, I64_MAX):
            self.assertEqual(
                parse_db_value({"kind": "int64", "value": n}),
                {"kind": "int64", "value": n},
            )

    def test_text_rejects_embedded_nul(self) -> None:
        with self.assertRaisesRegex(ValueError, "TEXT cannot contain U\\+0000"):
            parse_db_value({"kind": "text", "value": "héllo\x00world"})
        with self.assertRaisesRegex(ValueError, "TEXT cannot contain U\\+0000"):
            encode_db_value({"kind": "text", "value": "a\x00b"})
        self.assertEqual(
            decode_db_value(encode_db_value({"kind": "bytes", "value": b"\x00"}))["value"],
            b"\x00",
        )

    def test_multilingual_utf8_round_trip_is_not_normalized(self) -> None:
        samples = [
            "汉语",
            "日本語",
            "한국어",
            "𠀀",
            "مرحبا",
            "שלום",
            "नमस्ते",
            "สวัสดี",
            "γειά",
            "привет",
            "🎵🦀",
            "café",
            "cafe\u0301",
        ]
        for sample in samples:
            parsed = parse_db_value({"kind": "text", "value": sample})
            self.assertEqual(parsed["value"], sample)
            self.assertEqual(
                decode_db_value(encode_db_value({"kind": "text", "value": sample}))["value"],
                sample,
            )
        self.assertNotEqual("café", "cafe\u0301")
        self.assertNotEqual(
            encode_db_value({"kind": "text", "value": "café"}),
            encode_db_value({"kind": "text", "value": "cafe\u0301"}),
        )

    def test_embedded_zero_bytes(self) -> None:
        blob = parse_db_value({"kind": "bytes", "value": "b64:AAEC"})
        self.assertEqual(blob["kind"], "bytes")
        self.assertEqual(blob["value"], b"\x00\x01\x02")
        raw = parse_db_value({"kind": "bytes", "value": b"\x00\x01\x02"})
        self.assertEqual(raw["value"], b"\x00\x01\x02")

    def test_unknown_union_member(self) -> None:
        with self.assertRaisesRegex(ValueError, "unknown DbValue union member"):
            parse_db_value({"kind": "xml", "value": "<a/>"})

    def test_capnp_goldens(self) -> None:
        self.assertEqual(encode_db_value({"kind": "int64", "value": I64_MIN}).hex(), GOLDEN_I64_MIN)
        self.assertEqual(encode_db_value({"kind": "int64", "value": I64_MAX}).hex(), GOLDEN_I64_MAX)
        self.assertEqual(
            encode_db_value({"kind": "text", "value": "b64:AAAA"}).hex(), GOLDEN_TEXT_B64
        )
        self.assertEqual(
            encode_db_value({"kind": "bytes", "value": b"\x00\x01\x02"}).hex(),
            GOLDEN_BYTES_012,
        )
        self.assertEqual(encode_db_value({"kind": "boolean", "value": True}).hex(), GOLDEN_BOOL_TRUE)
        self.assertEqual(
            encode_db_value({"kind": "null", "value": "bytes"}).hex(), GOLDEN_NULL_BYTES
        )
        self.assertNotEqual(
            encode_db_value({"kind": "text", "value": "b64:AAAA"}),
            encode_db_value({"kind": "bytes", "value": b"\x00\x01\x02"}),
        )
        self.assertEqual(decode_db_value(encode_db_value({"kind": "int64", "value": I64_MIN}))["value"], I64_MIN)
        self.assertEqual(
            decode_db_value(encode_db_value({"kind": "text", "value": "b64:AAAA"}))["value"],
            "b64:AAAA",
        )
        self.assertEqual(
            decode_db_value(encode_db_value({"kind": "bytes", "value": b"\x00\x01\x02"}))["value"],
            b"\x00\x01\x02",
        )

    def test_execute_request_golden(self) -> None:
        request = {
            "operationId": "op",
            "requestHash": "abc",
            "statements": [
                {
                    "sql": "SELECT ?",
                    "parameters": [
                        {"kind": "int64", "value": I64_MIN},
                        {"kind": "text", "value": "b64:not-bytes"},
                        {"kind": "bytes", "value": b"\xff"},
                    ],
                    "kind": "select",
                    "maxRows": 1,
                    "resultSelection": "rows",
                }
            ],
            "deadlineUnixMs": 0,
        }
        encoded = encode_execute_request(request)
        self.assertEqual(encoded.hex(), GOLDEN_EXECUTE_REQUEST)
        back = decode_execute_request(encoded)
        self.assertEqual(back["operationId"], "op")
        self.assertEqual(back["statements"][0]["parameters"][0]["value"], I64_MIN)
        self.assertEqual(back["statements"][0]["parameters"][1]["value"], "b64:not-bytes")
        self.assertEqual(back["statements"][0]["parameters"][2]["value"], b"\xff")

        seen = {"n": 0}

        async def transport(req):
            seen["n"] = len(encode_execute_request(req))
            return {
                "operationId": req["operationId"],
                "statements": [],
                "timing": {
                    "attemptElapsedUs": 0,
                    "dbExecutionUs": 0,
                    "dbTimingSource": "test",
                },
            }

        binding = DatabaseBinding(transport, operation_id="op", request_hash="abc")

        async def run() -> None:
            reply = await binding.execute(request["statements"])
            self.assertEqual(reply["operationId"], "op")
            self.assertEqual(seen["n"], len(encoded))

        asyncio.run(run())

    def test_two_sequential_batches_use_distinct_operation_ids(self) -> None:
        seen: list[str] = []
        hashes: list[str] = []

        async def transport(req):
            seen.append(req["operationId"])
            self.assertNotEqual(req["operationId"], "op")
            return {
                "operationId": req["operationId"],
                "statements": [
                    {"rows": [], "columns": [], "rowsAffected": 0}
                ],
                "timing": {
                    "attemptElapsedUs": 0,
                    "dbExecutionUs": 0,
                    "dbTimingSource": "test",
                },
            }

        async def recording(req):
            hashes.append(req["requestHash"])
            return await transport(req)

        async def run() -> None:
            binding = DatabaseBinding(recording)
            first = [{"kind": "int64", "value": 1}]
            second = [{"kind": "int64", "value": 2}]
            await binding.prepare("INSERT INTO t VALUES (?)").bind(*first).run()
            await binding.prepare("INSERT INTO t VALUES (?)").bind(*second).run()
            self.assertEqual(len(seen), 2)
            self.assertNotEqual(seen[0], seen[1])
            self.assertNotEqual(seen[0], "op")
            self.assertNotEqual(seen[1], "op")
            self.assertEqual(hashes, ["", ""])
            retry = RetryToken(seen[0], canonical_execute_request_hash({
                "operationId": seen[0],
                "requestHash": "",
                "statements": [
                    {
                        "sql": "INSERT INTO t VALUES (?)",
                        "parameters": first,
                        "kind": "execute",
                        "maxRows": 0,
                        "resultSelection": "affectedRows",
                    }
                ],
                "deadlineUnixMs": 0,
            }))
            replayed = []

            async def replay_transport(req):
                replayed.append((req["operationId"], req["requestHash"]))
                return await transport(req)

            replay = DatabaseBinding(replay_transport)
            await replay.prepare("INSERT INTO t VALUES (?)").bind(*first).run(retry=retry)
            self.assertEqual(replayed[0][0], seen[0])
            self.assertEqual(replayed[0][1], retry.request_hash)

        asyncio.run(run())

    def test_capnp_reader_rejects_truncated_multisegment_and_oversized_count(self) -> None:
        with self.assertRaisesRegex(ValueError, "truncated"):
            decode_db_value(b"\x00\x00")
        multi = (
            (1).to_bytes(4, "little")
            + (1).to_bytes(4, "little")
            + (1).to_bytes(4, "little")
            + (0).to_bytes(4, "little")
            + b"\x00" * 16
        )
        with self.assertRaisesRegex(ValueError, "multi-segment"):
            decode_db_value(multi)
        # Composite list tag with count 2^29 and a 1-word payload (D=1).
        import struct

        def pack_word(n: int) -> bytes:
            return struct.pack("<Q", n)

        # single-segment header, size 4 words: root ptr + far-looking list + tag + pad
        # Build: nseg=1, size0=4. Root is a composite list pointer with huge count.
        list_ptr = 1 | (0 << 2) | (7 << 32) | (1 << 35)  # offset 0, composite, D=1
        tag = (0x20000000 << 2) | (1 << 32) | (0 << 48)  # count ~2^29, 1 data word
        buf = bytearray()
        buf += struct.pack("<II", 0, 4)
        buf += pack_word(list_ptr)  # word 0 root — not a struct; decode_db_value expects struct
        buf += pack_word(0)
        buf += pack_word(tag)
        buf += pack_word(0)
        with self.assertRaises(ValueError):
            decode_execute_request(bytes(buf))
        # Backward (negative) struct offset that leaves the segment.
        back_ptr = 0 | ((0x3FFFFFFF) << 2) | (2 << 32) | (1 << 48)
        back = bytearray()
        back += struct.pack("<II", 0, 2)
        back += pack_word(back_ptr)
        back += pack_word(0)
        with self.assertRaisesRegex(ValueError, "out of segment|far pointer|expected struct"):
            decode_db_value(bytes(back))

    def test_canonical_hash_ignores_deadline_golden(self) -> None:
        req = {
            "operationId": "op",
            "requestHash": "abc",
            "statements": [
                {
                    "sql": "SELECT 1",
                    "parameters": [],
                    "kind": "select",
                    "maxRows": 1,
                    "resultSelection": "rows",
                }
            ],
            "deadlineUnixMs": 0,
        }
        golden = "648cd28b3223c825c55ea99a7c6e52321ea733656f5965abfe4c7ed4ca21d111"
        self.assertEqual(canonical_execute_request_hash(req), golden)
        req["deadlineUnixMs"] = 9_999_999_999
        self.assertEqual(canonical_execute_request_hash(req), golden)

    def test_first_sends_max_rows_one_and_mixed_batch_keeps_per_statement_intent(self) -> None:
        seen: list[dict] = []

        async def transport(req):
            seen.append(req["statements"][0] if len(req["statements"]) == 1 else req["statements"])
            n = len(req["statements"])
            return {
                "operationId": req["operationId"],
                "statements": [
                    {
                        "rows": [{"values": [{"kind": "int64", "value": 1}]}],
                        "columns": [{"name": "n", "dbType": "int64"}],
                        "rowsAffected": 0,
                                            }
                    for _ in range(n)
                ],
                "timing": {
                    "attemptElapsedUs": 0,
                    "dbExecutionUs": 0,
                    "dbTimingSource": "test",
                },
            }

        async def run() -> None:
            binding = DatabaseBinding(transport)
            row = await binding.prepare("SELECT n FROM t").first()
            self.assertEqual(seen[0]["maxRows"], 1)
            self.assertEqual(seen[0]["resultSelection"], "rows")
            self.assertEqual(row["n"]["value"], 1)

            col = await binding.prepare("SELECT n FROM t").first("n")
            self.assertEqual(col["value"], 1)

            all_result = await binding.prepare("SELECT n FROM t").all()
            self.assertTrue(all_result["success"])
            self.assertEqual(all_result["results"][0]["n"]["value"], 1)
            self.assertEqual(all_result["meta"]["rows_read"], 1)

            run_result = await binding.prepare("SELECT n FROM t").run()
            self.assertEqual(run_result["results"][0]["n"]["value"], 1)

            seen.clear()
            await binding.batch(
                [
                    binding.prepare("INSERT INTO t VALUES (?)").bind(
                        {"kind": "int64", "value": 1}
                    ),
                    binding.prepare("SELECT n FROM t"),
                ]
            )
            stmts = seen[0]
            self.assertEqual(stmts[0]["resultSelection"], "rows")
            self.assertEqual(stmts[1]["resultSelection"], "rows")

            seen.clear()
            await binding.batch(
                [
                    binding.prepare("INSERT INTO t VALUES (?)").bind(
                        {"kind": "int64", "value": 1}
                    ).as_run(),
                    binding.prepare("SELECT n FROM t").as_all(),
                ]
            )
            stmts = seen[0]
            self.assertEqual(stmts[0]["resultSelection"], "affectedRows")
            self.assertEqual(stmts[0]["maxRows"], 0)
            self.assertEqual(stmts[1]["resultSelection"], "rows")

        asyncio.run(run())

    def test_execute_result_reply_preserves_i64_bytes_and_error_code(self) -> None:
        reply = {
            "operationId": "op",
            "statements": [
                {
                    "rows": [
                        {
                            "values": [
                                {"kind": "int64", "value": I64_MIN},
                                {"kind": "int64", "value": I64_MAX},
                                {"kind": "bytes", "value": b"\xff"},
                                {"kind": "text", "value": "b64:not-bytes"},
                                {"kind": "null", "value": "int64"},
                            ]
                        }
                    ],
                    "columns": [
                        {"name": "lo", "dbType": "int64"},
                        {"name": "hi", "dbType": "int64"},
                        {"name": "blob", "dbType": "bytes"},
                        {"name": "txt", "dbType": "text"},
                        {"name": "n", "dbType": "int64"},
                    ],
                    "rowsAffected": 0,
                                    }
            ],
            "timing": {
                "attemptElapsedUs": 1,
                "dbExecutionUs": 2,
                "dbTimingSource": "test",
            },
        }
        back = decode_execute_result_reply(encode_execute_result_reply(reply))
        cells = back["statements"][0]["rows"][0]["values"]
        self.assertEqual(cells[0]["value"], I64_MIN)
        self.assertEqual(cells[1]["value"], I64_MAX)
        self.assertEqual(cells[2]["value"], b"\xff")
        self.assertEqual(cells[3]["value"], "b64:not-bytes")
        from bookclerk_plugin_sdk.workerd import PluginError

        with self.assertRaises(PluginError) as ctx:
            decode_execute_result_reply(
                encode_execute_result_reply(("unavailable", "retry me"))
            )
        self.assertEqual(ctx.exception.code, "unavailable")
        self.assertEqual(ctx.exception.wire_code, "unavailable")

    def test_guest_statement_kind_matches_host(self) -> None:
        from bookclerk_plugin_sdk.guest_sql import guest_statement_kind

        self.assertEqual(guest_statement_kind("INSERT INTO t VALUES (?)"), "execute")
        self.assertEqual(
            guest_statement_kind("INSERT INTO t VALUES (?) RETURNING id"), "returning"
        )
        self.assertEqual(guest_statement_kind("SELECT 1"), "select")

    def test_dml_run_canonical_hash_uses_execute_kind(self) -> None:
        from bookclerk_plugin_sdk.guest_sql import guest_statement_kind

        req = {
            "operationId": "",
            "requestHash": "",
            "deadlineUnixMs": 0,
            "statements": [
                {
                    "sql": "INSERT INTO t VALUES (?)",
                    "parameters": [{"kind": "int64", "value": 1}],
                    "kind": guest_statement_kind("INSERT INTO t VALUES (?)"),
                    "maxRows": 0,
                    "resultSelection": "rows",
                }
            ],
        }
        self.assertEqual(len(canonical_execute_request_hash(req)), 64)

    async def _test_exec_two_queries(self) -> None:
        seen: list[int] = []

        async def transport(req):
            seen.append(len(req["statements"]))
            return {
                "operationId": req["operationId"],
                "statements": [
                    {
                        "rows": [],
                        "columns": [],
                        "rowsAffected": 1,
                        "cursor": "",
                    }
                    for _ in req["statements"]
                ],
                "timing": {
                    "attemptElapsedUs": 0,
                    "dbExecutionUs": 0,
                    "dbTimingSource": "test",
                },
            }

        binding = DatabaseBinding(transport)
        out = await binding.exec("INSERT INTO t VALUES (1)\nSELECT 1")
        self.assertEqual(out["count"], 2)
        self.assertEqual(seen[0], 2)

    def test_exec_two_queries(self) -> None:
        import asyncio

        asyncio.run(self._test_exec_two_queries())

    async def _test_first_missing_column_errors(self) -> None:
        async def transport(_req):
            return {
                "operationId": "op",
                "statements": [
                    {
                        "rows": [{"values": [{"kind": "int64", "value": 1}]}],
                        "columns": [{"name": "n", "dbType": "int64"}],
                        "rowsAffected": 0,
                        "cursor": "",
                    }
                ],
                "timing": {
                    "attemptElapsedUs": 0,
                    "dbExecutionUs": 0,
                    "dbTimingSource": "test",
                },
            }

        binding = DatabaseBinding(transport)
        with self.assertRaises(ValueError, msg="column missing not found in first() result"):
            await binding.prepare("SELECT 1 AS n").first("missing")

    def test_first_missing_column_errors(self) -> None:
        import asyncio

        asyncio.run(self._test_first_missing_column_errors())


if __name__ == "__main__":
    unittest.main()
