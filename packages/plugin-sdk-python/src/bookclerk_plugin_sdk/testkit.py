"""Contract conformance test kit: a plugin database binding like the host's.

A plain in-memory fake accepts anything. On a host, ``env.DB`` reaches an
isolated unit through the adapter the operator deployed: SQL is scoped to
the plugin's own tables, every batch is one transaction held to the
adapter's advertised caps, and a result past a cap is a typed
``limit_exceeded`` error. :func:`create_test_database` gives a pytest
suite that same binding, backed by the ``bookclerk-db-testkit`` binary from
the Bookclerk workspace under one of three capability profiles:

- ``first-party``: the shipped sqlite and Postgres adapters (256 KiB
  results, streamed reads).
- ``d1``: the D1 adapter (smaller payload and statement limits).
- ``minimum``: the smallest advertisement a host connects to (4,096-byte
  results, 1,024-byte cells, one row per statement, no streamed reads).

Run plugin database code under all three (:data:`TEST_DATABASE_PROFILES`):
what works on the shipped adapters has to work when an adapter advertises
only the minimums.

.. code-block:: python

    from bookclerk_plugin_sdk.testkit import TEST_DATABASE_PROFILES, create_test_database

    for profile in TEST_DATABASE_PROFILES:
        async with create_test_database(profile=profile, migrations=MIGRATIONS) as db:
            await db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)").bind(1, "hi").run()
            rows = (await db.binding.prepare("SELECT body FROM notes").all()).results

The binary is found through the ``BOOKCLERK_DB_TESTKIT`` environment variable
or the ``binary`` argument, else as ``bookclerk-db-testkit`` on ``PATH``
(``cargo build -p bookclerk-db-testkit`` in the workspace builds it).
``bookclerk-plugin test`` (``cargo plugin -- test <dir>`` in the workspace)
runs a plugin's suite once per profile, setting both that variable and
``BOOKCLERK_DB_TESTKIT_PROFILE``, which is the default ``profile``. This
module runs under CPython, not inside an isolate.
"""

from __future__ import annotations

import asyncio
import http.client
import json
import os
from collections.abc import AsyncIterable
from dataclasses import dataclass, field
from typing import Any, Literal, cast
from urllib.parse import urlsplit

from .abi import DbCapabilities
from .db_value import (
    DatabaseBinding,
    ExecuteReply,
    ExecuteRequest,
    create_database_binding,
    decode_db_capabilities_reply,
    decode_execute_result_reply,
    encode_execute_request,
)
from .limits import limit_exceeded_details, parse_limit_exceeded
from .workerd import PluginError

TestDatabaseProfile = Literal["first-party", "d1", "minimum"]
"""One of :data:`TEST_DATABASE_PROFILES`."""

TEST_DATABASE_PROFILES: tuple[TestDatabaseProfile, ...] = ("first-party", "d1", "minimum")

_STOP_TIMEOUT_SECONDS: float = 5.0
"""How long :meth:`TestDatabase.close` waits for the kit to exit on its own before killing it."""
"""The capability profiles the kit enforces, in the order conformance runs report them."""


def testkit_binary(binary: str | None = None) -> str:
    """Pick the kit binary.

    Args:
        binary: Explicit path, when given.

    Returns:
        ``binary``, else ``BOOKCLERK_DB_TESTKIT`` from the environment, else the
        bare ``bookclerk-db-testkit`` name resolved on ``PATH``.
    """
    if binary:
        return binary
    env = os.environ.get("BOOKCLERK_DB_TESTKIT", "")
    return env if env else "bookclerk-db-testkit"


@dataclass
class _Serving:
    """The kit's announcement line."""

    url: str
    """Loopback origin of the granted routes."""
    token: str
    """Bearer token of the grant."""
    profile: str
    """Profile being enforced."""


@dataclass
class _Response:
    """One granted-route response."""

    status: int
    """HTTP status."""
    body: bytes = field(default=b"")
    """Response body."""


