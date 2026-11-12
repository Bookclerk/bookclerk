"""Execution side of Bookclerk selective CI.

``resolve`` (plan job) wraps the planner's prediction with the execution mode
and writes the ``ci-plan.json`` artifact plus job outputs. Every downstream
job ``validate``s that artifact before running ``run <check>``, which runs
the check's prerequisites (once per job, in declared order) and then its
commands. ``gate`` compares job results with the *expected execution* so
intentional skips pass and unexpected skips, runs, failures or cancellations
fail the required ``CI Gate`` check.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tomllib
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping, Sequence

from .github_paths import open_github_actions_append
from .plan import (
    CHECK_JOB,
    E2E_PACKAGE,
    JOB_CHECKS,
    NON_CARGO_CHECKS,
    OS_LEG_JOBS,
    STEP_GATED_JOBS,
    PlanError,
    Plan,
    discover_guests,
    full_plan,
    load_metadata,
    load_relations,
    package_index_from_metadata,
    plan_from_dict,
    plan_from_event,
    plan_to_dict,
    plan_to_summary,
    plugin_test_runner,
)

SCHEMA_VERSION = 5
ARTIFACT_NAME = "ci-plan.json"
# One path segment. A fresh id is minted per resolve so marker dirs cannot
# collide across local re-resolves that share a temp dir, commit, and run id.
_EXECUTION_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
CLIPPY_LINTS = (
    "-D",
    "warnings",
    "-D",
    "clippy::missing_docs_in_private_items",
    "-D",
    "clippy::missing_errors_doc",
    "-D",
    "clippy::missing_panics_doc",
)
PUBLISH_CRATES_ARGS = (
    "-p",
    "bookclerk-plugin-abi",
    "-p",
    "bookclerk-plugin-manifest",
    "-p",
    "bookclerk-plugin-sdk",
)
CONFINEMENT_PACKAGES = (
    "bookclerk-sandbox",
    "bookclerk-media",
    "bookclerk-media-worker",
    "bookclerk-jail",
)
# `rust_test` runs under cargo-nextest: every test binary's tests share one
# parallel pool instead of running one binary after another, and each test
# gets its own process. Exit-status semantics match `cargo test --tests`.
NEXTEST_RUN = (
    "cargo",
    "nextest",
    "run",
    "--tests",
    "--failure-output",
    "immediate-final",
    "--no-tests",
    "warn",
)
# Merge-group runs retry a failed test once; nextest reports a pass on retry
# as FLAKY in the summary, and a test that fails twice still fails the job.
MERGE_GROUP_TEST_RETRIES = 1


def _nextest_mode(event: str) -> list[str]:
    """Failure handling for one nextest run, by the event the plan was resolved for.

    A merge group only needs to know whether it can land, so it retries once and
    stops at the first test that still fails. Pull requests run every test, so
    one round of CI shows every failure to fix.
    """
    if event == "merge_group":
        return ["--retries", str(MERGE_GROUP_TEST_RETRIES), "--fail-fast"]
    return ["--no-fail-fast"]

# `harness = false` test targets whose `main` does not answer libtest's
# `--list` protocol: nextest would list zero tests in them, so they are
# excluded from nextest and run with `cargo test --test`.
# scripts/tests/test_ci_contract.py fails when a new one is not listed here.
CUSTOM_HARNESS_TESTS: tuple[tuple[str, str], ...] = (("bookclerk-plugin-catalog", "plugin_mutation_lock"),)
# Staged-installation targets of the e2e crate. `native_gateway*` is its own
# check (three OSes) and must not run again here.
E2E_STAGED_TARGETS = ("--lib", "--test", "staged_plugins", "--test", "installed_plugin_path")
NATIVE_GATEWAY_TESTS = (
    "native_gateway",
    "native_gateway_isolation",
    "native_gateway_lifecycle",
    "native_gateway_authenticated_endpoint",
)
POSTGRES_STEPS: dict[str, tuple[str, list[str]]] = {
    "library_queue": (
        "Postgres job-queue tests",
        ["cargo", "test", "-p", "bookclerk-library", "--lib", "postgres_", "--", "--ignored", "--nocapture"],
    ),
    "library_totp": (
        "Postgres TOTP atomic tests",
        ["cargo", "test", "-p", "bookclerk-library", "--lib", "postgres_totp", "--", "--nocapture"],
    ),
    "library_vectors": (
        "Postgres shared SQL-plan vectors",
        [
            "cargo",
            "test",
            "-p",
            "bookclerk-library",
            "--lib",
            "typed_shared_vectors_on_postgres",
            "--",
            "--ignored",
            "--nocapture",
        ],
    ),
    "db_guest": (
        "Postgres guest page tests",
        ["cargo", "test", "-p", "bookclerk-db-guest", "--lib", "postgres_", "--", "--ignored", "--nocapture"],
    ),
    "binding_schema": (
        "Postgres plugin binding-schema isolation tests",
        [
            "cargo",
            "test",
            "-p",
            "bookclerk-plugin-database-postgres",
            "--lib",
            "postgres_",
            "--",
            "--ignored",
            "--nocapture",
        ],
    ),
    "adapter_conformance": (
        "Postgres adapter conformance suite (shipped guest over RPC)",
        [
            "cargo",
            "test",
            "-p",
            "bookclerk-plugin-database-postgres",
            "--test",
            "adapter_conformance",
            "--",
            "--ignored",
            "--nocapture",
        ],
    ),
    "rpc_like": (
        "Postgres production RPC proxy boundary (LIKE, identity inserts)",
        [
            "cargo",
            "test",
            "-p",
            "bookclerk-plugin-host",
            "--lib",
            "through_postgres_guest",
            "--",
            "--ignored",
            "--nocapture",
        ],
    ),
}


# Cargo subcommands kept in build-only runs: each leaves artifacts in
# `target/` that the dependency cache saves.
BUILD_SUBCOMMANDS = frozenset({"build", "build-app", "check", "clippy", "install-platform"})
# Jobs that save the dependency caches other runs restore (see ci.yml
# `save-if`). Build-only runs skip every other job.
BUILD_ONLY_JOBS = frozenset({"check", "test", "native-gateway", "release"})
# Checks in a build-only job that only measure or test, so a build-only run
# drops them (the small-VPS envelope shares the release job).
BUILD_ONLY_SKIPPED_CHECKS = frozenset({"envelope"})
# Jobs a draft pull request runs: lints, docs and the Rust unit and
# integration tests, the fast feedback a draft needs. Every other selected
# job (e2e, release, the confinement and gateway matrices, macOS, Postgres,
# Windows, the Workers) is deferred: it runs once the pull request is marked
# ready for review, on every later push, and in the merge queue, which always
# runs the full suite. GitHub keeps drafts out of the merge queue, so no
# deferred job is skipped before a merge (docs/contributing/ci.md#draft-pull-requests).
DRAFT_JOBS = frozenset({"check", "test"})
# A pull request's run (draft or ready) tests on Linux only. The Windows
# legs of the matrix jobs, the Windows-only checks below and the macOS job
# run in the merge queue, which always runs every platform, and on the
# build-only `main` push, which saves their caches. CI Gate requires them
# there, so a platform failure keeps the pull request out of `main`
# (docs/contributing/ci.md#platform-legs-in-the-merge-queue).
QUEUE_ONLY_CHECKS = frozenset({"windows_cluster"})
# Runner labels of the matrix jobs' `os` legs (`os_matrix` plan output).
LINUX_RUNNERS = ("ubuntu-latest",)
ALL_RUNNERS = ("ubuntu-latest", "windows-latest")
# Prerequisites kept in build-only runs (staging and installs only feed tests).
BUILD_PREREQS = frozenset({"build", "ensure_workerd"})
# Set to "true" by a build-only job whose dependency cache key is already
# saved (ci.yml "Skip the build when the saved cache is current"): the build
# would save nothing, so its checks issue no commands. Ignored outside
# build-only runs, so it can never skip a test.
SAVED_CACHE_CURRENT_ENV = "CI_SAVED_CACHE_CURRENT"


class ExecError(Exception):
    """Invalid plan artifact, unexpected check, or failed command."""


@dataclass
class Command:
    """One subprocess invocation."""

    label: str
    argv: list[str]
    cwd: str | None = None
    env: dict[str, str] = field(default_factory=dict)


@dataclass
class Context:
    """Paths and platform facts shared by prerequisites and commands."""

    workspace: Path
    os_name: str  # "Linux" | "Darwin" | "Windows"
    tmp: Path
    files_dir: Path
    artifacts: Path

    @classmethod
    def from_env(cls, workspace: Path) -> "Context":
        tmp = Path(os.environ.get("RUNNER_TEMP") or workspace / "target" / "ci-exec")
        files = Path(os.environ.get("BOOKCLERK_FILES_DIR") or tmp / "BookclerkFiles")
        artifacts = Path(os.environ.get("BOOKCLERK_PLUGIN_ARTIFACTS") or tmp / "bookclerk-plugins")
        return cls(workspace, platform.system(), tmp, files, artifacts)

    @property
    def bin_dir(self) -> Path:
        return self.workspace / "target" / "debug"

    def path_with_bins(self) -> str:
        return os.pathsep.join([str(self.bin_dir), os.environ.get("PATH", "")])


# ---------------------------------------------------------------------------
# resolve / validate


def execution_mode(predicted: Plan, selective_ci: str) -> str:
    """``full`` (predicted full suite), ``shadow`` or ``selective``."""
    if predicted.full_suite:
        return "full"
    return "selective" if selective_ci == "1" else "shadow"


def require_execution_id(artifact: Mapping[str, Any]) -> str:
    """Returns ``execution_id`` or raises when it is missing or unsafe as a path segment."""
    if "execution_id" not in artifact:
        raise ExecError("plan artifact missing `execution_id`")
    value = artifact["execution_id"]
    if not isinstance(value, str) or _EXECUTION_ID.fullmatch(value) is None:
        raise ExecError(f"plan execution_id {value!r} is not a single safe path segment")
    return value


def resolve(
    *,
    base: str | None,
    head: str | None,
    selective_ci: str,
    run_id: str,
    checkout_sha: str,
    event: str,
    force_full: bool = False,
    build_only: bool = False,
    draft: bool = False,
    linux_only: bool = False,
    workspace: Path | None = None,
    paths: Sequence[str] | None = None,
    metadata: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """Predicts the plan and resolves one execution the jobs must perform.

    Every call mints a new ``execution_id``. Prerequisite completion is
    namespaced by that id, so resolving again starts a fresh execution.
    ``build_only`` (push to ``main``) compiles what every check compiles,
    which saves the caches later runs restore, and runs no tests: the same
    commit already passed the full suite in its merge group.
    ``draft`` (a draft pull request) keeps only the checks of
    ``DRAFT_JOBS`` and records the others under ``deferred``; they run when
    the pull request is ready for review. ``linux_only`` (any pull request
    run) defers ``QUEUE_ONLY_CHECKS`` and the Windows and macOS legs to the
    merge queue.

    Raises:
        ExecError: ``build_only`` was set with ``draft`` or ``linux_only``
            (a push to ``main`` is never a pull request run).
    """
    if build_only and (draft or linux_only):
        raise ExecError("a run is either build-only (push to main) or a pull request run, not both")
    predicted = plan_from_event(
        base=base,
        head=head,
        workspace_root=workspace,
        metadata=metadata,
        paths=paths,
        force_full=force_full,
    )
    mode = execution_mode(predicted, selective_ci)
    if mode == "shadow":
        index = package_index_from_metadata(load_metadata(workspace, metadata))
        execution = full_plan(
            index,
            load_relations(),
            reason="shadow mode (SELECTIVE_CI != 1): run everything, record the prediction",
            changed_paths=predicted.changed_paths,
            guests=discover_guests(workspace or index.workspace_root, index),
        )
    else:
        execution = predicted
    if build_only:
        execution = plan_from_dict(plan_to_dict(execution))
        execution.checks = {
            c: v
            for c, v in execution.checks.items()
            if CHECK_JOB[c] in BUILD_ONLY_JOBS and c not in BUILD_ONLY_SKIPPED_CHECKS
        }
        # A job whose only checks were dropped has nothing to build.
        kept = {CHECK_JOB[c] for c in execution.checks}
        execution.jobs = {job: on and job in BUILD_ONLY_JOBS and job in kept for job, on in execution.jobs.items()}
    deferred: list[str] = []
    if draft:
        execution = plan_from_dict(plan_to_dict(execution))
        deferred = sorted(c for c in execution.checks if CHECK_JOB[c] not in DRAFT_JOBS)
        execution.checks = {c: v for c, v in execution.checks.items() if CHECK_JOB[c] in DRAFT_JOBS}
        execution.jobs = {job: on and job in DRAFT_JOBS for job, on in execution.jobs.items()}
    if linux_only:
        execution = plan_from_dict(plan_to_dict(execution))
        deferred += sorted(c for c in execution.checks if c in QUEUE_ONLY_CHECKS)
        execution.checks = {c: v for c, v in execution.checks.items() if c not in QUEUE_ONLY_CHECKS}
        # A job whose only checks were deferred has nothing left to run.
        execution.jobs = {
            job: on and any(c in execution.checks for c in JOB_CHECKS[job]) for job, on in execution.jobs.items()
        }
    return {
        "schema_version": SCHEMA_VERSION,
        "execution_id": uuid.uuid4().hex,
        "run_id": str(run_id),
        "event": event,
        "base_sha": base or "",
        "head_sha": head or "",
        "checkout_sha": checkout_sha,
        "selective_ci": selective_ci,
        "mode": mode,
        "build_only": bool(build_only),
        "draft": bool(draft),
        "linux_only": bool(linux_only),
        "deferred": sorted(deferred),
        "predicted": plan_to_dict(predicted),
        "execution": plan_to_dict(execution),
    }


def _example_runners(plan: Plan) -> list[str]:
    """The test runners the selected ``plugin_examples`` check needs.

    An artifact written before the planner recorded runners only had
    ``npm`` examples.
    """
    runners = plan.params("plugin_examples").get("runners")
    return [str(r) for r in runners] if isinstance(runners, list) else ["npm"]


def job_outputs(artifact: Mapping[str, Any]) -> dict[str, str]:
    """``GITHUB_OUTPUT`` keys for ``if:`` gates, all from ``execution``."""
    plan = plan_from_dict(artifact["execution"])
    out: dict[str, str] = {"mode": artifact["mode"]}
    for job, on in plan.jobs.items():
        out[f"job_{job.replace('-', '_')}"] = _b(on)
    for job in STEP_GATED_JOBS:
        for check in JOB_CHECKS[job]:
            out[f"check_{check}"] = _b(plan.selected(check))
    check_job = [c for c in JOB_CHECKS["check"] if plan.selected(c)]
    out["check_needs_cargo"] = _b(any(c not in NON_CARGO_CHECKS for c in check_job))
    docs = plan.params("api_docs")
    out["check_needs_node"] = _b(
        plan.selected("lint")
        or plan.selected("ui")
        or plan.selected("plugin_sdk_abi")
        or plan.selected("workerd_guests")
        or (plan.selected("plugin_examples") and "npm" in _example_runners(plan))
        or (plan.selected("api_docs") and (docs.get("all") or docs.get("typescript") or docs.get("ui")))
    )
    # uv installs the Python SDK and its pdoc extra from uv.lock.
    out["check_needs_uv"] = _b(
        plan.selected("python_sdk")
        or (plan.selected("plugin_examples") and "pytest" in _example_runners(plan))
        or (plan.selected("api_docs") and (docs.get("all") or docs.get("python")))
    )
    # The `os` legs of the confinement, native-gateway and tray matrices,
    # and whether the macOS job may run (see QUEUE_ONLY_CHECKS).
    linux_only = bool(artifact.get("linux_only"))
    out["os_matrix"] = json.dumps(list(LINUX_RUNNERS if linux_only else ALL_RUNNERS), separators=(",", ":"))
    out["os_legs"] = _b(not linux_only)
    out["release_mode"] = plan.params("release").get("mode", "none") if plan.selected("release") else "none"
    # The `test` job's matrix: one leg per part (see TEST_PARTS). A
    # build-only `main` push only compiles, so it keeps one leg.
    split = plan.selected("rust_test") and plan.params("rust_test").get("workspace") and not artifact.get("build_only")
    out["test_parts"] = json.dumps(list(range(1, (TEST_PARTS if split else 1) + 1)), separators=(",", ":"))
    return out


def _b(v: Any) -> str:
    return "true" if v else "false"


def load_artifact(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise ExecError(f"plan artifact missing: {path}") from exc
    except (OSError, json.JSONDecodeError) as exc:
        raise ExecError(f"plan artifact unreadable: {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise ExecError("plan artifact is not a JSON object")
    return data


def validate(
    artifact: Mapping[str, Any],
    *,
    run_id: str | None,
    checkout_sha: str | None,
) -> None:
    """Rejects artifacts from another schema, run, or commit."""
    if artifact.get("schema_version") != SCHEMA_VERSION:
        raise ExecError(
            f"plan schema_version {artifact.get('schema_version')!r} != {SCHEMA_VERSION}"
        )
    for key in (
        "run_id",
        "checkout_sha",
        "mode",
        "build_only",
        "draft",
        "linux_only",
        "deferred",
        "predicted",
        "execution",
        "execution_id",
    ):
        if key not in artifact:
            raise ExecError(f"plan artifact missing `{key}`")
    require_execution_id(artifact)
    if not isinstance(artifact["build_only"], bool):
        raise ExecError(f"plan build_only {artifact['build_only']!r} is not a boolean")
    for flag in ("draft", "linux_only"):
        if not isinstance(artifact[flag], bool):
            raise ExecError(f"plan {flag} {artifact[flag]!r} is not a boolean")
    if artifact["build_only"] and (artifact["draft"] or artifact["linux_only"]):
        raise ExecError("plan is both build-only and a pull request run")
    deferred = artifact["deferred"]
    if not isinstance(deferred, list) or not all(isinstance(c, str) and c in CHECK_JOB for c in deferred):
        raise ExecError(f"plan deferred {deferred!r} is not a list of known checks")
    # A draft may defer any check; a ready Linux-only run only the
    # merge-queue ones; any other run nothing.
    if not artifact["draft"] and any(not (artifact["linux_only"] and c in QUEUE_ONLY_CHECKS) for c in deferred):
        raise ExecError(f"plan defers {deferred!r}, which this run may not defer")
    if artifact["mode"] not in ("full", "shadow", "selective"):
        raise ExecError(f"plan mode {artifact['mode']!r} is invalid")
    if run_id is not None and str(artifact["run_id"]) != str(run_id):
        raise ExecError(f"plan run_id {artifact['run_id']} != this run {run_id}")
    if checkout_sha is not None and artifact["checkout_sha"] != checkout_sha:
        raise ExecError(
            f"plan was computed for {artifact['checkout_sha']}, checked out {checkout_sha}"
        )
    try:
        plan = plan_from_dict(artifact["execution"])
        plan_from_dict(artifact["predicted"])
    except (KeyError, TypeError, ValueError) as exc:
        raise ExecError(f"plan artifact malformed: {exc}") from exc
    for check in plan.checks:
        if check not in CHECK_JOB:
            raise ExecError(f"plan selects unknown check `{check}`")
    if set(plan.jobs) != set(JOB_CHECKS):
        raise ExecError(f"plan jobs {sorted(plan.jobs)} != {sorted(JOB_CHECKS)}")


def git_head(workspace: Path) -> str:
    return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=workspace, text=True).strip()


# ---------------------------------------------------------------------------
# prerequisites + commands


def prereq_commands(prereq: Mapping[str, Any], ctx: Context) -> list[Command]:
    kind = prereq["kind"]
    if kind == "build":
        argv = ["cargo", "build"]
        for pkg in prereq["packages"]:
            argv += ["-p", pkg]
        return [Command("prerequisite: build " + ", ".join(prereq["packages"]), argv)]
    if kind == "ensure_workerd":
        return [Command("prerequisite: pinned workerd", ["cargo", "ensure-workerd"])]
    if kind == "install_platform":
        return [
            Command(
                "prerequisite: install platform guests",
                ["cargo", "install-platform", "--skip-build"],
                env={"BOOKCLERK_FILES_DIR": str(ctx.files_dir)},
            )
        ]
    if kind == "stage_plugins":
        env = {"BOOKCLERK_PLUGIN_ARTIFACTS": str(ctx.artifacts)}
        argv = ["cargo", "stage-plugins", "--skip-build", "--dest", str(ctx.artifacts)]
        if prereq.get("all"):
            return [Command("prerequisite: stage optional + examples", argv + ["--optional", "--examples"], env=env)]
        if not prereq.get("plugins"):
            # Platform-only scope: the suite still requires an artifacts root.
            return [Command("prerequisite: empty staging root", ["<mkdir>", str(ctx.artifacts)])]
        for pid in prereq["plugins"]:
            argv += ["--plugin", pid]
        return [Command("prerequisite: stage " + ", ".join(prereq["plugins"]), argv, env=env)]
    raise ExecError(f"unknown prerequisite kind `{kind}`")


def _pkg_args(pkgs: Sequence[str]) -> list[str]:
    out: list[str] = []
    for p in pkgs:
        out += ["-p", p]
    return out


# Set by the workflow on the jobs that measure coverage: each measured check
# writes `<dir>/<area>/lcov.info` for CI Gate (scripts/coverage_gate.py and
# docs/contributing/ci.md#coverage), and its JUnit report as
# `<dir>/<area>/junit.xml` for Codecov Test Analytics. Unset, checks run
# without either.
COVERAGE_DIR_ENV = "CI_COVERAGE_DIR"
# Which part of the split `test` job runs (`<part>/<parts>`); see test_part().
TEST_PART_ENV = "CI_TEST_PART"
# How many parts the `test` job splits a whole-workspace `rust_test` run
# into. Each part compiles every test binary and runs every third test
# (`--partition count:<part>/<parts>`), so the job takes about a third of
# the time for two more compiles. A selective run of a few packages stays
# whole: its tests are too few to repay the extra compiles.
TEST_PARTS = 3
# Set by the workflow on the jobs that build the web UI and the Workers: each
# writes its built bundle to `<dir>/<bundle>/` for Codecov Bundle Analysis
# (docs/contributing/ci.md#bundle-sizes). Unset, nothing is kept.
BUNDLE_DIR_ENV = "CI_BUNDLE_DIR"
# The nextest profile (.config/nextest.toml) that writes a JUnit report, and
# where it writes it, relative to the workspace. The workflow copies the
# report into the coverage directory even when tests fail.
NEXTEST_JUNIT_PROFILE = "ci"
NEXTEST_JUNIT_DIR = "target/nextest/ci"


def _db_testkit(ctx: Context) -> tuple[Command, dict[str, str]]:
    """The `bookclerk-db-testkit` executable the SDK test wrappers spawn.

    Returns the build command and the environment that names the built
    executable (`BOOKCLERK_DB_TESTKIT`) for the commands that run the wrappers.
    """
    exe = "bookclerk-db-testkit" + (".exe" if ctx.os_name == "Windows" else "")
    build = Command(
        "cargo build -p bookclerk-db-testkit",
        ["cargo", "build", "-p", "bookclerk-db-testkit", "--bin", "bookclerk-db-testkit"],
    )
    return build, {"BOOKCLERK_DB_TESTKIT": str(ctx.bin_dir / exe)}


def _coverage_report(area: str, part: tuple[int, int] | None = None) -> Path | None:
    """Where ``area``'s LCOV report goes (its directory created), or ``None`` without coverage.

    One part of a split test job writes under ``<area>/part-<n>/``, which
    the coverage gate reads together with the other parts.
    """
    root = os.environ.get(COVERAGE_DIR_ENV)
    if not root:
        return None
    report = Path(root) / area / "lcov.info"
    if part is not None:
        report = Path(root) / area / f"part-{part[0]}" / "lcov.info"
    report.parent.mkdir(parents=True, exist_ok=True)
    return report


def test_part() -> tuple[int, int] | None:
    """Which part of a split ``test`` job this is, from ``CI_TEST_PART``.

    Returns:
        ``(part, parts)`` for a value ``<part>/<parts>`` with
        ``1 <= part <= parts <= 9``; ``None`` when the variable is unset or
        empty (the whole suite in one job).

    Raises:
        ExecError: The variable is set to anything else, so a typo never
            silently runs every test in every part, or none.
    """
    raw = os.environ.get(TEST_PART_ENV, "")
    if not raw:
        return None
    match = re.fullmatch(r"([1-9])/([1-9])", raw)
    if match is None or int(match.group(1)) > int(match.group(2)):
        raise ExecError(f"{TEST_PART_ENV} must be <part>/<parts> with 1 <= part <= parts <= 9, not {raw!r}")
    return int(match.group(1)), int(match.group(2))


def workerd_guest_dirs(workspace: Path) -> list[Path]:
    """First-party workerd guests that carry their own TypeScript build.

    A guest qualifies when it sits under ``crates/bookclerk-plugins/<tier>/``,
    its ``plugin.toml`` says ``runtime = "workerd"`` and it has a
    ``package.json``.

    Args:
        workspace: The repository root.

    Returns:
        The guest directories, sorted.
    """
    out: list[Path] = []
    for manifest in sorted((workspace / "crates" / "bookclerk-plugins").glob("*/*/plugin.toml")):
        try:
            runtime = tomllib.loads(manifest.read_text(encoding="utf-8")).get("runtime")
        except (OSError, tomllib.TOMLDecodeError) as exc:
            raise ExecError(f"cannot parse {manifest}: {exc}") from exc
        if runtime == "workerd" and (manifest.parent / "package.json").is_file():
            out.append(manifest.parent)
    return out


# Removes a guest's emitted `modules/` before the build, so a module whose
# source was deleted does not survive the rebuild unnoticed.
_CLEAR_MODULES = "import shutil; shutil.rmtree('modules', ignore_errors=True)"

# Fails when the rebuilt `modules/` differs from the committed one in any way:
# a changed or deleted tracked file, or a new untracked one.
_MODULES_DRIFT = (
    "import subprocess, sys; "
    "out = subprocess.run(['git', 'status', '--porcelain', '--untracked-files=all', '--', 'modules'], "
    "capture_output=True, text=True, check=True).stdout; "
    "print(out, end=''); sys.exit(1 if out else 0)"
)


def _workerd_guest_commands(ctx: "Context") -> list[Command]:
    """Type-check, rebuild and test every first-party workerd guest.

    The guests type-check against the TypeScript SDK, so it is built first.
    Each guest's shipped ``modules/`` is emitted from ``src/`` by its build;
    a committed tree that differs from a fresh build is drift. The unit tests
    of all guests run in one ``node --test`` from the repository root, so one
    LCOV report covers them.

    Args:
        ctx: Paths and platform facts.

    Returns:
        The commands, in order.
    """
    sdk = str(ctx.workspace / "packages" / "plugin-sdk")
    cmds = [
        Command("npm ci (plugin-sdk)", ["npm", "ci"], cwd=sdk),
        Command("npm run build (plugin-sdk)", ["npm", "run", "build"], cwd=sdk),
    ]
    dirs = workerd_guest_dirs(ctx.workspace)
    for guest in dirs:
        rel = guest.relative_to(ctx.workspace).as_posix()
        cmds += [
            Command(f"npm ci ({rel})", ["npm", "ci"], cwd=str(guest)),
            Command(f"tsc ({rel})", ["npm", "run", "typecheck"], cwd=str(guest)),
            Command(f"clear modules ({rel})", [sys.executable, "-c", _CLEAR_MODULES], cwd=str(guest)),
            Command(f"npm run build ({rel})", ["npm", "run", "build"], cwd=str(guest)),
            Command(f"modules drift ({rel})", [sys.executable, "-c", _MODULES_DRIFT], cwd=str(guest)),
        ]
    if not dirs:
        return cmds
    report = _coverage_report("workerd-guests")
    reporters = ["--test-reporter=spec", "--test-reporter-destination=stdout"]
    if report:
        reporters += [
            "--test-reporter=lcov",
            f"--test-reporter-destination={report}",
            "--test-reporter=junit",
            f"--test-reporter-destination={report.parent / 'junit.xml'}",
        ]
    tests = [f"{guest.relative_to(ctx.workspace).as_posix()}/test/**/*.test.ts" for guest in dirs]
    cmds.append(
        Command(
            "node --test (workerd guests)",
            [
                "node",
                "--test",
                "--experimental-test-module-mocks",
                "--experimental-test-coverage",
                "--test-coverage-include=crates/bookclerk-plugins/*/*/src/**",
                *reporters,
                *tests,
            ],
            cwd=str(ctx.workspace),
        )
    )
    return cmds


def plugin_example_dirs(workspace: Path) -> list[tuple[Path, str]]:
    """Reference examples whose suites open a database through a test kit.

    An example qualifies when ``examples/plugins-*/plugin.toml`` declares a
    ``[[databases]]`` binding and ``bookclerk-plugin test`` finds a runner
    for it (``plugin_test_runner``), the same rule the planner selects by.

    Args:
        workspace: The repository root.

    Returns:
        Each example directory with its runner (``npm``, ``pytest`` or
        ``cargo``), sorted by directory.

    Raises:
        ExecError: When a manifest cannot be parsed.
    """
    out: list[tuple[Path, str]] = []
    for manifest in sorted((workspace / "examples").glob("plugins-*/plugin.toml")):
        try:
            data = tomllib.loads(manifest.read_text(encoding="utf-8"))
            runner = plugin_test_runner(manifest.parent, data)
        except (OSError, tomllib.TOMLDecodeError, PlanError) as exc:
            raise ExecError(f"cannot parse {manifest}: {exc}") from exc
        if runner is not None:
            out.append((manifest.parent, runner))
    return out


def _plugin_example_commands(ctx: "Context", runners: list[str]) -> list[Command]:
    """Check, rebuild and run every database example under each profile.

    TypeScript (``npm``) examples build against the TypeScript SDK, so it is
    built first. Each one's shipped ``modules/`` is emitted from ``src/`` by
    its build, and a committed tree that differs is drift, as for the
    first-party guests. Python (``pytest``) examples ship ``modules/`` as
    written; they are linted with the Python SDK's ruff and run in its
    locked environment with the SDK imported from its source tree. Rust
    (``cargo``) examples are workspace packages that ``fmt`` and ``clippy``
    already cover, so they only run.
    ``bookclerk-plugin test`` then runs each example's own suite once per
    database profile with the kit executable named.

    Only examples whose runner the plan recorded run, so the commands never
    need a tool (Node, uv) the job did not install for them.

    Args:
        ctx: Paths and platform facts.
        runners: The runners the plan recorded (:func:`_example_runners`).

    Returns:
        The commands, in order.
    """
    examples = [(d, r) for d, r in plugin_example_dirs(ctx.workspace) if r in runners]
    used = {runner for _, runner in examples}
    sdk = str(ctx.workspace / "packages" / "plugin-sdk")
    kit_build, kit_env = _db_testkit(ctx)
    tool = str(ctx.bin_dir / ("bookclerk-plugin" + (".exe" if ctx.os_name == "Windows" else "")))
    venv = ctx.tmp / "venv-plugin-sdk"
    venv_bin = venv / ("Scripts" if ctx.os_name == "Windows" else "bin")
    py = str(venv_bin / "python")
    cmds: list[Command] = []
    if "npm" in used:
        cmds += [
            Command("npm ci (plugin-sdk)", ["npm", "ci"], cwd=sdk),
            Command("npm run build (plugin-sdk)", ["npm", "run", "build"], cwd=sdk),
        ]
    if "pytest" in used:
        # The same locked environment as the `python_sdk` check: pytest and
        # ruff at the versions in uv.lock, the SDK itself from its sources.
        cmds.append(
            Command(
                "uv sync plugin-sdk-python",
                [
                    "uv",
                    "sync",
                    "--quiet",
                    "--project",
                    "packages/plugin-sdk-python",
                    "--locked",
                    "--all-extras",
                    "--no-install-project",
                    "--python",
                    sys.executable,
                ],
                env={"UV_PROJECT_ENVIRONMENT": str(venv)},
            )
        )
    cmds += [
        kit_build,
        Command(
            "cargo build -p bookclerk-plugin-tools",
            ["cargo", "build", "-p", "bookclerk-plugin-tools", "--bin", "bookclerk-plugin"],
        ),
    ]
    # `bookclerk-plugin test` runs `python3 -m pytest`, so the environment's
    # interpreter comes first on PATH.
    pytest_env = {
        **kit_env,
        "PATH": os.pathsep.join([str(venv_bin), os.environ.get("PATH", "")]),
        "PYTHONPATH": str(ctx.workspace / "packages" / "plugin-sdk-python" / "src"),
    }
    for example, runner in examples:
        rel = example.relative_to(ctx.workspace).as_posix()
        if runner == "pytest":
            cmds += [
                Command(f"ruff ({rel})", [py, "-m", "ruff", "check", rel]),
                Command(f"ruff format ({rel})", [py, "-m", "ruff", "format", "--check", rel]),
                Command(f"bookclerk-plugin test ({rel})", [tool, "test", rel], env=pytest_env),
            ]
            continue
        if runner == "cargo":
            cmds.append(Command(f"bookclerk-plugin test ({rel})", [tool, "test", rel], env=kit_env))
            continue
        cmds += [
            Command(f"npm ci ({rel})", ["npm", "ci"], cwd=str(example)),
            Command(f"tsc ({rel})", ["npm", "run", "typecheck"], cwd=str(example)),
            Command(f"clear modules ({rel})", [sys.executable, "-c", _CLEAR_MODULES], cwd=str(example)),
            Command(f"npm run build ({rel})", ["npm", "run", "build"], cwd=str(example)),
            Command(f"modules drift ({rel})", [sys.executable, "-c", _MODULES_DRIFT], cwd=str(example)),
            Command(f"bookclerk-plugin test ({rel})", [tool, "test", rel], env=kit_env),
        ]
    return cmds


def _bundle_dir(bundle: str) -> Path | None:
    """Where ``bundle``'s built files go, or ``None`` when bundles are not kept."""
    root = os.environ.get(BUNDLE_DIR_ENV)
    return Path(root) / bundle if root else None


