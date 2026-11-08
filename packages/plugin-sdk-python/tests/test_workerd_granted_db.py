"""Granted workerd database binding uses async transport."""

from __future__ import annotations

import unittest

from bookclerk_plugin_sdk.workerd import granted_databases

_CAPS = {
    "sqlContractVersion": 1,
    "atomicBatch": True,
    "returning": True,
    "affectedRows": True,
    "schemaMigrations": True,
    "cancellation": True,
    "timing": True,
    "maxBinds": 100,
    "maxStatements": 100,
    "maxResultRows": 1000,
    "maxPayloadBytes": 100_000,
    "maxResultBytes": 262_144,
    "maxCellBytes": 65_536,
    "maxRequestBytes": 4096,
    "maxAtomicResultBytes": 262_144,
    "maxStreamedResultBytes": 67_108_864,
    "maxStreamedResultRows": 250_000,
    "pluginDatabases": True,
    "maxFunctionArgs": 32,
    "maxSchemaColumns": 100,
    "maxPatternBytes": 16,
    "maxLoweredStatementBytes": 100_000,
    "consistentBackupRead": True,
    "atomicUnitRestore": True,
}


_INLINE_ONLY_CAPS = {**_CAPS, "maxStreamedResultBytes": 0, "maxStreamedResultRows": 0}


def _frames(operation_id: str) -> bytes:
    """One `ExecuteStreamFrame` sequence: `n = 1` for the request's operation id."""
    from bookclerk_plugin_sdk._wire import _encode_message, _execute_stream_frame_codec

    def frame(kind: str, value) -> bytes:
        return _encode_message(_execute_stream_frame_codec, {"kind": kind, "value": value})

    return b"".join(
        [
            frame("head", {"operationId": operation_id, "statementCount": 1}),
            frame(
                "statement",
                {"index": 0, "columns": [{"name": "n", "dbType": "int64"}], "rowsAffected": 0},
            ),
            frame("rows", [{"values": [{"kind": "int64", "value": 1}]}]),
            frame("end", {"attemptElapsedUs": 0, "dbExecutionUs": 0, "dbTimingSource": "granted"}),
        ]
    )


class _MockFetcher:
    def __init__(
        self,
        status: int = 200,
        *,
        streamed_status: int | None = None,
        caps: dict | None = None,
    ) -> None:
        self.calls: list[tuple[str, dict]] = []
        self._status = status
        self._streamed_status = streamed_status
        self._caps = _CAPS if caps is None else caps

    async def fetch(self, url: str, **kwargs):
        self.calls.append((url, kwargs))
        status = self._status
        if url.endswith("/db/execute-streamed") and self._streamed_status is not None:
            status = self._streamed_status
        caps = self._caps

        class _Resp:
            async def text(self):
                if status == 413:
                    return "limit exceeded: maxRequestBytes is 16; used 32"
                return "nope"

            async def arrayBuffer(self):
                from bookclerk_plugin_sdk.db_value import (
                    encode_db_capabilities_reply,
                    encode_execute_result_reply,
                )

                if url.endswith("/db/limits"):
                    return encode_db_capabilities_reply(caps)
                if url.endswith("/db/execute-streamed"):
                    from bookclerk_plugin_sdk.db_value import decode_execute_request

                    return _frames(decode_execute_request(bytes(kwargs["body"]))["operationId"])
                return encode_execute_result_reply(
                    {
                        "operationId": "op",
                        "statements": [
                            {
                                "rows": [{"values": [{"kind": "int64", "value": 1}]}],
                                "columns": [{"name": "n", "dbType": "int64"}],
                                "rowsAffected": 0,
                            }
                        ],
                        "timing": {
                            "attemptElapsedUs": 0,
                            "dbExecutionUs": 0,
                            "dbTimingSource": "granted",
                        },
                    }
                )

        _Resp.status = status
        return _Resp()