def _request(serving: _Serving, method: str, route: str, body: bytes | None) -> _Response:
    """Perform one blocking HTTP exchange with the served binding.

    Args:
        serving: Where the binding is served.
        method: ``GET`` or ``POST``.
        route: Granted route, such as ``/db/execute``.
        body: Request body, when any.

    Returns:
        The status and body.
    """
    parts = urlsplit(serving.url)
    conn = http.client.HTTPConnection(parts.hostname or "127.0.0.1", parts.port, timeout=60)
    try:
        headers = {"Authorization": f"Bearer {serving.token}"}
        if body is not None:
            headers["Content-Type"] = "application/octet-stream"
        conn.request(method, route, body=body, headers=headers)
        resp = conn.getresponse()
        return _Response(resp.status, resp.read())
    finally:
        conn.close()


def _database_body(resp: _Response, method: str) -> bytes:
    """Map a granted ``/db/*`` response onto its body or the error it implies.

    ``401`` is ``unauthorized``, ``413`` is ``limit_exceeded`` (the canonical
    limit message parsed back into ``details``), ``404`` and ``405`` are
    ``unsupported`` and anything else ``unavailable``, as the isolate SDK
    maps the host's channel.

    Args:
        resp: The route's response.
        method: ``GuestDatabase`` method name for the error text.

    Returns:
        The body of a 2xx response.

    Raises:
        PluginError: For every non-2xx status.
    """
    if resp.status >= 400:
        text = resp.body.decode("utf-8", "replace")
        message = f"database grant: {resp.status} {text}".strip()
        if resp.status == 413:
            limit = parse_limit_exceeded(text)
            if limit is None:
                raise PluginError.from_wire("limit_exceeded", message)
            raise PluginError("limit_exceeded", message, limit_exceeded_details(limit))
        if resp.status in (404, 405):
            raise PluginError.from_wire(
                "unsupported", f"GuestDatabase.{method} is not served by this kit ({message})"
            )
        code = "unauthorized" if resp.status == 401 else "unavailable"
        raise PluginError.from_wire(code, message)
    return resp.body


class TestDatabase:
    """A served test binding and the process behind it.

    Use it as an async context manager, or call :meth:`close` when the test
    ends; the in-memory unit goes with the process.
    """

    def __init__(self, profile: TestDatabaseProfile, process: Any, serving: _Serving) -> None:
        """Wrap a running kit process.

        Args:
            profile: The profile being enforced.
            process: The ``asyncio`` subprocess serving the binding.
            serving: Its announcement.
        """
        self.profile: TestDatabaseProfile = profile
        self._process = process
        self._serving = serving
        self._closed: asyncio.Task[None] | None = None
        self.binding: DatabaseBinding = create_database_binding(
            self._execute, limits=self._limits, execute_streamed=self._execute_streamed
        )
        """The D1-shaped binding, as ``env.<BINDING>`` on a host."""

    async def _post(self, route: str, request: ExecuteRequest, method: str) -> bytes:
        """POST one encoded request and return the body of a 2xx reply."""
        body = encode_execute_request(request)
        try:
            resp = await asyncio.to_thread(_request, self._serving, "POST", route, body)
        except OSError as err:
            raise PluginError.from_wire("unavailable", f"GuestDatabase.{method}: {err}") from err
        return _database_body(resp, method)

    async def _execute(self, request: ExecuteRequest) -> ExecuteReply:
        """``GuestDatabase.execute`` over ``POST /db/execute``."""
        return decode_execute_result_reply(await self._post("/db/execute", request, "execute"))

    async def _execute_streamed(self, request: ExecuteRequest) -> bytes | AsyncIterable[bytes]:
        """``GuestDatabase.executeStreamed`` over ``POST /db/execute-streamed``."""
        return await self._post("/db/execute-streamed", request, "executeStreamed")

    async def _limits(self) -> DbCapabilities:
        """``GuestDatabase.limits`` over ``GET /db/limits``."""
        try:
            resp = await asyncio.to_thread(_request, self._serving, "GET", "/db/limits", None)
        except OSError as err:
            raise PluginError.from_wire("unavailable", f"GuestDatabase.limits: {err}") from err
        return decode_db_capabilities_reply(_database_body(resp, "limits"))

    async def limits(self) -> DbCapabilities:
        """Read the profile's advertisement: the caps every batch is held to.

        Returns:
            The capability advertisement, as ``binding.limits()`` reports it.
        """
        return await self.binding.limits()

    async def close(self) -> None:
        """Stop the kit process; the in-memory unit goes with it."""
        if self._closed is None:
            self._closed = asyncio.ensure_future(self._stop())
        await self._closed

    async def _stop(self) -> None:
        """Close the kit's stdin and wait for it, killing a process that lingers."""
        process = self._process
        if process.stdin is not None:
            process.stdin.close()
        try:
            await asyncio.wait_for(process.wait(), timeout=_STOP_TIMEOUT_SECONDS)
        except TimeoutError:
            process.kill()
            await process.wait()

    async def __aenter__(self) -> TestDatabase:
        """Enter the context: the binding is already served.

        Returns:
            This database.
        """
        return self

    async def __aexit__(self, *_exc: object) -> None:
        """Leave the context, stopping the kit."""
        await self.close()


