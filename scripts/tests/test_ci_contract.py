#!/usr/bin/env python3
"""Contract between .github/workflows/ci.yml and the planner/executor.

Parses the workflow with PyYAML (scripts/tests/requirements.txt) so job and
step structure is read, not pattern-matched. Fails when the workflow and the
planner could diverge: an output one side does not know, a check with no
step (or two), a job the gate does not see, a job that skips validation,
or a step that could hide a failure. Also fails when a Postgres-gated Rust
test is not selected by any ``postgres`` step filter in ci_plan.execute.
"""

from __future__ import annotations

import bisect
import functools
import os
import re
import sys
import tomllib
import unittest
from pathlib import Path

import yaml

SCRIPTS = Path(__file__).resolve().parents[1]
REPO = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

from ci_plan.plan import CHECK_JOB, JOB_CHECKS  # noqa: E402

WORKFLOW = REPO / ".github" / "workflows" / "ci.yml"
OUTPUT_REF = re.compile(r"needs\.plan\.outputs\.([A-Za-z0-9_]+)")
RUN_CHECK = re.compile(r"ci-exec\.py run ([a-z0-9_]+)\b")
# Watches a merge-group run and cancels it on the first failed job.
FAIL_FAST_JOB = "merge-queue-fail-fast"


def _load() -> dict:
    # PyYAML (YAML 1.1) reads a bare `on:` key as boolean True.
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))


def _emitted_keys() -> set[str]:
    keys = {"mode", "check_needs_cargo", "check_needs_node", "release_mode"}
    keys |= {f"job_{j.replace('-', '_')}" for j in JOB_CHECKS}
    keys |= {f"check_{c}" for c in JOB_CHECKS["check"]}
    return keys


def _strings(node) -> list[str]:
    if isinstance(node, str):
        return [node]
    if isinstance(node, dict):
        return [s for v in node.values() for s in _strings(v)]
    if isinstance(node, list):
        return [s for v in node for s in _strings(v)]
    return []


class WorkflowContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.wf = _load()
        cls.jobs = cls.wf["jobs"]

    def test_emitted_outputs_match_plan_job_outputs(self) -> None:
        from ci_plan.execute import job_outputs
        from ci_plan.plan import load_relations, package_index_from_metadata, plan_to_dict
        import json

        meta = json.loads((SCRIPTS / "tests/ci_plan_fixtures/workspace_metadata.json").read_text())
        from ci_plan.plan import build_plan, discover_guests

        index = package_index_from_metadata(meta)
        plan = build_plan(["docs/x.md"], index, load_relations(), guests=discover_guests(REPO, index))
        art = {"mode": "selective", "execution": plan_to_dict(plan)}
        self.assertEqual(set(job_outputs(art)), _emitted_keys())

        declared = self.jobs["plan"]["outputs"]
        self.assertEqual(set(declared), _emitted_keys(), "plan job outputs != ci-exec resolve outputs")
        for key, expr in declared.items():
            self.assertEqual(expr, "${{ steps.resolve.outputs.%s }}" % key)

    def test_every_referenced_output_is_emitted_and_every_output_is_used(self) -> None:
        used: set[str] = set()
        for name, job in self.jobs.items():
            if name == "plan":
                continue
            for s in _strings(job):
                used |= set(OUTPUT_REF.findall(s))
        self.assertLessEqual(used, _emitted_keys(), f"unknown outputs: {used - _emitted_keys()}")
        # `mode` is for humans (summary / logs); everything else gates work.
        self.assertEqual(_emitted_keys() - used, {"mode"})

    def test_jobs_match_planner(self) -> None:
        gated = [j for j in self.jobs if j not in ("plan", "ci-gate", FAIL_FAST_JOB)]
        self.assertEqual(set(gated), set(JOB_CHECKS))
        for job in gated:
            self.assertEqual(
                self.jobs[job].get("if"),
                f"needs.plan.outputs.job_{job.replace('-', '_')} == 'true'",
                job,
            )
            self.assertEqual(self.jobs[job].get("needs"), "plan", job)

    def test_each_check_has_exactly_one_step_in_its_job(self) -> None:
        seen: dict[str, str] = {}
        for job_name, job in self.jobs.items():
            for step in job.get("steps", []):
                for check in RUN_CHECK.findall(step.get("run", "")):
                    self.assertNotIn(check, seen, f"{check} run in {seen.get(check)} and {job_name}")
                    self.assertEqual(CHECK_JOB.get(check), job_name, f"{check} runs in wrong job {job_name}")
                    seen[check] = job_name
                    if job_name == "check":
                        self.assertEqual(step.get("if"), f"needs.plan.outputs.check_{check} == 'true'")
        self.assertEqual(set(seen), set(CHECK_JOB))

    def test_every_downstream_job_downloads_and_validates_first(self) -> None:
        for name, job in self.jobs.items():
            if name in ("plan", FAIL_FAST_JOB):
                continue
            steps = job["steps"]
            download = [i for i, s in enumerate(steps) if str(s.get("uses", "")).startswith("actions/download-artifact@")]
            self.assertEqual(len(download), 1, name)
            self.assertEqual(steps[download[0]]["with"]["name"], "ci-plan", name)
            self.assertEqual(steps[download[0]]["with"]["path"], "${{ runner.temp }}/ci-plan", name)
            execs = [i for i, s in enumerate(steps) if "ci-exec.py" in s.get("run", "")]
            self.assertTrue(execs, name)
            self.assertIn("ci-exec.py validate", steps[execs[0]]["run"], f"{name}: validate must be the first ci-exec call")
            self.assertLess(download[0], execs[0], name)

    def test_plan_job_uploads_the_resolved_artifact(self) -> None:
        steps = self.jobs["plan"]["steps"]
        resolve = [s for s in steps if "ci-exec.py resolve" in s.get("run", "")]
        self.assertEqual(len(resolve), 1)
        self.assertEqual(resolve[0].get("id"), "resolve")
        self.assertIn('--selective "$SELECTIVE_CI"', resolve[0]["run"])
        upload = [s for s in steps if str(s.get("uses", "")).startswith("actions/upload-artifact@")]
        self.assertEqual(len(upload), 1)
        self.assertEqual(upload[0]["with"]["path"], "${{ runner.temp }}/ci-plan/ci-plan.json")
        self.assertEqual(upload[0]["with"]["if-no-files-found"], "error")

    def test_full_suite_forced_on_main_and_merge_group(self) -> None:
        run = next(s["run"] for s in self.jobs["plan"]["steps"] if "ci-exec.py resolve" in s.get("run", ""))
        self.assertIn('"${{ github.event_name }}" = "merge_group"', run)
        self.assertIn('"${{ github.ref }}" = "refs/heads/main"', run)
        self.assertIn("--force-full", run)
        triggers = self.wf.get("on", self.wf.get(True))
        self.assertIn("merge_group", triggers)
        self.assertEqual(triggers["push"]["branches"], ["main"])

    def test_gate_sees_every_job_and_runs_always(self) -> None:
        gate = self.jobs["ci-gate"]
        self.assertEqual(gate["name"], "CI Gate")
        self.assertEqual(gate["if"], "always()")
        self.assertEqual(set(gate["needs"]), {"plan", *JOB_CHECKS})
        run = gate["steps"][-1]
        self.assertIn("ci-exec.py gate", run["run"])
        self.assertEqual(run["env"]["NEEDS_JSON"], "${{ toJSON(needs) }}")

    def test_native_gateway_runs_on_every_os(self) -> None:
        job = self.jobs["native-gateway"]
        self.assertEqual(job["runs-on"], "${{ matrix.os }}")
        self.assertIs(job["strategy"]["fail-fast"], False)
        self.assertEqual(job["strategy"]["matrix"], {"os": ["ubuntu-latest", "macos-latest", "windows-latest"]})
        steps = job["steps"]
        capnp = {s.get("if") for s in steps if "capnp" in s.get("run", "") and "ci-exec.py" not in s.get("run", "")}
        self.assertEqual(
            capnp, {"runner.os == 'Linux'", "runner.os == 'macOS'", "runner.os == 'Windows'"}
        )
        run = [s for s in steps if "ci-exec.py run native_gateway" in s.get("run", "")]
        self.assertEqual(len(run), 1)
        self.assertNotIn("if", run[0], "the smoke must run on every matrix child")
        text = WORKFLOW.read_text(encoding="utf-8")
        self.assertNotIn("windows_gateway", text)
        self.assertNotIn("windows-gateway", text)

    def test_windows_cluster_runs_on_windows_only(self) -> None:
        job = self.jobs["windows-cluster"]
        self.assertEqual(job["runs-on"], "windows-latest")
        self.assertNotIn("strategy", job)
        self.assertEqual(job["timeout-minutes"], 45)
        steps = job["steps"]
        capnp = [s for s in steps if "capnproto" in s.get("run", "") and "ci-exec.py" not in s.get("run", "")]
        self.assertEqual(len(capnp), 1)
        self.assertEqual(capnp[0].get("shell"), "pwsh")
        run = [s for s in steps if "ci-exec.py run windows_cluster" in s.get("run", "")]
        self.assertEqual(len(run), 1)
        self.assertNotIn("if", run[0])
        text = WORKFLOW.read_text(encoding="utf-8")
        self.assertEqual(text.count("ci-exec.py run windows_cluster"), 1)

    def test_envelope_builds_on_the_runner(self) -> None:
        job = self.jobs["envelope"]
        self.assertEqual(job["runs-on"], "ubuntu-latest")
        self.assertEqual(job["timeout-minutes"], 180)
        self.assertNotIn("container", job)
        self.assertNotIn("CARGO_BUILD_JOBS", job.get("env") or {})
        for step in job["steps"]:
            script = step.get("run", "")
            self.assertNotIn("pipefail", script, step.get("name"))
        runs = [s for s in job["steps"] if "ci-exec.py run envelope" in s.get("run", "")]
        self.assertEqual(len(runs), 1)
        self.assertNotIn("if", runs[0])

    def test_native_gateway_smoke_inputs_exist(self) -> None:
        from ci_plan.plan import load_relations

        for pattern in load_relations().native_gateway_paths:
            if "*" not in pattern:
                self.assertTrue((REPO / pattern).is_file(), pattern)

    def test_nothing_can_hide_a_failure(self) -> None:
        for name, job in self.jobs.items():
            self.assertNotIn("continue-on-error", job, name)
            for step in job.get("steps", []):
                self.assertNotIn("continue-on-error", step, f"{name}: {step.get('name')}")

    def test_selective_ci_default_is_explicit(self) -> None:
        self.assertIn(self.wf["env"]["SELECTIVE_CI"], ("0", "1"))

    def test_build_only_is_limited_to_main_pushes(self) -> None:
        run = next(s["run"] for s in self.jobs["plan"]["steps"] if "ci-exec.py resolve" in s.get("run", ""))
        guard = '"${{ github.event_name }}" = "push" ] && [ "${{ github.ref }}" = "refs/heads/main" ]'
        self.assertIn(guard, run)
        self.assertEqual(run.count("--build-only"), 1)
        self.assertLess(run.index(guard), run.index("--build-only"))

    def test_only_build_only_jobs_save_caches(self) -> None:
        """A job that saves a cache must still run on the build-only main push."""
        from ci_plan.execute import BUILD_ONLY_JOBS

        savers: set[str] = set()
        for name, job in self.jobs.items():
            for step in job.get("steps", []):
                if not str(step.get("uses", "")).startswith("Swatinem/rust-cache@"):
                    continue
                with self.subTest(job=name):
                    self.assertIn(step["with"].get("shared-key"), {"lint", "debug", "release"})
                    self.assertIn("CARGO_PROFILE_DEV_DEBUG", step["with"]["env-vars"].split())
                    if step["with"]["save-if"] is not False and step["with"]["save-if"] != "false":
                        self.assertIn("refs/heads/main", step["with"]["save-if"])
                        savers.add(name)
        self.assertEqual(savers, set(BUILD_ONLY_JOBS))

    def test_fail_fast_job_is_merge_group_only_and_alone_can_cancel(self) -> None:
        job = self.jobs[FAIL_FAST_JOB]
        self.assertEqual(job["if"], "github.event_name == 'merge_group'")
        self.assertEqual(job["permissions"], {"actions": "write"})
        self.assertNotIn("needs", job, "must watch from the start of the run")
        for step in job["steps"]:
            self.assertNotIn("uses", step, "must not check out or run repository code")
        self.assertIn('gh run cancel "$RUN_ID"', job["steps"][0]["run"])
        self.assertNotIn(FAIL_FAST_JOB, self.jobs["ci-gate"]["needs"])
        for name, other in self.jobs.items():
            if name != FAIL_FAST_JOB:
                self.assertNotIn("permissions", other, name)
        self.assertEqual(self.wf["permissions"], {"contents": "read"})

    def test_test_job_installs_pinned_nextest(self) -> None:
        steps = self.jobs["test"]["steps"]
        install = [s for s in steps if str(s.get("uses", "")).startswith("taiki-e/install-action@")]
        self.assertEqual(len(install), 1)
        self.assertRegex(install[0]["with"]["tool"], r"^cargo-nextest@\d+\.\d+\.\d+$")

    def test_custom_harness_targets_are_listed(self) -> None:
        """Every `harness = false` test target runs outside nextest (see CUSTOM_HARNESS_TESTS)."""
        from ci_plan.execute import CUSTOM_HARNESS_TESTS

        found = set()
        for manifest in REPO.glob("**/Cargo.toml"):
            if "target" in manifest.parts or "node_modules" in manifest.parts:
                continue
            data = tomllib.loads(manifest.read_text(encoding="utf-8"))
            for test in data.get("test", []):
                if test.get("harness") is False:
                    found.add((data["package"]["name"], test["name"]))
        self.assertEqual(found, set(CUSTOM_HARNESS_TESTS))

    def test_actions_are_pinned(self) -> None:
        for name, job in self.jobs.items():
            for step in job.get("steps", []):
                uses = step.get("uses")
                if uses:
                    self.assertRegex(uses, r"@[0-9a-f]{40}$", f"{name}: {uses}")