def _worker_bundle(bundle: str, path: str, cwd: str, *extra: str) -> list[Command]:
    """Bundle the Worker at ``path`` the way a deploy would, without deploying.

    ``wrangler deploy --dry-run`` writes the bundled script to the bundle's
    directory; it needs no Cloudflare account. Empty when bundles are not kept.
    """
    out = _bundle_dir(bundle)
    if out is None:
        return []
    argv = ["npx", "--no-install", "wrangler", "deploy", "--dry-run", *extra, "--outdir", str(out)]
    return [Command(f"bundle ({path})", argv, cwd=cwd, env={"WRANGLER_SEND_METRICS": "false"})]


# Set by the workflow on the Worker checks: each profiles its Worker's startup
# with scripts/worker_startup.py (docs/contributing/ci.md#worker-startup-time).
# Unset, nothing is profiled.
WORKER_STARTUP_ENV = "CI_WORKER_STARTUP"


def _worker_startup(worker: str) -> list[Command]:
    """Profile ``worker``'s startup against its budget, or nothing when not asked to."""
    if not os.environ.get(WORKER_STARTUP_ENV):
        return []
    return [Command(f"startup ({worker})", ["python3", "scripts/worker_startup.py", "profile", "--worker", worker])]


def _copy_bundle(bundle: str, built: Path) -> list[Command]:
    """Keep an already built bundle, or nothing when bundles are not kept."""
    out = _bundle_dir(bundle)
    return [] if out is None else [Command(f"keep the {bundle} bundle", ["<copytree>", str(built), str(out)])]


