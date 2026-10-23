#!/usr/bin/env python3
"""Decide which dependency alerts affect a shipped artifact and draft advisories.

Runs after every full OSV scan of `main` (`.github/workflows/osv-advisories.yml`).
A repository security advisory describes a vulnerability in what Bookclerk
ships, so a vulnerable dependency deserves one only when its code is inside a
shipped artifact (`shipped-artifacts.toml`). Everything else (examples,
fuzzing, scripts, dev dependencies) stays an alert plus the remediation bump.

Subcommands, in the order the workflow runs them:

- ``fetch`` (job ``fetch``, alert-read token): the open OSV-Scanner
  code-scanning alerts and Dependabot alerts on `main`, their OSV records
  (aliases folded in; alerts on the same lockfile and advisory merged), the
  newest published release, and the advisories already on record.
- ``plan`` (job ``fetch``, no token, runs no third-party code): what each
  artifact ships from `cargo metadata` and the lockfiles (normal Cargo
  closure, npm production dependencies), the Python SDK's runtime
  dependencies (which must stay empty), and whether the call-analysis build
  should run.
- ``bundle`` and ``calls`` (job ``analyze``, no secrets at all): build the
  web UI and list the packages in its bundle; build the shipped binaries the
  way OSV-Scanner's Rust call analysis does (fat LTO, one codegen unit,
  debug info) and record a digest of every function in their DWARF. Their
  output names no advisory, so it leaves the job unsealed.
- ``file`` (job ``file``, builds nothing, advisory-write token): gives every
  (advisory, artifact) pair a verdict, files a draft advisory for each
  affected one not on record, and writes the verdict report, which the
  workflow seals for maintainers.

Drafts are filed before the first release too: they are private to
maintainers, nothing is published without one, and filing them proves the
path. Until a release exists a draft proposes no affected range. Logs print
counts only; alert details stay in the sealed bundles, Security -> Code
scanning and the private drafts.

Exit codes:
  0 - ran to completion
  1 - error, including a Python SDK runtime dependency this script cannot
      resolve and a draft GitHub refused (the others are still filed)
"""

from __future__ import annotations

import argparse
import functools
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from pathlib import Path
from types import ModuleType
from typing import Any

import tomllib

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "shipped-artifacts.toml"
API = "https://api.github.com"
# Upper bound on draft advisories one run files, and separately on drafts it
# updates, so a bad map or a burst of upstream advisories cannot flood the
# advisory list.
MAX_FILED_PER_RUN = 10
# Wall-clock cap for `npm ci` and for `vite build` of the web UI.
BUNDLE_TIMEOUT_S = 900
# Wall-clock cap for the call-analysis release build.
BUILD_TIMEOUT_S = 4 * 3600
# Hex digits kept of each function name's SHA-256. 48 bits keep the chance
# that one checked name collides with any of a few million names in the
# binaries below 1e-7, and keep the analysis artifact small.
FUNCTION_DIGEST_HEX = 12
# A shipped binary with fewer Rust functions in its DWARF than this was not
# built with usable debug info; the analysis then fails closed.
MIN_FUNCTIONS_PER_BINARY = 100
# Triggering events of the OSV scan on which the call-analysis build may run.
# It takes hours, so it runs on the weekly scheduled scan and on manual runs,
# not on every push to main.
CALL_ANALYSIS_EVENTS = ("schedule", "workflow_dispatch")

# Body marker that identifies the draft advisory for one upstream advisory
# id and one Bookclerk package (`<ecosystem>/<name>`).
MARKER_RE = re.compile(r"<!-- osv-advisory: (\S+) (\S+) -->")

# Cargo profile overrides that reproduce OSV-Scanner's Rust call analysis
# (`-C opt-level=3 -C debuginfo=1 -C lto -C codegen-units=1 -C strip=none`):
# fat LTO drops functions nothing calls, so a function still in the DWARF is
# linked into the shipped binary.
CALL_ANALYSIS_PROFILE = {
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3",
    "CARGO_PROFILE_RELEASE_DEBUG": "limited",
    "CARGO_PROFILE_RELEASE_LTO": "fat",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
    "CARGO_PROFILE_RELEASE_STRIP": "none",
    "CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO": "off",
    "CARGO_INCREMENTAL": "0",
}

# GitHub advisory severities by GHSA `database_specific.severity`.
GHSA_SEVERITY = {
    "LOW": "low",
    "MODERATE": "medium",
    "MEDIUM": "medium",
    "HIGH": "high",
    "CRITICAL": "critical",
}
SEVERITY_RANK = {"low": 0, "medium": 1, "high": 2, "critical": 3}
CWE_RE = re.compile(r"^CWE-\d+$")
# Environment variables that hold tokens; never passed to child processes.
TOKEN_VARS = ("GH_TOKEN", "ALERTS_TOKEN", "GITHUB_TOKEN")
# Key under which `fetch` keeps the severity and CWE data of every alias
# record, which `merge_aliases` (it merges `affected` only) would drop.
ALIAS_META = "bookclerk_alias_meta"


def _load_script(filename: str, module_name: str) -> ModuleType:
    """Imports a sibling script whose file name is not a module name.

    Args:
        filename: File name under `scripts/`.
        module_name: Name to register it under in `sys.modules`.

    Returns:
        The module.

    Raises:
        ImportError: The file cannot be loaded.
    """
    if module_name in sys.modules:
        return sys.modules[module_name]
    path = ROOT / "scripts" / filename
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise ImportError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


# Version ranges, lockfile parsing and OSV lookups.
osv = _load_script("osv-remediate.py", "osv_remediate")
# The GitHub client and release lookup the fuzz-crash triage files with.
triage = _load_script("fuzz-crash-triage.py", "fuzz_crash_triage")


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Artifact:
    """A shipped Bookclerk artifact an advisory can name.

    Attributes:
        key: Key in `shipped-artifacts.toml` (and `fuzz/boundaries.toml`).
        title: Human description for advisory bodies and the report.
        ecosystem: GitHub advisory ecosystem (`other`, `npm`, `pip`).
        package: Package name in the advisory.
        version: `release-tag`, or a repository-relative manifest whose
            `version` is the package's published version.
    """

    key: str
    title: str
    ecosystem: str
    package: str
    version: str

    @property
    def advisory_key(self) -> str:
        """The `<ecosystem>/<package>` key used in advisory markers."""
        return f"{self.ecosystem}/{self.package}"


@dataclass(frozen=True)
class Shipped:
    """One lockfile whose entries end up, in part, inside an artifact.

    Attributes:
        artifact: The artifact.
        lockfile: Repository-relative lockfile path.
        evidence: `cargo`, `npm`, `npm-bundle` or `pip`.
        roots: Cargo packages whose closure ships (cargo only).
        default_members: Also root at the workspace default-members (cargo).
        root_dirs: Also root at every workspace package under these
            repository-relative directories (cargo).
        call_analysis: Build the roots' binaries and check advisory
            functions (cargo).
        manifest: `pyproject.toml` holding the runtime dependencies (pip).
        dev_extras: Optional-dependency groups that are dev tooling (pip).
    """

    artifact: Artifact
    lockfile: str
    evidence: str
    roots: tuple[str, ...] = ()
    default_members: bool = False
    root_dirs: tuple[str, ...] = ()
    call_analysis: bool = False
    manifest: str = ""
    dev_extras: tuple[str, ...] = ()

    @property
    def key(self) -> str:
        """Key of this entry in the evidence file."""
        return f"{self.artifact.key}|{self.lockfile}"


EVIDENCE_KINDS = ("cargo", "npm", "npm-bundle", "pip")


@dataclass(frozen=True)
class Config:
    """The parsed `shipped-artifacts.toml`.

    Attributes:
        artifacts: Artifacts by key.
        shipped: Lockfile entries, in file order.
        not_shipped: Lockfiles that never ship, path to reason.
    """

    artifacts: dict[str, Artifact]
    shipped: tuple[Shipped, ...]
    not_shipped: dict[str, str]

    def for_lockfile(self, lockfile: str) -> list[Shipped]:
        """Returns the shipped entries of one lockfile (none when it does not ship)."""
        return [s for s in self.shipped if s.lockfile == lockfile]