class WorkerdGrantedDbTests(unittest.IsolatedAsyncioTestCase):
    async def test_limits_hits_the_granted_limits_route(self) -> None:
        fetcher = _MockFetcher()
        databases = granted_databases(fetcher, {"DB": "grant-token"})
        caps = await databases["DB"].limits()
        self.assertEqual(caps["maxRequestBytes"], 4096)
        url, kwargs = fetcher.calls[0]
        self.assertEqual(url, "http://granted/db/limits")
        self.assertEqual(kwargs["method"], "GET")
        self.assertEqual(kwargs["headers"]["Authorization"], "Bearer grant-token")
        self.assertNotIn("body", kwargs)

    async def test_granted_status_codes_map_to_typed_errors(self) -> None:
        from bookclerk_plugin_sdk.limits import LimitExceeded
        from bookclerk_plugin_sdk.workerd import PluginError

        for status, code in (
            (401, "unauthorized"),
            (404, "unsupported"),
            (405, "unsupported"),
            (413, "limit_exceeded"),
            (503, "unavailable"),
        ):
            databases = granted_databases(_MockFetcher(status), {"DB": "t"})
            with self.assertRaises(PluginError) as ctx:
                await databases["DB"].limits()
            self.assertEqual(ctx.exception.code, code, status)
            if status == 413:
                # The 413 body is the canonical limit message: parsed into
                # `details`, so `err.limit` names the cap.
                self.assertEqual(
                    ctx.exception.limit,
                    LimitExceeded(limit="maxRequestBytes", cap=16, used=32),
                )
                self.assertIn("database grant: 413", str(ctx.exception))
            else:
                self.assertIsNone(ctx.exception.limit)
            if status in (404, 405):
                # An older host without the route: the author can tell
                # "not served here" from "try again later".
                self.assertIn("GuestDatabase.limits is not served", str(ctx.exception))

    async def test_binding_without_limits_transport_is_unsupported(self) -> None:
        from bookclerk_plugin_sdk.db_value import create_database_binding
        from bookclerk_plugin_sdk.limits import LimitExceeded
        from bookclerk_plugin_sdk.workerd import PluginError

        async def never(_request):
            raise AssertionError("must not send")

        binding = create_database_binding(never)
        with self.assertRaises(PluginError) as ctx:
            await binding.limits()
        self.assertEqual(ctx.exception.code, "unsupported")

        tiny = create_database_binding(never, max_request_bytes=16)
        with self.assertRaises(PluginError) as ctx:
            await tiny.prepare("SELECT title FROM books").all()
        self.assertEqual(ctx.exception.code, "limit_exceeded")
        limit = ctx.exception.limit
        self.assertIsInstance(limit, LimitExceeded)
        assert limit is not None
        self.assertEqual((limit.limit, limit.cap), ("maxRequestBytes", 16))
        self.assertGreater(limit.used, 16)

    async def test_context_database_prepare_first_hits_granted_route(self) -> None:
        # A session that does not stream reads inline over `/db/execute`.
        fetcher = _MockFetcher(caps=_INLINE_ONLY_CAPS)
        databases = granted_databases(fetcher, {"DB": "grant-token"})
        self.assertEqual(sorted(databases), ["DB"])
        row = await databases["DB"].prepare("SELECT 1").first()
        self.assertEqual(
            [url for url, _ in fetcher.calls],
            [
                "http://granted/db/limits",
                "http://granted/db/execute",
            ],
        )
        url, kwargs = fetcher.calls[1]
        self.assertEqual(kwargs["headers"]["Authorization"], "Bearer grant-token")
        self.assertEqual(kwargs["headers"]["content-type"], "application/octet-stream")
        self.assertIsInstance(kwargs["body"], (bytes, bytearray))
        self.assertEqual(row["n"]["value"], 1)

    async def test_streaming_session_reads_over_the_streamed_route(self) -> None:
        fetcher = _MockFetcher()
        databases = granted_databases(fetcher, {"DB": "grant-token"})
        result = await databases["DB"].prepare("SELECT 1").all()
        self.assertEqual(result["results"][0]["n"]["value"], 1)
        await databases["DB"].prepare("SELECT 1").all()
        self.assertEqual(
            [url for url, _ in fetcher.calls],
            [
                "http://granted/db/limits",
                "http://granted/db/execute-streamed",
                "http://granted/db/execute-streamed",
            ],
        )
        url, kwargs = fetcher.calls[1]
        self.assertEqual(kwargs["method"], "POST")
        self.assertEqual(kwargs["headers"]["Authorization"], "Bearer grant-token")
        self.assertEqual(kwargs["headers"]["content-type"], "application/octet-stream")
        rows = [r async for r in databases["DB"].prepare("SELECT 1").iterate()]
        self.assertEqual(rows, [{"n": {"kind": "int64", "value": 1}}])

    async def test_host_without_the_streamed_route_falls_back_to_inline(self) -> None:
        from bookclerk_plugin_sdk.workerd import PluginError

        fetcher = _MockFetcher(streamed_status=404)
        databases = granted_databases(fetcher, {"DB": "grant-token"})
        row = await databases["DB"].prepare("SELECT 1").first()
        self.assertEqual(row["n"]["value"], 1)
        self.assertEqual(
            [url for url, _ in fetcher.calls],
            [
                "http://granted/db/limits",
                "http://granted/db/execute-streamed",
                "http://granted/db/execute",
            ],
        )
        # Any other failure of the streamed route is the caller's.
        fetcher = _MockFetcher(streamed_status=503)
        databases = granted_databases(fetcher, {"DB": "grant-token"})
        with self.assertRaises(PluginError) as ctx:
            await databases["DB"].prepare("SELECT 1").first()
        self.assertEqual(ctx.exception.code, "unavailable")
        self.assertEqual(len(fetcher.calls), 2)


