"""Selective CI planner for Bookclerk.

Stage 1 (affected analysis) maps changed paths to packages, target types and
non-Cargo surfaces using ``cargo metadata --no-deps`` plus the small
declaration file ``relations.toml`` (relationships Cargo cannot describe).

Stage 2 (prerequisite expansion) turns each selected check into the builds,
installed/staged guests and pinned binaries it needs on a clean runner.

Every package, check and prerequisite records *why* it was selected. Anything
the planner cannot place fails closed to the full suite.
"""

from __future__ import annotations

import json
import re
import subprocess
import tomllib
from collections import defaultdict
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

RELATIONS_FILE = Path(__file__).resolve().parent / "relations.toml"

# Paths that always force the full suite (exact or prefix).
FULL_SUITE_PATHS = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "scripts/ci-plan.py",
        "scripts/ci-exec.py",
        "scripts/check-store-free-hosts.sh",
        "scripts/generate-api-docs.sh",
    }
)

FULL_SUITE_PATH_PREFIXES = (
    ".cargo/",
    ".github/workflows/",
    "scripts/ci_plan/",
    "fuzz/",
)

# Crates documented/linted as the public plugin authoring surface.
PUBLISH_CRATES = frozenset(
    {
        "bookclerk-plugin-abi",
        "bookclerk-plugin-manifest",
        "bookclerk-plugin-sdk",
    }
)

E2E_PACKAGE = "bookclerk-plugin-e2e"
SDK_PACKAGE = "bookclerk-plugin-sdk"
STORE_FREE_PACKAGES = frozenset({"bookclerk-cli", "bookclerkd", "bookclerk-plugin-host"})
UI_HOST_PACKAGE = "bookclerkd"

# Non-Cargo surfaces.
UI_PREFIX = "ui/"
TS_SDK_PREFIX = "packages/plugin-sdk/"
PY_SDK_PREFIX = "packages/plugin-sdk-python/"
ABI_SYNC_PATHS = frozenset(
    {
        "scripts/gen-plugin-abi.py",
        "scripts/sync-workerd-pin.py",
        "workerd.pin.json",
    }
)
DOCS_PREFIX = "docs/"

KNOWN_TOP_LEVEL = frozenset(
    {
        ".cargo",
        ".cursor",
        ".devcontainer",
        ".github",
        ".vscode",
        "BookclerkFiles",
        "assets",
        "config",
        "crates",
        "docs",
        "examples",
        "fuzz",
        "packages",
        "scripts",
        "third_party",
        "ui",
        "AGENTS.md",
        "Cargo.lock",
        "Cargo.toml",
        "CHANGELOG.md",
        "CODE_OF_CONDUCT.md",
        "CONTRIBUTING.md",
        "LICENSE",
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "README.md",
        "SECURITY.md",
        "deny.toml",
        "osv-scanner.toml",
        "rust-toolchain.toml",
        "rustfmt.toml",
        "workerd.pin.json",
        ".gitattributes",
        ".gitignore",
        ".gitmodules",
    }
)

# Package-relative directories whose files are only compiled into test,
# bench or example targets (Cargo's default target auto-discovery roots).
TEST_INPUT_DIRS = {"tests": "tests", "benches": "benches", "examples": "examples"}

# Jobs in ``.github/workflows/ci.yml`` and the checks each job executes.
JOB_CHECKS: dict[str, tuple[str, ...]] = {
    "check": (
        "ui",
        "plugin_sdk_abi",
        "python_sdk",
        "author_surface",
        "fmt",
        "clippy",
        "clippy_publish",
        "api_docs",
        "doctest",
        "store_free",
        "rust_test",
        "e2e",
    ),
    "release": ("release",),
    "confinement": ("confinement",),
    "native-gateway": ("native_gateway",),
    "tray": ("tray",),
    "postgres": ("postgres",),
}
CHECK_JOB = {check: job for job, checks in JOB_CHECKS.items() for check in checks}
ALL_CHECKS = tuple(CHECK_JOB)

# Checks in the ``check`` job that need no Cargo toolchain.
NON_CARGO_CHECKS = frozenset({"ui", "python_sdk"})

PREREQ_ORDER = ("build", "ensure_workerd", "install_platform", "stage_plugins")


class PlanError(Exception):
    """Hard planner failure — callers escalate to the full suite."""


# ---------------------------------------------------------------------------
# Workspace model


@dataclass
class PackageInfo:
    """One workspace member as seen by the planner."""

    name: str
    manifest_dir: str  # repo-relative, forward slashes, no trailing slash
    tier: str | None = None  # "platform" | "optional" | "example" | None
    supports_doctest: bool = False
    has_benches: bool = False
    has_examples: bool = False
    # Package-relative dirs holding test/bench/example target sources.
    test_input_dirs: dict[str, str] = field(default_factory=dict)


@dataclass
class PackageIndex:
    """Workspace members and their path-dependency edges by kind."""

    by_name: dict[str, PackageInfo]
    dirs: list[tuple[str, str]]  # (manifest_dir, name), longest first
    # dep -> consumers via normal or build edges (compiled into consumers).
    compile_consumers: dict[str, set[str]]
    # dep -> consumers via dev edges (tests/examples/benches/doctests only).
    dev_consumers: dict[str, set[str]]
    workspace_root: str


