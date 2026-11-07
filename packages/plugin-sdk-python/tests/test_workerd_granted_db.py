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
    "pluginDatabases": True,
    "maxFunctionArgs": 32,
    "maxSchemaColumns": 100,
    "maxPatternBytes": 16,
    "maxLoweredStatementBytes": 100_000,
    "consistentBackupRead": True,
    "atomicUnitRestore": True,
}


class _MockFetcher:
    def __init__(self, status: int = 200) -> None:
        self.calls: list[tuple[str, dict]] = []
        self._status = status

    async def fetch(self, url: str, **kwargs):
        self.calls.append((url, kwargs))
        status = self._status

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
                    return encode_db_capabilities_reply(_CAPS)
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

        for status, code in ((401, "unauthorized"), (413, "limit_exceeded"), (503, "unavailable")):
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
        fetcher = _MockFetcher()
        databases = granted_databases(fetcher, {"DB": "grant-token"})
        self.assertEqual(sorted(databases), ["DB"])
        row = await databases["DB"].prepare("SELECT 1").first()
        self.assertEqual(len(fetcher.calls), 1)
        url, kwargs = fetcher.calls[0]
        self.assertEqual(url, "http://granted/db/execute")
        self.assertEqual(kwargs["headers"]["Authorization"], "Bearer grant-token")
        self.assertEqual(kwargs["headers"]["content-type"], "application/octet-stream")
        self.assertIsInstance(kwargs["body"], (bytes, bytearray))
        self.assertEqual(row["n"]["value"], 1)


if __name__ == "__main__":
    unittest.main()


class _MockTransport:
    """Adapter `GrantedDatabaseTransport` stub: Cap'n bytes in, Cap'n bytes out."""

    def __init__(self) -> None:
        self.requests: list[bytes] = []

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
        row = await env.DB.prepare("SELECT 7").first()
        self.assertEqual(row["n"]["value"], 7)
        self.assertEqual(len(transport.requests), 1, "one Cap'n request crossed the stub")

    async def test_ready_binding_wins_over_transport(self) -> None:
        from bookclerk_plugin_sdk.workerd import _invocation_env

        marker = object()
        env = _invocation_env(
            {}, {"databases": [{"name": "DB", "database": marker, "transport": 1}]}
        )
        self.assertIs(env.DB, marker)
