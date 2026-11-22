"""Tracked file names must check out on every platform CI builds on.

Windows refuses a path with a reserved device name (``nul``, ``con``,
``com1``, with or without an extension) in any component, a component that
ends in a dot or a space, and a handful of characters. Git on Linux and
macOS accepts all of them, so one such file passes every pull request check
there and then fails the checkout of every Windows job in the merge queue,
for its own pull request and for the ones queued behind it. Two names that
differ only by letter case collide on Windows and macOS the same way.
"""

from __future__ import annotations

import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Device names Windows reserves, alone or before any extension.
RESERVED = re.compile(
    r"^(con|prn|aux|nul|com[0-9¹²³]|lpt[0-9¹²³]|conin\$|conout\$)(\..*)?$",
    re.IGNORECASE,
)

# Characters Windows does not allow in a file name.
FORBIDDEN = re.compile(r'[<>:"|?*\\\x00-\x1f]')


def problem(path: str) -> str | None:
    """Says why Windows cannot check a path out.

    Args:
        path: A tracked path with ``/`` separators.

    Returns:
        The reason, or ``None`` when the path is portable.
    """
    for part in path.split("/"):
        if RESERVED.match(part):
            return f"{part!r} is a reserved device name on Windows"
        if part.endswith((".", " ")):
            return f"{part!r} ends in a dot or a space"
        if FORBIDDEN.search(part):
            return f"{part!r} holds a character Windows forbids"
    return None


def tracked() -> list[str]:
    """Lists every path git tracks in this checkout."""
    listed = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True
    ).stdout
    return [p for p in listed.decode("utf-8", "surrogateescape").split("\0") if p]


class RuleTest(unittest.TestCase):
    """The rule itself, on names that never need to exist."""

    def test_unportable_names_are_refused(self) -> None:
        for path in (
            "corpus/token/nul",
            "a/NUL.txt",
            "con",
            "x/com1.json",
            "x/LPT9",
            "aux/file",
            "dir./file",
            "trailing ",
            "what?",
            "a:b",
            "back\\slash",
            "tab\there",
        ):
            with self.subTest(path=path):
                self.assertIsNotNone(problem(path))

    def test_ordinary_names_are_allowed(self) -> None:
        for path in (
            "corpus/token/nul_byte",
            "null",
            "console.rs",
            "com10",
            "auxiliary/a.b",
            ".github/x.yml",
            "a b/c",
        ):
            with self.subTest(path=path):
                self.assertIsNone(problem(path))


class TrackedPathsTest(unittest.TestCase):
    """Every tracked path, so a bad name fails here and not in the merge queue."""

    def test_every_tracked_path_checks_out_on_windows(self) -> None:
        paths = tracked()
        self.assertTrue(paths)
        bad = {p: reason for p in paths if (reason := problem(p))}
        self.assertEqual(bad, {}, "rename these files; Windows cannot check them out")

    def test_no_two_tracked_paths_differ_only_by_case(self) -> None:
        seen: dict[str, str] = {}
        clashes = []
        for path in tracked():
            other = seen.setdefault(path.casefold(), path)
            if other != path:
                clashes.append((other, path))
        self.assertEqual(clashes, [], "these collide on Windows and macOS")


if __name__ == "__main__":
    unittest.main()
