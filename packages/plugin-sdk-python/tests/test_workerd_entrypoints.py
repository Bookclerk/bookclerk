"""v3 author model: event / job outcome translation and named dispatch (authoring stubs)."""

from __future__ import annotations

import unittest

from bookclerk_plugin_sdk.workerd import (
    AdapterDatabaseSession,
    BookclerkEntrypoint,
    CliEntrypoint,
    DatabaseAdapterEntrypoint,
    EventBatch,
    EventMessage,
    JobController,
    PluginError,
    cli_args,
    decode_extensible_config,
    event_batch_results,
    job_outcome_for,
    json_payload,
)


def _event(event_type: str, payload: bytes = b"") -> dict:
    return {
        "eventId": f"evt-{event_type}",
        "eventType": event_type,
        "schemaVersion": 1,
        "occurredAtUnixMs": 1_700_000_000_000,
        "deliveryAttempt": 2,
        "payload": payload,
    }


class EventMessageTests(unittest.TestCase):
    def test_first_outcome_wins(self) -> None:
        msg = EventMessage(_event("book_acquired", b'{"titleId":"t1"}'))
        self.assertEqual(msg.type, "book_acquired")
        self.assertEqual(msg.attempts, 2)
        self.assertEqual(msg.json(), {"titleId": "t1"})
        msg.retry(delay_seconds=5, reason="later")
        msg.ack()
        assert msg.result is not None
        self.assertEqual(msg.result["kind"], "retry")
        self.assertGreater(msg.result["retryAtUnixMs"], 0)
        self.assertEqual(msg.result["reason"], "later")

    def test_suspend_records_checkpoint(self) -> None:
        msg = EventMessage(_event("test_suspend"))
        msg.suspend(checkpoint={"n": 1}, wake_at=1234, wake_on_event_type="wake")
        self.assertEqual(
            msg.result,
            {
                "kind": "suspended",
                "checkpointJson": '{"n": 1}',
                "checkpointSchemaVersion": 1,
                "wakeAtUnixMs": 1234,
                "wakeOnEventType": "wake",
                "wakeOnFilterJson": "",
            },
        )

    def test_oversized_checkpoint_is_payload_too_large(self) -> None:
        msg = EventMessage(_event("test_suspend"))
        with self.assertRaises(PluginError) as ctx:
            msg.suspend(checkpoint="x" * 70_000)
        self.assertEqual(ctx.exception.code, "payload_too_large")

    def test_batch_results_default_ack_or_retry(self) -> None:
        batch = EventBatch([_event("a"), _event("a")])
        self.assertEqual(batch.type, "a")
        batch.messages[0].reject("no")
        self.assertEqual(
            event_batch_results(batch, None),
            [{"kind": "reject", "reason": "no"}, {"kind": "ack"}],
        )
        self.assertEqual(
            event_batch_results(batch, RuntimeError("boom"))[1],
            {"kind": "retry", "retryAtUnixMs": 0, "reason": "boom"},
        )


