#!/usr/bin/env python3
"""Image release-tag parser: seed corpus replay and mutation fuzzing.

The image publishing workflow turns a release's ``tag_name`` into registry
tags with ``image_release_tags.py``, so the tag text is an untrusted-input
boundary. Every corpus input and every mutation of one must either be refused
with ``ReleaseTagError`` or yield the tags the oracle derives; the oracle
splits and checks the tag by hand instead of using the parser's expression.
"""

from __future__ import annotations

import importlib.util
import random
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "image_release_tags_corpus"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


irt = _load("image_release_tags", ROOT / "scripts" / "image_release_tags.py")

#: Fragments mutations splice in: version, shell, output and reference syntax.
FRAGMENTS = [
    "v", "V", ".", "0", "1", "9", "00", "-rc.1", "+b", " ", "\n", "\r", "\t", ":", "@", "/", "$", "`",
    "latest", "sha256", "٣", "１", "é", "\x00",
]


def run(raw: bytes) -> list[str] | None:
    """Parses ``raw``, mapping a refusal to ``None``."""
    try:
        return irt.release_tags(raw)
    except irt.ReleaseTagError:
        return None


def expected(raw: bytes) -> list[str] | None:
    """Independent oracle: the image tags ``raw`` should yield, or ``None``."""
    try:
        tag = raw.decode("utf-8")
    except UnicodeDecodeError:
        return None
    if not tag.startswith("v"):
        return None
    parts = tag[1:].split(".")
    if len(parts) != 3:
        return None
    for part in parts:
        if not 1 <= len(part) <= 9 or not all(c in "0123456789" for c in part):
            return None
        if len(part) > 1 and part[0] == "0":
            return None
    major, minor, patch = parts
    return [f"{major}.{minor}.{patch}", f"{major}.{minor}", major, "latest"]


def assert_safe(case: unittest.TestCase, raw: bytes, label: str) -> None:
    """Checks the parser against the oracle and the output invariants."""
    got = run(raw)
    case.assertEqual(got, expected(raw), label)
    for tag in got or []:
        case.assertRegex(tag, r"\A(latest|[0-9]{1,9}(\.[0-9]{1,9}){0,2})\Z", label)


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_matches_oracle(self) -> None:
        paths = sorted(p for p in CORPUS.iterdir() if p.is_file())
        self.assertGreater(len(paths), 0)
        for path in paths:
            assert_safe(self, path.read_bytes(), path.name)

    def test_expected_outcomes(self) -> None:
        def load(name: str) -> list[str] | None:
            return run((CORPUS / name).read_bytes())

        self.assertEqual(load("stable"), ["1.2.3", "1.2", "1", "latest"])
        self.assertEqual(load("zero_major"), ["0.1.0", "0.1", "0", "latest"])
        self.assertEqual(load("nine_digits"), ["123456789.0.0", "123456789.0", "123456789", "latest"])
        for refused in (
            "ten_digits", "leading_zero", "no_v", "capital_v", "two_parts", "four_parts", "prerelease",
            "build_metadata", "trailing_newline", "newline_injection", "space_injection", "colon", "path",
            "arabic_digit", "invalid_utf8", "empty", "command_substitution",
        ):
            self.assertIsNone(load(refused), refused)

    def test_oversized_tag_is_refused(self) -> None:
        self.assertIsNone(run(b"v1.2.3" + b" " * irt.MAX_TAG_BYTES))


class MutationFuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 4000

    def mutate(self, rng: random.Random, text: str) -> bytes:
        for _ in range(rng.randint(1, 4)):
            pos = rng.randint(0, len(text))
            op = rng.random()
            if op < 0.5:
                text = text[:pos] + rng.choice(FRAGMENTS) + text[pos:]
            elif op < 0.75:
                text = text[:pos] + text[pos + rng.randint(1, 3):]
            else:
                text = text[:pos] + chr(rng.randint(0, 0x2FF)) + text[pos:]
        raw = text.encode("utf-8", "surrogatepass")
        if rng.random() < 0.05 and raw:
            cut = rng.randint(0, len(raw) - 1)
            raw = raw[:cut] + bytes([rng.randint(0x80, 0xFF)]) + raw[cut + 1:]
        return raw

    def test_mutations_match_oracle(self) -> None:
        seeds = [p.read_bytes().decode("utf-8", "replace") for p in sorted(CORPUS.iterdir()) if p.is_file()]
        rng = random.Random(20261027)
        for n in range(self.ITERATIONS):
            raw = self.mutate(rng, rng.choice(seeds))
            assert_safe(self, raw, f"iteration {n}: {raw!r}")

    def test_generated_versions_match_oracle(self) -> None:
        rng = random.Random(479)
        pieces = ["0", "1", "01", "10", "999999999", "1000000000", "", "x", "٣"]
        for n in range(self.ITERATIONS):
            parts = [rng.choice(pieces) for _ in range(rng.randint(1, 4))]
            raw = (rng.choice(["v", "", "V", "vv"]) + ".".join(parts)).encode()
            assert_safe(self, raw, f"generated {n}: {raw!r}")


class CliTests(unittest.TestCase):
    """The command-line wrapper the publishing workflow runs."""

    def test_exit_codes(self) -> None:
        self.assertEqual(irt.main((CORPUS / "stable").read_bytes()), 0)
        self.assertEqual(irt.main((CORPUS / "newline_injection").read_bytes()), 1)


if __name__ == "__main__":
    unittest.main()
