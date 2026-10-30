#!/usr/bin/env python3
"""Tool pin bumps: releases-listing replay, mutation fuzzing and file rewrites.

``tool-versions-bump.yml`` hands ``scripts/tool_versions.py`` a GitHub releases
listing and a registry digest, so both are untrusted-input boundaries. Every
corpus listing and every mutation of one must either be refused with
``ToolVersionsError`` or select the version an independent oracle picks; the
oracle reads the JSON by exact types and compares versions digit by digit.
Corpus files are named ``<tool>__<case>``.
"""

from __future__ import annotations

import calendar
import importlib.util
import json
import random
import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "tool_versions_corpus"
DIGEST_CORPUS = CORPUS / "digest"


def _load(name: str, path: Path) -> ModuleType:
    """Import a script that is not on the module path."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


tv = _load("tool_versions", ROOT / "scripts" / "tool_versions.py")

#: Fixed clock, pins and cooldown for replay.
NOW = datetime(2026, 10, 30, tzinfo=timezone.utc)
CURRENT = {"uv": "0.13.6", "cargo-nextest": "0.9.146"}
COOLDOWN_DAYS = 7

#: Fragments mutations splice in: JSON, version and date syntax.
FRAGMENTS = [
    "{", "}", "[", "]", ",", ":", '"', "\\", "\\u0000", "null", "true", "false", "0", "9", ".", "-", "v",
    "T", "Z", "2026", "cargo-nextest-", '"draft"', '"prerelease"', '"published_at"', '"tag_name"', "٣", "é",
]


def run(tool: str, raw: bytes) -> str | None | type:
    """Selects a release, mapping a refusal to the ``ToolVersionsError`` class."""
    try:
        return tv.select_release(tv.TOOLS_BY_NAME[tool], raw, CURRENT[tool], NOW, COOLDOWN_DAYS)
    except tv.ToolVersionsError:
        return tv.ToolVersionsError


def _digits(text: str, lo: int, hi: int) -> bool:
    """Whether ``text`` is ``lo``..``hi`` ASCII digits."""
    return lo <= len(text) <= hi and all(c in "0123456789" for c in text)


def _exact(text: str) -> tuple[int, int, int] | None:
    """Oracle reading of ``<major>.<minor>.<patch>`` without leading zeros."""
    parts = text.split(".")
    if len(parts) != 3:
        return None
    for part in parts:
        if not _digits(part, 1, 9) or (len(part) > 1 and part[0] == "0"):
            return None
    return int(parts[0]), int(parts[1]), int(parts[2])


def _timestamp(text: str) -> int | None:
    """Oracle reading of ``YYYY-MM-DDTHH:MM:SSZ`` as seconds since the epoch."""
    if len(text) != 20 or text[4] != "-" or text[7] != "-" or text[10] != "T" or text[13] != ":" \
            or text[16] != ":" or text[19] != "Z":
        return None
    fields = [text[0:4], text[5:7], text[8:10], text[11:13], text[14:16], text[17:19]]
    if not all(_digits(f, len(f), len(f)) for f in fields):
        return None
    year, month, day, hour, minute, second = (int(f) for f in fields)
    if not (1 <= year and 1 <= month <= 12 and hour <= 23 and minute <= 59 and second <= 59):
        return None
    days_in_month = [31, 29 if calendar.isleap(year) else 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    if not 1 <= day <= days_in_month[month - 1]:
        return None
    return calendar.timegm((year, month, day, hour, minute, second))


def expected(tool: str, raw: bytes) -> str | None | type:
    """Independent oracle: the version to bump to, ``None``, or a refusal."""
    prefix = tv.TOOLS_BY_NAME[tool].tag_prefix
    if len(raw) > tv.MAX_RELEASES_BYTES:
        return tv.ToolVersionsError
    try:
        listing = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return tv.ToolVersionsError
    if type(listing) is not list:
        return tv.ToolVersionsError
    cutoff = calendar.timegm(NOW.timetuple()) - COOLDOWN_DAYS * 86400
    best: tuple[int, int, int] | None = None
    best_text = None
    for release in listing:
        if type(release) is not dict:
            return tv.ToolVersionsError
        tag = release.get("tag_name")
        if type(tag) is not str or tag[: len(prefix)] != prefix:
            continue
        version = _exact(tag[len(prefix):])
        if version is None:
            continue
        if release.get("draft") is not False or release.get("prerelease") is not False:
            continue
        published = release.get("published_at")
        stamp = _timestamp(published) if type(published) is str else None
        if stamp is None:
            return tv.ToolVersionsError
        if stamp > cutoff:
            continue
        if best is None or version > best:
            best, best_text = version, tag[len(prefix):]
    if best is None or best <= _exact(CURRENT[tool]):
        return None
    return best_text


def corpus() -> list[tuple[str, str, bytes]]:
    """Returns ``(name, tool, bytes)`` for each corpus file."""
    return [(p.name, p.name.split("__", 1)[0], p.read_bytes()) for p in sorted(CORPUS.iterdir()) if p.is_file()]


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus."""

    def test_corpus_matches_oracle(self) -> None:
        for name, tool, raw in corpus():
            with self.subTest(case=name):
                self.assertEqual(run(tool, raw), expected(tool, raw))

    def test_expected_outcomes(self) -> None:
        outcomes = {
            "uv__typical": "0.13.7",
            "uv__only_newer_than_cooldown": None,
            "uv__prerelease_and_draft": "0.13.7",
            "uv__unsorted": "0.13.10",
            "uv__odd_tags": None,
            "uv__bad_published_at": tv.ToolVersionsError,
            "uv__impossible_date": tv.ToolVersionsError,
            "uv__not_a_list": tv.ToolVersionsError,
            "uv__item_not_object": tv.ToolVersionsError,
            "uv__invalid_utf8": tv.ToolVersionsError,
            "uv__truncated": tv.ToolVersionsError,
            "uv__missing_flags": None,
            "uv__downgrade_only": None,
            "cargo-nextest__typical": "0.9.147",
            "cargo-nextest__prefix_only": None,
        }
        for name, tool, raw in corpus():
            if name in outcomes:
                with self.subTest(case=name):
                    self.assertEqual(run(tool, raw), outcomes[name])

    def test_oversized_listing_is_refused(self) -> None:
        self.assertIs(run("uv", b"[" + b" " * tv.MAX_RELEASES_BYTES + b"]"), tv.ToolVersionsError)


class MutationFuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 3000

    def mutate(self, rng: random.Random, text: str) -> bytes:
        for _ in range(rng.randint(1, 4)):
            pos = rng.randint(0, len(text))
            op = rng.random()
            if op < 0.5:
                text = text[:pos] + rng.choice(FRAGMENTS) + text[pos:]
            elif op < 0.75:
                text = text[:pos] + text[pos + rng.randint(1, 4):]
            else:
                text = text[:pos] + chr(rng.randint(0, 0x2FF)) + text[pos:]
        raw = text.encode("utf-8", "surrogatepass")
        if rng.random() < 0.05 and raw:
            cut = rng.randint(0, len(raw) - 1)
            raw = raw[:cut] + bytes([rng.randint(0x80, 0xFF)]) + raw[cut + 1:]
        return raw

    def test_mutations_match_oracle(self) -> None:
        seeds = [(tool, raw.decode("utf-8", "replace")) for _, tool, raw in corpus()]
        rng = random.Random(20261030)
        for n in range(self.ITERATIONS):
            tool, text = rng.choice(seeds)
            raw = self.mutate(rng, text)
            got = run(tool, raw)
            self.assertEqual(got, expected(tool, raw), f"iteration {n}: {raw!r}")
            if isinstance(got, str):
                self.assertRegex(got, r"\A[0-9]{1,9}\.[0-9]{1,9}\.[0-9]{1,9}\Z")

    def test_generated_listings_match_oracle(self) -> None:
        rng = random.Random(1030)
        tags = ["0.13.7", "0.13.6", "1.0.0", "01.0.0", "0.13", "v0.13.7", "cargo-nextest-0.9.150", "", None, 7]
        dates = ["2026-10-01T00:00:00Z", "2026-10-29T00:00:00Z", "2026-13-01T00:00:00Z", "2026-02-29T00:00:00Z",
                 "2024-02-29T00:00:00Z", "", None, 0]
        flags = [False, True, None, 0, "false"]
        for n in range(self.ITERATIONS):
            listing = []
            for _ in range(rng.randint(0, 4)):
                release = {}
                for key, values in (("tag_name", tags), ("published_at", dates), ("draft", flags),
                                    ("prerelease", flags)):
                    if rng.random() < 0.9:
                        release[key] = rng.choice(values)
                listing.append(release)
            tool = rng.choice(["uv", "cargo-nextest"])
            raw = json.dumps(listing).encode()
            self.assertEqual(run(tool, raw), expected(tool, raw), f"generated {n}: {raw!r}")


HEX = "0123456789abcdef"


def run_digest(raw: bytes) -> str | type:
    """Reads a digest, mapping a refusal to the ``ToolVersionsError`` class."""
    try:
        return tv.parse_digest(raw)
    except tv.ToolVersionsError:
        return tv.ToolVersionsError


def expected_digest(raw: bytes) -> str | type:
    """Independent oracle for ``parse_digest``, byte by byte."""
    if len(raw) > 256:
        return tv.ToolVersionsError
    if raw[-1:] == b"\n":
        raw = raw[:-1]
    if len(raw) != 71 or raw[:7] != b"sha256:":
        return tv.ToolVersionsError
    for byte in raw[7:]:
        if chr(byte) not in HEX:
            return tv.ToolVersionsError
    return raw.decode("ascii")


