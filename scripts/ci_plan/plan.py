"""Core planning logic for Bookclerk selective CI."""

from __future__ import annotations

import json
import os
import subprocess
from collections import defaultdict
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

# Paths that always force the full suite (exact or prefix).
FULL_SUITE_PATHS = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "scripts/ci-plan.py",
        "scripts/check-store-free-hosts.sh",
        "scripts/generate-api-docs.sh",
    }
)

FULL_SUITE_PATH_PREFIXES = (
    ".cargo/",
    ".github/workflows/",
    "scripts/ci_plan/",
)

PUBLISH_DOC_CRATES = frozenset(
    {
        "bookclerk-plugin-abi",
        "bookclerk-plugin-manifest",
        "bookclerk-plugin-sdk",
    }
)

CONFINEMENT_NAME_PREFIXES = (
    "bookclerk-sandbox",
    "bookclerk-jail",
    "bookclerk-media",
)

TRAY_PACKAGE = "bookclerk-tray"

# Non-Cargo surfaces (path prefix → plan flag).
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
ABI_SYNC_PREFIXES = (
    "crates/bookclerk-plugin-abi/",
    "crates/bookclerk-plugin-manifest/",
    "crates/bookclerk-plugin-sdk/",
    "packages/plugin-sdk/",
    "packages/plugin-sdk-python/",
)

KNOWN_TOP_LEVEL = frozenset(
    {
        ".cargo",
        ".cursor",
        ".devcontainer",
        ".github",
        ".vscode",
        "BookclerkFiles",
        "config",
        "crates",
        "docs",
        "examples",
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
        ".envrc.example",
        ".gitattributes",
        ".gitignore",
        ".gitmodules",
    }
)

DEP_KINDS = ("normal", "dev", "build")


class PlanError(Exception):
    """Hard planner failure — callers should escalate to full_suite."""


@dataclass
class PackageInfo:
    name: str
    manifest_dir: str  # repo-relative, forward slashes, no trailing slash
    is_platform_plugin: bool = False
    is_optional_plugin: bool = False
    is_example_plugin: bool = False
    # True when cargo metadata reports a lib target with doctest enabled.
    supports_doctest: bool = False


@dataclass
class PackageIndex:
    by_name: dict[str, PackageInfo]
    # Longest-prefix match: sorted dirs descending by length.
    dirs: list[tuple[str, str]]  # (manifest_dir, package_name)
    # name -> set of packages that depend on it (any path dep kind).
    reverse_deps: dict[str, set[str]]
    workspace_root: str


@dataclass
class Plan:
    full_suite: bool = False
    reasons: list[str] = field(default_factory=list)
    rust_packages: list[str] = field(default_factory=list)
    rust_doc_packages: list[str] = field(default_factory=list)
    # Subset of rust_doc_packages safe for ``cargo test -p … --doc``.
    rust_doctest_packages: list[str] = field(default_factory=list)
    ui: bool = False
    ts_sdk: bool = False
    python_sdk: bool = False
    abi_sync: bool = False
    build_app_platform: bool = False
    build_app_optional: bool = False
    build_app_examples: bool = False
    confinement: bool = False
    tray: bool = False
    store_free: bool = False
    release: bool = False
    docs_markdown: bool = False
    changed_paths: list[str] = field(default_factory=list)
    changed_packages: list[str] = field(default_factory=list)
    decisions: dict[str, str] = field(default_factory=dict)

    def mark_full(self, reason: str) -> None:
        if reason not in self.reasons:
            self.reasons.append(reason)
        self.full_suite = True

    def finalize_full_suite(self) -> None:
        """When full_suite, enable every job flag and clear selective lists."""
        if not self.full_suite:
            return
        self.ui = True
        self.ts_sdk = True
        self.python_sdk = True
        self.abi_sync = True
        self.build_app_platform = True
        self.build_app_optional = True
        self.build_app_examples = True
        self.confinement = True
        self.tray = True
        self.store_free = True
        self.release = True
        self.docs_markdown = True
        self.decisions = {
            "full_suite": "forced: " + "; ".join(self.reasons) if self.reasons else "forced",
            "ui": "full_suite",
            "ts_sdk": "full_suite",
            "python_sdk": "full_suite",
            "abi_sync": "full_suite",
            "build_app_platform": "full_suite",
            "build_app_optional": "full_suite",
            "build_app_examples": "full_suite",
            "confinement": "full_suite",
            "tray": "full_suite",
            "store_free": "full_suite",
            "release": "full_suite",
            "docs_markdown": "full_suite",
            "rust_packages": "full_suite (all workspace)",
            "rust_doc_packages": "full_suite (all workspace)",
            "rust_doctest_packages": "full_suite (lib+doctest packages)",
        }


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


