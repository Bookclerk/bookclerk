#!/usr/bin/env python3
"""Decide whether the current cgroup is already the 1 GiB / one-core envelope."""

from __future__ import annotations

import sys
from pathlib import Path

# 1 GiB. Docker `--memory=1g` writes this `memory.max`.
MEMORY_MAX_BYTES = 1024 * 1024 * 1024


def read_text(path: str) -> str:
    """File contents, or empty when the controller file is missing."""
    try:
        return Path(path).read_text(encoding="utf-8").strip()
    except OSError:
        return ""


def within_envelope(memory: str, cpu: str) -> bool:
    """True when both ceilings are set and neither is looser than 1 GiB / one core.

    `max`, a missing file, a memory ceiling above 1 GiB, or a `cpu.max` quota
    greater than its period are not this envelope. A tighter ceiling is.
    """
    if not memory.isdigit() or int(memory) > MEMORY_MAX_BYTES:
        return False
    parts = cpu.split()
    if len(parts) != 2 or not parts[0].isdigit() or not parts[1].isdigit():
        return False
    quota, period = int(parts[0]), int(parts[1])
    return period > 0 and quota <= period


def main(argv: list[str]) -> int:
    if len(argv) != 4 or argv[1] not in ("classify", "check"):
        print(
            "usage: envelope_limits.py classify|check memory.max cpu.max",
            file=sys.stderr,
        )
        return 2
    memory = read_text(argv[2])
    cpu = read_text(argv[3])
    ok = within_envelope(memory, cpu)
    if argv[1] == "classify":
        print("accept" if ok else "delegate")
        return 0
    if ok:
        return 0
    print(
        "cgroup limits are max or looser than 1 GiB / one core: "
        f"memory.max={memory!r} cpu.max={cpu!r}",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
