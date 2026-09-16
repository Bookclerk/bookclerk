"""Granted workerd database binding uses async transport."""

from __future__ import annotations

import unittest

from bookclerk_plugin_sdk.workerd import granted_job_context


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
        ctx = granted_job_context(fetcher, "grant-token")
        self.assertIsNotNone(ctx.database)
        row = await ctx.database.prepare("SELECT 1").first()
        self.assertEqual(len(fetcher.calls), 1)
        url, kwargs = fetcher.calls[0]
        self.assertEqual(url, "http://granted/db/execute")
        self.assertEqual(kwargs["headers"]["Authorization"], "Bearer grant-token")
        self.assertEqual(kwargs["headers"]["content-type"], "application/octet-stream")
        self.assertIsInstance(kwargs["body"], (bytes, bytearray))
        self.assertEqual(row["n"]["value"], 1)


if __name__ == "__main__":
    unittest.main()