def default_profile(option: TestDatabaseProfile | None = None) -> TestDatabaseProfile:
    """Pick the profile a test runs under.

    Args:
        option: The caller's ``profile`` argument.

    Returns:
        ``option`` when given, else the profile ``bookclerk-plugin test`` names
        in ``BOOKCLERK_DB_TESTKIT_PROFILE``, else ``minimum``. Not validated.
    """
    if option is not None:
        return option
    from_env = os.environ.get("BOOKCLERK_DB_TESTKIT_PROFILE", "")
    return cast(TestDatabaseProfile, from_env) if from_env else "minimum"


async def create_test_database(
    profile: TestDatabaseProfile | None = None,
    migrations: list[dict[str, Any]] | None = None,
    *,
    binary: str | None = None,
) -> TestDatabase:
    """Spawn the kit under ``profile`` with ``migrations`` applied.

    Args:
        profile: Capability profile to enforce. Defaults to
            ``BOOKCLERK_DB_TESTKIT_PROFILE`` when ``bookclerk-plugin test``
            sets it, else ``minimum``, the strictest.
        migrations: The plugin's ``database_migrations`` registration (each
            ``{"id": ..., "operations": [{"schema": sql} | {"data": sql}, ...]}``),
            applied the way the host applies it at enrollment.
        binary: Path of the ``bookclerk-db-testkit`` binary; see the module
            docs for the default.

    Returns:
        The served binding and its process; close it when the test ends.

    Raises:
        ValueError: When ``profile`` is not one of :data:`TEST_DATABASE_PROFILES`.
        RuntimeError: When the binary cannot be started, refuses the profile
            or the registration (its message is included), or exits early.
    """
    profile = default_profile(profile)
    if profile not in TEST_DATABASE_PROFILES:
        raise ValueError(
            f"unknown test database profile {profile!r} (one of {', '.join(TEST_DATABASE_PROFILES)})"
        )
    try:
        process = await asyncio.create_subprocess_exec(
            testkit_binary(binary),
            "serve",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except OSError as err:
        raise RuntimeError(f"bookclerk-db-testkit could not be started: {err}") from err
    assert process.stdin is not None and process.stdout is not None and process.stderr is not None
    request = json.dumps({"profile": profile, "migrations": migrations or []})
    process.stdin.write(f"{request}\n".encode())
    await process.stdin.drain()
    line = await process.stdout.readline()
    if not line:
        process.stdin.close()
        stderr = (await process.stderr.read()).decode("utf-8", "replace").strip()
        await process.wait()
        suffix = f": {stderr}" if stderr else ""
        raise RuntimeError(f"bookclerk-db-testkit exited before announcing its address{suffix}")
    try:
        announced = json.loads(line)
        serving = _Serving(
            url=str(announced["url"]),
            token=str(announced["token"]),
            profile=str(announced["profile"]),
        )
    except (ValueError, KeyError, TypeError) as err:
        process.stdin.close()
        process.kill()
        await process.wait()
        raise RuntimeError(
            f"bookclerk-db-testkit announced an unexpected line: {line.decode('utf-8', 'replace')!r}"
        ) from err
    return TestDatabase(profile, process, serving)
