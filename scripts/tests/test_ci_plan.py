#!/usr/bin/env python3
"""Unit tests for scripts/ci_plan (no live git history required).

Scenario tests assert the *executed* plan: selected checks, their
prerequisites, and the exact commands ``ci-exec.py run`` would issue.
"""

from __future__ import annotations

import copy
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
REPO = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

from ci_plan.execute import (  # noqa: E402
    SCHEMA_VERSION,
    Context,
    ExecError,
    execution_mode,
    gate,
    job_outputs,
    planned_commands,
    resolve,
    run_check,
    validate,
)
from ci_plan.github_paths import (  # noqa: E402
    github_actions_file_path,
    open_github_actions_append,
)
from ci_plan.plan import (  # noqa: E402
    ALL_CHECKS,
    E2E_PACKAGE,
    JOB_CHECKS,
    build_plan,
    discover_guests,
    glob_match,
    list_changed_paths,
    load_relations,
    package_for_path,
    package_index_from_metadata,
    PlanError,
    plan_from_dict,
    plan_from_event,
    plan_to_dict,
    plan_to_json,
    plan_to_summary,
    validate_relations,
)

FIXTURES = Path(__file__).resolve().parent / "ci_plan_fixtures"
META = json.loads((FIXTURES / "workspace_metadata.json").read_text(encoding="utf-8"))
INDEX = package_index_from_metadata(META)
RELATIONS = load_relations()
GUESTS = discover_guests(REPO, INDEX)
WORKERD_RUNTIME = sorted(g.id for g in GUESTS.values() if g.runtime == "workerd")


def plan(*paths: str, index=INDEX):
    return build_plan(list(paths), index, RELATIONS, guests=GUESTS)


def artifact(p, mode: str = "selective") -> dict:
    return {
        "schema_version": SCHEMA_VERSION,
        "execution_id": "fixtureexec0001",
        "run_id": "1",
        "event": "pull_request",
        "base_sha": "b",
        "head_sha": "h",
        "checkout_sha": "c",
        "selective_ci": "1",
        "mode": mode,
        "predicted": plan_to_dict(p),
        "execution": plan_to_dict(p),
    }


CTX = Context(
    workspace=Path("/ws"),
    os_name="Linux",
    tmp=Path("/tmp/x"),
    files_dir=Path("/tmp/x/files"),
    artifacts=Path("/tmp/x/art"),
)


def commands(p, check: str) -> list[list[str]]:
    return [c.argv for _, c in planned_commands(artifact(p), check, CTX)]


def check_argv(p, check: str) -> list[list[str]]:
    """Check commands only (no prerequisites)."""
    return [c.argv for key, c in planned_commands(artifact(p), check, CTX) if not key]