def load_config(path: Path = CONFIG) -> Config:
    """Reads and validates `shipped-artifacts.toml`.

    Args:
        path: The file.

    Returns:
        The configuration.

    Raises:
        ValueError: An entry names an unknown artifact or evidence kind, lacks
            what its kind needs, or a lockfile is listed twice.
    """
    doc = tomllib.loads(path.read_text(encoding="utf-8"))
    artifacts = {
        key: Artifact(
            key=key,
            title=spec["title"],
            ecosystem=spec["ecosystem"],
            package=spec["package"],
            version=spec["version"],
        )
        for key, spec in doc.get("artifacts", {}).items()
    }
    shipped: list[Shipped] = []
    seen: set[str] = set()
    for spec in doc.get("shipped", []):
        name = spec["artifact"]
        if name not in artifacts:
            raise ValueError(f"shipped entry names unknown artifact {name!r}")
        kind = spec["evidence"]
        if kind not in EVIDENCE_KINDS:
            raise ValueError(f"{spec['lockfile']}: unknown evidence {kind!r}")
        entry = Shipped(
            artifact=artifacts[name],
            lockfile=spec["lockfile"],
            evidence=kind,
            roots=tuple(spec.get("roots", ())),
            default_members=bool(spec.get("default_members", False)),
            root_dirs=tuple(spec.get("root_dirs", ())),
            call_analysis=bool(spec.get("call_analysis", False)),
            manifest=spec.get("manifest", ""),
            dev_extras=tuple(spec.get("dev_extras", ())),
        )
        if kind == "cargo" and not (entry.roots or entry.default_members or entry.root_dirs):
            raise ValueError(f"{entry.key}: cargo evidence needs roots")
        if kind == "pip" and not entry.manifest:
            raise ValueError(f"{entry.key}: pip evidence needs a manifest")
        if entry.call_analysis and kind != "cargo":
            raise ValueError(f"{entry.key}: call analysis is cargo only")
        if entry.key in seen:
            raise ValueError(f"{entry.key} listed twice")
        seen.add(entry.key)
        shipped.append(entry)
    not_shipped = {item["path"]: item["why"] for item in doc.get("not_shipped", [])}
    overlap = {s.lockfile for s in shipped} & set(not_shipped)
    if overlap:
        raise ValueError(f"lockfiles both shipped and not shipped: {sorted(overlap)}")
    return Config(artifacts=artifacts, shipped=tuple(shipped), not_shipped=not_shipped)


# ---------------------------------------------------------------------------
# Advisory records (pure)
# ---------------------------------------------------------------------------


def record_ids(record: dict[str, Any]) -> set[str]:
    """Returns an OSV record's id and aliases."""
    ids = {str(record.get("id", ""))}
    aliases = record.get("aliases")
    if isinstance(aliases, list):
        ids.update(a for a in aliases if isinstance(a, str))
    ids.discard("")
    return ids


def with_alias_meta(primary: dict[str, Any], aliases: list[dict[str, Any]]) -> dict[str, Any]:
    """Merges alias records the way the remediation does, keeping their severity data.

    Args:
        primary: The record the alert's id resolved to.
        aliases: Records of its aliases.

    Returns:
        `osv.merge_aliases(primary, aliases)` plus, under `ALIAS_META`, the
        id, `severity` and `database_specific` of every record.
    """
    merged = osv.merge_aliases(primary, aliases)
    meta: list[dict[str, Any]] = []
    for record in (primary, *aliases):
        meta += record.get(ALIAS_META) or [
            {
                "id": record.get("id"),
                "severity": record.get("severity"),
                "database_specific": record.get("database_specific"),
            }
        ]
    merged[ALIAS_META] = meta
    return merged


def vulnerable_functions(record: dict[str, Any], ecosystem: str, name: str) -> list[str]:
    """Returns the function paths a record says are vulnerable in one package.

    RustSec records carry them under
    `affected[].ecosystem_specific.affects.functions`
    (for example `smallvec::SmallVec::grow`); most records carry none.

    Args:
        record: The (alias-merged) OSV record.
        ecosystem: OSV ecosystem name.
        name: Package name.

    Returns:
        Sorted, deduplicated function paths; empty when the record has no
        function-level data for this package.
    """
    found: set[str] = set()
    for entry in osv._affected_entries(record, ecosystem, name):
        specific = entry.get("ecosystem_specific")
        affects = specific.get("affects") if isinstance(specific, dict) else None
        functions = affects.get("functions") if isinstance(affects, dict) else None
        if isinstance(functions, list):
            found.update(f for f in functions if isinstance(f, str) and f)
    return sorted(found)


@functools.lru_cache(maxsize=32)
def _locked_cached(path: str, mtime_ns: int) -> dict[str, list[Any]]:
    """Parses one lockfile once per content version (see `locked`)."""
    del mtime_ns  # part of the cache key only
    return osv.locked_versions(Path(path))


def locked(root: Path, lockfile: str) -> dict[str, list[Any]]:
    """Reads a lockfile's locked versions, empty when it is missing or unsupported.

    Args:
        root: Repository root.
        lockfile: Repository-relative lockfile path.

    Returns:
        Package name to locked versions.
    """
    path = root / lockfile
    if not path.is_file() or path.name not in osv.LOCKFILE_ECOSYSTEMS:
        return {}
    return _locked_cached(str(path), path.stat().st_mtime_ns)


def affected_locked(record: dict[str, Any], lockfile: str, root: Path) -> list[tuple[str, str]]:
    """Lists the locked (name, version) pairs of a lockfile that a record affects.

    Args:
        record: The (alias-merged) OSV record.
        lockfile: Repository-relative lockfile path.
        root: Repository root.

    Returns:
        Sorted affected pairs.
    """
    ecosystem = osv.LOCKFILE_ECOSYSTEMS.get(Path(lockfile).name)
    if ecosystem is None:
        return []
    out: set[tuple[str, str]] = set()
    for name, versions in locked(root, lockfile).items():
        if not osv._mentions(record, ecosystem, name):
            continue
        for version in versions:
            if osv.is_affected(record, ecosystem, name, version):
                out.add((name, version.text))
    return sorted(out)


@dataclass
class Group:
    """Open alerts on one lockfile that name the same upstream advisory.

    OSV-Scanner rules use RUSTSEC, GHSA or CVE ids and Dependabot uses GHSA
    ids; alerts whose ids or aliases overlap are one upstream advisory.

    Attributes:
        lockfile: Repository-relative lockfile path.
        ids: Every id and alias of the merged records, sorted.
        record: The OSV records merged (`with_alias_meta`).
        sources: `code-scanning` and/or `dependabot`.
    """

    lockfile: str
    ids: list[str]
    record: dict[str, Any]
    sources: list[str] = field(default_factory=list)

    @property
    def primary(self) -> str:
        """The id used in titles: a GHSA id when there is one, else the first id."""
        ghsa = [i for i in self.ids if i.startswith("GHSA-")]
        return (ghsa or self.ids)[0]

    def to_json(self) -> dict[str, Any]:
        """Serializes the group for the state file."""
        return {"lockfile": self.lockfile, "ids": self.ids, "record": self.record, "sources": self.sources}

    @classmethod
    def from_json(cls, raw: dict[str, Any]) -> Group:
        """Reads a group from the state file."""
        return cls(raw["lockfile"], list(raw["ids"]), raw["record"], list(raw.get("sources", [])))


def group_alerts(
    alerts: list[tuple[str, str, str]], records: dict[str, dict[str, Any] | None]
) -> tuple[list[Group], int]:
    """Merges alerts per lockfile and upstream advisory.

    Args:
        alerts: `(advisory id, lockfile, source)` triples.
        records: OSV record per advisory id (None when OSV does not know it).

    Returns:
        The groups, and how many alerts named an id OSV does not know (a
        deprecation notice rather than an advisory, for example).
    """
    unknown = 0
    groups: list[Group] = []
    for vuln_id, lockfile, source in sorted(set(alerts)):
        record = records.get(vuln_id)
        if record is None:
            unknown += 1
            continue
        ids = record_ids(record) | {vuln_id}
        match = [g for g in groups if g.lockfile == lockfile and ids & set(g.ids)]
        if not match:
            groups.append(Group(lockfile, sorted(ids), record, [source]))
            continue
        target = match[0]
        for other in match[1:]:
            target.ids = sorted(set(target.ids) | set(other.ids))
            target.record = with_alias_meta(target.record, [other.record])
            target.sources = sorted(set(target.sources) | set(other.sources))
            groups.remove(other)
        if not ids <= set(target.ids):
            target.record = with_alias_meta(target.record, [record])
        target.ids = sorted(set(target.ids) | ids)
        target.sources = sorted(set(target.sources) | {source})
    groups.sort(key=lambda g: (g.lockfile, g.primary))
    return groups, unknown


# ---------------------------------------------------------------------------
# GitHub API
# ---------------------------------------------------------------------------