def _worker_test(area: str, path: str, cwd: str) -> Command:
    """``npm test`` for the Worker at ``path``, as ``npm run coverage`` when measuring.

    The coverage run is the same Vitest suite with V8 coverage; only the LCOV
    reporter is kept, writing into the Worker's coverage area, and a JUnit
    report goes beside it.
    """
    report = _coverage_report(area)
    if report is None:
        return Command(f"npm test ({path})", ["npm", "test"], cwd=cwd)
    reporters = ["--coverage.reporter=text-summary", "--coverage.reporter=lcovonly"]
    junit = ["--reporter=default", "--reporter=junit", f"--outputFile.junit={report.parent / 'junit.xml'}"]
    return Command(
        f"npm run coverage ({path})",
        ["npm", "run", "coverage", "--", *reporters, f"--coverage.reportsDirectory={report.parent}", *junit],
        cwd=cwd,
    )


def _nextest_filter(custom: Sequence[tuple[str, str]]) -> list[str]:
    """``-E`` filterset excluding custom-harness test binaries (run separately)."""
    if not custom:
        return []
    ids = " | ".join(f"binary_id({pkg}::{target})" for pkg, target in custom)
    return ["-E", f"not ({ids})"]


def check_runtime_env(plan: Plan, check: str, ctx: Context) -> dict[str, str]:
    """Environment a check's commands run with once its prerequisites are built.

    Args:
        plan: The execution plan.
        check: The check.
        ctx: Paths of this job.

    Returns:
        ``PATH`` with the workspace's built helpers first, and
        ``BOOKCLERK_WORKERD_BIN`` when the check's prerequisites fetch the
        pinned workerd.
    """
    env = {"PATH": ctx.path_with_bins()}
    if any(p["kind"] == "ensure_workerd" for p in plan.prereqs(check)):
        env["BOOKCLERK_WORKERD_BIN"] = str(ctx.bin_dir / ("workerd.exe" if ctx.os_name == "Windows" else "workerd"))
    return env


