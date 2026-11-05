"""Wire envelopes decoded by the workerd author model: every field and default."""

from __future__ import annotations

from types import SimpleNamespace
from typing import Any

import pytest

from bookclerk_plugin_sdk.workerd import (
    EventMessage,
    JobController,
    KvNamespace,
    QueueBinding,
    _EnvView,
    _invocation_env,
    _invocation_of,
    decode_extensible_config,
)

FULL_EVENT: dict[str, Any] = {
    "eventId": "evt-1",
    "eventType": "thing_done",
    "schemaVersion": 3,
    "occurredAtUnixMs": 1_700_000_000_123,
    "deliveryAttempt": 4,
    "accountId": "acct-9",
    "source": "plugin:echo",
    "correlationId": "corr-1",
    "causationId": "cause-1",
    "deduplicationKey": "dedupe-1",
    "payload": b'{"a":1}',
    "invocationSequence": 7,
    "resumePending": True,
    "checkpointJson": '{"step":2}',
    "checkpointSchemaVersion": 5,
}

EVENT_FIELDS: dict[str, Any] = {
    "id": "evt-1",
    "type": "thing_done",
    "schema_version": 3,
    "timestamp_unix_ms": 1_700_000_000_123,
    "attempts": 4,
    "account_id": "acct-9",
    "source": "plugin:echo",
    "correlation_id": "corr-1",
    "causation_id": "cause-1",
    "deduplication_key": "dedupe-1",
    "body": b'{"a":1}',
    "invocation_sequence": 7,
    "resume_pending": True,
    "checkpoint": {"json": '{"step":2}', "schemaVersion": 5},
}

EVENT_DEFAULTS: dict[str, Any] = {
    "id": "",
    "type": "",
    "schema_version": 0,
    "timestamp_unix_ms": 0,
    "attempts": 1,
    "account_id": "",
    "source": "",
    "correlation_id": "",
    "causation_id": "",
    "deduplication_key": "",
    "body": b"",
    "invocation_sequence": 0,
    "resume_pending": False,
    "checkpoint": None,
}


def _fields(obj: Any, names: dict[str, Any]) -> dict[str, Any]:
    return {name: getattr(obj, name) for name in names}


def test_event_message_reads_every_wire_field() -> None:
    msg = EventMessage(FULL_EVENT)
    assert _fields(msg, EVENT_FIELDS) == EVENT_FIELDS
    assert msg.raw is FULL_EVENT
    assert msg.result is None
    assert msg.json() == {"a": 1}


def test_event_message_reads_attributes_of_a_wire_object() -> None:
    msg = EventMessage(SimpleNamespace(**FULL_EVENT))
    assert _fields(msg, EVENT_FIELDS) == EVENT_FIELDS


@pytest.mark.parametrize("event", [None, {}], ids=["none", "empty"])
def test_event_message_defaults(event: Any) -> None:
    msg = EventMessage(event)
    assert _fields(msg, EVENT_DEFAULTS) == EVENT_DEFAULTS
    assert msg.raw == {}
    assert msg.json() == {}


def test_event_message_falsy_numbers_take_the_defaults() -> None:
    msg = EventMessage(
        {
            "schemaVersion": 0,
            "occurredAtUnixMs": 0,
            "deliveryAttempt": 0,
            "invocationSequence": 0,
            "checkpointJson": "{}",
            "checkpointSchemaVersion": 0,
        }
    )
    assert msg.attempts == 1
    assert (msg.schema_version, msg.timestamp_unix_ms, msg.invocation_sequence) == (0, 0, 0)
    assert msg.checkpoint == {"json": "{}", "schemaVersion": 0}


def test_event_message_checkpoint_without_a_version() -> None:
    msg = EventMessage({"checkpointJson": '{"k":1}'})
    assert msg.checkpoint == {"json": '{"k":1}', "schemaVersion": 0}