class GitHub(triage.GitHub):
    """The triage client plus the alert listings this workflow reads."""

    def _items(self, path: str) -> list[dict[str, Any]]:
        """Collects a listing's object items."""
        return [item for item in self.paginate(path) if isinstance(item, dict)]

    def code_scanning_alerts(self, ref: str) -> list[dict[str, Any]]:
        """Returns the open OSV-Scanner code-scanning alerts on `ref`."""
        query = urllib.parse.urlencode({"tool_name": osv.TOOL_NAME, "state": "open", "ref": ref})
        return self._items(f"/repos/{self.repo}/code-scanning/alerts?{query}")

    def dependabot_alerts(self) -> list[dict[str, Any]]:
        """Returns the open Dependabot alerts (they always describe the default branch)."""
        return self._items(f"/repos/{self.repo}/dependabot/alerts?state=open")

    def advisory_markers(self) -> set[tuple[str, str]]:
        """Returns the `(upstream id, advisory key)` markers of every repository advisory.

        Advisories of every state count, so a draft a maintainer closed as
        not affecting Bookclerk is not filed again.
        """
        found: set[tuple[str, str]] = set()
        for advisory in self.advisories():
            found.update(MARKER_RE.findall(advisory.get("description") or ""))
        return found


def dependabot_to_alerts(raw: list[dict[str, Any]]) -> list[tuple[str, str]]:
    """Converts Dependabot alert JSON into `(GHSA id, manifest path)` pairs.

    Args:
        raw: Alerts as the Dependabot alerts API returns them.

    Returns:
        Pairs for alerts that name an advisory and a manifest.
    """
    out: set[tuple[str, str]] = set()
    for item in raw:
        advisory = item.get("security_advisory")
        dependency = item.get("dependency")
        ghsa = advisory.get("ghsa_id") if isinstance(advisory, dict) else None
        path = dependency.get("manifest_path") if isinstance(dependency, dict) else None
        if isinstance(ghsa, str) and ghsa and isinstance(path, str) and path:
            out.add((ghsa, path))
    return sorted(out)


def fetch_record(vuln_id: str, hints: tuple[str, ...] = ()) -> dict[str, Any] | None:
    """Looks an advisory and its aliases up on api.osv.dev (`with_alias_meta`).

    Args:
        vuln_id: The id an alert names.
        hints: Other advisory ids the alert names (`osv.Alert.hints`), used
            when OSV does not know `vuln_id` and folded in as aliases.

    Returns:
        The merged record, or None when OSV knows neither the id nor any hint.

    Raises:
        urllib.error.URLError: OSV could not be reached.
    """
    found = osv.fetch_primary(vuln_id, hints)
    if found is None:
        return None
    primary, alias_ids = found
    aliases = [r for alias in alias_ids if (r := osv.fetch_vuln(alias)) is not None]
    return with_alias_meta(primary, aliases)


def cmd_fetch(args: argparse.Namespace) -> int:
    """Reads alerts, records, the newest release and advisories on record.

    Args:
        args: `--out`, `--ref`.

    Returns:
        The exit code.
    """
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    token = os.environ.get("GH_TOKEN", "")
    if not repo or not token:
        print("GITHUB_REPOSITORY and GH_TOKEN are required", file=sys.stderr)
        return 1
    gh = GitHub(repo, token, os.environ.get("GITHUB_API_URL", API))
    try:
        scanning_alerts = osv.alerts_from_api(gh.code_scanning_alerts(args.ref))
        hints = osv.alert_hints_by_id(scanning_alerts)
        scanning = [(a.vuln_id, a.lockfile, "code-scanning") for a in scanning_alerts]
        dependabot = [(ghsa, path, "dependabot") for ghsa, path in dependabot_to_alerts(gh.dependabot_alerts())]
        alerts = scanning + dependabot
        records = {vuln_id: fetch_record(vuln_id, hints.get(vuln_id, ())) for vuln_id in sorted({a[0] for a in alerts})}
        release_tag = gh.latest_release_tag()
        on_record = gh.advisory_markers()
        drafts = [
            {
                "ghsa_id": d["ghsa_id"],
                "description": d["description"],
                "vulnerabilities": d.get("vulnerabilities") or [],
            }
            for d in gh.drafts()
            if isinstance(d.get("ghsa_id"), str)
            and isinstance(d.get("description"), str)
            and MARKER_RE.search(d["description"])
        ]
    except (urllib.error.URLError, TypeError, ValueError, KeyError) as err:
        # The type only: a message could name an alert.
        print(f"could not read alerts, advisories or releases: {type(err).__name__}", file=sys.stderr)
        return 1
    groups, unknown = group_alerts(alerts, records)
    state = {
        "release_tag": release_tag,
        "groups": [g.to_json() for g in groups],
        "unknown_alerts": unknown,
        "on_record": sorted([list(m) for m in on_record]),
        "drafts": drafts,
    }
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(state, indent=1, sort_keys=True), encoding="utf-8")
    print(
        f"open alerts: {len(scanning)} code scanning, {len(dependabot)} Dependabot; "
        f"advisories: {len(groups)}; release published: {'yes' if release_tag else 'no'}"
    )
    return 0


# ---------------------------------------------------------------------------
# plan: what each artifact ships (no third-party code runs)
# ---------------------------------------------------------------------------


def cargo_closure(metadata: dict[str, Any], roots: set[str]) -> set[tuple[str, str]]:
    """Walks the shipped dependency closure of `roots` in `cargo metadata` output.

    Normal edges are followed. Dev dependencies never ship. Build
    dependencies run at build time, so they are skipped too, except crates
    named `*-src` (`openssl-src` and the like), which exist to compile
    vendored C that the dependent `-sys` crate links in; their own
    dependencies (`cc` and other build tooling) are not followed, since
    only the C they compile is linked. Proc-macro crates
    and what only they pull in run in the compiler and are skipped.
    `cargo metadata` without `--filter-platform` resolves every target
    platform, and its features are the union across the workspace.

    Args:
        metadata: `cargo metadata --format-version 1` output.
        roots: Package ids to start from.

    Returns:
        `(name, version)` of every package reached, roots included.
    """
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}

    def is_proc_macro(pkg_id: str) -> bool:
        return any("proc-macro" in t.get("kind", []) for t in packages[pkg_id].get("targets", []))

    seen: set[str] = set()
    # Reached only through a build edge into a `*-src` crate: shipped, but
    # its own dependencies are build tooling.
    leaves: set[str] = set()
    stack = [(r, True) for r in roots if r in packages]
    while stack:
        pkg_id, follow = stack.pop()
        if is_proc_macro(pkg_id) or (pkg_id in seen and (follow is False or pkg_id not in leaves)):
            continue
        seen.add(pkg_id)
        if not follow:
            leaves.add(pkg_id)
            continue
        leaves.discard(pkg_id)
        for dep in nodes.get(pkg_id, {}).get("deps", []):
            kinds = {k.get("kind") for k in dep.get("dep_kinds") or [{"kind": None}]}
            if None in kinds:
                stack.append((dep["pkg"], True))
            elif "build" in kinds and packages.get(dep["pkg"], {}).get("name", "").endswith("-src"):
                stack.append((dep["pkg"], False))
    return {(packages[i]["name"], packages[i]["version"]) for i in seen}


def cargo_roots(metadata: dict[str, Any], entry: Shipped, workspace: Path) -> set[str]:
    """Resolves a cargo entry's roots to package ids.

    Args:
        metadata: `cargo metadata` output.
        entry: The shipped entry.
        workspace: Directory of the lockfile's workspace.

    Returns:
        Package ids.

    Raises:
        ValueError: A named root is not a workspace member, or a root
            directory matches no member.
    """
    members = set(metadata["workspace_members"])
    by_id = {p["id"]: p for p in metadata["packages"] if p["id"] in members}
    ids: set[str] = set()
    for name in entry.roots:
        found = [i for i, p in by_id.items() if p["name"] == name]
        if not found:
            raise ValueError(f"root {name!r} is not a workspace member")
        ids.update(found)
    if entry.default_members:
        ids.update(metadata.get("workspace_default_members") or [])
    for rel in entry.root_dirs:
        base = (workspace / rel).resolve()
        found = [i for i, p in by_id.items() if Path(p["manifest_path"]).resolve().is_relative_to(base)]
        if not found:
            raise ValueError(f"root directory {rel!r} holds no workspace member")
        ids.update(found)
    return ids


def cargo_metadata(workspace: Path) -> dict[str, Any]:
    """Runs `cargo metadata` for a workspace.

    It resolves and downloads packages but runs no build script or proc
    macro, so it is safe next to secrets.

    Args:
        workspace: Directory holding `Cargo.toml` and `Cargo.lock`.

    Returns:
        The parsed output.

    Raises:
        subprocess.CalledProcessError: cargo failed.
    """
    proc = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=workspace,
        env=_child_env(),
        capture_output=True,
        check=True,
    )
    return json.loads(proc.stdout)