def digest_corpus() -> list[tuple[str, bytes]]:
    """Returns ``(name, bytes)`` for each digest corpus file."""
    return [(p.name, p.read_bytes()) for p in sorted(DIGEST_CORPUS.iterdir()) if p.is_file()]


class DigestTests(unittest.TestCase):
    """Replays and mutation-fuzzes the image digest reader."""

    def test_corpus_matches_oracle(self) -> None:
        accepted = {"typical_newline", "no_newline"}
        for name, raw in digest_corpus():
            with self.subTest(case=name):
                got = run_digest(raw)
                self.assertEqual(got, expected_digest(raw))
                self.assertEqual(isinstance(got, str), name in accepted)

    def test_oversized_digest_is_refused(self) -> None:
        self.assertIs(run_digest(b"sha256:" + b"a" * 64 + b"\n" * tv.MAX_DIGEST_BYTES), tv.ToolVersionsError)

    def test_mutations_match_oracle(self) -> None:
        seeds = [raw for _, raw in digest_corpus()]
        rng = random.Random(20261031)
        for n in range(MutationFuzzTests.ITERATIONS):
            raw = bytearray(rng.choice(seeds))
            for _ in range(rng.randint(1, 3)):
                pos = rng.randint(0, len(raw))
                op = rng.random()
                if op < 0.4:
                    raw[pos:pos] = bytes([rng.choice(b"0af9AFgz:\n\r \x00\xc3\xa9")])
                elif op < 0.7:
                    del raw[pos:pos + 1]
                else:
                    raw[pos:pos + 1] = bytes([rng.randint(0, 0xFF)])
            got = run_digest(bytes(raw))
            self.assertEqual(got, expected_digest(bytes(raw)), f"iteration {n}: {bytes(raw)!r}")


class CommandTests(unittest.TestCase):
    """Command-line refusals that need no network."""

    def test_apply_refuses_hand_bumped_tools(self) -> None:
        self.assertEqual(tv.main(["apply", "nodejs", "27"]), 1)


class PinFileTests(unittest.TestCase):
    """``.tool-versions`` parsing, rendering and Dockerfile rewrites."""

    def test_round_trip(self) -> None:
        text = "nodejs 26\nuv 0.13.6\ncargo-nextest 0.9.146\n"
        self.assertEqual(tv.render_pins(tv.parse_pins(text)), text)

    def test_refusals(self) -> None:
        for text in (
            "uv 0.13.6\nnodejs 26\ncargo-nextest 0.9.146\n",  # nodejs must come first
            "# pins\nnodejs 26\nuv 0.13.6\ncargo-nextest 0.9.146\n",
            "nodejs 26\nuv 0.13.6\n",
            "nodejs 26\nuv 0.13.6\ncargo-nextest 0.9.146\npython 3.12\n",
            "nodejs 26\nuv  0.13.6\ncargo-nextest 0.9.146\n",
            "nodejs 26\nuv 0.13\ncargo-nextest 0.9.146\n",
            "nodejs 26.1\nuv 0.13.6\ncargo-nextest 0.9.146\n",
            "nodejs 26\nuv 0.13.6\nuv 0.13.6\ncargo-nextest 0.9.146\n",
        ):
            with self.subTest(text=text), self.assertRaises(tv.ToolVersionsError):
                tv.parse_pins(text)

    def test_rewrite_image(self) -> None:
        old = "sha256:" + "a" * 64
        new = "sha256:" + "b" * 64
        text = (
            f"FROM ghcr.io/astral-sh/uv:0.13.6@{old} AS uv\n"
            f"FROM --platform=$BUILDPLATFORM ghcr.io/astral-sh/uv:0.13.6@{old}\n"
            f"FROM ghcr.io/astral-sh/uvx:0.13.6@{old}\n"
            "# FROM ghcr.io/astral-sh/uv:0.13.6\n"
        )
        self.assertEqual(
            tv.rewrite_image(text, "ghcr.io/astral-sh/uv", "0.13.7", new),
            f"FROM ghcr.io/astral-sh/uv:0.13.7@{new} AS uv\n"
            f"FROM --platform=$BUILDPLATFORM ghcr.io/astral-sh/uv:0.13.7@{new}\n"
            f"FROM ghcr.io/astral-sh/uvx:0.13.6@{old}\n"
            "# FROM ghcr.io/astral-sh/uv:0.13.6\n",
        )

    def test_digest_and_version_refusals(self) -> None:
        for digest in ("sha256:" + "A" * 64, "sha256:" + "a" * 63, "sha512:" + "a" * 64, "sha256:" + "a" * 64 + "\n"):
            with self.subTest(digest=digest):
                self.assertIsNone(tv.DIGEST.fullmatch(digest))
        for version in ("0.13.7\n", "0.13.7 ", "v0.13.7", "0.13.07", "0.13.7;rm"):
            with self.subTest(version=version):
                self.assertIsNone(tv.EXACT_VERSION.fullmatch(version))


if __name__ == "__main__":
    unittest.main()
