"""Tests the object-store destination helpers and the queue change feed.

They run against an in-memory backend, a fake webhook queue and a fake KV
binding. Key translation runs the vectors the Rust storage helpers and the
TypeScript SDK check too.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path
from typing import Any

from bookclerk_plugin_sdk.object_store import (
    NO_FEED_CURSOR,
    ObjectStoreDestination,
    ParsedChange,
    QueueFeed,
    WriteCondition,
    check_byte_range,
    clamp_page_limit,
    order_changes,
    parse_host_key,
    parse_queue_cursor,
    require_disjoint_prefixes,
    stage_key,
    translate_for_object_store,
    webhook_queue,
    write_condition,
)
from bookclerk_plugin_sdk.testkit import TestKv, TestWebhooks
from bookclerk_plugin_sdk.workerd import PluginError

REPO_ROOT = next(
    p for p in Path(__file__).resolve().parents if (p / "crates" / "bookclerk-plugin-abi").is_dir()
)

VECTORS = REPO_ROOT / "packages" / "plugin-sdk" / "scripts" / "fixtures" / "object-store-keys.json"
"""Translation vectors shared with Rust (``key_syntax.rs``) and the TypeScript SDK."""


class _Body:
    """A body the backend reads whole, recording a cancel."""

    def __init__(self, data: bytes | str) -> None:
        """Hold ``data``.

        Args:
            data: The bytes, or text encoded as UTF-8.
        """
        self.data = data.encode() if isinstance(data, str) else data
        self.cancelled = False

    def cancel(self) -> None:
        """Record that the writer gave up on the body."""
        self.cancelled = True


class _MemoryStore:
    """A backend over a dict that counts calls."""

    def __init__(self, *, server_copy: bool = True, conditional_writes: str | None = None) -> None:
        """Start empty.

        Args:
            server_copy: Whether ``copy`` stays on the server.
            conditional_writes: ``"atomic"``, ``"checked"`` or ``None`` (the
                destination probes first).
        """
        self.objects: dict[str, tuple[bytes, dict[str, Any]]] = {}
        self.calls: list[str] = []
        self.server_copy = server_copy
        self.prefix_bytes = 8
        self.version = 0
        if conditional_writes is not None:
            self.conditional_writes = conditional_writes

    async def head(self, key: str) -> dict[str, Any] | None:
        """Return metadata or ``None``.

        Args:
            key: Object key.

        Returns:
            A copy of the metadata.
        """
        self.calls.append(f"head {key}")
        found = self.objects.get(key)
        return None if found is None else dict(found[1])

    async def list(self, prefix: str, cursor: str | None, limit: int) -> dict[str, Any]:
        """Return one page of keys after ``cursor``.

        Args:
            prefix: Key prefix.
            cursor: The last key of the previous page.
            limit: Page size.

        Returns:
            The page.
        """
        keys = sorted(
            k for k in self.objects if k.startswith(prefix) and (cursor is None or k > cursor)
        )
        page = keys[:limit]
        out: dict[str, Any] = {
            "objects": [{"key": k, "size": len(self.objects[k][0])} for k in page]
        }
        if len(keys) > limit:
            out["nextCursor"] = page[-1]
        return out

    async def get(self, key: str, byte_range: dict[str, int] | None) -> tuple[dict[str, Any], Any]:
        """Return metadata and a body.

        Args:
            key: Object key.
            byte_range: Ignored.

        Returns:
            The metadata and the body.

        Raises:
            PluginError: ``not_found``.
        """
        found = self.objects.get(key)
        if found is None:
            raise PluginError.from_wire("not_found", key)
        return dict(found[1]), _Body(found[0])

    async def put(self, key: str, body: _Body, write: dict[str, Any]) -> dict[str, Any]:
        """Store ``body``.

        Args:
            key: Object key.
            body: The bytes.
            write: Content type and commit token.

        Returns:
            Bytes written and the new etag.
        """
        self.calls.append(f"put {key}")
        self.version += 1
        meta: dict[str, Any] = {"key": key, "size": len(body.data), "etag": f"v{self.version}"}
        for name in ("contentType", "commitToken"):
            if write.get(name) is not None:
                meta[name] = write[name]
        self.objects[key] = (body.data, meta)
        return {"bytesWritten": len(body.data), "etag": meta["etag"]}

    async def put_conditional(
        self, key: str, body: _Body, write: dict[str, Any], condition: WriteCondition
    ) -> dict[str, Any]:
        """Store ``body`` when ``condition`` holds.

        Args:
            key: Object key.
            body: The bytes.
            write: Content type and commit token.
            condition: The precondition.

        Returns:
            Bytes written and the new etag.

        Raises:
            PluginError: ``conflict`` when the condition fails.
        """
        self.calls.append(f"put_conditional {key} {condition.kind}")
        current = self.objects.get(key)
        if condition.kind == "ifNoneMatch" and current is not None:
            raise PluginError.from_wire("conflict", f"{key} already exists")
        if condition.kind == "ifMatch" and (
            current is None or current[1]["etag"] != condition.etag
        ):
            raise PluginError.from_wire("conflict", f"{key} changed")
        return await self.put(key, body, write)

    async def copy(self, source: str, target: str) -> int:
        """Copy on the server.

        Args:
            source: Source key.
            target: Target key.

        Returns:
            Bytes copied.

        Raises:
            PluginError: ``not_found``.
        """
        self.calls.append(f"copy {source} {target}")
        found = self.objects.get(source)
        if found is None:
            raise PluginError.from_wire("not_found", source)
        self.version += 1
        self.objects[target] = (found[0], {**found[1], "key": target, "etag": f"v{self.version}"})
        return len(found[0])

    async def delete(self, key: str) -> None:
        """Remove ``key`` if present.

        Args:
            key: Object key.
        """
        self.calls.append(f"delete {key}")
        self.objects.pop(key, None)


def _parse(body: bytes) -> list[ParsedChange]:
    """Read a test body: comma-separated keys, ``!`` for unreadable.

    Args:
        body: The body.

    Returns:
        One change per key.

    Raises:
        ValueError: For ``!``.
    """
    text = body.decode()
    if text == "!":
        raise ValueError("unreadable")
    return [ParsedChange({"key": key, "kind": "changed"}) for key in text.split(",")]


def _keys(page: dict[str, Any]) -> list[str]:
    """Return the keys of a feed page.

    Args:
        page: The page.

    Returns:
        Its change keys.
    """
    return [change["key"] for change in page["changes"]]


class HelpersTest(unittest.TestCase):
    """The key, range, stage and condition helpers."""

    def _invalid(self, run: Any, code: str = "invalid_params") -> None:
        """Assert ``run`` raises a plugin error of ``code``.

        Args:
            run: The call.
            code: The expected code.
        """
        with self.assertRaises(PluginError) as ctx:
            run()
        self.assertEqual(ctx.exception.code, code)

    def test_translation_matches_the_shared_vectors(self) -> None:
        """Every shared vector translates as in Rust and TypeScript."""
        vectors = json.loads(VECTORS.read_text(encoding="utf-8"))
        self.assertGreater(len(vectors), 10)
        for case in vectors:
            got = translate_for_object_store(parse_host_key(case["key"]), case["prefixBytes"])
            self.assertEqual(got, case["expected"], case["key"])

    def test_host_keys_refuse_what_the_syntax_forbids(self) -> None:
        """Empty, absolute, dotted, backslashed and control keys are refused."""
        for bad in ("", "/a", "a/", "a//b", "./a", "a/../b", "a\\b", "a\x00b", "a\nb", "a\x85b"):
            self._invalid(lambda bad=bad: parse_host_key(bad))
        self._invalid(lambda: parse_host_key("a" * 1025))
        self.assertEqual(
            parse_host_key("Author: Name/Title?/Book.m4b"), "Author: Name/Title?/Book.m4b"
        )

    def test_small_checks(self) -> None:
        """Ranges, page limits, stage keys, conditions and prefixes."""
        self.assertIsNone(check_byte_range(None))
        self._invalid(lambda: check_byte_range({"offset": 0, "length": 0}))
        self._invalid(lambda: check_byte_range({"offset": 5, "length": 6}, 10))
        self._invalid(lambda: check_byte_range({"offset": 11}, 10))
        self._invalid(lambda: check_byte_range({"offset": -1}))
        self.assertEqual(check_byte_range({"offset": 10}, 10), {"offset": 10})
        for asked, want in ((0, 256), (None, 256), (9999, 256), (7, 7)):
            self.assertEqual(clamp_page_limit(asked), want)
        self.assertEqual(stage_key("tok", "a/b"), ".bookclerk-stage/tok/a/b")
        for bad in ("", "a/b", "a\\b", "a..b", "a\x00", None):
            self._invalid(lambda bad=bad: stage_key(bad, "k"))
        self._invalid(lambda: stage_key("t", ""))
        self.assertIsNone(write_condition({}))
        self.assertEqual(write_condition({"ifMatch": "e"}), WriteCondition("ifMatch", "e"))
        self.assertEqual(write_condition({"ifNoneMatch": True}), WriteCondition("ifNoneMatch"))
        self._invalid(lambda: write_condition({"ifMatch": "e", "ifNoneMatch": True}))
        self._invalid(lambda: write_condition({"ifNoneMatch": True, "stageOnly": True}))
        self._invalid(lambda: require_disjoint_prefixes("", "b/"))
        self._invalid(lambda: require_disjoint_prefixes("a/", "a/b/"))
        self._invalid(lambda: require_disjoint_prefixes("./a/b/", "a\\"))
        require_disjoint_prefixes("a/", "b/")

    def test_queue_cursors_and_ordering(self) -> None:
        """Cursors parse strictly; changes order by time, then sequencer."""
        self.assertEqual(parse_queue_cursor("q1.abc.7"), ("abc", 7))
        for bad in ("", "q2.abc.1", "q1..1", "q1.abc.", "q1.abc.x", "q1.abc"):
            self._invalid(lambda bad=bad: parse_queue_cursor(bad), "invalid_cursor")
        events = [
            ParsedChange({"key": "a"}, "2", "0B"),
            ParsedChange({"key": "b"}),
            ParsedChange({"key": "a"}, "1", "A"),
            ParsedChange({"key": "a"}, "3", "9"),
        ]
        order_changes(events)
        self.assertEqual(
            [f"{e.change['key']}{e.sequencer or '-'}" for e in events], ["b-", "a9", "aA", "a0B"]
        )

    def test_features_follow_the_backend(self) -> None:
        """Conditional writes and a feed add their features."""
        self.assertEqual(
            ObjectStoreDestination.features(object()), ["storage.copy", "storage.translateKeys"]
        )
        self.assertEqual(
            ObjectStoreDestination.features(
                _MemoryStore(conditional_writes="checked"), changes=True
            ),
            ["storage.copy", "storage.translateKeys", "storage.checkedWrite", "storage.changes"],
        )
        self.assertIn(
            "storage.conditionalWrite",
            ObjectStoreDestination.features(_MemoryStore(conditional_writes="atomic")),
        )


class DestinationTest(unittest.IsolatedAsyncioTestCase):
    """``ObjectStoreDestination`` over the in-memory backend."""

    async def _rejects(self, run: Any, code: str) -> None:
        """Assert awaiting ``run()`` raises a plugin error of ``code``.

        Args:
            run: Returns the awaitable.
            code: The expected code.
        """
        with self.assertRaises(PluginError) as ctx:
            await run()
        self.assertEqual(ctx.exception.code, code)

    async def test_staged_writes_commit_idempotently(self) -> None:
        """A stage commits once, a repeat commit succeeds, the head hides the token."""
        backend = _MemoryStore()
        dest = ObjectStoreDestination(backend)
        options = {"commitToken": "tok", "stageOnly": True, "contentType": "audio/mp4"}
        put = await dest.put("a/b.m4b", _Body("hello"), options)
        self.assertEqual(put, {"key": "a/b.m4b", "bytesWritten": 5, "etag": "tok"})
        self.assertIn(".bookclerk-stage/tok/a/b.m4b", backend.objects)
        self.assertIsNone(await dest.head("a/b.m4b"))
        committed = await dest.commit("a/b.m4b", "tok")
        self.assertEqual(committed, {"key": "a/b.m4b", "bytesWritten": 5, "etag": "tok"})
        self.assertNotIn(".bookclerk-stage/tok/a/b.m4b", backend.objects)
        self.assertEqual(backend.objects["a/b.m4b"][0], b"hello")
        self.assertEqual(await dest.commit("a/b.m4b", "tok"), committed)
        await self._rejects(lambda: dest.commit("a/b.m4b", "other"), "not_found")
        await self._rejects(lambda: dest.commit("missing", "tok"), "not_found")
        await dest.abort_stage("a/b.m4b", "tok")
        head = await dest.head("a/b.m4b")
        assert head is not None
        self.assertNotIn("commitToken", head)
        self.assertEqual(head["contentType"], "audio/mp4")
        await self._rejects(lambda: dest.put("k", _Body("x"), {"sha256": bytes(3)}), "internal")
        refused = _Body("x")
        await self._rejects(lambda: dest.put("k", refused, {"stageOnly": True}), "invalid_params")
        self.assertTrue(refused.cancelled)
        plain = await dest.put("p", _Body("x"), {"commitToken": ""})
        self.assertEqual(plain["etag"], "")

    async def test_conditional_writes(self) -> None:
        """The backend's own condition, or a probe first, keeps the rule."""
        for conditional_writes in (None, "checked"):
            backend = _MemoryStore(conditional_writes=conditional_writes)
            dest = ObjectStoreDestination(backend)
            await dest.put("k", _Body("1"), {"ifNoneMatch": True})
            body = _Body("2")
            await self._rejects(
                lambda b=body, d=dest: d.put("k", b, {"ifNoneMatch": True}), "conflict"
            )
            self.assertEqual(body.cancelled, conditional_writes is None)
            head = await dest.head("k")
            assert head is not None
            await dest.put("k", _Body("3"), {"ifMatch": head["etag"]})
            await self._rejects(
                lambda e=head["etag"], d=dest: d.put("k", _Body("4"), {"ifMatch": e}), "conflict"
            )
            self.assertEqual(
                any(c.startswith("put_conditional") for c in backend.calls),
                conditional_writes is not None,
            )

    async def test_moves(self) -> None:
        """Server or streamed copies, the overwrite rule and the source check."""
        for server_copy in (True, False):
            backend = _MemoryStore(server_copy=server_copy)
            dest = ObjectStoreDestination(backend)
            await dest.put("from", _Body("abc"), {})
            self.assertEqual(
                await dest.move("from", "to", {}), {"atomic": False, "alreadyMoved": False}
            )
            self.assertNotIn("from", backend.objects)
            self.assertEqual("copy from to" in backend.calls, server_copy)
            self.assertEqual(
                await dest.move("from", "to", {}), {"atomic": False, "alreadyMoved": True}
            )
            await self._rejects(lambda d=dest: d.move("nope", "nope2", {}), "not_found")
            self.assertEqual(
                await dest.move("to", "to", {}), {"atomic": True, "alreadyMoved": False}
            )
            await dest.put("other", _Body("z"), {})
            await self._rejects(
                lambda d=dest: d.move("to", "other", {"noOverwrite": True}), "conflict"
            )
            await self._rejects(
                lambda d=dest: d.move("to", "x", {"ifSourceMatch": "stale"}), "conflict"
            )
        backend = _MemoryStore(conditional_writes="checked")
        dest = ObjectStoreDestination(backend)
        await dest.put("s", _Body("1"), {})
        await dest.put("t", _Body("2"), {})
        await self._rejects(lambda: dest.move("s", "t", {"noOverwrite": True}), "conflict")
        self.assertIn("put_conditional t ifNoneMatch", backend.calls)

    async def test_batches(self) -> None:
        """Pairs, prefix pages, deletes, probes, translation, list, get and copy."""
        backend = _MemoryStore()
        dest = ObjectStoreDestination(backend)
        for key in ("p/a", "p/b", "p/c"):
            await dest.put(key, _Body(key), {})
        pairs = await dest.move_many(
            {
                "target": {
                    "kind": "pairs",
                    "value": [{"from": "p/a", "to": "q/a"}, {"from": "nope", "to": "q/n"}],
                }
            }
        )
        self.assertEqual(pairs["objectsMoved"], 1)
        self.assertEqual(pairs["results"][0], {"key": "p/a"})
        self.assertEqual(pairs["results"][1]["error"]["code"], "not_found")
        prefix = {"kind": "prefix", "value": {"from": "p/", "to": "r/"}}
        first = await dest.move_many({"target": prefix, "limit": 1})
        self.assertEqual(first["results"], [{"key": "p/b"}])
        self.assertEqual(first["nextCursor"], "p/b")
        second = await dest.move_many({"target": prefix, "cursor": first["nextCursor"], "limit": 1})
        self.assertEqual(second["results"], [{"key": "p/c"}])
        self.assertNotIn("nextCursor", second)
        self.assertEqual(sorted(backend.objects), ["q/a", "r/b", "r/c"])
        await self._rejects(
            lambda: dest.move_many(
                {"target": {"kind": "prefix", "value": {"from": "r/", "to": "r/x/"}}}
            ),
            "invalid_params",
        )
        many = [{"from": "a", "to": "b"}] * 257
        await self._rejects(
            lambda: dest.move_many({"target": {"kind": "pairs", "value": many}}), "invalid_params"
        )
        self.assertEqual(await dest.delete_many(["q/a", "gone"]), [{"key": "q/a"}, {"key": "gone"}])
        probes = await dest.head_many(["r/b", "gone"])
        assert probes[0] is not None
        self.assertEqual(probes[0]["key"], "r/b")
        self.assertIsNone(probes[1])
        await self._rejects(lambda: dest.head_many(["k"] * 257), "invalid_params")
        await self._rejects(lambda: dest.delete_many(["k"] * 257), "invalid_params")
        self.assertEqual(await dest.translate_keys(["A/B{1}.mp3"]), ["A/B_1_.mp3"])
        await self._rejects(lambda: dest.translate_keys(["a//b"]), "invalid_params")
        self.assertEqual(
            await dest.list({"prefix": "r/", "limit": 1}),
            {
                "objects": [{"key": "r/b", "size": 3}],
                "nextCursor": "r/b",
                "signaturesIncluded": False,
                "modifiedSinceApplied": False,
            },
        )
        read = await dest.get("r/c")
        self.assertEqual(read["body"].data, b"p/c")
        await self._rejects(
            lambda: dest.get("r/c", {"range": {"offset": 0, "length": 0}}), "invalid_params"
        )
        self.assertEqual(await dest.copy("r/c", "r/d"), {"bytesCopied": 3})
        await dest.delete("r/d")
        self.assertNotIn("r/d", backend.objects)
        self.assertEqual(
            await dest.changes({}),
            {"changes": [], "cursor": NO_FEED_CURSOR, "more": False, "complete": False},
        )