class WorkspaceModelTests(unittest.TestCase):
    def test_fixture_matches_live_workspace_members(self) -> None:
        if shutil.which("cargo") is None:
            self.skipTest("cargo not installed")
        live = json.loads(
            subprocess.check_output(
                ["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=REPO, text=True
            )
        )
        self.assertEqual(
            sorted(p["name"] for p in META["packages"]),
            sorted(p["name"] for p in live["packages"]),
            "regenerate scripts/tests/ci_plan_fixtures/workspace_metadata.json "
            "(cargo metadata --no-deps --format-version 1)",
        )

    def test_relations_reference_real_packages(self) -> None:
        self.assertEqual(validate_relations(RELATIONS, INDEX), [])

    def test_dev_edges_are_separate_from_compile_edges(self) -> None:
        # library dev-depends on the sqlite guest; that edge must not make
        # sqlite changes recompile library's production consumers.
        self.assertIn("bookclerk-library", INDEX.dev_consumers["bookclerk-plugin-database-sqlite"])
        self.assertNotIn(
            "bookclerk-library", INDEX.compile_consumers.get("bookclerk-plugin-database-sqlite", set())
        )

    def test_sdk_no_longer_depends_on_workerd(self) -> None:
        self.assertNotIn(
            "bookclerk-plugin-sdk", INDEX.compile_consumers.get("bookclerk-workerd", set())
        )
        self.assertIn(
            "bookclerk-plugin-tools", INDEX.compile_consumers["bookclerk-workerd"]
        )

    def test_guest_inventory(self) -> None:
        self.assertEqual(GUESTS["libro"].package, "bookclerk-plugin-source-libro")
        self.assertEqual(GUESTS["sqlite"].tier, "platform")
        self.assertEqual(GUESTS["echo_workerd_ts"].runtime, "workerd")
        self.assertIsNone(GUESTS["echo_workerd_ts"].package)

    def test_glob_match(self) -> None:
        self.assertTrue(glob_match("crates/bookclerk-plugins/optional/source-libro/plugin.toml", "crates/bookclerk-plugins/*/*/plugin.toml"))
        self.assertFalse(glob_match("crates/bookclerk-plugin-abi/fixtures/tools/x/plugin.toml", "crates/bookclerk-plugins/*/*/plugin.toml"))
        self.assertTrue(glob_match("a/b/c.rs", "a/**"))
        self.assertTrue(glob_match("plugin.toml", "**/plugin.toml"))

    def test_package_for_path_counts_every_file(self) -> None:
        self.assertEqual(package_for_path("crates/bookclerk-config/README.md", INDEX), "bookclerk-config")


class ScenarioTests(unittest.TestCase):
    def test_docs_only_runs_nothing(self) -> None:
        p = plan("docs/architecture.md", "docs/README.md")
        self.assertFalse(p.full_suite)
        self.assertEqual(p.checks, {})
        self.assertFalse(any(p.jobs.values()))

    def test_ui_only(self) -> None:
        p = plan("ui/src/App.tsx")
        self.assertEqual(sorted(p.checks), ["api_docs", "ui"])
        self.assertEqual(p.params("api_docs"), {"rust_packages": [], "typescript": False, "ui": True, "python": False})
        self.assertEqual({j for j, on in p.jobs.items() if on}, {"check"})

    def test_cli_only_binary_package_uses_tests_target_selection(self) -> None:
        p = plan("crates/bookclerk-cli/src/main.rs")
        self.assertEqual(p.lint_packages(), ["bookclerk-cli"])
        self.assertEqual(check_argv(p, "rust_test"), [["cargo", "test", "-p", "bookclerk-cli", "--tests"]])
        self.assertNotIn("--lib", sum(check_argv(p, "rust_test"), []))
        self.assertFalse(p.selected("doctest"))
        self.assertFalse(p.selected("postgres"))
        self.assertFalse(p.selected("e2e"))
        self.assertEqual(p.params("release"), {"mode": "affected", "packages": ["bookclerk-cli"]})
        self.assertEqual(
            check_argv(p, "release"), [["cargo", "build", "--release", "-p", "bookclerk-cli"]]
        )
        self.assertEqual(p.prereqs("rust_test"), [])

    def test_host_only(self) -> None:
        p = plan("crates/bookclerk-plugin-host/src/rpc.rs")
        self.assertEqual(p.params("e2e")["scope"], sorted(RELATIONS.runtime_smoke))
        self.assertNotIn(E2E_PACKAGE, p.params("rust_test")["packages"])
        self.assertIn(E2E_PACKAGE, p.lint_packages())
        build = next(x for x in p.prereqs("rust_test") if x["kind"] == "build")
        for pkg in ("bookclerk-plugin-database-sqlite", "bookclerk-plugin-database-postgres", "bookclerk-jail", "bookclerk-workerd"):
            self.assertIn(pkg, build["packages"])
        self.assertIn("ensure_workerd", [x["kind"] for x in p.prereqs("rust_test")])
        self.assertEqual(p.params("postgres")["steps"], ["rpc_like"])
        self.assertTrue(p.selected("native_gateway"))
        self.assertFalse(p.selected("confinement"))
        self.assertEqual(p.params("release")["packages"], ["bookclerk-cli", "bookclerkd"])
        stage = next(x for x in p.prereqs("e2e") if x["kind"] == "stage_plugins")
        self.assertEqual(stage["plugins"], sorted(RELATIONS.runtime_smoke))

    def test_host_discovery_change_checks_full_installation(self) -> None:
        p = plan("crates/bookclerk-plugin-host/src/discover.rs")
        self.assertEqual(p.params("e2e")["scope"], "full")

    def test_workerd_source_change(self) -> None:
        p = plan("crates/bookclerk-workerd/src/config.rs")
        self.assertEqual(p.params("e2e")["scope"], "full")
        self.assertEqual(
            sorted(p.lint_packages()),
            # host: its tests spawn guests through the launcher (test_guests).
            ["bookclerk-dev", "bookclerk-plugin-host", "bookclerk-plugin-tools", "bookclerk-workerd"],
        )
        self.assertFalse(p.packages["bookclerk-plugin-host"].compiled)
        build = next(x for x in p.prereqs("rust_test") if x["kind"] == "build")
        self.assertLessEqual(
            {"bookclerk-plugin-database-sqlite", "bookclerk-plugin-destination-local", "bookclerk-plugin-echo-native-rust"},
            set(build["packages"]),
        )
        stage = next(x for x in p.prereqs("e2e") if x["kind"] == "stage_plugins")
        self.assertTrue(stage["all"])

    def test_embedded_ts_sdk_selects_workerd_runtime_subset(self) -> None:
        p = plan("packages/plugin-sdk/embed/bookclerk_plugin.js")
        self.assertIn("bookclerk-workerd", p.test_packages())
        self.assertTrue(p.packages["bookclerk-workerd"].compiled)
        self.assertEqual(p.params("e2e")["scope"], WORKERD_RUNTIME)
        self.assertTrue(p.selected("plugin_sdk_abi"))
        self.assertTrue(p.params("api_docs")["typescript"])

    def test_embedded_python_sdk_selects_workerd_runtime_subset(self) -> None:
        p = plan("packages/plugin-sdk-python/src/bookclerk_plugin_sdk/workerd.py")
        self.assertIn("bookclerk-workerd", p.test_packages())
        self.assertEqual(p.params("e2e")["scope"], WORKERD_RUNTIME)
        self.assertTrue(p.selected("python_sdk"))

    def test_non_embedded_python_sdk_file_does_not_select_workerd(self) -> None:
        p = plan("packages/plugin-sdk-python/src/bookclerk_plugin_sdk/__init__.py")
        self.assertNotIn("bookclerk-workerd", p.packages)
        self.assertFalse(p.selected("e2e"))

    def test_single_optional_plugin(self) -> None:
        p = plan("crates/bookclerk-plugins/optional/source-libro/src/client.rs")
        self.assertEqual(p.test_packages(), ["bookclerk-plugin-source-libro"])
        self.assertEqual(p.params("e2e")["scope"], ["libro"])
        build = next(x for x in p.prereqs("e2e") if x["kind"] == "build")
        self.assertIn("bookclerk-plugin-source-libro", build["packages"])
        self.assertNotIn("bookclerk-plugin-source-audible", build["packages"])
        stage = next(x for x in p.prereqs("e2e") if x["kind"] == "stage_plugins")
        self.assertEqual(stage["plugins"], ["libro"])
        self.assertFalse(p.selected("release"))
        self.assertFalse(p.selected("postgres"))
        e2e_cmd = [c for key, c in planned_commands(artifact(p), "e2e", CTX) if not key][0]
        self.assertEqual(
            e2e_cmd.argv,
            ["cargo", "test", "-p", E2E_PACKAGE, "--lib", "--test", "staged_plugins", "--test", "installed_plugin_path"],
        )
        self.assertEqual(e2e_cmd.env["BOOKCLERK_STAGED_PLUGINS"], "libro")
        self.assertEqual(e2e_cmd.env["BOOKCLERK_REQUIRE_STAGED_PLUGINS"], "1")
        stage_cmd = [c.argv for key, c in planned_commands(artifact(p), "e2e", CTX) if key.startswith("stage_plugins")]
        self.assertEqual(
            stage_cmd,
            [["cargo", "stage-plugins", "--skip-build", "--dest", "/tmp/x/art", "--plugin", "libro"]],
        )

    def test_test_only_file_selects_owner_only(self) -> None:
        p = plan("crates/bookclerk-plugin-host/tests/guest_jail.rs")
        self.assertEqual(p.lint_packages(), ["bookclerk-plugin-host"])
        self.assertFalse(p.packages["bookclerk-plugin-host"].compiled)
        self.assertEqual(check_argv(p, "rust_test"), [["cargo", "test", "-p", "bookclerk-plugin-host", "--tests"]])
        self.assertFalse(p.selected("doctest"))
        self.assertFalse(p.selected("postgres"))
        self.assertFalse(p.selected("native_gateway"))
        self.assertFalse(p.selected("e2e"))

    def test_example_only_file_builds_examples(self) -> None:
        p = plan("crates/bookclerk-mp4/examples/bench_remux.rs")
        self.assertEqual(p.params("rust_test")["packages"], [])
        self.assertEqual(check_argv(p, "rust_test"), [["cargo", "build", "-p", "bookclerk-mp4", "--examples"]])

    def test_bench_only_file_builds_benches(self) -> None:
        meta = copy.deepcopy(META)
        pkg = next(x for x in meta["packages"] if x["name"] == "bookclerk-naming")
        root = os.path.dirname(pkg["manifest_path"])
        pkg["targets"].append({"kind": ["bench"], "name": "b", "src_path": f"{root}/benches/b.rs"})
        idx = package_index_from_metadata(meta)
        p = build_plan(["crates/bookclerk-naming/benches/b.rs"], idx, RELATIONS, guests=GUESTS)
        self.assertEqual(p.params("rust_test")["packages"], [])
        self.assertEqual(
            check_argv(p, "rust_test"),
            [["cargo", "bench", "-p", "bookclerk-naming", "--benches", "--no-run"]],
        )

    def test_shared_fixture_selects_declared_consumers_only(self) -> None:
        p = plan("crates/bookclerk-plugin-abi/fixtures/tools/valid-workerd/plugin.toml")
        self.assertEqual(p.lint_packages(), ["bookclerk-plugin-tools"])
        self.assertNotIn("bookclerk-plugin-abi", p.packages)
        self.assertTrue(p.selected("plugin_sdk_abi"))
        self.assertTrue(p.selected("python_sdk"))
        self.assertFalse(p.selected("e2e"))

    def test_dev_edge_selects_consumer_tests_and_doctests_not_its_consumers(self) -> None:
        p = plan("crates/bookclerk-plugins/platform/database-sqlite/src/plugin.rs")
        lib = p.packages["bookclerk-library"]
        self.assertFalse(lib.compiled)
        self.assertTrue(lib.tests and lib.doctest)
        self.assertIn("bookclerk-library", p.doctest_packages())
        self.assertNotIn("bookclerk-library", p.doc_packages())
        # Production consumers of library that have no dev edge to sqlite of
        # their own must not be selected through library's dev edge.
        sqlite = "bookclerk-plugin-database-sqlite"
        production_only = (
            INDEX.compile_consumers["bookclerk-library"]
            - INDEX.dev_consumers[sqlite]
            - INDEX.compile_consumers.get(sqlite, set())
            - {sqlite}
        )
        self.assertIn("bookclerk-cli", production_only)
        self.assertEqual(production_only & set(p.packages), set())

    def test_database_changes_select_postgres_steps(self) -> None:
        guest = plan("crates/bookclerk-db-guest/src/session.rs")
        self.assertIn("db_guest", guest.params("postgres")["steps"])
        self.assertIn("binding_schema", guest.params("postgres")["steps"])
        lib = plan("crates/bookclerk-library/src/store.rs")
        self.assertTrue({"library_queue", "library_totp", "library_vectors"} <= set(lib.params("postgres")["steps"]))
        self.assertNotIn("binding_schema", lib.params("postgres")["steps"])
        pg = plan("crates/bookclerk-plugins/optional/database-postgres/src/main.rs")
        self.assertEqual(sorted(pg.params("postgres")["steps"]), ["binding_schema", "rpc_like"])

    def test_e2e_full_takes_precedence_over_subsets(self) -> None:
        p = plan(
            "crates/bookclerk-plugins/optional/source-libro/src/client.rs",
            "crates/bookclerk-workerd/src/config.rs",
        )
        self.assertEqual(p.params("e2e")["scope"], "full")

    def test_e2e_subsets_union(self) -> None:
        p = plan(
            "crates/bookclerk-plugins/optional/source-libro/src/client.rs",
            "crates/bookclerk-plugins/optional/source-chirp/src/main.rs",
        )
        self.assertEqual(p.params("e2e")["scope"], ["chirp", "libro"])

    def test_platform_only_e2e_scope_stages_nothing(self) -> None:
        p = plan("crates/bookclerk-plugins/platform/destination-local/src/plugin.rs")
        self.assertEqual(p.params("e2e")["scope"], ["local"])
        cmds = [c.argv for key, c in planned_commands(artifact(p), "e2e", CTX) if key.startswith("stage_plugins")]
        self.assertEqual(cmds, [["<mkdir>", "/tmp/x/art"]])

    def test_prerequisites_do_not_select_suites(self) -> None:
        # Host tests build the sqlite/postgres guests; that must not select
        # their own tests or E2E ids.
        p = plan("crates/bookclerk-plugin-host/tests/guest_jail.rs")
        self.assertNotIn("bookclerk-plugin-database-sqlite", p.packages)
        self.assertNotIn("bookclerk-plugin-database-postgres", p.packages)

    def test_tests_that_launch_a_changed_guest_rerun(self) -> None:
        p = plan("crates/bookclerk-plugins/optional/database-postgres/src/main.rs")
        host = p.packages["bookclerk-plugin-host"]
        self.assertIn("tests launch bookclerk-plugin-database-postgres", host.why)
        self.assertTrue(host.tests and not host.compiled)
        # No propagation to host's production consumers.
        self.assertNotIn("bookclerk-cli", p.packages)
        local = plan("crates/bookclerk-plugins/platform/destination-local/src/plugin.rs")
        self.assertIn("bookclerk-workerd", local.test_packages())
        self.assertFalse(local.packages["bookclerk-workerd"].compiled)

    def test_non_cargo_example_guest(self) -> None:
        p = plan("examples/plugins-echo-workerd-ts/src/index.ts")
        self.assertEqual(p.params("e2e")["scope"], ["echo_workerd_ts"])
        self.assertEqual(p.lint_packages(), [])

    def test_manifest_change_compiles_package(self) -> None:
        p = plan("crates/bookclerk-naming/Cargo.toml")
        self.assertTrue(p.packages["bookclerk-naming"].compiled)

    def test_plugin_manifest_is_inventory(self) -> None:
        p = plan("crates/bookclerk-plugins/optional/source-libro/plugin.toml")
        self.assertEqual(p.params("e2e")["scope"], "full")
        self.assertIn("bookclerk-plugin-host", p.test_packages())

    def test_packaging_change_uses_full_release(self) -> None:
        p = plan("crates/bookclerk-dev/src/plugins.rs")
        self.assertEqual(p.params("release"), {"mode": "full"})
        self.assertEqual(p.params("e2e")["scope"], "full")

    def test_embedded_asset(self) -> None:
        p = plan("assets/brand/svg/bookclerk-logo.svg")
        self.assertTrue(p.packages["bookclerkd"].compiled)
        self.assertTrue(p.selected("ui"))

    def test_e2e_crate_owns_its_suite(self) -> None:
        p = plan("crates/bookclerk-plugin-e2e/src/lib.rs")
        self.assertEqual(p.params("e2e")["scope"], "full")
        self.assertFalse(p.selected("rust_test"))
        self.assertIn(E2E_PACKAGE, p.params("clippy")["packages"])

    def test_confinement_and_tray_jobs(self) -> None:
        self.assertTrue(plan("crates/bookclerk-sandbox/src/lib.rs").selected("confinement"))
        self.assertTrue(plan("crates/bookclerk-tray/src/lib.rs").selected("tray"))
        self.assertFalse(plan("crates/bookclerk-tray/src/lib.rs").selected("confinement"))

    def test_rename_and_deletion_report_both_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            def git(*a: str) -> str:
                return subprocess.check_output(["git", *a], cwd=tmp, text=True).strip()

            git("init", "-q")
            git("config", "user.email", "t@example.com")
            git("config", "user.name", "t")
            os.makedirs(os.path.join(tmp, "crates/x/src"))
            for name in ("a.rs", "gone.rs"):
                Path(tmp, "crates/x/src", name).write_text("fn a() {}\n" * 20)
            git("add", "-A")
            git("commit", "-qm", "base")
            base = git("rev-parse", "HEAD")
            git("mv", "crates/x/src/a.rs", "crates/x/src/b.rs")
            git("rm", "-q", "crates/x/src/gone.rs")
            git("commit", "-qm", "rename")
            head = git("rev-parse", "HEAD")
            self.assertEqual(
                list_changed_paths(base, head, tmp),
                ["crates/x/src/a.rs", "crates/x/src/b.rs", "crates/x/src/gone.rs"],
            )

    def test_deleted_package_file_fails_closed(self) -> None:
        p = plan("crates/bookclerk-removed/src/lib.rs")
        self.assertTrue(p.full_suite)


class FullSuiteTests(unittest.TestCase):
    def assert_full(self, p) -> None:
        self.assertTrue(p.full_suite)
        self.assertEqual(set(p.checks), set(ALL_CHECKS))
        self.assertTrue(all(p.jobs.values()))
        self.assertEqual(p.params("e2e"), {"scope": "full"})
        self.assertEqual(p.params("release"), {"mode": "full"})
        self.assertEqual(p.params("postgres")["steps"], list(RELATIONS.postgres_steps))

    def test_global_paths(self) -> None:
        for path in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".github/workflows/ci.yml", "scripts/ci-plan.py", "scripts/ci-exec.py", "scripts/ci_plan/relations.toml", "fuzz/fuzz_targets/sql_parse.rs"):
            with self.subTest(path=path):
                self.assert_full(plan(path))

    def test_unknown_and_unclassified(self) -> None:
        for path in ("brand-new-tooling/foo.sh", "third_party/audible-rs/src/lib.rs", "packages/brand-new-sdk/src/index.ts", ".github/actions/x/action.yml", "scripts/ad-hoc-helper.sh", "crates/does-not-exist-yet/Cargo.toml", "assets/other.png"):
            with self.subTest(path=path):
                self.assert_full(plan(path))

    def test_full_suite_commands(self) -> None:
        p = plan("Cargo.lock")
        self.assertEqual(
            check_argv(p, "rust_test")[0],
            ["cargo", "test", "--workspace", "--exclude", E2E_PACKAGE, "--tests"],
        )
        self.assertIn(["cargo", "build", "-p", "bookclerk-mp4", "--examples"], check_argv(p, "rust_test"))
        self.assertIn(
            ["cargo", "test", "-p", "bookclerk-plugin-database-sqlite", "--features", "host-helpers", "--tests"],
            check_argv(p, "rust_test"),
        )
        self.assertEqual(check_argv(p, "release")[0], ["cargo", "build-app", "--release", "--platform"])

    def test_planner_error_escalates_to_full(self) -> None:
        p = plan_from_event(base=None, head=None, metadata=META, paths=None, workspace_root=REPO)
        self.assert_full(p)
        self.assertTrue(any("planner error" in r for r in p.reasons))
        self.assertTrue(p.prereqs("e2e"))
        self.assertEqual(p.params("postgres")["steps"], list(RELATIONS.postgres_steps))

    def test_persistent_metadata_failure_is_not_executable(self) -> None:
        with mock.patch("ci_plan.plan.load_metadata", side_effect=PlanError("cargo metadata failed: boom")):
            with self.assertRaises(PlanError) as ctx:
                plan_from_event(
                    base="a", head="b", metadata=None, paths=["docs/a.md"], workspace_root=REPO
                )
        message = str(ctx.exception)
        self.assertIn("cargo metadata failed", message)
        self.assertIn("prerequisite-complete", message)

    def test_persistent_guest_discovery_failure_is_not_executable(self) -> None:
        with mock.patch("ci_plan.plan.discover_guests", side_effect=PlanError("cannot parse plugin.toml")):
            with self.assertRaises(PlanError) as ctx:
                plan_from_event(
                    base="a",
                    head="b",
                    metadata=META,
                    paths=["docs/a.md"],
                    workspace_root=REPO,
                )
        message = str(ctx.exception)
        self.assertIn("cannot parse plugin.toml", message)
        self.assertIn("prerequisite-complete", message)

    def test_unavailable_diff_still_builds_the_normal_full_plan(self) -> None:
        with mock.patch("ci_plan.plan.list_changed_paths", side_effect=PlanError("git diff failed: boom")):
            p = plan_from_event(base="a", head="b", metadata=META, paths=None, workspace_root=REPO)
        self.assert_full(p)
        self.assertTrue(any("git diff failed" in r for r in p.reasons))
        kinds = [item["kind"] for item in p.prereqs("e2e")]
        self.assertEqual(kinds, ["build", "ensure_workerd", "install_platform", "stage_plugins"])
        self.assertTrue(next(item for item in p.prereqs("e2e") if item["kind"] == "stage_plugins")["all"])
        self.assertEqual(p.params("postgres")["steps"], list(RELATIONS.postgres_steps))

    def test_resolve_failure_publishes_no_artifact_or_success_outputs(self) -> None:
        spec = importlib.util.spec_from_file_location("ci_exec_cli", SCRIPTS / "ci-exec.py")
        assert spec is not None and spec.loader is not None
        cli = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cli)
        with tempfile.TemporaryDirectory(prefix="bc-resolve-") as tmp:
            root = Path(tmp)
            plan_path = root / "ci-plan" / "ci-plan.json"
            output = root / "github_output"
            summary = root / "summary"
            output.write_text("before\n", encoding="utf-8")
            summary.write_text("before\n", encoding="utf-8")
            with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(summary)}):
                with mock.patch("ci_plan.plan.load_metadata", side_effect=PlanError("cargo metadata failed: boom")):
                    code = cli.main(["--plan", str(plan_path), "resolve", "--paths", "docs/a.md", "--run-id", "local"])
            self.assertEqual(code, 1)
            self.assertFalse(plan_path.exists())
            self.assertFalse(plan_path.parent.exists())
            self.assertEqual(output.read_text(encoding="utf-8"), "before\n")
            self.assertEqual(summary.read_text(encoding="utf-8"), "before\n")

    def test_force_full(self) -> None:
        self.assert_full(build_plan(["docs/x.md"], INDEX, RELATIONS, force_full=True, guests=GUESTS))