class BookclerkEntrypointTests(unittest.IsolatedAsyncioTestCase):
    async def test_event_dispatch_translates_outcomes(self) -> None:
        class Default(BookclerkEntrypoint):
            async def event(self, batch: EventBatch) -> None:
                for msg in batch.messages:
                    if msg.type == "test_dead_letter":
                        msg.dead_letter("dead")

        plugin = Default()
        results = await plugin.bookclerkEvent(
            {"invocation": {"id": "inv-1"}},
            {"events": [_event("book_acquired"), _event("test_dead_letter")]},
        )
        self.assertEqual(results, [{"kind": "ack"}, {"kind": "deadLetter", "reason": "dead"}])

    async def test_event_handler_exception_retries_undecided(self) -> None:
        class Default(BookclerkEntrypoint):
            async def event(self, batch: EventBatch) -> None:
                batch.messages[0].ack()
                raise RuntimeError("half way")

        results = await Default().bookclerkEvent({}, {"events": [_event("a"), _event("b")]})
        self.assertEqual(results[0], {"kind": "ack"})
        self.assertEqual(results[1]["kind"], "retry")
        self.assertEqual(results[1]["reason"], "half way")

    async def test_event_unsupported_without_handler(self) -> None:
        with self.assertRaises(PluginError) as ctx:
            await BookclerkEntrypoint().bookclerkEvent({}, {"events": []})
        self.assertEqual(ctx.exception.code, "unsupported")

    async def test_job_completed_and_rejected(self) -> None:
        class Default(BookclerkEntrypoint):
            async def job(self, job: JobController) -> dict:
                if job.type == "fail":
                    raise PluginError.from_wire("unavailable", "try later")
                return {"message": f"done {job.json()['n']}", "bytesCopied": 3}

        plugin = Default()
        done = await plugin.bookclerkJob(
            {}, {"invocationId": "j1", "commandType": "copy", "payloadJson": '{"n": 7}'}
        )
        self.assertEqual(done, {"kind": "completed", "message": "done 7", "bytesCopied": 3})
        retry = await plugin.bookclerkJob({}, {"invocationId": "j2", "commandType": "fail"})
        self.assertEqual(
            retry, {"kind": "retryable", "message": "try later", "retryAfterUnixMs": 0}
        )

    async def test_job_suspend_and_retry_later(self) -> None:
        class Default(BookclerkEntrypoint):
            async def job(self, job: JobController) -> None:
                if job.attempt == 1:
                    job.suspend(checkpoint={"pos": 1}, wake_at=99)
                else:
                    job.retry_later(reason="busy", retry_at=100)

        plugin = Default()
        suspended = await plugin.bookclerkJob({}, {"invocationId": "j", "attempt": 1})
        self.assertEqual(
            suspended,
            {
                "kind": "suspended",
                "checkpoint": {"schemaVersion": 1, "json": '{"pos": 1}'},
                "wakeAtUnixMs": 99,
            },
        )
        retry = await plugin.bookclerkJob({}, {"invocationId": "j", "attempt": 2})
        self.assertEqual(retry, {"kind": "retryable", "message": "busy", "retryAfterUnixMs": 100})

    async def test_env_receives_granted_bindings(self) -> None:
        seen: dict = {}

        class Default(BookclerkEntrypoint):
            async def event(self, batch: EventBatch) -> None:
                seen["config"] = self.env.CONFIG
                seen["db"] = self.env.DB
                seen["missing"] = self.env.get("NOPE", "fallback")

        marker = object()
        await Default().bookclerkEvent(
            {
                "config": json_payload({"level": "info"}),
                "databases": [{"name": "DB", "database": marker}],
            },
            {"events": [_event("a")]},
        )
        self.assertEqual(seen["config"], {"level": "info"})
        self.assertIs(seen["db"], marker)
        self.assertEqual(seen["missing"], "fallback")

    async def test_describe_optional(self) -> None:
        self.assertIsNone(await BookclerkEntrypoint().bookclerkDescribe())

        class Default(BookclerkEntrypoint):
            def describe(self) -> dict:
                return {"displayName": "Echo"}

        self.assertEqual(await Default().bookclerkDescribe(), {"displayName": "Echo"})

    def test_job_outcome_for_cancelled(self) -> None:
        job = JobController({"invocationId": "j"})
        job.cancel()
        outcome = job_outcome_for(job, None, RuntimeError("stop"))
        self.assertEqual(outcome, {"kind": "cancelled", "message": "stop"})


class NamedEntrypointTests(unittest.IsolatedAsyncioTestCase):
    async def test_cli_invoke_allowlisted(self) -> None:
        class Cli(CliEntrypoint):
            async def invoke(self, params):
                args = cli_args(params)
                return {
                    "exitCode": 0,
                    "stdout": args["message"],
                    "stderr": "",
                    "payload": json_payload(args),
                }

        result = await Cli().bookclerkInvoke(
            {}, "invoke", {"command": "ping", "args": [{"name": "message", "value": "hi"}]}
        )
        self.assertEqual(result["stdout"], "hi")
        self.assertEqual(decode_extensible_config(result["payload"]), {"message": "hi"})
        with self.assertRaises(PluginError) as ctx:
            await Cli().bookclerkInvoke({}, "bookclerkInvoke")
        self.assertEqual(ctx.exception.code, "unsupported")
        with self.assertRaises(PluginError) as ctx:
            await Cli().bookclerkInvoke({}, "not_a_method")
        self.assertEqual(ctx.exception.code, "unsupported")


