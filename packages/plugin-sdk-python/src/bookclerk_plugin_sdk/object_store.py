"""Object-store destination helpers and the queue change feed.

The same code as the TypeScript SDK's ``object-store.ts`` and
``queue-feed.ts`` and the Rust storage helpers, so a ``storage`` plugin over a
bucket-style backend behaves the same in every language. A plugin implements
:class:`ObjectStore` (head, list, get, put, copy and delete against its
service) and hands it to :class:`ObjectStoreDestination`, which serves the
whole ``storage`` entrypoint: staged writes under a commit token with an
idempotent commit, conditional writes, moves and batch calls built from the
backend's calls, object-store key translation, and ``changes`` from a
:class:`QueueFeed` when the plugin has one. Inside a workerd isolate, import
it as ``from bookclerk_plugin_sdk.object_store import ObjectStoreDestination``.

Bodies are opaque here: whatever the backend's ``get`` returns is what its
``put`` takes (a JS ``ReadableStream`` inside the isolate).
"""

from __future__ import annotations

import asyncio
import inspect
import json
import re
import uuid
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any, Literal, Protocol

from ._abi import (
    MAX_BATCH_KEYS,
    MAX_LIST_PAGE,
    MAX_WEBHOOK_PULL_BATCH,
    MAX_WEBHOOK_VISIBILITY_MS,
)
from .workerd import PluginError

__all__ = [
    "MAX_HOST_KEY_BYTES",
    "MAX_OBJECT_KEY_BYTES",
    "NO_FEED_CURSOR",
    "QUEUE_LEASE_MS",
    "STAGE_FOLDER",
    "ChangeFeed",
    "LeasedMessage",
    "NotificationQueue",
    "ObjectStore",
    "ObjectStoreDestination",
    "ParsedChange",
    "QueueFeed",
    "Received",
    "WriteCondition",
    "check_byte_range",
    "clamp_page_limit",
    "order_changes",
    "parse_host_key",
    "parse_queue_cursor",
    "require_disjoint_prefixes",
    "stage_key",
    "translate_for_object_store",
    "webhook_queue",
    "write_condition",
]

NO_FEED_CURSOR = "none"
"""Cursor a destination with no feed (or a feed that is off) hands out."""

STAGE_FOLDER = ".bookclerk-stage"
"""Folder under which staged writes wait for their commit."""

MAX_HOST_KEY_BYTES = 1024
"""Longest host key, in UTF-8 bytes."""

MAX_OBJECT_KEY_BYTES = 1024
"""Longest object-store key, in UTF-8 bytes (prefix included)."""

_MAX_KEPT_EXTENSION_BYTES = 16
"""Longest extension key translation keeps when it shortens a name, in bytes."""


# ---------------------------------------------------------------------------
# Backend contract
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class WriteCondition:
    """A write's precondition.

    Attributes:
        kind: ``ifMatch`` (the stored etag must equal ``etag``) or
            ``ifNoneMatch`` (no object may exist).
        etag: The expected entity tag, for ``ifMatch``.
    """

    kind: Literal["ifMatch", "ifNoneMatch"]
    etag: str = ""


class ObjectStore(Protocol):
    """The calls a bucket-style backend makes.

    Keys are the host's object keys; the backend adds its own prefix. Objects
    are dicts shaped like ``ObjectMetadata`` (``key``, ``size``, optional
    ``contentType``, ``etag``, ``sha256``, ``modifiedAtMs``, ``versionId``,
    ``signature``) plus an optional ``commitToken``. Every failure is a
    :class:`PluginError`.

    Attributes:
        server_copy: Whether :meth:`copy` runs on the server.
        conditional_writes: ``atomic``, ``checked``, or ``None`` when the
            backend has no :meth:`put_conditional`.
        prefix_bytes: UTF-8 bytes of the backend's own key prefix.
    """

    server_copy: bool
    conditional_writes: Literal["atomic", "checked"] | None
    prefix_bytes: int

    async def head(self, key: str) -> dict[str, Any] | None:
        """Return the object at ``key``, or ``None``."""
        ...

    async def list(self, prefix: str, cursor: str | None, limit: int) -> dict[str, Any]:
        """Return one page ``{"objects": [...], "nextCursor"?: str}``."""
        ...

    async def get(self, key: str, range: dict[str, int] | None) -> tuple[dict[str, Any], Any]:
        """Return ``(object, body)``; ``not_found`` when missing."""
        ...

    async def put(self, key: str, body: Any, write: dict[str, Any]) -> dict[str, Any]:
        """Store ``body``; return ``{"bytesWritten", "etag"?, "sha256"?}``."""
        ...

    async def copy(self, source: str, target: str) -> int:
        """Copy ``source`` to ``target``; return the bytes copied."""
        ...

    async def delete(self, key: str) -> None:
        """Remove ``key``; a missing object is not an error."""
        ...

    async def put_conditional(
        self, key: str, body: Any, write: dict[str, Any], condition: WriteCondition
    ) -> dict[str, Any]:
        """Store ``body`` only while ``condition`` holds; ``conflict`` otherwise."""
        ...