def npm_production(lock: dict[str, Any]) -> set[tuple[str, str]]:
    """Returns the production packages of a package-lock.json (v2/v3).

    Entries marked `dev` are dev-only. Everything else, `devOptional` and
    `optional` included, can be installed by a production install, so it
    counts.

    Args:
        lock: The parsed lockfile.

    Returns:
        `(name, version)` pairs.
    """
    out: set[tuple[str, str]] = set()
    for key, meta in (lock.get("packages") or {}).items():
        if not key or not isinstance(meta, dict) or meta.get("dev") or meta.get("link"):
            continue
        name = meta.get("name") if isinstance(meta.get("name"), str) else key.rsplit("node_modules/", 1)[-1]
        version = meta.get("version")
        if isinstance(version, str):
            out.add((name, version))
    return out


def python_runtime_deps(pyproject: dict[str, Any], dev_extras: tuple[str, ...]) -> list[str]:
    """Returns what installing the Python SDK (any non-dev extra) pulls in.

    Args:
        pyproject: The parsed `pyproject.toml`.
        dev_extras: Optional-dependency groups that are dev tooling.

    Returns:
        Requirement strings; empty while the SDK has no runtime dependencies.
    """
    project = pyproject.get("project") or {}
    deps = [d for d in project.get("dependencies") or [] if isinstance(d, str)]
    for extra, reqs in (project.get("optional-dependencies") or {}).items():
        if extra not in dev_extras:
            deps += [f"{r} (extra {extra})" for r in reqs if isinstance(r, str)]
    return deps


def collect_evidence(
    config: Config, root: Path, metadata_for: Callable[[Path], dict[str, Any]] = cargo_metadata
) -> dict[str, Any]:
    """Computes what each shipped entry contains, without running build code.

    Args:
        config: Shipped-artifact map.
        root: Repository root.
        metadata_for: Runs `cargo metadata` for a workspace (a seam for tests).

    Returns:
        JSON-ready evidence per `Shipped.key`: the shipped `(name, version)`
        pairs (`packages`); for a bundle entry these are its production
        dependencies, which `bundle` narrows later.

    Raises:
        ValueError: The Python SDK declares runtime dependencies, which this
            script does not resolve yet; or a cargo root is wrong.
        subprocess.CalledProcessError: `cargo metadata` failed.
    """
    evidence: dict[str, Any] = {}
    metadata_cache: dict[str, dict[str, Any]] = {}
    for entry in config.shipped:
        lockfile = root / entry.lockfile
        if entry.evidence == "pip":
            pyproject = tomllib.loads((root / entry.manifest).read_text(encoding="utf-8"))
            deps = python_runtime_deps(pyproject, entry.dev_extras)
            if deps:
                raise ValueError(
                    f"{entry.manifest} declares runtime dependencies ({len(deps)}); resolve their "
                    "closure in scripts/osv-advisories.py before shipping them"
                )
            evidence[entry.key] = {"packages": []}
        elif not lockfile.is_file():
            evidence[entry.key] = {"packages": [], "missing": True}
        elif entry.evidence == "cargo":
            if entry.lockfile not in metadata_cache:
                metadata_cache[entry.lockfile] = metadata_for(lockfile.parent)
            metadata = metadata_cache[entry.lockfile]
            closure = cargo_closure(metadata, cargo_roots(metadata, entry, lockfile.parent))
            evidence[entry.key] = {"packages": sorted(closure)}
        else:
            lock = json.loads(lockfile.read_text(encoding="utf-8"))
            evidence[entry.key] = {"packages": sorted(npm_production(lock))}
    return evidence


def call_candidates(
    groups: list[Group], config: Config, evidence: dict[str, Any], on_record: set[tuple[str, str]], root: Path
) -> list[str]:
    """Lists the advisories whose verdict waits on call analysis.

    An advisory qualifies when it affects a package of a call-analysis
    entry's shipped closure, names vulnerable functions for it, and that
    artifact has no closed or published advisory for it (an open draft
    still qualifies, so the analysis can refresh it).

    Args:
        groups: Open advisory groups.
        config: Shipped-artifact map.
        evidence: Output of `collect_evidence`.
        on_record: Markers of advisories that are closed or published
            (`settled_markers`).
        root: Repository root.

    Returns:
        Primary ids of the qualifying groups.
    """
    out: list[str] = []
    for group in groups:
        for entry in config.for_lockfile(group.lockfile):
            if not entry.call_analysis:
                continue
            if any((i, entry.artifact.advisory_key) in on_record for i in group.ids):
                continue
            shipped = {tuple(p) for p in evidence.get(entry.key, {}).get("packages", [])}
            ecosystem = osv.LOCKFILE_ECOSYSTEMS.get(Path(entry.lockfile).name, "")
            if any(
                pair in shipped and vulnerable_functions(group.record, ecosystem, pair[0])
                for pair in affected_locked(group.record, group.lockfile, root)
            ):
                out.append(group.primary)
                break
    return out


def settled_markers(state: dict[str, Any]) -> set[tuple[str, str]]:
    """Returns the markers of advisories on record that are no longer drafts.

    Args:
        state: Output of `fetch`.

    Returns:
        `(upstream id, advisory key)` pairs of closed and published advisories.
    """
    open_drafts = {m for d in state.get("drafts", []) for m in MARKER_RE.findall(str(d.get("description") or ""))}
    return {tuple(m) for m in state.get("on_record", [])} - open_drafts


def bundle_needed(groups: list[Group], config: Config) -> bool:
    """Says whether any open advisory needs the web UI bundle's contents.

    Args:
        groups: Open advisory groups.
        config: Shipped-artifact map.

    Returns:
        True when an advisory is on a lockfile with `npm-bundle` evidence.
    """
    return any(e.evidence == "npm-bundle" for g in groups for e in config.for_lockfile(g.lockfile))


def plan_call_analysis(candidates: list[str], event: str, force: bool) -> str:
    """Decides whether the call-analysis build runs.

    Args:
        candidates: Output of `call_candidates`.
        event: Event that triggered the OSV scan (or this workflow, when run
            by hand).
        force: The manual run asked for the build.

    Returns:
        `run`, `deferred` (candidates wait for the weekly scheduled scan or
        a manual run), or `not needed`.
    """
    if force:
        return "run"
    if not candidates:
        return "not needed"
    return "run" if event in CALL_ANALYSIS_EVENTS else "deferred"


def cmd_plan(args: argparse.Namespace) -> int:
    """Writes the evidence file and the call-analysis decision.

    Args:
        args: `--state`, `--out`, `--event`, `--force-call-analysis`,
            `--github-output`.

    Returns:
        The exit code.
    """
    config = load_config(Path(args.config))
    state = json.loads(Path(args.state).read_text(encoding="utf-8"))
    try:
        evidence = collect_evidence(config, ROOT)
    except (ValueError, subprocess.CalledProcessError, OSError) as err:
        print(f"could not collect evidence: {err}", file=sys.stderr)
        return 1
    groups = [Group.from_json(g) for g in state["groups"]]
    candidates = call_candidates(groups, config, evidence, settled_markers(state), ROOT)
    calls = plan_call_analysis(candidates, args.event, args.force_call_analysis)
    bundle = bundle_needed(groups, config)
    out = {"entries": evidence, "call_analysis": calls}
    Path(args.out).write_text(json.dumps(out, indent=1, sort_keys=True), encoding="utf-8")
    print(
        f"shipped entries: {len(evidence)}; call analysis: {calls}; web bundle: {'needed' if bundle else 'not needed'}"
    )
    if args.github_output:
        with Path(args.github_output).open("a", encoding="utf-8") as fh:
            fh.write(f"call-analysis={'true' if calls == 'run' else 'false'}\n")
            fh.write(f"bundle={'true' if bundle else 'false'}\n")
    return 0


# ---------------------------------------------------------------------------
# bundle and calls: build steps without secrets
# ---------------------------------------------------------------------------


def _child_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    """Returns the environment for tools, without any token."""
    env = {k: v for k, v in os.environ.items() if k not in TOKEN_VARS}
    env.update(extra or {})
    return env


