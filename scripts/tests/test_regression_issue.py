#!/usr/bin/env python3
"""Regression issue planner: seed corpus replay and mutation fuzzing.

``regression_issue.py`` turns the ``regression_of`` entries of an ingest
pull into public GitHub issues. The pull is the collector's answer, but the
versions and platforms in it come from reports anyone can submit and the
closed issue's title from an issue anyone with triage rights can edit, so
the planner is an untrusted-input boundary. Every corpus input and every
mutation of one must yield issues that satisfy the invariants
``assert_planned`` checks, which re-derives the expected groups from the
input with its own reading of the pull rather than the module's helpers.
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
CORPUS = Path(__file__).resolve().parent / "regression_issue_corpus"
#: Opening line of the hidden report block the dashboard reads.
BLOCK_OPEN = "<!-- bookclerk-diagnostics: report ids"
UUID_A = "11111111-1111-4111-8111-111111111111"
UUID_B = "22222222-2222-4222-8222-222222222222"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


ri = _load("regression_issue", ROOT / "tools" / "diagnostics-collector" / "scripts" / "regression_issue.py")

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
#: The marker the dashboard parser (dashboard/report-ids.ts) reads.
MARKER = re.compile(r"^<!-- bookclerk-diagnostics: regression of #([1-9]\d{0,8}) -->$")
#: What the dashboard's parser reads as an ID line inside the block.
ORACLE_LINE = re.compile(r"[A-Za-z0-9._:/+-]{1,160}")
ORACLE_ID = re.compile(r"[A-Za-z0-9._:-]{1,128}")
ORACLE_VERSION = re.compile(r"v?(\d{1,6})\.(\d{1,6})(?:\.\d{1,6})?(?:[-+._~][0-9A-Za-z.+_~-]{1,40})?")


def oracle_number(value: object) -> int | None:
    """A GitHub issue or pull request number: a plain positive int."""
    if isinstance(value, bool) or not isinstance(value, int) or not 1 <= value <= 999_999_999:
        return None
    return value


def oracle_line(value: object) -> tuple[int, int] | None:
    """The ``major.minor`` line of a version string, or None."""
    if not isinstance(value, str):
        return None
    m = ORACLE_VERSION.fullmatch(value.strip())
    return (int(m.group(1)), int(m.group(2))) if m else None


def oracle_groups(pull: object) -> dict[tuple[int, int, int], dict]:
    """Re-derives the issues a pull should yield, keyed by (issue, major, minor).

    Args:
        pull: Decoded pull (object with ``reports`` or the list itself).

    Returns:
        For each group, the distinct report IDs in input order (capped at
        500) and the pull request number of the group's first report.
    """
    reports = pull.get("reports") if isinstance(pull, dict) else pull
    groups: dict[tuple[int, int, int], dict] = {}
    if not isinstance(reports, list):
        return groups
    for entry in reports:
        if not isinstance(entry, dict) or not isinstance(entry.get("regression_of"), dict):
            continue
        of = entry["regression_of"]
        issue = oracle_number(of.get("issue"))
        report = entry.get("report")
        source = report.get("payload") if isinstance(report, dict) and isinstance(report.get("payload"), dict) else report
        line = oracle_line(source.get("version")) if isinstance(source, dict) else None
        rid = entry.get("report_id")
        if issue is None or line is None or oracle_line(of.get("fixed_in")) is None:
            continue
        if not isinstance(rid, str) or not ORACLE_ID.fullmatch(rid):
            continue
        group = groups.setdefault((issue, *line), {"ids": [], "pr": oracle_number(of.get("pr_number"))})
        if rid not in group["ids"] and len(group["ids"]) < 500:
            group["ids"].append(rid)
    return groups


def assert_planned(case: unittest.TestCase, pull: object, issues: list, label: str) -> None:
    """Checks every invariant a planned set of issues promises.

    Args:
        case: Test case to fail.
        pull: The decoded input.
        issues: Planner output.
        label: Input name for failure messages.
    """
    expected = oracle_groups(pull)
    keys = sorted(expected)[:10]
    case.assertEqual(len(issues), len(keys), label)
    overflow = [i for k in sorted(expected)[10:] for i in expected[k]["ids"]]
    case.assertEqual(ri.overflow_ids(pull), overflow, label)
    case.assertFalse(set(overflow) & {i for issue in issues for i in issue.report_ids}, label)
    for key, issue in zip(keys, issues):
        number, major, minor = key
        group = expected[key]
        case.assertEqual(issue.regression_of, number, label)
        case.assertEqual(issue.version_line, f"{major}.{minor}", label)
        case.assertTrue(issue.title.startswith(f"[diagnostics] Regression of #{number} in {major}.{minor}"), label)
        case.assertNotIn("\n", issue.title, label)
        case.assertLessEqual(len(issue.title), 120, label)
        case.assertEqual(issue.labels, "diagnostics,bug,regression", label)
        marker, _, rest = issue.body.partition("\n")
        case.assertEqual(MARKER.match(marker).group(1), str(number), label)
        head, sep, ids = rest.rpartition("\n\n" + BLOCK_OPEN + "\n")
        case.assertTrue(sep, label)
        case.assertNotIn("<!--", head, f"{label}: forged comment or report block")
        case.assertIsNone(REPORT_IDS_HEADING.search(head), f"{label}: forged Report IDs section")
        case.assertTrue(head.startswith(f"## Regression of #{number}\n"), label)
        case.assertTrue(ids.endswith("\n-->\n"), label)
        listed = ids[: -len("-->\n")].splitlines()
        for line in listed:
            case.assertIsNotNone(ORACLE_LINE.fullmatch(line), f"{label}: {line!r}")
        case.assertEqual(listed, group["ids"], label)
        case.assertEqual(issue.report_ids, tuple(listed), label)
        # The deliberate references are the closed issue and its fix pull
        # request, both validated integers; nothing else may reference.
        allowed = [f"#{number}"] + ([f"#{group['pr']}"] if group["pr"] is not None else [])
        for field in (issue.title, head):
            text = field
            for ref in allowed:
                text = re.sub(re.escape(ref) + r"(?!\d)", "", text)
            for what, pattern in FORBIDDEN.items():
                case.assertIsNone(pattern.search(text), f"{label}: {what} in {text[:300]!r}")


def load(name: str) -> object:
    """Decodes one corpus input the way ``main`` does (None when it cannot)."""
    try:
        return json.loads((CORPUS / name).read_text(encoding="utf-8"))
    except (json.JSONDecodeError, RecursionError):
        return None


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_is_planned(self) -> None:
        for path in sorted(CORPUS.iterdir()):
            pull = load(path.name)
            assert_planned(self, pull, ri.plan_regressions(pull), path.name)

    def test_expected_outcomes(self) -> None:
        issues = ri.plan_regressions(load("normal.json"))
        self.assertEqual(len(issues), 1)
        issue = issues[0]
        self.assertEqual(issue.title, "[diagnostics] Regression of #412 in 1.2: Scan fails on empty library")
        self.assertIn("2 reports from versions 1.2.1, 1.2.3 on linux, windows share the error signature of #412, which was fixed in v1.2.0 by #415.", issue.body)
        self.assertIn("The fix may not cover the 1.2 line", issue.body)
        self.assertEqual(issue.report_ids, (UUID_A, UUID_B))
        two = ri.plan_regressions(load("two_lines.json"))
        self.assertEqual([(i.regression_of, i.version_line) for i in two], [(7, "1.2"), (412, "1.2"), (412, "1.3")])
        self.assertIn("1 report from version v1.2.2 on linux shares the error signature of #7, which was fixed in 1.0.0 by a fix recorded by hand.", two[0].body)
        self.assertEqual(two[0].title, "[diagnostics] Regression of #7 in 1.2")
        self.assertEqual(len(ri.plan_regressions(load("many_groups.json"))), 10)
        self.assertEqual(ri.overflow_ids(load("many_groups.json")), [f"{i:08x}-0000-4000-8000-000000000000" for i in (10, 11, 12, 13)])
        self.assertEqual(ri.plan_regressions({"reports": [{"report_id": UUID_A, "report": {"version": "1.2.5_1"}, "regression_of": {"issue": 3, "fixed_in": "1.2.5"}}]})[0].version_line, "1.2")
        for name in ("invalid.json", "no_regressions.json", "not_a_pull.json", "empty.json", "deep_nesting.json", "not_json.json"):
            self.assertEqual(ri.plan_regressions(load(name)), [], name)
        dup = ri.plan_regressions(load("duplicates.json"))
        self.assertEqual(dup[0].report_ids, (UUID_A,))
        self.assertIn("1 report from version 1.2.1 on linux shares", dup[0].body)
        self.assertEqual(ri.plan_regressions(load("flat_report.json"))[0].version_line, "1.2")

    def test_hostile_fields_are_rewritten(self) -> None:
        issues = ri.plan_regressions(load("hostile.json"))
        self.assertEqual(len(issues), 1)
        body = issues[0].body
        self.assertIn("[link removed]", issues[0].title)
        self.assertIn("1.2.1-[link removed]", body)
        self.assertIn(" on linux, unknown share", body)
        long_title = ri.plan_regressions({"reports": [{"report_id": UUID_A, "report": {"version": "1.2.1"}, "regression_of": {"issue": 412, "fixed_in": "1.2.0", "title": "x" * 500}}]})
        self.assertTrue(long_title[0].title.endswith("…"))
        self.assertLessEqual(len(long_title[0].title), 120)
        # The pull request number came from the first report; out of range
        # means no fix reference at all.
        self.assertIn("by #415", body)
        self.assertEqual(body.count(BLOCK_OPEN), 1)
        self.assertNotIn("[dashboard](", body)
        # An object name is not a report ID (the collector sends the UUID).
        self.assertEqual(issues[0].report_ids, (UUID_A, UUID_B))

    def test_id_cap_counts_distinct_ids(self) -> None:
        reports = [{"report_id": f"{i:08x}-0000-4000-8000-000000000000", "report": {"payload": {"version": "1.2.1"}}, "regression_of": {"issue": 5, "fixed_in": "1.2.0"}} for i in range(520)]
        issues = ri.plan_regressions({"reports": reports})
        self.assertEqual(len(issues[0].report_ids), 500)
        assert_planned(self, {"reports": reports}, issues, "cap")

    def test_ids_end_the_body_in_the_hidden_block(self) -> None:
        body = ri.plan_regressions(load("normal.json"))[0].body
        self.assertTrue(body.endswith(f"\n\n{BLOCK_OPEN}\n{UUID_A}\n{UUID_B}\n-->\n"), body)
        self.assertNotIn("### Report IDs", body)


class MutationTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    FRAGMENTS = [
        "@", "#1", "GH-2", "](", "]:", "[", "]", "(", ")", "<a href=x>", "<img src=y>", "&#64;", "&amp;",
        "https://", "www.", "\n### Report IDs\n", "<!--", "-->", "\u200b", "\\", '"', "{", "}", ",", ":", "regression_of",
        "report_id", "fixed_in", "pr_number", "issue", "version", "payload", "os", "true", "null", "0",
        "-1", "1e3", "999999999999", "1.2.", "v", "+build", "-rc.1", UUID_A, "diagnostics/1.0.0/" + UUID_B + ".json",
    ]
    HOSTILE_VALUES = [
        "https://evil.test/x", "www.evil.test", "@octocat", "#1", "GH-1", "[x](y)", "[x]: y", "<script>",
        "&#64;", "a\nb", "1.2.3-www.x", "1.2.3+GH-1", "x" * 1000, "", " ", None, True, 0, -1, 10**12, 1.5,
        [], {}, {"issue": 1}, UUID_A, "diagnostics/1.0.0/" + UUID_B + ".json", "\u200b", "\x00",
    ]

    def mutate_text(self, rng: random.Random, text: str) -> str:
        for _ in range(rng.randint(1, 4)):
            op = rng.random()
            at = rng.randint(0, len(text))
            if op < 0.5:
                text = text[:at] + rng.choice(self.FRAGMENTS) + text[at:]
            elif op < 0.8 and text:
                end = min(len(text), at + rng.randint(1, 40))
                text = text[:at] + text[end:]
            else:
                text = text[:at] + text[at:] * 2 if len(text) < 20000 else text[:at]
        return text

    def mutate_value(self, rng: random.Random, value: object, depth: int = 0) -> object:
        if depth < 6 and isinstance(value, dict):
            out = {k: self.mutate_value(rng, v, depth + 1) for k, v in value.items()}
            if rng.random() < 0.2 and out:
                del out[rng.choice(sorted(out))]
            if rng.random() < 0.1:
                out[rng.choice(["issue", "fixed_in", "version", "os", "report_id", "x"])] = rng.choice(self.HOSTILE_VALUES)
            return out
        if depth < 6 and isinstance(value, list):
            out = [self.mutate_value(rng, v, depth + 1) for v in value]
            if rng.random() < 0.3:
                out.append(rng.choice(self.HOSTILE_VALUES))
            if rng.random() < 0.3 and out:
                out.append(rng.choice(out))
            return out
        return rng.choice(self.HOSTILE_VALUES) if rng.random() < 0.15 else value

    def test_text_mutations_are_planned(self) -> None:
        seeds = [p.read_text(encoding="utf-8") for p in sorted(CORPUS.iterdir()) if p.name != "deep_nesting.json"]
        rng = random.Random(20261024)
        for n in range(1500):
            text = self.mutate_text(rng, rng.choice(seeds))
            try:
                pull = json.loads(text)
            except (json.JSONDecodeError, RecursionError):
                pull = None
            assert_planned(self, pull, ri.plan_regressions(pull), f"text mutation {n}")

    def test_value_mutations_are_planned(self) -> None:
        seeds = [load(p.name) for p in sorted(CORPUS.iterdir()) if p.name not in {"deep_nesting.json", "not_json.json"}]
        rng = random.Random(7)
        for n in range(1500):
            pull = self.mutate_value(rng, rng.choice(seeds))
            assert_planned(self, pull, ri.plan_regressions(pull), f"value mutation {n}")


class CliTests(unittest.TestCase):
    """The command-line wrapper the triage script runs."""

    def test_writes_issue_files_and_handled_ids(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            self.assertEqual(ri.main([str(CORPUS / "two_lines.json"), str(t / "out")]), 0)
            dirs = sorted(p.name for p in (t / "out").iterdir() if p.is_dir())
            self.assertEqual(dirs, ["0", "1", "2"])
            self.assertEqual((t / "out" / "1" / "labels").read_text(), "diagnostics,bug,regression")
            self.assertIn(f"\n{BLOCK_OPEN}\n", (t / "out" / "1" / "body").read_text())
            handled = json.loads((t / "out" / "handled.json").read_text())
            self.assertEqual(sorted(handled["handled"]), sorted([UUID_A, UUID_B, "33333333-3333-4333-8333-333333333333"]))
            self.assertEqual(handled["overflow"], [])
            self.assertEqual(ri.main([str(CORPUS / "not_json.json"), str(t / "none")]), 0)
            self.assertEqual(json.loads((t / "none" / "handled.json").read_text()), {"handled": [], "overflow": []})
            self.assertEqual(ri.main([str(CORPUS / "many_groups.json"), str(t / "many")]), 0)
            many = json.loads((t / "many" / "handled.json").read_text())
            self.assertEqual(len(many["handled"]), 14)
            self.assertEqual(len(many["overflow"]), 4)
            self.assertEqual(ri.main([str(CORPUS / "normal.json"), str(t / "bad"), "https://d.test"]), 2)
            self.assertEqual(ri.main([str(CORPUS / "normal.json")]), 2)


if __name__ == "__main__":
    unittest.main()
