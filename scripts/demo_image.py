#!/usr/bin/env python3
"""Reads the registry and health answers the demo deploy decides on.

``demo-deploy.yml`` chooses what reaches the public demo from two answers it
did not produce: the registry's manifest for the published demo image
(``docker buildx imagetools inspect --format '{{json .Manifest}}'``), whose
digest it then verifies and deploys, and the live demo's ``/_demo/health``
JSON, whose version decides whether a deploy is needed at all. Both are
parsed here, sans-IO, so only a ``sha256:`` digest or a 40-hex commit id
ever reaches ``$GITHUB_OUTPUT``, a registry reference or a Dockerfile.

Usage:

- ``demo_image.py digest < MANIFEST_JSON`` prints the manifest's digest. It
  exits 1, printing the reason to stderr, when the answer is anything else.
- ``demo_image.py live < HEALTH_JSON`` prints the commit the live demo runs,
  or nothing when the answer is not a configured demo reporting a commit.
"""

from __future__ import annotations

import json
import re
import sys

#: Largest registry or health answer read, in bytes. A manifest descriptor
#: and the health JSON are a few hundred bytes.
MAX_ANSWER_BYTES = 64 * 1024

#: An OCI content digest the deploy accepts: sha256 and 64 lowercase hex.
DIGEST = re.compile(r"sha256:[0-9a-f]{64}", re.ASCII)

#: A full Git commit id, as the demo Worker reports its version.
COMMIT = re.compile(r"[0-9a-f]{40}", re.ASCII)


class DemoImageError(ValueError):
    """The registry's answer does not name a usable image digest."""


def _json_object(raw: bytes) -> dict[str, object]:
    """Decodes ``raw`` as one UTF-8 JSON object.

    # Arguments
    * ``raw``: the answer's bytes.

    # Returns
    The decoded object.

    # Errors
    ``DemoImageError`` when ``raw`` is too long, not UTF-8, not JSON, or not
    a JSON object.
    """
    if len(raw) > MAX_ANSWER_BYTES:
        raise DemoImageError("answer too long")
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError) as err:
        raise DemoImageError("answer is not UTF-8 JSON") from err
    if not isinstance(value, dict):
        raise DemoImageError("answer is not a JSON object")
    return value


def manifest_digest(raw: bytes) -> str:
    """Returns the digest of the manifest descriptor the registry printed.

    # Arguments
    * ``raw``: the ``{{json .Manifest}}`` output, one JSON object with a
      ``digest`` string.

    # Returns
    The ``sha256:<64 hex>`` digest.

    # Errors
    ``DemoImageError`` when ``raw`` is not a JSON object whose ``digest`` is
    exactly ``sha256:`` and 64 lowercase hex digits.
    """
    digest = _json_object(raw).get("digest")
    if not isinstance(digest, str) or DIGEST.fullmatch(digest) is None:
        raise DemoImageError("digest must be sha256:<64 lowercase hex>")
    return digest


def live_version(raw: bytes) -> str | None:
    """Returns the commit the live demo reports, when it reports one.

    # Arguments
    * ``raw``: the ``/_demo/health`` body.

    # Returns
    The 40-hex commit id when the body is a JSON object with ``configured``
    exactly ``true`` and ``version`` a full lowercase commit id; ``None`` for
    any other answer, which the deploy treats as "the live commit is
    unknown" and deploys.
    """
    try:
        health = _json_object(raw)
    except DemoImageError:
        return None
    version = health.get("version")
    if health.get("configured") is not True or not isinstance(version, str):
        return None
    if COMMIT.fullmatch(version) is None:
        return None
    return version


def main(argv: list[str], stdin: bytes) -> int:
    """Runs one reader on ``stdin`` and prints its result.

    # Arguments
    * ``argv``: the arguments after the program name, ``["digest"]`` or
      ``["live"]``.
    * ``stdin``: the answer to read.

    # Returns
    0 after printing the result, 1 after printing the error, 2 for usage.
    """
    if argv == ["digest"]:
        try:
            print(manifest_digest(stdin))
        except DemoImageError as err:
            print(f"demo_image: {err}", file=sys.stderr)
            return 1
        return 0
    if argv == ["live"]:
        print(live_version(stdin) or "")
        return 0
    print("usage: demo_image.py digest|live < ANSWER", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], sys.stdin.buffer.read(MAX_ANSWER_BYTES + 1)))