def _quiet(cmd: list[str], cwd: Path, timeout: int) -> bool:
    """Runs a command with its output discarded; whether it exited 0 in time."""
    try:
        proc = subprocess.run(cmd, cwd=cwd, env=_child_env(), capture_output=True, timeout=timeout, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return False
    return proc.returncode == 0


def lock_key_for_source(source: str) -> str | None:
    """Maps a source-map `sources` entry to its package-lock.json `packages` key.

    `../node_modules/@scope/a/node_modules/b/index.js` becomes
    `node_modules/@scope/a/node_modules/b`.

    Args:
        source: The source path as the map records it.

    Returns:
        The key, or None for application sources outside `node_modules`.
    """
    parts = source.replace("\\", "/").split("/")
    try:
        i = parts.index("node_modules")
    except ValueError:
        return None
    key: list[str] = []
    while i + 1 < len(parts) and parts[i] == "node_modules":
        name = parts[i + 1]
        if name.startswith("@"):
            if i + 2 >= len(parts):
                break
            key += ["node_modules", name, parts[i + 2]]
            i += 3
        else:
            key += ["node_modules", name]
            i += 2
    return "/".join(key) if key else None


def bundle_packages(maps: Iterable[dict[str, Any]], lock: dict[str, Any]) -> set[tuple[str, str]]:
    """Lists the lockfile packages whose code a production bundle contains.

    Args:
        maps: Parsed source maps of the bundle's chunks.
        lock: The package-lock.json the bundle was built from.

    Returns:
        `(name, version)` pairs.
    """
    packages = lock.get("packages") or {}
    out: set[tuple[str, str]] = set()
    for source_map in maps:
        for source in source_map.get("sources") or []:
            key = lock_key_for_source(source) if isinstance(source, str) else None
            # A nested path the lockfile lacks belongs to the enclosing package.
            while key and key not in packages:
                key = key.rsplit("/node_modules/", 1)[0] if "/node_modules/" in key else None
            meta = packages.get(key) if key else None
            if isinstance(meta, dict) and isinstance(meta.get("version"), str):
                name = meta.get("name") if isinstance(meta.get("name"), str) else key.rsplit("node_modules/", 1)[-1]
                out.add((name, meta["version"]))
    return out


def ui_bundle(lockfile: Path) -> set[tuple[str, str]] | None:
    """Builds the web UI with source maps and lists the packages in the bundle.

    Code that reaches `ui/dist` without a source map entry (`?url` or `?raw`
    imports of package files, wasm) would be missed; a test keeps the UI
    from importing package files that way.

    Args:
        lockfile: `ui/package-lock.json`.

    Returns:
        The bundled packages, or None when the build failed (verdicts then
        fall back to the production dependency list).
    """
    workdir = lockfile.parent
    if not _quiet(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], workdir, BUNDLE_TIMEOUT_S):
        return None
    with tempfile.TemporaryDirectory() as out:
        cmd = ["npx", "--no-install", "vite", "build", "--sourcemap", "--outDir", out, "--emptyOutDir"]
        if not _quiet([*cmd, "--logLevel", "error"], workdir, BUNDLE_TIMEOUT_S):
            return None
        try:
            maps = [json.loads(p.read_text(encoding="utf-8")) for p in Path(out).rglob("*.map")]
        except (OSError, ValueError):
            return None
        if not maps:
            return None
    return bundle_packages(maps, json.loads(lockfile.read_text(encoding="utf-8")))


def cmd_bundle(args: argparse.Namespace) -> int:
    """Writes the packages of every `npm-bundle` entry's production bundle.

    Args:
        args: `--out`.

    Returns:
        0; a failed build is recorded as `null` for that entry.
    """
    config = load_config(Path(args.config))
    result: dict[str, Any] = {}
    for entry in config.shipped:
        if entry.evidence == "npm-bundle":
            bundled = ui_bundle(ROOT / entry.lockfile)
            result[entry.key] = None if bundled is None else sorted(bundled)
    Path(args.out).write_text(json.dumps(result, indent=1, sort_keys=True), encoding="utf-8")
    print(f"web bundles built: {sum(1 for v in result.values() if v is not None)}/{len(result)}")
    return 0


_LINKAGE_RE = re.compile(r"DW_AT_linkage_name\s*:.*?((?:_ZN|_R)\S+)\s*$")
_HASH_RE = re.compile(r"::h[0-9a-f]{16}$")
_DISAMBIGUATOR_RE = re.compile(r"\[[0-9a-f]+\]")
# Generic arguments: `Type<...>` and turbofish `func::<...>`, innermost first.
_GENERIC_RE = re.compile(r"(?:(?<=\w)|::)<(?:(?! as )[^<>])*>")
# `<Type as Trait>` and a qualified `<Type>`, once their arguments are gone.
_TRAIT_IMPL_RE = re.compile(r"<([^<>\s]+) as [^<>]+>")
_QUALIFIED_RE = re.compile(r"(?<!::)<([^<>\s]+)>")


def clean_symbol(demangled: str) -> str:
    """Normalizes a demangled Rust symbol to the advisory function format.

    Mirrors OSV-Scanner's cleanup: hash suffixes, crate disambiguators and
    generic arguments go, a trait implementation `<Type as Trait>::method`
    becomes `Type::method`, and v0's qualified `<Type>::method` becomes
    `Type::method`, so `<smallvec[86b6]::SmallVec<[u8; 2: usize]>>::grow`
    matches `smallvec::SmallVec::grow`.

    Args:
        demangled: Output of `c++filt` for one symbol.

    Returns:
        The cleaned path.
    """
    value = _HASH_RE.sub("", demangled.strip())
    value = _DISAMBIGUATOR_RE.sub("", value)
    previous = None
    while previous != value:
        previous = value
        value = _GENERIC_RE.sub("", value)
        value = _TRAIT_IMPL_RE.sub(r"\1", value)
        value = _QUALIFIED_RE.sub(r"\1", value)
    return value


def function_digest(name: str) -> str:
    """Returns the truncated SHA-256 the analysis records for a function path."""
    return hashlib.sha256(name.encode("utf-8")).hexdigest()[:FUNCTION_DIGEST_HEX]


def linkage_names(readelf_output: Iterable[str]) -> set[str]:
    """Extracts mangled Rust linkage names from `readelf --debug-dump=info` lines."""
    found: set[str] = set()
    for line in readelf_output:
        match = _LINKAGE_RE.search(line)
        if match:
            found.add(match.group(1))
    return found


def binary_functions(binary: Path) -> set[str]:
    """Lists the cleaned Rust function paths in a binary's DWARF.

    Args:
        binary: An unstripped ELF executable with debug info.

    Returns:
        Function paths.

    Raises:
        OSError: `readelf` or `c++filt` could not run.
        subprocess.CalledProcessError: Either failed.
    """
    with subprocess.Popen(
        ["readelf", "--debug-dump=info", "--wide", str(binary)],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        errors="replace",
    ) as proc:
        if proc.stdout is None:
            raise OSError("readelf produced no output stream")
        mangled = linkage_names(proc.stdout)
    if proc.returncode != 0:
        raise subprocess.CalledProcessError(proc.returncode, "readelf")
    demangled = subprocess.run(
        ["c++filt"], input="\n".join(sorted(mangled)) + "\n", capture_output=True, text=True, check=True
    ).stdout.splitlines()
    return {clean_symbol(name) for name in demangled if name}


def build_binaries(entry: Shipped, workspace: Path, target_dir: Path) -> list[Path]:
    """Builds an entry's shipped binaries the way the call analysis needs.

    Args:
        entry: A cargo entry with `call_analysis`.
        workspace: The lockfile's workspace directory.
        target_dir: A fresh target directory (the profile differs from CI's).

    Returns:
        The executables cargo reported.

    Raises:
        subprocess.CalledProcessError: The build failed.
        subprocess.TimeoutExpired: It ran past `BUILD_TIMEOUT_S`.
        ValueError: A root is wrong, or cargo reported no executable.
    """
    metadata = cargo_metadata(workspace)
    roots = cargo_roots(metadata, entry, workspace)
    names = sorted({p["name"] for p in metadata["packages"] if p["id"] in roots})
    cmd = ["cargo", "build", "--release", "--locked", "--bins", "--message-format=json-render-diagnostics"]
    for name in names:
        cmd += ["-p", name]
    env = _child_env({**CALL_ANALYSIS_PROFILE, "CARGO_TARGET_DIR": str(target_dir)})
    # Compiler diagnostics go to the job log (stderr); stdout carries the
    # JSON messages that name the executables.
    build = subprocess.run(
        cmd, cwd=workspace, env=env, stdout=subprocess.PIPE, timeout=BUILD_TIMEOUT_S, check=True, text=True
    )
    executables: set[Path] = set()
    for line in build.stdout.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if isinstance(message, dict) and message.get("reason") == "compiler-artifact" and message.get("executable"):
            executables.add(Path(message["executable"]))
    if not executables:
        raise ValueError("cargo reported no executables")
    return sorted(executables)


def check_functions(binary: str, functions: set[str]) -> None:
    """Fails closed when a binary's DWARF does not look like a usable analysis.

    Args:
        binary: Binary name, for the message.
        functions: Its cleaned function paths.

    Raises:
        ValueError: Too few functions, or no `main`: the debug info is missing
            or the symbols were not parsed, so absence proves nothing.
    """
    if len(functions) < MIN_FUNCTIONS_PER_BINARY or not any(f.endswith("::main") for f in functions):
        raise ValueError(f"{binary}: no usable debug info ({len(functions)} functions)")


