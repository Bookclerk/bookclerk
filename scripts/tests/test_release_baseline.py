#!/usr/bin/env python3
"""Diagnostics ingest release-tag parser: seed corpus replay and mutation fuzzing.

The ingest job writes the tag ``release_baseline.py`` returns to
``$GITHUB_OUTPUT`` and into the collector's query string, so the GitHub
response it parses is an untrusted-input boundary. Every corpus input and
every mutation of one must either be refused with ``BaselineError`` or yield
a tag that ``assert_safe`` accepts; the oracle re-derives the expected tag
with a regular expression instead of the parser's character set.
"""

from __future__ import annotations

import importlib.util
import json
import random
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "release_baseline_corpus"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


rb = _load("release_baseline", ROOT / "tools" / "diagnostics-collector" / "scripts" / "release_baseline.py")

#: A tag the ingest job may pass on: a plain version label, nothing else.
SAFE_TAG = re.compile(r"[A-Za-z0-9._-]{1,64}")

#: Fragments mutations splice in: query, output and JSON syntax.
FRAGMENTS = [
    "&", "=", "?", "#", "%0a", "\\n", "\\r", "\\u000a", "\\u0026", "+", " ", "/", '"', "{", "}",
    ",", ":", "[", "]", "true", "false", "null", '"draft":', '"prerelease":', '"tag_name":', "v1.2.3",
    "é", "​",
]


def run(body: bytes) -> str | None:
    """Parses ``body``, mapping a refusal to ``None``."""
    try:
        return rb.parse_release(body)
    except rb.BaselineError:
        return None


def expected(body: bytes) -> str | None:
    """Independent oracle: the tag ``body`` should yield, or ``None``."""
    try:
        data = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError):
        return None
    if type(data) is not dict:
        return None
    if data.get("draft") is True or data.get("prerelease") is True:
        return ""
    tag = data.get("tag_name")
    if isinstance(tag, str) and SAFE_TAG.fullmatch(tag):
        return tag
    return None


def assert_safe(case: unittest.TestCase, body: bytes, label: str) -> None:
    """Checks the parser against the oracle and the output invariants."""
    got = run(body)
    case.assertEqual(got, expected(body), label)
    if got:
        case.assertNotIn("\n", got, label)
        case.assertNotIn("&", got, label)
        case.assertLessEqual(len(got), rb.MAX_TAG_CHARS, label)


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_matches_oracle(self) -> None:
        paths = sorted(CORPUS.glob("*.json"))
        self.assertGreater(len(paths), 0)
        for path in paths:
            assert_safe(self, path.read_bytes(), path.name)

    def test_expected_outcomes(self) -> None:
        def load(name: str) -> str | None:
            return run((CORPUS / name).read_bytes())

        self.assertEqual(load("stable.json"), "v1.2.3")
        self.assertEqual(load("prerelease.json"), "")
        self.assertEqual(load("draft.json"), "")
        for refused in (
            "query_injection.json", "newline_injection.json", "empty_tag.json", "numeric_tag.json",
            "array.json", "html.json", "truncated.json", "plus_tag.json", "long_tag.json",
        ):
            self.assertIsNone(load(refused), refused)

    def test_oversized_body_is_refused(self) -> None:
        body = b'{"tag_name":"v1","pad":"' + b"x" * rb.MAX_BODY_BYTES + b'"}'
        self.assertIsNone(run(body))


class MutationFuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 4000

    def mutate(self, rng: random.Random, text: str) -> bytes:
        for _ in range(rng.randint(1, 6)):
            pos = rng.randint(0, len(text))
            op = rng.random()
            if op < 0.5:
                text = text[:pos] + rng.choice(FRAGMENTS) + text[pos:]
            elif op < 0.75:
                text = text[:pos] + text[pos + rng.randint(1, 8):]
            else:
                text = text[:pos] + chr(rng.randint(0, 0x2FF)) + text[pos:]
        raw = text.encode("utf-8", "surrogatepass")
        if rng.random() < 0.05 and raw:
            cut = rng.randint(0, len(raw) - 1)
            raw = raw[:cut] + bytes([rng.randint(0x80, 0xFF)]) + raw[cut + 1:]
        return raw

    def test_mutations_match_oracle(self) -> None:
        seeds = [p.read_text(encoding="utf-8") for p in sorted(CORPUS.glob("*.json"))]
        rng = random.Random(20261024)
        for n in range(self.ITERATIONS):
            body = self.mutate(rng, rng.choice(seeds))
            assert_safe(self, body, f"iteration {n}: {body[:200]!r}")

    def test_generated_tags_match_oracle(self) -> None:
        rng = random.Random(398)
        alphabet = [chr(c) for c in range(0x20, 0x7F)] + ["\n", "\r", "\t", "é"]
        for n in range(self.ITERATIONS):
            tag = "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 70)))
            body = json.dumps({"tag_name": tag, "draft": rng.random() < 0.1}).encode()
            assert_safe(self, body, f"generated {n}: {tag!r}")


class CliTests(unittest.TestCase):
    """The command-line wrapper the ingest workflow runs."""

    def test_exit_codes(self) -> None:
        self.assertEqual(rb.main((CORPUS / "stable.json").read_bytes()), 0)
        self.assertEqual(rb.main((CORPUS / "query_injection.json").read_bytes()), 1)


if __name__ == "__main__":
    unittest.main()