class ChangeFeed(Protocol):
    """A change feed a destination serves ``changes`` from."""

    async def changes(self, cursor: str | None, limit: int) -> dict[str, Any] | None:
        """Return one page after ``cursor``, or ``None`` when the feed is off."""
        ...


# ---------------------------------------------------------------------------
# Checks shared by every backend
# ---------------------------------------------------------------------------


def _utf8_len(text: str) -> int:
    return len(text.encode("utf-8"))


def _is_control(char: str) -> bool:
    code = ord(char)
    return code < 0x20 or 0x7F <= code <= 0x9F


def _invalid(message: str) -> PluginError:
    return PluginError.from_wire("invalid_params", message)


def parse_host_key(key: str) -> str:
    """Check a host key.

    1 to :data:`MAX_HOST_KEY_BYTES` bytes of ``/``-separated segments, none
    empty, ``.`` or ``..``, and none holding a backslash or a control
    character.

    Args:
        key: The host key.

    Returns:
        The key.

    Raises:
        PluginError: ``invalid_params`` when it is not one.
    """
    if not key:
        raise _invalid("empty host key")
    size = _utf8_len(key)
    if size > MAX_HOST_KEY_BYTES:
        raise _invalid(f"host key is {size} bytes, more than {MAX_HOST_KEY_BYTES}")
    for segment in key.split("/"):
        if segment in ("", ".", ".."):
            raise _invalid(f"host key segment {json.dumps(segment)} is empty or a dot segment")
        if any(char == "\\" or _is_control(char) for char in segment):
            raise _invalid(
                f"host key segment {json.dumps(segment)} holds a backslash or a control character"
            )
    return key


_OBJECT_STORE_AVOIDED = {
    "{": "_",
    "}": "_",
    "^": "_",
    "%": "_",
    "`": "_",
    '"': "'",
    "<": "(",
    ">": ")",
    "[": "(",
    "]": ")",
    "#": "_",
    "|": "_",
    "~": "_",
}
"""Characters object stores and their tools handle badly, and their stand-ins."""


def _kept_extension(segment: str) -> str | None:
    dot = segment.rfind(".")
    if dot <= 0:
        return None
    ext = segment[dot:]
    return ext if _utf8_len(ext) <= _MAX_KEPT_EXTENSION_BYTES else None


def _cut(text: str, limit: int) -> str:
    used = 0
    out = []
    for char in text:
        used += _utf8_len(char)
        if used > limit:
            break
        out.append(char)
    trimmed = "".join(out).rstrip(". ")
    if trimmed:
        return trimmed
    return text[:1]


def _shorten_keeping_extension(segment: str, limit: int) -> str:
    if _utf8_len(segment) <= limit:
        return segment
    ext = _kept_extension(segment)
    if ext is not None and _utf8_len(ext) < limit:
        stem = segment[: len(segment) - len(ext)]
        return _cut(stem, limit - _utf8_len(ext)) + ext
    return _cut(segment, limit)


def _fit_total(segments: list[str], budget: int) -> None:
    def total() -> int:
        return sum(_utf8_len(s) for s in segments) + max(len(segments) - 1, 0)

    last_at = len(segments) - 1
    over = max(total() - budget, 0)
    if over == 0:
        return
    last = segments[last_at]
    ext = _kept_extension(last)
    floor = 1 if ext is None else _utf8_len(ext) + 1
    segments[last_at] = _shorten_keeping_extension(last, max(_utf8_len(last) - over, floor))
    while total() > budget:
        index, longest = -1, 0
        for i in range(last_at):
            length = _utf8_len(segments[i])
            # Ties go to the later folder, as Rust's `max_by_key` does.
            if length >= longest:
                index, longest = i, length
        if index < 0 or longest <= 1:
            break
        over = total() - budget
        segments[index] = _cut(segments[index], max(longest - over, 1))
    over = max(total() - budget, 0)
    if over > 0:
        final = segments[last_at]
        segments[last_at] = _shorten_keeping_extension(final, max(_utf8_len(final) - over, 1))


def translate_for_object_store(key: str, prefix_bytes: int) -> str:
    """Translate a checked host key into a key an object store holds well.

    Awkward characters are replaced, and the whole key is shortened to fit
    :data:`MAX_OBJECT_KEY_BYTES` after the backend's own prefix.

    Args:
        key: A key :func:`parse_host_key` accepted.
        prefix_bytes: UTF-8 bytes of the backend's prefix.

    Returns:
        The object-store key.
    """
    segments = [
        "".join(_OBJECT_STORE_AVOIDED.get(char, char) for char in segment)
        for segment in key.split("/")
    ]
    _fit_total(segments, max(MAX_OBJECT_KEY_BYTES - prefix_bytes, 0))
    return "/".join(segments)