def check_commands(check: str, plan: Plan, ctx: Context, *, event: str = "", commit: str = "") -> list[Command]:
    """Commands for ``check`` (prerequisites excluded).

    ``event`` is the GitHub event the plan was resolved for; merge-group runs
    retry a failed ``rust_test`` test once. ``commit`` is the checked-out
    commit, which the website build stamps on its pages.
    """
    params = plan.params(check)
    ws = str(ctx.workspace)
    runtime_env = check_runtime_env(plan, check, ctx)

    if check == "fmt":
        return [Command("rustfmt", ["cargo", "fmt", "--all", "--", "--check"])]
    if check == "clippy":
        scope = ["--workspace"] if params.get("workspace") else _pkg_args(params["packages"])
        return [Command("clippy", ["cargo", "clippy", *scope, "--all-targets", "--", *CLIPPY_LINTS])]
    if check == "clippy_publish":
        return [
            Command(
                "clippy publish-crate doc lints",
                [
                    "cargo",
                    "clippy",
                    *PUBLISH_CRATES_ARGS,
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                    "-D",
                    "clippy::missing_errors_doc",
                    "-D",
                    "clippy::missing_panics_doc",
                ],
                env={"CLIPPY_CONF_DIR": str(ctx.workspace / "clippy-publish")},
            )
        ]
    if check == "author_surface":
        return [Command("author surface", ["./scripts/check-author-surface.sh"])]
    if check == "plugin_sdk_abi":
        sdk = str(ctx.workspace / "packages" / "plugin-sdk")
        kit_build, kit_env = _db_testkit(ctx)
        cmds = [
            Command("ABI schema drift", ["python3", "scripts/gen-plugin-abi.py", "--check"]),
            Command("workerd pin sync", ["python3", "scripts/sync-workerd-pin.py", "--check"]),
            Command("npm ci (plugin-sdk)", ["npm", "ci"], cwd=sdk),
            Command("npm run build (plugin-sdk)", ["npm", "run", "build"], cwd=sdk),
            # The isolate embed is bundled from src/; a stale committed bundle is drift.
            Command("embed drift", ["git", "diff", "--exit-code", "--", "embed/bookclerk_plugin.js"], cwd=sdk),
        ]
        for script in (
            "lint",
            "test:tools",
            "test:path-security",
            "test:db-value",
            "test:db-stream",
            "test:db-adapter",
            "test:plugin-migrations",
            "test:entrypoints",
            "test:entrypoints-codec",
            "test:egress-user-agent",
        ):
            cmds.append(Command(f"npm run {script}", ["npm", "run", script], cwd=sdk))
        # `createTestDatabase` spawns the kit executable.
        cmds.append(kit_build)
        cmds.append(Command("npm run test:testkit", ["npm", "run", "test:testkit"], cwd=sdk, env=kit_env))
        return cmds
    if check == "python_sdk":
        kit_build, kit_env = _db_testkit(ctx)
        venv = ctx.tmp / "venv-plugin-sdk"
        py = str(venv / ("Scripts" if ctx.os_name == "Windows" else "bin") / "python")
        # Tests import the SDK from its source tree.
        src = {"PYTHONPATH": str(ctx.workspace / "packages" / "plugin-sdk-python" / "src")}
        report = _coverage_report("python-sdk")
        cov = []
        if report:
            cov = [
                "--cov=packages/plugin-sdk-python/src",
                f"--cov-report=lcov:{report}",
                f"--junitxml={report.parent / 'junit.xml'}",
                "-o",
                "junit_family=legacy",
            ]
        return [
            # Installs the dev/docs extras at the versions in uv.lock (the
            # lockfile OSV scans); `--locked` fails when the lockfile no longer
            # matches pyproject.toml. `--no-install-project` skips building the
            # SDK itself, whose build backend would otherwise be resolved
            # outside the lockfile. Quiet: a sync lists every installed version,
            # which would show an unpublished OSV remediation bump in the
            # public log.
            Command(
                "uv sync plugin-sdk-python",
                [
                    "uv",
                    "sync",
                    "--quiet",
                    "--project",
                    "packages/plugin-sdk-python",
                    "--locked",
                    "--all-extras",
                    "--no-install-project",
                    "--python",
                    sys.executable,
                ],
                env={"UV_PROJECT_ENVIRONMENT": str(venv)},
            ),
            Command("ruff", [py, "-m", "ruff", "check", "packages/plugin-sdk-python"]),
            Command("ruff format", [py, "-m", "ruff", "format", "--check", "packages/plugin-sdk-python"]),
            # `create_test_database` spawns the kit executable.
            kit_build,
            Command(
                "pytest",
                [py, "-m", "pytest", "packages/plugin-sdk-python/tests", "-q", *cov],
                env={**src, **kit_env},
            ),
        ]
    if check == "workerd_guests":
        return _workerd_guest_commands(ctx)
    if check == "plugin_examples":
        return _plugin_example_commands(ctx, _example_runners(plan))
    if check == "site_test":
        site = str(ctx.workspace / "tools" / "site")
        return [
            Command("npm ci (tools/site)", ["npm", "ci"], cwd=site),
            Command("tsc (tools/site)", ["npm", "run", "typecheck"], cwd=site),
            Command("oxlint (tools/site)", ["npm", "run", "lint"], cwd=site),
            _worker_test("site", "tools/site", site),
        ]
    if check == "site_audit":
        site = ctx.workspace / "tools" / "site"
        return [
            Command("npm ci (tools/site)", ["npm", "ci"], cwd=str(site)),
            Command("npm run build (tools/site)", ["npm", "run", "build"], cwd=str(site), env={"SITE_COMMIT": commit}),
            Command("npm ci (tools/site/audit)", ["npm", "ci"], cwd=str(site / "audit")),
            Command("tsc (tools/site/audit)", ["npm", "run", "typecheck"], cwd=str(site / "audit")),
            Command("npm test (tools/site/audit)", ["npm", "test"], cwd=str(site / "audit")),
            Command(
                "Lighthouse + axe-core (tools/site/audit)",
                ["node", "audit.ts", "--out", str(ctx.tmp / "site-audit")],
                cwd=str(site / "audit"),
            ),
            # After the build: the Worker imports what it generates.
            *_worker_bundle("site", "tools/site", str(site)),
            *_worker_startup("site"),
        ]
    if check == "collector_test":
        collector = str(ctx.workspace / "tools" / "diagnostics-collector")
        return [
            Command("npm ci (tools/diagnostics-collector)", ["npm", "ci"], cwd=collector),
            Command("tsc (tools/diagnostics-collector)", ["npm", "run", "typecheck"], cwd=collector),
            Command("oxlint (tools/diagnostics-collector)", ["npm", "run", "lint"], cwd=collector),
            _worker_test("collector", "tools/diagnostics-collector", collector),
            *_worker_bundle("collector", "tools/diagnostics-collector", collector),
            *_worker_startup("collector"),
        ]
    if check == "demo_test":
        demo = str(ctx.workspace / "tools" / "demo")
        return [
            Command("npm ci (tools/demo)", ["npm", "ci"], cwd=demo),
            Command("tsc (tools/demo)", ["npm", "run", "typecheck"], cwd=demo),
            Command("oxlint (tools/demo)", ["npm", "run", "lint"], cwd=demo),
            _worker_test("demo", "tools/demo", demo),
            # The container image is not built: only the Worker's script counts.
            *_worker_bundle("demo", "tools/demo", demo, "--containers-rollout=none"),
            *_worker_startup("demo"),
        ]
    if check == "lint":
        # Repository-wide: Prettier formatting and markdownlint, both run from
        # the root so their root configuration and ignore files apply.
        lint = ctx.workspace / "tools" / "lint" / "node_modules"
        return [
            Command("npm ci (tools/lint)", ["npm", "ci"], cwd=str(ctx.workspace / "tools" / "lint")),
            Command("prettier --check", ["node", str(lint / "prettier" / "bin" / "prettier.cjs"), "--check", "."]),
            Command("markdownlint", ["node", str(lint / "markdownlint-cli2" / "markdownlint-cli2-bin.mjs")]),
        ]
    if check == "ui":
        ui = str(ctx.workspace / "ui")
        return [
            Command("npm ci (ui)", ["npm", "ci"], cwd=ui),
            Command("npm run lint (ui)", ["npm", "run", "lint"], cwd=ui),
            Command("npm run test:safe-html (ui)", ["npm", "run", "test:safe-html"], cwd=ui),
            Command("npm test (ui)", ["npm", "test"], cwd=ui),
            Command("npm run build (ui)", ["npm", "run", "build"], cwd=ui),
            *_copy_bundle("ui", ctx.workspace / "ui" / "dist"),
        ]
    if check == "api_docs":
        if params.get("all"):
            return [Command("API docs (all)", ["./scripts/generate-api-docs.sh", "--check", "--all"])]
        argv = ["./scripts/generate-api-docs.sh", "--check"]
        for p in params.get("rust_packages", []):
            argv += ["--rust-package", p]
        if params.get("typescript"):
            argv.append("--typescript-sdk")
        if params.get("ui"):
            argv.append("--ui")
        if params.get("python"):
            argv.append("--python")
        return [Command("API docs", argv)]
    if check == "doctest":
        scope = ["--workspace"] if params.get("workspace") else _pkg_args(params["packages"])
        return [Command("doctests", ["cargo", "test", *scope, "--doc", "--all-features"])]
    if check == "store_free":
        return [
            Command("plugin host without in-process sources", ["cargo", "check", "-p", "bookclerk-plugin-host", "--no-default-features"]),
            Command(
                "store-free hosts",
                ["cargo", "clippy", "-p", "bookclerk-cli", "-p", "bookclerkd", "--no-default-features", "--all-targets", "--", "-D", "warnings"],
            ),
            Command("store-free host scan", ["./scripts/check-store-free-hosts.sh"]),
            Command("plugin architecture scan", ["./scripts/check-plugin-architecture.sh"]),
        ]
    if check == "rust_test":
        if os.environ.get("BOOKCLERK_SKIP_WORKERD"):
            raise ExecError("CI must run the workerd contract (do not set BOOKCLERK_SKIP_WORKERD)")
        env = {**runtime_env, "BOOKCLERK_REQUIRE_TEST_GUESTS": "1"}
        failure_mode = _nextest_mode(event)
        cmds: list[Command] = []
        part = test_part()
        if part is not None and part[1] > 1 and not params.get("workspace"):
            raise ExecError(f"{TEST_PART_ENV}={part[0]}/{part[1]} splits only a whole-workspace rust_test run")
        # The first part (or the only one) also runs everything nextest
        # doesn't partition: custom harnesses, feature runs, bench and
        # example builds.
        first = part is None or part[0] == 1
        partition = ["--partition", f"count:{part[0]}/{part[1]}"] if part is not None and part[1] > 1 else []
        # When measuring, the main run also writes a JUnit report; a stale
        # one from an earlier run in the same checkout is cleared first.
        main_mode = failure_mode
        if _coverage_report("rust", part) is not None:
            cmds.append(Command("clear the JUnit report", ["<mkdir>", str(ctx.workspace / NEXTEST_JUNIT_DIR)]))
            main_mode = [*failure_mode, "--profile", NEXTEST_JUNIT_PROFILE]
        if params.get("workspace"):
            scope = ["--workspace"]
            for ex in params.get("exclude", []):
                scope += ["--exclude", ex]
            custom = [t for t in CUSTOM_HARNESS_TESTS if t[0] not in params.get("exclude", [])]
            label = "unit + integration tests (workspace)"
            if part is not None and partition:
                label = f"unit + integration tests (workspace, part {part[0]} of {part[1]})"
            cmds.append(Command(label, [*NEXTEST_RUN, *main_mode, *partition, *scope, *_nextest_filter(custom)], env=env))
        elif params.get("packages"):
            custom = [t for t in CUSTOM_HARNESS_TESTS if t[0] in params["packages"]]
            cmds.append(
                Command(
                    "unit + integration tests",
                    [*NEXTEST_RUN, *main_mode, *_pkg_args(params["packages"]), *_nextest_filter(custom)],
                    env=env,
                )
            )
        else:
            custom = []
        if not first:
            custom = []
        for pkg, target in custom:
            cmds.append(Command(f"{pkg} {target} (custom harness)", ["cargo", "test", "-p", pkg, "--test", target], env=env))
        for ft in params.get("feature_tests", []) if first else []:
            ft_custom = [t for t in CUSTOM_HARNESS_TESTS if t[0] == ft["package"]]
            cmds.append(
                Command(
                    f"{ft['package']} tests with {','.join(ft['features'])}",
                    [
                        *NEXTEST_RUN,
                        *failure_mode,
                        "-p",
                        ft["package"],
                        "--features",
                        ",".join(ft["features"]),
                        *_nextest_filter(ft_custom),
                    ],
                    env=env,
                )
            )
        report = _coverage_report("rust", part)
        if report is not None:
            # The workflow instruments the build (`cargo llvm-cov show-env`);
            # a report without it would fail on missing profiles.
            if not os.environ.get("CARGO_LLVM_COV"):
                raise ExecError(f"{COVERAGE_DIR_ENV} is set but the build is not instrumented (cargo llvm-cov show-env)")
            # `--failure-mode all`: a helper killed mid-write (a timeout, a
            # test that kills its child) leaves a corrupt profile; skip it
            # with a warning instead of losing the whole report.
            report_args = ["--lcov", "--failure-mode", "all", "--output-path", str(report)]
            cmds.append(Command("coverage report", ["cargo", "llvm-cov", "report", *report_args]))
        if params.get("benches") and first:
            cmds.append(Command("bench targets build", ["cargo", "bench", *_pkg_args(params["benches"]), "--benches", "--no-run"]))
        if params.get("examples") and first:
            cmds.append(Command("example targets build", ["cargo", "build", *_pkg_args(params["examples"]), "--examples"]))
        return cmds
    if check == "e2e":
        env = {
            **runtime_env,
            "BOOKCLERK_FILES_DIR": str(ctx.files_dir),
            "BOOKCLERK_PLUGIN_ARTIFACTS": str(ctx.artifacts),
            "BOOKCLERK_REQUIRE_STAGED_PLUGINS": "1",
        }
        scope = params["scope"]
        if scope != "full":
            env["BOOKCLERK_STAGED_PLUGINS"] = ",".join(scope)
        label = "staged e2e (full)" if scope == "full" else "staged e2e (" + ", ".join(scope) + ")"
        return [Command(label, ["cargo", "test", "-p", E2E_PACKAGE, *E2E_STAGED_TARGETS], env=env)]
    if check == "release":
        if params.get("mode") == "full":
            cmds = [Command("release build (platform)", ["cargo", "build-app", "--release", "--platform"])]
            exe = ".exe" if ctx.os_name == "Windows" else ""
            rel = ctx.workspace / "target" / "release"
            for name in ("bookclerk-media-worker", "bookclerk-jail", "bookclerk-workerd", "workerd"):
                cmds.append(Command(f"helper present: {name}", ["<assert-exec>", str(rel / f"{name}{exe}")]))
            cmds.append(Command("workerd.version present", ["<assert-file>", str(rel / "workerd.version")]))
            return cmds
        return [Command("release build (affected)", ["cargo", "build", "--release", *_pkg_args(params["packages"])])]
    if check == "confinement":
        env: dict[str, str] = {"BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT": "1"}
        test = ["cargo", "test", *_pkg_args(CONFINEMENT_PACKAGES)]
        # `--nocapture` keeps cgroup skip and enforcement lines in the job log.
        if ctx.os_name == "Windows":
            test += ["--", "--test-threads=1", "--nocapture"]
        else:
            env["BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT"] = "1"
            test += ["--", "--nocapture"]
        return [
            Command("clippy (confinement)", ["cargo", "clippy", *_pkg_args(CONFINEMENT_PACKAGES), "--all-targets", "--", "-D", "warnings"]),
            Command("confinement tests", test, env=env),
        ]
    if check == "native_gateway":
        cmds = []
        if ctx.os_name == "Windows":
            cmds += [
                Command("clippy bookclerk-workerd", ["cargo", "clippy", "-p", "bookclerk-workerd", "--all-targets", "--", "-D", "warnings"]),
                Command(
                    "clippy bookclerk-plugin-sdk (http)",
                    ["cargo", "clippy", "-p", "bookclerk-plugin-sdk", "--features", "http", "--all-targets", "--", "-D", "warnings"],
                ),
                Command("clippy bookclerk-plugin-host --lib", ["cargo", "clippy", "-p", "bookclerk-plugin-host", "--lib", "--", "-D", "warnings"]),
                Command(
                    "plugin-host ACL journal lib tests",
                    [
                        "cargo",
                        "test",
                        "-p",
                        "bookclerk-plugin-host",
                        "--lib",
                        "--",
                        "spawn_stdio::tests",
                        "acl_journal_",
                        "--test-threads=1",
                        "--nocapture",
                    ],
                ),
                Command("named-pipe SOCKET_PROXY", ["cargo", "test", "-p", "bookclerk-workerd", "--lib"]),
                # Gateway clippy does not run bookclerk-storage. This is the
                # Windows FlushFileBuffers path for a writable stage journal.
                Command(
                    "Windows stage journal transfer",
                    [
                        "cargo",
                        "test",
                        "-p",
                        "bookclerk-storage",
                        "--lib",
                        "--",
                        "transfer::tests::writable_stage_journal_completes_a_transfer",
                        "--exact",
                        "--nocapture",
                        "--test-threads=1",
                    ],
                ),
                # Alone, before the suite. The suite still runs this test in
                # its original order (shared serialization lock with Job extras).
                Command(
                    "lifecycle churn alone",
                    [
                        "cargo",
                        "test",
                        "-p",
                        E2E_PACKAGE,
                        "--test",
                        "native_gateway_lifecycle",
                        "--",
                        "sequential_and_concurrent_spawn_cycles_do_not_leak",
                        "--exact",
                        "--nocapture",
                        "--test-threads=1",
                    ],
                    env=runtime_env,
                ),
            ]
        elif ctx.os_name == "Darwin":
            cmds.append(Command("bookclerk-workerd lib tests", ["cargo", "test", "-p", "bookclerk-workerd", "--lib"]))
        smoke = ["cargo", "test", "-p", E2E_PACKAGE]
        for target in NATIVE_GATEWAY_TESTS:
            smoke.extend(["--test", target])
        smoke.extend(["--", "--nocapture"])
        cmds.append(
            Command(
                "native-behind-workerd gateway smoke",
                smoke,
                env=runtime_env,
            )
        )
        return cmds
    if check == "windows_cluster":
        # Guests are prerequisites. The CLI target keeps isolation = "required"
        # and its own assertions; identity filters are the existing library tests.
        guest_env = {**runtime_env, "BOOKCLERK_REQUIRE_TEST_GUESTS": "1"}
        identity = (
            ("node identity create", "node_identity_is_stable_and_distinct"),
            ("node identity restart and binding", "restart_keeps_node_id_and_distinct_nodes_stay_distinct"),
            ("copied identity rejects a different cluster", "copied_identity_rejects_a_different_cluster"),
        )
        cmds = [
            Command(
                label,
                ["cargo", "test", "-p", "bookclerk-library", "--lib", filt, "--", "--nocapture"],
            )
            for label, filt in identity
        ]
        cmds.append(
            Command(
                "cluster_secret_startup",
                [
                    "cargo",
                    "test",
                    "-p",
                    "bookclerk-cli",
                    "--test",
                    "cluster_secret_startup",
                    "--",
                    "--nocapture",
                ],
                env=guest_env,
            )
        )
        return cmds
    if check == "tray":
        return [
            Command("clippy bookclerk-tray", ["cargo", "clippy", "-p", "bookclerk-tray", "--all-targets", "--", "-D", "warnings"]),
            Command("test bookclerk-tray", ["cargo", "test", "-p", "bookclerk-tray"]),
        ]
    if check == "envelope":
        # Compile on the runner. The container is only the seed, daemon, and load.
        files = str(ctx.files_dir / "envelope")
        release = ctx.workspace / "target" / "release"
        env = {"BOOKCLERK_FILES_DIR": files}
        return [
            Command(
                "release platform (hosts, helpers, sqlite guest)",
                ["cargo", "install-platform", "--release"],
                env=env,
            ),
            Command(
                "compile envelope seed",
                [
                    "cargo",
                    "test",
                    "--release",
                    "-p",
                    "bookclerk-library",
                    "--test",
                    "envelope_seed",
                    "--no-run",
                ],
                env=env,
            ),
            Command(
                "1 vCPU / 1 GiB envelope",
                ["bash", "scripts/bench/envelope-container.sh"],
                env={
                    **env,
                    "BOOKCLERK_DAEMON_BIN": str(release / "bookclerkd"),
                    "BOOKCLERK_CLI_BIN": str(release / "bookclerk"),
                    "ENVELOPE_LABEL": "ci",
                },
            ),
        ]
    if check == "postgres":
        cmds = []
        for step in params.get("steps", []):
            label, argv = POSTGRES_STEPS[step]
            env = dict(runtime_env) if step == "rpc_like" else {}
            cmds.append(Command(label, list(argv), env=env))
        return cmds
    raise ExecError(f"no commands for check `{check}`")


