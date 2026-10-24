#!/usr/bin/env python3
"""Holding issue body editor: seed corpus replay and mutation fuzzing.

``holding_issue.py`` rewrites the body of the issue that holds untriaged
diagnostics reports, which anyone with triage rights can edit on GitHub, so
it is an untrusted-input boundary. Every corpus body and every mutation of
one must either be refused with ``BodyError`` or come back satisfying the
invariants ``assert_appended`` checks, which re-derives them with its own
reading of the body rather than the module's helpers.
"""

from __future__ import annotations

import importlib.util
import random
import re
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "holding_issue_corpus"
DASHBOARD = "https://diagnostics.example.test"
NEW_IDS = ["33333333-3333-4333-8333-333333333333", "44444444-4444-4444-8444-444444444444"]
#: Lines the triage script would ask to append.
LINES = "".join(f"- `{i}` · [dashboard]({DASHBOARD}/reports/{i})\n" for i in NEW_IDS)


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


hi = _load("holding_issue", ROOT / "tools" / "diagnostics-collector" / "scripts" / "holding_issue.py")

#: What the dashboard's parser (dashboard/report-ids.js) reads: a list line
#: directly under the last Report IDs heading.
ORACLE_LINE = re.compile(r"^[-*] `([A-Za-z0-9._:/+-]{1,160})`(?: · \[dashboard\]\([^\s()]{1,300}\))?\s*$")


def oracle_ids(body: str) -> list[str]:
    """Reads the listed IDs the way the dashboard does.

    Args:
        body: Issue body.

    Returns:
        IDs under the last ``### Report IDs`` heading, in order, duplicates
        kept, stopping at the first line that is not a list line.
    """
    lines = body.split("\n")
    starts = [i for i, line in enumerate(lines) if re.match(r"^###\s+Report IDs\s*$", line)]
    if not starts:
        return []
    out = []
    for line in lines[starts[-1] + 1 :]:
        m = ORACLE_LINE.match(line)
        if not m:
            break
        out.append(m.group(1))
    return out


def wanted_ids(lines_text: str) -> list[str]:
    """IDs of the well-formed candidate lines, first occurrence each."""
    out: list[str] = []
    for line in lines_text.splitlines():
        m = ORACLE_LINE.match(line.rstrip("\r"))
        if m and m.group(1) not in out:
            out.append(m.group(1))
    return out


def assert_appended(case: unittest.TestCase, body: str, lines_text: str, out: str, label: str) -> None:
    """Checks every invariant a successful append promises.

    Args:
        case: Test case to fail.
        body: Input body.
        lines_text: Candidate lines.
        out: The module's output.
        label: Input name for failure messages.
    """
    normalized = body.replace("\r\n", "\n")
    before = oracle_ids(normalized)
    added = [i for i in wanted_ids(lines_text) if i not in before]
    case.assertEqual(oracle_ids(out), before + added, label)
    case.assertLessEqual(len(set(oracle_ids(out))), hi.MAX_IDS, label)
    case.assertLessEqual(len(out), hi.MAX_BODY_CHARS, label)
    # Everything but the inserted lines is unchanged: removing them, in
    # place, gives the normalized input back.
    out_lines = out.split("\n")
    in_lines = normalized.split("\n")
    case.assertEqual(len(out_lines), len(in_lines) + len(added), label)
    cut = [line for line in out_lines]
    for rid in added:
        idx = next(i for i, line in enumerate(cut) if ORACLE_LINE.match(line) and ORACLE_LINE.match(line).group(1) == rid)
        del cut[idx]
    case.assertEqual(cut, in_lines, label)
    case.assertEqual(hi.append_ids(out, lines_text), out, f"{label}: idempotent")