# --- Postgres-gated Rust tests vs the executor's `postgres` steps ----------
#
# Postgres tests skip (or are `#[ignore]`d) without BOOKCLERK_TEST_POSTGRES_URL,
# so ordinary `cargo test` passes whether or not they work. Only the
# `postgres` check runs them, through name filters in ci_plan.execute
# POSTGRES_STEPS. A test those filters miss never runs anywhere.

PG_ENV = "BOOKCLERK_TEST_POSTGRES_URL"
TEST_ATTR = re.compile(r"#\[(?:tokio::)?test(?:\([^\]]*\))?\]")
IGNORE_ATTR = re.compile(r'#\[ignore(?:\s*=\s*"((?:[^"\\]|\\.)*)")?\]')
PG_IGNORE_HINT = re.compile(r'#\[ignore\s*=\s*"[^"]*postgres', re.I)
FN_DECL = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\b")
CALL = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*(?:::<[^>]*>)?\s*\(")
# Cargo options that take a value (the value is not a test-name filter).
CARGO_VALUE_FLAGS = {
    "-p", "--package", "--test", "--bin", "--example", "--bench", "--features",
    "-F", "--target", "--profile", "-j", "--jobs", "--manifest-path", "--exclude",
}


RUST_TOKEN = re.compile(r"""//|/\*|\bb?r(#*)"|"|'""")
RUST_CHAR = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'])'")
RUST_BLOCK_COMMENT = re.compile(r"/\*|\*/")
RUST_STRING_END = re.compile(r'\\.|"', re.S)


def _mask_rust(text: str) -> str:
    """``text`` with comments and string/char literal bodies blanked.

    Offsets are preserved so matches in the masked copy index the original.
    """
    parts: list[str] = []
    i, n = 0, len(text)

    def keep(a: int, b: int) -> None:
        parts.append(text[a:b])

    def blank(a: int, b: int) -> None:
        parts.append(re.sub(r"[^\n]", " ", text[a:b]))

    while i < n:
        m = RUST_TOKEN.search(text, i)
        if m is None:
            keep(i, n)
            break
        keep(i, m.start())
        tok, start = m.group(0), m.start()
        if tok == "//":
            end = text.find("\n", start)
            end = n if end < 0 else end
            blank(start, end)
        elif tok == "/*":
            depth, end = 1, m.end()
            while depth and (c := RUST_BLOCK_COMMENT.search(text, end)):
                depth += 1 if c.group(0) == "/*" else -1
                end = c.end()
            end = end if not depth else n
            blank(start, end)
        elif tok.endswith('"') and tok != '"':  # raw string r#"…"#
            close = '"' + m.group(1)
            end = text.find(close, m.end())
            end = n if end < 0 else end + len(close)
            keep(start, m.end())
            blank(m.end(), max(m.end(), end - len(close)))
            keep(max(m.end(), end - len(close)), end)
        elif tok == '"':
            end = m.end()
            while (c := RUST_STRING_END.search(text, end)) and c.group(0) != '"':
                end = c.end()
            end = c.end() if c else n
            keep(start, start + 1)
            blank(start + 1, end - 1)
            keep(end - 1, end)
        elif ch := RUST_CHAR.match(text, start):
            end = ch.end()
            keep(start, start + 1)
            blank(start + 1, end - 1)
            keep(end - 1, end)
        else:  # lifetime or label
            end = start + 1
            keep(start, end)
        i = end
    return "".join(parts)


@functools.cache
def _manifest_package(directory: Path) -> str | None:
    manifest = directory / "Cargo.toml"
    if not manifest.is_file():
        return None
    return tomllib.loads(manifest.read_text(encoding="utf-8")).get("package", {}).get("name")


def _package_of(path: Path) -> tuple[str, Path] | None:
    """Name and directory of the Cargo package that owns ``path``."""
    for parent in path.parents:
        if (parent / "Cargo.toml").is_file():
            name = _manifest_package(parent)
            return (name, parent) if name else None
        if parent == REPO:
            return None
    return None


def _target_of(path: Path, package_dir: Path) -> str:
    """``lib``, ``test:<stem>``, or ``other`` for a source file of a package."""
    rel = path.relative_to(package_dir).parts
    if rel[0] == "src":
        return "other" if rel[1:2] in (("main.rs",), ("bin",)) else "lib"
    if rel[0] == "tests" and len(rel) == 2:
        return f"test:{Path(rel[1]).stem}"
    if rel[0] == "tests" and len(rel) == 3 and rel[2] == "main.rs":
        return f"test:{rel[1]}"
    return "other"


SIGNATURE_END = re.compile(r"[()\[\]{;]")
BRACE = re.compile(r"[{}]")


def _rust_fns(text: str) -> list[dict]:
    """Every ``fn`` with a body: name, test/ignore attributes, env use, calls."""
    masked = _mask_rust(text)
    newlines = [m.start() for m in re.finditer("\n", text)]
    fns = []
    for m in FN_DECL.finditer(masked):
        depth, body_start = 0, None
        for tok in SIGNATURE_END.finditer(masked, m.end()):
            ch = tok.group(0)
            if ch in "([":
                depth += 1
            elif ch in ")]":
                depth -= 1
            elif depth == 0:
                body_start = tok.start() if ch == "{" else None
                break
        if body_start is None:
            continue
        depth, body_end = 0, len(masked)
        for tok in BRACE.finditer(masked, body_start):
            depth += 1 if tok.group(0) == "{" else -1
            if depth == 0:
                body_end = tok.end()
                break
        head_start = max(masked.rfind(t, 0, m.start()) for t in "{};") + 1
        ignore = IGNORE_ATTR.search(text, head_start, m.start())
        fns.append(
            {
                "name": m.group(1),
                "line": bisect.bisect_left(newlines, m.start()) + 1,
                "test": bool(TEST_ATTR.search(masked, head_start, m.start())),
                "ignored": ignore is not None,
                "reason": (ignore.group(1) or "") if ignore else "",
                "env": PG_ENV in text[body_start:body_end],
                "calls": set(CALL.findall(masked, body_start, body_end)),
            }
        )
    return fns


def postgres_gated_tests(root: Path = REPO, files: dict[Path, str] | None = None) -> list[dict]:
    """Tests that need ``BOOKCLERK_TEST_POSTGRES_URL`` to actually run.

    A test is gated when its ``#[ignore]`` reason names Postgres, or its body
    reads the variable or (transitively, within its package) calls a helper
    that does.
    """
    if files is None:
        files = {}
        for top in ("crates", "examples"):
            for dirpath, dirnames, filenames in os.walk(root / top):
                dirnames[:] = sorted(
                    d for d in dirnames if d not in ("target", "node_modules") and not d.startswith(".")
                )
                for f in sorted(filenames):
                    if f.endswith(".rs"):
                        p = Path(dirpath) / f
                        files[p] = p.read_text(encoding="utf-8", errors="replace")
    sources: dict[tuple[str, Path], list[tuple[Path, str]]] = {}
    for path, text in files.items():
        found = _package_of(path)
        if found is not None:
            sources.setdefault(found, []).append((path, text))
    gated = []
    for (pkg, pkg_dir), pkg_files in sources.items():
        # Neither the variable nor a Postgres ignore reason anywhere: nothing gated.
        if not any(PG_ENV in text or PG_IGNORE_HINT.search(text) for _, text in pkg_files):
            continue
        fns = []
        for path, text in pkg_files:
            target, rel = _target_of(path, pkg_dir), path.relative_to(root)
            for fn in _rust_fns(text):
                fn.update(package=pkg, target=target, path=rel)
                fns.append(fn)
        helpers = {f["name"] for f in fns if not f["test"] and f["env"]}
        while True:
            more = {f["name"] for f in fns if not f["test"] and f["calls"] & helpers} - helpers
            if not more:
                break
            helpers |= more
        for f in fns:
            if f["test"] and (
                "postgres" in f["reason"].lower() or f["env"] or f["calls"] & helpers
            ):
                gated.append(f)
    return gated


def parse_cargo_test(argv: list[str]) -> dict:
    """Package, targets, filters and ignore mode of one ``cargo test`` argv."""
    assert argv[:2] == ["cargo", "test"], argv
    args = argv[2:]
    sep = args.index("--") if "--" in args else len(args)
    cargo, harness = args[:sep], args[sep + 1 :]
    step = {"package": None, "targets": [], "filters": [], "ignored": "skip", "exact": False}
    i = 0
    while i < len(cargo):
        a = cargo[i]
        if a in ("-p", "--package"):
            step["package"] = cargo[i + 1]
        elif a == "--lib":
            step["targets"].append("lib")
        elif a == "--test":
            step["targets"].append(f"test:{cargo[i + 1]}")
        if a in CARGO_VALUE_FLAGS:
            i += 2
            continue
        if not a.startswith("-"):
            step["filters"].append(a)
        i += 1
    i = 0
    while i < len(harness):
        a = harness[i]
        if a == "--ignored":
            step["ignored"] = "only"
        elif a == "--include-ignored":
            step["ignored"] = "include"
        elif a == "--exact":
            step["exact"] = True
        elif a == "--skip":
            i += 1
        elif not a.startswith("-"):
            step["filters"].append(a)
        i += 1
    return step


def step_selects(step: dict, test: dict) -> bool:
    """True when ``step`` runs ``test``, judged by the function name only.

    libtest filters match the full ``module::path::name``; requiring the
    filter to match the function name keeps a module rename from silently
    dropping a test.
    """
    if step["package"] != test["package"]:
        return False
    if step["targets"] and test["target"] not in step["targets"]:
        return False
    if step["ignored"] == "only" and not test["ignored"]:
        return False
    if step["ignored"] == "skip" and test["ignored"]:
        return False
    name = test["name"]
    if not step["filters"]:
        return True
    if step["exact"]:
        return any(f == name or f.endswith("::" + name) for f in step["filters"])
    return any(f in name for f in step["filters"])


class PostgresTestSelectionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        from ci_plan.execute import POSTGRES_STEPS

        cls.steps = {key: parse_cargo_test(argv) for key, (_, argv) in POSTGRES_STEPS.items()}
        cls.gated = postgres_gated_tests()

    def test_every_postgres_gated_test_runs_in_the_postgres_check(self) -> None:
        missed = [
            f"{t['path']}:{t['line']} {t['name']} ({t['package']}, "
            f"{'ignored' if t['ignored'] else 'env-gated'})"
            for t in self.gated
            if not any(step_selects(s, t) for s in self.steps.values())
        ]
        self.assertEqual(
            missed,
            [],
            "Postgres-gated tests no POSTGRES_STEPS filter selects (they never run in CI); "
            "prefix them `postgres_` or add a step in scripts/ci_plan/execute.py",
        )

    def test_every_postgres_step_selects_a_gated_test(self) -> None:
        for key, step in self.steps.items():
            with self.subTest(step=key):
                self.assertTrue(
                    any(step_selects(step, t) for t in self.gated),
                    f"postgres step `{key}` selects no Postgres-gated test (stale name filter?)",
                )

    def test_scan_sees_ignored_and_env_gated_tests(self) -> None:
        ignored = [t for t in self.gated if t["ignored"]]
        env_gated = [t for t in self.gated if not t["ignored"]]
        self.assertGreater(len(ignored), 50, "scanner lost the #[ignore]d Postgres tests")
        self.assertTrue(
            any(t["name"].startswith("postgres_totp") for t in env_gated),
            "scanner lost the env-gated (non-ignored) Postgres tests",
        )

    def test_scan_flags_a_test_the_filters_miss(self) -> None:
        pkg = REPO / "crates" / "bookclerk-library"
        src = r'''
fn pg_url() -> Option<String> { std::env::var("BOOKCLERK_TEST_POSTGRES_URL").ok() }
fn pg_db() -> Option<String> { pg_url() }
#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
async fn apply_is_idempotent_on_postgres() { let s = "}"; let _ = s; }
#[tokio::test]
async fn env_gated_helper_user() { if pg_db().is_none() { return; } }
#[test]
fn sqlite_only() { let c = '{'; let _ = c; }
'''
        gated = postgres_gated_tests(REPO, {pkg / "src" / "fake.rs": src})
        self.assertEqual(
            sorted((t["name"], t["ignored"], t["target"]) for t in gated),
            [("apply_is_idempotent_on_postgres", True, "lib"), ("env_gated_helper_user", False, "lib")],
        )
        selected = {t["name"] for t in gated if any(step_selects(s, t) for s in self.steps.values())}
        self.assertEqual(selected, set())


if __name__ == "__main__":
    unittest.main()