def _ctx(os_name: str) -> Context:
    return Context(
        workspace=Path("/ws"),
        os_name=os_name,
        tmp=Path("/tmp/x"),
        files_dir=Path("/tmp/x/files"),
        artifacts=Path("/tmp/x/art"),
    )


class NativeGatewayTests(unittest.TestCase):
    SMOKE = ["cargo", "test", "-p", E2E_PACKAGE, "--test", "native_gateway", "--", "--nocapture"]

    def test_launch_path_changes_select_the_smoke(self) -> None:
        for path in (
            "crates/bookclerk-plugin-host/src/spawn_stdio.rs",
            "crates/bookclerk-plugin-host/src/rpc_session.rs",
            "crates/bookclerk-workerd/src/socket_proxy.rs",
            "crates/bookclerk-workerd/src/pipe_bind.rs",
            "crates/bookclerk-workerd/workerd-pin.json",
            "packages/plugin-sdk/embed/bookclerk_plugin.js",
            "crates/bookclerk-plugin-sdk/src/net.rs",
            "crates/bookclerk-sandbox/src/lib.rs",
            "crates/bookclerk-jail/src/main.rs",
            "crates/bookclerk-plugin-e2e/Cargo.toml",
            "crates/bookclerk-plugin-e2e/src/bin/native_gateway_probe.rs",
            "crates/bookclerk-plugin-e2e/tests/native_gateway.rs",
            "crates/bookclerk-plugin-e2e/tests/native_gateway/support.rs",
        ):
            with self.subTest(path=path):
                p = plan(path)
                self.assertTrue(p.selected("native_gateway"), p.checks.keys())
                self.assertTrue(p.jobs["native-gateway"])

    def test_unrelated_changes_skip_the_smoke(self) -> None:
        for path in (
            "crates/bookclerk-cli/src/main.rs",
            "ui/src/App.tsx",
            "docs/plugins.md",
            "crates/bookclerk-plugins/optional/source-libro/src/client.rs",
            "crates/bookclerk-plugin-host/tests/guest_jail.rs",
            "crates/bookclerk-workerd/tests/connect_gateway.rs",
            "crates/bookclerk-plugin-e2e/tests/staged_plugins.rs",
        ):
            with self.subTest(path=path):
                p = plan(path)
                self.assertFalse(p.selected("native_gateway"), p.checks.get("native_gateway"))
                self.assertFalse(p.jobs["native-gateway"])

    def test_prerequisites_are_complete_and_minimal(self) -> None:
        for p in (plan("crates/bookclerk-plugin-sdk/src/net.rs"), plan("Cargo.lock")):
            with self.subTest(full=p.full_suite):
                self.assertEqual(
                    p.prereqs("native_gateway"),
                    [
                        {
                            "kind": "build",
                            "why": ["the smoke spawns its guest through bookclerk-jail + bookclerk-workerd"],
                            "packages": ["bookclerk-jail", "bookclerk-workerd"],
                        },
                        {"kind": "ensure_workerd", "why": ["the front door runs pinned workerd"]},
                    ],
                )
        # Building the helpers must not select their own suites, and the
        # smoke's own inputs do not pull in the staged installation.
        for path in (
            "crates/bookclerk-plugin-e2e/tests/native_gateway.rs",
            "crates/bookclerk-plugin-e2e/src/bin/native_gateway_probe.rs",
        ):
            with self.subTest(path=path):
                p = plan(path)
                self.assertTrue(p.selected("native_gateway"))
                for check in ("e2e", "rust_test", "confinement", "release"):
                    self.assertFalse(p.selected(check), check)
                self.assertEqual(p.lint_packages(), [E2E_PACKAGE])
        self.assertEqual(plan("crates/bookclerk-plugin-e2e/src/lib.rs").params("e2e"), {"scope": "full"})

    def test_commands_per_os_on_a_fresh_runner(self) -> None:
        art = artifact(plan("crates/bookclerk-plugin-sdk/src/net.rs"))
        for os_name, extra in (
            ("Linux", []),
            ("Darwin", [["cargo", "test", "-p", "bookclerk-workerd", "--lib"]]),
            (
                "Windows",
                [
                    ["cargo", "clippy", "-p", "bookclerk-workerd", "--all-targets", "--", "-D", "warnings"],
                    ["cargo", "clippy", "-p", "bookclerk-plugin-sdk", "--features", "http", "--all-targets", "--", "-D", "warnings"],
                    ["cargo", "clippy", "-p", "bookclerk-plugin-host", "--lib", "--", "-D", "warnings"],
                    ["cargo", "test", "-p", "bookclerk-workerd", "--lib"],
                ],
            ),
        ):
            with self.subTest(os=os_name):
                cmds = planned_commands(art, "native_gateway", _ctx(os_name))
                self.assertEqual(
                    [c.argv for key, c in cmds if key],
                    [["cargo", "build", "-p", "bookclerk-jail", "-p", "bookclerk-workerd"], ["cargo", "ensure-workerd"]],
                )
                checks = [c for key, c in cmds if not key]
                self.assertEqual([c.argv for c in checks], extra + [self.SMOKE])
                exe = "workerd.exe" if os_name == "Windows" else "workerd"
                self.assertEqual(
                    checks[-1].env["BOOKCLERK_WORKERD_BIN"], str(Path("/ws/target/debug") / exe)
                )

    def test_smoke_runs_only_in_its_own_check(self) -> None:
        full = artifact(plan("Cargo.lock"))
        for check in ALL_CHECKS:
            if check == "native_gateway":
                continue
            for _, cmd in planned_commands(full, check, CTX):
                with self.subTest(check=check, argv=cmd.argv):
                    self.assertNotIn("native_gateway", cmd.argv)
                    if cmd.argv[:2] == ["cargo", "test"] and ["-p", E2E_PACKAGE] == cmd.argv[2:4]:
                        self.assertNotIn("--tests", cmd.argv)
                        self.assertNotIn("--all-targets", cmd.argv)
        workspace = check_argv(plan("Cargo.lock"), "rust_test")[0]
        self.assertEqual(workspace[workspace.index("--exclude") + 1], E2E_PACKAGE)

    def test_staged_e2e_runs_every_other_e2e_target(self) -> None:
        tests_dir = REPO / "crates" / "bookclerk-plugin-e2e" / "tests"
        targets = sorted(f.stem for f in tests_dir.glob("*.rs") if f.stem != "native_gateway")
        argv = check_argv(plan("crates/bookclerk-plugins/optional/source-libro/src/client.rs"), "e2e")[0]
        named = sorted(argv[i + 1] for i, a in enumerate(argv) if a == "--test")
        self.assertEqual(named, targets)
        self.assertIn("--lib", argv)

    def test_full_and_shadow_run_the_gateway(self) -> None:
        self.assertTrue(plan("Cargo.lock").selected("native_gateway"))
        art = resolve(
            base=None,
            head=None,
            selective_ci="0",
            run_id="9",
            checkout_sha="abc",
            event="pull_request",
            paths=["docs/a.md"],
            metadata=META,
            workspace=REPO,
        )
        self.assertEqual(art["mode"], "shadow")
        self.assertFalse(plan_from_dict(art["predicted"]).selected("native_gateway"))
        self.assertTrue(plan_from_dict(art["execution"]).selected("native_gateway"))
        self.assertEqual(job_outputs(art)["job_native_gateway"], "true")

    def test_gate_covers_the_matrix_job(self) -> None:
        for path, want in (("crates/bookclerk-plugin-sdk/src/net.rs", True), ("docs/a.md", False)):
            p = plan(path)
            art = artifact(p)
            ok = {"plan": {"result": "success"}}
            for job, on in p.jobs.items():
                ok[job] = {"result": "success" if on else "skipped"}
            self.assertEqual(ok["native-gateway"]["result"], "success" if want else "skipped")
            self.assertEqual(gate(art, ok), [])
            for result in ("failure", "cancelled", "skipped" if want else "success"):
                with self.subTest(path=path, result=result):
                    bad = copy.deepcopy(ok)
                    bad["native-gateway"]["result"] = result
                    self.assertTrue(any(x.startswith("native-gateway:") for x in gate(art, bad)))


