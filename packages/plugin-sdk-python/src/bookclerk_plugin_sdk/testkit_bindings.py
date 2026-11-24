"""In-memory ``KV`` and ``WEBHOOKS`` bindings for plugin tests.

They hold the limits and error codes the host applies, so code that passes
against them does not first meet a cap on a deployed host: keys are 1 to
``MAX_KV_KEY_BYTES`` UTF-8 bytes, values at most the namespace's
``max_value_bytes``, metadata at most ``MAX_KV_METADATA_BYTES`` of JSON, and
a webhook pull leases 1 to ``MAX_WEBHOOK_PULL_BATCH`` requests for at most
``MAX_WEBHOOK_VISIBILITY_MS``. The TypeScript testkit's ``createTestKv`` and
``createTestWebhooks`` behave the same way.

Example:
    >>> from bookclerk_plugin_sdk.testkit import TestKv, TestWebhooks
    >>> env = {"KV": TestKv(), "WEBHOOKS": TestWebhooks()}
    >>> env["WEBHOOKS"].deliver("changes", b"{}")
"""

from __future__ import annotations

import json
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any, Literal
from urllib.parse import quote

from ._abi import (
    MAX_KV_KEY_BYTES,
    MAX_KV_METADATA_BYTES,
    MAX_KV_VALUE_BYTES,
    MAX_LIST_PAGE,
    MAX_WEBHOOK_PULL_BATCH,
    MAX_WEBHOOK_VISIBILITY_MS,
)
from .workerd import (
    DEFAULT_WEBHOOK_PULL_BATCH,
    PluginError,
    WebhookMessage,
    WebhookPullResult,
)

TestKvOperation = Literal["get", "put", "delete", "list"]
"""Operations a test can make fail once with :meth:`TestKv.fail_next`."""


@dataclass
class _KvEntry:
    """One stored KV entry.

    Attributes:
        value: The value bytes.
        metadata: Metadata as written, or ``None``.
        expires_at_ms: Expiry in milliseconds since the Unix epoch, or
            ``None`` for none.
    """

    value: bytes
    metadata: Any = None
    expires_at_ms: int | None = None


def _check_key(key: str) -> None:
    """Check a key the way the host does.

    Args:
        key: The key.

    Raises:
        PluginError: ``invalid_params`` when empty, ``payload_too_large``
            when longer than ``MAX_KV_KEY_BYTES``.
    """
    if not isinstance(key, str) or not key:
        raise PluginError.from_wire("invalid_params", "kv key must not be empty")
    size = len(key.encode("utf-8"))
    if size > MAX_KV_KEY_BYTES:
        raise PluginError.from_wire(
            "payload_too_large", f"kv key of {size} bytes exceeds {MAX_KV_KEY_BYTES}"
        )


def _is_whole(value: Any) -> bool:
    """Whether ``value`` is a non-negative whole number below 2**53.

    Args:
        value: The value.

    Returns:
        True for a count.
    """
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value < 2**53


