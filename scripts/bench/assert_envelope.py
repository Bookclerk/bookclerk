#!/usr/bin/env python3
"""CI checks for envelope-metrics.json. Latency is recorded and not gated."""

from __future__ import annotations

import json
import sys
from pathlib import Path

# Daemon anonymous memory after the seed. The cgroup lifetime peak includes
# the seed and cannot be reset inside the container, so it is not the gate.
ANON_LIMIT_BYTES = 256 * 1024 * 1024


def _daemon_memory(sample: dict) -> tuple[str, int | None]:
    """Anonymous memory for the daemon window, else `memory.current`."""
    anon = sample.get("anon")
    if isinstance(anon, int):
        return "anon", anon
    current = sample.get("memory_current")
    if isinstance(current, int):
        return "memory.current", current
    return "anon", None


def _cpu_is_one_core(cpu_max: str) -> bool:
    parts = str(cpu_max).split()
    if len(parts) != 2 or not parts[0].isdigit() or not parts[1].isdigit():
        return False
    quota, period = int(parts[0]), int(parts[1])
    return period > 0 and quota <= period


def assert_envelope(doc: dict) -> list[str]:
    """Returns human-readable failures. An empty list is a passing run."""
    problems: list[str] = []
    if "available_parallelism" not in doc or "affinity_cpus" not in doc:
        problems.append("available_parallelism and affinity_cpus must both be recorded")
    parallelism = doc.get("available_parallelism")
    affinity = doc.get("affinity_cpus")
    if not isinstance(parallelism, int) or parallelism < 1:
        problems.append(f"available_parallelism must be a positive int, got {parallelism!r}")
    if not isinstance(affinity, int) or affinity < 1:
        problems.append(f"affinity_cpus must be a positive int, got {affinity!r}")

    windows = doc.get("windows") or {}
    api = (windows.get("api") or {}).get("per_route") or {}
    for name, stats in api.items():
        if "/api/library/books" not in name:
            continue
        errors = stats.get("errors")
        if errors != 0:
            problems.append(f"{name} returned non-200 ({errors} errors)")
        total = stats.get("min_total")
        if "q=Title" in name and not (isinstance(total, int) and total > 0):
            problems.append(f"{name} did not hit a populated index (min_total={total!r})")

    for label in ("api", "api_with_rebuild"):
        mix = ((windows.get(label) or {}).get("mix_60s") or {})
        if mix.get("errors") != 0:
            problems.append(f"{label} mix errors={mix.get('errors')!r}")
        if not mix.get("requests"):
            problems.append(f"{label} mix recorded no requests")

    samples = [("idle", windows.get("idle") or {})]
    for label in ("api", "api_with_rebuild"):
        samples.append((label, (windows.get(label) or {}).get("sample") or {}))
    for label, sample in samples:
        kind, value = _daemon_memory(sample)
        if not isinstance(value, int) or value >= ANON_LIMIT_BYTES:
            problems.append(f"{label} {kind} {value} is not under 256 MiB")

    indexed = (windows.get("api_with_rebuild") or {}).get("indexed")
    if indexed != 10_000:
        problems.append(f"search index has {indexed} books, expected 10000")

    if _cpu_is_one_core(doc.get("cpu_max", "")):
        line = doc.get("media_pool_line") or ""
        if "media pool: 1 workers" not in line:
            problems.append(f"cpu.max is one core but media pool line is {line!r}")
    else:
        problems.append(f"cpu.max is not one core: {doc.get('cpu_max')!r}")
    return problems


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: assert_envelope.py envelope-metrics.json", file=sys.stderr)
        return 2
    path = Path(argv[1])
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"envelope: cannot read {path}: {exc}", file=sys.stderr)
        return 1
    problems = assert_envelope(doc)
    if problems:
        for problem in problems:
            print(f"envelope: {problem}", file=sys.stderr)
        return 1
    print(f"envelope checks passed ({path})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
