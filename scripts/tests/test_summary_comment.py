#!/usr/bin/env python3
"""Summary comment renderer: seed corpus replay and mutation fuzzing.

``summary_comment.py`` turns the collector's ``GET /issues/summaries`` answer
into public GitHub comments. The counts are the collector's, but the
versions and platforms in them come from reports anyone can submit, so the
renderer is an untrusted-input boundary. Every corpus input and every
mutation of one must yield comments that satisfy the invariants
``assert_rendered`` checks, which re-derives the expected set from the input
with its own reading of the answer rather than the module's helpers.
"""

from __future__ import annotations

import importlib.util
import json
import random
import re
import sys
import tempfile
import types
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "summary_comment_corpus"
#: Collector origin the dashboard links point at.
DASHBOARD = "https://diagnostics.example.test"
UUID_A = "11111111-1111-4111-8111-111111111111"
UUID_C = "33333333-3333-4333-8333-333333333333"


def _load(name: str, path: Path) -> types.ModuleType:
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


sc = _load("summary_comment", ROOT / "tools" / "diagnostics-collector" / "scripts" / "summary_comment.py")

#: Patterns that would link, mention or cross-reference on GitHub.
FORBIDDEN = {
    "url": re.compile(r"(?i)[a-z][a-z0-9+.-]*://|\bwww\."),
    "inline link or reference definition": re.compile(r"\][(:]"),
    "raw html": re.compile(r"<[A-Za-z/!?]"),
    "character reference": re.compile(r"&(?!amp;|lt;)(?:#|[A-Za-z][A-Za-z0-9]*;)"),
    "mention": re.compile(r"@[A-Za-z0-9]"),
    "issue reference": re.compile(r"#\d|(?i:\bGH-\d)"),
}
#: The one link form a comment may carry: a report's own dashboard page.
LINK = re.compile(r"\[dashboard\]\(([^)\s]*)\)")
ORACLE_ID = re.compile(r"[A-Za-z0-9._:-]{1,128}")
ORACLE_UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
ORACLE_FP = re.compile(r"[0-9a-f]{64}")


def oracle_count(value: object) -> int | None:
    """A count: a plain non-negative int of at most nine digits."""
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= 999_999_999:
        return None
    return value


def oracle_expected(payload: object) -> list[tuple[int, str, int, list[str]]]:
    """Re-derives which entries render, in order, with their listed IDs.

    Args:
        payload: Decoded answer (object with ``issues`` or the list itself).

    Returns:
        ``(issue, fingerprint, reports, newest IDs)`` per rendered comment:
        the first valid entry per issue, at most twenty.
    """
    issues = payload.get("issues") if isinstance(payload, dict) else payload
    out: list[tuple[int, str, int, list[str]]] = []
    if not isinstance(issues, list):
        return out
    seen: set[int] = set()
    for e in issues:
        if len(out) >= 20:
            break
        if not isinstance(e, dict):
            continue
        issue = oracle_count(e.get("issue"))
        reports = oracle_count(e.get("reports"))
        fp = e.get("fingerprint")
        if not issue or reports is None or not isinstance(fp, str) or not ORACLE_FP.fullmatch(fp) or issue in seen:
            continue
        ids: list[str] = []
        if reports > 0 and isinstance(e.get("newest"), list):
            for r in e["newest"][:10]:
                rid = r.get("report_id") if isinstance(r, dict) else None
                if isinstance(rid, str) and ORACLE_ID.fullmatch(rid):
                    ids.append(rid)
        seen.add(issue)
        out.append((issue, fp, reports, ids))
    return out