class DatabaseAdapterSessionTests(unittest.IsolatedAsyncioTestCase):
    async def test_open_session_then_session_calls_by_id(self) -> None:
        dropped: list[list[str]] = []

        class Session(AdapterDatabaseSession):
            async def capabilities(self):
                return {"sqlContractVersion": 1, "atomicBatch": True}

            async def dropUserRelations(self, names):
                dropped.append(list(names))

        class DatabaseAdapter(DatabaseAdapterEntrypoint):
            async def openSession(self):
                return Session()

        adapter = DatabaseAdapter()
        session_id = await adapter.bookclerkInvoke({"invocation": {"id": "inv-1"}}, "openSession")
        self.assertIsInstance(session_id, str)
        self.assertEqual(len(session_id), 32)
        self.assertEqual(adapter.invocation["id"], "inv-1")

        caps = await adapter.bookclerkInvoke({}, "session", session_id, "capabilities")
        self.assertEqual(caps, {"sqlContractVersion": 1, "atomicBatch": True})
        self.assertIsNone(
            await adapter.bookclerkInvoke({}, "session", session_id, "dropUserRelations", ["books"])
        )
        self.assertEqual(dropped, [["books"]])

        with self.assertRaises(PluginError) as ctx:
            await adapter.bookclerkInvoke({}, "session", session_id, "bootstrap")
        self.assertEqual(ctx.exception.code, "unsupported")
        with self.assertRaises(PluginError) as ctx:
            await adapter.bookclerkInvoke({}, "session", session_id, "__class__")
        self.assertEqual(ctx.exception.code, "unsupported")
        with self.assertRaises(PluginError) as ctx:
            await adapter.bookclerkInvoke({}, "session", "nope", "capabilities")
        self.assertEqual(ctx.exception.code, "invalid_params")

        self.assertIsNone(await adapter.bookclerkInvoke({}, "session", session_id, "close"))
        with self.assertRaises(PluginError) as ctx:
            await adapter.bookclerkInvoke({}, "session", session_id, "capabilities")
        self.assertEqual(ctx.exception.code, "invalid_params")

    async def test_open_session_unsupported_by_default(self) -> None:
        with self.assertRaises(PluginError) as ctx:
            await DatabaseAdapterEntrypoint().bookclerkInvoke({}, "openSession")
        self.assertEqual(ctx.exception.code, "unsupported")
        with self.assertRaises(PluginError) as ctx:
            await DatabaseAdapterEntrypoint().bookclerkInvoke({}, "dropUnit", "unit-ref")
        self.assertEqual(ctx.exception.code, "unsupported")
        with self.assertRaises(PluginError) as ctx:
            await DatabaseAdapterEntrypoint().bookclerkInvoke({}, "session")
        self.assertEqual(ctx.exception.code, "invalid_params")

    async def test_drop_unit_dispatch(self) -> None:
        dropped: list[str] = []

        class DatabaseAdapter(DatabaseAdapterEntrypoint):
            async def dropUnit(self, unit_ref: str):
                dropped.append(unit_ref)

        adapter = DatabaseAdapter()
        self.assertIsNone(
            await adapter.bookclerkInvoke({"invocation": {"id": "inv-drop"}}, "dropUnit", "unit-a")
        )
        self.assertEqual(dropped, ["unit-a"])
        self.assertEqual(adapter.invocation["id"], "inv-drop")


if __name__ == "__main__":
    unittest.main()


def _queue_message(message_id: str, body: bytes = b'{"n":1}', **overrides) -> dict:
    return {
        "id": message_id,
        "timestampUnixMs": 1_700_000_000_000,
        "body": body,
        "contentType": "json",
        "attempts": 2,
        "source": "echo",
        **overrides,
    }


class QueueMessageTests(unittest.TestCase):
    def test_fields_and_first_outcome_wins(self) -> None:
        from bookclerk_plugin_sdk.workerd import QueueBatch, QueueMessage, queue_batch_results

        msg = QueueMessage(_queue_message("msg-1"))
        self.assertEqual(msg.id, "msg-1")
        self.assertEqual(msg.timestamp_unix_ms, 1_700_000_000_000)
        self.assertEqual(msg.content_type, "json")
        self.assertEqual(msg.attempts, 2)
        self.assertEqual(msg.source, "echo")
        self.assertEqual(msg.json(), {"n": 1})
        self.assertEqual(msg.text(), '{"n":1}')
        self.assertIsNone(msg.result)
        msg.retry(delay_seconds=2.9, reason="later")
        msg.ack()
        self.assertEqual(msg.result, {"kind": "retry", "delaySeconds": 2, "reason": "later"})

        empty = QueueMessage(_queue_message("msg-2", body=b"", contentType=""))
        self.assertEqual(empty.body, b"")
        self.assertEqual(empty.content_type, "json")
        self.assertIsNone(empty.json())

        batch = QueueBatch("jobs", [_queue_message("a"), _queue_message("b"), _queue_message("c")])
        self.assertEqual(batch.queue, "jobs")
        batch.messages[0].retry(reason="again")
        self.assertEqual(
            queue_batch_results(batch, None),
            [
                {"kind": "retry", "delaySeconds": 0, "reason": "again"},
                {"kind": "ack"},
                {"kind": "ack"},
            ],
        )
        self.assertEqual(
            queue_batch_results(batch, RuntimeError("boom"))[2],
            {"kind": "retry", "delaySeconds": 0, "reason": "boom"},
        )


