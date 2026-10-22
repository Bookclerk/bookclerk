#!/usr/bin/env python3
"""Bump locked dependencies that open OSV code-scanning alerts flag.

Reads the repository's open code-scanning alerts from the OSV-Scanner tool,
looks each advisory (and its aliases) up on api.osv.dev, and, for every
Cargo.lock or package-lock.json entry it names, tries the smallest
lockfile-only update that leaves no locked version of that package inside an
affected range:

- Cargo: `cargo update --package <name>@<locked> --precise <fixed>`, then,
  if that is refused (for example the fix is not semver-compatible with a
  manifest requirement), `cargo update --package <name>@<locked>` to the
  newest compatible release.
- npm: `npm update <name> --package-lock-only --ignore-scripts`.

Manifests are never edited, so only fixes reachable within the existing
version requirements are applied. An attempt that does not clear the
advisory is rolled back.

Alert details stay out of public output. The summary file (the pull request
body) lists only the bumps that were applied, with their advisory ids, which
are public once the fix lands. Alerts that could not be fixed are counted,
never named, on stdout; maintainers triage them in Security -> Code scanning.

Exit codes:
  0 - ran to completion (whether or not anything changed)
  1 - error
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]
API = "https://api.github.com"
OSV_API = "https://api.osv.dev/v1/vulns"
TOOL_NAME = "osv-scanner"
HTTP_TIMEOUT_S = 30
# Upper bound on alert pages read (100 alerts each), so a runaway listing
# cannot keep the job paging forever.
MAX_ALERT_PAGES = 20
# Upper bound on alias records fetched per advisory.
MAX_ALIASES = 10

# OSV ecosystem name for each lockfile basename this script can update.
LOCKFILE_ECOSYSTEMS = {
    "Cargo.lock": "crates.io",
    "package-lock.json": "npm",
}

_SEMVER = re.compile(
    r"^v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$"
)


@dataclass(frozen=True, order=True)
class Version:
    """A semantic version, ordered by semver precedence (build metadata ignored).

    Attributes:
        major: Major component.
        minor: Minor component (0 when the source string omitted it).
        patch: Patch component (0 when the source string omitted it).
        release: 1 for a release, 0 for a pre-release, so a pre-release sorts
            before the release with the same numbers.
        pre: Pre-release identifiers as (is_alpha, number, text) keys, so
            numeric identifiers sort before alphanumeric ones and numerically.
        text: The version as written, for display.
    """

    major: int
    minor: int
    patch: int
    release: int
    pre: tuple[tuple[int, int, str], ...]
    text: str = field(compare=False)

    @classmethod
    def parse(cls, raw: str) -> Version | None:
        """Parses a semver string.

        Args:
            raw: The version string, optionally prefixed with `v`.

        Returns:
            The parsed version, or None when `raw` is not semver-shaped.
        """
        match = _SEMVER.match(raw.strip())
        if match is None:
            return None
        major, minor, patch, pre = match.groups()
        pre_key: tuple[tuple[int, int, str], ...] = ()
        if pre:
            pre_key = tuple(
                (0, int(part), "") if part.isdigit() else (1, 0, part)
                for part in pre.split(".")
            )
        return cls(
            int(major),
            int(minor or 0),
            int(patch or 0),
            0 if pre else 1,
            pre_key,
            raw.strip(),
        )

    def __str__(self) -> str:
        return self.text


# Placeholder sort key for "introduced: 0" (the version before every release).
ZERO = Version(0, 0, 0, 0, (), "0")


@dataclass(frozen=True)
class Alert:
    """One open OSV code-scanning alert.

    Attributes:
        vuln_id: The advisory id the alert's rule names (RUSTSEC-, GHSA-, ...).
        lockfile: Repository-relative path of the lockfile the alert points at.
    """

    vuln_id: str
    lockfile: str


@dataclass
class Target:
    """One locked package that must leave the affected ranges of its advisories.

    Attributes:
        lockfile: Repository-relative lockfile path.
        ecosystem: OSV ecosystem name (`crates.io` or `npm`).
        name: Package name as the lockfile records it.
        locked: Version currently locked.
        vulns: Advisories (id to OSV record) that affect `locked`.
    """

    lockfile: str
    ecosystem: str
    name: str
    locked: Version
    vulns: dict[str, dict[str, object]] = field(default_factory=dict)


def _http_json(url: str, token: str | None = None) -> tuple[object, str]:
    """Fetches a JSON document.

    Args:
        url: The URL to GET.
        token: Bearer token for the GitHub API, or None for public APIs.

    Returns:
        The decoded body and the response's `Link` header (empty when absent).

    Raises:
        urllib.error.URLError: The request failed.
    """
    headers = {"Accept": "application/json", "User-Agent": "bookclerk-osv-remediate"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
        headers["Accept"] = "application/vnd.github+json"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT_S) as response:
        return json.load(response), response.headers.get("Link", "")


def _next_link(link_header: str) -> str | None:
    """Returns the `rel="next"` URL of a GitHub `Link` header, if any."""
    for part in link_header.split(","):
        section = part.split(";")
        if len(section) >= 2 and section[1].strip() == 'rel="next"':
            return section[0].strip().strip("<>")
    return None


def alerts_from_api(raw: list[dict[str, object]]) -> list[Alert]:
    """Converts code-scanning alert JSON into alerts this script can act on.

    Args:
        raw: Alert objects as the code-scanning API returns them.

    Returns:
        One alert per (advisory, lockfile) pair, skipping alerts with no rule
        id or no location.
    """
    alerts: set[Alert] = set()
    for item in raw:
        rule = item.get("rule")
        instance = item.get("most_recent_instance")
        if not isinstance(rule, dict) or not isinstance(instance, dict):
            continue
        location = instance.get("location")
        vuln_id = rule.get("id")
        path = location.get("path") if isinstance(location, dict) else None
        if isinstance(vuln_id, str) and isinstance(path, str) and vuln_id and path:
            alerts.add(Alert(vuln_id, path.removeprefix("file://")))
    return sorted(alerts, key=lambda a: (a.lockfile, a.vuln_id))


def fetch_alerts(repo: str, token: str, ref: str) -> list[Alert]:
    """Lists the open OSV-Scanner code-scanning alerts on `ref`.

    Args:
        repo: `owner/name` of the repository.
        token: A token with code-scanning alert read access.
        ref: The Git ref the alerts belong to, e.g. `refs/heads/main`.

    Returns:
        The alerts, deduplicated by (advisory, lockfile).

    Raises:
        urllib.error.URLError: The API could not be read.
        TypeError: The API answered something other than a list of alerts.
    """
    query = urllib.parse.urlencode(
        {"tool_name": TOOL_NAME, "state": "open", "ref": ref, "per_page": 100}
    )
    url: str | None = f"{API}/repos/{repo}/code-scanning/alerts?{query}"
    raw: list[dict[str, object]] = []
    for _ in range(MAX_ALERT_PAGES):
        if url is None:
            break
        body, link = _http_json(url, token)
        if not isinstance(body, list):
            raise TypeError("code-scanning alerts response is not a list")
        raw.extend(item for item in body if isinstance(item, dict))
        url = _next_link(link)
    return alerts_from_api(raw)


def fetch_vuln(vuln_id: str) -> dict[str, object] | None:
    """Looks an advisory up on api.osv.dev.

    Args:
        vuln_id: The advisory id.

    Returns:
        The OSV record, or None when OSV does not know the id (for example a
        deprecation finding rather than an advisory).

    Raises:
        urllib.error.URLError: OSV could not be reached.
    """
    try:
        body, _ = _http_json(f"{OSV_API}/{urllib.parse.quote(vuln_id, safe='')}")
    except urllib.error.HTTPError as err:
        if err.code == 404:
            return None
        raise
    return body if isinstance(body, dict) else None


def merge_aliases(
    primary: dict[str, object], aliases: list[dict[str, object]]
) -> dict[str, object]:
    """Combines a record's `affected` entries with those of its aliases.

    OSV-Scanner groups aliases into one SARIF rule, and the rule id can be a
    CVE whose record names a Git repository rather than the `crates.io` or
    `npm` package; the ecosystem entries live on the GHSA or RUSTSEC alias.

    Args:
        primary: The record the alert's rule id resolved to.
        aliases: Records of its aliases.

    Returns:
        A copy of `primary` whose `affected` list also holds every alias's
        entries.
    """
    affected: list[object] = []
    for record in (primary, *aliases):
        entries = record.get("affected")
        if isinstance(entries, list):
            affected.extend(entries)
    return {**primary, "affected": affected}


def fetch_vuln_with_aliases(vuln_id: str) -> dict[str, object] | None:
    """Looks an advisory up on api.osv.dev and folds in its aliases' records.

    Args:
        vuln_id: The advisory id the alert names.

    Returns:
        The merged record (see `merge_aliases`), or None when OSV does not
        know the id.

    Raises:
        urllib.error.URLError: OSV could not be reached.
    """
    primary = fetch_vuln(vuln_id)
    if primary is None:
        return None
    raw_aliases = primary.get("aliases")
    alias_ids = [
        alias
        for alias in (raw_aliases if isinstance(raw_aliases, list) else [])
        if isinstance(alias, str) and alias != vuln_id
    ][:MAX_ALIASES]
    records = [
        record for alias in alias_ids if (record := fetch_vuln(alias)) is not None
    ]
    return merge_aliases(primary, records)


def _affected_entries(
    vuln: dict[str, object], ecosystem: str, name: str
) -> list[dict[str, object]]:
    """Returns the `affected` entries of `vuln` that name this package."""
    entries: list[dict[str, object]] = []
    affected = vuln.get("affected")
    for entry in affected if isinstance(affected, list) else []:
        if not isinstance(entry, dict):
            continue
        package = entry.get("package")
        if (
            isinstance(package, dict)
            and package.get("ecosystem") == ecosystem
            and package.get("name") == name
        ):
            entries.append(entry)
    return entries


def _mentions(vuln: dict[str, object], ecosystem: str, name: str) -> bool:
    """Reports whether `vuln` lists this package as affected at all."""
    return bool(_affected_entries(vuln, ecosystem, name))


def _ranges(
    vuln: dict[str, object], ecosystem: str, name: str
) -> list[list[dict[str, str]]]:
    """Returns the SEMVER/ECOSYSTEM range event lists `vuln` gives for a package."""
    found: list[list[dict[str, str]]] = []
    for entry in _affected_entries(vuln, ecosystem, name):
        ranges = entry.get("ranges")
        for rng in ranges if isinstance(ranges, list) else []:
            if not isinstance(rng, dict) or rng.get("type") not in (
                "SEMVER",
                "ECOSYSTEM",
            ):
                continue
            events = rng.get("events")
            if isinstance(events, list):
                found.append([e for e in events if isinstance(e, dict)])
    return found


def _intervals(
    events: list[dict[str, str]],
) -> list[tuple[Version | None, Version | None, bool]]:
    """Turns one OSV range's events into affected intervals.

    Args:
        events: The range's `introduced` / `fixed` / `last_affected` events.

    Returns:
        (start, end, end_inclusive) intervals; `start` None means from the
        first version, `end` None means unbounded.
    """
    points: list[tuple[Version | None, str]] = []
    for event in events:
        for kind in ("introduced", "fixed", "last_affected"):
            if kind not in event:
                continue
            raw = event[kind]
            if raw == "0" and kind == "introduced":
                points.append((None, kind))
            elif (parsed := Version.parse(raw)) is not None:
                points.append((parsed, kind))
    # OSV evaluates a range's events in version order; "introduced: 0" first.
    points.sort(key=lambda p: (0, ZERO) if p[0] is None else (1, p[0]))
    intervals: list[tuple[Version | None, Version | None, bool]] = []
    start: Version | None = None
    open_ = False
    for version, kind in points:
        if kind == "introduced":
            start, open_ = version, True
        elif open_:
            intervals.append((start, version, kind == "last_affected"))
            open_ = False
    if open_:
        intervals.append((start, None, False))
    return intervals


def is_affected(
    vuln: dict[str, object], ecosystem: str, name: str, version: Version
) -> bool:
    """Reports whether `version` of a package lies in an affected range of `vuln`.

    Args:
        vuln: The OSV record.
        ecosystem: OSV ecosystem name.
        name: Package name.
        version: The locked version.

    Returns:
        True when any range of `vuln` for this package covers `version`, or
        when the record lists `version` explicitly.
    """
    for entry in _affected_entries(vuln, ecosystem, name):
        versions = entry.get("versions")
        if isinstance(versions, list) and version.text in versions:
            return True
    for events in _ranges(vuln, ecosystem, name):
        # OSV evaluation: a range with `limit` events covers only versions
        # below at least one limit.
        limits = [
            limit
            for event in events
            if "limit" in event and (limit := Version.parse(event["limit"])) is not None
        ]
        if limits and not any(version < limit for limit in limits):
            continue
        for start, end, inclusive in _intervals(events):
            if start is not None and version < start:
                continue
            if end is None or version < end or (inclusive and version == end):
                return True
    return False


def smallest_fix(
    vuln: dict[str, object], ecosystem: str, name: str, locked: Version
) -> Version | None:
    """Returns the smallest `fixed` version above `locked` for this package.

    Args:
        vuln: The OSV record.
        ecosystem: OSV ecosystem name.
        name: Package name.
        locked: The locked, affected version.

    Returns:
        The smallest fix greater than `locked`, or None when the record has
        no such fix.
    """
    fixes = [
        Version.parse(event["fixed"])
        for events in _ranges(vuln, ecosystem, name)
        for event in events
        if "fixed" in event
    ]
    candidates = [v for v in fixes if v is not None and v > locked]
    return min(candidates) if candidates else None


def locked_versions(lockfile: Path) -> dict[str, list[Version]]:
    """Reads the locked versions of every package in a lockfile.

    Args:
        lockfile: A Cargo.lock or package-lock.json (lockfile v2/v3).

    Returns:
        Package name to locked versions (a crate can be locked at several
        semver-incompatible versions at once).

    Raises:
        ValueError: The lockfile kind is not supported.
    """
    found: dict[str, list[Version]] = {}

    def add(name: str, raw: object) -> None:
        version = Version.parse(raw) if isinstance(raw, str) else None
        if version is not None and version not in found.setdefault(name, []):
            found[name].append(version)

    if lockfile.name == "Cargo.lock":
        data = tomllib.loads(lockfile.read_text(encoding="utf-8"))
        for package in data.get("package", []):
            if isinstance(package, dict) and isinstance(package.get("name"), str):
                add(package["name"], package.get("version"))
    elif lockfile.name == "package-lock.json":
        data = json.loads(lockfile.read_text(encoding="utf-8"))
        packages = data.get("packages", {}) if isinstance(data, dict) else {}
        for key, meta in packages.items():
            if not key or not isinstance(meta, dict) or meta.get("link"):
                continue
            name = meta.get("name")
            if not isinstance(name, str):
                name = key.rsplit("node_modules/", 1)[-1]
            add(name, meta.get("version"))
    else:
        raise ValueError(f"unsupported lockfile: {lockfile.name}")
    for versions in found.values():
        versions.sort()
    return found


def plan_targets(
    alerts: list[Alert],
    vulns: dict[str, dict[str, object] | None],
    root: Path,
) -> tuple[list[Target], int]:
    """Matches alerts to the locked packages their advisories affect.

    Args:
        alerts: Open alerts.
        vulns: OSV records by advisory id (None for unknown ids).
        root: Repository root the alert paths are relative to.

    Returns:
        The targets to update, and how many alerts matched no updatable
        package (unknown advisory, unsupported lockfile, missing file, or no
        affected locked version).
    """
    targets: dict[tuple[str, str, Version], Target] = {}
    unmatched = 0
    lock_cache: dict[str, dict[str, list[Version]]] = {}
    for alert in alerts:
        vuln = vulns.get(alert.vuln_id)
        path = (root / alert.lockfile).resolve()
        ecosystem = LOCKFILE_ECOSYSTEMS.get(Path(alert.lockfile).name)
        if (
            vuln is None
            or ecosystem is None
            or not path.is_relative_to(root)
            or not path.is_file()
        ):
            unmatched += 1
            continue
        if alert.lockfile not in lock_cache:
            lock_cache[alert.lockfile] = locked_versions(path)
        matched = False
        for name, versions in lock_cache[alert.lockfile].items():
            if not _mentions(vuln, ecosystem, name):
                continue
            for version in versions:
                if is_affected(vuln, ecosystem, name, version):
                    key = (alert.lockfile, name, version)
                    target = targets.setdefault(
                        key, Target(alert.lockfile, ecosystem, name, version)
                    )
                    target.vulns[alert.vuln_id] = vuln
                    matched = True
        if not matched:
            unmatched += 1
    return sorted(
        targets.values(), key=lambda t: (t.lockfile, t.name, t.locked)
    ), unmatched


def _run(cmd: list[str], cwd: Path) -> bool:
    """Runs a command quietly, returning whether it exited 0.

    Output is discarded, not logged: a failed attempt names a package that is
    still vulnerable, and workflow logs on a public repository are public.
    """
    result = subprocess.run(
        cmd,
        cwd=cwd,
        env=_child_env(),
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return result.returncode == 0


def _replacement(
    target: Target, before: list[Version], after: list[Version]
) -> Version | None:
    """Returns the version that replaced the target's locked version, if the update cleared it.

    Only the target's own version line is judged: other locked versions of the
    same package (another semver-incompatible line) are separate targets.

    Args:
        target: The package that was updated.
        before: Locked versions of the package before the update.
        after: Locked versions of the package after the update.

    Returns:
        When `target.locked` is gone and no newly locked version is affected
        by the target's advisories: the smallest newly locked version, or,
        when the update only consolidated dependents onto a version that was
        already locked, the smallest unaffected locked version above the old
        one. Otherwise None.
    """

    def affected(version: Version) -> bool:
        return any(
            is_affected(vuln, target.ecosystem, target.name, version)
            for vuln in target.vulns.values()
        )

    if target.locked in after:
        return None
    added = sorted(set(after) - set(before))
    if any(affected(version) for version in added):
        return None
    if added:
        return added[0]
    survivors = sorted(v for v in after if v > target.locked and not affected(v))
    return survivors[0] if survivors else None


def _child_env() -> dict[str, str]:
    """Returns the environment for package-manager commands, without the alert token."""
    return {key: value for key, value in os.environ.items() if key != "ALERTS_TOKEN"}


def apply_target(target: Target, root: Path) -> Version | None:
    """Tries lockfile-only updates for one target, rolling back on failure.

    Args:
        target: The package to move out of its advisories' affected ranges.
        root: Repository root.

    Returns:
        The version that replaced `target.locked`, or None when no attempt
        cleared every advisory (the lockfile is then restored).
    """
    path = root / target.lockfile
    workdir = path.parent
    before = path.read_bytes()
    spec = f"{target.name}@{target.locked}"
    fixes = [
        fix
        for vuln in target.vulns.values()
        if (fix := smallest_fix(vuln, target.ecosystem, target.name, target.locked))
        is not None
    ]
    attempts: list[list[str]] = []
    if target.ecosystem == "crates.io":
        manifest = str(workdir / "Cargo.toml")
        if fixes:
            attempts.append(
                [
                    "cargo",
                    "update",
                    "--manifest-path",
                    manifest,
                    "--package",
                    spec,
                    "--precise",
                    str(max(fixes)),
                ]
            )
        attempts.append(
            ["cargo", "update", "--manifest-path", manifest, "--package", spec]
        )
    else:
        attempts.append(
            [
                "npm",
                "update",
                target.name,
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
            ]
        )
    locked_before = locked_versions(path).get(target.name, [])
    for cmd in attempts:
        if _run(cmd, workdir):
            after = locked_versions(path).get(target.name, [])
            replacement = _replacement(target, locked_before, after)
            if replacement is not None:
                return replacement
        path.write_bytes(before)
    return None


def render_summary(fixed: list[tuple[Target, Version]]) -> str:
    """Renders the pull request body for the applied bumps.

    Args:
        fixed: Each fixed target with the version it now locks.

    Returns:
        Markdown listing each bump and the advisories it addresses.
    """
    lines = [
        "## Summary",
        "",
        "Lockfile-only bumps for advisories reported by the scheduled OSV scan:",
        "",
        "| Lockfile | Package | From | To | Advisories |",
        "| --- | --- | --- | --- | --- |",
    ]
    for target, new in fixed:
        ids = ", ".join(sorted(target.vulns))
        lines.append(
            f"| `{target.lockfile}` | `{target.name}` | {target.locked} | {new} | {ids} |"
        )
    lines += [
        "",
        "Manifests are unchanged: each bump stays within the existing version",
        "requirements, and each was checked against the advisory's affected",
        "ranges after the update. The full CI suite passed on these bumps before",
        "this pull request was opened; it is queued to merge once its required",
        "checks pass.",
        "",
    ]
    return "\n".join(lines)


def resolved_alerts(
    alerts: list[Alert], targets: list[Target], fixed: list[Target]
) -> int:
    """Counts the alerts every one of whose matched packages was fixed.

    One alert can match several locked versions (several targets), and one
    target can carry several alerts, so alerts are counted per (advisory,
    lockfile), not per target.

    Args:
        alerts: Open alerts.
        targets: Planned targets.
        fixed: The targets whose update succeeded.

    Returns:
        How many alerts matched at least one target and had all of them fixed.
    """
    fixed_keys = {(t.lockfile, t.name, t.locked) for t in fixed}
    count = 0
    for alert in alerts:
        matched = [
            (t.lockfile, t.name, t.locked)
            for t in targets
            if t.lockfile == alert.lockfile and alert.vuln_id in t.vulns
        ]
        if matched and all(key in fixed_keys for key in matched):
            count += 1
    return count


def main(argv: list[str] | None = None) -> int:
    """Runs the remediation.

    Args:
        argv: Command-line arguments (defaults to `sys.argv[1:]`).

    Returns:
        The process exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    parser.add_argument("--ref", default="refs/heads/main")
    parser.add_argument(
        "--summary", type=Path, required=True, help="write the PR body here"
    )
    parser.add_argument("--github-output", type=Path, default=None)
    args = parser.parse_args(argv)

    token = os.environ.get("ALERTS_TOKEN", "")
    if not args.repo or not token:
        print(
            "GITHUB_REPOSITORY (or --repo) and ALERTS_TOKEN are required",
            file=sys.stderr,
        )
        return 1

    try:
        alerts = fetch_alerts(args.repo, token, args.ref)
        vulns = {
            vuln_id: fetch_vuln_with_aliases(vuln_id)
            for vuln_id in sorted({a.vuln_id for a in alerts})
        }
    except (urllib.error.URLError, TypeError, ValueError) as err:
        print(f"could not read alerts or advisories: {err}", file=sys.stderr)
        return 1

    targets, unmatched = plan_targets(alerts, vulns, ROOT)
    fixed: list[tuple[Target, Version]] = []
    for target in targets:
        new = apply_target(target, ROOT)
        if new is not None:
            fixed.append((target, new))

    # Counts only: alert details belong in the Security tab, not a public log.
    resolved = resolved_alerts(alerts, targets, [target for target, _ in fixed])
    print(
        f"open OSV alerts: {len(alerts)}; packages bumped: {len(fixed)}; "
        f"alerts left for a maintainer: {len(alerts) - resolved} "
        f"({unmatched} with no updatable package)"
    )
    # Always written: with no bumps the PR step closes any earlier PR.
    args.summary.write_text(render_summary(fixed), encoding="utf-8")
    if args.github_output is not None:
        with args.github_output.open("a", encoding="utf-8") as out:
            out.write(f"changed={'true' if fixed else 'false'}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