class ExecutionTests(unittest.TestCase):
    def test_modes(self) -> None:
        self.assertEqual(execution_mode(plan("Cargo.lock"), "1"), "full")
        self.assertEqual(execution_mode(plan("docs/a.md"), "0"), "shadow")
        self.assertEqual(execution_mode(plan("docs/a.md"), "1"), "selective")

    def test_shadow_executes_full_scope_and_records_prediction(self) -> None:
        art = resolve(
            base=None,
            head=None,
            selective_ci="0",
            run_id="9",
            checkout_sha="abc",
            event="pull_request",
            paths=["crates/bookclerk-cli/src/main.rs"],
            metadata=META,
            workspace=REPO,
        )
        self.assertEqual(art["mode"], "shadow")
        execution = plan_from_dict(art["execution"])
        self.assertTrue(all(execution.jobs.values()))
        self.assertEqual(execution.params("e2e"), {"scope": "full"})
        predicted = plan_from_dict(art["predicted"])
        self.assertFalse(predicted.full_suite)
        self.assertFalse(predicted.selected("e2e"))
        outputs = job_outputs(art)
        self.assertEqual(outputs["job_tray"], "true")
        self.assertEqual(outputs["release_mode"], "full")

    def test_selective_outputs_follow_prediction(self) -> None:
        art = resolve(
            base=None,
            head=None,
            selective_ci="1",
            run_id="9",
            checkout_sha="abc",
            event="pull_request",
            paths=["crates/bookclerk-cli/src/main.rs"],
            metadata=META,
            workspace=REPO,
        )
        out = job_outputs(art)
        self.assertEqual(out["mode"], "selective")
        self.assertEqual(out["job_check"], "true")
        self.assertEqual(out["job_postgres"], "false")
        self.assertEqual(out["check_e2e"], "false")
        self.assertEqual(out["release_mode"], "affected")
        self.assertEqual(out["check_needs_node"], "false")
        self.assertEqual(
            set(out),
            {"mode", "check_needs_cargo", "check_needs_node", "release_mode"}
            | {f"job_{j.replace('-', '_')}" for j in JOB_CHECKS}
            | {f"check_{c}" for c in JOB_CHECKS["check"]},
        )

    def test_validate(self) -> None:
        art = artifact(plan("crates/bookclerk-cli/src/main.rs"))
        validate(art, run_id="1", checkout_sha="c")
        with self.assertRaisesRegex(ExecError, "run_id"):
            validate(art, run_id="2", checkout_sha="c")
        with self.assertRaisesRegex(ExecError, "computed for"):
            validate(art, run_id="1", checkout_sha="other")
        bad = dict(art, schema_version=1)
        with self.assertRaisesRegex(ExecError, "schema_version"):
            validate(bad, run_id="1", checkout_sha="c")
        broken = dict(art)
        del broken["execution"]
        with self.assertRaisesRegex(ExecError, "missing `execution`"):
            validate(broken, run_id="1", checkout_sha="c")
        missing_id = dict(art)
        del missing_id["execution_id"]
        with self.assertRaisesRegex(ExecError, "missing `execution_id`"):
            validate(missing_id, run_id="1", checkout_sha="c")
        for unsafe in ("", ".", "..", "../escape", "a/b", "has space", "id\x00", 1, "x" * 65):
            with self.subTest(execution_id=unsafe):
                bad_id = dict(art, execution_id=unsafe)
                with self.assertRaisesRegex(ExecError, "safe path segment"):
                    validate(bad_id, run_id="1", checkout_sha="c")

    def test_unselected_check_is_rejected(self) -> None:
        with self.assertRaisesRegex(ExecError, "not in this run's execution plan"):
            planned_commands(artifact(plan("docs/a.md")), "rust_test", CTX)

    def test_prerequisites_run_before_the_check_in_declared_order(self) -> None:
        p = plan("crates/bookclerk-plugins/optional/source-libro/src/client.rs")
        kinds = [key.split("-")[0] for key, _ in planned_commands(artifact(p), "e2e", CTX)]
        self.assertEqual(kinds, ["build", "ensure_workerd", "install_platform", "stage_plugins", ""])

    def test_gate(self) -> None:
        p = plan("crates/bookclerk-cli/src/main.rs")
        art = artifact(p)
        ok = {"plan": {"result": "success"}}
        for job, on in p.jobs.items():
            ok[job] = {"result": "success" if on else "skipped"}
        self.assertEqual(gate(art, ok), [])
        for job, result, needle in (
            ("check", "skipped", "expected to run"),
            ("check", "failure", "expected to run"),
            ("check", "cancelled", "expected to run"),
            ("tray", "success", "expected to skip"),
            ("plan", "failure", "plan: failure"),
        ):
            with self.subTest(job=job, result=result):
                bad = copy.deepcopy(ok)
                bad[job]["result"] = result
                self.assertTrue(any(needle in x for x in gate(art, bad)), gate(art, bad))
        missing = copy.deepcopy(ok)
        del missing["postgres"]
        self.assertTrue(any("not reported" in x for x in gate(art, missing)))

    def _exec_ctx(self, root: Path) -> Context:
        tmp = root / "tmp"
        return Context(
            workspace=root,
            os_name="Linux",
            tmp=tmp,
            files_dir=tmp / "files",
            artifacts=tmp / "plugins",
        )

    def _resolve_same_selection(self) -> dict:
        return resolve(
            base=None,
            head=None,
            selective_ci="1",
            run_id="local",
            checkout_sha="abc",
            event="pull_request",
            paths=["crates/bookclerk-plugins/optional/source-libro/src/client.rs"],
            metadata=META,
            workspace=REPO,
        )

    def test_separate_resolves_rerun_prerequisites_in_the_same_temp_dir(self) -> None:
        calls: list[list[str]] = []

        def fake_run(argv, cwd=None, env=None):
            calls.append(list(argv))
            return subprocess.CompletedProcess(argv, 0)

        with tempfile.TemporaryDirectory(prefix="bc-exec-") as tmp:
            ctx = self._exec_ctx(Path(tmp))
            with mock.patch("ci_plan.execute.subprocess.run", side_effect=fake_run):
                first = self._resolve_same_selection()
                second = self._resolve_same_selection()
                self.assertNotEqual(first["execution_id"], second["execution_id"])
                self.assertEqual(first["run_id"], second["run_id"])
                self.assertEqual(first["checkout_sha"], second["checkout_sha"])
                self.assertEqual(first["execution"], second["execution"])
                run_check(first, "e2e", ctx)
                run_check(second, "e2e", ctx)
            marker_root = ctx.tmp / "ci-exec-prereqs"
            self.assertTrue((marker_root / first["execution_id"]).is_dir())
            self.assertTrue((marker_root / second["execution_id"]).is_dir())
            self.assertEqual(len(list((marker_root / first["execution_id"]).iterdir())), 4)
            self.assertEqual(len(list((marker_root / second["execution_id"]).iterdir())), 4)
        for argv in (
            ["cargo", "build"],
            ["cargo", "ensure-workerd"],
            ["cargo", "install-platform", "--skip-build"],
            ["cargo", "stage-plugins", "--skip-build"],
        ):
            hits = [c for c in calls if c[: len(argv)] == argv]
            self.assertEqual(len(hits), 2, calls)

    def test_checks_in_one_execution_share_prerequisite_markers(self) -> None:
        p = plan("crates/bookclerk-plugin-host/src/rpc.rs")
        art = artifact(p)
        calls: list[list[str]] = []

        def fake_run(argv, cwd=None, env=None):
            calls.append(list(argv))
            return subprocess.CompletedProcess(argv, 0)

        with tempfile.TemporaryDirectory(prefix="bc-exec-") as tmp:
            ctx = self._exec_ctx(Path(tmp))
            with mock.patch("ci_plan.execute.subprocess.run", side_effect=fake_run):
                run_check(art, "rust_test", ctx)
                run_check(art, "e2e", ctx)
                run_check(art, "e2e", ctx)
        workerd = [c for c in calls if c[:2] == ["cargo", "ensure-workerd"]]
        staged = [c for c in calls if c[:2] == ["cargo", "stage-plugins"]]
        e2e = [c for c in calls if c[:4] == ["cargo", "test", "-p", E2E_PACKAGE]]
        self.assertEqual(len(workerd), 1, calls)
        self.assertEqual(len(staged), 1, calls)
        self.assertEqual(len(e2e), 2, calls)

    def test_failed_prerequisite_is_not_marked_complete(self) -> None:
        art = artifact(plan("crates/bookclerk-plugins/optional/source-libro/src/client.rs"))
        calls: list[list[str]] = []
        fail_workerd = True

        def fake_run(argv, cwd=None, env=None):
            calls.append(list(argv))
            if fail_workerd and list(argv)[:2] == ["cargo", "ensure-workerd"]:
                return subprocess.CompletedProcess(argv, 1)
            return subprocess.CompletedProcess(argv, 0)

        with tempfile.TemporaryDirectory(prefix="bc-exec-") as tmp:
            ctx = self._exec_ctx(Path(tmp))
            markers = ctx.tmp / "ci-exec-prereqs" / art["execution_id"]
            with mock.patch("ci_plan.execute.subprocess.run", side_effect=fake_run):
                with self.assertRaisesRegex(ExecError, "ensure-workerd"):
                    run_check(art, "e2e", ctx)
                names = sorted(p.name for p in markers.iterdir())
                self.assertEqual(len(names), 1)
                self.assertTrue(names[0].startswith("build-"))
                self.assertFalse(any(n.startswith("ensure_workerd-") for n in names))
                fail_workerd = False
                before = len(calls)
                run_check(art, "e2e", ctx)
            retried = calls[before:]
            self.assertFalse(any(c[:2] == ["cargo", "build"] for c in retried), retried)
            self.assertTrue(any(c[:2] == ["cargo", "ensure-workerd"] for c in retried), retried)
            names = sorted(p.name for p in markers.iterdir())
            self.assertEqual(len(names), 4)
            self.assertTrue(any(n.startswith("ensure_workerd-") for n in names))

    def test_dry_run_does_not_create_completion_markers(self) -> None:
        art = artifact(plan("crates/bookclerk-plugins/optional/source-libro/src/client.rs"))
        with tempfile.TemporaryDirectory(prefix="bc-exec-") as tmp:
            ctx = self._exec_ctx(Path(tmp))
            with mock.patch("ci_plan.execute.subprocess.run") as run:
                run_check(art, "e2e", ctx, dry_run=True)
            run.assert_not_called()
            self.assertFalse((ctx.tmp / "ci-exec-prereqs").exists())

    def test_json_roundtrip(self) -> None:
        p = plan("crates/bookclerk-plugin-host/src/rpc.rs")
        again = plan_from_dict(json.loads(plan_to_json(p)))
        self.assertEqual(plan_to_dict(again), plan_to_dict(p))
        self.assertIn("### Checks", plan_to_summary(p))