class QueueDispatchTests(unittest.IsolatedAsyncioTestCase):
    async def test_queue_dispatch_translates_outcomes_and_installs_binding(self) -> None:
        from bookclerk_plugin_sdk.workerd import QueueBatch, QueueBinding

        seen: list = []

        class _Stub:
            calls: list = []

            async def sendBatch(self, messages, options):
                self.calls.append((messages, options))
                return {"messageIds": ["q-1"]}

        stub = _Stub()

        class Default(BookclerkEntrypoint):
            async def queue(self, batch: QueueBatch) -> None:
                seen.append((batch.queue, batch.invocation["id"]))
                notify = self.env.NOTIFY
                assert isinstance(notify, QueueBinding)
                seen.append(await notify.send({"seen": batch.queue}, delay_seconds=3))
                for msg in batch.messages:
                    if msg.id == "msg-2":
                        msg.retry(delay_seconds=5, reason="again")
                    else:
                        msg.ack()

        results = await Default().bookclerkQueue(
            {"invocation": {"id": "inv-q"}, "queues": [{"name": "NOTIFY", "queue": stub}]},
            {"queue": "jobs", "messages": [_queue_message("msg-1"), _queue_message("msg-2")]},
        )
        self.assertEqual(
            results,
            [{"kind": "ack"}, {"kind": "retry", "delaySeconds": 5, "reason": "again"}],
        )
        self.assertEqual(seen, [("jobs", "inv-q"), {"messageIds": ["q-1"]}])
        self.assertEqual(len(stub.calls), 1)
        messages, options = stub.calls[0]
        self.assertEqual(options, {"delaySeconds": 3})
        self.assertEqual(
            messages, [{"body": {"seen": "jobs"}, "contentType": "json", "delaySeconds": 0}]
        )

    async def test_kv_binding_wraps_the_granted_stub(self) -> None:
        from bookclerk_plugin_sdk.workerd import KvNamespace, QueueBatch

        class _KvStub:
            def __init__(self) -> None:
                self.rows: dict = {}
                self.calls: list = []

            async def getWithMetadata(self, key, kind):
                self.calls.append(("get", key, kind))
                row = self.rows.get(key)
                if row is None:
                    return {"value": None, "metadata": None}
                value = row[0].encode() if kind == "arrayBuffer" else row[0]
                return {"value": value, "metadata": row[1]}

            async def put(self, key, value, options):
                self.calls.append(("put", key, options))
                self.rows[key] = (value, options.get("metadata"))

            async def delete(self, key):
                self.calls.append(("delete", key))
                self.rows.pop(key, None)

            async def list(self, options):
                self.calls.append(("list", options))
                return {"keys": [{"name": k} for k in sorted(self.rows)], "list_complete": True}

        stub = _KvStub()
        seen: list = []

        class Default(BookclerkEntrypoint):
            async def queue(self, batch: QueueBatch) -> None:
                kv = self.env.KV
                assert isinstance(kv, KvNamespace)
                seen.append(await kv.get("cursor"))
                await kv.put("cursor", "page-7", expiration_ttl=60, metadata={"page": 7})
                seen.append(await kv.get_with_metadata("cursor"))
                seen.append(await kv.get("cursor", "bytes"))
                seen.append(await kv.list(prefix="cur", limit=10))
                await kv.delete("cursor")
                seen.append(await kv.get("cursor"))
                for msg in batch.messages:
                    msg.ack()

        await Default().bookclerkQueue(
            {"kv": [{"name": "KV", "kv": stub}]},
            {"queue": "jobs", "messages": [_queue_message("a")]},
        )
        self.assertEqual(
            seen,
            [
                None,
                ("page-7", {"page": 7}),
                b"page-7",
                {"keys": [{"name": "cursor"}], "list_complete": True},
                None,
            ],
        )
        self.assertIn(("put", "cursor", {"expirationTtl": 60, "metadata": {"page": 7}}), stub.calls)
        self.assertIn(("list", {"prefix": "cur", "limit": 10}), stub.calls)
        self.assertIn(("get", "cursor", "arrayBuffer"), stub.calls)

    async def test_queue_handler_exception_retries_undecided(self) -> None:
        from bookclerk_plugin_sdk.workerd import QueueBatch

        class Default(BookclerkEntrypoint):
            async def queue(self, batch: QueueBatch) -> None:
                batch.messages[0].ack()
                raise RuntimeError("half way")

        results = await Default().bookclerkQueue(
            {}, {"queue": "jobs", "messages": [_queue_message("a"), _queue_message("b")]}
        )
        self.assertEqual(results[0], {"kind": "ack"})
        self.assertEqual(results[1], {"kind": "retry", "delaySeconds": 0, "reason": "half way"})

    async def test_queue_unsupported_without_handler(self) -> None:
        with self.assertRaises(PluginError) as ctx:
            await BookclerkEntrypoint().bookclerkQueue({}, {"queue": "jobs", "messages": []})
        self.assertEqual(ctx.exception.code, "unsupported")
