#!/usr/bin/env python3
"""Demo deploy answer readers: seed corpus replay and mutation fuzzing.

``demo-deploy.yml`` reads the registry's manifest for the published demo
image and the live demo's health JSON with ``scripts/demo_image.py``, and
what they return decides which image reaches the public demo, so both are an
untrusted-input boundary. Every corpus input and every mutation of one must
give the oracle's answer: the oracle checks digests and commit ids character
by character instead of using the reader's expressions.
"""

from __future__ import annotations

import importlib.util
import json
import random
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "demo_image_corpus"
HEX = "0123456789abcdef"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


di = _load("demo_image", ROOT / "scripts" / "demo_image.py")

#: Fragments mutations splice in: JSON, digest, shell, output and reference syntax.
FRAGMENTS = [
    '"', "{", "}", "[", "]", ":", ",", "\\n", "\n", " ", "sha256:", "sha512:", "@", "/", "=", "$", "`",
    "true", "false", "null", "1", "0", "a", "f", "g", "A", "٣", "é", "\\u0000", '"digest":', '"version":',
    '"configured":', "NaN",
]


def _object(raw: bytes) -> dict | None:
    """The JSON object ``raw`` holds, or ``None``."""
    if len(raw) > di.MAX_ANSWER_BYTES:
        return None
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError):
        return None
    return value if isinstance(value, dict) else None


def _hex(text: str, length: int) -> bool:
    """Whether ``text`` is exactly ``length`` lowercase hex digits."""
    return len(text) == length and all(c in HEX for c in text)


def expected_digest(raw: bytes) -> str | None:
    """Independent oracle: the digest ``raw`` names, or ``None``."""
    obj = _object(raw)
    if obj is None:
        return None
    digest = obj.get("digest")
    if type(digest) is not str or not digest.startswith("sha256:"):
        return None
    return digest if _hex(digest[len("sha256:"):], 64) else None


def expected_live(raw: bytes) -> str | None:
    """Independent oracle: the commit ``raw`` reports, or ``None``."""
    obj = _object(raw)
    if obj is None or obj.get("configured") is not True:
        return None
    version = obj.get("version")
    return version if type(version) is str and _hex(version, 40) else None


def run_digest(raw: bytes) -> str | None:
    """Reads a digest, mapping a refusal to ``None``."""
    try:
        return di.manifest_digest(raw)
    except di.DemoImageError:
        return None


def assert_safe(case: unittest.TestCase, kind: str, raw: bytes, label: str) -> None:
    """Checks one reader against its oracle and the output invariants."""
    if kind == "digest":
        got = run_digest(raw)
        case.assertEqual(got, expected_digest(raw), label)
        if got is not None:
            case.assertRegex(got, r"\Asha256:[0-9a-f]{64}\Z", label)
    else:
        got = di.live_version(raw)
        case.assertEqual(got, expected_live(raw), label)
        if got is not None:
            case.assertRegex(got, r"\A[0-9a-f]{40}\Z", label)


def corpus(kind: str) -> list[Path]:
    """The seed files for one reader."""
    return sorted(p for p in (CORPUS / kind).iterdir() if p.is_file())


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_matches_oracle(self) -> None:
        for kind in ("digest", "live"):
            paths = corpus(kind)
            self.assertGreater(len(paths), 0)
            for path in paths:
                assert_safe(self, kind, path.read_bytes(), f"{kind}/{path.name}")

    def test_expected_outcomes(self) -> None:
        digest = "sha256:38048f0f939e3835925ca7e460ddc5e7bdd91274e4c1252349cb0d36ebabf705"
        for name in ("typical", "trailing_newline", "duplicate_key"):
            self.assertEqual(run_digest((CORPUS / "digest" / name).read_bytes()), digest, name)
        for name in (
            "uppercase", "short", "long", "sha512", "missing", "null", "number", "array", "bare_string", "empty",
            "not_json", "newline_injection", "reference_injection", "space", "arabic_digit", "invalid_utf8",
        ):
            self.assertIsNone(run_digest((CORPUS / "digest" / name).read_bytes()), name)
        commit = "b37d5601bb78960e0cd50a80c11c208f76e0d91f"
        self.assertEqual(di.live_version((CORPUS / "live" / "typical").read_bytes()), commit)
        for name in (
            "unconfigured", "configured_string", "configured_one", "short_version", "uppercase_version",
            "dev_version", "null_version", "missing_version", "newline_injection", "html", "empty", "array",
            "invalid_utf8",
        ):
            self.assertIsNone(di.live_version((CORPUS / "live" / name).read_bytes()), name)

    def test_oversized_answers_are_refused(self) -> None:
        padded = (CORPUS / "digest" / "typical").read_bytes() + b" " * di.MAX_ANSWER_BYTES
        self.assertIsNone(run_digest(padded))
        padded = (CORPUS / "live" / "typical").read_bytes() + b" " * di.MAX_ANSWER_BYTES
        self.assertIsNone(di.live_version(padded))

    def test_deep_nesting_is_refused(self) -> None:
        deep = b"[" * 100_000 + b"]" * 100_000
        self.assertIsNone(run_digest(deep[: di.MAX_ANSWER_BYTES]))
        self.assertIsNone(di.live_version(b'{"a":' * 10_000 + b"1" + b"}" * 10_000))


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
        for kind, seed in (("digest", 20261107), ("live", 740)):
            seeds = [p.read_bytes().decode("utf-8", "replace") for p in corpus(kind)]
            rng = random.Random(seed)
            for n in range(self.ITERATIONS):
                raw = self.mutate(rng, rng.choice(seeds))
                assert_safe(self, kind, raw, f"{kind} iteration {n}: {raw!r}")

    def test_generated_values_match_oracle(self) -> None:
        rng = random.Random(4096)
        alphabet = HEX + "ABCDEFgz:٣ "
        for n in range(self.ITERATIONS):
            body = "".join(rng.choice(alphabet) for _ in range(rng.choice([39, 40, 41, 63, 64, 65])))
            prefix = rng.choice(["sha256:", "", "sha512:", "SHA256:"])
            raw = json.dumps({"digest": prefix + body}).encode()
            assert_safe(self, "digest", raw, f"generated digest {n}: {raw!r}")
            configured = rng.choice([True, False, 1, "true", None])
            raw = json.dumps({"configured": configured, "version": body}).encode()
            assert_safe(self, "live", raw, f"generated live {n}: {raw!r}")


class CliTests(unittest.TestCase):
    """The command-line wrapper the deploy workflow runs."""

    def test_exit_codes(self) -> None:
        self.assertEqual(di.main(["digest"], (CORPUS / "digest" / "typical").read_bytes()), 0)
        self.assertEqual(di.main(["digest"], (CORPUS / "digest" / "newline_injection").read_bytes()), 1)
        self.assertEqual(di.main(["live"], (CORPUS / "live" / "html").read_bytes()), 0)
        self.assertEqual(di.main(["live"], (CORPUS / "live" / "typical").read_bytes()), 0)
        self.assertEqual(di.main([], b""), 2)


if __name__ == "__main__":
    unittest.main()
