#!/usr/bin/env python3
"""Unit tests for scripts/ci_plan (no live git history required)."""

from __future__ import annotations

import json
import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

from ci_plan.plan import (  # noqa: E402
    PlanError,
    build_plan,
    package_for_path,
    package_index_from_metadata,
    plan_to_github_output,
    plan_to_json,
    reverse_closure,
)

FIXTURES = Path(__file__).resolve().parent / "ci_plan_fixtures"
META = json.loads((FIXTURES / "workspace_metadata.json").read_text(encoding="utf-8"))


class CiPlanTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.index = package_index_from_metadata(META)

    def plan(self, *paths: str):
        return build_plan(list(paths), self.index)

    def test_docs_only(self) -> None:
        p = self.plan("docs/architecture.md", "docs/README.md")
        self.assertFalse(p.full_suite)
        self.assertTrue(p.docs_markdown)
        self.assertEqual(p.rust_packages, [])
        self.assertFalse(p.confinement)
        self.assertFalse(p.tray)
        self.assertFalse(p.release)
        self.assertFalse(p.ui)

    def test_isolated_leaf_naming(self) -> None:
        # bookclerk-naming is a small leaf; only its reverse dependents run.
        p = self.plan("crates/bookclerk-naming/src/lib.rs")
        self.assertFalse(p.full_suite)
        self.assertIn("bookclerk-naming", p.changed_packages)
        self.assertIn("bookclerk-naming", p.rust_packages)
        # Should include reverse dependents if any.
        closure = reverse_closure(["bookclerk-naming"], self.index)
        self.assertEqual(set(p.rust_packages), closure)

    def test_shared_crate_reverse_dependents(self) -> None:
        p = self.plan("crates/bookclerk-config/src/lib.rs")
        self.assertFalse(p.full_suite)
        self.assertIn("bookclerk-config", p.rust_packages)
        # Config is widely depended on — expect multiple packages.
        self.assertGreater(len(p.rust_packages), 3)

    def test_confinement_dependency(self) -> None:
        p = self.plan("crates/bookclerk-sandbox/src/lib.rs")
        self.assertTrue(p.confinement)
        self.assertIn("bookclerk-sandbox", p.rust_packages)

    def test_media_triggers_confinement(self) -> None:
        p = self.plan("crates/bookclerk-media/src/lib.rs")
        self.assertTrue(p.confinement)

    def test_tray_package(self) -> None:
        p = self.plan("crates/bookclerk-tray/src/lib.rs")
        self.assertTrue(p.tray)
        self.assertFalse(p.confinement)

    def test_ui_path(self) -> None:
        p = self.plan("ui/src/App.tsx")
        self.assertTrue(p.ui)
        self.assertEqual(p.rust_packages, [])

    def test_bookclerkd_pulls_ui(self) -> None:
        p = self.plan("crates/bookclerkd/src/main.rs")
        self.assertIn("bookclerkd", p.rust_packages)
        self.assertTrue(p.ui)
        self.assertTrue(p.store_free)
        self.assertTrue(p.release)

    def test_binary_only_excluded_from_doctest_packages(self) -> None:
        p = self.plan("crates/bookclerkd/src/main.rs")
        self.assertIn("bookclerkd", p.rust_packages)
        self.assertIn("bookclerkd", p.rust_doc_packages)
        self.assertNotIn("bookclerkd", p.rust_doctest_packages)
        self.assertFalse(self.index.by_name["bookclerkd"].supports_doctest)
        # Library dependents in the reverse closure remain doctestable.
        self.assertTrue(
            any(self.index.by_name[n].supports_doctest for n in p.rust_doctest_packages)
            or len(p.rust_doctest_packages) == 0
        )

    def test_lib_crate_included_in_doctest_packages(self) -> None:
        p = self.plan("crates/bookclerk-config/src/lib.rs")
        self.assertIn("bookclerk-config", p.rust_doctest_packages)
        self.assertTrue(self.index.by_name["bookclerk-config"].supports_doctest)

    def test_abi_schema_paths(self) -> None:
        p = self.plan("scripts/gen-plugin-abi.py")
        self.assertTrue(p.abi_sync)
        # Script alone is not a Cargo package.
        self.assertEqual(p.rust_packages, [])
        self.assertFalse(p.full_suite)

    def test_plugin_sdk_ts(self) -> None:
        p = self.plan("packages/plugin-sdk/src/index.ts")
        self.assertTrue(p.ts_sdk)
        self.assertTrue(p.abi_sync)

    def test_python_sdk(self) -> None:
        p = self.plan("packages/plugin-sdk-python/src/bookclerk_plugin_sdk/__init__.py")
        self.assertTrue(p.python_sdk)
        self.assertTrue(p.abi_sync)

    def test_new_optional_plugin_under_glob(self) -> None:
        # Existing optional plugin change.
        p = self.plan(
            "crates/bookclerk-plugins/optional/source-audible/src/main.rs"
        )
        self.assertTrue(p.build_app_optional)
        self.assertIn("bookclerk-plugin-source-audible", p.rust_packages)

    def test_platform_plugin(self) -> None:
        p = self.plan(
            "crates/bookclerk-plugins/platform/database-sqlite/src/main.rs"
        )
        self.assertTrue(p.build_app_platform)
        self.assertTrue(p.release)

    def test_example_rust_plugin(self) -> None:
        p = self.plan("examples/plugins-echo-native-rust/src/main.rs")
        self.assertTrue(p.build_app_examples)

    def test_non_cargo_example_plugin(self) -> None:
        p = self.plan("examples/plugins-echo-workerd-ts/src/index.ts")
        self.assertTrue(p.build_app_examples)
        self.assertEqual(p.rust_packages, [])
        self.assertFalse(p.full_suite)

    def test_root_cargo_toml_full_suite(self) -> None:
        p = self.plan("Cargo.toml")
        self.assertTrue(p.full_suite)
        self.assertTrue(p.confinement)
        self.assertTrue(p.tray)
        self.assertTrue(p.release)
        self.assertEqual(len(p.rust_packages), len(self.index.by_name))

    def test_cargo_lock_full_suite(self) -> None:
        p = self.plan("Cargo.lock")
        self.assertTrue(p.full_suite)

    def test_toolchain_full_suite(self) -> None:
        p = self.plan("rust-toolchain.toml")
        self.assertTrue(p.full_suite)

    def test_workflow_full_suite(self) -> None:
        p = self.plan(".github/workflows/ci.yml")
        self.assertTrue(p.full_suite)

    def test_ci_plan_script_full_suite(self) -> None:
        p = self.plan("scripts/ci-plan.py")
        self.assertTrue(p.full_suite)

    def test_unknown_top_level_full_suite(self) -> None:
        p = self.plan("brand-new-tooling/foo.sh")
        self.assertTrue(p.full_suite)

    def test_unresolved_manifest_full_suite(self) -> None:
        p = self.plan("crates/does-not-exist-yet/Cargo.toml")
        self.assertTrue(p.full_suite)

    def test_third_party_unclassified_full_suite(self) -> None:
        p = self.plan("third_party/audible-rs/src/lib.rs")
        self.assertTrue(p.full_suite)
        self.assertTrue(any("unclassified path" in r for r in p.reasons))

    def test_unknown_child_under_known_root_full_suite(self) -> None:
        p = self.plan("packages/brand-new-sdk/src/index.ts")
        self.assertTrue(p.full_suite)
        self.assertTrue(any("unclassified path" in r for r in p.reasons))

    def test_github_actions_unclassified_full_suite(self) -> None:
        p = self.plan(".github/actions/setup-rust/action.yml")
        self.assertTrue(p.full_suite)
        self.assertTrue(any("unclassified path" in r for r in p.reasons))

    def test_arbitrary_script_unclassified_full_suite(self) -> None:
        p = self.plan("scripts/ad-hoc-helper.sh")
        self.assertTrue(p.full_suite)
        self.assertTrue(any("unclassified path" in r for r in p.reasons))

    def test_all_files_under_crate_count(self) -> None:
        # README / fixtures / non-rs still map to the package.
        self.assertEqual(
            package_for_path("crates/bookclerk-config/README.md", self.index),
            "bookclerk-config",
        )
        p = self.plan("crates/bookclerk-config/README.md")
        self.assertIn("bookclerk-config", p.changed_packages)

    def test_force_full(self) -> None:
        p = build_plan(["docs/x.md"], self.index, force_full=True, force_full_reason="err")
        self.assertTrue(p.full_suite)
        self.assertIn("err", p.reasons[0])

    def test_github_output_format(self) -> None:
        p = self.plan("docs/a.md")
        text = plan_to_github_output(p)
        self.assertIn("full_suite=false", text)
        self.assertIn("docs_markdown=true", text)
        self.assertIn("confinement=false", text)
        self.assertIn("rust_doctest_packages=", text)

    def test_json_roundtrip_keys(self) -> None:
        p = self.plan("ui/src/main.tsx")
        data = json.loads(plan_to_json(p))
        self.assertTrue(data["ui"])
        self.assertIn("decisions", data)
        self.assertIn("rust_doctest_packages", data)

    def test_planner_error_escalation_via_empty_base(self) -> None:
        # plan_from_event with missing base/head and no paths escalates.
        from ci_plan.plan import plan_from_event

        p = plan_from_event(base=None, head=None, metadata=META, paths=None)
        self.assertTrue(p.full_suite)
        self.assertTrue(any("planner error" in r for r in p.reasons))


if __name__ == "__main__":
    unittest.main()
