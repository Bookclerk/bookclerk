#!/usr/bin/env python3
"""Turns a GitHub release tag into the image tags a release publishes.

The image publishing workflow reads ``github.event.release.tag_name`` (text
anyone who can publish a release chooses) and pushes the image under tags
derived from it. The tag is parsed here, sans-IO, so only a plain
``v<major>.<minor>.<patch>`` ever reaches ``$GITHUB_OUTPUT`` or a registry
reference.

Usage: ``image_release_tags.py < TAG`` prints ``<major>.<minor>.<patch>
<major>.<minor> <major> latest`` on one line. It exits 1, printing the reason
to stderr, when the tag is anything else.
"""

from __future__ import annotations

import re
import sys

#: Largest tag read, in bytes; GitHub caps tag names well below this.
MAX_TAG_BYTES = 256

#: The accepted release tag: ``v`` and three ASCII version numbers without
#: leading zeros, each at most nine digits.
RELEASE_TAG = re.compile(r"v(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})", re.ASCII)


class ReleaseTagError(ValueError):
    """The release tag cannot yield image tags."""


def release_tags(raw: bytes) -> list[str]:
    """Returns the image tags a release with tag ``raw`` publishes.

    # Arguments
    * ``raw``: the release's ``tag_name``, as bytes.

    # Returns
    ``[<major>.<minor>.<patch>, <major>.<minor>, <major>, "latest"]``.

    # Errors
    ``ReleaseTagError`` when ``raw`` is too long, not UTF-8, or not exactly
    ``v<major>.<minor>.<patch>`` as ``RELEASE_TAG`` describes.
    """
    if len(raw) > MAX_TAG_BYTES:
        raise ReleaseTagError("release tag too long")
    try:
        tag = raw.decode("utf-8")
    except UnicodeDecodeError as err:
        raise ReleaseTagError("release tag is not UTF-8") from err
    match = RELEASE_TAG.fullmatch(tag)
    if match is None:
        raise ReleaseTagError("release tag must be v<major>.<minor>.<patch>")
    major, minor, patch = match.groups()
    return [f"{major}.{minor}.{patch}", f"{major}.{minor}", major, "latest"]


def main(stdin: bytes) -> int:
    """Prints the image tags for the release tag read from ``stdin``.

    # Arguments
    * ``stdin``: the release tag.

    # Returns
    0 after printing the tags, 1 after printing the error.
    """
    try:
        tags = release_tags(stdin)
    except ReleaseTagError as err:
        print(f"image_release_tags: {err}", file=sys.stderr)
        return 1
    print(" ".join(tags))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.stdin.buffer.read(MAX_TAG_BYTES + 1)))
