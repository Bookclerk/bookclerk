#!/usr/bin/env python3
"""Fuzz-crash triage: stack parsing, classification, routing and filing (offline)."""

from __future__ import annotations

import http.server
import importlib.util
import json
import re
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from types import ModuleType
from typing import Any, ClassVar

import tomllib

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = Path(__file__).resolve().parent / "fuzz_crash_fixtures"


def _load() -> ModuleType:
    path = ROOT / "scripts" / "fuzz-crash-triage.py"
    spec = importlib.util.spec_from_file_location("fuzz_crash_triage", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["fuzz_crash_triage"] = module
    spec.loader.exec_module(module)
    return module


triage = _load()

# A real libFuzzer + AddressSanitizer report of a Rust panic raised from a
# fuzz target (an oracle failure), recorded from the run-fuzzers image.
ORACLE_PANIC = (FIXTURES / "oracle_panic.txt").read_text(encoding="utf-8")
# The same report with the panic moved into a workspace crate.
HOST_PANIC = ORACLE_PANIC.replace(
    "sample_target::parse /src/bookclerk/fuzz/fuzz_targets/sample_target.rs:7:17",
    "bookclerk_workerd::granted_http::parse_head::h0123456789abcdef "
    "/src/bookclerk/crates/bookclerk-workerd/src/granted_http.rs:120:9",
).replace(
    "panicked at fuzz_targets/sample_target.rs:7:17",
    "panicked at /src/bookclerk/crates/bookclerk-workerd/src/granted_http.rs:120:9",
)
# A real AddressSanitizer heap-buffer-overflow report.
HEAP_OVERFLOW = (FIXTURES / "heap_overflow.txt").read_text(encoding="utf-8")


def _boundaries() -> dict[str, Any]:
    return triage.load_boundaries(ROOT / "fuzz" / "boundaries.toml")


def _finding(**overrides: Any) -> Any:
    base = {
        "target": "granted_http",
        "sanitizer": "address",
        "kind": "panic",
        "signature": "a" * 64,
        "oracle": False,
        "summary": "panic at crates/x.rs:1:1: boom",
        "panic_location": "crates/x.rs:1:1",
        "frames": ["bookclerk_workerd::granted_http::parse_head @ crates/bookclerk-workerd/src/granted_http.rs:120:9"],
        "reproducer": "a.bin",
        "reproducer_size": 4,
        "minimized": True,
        "top_function": "bookclerk_workerd::granted_http::parse_head",
    }
    base.update(overrides)
    return triage.Finding(**base)


class ClassifyTest(unittest.TestCase):
    def test_panic_in_fuzz_target_is_an_oracle_failure(self) -> None:
        report = triage.classify(ORACLE_PANIC)
        self.assertEqual(report.kind, "panic")
        self.assertEqual(report.panic_location, "fuzz_targets/sample_target.rs:7:17")
        self.assertIn("index out of bounds", report.summary)
        self.assertTrue(triage.is_oracle_failure(report))
        top = triage.code_frames(report.frames)[0]
        self.assertEqual(top.function, "sample_target::parse")
        self.assertEqual(top.location, "fuzz/fuzz_targets/sample_target.rs:7:17")

    def test_panic_in_crate_is_a_product_bug(self) -> None:
        report = triage.classify(HOST_PANIC)
        self.assertEqual(report.kind, "panic")
        self.assertFalse(triage.is_oracle_failure(report))
        top = triage.code_frames(report.frames)[0]
        # Hash suffix stripped, path made repository-relative.
        self.assertEqual(top.function, "bookclerk_workerd::granted_http::parse_head")
        self.assertEqual(top.location, "crates/bookclerk-workerd/src/granted_http.rs:120:9")

    def test_heap_overflow(self) -> None:
        report = triage.classify(HEAP_OVERFLOW)
        self.assertEqual(report.kind, "asan:heap-buffer-overflow")
        self.assertFalse(triage.is_oracle_failure(report))
        self.assertEqual(triage.code_frames(report.frames)[0].function, "sample_target::read_past")
        self.assertTrue(report.summary.startswith("AddressSanitizer: heap-buffer-overflow"))

    def test_libfuzzer_verdicts(self) -> None:
        self.assertEqual(triage.classify("==1== ERROR: libFuzzer: out-of-memory (used: 3000Mb)").kind, "oom")
        self.assertEqual(triage.classify("==1== ERROR: libFuzzer: timeout after 25 seconds").kind, "timeout")
        self.assertEqual(triage.classify("==1== ERROR: libFuzzer: deadly signal").kind, "signal")
        self.assertEqual(triage.classify("==1==ERROR: LeakSanitizer: detected memory leaks").kind, "leak")
        self.assertEqual(
            triage.classify(
                "==1==ERROR: LeakSanitizer: detected memory leaks\n"
                "SUMMARY: AddressSanitizer: 4 byte(s) leaked in 1 allocation(s).\n"
            ).kind,
            "leak",
        )
        self.assertEqual(
            triage.classify("==1==ERROR: AddressSanitizer: stack-overflow on address 0x1").kind,
            "stack-overflow",
        )
        self.assertEqual(triage.classify("==1==ERROR: AddressSanitizer: ABRT on unknown address").kind, "signal")
        self.assertEqual(triage.classify("something else").kind, "unknown")

    def test_allocation_failures_are_denial_of_service(self) -> None:
        too_big = (
            "==1==ERROR: AddressSanitizer: requested allocation size 0xffffffff exceeds maximum\n"
            "SUMMARY: AddressSanitizer: allocation-size-too-big in malloc\n"
        )
        self.assertEqual(triage.classify(too_big).kind, "asan:allocation-size-too-big")
        oom = (
            "==1==ERROR: AddressSanitizer: out of memory: allocator is trying to allocate 0x1 bytes\n"
            "SUMMARY: AddressSanitizer: out-of-memory\n"
        )
        self.assertEqual(triage.classify(oom).kind, "oom")
        rust_alloc = (
            "memory allocation of 18446744073709551615 bytes failed\n"
            "==1==ERROR: AddressSanitizer: ABRT on unknown address 0x1\n"
            "SUMMARY: AddressSanitizer: ABRT (/lib/x86_64-linux-gnu/libc.so.6+0x4300b) in raise\n"
        )
        self.assertEqual(triage.classify(rust_alloc).kind, "oom")
        double_free = (
            "==1==ERROR: AddressSanitizer: attempting double-free on 0x1\n"
            "SUMMARY: AddressSanitizer: double-free in free\n"
        )
        self.assertEqual(triage.classify(double_free).kind, "asan:double-free")

    def test_generated_code_under_fuzz_target_dir_is_not_an_oracle(self) -> None:
        generated = HOST_PANIC.replace(
            "/src/bookclerk/crates/bookclerk-workerd/src/granted_http.rs:120:9",
            "/src/bookclerk/fuzz/target/x86_64-unknown-linux-gnu/release/build/"
            "bookclerk-plugin-abi-0123/out/plugin_capnp.rs:120:9",
        )
        self.assertFalse(triage.is_oracle_failure(triage.classify(generated)))

    def test_unsymbolized_frames_do_not_shape_signatures(self) -> None:
        a = "    #0 0x5 (/out/t+0x1234) (BuildId: abc)\n    #1 0x6 (/out/t+0x5678)\n"
        b = "    #0 0x9 (/out/t+0x4321) (BuildId: abd)\n    #1 0xa (/out/t+0x8765)\n"
        ra, rb = triage.classify("ERROR: libFuzzer: deadly signal\n" + a), triage.classify(
            "ERROR: libFuzzer: deadly signal\n" + b
        )
        self.assertEqual(triage.code_frames(ra.frames), [])
        self.assertEqual(triage.signature("t", ra), triage.signature("t", rb))


class SignatureTest(unittest.TestCase):
    def test_ignores_addresses_lines_and_plumbing(self) -> None:
        moved = HOST_PANIC.replace("granted_http.rs:120:9", "granted_http.rs:131:13").replace(
            "0x55eb81493150", "0x5600deadbeef"
        )
        a = triage.signature("granted_http", triage.classify(HOST_PANIC))
        b = triage.signature("granted_http", triage.classify(moved))
        self.assertEqual(a, b)
        self.assertRegex(a, r"^[0-9a-f]{64}$")

    def test_differs_by_target_kind_and_function(self) -> None:
        report = triage.classify(HOST_PANIC)
        base = triage.signature("granted_http", report)
        self.assertNotEqual(base, triage.signature("egress_policy", report))
        other_kind = triage.CrashReport("oom", report.frames, report.panic_location, "")
        self.assertNotEqual(base, triage.signature("granted_http", other_kind))
        renamed = triage.classify(HOST_PANIC.replace("parse_head", "parse_body"))
        self.assertNotEqual(base, triage.signature("granted_http", renamed))

    def test_falls_back_to_panic_file(self) -> None:
        report = triage.CrashReport("panic", [], "crates/x/src/a.rs:1:2", "")
        self.assertEqual(
            triage.signature("t", report),
            triage.signature("t", triage.CrashReport("panic", [], "crates/x/src/a.rs:9:9", "")),
        )


class SeverityTest(unittest.TestCase):
    def test_defaults(self) -> None:
        bounds = _boundaries()
        host = bounds["granted_http"]
        guest = triage.Boundary("t", "c", "d", "host", "guest", host.artifacts)
        self.assertEqual(triage.default_severity("asan:heap-buffer-overflow", host), "high")
        self.assertEqual(triage.default_severity("asan:heap-buffer-overflow", guest), "high")
        self.assertEqual(triage.default_severity("panic", host), "medium")
        self.assertEqual(triage.default_severity("panic", guest), "low")
        self.assertEqual(triage.default_severity("oom", host), "medium")
        self.assertEqual(triage.default_severity("asan:allocation-size-too-big", host), "medium")
        self.assertEqual(triage.default_severity("leak", host), "low")
        self.assertEqual(triage.default_severity("unknown", host), "high")
        self.assertEqual(triage.default_severity("signal", host), "high")
        # A target missing from the map errs towards attention.
        self.assertEqual(triage.default_severity("panic", None), "high")

    def test_cwe(self) -> None:
        self.assertEqual(triage.cwe_for("asan:heap-use-after-free"), "CWE-416")
        self.assertEqual(triage.cwe_for("ubsan:shift-exponent"), "CWE-119")


class BoundariesTest(unittest.TestCase):
    def test_every_fuzz_target_is_mapped(self) -> None:
        fuzz = tomllib.loads((ROOT / "fuzz" / "Cargo.toml").read_text(encoding="utf-8"))
        targets = {b["name"] for b in fuzz.get("bin", [])}
        missing = targets - set(_boundaries())
        self.assertFalse(missing, f"fuzz/boundaries.toml lacks {sorted(missing)}")

    def test_every_documented_target_is_mapped(self) -> None:
        docs = (ROOT / "docs" / "ci.md").read_text(encoding="utf-8")
        section = docs.split("### Fuzzing", 1)[1].split("Still to add", 1)[0]
        documented = set(re.findall(r"^\| `([a-z0-9_]+)` \|", section, re.MULTILINE))
        self.assertTrue(documented)
        missing = documented - set(_boundaries())
        self.assertFalse(missing, f"fuzz/boundaries.toml lacks {sorted(missing)}")

    def test_rejects_bad_entries(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "b.toml"
            path.write_text(
                '[artifacts.a]\necosystem = "other"\npackage = "a"\n'
                '[targets.t]\ncrate = "c"\nboundary = "b"\ninput_from = "guest"\n'
                'side = "host"\nartifacts = ["nope"]\n',
                encoding="utf-8",
            )
            with self.assertRaises(ValueError):
                triage.load_boundaries(path)
            path.write_text(path.read_text(encoding="utf-8").replace('"nope"', '"a"').replace('"host"', '"edge"'))
            with self.assertRaises(ValueError):
                triage.load_boundaries(path)


class WorkflowTest(unittest.TestCase):
    def test_one_runner_image(self) -> None:
        """Triage reproduces in the image the fuzzers ran in."""
        pattern = r"gcr\.io/oss-fuzz-base/clusterfuzzlite-run-fuzzers@sha256:[0-9a-f]{64}"
        action = (ROOT / ".github" / "actions" / "cflite-private-run" / "action.yml").read_text(encoding="utf-8")
        workflow = (ROOT / ".github" / "workflows" / "cflite-batch.yml").read_text(encoding="utf-8")
        images = set(re.findall(pattern, action)) | set(re.findall(pattern, workflow))
        self.assertEqual(len(images), 1, images)

    def test_scheduled_fuzzing_never_uses_the_public_runner(self) -> None:
        """The stock run_fuzzers action logs crashes publicly."""
        workflow = (ROOT / ".github" / "workflows" / "cflite-batch.yml").read_text(encoding="utf-8")
        self.assertNotIn("actions/run_fuzzers@", workflow)
        self.assertEqual(workflow.count("uses: ./.github/actions/cflite-private-run"), 3)

    def test_scheduled_builds_skip_the_public_build_check(self) -> None:
        """The build check prints each target's output to the job log."""
        workflow = (ROOT / ".github" / "workflows" / "cflite-batch.yml").read_text(encoding="utf-8")
        builds = workflow.count("actions/build_fuzzers@")
        self.assertGreater(builds, 0)
        self.assertEqual(workflow.count("bad-build-check: false"), builds)

    def test_timeout_inputs_are_kept(self) -> None:
        """ClusterFuzzLite drops timeout inputs unless told to keep them."""
        action = (ROOT / ".github" / "actions" / "cflite-private-run" / "action.yml").read_text(encoding="utf-8")
        self.assertIn('REPORT_TIMEOUTS: "True"', action)


class RouteAndRenderTest(unittest.TestCase):
    def test_route(self) -> None:
        self.assertEqual(triage.route(_finding(), None), "issue")
        self.assertEqual(triage.route(_finding(), "v1.0.0"), "advisory")
        self.assertEqual(triage.route(_finding(oracle=True), "v1.0.0"), "issue")

    def test_pre_release_issue_names_only_the_signature(self) -> None:
        finding = _finding(summary="panic at secret.rs: the len is 0 but the index is 4")
        title, body = triage.render_issue(finding, _boundaries()["granted_http"], "https://run", "fuzz-triage-address")
        self.assertIn(finding.signature[:16], title)
        self.assertIn(triage.marker(finding.signature), body)
        for secret in ("granted_http", "the len is 0", "parse_head", "crates/x.rs", "a.bin", "panic", "severity"):
            self.assertNotIn(secret, title + body)
        self.assertIn("fuzz-triage-address", body)
        self.assertIn("no published release", body)

    def test_oracle_issue_names_the_target(self) -> None:
        finding = _finding(oracle=True, summary="assertion failed: secret")
        title, body = triage.render_issue(finding, _boundaries()["granted_http"], "https://run", "b")
        self.assertIn("granted_http", title)
        self.assertIn("Granted reverse channel", body)
        self.assertNotIn("assertion failed", body)

    def test_advisory_body(self) -> None:
        bounds = _boundaries()
        finding = _finding(kind="asan:heap-buffer-overflow", target="abi_decode")
        body = triage.render_advisory(
            finding, bounds["abi_decode"], "v1.2.0", "abc123", "https://run", "b", b"\x00\x01"
        )
        self.assertEqual(body["severity"], "high")
        self.assertTrue(body["start_private_fork"])
        self.assertEqual(body["cwe_ids"], ["CWE-122"])
        packages = [v["package"]["name"] for v in body["vulnerabilities"]]
        self.assertEqual(packages, ["bookclerk", "bookclerk-plugin-sdk"])
        self.assertTrue(all(v["vulnerable_version_range"] == "<= v1.2.0" for v in body["vulnerabilities"]))
        self.assertEqual(body["vulnerabilities"][0]["vulnerable_functions"], [finding.top_function])
        self.assertIn("AAE=", body["description"])
        self.assertIn("`abc123`", body["description"])
        self.assertIn(triage.marker(finding.signature), body["description"])
        json.dumps(body)

    def test_large_reproducer_is_not_inlined(self) -> None:
        big = b"A" * (triage.INLINE_REPRODUCER_MAX + 1)
        body = triage.render_advisory(_finding(), _boundaries()["granted_http"], "v1", "c", "u", "b", big)
        self.assertNotIn("QUFB", body["description"])
        self.assertFalse(body["start_private_fork"])


class FakeGitHub:
    """Stands in for `GitHub` with canned listings and recorded writes."""

    def __init__(self, release: str | None, issues: list[str], advisories: list[str]) -> None:
        self.release = release
        self.issue_bodies = issues
        self.advisory_bodies = advisories
        self.opened: list[tuple[str, str]] = []
        self.created: list[dict[str, Any]] = []
        self.label_checks = 0

    def latest_release_tag(self) -> str | None:
        return self.release

    def known_signatures(self) -> tuple[set[str], set[str]]:
        issues: set[str] = set()
        for body in self.issue_bodies:
            issues |= triage.extract_signatures(body)
        advisories: set[str] = set()
        for body in self.advisory_bodies:
            advisories |= triage.extract_signatures(body)
        return issues, advisories

    def ensure_label(self) -> None:
        self.label_checks += 1

    def open_issue(self, title: str, body: str) -> int:
        self.opened.append((title, body))
        return len(self.opened)

    def create_advisory(self, body: dict[str, Any]) -> str:
        self.created.append(body)
        return "GHSA-xxxx"


class FileFindingsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.inputs = Path(self.tmp.name)
        (self.inputs / "a.bin").write_bytes(b"boom")

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _file(self, gh: FakeGitHub, findings: list[Any]) -> dict[str, int]:
        return triage.file_findings(gh, findings, _boundaries(), self.inputs, "abc", "https://run", "b")

    def test_before_a_release_everything_is_an_issue(self) -> None:
        gh = FakeGitHub(None, [], [])
        counts = self._file(gh, [_finding(), _finding(signature="b" * 64, kind="asan:SEGV")])
        self.assertEqual(counts, {"advisory": 0, "issue": 2, "known": 0})
        self.assertEqual(gh.created, [])
        self.assertEqual(gh.label_checks, 1)

    def test_after_a_release_product_crashes_get_draft_advisories(self) -> None:
        gh = FakeGitHub("v1.0.0", [], [])
        counts = self._file(gh, [_finding(), _finding(signature="b" * 64, oracle=True)])
        self.assertEqual(counts, {"advisory": 1, "issue": 1, "known": 0})
        self.assertEqual(gh.created[0]["vulnerabilities"][0]["vulnerable_version_range"], "<= v1.0.0")

    def test_known_signatures_are_skipped(self) -> None:
        sig = "c" * 64
        gh = FakeGitHub(None, [f"x {triage.marker(sig)} y"], [])
        self.assertEqual(self._file(gh, [_finding(signature=sig)])["known"], 1)
        # A closed or draft advisory also suppresses a new issue.
        gh = FakeGitHub(None, [], [triage.marker(sig)])
        self.assertEqual(self._file(gh, [_finding(signature=sig)])["known"], 1)
        self.assertEqual(gh.opened, [])

    def test_pre_release_issue_does_not_stand_in_for_the_advisory(self) -> None:
        sig = "d" * 64
        gh = FakeGitHub("v1.0.0", [triage.marker(sig)], [])
        self.assertEqual(self._file(gh, [_finding(signature=sig)])["advisory"], 1)

    def test_same_signature_twice_in_one_run_files_once(self) -> None:
        gh = FakeGitHub(None, [], [])
        counts = self._file(gh, [_finding(), _finding()])
        self.assertEqual(counts, {"advisory": 0, "issue": 1, "known": 1})


class _Api(http.server.BaseHTTPRequestHandler):
    """Canned REST API: drafts before a published release, issues in every state, and
    advisories split across two cursor pages linked by a ``Link`` header."""

    posts: ClassVar[list[tuple[str, dict[str, Any]]]] = []

    def log_message(self, format: str, *args: Any) -> None:
        del format, args

    def _send(self, status: int, body: Any, link: str = "") -> None:
        raw = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if link:
            self.send_header("Link", link)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:
        assert self.headers["Authorization"] == "Bearer tok"
        base = f"http://127.0.0.1:{self.server.server_port}"
        if self.path.startswith("/repos/o/r/releases?per_page=100&page=2"):
            self._send(200, [{"tag_name": "v0.8.0", "draft": False}])
        elif self.path.startswith("/repos/o/r/releases"):
            link = f'<{base}/repos/o/r/releases?per_page=100&page=2>; rel="next"'
            self._send(200, [{"tag_name": "v0.9.0", "draft": True}], link)
        elif self.path == "/repos/o/r/labels/fuzz-crash":
            self._send(404, {"message": "Not Found"})
        elif self.path.startswith("/repos/o/r/issues"):
            assert "state=all" in self.path and "labels=fuzz-crash" in self.path
            self._send(
                200,
                [
                    {"state": "open", "body": triage.marker("1" * 64)},
                    {"state": "closed", "state_reason": "not_planned", "body": triage.marker("3" * 64)},
                    {"state": "closed", "state_reason": "completed", "body": triage.marker("4" * 64)},
                    {"state": "open", "pull_request": {}, "body": triage.marker("5" * 64)},
                ],
            )
        elif self.path.startswith("/repos/o/r/security-advisories?per_page=100&after=c1"):
            self._send(200, [{"description": triage.marker("6" * 64)}])
        elif self.path.startswith("/repos/o/r/security-advisories"):
            link = f'<{base}/repos/o/r/security-advisories?per_page=100&after=c1>; rel="next"'
            self._send(200, [{"description": triage.marker("2" * 64)}, {"description": None}], link)
        else:
            self._send(500, {})

    def do_POST(self) -> None:
        length = int(self.headers["Content-Length"])
        body = json.loads(self.rfile.read(length))
        type(self).posts.append((self.path, body))
        if self.path == "/repos/o/r/issues":
            self._send(201, {"number": 7})
        elif self.path == "/repos/o/r/labels":
            self._send(201, {"name": body["name"]})
        else:
            self._send(201, {"ghsa_id": "GHSA-aaaa-bbbb-cccc"})


class GitHubClientTest(unittest.TestCase):
    def test_against_a_local_api(self) -> None:
        server = http.server.HTTPServer(("127.0.0.1", 0), _Api)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            gh = triage.GitHub("o/r", "tok", f"http://127.0.0.1:{server.server_port}")
            # The only published release sits behind a page of drafts.
            self.assertEqual(gh.latest_release_tag(), "v0.8.0")
            issues, advisories = gh.known_signatures()
            self.assertEqual(issues, {"1" * 64, "3" * 64})
            self.assertEqual(advisories, {"2" * 64, "6" * 64})
            gh.ensure_label()
            self.assertEqual(_Api.posts[-1][0], "/repos/o/r/labels")
            self.assertEqual(gh.open_issue("t", "b"), 7)
            self.assertEqual(gh.create_advisory({"summary": "s"}), "GHSA-aaaa-bbbb-cccc")
            self.assertEqual(_Api.posts[1], ("/repos/o/r/issues", {"title": "t", "body": "b", "labels": ["fuzz-crash"]}))
            with self.assertRaises(ValueError):
                gh.request("GET", "https://elsewhere.example/x")
        finally:
            server.shutdown()
            server.server_close()

    def test_next_link(self) -> None:
        header = '<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last"'
        self.assertEqual(triage.next_link(header), "https://api.github.com/x?page=2")
        self.assertIsNone(triage.next_link(""))


class CrashFilesTest(unittest.TestCase):
    def test_layout(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp) / "granted_http" / "address"
            base.mkdir(parents=True)
            for name in ("crash-1", "crash-1.summary", "crash-1-minimized", "oom-2"):
                (base / name).write_bytes(b"x")
            self.assertEqual([p.name for p in triage.crash_files(Path(tmp))], ["crash-1", "oom-2"])
            self.assertEqual(triage.crash_files(Path(tmp) / "missing"), [])


if __name__ == "__main__":
    unittest.main()