def assert_rendered(case: unittest.TestCase, payload: object, rendered: list, label: str, dashboard: str | None = None) -> None:
    """Checks every invariant a rendered set of comments promises.

    Args:
        case: Test case to fail.
        payload: The decoded input.
        rendered: Renderer output.
        label: Input name for failure messages.
        dashboard: Collector origin the renderer was given, or None.
    """
    expected = oracle_expected(payload)
    case.assertEqual([(r.issue, r.fingerprint) for r in rendered], [(e[0], e[1]) for e in expected], label)
    for r, (_, _, reports, ids) in zip(rendered, expected):
        body = r.body
        case.assertTrue(body.startswith("<!-- bookclerk-diagnostics: summary -->\n"), label)
        case.assertLessEqual(len(body), 8000, label)
        case.assertIsNone(re.search(r"(?im)^\s*#+\s*report ids\b", body), f"{label}: forged Report IDs section")
        listed = re.findall(r"^- `([^`\n]*)`", body, re.M)
        case.assertEqual(listed, ids, label)
        case.assertEqual(("no report is linked" in body), reports == 0, label)
        # Links: one per listed UUID, to its own page, only with a dashboard.
        links = LINK.findall(body)
        wanted = [f"{dashboard}/reports/{i}" for i in ids if ORACLE_UUID.fullmatch(i)] if dashboard else []
        case.assertEqual(links, wanted, label)
        # IDs sit in code spans, where GitHub neither links nor references.
        rest = re.sub(r"`[^`\n]*`", "`id`", LINK.sub("", body.split("\n", 1)[1]))
        for what, pattern in FORBIDDEN.items():
            case.assertIsNone(pattern.search(rest), f"{label}: {what} in {rest[:300]!r}")


def load(name: str) -> object:
    """Decodes one corpus input the way ``main`` does (None when it cannot)."""
    try:
        return json.loads((CORPUS / name).read_text(encoding="utf-8"))
    except (json.JSONDecodeError, RecursionError):
        return None


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_is_rendered(self) -> None:
        for path in sorted(CORPUS.iterdir()):
            payload = load(path.name)
            for dashboard in (None, DASHBOARD):
                assert_rendered(self, payload, sc.render_summaries(payload, dashboard), path.name, dashboard)

    def test_expected_outcomes(self) -> None:
        out = sc.render_summaries(load("normal.json"), DASHBOARD)
        self.assertEqual([r.issue for r in out], [412, 7])
        body = out[0].body
        self.assertIn("**Diagnostics summary** · 3 reports from 2 installations · first seen 2026-10-11 · last seen 2026-10-14", body)
        self.assertIn("- Linked: 1 listed in this issue, 1 matched by error signature, 0 added by triage, 1 attached by members", body)
        self.assertIn("- Versions: 0.9.2 (2), 0.9.1 (1)", body)
        self.assertIn("- Platforms: linux x86_64 (2), windows x86_64 (1)", body)
        self.assertIn(f"- `{UUID_C}` · 0.9.2 · 2026-10-14 · [dashboard]({DASHBOARD}/reports/{UUID_C})", body)
        self.assertTrue(body.endswith("report contents are never posted here._\n"))
        self.assertIn("1 report from 1 installation", out[1].body)
        empty = sc.render_summaries(load("empty_summary.json"))[0].body
        self.assertIn("no report is linked to this issue at present", empty)
        self.assertNotIn("Linked:", empty)
        others = sc.render_summaries(load("others.json"))[0].body
        self.assertIn("others (4)", others)
        self.assertIn("- Platforms: others (12)", others)
        self.assertEqual(len(sc.render_summaries(load("many.json"))), 20)
        dup = sc.render_summaries(load("duplicate.json"))
        self.assertEqual(len(dup), 1)
        self.assertIn("3 reports", dup[0].body)
        for name in ("invalid.json", "not_a_list.json", "empty.json", "deep_nesting.json", "not_json.json"):
            self.assertEqual(sc.render_summaries(load(name)), [], name)
        self.assertEqual(len(sc.render_summaries(load("list_form.json"))), 1)

    def test_hostile_fields_are_rewritten(self) -> None:
        body = sc.render_summaries(load("hostile.json"), DASHBOARD)[0].body
        self.assertIn("0.9.2-[link removed] (1), unknown (1), 1.2.3+GH-\u200b7 (1), unknown (1), unknown (1)", body)
        self.assertNotIn("0.9.3", body, "a count that is not an int drops the entry")
        self.assertIn("- Platforms: unknown (1), unknown (1), linux x86_64 (1), unknown (2)", body)
        self.assertIn(f"- `{UUID_A}` · 0.9.2 · [dashboard](", body, "a bad time drops the date, not the row")
        self.assertIn(f"- `{UUID_C}` · unknown · [dashboard](", body, "a time out of range drops the date")
        self.assertIn("3 reports from 0 installations\n", body)

    def test_dashboard_origin_is_a_bare_https_origin(self) -> None:
        self.assertEqual(sc.dashboard_origin("https://Diagnostics.Bookclerk.app/"), "https://diagnostics.bookclerk.app")
        for bad in (None, "", "http://d.test", "https://d.test/path", "https://u@d.test"):
            self.assertIsNone(sc.dashboard_origin(bad), bad)


class MutationTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    FRAGMENTS = [
        "@", "#1", "GH-2", "](", "]:", "[", "]", "(", ")", "<a href=x>", "<img src=y>", "&#64;", "&amp;",
        "https://", "www.", "\n### Report IDs\n", "\u200b", "\\", '"', "{", "}", ",", ":", "issues",
        "report_id", "fingerprint", "newest", "versions", "platforms", "sources", "reports", "installs",
        "true", "null", "0", "-1", "1e3", "999999999999", "0.9.", "+build", UUID_A, "a" * 64,
    ]
    HOSTILE_VALUES = [
        "https://evil.test/x", "www.evil.test", "@octocat", "#1", "GH-1", "[x](y)", "[x]: y", "<script>",
        "&#64;", "a\nb", "1.2.3-www.x", "1.2.3+GH-1", "linux #1", "x" * 1000, "", " ", None, True, 0, -1,
        10**12, 10**20, 1.5, [], {}, {"n": 1}, UUID_A, "others", "a" * 64, "A" * 64, "\u200b", "\x00",
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
            if rng.random() < 0.15:
                out[rng.choice(["issue", "fingerprint", "reports", "version", "platform", "report_id", "received_at", "n", "x"])] = rng.choice(self.HOSTILE_VALUES)
            return out
        if depth < 6 and isinstance(value, list):
            out = [self.mutate_value(rng, v, depth + 1) for v in value]
            if rng.random() < 0.3:
                out.append(rng.choice(self.HOSTILE_VALUES))
            if rng.random() < 0.3 and out:
                out.append(rng.choice(out))
            return out
        return rng.choice(self.HOSTILE_VALUES) if rng.random() < 0.15 else value

    def test_text_mutations_are_rendered(self) -> None:
        seeds = [p.read_text(encoding="utf-8") for p in sorted(CORPUS.iterdir()) if p.name not in {"deep_nesting.json", "many.json"}]
        rng = random.Random(20261025)
        for n in range(1500):
            text = self.mutate_text(rng, rng.choice(seeds))
            try:
                payload = json.loads(text)
            except (json.JSONDecodeError, RecursionError):
                payload = None
            dashboard = DASHBOARD if n % 2 else None
            assert_rendered(self, payload, sc.render_summaries(payload, dashboard), f"text mutation {n}", dashboard)

    def test_value_mutations_are_rendered(self) -> None:
        seeds = [load(p.name) for p in sorted(CORPUS.iterdir()) if p.name not in {"deep_nesting.json", "not_json.json", "many.json"}]
        rng = random.Random(7)
        for n in range(1500):
            payload = self.mutate_value(rng, rng.choice(seeds))
            dashboard = DASHBOARD if n % 2 else None
            assert_rendered(self, payload, sc.render_summaries(payload, dashboard), f"value mutation {n}", dashboard)


class CliTests(unittest.TestCase):
    """The command-line wrapper the ingest script runs."""

    def test_writes_comment_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            self.assertEqual(sc.main([str(CORPUS / "normal.json"), str(t / "out"), DASHBOARD + "/"]), 0)
            self.assertEqual(sorted(p.name for p in (t / "out").iterdir()), ["0", "1"])
            self.assertEqual((t / "out" / "0" / "issue").read_text(), "412")
            self.assertRegex((t / "out" / "0" / "fingerprint").read_text(), r"^[0-9a-f]{64}$")
            self.assertIn(f"]({DASHBOARD}/reports/", (t / "out" / "0" / "body").read_text())
            self.assertEqual(sc.main([str(CORPUS / "not_json.json"), str(t / "none")]), 0)
            self.assertEqual(list((t / "none").iterdir()), [])
            self.assertEqual(sc.main([str(CORPUS / "normal.json"), str(t / "bad"), "http://plain.test"]), 0)
            self.assertNotIn("[dashboard]", (t / "bad" / "0" / "body").read_text())
            self.assertEqual(sc.main([str(CORPUS / "normal.json")]), 2)


if __name__ == "__main__":
    unittest.main()