def execution_plan(artifact: Mapping[str, Any]) -> Plan:
    return plan_from_dict(artifact["execution"])


def planned_commands(
    artifact: Mapping[str, Any], check: str, ctx: Context, *, saved_cache_current: bool = False
) -> list[tuple[str, Command]]:
    """``(key, command)`` pairs: prerequisites first, then the check.

    ``saved_cache_current`` (the job's cache key is already saved) empties a
    build-only run's list, since its builds exist only to save that cache; it
    has no effect on any other run.
    """
    plan = execution_plan(artifact)
    if check not in CHECK_JOB:
        raise ExecError(f"unknown check `{check}`")
    if not plan.selected(check):
        raise ExecError(f"check `{check}` is not in this run's execution plan")
    build_only = bool(artifact.get("build_only"))
    if build_only and saved_cache_current:
        return []
    out: list[tuple[str, Command]] = []
    for prereq in plan.prereqs(check):
        if build_only and prereq["kind"] not in BUILD_PREREQS:
            continue
        key_src = json.dumps({k: v for k, v in prereq.items() if k != "why"}, sort_keys=True)
        key = hashlib.sha256(key_src.encode()).hexdigest()[:16]
        for i, cmd in enumerate(prereq_commands(prereq, ctx)):
            out.append((f"{prereq['kind']}-{key}-{i}", cmd))
    for cmd in check_commands(
        check, plan, ctx, event=str(artifact.get("event", "")), commit=str(artifact.get("checkout_sha", ""))
    ):
        if build_only:
            built = build_only_command(cmd)
            if built is not None:
                out.append(("", built))
        else:
            out.append(("", cmd))
    return out