def cmd_calls(args: argparse.Namespace) -> int:
    """Builds the shipped binaries and records a digest of every function they contain.

    Args:
        args: `--out`, `--target-dir`.

    Returns:
        0. A failed build or analysis is recorded as an `error`, and the
        advisories waiting on it are then filed on the closure alone.
    """
    config = load_config(Path(args.config))
    binaries: dict[str, list[str]] = {}
    try:
        for entry in config.shipped:
            if not entry.call_analysis:
                continue
            for binary in build_binaries(entry, (ROOT / entry.lockfile).parent, Path(args.target_dir)):
                functions = binary_functions(binary)
                check_functions(binary.name, functions)
                binaries[f"{entry.key}|{binary.name}"] = sorted({function_digest(f) for f in functions})
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError, ValueError) as err:
        print(f"call analysis failed ({type(err).__name__}); affected advisories fall back to the closure")
        Path(args.out).write_text(json.dumps({"error": type(err).__name__}), encoding="utf-8")
        return 0
    Path(args.out).write_text(json.dumps({"binaries": binaries}), encoding="utf-8")
    print(f"call analysis: {len(binaries)} binaries")
    return 0


# ---------------------------------------------------------------------------
# Verdicts and filing (pure)
# ---------------------------------------------------------------------------

# Verdicts that make an artifact affected (a draft advisory).
REACHABLE = "reachable"  # call analysis found a vulnerable function in a shipped binary
SHIPPED_NO_FUNCTIONS = "shipped, no function data"  # in the closure; record has no function list
SHIPPED_UNANALYZED = "shipped, not analyzed"  # function data exists, no binary to check or analysis failed
IN_BUNDLE = "in production bundle"  # the web bundle contains the package
PRODUCTION_DEP = "production dependency"  # npm production dependency (no bundle to narrow it)
AFFECTED = {REACHABLE, SHIPPED_NO_FUNCTIONS, SHIPPED_UNANALYZED, IN_BUNDLE, PRODUCTION_DEP}
# Verdicts that do not (yet).
AWAITING_CALLS = "awaiting call analysis"  # runs on the weekly scheduled scan
NOT_CALLED = "not called"  # call analysis: none of the functions are in any shipped binary
TREE_SHAKEN = "tree-shaken"  # production dependency, but no code in the bundle
NOT_SHIPPED = "not shipped"  # not in the artifact at all


@dataclass
class Analysis:
    """What the `analyze` job found, joined with the plan.

    Attributes:
        plan: `run`, `deferred` or `not needed` (`plan_call_analysis`).
        binaries: Function digests per `<entry key>|<binary>`, when the
            analysis ran and succeeded.
        error: Why the analysis failed, when it ran and failed (or its
            output is missing).
        bundles: Bundled packages per `npm-bundle` entry key (None when the
            build failed).
    """

    plan: str = "not needed"
    binaries: dict[str, set[str]] | None = None
    error: str = ""
    bundles: dict[str, list[tuple[str, str]] | None] = field(default_factory=dict)

    def binaries_with(self, entry: Shipped, function: str) -> list[str]:
        """Names the entry's binaries whose DWARF contains `function`."""
        digest = function_digest(function)
        prefix = f"{entry.key}|"
        return sorted(
            k.removeprefix(prefix) for k, v in (self.binaries or {}).items() if k.startswith(prefix) and digest in v
        )


def load_analysis(plan: str, calls_path: Path | None, bundle_path: Path | None) -> Analysis:
    """Reads the `analyze` job's output; missing or broken output fails closed.

    Args:
        plan: The planned call analysis.
        calls_path: Output of `calls`, if it ran.
        bundle_path: Output of `bundle`.

    Returns:
        The analysis.
    """
    analysis = Analysis(plan=plan)
    if bundle_path and bundle_path.is_file():
        try:
            raw = json.loads(bundle_path.read_text(encoding="utf-8"))
            analysis.bundles = {k: None if v is None else [tuple(p) for p in v] for k, v in raw.items()}
        except (OSError, ValueError, TypeError, AttributeError):
            analysis.bundles = {}
    if plan != "run":
        return analysis
    try:
        raw = json.loads(calls_path.read_text(encoding="utf-8")) if calls_path else None
    except (OSError, ValueError):
        raw = None
    if not isinstance(raw, dict) or not isinstance(raw.get("binaries"), dict) or not raw["binaries"]:
        analysis.error = str(raw.get("error")) if isinstance(raw, dict) and raw.get("error") else "missing output"
        return analysis
    analysis.binaries = {k: set(v) for k, v in raw["binaries"].items()}
    return analysis


@dataclass
class Decision:
    """The verdict for one upstream advisory and one shipped entry.

    Attributes:
        group: The advisory group.
        entry: The shipped entry.
        verdict: One of the verdict constants.
        packages: Affected `(name, version)` pairs inside the artifact.
        functions: Vulnerable functions found, or checked when none were.
        binaries: Binaries the functions were found in.
        outcome: `filed`, `updated`, `on record`, `no token`, `capped`, `failed`, or
            empty when not affected.
    """

    group: Group
    entry: Shipped
    verdict: str
    packages: list[tuple[str, str]]
    functions: list[str] = field(default_factory=list)
    binaries: list[str] = field(default_factory=list)
    outcome: str = ""


def decide(group: Group, entry: Shipped, evidence: dict[str, Any], analysis: Analysis, root: Path) -> Decision:
    """Gives one (advisory, shipped entry) pair its verdict.

    Args:
        group: The advisory group (its lockfile is the entry's).
        entry: The shipped entry.
        evidence: `entries` of the plan output.
        analysis: The analyze job's output.
        root: Repository root.

    Returns:
        The decision (outcome not set).
    """
    ecosystem = osv.LOCKFILE_ECOSYSTEMS.get(Path(entry.lockfile).name, "")
    shipped = {tuple(p) for p in evidence.get(entry.key, {}).get("packages", [])}
    affected = affected_locked(group.record, entry.lockfile, root)
    inside = [p for p in affected if p in shipped]
    if not inside:
        return Decision(group, entry, NOT_SHIPPED, [])
    if entry.evidence == "npm-bundle":
        bundled = analysis.bundles.get(entry.key)
        if bundled is None:
            return Decision(group, entry, PRODUCTION_DEP, inside)
        in_bundle = [p for p in inside if p in set(bundled)]
        if in_bundle:
            return Decision(group, entry, IN_BUNDLE, in_bundle)
        return Decision(group, entry, TREE_SHAKEN, inside)
    if entry.evidence in ("npm", "pip"):
        return Decision(group, entry, PRODUCTION_DEP, inside)
    functions = sorted({f for name, _ in inside for f in vulnerable_functions(group.record, ecosystem, name)})
    if not functions:
        return Decision(group, entry, SHIPPED_NO_FUNCTIONS, inside)
    if not entry.call_analysis or analysis.error:
        return Decision(group, entry, SHIPPED_UNANALYZED, inside, functions)
    if analysis.binaries is None:
        return Decision(group, entry, AWAITING_CALLS, inside, functions)
    found = [f for f in functions if analysis.binaries_with(entry, f)]
    if found:
        where = sorted({b for f in found for b in analysis.binaries_with(entry, f)})
        return Decision(group, entry, REACHABLE, inside, found, where)
    return Decision(group, entry, NOT_CALLED, inside, functions)


def evidence_text(decision: Decision, analysis: Analysis) -> str:
    """Explains a verdict for the advisory body and the report."""
    entry = decision.entry
    names = ", ".join(f"`{f}`" for f in decision.functions)
    if decision.verdict == REACHABLE:
        return (
            f"Call analysis found {names} in the debug info of the fat-LTO release build of "
            + ", ".join(f"`{b}`" for b in decision.binaries)
            + ". LTO drops functions nothing calls, so the vulnerable code is linked in; whether"
            " attacker-controlled input reaches it still needs a maintainer's check."
        )
    if decision.verdict == SHIPPED_NO_FUNCTIONS:
        return (
            "The crate is in the shipped dependency closure (`cargo metadata`, every target platform). The"
            " upstream advisory names no vulnerable functions, so reachability is unproven: the draft is filed"
            " because the code ships."
        )
    if decision.verdict == SHIPPED_UNANALYZED:
        reason = (
            "this artifact is a library whose code runs in third-party plugins, with no binary to analyze"
            if not entry.call_analysis
            else f"the call-analysis build failed ({analysis.error})"
        )
        return (
            f"The crate is in the shipped dependency closure. The upstream advisory names vulnerable functions"
            f" ({names}), but {reason}, so reachability is unproven."
        )
    if decision.verdict == IN_BUNDLE:
        return (
            "The package's code is in the production web UI bundle (`vite build` source maps). Function-level"
            " reachability analysis does not exist for npm, so whether the vulnerable code runs is unproven."
        )
    if decision.verdict == PRODUCTION_DEP:
        extra = " The web UI bundle could not be built for a closer check." if entry.evidence == "npm-bundle" else ""
        return (
            "The package is a production dependency of the artifact (installed with it). Function-level"
            " reachability analysis does not exist for npm, so whether the vulnerable code runs is unproven." + extra
        )
    if decision.verdict == AWAITING_CALLS:
        return f"The crate ships and the advisory names {names}; the weekly call analysis decides."
    if decision.verdict == NOT_CALLED:
        return f"Call analysis found none of {names} in any shipped binary."
    if decision.verdict == TREE_SHAKEN:
        return "A production dependency, but none of its code is in the web UI bundle."
    return "Not part of this artifact."