FULL_INVOCATION: dict[str, Any] = {
    "invocationId": "inv-1",
    "commandType": "sync_library",
    "payloadJson": '{"full":true}',
    "payloadSchemaVersion": 2,
    "idempotencyKey": "idem-1",
    "attempt": 3,
    "deadlineUnixMs": 1_700_000_999_000,
    "correlationId": "corr-2",
    "causationId": "cause-2",
    "invocationSequence": 9,
    "stepId": "step-a",
    "checkpointJson": '{"cursor":"x"}',
    "checkpointSchemaVersion": 4,
}

JOB_FIELDS: dict[str, Any] = {
    "id": "inv-1",
    "type": "sync_library",
    "payload_json": '{"full":true}',
    "payload_schema_version": 2,
    "idempotency_key": "idem-1",
    "attempt": 3,
    "deadline_unix_ms": 1_700_000_999_000,
    "correlation_id": "corr-2",
    "causation_id": "cause-2",
    "invocation_sequence": 9,
    "step_id": "step-a",
    "checkpoint": {"json": '{"cursor":"x"}', "schemaVersion": 4},
}

JOB_DEFAULTS: dict[str, Any] = {
    "id": "",
    "type": "",
    "payload_json": "",
    "payload_schema_version": 0,
    "idempotency_key": "",
    "attempt": 1,
    "deadline_unix_ms": 0,
    "correlation_id": "",
    "causation_id": "",
    "invocation_sequence": 0,
    "step_id": "",
    "checkpoint": None,
}


def test_job_controller_reads_every_wire_field() -> None:
    granted = {"input": object(), "output": object(), "progress": object()}
    job = JobController(FULL_INVOCATION, granted)
    assert _fields(job, JOB_FIELDS) == JOB_FIELDS
    assert job.invocation == FULL_INVOCATION
    assert job.invocation is not FULL_INVOCATION
    assert job.input is granted["input"]
    assert job.output is granted["output"]
    assert job._progress is granted["progress"]
    assert job.cancelled is False
    assert job.result is None
    assert job.json() == {"full": True}


def test_job_controller_reads_attributes_of_wire_objects() -> None:
    granted = SimpleNamespace(input="in", output="out", progress=None)
    job = JobController(SimpleNamespace(**FULL_INVOCATION), granted)
    assert _fields(job, JOB_FIELDS) == JOB_FIELDS
    assert job.invocation == {}
    assert (job.input, job.output, job._progress) == ("in", "out", None)


@pytest.mark.parametrize("invocation", [None, {}], ids=["none", "empty"])
def test_job_controller_defaults(invocation: Any) -> None:
    job = JobController(invocation)
    assert _fields(job, JOB_DEFAULTS) == JOB_DEFAULTS
    assert job.invocation == {}
    assert (job.input, job.output, job._progress) == (None, None, None)
    assert job.json() == {}


def test_job_controller_falsy_numbers_take_the_defaults() -> None:
    job = JobController(
        {
            "attempt": 0,
            "payloadSchemaVersion": 0,
            "deadlineUnixMs": 0,
            "invocationSequence": 0,
            "checkpointJson": "{}",
        }
    )
    assert job.attempt == 1
    assert (job.payload_schema_version, job.deadline_unix_ms, job.invocation_sequence) == (0, 0, 0)
    assert job.checkpoint == {"json": "{}", "schemaVersion": 0}


def test_invocation_of_reads_every_field() -> None:
    context = {
        "invocation": {
            "id": "inv-3",
            "accountId": "acct-3",
            "deadlineUnixMs": 1_700_000_000_500,
            "correlationId": "corr-3",
            "causationId": "cause-3",
        }
    }
    assert _invocation_of(context) == context["invocation"]
    assert (
        _invocation_of(SimpleNamespace(invocation=SimpleNamespace(**context["invocation"])))
        == (context["invocation"])
    )


@pytest.mark.parametrize("context", [None, {}, {"invocation": None}, {"invocation": {}}])
def test_invocation_of_defaults(context: Any) -> None:
    assert _invocation_of(context) == {
        "id": "",
        "accountId": "",
        "deadlineUnixMs": 0,
        "correlationId": "",
        "causationId": "",
    }


