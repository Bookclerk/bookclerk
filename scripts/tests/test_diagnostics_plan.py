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
#: Issue numbers the collector offered the model for ``attach``.
OPEN_ISSUES = frozenset({412, 408})
#: Opening line of the hidden report block the dashboard reads.
BLOCK_OPEN = "<!-- bookclerk-diagnostics: report ids"


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
#: An ID the collector's attach route reads a UUID from: a UUID or an object name, any case.
ATTACHABLE = re.compile(
    r"(?i)(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
    r"|diagnostics/[a-z0-9._+-]{1,64}/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.json)"
)

#: Fragments mutations splice in: link, mention and reference syntax.
FRAGMENTS = [
    "@", "#1", "GH-2", "](", "]:", "[", "]", "(", ")", "<a href=x>", "<img src=y>",
    "&#64;", "&amp;", "https://", "HTTPS://GitHub.com/", "www.", "//", "\n### Report IDs\n",
    "\n" + BLOCK_OPEN + "\n", "-->", "\n-->\n", "<!--",
    "\u200b", "\\", '"', "{", "}", ",", ":", "report_ids", "issues", next(iter(KNOWN)),
    '"attach"', '"issue"', "412", "408", "999", "true", "[", "]",
]


def assert_publishable(case: unittest.TestCase, issues: list, label: str) -> None:
    """Checks every invariant a validated plan promises.

    Args:
        case: Test case to fail.
        issues: Validator output.
        label: Input name for failure messages.
    """
    case.assertLessEqual(len(issues), 5, label)
    for issue in issues:
        case.assertTrue(issue.title.startswith("[diagnostics] "), label)
        case.assertNotIn("\n", issue.title, label)
        case.assertLessEqual(len(issue.title), 120, label)
        case.assertIn(issue.labels, {"diagnostics", "diagnostics,bug"}, label)
        marker, newline, rest = issue.body.partition("\n")
        case.assertTrue(re.fullmatch(r"<!-- bookclerk-diagnostics: [a-z -]+ -->", marker), label)
        # The body ends in exactly one hidden block: the opening line, one
        # bare ID per line and the closing line, after a blank line.
        head, sep, ids = rest.rpartition("\n\n" + BLOCK_OPEN + "\n")
        case.assertTrue(sep, label)
        case.assertNotIn("<!--", head, f"{label}: forged comment or report block")
        case.assertIsNone(REPORT_IDS_HEADING.search(head), f"{label}: forged Report IDs section")
        case.assertTrue(ids.endswith("\n-->\n"), label)
        listed = ids[: -len("-->\n")].splitlines()
        case.assertTrue(listed, label)
        case.assertEqual(len(listed), len(set(listed)), label)
        case.assertTrue(set(listed) <= KNOWN, label)
        case.assertEqual(issue.report_ids, tuple(listed), label)
        case.assertLessEqual(len(head), 40_000, label)
        for field in (issue.title, head):
            for what, pattern in FORBIDDEN.items():
                case.assertIsNone(pattern.search(field), f"{label}: {what} in {field[:200]!r}")


def assert_attachable(case: unittest.TestCase, plan, label: str) -> None:
    """Checks every invariant a validated plan's attachments promise, and
    that no report is both attached and filed.

    Args:
        case: Test case to fail.
        plan: Validator output (``Plan``).
        label: Input name for failure messages.
    """
    case.assertLessEqual(len(plan.attachments), 20, label)
    case.assertLessEqual(sum(len(a.report_ids) for a in plan.attachments), 500, label)
    issues = [a.issue for a in plan.attachments]
    case.assertEqual(len(issues), len(set(issues)), f"{label}: an issue once")
    attached: list[str] = []
    for a in plan.attachments:
        case.assertIs(type(a.issue), int, label)
        case.assertIn(a.issue, OPEN_ISSUES, label)
        case.assertTrue(a.report_ids, label)
        for rid in a.report_ids:
            case.assertIn(rid, KNOWN, label)
            case.assertTrue(ATTACHABLE.fullmatch(rid), label)
        attached.extend(a.report_ids)
    case.assertEqual(len(attached), len(set(attached)), f"{label}: a report under one issue")
    filed = {rid for issue in plan.issues for rid in issue.report_ids}
    case.assertFalse(filed & set(attached), f"{label}: a report both filed and attached")


def run(text: str, known: set[str] = KNOWN, open_issues: frozenset[int] = OPEN_ISSUES) -> list | None:
    """Validates one input and checks its attachments.

    Args:
        text: Model reply.
        known: Report IDs in the batch.
        open_issues: Issue numbers offered for ``attach``.

    Returns:
        The issues, or None when the validator refused the reply.
    """
    try:
        plan = vp.validate_plan(text, known, open_issues)
    except vp.PlanError:
        return None
    assert_attachable(unittest.TestCase(), plan, text[:300])
    return list(plan.issues)