class GithubActionsFilePathTests(unittest.TestCase):
    def test_accepts_path_under_runner_temp(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bc-runner-") as tmp:
            root = os.path.realpath(tmp)
            target_dir = os.path.join(root, "_runner_file_commands")
            os.makedirs(target_dir, exist_ok=True)
            # Spaces / Unicode must survive (no ASCII regex allowlist).
            target = os.path.join(target_dir, "set output café")
            with open(target, "w", encoding="utf-8") as fh:
                fh.write("")
            with mock.patch.dict(os.environ, {"RUNNER_TEMP": root}, clear=True):
                got = github_actions_file_path(target, label="GITHUB_OUTPUT")
            self.assertEqual(got, os.path.realpath(target))

    def test_rejects_escape_outside_runner_roots(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bc-runner-") as tmp:
            root = os.path.realpath(tmp)
            outside_dir = tempfile.mkdtemp(prefix="bc-outside-")
            outside = os.path.join(os.path.realpath(outside_dir), "leak")
            with open(outside, "w", encoding="utf-8") as fh:
                fh.write("x")
            try:
                with mock.patch.dict(os.environ, {"RUNNER_TEMP": root}, clear=True):
                    with self.assertRaises(SystemExit) as ctx:
                        github_actions_file_path(outside, label="GITHUB_OUTPUT")
                self.assertIn("outside RUNNER_TEMP", str(ctx.exception))
            finally:
                os.unlink(outside)
                os.rmdir(outside_dir)

    def test_rejects_symlink_escape(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bc-runner-") as tmp:
            root = os.path.realpath(tmp)
            outside_dir = tempfile.mkdtemp(prefix="bc-outside-")
            outside = os.path.join(os.path.realpath(outside_dir), "secret")
            with open(outside, "w", encoding="utf-8") as fh:
                fh.write("x")
            link = os.path.join(root, "leak")
            try:
                os.symlink(outside, link)
                with mock.patch.dict(os.environ, {"RUNNER_TEMP": root}, clear=True):
                    with self.assertRaises(SystemExit) as ctx:
                        github_actions_file_path(link, label="GITHUB_OUTPUT")
                self.assertIn("outside RUNNER_TEMP", str(ctx.exception))
            finally:
                if os.path.lexists(link):
                    os.unlink(link)
                os.unlink(outside)
                os.rmdir(outside_dir)

    def test_rejects_resolved_escape_via_dotdot(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bc-runner-") as tmp:
            root = os.path.realpath(tmp)
            escape = os.path.join(root, "..", "not-under-root")
            with mock.patch.dict(os.environ, {"RUNNER_TEMP": root}, clear=True):
                with self.assertRaises(SystemExit) as ctx:
                    github_actions_file_path(escape, label="GITHUB_OUTPUT")
            self.assertIn("outside RUNNER_TEMP", str(ctx.exception))

    def test_requires_runner_roots(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(SystemExit) as ctx:
                github_actions_file_path("/tmp/x", label="GITHUB_STEP_SUMMARY")
            self.assertIn("neither RUNNER_TEMP nor GITHUB_WORKSPACE", str(ctx.exception))

    def test_open_append_roundtrip(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bc-runner-") as tmp:
            root = os.path.realpath(tmp)
            target_dir = os.path.join(root, "_runner_file_commands")
            os.makedirs(target_dir, exist_ok=True)
            target = os.path.join(target_dir, "set_output_x")
            with mock.patch.dict(os.environ, {"RUNNER_TEMP": root}, clear=True):
                with open_github_actions_append(target, label="GITHUB_OUTPUT") as fh:
                    fh.write("full_suite=false\n")
            with open(target, encoding="utf-8") as fh:
                self.assertEqual(fh.read(), "full_suite=false\n")

    def test_open_append_accepts_non_hosted_runner_root(self) -> None:
        # Self-hosted RUNNER_TEMP is often /var/tmp or /srv/actions, not a
        # GitHub-hosted prefix. Containment is the runner root itself.
        if not (os.path.isdir("/var/tmp") and os.access("/var/tmp", os.W_OK)):
            self.skipTest("/var/tmp is not writable")
        with tempfile.TemporaryDirectory(prefix="bc-runner-", dir="/var/tmp") as tmp:
            root = os.path.realpath(tmp)
            self.assertTrue(root.startswith("/var/tmp") or root.startswith("/private/var/tmp"))
            target = os.path.join(root, "edition..2")
            with mock.patch.dict(os.environ, {"RUNNER_TEMP": root}, clear=True):
                got = github_actions_file_path(target, label="GITHUB_OUTPUT")
                self.assertEqual(got, os.path.realpath(target))
                with open_github_actions_append(target, label="GITHUB_OUTPUT") as fh:
                    fh.write("ok=1\n")
            with open(target, encoding="utf-8") as fh:
                self.assertEqual(fh.read(), "ok=1\n")


if __name__ == "__main__":
    unittest.main()
