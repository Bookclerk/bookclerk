"""Granted workerd database binding uses async transport."""

from __future__ import annotations

import unittest

from bookclerk_plugin_sdk.workerd import granted_databases


class _MockFetcher:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict]] = []

    async def fetch(self, url: str, **kwargs):
        self.calls.append((url, kwargs))

        class _Resp:
            status = 200

            async def arrayBuffer(self):
                from bookclerk_plugin_sdk.db_value import encode_execute_result_reply

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

        return _Resp()


class WorkerdGrantedDbTests(unittest.IsolatedAsyncioTestCase):
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
        env = _invocation_env({}, {"databases": [{"name": "DB", "database": marker, "transport": 1}]})
        self.assertIs(env.DB, marker)