@dataclass
class Guest:
    """A first-party plugin discovered from ``plugin.toml``."""

    id: str
    tier: str
    runtime: str
    rel_dir: str
    package: str | None


@dataclass
class Relations:
    """Parsed ``relations.toml``."""

    embeds: list[dict[str, Any]]
    test_inputs: list[dict[str, Any]]
    fixtures: list[dict[str, Any]]
    test_guests: dict[str, dict[str, Any]]
    feature_tests: list[dict[str, Any]]
    postgres_steps: dict[str, str]
    e2e_full_packages: frozenset[str]
    e2e_full_paths: tuple[str, ...]
    runtime_smoke: tuple[str, ...]
    runtime_smoke_packages: frozenset[str]
    platform_jobs: dict[str, frozenset[str]]
    native_gateway_packages: frozenset[str]
    native_gateway_paths: tuple[str, ...]
    release_shipped: tuple[str, ...]
    release_full_packages: frozenset[str]
    release_full_paths: tuple[str, ...]
    reviewed: list[dict[str, str]]


def load_relations(path: str | Path | None = None) -> Relations:
    """Loads and validates ``relations.toml``."""
    p = Path(path) if path else RELATIONS_FILE
    try:
        raw = tomllib.loads(p.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise PlanError(f"relations: cannot read {p}: {exc}") from exc
    e2e = raw.get("staged_e2e", {})
    release = raw.get("release", {})
    jobs = raw.get("platform_jobs", {})
    gateway = raw.get("native_gateway", {})
    return Relations(
        embeds=list(raw.get("embed", [])),
        test_inputs=list(raw.get("test_input", [])),
        fixtures=list(raw.get("fixture", [])),
        test_guests=dict(raw.get("test_guests", {})),
        feature_tests=list(raw.get("feature_tests", [])),
        postgres_steps=dict(raw.get("postgres", {}).get("steps", {})),
        e2e_full_packages=frozenset(e2e.get("full_packages", [])),
        e2e_full_paths=tuple(e2e.get("full_paths", [])),
        runtime_smoke=tuple(e2e.get("runtime_smoke", [])),
        runtime_smoke_packages=frozenset(e2e.get("runtime_smoke_packages", [])),
        platform_jobs={k: frozenset(v) for k, v in jobs.items()},
        native_gateway_packages=frozenset(gateway.get("packages", [])),
        native_gateway_paths=tuple(gateway.get("paths", [])),
        release_shipped=tuple(release.get("shipped", [])),
        release_full_packages=frozenset(release.get("full_packages", [])),
        release_full_paths=tuple(release.get("full_paths", [])),
        reviewed=list(raw.get("reviewed", [])),
    )


def validate_relations(rel: Relations, index: PackageIndex) -> list[str]:
    """Returns problems (unknown packages) in ``rel`` for ``index``."""
    problems: list[str] = []
    names = set(index.by_name)

    def need(pkg: str, where: str) -> None:
        if pkg not in names:
            problems.append(f"{where}: unknown package `{pkg}`")

    for e in rel.embeds:
        need(e["package"], "embed")
    for f in rel.fixtures:
        for check in f.get("checks", []):
            if check not in CHECK_JOB:
                problems.append(f"fixture: unknown check `{check}`")
    for t in rel.test_inputs:
        need(t["package"], "test_input")
    for pkg, spec in rel.test_guests.items():
        need(pkg, "test_guests")
        for b in spec.get("build", []):
            need(b, f"test_guests.{pkg}.build")
    for f in rel.feature_tests:
        need(f["package"], "feature_tests")
    for step, owner in rel.postgres_steps.items():
        need(owner, f"postgres.steps.{step}")
    for pkg in rel.e2e_full_packages | rel.runtime_smoke_packages:
        need(pkg, "staged_e2e")
    for job, pkgs in rel.platform_jobs.items():
        if job not in CHECK_JOB:
            problems.append(f"platform_jobs: unknown check `{job}`")
        for pkg in pkgs:
            need(pkg, f"platform_jobs.{job}")
    for pkg in rel.native_gateway_packages:
        need(pkg, "native_gateway")
    for pkg in (*rel.release_shipped, *rel.release_full_packages):
        need(pkg, "release")
    return problems


def _norm_path(path: str) -> str:
    norm = path.replace("\\", "/")
    while norm.startswith("./"):
        norm = norm[2:]
    return norm


def _repo_rel(path: str, workspace_root: str) -> str:
    p = Path(path).resolve()
    root = Path(workspace_root).resolve()
    try:
        return _norm_path(str(p.relative_to(root)))
    except ValueError:
        return _norm_path(str(p))


_GLOB_CACHE: dict[str, re.Pattern[str]] = {}


def glob_match(path: str, pattern: str) -> bool:
    """Matches repo-relative ``path`` against a ``*``/``**`` glob."""
    rx = _GLOB_CACHE.get(pattern)
    if rx is None:
        out = []
        i = 0
        while i < len(pattern):
            if pattern.startswith("**/", i):
                out.append("(?:.*/)?")
                i += 3
            elif pattern.startswith("**", i):
                out.append(".*")
                i += 2
            elif pattern[i] == "*":
                out.append("[^/]*")
                i += 1
            elif pattern[i] == "?":
                out.append("[^/]")
                i += 1
            else:
                out.append(re.escape(pattern[i]))
                i += 1
        rx = re.compile("^" + "".join(out) + "$")
        _GLOB_CACHE[pattern] = rx
    return bool(rx.match(path))


def load_metadata(
    workspace_root: str | Path | None = None,
    metadata: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """Loads ``cargo metadata --no-deps`` JSON (live or injected)."""
    if metadata is not None:
        return dict(metadata)
    root = Path(workspace_root or Path.cwd())
    try:
        out = subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=root,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise PlanError(f"cargo metadata failed: {exc}") from exc
    return json.loads(out)


def package_index_from_metadata(meta: Mapping[str, Any]) -> PackageIndex:
    """Builds the package index and kind-separated reverse edges."""
    workspace_root = _norm_path(meta.get("workspace_root", ""))
    by_name: dict[str, PackageInfo] = {}
    dirs: list[tuple[str, str]] = []

    for pkg in meta.get("packages", []):
        name = pkg["name"]
        manifest_dir = _repo_rel(str(Path(pkg["manifest_path"]).parent), workspace_root)
        if manifest_dir.startswith("crates/bookclerk-plugins/platform/"):
            tier = "platform"
        elif manifest_dir.startswith("crates/bookclerk-plugins/optional/"):
            tier = "optional"
        elif manifest_dir.startswith("examples/plugins-"):
            tier = "example"
        else:
            tier = None
        info = PackageInfo(name=name, manifest_dir=manifest_dir, tier=tier)
        info.test_input_dirs = dict(TEST_INPUT_DIRS)
        for target in pkg.get("targets", []):
            kinds = target.get("kind") or []
            if "lib" in kinds and target.get("doctest", True) is not False:
                info.supports_doctest = True
            if "bench" in kinds:
                info.has_benches = True
            if "example" in kinds:
                info.has_examples = True
            for kind, label in (("test", "tests"), ("bench", "benches"), ("example", "examples")):
                if kind in kinds:
                    src = _repo_rel(str(Path(target["src_path"]).parent), workspace_root)
                    prefix = manifest_dir + "/"
                    if src.startswith(prefix):
                        info.test_input_dirs.setdefault(src[len(prefix) :], label)
        by_name[name] = info
        dirs.append((manifest_dir, name))

    dirs.sort(key=lambda t: len(t[0]), reverse=True)

    compile_consumers: dict[str, set[str]] = defaultdict(set)
    dev_consumers: dict[str, set[str]] = defaultdict(set)
    for pkg in meta.get("packages", []):
        consumer = pkg["name"]
        for dep in pkg.get("dependencies", []):
            dep_name = dep["name"]
            if not dep.get("path") or dep_name not in by_name:
                continue
            # Cargo metadata: kind null = normal, "build", or "dev". Optional
            # normal edges stay compile edges (no feature resolution here).
            if dep.get("kind") == "dev":
                dev_consumers[dep_name].add(consumer)
            else:
                compile_consumers[dep_name].add(consumer)

    return PackageIndex(
        by_name=by_name,
        dirs=dirs,
        compile_consumers=dict(compile_consumers),
        dev_consumers=dict(dev_consumers),
        workspace_root=workspace_root,
    )


def discover_guests(workspace_root: str | Path, index: PackageIndex) -> dict[str, Guest]:
    """First-party guests by manifest id (platform, optional, examples)."""
    root = Path(workspace_root)
    by_dir = {info.manifest_dir: name for name, info in index.by_name.items()}
    out: dict[str, Guest] = {}
    roots = (
        ("platform", root / "crates/bookclerk-plugins/platform", "*"),
        ("optional", root / "crates/bookclerk-plugins/optional", "*"),
        ("example", root / "examples", "plugins-*"),
    )
    for tier, base, pattern in roots:
        if not base.is_dir():
            continue
        for manifest in sorted(base.glob(f"{pattern}/plugin.toml")):
            try:
                data = tomllib.loads(manifest.read_text(encoding="utf-8"))
            except (OSError, tomllib.TOMLDecodeError) as exc:
                raise PlanError(f"cannot parse {manifest}: {exc}") from exc
            rel_dir = _repo_rel(str(manifest.parent), str(root))
            gid = data.get("id")
            if not gid:
                raise PlanError(f"{manifest} has no id")
            out[gid] = Guest(
                id=gid,
                tier=tier,
                runtime=str(data.get("runtime", "native")),
                rel_dir=rel_dir,
                package=by_dir.get(rel_dir),
            )
    return out


def list_changed_paths(
    base: str,
    head: str,
    workspace_root: str | Path | None = None,
    paths: Sequence[str] | None = None,
) -> list[str]:
    """Changed paths; renames contribute both old and new paths."""
    if paths is not None:
        return sorted({_norm_path(p) for p in paths if p})
    root = Path(workspace_root or Path.cwd())
    try:
        out = subprocess.check_output(
            ["git", "diff", "--name-only", "--no-renames", f"{base}...{head}"],
            cwd=root,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise PlanError(f"git diff failed: {exc}") from exc
    return sorted({_norm_path(line) for line in out.splitlines() if line.strip()})


def package_for_path(path: str, index: PackageIndex) -> str | None:
    """Workspace package owning ``path`` (longest manifest-dir prefix)."""
    norm = _norm_path(path)
    for manifest_dir, name in index.dirs:
        if norm == manifest_dir or norm.startswith(manifest_dir + "/"):
            return name
    return None


def test_input_kind(path: str, info: PackageInfo) -> str | None:
    """``tests`` / ``benches`` / ``examples`` when ``path`` only feeds those targets."""
    rel = _norm_path(path)[len(info.manifest_dir) + 1 :]
    best: tuple[int, str] | None = None
    for d, kind in info.test_input_dirs.items():
        if rel == d or rel.startswith(d + "/"):
            if best is None or len(d) > best[0]:
                best = (len(d), kind)
    return best[1] if best else None


def _top_level(path: str) -> str:
    return path.split("/", 1)[0]


# ---------------------------------------------------------------------------
# Plan model


@dataclass
class PackageSelection:
    """What runs for one package, and why."""

    compiled: bool = False
    tests: bool = False
    # False when only files under the package's own ``tests/`` changed:
    # ``--lib`` / ``src/``-level suites (postgres steps, Windows gateway
    # ``--lib`` checks) are unaffected.
    unit_tests: bool = False
    benches: bool = False
    examples: bool = False
    doctest: bool = False
    docs: bool = False
    why: list[str] = field(default_factory=list)

    def add_why(self, reason: str) -> None:
        if reason not in self.why:
            self.why.append(reason)


@dataclass
class Plan:
    """A selective (or full) CI plan."""

    full_suite: bool = False
    reasons: list[str] = field(default_factory=list)
    changed_paths: list[str] = field(default_factory=list)
    changed_packages: list[str] = field(default_factory=list)
    packages: dict[str, PackageSelection] = field(default_factory=dict)
    # check id -> {"why": [...], "params": {...}, "prereqs": [...]}
    checks: dict[str, dict[str, Any]] = field(default_factory=dict)
    jobs: dict[str, bool] = field(default_factory=dict)

    def mark_full(self, reason: str) -> None:
        if reason not in self.reasons:
            self.reasons.append(reason)
        self.full_suite = True

    def selected(self, check: str) -> bool:
        return check in self.checks

    def params(self, check: str) -> dict[str, Any]:
        return self.checks.get(check, {}).get("params", {})

    def prereqs(self, check: str) -> list[dict[str, Any]]:
        return self.checks.get(check, {}).get("prereqs", [])

    def lint_packages(self) -> list[str]:
        return sorted(self.packages)

    def test_packages(self) -> list[str]:
        return sorted(n for n, s in self.packages.items() if s.tests and n != E2E_PACKAGE)

    def doc_packages(self) -> list[str]:
        return sorted(n for n, s in self.packages.items() if s.docs)

    def doctest_packages(self) -> list[str]:
        return sorted(n for n, s in self.packages.items() if s.doctest)


def _select(plan: Plan, check: str, why: Iterable[str], **params: Any) -> None:
    entry = plan.checks.setdefault(check, {"why": [], "params": {}, "prereqs": []})
    for w in why:
        if w not in entry["why"]:
            entry["why"].append(w)
    entry["params"].update(params)


# ---------------------------------------------------------------------------
# Stage 1 + 2


class _Surfaces:
    """Non-Cargo inputs touched by the change set."""

    def __init__(self) -> None:
        self.ui: list[str] = []
        self.ts_sdk: list[str] = []
        self.python_sdk: list[str] = []
        self.abi: list[str] = []
        self.e2e_full: list[str] = []
        self.e2e_ids: dict[str, str] = {}
        self.release_full: list[str] = []
        self.native_gateway: list[str] = []
        self.fixture_checks: dict[str, list[str]] = {}


def build_plan(
    changed_paths: Sequence[str],
    index: PackageIndex,
    relations: Relations | None = None,
    *,
    force_full: bool = False,
    force_full_reason: str | None = None,
    guests: Mapping[str, Guest] | None = None,
) -> Plan:
    """Plans checks for ``changed_paths``."""
    rel = relations or load_relations()
    guest_map = dict(guests) if guests is not None else discover_guests(index.workspace_root, index)
    plan = Plan(changed_paths=sorted({_norm_path(p) for p in changed_paths}))

    if force_full:
        plan.mark_full(force_full_reason or "forced by caller")
        return _finalize_full(plan, index, rel, guest_map)
    if not plan.changed_paths:
        return plan

    compiled_seeds: dict[str, list[str]] = defaultdict(list)
    test_seeds: dict[str, dict[str, list[str]]] = defaultdict(lambda: defaultdict(list))
    e2e_direct: list[str] = []
    surf = _Surfaces()
    guest_by_dir = {g.rel_dir: g for g in guest_map.values()}
    changed_pkgs: set[str] = set()

    for path in plan.changed_paths:
        if path in FULL_SUITE_PATHS or any(path.startswith(p) for p in FULL_SUITE_PATH_PREFIXES):
            plan.mark_full(f"global path {path}")
            continue
        top = _top_level(path)
        if top not in KNOWN_TOP_LEVEL or ("/" not in path and path not in KNOWN_TOP_LEVEL):
            plan.mark_full(f"unknown top-level path {path}")
            continue
        pkg = package_for_path(path, index)
        if path.endswith("Cargo.toml") and pkg is None:
            plan.mark_full(f"unresolved package manifest {path}")
            continue

        classified = False
        fixture = next(
            (f for f in rel.fixtures if any(glob_match(path, p) for p in f["paths"])), None
        )
        if fixture is not None:
            classified = True
            for check in fixture.get("checks", []):
                surf.fixture_checks.setdefault(check, []).append(path)
        elif pkg is not None:
            classified = True
            changed_pkgs.add(pkg)
            # Smoke-only inputs (its target, support and fixture guest) do
            # not feed the staged installation suite.
            smoke_only = any(glob_match(path, p) for p in rel.native_gateway_paths)
            if pkg == E2E_PACKAGE and not smoke_only:
                e2e_direct.append(path)
            kind = test_input_kind(path, index.by_name[pkg])
            if kind is None:
                compiled_seeds[pkg].append(f"{'smoke input' if smoke_only else 'changed'} {path}")
            else:
                test_seeds[pkg][kind].append(f"{kind} input {path}")

        for embed in rel.embeds:
            if path in embed["paths"]:
                compiled_seeds[embed["package"]].append(f"embeds {path}")
                if embed.get("e2e") == "workerd-runtime":
                    for g in guest_map.values():
                        if g.runtime == "workerd":
                            surf.e2e_ids.setdefault(g.id, f"workerd runtime embed {path}")
                classified = True
        for ti in rel.test_inputs:
            if any(glob_match(path, p) for p in ti["paths"]):
                # Declared inputs may feed ``src/`` test modules too.
                test_seeds[ti["package"]]["declared"].append(f"test input {path}")
                classified = True

        if path.startswith(UI_PREFIX):
            surf.ui.append(path)
            classified = True
        if path.startswith(TS_SDK_PREFIX):
            surf.ts_sdk.append(path)
            classified = True
        if path.startswith(PY_SDK_PREFIX):
            surf.python_sdk.append(path)
            classified = True
        if path in ABI_SYNC_PATHS:
            surf.abi.append(path)
            classified = True
        if path.startswith(DOCS_PREFIX):
            classified = True
        if pkg is None and path.startswith("examples/plugins-"):
            guest_dir = "/".join(path.split("/")[:2])
            guest = guest_by_dir.get(guest_dir)
            if guest is not None:
                surf.e2e_ids.setdefault(guest.id, f"guest files {path}")
                classified = True
        if any(glob_match(path, p) for p in rel.e2e_full_paths):
            surf.e2e_full.append(path)
        if any(glob_match(path, p) for p in rel.release_full_paths):
            surf.release_full.append(path)
        if any(glob_match(path, p) for p in rel.native_gateway_paths):
            surf.native_gateway.append(path)

        if not classified:
            plan.mark_full(f"unclassified path {path}")

    plan.changed_packages = sorted(changed_pkgs)
    if plan.full_suite:
        return _finalize_full(plan, index, rel, guest_map)

    # Compiled closure over normal/build edges.
    sel: dict[str, PackageSelection] = {}
    compiled: set[str] = set()
    stack = [(p, w) for p, ws in compiled_seeds.items() for w in ws]
    while stack:
        name, why = stack.pop()
        s = sel.setdefault(name, PackageSelection())
        s.add_why(why)
        if name in compiled:
            continue
        compiled.add(name)
        for consumer in sorted(index.compile_consumers.get(name, ())):
            stack.append((consumer, f"depends on {name}"))

    for name in compiled:
        info = index.by_name[name]
        s = sel[name]
        s.compiled = s.tests = s.unit_tests = s.docs = True
        s.benches = info.has_benches
        s.examples = info.has_examples
        s.doctest = info.supports_doctest

    # Dev edges: consumers' tests (and doctests), no further propagation.
    for name in sorted(compiled):
        for consumer in sorted(index.dev_consumers.get(name, ())):
            if consumer in compiled:
                continue
            info = index.by_name[consumer]
            s = sel.setdefault(consumer, PackageSelection())
            s.add_why(f"dev-depends on {name}")
            s.tests = s.unit_tests = True
            s.benches = s.benches or info.has_benches
            s.examples = s.examples or info.has_examples
            s.doctest = s.doctest or info.supports_doctest

    # Tests that launch a changed workspace executable re-run (no further
    # propagation); building it for them is a prerequisite, not a selection.
    for consumer, spec in sorted(rel.test_guests.items()):
        launched = sorted(set(spec.get("build", [])) & compiled)
        if not launched or consumer in compiled:
            continue
        s = sel.setdefault(consumer, PackageSelection())
        for g in launched:
            s.add_why(f"tests launch {g}")
        s.tests = s.unit_tests = True

    # Test-input changes: only the owning target type.
    for name, kinds in test_seeds.items():
        s = sel.setdefault(name, PackageSelection())
        for kind, whys in kinds.items():
            for w in whys:
                s.add_why(w)
            if kind == "tests":
                s.tests = True
            elif kind == "declared":
                s.tests = s.unit_tests = True
            elif kind == "benches":
                s.benches = True
            elif kind == "examples":
                s.examples = True

    plan.packages = dict(sorted(sel.items()))
    _select_checks(plan, index, rel, guest_map, surf, compiled, e2e_direct)
    _expand_prereqs(plan, index, rel, guest_map)
    _derive_jobs(plan)
    return plan


def _why(sel: Mapping[str, PackageSelection], names: Iterable[str], label: str) -> list[str]:
    return [f"{label} {n}" for n in sorted(names) if n in sel]


def _select_checks(
    plan: Plan,
    index: PackageIndex,
    rel: Relations,
    guest_map: Mapping[str, Guest],
    surf: _Surfaces,
    compiled: set[str],
    e2e_direct: list[str],
) -> None:
    sel = plan.packages
    lint = set(sel)
    tests = set(plan.test_packages())

    if lint:
        _select(plan, "fmt", [f"{len(lint)} Rust package(s) affected"])
        _select(plan, "clippy", [f"{len(lint)} package(s) to lint"], packages=sorted(lint))
    publish = lint & PUBLISH_CRATES
    if publish:
        _select(plan, "clippy_publish", _why(sel, publish, "publish crate"))
    if SDK_PACKAGE in compiled:
        _select(plan, "author_surface", [f"{SDK_PACKAGE} compiled"])

    abi_why = (
        [f"path {p}" for p in surf.ts_sdk + surf.python_sdk + surf.abi]
        + _why(sel, compiled & PUBLISH_CRATES, "publish crate compiled")
    )
    if abi_why:
        _select(plan, "plugin_sdk_abi", abi_why)
        _select(plan, "python_sdk", abi_why)
    for check, paths in surf.fixture_checks.items():
        _select(plan, check, [f"shared fixture {p}" for p in paths])

    ui_why = [f"path {p}" for p in surf.ui]
    if UI_HOST_PACKAGE in compiled:
        ui_why.append(f"{UI_HOST_PACKAGE} serves ui/dist")
    if ui_why:
        _select(plan, "ui", ui_why)

    doc_pkgs = plan.doc_packages()
    if doc_pkgs or surf.ts_sdk or surf.ui or surf.python_sdk:
        _select(
            plan,
            "api_docs",
            [f"{len(doc_pkgs)} compiled package(s)"] if doc_pkgs else ["non-Rust doc surface"],
            rust_packages=doc_pkgs,
            typescript=bool(surf.ts_sdk),
            ui=bool(surf.ui),
            python=bool(surf.python_sdk),
        )
    doctests = plan.doctest_packages()
    if doctests:
        _select(plan, "doctest", [f"{len(doctests)} doctestable package(s)"], packages=doctests)

    store_free = compiled & STORE_FREE_PACKAGES
    if store_free:
        _select(plan, "store_free", _why(sel, store_free, "compiled"))

    benches = sorted(n for n, s in sel.items() if s.benches and index.by_name[n].has_benches)
    examples = sorted(n for n, s in sel.items() if s.examples and index.by_name[n].has_examples)
    feature_tests = [
        {"package": f["package"], "features": list(f["features"])}
        for f in rel.feature_tests
        if f["package"] in tests
    ]
    if tests or benches or examples:
        _select(
            plan,
            "rust_test",
            [f"{len(tests)} package(s) with tests"],
            packages=sorted(tests),
            benches=benches,
            examples=examples,
            feature_tests=feature_tests,
        )

    # E2E: full beats subsets; subsets union; prerequisites add nothing.
    full_why = [f"e2e crate {p}" for p in e2e_direct]
    full_why += [
        f"direct source change in {n}"
        for n in sorted(rel.e2e_full_packages)
        if n in sel and any(w.startswith("changed ") for w in sel[n].why)
    ]
    full_why += [f"inventory path {p}" for p in surf.e2e_full]
    if full_why:
        _select(plan, "e2e", full_why, scope="full")
    else:
        ids: dict[str, str] = dict(surf.e2e_ids)
        for g in guest_map.values():
            if g.package and g.package in compiled:
                ids.setdefault(g.id, f"guest package {g.package} compiled")
        if compiled & rel.runtime_smoke_packages:
            for gid in rel.runtime_smoke:
                ids.setdefault(gid, "runtime smoke for shared host runtime")
        if ids:
            _select(
                plan,
                "e2e",
                [f"{gid}: {w}" for gid, w in sorted(ids.items())],
                scope=sorted(ids),
            )

    unit = {n for n, s in sel.items() if s.unit_tests}
    for check in ("confinement", "tray"):
        hit = lint & rel.platform_jobs.get(check, frozenset())
        if hit:
            _select(plan, check, _why(sel, hit, "affected"))

    # Shipped launch-path code (Cargo-compiled closure), not test-only edits.
    gateway_why = _why(sel, compiled & rel.native_gateway_packages, "launch path compiled")
    gateway_why += [f"smoke input {p}" for p in surf.native_gateway]
    if gateway_why:
        _select(plan, "native_gateway", gateway_why)

    steps = [step for step, owner in rel.postgres_steps.items() if owner in unit]
    if steps:
        owners = sorted({rel.postgres_steps[s] for s in steps})
        _select(plan, "postgres", [f"{o} tests selected" for o in owners], steps=steps)

    release_full_why = [f"packaging input {p}" for p in surf.release_full]
    release_full_why += [
        f"direct change in {n}"
        for n in sorted(rel.release_full_packages)
        if n in sel and any(w.startswith("changed ") for w in sel[n].why)
    ]
    shipped = [n for n in rel.release_shipped if n in compiled]
    if release_full_why:
        _select(plan, "release", release_full_why, mode="full")
    elif shipped:
        _select(plan, "release", [f"shipped binary {n} compiled" for n in shipped], mode="affected", packages=shipped)


def _finalize_full(
    plan: Plan, index: PackageIndex, rel: Relations, guest_map: Mapping[str, Guest]
) -> Plan:
    """Every package, check and prerequisite at full scope."""
    reason = "full suite: " + "; ".join(plan.reasons)
    plan.packages = {}
    for name, info in sorted(index.by_name.items()):
        plan.packages[name] = PackageSelection(
            compiled=True,
            tests=True,
            benches=info.has_benches,
            examples=info.has_examples,
            doctest=info.supports_doctest,
            docs=True,
            why=["full suite"],
        )
    why = [reason]
    plan.checks = {}
    for check in ALL_CHECKS:
        _select(plan, check, why)
    plan.checks["clippy"]["params"] = {"workspace": True}
    plan.checks["api_docs"]["params"] = {"all": True}
    plan.checks["doctest"]["params"] = {"workspace": True}
    plan.checks["rust_test"]["params"] = {
        "workspace": True,
        "exclude": [E2E_PACKAGE],
        "benches": sorted(n for n, i in index.by_name.items() if i.has_benches),
        "examples": sorted(n for n, i in index.by_name.items() if i.has_examples),
        "feature_tests": [
            {"package": f["package"], "features": list(f["features"])} for f in rel.feature_tests
        ],
    }
    plan.checks["e2e"]["params"] = {"scope": "full"}
    plan.checks["release"]["params"] = {"mode": "full"}
    plan.checks["postgres"]["params"] = {"steps": list(rel.postgres_steps)}
    _expand_prereqs(plan, index, rel, guest_map)
    _derive_jobs(plan)
    return plan


def _prereq(plan: Plan, check: str, kind: str, why: str, **args: Any) -> None:
    items = plan.checks[check]["prereqs"]
    for item in items:
        if item["kind"] == kind:
            if "packages" in args:
                merged = list(item.get("packages", []))
                for p in args["packages"]:
                    if p not in merged:
                        merged.append(p)
                item["packages"] = merged
            if why not in item["why"]:
                item["why"].append(why)
            return
    items.append({"kind": kind, "why": [why], **args})
    items.sort(key=lambda i: PREREQ_ORDER.index(i["kind"]))


def _expand_prereqs(
    plan: Plan, index: PackageIndex, rel: Relations, guest_map: Mapping[str, Guest]
) -> None:
    """Stage 2: builds, pinned binaries, installs and staging per check."""
    if plan.selected("rust_test"):
        params = plan.params("rust_test")
        if params.get("workspace"):
            tested = [n for n in index.by_name if n not in params.get("exclude", [])]
        else:
            tested = list(params.get("packages", []))
        tested += [f["package"] for f in params.get("feature_tests", [])]
        for pkg in sorted(set(tested)):
            spec = rel.test_guests.get(pkg)
            if not spec:
                continue
            if spec.get("build"):
                _prereq(plan, "rust_test", "build", f"{pkg} tests launch them", packages=list(spec["build"]))
            if spec.get("ensure_workerd"):
                _prereq(plan, "rust_test", "ensure_workerd", f"{pkg} tests spawn through pinned workerd")

    if plan.selected("e2e"):
        scope = plan.params("e2e")["scope"]
        if scope == "full":
            ids = sorted(guest_map)
        else:
            ids = sorted(set(scope) | {g.id for g in guest_map.values() if g.tier == "platform"})
        native = [
            guest_map[i].package
            for i in ids
            if i in guest_map and guest_map[i].runtime != "workerd" and guest_map[i].package
        ]
        _prereq(
            plan,
            "e2e",
            "build",
            "launcher, jail and native guests for the staged installation",
            packages=["bookclerk-jail", "bookclerk-workerd", *sorted(set(native))],
        )
        _prereq(plan, "e2e", "ensure_workerd", "guests run behind pinned workerd")
        _prereq(plan, "e2e", "install_platform", "platform guests live in FILES_DIR")
        staged = [i for i in ids if i in guest_map and guest_map[i].tier != "platform"]
        if scope == "full":
            _prereq(plan, "e2e", "stage_plugins", "full installation", all=True)
        else:
            _prereq(plan, "e2e", "stage_plugins", "scoped guests", plugins=staged)

    if plan.selected("postgres") and "rpc_like" in plan.params("postgres").get("steps", []):
        _prereq(
            plan,
            "postgres",
            "build",
            "rpc_like spawns the postgres guest through workerd + jail",
            packages=["bookclerk-plugin-database-postgres", "bookclerk-workerd", "bookclerk-jail"],
        )
        _prereq(plan, "postgres", "ensure_workerd", "rpc_like spawns through pinned workerd")

    if plan.selected("native_gateway"):
        _prereq(
            plan,
            "native_gateway",
            "build",
            "the smoke spawns its guest through bookclerk-jail + bookclerk-workerd",
            packages=["bookclerk-jail", "bookclerk-workerd"],
        )
        _prereq(plan, "native_gateway", "ensure_workerd", "the front door runs pinned workerd")


def _derive_jobs(plan: Plan) -> None:
    plan.jobs = {job: any(c in plan.checks for c in checks) for job, checks in JOB_CHECKS.items()}


def full_plan(
    index: PackageIndex,
    relations: Relations | None = None,
    *,
    reason: str,
    guests: Mapping[str, Guest] | None = None,
    changed_paths: Sequence[str] = (),
) -> Plan:
    """Full-suite plan (``main`` / ``merge_group`` / shadow execution)."""
    return build_plan(
        changed_paths,
        index,
        relations,
        force_full=True,
        force_full_reason=reason,
        guests=guests,
    )


def plan_from_event(
    *,
    base: str | None,
    head: str | None,
    workspace_root: str | Path | None = None,
    metadata: Mapping[str, Any] | None = None,
    paths: Sequence[str] | None = None,
    force_full: bool = False,
    relations: Relations | None = None,
) -> Plan:
    """Plans from a git range (or explicit paths); errors escalate to full."""
    changed: list[str] = []
    try:
        meta = load_metadata(workspace_root, metadata)
        index = package_index_from_metadata(meta)
        rel = relations or load_relations()
        problems = validate_relations(rel, index)
        if problems:
            raise PlanError("; ".join(problems))
        guests = discover_guests(workspace_root or index.workspace_root, index)
        if paths is None and (not base or not head):
            raise PlanError("base and head SHAs are required unless --paths is provided")
        changed = list_changed_paths(base or "", head or "", workspace_root, paths)
        return build_plan(
            changed,
            index,
            rel,
            force_full=force_full,
            force_full_reason="forced (main / merge_group)",
            guests=guests,
        )
    except PlanError as exc:
        # A recoverable error (for example an unavailable diff) still runs the
        # normal full plan. An incomplete handwritten plan is not executable:
        # without metadata or the guest inventory, resolve must fail.
        reason = f"planner error: {exc}"
        try:
            meta = load_metadata(workspace_root, metadata)
            index = package_index_from_metadata(meta)
            rel = relations or load_relations()
            guests = discover_guests(workspace_root or index.workspace_root, index)
        except PlanError as blocked:
            raise PlanError(
                f"{reason}; cannot build a prerequisite-complete full plan "
                f"(metadata, relations, and guest inventory are required): {blocked}"
            ) from blocked
        return build_plan(
            changed, index, rel, force_full=True, force_full_reason=reason, guests=guests
        )


# ---------------------------------------------------------------------------
# Serialization


def plan_to_dict(plan: Plan) -> dict[str, Any]:
    return asdict(plan)


def plan_from_dict(data: Mapping[str, Any]) -> Plan:
    plan = Plan(
        full_suite=bool(data["full_suite"]),
        reasons=list(data["reasons"]),
        changed_paths=list(data["changed_paths"]),
        changed_packages=list(data["changed_packages"]),
        checks={k: dict(v) for k, v in data["checks"].items()},
        jobs=dict(data["jobs"]),
    )
    plan.packages = {k: PackageSelection(**v) for k, v in data["packages"].items()}
    return plan


def plan_to_json(plan: Plan) -> str:
    return json.dumps(plan_to_dict(plan), indent=2, sort_keys=True) + "\n"


def _fmt_prereq(p: Mapping[str, Any]) -> str:
    kind = p["kind"]
    if kind == "build":
        return "build " + ", ".join(p["packages"])
    if kind == "stage_plugins":
        return "stage all" if p.get("all") else "stage " + (", ".join(p["plugins"]) or "(platform only)")
    return kind.replace("_", "-")


def plan_to_summary(plan: Plan, title: str = "CI plan") -> str:
    lines = [f"## {title}", "", f"- **full_suite:** `{plan.full_suite}`"]
    for r in plan.reasons:
        lines.append(f"  - {r}")
    lines.append(f"- **changed paths:** {len(plan.changed_paths)}")
    lines.append(f"- **changed packages:** {', '.join(plan.changed_packages) or '(none)'}")
    lines.append("")
    lines.append("### Jobs")
    lines.append("")
    for job, on in plan.jobs.items():
        lines.append(f"- `{job}`: **{'run' if on else 'skip'}**")
    lines.append("")
    lines.append("### Checks")
    lines.append("")
    lines.append("| Check | Why | Prerequisites |")
    lines.append("| --- | --- | --- |")
    for check in ALL_CHECKS:
        if check not in plan.checks:
            lines.append(f"| `{check}` | skip: not affected | |")
            continue
        entry = plan.checks[check]
        why = "; ".join(entry["why"][:4]) + (" …" if len(entry["why"]) > 4 else "")
        pre = "; ".join(_fmt_prereq(p) for p in entry["prereqs"])
        lines.append(f"| `{check}` | {why} | {pre} |")
    if not plan.full_suite and plan.packages:
        lines.append("")
        lines.append("### Packages")
        lines.append("")
        lines.append("| Package | Scope | Why |")
        lines.append("| --- | --- | --- |")
        for name, s in plan.packages.items():
            scope = [
                k
                for k, on in (
                    ("compiled", s.compiled),
                    ("tests", s.tests),
                    ("benches", s.benches),
                    ("examples", s.examples),
                    ("doctest", s.doctest),
                )
                if on
            ] or ["lint"]
            lines.append(f"| `{name}` | {', '.join(scope)} | {'; '.join(s.why[:3])} |")
    lines.append("")
    return "\n".join(lines)