def load_metadata(
    workspace_root: str | Path | None = None,
    metadata: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """Load cargo metadata JSON (live or injected)."""
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
    workspace_root = _norm_path(meta.get("workspace_root", ""))
    by_name: dict[str, PackageInfo] = {}
    dirs: list[tuple[str, str]] = []
    name_by_manifest: dict[str, str] = {}

    for pkg in meta.get("packages", []):
        name = pkg["name"]
        manifest_path = pkg["manifest_path"]
        manifest_dir = _repo_rel(str(Path(manifest_path).parent), workspace_root)
        is_platform = manifest_dir.startswith("crates/bookclerk-plugins/platform/")
        is_optional = manifest_dir.startswith("crates/bookclerk-plugins/optional/")
        is_example = manifest_dir.startswith("examples/plugins-")
        supports_doctest = False
        for target in pkg.get("targets", []):
            kinds = target.get("kind") or []
            if "lib" not in kinds:
                continue
            # Cargo omits ``doctest`` only in older metadata; default is enabled.
            if target.get("doctest", True) is not False:
                supports_doctest = True
                break
        info = PackageInfo(
            name=name,
            manifest_dir=manifest_dir,
            is_platform_plugin=is_platform,
            is_optional_plugin=is_optional,
            is_example_plugin=is_example,
            supports_doctest=supports_doctest,
        )
        by_name[name] = info
        dirs.append((manifest_dir, name))
        name_by_manifest[manifest_dir] = name

    dirs.sort(key=lambda t: len(t[0]), reverse=True)

    # Reverse edges from path dependencies listed in each package.
    reverse: dict[str, set[str]] = defaultdict(set)
    for pkg in meta.get("packages", []):
        consumer = pkg["name"]
        for dep in pkg.get("dependencies", []):
            if not dep.get("path"):
                continue
            dep_name = dep["name"]
            if dep_name in by_name:
                reverse[dep_name].add(consumer)

    return PackageIndex(
        by_name=by_name,
        dirs=dirs,
        reverse_deps=dict(reverse),
        workspace_root=workspace_root,
    )


def list_changed_paths(
    base: str,
    head: str,
    workspace_root: str | Path | None = None,
    paths: Sequence[str] | None = None,
) -> list[str]:
    if paths is not None:
        return sorted({_norm_path(p) for p in paths if p})
    root = Path(workspace_root or Path.cwd())
    try:
        out = subprocess.check_output(
            ["git", "diff", "--name-only", f"{base}...{head}"],
            cwd=root,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as exc:
        raise PlanError(f"git diff failed: {exc}") from exc
    return sorted({_norm_path(line) for line in out.splitlines() if line.strip()})


def package_for_path(path: str, index: PackageIndex) -> str | None:
    """Return workspace package owning ``path``, or None."""
    norm = _norm_path(path)
    for manifest_dir, name in index.dirs:
        if norm == manifest_dir or norm.startswith(manifest_dir + "/"):
            return name
    return None


def reverse_closure(seeds: Iterable[str], index: PackageIndex) -> set[str]:
    """Changed packages plus reverse-transitive workspace dependents."""
    out: set[str] = set()
    stack = list(seeds)
    while stack:
        name = stack.pop()
        if name in out:
            continue
        if name not in index.by_name:
            continue
        out.add(name)
        for consumer in index.reverse_deps.get(name, ()):
            if consumer not in out:
                stack.append(consumer)
    return out


def is_confinement_package(name: str) -> bool:
    return any(name == p or name.startswith(p) for p in CONFINEMENT_NAME_PREFIXES)


def _top_level(path: str) -> str:
    norm = _norm_path(path)
    if "/" not in norm:
        return norm
    return norm.split("/", 1)[0]


def doctest_packages(names: Iterable[str], index: PackageIndex) -> list[str]:
    """Packages that support ``cargo test -p <name> --doc``."""
    return sorted(
        name
        for name in names
        if name in index.by_name and index.by_name[name].supports_doctest
    )


def _assign_rust_lists(plan: Plan, index: PackageIndex, package_names: Iterable[str]) -> None:
    names = sorted(package_names)
    plan.rust_packages = names
    plan.rust_doc_packages = list(names)
    plan.rust_doctest_packages = doctest_packages(names, index)


def build_plan(
    changed_paths: Sequence[str],
    index: PackageIndex,
    *,
    force_full: bool = False,
    force_full_reason: str | None = None,
) -> Plan:
    plan = Plan(changed_paths=sorted({_norm_path(p) for p in changed_paths}))

    if force_full:
        plan.mark_full(force_full_reason or "forced by caller")
        plan.finalize_full_suite()
        _assign_rust_lists(plan, index, index.by_name)
        plan.changed_packages = []
        return plan

    if not plan.changed_paths:
        plan.decisions["empty"] = "no changed paths — skip selective work"
        return plan

    changed_pkgs: set[str] = set()

    for path in plan.changed_paths:
        # Full-suite triggers.
        if path in FULL_SUITE_PATHS or any(
            path == p.rstrip("/") or path.startswith(p) for p in FULL_SUITE_PATH_PREFIXES
        ):
            plan.mark_full(f"global path {path}")
            continue

        top = _top_level(path)
        # Unknown top-level (new root file/dir not in allowlist).
        if top not in KNOWN_TOP_LEVEL and path not in KNOWN_TOP_LEVEL:
            # Allow files under known trees only; stray top-level → full.
            if "/" not in path or top not in KNOWN_TOP_LEVEL:
                plan.mark_full(f"unknown top-level path {path}")
                continue

        # Deleted/changed Cargo.toml under a path that is not a known package.
        if path.endswith("/Cargo.toml") or path.endswith("Cargo.toml"):
            pkg = package_for_path(path, index)
            if pkg is None and path != "Cargo.toml":
                plan.mark_full(f"unresolved package manifest {path}")
                continue

        classified = False
        pkg = package_for_path(path, index)
        if pkg is not None:
            changed_pkgs.add(pkg)
            classified = True

        # Non-Cargo surfaces (explicit classifiers).
        if path.startswith(UI_PREFIX) or path == "ui":
            plan.ui = True
            plan.decisions.setdefault("ui", f"path {path}")
            classified = True
        if path.startswith(TS_SDK_PREFIX):
            plan.ts_sdk = True
            plan.decisions.setdefault("ts_sdk", f"path {path}")
            plan.abi_sync = True
            plan.decisions.setdefault("abi_sync", f"path {path}")
            classified = True
        if path.startswith(PY_SDK_PREFIX):
            plan.python_sdk = True
            plan.decisions.setdefault("python_sdk", f"path {path}")
            plan.abi_sync = True
            plan.decisions.setdefault("abi_sync", f"path {path}")
            classified = True
        if path in ABI_SYNC_PATHS or any(path.startswith(p) for p in ABI_SYNC_PREFIXES):
            plan.abi_sync = True
            plan.decisions.setdefault("abi_sync", f"path {path}")
            classified = True
        if path.startswith("docs/") or path == "docs":
            plan.docs_markdown = True
            plan.decisions.setdefault("docs_markdown", f"path {path}")
            classified = True
        # Non-Cargo plugin examples still stage via build-app --examples.
        if path.startswith("examples/plugins-") and pkg is None:
            plan.build_app_examples = True
            plan.decisions.setdefault("build_app_examples", f"path {path}")
            classified = True

        # Fail closed: known roots can still host paths with no classifier
        # (third_party vendored trees, .github/actions, arbitrary scripts/, etc.).
        if not classified:
            plan.mark_full(f"unclassified path {path}")

    if plan.full_suite:
        plan.finalize_full_suite()
        _assign_rust_lists(plan, index, index.by_name)
        plan.changed_packages = sorted(changed_pkgs)
        return plan

    affected = reverse_closure(changed_pkgs, index)
    plan.changed_packages = sorted(changed_pkgs)
    _assign_rust_lists(plan, index, affected)

    for name in affected:
        info = index.by_name[name]
        if is_confinement_package(name):
            plan.confinement = True
            plan.decisions.setdefault("confinement", f"package {name} in affected set")
        if name == TRAY_PACKAGE:
            plan.tray = True
            plan.decisions.setdefault("tray", f"package {name} in affected set")
        if info.is_platform_plugin or name in (
            "bookclerk-cli",
            "bookclerkd",
            "bookclerk-media-worker",
            "bookclerk-jail",
            "bookclerk-workerd",
            "bookclerk-dev",
        ):
            plan.build_app_platform = True
            plan.decisions.setdefault("build_app_platform", f"package {name}")
        if info.is_optional_plugin:
            plan.build_app_optional = True
            plan.decisions.setdefault("build_app_optional", f"package {name}")
        if info.is_example_plugin:
            plan.build_app_examples = True
            plan.decisions.setdefault("build_app_examples", f"package {name}")
        if name in ("bookclerk-cli", "bookclerkd", "bookclerk-plugin-host"):
            plan.store_free = True
            plan.decisions.setdefault("store_free", f"package {name}")
        if name in (
            "bookclerk-cli",
            "bookclerkd",
            "bookclerk-media-worker",
            "bookclerk-jail",
            "bookclerk-workerd",
            "bookclerk-dev",
        ) or info.is_platform_plugin:
            plan.release = True
            plan.decisions.setdefault("release", f"package {name}")
        if name in PUBLISH_DOC_CRATES:
            plan.abi_sync = True
            plan.decisions.setdefault("abi_sync", f"package {name}")

    # Hosts that embed the UI must rebuild UI when bookclerkd changes.
    if "bookclerkd" in affected:
        plan.ui = True
        plan.decisions.setdefault("ui", "bookclerkd embeds ui/dist")

    # Skip reasons for disabled jobs.
    for key, enabled in (
        ("ui", plan.ui),
        ("ts_sdk", plan.ts_sdk),
        ("python_sdk", plan.python_sdk),
        ("abi_sync", plan.abi_sync),
        ("build_app_platform", plan.build_app_platform),
        ("build_app_optional", plan.build_app_optional),
        ("build_app_examples", plan.build_app_examples),
        ("confinement", plan.confinement),
        ("tray", plan.tray),
        ("store_free", plan.store_free),
        ("release", plan.release),
        ("docs_markdown", plan.docs_markdown),
    ):
        if not enabled:
            plan.decisions.setdefault(key, "skip: not affected")

    if plan.rust_packages:
        plan.decisions["rust_packages"] = (
            f"run: {len(plan.rust_packages)} packages "
            f"({len(plan.changed_packages)} directly changed)"
        )
    else:
        plan.decisions["rust_packages"] = "skip: no Cargo packages affected"
    plan.decisions["rust_doc_packages"] = plan.decisions["rust_packages"]
    plan.decisions["rust_doctest_packages"] = (
        f"run: {len(plan.rust_doctest_packages)} lib+doctest packages"
        if plan.rust_doctest_packages
        else "skip: no doctestable packages affected"
    )
    plan.decisions["full_suite"] = "false"

    # If only docs/*.md changed (no code), keep rust empty — docs_markdown alone.
    return plan


def plan_to_json(plan: Plan) -> str:
    data = asdict(plan)
    return json.dumps(data, indent=2, sort_keys=True) + "\n"


def plan_to_summary(plan: Plan) -> str:
    lines = [
        "## CI plan",
        "",
        f"- **full_suite:** `{plan.full_suite}`",
    ]
    if plan.reasons:
        lines.append("- **reasons:**")
        for r in plan.reasons:
            lines.append(f"  - {r}")
    lines.append(f"- **changed paths:** {len(plan.changed_paths)}")
    lines.append(f"- **changed packages:** {', '.join(plan.changed_packages) or '(none)'}")
    lines.append(
        f"- **rust_packages ({len(plan.rust_packages)}):** "
        + (", ".join(plan.rust_packages[:20]) + ("…" if len(plan.rust_packages) > 20 else "")
           if plan.rust_packages
           else "(none)")
    )
    lines.append(
        f"- **rust_doctest_packages ({len(plan.rust_doctest_packages)}):** "
        + (
            ", ".join(plan.rust_doctest_packages[:20])
            + ("…" if len(plan.rust_doctest_packages) > 20 else "")
            if plan.rust_doctest_packages
            else "(none)"
        )
    )
    lines.append("")
    lines.append("### Job decisions")
    lines.append("")
    lines.append("| Job | Decision |")
    lines.append("| --- | --- |")
    for key in sorted(plan.decisions):
        lines.append(f"| `{key}` | {plan.decisions[key]} |")
    lines.append("")
    flags = [
        ("ui", plan.ui),
        ("ts_sdk", plan.ts_sdk),
        ("python_sdk", plan.python_sdk),
        ("abi_sync", plan.abi_sync),
        ("build_app_platform", plan.build_app_platform),
        ("build_app_optional", plan.build_app_optional),
        ("build_app_examples", plan.build_app_examples),
        ("confinement", plan.confinement),
        ("tray", plan.tray),
        ("store_free", plan.store_free),
        ("release", plan.release),
        ("docs_markdown", plan.docs_markdown),
    ]
    lines.append("### Flags")
    lines.append("")
    for name, val in flags:
        lines.append(f"- `{name}`: **{'run' if val else 'skip'}**")
    lines.append("")
    return "\n".join(lines)


def _gh_bool(v: bool) -> str:
    return "true" if v else "false"


def plan_to_github_output(plan: Plan) -> str:
    """Emit GitHub Actions `GITHUB_OUTPUT` lines."""
    pkgs = " ".join(plan.rust_packages)
    doc_pkgs = " ".join(plan.rust_doc_packages)
    doctest_pkgs = " ".join(plan.rust_doctest_packages)
    lines = [
        f"full_suite={_gh_bool(plan.full_suite)}",
        f"ui={_gh_bool(plan.ui)}",
        f"ts_sdk={_gh_bool(plan.ts_sdk)}",
        f"python_sdk={_gh_bool(plan.python_sdk)}",
        f"abi_sync={_gh_bool(plan.abi_sync)}",
        f"build_app_platform={_gh_bool(plan.build_app_platform)}",
        f"build_app_optional={_gh_bool(plan.build_app_optional)}",
        f"build_app_examples={_gh_bool(plan.build_app_examples)}",
        f"confinement={_gh_bool(plan.confinement)}",
        f"tray={_gh_bool(plan.tray)}",
        f"store_free={_gh_bool(plan.store_free)}",
        f"release={_gh_bool(plan.release)}",
        f"docs_markdown={_gh_bool(plan.docs_markdown)}",
        f"rust_packages={pkgs}",
        f"rust_doc_packages={doc_pkgs}",
        f"rust_doctest_packages={doctest_pkgs}",
        f"rust_package_count={len(plan.rust_packages)}",
    ]
    return "\n".join(lines) + "\n"


def plan_from_event(
    *,
    base: str | None,
    head: str | None,
    workspace_root: str | Path | None = None,
    metadata: Mapping[str, Any] | None = None,
    paths: Sequence[str] | None = None,
    force_full: bool = False,
) -> Plan:
    try:
        meta = load_metadata(workspace_root, metadata)
        index = package_index_from_metadata(meta)
        if paths is None and (not base or not head):
            raise PlanError("base and head SHAs are required unless --paths is provided")
        changed = list_changed_paths(base or "", head or "", workspace_root, paths)
        return build_plan(changed, index, force_full=force_full)
    except PlanError as exc:
        # Escalate: empty index full suite plan with reason.
        plan = Plan()
        plan.mark_full(f"planner error: {exc}")
        plan.finalize_full_suite()
        return plan