def _metas(record: dict[str, Any]) -> list[dict[str, Any]]:
    """Returns the severity data of a merged record and each of its aliases, GHSA records first."""
    metas = record.get(ALIAS_META) or [record]
    return sorted(metas, key=lambda m: 0 if str(m.get("id", "")).startswith("GHSA-") else 1)


def severity_fields(record: dict[str, Any]) -> dict[str, Any]:
    """Copies the upstream severity for the advisory body.

    A CVSS 3.1 vector is preferred (from the GHSA record when there is one,
    since GitHub reviewed it; other CVSS versions are not sent, as the
    advisory API may refuse them), then the highest GHSA severity level,
    then medium.

    Args:
        record: The merged record.

    Returns:
        `{"cvss_vector_string": ...}` or `{"severity": ...}`.
    """
    metas = _metas(record)
    for meta in metas:
        for item in meta.get("severity") or []:
            if not isinstance(item, dict):
                continue
            score = item.get("score")
            if item.get("type") == "CVSS_V3" and isinstance(score, str) and score.startswith("CVSS:3.1/"):
                return {"cvss_vector_string": score}
    levels = []
    for meta in metas:
        specific = meta.get("database_specific")
        level = str(specific.get("severity") if isinstance(specific, dict) else "").upper()
        if level in GHSA_SEVERITY:
            levels.append(GHSA_SEVERITY[level])
    return {"severity": max(levels, key=SEVERITY_RANK.__getitem__) if levels else "medium"}


def cwe_ids(record: dict[str, Any]) -> list[str]:
    """Copies the upstream CWE ids from every alias record."""
    found: set[str] = set()
    for meta in _metas(record):
        specific = meta.get("database_specific")
        cwes = specific.get("cwe_ids") if isinstance(specific, dict) else None
        found.update(c for c in cwes or [] if isinstance(c, str) and CWE_RE.match(c))
    return sorted(found)


def artifact_version(artifact: Artifact, release_tag: str | None, root: Path) -> str | None:
    """Returns the top of the proposed affected range for an artifact.

    Args:
        artifact: The artifact.
        release_tag: Newest published release tag, or None before the first.
        root: Repository root.

    Returns:
        None before the first release (no range is proposed), otherwise the
        release tag or the version in the artifact's manifest.

    Raises:
        ValueError: The manifest has no version.
    """
    if release_tag is None:
        # Nothing is published before the first release, so no version is
        # affected yet, whatever a manifest says.
        return None
    if artifact.version == "release-tag":
        return release_tag
    path = root / artifact.version
    if path.suffix == ".json":
        version = json.loads(path.read_text(encoding="utf-8")).get("version")
    else:
        version = (tomllib.loads(path.read_text(encoding="utf-8")).get("project") or {}).get("version")
    if not isinstance(version, str) or not version:
        raise ValueError(f"{artifact.version} has no version")
    return version


def marker_lines(group: Group, artifact: Artifact) -> list[str]:
    """Returns the hidden marker lines that tie a draft to its upstream ids.

    Args:
        group: The upstream advisory (every id and alias it goes by).
        artifact: The Bookclerk artifact the draft is for.

    Returns:
        One `<!-- osv-advisory: <id> <advisory key> -->` line per id.
    """
    return [f"<!-- osv-advisory: {i} {artifact.advisory_key} -->" for i in group.ids]


def render_advisory(
    decision: Decision, analysis: Analysis, release_tag: str | None, version: str | None, commit: str, run_url: str
) -> dict[str, Any]:
    """Renders the draft advisory for an affected artifact.

    Args:
        decision: An affected decision.
        analysis: The analyze job's output.
        release_tag: Newest published release tag, or None before the first.
        version: Top of the proposed affected range (`artifact_version`),
            or None when no range is proposed.
        commit: The `main` commit the verdict was reached on.
        run_url: URL of the workflow run.

    Returns:
        A JSON-ready body for `POST /repos/{repo}/security-advisories`.
    """
    group = decision.group
    artifact = decision.entry.artifact
    ecosystem = osv.LOCKFILE_ECOSYSTEMS.get(Path(decision.entry.lockfile).name, "")
    names = sorted({n for n, _ in decision.packages})
    fixes = []
    for name, locked_version in decision.packages:
        parsed = osv.Version.parse(locked_version)
        fix = osv.smallest_fix(group.record, ecosystem, name, parsed) if parsed else None
        fixes.append(f"`{name}` {locked_version} → {fix if fix else 'no fixed release yet'}")
    lines: list[str] = []
    if version is None:
        lines += [
            "Filed automatically as a draft by the OSV advisory workflow. Check the verdict and severity.",
            "Bookclerk has no published release yet, so no affected range is proposed: fix it before the",
            "first release and close this draft, or set the range to the releases that ship the vulnerable",
            "code before publishing.",
            "",
        ]
    else:
        newest = f" (newest release `{release_tag}`)" if release_tag else ""
        lines += [
            "Filed automatically as a draft by the OSV advisory workflow. Check the verdict, severity and",
            f"affected range before publishing: the automation proposes every version up to `{version}`{newest}",
            "without having checked each release (it fills the range only while it is unset; the range set",
            "on this advisory is the one that counts). Narrow it, or close this draft, if a release does not",
            "contain the vulnerable code.",
            "",
        ]
    lines += [
        "## Upstream advisory",
        "",
        f"- Ids: {', '.join(group.ids)}",
        f"- Lockfile: `{decision.entry.lockfile}`",
        f"- Locked → first fixed version: {'; '.join(fixes)}",
    ]
    summary = group.record.get("summary")
    if isinstance(summary, str) and summary:
        lines.append(f"- Summary: {summary}")
    lines += [
        "",
        f"## Why {artifact.title} is affected",
        "",
        f"Verdict: **{decision.verdict}**. {evidence_text(decision, analysis)}",
        "",
        "## Fix",
        "",
        "When a fixed version is reachable within the manifests' requirements, the OSV remediation",
        "workflow opens a lockfile bump; otherwise the fix is code work (raise the requirement, patch or",
        "replace the dependency, or gate the feature). Prepare code work in a temporary private fork of",
        "this advisory. Once the fix is in a release, set the patched version here and publish.",
        "",
        f"{triage.VOLATILE_PREFIX}verdict reached on `{commit}` by {run_url}",
    ]
    description = [*marker_lines(group, artifact), "", *triage.auto_section(lines), ""]
    body: dict[str, Any] = {
        "summary": f"Vulnerable dependency {', '.join(names)} shipped in {artifact.package} ({group.primary})"[:1024],
        "description": "\n".join(description) + "\n",
        "cwe_ids": cwe_ids(group.record),
        "vulnerabilities": [
            {
                "package": {"ecosystem": artifact.ecosystem, "name": artifact.package},
                "vulnerable_version_range": f"<= {version}" if version else None,
                "patched_versions": None,
                "vulnerable_functions": [],
            }
        ],
    }
    body.update(severity_fields(group.record))
    return body


