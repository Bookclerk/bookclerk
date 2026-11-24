"""Tests the ``stream_copy`` job helper against fake job capabilities.

The commit token matches the Rust and TypeScript SDKs, the copy stages then
commits, and every failure path aborts the stage.
"""

from __future__ import annotations

import json
import unittest
from typing import Any

from bookclerk_plugin_sdk.workerd import (
    STREAM_COPY_COMMAND,
    JobController,
    PluginError,
    stream_copy,
    stream_copy_commit_token,
)

VECTORS = [
    ("job/a:b", "", "out/book.m4b", "sc-d860fba581892fced82f4b4bba5c6d37"),
    ("dedup-stable", "step-2", "library/title.m4b", "sc-a8f88bc8ccfa06a5848f2b02b5d6abb2"),
]
"""Token vectors shared with the Rust (``jobs.rs``) and TypeScript SDK tests."""


class _Input:
    """Fake job input that serves one three-byte object."""

    def __init__(self, calls: list[str]) -> None:
        """Record calls into ``calls``.

        Args:
            calls: The shared call log.
        """
        self.calls = calls

    async def open(self, key: str) -> dict[str, Any]:
        """Open ``key``.

        Args:
            key: The source key.

        Returns:
            Metadata and a body.
        """
        self.calls.append(f"open {key}")
        return {
            "meta": {"key": key, "size": 3, "contentType": "audio/mp4", "sha256": bytes(32)},
            "body": b"\x01\x02\x03",
        }


class _Output:
    """Fake job output that records stage, commit and abort calls."""

    def __init__(self, calls: list[str], job: list[JobController], **fail: bool) -> None:
        """Record calls into ``calls`` and fail where ``fail`` says.

        Args:
            calls: The shared call log.
            job: Holds the controller, so a put can cancel it.
            **fail: ``put``, ``commit`` or ``cancel_during_put``.
        """
        self.calls = calls
        self.job = job
        self.fail = fail

    async def put(self, key: str, body: bytes, options: dict[str, Any]) -> dict[str, Any]:
        """Stage ``body`` at ``key``.

        Args:
            key: The destination key.
            body: The bytes.
            options: The write options.

        Returns:
            The bytes written.

        Raises:
            PluginError: When told to fail.
        """
        self.calls.append(
            f"put {key} stage={options['stageOnly']} token={options['commitToken']}"
            f" len={options.get('contentLength')}"
        )
        if self.fail.get("put"):
            raise PluginError.from_wire("internal", "put failed")
        if self.fail.get("cancel_during_put"):
            self.job[0].cancel()
        return {"key": key, "bytesWritten": len(body)}

    async def commit(self, key: str, token: str) -> dict[str, Any]:
        """Publish the stage.

        Args:
            key: The destination key.
            token: The commit token.

        Returns:
            The published object.

        Raises:
            PluginError: When told to fail.
        """
        self.calls.append(f"commit {key} {token}")
        if self.fail.get("commit"):
            raise PluginError.from_wire("conflict", "commit failed")
        return {"key": key, "bytesWritten": 3}

    async def abortStage(self, key: str, token: str) -> None:  # noqa: N802  (ABI name)
        """Drop the stage.

        Args:
            key: The destination key.
            token: The commit token.
        """
        self.calls.append(f"abort {key} {token}")


class _Progress:
    """Fake progress sink."""

    def __init__(self, calls: list[str]) -> None:
        """Record calls into ``calls``.

        Args:
            calls: The shared call log.
        """
        self.calls = calls

    async def report(self, percent: float, message: str) -> None:
        """Record one report.

        Args:
            percent: Completion percent.
            message: Status text.
        """
        self.calls.append(f"progress {int(percent)} {message}")


def _harness(
    *,
    command: str = STREAM_COPY_COMMAND,
    payload: str | None = None,
    no_input: bool = False,
    **fail: bool,
) -> tuple[JobController, list[str]]:
    """Build a controller over fake capabilities.

    Args:
        command: The job command type.
        payload: The payload text; a copy of ``src/a`` to ``dst/a`` by default.
        no_input: Grant no input.
        **fail: What the output fails at.

    Returns:
        The controller and the call log.
    """
    calls: list[str] = []
    holder: list[JobController] = []
    job = JobController(
        {
            "invocationId": "inv-1",
            "commandType": command,
            "payloadJson": payload
            if payload is not None
            else json.dumps({"from": "src/a", "to": "dst/a"}),
            "idempotencyKey": "job/a:b",
            "stepId": "",
        },
        {
            "input": None if no_input else _Input(calls),
            "output": _Output(calls, holder, **fail),
            "progress": _Progress(calls),
        },
    )
    holder.append(job)
    return job, calls


TOKEN = stream_copy_commit_token("job/a:b", "", "dst/a")


class StreamCopyTest(unittest.IsolatedAsyncioTestCase):
    """``stream_copy`` and its commit token."""

    def test_tokens_match_the_other_sdks(self) -> None:
        """The shared vectors hold and an empty step is the default step."""
        for key, step, dest, want in VECTORS:
            self.assertEqual(stream_copy_commit_token(key, step, dest), want)
        self.assertEqual(
            stream_copy_commit_token("k", "", "d"),
            stream_copy_commit_token("k", "stream_copy", "d"),
        )

    async def test_stages_then_commits(self) -> None:
        """The happy path stages under the token, then commits."""
        job, calls = _harness()
        done = await stream_copy(job)
        self.assertEqual(done, {"message": "copied src/a -> dst/a", "bytesCopied": 3})
        self.assertEqual(
            calls,
            [
                "progress 0 opening source",
                "open src/a",
                "progress 10 staging",
                f"put dst/a stage=True token={TOKEN} len=3",
                "progress 90 committing",
                f"commit dst/a {TOKEN}",
                "progress 100 done",
            ],
        )

    async def _rejects(self, job: JobController, code: str) -> None:
        """Assert the copy fails with ``code``.

        Args:
            job: The controller.
            code: The expected error code.
        """
        with self.assertRaises(PluginError) as ctx:
            await stream_copy(job)
        self.assertEqual(ctx.exception.code, code)

    async def test_refuses_bad_requests_untouched(self) -> None:
        """Another command, a bad payload and a missing input are refused."""
        job, calls = _harness(command="transcode")
        await self._rejects(job, "unsupported")
        self.assertEqual(calls, [])
        for payload in ("[]", "{", '{"from": 1, "to": "x"}'):
            await self._rejects(_harness(payload=payload)[0], "invalid_params")
        await self._rejects(_harness(no_input=True)[0], "unavailable")

    async def test_failures_abort_the_stage(self) -> None:
        """A failed put or commit aborts the stage and passes the error on."""
        job, calls = _harness(put=True)
        await self._rejects(job, "internal")
        self.assertEqual(calls[-1], f"abort dst/a {TOKEN}")
        job, calls = _harness(commit=True)
        await self._rejects(job, "conflict")
        self.assertEqual(calls[-1], f"abort dst/a {TOKEN}")

    async def test_cancellation_aborts_instead_of_committing(self) -> None:
        """Cancelled after the stage, the copy aborts; before it, nothing runs."""
        job, calls = _harness(cancel_during_put=True)
        await self._rejects(job, "cancelled")
        self.assertFalse(any(call.startswith("commit") for call in calls))
        self.assertEqual(calls[-1], f"abort dst/a {TOKEN}")
        job, calls = _harness()
        job.cancel()
        await self._rejects(job, "cancelled")
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