if __name__ == "__main__":
    unittest.main()


class _MockTransport:
    """Adapter `GrantedDatabaseTransport` stub: Cap'n bytes in, Cap'n bytes out."""

    def __init__(self) -> None:
        self.requests: list[bytes] = []
        self.methods: list[tuple[str, bytes | None]] = []

    async def limitsBytes(self):  # noqa: N802  (Workers RPC method name)
        from bookclerk_plugin_sdk.db_value import encode_db_capabilities_reply

        self.methods.append(("limitsBytes", None))
        return encode_db_capabilities_reply(_CAPS)

    async def executeStreamedBytes(self, body):  # noqa: N802  (Workers RPC method name)
        from bookclerk_plugin_sdk.db_value import decode_execute_request

        self.requests.append(bytes(body))
        self.methods.append(("executeStreamedBytes", bytes(body)))
        return _frames(decode_execute_request(bytes(body))["operationId"])

    async def executeBytes(self, body):  # noqa: N802  (Workers RPC method name)
        from bookclerk_plugin_sdk.db_value import (
            decode_execute_request,
            encode_execute_result_reply,
        )

        self.requests.append(bytes(body))
        request = decode_execute_request(bytes(body))
        return encode_execute_result_reply(
            {
                "operationId": request["operationId"],
                "statements": [
                    {
                        "rows": [{"values": [{"kind": "int64", "value": 7}]}],
                        "columns": [{"name": "n", "dbType": "int64"}],
                        "rowsAffected": 0,
                    }
                ],
                "timing": {
                    "attemptElapsedUs": 0,
                    "dbExecutionUs": 0,
                    "dbTimingSource": "granted",
                },
            }
        )


class WorkerdTransportDbTests(unittest.IsolatedAsyncioTestCase):
    async def test_transport_entry_becomes_env_binding(self) -> None:
        from bookclerk_plugin_sdk.workerd import _invocation_env

        transport = _MockTransport()
        env = _invocation_env({}, {"databases": [{"name": "DB", "transport": transport}]})
        # The stub's session streams, so the read crosses `executeStreamedBytes`.
        row = await env.DB.prepare("SELECT 1").first()
        self.assertEqual(row["n"]["value"], 1)
        self.assertEqual(len(transport.requests), 1, "one Cap'n request crossed the stub")
        self.assertEqual(
            [name for name, _ in transport.methods], ["limitsBytes", "executeStreamedBytes"]
        )

    async def test_transport_limits_decode_the_capabilities_reply(self) -> None:
        from bookclerk_plugin_sdk.workerd import PluginError, _invocation_env

        transport = _MockTransport()
        env = _invocation_env({}, {"databases": [{"name": "DB", "transport": transport}]})
        caps = await env.DB.limits()
        self.assertEqual(caps["maxRequestBytes"], _CAPS["maxRequestBytes"])

        class _Empty:
            async def limitsBytes(self):  # noqa: N802
                return b""

        env = _invocation_env({}, {"databases": [{"name": "DB", "transport": _Empty()}]})
        with self.assertRaises(PluginError) as ctx:
            await env.DB.limits()
        self.assertEqual(ctx.exception.code, "internal")

    async def test_ready_binding_wins_over_transport(self) -> None:
        from bookclerk_plugin_sdk.workerd import _invocation_env

        marker = object()
        env = _invocation_env(
            {}, {"databases": [{"name": "DB", "database": marker, "transport": 1}]}
        )
        self.assertIs(env.DB, marker)
