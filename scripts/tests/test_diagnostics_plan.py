#!/usr/bin/env python3
"""Diagnostics triage plan validator: seed corpus replay and mutation fuzzing.

The validator decides what model output reaches public GitHub issues, so it
is an untrusted-input boundary. Every corpus input and every mutation of one
must either be refused with ``PlanError`` or yield issues that satisfy the
invariants checked by ``assert_publishable``, which re-derives them without
calling the validator's own helpers.
"""

from __future__ import annotations

import importlib.util
import json
import random
import re
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "diagnostics_plan_corpus"
KNOWN = {"11111111-1111-4111-8111-111111111111", "22222222-2222-4222-8222-222222222222"}
#: Collector origin the dashboard links in the Report IDs section point at.
DASHBOARD = "https://diagnostics.example.test"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


vp = _load("validate_plan", ROOT / "tools" / "diagnostics-collector" / "scripts" / "validate_plan.py")

#: Patterns that would link, mention or cross-reference on GitHub.
FORBIDDEN = {
    "url": re.compile(r"(?i)[a-z][a-z0-9+.-]*://|\bwww\."),
    "inline link or reference definition": re.compile(r"\][(:]"),
    "raw html": re.compile(r"<[A-Za-z/!?]"),
    "character reference": re.compile(r"&(?!amp;|lt;)(?:#|[A-Za-z][A-Za-z0-9]*;)"),
    "mention": re.compile(r"@[A-Za-z0-9]"),
    "issue reference": re.compile(r"#\d|(?i:\bGH-\d)"),
}
REPORT_IDS_HEADING = re.compile(r"(?im)^\s*#+\s*report ids\b")

#: Fragments mutations splice in: link, mention and reference syntax.
FRAGMENTS = [
    "@", "#1", "GH-2", "](", "]:", "[", "]", "(", ")", "<a href=x>", "<img src=y>",
    "&#64;", "&amp;", "https://", "HTTPS://GitHub.com/", "www.", "//", "\n### Report IDs\n",
    "\u200b", "\\", '"', "{", "}", ",", ":", "report_ids", "issues", next(iter(KNOWN)),
]


def assert_publishable(case: unittest.TestCase, issues: list, label: str, dashboard: str | None = None) -> None:
    """Checks every invariant a validated plan promises.

    Args:
        case: Test case to fail.
        issues: Validator output.
        label: Input name for failure messages.
        dashboard: Collector origin the validator was given, or None.
    """
    case.assertLessEqual(len(issues), 5, label)
    for issue in issues:
        case.assertTrue(issue.title.startswith("[diagnostics] "), label)
        case.assertNotIn("\n", issue.title, label)
        case.assertLessEqual(len(issue.title), 120, label)
        case.assertIn(issue.labels, {"diagnostics", "diagnostics,bug"}, label)
        marker, newline, rest = issue.body.partition("\n")
        case.assertTrue(re.fullmatch(r"<!-- bookclerk-diagnostics: [a-z -]+ -->", marker), label)
        head, sep, ids = rest.rpartition("\n### Report IDs\n")
        case.assertTrue(sep, label)
        case.assertIsNone(REPORT_IDS_HEADING.search(head), f"{label}: forged Report IDs section")
        listed = re.findall(r"^- `([^`\n]*)`", ids, re.M)
        case.assertTrue(listed, label)
        # Every KNOWN ID is a UUID, so each line links exactly when a
        # dashboard was given, and only to that report's page on it.
        suffix = (lambda i: f" \u00b7 [dashboard]({dashboard}/reports/{i})") if dashboard else (lambda i: "")
        case.assertEqual(ids, "".join(f"- `{i}`{suffix(i)}\n" for i in listed), label)
        case.assertTrue(set(listed) <= KNOWN, label)
        case.assertEqual(issue.report_ids, tuple(listed), label)
        case.assertLessEqual(len(head), 40_000, label)
        for field in (issue.title, head):
            for what, pattern in FORBIDDEN.items():
                case.assertIsNone(pattern.search(field), f"{label}: {what} in {field[:200]!r}")


def run(text: str, dashboard: str | None = None) -> list | None:
    """Validates one input.

    Args:
        text: Model reply.
        dashboard: Collector origin for report links, or None.

    Returns:
        The issues, or None when the validator refused the reply.
    """
    try:
        return vp.plan_issues(text, KNOWN, dashboard)
    except vp.PlanError:
        return None


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_is_publishable_or_refused(self) -> None:
        files = sorted(CORPUS.glob("*.txt"))
        self.assertTrue(files)
        for path in files:
            issues = run(path.read_text(encoding="utf-8"))
            if issues is not None:
                assert_publishable(self, issues, path.name)

    def test_expected_outcomes(self) -> None:
        def load(name: str) -> str:
            return (CORPUS / name).read_text(encoding="utf-8")

        self.assertEqual(run(load("empty_plan.txt")), [])
        for refused in ("no_plan.txt", "issues_not_list.txt", "unknown_ids_plan.txt", "deep_nesting.txt"):
            self.assertIsNone(run(load(refused)), refused)
        fenced = run(load("fenced_plan.txt"))
        self.assertEqual(len(fenced), 1)
        self.assertEqual(fenced[0].labels, "diagnostics,bug")
        self.assertEqual(fenced[0].body.lower().count("report ids"), 1)
        self.assertEqual(len(run(load("oversized_plan.txt"))), 5)
        expanding = json.dumps({"issues": [{"title": "<a" * 200, "body": "b", "report_ids": sorted(KNOWN)}]})
        self.assertEqual(len(run(expanding)[0].title), 120)
        injected = run(load("injection_plan.txt"))[0]
        self.assertIn("[link removed]", injected.body)


class MutationFuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 3000

    def mutate(self, rng: random.Random, text: str) -> str:
        for _ in range(rng.randint(1, 6)):
            pos = rng.randint(0, len(text))
            op = rng.random()
            if op < 0.5:
                text = text[:pos] + rng.choice(FRAGMENTS) + text[pos:]
            elif op < 0.75:
                text = text[:pos] + text[pos + rng.randint(1, 8):]
            else:
                text = text[:pos] + chr(rng.randint(0, 0x2FF)) + text[pos:]
        return text

    def test_mutations_are_publishable_or_refused(self) -> None:
        seeds = [p.read_text(encoding="utf-8") for p in sorted(CORPUS.glob("*.txt")) if p.stat().st_size < 4096]
        rng = random.Random(20261023)
        for n in range(self.ITERATIONS):
            text = self.mutate(rng, rng.choice(seeds))
            dashboard = DASHBOARD if n % 2 else None
            issues = run(text, dashboard)
            if issues is not None:
                assert_publishable(self, issues, f"iteration {n}: {text[:300]!r}", dashboard)

    def test_hostile_fields_are_publishable(self) -> None:
        rng = random.Random(7)
        for n in range(self.ITERATIONS):
            junk = lambda: "".join(rng.choice(FRAGMENTS + list("ab x\n")) for _ in range(rng.randint(0, 40)))
            plan = {"issues": [{"title": junk(), "body": junk(), "report_ids": sorted(KNOWN), "bug": rng.random() < 0.5}]}
            issues = run(json.dumps(plan))
            self.assertIsNotNone(issues)
            assert_publishable(self, issues, f"hostile {n}: {plan!r}"[:400])


class CoverageAndLinkTests(unittest.TestCase):
    """Reports a plan leaves out, and the dashboard origin links point at."""

    def test_partial_plan_leaves_the_rest_uncovered(self) -> None:
        first, second = sorted(KNOWN)
        plan = json.dumps({"issues": [{"title": "t", "body": "b", "report_ids": [first]}]})
        issues = run(plan)
        self.assertEqual(vp.uncovered_ids(issues, KNOWN), [second])
        self.assertEqual(vp.uncovered_ids(run(load_corpus("empty_plan.txt")), KNOWN), [])
        whole = json.dumps({"issues": [{"title": "t", "body": "b", "report_ids": sorted(KNOWN)}]})
        self.assertEqual(vp.uncovered_ids(run(whole), KNOWN), [])

    def test_dashboard_origin_is_a_bare_https_origin(self) -> None:
        self.assertEqual(vp.dashboard_origin("https://Diagnostics.Bookclerk.app/"), "https://diagnostics.bookclerk.app")
        self.assertEqual(vp.dashboard_origin("https://x.workers.dev:8443"), "https://x.workers.dev:8443")
        for bad in (None, "", "http://d.test", "https://d.test/path", "https://u@d.test", "https://d.test?q", "https://d.test)", "javascript:x"):
            self.assertIsNone(vp.dashboard_origin(bad), bad)

    def test_only_uuids_get_links(self) -> None:
        self.assertEqual(vp.id_line("not-a-uuid", DASHBOARD), "- `not-a-uuid`\n")
        uuid = next(iter(KNOWN))
        self.assertEqual(vp.id_line(uuid, None), f"- `{uuid}`\n")
        self.assertEqual(vp.id_line(uuid, DASHBOARD), f"- `{uuid}` \u00b7 [dashboard]({DASHBOARD}/reports/{uuid})\n")


def load_corpus(name: str) -> str:
    """Reads one seed input.

    Args:
        name: File name in the corpus.

    Returns:
        The file's text.
    """
    return (CORPUS / name).read_text(encoding="utf-8")


class CliTests(unittest.TestCase):
    """The command-line wrapper the triage script runs."""

    def test_writes_issue_files_and_fails_on_bad_plan(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            (t / "data.json").write_text(json.dumps([{"report_id": i} for i in KNOWN]))
            (t / "plan").write_text((CORPUS / "fenced_plan.txt").read_text())
            (t / "out").mkdir()
            self.assertEqual(vp.main([str(t / "plan"), str(t / "data.json"), str(t / "out")]), 0)
            self.assertEqual((t / "out" / "0" / "labels").read_text(), "diagnostics,bug")
            self.assertIsInstance(json.loads((t / "out" / "uncovered.json").read_text()), list)
            (t / "linked").mkdir()
            self.assertEqual(vp.main([str(t / "plan"), str(t / "data.json"), str(t / "linked"), DASHBOARD + "/"]), 0)
            self.assertIn(f"]({DASHBOARD}/reports/", (t / "linked" / "0" / "body").read_text())
            (t / "plan").write_text((CORPUS / "unknown_ids_plan.txt").read_text())
            (t / "out2").mkdir()
            self.assertEqual(vp.main([str(t / "plan"), str(t / "data.json"), str(t / "out2")]), 1)
            self.assertEqual(list((t / "out2").iterdir()), [])


if __name__ == "__main__":
    unittest.main()
