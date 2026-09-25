#!/usr/bin/env python3
"""Run Bookclerk CI from one validated plan artifact.

Subcommands:
  resolve   Plan job: predict, apply the execution mode, write ci-plan.json,
            GITHUB_OUTPUT and the step summary.
  validate  First step of every downstream job: the downloaded artifact must
            match this schema, run and checked-out commit.
  run CHECK Run one check (prerequisites first, once per job).
  gate      CI Gate: job results must match the expected execution.
  show      Print commands (prerequisites + check) without running them.

The artifact path is ``$BOOKCLERK_CI_PLAN`` or ``$RUNNER_TEMP/ci-plan/ci-plan.json``
(``--plan`` overrides). See docs/ci.md.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

_SCRIPTS = Path(__file__).resolve().parent
if str(_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS))

from ci_plan.execute import (  # noqa: E402
    ARTIFACT_NAME,
    Context,
    ExecError,
    artifact_summary,
    gate,
    git_head,
    job_outputs,
    load_artifact,
    resolve,
    run_check,
    validate,
    write_github_output,
    write_step_summary,
)
from ci_plan.plan import ALL_CHECKS, PlanError  # noqa: E402

WORKSPACE = _SCRIPTS.parent


def _default_plan_path() -> Path:
    if os.environ.get("BOOKCLERK_CI_PLAN"):
        return Path(os.environ["BOOKCLERK_CI_PLAN"])
    base = Path(os.environ.get("RUNNER_TEMP") or WORKSPACE / ".tmp" / "ci-exec")
    return base / "ci-plan" / ARTIFACT_NAME


def _load_validated(args: argparse.Namespace) -> dict:
    artifact = load_artifact(args.plan)
    run_id = args.run_id if args.run_id is not None else os.environ.get("GITHUB_RUN_ID")
    sha = None if args.no_sha_check else git_head(WORKSPACE)
    validate(artifact, run_id=run_id, checkout_sha=sha)
    return artifact


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--plan", type=Path, default=None, help="Plan artifact path")
    sub = parser.add_subparsers(dest="cmd", required=True)

    r = sub.add_parser("resolve")
    r.add_argument("--base")
    r.add_argument("--head")
    r.add_argument("--paths", nargs="*", help="Explicit changed paths (local dry runs)")
    r.add_argument("--force-full", action="store_true")
    r.add_argument("--selective", default=os.environ.get("SELECTIVE_CI", "0"))
    r.add_argument("--event", default=os.environ.get("GITHUB_EVENT_NAME", "local"))
    r.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", "local"))

    for name in ("validate", "run", "gate", "show"):
        p = sub.add_parser(name)
        p.add_argument("--run-id", default=None, help="Expected run id (default: $GITHUB_RUN_ID)")
        p.add_argument("--no-sha-check", action="store_true", help="Local only: skip checkout SHA match")
        if name in ("run", "show"):
            p.add_argument("check", choices=ALL_CHECKS)
        if name == "gate":
            p.add_argument("--needs-json", default=os.environ.get("NEEDS_JSON"), help="toJSON(needs)")

    args = parser.parse_args(argv)
    if args.plan is None:
        args.plan = _default_plan_path()

    try:
        if args.cmd == "resolve":
            artifact = resolve(
                base=args.base,
                head=args.head,
                selective_ci=args.selective,
                run_id=args.run_id,
                checkout_sha=git_head(WORKSPACE),
                event=args.event,
                force_full=args.force_full,
                workspace=WORKSPACE,
                paths=args.paths,
            )
            args.plan.parent.mkdir(parents=True, exist_ok=True)
            args.plan.write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")
            outputs = job_outputs(artifact)
            write_github_output(outputs)
            write_step_summary(artifact_summary(artifact))
            print(f"wrote {args.plan} (mode={artifact['mode']})")
            for k, v in outputs.items():
                print(f"  {k}={v}")
            return 0

        artifact = _load_validated(args)
        if args.cmd == "validate":
            print(f"plan ok: mode={artifact['mode']} run={artifact['run_id']} sha={artifact['checkout_sha']}")
            return 0
        if args.cmd in ("run", "show"):
            run_check(artifact, args.check, Context.from_env(WORKSPACE), dry_run=args.cmd == "show")
            return 0
        if args.cmd == "gate":
            if not args.needs_json:
                raise ExecError("gate needs --needs-json or $NEEDS_JSON (toJSON(needs))")
            problems = gate(artifact, json.loads(args.needs_json))
            for p in problems:
                print(f"::error::CI Gate: {p}")
            if problems:
                return 1
            print("CI Gate: every job matched the expected execution")
            return 0
    except (ExecError, PlanError) as exc:
        print(f"::error::ci-exec: {exc}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
