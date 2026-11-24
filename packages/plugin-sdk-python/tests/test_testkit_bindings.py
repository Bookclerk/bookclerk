"""The testkit's in-memory ``KV`` and ``WEBHOOKS`` bindings hold the host's limits.

The TypeScript testkit's bindings are checked against the same cases
(``scripts/test-testkit-bindings.mjs``).
"""

from __future__ import annotations

import unittest
from datetime import datetime
from typing import Any

from bookclerk_plugin_sdk.testkit import TestKv, TestWebhooks
from bookclerk_plugin_sdk.workerd import PluginError


class _Case(unittest.IsolatedAsyncioTestCase):
    """Shared assertion."""

    async def _rejects(self, run: Any, code: str) -> None:
        """Assert awaiting ``run()`` raises a plugin error of ``code``.

        Args:
            run: Returns the awaitable.
            code: The expected code.
        """
        with self.assertRaises(PluginError) as ctx:
            await run()
        self.assertEqual(ctx.exception.code, code)


class TestKvTest(_Case):
    """``TestKv``."""

    async def test_values_expiry_listing_and_limits(self) -> None:
        """Values, metadata, expiry, listing, limits and one-shot failures."""
        clock = [1_000_000]
        kv = TestKv(max_value_bytes=16, max_keys=3, now=lambda: clock[0])
        self.assertIsNone(await kv.get("missing"))
        await kv.put("a", "hello", metadata={"n": 1})
        self.assertEqual(await kv.get("a"), "hello")
        self.assertEqual(await kv.get_with_metadata("a"), ("hello", {"n": 1}))
        await kv.put("j", '{"x":[1]}')
        self.assertEqual(await kv.get("j", "json"), {"x": [1]})
        self.assertEqual(await kv.get("a", "bytes"), b"hello")
        await kv.put("t", b"\x01\x02", expiration_ttl=10)
        self.assertIsNotNone(await kv.get("t"))
        await self._rejects(lambda: kv.put("four", "x"), "forbidden")
        await kv.put("a", "again")
        clock[0] += 10_000
        self.assertIsNone(await kv.get("t"))
        await kv.put("four", "x")
        await self._rejects(lambda: kv.put("", "x"), "invalid_params")
        await self._rejects(lambda: kv.put("k" * 513, "x"), "payload_too_large")
        await self._rejects(lambda: kv.get("k" * 513), "payload_too_large")
        await self._rejects(lambda: kv.put("a", "x" * 17), "payload_too_large")
        await self._rejects(lambda: kv.put("a", "x", metadata="m" * 1100), "payload_too_large")
        await self._rejects(lambda: kv.put("a", "x", expiration_ttl=-1), "invalid_params")
        first = await kv.list(limit=2)
        self.assertEqual([k["name"] for k in first["keys"]], ["a", "four"])
        self.assertFalse(first["list_complete"])
        rest = await kv.list(cursor=first["cursor"])
        self.assertEqual(rest, {"keys": [{"name": "j"}], "list_complete": True})
        self.assertEqual((await kv.list(prefix="f"))["keys"], [{"name": "four"}])
        await self._rejects(lambda: kv.list(limit=257), "invalid_params")
        order = TestKv()
        await order.put("a\U0001f600", "1")
        await order.put("a\ufffd", "2")
        self.assertEqual(
            [k["name"] for k in (await order.list())["keys"]], ["a\ufffd", "a\U0001f600"]
        )
        await kv.delete("j")
        await kv.delete("j")
        self.assertEqual(list(kv.snapshot()), ["a", "four"])
        kv.fail_next("get", PluginError.from_wire("unavailable", "down"))
        await self._rejects(lambda: kv.get("a"), "unavailable")
        self.assertEqual(await kv.get("a"), "again")
        with self.assertRaises(ValueError):
            await kv.get("a", "yaml")
        big = TestKv(max_value_bytes=10 * 1024 * 1024)
        await self._rejects(lambda: big.put("k", bytes(1024 * 1024 + 1)), "payload_too_large")


class TestWebhooksTest(_Case):
    """``TestWebhooks``."""

    async def test_leases_acks_retries_and_limits(self) -> None:
        """Leases, acks under the right name, retries first, missed and limits."""
        hooks = TestWebhooks()
        self.assertEqual(await hooks.url("changes"), "https://library.example/hooks/changes")
        self.assertIsNone(await TestWebhooks(base_url=None).url("changes"))
        hooks.deliver("changes", "one", {"x-sig": "s"})
        hooks.deliver("changes", b"\x02")
        hooks.deliver("other", "elsewhere")
        first = await hooks.pull("changes", batch_size=1)
        self.assertFalse(first.missed)
        (one,) = first.messages
        self.assertEqual(one.body, b"one")
        self.assertEqual(one.headers, {"x-sig": "s"})
        self.assertEqual(one.attempts, 1)
        self.assertIsInstance(one.timestamp, datetime)
        self.assertEqual(hooks.waiting("changes"), 1)
        await hooks.ack("changes", retries=[one.lease_id])
        self.assertEqual(hooks.retried("changes"), [one.lease_id])
        again = await hooks.pull("changes")
        self.assertEqual(len(again.messages), 2)
        self.assertEqual(again.messages[0].attempts, 2)
        await hooks.ack("other", acks=[again.messages[0].lease_id])
        self.assertEqual(hooks.acked("other"), [])
        await hooks.ack("changes", acks=[m.lease_id for m in again.messages])
        self.assertEqual(len(hooks.acked("changes")), 2)
        self.assertEqual(hooks.waiting("changes"), 0)
        hooks.mark_missed("changes")
        self.assertTrue((await hooks.pull("changes")).missed)
        self.assertFalse((await hooks.pull("changes")).missed)
        await self._rejects(lambda: hooks.pull("changes", batch_size=0), "invalid_params")
        await self._rejects(lambda: hooks.pull("changes", batch_size=101), "invalid_params")
        await self._rejects(
            lambda: hooks.pull("changes", visibility_timeout_ms=43_200_001), "invalid_params"
        )
        await self._rejects(lambda: hooks.pull("Changes"), "invalid_params")
        await self._rejects(lambda: hooks.url("bad name"), "invalid_params")
        await self._rejects(lambda: hooks.ack("changes", acks=["bad lease!"]), "invalid_params")
        await self._rejects(lambda: hooks.ack("changes", acks=["l"] * 101), "invalid_params")
        await self._rejects(
            lambda: hooks.ack("changes", acks=["l1"], retries=["l1"]), "invalid_params"
        )


if __name__ == "__main__":
    unittest.main()