class TestKv:
    """A ``KvNamespace`` held in memory that applies the host's limits.

    Attributes:
        max_value_bytes: The namespace's ``max_value_bytes``, never above
            ``MAX_KV_VALUE_BYTES``.
        max_keys: The namespace's key-count limit, or ``None``.
    """

    __test__ = False  # not a pytest test class

    def __init__(
        self,
        *,
        max_value_bytes: int = MAX_KV_VALUE_BYTES,
        max_keys: int | None = None,
        now: Callable[[], int] | None = None,
    ) -> None:
        """Start empty.

        Args:
            max_value_bytes: The namespace's value cap.
            max_keys: The namespace's key-count limit; a new key past it is
                ``forbidden``.
            now: Clock in milliseconds since the Unix epoch.
        """
        self.max_value_bytes = min(max_value_bytes, MAX_KV_VALUE_BYTES)
        self.max_keys = max_keys
        self._now = now or (lambda: int(time.time() * 1000))
        self._entries: dict[str, _KvEntry] = {}
        self._failures: dict[str, PluginError] = {}

    def fail_next(self, op: TestKvOperation, error: PluginError) -> None:
        """Make the next call of ``op`` raise ``error`` instead of running.

        Args:
            op: The operation.
            error: What it raises.
        """
        self._failures[op] = error

    def snapshot(self) -> dict[str, str]:
        """Return live keys and their values as text, for assertions.

        Returns:
            Key to UTF-8 value, in key order.
        """
        return {
            name: entry.value.decode("utf-8", "replace")
            for name in sorted(self._entries)
            if (entry := self._live(name)) is not None
        }

    def _failure(self, op: str) -> None:
        err = self._failures.pop(op, None)
        if err is not None:
            raise err

    def _live(self, key: str) -> _KvEntry | None:
        entry = self._entries.get(key)
        expires = None if entry is None else entry.expires_at_ms
        if expires is not None and expires <= self._now():
            del self._entries[key]
            return None
        return entry

    async def get(self, key: str, type: str = "text") -> Any:
        """Read one key.

        Args:
            key: The key.
            type: ``text``, ``json`` or ``bytes``.

        Returns:
            The value, or ``None`` when absent or expired.
        """
        value, _ = await self.get_with_metadata(key, type)
        return value

    async def get_with_metadata(self, key: str, type: str = "text") -> tuple[Any, Any]:
        """Read one key with its metadata.

        Args:
            key: The key.
            type: ``text``, ``json`` or ``bytes``.

        Returns:
            ``(value, metadata)``; both ``None`` when absent.

        Raises:
            ValueError: For another ``type``.
        """
        if type not in ("text", "json", "bytes"):
            raise ValueError(f"unknown kv value type `{type}`")
        self._failure("get")
        _check_key(key)
        entry = self._live(key)
        if entry is None:
            return None, None
        if type == "bytes":
            return bytes(entry.value), entry.metadata
        text = entry.value.decode("utf-8")
        return (json.loads(text) if type == "json" else text), entry.metadata

    async def put(
        self,
        key: str,
        value: str | bytes,
        *,
        expiration: int | None = None,
        expiration_ttl: int | None = None,
        metadata: Any = None,
    ) -> None:
        """Write one key, replacing any previous value and metadata.

        Args:
            key: The key.
            value: ``str`` (stored as UTF-8) or ``bytes``.
            expiration: Absolute expiry in seconds since the Unix epoch.
            expiration_ttl: Relative expiry in seconds from the write.
            metadata: JSON-serializable metadata.

        Raises:
            PluginError: ``payload_too_large``, ``invalid_params`` or
                ``forbidden`` as the host raises them.
        """
        self._failure("put")
        _check_key(key)
        data = value.encode("utf-8") if isinstance(value, str) else bytes(value)
        if len(data) > self.max_value_bytes:
            raise PluginError.from_wire(
                "payload_too_large", f"kv value of {len(data)} bytes exceeds {self.max_value_bytes}"
            )
        if metadata is not None:
            encoded = len(json.dumps(metadata, separators=(",", ":")).encode("utf-8"))
            if encoded > MAX_KV_METADATA_BYTES:
                raise PluginError.from_wire(
                    "payload_too_large",
                    f"kv metadata of {encoded} bytes exceeds {MAX_KV_METADATA_BYTES}",
                )
        expires: int | None = None
        if expiration is not None:
            if not _is_whole(expiration):
                raise PluginError.from_wire("invalid_params", "expiration is out of range")
            expires = expiration * 1000
        elif expiration_ttl is not None:
            if not _is_whole(expiration_ttl):
                raise PluginError.from_wire("invalid_params", "expiration_ttl is out of range")
            expires = self._now() + expiration_ttl * 1000
        if self._live(key) is None and self.max_keys is not None:
            count = sum(1 for name in list(self._entries) if self._live(name) is not None)
            if count >= self.max_keys:
                raise PluginError.from_wire(
                    "forbidden", f"kv namespace holds at most {self.max_keys} keys"
                )
        self._entries[key] = _KvEntry(data, metadata, expires)

    async def delete(self, key: str) -> None:
        """Delete one key; a missing key is not an error.

        Args:
            key: The key.
        """
        self._failure("delete")
        _check_key(key)
        self._entries.pop(key, None)

    async def list(self, *, prefix: str = "", cursor: str = "", limit: int = 0) -> dict[str, Any]:
        """List keys in lexical order, one page at a time.

        Args:
            prefix: Only keys starting with this prefix.
            cursor: Cursor from a previous page.
            limit: Most keys to return (``MAX_LIST_PAGE`` when 0).

        Returns:
            ``{"keys": [...], "list_complete": bool, "cursor"?: str}``.

        Raises:
            PluginError: ``invalid_params`` for a limit over ``MAX_LIST_PAGE``.
        """
        self._failure("list")
        if not _is_whole(limit) or limit > MAX_LIST_PAGE:
            raise PluginError.from_wire(
                "invalid_params", f"kv list limit must be at most {MAX_LIST_PAGE}"
            )
        names = sorted(
            name
            for name in list(self._entries)
            if name.startswith(prefix) and name > cursor and self._live(name) is not None
        )
        page = names[: limit or MAX_LIST_PAGE]
        keys = []
        for name in page:
            entry = self._entries[name]
            key: dict[str, Any] = {"name": name}
            if entry.expires_at_ms is not None:
                key["expiration"] = entry.expires_at_ms // 1000
            if entry.metadata is not None:
                key["metadata"] = entry.metadata
            keys.append(key)
        out: dict[str, Any] = {"keys": keys, "list_complete": len(page) == len(names)}
        if not out["list_complete"]:
            out["cursor"] = page[-1]
        return out


