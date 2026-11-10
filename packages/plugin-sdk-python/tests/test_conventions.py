"""Conventions every test module follows so the nightly mutation run can use it."""

from __future__ import annotations

import re
from pathlib import Path

#: A path climbed a fixed number of folders, such as ``Path(__file__).parents[3]``.
FIXED_DEPTH = re.compile(r"\.parents\[\d+\]")


def test_no_test_finds_the_repository_at_a_fixed_depth() -> None:
    """Every test module finds the repository root by a marker, never a fixed depth.

    The nightly mutation run (``.github/workflows/mutation-python.yml``)
    copies the tests one folder deeper, under ``mutants/``, so a fixed
    ``parents[N]`` points outside the repository there and fails the run's
    baseline. Look for a folder the root holds instead, as ``REPO_ROOT`` in
    ``test_db_stream.py`` does.
    """
    here = Path(__file__).resolve().parent
    offenders = [
        f"{path.name}:{number}"
        for path in sorted(here.glob("*.py"))
        if path.name != Path(__file__).name
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1)
        if FIXED_DEPTH.search(line)
    ]
    assert offenders == []