def test_invocation_env_grants_every_binding() -> None:
    storage, events, queue, kv, database = object(), object(), object(), object(), object()
    env = _invocation_env(
        SimpleNamespace(STATIC="static"),
        {
            "config": {"payload": {"k": "v"}},
            "secrets": {
                "schemaVersion": 1,
                "mediaType": "application/json",
                "payload": b'{"s":"x"}',
            },
            "storage": storage,
            "events": events,
            "databases": [{"name": "DB", "database": database}],
            "queues": [{"name": "JOBS", "queue": queue}],
            "kv": [{"name": "CACHE", "kv": kv}],
        },
    )
    assert env.CONFIG == {"k": "v"}
    assert env.SECRETS == {"s": "x"}
    assert env.WORK_FS is storage
    assert env.EVENTS is events
    assert env.DB is database
    assert isinstance(env.JOBS, QueueBinding)
    assert env.JOBS._stub is queue
    assert isinstance(env.CACHE, KvNamespace)
    assert env.CACHE._stub is kv
    assert env.STATIC == "static"
    assert env["STATIC"] == "static"
    assert env.get("STATIC") == "static"


def test_invocation_env_without_a_context_grants_nothing() -> None:
    env = _invocation_env(None, None)
    assert env._granted == {}
    for name in ("CONFIG", "SECRETS", "WORK_FS", "EVENTS"):
        assert env.get(name, "absent") == "absent"
        with pytest.raises(KeyError):
            env[name]
        with pytest.raises(AttributeError):
            getattr(env, name)


def test_invocation_env_skips_invalid_entries() -> None:
    queue, kv = object(), object()
    env = _invocation_env(
        None,
        SimpleNamespace(
            databases=[{"name": ""}, {"name": 1, "database": object()}, {"name": "NONE"}],
            queues=[{"name": "", "queue": queue}, {"name": 2, "queue": queue}, {"name": "NOQ"}],
            kv=[{"name": "", "kv": kv}, {"name": 3, "kv": kv}, {"name": "NOKV"}],
        ),
    )
    assert env._granted == {}


def test_invocation_env_layers_over_an_earlier_view() -> None:
    raw = SimpleNamespace(STATIC="static", CONFIG="raw-config")
    first = _invocation_env(raw, {"config": {"payload": {"a": 1}}, "storage": "fs-1"})
    second = _invocation_env(first, {"events": "ev-2", "storage": "fs-2"})
    assert second._raw is raw
    assert second._granted == {"CONFIG": {"a": 1}, "WORK_FS": "fs-2", "EVENTS": "ev-2"}
    assert second.CONFIG == {"a": 1}
    assert second.STATIC == "static"
    assert isinstance(second, _EnvView)


@pytest.mark.parametrize(
    ("config", "expected"),
    [
        (None, {}),
        ("not-a-dict", "not-a-dict"),
        ({"payload": {"k": 1}}, {"k": 1}),
        ({"schemaVersion": 1, "mediaType": "application/json", "payload": b'{"k":1}'}, {"k": 1}),
        ({"schemaVersion": 1, "mediaType": "", "payload": '{"k":2}'}, {"k": 2}),
        ({"schemaVersion": 1, "mediaType": "Application/JSON", "payload": '{"k":3}'}, {"k": 3}),
        ({"schemaVersion": 1, "mediaType": "application/json", "payload": b"  "}, {}),
        ({"schemaVersion": 1, "mediaType": "application/json", "payload": b"{oops"}, {}),
        ({"schemaVersion": 1, "mediaType": "application/json", "payload": b"[1]"}, {}),
        (
            {"schemaVersion": 1, "mediaType": "text/plain", "payload": b"hi"},
            {"mediaType": "text/plain", "text": "hi"},
        ),
        ({"schemaVersion": 1, "payload": {"k": 4}}, {}),
    ],
    ids=[
        "none",
        "not-a-dict",
        "plain-payload",
        "json-bytes",
        "no-media-type",
        "media-type-case",
        "blank",
        "malformed",
        "not-an-object",
        "text",
        "versioned-dict-payload",
    ],
)
def test_decode_extensible_config(config: Any, expected: Any) -> None:
    assert decode_extensible_config(config) == expected