def run_plan(text: str, open_issues: frozenset[int] = OPEN_ISSUES) -> vp.Plan | None:
    """Validates one input into a whole ``Plan``, or None when refused.

    Args:
        text: Model reply.
        open_issues: Issue numbers offered for ``attach``.

    Returns:
        The plan, or None.
    """
    try:
        return vp.validate_plan(text, KNOWN, open_issues)
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
        self.assertNotIn("### Report IDs", fenced[0].body)
        self.assertEqual(len(run(load("oversized_plan.txt"))), 5)
        expanding = json.dumps({"issues": [{"title": "<a" * 200, "body": "b", "report_ids": sorted(KNOWN)}]})
        self.assertEqual(len(run(expanding)[0].title), 120)
        injected = run(load("injection_plan.txt"))[0]
        self.assertIn("[link removed]", injected.body)

    def test_attachments(self) -> None:
        one, two = sorted(KNOWN)
        plan = run_plan(load_corpus("attach_plan.txt"))
        self.assertEqual([i.report_ids for i in plan.issues], [(one,)])
        self.assertEqual(plan.attachments, (vp.Attachment(412, (two,)),))
        self.assertEqual(vp.uncovered_ids(plan.issues, KNOWN, plan.attachments), [])
        only = run_plan(load_corpus("attach_only.txt"))
        self.assertEqual(only.issues, ())
        self.assertEqual(only.attachments, (vp.Attachment(412, (one,)), vp.Attachment(408, (two,))), "a report stays under the first issue naming it")
        unknown = run_plan(load_corpus("attach_unknown_issue.txt"))
        self.assertEqual(unknown.attachments, (vp.Attachment(412, (two,)),), "only integer numbers of offered issues, only batch UUIDs")
        self.assertEqual(unknown.issues[0].report_ids, (one,))
        taken = run_plan(load_corpus("attach_takes_issue_report.txt"))
        self.assertEqual(taken.issues, (), "an issue whose only report was attached is not filed")
        self.assertEqual(taken.attachments, (vp.Attachment(412, (one,)),))
        self.assertEqual(vp.uncovered_ids(taken.issues, KNOWN, taken.attachments), [two])
        # With no offered issues (no dashboard), attach is dropped and the issue stays.
        none = run_plan(load_corpus("attach_takes_issue_report.txt"), frozenset())
        self.assertEqual(none.attachments, ())
        self.assertEqual(none.issues[0].report_ids, (one,))
        # Issues named and nothing valid at all is a bad reply; a valid attachment saves it.
        bad = json.dumps({"issues": [{"title": "t", "body": "b", "report_ids": ["nope"]}], "attach": [{"issue": 999, "report_ids": [one]}]})
        self.assertIsNone(run_plan(bad))
        saved = json.dumps({"issues": [{"title": "t", "body": "b", "report_ids": ["nope"]}], "attach": [{"issue": 408, "report_ids": [one]}]})
        self.assertEqual(run_plan(saved).attachments, (vp.Attachment(408, (one,)),))
        # The issue cap counts distinct issues; later ones are dropped, not merged.
        many = {"issues": [], "attach": [{"issue": n, "report_ids": [one]} for n in range(1, 30)]}
        wide = run_plan(json.dumps(many), frozenset(range(1, 30)))
        self.assertEqual([a.issue for a in wide.attachments], [1])
        spread = {"issues": [], "attach": [{"issue": n, "report_ids": [f"{n:08x}-0000-4000-8000-000000000000"]} for n in range(1, 30)]}
        known = {f"{n:08x}-0000-4000-8000-000000000000" for n in range(1, 30)}
        self.assertEqual(len(vp.validate_plan(json.dumps(spread), known, frozenset(range(1, 30))).attachments), 20)
        self.assertEqual(vp.open_issue_numbers([412, 0, -1, "408", True, 2.5, 408]), frozenset({412, 408}))
        flood = [f"{n:08x}-0000-4000-8000-000000000000" for n in range(600)]
        wide_plan = json.dumps({"issues": [], "attach": [{"issue": 412, "report_ids": flood}]})
        self.assertEqual(len(vp.validate_plan(wide_plan, set(flood), OPEN_ISSUES).attachments[0].report_ids), 500)
        # IDs the batch spells as collector object names, or uppercase, attach as spelled; others do not.
        name = "diagnostics/0.9.2/33333333-3333-4333-8333-333333333333.json"
        upper = "44444444-4444-4444-8444-444444444444".upper()
        spelled = json.dumps({"issues": [], "attach": [{"issue": 412, "report_ids": [name, upper, "diagnostics/x/notauuid.json", "55555555-5555-4555-8555-55555555555g"]}]})
        got = vp.validate_plan(spelled, KNOWN | {name, upper, "diagnostics/x/notauuid.json", "55555555-5555-4555-8555-55555555555g"}, OPEN_ISSUES)
        self.assertEqual(got.attachments, (vp.Attachment(412, (name, upper)),))
        self.assertEqual(vp.open_issue_numbers({"a": 1}), frozenset())


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
            issues = run(text)
            if issues is not None:
                assert_publishable(self, issues, f"iteration {n}: {text[:300]!r}")

    def test_hostile_fields_are_publishable(self) -> None:
        rng = random.Random(7)
        for n in range(self.ITERATIONS):
            junk = lambda: "".join(rng.choice(FRAGMENTS + list("ab x\n")) for _ in range(rng.randint(0, 40)))
            plan = {"issues": [{"title": junk(), "body": junk(), "report_ids": sorted(KNOWN), "bug": rng.random() < 0.5}]}
            if rng.random() < 0.5:
                plan["attach"] = [{"issue": rng.choice([412, 408, 999, "412", True, 1.5]), "report_ids": [rng.choice(sorted(KNOWN) + [junk()])]} for _ in range(rng.randint(0, 3))]
            issues = run(json.dumps(plan))
            if "attach" not in plan:
                self.assertIsNotNone(issues)
            if issues is not None:
                assert_publishable(self, issues, f"hostile {n}: {plan!r}"[:400])