class QueueFeedTest(unittest.IsolatedAsyncioTestCase):
    """``QueueFeed`` over the fake webhook queue and KV."""

    async def test_pages_acknowledge_once_confirmed(self) -> None:
        """Start, take, confirm, replay, foreign cursors, gaps and a restart."""
        hooks, kv = TestWebhooks(), TestKv()
        feed = QueueFeed(queue=webhook_queue(hooks, "changes"), parse=_parse, kv=kv, state_key="f1")
        dest = ObjectStoreDestination(_MemoryStore(), feed)
        hooks.deliver("changes", b"a")
        hooks.deliver("changes", b"b,c")
        start = await dest.changes({"cursor": NO_FEED_CURSOR})
        self.assertEqual(start["changes"], [])
        self.assertFalse(start["complete"])
        self.assertEqual(hooks.waiting("changes"), 2)
        one = await dest.changes({"cursor": start["cursor"]})
        self.assertEqual(_keys(one), ["a", "b", "c"])
        self.assertTrue(one["complete"])
        self.assertEqual(hooks.acked("changes"), [])
        two = await dest.changes({"cursor": one["cursor"]})
        self.assertEqual(two["changes"], [])
        self.assertTrue(two["complete"])
        self.assertEqual(sorted(hooks.acked("changes")), ["lease-1", "lease-2"])
        hooks.deliver("changes", b"d")
        three = await dest.changes({"cursor": two["cursor"]})
        self.assertEqual(_keys(three), ["d"])
        replay = await dest.changes({"cursor": two["cursor"]})
        self.assertEqual(_keys(replay), ["d"])
        self.assertGreaterEqual(len(hooks.retried("changes")), 1)
        ahead = ".".join(replay["cursor"].split(".")[:2]) + ".999"
        with self.assertRaises(PluginError) as ctx:
            await dest.changes({"cursor": ahead})
        self.assertEqual(ctx.exception.code, "invalid_cursor")
        restart = await dest.changes({"cursor": "q1.otherfeed.3"})
        self.assertFalse(restart["complete"])
        hooks.deliver("changes", b"!")
        bad = await dest.changes({"cursor": restart["cursor"]})
        self.assertFalse(bad["complete"])
        hooks.mark_missed("changes")
        missed = await dest.changes({"cursor": bad["cursor"]})
        self.assertFalse(missed["complete"])
        after = await dest.changes({"cursor": missed["cursor"]})
        self.assertTrue(after["complete"])
        again = QueueFeed(
            queue=webhook_queue(hooks, "changes"), parse=_parse, kv=kv, state_key="f1"
        )
        self.assertTrue((await again.changes(after["cursor"], 0))["complete"])

    async def test_an_unreadable_state_starts_over(self) -> None:
        """A saved state that is not JSON starts a fresh feed instead of failing."""
        hooks, kv = TestWebhooks(), TestKv()
        await kv.put("f3", "{not json")
        feed = QueueFeed(queue=webhook_queue(hooks, "changes"), parse=_parse, kv=kv, state_key="f3")
        page = await feed.changes("q1.gone.4", 0)
        self.assertFalse(page["complete"])
        self.assertTrue(page["cursor"].endswith(".1"))

    async def test_page_limits_and_a_failed_save(self) -> None:
        """Overflow is released, one oversized message is cut, a KV failure fails the call."""
        hooks, kv = TestWebhooks(), TestKv()
        feed = QueueFeed(queue=webhook_queue(hooks, "changes"), parse=_parse, kv=kv, state_key="f2")
        start = await feed.changes(None, 0)
        hooks.deliver("changes", b"a,b")
        hooks.deliver("changes", b"c,d")
        page = await feed.changes(start["cursor"], 3)
        self.assertEqual(_keys(page), ["a", "b"])
        self.assertTrue(page["more"])
        left = await hooks.pull("changes", batch_size=10)
        await hooks.ack("changes", acks=[m.lease_id for m in left.messages])
        hooks.deliver("changes", b"w,x,y,z")
        cut = await feed.changes(page["cursor"], 3)
        self.assertEqual(_keys(cut), ["w", "x", "y"])
        self.assertFalse(cut["complete"])
        hooks.deliver("changes", b"q")
        kv.fail_next("put", PluginError.from_wire("internal", "kv down"))
        with self.assertRaises(PluginError) as ctx:
            await feed.changes(cut["cursor"], 3)
        self.assertEqual(ctx.exception.code, "internal")


if __name__ == "__main__":
    unittest.main()
