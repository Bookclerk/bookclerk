"""v3 author model: event / job outcome translation and named dispatch (authoring stubs)."""

from __future__ import annotations

import asyncio
import unittest
from unittest import mock

from bookclerk_plugin_sdk import workerd as workerd_module
from bookclerk_plugin_sdk.workerd import (
    AdapterDatabaseSession,
    AiEntrypoint,
    BookclerkEntrypoint,
    CliEntrypoint,
    MediaDownloadEntrypoint,
    DatabaseAdapterEntrypoint,
    EventBatch,
    EventMessage,
    IngressEntrypoint,
    JobController,
    MetadataEntrypoint,
    MetadataFilesEntrypoint,
    NotifierEntrypoint,
    PluginError,
    ProvisioningEntrypoint,
    RemoteLibraryEntrypoint,
    ScheduledController,
    StorageEntrypoint,
    StorefrontEntrypoint,
    cli_args,
    decode_extensible_config,
    event_batch_results,
    job_outcome_for,
    json_payload,
    provisioned,
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

    async def test_scheduled_receives_a_frozen_controller(self) -> None:
        seen: list[ScheduledController] = []

        class Default(BookclerkEntrypoint):
            async def scheduled(self, controller: ScheduledController) -> None:
                seen.append(controller)

        await Default().bookclerkScheduled(
            {},
            {
                "cron": "0 3 * * MON",
                "scheduledTime": 1_700_000_040_000,
                "fireId": "job-1",
                "attempt": 2,
                "deadlineUnixMs": 1_700_000_100_000,
            },
        )
        self.assertEqual(
            seen,
            [
                ScheduledController(
                    cron="0 3 * * MON",
                    scheduled_time=1_700_000_040_000,
                    fire_id="job-1",
                    attempt=2,
                    deadline_unix_ms=1_700_000_100_000,
                )
            ],
        )
        with self.assertRaises(AttributeError):
            seen[0].attempt = 3  # type: ignore[misc]
        self.assertEqual(ScheduledController.from_wire({}), ScheduledController())

    async def test_scheduled_sees_the_host_cancel(self) -> None:
        """The host's cancel watch marks the fire cancelled while the handler runs."""
        released = asyncio.Event()
        observed: list[bool] = []

        class Watch:
            async def wait(self) -> None:
                await released.wait()

        class Default(BookclerkEntrypoint):
            async def scheduled(self, controller: ScheduledController) -> None:
                observed.append(controller.cancelled)
                released.set()
                await controller.wait_cancelled()
                observed.append(controller.cancelled)

        await Default().bookclerkScheduled({}, {"cron": "* * * * *"}, Watch())
        self.assertEqual(observed, [False, True])
        self.assertFalse(ScheduledController().cancelled)

    async def test_scheduled_errors_propagate_and_missing_is_unsupported(self) -> None:
        class Default(BookclerkEntrypoint):
            def scheduled(self, controller: ScheduledController) -> None:
                raise PluginError.from_wire("unavailable", controller.cron)

        with self.assertRaises(PluginError) as ctx:
            await Default().bookclerkScheduled({}, {"cron": "* * * * *"})
        self.assertEqual(ctx.exception.code, "unavailable")
        with self.assertRaises(PluginError) as ctx:
            await BookclerkEntrypoint().bookclerkScheduled({}, {})
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


class PurchaseHintsDefaultTests(unittest.IsolatedAsyncioTestCase):
    async def test_named_products_go_first_and_answers_keep_their_order(self) -> None:
        asked: list[str] = []

        class Store(StorefrontEntrypoint):
            async def purchaseHint(self, params):
                asked.append(params.get("title", ""))
                return {"productId": params.get("productId") or "searched"}

        store = Store.__new__(Store)
        out = await store.purchaseHints(
            {"queries": [{"title": "by-title"}, {"title": "by-id", "productId": "p1"}]}
        )
        self.assertEqual(asked, ["by-id", "by-title"])
        self.assertEqual([h["productId"] for h in out], ["searched", "p1"])

    async def test_nothing_starts_past_the_budget(self) -> None:
        class Store(StorefrontEntrypoint):
            async def purchaseHint(self, params):
                return {"productId": "p"}

        store = Store.__new__(Store)
        with mock.patch.object(workerd_module, "PURCHASE_HINTS_BUDGET_SECONDS", 0.0):
            out = await store.purchaseHints({"queries": [{"title": "late"}]})
        self.assertEqual(out, [None])

    async def test_a_few_lookups_overlap_and_a_failure_fails_the_batch(self) -> None:
        running = 0
        peak = 0

        class Store(StorefrontEntrypoint):
            async def purchaseHint(self, params):
                nonlocal running, peak
                running += 1
                peak = max(peak, running)
                await asyncio.sleep(0.01)
                running -= 1
                return {"productId": params["productId"]} if params.get("productId") else None

        queries = [{"productId": f"p{n}"} if n % 3 else {"title": f"t{n}"} for n in range(10)]
        out = await Store.__new__(Store).purchaseHints({"queries": queries})
        self.assertEqual(
            [h["productId"] if h else None for h in out],
            [q.get("productId") for q in queries],
        )
        self.assertEqual(peak, workerd_module.PURCHASE_HINTS_CONCURRENCY)

        class Limited(StorefrontEntrypoint):
            async def purchaseHint(self, params):
                raise PluginError("rate_limited", "slow down")

        with self.assertRaises(PluginError) as ctx:
            await Limited.__new__(Limited).purchaseHints({"queries": queries})
        self.assertEqual(ctx.exception.code, "rate_limited")


class NamedEntrypointTests(unittest.IsolatedAsyncioTestCase):
    async def test_remote_library_poll_and_ack(self) -> None:
        class Acking(RemoteLibraryEntrypoint):
            def __init__(self) -> None:
                super().__init__()
                self.pending = ["ada"]
                self.seen: list[object] = []

            async def pollEvents(
                self, params: dict[str, object] | None = None
            ) -> list[dict[str, str]]:
                self.seen.append(params)
                return [
                    {"provider": "test", "externalUserId": user, "displayName": ""}
                    for user in self.pending
                ]

            async def ackEvents(self, params: dict[str, list[str]]) -> None:
                self.pending = [u for u in self.pending if u not in params["externalUserIds"]]

        plugin = Acking()
        polled = await plugin.bookclerkInvoke({}, "pollEvents", {"ackFollows": True})
        self.assertEqual([u["externalUserId"] for u in polled], ["ada"])
        self.assertEqual(plugin.seen, [{"ackFollows": True}])
        await plugin.bookclerkInvoke({}, "ackEvents", {"externalUserIds": ["ada"]})
        self.assertEqual(await plugin.bookclerkInvoke({}, "pollEvents", {"ackFollows": True}), [])

        class Legacy(RemoteLibraryEntrypoint):
            async def pollEvents(self) -> list[dict[str, str]]:
                return [{"provider": "test", "externalUserId": "old", "displayName": ""}]

        legacy = await Legacy().bookclerkInvoke({}, "pollEvents", {"ackFollows": True})
        self.assertEqual([u["externalUserId"] for u in legacy], ["old"])
        self.assertIsNone(
            await Legacy().bookclerkInvoke({}, "ackEvents", {"externalUserIds": ["old"]})
        )
        with self.assertRaises(PluginError) as ctx:
            await RemoteLibraryEntrypoint().bookclerkInvoke({}, "pollEvents", {})
        self.assertEqual(ctx.exception.code, "unsupported")

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


class StorageOperationTests(unittest.IsolatedAsyncioTestCase):
    async def test_intent_operations_dispatch_and_default_to_unsupported(self) -> None:
        """A storage guest answers only the intent operations it overrides."""

        class Storage(StorageEntrypoint):
            async def deleteMany(self, keys):
                return [{"key": k} for k in keys]

        storage = Storage()
        self.assertEqual(
            await storage.bookclerkInvoke({}, "deleteMany", ["a", "b"]),
            [{"key": "a"}, {"key": "b"}],
        )
        calls = {
            "move": ("a", "b", {}),
            "moveMany": ({"target": {"kind": "pairs", "value": []}},),
            "headMany": (["a"],),
            "changes": ({"cursor": ""},),
            "translateKeys": (["a/b.m4b"],),
        }
        for method, args in calls.items():
            with self.assertRaises(PluginError) as ctx:
                await storage.bookclerkInvoke({}, method, *args)
            self.assertEqual(ctx.exception.code, "unsupported", method)

    def test_change_records_round_trip_without_info_and_with_new_kinds(self) -> None:
        """A change may omit `info`, and a kind newer than this SDK reads as `changed`."""
        from bookclerk_plugin_sdk import _wire

        codec = _wire._object_change_codec
        removed = _wire._decode_message(
            codec, _wire._encode_message(codec, {"key": "a/2", "kind": "removed"})
        )
        self.assertEqual((removed["key"], removed["kind"]), ("a/2", "removed"))

        def write_future(s, _v, _caps) -> None:
            s.set_text(0, "k")
            s.set_u16(0, 3)

        future_codec = _wire._Codec(codec.data_words, codec.pointer_count, write_future, codec.read)
        future = _wire._decode_message(codec, _wire._encode_message(future_codec, {}))
        self.assertEqual((future["key"], future["kind"]), ("k", "changed"))


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

    async def test_metadata_defaults(self) -> None:
        metadata = MetadataEntrypoint()
        self.assertEqual(await metadata.health(), {"ok": True, "detail": ""})
        for method in ("lookup", "search", "chapters"):
            with self.assertRaises(PluginError) as ctx:
                await getattr(metadata, method)({})
            self.assertEqual(ctx.exception.code, "unsupported")

    async def test_metadata_dispatch_uses_overrides(self) -> None:
        class Metadata(MetadataEntrypoint):
            async def lookup(self, params: object) -> list[dict]:
                return [{"title": "Dune", "params": params}]

            async def chapters(self, _params: object) -> dict:
                return {"found": False, "chapters": [], "accurate": False}

        metadata = Metadata()
        records = await metadata.bookclerkInvoke({}, "lookup", {"limit": 1})
        self.assertEqual(records, [{"title": "Dune", "params": {"limit": 1}}])
        chapters = await metadata.bookclerkInvoke({}, "chapters", {})
        self.assertFalse(chapters["found"])
        self.assertEqual((await metadata.bookclerkInvoke({}, "health"))["ok"], True)
        with self.assertRaises(PluginError) as ctx:
            await metadata.bookclerkInvoke({}, "search", {"title": "Dune"})
        self.assertEqual(ctx.exception.code, "unsupported")

    async def test_media_download_defaults_and_dispatch(self) -> None:
        downloader = MediaDownloadEntrypoint()
        self.assertEqual(await downloader.health(), {"ok": True, "detail": ""})
        with self.assertRaises(PluginError) as ctx:
            await downloader.downloadMedia({"urls": [], "maxBytes": 1}, object())
        self.assertEqual(ctx.exception.code, "unsupported")

        class MediaDownload(MediaDownloadEntrypoint):
            async def downloadMedia(self, params: dict, sink: object) -> list[dict]:
                return [{"url": u, "stored": False, "error": repr(sink)} for u in params["urls"]]

        images = await MediaDownload().bookclerkInvoke({}, "downloadMedia", {"urls": ["https://a/"]}, "sink")
        self.assertEqual(images, [{"url": "https://a/", "stored": False, "error": "'sink'"}])

    async def test_provisioning_defaults_and_dispatch(self) -> None:
        """ProvisioningEntrypoint refuses by default and dispatches overrides."""
        setup = ProvisioningEntrypoint()
        for method in (setup.provision, setup.deprovision):
            with self.assertRaises(PluginError) as ctx:
                await method({})
            self.assertEqual(ctx.exception.code, "unsupported")

        class Bucket(ProvisioningEntrypoint):
            async def provision(self, request: dict) -> dict:
                return provisioned(
                    settings={"bucket": "books", "port": 443},
                    secrets={"access_key_id": request["grant"]["token"]},
                    summary=["Created bucket books"],
                )

        done = await Bucket().bookclerkInvoke(
            {}, "provision", {"grant": {"kind": "apiToken", "token": "id-1"}, "inputsJson": "{}"}
        )
        self.assertEqual(
            done,
            {
                "settingsJson": '{"bucket":"books","port":443}',
                "secrets": [{"key": "access_key_id", "value": "id-1"}],
                "summary": ["Created bucket books"],
            },
        )

    async def test_ingress_defaults_and_dispatch(self) -> None:
        """IngressEntrypoint refuses start, reports serving, stops, and dispatches overrides."""
        ingress = IngressEntrypoint()
        with self.assertRaises(PluginError) as ctx:
            await ingress.start()
        self.assertEqual(ctx.exception.code, "unsupported")
        self.assertEqual(await ingress.health(), {"serving": True})
        self.assertIsNone(await ingress.stop())

        class Ddns(IngressEntrypoint):
            async def start(self) -> dict:
                return {"origin": "https://lib.example.net"}

        started = await Ddns().bookclerkInvoke({}, "start")
        self.assertEqual(started, {"origin": "https://lib.example.net"})

    async def test_metadata_files_defaults_and_dispatch(self) -> None:
        """MetadataFilesEntrypoint reports healthy, refuses parse/render, and dispatches overrides."""
        files = MetadataFilesEntrypoint()
        self.assertEqual(await files.health(), {"ok": True, "detail": ""})
        for method, params in (
            ("parse", {"dialect": "opf", "fileName": "a.opf", "bytes": b""}),
            (
                "render",
                {"dialect": "opf", "record": {"present": []}, "fields": [], "existing": b""},
            ),
        ):
            with self.assertRaises(PluginError) as ctx:
                await getattr(files, method)(params)
            self.assertEqual(ctx.exception.code, "unsupported")

        class Files(MetadataFilesEntrypoint):
            async def parse(self, params: dict) -> dict:
                return {"present": ["title"], "title": params["bytes"].decode()}

            async def render(self, params: dict) -> str:
                return params["record"]["title"]

        record = await Files().bookclerkInvoke(
            {}, "parse", {"dialect": "txt", "fileName": "desc.txt", "bytes": b"Hi"}
        )
        self.assertEqual(record, {"present": ["title"], "title": "Hi"})
        rendered = await Files().bookclerkInvoke(
            {}, "render", {"dialect": "txt", "record": {"title": "Hi"}, "fields": ["title"]}
        )
        self.assertEqual(rendered, "Hi")

        class JsParams:
            """Stands in for a JsProxy whose ``to_py()`` nests a ``memoryview``."""

            def to_py(self) -> dict:
                return {
                    "dialect": "txt",
                    "fileName": "desc.txt",
                    "bytes": memoryview(b"Nested"),
                    "list": [memoryview(b"x")],
                }

        class Echo(MetadataFilesEntrypoint):
            async def parse(self, params: dict) -> dict:
                self.seen = params
                return {"present": ["title"], "title": params["bytes"].decode()}

        echo = Echo()
        record = await echo.bookclerkInvoke({}, "parse", JsParams())
        self.assertEqual(record, {"present": ["title"], "title": "Nested"})
        self.assertEqual(echo.seen["list"], [b"x"])
        self.assertIsInstance(echo.seen["bytes"], bytes)

    async def test_ai_defaults_and_dispatch(self) -> None:
        """AiEntrypoint is healthy, lists nothing, refuses run, and dispatches overrides."""
        from bookclerk_plugin_sdk.workerd import pack_embedding_vectors, unpack_embedding_vectors

        models = AiEntrypoint()
        self.assertEqual(await models.health(), {"ok": True, "detail": ""})
        self.assertEqual(await models.models(), {"models": []})
        with self.assertRaises(PluginError) as ctx:
            await models.run({"model": "tiny", "input": {}})
        self.assertEqual(ctx.exception.code, "unsupported")

        class Tiny(AiEntrypoint):
            async def run(self, params: dict) -> dict:
                texts = params["input"]["textEmbeddings"]["texts"]
                vectors = pack_embedding_vectors([[float(len(t["text"])), 0.5] for t in texts])
                return {
                    "output": {
                        "textEmbeddings": {
                            "dimensions": 2,
                            "vectors": vectors,
                            "revision": "r1",
                            "fallbacks": [],
                        }
                    }
                }

        ran = await Tiny().bookclerkInvoke(
            {},
            "run",
            {
                "model": "tiny",
                "input": {"textEmbeddings": {"texts": [{"text": "abc", "role": "document"}]}},
            },
        )
        out = ran["output"]["textEmbeddings"]
        self.assertEqual(unpack_embedding_vectors(out["vectors"], 2), [[3.0, 0.5]])
        with self.assertRaises(PluginError):
            pack_embedding_vectors([[1.0, 2.0], [3.0]])
        with self.assertRaises(PluginError):
            unpack_embedding_vectors(b"\x00" * 6, 2)

    async def test_ai_binding_wraps_the_granted_stub(self) -> None:
        """``env.AI`` forwards runs in the adapter's shape and refuses a run without a model."""
        from bookclerk_plugin_sdk.workerd import AiBinding

        class _AiStub:
            def __init__(self) -> None:
                self.calls: list[tuple] = []

            async def run(self, model: str, request: dict) -> dict:
                self.calls.append((model, request))
                return {
                    "shape": [1, 2],
                    "data": [[0.5, -1]],
                    "model": "@embedder/tiny",
                    "revision": "r1",
                    "fallbacks": [0],
                }

        stub = _AiStub()
        seen: list = []

        class Tagger(AiEntrypoint):
            async def run(self, params: dict) -> dict:
                ai = self.env.AI
                assert isinstance(ai, AiBinding)
                seen.append(await ai.run("@library/text-embeddings", text="a query", role="query"))
                for bad in ({"model": "", "text": "a"}, {"model": "@embedder/tiny", "text": 3}):
                    try:
                        await ai.run(bad["model"], text=bad["text"])
                    except PluginError as err:
                        seen.append(err.code)
                return {}

        await Tagger().bookclerkInvoke(
            {"ai": _Settled(stub)}, "run", {"model": "tiny", "input": {}}
        )
        self.assertEqual(
            seen,
            [
                {
                    "shape": [1, 2],
                    "data": [[0.5, -1.0]],
                    "model": "@embedder/tiny",
                    "revision": "r1",
                    "fallbacks": [0],
                },
                "invalid_params",
                "invalid_params",
            ],
        )
        self.assertEqual(
            stub.calls, [("@library/text-embeddings", {"text": ["a query"], "role": "query"})]
        )

    async def test_notifier_defaults_and_dispatch(self) -> None:
        """NotifierEntrypoint is healthy, refuses send, echoes addresses, and dispatches overrides."""
        notifier = NotifierEntrypoint()
        self.assertEqual(await notifier.health(), {"ok": True, "detail": ""})
        with self.assertRaises(PluginError) as ctx:
            await notifier.send({"message": {}, "destination": {}})
        self.assertEqual(ctx.exception.code, "unsupported")
        self.assertEqual(
            await notifier.validateDestination({"addressJson": '{"chat":"1"}'}),
            {"addressJson": '{"chat":"1"}'},
        )

        class Chat(NotifierEntrypoint):
            async def send(self, params: dict) -> dict:
                return {"providerId": params["destination"]["destinationId"]}

        sent = await Chat().bookclerkInvoke(
            {},
            "send",
            {"message": {"title": "Hi"}, "destination": {"kind": "channel", "destinationId": "d1"}},
        )
        self.assertEqual(sent, {"providerId": "d1"})

    async def test_events_binding_returns_the_abi_reply_unions(self) -> None:
        """``env.EVENTS`` sends the ABI request dicts and maps settled replies onto the unions."""
        from bookclerk_plugin_sdk.workerd import EventsBinding

        class _EventsStub:
            def __init__(self) -> None:
                self.calls: list[tuple[str, list]] = []

            async def settle(self, method: str, args: list) -> dict:
                self.calls.append((method, args))
                if method == "publish" and args[0]["eventType"] == "denied":
                    return {
                        "err": {
                            "code": "forbidden",
                            "message": "not granted",
                            "details": '{"type":"denied"}',
                        }
                    }
                if method == "publish":
                    return {"ok": {"eventId": "evt-1", "duplicate": False}}
                return {
                    "ok": {
                        "events": [{"eventId": "evt-0"}],
                        "cursor": "v1.next",
                        "more": True,
                        "expiredEventIds": [],
                        "truncated": False,
                    }
                }

        stub = _EventsStub()
        seen: list = []

        class Chat(NotifierEntrypoint):
            async def send(self, params: dict) -> None:
                events = self.env.EVENTS
                assert isinstance(events, EventsBinding)
                seen.append(await events.publish({"eventType": "pinged", "payload": b"{}"}))
                seen.append(await events.publish({"eventType": "denied", "payload": b""}))
                seen.append(await events.read({"after": "v1.start", "types": ["pinged"]}))
                return None

        await Chat().bookclerkInvoke({"events": stub}, "send", {"message": {}, "destination": {}})
        self.assertEqual(
            seen,
            [
                {"ok": {"eventId": "evt-1", "duplicate": False}},
                {
                    "err": {
                        "code": "forbidden",
                        "message": "not granted",
                        "details": '{"type":"denied"}',
                    }
                },
                {
                    "ok": {
                        "events": [{"eventId": "evt-0"}],
                        "cursor": "v1.next",
                        "more": True,
                        "expiredEventIds": [],
                        "truncated": False,
                    }
                },
            ],
        )
        self.assertEqual(
            stub.calls[2],
            ("read", [{"after": "v1.start", "types": ["pinged"], "limit": 0}]),
        )

    async def test_notify_binding_wraps_the_granted_stub(self) -> None:
        """``env.NOTIFY`` forwards raises in wire shape and refuses a raise without a type."""
        from bookclerk_plugin_sdk.workerd import NotifyBinding

        class _NotifyStub:
            def __init__(self) -> None:
                self.calls: list[dict] = []

            async def settle(self, method: str, args: list) -> dict:
                assert method == "raise"
                self.calls.append(args[0])
                return {"ok": {"notificationId": "ntf-1"}}

        stub = _NotifyStub()
        seen: list = []

        class Chat(NotifierEntrypoint):
            async def send(self, params: dict) -> None:
                notify = self.env.NOTIFY
                assert isinstance(notify, NotifyBinding)
                seen.append(
                    await notify.raise_("sync_failed", fields={"pages": 3}, link_path="/books/42")
                )
                try:
                    await notify.raise_("")
                except PluginError as err:
                    seen.append(err.code)
                return None

        await Chat().bookclerkInvoke({"notify": stub}, "send", {"message": {}, "destination": {}})
        self.assertEqual(seen, [{"notificationId": "ntf-1"}, "invalid_params"])
        self.assertEqual(
            stub.calls,
            [
                {
                    "notificationType": "sync_failed",
                    "fields": [{"name": "pages", "value": "3"}],
                    "alertAction": "none",
                    "linkPath": "/books/42",
                }
            ],
        )

    async def test_execute_streamed_unsupported_by_default(self) -> None:
        """A session that advertises no streamed budget keeps the default, which is unsupported."""
        session = AdapterDatabaseSession()
        request = {
            "operationId": "op-1",
            "requestHash": "",
            "statements": [],
            "deadlineUnixMs": 0,
            "isolation": "atomicBatch",
        }
        with self.assertRaises(PluginError) as ctx:
            await session.executeStreamed(request)  # type: ignore[arg-type]
        self.assertEqual(ctx.exception.code, "unsupported")
        self.assertIn("executeStreamed", str(ctx.exception))

        class Streaming(AdapterDatabaseSession):
            async def executeStreamed(self, _request: object) -> bytes:
                return b"frames"

        class DatabaseAdapter(DatabaseAdapterEntrypoint):
            async def openSession(self) -> Streaming:
                return Streaming()

        adapter = DatabaseAdapter()
        session_id = await adapter.bookclerkInvoke({"invocation": {"id": "inv-s"}}, "openSession")
        frames = await adapter.bookclerkInvoke(
            {}, "session", session_id, "executeStreamed", request
        )
        self.assertEqual(frames, b"frames")

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


class _Settled:
    """Adapter-side settled transport over a plain binding stub, as the SDK serves it."""

    def __init__(self, inner: object) -> None:
        self.inner = inner

    async def settle(self, method: str, args: list) -> dict:
        try:
            return {"ok": await getattr(self.inner, method)(*args)}
        except PluginError as err:
            return {"err": {"code": err.wire_code, "message": str(err), "details": ""}}


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
            {
                "invocation": {"id": "inv-q"},
                "queues": [{"name": "NOTIFY", "queue": _Settled(stub)}],
            },
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
            {"kv": [{"name": "KV", "kv": _Settled(stub)}]},
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

    async def test_webhooks_binding_pulls_and_acks(self) -> None:
        from bookclerk_plugin_sdk.workerd import WebhooksBinding

        class _WebhooksStub:
            def __init__(self) -> None:
                self.calls: list = []

            async def call(self, op, body):
                self.calls.append((op, body))
                if op == "url":
                    return {"ok": {"url": "https://lib.example.org/hooks/t/events"}}
                if op == "pull":
                    return {
                        "ok": {
                            "deliveries": [
                                {
                                    "leaseId": "l1",
                                    "receivedAtUnixMs": 1_000,
                                    "attempt": 2,
                                    "headers": [{"name": "content-type", "value": "a/b"}],
                                    "body": "e30=",
                                }
                            ],
                            "missed": True,
                        }
                    }
                if op == "ack" and "bad" in body["acks"]:
                    return {"err": {"code": "not_found", "message": "gone"}}
                return {"ok": {}}

        stub = _WebhooksStub()
        seen: list = []

        class Default(BookclerkEntrypoint):
            async def scheduled(self, controller) -> None:
                hooks = self.env.WEBHOOKS
                assert isinstance(hooks, WebhooksBinding)
                seen.append(await hooks.url("events"))
                page = await hooks.pull("events", batch_size=5)
                message = page.messages[0]
                seen.append((message.lease_id, message.attempts, message.headers, message.body))
                seen.append((page.missed, int(message.timestamp.timestamp() * 1000)))
                await hooks.ack("events", acks=["l1"], retries=[])
                for bad in (
                    lambda: hooks.pull("Events"),
                    lambda: hooks.pull("events", batch_size=0),
                    lambda: hooks.ack("events", acks=["x"], retries=["x"]),
                    lambda: hooks.ack("events", acks=["no/slash"]),
                ):
                    try:
                        await bad()
                    except PluginError as err:
                        seen.append(err.code)

            async def webhook_delivered(self, name: str, waiting: int) -> None:
                seen.append(("delivered", name, waiting))

        context = {"webhooks": stub}
        await Default().bookclerkScheduled(context, {"cron": "* * * * *"})
        await Default().bookclerkWebhookDelivered(context, {"name": "events", "waiting": 3})
        self.assertEqual(
            seen,
            [
                "https://lib.example.org/hooks/t/events",
                ("l1", 2, {"content-type": "a/b"}, b"{}"),
                (True, 1_000),
                "invalid_params",
                "invalid_params",
                "invalid_params",
                "invalid_params",
                ("delivered", "events", 3),
            ],
        )
        self.assertIn(
            ("pull", {"name": "events", "batchSize": 5, "visibilityTimeoutMs": 0}), stub.calls
        )
        self.assertIn(("ack", {"name": "events", "acks": ["l1"], "retries": []}), stub.calls)

    async def test_socket_handlers_receive_host_web_sockets(self) -> None:
        from bookclerk_plugin_sdk.workerd import HostWebSocket, SocketsBinding

        class _SocketsStub:
            def __init__(self) -> None:
                self.calls: list = []

            async def call(self, op, body):
                self.calls.append((op, body))
                if op == "connect":
                    return {
                        "ok": {
                            "socket": "g1_a",
                            "url": body["url"],
                            "protocol": "chat",
                            "attachment": body["attachment"],
                        }
                    }
                if op == "list":
                    return {"ok": {"sockets": [{"socket": "g1_a", "url": "wss://abs.example/"}]}}
                if op == "send" and body["frame"] == {"text": "boom"}:
                    return {"err": {"code": "not_found", "message": "socket closed"}}
                return {"ok": {}}

        stub = _SocketsStub()
        seen: list = []

        class Default(BookclerkEntrypoint):
            async def scheduled(self, controller) -> None:
                sockets = self.env.SOCKETS
                assert isinstance(sockets, SocketsBinding)
                ws = await sockets.connect(
                    "wss://abs.example/socket.io/",
                    protocols=["chat"],
                    headers={"Authorization": "Bearer t"},
                    auto_responses=[("2", "3"), (b"\x01", b"\x02")],
                    attachment={"user": "u1"},
                )
                seen.append((ws.id, ws.url, ws.protocol, ws.deserialize_attachment()))
                seen.append([s.id for s in await sockets.get_web_sockets()])

            async def web_socket_message(self, ws: HostWebSocket, message) -> None:
                seen.append(("message", ws.id, message, ws.deserialize_attachment()))
                await ws.send(b"\x00\xff")
                await ws.serialize_attachment({"n": 2})
                seen.append(ws.deserialize_attachment())
                try:
                    await ws.send("boom")
                except PluginError as err:
                    seen.append(err.code)

            async def web_socket_close(self, ws, code, reason, was_clean) -> None:
                seen.append(("close", ws.id, code, reason, was_clean))
                await ws.close(1000, "bye")

        context = {"sockets": [{"name": "SOCKETS", "sockets": stub}]}
        await Default().bookclerkScheduled(context, {"cron": "* * * * *"})
        await Default().bookclerkSocketMessage(
            context,
            {"socket": "g1_a", "frame": {"kind": "text", "value": "42"}, "attachment": b"7"},
        )
        await Default().bookclerkSocketMessage(
            context, {"socket": "g1_a", "frame": {"kind": "binary", "value": b"\x01"}}
        )
        await Default().bookclerkSocketClosed(
            context, {"socket": "g1_a", "code": 1006, "reason": "", "clean": False}
        )
        self.assertEqual(
            seen[:3],
            [
                ("g1_a", "wss://abs.example/socket.io/", "chat", {"user": "u1"}),
                ["g1_a"],
                ("message", "g1_a", "42", 7),
            ],
        )
        self.assertEqual(seen[3:5], [{"n": 2}, "not_found"])
        self.assertEqual(seen[5], ("message", "g1_a", b"\x01", None))
        self.assertEqual(seen[-1], ("close", "g1_a", 1006, "", False))
        connect = stub.calls[0]
        self.assertEqual(connect[0], "connect")
        self.assertEqual(
            connect[1],
            {
                "url": "wss://abs.example/socket.io/",
                "protocols": ["chat"],
                "headers": [{"name": "Authorization", "value": "Bearer t"}],
                "autoResponses": [
                    {"request": {"text": "2"}, "response": {"text": "3"}},
                    {"request": {"binary": "AQ=="}, "response": {"binary": "Ag=="}},
                ],
                "attachment": "eyJ1c2VyIjoidTEifQ==",
            },
        )
        self.assertIn(("send", {"socket": "g1_a", "frame": {"binary": "AP8="}}), stub.calls)
        self.assertIn(("attachment", {"socket": "g1_a", "attachment": "eyJuIjoyfQ=="}), stub.calls)
        self.assertEqual(
            stub.calls[-1], ("close", {"socket": "g1_a", "code": 1000, "reason": "bye"})
        )

    async def test_a_socket_message_can_ask_for_a_poll(self) -> None:
        class Asks(BookclerkEntrypoint):
            async def web_socket_message(self, ws, message):
                if message == "user":
                    return {"poll_events": True}
                if message == "camel":
                    return {"pollEvents": True}
                if message == "truthy":
                    return {"poll_events": 1}
                return None

        wire = lambda text: {"socket": "g1_a", "frame": {"kind": "text", "value": text}}  # noqa: E731
        self.assertEqual(
            await Asks().bookclerkSocketMessage({}, wire("user")), {"pollEvents": True}
        )
        self.assertEqual(
            await Asks().bookclerkSocketMessage({}, wire("camel")), {"pollEvents": True}
        )
        self.assertEqual(
            await Asks().bookclerkSocketMessage({}, wire("truthy")), {"pollEvents": False}
        )
        self.assertEqual(await Asks().bookclerkSocketMessage({}, wire("42")), {"pollEvents": False})

    async def test_socket_dispatch_without_handlers_or_grant(self) -> None:
        class Bare(BookclerkEntrypoint):
            pass

        with self.assertRaises(PluginError) as raised:
            await Bare().bookclerkSocketMessage({}, {"socket": "g1_a"})
        self.assertEqual(raised.exception.code, "unsupported")
        self.assertIsNone(await Bare().bookclerkSocketClosed({}, {"socket": "g1_a"}))

        class Camel(BookclerkEntrypoint):
            async def webSocketMessage(self, ws, message) -> None:  # noqa: N802
                await ws.send(message)

        with self.assertRaises(PluginError) as raised:
            await Camel().bookclerkSocketMessage(
                {}, {"socket": "g1_a", "frame": {"kind": "text", "value": "x"}}
            )
        self.assertEqual(raised.exception.code, "unavailable")

        class _BrokenStub:
            async def call(self, op, body):
                raise RuntimeError("channel closed")

        with self.assertRaises(PluginError) as raised:
            await Camel().bookclerkSocketMessage(
                {"sockets": [{"name": "SOCKETS", "sockets": _BrokenStub()}]},
                {"socket": "g1_a", "frame": {"kind": "text", "value": "x"}},
            )
        self.assertEqual(raised.exception.code, "unavailable")

    def test_socket_attachment_and_frame_limits(self) -> None:
        from bookclerk_plugin_sdk.workerd import (
            MAX_SOCKET_ATTACHMENT_BYTES,
            MAX_SOCKET_FRAME_BYTES,
            decode_socket_attachment,
            encode_socket_attachment,
            socket_message_data,
        )

        self.assertEqual(encode_socket_attachment(None), b"")
        self.assertIsNone(decode_socket_attachment(b""))
        self.assertEqual(decode_socket_attachment(b"not json"), "not json")
        with self.assertRaises(PluginError) as raised:
            encode_socket_attachment("x" * MAX_SOCKET_ATTACHMENT_BYTES)
        self.assertEqual(raised.exception.code, "payload_too_large")
        with self.assertRaises(PluginError) as raised:
            encode_socket_attachment({1, 2})
        self.assertEqual(raised.exception.code, "invalid_params")
        with self.assertRaises(PluginError) as raised:
            workerd_module._route_frame("x" * (MAX_SOCKET_FRAME_BYTES + 1))
        self.assertEqual(raised.exception.code, "payload_too_large")
        with self.assertRaises(PluginError) as raised:
            workerd_module._route_frame(7)  # type: ignore[arg-type]
        self.assertEqual(raised.exception.code, "invalid_params")
        self.assertEqual(socket_message_data(None), "")
        self.assertEqual(socket_message_data({"kind": "binary", "value": [1, 2]}), b"\x01\x02")

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