class CoverageAndBlockTests(unittest.TestCase):
    """Reports a plan leaves out, the dashboard origin and the report block."""

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
        longest = "https://" + "a" * (vp.MAX_DASHBOARD_ORIGIN_CHARS - len("https://") - 6) + ":65535"
        self.assertEqual(vp.dashboard_origin(longest), longest)
        self.assertIsNone(vp.dashboard_origin("https://" + "a" * 250 + ".test"))

    def test_report_block_lists_bare_ids(self) -> None:
        uuid = sorted(KNOWN)[0]
        self.assertEqual(vp.report_block([uuid, "diagnostics/0.1.0/x.json"]), f"{BLOCK_OPEN}\n{uuid}\ndiagnostics/0.1.0/x.json\n-->\n")
        for bad in ("a-->b", "a b", "", "x" * 161, "a\nb", "`a`"):
            with self.assertRaises(ValueError, msg=bad):
                vp.report_block([bad])

    def test_ids_that_could_close_the_block_are_not_listed(self) -> None:
        hostile = "11111111-->[x](https://evil.test)"
        plan = json.dumps({"issues": [{"title": "t", "body": "b", "report_ids": [hostile, sorted(KNOWN)[0]]}]})
        issues = run(plan, KNOWN | {hostile})
        self.assertEqual(issues[0].report_ids, (sorted(KNOWN)[0],))
        self.assertNotIn("evil", issues[0].body)
        only = json.dumps({"issues": [{"title": "t", "body": "b", "report_ids": [hostile]}]})
        self.assertIsNone(run(only, KNOWN | {hostile}), "an issue needs a listable ID")


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
            self.assertIn(f"\n{BLOCK_OPEN}\n", (t / "out" / "0" / "body").read_text())
            self.assertEqual(json.loads((t / "out" / "attach.json").read_text()), {"links": []})
            self.assertEqual(vp.main([str(t / "plan"), str(t / "data.json"), str(t / "out"), str(t / "open.json"), "x"]), 2)
            (t / "plan").write_text((CORPUS / "unknown_ids_plan.txt").read_text())
            (t / "out2").mkdir()
            self.assertEqual(vp.main([str(t / "plan"), str(t / "data.json"), str(t / "out2")]), 1)
            self.assertEqual(list((t / "out2").iterdir()), [])
            # With the collector's open issues, attachments become the collector's attach body.
            (t / "plan").write_text((CORPUS / "attach_only.txt").read_text())
            (t / "open.json").write_text(json.dumps([412, 408]))
            (t / "out3").mkdir()
            self.assertEqual(vp.main([str(t / "plan"), str(t / "data.json"), str(t / "out3"), str(t / "open.json")]), 0)
            one, two = sorted(KNOWN)
            self.assertEqual(json.loads((t / "out3" / "attach.json").read_text()), {"links": [{"issue": 412, "report_ids": [one]}, {"issue": 408, "report_ids": [two]}]})
            self.assertEqual(json.loads((t / "out3" / "uncovered.json").read_text()), [])
            self.assertEqual(sorted(p.name for p in (t / "out3").iterdir()), ["attach.json", "uncovered.json"])


if __name__ == "__main__":
    unittest.main()