def _is_count(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and 0 <= value < 2**53


def check_byte_range(
    byte_range: dict[str, int] | None, size: int | None = None
) -> dict[str, int] | None:
    """Check a read range ``{"offset", "length"?}``.

    A length of 0 is ambiguous (leave it out to read to the end); with
    ``size`` the range must also lie within the object.

    Args:
        byte_range: The requested range, or ``None``.
        size: The object's size, when known.

    Returns:
        The range.

    Raises:
        PluginError: ``invalid_params`` for a range that cannot be read.
    """
    if byte_range is None:
        return None
    offset = byte_range.get("offset")
    length = byte_range.get("length")
    if not _is_count(offset):
        raise _invalid("byte range offset is not a whole number")
    assert isinstance(offset, int)
    if length is not None:
        if length == 0:
            raise _invalid("byte range length 0 is ambiguous; omit length to read through EOF")
        if not _is_count(length) or not _is_count(offset + length):
            raise _invalid("byte range offset + length overflows")
        if size is not None and offset + length > size:
            raise _invalid(f"byte range {offset + length} extends past object size {size}")
    if size is not None and offset > size:
        raise _invalid(f"byte range offset {offset} is past object size {size}")
    return byte_range


def clamp_page_limit(limit: int | None) -> int:
    """Clamp a page size to ``1..=MAX_LIST_PAGE`` (0 or ``None`` asks for the most).

    Args:
        limit: The requested size.

    Returns:
        The size to use.
    """
    requested = MAX_LIST_PAGE if not limit else int(limit)
    return min(max(requested, 1), MAX_LIST_PAGE)


def stage_key(token: str | None, key: str) -> str:
    """Return the key a staged write waits under: ``.bookclerk-stage/<token>/<key>``.

    Args:
        token: The commit token.
        key: The object key.

    Returns:
        The stage key.

    Raises:
        PluginError: ``invalid_params`` for an empty token, one holding a path
            separator, ``..`` or NUL, or an empty key.
    """
    t = token or ""
    if not t or "/" in t or "\\" in t or ".." in t or "\0" in t:
        raise _invalid("commit_token must be a non-empty identifier without path separators")
    if not key:
        raise _invalid("object key required")
    return f"{STAGE_FOLDER}/{t}/{key}"


def write_condition(options: dict[str, Any]) -> WriteCondition | None:
    """Return the precondition write ``options`` ask for.

    Args:
        options: ``WriteOptions`` as a dict.

    Returns:
        The condition, or ``None`` for a plain write.

    Raises:
        PluginError: ``invalid_params`` for both ``ifMatch`` and
            ``ifNoneMatch``, or a conditional write that is also staged.
    """
    if_match = options.get("ifMatch")
    if if_match is not None and options.get("ifNoneMatch") is True:
        raise _invalid("a write takes ifMatch or ifNoneMatch, not both")
    condition: WriteCondition | None = None
    if if_match is not None:
        condition = WriteCondition("ifMatch", str(if_match))
    elif options.get("ifNoneMatch") is True:
        condition = WriteCondition("ifNoneMatch")
    if condition is not None and options.get("stageOnly") is True:
        raise _invalid("a conditional write cannot be staged")
    return condition


def _require_batch(length: int, op: str) -> None:
    if length > MAX_BATCH_KEYS:
        raise _invalid(f"{op} takes at most {MAX_BATCH_KEYS} keys")


def _normalized_prefix(prefix: str) -> str:
    parts = [part for part in re.split(r"[/\\]", prefix) if part not in ("", ".")]
    out = "/".join(parts)
    if prefix.endswith(("/", "\\")) and out:
        out += "/"
    return out


def require_disjoint_prefixes(source: str, target: str) -> None:
    """Refuse a prefix move whose source is empty or whose prefixes overlap.

    Args:
        source: Source prefix.
        target: Target prefix.

    Raises:
        PluginError: ``invalid_params`` when they cannot be moved apart.
    """
    a = _normalized_prefix(source)
    b = _normalized_prefix(target)
    if not a:
        raise _invalid("a prefix move needs a non-empty source prefix")
    if b.startswith(a) or a.startswith(b):
        raise _invalid(f"target prefix {target} overlaps source prefix {source}")


def _outcome_error(err: BaseException) -> dict[str, str]:
    if isinstance(err, PluginError):
        return {"code": err.code, "message": str(err)}
    return {"code": "internal", "message": str(err) or err.__class__.__name__}


def _same_object(before: dict[str, Any], after: dict[str, Any]) -> bool:
    a, b = before.get("etag"), after.get("etag")
    if a is not None and b is not None:
        return bool(a == b)
    modified = before.get("modifiedAtMs")
    return (
        before.get("size") == after.get("size")
        and modified is not None
        and modified == after.get("modifiedAtMs")
    )


def _metadata(stored: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in stored.items() if k != "commitToken" and v is not None}


async def _cancel(body: Any) -> None:
    cancel = getattr(body, "cancel", None)
    if callable(cancel):
        result = cancel()
        if inspect.isawaitable(result):
            await result


# ---------------------------------------------------------------------------
# The destination
# ---------------------------------------------------------------------------


class ObjectStoreDestination:
    """Serves the ``storage`` entrypoint over an :class:`ObjectStore`.

    Attributes:
        store: The backend.
        feed: The change feed, or ``None`` (the host lists).
    """

    def __init__(self, store: Any, feed: Any = None) -> None:
        """Wrap a backend.

        Args:
            store: The backend (an :class:`ObjectStore`).
            feed: The change feed (a :class:`ChangeFeed`), or ``None``.
        """
        self.store = store
        self.feed = feed

    @staticmethod
    def features(store: Any, *, changes: bool = False) -> list[str]:
        """Return the ``rpcFeatures`` the destination supports natively.

        Args:
            store: The backend (only ``conditional_writes`` is read).
            changes: Whether the plugin serves a change feed.

        Returns:
            Feature names.
        """
        out = ["storage.copy", "storage.translateKeys"]
        kind = getattr(store, "conditional_writes", None)
        if kind == "atomic":
            out.append("storage.conditionalWrite")
        if kind == "checked":
            out.append("storage.checkedWrite")
        if changes:
            out.append("storage.changes")
        return out

    async def head(self, key: str) -> dict[str, Any] | None:
        """Return metadata without a body, or ``None`` when missing.

        Args:
            key: Object key.

        Returns:
            The metadata.
        """
        found = await self.store.head(key)
        return None if found is None else _metadata(found)

    async def list(self, options: dict[str, Any]) -> dict[str, Any]:
        """Return one page of keys (listing hints not applied).

        Args:
            options: ``ListOptions`` as a dict.

        Returns:
            The page.
        """
        page = await self.store.list(
            options.get("prefix") or "",
            options.get("cursor"),
            clamp_page_limit(options.get("limit")),
        )
        out: dict[str, Any] = {
            "objects": page.get("objects", []),
            "signaturesIncluded": False,
            "modifiedSinceApplied": False,
        }
        if page.get("nextCursor") is not None:
            out["nextCursor"] = page["nextCursor"]
        return out

    async def get(self, key: str, options: dict[str, Any] | None = None) -> dict[str, Any]:
        """Stream an object.

        Args:
            key: Object key.
            options: Optional ``{"range": ...}``.

        Returns:
            ``{"meta", "body"}``.
        """
        byte_range = check_byte_range((options or {}).get("range"))
        stored, body = await self.store.get(key, byte_range)
        return {"meta": _metadata(stored), "body": body}

    async def put(
        self, key: str, body: Any, options: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """Write an object: plain, conditional, or staged under a commit token.

        Args:
            key: Object key.
            body: The bytes.
            options: ``WriteOptions`` as a dict.

        Returns:
            What was written; the etag is the commit token when one is set.

        Raises:
            PluginError: ``internal`` for a SHA-256 that is not 32 bytes.
        """
        opts = options or {}
        condition = write_condition(opts)
        target = stage_key(opts.get("commitToken"), key) if opts.get("stageOnly") else key
        sha = opts.get("sha256")
        if sha is not None and len(sha) not in (0, 32):
            raise PluginError.from_wire(
                "internal", f"integrity: sha256 must be 32 bytes, got {len(sha)}"
            )
        write: dict[str, Any] = {}
        for name in ("contentType", "contentLength", "commitToken"):
            if opts.get(name) is not None:
                write[name] = opts[name]
        if sha is not None and len(sha) == 32:
            write["sha256"] = sha
        if condition is None:
            written = await self.store.put(target, body, write)
        else:
            written = await self._put_conditional(target, body, write, condition)
        out: dict[str, Any] = {"key": key, "bytesWritten": written["bytesWritten"]}
        etag = opts.get("commitToken") or written.get("etag")
        if etag is not None:
            out["etag"] = etag
        if written.get("sha256") is not None:
            out["sha256"] = written["sha256"]
        return out

    async def _put_conditional(
        self, key: str, body: Any, write: dict[str, Any], condition: WriteCondition
    ) -> dict[str, Any]:
        if getattr(self.store, "conditional_writes", None) is not None:
            result: dict[str, Any] = await self.store.put_conditional(key, body, write, condition)
            return result
        current = await self.store.head(key)
        if condition.kind == "ifNoneMatch" and current is not None:
            await _cancel(body)
            raise PluginError.from_wire("conflict", f"{key} already exists")
        if condition.kind == "ifMatch" and (current or {}).get("etag") != condition.etag:
            await _cancel(body)
            raise PluginError.from_wire("conflict", f"{key} changed since it was read")
        result = await self.store.put(key, body, write)
        return result

    async def copy(self, source: str, target: str) -> dict[str, int]:
        """Copy within the destination.

        Args:
            source: Source key.
            target: Target key.

        Returns:
            ``{"bytesCopied"}``.
        """
        return {"bytesCopied": int(await self.store.copy(source, target))}

    async def delete(self, key: str) -> None:
        """Remove an object.

        Args:
            key: Object key.
        """
        await self.store.delete(key)

    async def commit(self, key: str, commit_token: str) -> dict[str, Any]:
        """Publish a staged write; again after the stage is gone, check the token.

        Args:
            key: Object key.
            commit_token: The stage's token.

        Returns:
            The published object.

        Raises:
            PluginError: ``not_found`` when neither the stage nor a commit of
                this token exists.
        """
        staged = stage_key(commit_token, key)
        stage = await self.store.head(staged)
        if stage is not None:
            await self.store.copy(staged, key)
            try:
                await self.store.delete(staged)
            except Exception:  # noqa: BLE001  (a leftover stage costs space only)
                pass
            published = stage
        else:
            current = await self.store.head(key)
            if current is None:
                raise PluginError.from_wire("not_found", key)
            if current.get("commitToken") != commit_token:
                raise PluginError.from_wire(
                    "not_found", f"staged object missing and `{key}` is not commit {commit_token}"
                )
            published = current
        out: dict[str, Any] = {"key": key, "bytesWritten": published["size"], "etag": commit_token}
        if published.get("sha256") is not None:
            out["sha256"] = published["sha256"]
        return out

    async def abort_stage(self, key: str, commit_token: str) -> None:
        """Discard a staged write; a missing stage is not an error.

        Args:
            key: Object key.
            commit_token: The stage's token.
        """
        try:
            await self.store.delete(stage_key(commit_token, key))
        except PluginError as err:
            if err.code != "not_found":
                raise

    async def _copy_streamed(
        self, source: dict[str, Any], target: str, condition: WriteCondition | None
    ) -> None:
        stored, body = await self.store.get(source["key"], None)
        write: dict[str, Any] = {"contentLength": stored["size"]}
        if stored.get("contentType") is not None:
            write["contentType"] = stored["contentType"]
        if stored.get("sha256") is not None:
            write["sha256"] = stored["sha256"]
        if condition is None:
            await self.store.put(target, body, write)
        else:
            await self._put_conditional(target, body, write, condition)

    async def move(
        self, source_key: str, target: str, options: dict[str, Any] | None = None
    ) -> dict[str, bool]:
        """Move one object: copy, check it landed whole, then remove the unchanged source.

        Args:
            source_key: Source key.
            target: Target key.
            options: ``MoveOptions`` as a dict.

        Returns:
            ``{"atomic", "alreadyMoved"}``.

        Raises:
            PluginError: ``not_found``, ``conflict``, or ``internal`` when the
                copy does not match.
        """
        opts = options or {}
        source = await self.store.head(source_key)
        if source is None:
            if source_key != target and await self.store.head(target) is not None:
                return {"atomic": False, "alreadyMoved": True}
            raise PluginError.from_wire("not_found", source_key)
        expected = opts.get("ifSourceMatch")
        if expected is not None and source.get("etag") != expected:
            raise PluginError.from_wire("conflict", f"{source_key} changed since it was read")
        if source_key == target:
            return {"atomic": True, "alreadyMoved": False}
        no_overwrite = opts.get("noOverwrite") is True
        if no_overwrite and getattr(self.store, "conditional_writes", None) is not None:
            await self._copy_streamed(source, target, WriteCondition("ifNoneMatch"))
        else:
            if no_overwrite and await self.store.head(target) is not None:
                raise PluginError.from_wire("conflict", f"{target} already exists")
            if self.store.server_copy:
                await self.store.copy(source_key, target)
            else:
                await self._copy_streamed(source, target, None)
        copied = await self.store.head(target)
        if copied is None or copied.get("size") != source.get("size"):
            raise PluginError.from_wire(
                "internal",
                f"integrity: {target} does not match {source_key} after the copy; the source was kept",
            )
        now = await self.store.head(source_key)
        if now is not None:
            if not _same_object(source, now):
                raise PluginError.from_wire(
                    "conflict",
                    f"{source_key} changed while it was copied to {target}; the source was kept",
                )
            await self.store.delete(source_key)
        return {"atomic": False, "alreadyMoved": False}

    async def move_many(self, request: dict[str, Any]) -> dict[str, Any]:
        """Move pairs, or one page of objects under a prefix.

        Args:
            request: ``MoveManyRequest`` as a dict.

        Returns:
            Per-key outcomes and the continuation.

        Raises:
            PluginError: ``invalid_params`` for too many pairs or overlapping
                prefixes.
        """
        options: dict[str, Any] = {}
        no_overwrite = (request.get("options") or {}).get("noOverwrite")
        if no_overwrite is not None:
            options["noOverwrite"] = no_overwrite
        target = request.get("target") or {}
        if target.get("kind") == "pairs":
            pairs = target.get("value") or []
            if len(pairs) > MAX_BATCH_KEYS:
                raise _invalid(f"moveMany takes at most {MAX_BATCH_KEYS} pairs")
            return await self._move_pairs(pairs, options)
        prefix = target.get("value") or {}
        source, dest = prefix.get("from", ""), prefix.get("to", "")
        require_disjoint_prefixes(source, dest)
        limit = min(max(int(request.get("limit") or 1), 1), MAX_BATCH_KEYS)
        page = await self.store.list(source, request.get("cursor"), limit)
        pairs = [
            {"from": obj["key"], "to": dest + obj["key"][len(source) :]}
            for obj in page.get("objects", [])
            if obj["key"].startswith(source)
        ]
        moved = await self._move_pairs(pairs, options)
        out: dict[str, Any] = {
            "results": moved["results"],
            "atomic": False,
            "objectsMoved": moved["objectsMoved"],
        }
        if page.get("nextCursor") is not None:
            out["nextCursor"] = page["nextCursor"]
        return out

    async def _move_pairs(
        self, pairs: list[dict[str, str]], options: dict[str, Any]
    ) -> dict[str, Any]:
        results: list[dict[str, Any]] = []
        atomic = True
        moved = 0
        for pair in pairs:
            try:
                done = await self.move(pair["from"], pair["to"], options)
                atomic = atomic and done["atomic"]
                moved += 1
                results.append({"key": pair["from"]})
            except Exception as err:  # noqa: BLE001  (recorded per key)
                results.append({"key": pair["from"], "error": _outcome_error(err)})
        return {"results": results, "atomic": atomic, "objectsMoved": moved}

    async def delete_many(self, keys: list[str]) -> list[dict[str, Any]]:
        """Remove up to ``MAX_BATCH_KEYS`` objects.

        Args:
            keys: Object keys.

        Returns:
            One outcome per key.
        """
        _require_batch(len(keys), "deleteMany")
        out: list[dict[str, Any]] = []
        for key in keys:
            try:
                await self.store.delete(key)
                out.append({"key": key})
            except Exception as err:  # noqa: BLE001  (recorded per key)
                out.append({"key": key, "error": _outcome_error(err)})
        return out

    async def head_many(self, keys: list[str]) -> list[dict[str, Any] | None]:
        """Probe up to ``MAX_BATCH_KEYS`` objects.

        Args:
            keys: Object keys.

        Returns:
            One entry per key; ``None`` when missing.
        """
        _require_batch(len(keys), "headMany")
        return [await self.head(key) for key in keys]

    async def translate_keys(self, keys: list[str]) -> list[str]:
        """Translate host keys into object-store keys.

        Args:
            keys: Host keys.

        Returns:
            One translated key per host key.
        """
        _require_batch(len(keys), "translateKeys")
        return [
            translate_for_object_store(parse_host_key(key), self.store.prefix_bytes) for key in keys
        ]

    async def changes(self, request: dict[str, Any]) -> dict[str, Any]:
        """Return one page of the change feed, or the "no feed" page.

        Args:
            request: ``ChangesRequest`` as a dict.

        Returns:
            The page.
        """
        cursor = request.get("cursor")
        if cursor == NO_FEED_CURSOR:
            cursor = None
        page = None
        if self.feed is not None:
            page = await self.feed.changes(cursor, int(request.get("limit") or 0))
        if page is None:
            return {"changes": [], "cursor": NO_FEED_CURSOR, "more": False, "complete": False}
        result: dict[str, Any] = page
        return result


# ---------------------------------------------------------------------------
# Queue change feed
# ---------------------------------------------------------------------------

_CURSOR_TAG = "q1"
_RECEIVE_BATCH = 10
_MAX_PENDING_PAGES = 64
_MAX_PAGE = 256
QUEUE_LEASE_MS = min(12 * 60 * 60 * 1000, MAX_WEBHOOK_VISIBILITY_MS)
"""How long a received message stays leased: 12 hours, within the binding's cap."""


@dataclass(frozen=True)
class LeasedMessage:
    """One leased message.

    Attributes:
        body: The message body.
        receipt: The lease to acknowledge or release it by.
    """

    body: bytes
    receipt: str


@dataclass(frozen=True)
class Received:
    """What one receive returned.

    Attributes:
        messages: Leased messages, oldest first.
        missed: True when the queue dropped messages since the last receive.
    """

    messages: list[LeasedMessage]
    missed: bool = False


class NotificationQueue(Protocol):
    """A queue of notification messages with leases."""

    async def receive(self, max_messages: int) -> Received:
        """Lease up to ``max_messages`` waiting messages."""
        ...

    async def delete(self, receipts: list[str]) -> None:
        """Acknowledge handled messages."""
        ...

    async def release(self, receipts: list[str]) -> None:
        """Offer messages again on the next receive."""
        ...


@dataclass
class ParsedChange:
    """One change read from a message, with what orders it.

    Attributes:
        change: The ``ObjectChange`` dict.
        time: Comparable event time text, or ``None``.
        sequencer: Per-key hex sequencer, or ``None``.
    """

    change: dict[str, Any]
    time: str | None = None
    sequencer: str | None = None


class _WebhookQueue:
    """The ``WEBHOOKS`` binding as a :class:`NotificationQueue` for one row."""

    def __init__(self, webhooks: Any, name: str) -> None:
        self._webhooks = webhooks
        self._name = name

    async def receive(self, max_messages: int) -> Received:
        size = min(max(max_messages, 1), MAX_WEBHOOK_PULL_BATCH)
        page = await self._webhooks.pull(
            self._name, batch_size=size, visibility_timeout_ms=QUEUE_LEASE_MS
        )
        return Received(
            messages=[LeasedMessage(body=m.body, receipt=m.lease_id) for m in page.messages],
            missed=page.missed,
        )

    async def _settle(self, acks: list[str], retries: list[str]) -> None:
        if acks or retries:
            await self._webhooks.ack(self._name, acks=acks, retries=retries)

    async def delete(self, receipts: list[str]) -> None:
        await self._settle(receipts, [])

    async def release(self, receipts: list[str]) -> None:
        await self._settle([], receipts)


def webhook_queue(webhooks: Any, name: str) -> NotificationQueue:
    """Return the plugin's ``WEBHOOKS`` binding as a queue for one ``[[webhooks]]`` row.

    Args:
        webhooks: The binding.
        name: The row's name.

    Returns:
        The queue.
    """
    return _WebhookQueue(webhooks, name)


def parse_queue_cursor(cursor: str) -> tuple[str, int]:
    """Split a ``q1.<feed>.<seq>`` cursor.

    Args:
        cursor: A cursor this kind of feed wrote.

    Returns:
        The feed id and sequence.

    Raises:
        PluginError: ``invalid_cursor`` for any other text.
    """
    invalid = PluginError.from_wire("invalid_cursor", "not a notification-queue cursor")
    parts = cursor.split(".", 2)
    if len(parts) != 3 or parts[0] != _CURSOR_TAG or not parts[1]:
        raise invalid
    if not re.fullmatch(r"\+?[0-9]+", parts[2]):
        raise invalid
    seq = int(parts[2])
    if seq >= 2**53:
        raise invalid
    return parts[1], seq


def _sequencer_key(a: str, width: int) -> str:
    return a.rjust(width, "0").upper()


def order_changes(events: list[ParsedChange]) -> None:
    """Order changes by time (unknown first, stable), then each key's by sequencer.

    Args:
        events: Changes, reordered in place.
    """
    events.sort(key=lambda e: (e.time is not None, e.time or ""))
    by_key: dict[str, list[int]] = {}
    for at, event in enumerate(events):
        if event.sequencer is not None:
            by_key.setdefault(event.change["key"], []).append(at)
    for slots in by_key.values():
        if len(slots) < 2:
            continue
        group = [events[at] for at in slots]
        width = max(len(e.sequencer or "") for e in group)
        group.sort(key=lambda e: _sequencer_key(e.sequencer or "", width))
        for at, event in zip(slots, group, strict=True):
            events[at] = event


@dataclass
class _FeedState:
    feed: str
    next_seq: int = 1
    pending: list[dict[str, Any]] = field(default_factory=list)
    receiving: bool = False
    gap_at: int | None = None

    def to_json(self) -> str:
        return json.dumps(
            {
                "feed": self.feed,
                "nextSeq": self.next_seq,
                "pending": self.pending,
                "receiving": self.receiving,
                "gapAt": self.gap_at,
            }
        )


def _read_state(value: Any) -> _FeedState | None:
    if not isinstance(value, dict):
        return None
    feed, next_seq, pending = value.get("feed"), value.get("nextSeq"), value.get("pending")
    if not isinstance(feed, str) or not feed or not _is_count(next_seq):
        return None
    if not isinstance(pending, list):
        return None
    pages = []
    for page in pending:
        if not isinstance(page, dict) or not _is_count(page.get("seq")):
            return None
        receipts = page.get("receipts")
        if not isinstance(receipts, list) or not all(isinstance(r, str) for r in receipts):
            return None
        pages.append({"seq": page["seq"], "receipts": receipts})
    gap = value.get("gapAt")
    return _FeedState(
        feed=feed,
        next_seq=next_seq,
        pending=pages,
        receiving=value.get("receiving") is True,
        gap_at=gap if _is_count(gap) else None,
    )


_LOCKS: dict[str, asyncio.Lock] = {}
"""One lock per feed, so two calls in one isolate never interleave."""


class QueueFeed:
    """A change feed over a :class:`NotificationQueue`, state in KV.

    A message is acknowledged once the host sends back a cursor at or past
    the page that carried it; a page the host never finished is offered
    again. Cursors read ``q1.<feed>.<seq>``, as in the other SDKs.
    """

    def __init__(
        self,
        *,
        queue: NotificationQueue,
        parse: Callable[[bytes], list[ParsedChange]],
        kv: Any,
        state_key: str,
    ) -> None:
        """Create the feed; nothing is read until the first page.

        Args:
            queue: Where the messages come from.
            parse: Reads one message into changes; raise to drop it.
            kv: The KV binding the state lives in.
            state_key: KV key of the state (name it after the queue, bucket
                and prefix).
        """
        self._queue = queue
        self._parse = parse
        self._kv = kv
        self._state_key = state_key

    async def _load(self) -> _FeedState:
        stored = await self._kv.get(self._state_key, "json")
        return _read_state(stored) or _FeedState(feed=uuid.uuid4().hex)

    async def _persist(self, state: _FeedState) -> None:
        await self._kv.put(self._state_key, state.to_json())

    async def _release_all(self, receipts: list[str]) -> bool:
        released = True
        for at in range(0, len(receipts), _RECEIVE_BATCH):
            try:
                await self._queue.release(receipts[at : at + _RECEIVE_BATCH])
            except Exception:  # noqa: BLE001  (reported through the return value)
                released = False
        return released

    async def _settle(self, state: _FeedState, seq: int) -> bool:
        if not state.pending:
            return True
        read = [r for page in state.pending if page["seq"] <= seq for r in page["receipts"]]
        later = [r for page in state.pending if page["seq"] > seq for r in page["receipts"]]
        for at in range(0, len(read), _RECEIVE_BATCH):
            await self._queue.delete(read[at : at + _RECEIVE_BATCH])
        released = await self._release_all(later)
        state.pending = []
        state.receiving = state.receiving or not released
        await self._persist(state)
        return released

    async def _hand_out(
        self,
        state: _FeedState,
        events: list[ParsedChange],
        receipts: list[str],
        more: bool,
        complete: bool,
    ) -> dict[str, Any]:
        seq = state.next_seq
        pending = list(state.pending)
        if receipts:
            pending.append({"seq": seq, "receipts": receipts})
            del pending[: max(len(pending) - _MAX_PENDING_PAGES, 0)]
        nxt = _FeedState(
            feed=state.feed,
            next_seq=seq + 1,
            pending=pending,
            receiving=False,
            gap_at=state.gap_at if complete else seq,
        )
        try:
            await self._persist(nxt)
        except BaseException:
            await self._release_all(receipts)
            raise
        return {
            "changes": [e.change for e in events],
            "cursor": f"{_CURSOR_TAG}.{nxt.feed}.{seq}",
            "more": more,
            "complete": complete,
        }

    async def changes(self, cursor: str | None, limit: int) -> dict[str, Any]:
        """Return one page of changes after ``cursor``.

        Args:
            cursor: The previous page's cursor, or ``None`` to start over.
            limit: Requested changes (0 for the most).

        Returns:
            The page.

        Raises:
            PluginError: ``invalid_cursor`` for a cursor that is not this
                feed's or is ahead of it.
        """
        lock = _LOCKS.setdefault(self._state_key, asyncio.Lock())
        async with lock:
            return await self._changes(cursor, limit)

    async def _changes(self, cursor: str | None, limit_asked: int) -> dict[str, Any]:
        limit = _MAX_PAGE if limit_asked == 0 else min(max(limit_asked, 1), _MAX_PAGE)
        state = await self._load()
        read_up_to: int | None = None
        if cursor is not None:
            feed, seq = parse_queue_cursor(cursor)
            if feed == state.feed and seq >= state.next_seq:
                raise PluginError.from_wire("invalid_cursor", "queue cursor is ahead of the feed")
            if feed == state.feed:
                read_up_to = seq
        if read_up_to is None:
            await self._settle(state, 0)
            return await self._hand_out(state, [], [], False, False)
        complete = await self._settle(state, read_up_to) and not state.receiving
        if state.gap_at is not None:
            if read_up_to < state.gap_at:
                complete = False
            else:
                state.gap_at = None
        state.receiving = True
        await self._persist(state)

        events: list[ParsedChange] = []
        receipts: list[str] = []
        full = False
        taken = 0
        while len(events) < limit:
            if taken >= limit:
                full = True
                break
            want = min(limit - max(len(events), taken), _RECEIVE_BATCH)
            try:
                received = await self._queue.receive(want)
            except BaseException:
                await self._release_all(receipts)
                raise
            complete = complete and not received.missed
            taken += len(received.messages)
            overflow: list[str] = []
            for message in received.messages:
                try:
                    parsed = self._parse(message.body)
                except Exception:  # noqa: BLE001  (an unreadable message is a gap)
                    receipts.append(message.receipt)
                    complete = False
                    continue
                if len(events) + len(parsed) <= limit:
                    events.extend(parsed)
                    receipts.append(message.receipt)
                elif not events:
                    events.extend(parsed[:limit])
                    receipts.append(message.receipt)
                    complete = False
                else:
                    overflow.append(message.receipt)
            if overflow:
                complete = await self._release_all(overflow) and complete
                full = True
                break
            if not received.messages:
                break
            if len(events) >= limit:
                full = True
        order_changes(events)
        return await self._hand_out(state, events, receipts, full, complete)
