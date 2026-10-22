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
# Prerequisites kept in build-only runs (staging and installs only feed tests).
BUILD_PREREQS = frozenset({"build", "ensure_workerd"})
# Runner labels of the per-OS matrix jobs (`confinement`, `native-gateway`,
# `tray`), in matrix order. ci.yml reads the list from the `matrix_os` output.
MATRIX_OS = ("ubuntu-latest", "macos-latest", "windows-latest")
# Linux-only runs keep only this matrix leg.
LINUX_OS = "ubuntu-latest"
# Jobs that run only on a non-Linux runner; Linux-only runs skip them.
NON_LINUX_JOBS = frozenset({"windows-cluster"})


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
        tmp = Path(os.environ.get("RUNNER_TEMP") or workspace / ".tmp" / "ci-exec")
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
    ``linux_only`` (Dependabot pull requests) keeps only the Linux leg of
    the per-OS matrix jobs and skips the Windows-only jobs, leaving the
    scarce macOS and Windows runners to the merge queue, whose full-suite
    run tests every platform before the update lands.
    """
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
        execution.checks = {c: v for c, v in execution.checks.items() if CHECK_JOB[c] in BUILD_ONLY_JOBS}
        execution.jobs = {job: on and job in BUILD_ONLY_JOBS for job, on in execution.jobs.items()}
    if linux_only:
        execution = plan_from_dict(plan_to_dict(execution))
        execution.checks = {c: v for c, v in execution.checks.items() if CHECK_JOB[c] not in NON_LINUX_JOBS}
        execution.jobs = {job: on and job not in NON_LINUX_JOBS for job, on in execution.jobs.items()}
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
        "matrix_os": [LINUX_OS] if linux_only else list(MATRIX_OS),
        "predicted": plan_to_dict(predicted),
        "execution": plan_to_dict(execution),
    }


def job_outputs(artifact: Mapping[str, Any]) -> dict[str, str]:
    """``GITHUB_OUTPUT`` keys for ``if:`` gates, all from ``execution``."""
    plan = plan_from_dict(artifact["execution"])
    out: dict[str, str] = {"mode": artifact["mode"], "matrix_os": json.dumps(artifact["matrix_os"])}
    for job, on in plan.jobs.items():
        out[f"job_{job.replace('-', '_')}"] = _b(on)
    for check in JOB_CHECKS["check"]:
        out[f"check_{check}"] = _b(plan.selected(check))
    check_job = [c for c in JOB_CHECKS["check"] if plan.selected(c)]
    out["check_needs_cargo"] = _b(any(c not in NON_CARGO_CHECKS for c in check_job))
    docs = plan.params("api_docs")
    out["check_needs_node"] = _b(
        plan.selected("ui")
        or plan.selected("plugin_sdk_abi")
        or (plan.selected("api_docs") and (docs.get("all") or docs.get("typescript") or docs.get("ui")))
    )
    # uv installs the Python SDK and its pdoc extra from uv.lock.
    out["check_needs_uv"] = _b(
        plan.selected("python_sdk")
        or (plan.selected("api_docs") and (docs.get("all") or docs.get("python")))
    )
    out["release_mode"] = plan.params("release").get("mode", "none") if plan.selected("release") else "none"
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
    for key in ("run_id", "checkout_sha", "mode", "build_only", "matrix_os", "predicted", "execution", "execution_id"):
        if key not in artifact:
            raise ExecError(f"plan artifact missing `{key}`")
    require_execution_id(artifact)
    if not isinstance(artifact["build_only"], bool):
        raise ExecError(f"plan build_only {artifact['build_only']!r} is not a boolean")
    matrix_os = artifact["matrix_os"]
    if matrix_os not in (list(MATRIX_OS), [LINUX_OS]):
        raise ExecError(f"plan matrix_os {matrix_os!r} is neither {list(MATRIX_OS)} nor {[LINUX_OS]}")
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


def _nextest_filter(custom: Sequence[tuple[str, str]]) -> list[str]:
    """``-E`` filterset excluding custom-harness test binaries (run separately)."""
    if not custom:
        return []
    ids = " | ".join(f"binary_id({pkg}::{target})" for pkg, target in custom)
    return ["-E", f"not ({ids})"]


def check_commands(check: str, plan: Plan, ctx: Context, *, event: str = "") -> list[Command]:
    """Commands for ``check`` (prerequisites excluded).

    ``event`` is the GitHub event the plan was resolved for; merge-group runs
    retry a failed ``rust_test`` test once.
    """
    params = plan.params(check)
    ws = str(ctx.workspace)
    ensured = any(p["kind"] == "ensure_workerd" for p in plan.prereqs(check))
    runtime_env = {"PATH": ctx.path_with_bins()}
    if ensured:
        runtime_env["BOOKCLERK_WORKERD_BIN"] = str(ctx.bin_dir / ("workerd.exe" if ctx.os_name == "Windows" else "workerd"))

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
            "test:plugin-migrations",
            "test:entrypoints",
            "test:entrypoints-codec",
        ):
            cmds.append(Command(f"npm run {script}", ["npm", "run", script], cwd=sdk))
        return cmds
    if check == "python_sdk":
        venv = ctx.tmp / "venv-plugin-sdk"
        py = str(venv / ("Scripts" if ctx.os_name == "Windows" else "bin") / "python")
        # Tests import the SDK from its source tree.
        src = {"PYTHONPATH": str(ctx.workspace / "packages" / "plugin-sdk-python" / "src")}
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
            Command("pytest", [py, "-m", "pytest", "packages/plugin-sdk-python/tests", "-q"], env=src),
        ]
    if check == "ui":
        ui = str(ctx.workspace / "ui")
        return [
            Command("npm ci (ui)", ["npm", "ci"], cwd=ui),
            Command("npm run lint (ui)", ["npm", "run", "lint"], cwd=ui),
            Command("npm run test:safe-html (ui)", ["npm", "run", "test:safe-html"], cwd=ui),
            Command("npm test (ui)", ["npm", "test"], cwd=ui),
            Command("npm run build (ui)", ["npm", "run", "build"], cwd=ui),
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
        if params.get("workspace"):
            scope = ["--workspace"]
            for ex in params.get("exclude", []):
                scope += ["--exclude", ex]
            custom = [t for t in CUSTOM_HARNESS_TESTS if t[0] not in params.get("exclude", [])]
            cmds.append(
                Command("unit + integration tests (workspace)", [*NEXTEST_RUN, *failure_mode, *scope, *_nextest_filter(custom)], env=env)
            )
        elif params.get("packages"):
            custom = [t for t in CUSTOM_HARNESS_TESTS if t[0] in params["packages"]]
            cmds.append(
                Command(
                    "unit + integration tests",
                    [*NEXTEST_RUN, *failure_mode, *_pkg_args(params["packages"]), *_nextest_filter(custom)],
                    env=env,
                )
            )
        else:
            custom = []
        for pkg, target in custom:
            cmds.append(Command(f"{pkg} {target} (custom harness)", ["cargo", "test", "-p", pkg, "--test", target], env=env))
        for ft in params.get("feature_tests", []):
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
        if params.get("benches"):
            cmds.append(Command("bench targets build", ["cargo", "bench", *_pkg_args(params["benches"]), "--benches", "--no-run"]))
        if params.get("examples"):
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


def planned_commands(artifact: Mapping[str, Any], check: str, ctx: Context) -> list[tuple[str, Command]]:
    """``(key, command)`` pairs: prerequisites first, then the check."""
    plan = execution_plan(artifact)
    if check not in CHECK_JOB:
        raise ExecError(f"unknown check `{check}`")
    if not plan.selected(check):
        raise ExecError(f"check `{check}` is not in this run's execution plan")
    build_only = bool(artifact.get("build_only"))
    out: list[tuple[str, Command]] = []
    for prereq in plan.prereqs(check):
        if build_only and prereq["kind"] not in BUILD_PREREQS:
            continue
        key_src = json.dumps({k: v for k, v in prereq.items() if k != "why"}, sort_keys=True)
        key = hashlib.sha256(key_src.encode()).hexdigest()[:16]
        for i, cmd in enumerate(prereq_commands(prereq, ctx)):
            out.append((f"{prereq['kind']}-{key}-{i}", cmd))
    for cmd in check_commands(check, plan, ctx, event=str(artifact.get("event", ""))):
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
            if argv[i] in ("--failure-output", "--no-tests", "--retries", "-E"):
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


def run_check(artifact: Mapping[str, Any], check: str, ctx: Context, *, dry_run: bool = False) -> None:
    """Runs ``check`` with its prerequisites (each prerequisite once per execution)."""
    # Validated before any path join. Checks that share this artifact share
    # the directory; a new resolve gets a new id and cannot see these markers.
    markers = ctx.tmp / "ci-exec-prereqs" / require_execution_id(artifact)
    for key, cmd in planned_commands(artifact, check, ctx):
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


def gate(artifact: Mapping[str, Any], needs: Mapping[str, Any]) -> list[str]:
    """Problems comparing job results with the expected execution."""
    problems: list[str] = []
    plan_result = (needs.get("plan") or {}).get("result")
    if plan_result != "success":
        problems.append(f"plan: {plan_result}")
    expected = execution_plan(artifact).jobs
    for job, want in expected.items():
        result = (needs.get(job) or {}).get("result")
        if result is None:
            problems.append(f"{job}: not reported to the gate (missing from needs)")
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
    if artifact["matrix_os"] == [LINUX_OS]:
        banner += (
            " **Linux only** (Dependabot pull request): macOS and Windows jobs are left to the"
            " merge queue, whose full-suite run tests every platform before the update lands."
        )
    parts = [f"# CI execution: `{mode}`", "", banner, "", plan_to_summary(execution_plan(artifact), "Execution")]
    if mode == "shadow":
        parts.append(plan_to_summary(plan_from_dict(artifact["predicted"]), "Predicted (selective)"))
    return "\n".join(parts)


__all__ = [
    "ARTIFACT_NAME",
    "Command",
    "Context",
    "ExecError",
    "PlanError",
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
