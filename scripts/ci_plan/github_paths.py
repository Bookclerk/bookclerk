"""Trusted-root checks for GitHub Actions file-command env paths."""

from __future__ import annotations

import os
from typing import IO, Any


def _runner_roots() -> list[str]:
    """Real paths of runner-owned roots (``RUNNER_TEMP``, ``GITHUB_WORKSPACE``)."""
    roots: list[str] = []
    for key in ("RUNNER_TEMP", "GITHUB_WORKSPACE"):
        raw = os.environ.get(key)
        if not raw or "\0" in raw:
            continue
        roots.append(os.path.realpath(raw))
    return roots


def github_actions_file_path(path_s: str, *, label: str) -> str:
    """Resolve a GitHub Actions file-command path under a trusted runner root.

    ``GITHUB_OUTPUT`` / ``GITHUB_STEP_SUMMARY`` are runner-owned channels.
    GitHub documents that workflow ``env:`` cannot overwrite default ``GITHUB_*``
    / ``RUNNER_*`` variables. This helper preserves the path (spaces, Unicode,
    Windows drive letters) and requires canonical containment under
    ``RUNNER_TEMP`` or ``GITHUB_WORKSPACE``.

    Args:
        path_s: Raw env value for the file command.
        label: Env var name for error messages.

    Returns:
        Canonical absolute path under a trusted runner root.

    Raises:
        SystemExit: When the path is empty/NUL, runner roots are unset, or the
            resolved path escapes every trusted root.
    """
    if not path_s or "\0" in path_s:
        raise SystemExit(f"{label} must be a non-empty path without NUL")

    roots = _runner_roots()
    if not roots:
        raise SystemExit(
            f"{label} is set but neither RUNNER_TEMP nor GITHUB_WORKSPACE is a "
            "usable path; refusing to open a runner file-command path outside "
            "Actions"
        )

    resolved = os.path.realpath(path_s)
    for root in roots:
        if resolved == root or resolved.startswith(root + os.sep):
            return resolved

    raise SystemExit(
        f"refusing {label} outside RUNNER_TEMP/GITHUB_WORKSPACE: {resolved}"
    )


def open_github_actions_append(path_s: str, *, label: str) -> IO[Any]:
    """Open a runner file-command path for append.

    ``RUNNER_TEMP`` and ``GITHUB_WORKSPACE`` are the trusted runner-origin
    boundary. GitHub documents that workflow ``env:`` cannot overwrite the
    default ``GITHUB_*`` / ``RUNNER_*`` variables. This opens the path
    :func:`github_actions_file_path` already verified (canonical containment
    under that boundary). Ordinary filenames are preserved, including spaces,
    Unicode, and ``..`` inside a name such as ``edition..2``.

    CodeQL's local threat model still taints those environment variables, so
    this ``open`` may remain a path-injection result. A hard-coded prefix
    allowlist is not a substitute: self-hosted runners use ``/var/tmp``,
    ``/srv/actions``, and macOS ``/private/var/folders`` layouts that no
    literal hosted-runner prefix covers.
    """
    resolved = github_actions_file_path(path_s, label=label)
    return open(resolved, "a", encoding="utf-8")
