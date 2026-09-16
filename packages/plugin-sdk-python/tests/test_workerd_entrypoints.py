"""v3 author model: event / job outcome translation and named dispatch (authoring stubs)."""

from __future__ import annotations

import unittest

from bookclerk_plugin_sdk.workerd import (
    BookclerkEntrypoint,
    CliEntrypoint,
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
        self.assertEqual(retry, {"kind": "retryable", "message": "try later", "retryAfterUnixMs": 0})

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
                return {"exitCode": 0, "stdout": args["message"], "stderr": "", "payload": json_payload(args)}

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


if __name__ == "__main__":
    unittest.main()