@dataclass
class _Request:
    """One waiting or leased webhook request.

    Attributes:
        headers: Request headers.
        body: Request body.
        timestamp: When it arrived.
        attempts: How many pulls have leased it.
    """

    headers: dict[str, str]
    body: bytes
    timestamp: datetime
    attempts: int = 0


@dataclass
class _Row:
    """One webhook row's queue and logs.

    Attributes:
        waiting: Requests not leased, oldest first.
        acked: Acknowledged lease ids, in order.
        retried: Released lease ids, in order.
        missed: Whether the next pull reports ``missed``.
    """

    waiting: list[_Request] = field(default_factory=list)
    acked: list[str] = field(default_factory=list)
    retried: list[str] = field(default_factory=list)
    missed: bool = False


class TestWebhooks:
    """A ``WebhooksBinding`` held in memory that applies the host's limits.

    Leased requests stay leased until acknowledged or retried; lease expiry
    is not modelled, so release a lease with ``ack(name, retries=[...])``.
    """

    __test__ = False  # not a pytest test class

    def __init__(self, *, base_url: str | None = "https://library.example/hooks/") -> None:
        """Start with nothing waiting.

        Args:
            base_url: What ``url(name)`` starts with, or ``None`` for a
                library the Internet cannot reach.
        """
        self._base_url = base_url
        self._rows: dict[str, _Row] = {}
        self._leases: dict[str, tuple[str, _Request]] = {}
        self._next_lease = 0

    def _row(self, name: str) -> _Row:
        return self._rows.setdefault(name, _Row())

    def deliver(self, name: str, body: str | bytes, headers: dict[str, str] | None = None) -> None:
        """Queue one inbound request, as if the outside service sent it.

        Args:
            name: ``[[webhooks]]`` name.
            body: Body text or bytes.
            headers: Request headers.
        """
        data = body.encode("utf-8") if isinstance(body, str) else bytes(body)
        self._row(name).waiting.append(_Request(dict(headers or {}), data, datetime.now(UTC)))

    def mark_missed(self, name: str) -> None:
        """Make the next pull of ``name`` report ``missed``.

        Args:
            name: ``[[webhooks]]`` name.
        """
        self._row(name).missed = True

    def waiting(self, name: str) -> int:
        """Return how many requests wait (not leased) for ``name``.

        Args:
            name: ``[[webhooks]]`` name.

        Returns:
            The count.
        """
        return len(self._row(name).waiting)

    def acked(self, name: str) -> list[str]:
        """Return lease ids acknowledged for ``name``, in order.

        Args:
            name: ``[[webhooks]]`` name.

        Returns:
            The ids.
        """
        return list(self._row(name).acked)

    def retried(self, name: str) -> list[str]:
        """Return lease ids released for retry for ``name``, in order.

        Args:
            name: ``[[webhooks]]`` name.

        Returns:
            The ids.
        """
        return list(self._row(name).retried)

    async def url(self, name: str) -> str | None:
        """Return the URL an outside service should call for ``name``.

        Args:
            name: ``[[webhooks]]`` name.

        Returns:
            The URL, or ``None`` for an unreachable library.
        """
        return None if self._base_url is None else f"{self._base_url}{quote(name, safe='')}"

    async def pull(
        self,
        name: str,
        *,
        batch_size: int = DEFAULT_WEBHOOK_PULL_BATCH,
        visibility_timeout_ms: int = 0,
    ) -> WebhookPullResult:
        """Lease up to ``batch_size`` waiting requests, oldest first.

        Args:
            name: ``[[webhooks]]`` name.
            batch_size: 1 to ``MAX_WEBHOOK_PULL_BATCH``.
            visibility_timeout_ms: 0 to ``MAX_WEBHOOK_VISIBILITY_MS``.

        Returns:
            The leased requests and the missed flag.

        Raises:
            PluginError: ``invalid_params`` for an out-of-range option.
        """
        if not _is_whole(batch_size) or not 1 <= batch_size <= MAX_WEBHOOK_PULL_BATCH:
            raise PluginError.from_wire(
                "invalid_params", f"batch_size must be 1 to {MAX_WEBHOOK_PULL_BATCH}"
            )
        if (
            not _is_whole(visibility_timeout_ms)
            or visibility_timeout_ms > MAX_WEBHOOK_VISIBILITY_MS
        ):
            raise PluginError.from_wire(
                "invalid_params",
                f"visibility_timeout_ms must be 0 to {MAX_WEBHOOK_VISIBILITY_MS}",
            )
        row = self._row(name)
        taken, row.waiting = row.waiting[:batch_size], row.waiting[batch_size:]
        messages = []
        for request in taken:
            self._next_lease += 1
            lease_id = f"lease-{self._next_lease}"
            request.attempts += 1
            self._leases[lease_id] = (name, request)
            messages.append(
                WebhookMessage(
                    lease_id=lease_id,
                    timestamp=request.timestamp,
                    attempts=request.attempts,
                    headers=dict(request.headers),
                    body=request.body,
                )
            )
        missed, row.missed = row.missed, False
        return WebhookPullResult(messages=messages, missed=missed)

    async def ack(
        self, name: str, *, acks: list[str] | None = None, retries: list[str] | None = None
    ) -> None:
        """Finish leased requests: ``acks`` are deleted, ``retries`` offered again first.

        Args:
            name: ``[[webhooks]]`` name.
            acks: Lease ids handled.
            retries: Lease ids to offer again.
        """
        row = self._row(name)
        for lease_id in acks or []:
            lease = self._leases.get(lease_id)
            if lease is not None and lease[0] == name:
                del self._leases[lease_id]
                row.acked.append(lease_id)
        back = []
        for lease_id in retries or []:
            lease = self._leases.get(lease_id)
            if lease is not None and lease[0] == name:
                del self._leases[lease_id]
                back.append(lease[1])
                row.retried.append(lease_id)
        row.waiting[:0] = back


__all__ = ["TestKv", "TestKvOperation", "TestWebhooks"]
