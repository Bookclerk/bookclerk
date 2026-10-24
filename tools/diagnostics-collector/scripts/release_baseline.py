#!/usr/bin/env python3
"""Reads the release tag the diagnostics ingest passes to ``/report``.

The ingest job fetches ``releases/latest`` from the GitHub API and hands the
tag to the collector as ``?baseline_version=``. The response body is bytes
this job did not produce, so it is parsed here, sans-IO, into a tag that is
safe to write to ``$GITHUB_OUTPUT`` and to place in a query string.

Usage: ``release_baseline.py < RELEASE_JSON`` prints the tag, or nothing when
the release is a draft or a prerelease (the collector then ingests every
version). It exits 1, printing the reason to stderr, when the body is not a
release object or the tag is not a plain version label.
"""

from __future__ import annotations

import json
import sys

#: Longest tag accepted, in characters.
MAX_TAG_CHARS = 64

#: Largest response body read, in bytes; a release object is far smaller.
MAX_BODY_BYTES = 1024 * 1024

#: Characters a tag may hold: unreserved in a URL query and free of newlines.
TAG_CHARS = frozenset("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-")


class BaselineError(ValueError):
    """The release body cannot yield a baseline tag."""


def parse_release(body: bytes) -> str:
    """Returns the baseline tag from a ``releases/latest`` response body.

    # Arguments
    * ``body``: the raw response body.

    # Returns
    The tag name, or ``""`` when the release is a draft or a prerelease.

    # Errors
    ``BaselineError`` when the body is too large, not UTF-8 JSON, not an
    object, or its ``tag_name`` is not 1 to ``MAX_TAG_CHARS`` characters from
    ``TAG_CHARS``.
    """
    if len(body) > MAX_BODY_BYTES:
        raise BaselineError("release body too large")
    try:
        data = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError) as err:
        raise BaselineError("release body is not JSON") from err
    if not isinstance(data, dict):
        raise BaselineError("release body is not an object")
    if data.get("draft") is True or data.get("prerelease") is True:
        return ""
    tag = data.get("tag_name")
    if not isinstance(tag, str) or not 1 <= len(tag) <= MAX_TAG_CHARS:
        raise BaselineError("release tag missing or too long")
    if not set(tag) <= TAG_CHARS:
        raise BaselineError("release tag has unexpected characters")
    return tag


def main(stdin: bytes) -> int:
    """Prints the baseline tag parsed from ``stdin``.

    # Arguments
    * ``stdin``: the ``releases/latest`` response body.

    # Returns
    0 after printing the tag (possibly empty), 1 after printing the error.
    """
    try:
        tag = parse_release(stdin)
    except BaselineError as err:
        print(f"release_baseline: {err}", file=sys.stderr)
        return 1
    print(tag)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.stdin.buffer.read(MAX_BODY_BYTES + 1)))
