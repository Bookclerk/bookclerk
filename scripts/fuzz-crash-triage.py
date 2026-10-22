#!/usr/bin/env python3
"""Triage crashes from the scheduled fuzzing run without making them public.

The daily ClusterFuzzLite batch run (`.github/workflows/cflite-batch.yml`)
runs on a public repository, where every job log and Actions artifact can be
read by anyone. The stock `run_fuzzers` action prints each crash's stack
trace and libFuzzer output (which includes the crashing input for small
inputs) to the job log and uploads the reproducer as an artifact. This script
replaces those public paths with three private steps:

- ``run`` runs inside the pinned ClusterFuzzLite run-fuzzers image as a
  container action (`.github/actions/cflite-private-run`). It runs the
  image's own batch, prune or coverage mode with stdout and stderr
  redirected to a private log and with the crash upload disabled, so the
  corpus and coverage artifacts work as before while crashes stay in the
  job's workspace. It prints a crash count only.
- ``reproduce`` runs inside the same image against a fresh build of the
  fuzzers. It re-runs each crash and drops the ones that do not reproduce,
  minimizes the rest, classifies them (sanitizer error, panic, out of
  memory, timeout, failed test oracle) and deduplicates them by the top
  frames of their stack.
- ``file`` runs on the runner with the automation App token. It maps each
  unique crash to its boundary (``fuzz/boundaries.toml``) for a default
  severity and files it: a draft repository security advisory for a crash
  in shipped code (drafts are private to maintainers, so this holds before
  the first release too), or an issue for a failed test oracle. Crashes
  already on record (by signature marker) are skipped.

The workflow seals the crash files, the logs and the triage output with the
FUZZ_CRASH_BUNDLE_KEY secret (`scripts/osv-remediation-bundle.sh`) before
anything leaves the job, so the only copies of a reproducer outside the
runner are the sealed bundle and the body of a private draft advisory.

Exit codes:
  0 - ran to completion (whether or not anything crashed or was filed)
  1 - error
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Callable, Iterable
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
API = "https://api.github.com"
HTTP_TIMEOUT_S = 30
# Upper bound on pages read from one listing (100 items each).
MAX_PAGES = 20

# Where the ClusterFuzzLite run-fuzzers image keeps its Python package.
CIFUZZ_DIR = "/opt/oss-fuzz/infra/cifuzz"
# Directory under the workspace for the private fuzzing log; sealed, never
# uploaded in the clear.
PRIVATE_DIR = "fuzz-private"
# Exit codes of the image's entrypoint: 0 clean, 2 bug found.
CIFUZZ_OK = 0
CIFUZZ_BUG_FOUND = 2

# Attempts to reproduce one crash before it is dropped as flaky.
REPRODUCE_ATTEMPTS = 3
# Wall-clock cap for one reproduction run.
REPRODUCE_TIMEOUT_S = 120
# Time libFuzzer may spend minimizing one crash.
MINIMIZE_SECONDS = 120
# Wall-clock cap for one minimization run, above MINIMIZE_SECONDS so
# libFuzzer's own budget ends it first.
MINIMIZE_TIMEOUT_S = MINIMIZE_SECONDS + 60
# libFuzzer flags for reproduction: the same memory and per-input time limits
# ClusterFuzzLite fuzzes with (FUZZER_ARGS in the image).
LIBFUZZER_LIMITS = ("-rss_limit_mb=2560", "-timeout=25")
# Stack frames that make up a crash signature.
SIGNATURE_FRAMES = 3
# Frames kept in the private triage record and advisory body.
STACK_FRAMES_KEPT = 40
# Reproducers up to this size go into the private advisory body (base64);
# larger ones are only in the sealed bundle.
INLINE_REPRODUCER_MAX = 4096

# Body marker that identifies the issue or advisory for one crash signature.
MARKER_PREFIX = "<!-- fuzz-crash-signature: "
MARKER_SUFFIX = " -->"
# Label on every issue this script opens.
ISSUE_LABEL = "fuzz-crash"

# Bounds of the part of a draft advisory the automation owns. A later run that
# finds the same problem rewrites only this part of a draft (never a closed
# or published advisory), so a maintainer's notes outside it survive.
AUTO_BEGIN = "<!-- bookclerk-auto: begin (rewritten by automation while this is a draft) -->"
AUTO_END = "<!-- bookclerk-auto: end -->"

# Lines inside the automated part that change on every run (the commit and
# run that last saw the problem). They are refreshed only together with a
# real change, so an unchanged finding does not rewrite its draft daily.
VOLATILE_PREFIX = "Last seen: "

# Function names (exact) and name prefixes of frames that are runtime,
# sanitizer, fuzzer or standard-library plumbing rather than the code under
# test.
IGNORED_FRAME_NAMES = frozenset(
    {
        "_start",
        "abort",
        "raise",
        "gsignal",
        "main",
        "malloc",
        "calloc",
        "realloc",
        "free",
        "memcpy",
        "memmove",
        "memset",
        "memcmp",
        "bcmp",
        "strlen",
        "LLVMFuzzerTestOneInput",
        "rust_fuzzer_test_input",
        "rust_panic",
        "rust_begin_unwind",
    }
)
# Location fragments of toolchain, fuzzer-runtime and libc frames.
IGNORED_LOCATION_PARTS = (
    "/rustc/",
    "/rustlib/src/rust/library/",
    "compiler-rt/",
    "libfuzzer-sys-",
    "libc.so",
)
IGNORED_FRAME_PREFIXES = (
    "__sanitizer",
    "__asan",
    "__lsan",
    "__ubsan",
    "__msan",
    "__interceptor",
    "__interception",
    "__libc",
    "__GI_",
    "__rust",
    "__scrt",
    "__cxa",
    "operator new",
    "operator delete",
    "fuzzer::",
    "libfuzzer_sys::",
    "panic_abort::",
    "std::",
    "core::",
    "alloc::",
    "<std::",
    "<core::",
    "<alloc::",
)

# Matches one symbolized sanitizer frame: index, address, then the rest.
FRAME_RE = re.compile(r"^\s*#(\d+)\s+0x[0-9a-fA-F]+\s+(?:in\s+)?(.*)$")
# A trailing source location (`path:line[:col]`) or module offset
# (`(module+0x1f)`) after the function name.
FRAME_LOCATION_RE = re.compile(r"\s+(\S+:\d+(?::\d+)?|\(\S+\+0x[0-9a-fA-F]+\))$")
# Trailing `(BuildId: ...)` annotation on frames in shared objects.
BUILD_ID_RE = re.compile(r"\s+\(BuildId: [0-9a-fA-F]+\)$")
# Rust symbol hash suffix (`::h0123456789abcdef`).
RUST_HASH_RE = re.compile(r"::h[0-9a-f]{16}$")
# Rust panic header (1.73+): `panicked at <path>:<line>:<col>:`.
PANIC_RE = re.compile(r"panicked at (\S+?):(\d+):(\d+)")
# Sanitizer summary line: tool and error name. The SUMMARY line names the
# error class (`allocation-size-too-big`); the header above the stack is
# prose for some classes (`requested allocation size ...`). A leak summary
# starts with a byte count, so the name must start with a letter.
SANITIZER_SUMMARY_RE = re.compile(
    r"^SUMMARY: (AddressSanitizer|UndefinedBehaviorSanitizer|MemorySanitizer): ([A-Za-z][\w-]*)",
    re.MULTILINE,
)
# Sanitizer error header, for reports cut off before their SUMMARY line.
SANITIZER_RE = re.compile(r"ERROR: (AddressSanitizer|UndefinedBehaviorSanitizer|MemorySanitizer): ([\w-]+)")

# Crash kinds that are memory-safety errors: high by default, and the
# advisory suggests fixing them in a temporary private fork.
MEMORY_KINDS_PREFIX = ("asan:", "msan:", "ubsan:")
# Crash kinds that deny service (the process aborts or stalls).
DOS_KINDS = frozenset({"panic", "stack-overflow", "oom", "timeout", "asan:allocation-size-too-big"})

# CWE for each crash kind (or `asan:` error name); the advisory carries it.
CWE_BY_KIND = {
    "asan:heap-buffer-overflow": "CWE-122",
    "asan:stack-buffer-overflow": "CWE-121",
    "asan:global-buffer-overflow": "CWE-787",
    "asan:heap-use-after-free": "CWE-416",
    "asan:stack-use-after-return": "CWE-562",
    "asan:stack-use-after-scope": "CWE-416",
    "asan:double-free": "CWE-415",
    "asan:SEGV": "CWE-119",
    "asan:allocation-size-too-big": "CWE-789",
    "stack-overflow": "CWE-674",
    "oom": "CWE-789",
    "timeout": "CWE-400",
    "panic": "CWE-617",
    "leak": "CWE-401",
    "signal": "CWE-119",
}
# CWE for memory-safety kinds without a specific entry above.
DEFAULT_MEMORY_CWE = "CWE-119"

# Repository directories whose assertions are test oracles, not product code.
ORACLE_DIRS = ("fuzz/fuzz_targets/", "fuzz/oracles/")

# GitHub advisory ecosystem for packages that are not on a public registry.
OTHER_ECOSYSTEM = "other"


# ---------------------------------------------------------------------------
# Stack parsing and classification (pure, unit-tested)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Frame:
    """One symbolized stack frame from a sanitizer report.

    Attributes:
        function: Demangled function name with the Rust hash suffix removed.
        location: Repository-relative ``path:line[:col]`` (or the raw module
            offset when the frame has no debug info); empty when absent.
    """

    function: str
    location: str


@dataclass
class CrashReport:
    """What one reproduction run says about a crash.

    Attributes:
        kind: Crash class: ``asan:<error>``, ``ubsan:<check>``, ``panic``,
            ``stack-overflow``, ``oom``, ``timeout``, ``leak``, ``signal`` or
            ``unknown``.
        frames: Stack frames of the crashing thread, outermost last.
        panic_location: Repository-relative ``path:line:col`` of a Rust
            panic, or empty.
        summary: One line describing the crash (the sanitizer summary or the
            panic message), without the input.
    """

    kind: str
    frames: list[Frame]
    panic_location: str
    summary: str


def normalize_path(path: str) -> str:
    """Returns a build path relative to the repository or the crate registry.

    Builds run from ``/src/bookclerk``; registry crates live under
    ``.../registry/src/<index>/<crate-version>/``. Both prefixes vary between
    builds, so signatures and records use the stable remainder.

    Args:
        path: Path as printed by the sanitizer or the panic hook.

    Returns:
        The path from the repository root (``crates/...``, ``fuzz/...``), or
        from the crate directory for registry crates, or ``path`` unchanged.
    """
    marker = "/bookclerk/"
    if marker in path:
        return path.split(marker, 1)[1]
    registry = re.search(r"/registry/src/[^/]+/(.+)$", path)
    if registry:
        return registry.group(1)
    return path


def parse_frames(output: str) -> list[Frame]:
    """Parses the first symbolized stack trace in a sanitizer report.

    Args:
        output: Combined stdout and stderr of the crashing run.

    Returns:
        The frames of the first trace (``#0`` onwards), in report order;
        empty when the report has no trace.
    """
    frames: list[Frame] = []
    for line in output.splitlines():
        match = FRAME_RE.match(line)
        if not match:
            if frames:
                break
            continue
        if int(match.group(1)) == 0 and frames:
            break
        rest = BUILD_ID_RE.sub("", match.group(2).strip())
        location = ""
        loc_match = FRAME_LOCATION_RE.search(rest)
        if loc_match:
            location = loc_match.group(1)
            rest = rest[: loc_match.start()].strip()
            if not location.startswith("("):
                location = normalize_path(location)
        frames.append(Frame(function=RUST_HASH_RE.sub("", rest), location=location))
    return frames


def is_ignored_frame(frame: Frame) -> bool:
    """Says whether a frame is plumbing that must not shape a signature.

    Args:
        frame: A parsed frame.

    Returns:
        True for sanitizer, fuzzer, libc and standard-library frames, by
        function name or by source location (trait impls such as
        ``<usize as SliceIndex>::index`` are only recognizable by the
        latter).
    """
    if frame.function in IGNORED_FRAME_NAMES or frame.function.startswith(IGNORED_FRAME_PREFIXES):
        return True
    # An unsymbolized frame (`(module+0x1f)`) has a build-specific offset in
    # place of a name, so it would make the signature change every build.
    if frame.function.startswith("(") or not frame.function:
        return True
    return any(part in frame.location for part in IGNORED_LOCATION_PARTS)


def code_frames(frames: list[Frame]) -> list[Frame]:
    """Returns the frames of the code under test, top first.

    Args:
        frames: Parsed frames.

    Returns:
        ``frames`` without the plumbing ``is_ignored_frame`` recognizes.
    """
    return [f for f in frames if not is_ignored_frame(f)]


def classify(output: str) -> CrashReport:
    """Classifies the output of a crashing run.

    Args:
        output: Combined stdout and stderr of the crashing run.

    Returns:
        The crash report. Sanitizer errors win over a panic header (a stack
        overflow inside a panic is still a stack overflow), except the abort
        signal every panic ends in.
    """
    frames = parse_frames(output)
    panic = PANIC_RE.search(output)
    panic_location = ""
    if panic:
        panic_location = f"{normalize_path(panic.group(1))}:{panic.group(2)}:{panic.group(3)}"
    sanitizer = SANITIZER_SUMMARY_RE.search(output) or SANITIZER_RE.search(output)
    # Rust's allocation-failure handler aborts without a panic header (the
    # image runs with allocator_may_return_null=1).
    alloc_failure = "memory allocation of " in output and " bytes failed" in output
    summary = ""
    for line in output.splitlines():
        if line.startswith("SUMMARY: "):
            summary = line[len("SUMMARY: ") :].strip()
            break

    # A Rust panic or allocation failure aborts the process, which
    # AddressSanitizer reports as ABRT.
    if sanitizer and not ((panic or alloc_failure) and sanitizer.group(2) == "ABRT"):
        tool, error = sanitizer.group(1), sanitizer.group(2)
        if error == "stack-overflow":
            kind = "stack-overflow"
        elif error in ("out-of-memory", "rss-limit-exceeded", "malloc-limit-exceeded"):
            kind = "oom"
        elif error == "ABRT":
            kind = "signal"
        elif tool == "AddressSanitizer":
            kind = f"asan:{error}"
        elif tool == "MemorySanitizer":
            kind = f"msan:{error}"
        else:
            kind = f"ubsan:{error}"
    elif "ERROR: LeakSanitizer" in output:
        kind = "leak"
    elif panic:
        kind = "panic"
        message = output[panic.end() :].lstrip(":").strip().splitlines()
        summary = f"panic at {panic_location}: {message[0] if message else ''}".strip()
    elif alloc_failure or "ERROR: libFuzzer: out-of-memory" in output or "malloc limit" in output:
        kind = "oom"
    elif "ERROR: libFuzzer: timeout" in output:
        kind = "timeout"
    elif "ERROR: libFuzzer: deadly signal" in output:
        kind = "signal"
    else:
        kind = "unknown"
    return CrashReport(kind=kind, frames=frames, panic_location=panic_location, summary=summary[:300])


def signature(target: str, report: CrashReport, data: bytes) -> str:
    """Computes the deduplication key of a crash.

    The key covers the target, the crash kind and the top
    ``SIGNATURE_FRAMES`` frames of the code under test (function names only,
    so a moved line does not make a new crash). A report without usable
    frames falls back to the panic file, and one without that either (a
    wall-clock timeout, an unsymbolized signal) to a digest of the crashing
    input: such crashes would otherwise all share one key per target and
    kind, and a recorded key suppresses every later crash that has it.

    Args:
        target: Fuzz target name.
        report: The classified crash.
        data: The input the crash was first seen with (not a minimized
            copy, so a minimized input keeps the key it is compared to).

    Returns:
        A hex SHA-256 digest.
    """
    picked = [f.function for f in code_frames(report.frames)][:SIGNATURE_FRAMES]
    if not picked and report.panic_location:
        picked = [report.panic_location.split(":", 1)[0]]
    if not picked:
        picked = ["input:" + hashlib.sha256(data).hexdigest()]
    material = "\n".join([target, report.kind, *picked])
    return hashlib.sha256(material.encode("utf-8")).hexdigest()


def is_oracle_failure(report: CrashReport) -> bool:
    """Says whether a crash is a failed test oracle rather than a product bug.

    Targets check invariants with assertions in the target or in
    ``fuzz/oracles/``; a panic raised from code there is such a check
    failing (``fuzz/target/`` holds build output such as generated codecs,
    which is product code). It is still a bug (in the code or the oracle) but not by
    itself a vulnerability, and ``fuzz/`` is never shipped, so it is filed
    as an issue.

    Args:
        report: The classified crash.

    Returns:
        True when the crash is a panic whose innermost frame of code under
        test lies in ``ORACLE_DIRS``.
    """
    if report.kind != "panic":
        return False
    top = code_frames(report.frames)
    if top:
        return top[0].location.startswith(ORACLE_DIRS)
    return report.panic_location.startswith((*ORACLE_DIRS, "fuzz_targets/", "oracles/"))


# ---------------------------------------------------------------------------
# Boundaries, severity and routing (pure, unit-tested)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Artifact:
    """A shipped Bookclerk artifact a crash can affect.

    Attributes:
        key: Name used in ``fuzz/boundaries.toml``.
        ecosystem: GitHub advisory ecosystem.
        package: Package name in the advisory.
    """

    key: str
    ecosystem: str
    package: str


@dataclass(frozen=True)
class Boundary:
    """The untrusted-input boundary one fuzz target exercises.

    Attributes:
        target: Fuzz target name.
        crate: Crate that owns the boundary code.
        description: What crosses the boundary, for humans.
        input_from: Who controls the bytes (``guest``, ``store``, ``plugin
            package``, ``peer node``, ...).
        side: ``host`` when the code runs in a host process or confined host
            helper, ``guest`` when only inside a plugin guest.
        artifacts: Shipped artifacts that contain the crate.
    """

    target: str
    crate: str
    description: str
    input_from: str
    side: str
    artifacts: tuple[Artifact, ...]


def load_boundaries(path: Path) -> dict[str, Boundary]:
    """Reads the fuzz target to boundary map.

    Args:
        path: ``fuzz/boundaries.toml``.

    Returns:
        Boundaries keyed by target name.

    Raises:
        ValueError: An entry is incomplete or names an unknown artifact or
            side.
    """
    import tomllib

    doc = tomllib.loads(path.read_text(encoding="utf-8"))
    artifacts: dict[str, Artifact] = {}
    for key, spec in doc.get("artifacts", {}).items():
        artifacts[key] = Artifact(key=key, ecosystem=spec["ecosystem"], package=spec["package"])
    out: dict[str, Boundary] = {}
    for target, spec in doc.get("targets", {}).items():
        side = spec["side"]
        if side not in ("host", "guest"):
            raise ValueError(f"{target}: side must be host or guest, not {side!r}")
        names = spec["artifacts"]
        if not names:
            raise ValueError(f"{target}: lists no artifacts")
        missing = [n for n in names if n not in artifacts]
        if missing:
            raise ValueError(f"{target}: unknown artifacts {missing}")
        out[target] = Boundary(
            target=target,
            crate=spec["crate"],
            description=spec["boundary"],
            input_from=spec["input_from"],
            side=side,
            artifacts=tuple(artifacts[n] for n in names),
        )
    return out


def default_severity(kind: str, boundary: Boundary | None) -> str:
    """Picks the default advisory severity of a crash.

    A memory-safety error is high wherever it is. A crash that only denies
    service is medium in host code (a guest, store or peer can stop a host
    process or helper) and low inside a guest. A leak is low. Anything
    unrecognized, including a target missing from the boundary map, is high
    so that doubt errs towards attention.

    Args:
        kind: Crash kind from ``classify``.
        boundary: The target's boundary, or None when the map lacks it.

    Returns:
        ``low``, ``medium`` or ``high``.
    """
    if boundary is None:
        return "high"
    if kind.startswith(MEMORY_KINDS_PREFIX) and kind not in DOS_KINDS:
        return "high"
    if kind in DOS_KINDS:
        return "medium" if boundary.side == "host" else "low"
    if kind == "leak":
        return "low"
    return "high"


def cwe_for(kind: str) -> str:
    """Returns the CWE id for a crash kind.

    Args:
        kind: Crash kind from ``classify``.

    Returns:
        A ``CWE-<n>`` id.
    """
    if kind in CWE_BY_KIND:
        return CWE_BY_KIND[kind]
    return DEFAULT_MEMORY_CWE


def auto_section(lines: list[str]) -> list[str]:
    """Wraps generated description lines in the automation's bounds.

    Args:
        lines: The generated part of a description.

    Returns:
        The lines between ``AUTO_BEGIN`` and ``AUTO_END``.
    """
    return [AUTO_BEGIN, *lines, AUTO_END]


def _stable(section: str) -> str:
    """Drops the volatile lines of an automated section for comparison."""
    return "\n".join(line for line in section.splitlines() if not line.startswith(VOLATILE_PREFIX)).strip()


def splice_auto(existing: str, generated: str, markers: Iterable[str] = ()) -> str | None:
    """Replaces the automated part of a draft's description.

    Args:
        existing: The draft's current description.
        generated: A freshly rendered description holding one automated part.
        markers: Marker lines the description must carry (new upstream
            aliases); missing ones are added at the top.

    Returns:
        The new description, or None when nothing but volatile lines would
        change, or the existing description has no automated part (written
        by hand or by an older version of the automation: left alone).
    """
    start, end = existing.find(AUTO_BEGIN), existing.find(AUTO_END)
    new_start, new_end = generated.find(AUTO_BEGIN), generated.find(AUTO_END)
    if start < 0 or end < start or new_start < 0 or new_end < new_start:
        return None
    old = existing[start : end + len(AUTO_END)]
    new = generated[new_start : new_end + len(AUTO_END)]
    missing = [m for m in markers if m not in existing]
    if _stable(old) == _stable(new) and not missing:
        return None
    head = "".join(f"{m}\n" for m in missing)
    return head + existing[:start] + new + existing[end + len(AUTO_END) :]


def draft_changes(draft: dict[str, Any], body: dict[str, Any], markers: Iterable[str] = ()) -> dict[str, Any]:
    """Computes the update that brings an open draft up to date.

    Only the automation's own part of the description is rewritten
    (``splice_auto``), plus any affected range that is still unset when the
    fresh body proposes one (a release has been published since). Severity,
    summary, CWE ids, a range a maintainer set and text outside the
    automated part stay as they are.

    Args:
        draft: The open draft, as listed by the API.
        body: A freshly rendered advisory request body.
        markers: Marker lines the description must carry.

    Returns:
        The fields to PATCH; empty when the draft is current.
    """
    changes: dict[str, Any] = {}
    description = splice_auto(str(draft.get("description") or ""), body["description"], markers)
    if description is not None:
        changes["description"] = description
    proposed = {
        (v["package"]["ecosystem"], v["package"]["name"]): v["vulnerable_version_range"]
        for v in body.get("vulnerabilities") or []
        if v.get("vulnerable_version_range")
    }
    filled = False
    vulnerabilities = []
    for vuln in draft.get("vulnerabilities") or []:
        entry = {
            k: vuln.get(k) for k in ("package", "vulnerable_version_range", "patched_versions", "vulnerable_functions")
        }
        package = vuln.get("package") or {}
        new_range = proposed.get((package.get("ecosystem"), package.get("name")))
        if new_range and not vuln.get("vulnerable_version_range"):
            entry["vulnerable_version_range"] = new_range
            filled = True
        vulnerabilities.append(entry)
    if filled:
        changes["vulnerabilities"] = vulnerabilities
    return changes


def marker(sig: str) -> str:
    """Returns the body marker for a crash signature.

    Args:
        sig: Signature from ``signature``.

    Returns:
        An HTML comment that never renders.
    """
    return f"{MARKER_PREFIX}{sig}{MARKER_SUFFIX}"


@dataclass
class Finding:
    """One unique, reproducible crash, as ``reproduce`` records it.

    Attributes:
        target: Fuzz target name.
        sanitizer: Sanitizer the fuzzer was built with.
        kind: Crash kind.
        signature: Deduplication key.
        oracle: True when a test oracle failed (see ``is_oracle_failure``).
        summary: One-line description, without the input.
        panic_location: Location of a Rust panic, or empty.
        frames: ``function @ location`` lines of the stack, top first.
        reproducer: File name of the (minimized) input in the output
            directory's ``inputs/``.
        reproducer_size: Size of that input in bytes.
        minimized: True when minimization shrank the input and kept the
            signature.
        top_function: Innermost function of the code under test, or empty.
        duplicates: Other crash files of this run with the same signature.
    """

    target: str
    sanitizer: str
    kind: str
    signature: str
    oracle: bool
    summary: str
    panic_location: str
    frames: list[str]
    reproducer: str
    reproducer_size: int
    minimized: bool
    top_function: str = ""
    duplicates: int = 0


def route(finding: Finding) -> str:
    """Decides how a finding is filed.

    Args:
        finding: The crash.

    Returns:
        ``advisory`` for a crash in shipped code, released or not (a draft
        is private, and filing before the first release proves the path);
        ``issue`` for a failed test oracle, which is not a vulnerability.
    """
    return "issue" if finding.oracle else "advisory"


# ---------------------------------------------------------------------------
# Rendering (pure, unit-tested)
# ---------------------------------------------------------------------------


def kind_title(kind: str) -> str:
    """Returns a short human name for a crash kind.

    Args:
        kind: Crash kind.

    Returns:
        A phrase such as ``panic`` or ``memory-safety error (heap-buffer-overflow)``.
    """
    if kind.startswith(MEMORY_KINDS_PREFIX) and kind not in DOS_KINDS:
        return f"memory-safety error ({kind.split(':', 1)[1]})"
    names = {
        "panic": "panic",
        "stack-overflow": "stack overflow",
        "oom": "out-of-memory",
        "timeout": "timeout",
        "leak": "memory leak",
        "signal": "fatal signal",
        "asan:allocation-size-too-big": "oversized allocation",
    }
    return names.get(kind, "crash")


def render_issue(finding: Finding, boundary: Boundary | None, run_url: str, bundle: str) -> tuple[str, str]:
    """Renders the public issue for a failed test oracle.

    A failed oracle is not a vulnerability (every other crash goes to a
    private draft advisory), so its issue names the target and its boundary.
    It never carries the stack, the panic message or the input; those are in
    the sealed bundle.

    Args:
        finding: The crash (a failed oracle).
        boundary: Its boundary, or None when the map lacks the target.
        run_url: URL of the workflow run.
        bundle: Name of the sealed triage artifact of that run.

    Returns:
        ``(title, body)``.
    """
    short = finding.signature[:16]
    where_details = [
        "The stack, the minimized reproducer and the logs are in the sealed",
        f"`{bundle}` artifact of {run_url} (open it with",
        "`BUNDLE_KEY=<FUZZ_CRASH_BUNDLE_KEY> scripts/osv-remediation-bundle.sh open sealed.bin <dir>`).",
    ]
    where = boundary.description if boundary else "not in fuzz/boundaries.toml"
    crate = boundary.crate if boundary else "unknown"
    title = f"Fuzz oracle failure in `{finding.target}`"
    lines = [
        marker(finding.signature),
        "",
        f"A test oracle of the `{finding.target}` fuzz target failed in the scheduled",
        "fuzzing run: a bug in the code under test or in the oracle.",
        "",
        f"- Boundary: {where} (`{crate}`)",
        f"- Signature: `{short}`",
        "",
        *where_details,
        "",
        "Fix it, then land the minimized input as",
        f"`fuzz/corpus/{finding.target}/regression_<name>` in the fixing pull request",
        "(docs/ci.md#fuzzing).",
    ]
    return title, "\n".join(lines) + "\n"


def render_advisory(
    finding: Finding,
    boundary: Boundary | None,
    release_tag: str | None,
    commit: str,
    run_url: str,
    bundle: str,
    reproducer: bytes,
) -> dict[str, Any]:
    """Renders the draft advisory request body for a finding.

    The draft is private to repository maintainers, so it carries the stack
    and, when small, the reproducer.

    Args:
        finding: The crash.
        boundary: Its boundary, or None when the map lacks the target (then
            the host binaries are named as affected).
        release_tag: Newest published release tag (the top of the proposed
            affected range), or None before the first release (no range is
            proposed).
        commit: Commit the crash reproduced on.
        run_url: URL of the workflow run.
        bundle: Name of the sealed triage artifact of that run.
        reproducer: The (minimized) crashing input.

    Returns:
        A JSON-ready body for ``POST /repos/{repo}/security-advisories``.
    """
    severity = default_severity(finding.kind, boundary)
    artifacts = boundary.artifacts if boundary else (Artifact("bookclerk", OTHER_ECOSYSTEM, "bookclerk"),)
    where = boundary.description if boundary else f"`{finding.target}` (not in fuzz/boundaries.toml)"
    crate = boundary.crate if boundary else "unknown"
    input_from = boundary.input_from if boundary else "unknown"
    if release_tag:
        scope = [
            "Filed automatically by the scheduled fuzzing run as a draft. Check the",
            "severity and the affected range before publishing: the crash reproduced",
            f"on `main`, and the range proposes every release up to `{release_tag}`",
            "without having reproduced it there. Narrow it (or close this draft) if",
            "the bug arrived after that release.",
        ]
    else:
        scope = [
            "Filed automatically by the scheduled fuzzing run as a draft. The crash",
            "reproduced on `main`. Bookclerk has no published release yet, so no",
            "affected range is proposed: fix it before the first release, or set the",
            "range to the releases that ship the code before publishing.",
        ]
    lines = [
        *scope,
        "",
        "## Details",
        "",
        f"- Fuzz target: `{finding.target}` ({finding.sanitizer} build)",
        f"- Boundary: {where} (`{crate}`)",
        f"- Input controlled by: {input_from}",
        f"- Crash: {kind_title(finding.kind)}",
        f"- Default severity: {severity}",
    ]
    if finding.summary:
        lines.append(f"- Summary: {finding.summary}")
    lines += ["", "## Stack", "", "```", *finding.frames, "```", "", "## Reproducer", ""]
    if len(reproducer) <= INLINE_REPRODUCER_MAX:
        encoded = base64.b64encode(reproducer).decode("ascii")
        lines += [
            f"{len(reproducer)} bytes{' (minimized)' if finding.minimized else ''}, base64:",
            "",
            "```",
            *[encoded[i : i + 76] for i in range(0, len(encoded), 76)],
            "```",
            "",
            "```sh",
            "base64 -d > crash.bin  # paste the block above",
            f"cd fuzz && cargo +nightly fuzz run {finding.target} ../crash.bin",
            "```",
        ]
    else:
        lines.append(f"{len(reproducer)} bytes: too large to inline; see the sealed bundle below.")
    lines += [
        "",
        "Land the minimized input as",
        f"`fuzz/corpus/{finding.target}/regression_<name>` with the fix.",
    ]
    if severity == "high":
        # The filing token cannot start the fork itself: that also needs the
        # Administration permission, which the automation App does not get.
        lines += [
            "",
            "Prepare the fix in a temporary private fork (Start a temporary",
            "private fork on this advisory), not in a public pull request.",
        ]
    lines += [
        "",
        f"{VOLATILE_PREFIX}`{commit}`; sealed bundle (stack, logs, reproducer): `{bundle}` artifact of {run_url}.",
    ]
    vulnerable_functions = [finding.top_function] if finding.top_function else []
    description = [marker(finding.signature), "", *auto_section(lines), ""]
    return {
        "summary": f"{kind_title(finding.kind).capitalize()} in {crate} ({finding.target})"[:1024],
        "description": "\n".join(description) + "\n",
        "severity": severity,
        "cwe_ids": [cwe_for(finding.kind)],
        "vulnerabilities": [
            {
                "package": {"ecosystem": a.ecosystem, "name": a.package},
                "vulnerable_version_range": f"<= {release_tag}" if release_tag else None,
                "patched_versions": None,
                "vulnerable_functions": vulnerable_functions,
            }
            for a in artifacts
        ],
    }


# ---------------------------------------------------------------------------
# GitHub API
# ---------------------------------------------------------------------------


class GitHub:
    """Minimal GitHub REST client for the filing step.

    Attributes:
        repo: ``owner/name``.
    """

    def __init__(self, repo: str, token: str, api: str = API) -> None:
        """Creates a client.

        Args:
            repo: ``owner/name``.
            token: Installation token of the automation App.
            api: REST base URL.
        """
        self.repo = repo
        self._token = token
        self._api = api
        self._advisories: list[dict[str, Any]] | None = None

    def request(self, method: str, path: str, body: Any = None) -> tuple[int, Any, str]:
        """Sends one request.

        Args:
            method: HTTP method.
            path: Path under the API root starting with ``/``, or an absolute
                URL under it (a ``Link`` header's next page).
            body: JSON body, or None.

        Returns:
            ``(status, parsed JSON or None, Link header)``. A 404 is
            returned, not raised.

        Raises:
            urllib.error.HTTPError: Any other error status.
            ValueError: An absolute URL outside the API root.
        """
        if path.startswith("/"):
            url = f"{self._api}{path}"
        elif path.startswith(f"{self._api}/"):
            url = path
        else:
            raise ValueError(f"refusing to follow {path!r} outside {self._api}")
        data = None if body is None else json.dumps(body).encode("utf-8")
        request = urllib.request.Request(
            url,
            data=data,
            method=method,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self._token}",
                "X-GitHub-Api-Version": "2022-11-28",
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT_S) as response:
                raw = response.read()
                link = response.headers.get("Link", "")
                return response.status, json.loads(raw) if raw else None, link
        except urllib.error.HTTPError as err:
            if err.code == 404:
                return 404, None, ""
            raise

    def paginate(self, path: str) -> Iterable[dict[str, Any]]:
        """Yields every item of a list endpoint, up to ``MAX_PAGES`` pages.

        Follows the ``Link`` header, which works for both page-numbered
        listings and cursor-paged ones such as repository advisories.

        Args:
            path: List path; ``per_page=100`` is appended.

        Yields:
            Items in API order.
        """
        sep = "&" if "?" in path else "?"
        url: str | None = f"{path}{sep}per_page=100"
        for _ in range(MAX_PAGES):
            if url is None:
                return
            _, items, link = self.request("GET", url)
            yield from items or []
            url = next_link(link)

    def latest_release_tag(self) -> str | None:
        """Returns the tag of the newest published release.

        Prereleases count (their binaries are downloadable); drafts do not.
        Newest means the highest version (``newest_tag``), not the release
        created last, so a backport cut after a later release does not
        shrink the proposed affected range.

        Returns:
            The tag, or None when the repository has no published release.
        """
        return newest_tag(
            [str(r["tag_name"]) for r in self.paginate(f"/repos/{self.repo}/releases") if not r.get("draft")]
        )

    def known_signatures(self) -> tuple[set[str], set[str]]:
        """Collects the signatures already on record.

        Returns:
            ``(signatures in fuzz-crash issues that still stand, signatures
            in any repository advisory)``. An issue stands while open or when
            closed as not planned or duplicate; one closed as completed (the
            fix landed) does not, so a regression is filed again. Advisories
            of every state count, so a draft closed as not a vulnerability is
            not filed again.
        """
        issues = set()
        for issue in self.paginate(f"/repos/{self.repo}/issues?state=all&labels={ISSUE_LABEL}"):
            if "pull_request" in issue:
                continue
            if issue.get("state") == "closed" and issue.get("state_reason") not in ("not_planned", "duplicate"):
                continue
            issues.update(extract_signatures(issue.get("body") or ""))
        advisories = set()
        for advisory in self.advisories():
            advisories.update(extract_signatures(advisory.get("description") or ""))
        return issues, advisories

    def advisories(self) -> list[dict[str, Any]]:
        """Returns every repository advisory, listed once per client.

        Returns:
            Advisory objects of every state, as the API lists them.
        """
        if self._advisories is None:
            self._advisories = [
                a for a in self.paginate(f"/repos/{self.repo}/security-advisories") if isinstance(a, dict)
            ]
        return self._advisories

    def drafts(self) -> list[dict[str, Any]]:
        """Returns the repository advisories still in the draft state.

        Returns:
            Advisory objects as the API lists them (``ghsa_id``,
            ``description``, ``vulnerabilities``, ...). Closed and published
            advisories are never rewritten, so they are left out.
        """
        return [a for a in self.advisories() if a.get("state") == "draft"]

    def ensure_label(self) -> None:
        """Creates ``ISSUE_LABEL`` unless it exists.

        Deduplication lists issues by this label, so an issue must never be
        filed without it.

        Raises:
            urllib.error.HTTPError: The label could not be read or created.
        """
        status, _, _ = self.request("GET", f"/repos/{self.repo}/labels/{ISSUE_LABEL}")
        if status == 404:
            self.request(
                "POST",
                f"/repos/{self.repo}/labels",
                {"name": ISSUE_LABEL, "color": "b60205", "description": "Crash found by scheduled fuzzing"},
            )

    def open_issue(self, title: str, body: str) -> int:
        """Opens an issue labeled ``ISSUE_LABEL``.

        Args:
            title: Issue title.
            body: Issue body.

        Returns:
            The issue number.
        """
        _, created, _ = self.request(
            "POST", f"/repos/{self.repo}/issues", {"title": title, "body": body, "labels": [ISSUE_LABEL]}
        )
        return int(created["number"])

    def create_advisory(self, body: dict[str, Any]) -> str:
        """Creates a draft repository security advisory.

        Args:
            body: Request body from ``render_advisory``.

        Returns:
            The advisory's GHSA id.
        """
        _, created, _ = self.request("POST", f"/repos/{self.repo}/security-advisories", body)
        return str(created["ghsa_id"])

    def update_advisory(self, ghsa_id: str, body: dict[str, Any]) -> None:
        """Updates fields of a draft repository security advisory.

        Args:
            ghsa_id: The draft's GHSA id.
            body: The fields to change (``description``, ``vulnerabilities``).
        """
        self.request("PATCH", f"/repos/{self.repo}/security-advisories/{urllib.parse.quote(ghsa_id)}", body)


_TAG_VERSION = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$")


def newest_tag(tags: list[str]) -> str | None:
    """Picks the highest semver release tag.

    Args:
        tags: Published release tags in API order (newest created first).

    Returns:
        The tag with the highest version in SemVer precedence (a prerelease
        sorts below its release; pre-release identifiers compare
        numerically when numeric, as text otherwise, numeric below text, and
        a shorter list below a longer one with the same prefix); the first
        tag when none is semver-shaped; None for no tags.
    """
    best: tuple[tuple[Any, ...], str] | None = None
    for tag in tags:
        match = _TAG_VERSION.match(tag)
        if match is None:
            continue
        major, minor, patch, pre = match.groups()
        identifiers = tuple((0, int(i), "") if i.isdigit() else (1, 0, i) for i in pre.split(".")) if pre else ()
        key = (int(major), int(minor), int(patch), 0 if pre else 1, identifiers)
        if best is None or key > best[0]:
            best = (key, tag)
    if best is not None:
        return best[1]
    return tags[0] if tags else None


def next_link(link_header: str) -> str | None:
    """Returns the ``rel="next"`` URL of a GitHub ``Link`` header.

    Args:
        link_header: The header value, possibly empty.

    Returns:
        The next page's URL, or None on the last page.
    """
    for part in link_header.split(","):
        section = part.split(";")
        if len(section) >= 2 and section[1].strip() == 'rel="next"':
            return section[0].strip().strip("<>")
    return None


def extract_signatures(text: str) -> set[str]:
    """Finds every signature marker in a body.

    Args:
        text: Issue or advisory body.

    Returns:
        The signatures it carries.
    """
    pattern = re.escape(MARKER_PREFIX) + r"([0-9a-f]{64})" + re.escape(MARKER_SUFFIX)
    return set(re.findall(pattern, text))


# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------


def _make_readable(root: Path) -> None:
    """Makes files the container wrote as root readable by the runner user.

    Args:
        root: Directory to walk.
    """
    if not root.exists():
        return
    for dirpath, _, files in os.walk(root):
        os.chmod(dirpath, 0o755)
        for name in files:
            os.chmod(os.path.join(dirpath, name), 0o644)


def crash_files(artifacts: Path) -> list[Path]:
    """Lists the crash inputs ClusterFuzzLite left in an artifacts directory.

    Layout: ``<artifacts>/<target>/<sanitizer>/<input>`` plus an
    ``<input>.summary`` stack file and, when minimized, ``<input>-minimized``.

    Args:
        artifacts: The ``out/artifacts`` directory.

    Returns:
        Crash inputs, sorted.
    """
    if not artifacts.is_dir():
        return []
    return sorted(
        p
        for p in artifacts.glob("*/*/*")
        if p.is_file() and not p.name.endswith(".summary") and not p.name.endswith("-minimized")
    )


def cmd_run(args: argparse.Namespace) -> int:
    """Runs ClusterFuzzLite (mode from ``MODE``) with private output.

    Must run inside the ClusterFuzzLite run-fuzzers image (as a container
    action, so the image can still read and write corpus artifacts).

    Args:
        args: Parsed arguments (unused).

    Returns:
        0 when fuzzing ran (crashes or not), else the image's error code.
    """
    del args
    workspace = Path(os.environ["GITHUB_WORKSPACE"])
    private = workspace / PRIVATE_DIR
    private.mkdir(parents=True, exist_ok=True)
    sys.path.insert(0, CIFUZZ_DIR)

    public_stdout = os.dup(1)
    public_stderr = os.dup(2)
    log_fd = os.open(private / "batch.log", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    sys.stdout.flush()
    sys.stderr.flush()
    os.dup2(log_fd, 1)
    os.dup2(log_fd, 2)
    try:
        import clusterfuzz_deployment  # type: ignore[import-not-found]
        import run_fuzzers_entrypoint  # type: ignore[import-not-found]

        deployment = getattr(clusterfuzz_deployment, "ClusterFuzzLite", None)
        if deployment is None or not callable(getattr(deployment, "upload_crashes", None)):
            # Fail closed: without the hook the image would publish crashes.
            raise RuntimeError("ClusterFuzzLite.upload_crashes not found; refusing to fuzz")

        def keep_crashes_private(self: Any) -> None:
            """Leaves crashes in the workspace for the sealed bundle."""
            del self

        deployment.upload_crashes = keep_crashes_private
        code = run_fuzzers_entrypoint.main()
    except Exception as err:  # noqa: BLE001 - reported below without details
        print(f"private run wrapper error: {err!r}", flush=True)
        code = 1
    finally:
        sys.stdout.flush()
        sys.stderr.flush()
        os.dup2(public_stdout, 1)
        os.dup2(public_stderr, 2)
        os.close(log_fd)

    artifacts = workspace / "out" / "artifacts"
    count = len(crash_files(artifacts))
    _make_readable(private)
    _make_readable(artifacts)
    print(f"fuzzing finished with code {code}; {count} crash file(s) kept private")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"crashes={count}\n")
    # An error after a crash was kept still fails the step; the workflow
    # seals and triages whatever crashes exist regardless.
    if code in (CIFUZZ_OK, CIFUZZ_BUG_FOUND):
        return 0
    return 1


def _run(cmd: list[str], timeout: int, log: Callable[[str], None]) -> tuple[int, str]:
    """Runs a fuzzer command and captures its output.

    Runs in a scratch directory, because libFuzzer writes a ``crash-*`` copy
    of a crashing input into its working directory.

    Args:
        cmd: Command line.
        timeout: Wall-clock limit in seconds.
        log: Sink for the private log.

    Returns:
        ``(exit code, combined output)``; a wall-clock timeout counts as a
        crash with code -1.
    """
    log(f"$ {' '.join(cmd)}")
    try:
        with tempfile.TemporaryDirectory() as scratch:
            proc = subprocess.run(
                cmd,
                cwd=scratch,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=timeout,
                check=False,
            )
        out = proc.stdout.decode("utf-8", "replace")
        log(out)
        return proc.returncode, out
    except subprocess.TimeoutExpired as err:
        out = (err.stdout or b"").decode("utf-8", "replace") + "\nERROR: libFuzzer: timeout (wall clock)\n"
        log(out)
        return -1, out


def reproduce_one(binary: Path, data: Path, log: Callable[[str], None]) -> CrashReport | None:
    """Re-runs one input until it crashes or the attempts run out.

    Args:
        binary: Fuzz target binary.
        data: Input file.
        log: Sink for the private log.

    Returns:
        The crash report, or None when the input did not crash (or only
        exited non-zero without a crash report).
    """
    for _ in range(REPRODUCE_ATTEMPTS):
        code, out = _run([str(binary), *LIBFUZZER_LIMITS, str(data)], REPRODUCE_TIMEOUT_S, log)
        if code == 0:
            continue
        report = classify(out)
        # A signal death can leave no marker; a plain positive exit without
        # one is not a crash the target reported.
        if code < 0 or report.kind != "unknown":
            return report
        log(f"{data}: exit {code} without a recognized crash report")
    return None


def cmd_reproduce(args: argparse.Namespace) -> int:
    """Reproduces, minimizes, classifies and deduplicates crashes.

    Must run where the fuzz target binaries can execute (the ClusterFuzzLite
    run-fuzzers image, which has the sanitizer runtime and symbolizer).
    Writes ``findings.json`` and ``inputs/<signature>.bin`` to ``--out`` and
    the full log to ``--out/reproduce.log``; prints counts only.

    Args:
        args: ``--build-out``, ``--artifacts`` and ``--out``.

    Returns:
        0 on completion.
    """
    build_out = Path(args.build_out)
    out = Path(args.out)
    inputs = out / "inputs"
    inputs.mkdir(parents=True, exist_ok=True)
    symbolizer = build_out / "llvm-symbolizer"
    if symbolizer.is_file():
        os.environ.setdefault("ASAN_SYMBOLIZER_PATH", str(symbolizer))
    os.environ.setdefault("RUST_BACKTRACE", "1")

    with open(out / "reproduce.log", "w", encoding="utf-8") as log_handle:

        def log(text: str) -> None:
            log_handle.write(text.rstrip("\n") + "\n")

        found: dict[str, Finding] = {}
        dropped = missing = 0
        files = crash_files(Path(args.artifacts))
        for path in files:
            target, sanitizer = path.parent.parent.name, path.parent.name
            binary = build_out / target
            if not binary.is_file():
                log(f"{path}: no binary for target {target}")
                missing += 1
                continue
            report = reproduce_one(binary, path, log)
            if report is None:
                log(f"{path}: did not reproduce")
                dropped += 1
                continue
            original = path.read_bytes()
            sig = signature(target, report, original)
            if sig in found:
                found[sig].duplicates += 1
                continue
            data = original
            minimized = False
            if report.kind not in ("timeout", "oom", "leak"):
                with tempfile.TemporaryDirectory() as tmp:
                    candidate = Path(tmp) / "minimized"
                    _run(
                        [
                            str(binary),
                            "-minimize_crash=1",
                            f"-max_total_time={MINIMIZE_SECONDS}",
                            f"-exact_artifact_path={candidate}",
                            *LIBFUZZER_LIMITS,
                            str(path),
                        ],
                        MINIMIZE_TIMEOUT_S,
                        log,
                    )
                    if candidate.is_file() and candidate.stat().st_size < len(data):
                        again = reproduce_one(binary, candidate, log)
                        if again is not None and signature(target, again, original) == sig:
                            data = candidate.read_bytes()
                            report = again
                            minimized = True
            name = f"{sig}.bin"
            (inputs / name).write_bytes(data)
            frames = [
                f"{f.function} @ {f.location}" if f.location else f.function for f in report.frames[:STACK_FRAMES_KEPT]
            ]
            found[sig] = Finding(
                target=target,
                sanitizer=sanitizer,
                kind=report.kind,
                signature=sig,
                oracle=is_oracle_failure(report),
                summary=report.summary,
                panic_location=report.panic_location,
                frames=frames,
                reproducer=name,
                reproducer_size=len(data),
                minimized=minimized,
                top_function=next((f.function for f in code_frames(report.frames)), ""),
            )
    (out / "findings.json").write_text(
        json.dumps([asdict(f) for f in found.values()], indent=2) + "\n", encoding="utf-8"
    )
    _make_readable(out)
    print(f"{len(files)} crash file(s): {len(found)} unique, {dropped} did not reproduce, {missing} without a binary")
    return 0


def file_findings(
    gh: GitHub,
    findings: list[Finding],
    boundaries: dict[str, Boundary],
    inputs: Path,
    commit: str,
    run_url: str,
    bundle: str,
) -> dict[str, int]:
    """Files each finding that is not already on record.

    A crash whose draft advisory is still a draft gets that draft's automated
    part refreshed when it changed (a new reproducer, boundary or severity
    mapping); a closed or published advisory is left alone.

    Args:
        gh: API client with the automation App token.
        findings: Output of ``reproduce``.
        boundaries: Target map.
        inputs: Directory holding the reproducers named in the findings.
        commit: Commit the crashes reproduced on.
        run_url: URL of the workflow run.
        bundle: Name of the sealed triage artifact.

    Returns:
        Counts by outcome: ``advisory``, ``updated``, ``failed`` (a refresh
        GitHub refused), ``issue``, ``known``.
    """
    release_tag = gh.latest_release_tag()
    issue_sigs, advisory_sigs = gh.known_signatures()
    drafts = gh.drafts()
    counts = {"advisory": 0, "updated": 0, "failed": 0, "issue": 0, "known": 0}
    label_ready = False
    for finding in findings:
        boundary = boundaries.get(finding.target)
        how = route(finding)
        if how == "advisory":
            # An opaque issue filed by an older version of this workflow does
            # not stand in for the advisory.
            reproducer = (inputs / finding.reproducer).read_bytes()
            body = render_advisory(finding, boundary, release_tag, commit, run_url, bundle, reproducer)
            if finding.signature in advisory_sigs:
                draft = next(
                    (d for d in drafts if finding.signature in extract_signatures(d.get("description") or "")), None
                )
                changes = draft_changes(draft, body) if draft else {}
                if draft is None or not changes:
                    counts["known"] += 1
                    continue
                try:
                    gh.update_advisory(str(draft["ghsa_id"]), changes)
                except (OSError, ValueError):
                    # A refused refresh (the draft was just closed, say) must
                    # not hold back the crashes after it.
                    counts["failed"] += 1
                    continue
                draft.update(changes)
                counts["updated"] += 1
                continue
            gh.create_advisory(body)
            advisory_sigs.add(finding.signature)
        else:
            if finding.signature in issue_sigs or finding.signature in advisory_sigs:
                counts["known"] += 1
                continue
            title, body = render_issue(finding, boundary, run_url, bundle)
            if not label_ready:
                gh.ensure_label()
                label_ready = True
            gh.open_issue(title, body)
            issue_sigs.add(finding.signature)
        counts[how] += 1
    return counts


def cmd_file(args: argparse.Namespace) -> int:
    """Files the findings of ``reproduce`` with the automation App token.

    Reads the token from ``GH_TOKEN`` and the repository and run from the
    standard Actions variables. Prints counts only.

    Args:
        args: ``--triage`` (the ``reproduce`` output directory),
            ``--boundaries`` and ``--bundle``.

    Returns:
        0 on completion.
    """
    triage = Path(args.triage)
    raw = json.loads((triage / "findings.json").read_text(encoding="utf-8"))
    findings = [Finding(**item) for item in raw]
    boundaries = load_boundaries(Path(args.boundaries))
    repo = os.environ["GITHUB_REPOSITORY"]
    run_url = f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/{repo}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}"
    gh = GitHub(repo, os.environ["GH_TOKEN"], os.environ.get("GITHUB_API_URL", API))
    commit = os.environ.get("GITHUB_SHA", "unknown")
    counts = file_findings(gh, findings, boundaries, triage / "inputs", commit, run_url, args.bundle)
    unmapped = sum(1 for f in findings if f.target not in boundaries)
    print(
        f"{len(findings)} finding(s): {counts['advisory']} draft advisories, {counts['updated']} drafts refreshed ({counts['failed']} refused), {counts['issue']} issues, "
        f"{counts['known']} already on record, {unmapped} from targets missing in boundaries.toml"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    """Parses arguments and runs one subcommand.

    Args:
        argv: Arguments without the program name; defaults to ``sys.argv``.

    Returns:
        Process exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("run", help="run ClusterFuzzLite with private output (inside the image)")
    rep = sub.add_parser("reproduce", help="reproduce, minimize and dedupe crashes (inside the image)")
    rep.add_argument("--build-out", required=True, help="directory with the fuzz target binaries")
    rep.add_argument("--artifacts", required=True, help="crash directory (<target>/<sanitizer>/<input>)")
    rep.add_argument("--out", required=True, help="output directory")
    fil = sub.add_parser("file", help="file findings as draft advisories or issues")
    fil.add_argument("--triage", required=True, help="output directory of `reproduce`")
    fil.add_argument("--boundaries", default=str(ROOT / "fuzz" / "boundaries.toml"))
    fil.add_argument("--bundle", required=True, help="name of the sealed triage artifact")
    args = parser.parse_args(argv)
    handlers: dict[str, Callable[[argparse.Namespace], int]] = {
        "run": cmd_run,
        "reproduce": cmd_reproduce,
        "file": cmd_file,
    }
    return handlers[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