def render_report(
    decisions: list[Decision], unshipped: list[Group], analysis: Analysis, release_tag: str | None, unknown: int
) -> str:
    """Renders the maintainers' verdict report (sealed, never public).

    Args:
        decisions: Every decision of the run.
        unshipped: Groups on lockfiles that do not ship.
        analysis: The analyze job's output.
        release_tag: Newest release tag, or None.
        unknown: Alerts naming ids OSV does not know.

    Returns:
        Markdown.
    """
    if analysis.binaries is not None:
        calls = f"ran on {len(analysis.binaries)} binaries"
    else:
        calls = f"failed ({analysis.error})" if analysis.error else analysis.plan
    lines = [
        "# OSV advisory verdicts",
        "",
        f"Newest release: {release_tag or 'none (drafts propose no affected range)'}.",
        f"Call analysis: {calls}.",
        f"Alerts naming ids OSV does not know: {unknown}.",
        "",
        "| Advisory | Lockfile | Artifact | Verdict | Packages | Outcome |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    for d in decisions:
        packages = ", ".join(f"{n} {v}" for n, v in d.packages) or "-"
        lines.append(
            f"| {', '.join(d.group.ids)} | `{d.group.lockfile}` | {d.entry.artifact.key} "
            f"| {d.verdict} | {packages} | {d.outcome or '-'} |"
        )
    for g in unshipped:
        lines.append(f"| {', '.join(g.ids)} | `{g.lockfile}` | - | {NOT_SHIPPED} (lockfile) | - | - |")
    lines += ["", "## Evidence", ""]
    lines += [
        f"- {d.group.primary} / {d.entry.artifact.key}: {evidence_text(d, analysis)}"
        for d in decisions
        if d.verdict != NOT_SHIPPED
    ]
    return "\n".join(lines) + "\n"


def decide_all(
    state: dict[str, Any], config: Config, evidence: dict[str, Any], analysis: Analysis, root: Path
) -> tuple[list[Decision], list[Group]]:
    """Gives every open advisory a verdict per shipped entry of its lockfile.

    Args:
        state: Output of `fetch`.
        config: Shipped-artifact map.
        evidence: `entries` of the plan output.
        analysis: The analyze job's output.
        root: Repository root.

    Returns:
        The decisions, and the groups whose lockfile does not ship.
    """
    decisions: list[Decision] = []
    unshipped: list[Group] = []
    for raw in state["groups"]:
        group = Group.from_json(raw)
        entries = config.for_lockfile(group.lockfile)
        if not entries:
            unshipped.append(group)
            continue
        decisions += [decide(group, entry, evidence, analysis, root) for entry in entries]
    return decisions, unshipped


def draft_for(drafts: list[dict[str, Any]], group: Group, key: str) -> dict[str, Any] | None:
    """Finds the open draft that carries a marker of this advisory and package.

    Args:
        drafts: `drafts` of the fetch output (advisories still in draft).
        group: The upstream advisory.
        key: The artifact's advisory key (`<ecosystem>/<package>`).

    Returns:
        The draft, or None when the advisory is only on record as closed or
        published (or not at all).
    """
    for draft in drafts:
        found = set(MARKER_RE.findall(str(draft.get("description") or "")))
        if any((i, key) in found for i in group.ids):
            return draft
    return None


def file_decisions(
    decisions: list[Decision],
    gh: Any,
    state: dict[str, Any],
    analysis: Analysis,
    root: Path,
    commit: str,
    run_url: str,
) -> dict[str, int]:
    """Files a draft advisory for each affected decision not on record.

    An affected decision whose draft is still open gets that draft updated
    (`triage.draft_changes`) instead of a second one; one whose advisory was closed
    or published is left alone. Without a token (a local dry run) nothing is filed and the affected
    decisions are marked `no token`. Several shipped entries of
    one artifact (the Rust host and the web UI are both `bookclerk`) that
    hit the same upstream advisory get one draft. A draft GitHub refuses is
    marked `failed` and the others are still filed.

    Args:
        decisions: Output of `decide_all`; `outcome` is set in place.
        gh: Client with an advisory-write token (`create_advisory`,
            `update_advisory`), or None.
        state: Output of `fetch` (release tag or None, markers on record,
            open drafts).
        analysis: The analyze job's output.
        root: Repository root.
        commit: Commit the verdicts were reached on.
        run_url: URL of the workflow run.

    Returns:
        Counts by outcome.
    """
    release_tag = state.get("release_tag")
    on_record = {tuple(m) for m in state.get("on_record", [])}
    drafts = state.get("drafts", [])
    refreshed: set[str] = set()
    counts = {"filed": 0, "updated": 0, "on record": 0, "no token": 0, "capped": 0, "failed": 0}
    for decision in decisions:
        if decision.verdict not in AFFECTED:
            if decision.verdict in (NOT_CALLED, TREE_SHAKEN) and draft_for(
                drafts, decision.group, decision.entry.artifact.advisory_key
            ):
                # The draft is left as is; the report tells a maintainer it may be closed.
                decision.outcome = "open draft, now not affected"
            continue
        artifact = decision.entry.artifact
        key = artifact.advisory_key
        if any((i, key) in on_record for i in decision.group.ids):
            decision.outcome = "on record"
            draft = draft_for(drafts, decision.group, key)
            if gh is not None and draft is not None and draft["ghsa_id"] not in refreshed:
                refreshed.add(draft["ghsa_id"])
                try:
                    version = artifact_version(artifact, release_tag, root)
                    body = render_advisory(decision, analysis, release_tag, version, commit, run_url)
                    changes = triage.draft_changes(draft, body, marker_lines(decision.group, artifact))
                    if changes and counts["updated"] >= MAX_FILED_PER_RUN:
                        decision.outcome = "capped"
                    elif changes:
                        gh.update_advisory(draft["ghsa_id"], changes)
                        decision.outcome = "updated"
                except (OSError, ValueError, KeyError, TypeError):
                    decision.outcome = "failed"
        elif gh is None:
            decision.outcome = "no token"
        elif counts["filed"] >= MAX_FILED_PER_RUN:
            decision.outcome = "capped"
        else:
            try:
                version = artifact_version(decision.entry.artifact, release_tag, root)
                gh.create_advisory(render_advisory(decision, analysis, release_tag, version, commit, run_url))
            except (OSError, ValueError, KeyError, TypeError):
                decision.outcome = "failed"
            else:
                on_record.update((i, key) for i in decision.group.ids)
                decision.outcome = "filed"
        counts[decision.outcome] += 1
    return counts


def cmd_file(args: argparse.Namespace) -> int:
    """Decides every advisory, files the affected ones and writes the report.

    Args:
        args: `--state`, `--plan`, `--calls`, `--bundle`, `--report`.

    Returns:
        The exit code: 1 when a draft could not be filed.
    """
    config = load_config(Path(args.config))
    state = json.loads(Path(args.state).read_text(encoding="utf-8"))
    plan = json.loads(Path(args.plan).read_text(encoding="utf-8"))
    analysis = load_analysis(
        plan.get("call_analysis", "not needed"),
        Path(args.calls) if args.calls else None,
        Path(args.bundle) if args.bundle else None,
    )
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    token = os.environ.get("GH_TOKEN", "")
    gh = GitHub(repo, token, os.environ.get("GITHUB_API_URL", API)) if repo and token else None
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    run_url = f"{server}/{repo}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}"
    decisions, unshipped = decide_all(state, config, plan["entries"], analysis, ROOT)
    counts = file_decisions(decisions, gh, state, analysis, ROOT, os.environ.get("GITHUB_SHA", "unknown"), run_url)
    report = render_report(
        decisions, unshipped, analysis, state.get("release_tag"), int(state.get("unknown_alerts", 0))
    )
    Path(args.report).write_text(report, encoding="utf-8")
    affected = sum(1 for d in decisions if d.verdict in AFFECTED)
    print(
        f"verdicts: {len(decisions)} (advisory x shipped entry), {affected} affected; "
        f"drafts filed: {counts['filed']}, updated: {counts['updated']}, failed: {counts['failed']}, on record: {counts['on record']}, "
        f"not filed (no token): {counts['no token']}, over the per-run cap: {counts['capped']}; "
        f"advisories on lockfiles that do not ship: {len(unshipped)}"
    )
    return 1 if counts["failed"] else 0


def main(argv: list[str] | None = None) -> int:
    """Parses arguments and runs one subcommand.

    Args:
        argv: Arguments without the program name; defaults to `sys.argv`.

    Returns:
        The process exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", default=str(CONFIG), help="shipped-artifact map")
    sub = parser.add_subparsers(dest="command", required=True)
    fetch = sub.add_parser("fetch", help="read alerts, records, releases and advisories (alert-read token)")
    fetch.add_argument("--out", required=True)
    fetch.add_argument("--ref", default="refs/heads/main")
    plan = sub.add_parser("plan", help="compute what each artifact ships; decide on call analysis (no token)")
    plan.add_argument("--state", required=True)
    plan.add_argument("--out", required=True)
    plan.add_argument("--event", default="", help="event that triggered the OSV scan")
    plan.add_argument("--force-call-analysis", action="store_true")
    plan.add_argument("--github-output", default="")
    bundle = sub.add_parser("bundle", help="build the web UI and list its bundled packages (no secrets)")
    bundle.add_argument("--out", required=True)
    calls = sub.add_parser("calls", help="build the shipped binaries and digest their functions (no secrets)")
    calls.add_argument("--out", required=True)
    calls.add_argument("--target-dir", required=True)
    fil = sub.add_parser("file", help="decide verdicts, file drafts, write the report")
    fil.add_argument("--state", required=True)
    fil.add_argument("--plan", required=True)
    fil.add_argument("--calls", default="")
    fil.add_argument("--bundle", default="")
    fil.add_argument("--report", required=True)
    args = parser.parse_args(argv)
    handlers: dict[str, Callable[[argparse.Namespace], int]] = {
        "fetch": cmd_fetch,
        "plan": cmd_plan,
        "bundle": cmd_bundle,
        "calls": cmd_calls,
        "file": cmd_file,
    }
    return handlers[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