def build_only_command(cmd: Command) -> Command | None:
    """``cmd`` reduced to compiling, or ``None`` when it only runs or lints.

    Builds, Clippy, installs and ``cargo bench --no-run`` keep running (they
    produce the cached artifacts); test commands compile their targets with
    ``--no-run``;
    doctests, formatting, scripts and runtime assertions are dropped.
    """
    argv = cmd.argv
    if argv[:3] == ["cargo", "nextest", "run"]:
        # Same targets and profile as nextest's own build step.
        scope: list[str] = []
        i = 3
        while i < len(argv):
            if argv[i] in ("--failure-output", "--no-tests", "--retries", "--profile", "-E"):
                i += 2
                continue
            if argv[i] not in ("--no-fail-fast", "--fail-fast"):
                scope.append(argv[i])
            i += 1
        return Command(cmd.label + " (build only)", ["cargo", "test", *scope, "--no-run"], cmd.cwd, cmd.env)
    if argv[:2] == ["cargo", "test"]:
        cargo_args = argv[2 : argv.index("--")] if "--" in argv else argv[2:]
        if "--doc" in cargo_args:
            return None
        no_run = [] if "--no-run" in cargo_args else ["--no-run"]
        return Command(cmd.label + " (build only)", ["cargo", "test", *cargo_args, *no_run], cmd.cwd, cmd.env)
    if argv[:2] == ["cargo", "bench"] and "--no-run" in argv:
        return cmd
    if argv[0] == "cargo" and len(argv) > 1 and argv[1] in BUILD_SUBCOMMANDS:
        return cmd
    return None


