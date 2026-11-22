#!/usr/bin/env python3
"""Oracle for the dashboard's one-report reading of a triage reply.

``tools/diagnostics-collector/triage-now.fuzz.test.ts`` reads the same model
replies with the dashboard's **Triage now** reader
(``dashboard/triage-now.ts``) and with the ingest run's plan validator
(``tools/diagnostics-collector/scripts/validate_plan.py``), and requires the
same proposal, so a member's button files only what the ingest run would
file for that report. Each line of stdin is one request; each line of stdout
is its answer:

- ``{"text": <str>, "report_id": <str>, "offered": [<int>, ...]}`` answers
  ``{"proposal": <object | null>}``: ``validate_plan`` over the reply with
  the report as the whole batch and the offered issues open, reduced to one
  proposal. An attachment gives ``{"kind": "attach", "issue": n}``; else the
  first issue gives ``{"kind": "issue", "title", "body", "bug"}`` with
  ``body`` the whole filed body; else ``{"kind": "none"}``. A ``PlanError``
  gives null.

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


def proposal(vp: types.ModuleType, text: str, report_id: str, offered: list[int]) -> dict[str, object] | None:
    """Reduces the validator's plan for a one-report batch to one proposal.

    Args:
        vp: The ``validate_plan`` module.
        text: The model's reply.
        report_id: The batch's only report ID.
        offered: Issue numbers offered for ``attach``.

    Returns:
        The proposal, or None when the validator refuses the reply.
    """
    try:
        plan = vp.validate_plan(text, {report_id}, frozenset(offered))
    except vp.PlanError:
        return None
    if plan.attachments:
        return {"kind": "attach", "issue": plan.attachments[0].issue}
    if plan.issues:
        issue = plan.issues[0]
        return {
            "kind": "issue",
            "title": issue.title,
            "body": issue.body,
            "bug": "bug" in issue.labels.split(","),
        }
    return {"kind": "none"}


def main() -> int:
    """Answers every request line on stdin.

    Returns:
        The process exit status, always 0.
    """
    vp = _load("validate_plan", ROOT / "tools" / "diagnostics-collector" / "scripts" / "validate_plan.py")
    out = sys.stdout
    for line in sys.stdin:
        try:
            request = json.loads(line)
        except (json.JSONDecodeError, RecursionError):
            out.write(json.dumps({"error": "decode"}) + "\n")
            continue
        answer = proposal(vp, request["text"], request["report_id"], request["offered"])
        out.write(json.dumps({"proposal": answer}) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