def run(body: str, lines_text: str) -> str | None:
    """Appends, or returns None when the module refused."""
    try:
        return hi.append_ids(body, lines_text)
    except hi.BodyError:
        return None


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_is_appended_or_refused(self) -> None:
        for path in sorted(CORPUS.iterdir()):
            body = path.read_text(encoding="utf-8")
            out = run(body, LINES)
            if out is not None:
                assert_appended(self, body, LINES, out, path.name)

    def test_expected_outcomes(self) -> None:
        refused = {"no_heading.md", "full.md", "long.md"}
        for path in sorted(CORPUS.iterdir()):
            out = run(path.read_text(encoding="utf-8"), LINES)
            self.assertEqual(out is None, path.name in refused, path.name)
        normal = (CORPUS / "normal.md").read_text(encoding="utf-8")
        out = hi.append_ids(normal, LINES)
        self.assertIn(f"- `{NEW_IDS[1]}` · [dashboard]({DASHBOARD}/reports/{NEW_IDS[1]})\n\nReports no triage", out)
        self.assertTrue(out.startswith(normal.split("\n\nReports no triage")[0]))
        # Lines go under the last heading, after its last listed ID.
        two = hi.append_ids((CORPUS / "two_sections.md").read_text(encoding="utf-8"), LINES)
        self.assertEqual(oracle_ids(two)[-2:], NEW_IDS)
        self.assertEqual(two.count("### Report IDs"), 2)

    def test_already_listed_and_malformed_lines_are_skipped(self) -> None:
        normal = (CORPUS / "normal.md").read_text(encoding="utf-8")
        listed = "11111111-1111-4111-8111-111111111111"
        lines = f"- `{listed}`\nnot a line\n- `<b>`\n{LINES}- `{NEW_IDS[0]}`\n"
        out = hi.append_ids(normal, lines)
        self.assertEqual(oracle_ids(out), [listed] + NEW_IDS)
        self.assertEqual(hi.append_ids(normal, ""), normal)

    def test_cap_counts_distinct_listed_ids(self) -> None:
        body = "### Report IDs\n" + "".join(f"- `{i:08x}-0000-4000-8000-000000000000`\n" for i in range(499))
        self.assertIsNotNone(run(body, f"- `{NEW_IDS[0]}`\n"))
        self.assertIsNone(run(body, LINES))


class MutationTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    FRAGMENTS = [
        "\n### Report IDs\n", "### Report IDs", "\r\n", "\n", "- `", "`", "* `", " · [dashboard](", ")", "(",
        "](", "<img src=x>", "@", "#1", "x" * 200, "11111111-1111-4111-8111-111111111111",
        NEW_IDS[0], "diagnostics/0.1.0/" + NEW_IDS[1] + ".json", " ", "\t", "​", "\x00",
    ]

    def mutate(self, rng: random.Random, text: str) -> str:
        for _ in range(rng.randint(1, 4)):
            op = rng.random()
            at = rng.randint(0, len(text))
            if op < 0.5:
                text = text[:at] + rng.choice(self.FRAGMENTS) + text[at:]
            elif op < 0.8 and text:
                end = min(len(text), at + rng.randint(1, 40))
                text = text[:at] + text[end:]
            else:
                text = text[:at] + text[at:] * 2 if len(text) < 4000 else text[:at]
        return text

    def test_mutations_are_appended_or_refused(self) -> None:
        seeds = [p.read_text(encoding="utf-8") for p in sorted(CORPUS.iterdir()) if p.name != "long.md"]
        rng = random.Random(20261024)
        for n in range(1500):
            body = self.mutate(rng, rng.choice(seeds))
            lines = self.mutate(rng, LINES) if rng.random() < 0.3 else LINES
            out = run(body, lines)
            if out is not None:
                assert_appended(self, body, lines, out, f"mutation {n}")

    def test_random_bodies_never_raise_anything_else(self) -> None:
        rng = random.Random(7)
        alphabet = "#- `*\n\r·[]()abc0-19\t"
        for n in range(500):
            body = "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 300)))
            out = run(body, LINES)
            if out is not None:
                assert_appended(self, body, LINES, out, f"random {n}")


class CliTests(unittest.TestCase):
    """The command-line wrapper the triage script runs."""

    def test_prints_the_body_and_fails_when_it_cannot(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            (t / "body").write_text((CORPUS / "normal.md").read_text(encoding="utf-8"), encoding="utf-8")
            (t / "lines").write_text(LINES, encoding="utf-8")
            self.assertEqual(hi.main([str(t / "body"), str(t / "lines")]), 0)
            (t / "body").write_text("no section", encoding="utf-8")
            self.assertEqual(hi.main([str(t / "body"), str(t / "lines")]), 1)
            self.assertEqual(hi.main([str(t / "body")]), 2)


if __name__ == "__main__":
    unittest.main()