def _run_one(cmd: Command, ctx: Context) -> None:
    head = cmd.argv[0]
    if head == "<mkdir>":
        target = Path(cmd.argv[1])
        if target.exists():
            shutil.rmtree(target)
        target.mkdir(parents=True)
        return
    if head == "<copytree>":
        target = Path(cmd.argv[2])
        if target.exists():
            shutil.rmtree(target)
        shutil.copytree(cmd.argv[1], target)
        return
    if head in ("<assert-exec>", "<assert-file>"):
        target = Path(cmd.argv[1])
        ok = target.is_file() and (head == "<assert-file>" or os.access(target, os.X_OK))
        if not ok:
            raise ExecError(f"{cmd.label}: missing {target}")
        return
    env = {**os.environ, **cmd.env}
    try:
        proc = subprocess.run(cmd.argv, cwd=cmd.cwd or str(ctx.workspace), env=env)
    except FileNotFoundError as err:
        raise ExecError(f"{cmd.label}: `{head}` is not installed or not on PATH") from err
    if proc.returncode != 0:
        raise ExecError(f"{cmd.label}: `{' '.join(cmd.argv)}` exited {proc.returncode}")


def run_check(
    artifact: Mapping[str, Any],
    check: str,
    ctx: Context,
    *,
    dry_run: bool = False,
    saved_cache_current: bool = False,
    prereqs_only: bool = False,
) -> None:
    """Runs ``check`` with its prerequisites (each prerequisite once per execution).

    ``saved_cache_current`` is passed to :func:`planned_commands`.
    ``prereqs_only`` runs the prerequisites and not the check, for a job that
    runs the check's tests its own way (the mutation testing workflow).
    """
    # Validated before any path join. Checks that share this artifact share
    # the directory; a new resolve gets a new id and cannot see these markers.
    markers = ctx.tmp / "ci-exec-prereqs" / require_execution_id(artifact)
    commands = planned_commands(artifact, check, ctx, saved_cache_current=saved_cache_current)
    if not commands and artifact.get("build_only") and saved_cache_current:
        print(f"::notice::{check}: the saved dependency cache is current; nothing to build")
    if prereqs_only:
        commands = [(key, cmd) for key, cmd in commands if key]
    for key, cmd in commands:
        env_note = " ".join(f"{k}={v}" for k, v in sorted(cmd.env.items()) if k != "PATH")
        line = f"{cmd.label}: {' '.join(cmd.argv)}" + (f"  [{env_note}]" if env_note else "")
        if dry_run:
            print(line)
            continue
        marker = markers / key if key else None
        if marker is not None and marker.exists():
            print(f"::notice::{cmd.label}: already done in this job")
            continue
        print(f"::group::{line}", flush=True)
        try:
            _run_one(cmd, ctx)
        finally:
            print("::endgroup::", flush=True)
        if marker is not None:
            markers.mkdir(parents=True, exist_ok=True)
            marker.write_text(line + "\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# gate


# What ``needs.<job>.result`` reports for a job that never got a runner.
STRANDED_RESULTS = frozenset({"cancelled", "abandoned"})


def expected_jobs(artifact: Mapping[str, Any]) -> dict[str, bool]:
    """Whether each job ``CI Gate`` needs is expected to run.

    Args:
        artifact: The validated plan artifact.

    Returns:
        The execution plan's jobs, plus each ``OS_LEG_JOBS`` job, expected
        when any of the jobs it runs a leg of is, except in a Linux-only
        pull request run.
    """
    expected = dict(execution_plan(artifact).jobs)
    for job, owners in OS_LEG_JOBS.items():
        expected[job] = not artifact.get("linux_only") and any(expected.get(owner, False) for owner in owners)
    return expected


def gate(
    artifact: Mapping[str, Any],
    needs: Mapping[str, Any],
    stranded: frozenset[str] = frozenset(),
) -> list[str]:
    """Problems comparing job results with the expected execution.

    Args:
        artifact: The validated plan artifact.
        needs: ``toJSON(needs)`` of the ``CI Gate`` job.
        stranded: ``OS_LEG_JOBS`` jobs that GitHub cancelled without ever
            giving them a runner, on a run that already retried them
            (``scripts/merge_queue_requeue.py --stranded-job``). Such a job
            passes the gate as ``cancelled`` or ``abandoned`` (what
            ``needs`` reports for a job no runner took); any other job, or
            any other result, does not.

    Returns:
        One sentence per problem; empty when the gate passes.
    """
    problems: list[str] = []
    plan_result = (needs.get("plan") or {}).get("result")
    if plan_result != "success":
        problems.append(f"plan: {plan_result}")
    for job in sorted(stranded - set(OS_LEG_JOBS)):
        problems.append(f"{job}: only an OS leg job may pass without a runner")
    expected = expected_jobs(artifact)
    for job, want in expected.items():
        result = (needs.get(job) or {}).get("result")
        if result is None:
            problems.append(f"{job}: not reported to the gate (missing from needs)")
        elif want and result in STRANDED_RESULTS and job in stranded and job in OS_LEG_JOBS:
            continue
        elif want and result != "success":
            problems.append(f"{job}: expected to run, result {result}")
        elif not want and result != "skipped":
            problems.append(f"{job}: expected to skip, result {result}")
    for job in needs:
        if job != "plan" and job not in expected:
            problems.append(f"{job}: gated job unknown to the planner")
    return problems


# ---------------------------------------------------------------------------
# GitHub file commands


def write_github_output(values: Mapping[str, str]) -> None:
    path = os.environ.get("GITHUB_OUTPUT")
    if not path:
        return
    with open_github_actions_append(path, label="GITHUB_OUTPUT") as fh:
        for k, v in values.items():
            fh.write(f"{k}={v}\n")


def write_github_env(values: Mapping[str, str]) -> None:
    """Append ``values`` to ``$GITHUB_ENV`` so the job's later steps see them.

    Args:
        values: Variable names and values; a value with a newline is refused,
            since it would start another variable.

    Raises:
        ExecError: ``$GITHUB_ENV`` is not set, or a value holds a newline.
    """
    path = os.environ.get("GITHUB_ENV")
    if not path:
        raise ExecError("--github-env needs $GITHUB_ENV (run inside a workflow step)")
    for k, v in values.items():
        if "\n" in v or "\r" in v or "\n" in k or "=" in k:
            raise ExecError(f"refusing to write `{k}` to GITHUB_ENV: a newline or `=` would start another variable")
    with open_github_actions_append(path, label="GITHUB_ENV") as fh:
        for k, v in values.items():
            fh.write(f"{k}={v}\n")


def write_step_summary(text: str) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        sys.stderr.write(text)
        return
    with open_github_actions_append(path, label="GITHUB_STEP_SUMMARY") as fh:
        fh.write(text if text.endswith("\n") else text + "\n")


def artifact_summary(artifact: Mapping[str, Any]) -> str:
    mode = artifact["mode"]
    banner = {
        "full": "Full suite (predicted or forced).",
        "shadow": "Shadow mode (`SELECTIVE_CI != 1`): every job runs; the selective prediction is recorded below for audit.",
        "selective": "Selective: jobs and checks follow the prediction.",
    }[mode]
    if artifact.get("build_only"):
        banner += (
            " **Build only** (push to `main`): checks compile and save caches; no tests run,"
            " because this commit already passed the full suite in its merge group."
        )
    deferred = artifact.get("deferred") or []
    if artifact.get("draft"):
        waiting = [c for c in deferred if c not in QUEUE_ONLY_CHECKS]
        listed = ", ".join(f"`{c}`" for c in waiting) if waiting else "none"
        banner += (
            " **Draft pull request**: only lints, docs and unit and integration tests run."
            f" Deferred until the pull request is ready for review: {listed}."
        )
    if artifact.get("linux_only"):
        banner += (
            " **Linux only**: the Windows legs, `windows_cluster` and the macOS job run in the"
            " merge queue, which runs every platform before a merge."
        )
    parts = [f"# CI execution: `{mode}`", "", banner, "", plan_to_summary(execution_plan(artifact), "Execution", artifact.get("deferred") or ())]
    if mode == "shadow":
        parts.append(plan_to_summary(plan_from_dict(artifact["predicted"]), "Predicted (selective)"))
    return "\n".join(parts)


__all__ = [
    "ARTIFACT_NAME",
    "DRAFT_JOBS",
    "QUEUE_ONLY_CHECKS",
    "Command",
    "Context",
    "ExecError",
    "PlanError",
    "SAVED_CACHE_CURRENT_ENV",
    "SCHEMA_VERSION",
    "artifact_summary",
    "build_only_command",
    "check_commands",
    "execution_mode",
    "gate",
    "git_head",
    "job_outputs",
    "load_artifact",
    "planned_commands",
    "prereq_commands",
    "resolve",
    "run_check",
    "validate",
    "write_github_output",
    "write_step_summary",
]
