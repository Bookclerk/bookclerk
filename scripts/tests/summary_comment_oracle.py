#!/usr/bin/env python3
"""Oracle for the dashboard's TypeScript port of the summary comment renderer.

``tools/diagnostics-collector/summary.fuzz.test.ts`` runs the TypeScript
renderer (``dashboard/summary.ts``) and this script on the same inputs and
requires identical output, so the comment the owner's "Update GitHub now"
button writes is the one the ingest run would write. Each line of stdin is
one request; each line of stdout is its answer:

- ``{"payload": <any>, "dashboard": <str | null>}`` answers
  ``{"rendered": [[issue, fingerprint, body], ...]}`` from
  ``render_summaries``, with ``dashboard`` first passed through
  ``dashboard_origin`` as the ingest script does.
- ``{"origin": <any>}`` answers ``{"origin": <str | null>}`` from
  ``dashboard_origin``.

A line Python cannot decode answers ``{"error": "decode"}``.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def _load(name: str, path: Path) -> types.ModuleType:
    """Loads a module by path (``-I`` keeps script directories off ``sys.path``)."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def main() -> int:
    """Answers every request line on stdin.

    Returns:
        The process exit status, always 0.
    """
    sc = _load("summary_comment", ROOT / "tools" / "diagnostics-collector" / "scripts" / "summary_comment.py")
    out = sys.stdout
    for line in sys.stdin:
        try:
            request = json.loads(line)
        except (json.JSONDecodeError, RecursionError):
            out.write(json.dumps({"error": "decode"}) + "\n")
            continue
        if "origin" in request:
            raw = request["origin"]
            answer = {"origin": sc.dashboard_origin(raw) if isinstance(raw, str) or raw is None else None}
        else:
            dashboard = request.get("dashboard")
            origin = sc.dashboard_origin(dashboard) if isinstance(dashboard, str) else None
            rendered = sc.render_summaries(request.get("payload"), origin)
            answer = {"rendered": [[r.issue, r.fingerprint, r.body] for r in rendered]}
        out.write(json.dumps(answer) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
