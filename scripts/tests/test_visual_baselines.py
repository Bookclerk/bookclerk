#!/usr/bin/env python3
"""Visual baselines: seed corpus replay, mutation fuzzing and the CLI.

``scripts/visual_baselines.py`` reads two untrusted inputs: the
``baselines.lock`` a pull request edits, and the screenshot set in a pull
request run's artifact, which the bot commits with a write token, plus the
``edited`` event of the bot's review comment, whose checkbox approves a set,
and the collaborator role GitHub reports for whoever ticked it. Each corpus
file is named ``<kind>__<case>`` (``lock``, ``png``, ``approval``,
``command``, ``membership``, ``role``, ``changes`` for a proposed set's
commit message, or ``report`` for a comparison run's Playwright report);
every corpus input and every mutation of one must get the same verdict from
the module and from the independent oracle below, which reads bytes one by
one instead of with ``re`` and ``struct``.
"""

from __future__ import annotations

import base64
import io
import json
import os
import random
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts"))

import visual_baselines as vb

CORPUS = Path(__file__).resolve().parent / "visual_baselines_corpus"

HEX = "0123456789abcdef"
LOWER_DIGITS = "abcdefghijklmnopqrstuvwxyz0123456789"


def oracle_lock(raw: bytes) -> dict[str, str] | Exception:
    """Independent reading of a lock file: its entries, or a refusal."""
    if len(raw) > 1024 * 1024:
        return ValueError()
    for b in raw:
        if b > 127:
            return ValueError()
    entries: dict[str, str] = {}
    prev = None
    for line in raw.decode("ascii").split("\n"):
        if line == "":
            continue
        if line[0] == "#":
            if entries:
                return ValueError()
            continue
        space = line.find(" ")
        if space < 0:
            return ValueError()
        path, commit = line[:space], line[space + 1 :]
        if not oracle_path(path) or len(commit) != 40:
            return ValueError()
        for c in commit:
            if c not in HEX:
                return ValueError()
        if prev is not None and path <= prev:
            return ValueError()
        if len(entries) >= 5000:
            return ValueError()
        entries[path] = commit
        prev = path
    return entries


def oracle_changes(raw: bytes) -> list[tuple[str, str]] | Exception:
    """Independent reading of a set commit message's ``Change:`` lines."""
    text = raw.decode("utf-8", "replace")
    if len(text) > 1024 * 1024:
        return ValueError()
    out: list[tuple[str, str]] = []
    for line in text.split("\n"):
        if line[:7] != "Change:":
            continue
        if line[7:8] != " ":
            return ValueError()
        kind, space, path = line[8:].partition(" ")
        if not space or kind not in ("added", "changed", "removed"):
            return ValueError()
        if not oracle_path(path):
            return ValueError()
        if out and path <= out[-1][0]:
            return ValueError()
        if len(out) >= 5000:
            return ValueError()
        out.append((path, kind))
    return out


def oracle_report(raw: bytes) -> int | None:
    """Independent reading of a Playwright JSON report: failed count if finished."""
    if len(raw) > 64 * 1024 * 1024:
        return None
    try:
        report = json.loads(str(raw, "utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError):
        return None
    if type(report) is not dict:
        return None
    errors, stats = report.get("errors"), report.get("stats")
    if type(errors) is not list or len(errors) != 0 or type(stats) is not dict:
        return None
    counts = []
    for key in ("expected", "unexpected"):
        value = stats.get(key)
        if type(value) is not int or value < 0:
            return None
        counts.append(value)
    return counts[1] if counts[0] + counts[1] > 0 else None


def be32(data: bytes, at: int) -> int:
    """Big-endian 32-bit value at ``at``, by hand."""
    return (data[at] << 24) | (data[at + 1] << 16) | (data[at + 2] << 8) | data[at + 3]


def oracle_png(data: bytes) -> tuple[int, int] | Exception:
    """Independent reading of a PNG header: the size, or a refusal."""
    sig = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    if len(data) < 33 or list(data[:8]) != sig:
        return ValueError()
    if be32(data, 8) != 13 or data[12:16] != b"IHDR":
        return ValueError()
    w, h = be32(data, 16), be32(data, 20)
    if w == 0 or h == 0 or w > 16384 or h > 16384:
        return ValueError()
    return (w, h)


def oracle_story_id(s: str) -> bool:
    """Kebab-case id: alphanumeric runs joined by one or two dashes."""
    if not s or s[0] == "-" or s[-1] == "-":
        return False
    run = 0
    for c in s:
        if c == "-":
            run += 1
            if run > 2:
                return False
        elif c in LOWER_DIGITS:
            run = 0
        else:
            return False
    return True


def oracle_path(path: str) -> bool:
    """``<viewport>/<story-id>--<theme>.png``, read piece by piece."""
    viewport, slash, rest = path.partition("/")
    if not slash or viewport not in ("desktop", "phone") or not rest.endswith(".png"):
        return False
    stem = rest[: -len(".png")]
    for theme in ("light", "dark"):
        suffix = "--" + theme
        if stem.endswith(suffix) and oracle_story_id(stem[: -len(suffix)]):
            return True
    return False


BOT = "bookclerk[bot]"


def oracle_approval(raw: bytes) -> tuple | None | Exception:
    """Independent reading of an edit: the approval's fields, ``None``, or a refusal."""
    try:
        e = json.loads(raw)
    except (ValueError, RecursionError):
        return ValueError()
    if not isinstance(e, dict):
        return ValueError()
    if e.get("action") != "edited":
        return None
    c, i, s = e.get("comment"), e.get("issue"), e.get("sender")
    if not (isinstance(c, dict) and isinstance(i, dict) and isinstance(s, dict)):
        return ValueError()
    u = c.get("user")
    if not isinstance(u, dict) or not isinstance(u.get("login"), str):
        return ValueError()
    if u["login"] != BOT or u.get("type") != "Bot":
        return None
    if s.get("type") != "User" or not isinstance(i.get("pull_request"), dict):
        return None
    for value, kind in (
        (s.get("login"), str),
        (i.get("number"), int),
        (c.get("id"), int),
        (c.get("body"), str),
    ):
        if type(value) is not kind:
            return ValueError()
    ch = e.get("changes")
    if not isinstance(ch, dict) or not isinstance(ch.get("body"), dict):
        return None
    old = ch["body"].get("from")
    if type(old) is not str:
        return ValueError()
    new = c["body"]
    prefix = "<!-- bookclerk-visual-baselines v1 head="
    if not new.startswith(prefix) or i["number"] <= 0 or c["id"] <= 0:
        return None
    n = len(prefix)
    head, mid, set_commit, end = (
        new[n : n + 40],
        new[n + 40 : n + 45],
        new[n + 45 : n + 85],
        new[n + 85 : n + 89],
    )
    if (
        mid != " set="
        or end != " -->"
        or any(ch not in HEX for ch in head + set_commit)
    ):
        return None
    box = (
        "] Approve these baselines for `"
        + head[:12]
        + "` (maintainers and visual reviewers)"
    )
    off, on = "- [ " + box, "- [x" + box
    if old.count(off) != 1 or on in old:
        return None
    k = old.index(off)
    if old[:k] + on + old[k + len(off) :] != new:
        return None
    return (i["number"], c["id"], s["login"], head, set_commit)


def oracle_role(raw: bytes) -> str | Exception:
    """Independent reading of a permission answer's role."""
    try:
        e = json.loads(raw)
    except (ValueError, RecursionError):
        return ValueError()
    if not isinstance(e, dict) or type(e.get("role_name")) is not str:
        return ValueError()
    return (
        e["role_name"]
        if e["role_name"] in ("admin", "maintain", "write", "triage", "read", "none")
        else "none"
    )


def oracle_command(raw: bytes) -> tuple | None | Exception:
    """Independent reading of a reply: ``(number, login)``, ``None``, or a refusal."""
    try:
        e = json.loads(raw)
    except (ValueError, RecursionError):
        return ValueError()
    if not isinstance(e, dict):
        return ValueError()
    if e.get("action") != "created":
        return None
    c, i, s = e.get("comment"), e.get("issue"), e.get("sender")
    if not (isinstance(c, dict) and isinstance(i, dict) and isinstance(s, dict)):
        return ValueError()
    if s.get("type") != "User" or not isinstance(i.get("pull_request"), dict):
        return None
    if type(c.get("body")) is not str:
        return ValueError()
    text = c["body"].strip()
    if not text.isascii() or text.lower() != "@bookclerk approve-visuals":
        return None
    if type(i.get("number")) is not int or type(s.get("login")) is not str:
        return ValueError()
    return (i["number"], s["login"]) if i["number"] > 0 else None


def oracle_membership(raw: bytes) -> bool | Exception:
    """Independent reading of a team membership answer."""
    try:
        e = json.loads(raw)
    except (ValueError, RecursionError):
        return ValueError()
    if not isinstance(e, dict) or type(e.get("state")) is not str:
        return ValueError()
    return e["state"] == "active"


def module_approval(raw: bytes):
    """The module's verdict on an edit, as a tuple like the oracle's."""
    a = vb.parse_approval(raw, BOT)
    return (
        None if a is None else (a.number, a.comment_id, a.sender, a.head, a.set_commit)
    )


def corpus() -> list[tuple[str, str, bytes]]:
    """Every corpus file as ``(kind, case, bytes)``."""
    out = []
    for f in sorted(CORPUS.iterdir()):
        kind, _, case = f.name.partition("__")
        out.append((kind, case, f.read_bytes()))
    return out


def verdict(fn, raw):
    """``fn(raw)``, or the exception class it raised."""
    try:
        return fn(raw)
    except vb.VisualBaselinesError:
        return ValueError


def assert_matches(test: unittest.TestCase, kind: str, raw: bytes, msg: str) -> None:
    """The module and the oracle agree on ``raw``."""
    if kind == "lock":
        got, want = verdict(vb.parse_lock, raw), oracle_lock(raw)
    elif kind == "changes":
        got = verdict(lambda r: vb.parse_changes(r.decode("utf-8", "replace")), raw)
        want = oracle_changes(raw)
    elif kind == "report":
        got, want = vb.compare_finished(raw), oracle_report(raw)
    elif kind == "approval":
        got, want = verdict(module_approval, raw), oracle_approval(raw)
    elif kind == "command":
        got, want = (
            verdict(lambda r: vb.parse_command(r, BOT), raw),
            oracle_command(raw),
        )
    elif kind == "membership":
        got, want = verdict(vb.parse_membership, raw), oracle_membership(raw)
    elif kind == "role":
        got, want = verdict(vb.parse_role, raw), oracle_role(raw)
    else:
        got, want = verdict(vb.png_size, raw), oracle_png(raw)
    if isinstance(want, Exception):
        test.assertIs(got, ValueError, msg)
    else:
        test.assertEqual(got, want, msg)


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus against the oracle."""

    def test_corpus_matches_oracle(self) -> None:
        cases = corpus()
        self.assertGreater(len(cases), 10)
        for kind, case, raw in cases:
            with self.subTest(case=f"{kind}__{case}"):
                assert_matches(self, kind, raw, case)

    def test_expected_verdicts(self) -> None:
        load = lambda n: (CORPUS / n).read_bytes()
        a = "0123456789abcdef0123456789abcdef01234567"
        self.assertEqual(vb.parse_lock(load("lock__empty")), {})
        self.assertEqual(vb.parse_lock(load("lock__header_only")), {})
        self.assertEqual(
            vb.parse_lock(load("lock__one")), {"desktop/library--page--light.png": a}
        )
        self.assertEqual(len(vb.parse_lock(load("lock__packed"))), 2)
        for bad in (
            "lock__unsorted",
            "lock__duplicate",
            "lock__comment_after",
            "lock__upper",
            "lock__crlf",
            "lock__utf8",
            "lock__legacy_sha",
            "lock__legacy_none",
            "lock__bad_path",
            "lock__two_spaces",
            "lock__tab",
            "lock__short",
        ):
            with self.subTest(bad=bad), self.assertRaises(vb.VisualBaselinesError):
                vb.parse_lock(load(bad))
        changes = lambda n: vb.parse_changes(load(n).decode())
        self.assertEqual(
            changes("changes__mixed"),
            [
                ("desktop/a--light.png", "changed"),
                ("desktop/b--dark.png", "added"),
                ("phone/c--light.png", "removed"),
            ],
        )
        self.assertEqual(changes("changes__none"), [])
        self.assertEqual(
            changes("changes__other_lines"), [("desktop/a--light.png", "added")]
        )
        for bad in (
            "changes__unknown_kind",
            "changes__bad_path",
            "changes__unsorted",
            "changes__repeated",
            "changes__no_space",
            "changes__trailing",
        ):
            with self.subTest(bad=bad), self.assertRaises(vb.VisualBaselinesError):
                changes(bad)
        for name, failed in (
            ("report__finished", 0),
            ("report__failures", 10),
            ("report__setup_error", None),
            ("report__no_tests", None),
            ("report__bool_counts", None),
            ("report__negative", None),
            ("report__no_errors_key", None),
            ("report__not_object", None),
            ("report__not_json", None),
            ("report__utf16", None),
        ):
            with self.subTest(report=name):
                self.assertEqual(vb.compare_finished(load(name)), failed)
        self.assertEqual(vb.png_size(load("png__valid")), (4, 3))
        self.assertEqual(
            module_approval(load("approval__tick")),
            (990, 7, "maint", "a" * 40, "b" * 40),
        )
        for ignored in (
            "approval__untick",
            "approval__other_author",
            "approval__bot_sender",
            "approval__also_edits_text",
            "approval__marker_swapped",
            "approval__issue_not_pr",
        ):
            with self.subTest(ignored=ignored):
                self.assertIsNone(module_approval(load(ignored)))
        self.assertEqual(vb.parse_role(load("role__maintain")), "maintain")
        self.assertEqual(vb.parse_role(load("role__custom")), "none")
        for bad in (
            "png__too_wide",
            "png__zero_height",
            "png__idat_first",
            "png__jpeg",
            "png__truncated",
        ):
            with self.subTest(bad=bad), self.assertRaises(vb.VisualBaselinesError):
                vb.png_size(load(bad))


class SetTests(unittest.TestCase):
    """Whole-set rules: names, duplicates and limits."""

    PNG = (CORPUS / "png__valid").read_bytes()

    def test_valid_set(self) -> None:
        files = [
            ("desktop/primitives-button--primary--light.png", self.PNG),
            ("phone/primitives-button--primary--dark.png", self.PNG),
        ]
        self.assertEqual(vb.validate_set(files), 2)

    def test_refusals(self) -> None:
        cases = {
            "empty": [],
            "dot dot": [("desktop/../x--light.png", self.PNG)],
            "absolute": [("/desktop/x--light.png", self.PNG)],
            "other folder": [("tablet/x--light.png", self.PNG)],
            "no theme": [("desktop/x.png", self.PNG)],
            "html": [("desktop/x--light.html", self.PNG)],
            "nested": [("desktop/a/x--light.png", self.PNG)],
            "duplicate": [("phone/x--dark.png", self.PNG)] * 2,
            "not png": [("phone/x--dark.png", b"GIF89a" + b"\0" * 40)],
            "too large": [("phone/x--dark.png", self.PNG + b"\0" * vb.MAX_FILE_BYTES)],
        }
        for name, files in cases.items():
            with self.subTest(name=name), self.assertRaises(vb.VisualBaselinesError):
                vb.validate_set(files)

    def test_read_set_refuses_links(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "desktop").mkdir()
            (root / "desktop" / "a--light.png").write_bytes(self.PNG)
            self.assertEqual(vb.validate_set(vb.read_set(root)), 1)
            os.symlink("/etc/passwd", root / "desktop" / "b--light.png")
            with self.assertRaises(vb.VisualBaselinesError):
                vb.read_set(root)


FRAGMENTS = [
    b"[x]",
    b"[ ]",
    b'"User"',
    b'"Bot"',
    b"null",
    b"0",
    b"{}",
    b"[]",
    b"",
    b"\n",
    b"\r\n",
    b"none",
    b"0",
    b"f",
    b"F",
    b"\x00",
    b"\xff",
    b"IHDR",
    b"IDAT",
    b"\x89PNG",
    b"\x00\x00\x00\x0d",
]


def mutate(rng: random.Random, raw: bytes) -> bytes:
    """Flips, inserts or deletes bytes, sometimes splicing known fragments."""
    raw = bytearray(raw)
    for _ in range(rng.randint(1, 4)):
        op = rng.random()
        pos = rng.randint(0, len(raw))
        if op < 0.4 and raw:
            raw[min(pos, len(raw) - 1)] = rng.randint(0, 255)
        elif op < 0.7:
            raw[pos:pos] = rng.choice(FRAGMENTS)
        else:
            del raw[pos : pos + rng.randint(1, 3)]
    return bytes(raw)


def random_path(rng: random.Random) -> str:
    """A path near the accepted shape, often just outside it."""
    alphabet = LOWER_DIGITS + "-_./A "
    story = "".join(
        rng.choice(alphabet if rng.random() < 0.2 else LOWER_DIGITS + "-")
        for _ in range(rng.randint(0, 12))
    )
    viewport = rng.choice(["desktop", "phone", "tablet", "", "desktop/x", ".."])
    theme = rng.choice(["light", "dark", "dim", ""])
    ext = rng.choice([".png", ".PNG", ".png.html", ""])
    return f"{viewport}/{story}--{theme}{ext}" if rng.random() < 0.9 else story


class MutationFuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 6000

    def test_mutations_match_oracle(self) -> None:
        cases = corpus()
        rng = random.Random(20261115)
        for n in range(self.ITERATIONS):
            kind, case, raw = rng.choice(cases)
            mutated = mutate(rng, raw)
            assert_matches(
                self,
                kind,
                mutated,
                f"iteration {n} from {kind}__{case}: {mutated[:80]!r}",
            )

    def test_paths_match_oracle(self) -> None:
        rng = random.Random(990)
        png = (CORPUS / "png__valid").read_bytes()
        for n in range(self.ITERATIONS):
            path = random_path(rng)
            accepted = verdict(vb.validate_set, [(path, png)]) == 1
            self.assertEqual(accepted, oracle_path(path), f"iteration {n}: {path!r}")


class CliTests(unittest.TestCase):
    """The two commands the workflows run."""

    def run_main(self, *argv: str) -> tuple[int, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = vb.main(list(argv))
        return code, out.getvalue()

    def baselines_repo(self, tmp: Path, files: dict[str, bytes], tag: bool) -> str:
        """A local stand-in for the baselines repository with one set commit."""
        repo = tmp / "baselines"
        repo.mkdir()
        env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.invalid",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.invalid",
        }
        git = lambda *a: subprocess.run(
            ["git", *a], cwd=repo, env=env, check=True, capture_output=True, text=True
        ).stdout.strip()
        git("init", "-q")
        git("config", "uploadpack.allowAnySHA1InWant", "true")
        for path, data in files.items():
            (repo / path).parent.mkdir(parents=True, exist_ok=True)
            (repo / path).write_bytes(data)
        git("add", "-A")
        git("commit", "-q", "--no-gpg-sign", "-m", "set")
        commit = git("rev-parse", "HEAD")
        if tag:
            git("tag", f"approved/{commit}")
        return commit

    def test_assemble_fetches_each_locked_image(self) -> None:
        png = (CORPUS / "png__valid").read_bytes()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            commit = self.baselines_repo(
                root, {"desktop/a--light.png": png, "phone/b--dark.png": png}, tag=True
            )
            lock = root / "baselines.lock"
            lock.write_text(vb.render_lock({"desktop/a--light.png": commit}))
            dest = root / "out"
            code, out = self.run_main(
                "assemble", str(lock), str(root / "baselines"), str(dest)
            )
            self.assertEqual((code, out), (0, f"dir={dest}\n"))
            self.assertEqual(vb.read_set(dest), [("desktop/a--light.png", png)])

    def test_assemble_refuses_an_unapproved_commit(self) -> None:
        png = (CORPUS / "png__valid").read_bytes()
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            commit = self.baselines_repo(root, {"desktop/a--light.png": png}, tag=False)
            lock = root / "baselines.lock"
            lock.write_text(vb.render_lock({"desktop/a--light.png": commit}))
            code, _ = self.run_main(
                "assemble", str(lock), str(root / "baselines"), str(root / "out")
            )
            self.assertEqual(code, 1)
            self.assertFalse((root / "out" / "desktop").exists())

    def test_assemble_with_nothing_locked(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp, "out")
            code, printed = self.run_main(
                "assemble", str(CORPUS / "lock__header_only"), "unused", str(out)
            )
            self.assertEqual((code, printed), (0, "dir=\n"))
            self.assertEqual(
                self.run_main(
                    "assemble", str(CORPUS / "lock__legacy_sha"), "u", str(out)
                )[0],
                1,
            )

    def test_approved_commits_need_a_tag_naming_themselves(self) -> None:
        a, b = "a" * 40, "b" * 40
        raw = (
            f"{a}\trefs/tags/approved/{a}\n{a}\trefs/tags/approved/{b}\n"
            f"{b}\trefs/tags/other/{b}\n"
        ).encode()
        self.assertEqual(vb.approved_commits(raw), {a})
        with self.assertRaises(vb.VisualBaselinesError):
            vb.approved_commits(b"\xff")

    def test_committed_lock_parses(self) -> None:
        raw = (REPO / "ui" / "visual" / "baselines.lock").read_bytes()
        lock = vb.parse_lock(raw)
        self.assertGreater(len(lock), 0)
        self.assertEqual(vb.render_lock(lock), raw.decode())

    def test_usage(self) -> None:
        self.assertEqual(self.run_main("nope")[0], 2)


class DiffAndRenderTests(unittest.TestCase):
    """The comment the bot posts and the changes it lists."""

    def test_render_lock_round_trips(self) -> None:
        lock = {"phone/b--dark.png": "b" * 40, "desktop/a--light.png": "a" * 40}
        text = vb.render_lock(lock)
        self.assertEqual(vb.parse_lock(text.encode()), lock)
        self.assertTrue(text.startswith(vb.LOCK_HEADER))
        self.assertIn(f"\ndesktop/a--light.png {'a' * 40}\n\nphone/", text)
        self.assertEqual(vb.render_lock({}), vb.LOCK_HEADER)

    def test_apply_changes_touches_only_the_listed_entries(self) -> None:
        lock = {"desktop/a--light.png": "a" * 40, "desktop/b--light.png": "a" * 40}
        self.assertEqual(
            vb.apply_changes(
                lock,
                "c" * 40,
                [
                    ("desktop/a--light.png", "changed"),
                    ("desktop/b--light.png", "removed"),
                    ("phone/n--dark.png", "added"),
                ],
            ),
            {"desktop/a--light.png": "c" * 40, "phone/n--dark.png": "c" * 40},
        )
        self.assertEqual(lock["desktop/b--light.png"], "a" * 40)

    def test_changes_round_trip_through_the_commit_message(self) -> None:
        changes = [("desktop/a--light.png", "changed"), ("phone/b--dark.png", "added")]
        message = "Proposed baselines\n\n" + vb.render_changes(changes)
        self.assertEqual(vb.parse_changes(message), changes)

    def test_compare_mismatches_reads_failed_screenshots(self) -> None:
        self.assertEqual(
            vb.compare_mismatches(
                [
                    "app-library-page--desktop/library--page--light-actual.png",
                    "app-library-page--desktop/library--page--light-diff.png",
                    "stories-x--phone-retry1/x--story--dark-actual.png",
                    "stories-x--tablet/x--story--dark-actual.png",
                    "nested/dir--phone/x--light-actual.png",
                    "y--phone/Y--light-actual.png",
                ]
            ),
            {"desktop/library--page--light.png", "phone/x--story--dark.png"},
        )

    def test_diff_sets_ignores_byte_noise_the_comparison_passed(self) -> None:
        old = {"desktop/a--light.png": b"1", "desktop/b--light.png": b"2"}
        new = {
            "desktop/a--light.png": b"1x",
            "desktop/b--light.png": b"3",
            "phone/c--dark.png": b"4",
        }
        self.assertEqual(
            vb.diff_sets(old, new, {"desktop/b--light.png"}),
            [("desktop/b--light.png", "changed"), ("phone/c--dark.png", "added")],
        )

    def test_diff_sets(self) -> None:
        old = {"desktop/a--light.png": b"1", "desktop/b--light.png": b"2"}
        new = {
            "desktop/a--light.png": b"1",
            "desktop/b--light.png": b"3",
            "phone/c--dark.png": b"4",
        }
        self.assertEqual(
            vb.diff_sets(old, new),
            [("desktop/b--light.png", "changed"), ("phone/c--dark.png", "added")],
        )
        self.assertEqual(vb.diff_sets(new, old)[-1], ("phone/c--dark.png", "removed"))

    def test_rendered_comment_round_trips_through_a_tick(self) -> None:
        body = vb.render_comment(
            "a" * 40,
            "b" * 40,
            {"desktop/x--light.png": "c" * 40},
            [("desktop/x--light.png", "changed")] * 50,
            "https://r",
            BOT,
        )
        self.assertEqual(body.count("![proposed]"), 50)
        self.assertIn("`@bookclerk approve-visuals`", body)
        event = {
            "action": "edited",
            "comment": {
                "id": 1,
                "user": {"login": BOT, "type": "Bot"},
                "body": body.replace("- [ ]", "- [x]"),
            },
            "issue": {"number": 2, "pull_request": {}},
            "sender": {"login": "m", "type": "User"},
            "changes": {"body": {"from": body}},
        }
        self.assertEqual(
            module_approval(json.dumps(event).encode()), (2, 1, "m", "a" * 40, "b" * 40)
        )


class CategoryCommentTests(unittest.TestCase):
    """``render_comment`` groups changes by kind in one comment."""

    def test_each_category_is_collapsed_with_only_its_columns(self) -> None:
        changes = [
            ("desktop/a--light.png", "changed"),
            ("desktop/b--light.png", "added"),
            ("desktop/c--light.png", "added"),
            ("phone/d--dark.png", "removed"),
        ]
        approved = {"desktop/a--light.png": "c" * 40, "phone/d--dark.png": "d" * 40}
        body = vb.render_comment(
            "a" * 40, "b" * 40, approved, changes, "https://r", BOT
        )
        self.assertIn("differ from the approved baselines", body)
        self.assertIn("1 changed, 2 new, 1 removed", body)
        self.assertEqual(body.count("<details>"), 3)
        self.assertLess(body.index("Changed (1)"), body.index("New (2)"))
        self.assertLess(body.index("New (2)"), body.index("Removed (1)"))
        self.assertIn(
            "| `desktop/a--light` | ![approved](https://r/"
            + "c" * 40
            + "/desktop/a--light.png)"
            " | ![proposed](https://r/" + "b" * 40 + "/desktop/a--light.png) |",
            body,
        )
        self.assertIn("| Screenshot | Proposed |", body)
        self.assertIn(
            "| `desktop/b--light` | ![proposed](https://r/"
            + "b" * 40
            + "/desktop/b--light.png) |",
            body,
        )
        self.assertIn(
            "| `phone/d--dark` | ![approved](https://r/"
            + "d" * 40
            + "/phone/d--dark.png) |",
            body,
        )
        self.assertNotIn("| new |", body)
        self.assertLess(body.index("- [ ]"), body.index("<details>"))

    def test_a_set_too_large_for_one_comment_keeps_every_name(self) -> None:
        changes = [(f"desktop/story-{i:04}--light.png", "added") for i in range(900)]
        body = vb.render_comment("a" * 40, "b" * 40, {}, changes, "https://r" * 5, BOT)
        self.assertLessEqual(len(body), vb.MAX_COMMENT_CHARS)
        for path, _ in changes:
            self.assertEqual(body.count(f"`{path[:-4]}`"), 1)
        self.assertGreater(body.count("![proposed]"), 100)

    def test_images_without_room_move_to_continuation_comments(self) -> None:
        changes = [(f"desktop/c-{i:04}--light.png", "changed") for i in range(150)]
        changes += [(f"phone/n-{i:04}--dark.png", "added") for i in range(600)]
        approved = {path: "c" * 40 for path, _ in changes}
        bodies = vb.render_comments(
            "a" * 40, "b" * 40, approved, changes, "https://raw.example/o/r", BOT
        )
        self.assertGreater(len(bodies), 2)
        self.assertTrue(vb.MARKER.match(bodies[0]))
        for part, page in enumerate(bodies[1:], start=1):
            more = vb.MORE_MARKER.match(page)
            self.assertIsNotNone(more)
            assert more is not None
            self.assertEqual((more.group(1), more.group(2)), ("b" * 40, str(part)))
        for body in bodies:
            self.assertLessEqual(len(body), vb.MAX_COMMENT_CHARS)
        joined = "\n".join(bodies)
        # Every image appears exactly once across the comments.
        for path, change in changes:
            self.assertEqual(joined.count(f"/{'b' * 40}/{path})"), 1)
            if change == "changed":
                self.assertEqual(joined.count(f"/{'c' * 40}/{path})"), 1)
            # Each name is listed once in the review comment.
            self.assertEqual(bodies[0].count(f"`{path[:-4]}`"), 1)
        self.assertIn(
            f"they are in the {len(bodies) - 1} comment(s) that follow", bodies[0]
        )

    def test_a_set_that_fits_has_no_continuation(self) -> None:
        bodies = vb.render_comments(
            "a" * 40,
            "b" * 40,
            {},
            [("desktop/x--light.png", "added")],
            "https://r",
            BOT,
        )
        self.assertEqual(len(bodies), 1)
        self.assertNotIn("no room", bodies[0])


X = "desktop/x--light.png"


def lock_of(commit: str | None, *paths: str) -> bytes:
    """A lock with ``paths`` (``X`` by default) locked to ``commit``, or none."""
    if commit is None:
        return vb.render_lock({}).encode()
    return vb.render_lock({p: commit for p in paths or (X,)}).encode()


#: A proposed set's commit as GitHub returns it: it adds ``X``.
SET_MESSAGE = {"message": f"Proposed baselines\n\nChange: added {X}\n"}

#: The lock on the branch an approval writes to: another screenshot locked.
BRANCH_LOCK = {
    "sha": "f" * 40,
    "content": base64.b64encode(lock_of("a" * 40, "phone/kept--dark.png")).decode(),
}


class FakeGitHub:
    """Records requests and answers them from a table of ``(method, path)``."""

    def __init__(self, answers: dict[tuple[str, str], tuple[int, object]]) -> None:
        self.answers = answers
        self.sent: list[tuple[str, str, object]] = []

    def __call__(self, method: str, path: str, body: object) -> tuple[int, bytes]:
        self.sent.append((method, path, body))
        status, payload = self.answers.get((method, path), (404, {}))
        return status, json.dumps(payload).encode()


class BotTests(unittest.TestCase):
    """``visual_baselines_bot`` over fake GitHub answers and a stubbed git."""

    def setUp(self) -> None:
        import visual_baselines_bot as bot

        self.bot = bot
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.env = {
            "GITHUB_REPOSITORY": "Bookclerk/bookclerk",
            "BASELINES_REPOSITORY": "Bookclerk/bookclerk-visual-baselines",
            "BASELINES_TOKEN": "t",
            "APP_SLUG": "bookclerk",
        }

    def event(self, payload: object) -> None:
        path = self.root / "event.json"
        path.write_text(json.dumps(payload))
        self.env["GITHUB_EVENT_PATH"] = str(path)

    def approval_event(self, sender: str = "maint") -> str:
        body = vb.render_comment(
            "a" * 40,
            "b" * 40,
            {},
            [("desktop/x--light.png", "added")],
            "https://r",
            BOT,
        )
        self.event(
            {
                "action": "edited",
                "comment": {
                    "id": 7,
                    "user": {"login": BOT, "type": "Bot"},
                    "body": body.replace("- [ ]", "- [x]"),
                },
                "issue": {"number": 990, "pull_request": {}},
                "sender": {"login": sender, "type": "User"},
                "changes": {"body": {"from": body}},
            }
        )
        return body

    def test_propose_posts_a_comment_with_the_pushed_set(self) -> None:
        sha = "d" * 40
        self.event(
            {
                "workflow_run": {
                    "event": "pull_request",
                    "head_sha": sha,
                    "head_branch": "b",
                    "head_repository": {"id": 5, "owner": {"login": "Bookclerk"}},
                }
            }
        )
        art = self.root / "artifact"
        (art / "baselines" / "desktop").mkdir(parents=True)
        (art / "baselines" / "desktop" / "x--light.png").write_bytes(
            (CORPUS / "png__valid").read_bytes()
        )
        (art / "record-complete").write_text("")
        self.env["ARTIFACT_DIR"] = str(art)
        repo = "repos/Bookclerk/bookclerk"
        lock = base64.b64encode(lock_of(None)).decode()
        send = FakeGitHub(
            {
                ("GET", f"{repo}/commits/{sha}/pulls?per_page=100"): (
                    200,
                    [
                        {
                            "number": 990,
                            "state": "open",
                            "head": {"sha": sha, "repo": {"id": 5}},
                        }
                    ],
                ),
                ("GET", f"{repo}/pulls?state=open&head=Bookclerk:b&per_page=100"): (
                    200,
                    [],
                ),
                (
                    "GET",
                    f"{repo}/contents/ui/visual/baselines.lock?ref=refs/pull/990/merge",
                ): (
                    200,
                    {"content": lock},
                ),
                ("GET", f"{repo}/issues/990/comments?per_page=100&page=1"): (200, []),
            }
        )
        write = FakeGitHub(
            {
                ("POST", f"{repo}/issues/990/comments"): (201, {}),
                ("POST", f"{repo}/pulls/990/requested_reviewers"): (201, {}),
            }
        )
        pushed = []
        with unittest.mock.patch.object(
            self.bot,
            "push_set",
            lambda r, f, ref, m, t: pushed.append((r, sorted(f), ref)) or "e" * 40,
        ):
            out = self.bot.propose(self.env, send, write)
        self.assertIn("proposed", out)
        self.assertEqual(
            pushed,
            [
                (
                    "Bookclerk/bookclerk-visual-baselines",
                    ["desktop/x--light.png"],
                    "refs/heads/pending/pr-990",
                )
            ],
        )
        self.assertIn("set=" + "e" * 40, write.sent[0][2]["body"])
        self.assertEqual(
            write.sent[1],
            (
                "POST",
                f"{repo}/pulls/990/requested_reviewers",
                {"team_reviewers": ["visual-reviewers"]},
            ),
        )
        self.assertNotIn("refused", out)

    def test_propose_ignores_a_lock_that_was_never_approved(self) -> None:
        sha = "d" * 40
        self.event(
            {
                "workflow_run": {
                    "event": "pull_request",
                    "head_sha": sha,
                    "head_branch": "b",
                    "head_repository": {"id": 5, "owner": {"login": "Bookclerk"}},
                }
            }
        )
        art = self.root / "artifact"
        (art / "baselines" / "desktop").mkdir(parents=True)
        (art / "baselines" / "desktop" / "x--light.png").write_bytes(
            (CORPUS / "png__valid").read_bytes()
        )
        (art / "record-complete").write_text("")
        self.env["ARTIFACT_DIR"] = str(art)
        repo = "repos/Bookclerk/bookclerk"
        lock = base64.b64encode(lock_of("f" * 40)).decode()
        send = FakeGitHub(
            {
                ("GET", f"{repo}/commits/{sha}/pulls?per_page=100"): (
                    200,
                    [
                        {
                            "number": 990,
                            "state": "open",
                            "head": {"sha": sha, "repo": {"id": 5}},
                        }
                    ],
                ),
                ("GET", f"{repo}/pulls?state=open&head=Bookclerk:b&per_page=100"): (
                    200,
                    [],
                ),
                (
                    "GET",
                    f"{repo}/contents/ui/visual/baselines.lock?ref=refs/pull/990/merge",
                ): (
                    200,
                    {"content": lock},
                ),
                ("GET", f"{repo}/issues/990/comments?per_page=100&page=1"): (200, []),
                (
                    "GET",
                    "repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/"
                    + "f" * 40,
                ): (404, {}),
            }
        )
        write = FakeGitHub(
            {
                ("POST", f"{repo}/issues/990/comments"): (201, {}),
                ("POST", f"{repo}/pulls/990/requested_reviewers"): (201, {}),
            }
        )
        pushed = []
        with (
            unittest.mock.patch.object(
                self.bot,
                "push_set",
                lambda r, f, ref, m, t: pushed.append((r, sorted(f), ref)) or "e" * 40,
            ),
            unittest.mock.patch.object(
                self.bot, "fetch_set", side_effect=AssertionError("fetched")
            ),
        ):
            out = self.bot.propose(self.env, send, write)
        self.assertIn("proposed", out)
        self.assertEqual(
            pushed,
            [
                (
                    "Bookclerk/bookclerk-visual-baselines",
                    ["desktop/x--light.png"],
                    "refs/heads/pending/pr-990",
                )
            ],
        )
        self.assertIn("set=" + "e" * 40, write.sent[0][2]["body"])
        self.assertEqual(
            write.sent[1],
            (
                "POST",
                f"{repo}/pulls/990/requested_reviewers",
                {"team_reviewers": ["visual-reviewers"]},
            ),
        )
        self.assertNotIn("refused", out)
        self.assertIn("New (1)", write.sent[0][2]["body"])

    def propose_with(
        self,
        lock: bytes,
        extra: dict,
        comments: list,
        sets: dict | None = None,
        shown_changes: list | None = None,
        compare: tuple[bytes, list[str]] | None = None,
    ) -> tuple[str, FakeGitHub, list]:
        """Runs ``propose`` for #990 at head ``d...`` with one recorded image.

        ``sets`` maps a set commit to its images (one valid ``X`` by
        default); ``shown_changes`` is what the open comment's set commit
        message lists (adding ``X`` by default); ``compare`` puts a
        comparison report and failure image names in the artifact, run
        against ``lock``.
        """
        sha = "d" * 40
        self.event(
            {
                "workflow_run": {
                    "event": "pull_request",
                    "head_sha": sha,
                    "head_branch": "b",
                    "head_repository": {"id": 5, "owner": {"login": "Bookclerk"}},
                }
            }
        )
        art = self.root / "artifact"
        (art / "baselines" / "desktop").mkdir(parents=True, exist_ok=True)
        (art / "baselines" / "desktop" / "x--light.png").write_bytes(
            (CORPUS / "png__valid").read_bytes()
        )
        (art / "record-complete").write_text("")
        if compare is not None:
            report, names = compare
            (art / "compare" / "test-results").mkdir(parents=True)
            (art / "compare" / "results.json").write_bytes(report)
            for name in names:
                f = art / "compare" / "test-results" / name
                f.parent.mkdir(parents=True, exist_ok=True)
                f.write_bytes(b"png")
            (art / "compared.lock").write_bytes(lock)
        self.env["ARTIFACT_DIR"] = str(art)
        repo = "repos/Bookclerk/bookclerk"
        answers = {
            ("GET", f"{repo}/commits/{sha}/pulls?per_page=100"): (
                200,
                [
                    {
                        "number": 990,
                        "state": "open",
                        "head": {"sha": sha, "repo": {"id": 5}},
                    }
                ],
            ),
            ("GET", f"{repo}/pulls?state=open&head=Bookclerk:b&per_page=100"): (
                200,
                [],
            ),
            (
                "GET",
                f"{repo}/contents/ui/visual/baselines.lock?ref=refs/pull/990/merge",
            ): (
                200,
                {"content": base64.b64encode(lock).decode()},
            ),
            ("GET", f"{repo}/issues/990/comments?per_page=100&page=1"): (200, comments),
            **extra,
        }
        write = FakeGitHub(
            {
                ("POST", f"{repo}/issues/990/comments"): (201, {}),
                ("PATCH", f"{repo}/issues/comments/7"): (200, {}),
                ("DELETE", f"{repo}/issues/comments/8"): (204, {}),
                ("PATCH", f"{repo}/issues/comments/10"): (200, {}),
                ("POST", f"{repo}/pulls/990/requested_reviewers"): (201, {}),
            }
        )
        pushed: list = []
        self.pushed_sets: list = []
        image = {X: (CORPUS / "png__valid").read_bytes()}

        def approved_images(_: str, entries: dict) -> dict:
            out = {}
            for path, commit in entries.items():
                held = (sets or {}).get(commit, image)
                if path in held:
                    out[path] = held[path]
            return out

        def push(r: str, files: dict, ref: str, message: str, t: str) -> str:
            pushed.append(ref)
            self.pushed_sets.append((sorted(files), message))
            return "e" * 40

        with (
            unittest.mock.patch.object(self.bot, "push_set", push),
            unittest.mock.patch.object(
                self.bot, "fetch_set", lambda r, c, t: dict((sets or {}).get(c, image))
            ),
            unittest.mock.patch.object(self.bot, "approved_images", approved_images),
            unittest.mock.patch.object(
                self.bot,
                "set_changes",
                lambda s, b, c: (
                    [(X, "added")] if shown_changes is None else shown_changes
                ),
            ),
        ):
            out = self.bot.propose(self.env, FakeGitHub(answers), write)
        return out, write, pushed

    def compared_with(self, head: bytes, base: bytes, tagged: set[str]) -> FakeGitHub:
        """GitHub answers for #990 with no merge ref, ``head`` on the branch
        and ``base`` on main, and ``approved`` tags for ``tagged``."""
        repo = "repos/Bookclerk/bookclerk"
        answers: dict = {
            ("GET", f"{repo}/contents/ui/visual/baselines.lock?ref={'d' * 40}"): (
                200,
                {"content": base64.b64encode(head).decode()},
            ),
            ("GET", f"{repo}/pulls/990"): (200, {"base": {"ref": "main"}}),
            ("GET", f"{repo}/contents/ui/visual/baselines.lock?ref=main"): (
                200,
                {"content": base64.b64encode(base).decode()},
            ),
        }
        for commit in tagged:
            answers[("GET", f"repos/B/v/git/ref/tags/approved/{commit}")] = (
                200,
                {"object": {"sha": commit}},
            )
        return FakeGitHub(answers)

    def test_propose_without_a_merge_ref_falls_back_to_the_base_lock(self) -> None:
        """A conflicting branch that approved nothing compares with its base's lock."""
        repo = "repos/Bookclerk/bookclerk"
        approved = "a" * 40
        send = self.compared_with(lock_of(None), lock_of(approved), {approved})
        self.assertEqual(
            self.bot.compared_lock(send, "Bookclerk/bookclerk", "B/v", 990, "d" * 40),
            {X: approved},
        )
        self.assertEqual(
            [p for _, p, _ in send.sent if "baselines.lock" in p],
            [
                f"{repo}/contents/ui/visual/baselines.lock?ref=refs/pull/990/merge",
                f"{repo}/contents/ui/visual/baselines.lock?ref=main",
                f"{repo}/contents/ui/visual/baselines.lock?ref={'d' * 40}",
            ],
        )

    def test_propose_without_a_merge_ref_keeps_the_heads_approved_entries(self) -> None:
        """Screenshots approved on the pull request itself stay its comparison."""
        older, mine, y = "a" * 40, "b" * 40, "phone/y--dark.png"
        send = self.compared_with(
            lock_of(mine),
            vb.render_lock({X: older, y: older}).encode(),
            {older, mine},
        )
        self.assertEqual(
            self.bot.compared_lock(send, "Bookclerk/bookclerk", "B/v", 990, "d" * 40),
            {X: mine, y: older},
        )

    def test_propose_without_a_merge_ref_skips_an_unreadable_head_lock(self) -> None:
        """A branch still on the old one-line lock compares with its base's."""
        approved = "a" * 40
        send = self.compared_with(b"b" * 40 + b"\n", lock_of(approved), {approved})
        self.assertEqual(
            self.bot.compared_lock(send, "Bookclerk/bookclerk", "B/v", 990, "d" * 40),
            {X: approved},
        )

    def test_propose_without_a_merge_ref_or_any_approved_set(self) -> None:
        """No approved entry anywhere: every image is new."""
        send = self.compared_with(lock_of("f" * 40), lock_of(None), set())
        self.assertEqual(
            self.bot.compared_lock(send, "Bookclerk/bookclerk", "B/v", 990, "d" * 40),
            {},
        )

    def test_propose_is_quiet_when_the_base_already_approved_the_images(self) -> None:
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        out, write, pushed = self.propose_with(
            lock_of(approved), {tag: (200, {"object": {"sha": approved}})}, []
        )
        self.assertIn("matches the approved baselines", out)
        self.assertEqual((write.sent, pushed), ([], []))

    def test_no_changes_left_clears_the_open_comment_and_its_continuations(
        self,
    ) -> None:
        approved, pending = "a" * 40, "9" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={pending} -->\n- [ ] x"
        more = f"<!-- bookclerk-visual-baselines-more v1 set={pending} part=1 -->\nx"
        out, write, pushed = self.propose_with(
            lock_of(approved),
            {tag: (200, {"object": {"sha": approved}})},
            [
                {"id": 7, "user": {"login": BOT}, "body": body},
                {"id": 8, "user": {"login": BOT}, "body": more},
            ],
        )
        self.assertIn("matches the approved baselines", out)
        self.assertEqual(pushed, [])
        self.assertEqual(
            [(m, p.rsplit("/", 1)[-1]) for m, p, _ in write.sent],
            [("PATCH", "7"), ("DELETE", "8")],
        )
        self.assertIn("No visual changes left", write.sent[0][2]["body"])

    def test_propose_keeps_the_comment_that_approved_the_set(self) -> None:
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={approved} -->\n> Approved"
        out, write, _ = self.propose_with(
            lock_of(approved),
            {tag: (200, {"object": {"sha": approved}})},
            [{"id": 7, "user": {"login": BOT}, "body": body}],
        )
        self.assertIn("matches", out)
        self.assertEqual(write.sent, [])

    def test_propose_keeps_a_pending_set_a_rerun_recorded_again(self) -> None:
        pending = "e" * 40
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={pending} -->\n- [ ] x"
        out, write, pushed = self.propose_with(
            lock_of(None), {}, [{"id": 7, "user": {"login": BOT}, "body": body}]
        )
        self.assertIn("still pending", out)
        self.assertEqual((write.sent, pushed), ([], []))

    def test_propose_updates_the_open_comment_for_new_images(self) -> None:
        pending = "9" * 40
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={pending} -->\n- [ ] x"
        out, write, pushed = self.propose_with(
            lock_of(None),
            {},
            [
                {"id": 7, "user": {"login": BOT}, "body": body},
                {"id": 9, "user": {"login": "someone"}, "body": body},
            ],
            {pending: {"desktop/x--light.png": b"older"}},
        )
        self.assertIn("proposed", out)
        self.assertEqual(pushed, ["refs/heads/pending/pr-990"])
        self.assertEqual(
            write.sent[0][:2], ("PATCH", "repos/Bookclerk/bookclerk/issues/comments/7")
        )

    def test_images_that_do_not_fit_move_to_continuation_comments(self) -> None:
        """The open comment's old continuations are reused in order, extras deleted."""
        pending, other = "9" * 40, "8" * 40
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={pending} -->\n- [ ] x"

        def more(set_commit: str, part: int) -> str:
            return f"<!-- bookclerk-visual-baselines-more v1 set={set_commit} part={part} -->\nx"

        with unittest.mock.patch.object(vb, "MAX_COMMENT_CHARS", 700):
            out, write, _ = self.propose_with(
                lock_of(None),
                {},
                [
                    {"id": 7, "user": {"login": BOT}, "body": body},
                    {"id": 8, "user": {"login": BOT}, "body": more(pending, 2)},
                    {"id": 10, "user": {"login": BOT}, "body": more(pending, 1)},
                    {"id": 11, "user": {"login": BOT}, "body": more(other, 1)},
                    {"id": 12, "user": {"login": "someone"}, "body": more(pending, 3)},
                ],
                {pending: {"desktop/x--light.png": b"older"}},
            )
        self.assertIn("proposed", out)
        sent = [(m, p.rsplit("/", 1)[-1]) for m, p, _ in write.sent]
        self.assertEqual(
            sent,
            [
                ("PATCH", "7"),
                ("PATCH", "10"),
                ("DELETE", "8"),
                ("POST", "requested_reviewers"),
            ],
        )
        review, page = write.sent[0][2]["body"], write.sent[1][2]["body"]
        self.assertIn("1 row(s) below have no room for their images here", review)
        self.assertNotIn("![proposed]", review)
        self.assertTrue(
            page.startswith(
                f"<!-- bookclerk-visual-baselines-more v1 set={'e' * 40} part=1 -->"
            )
        )
        self.assertIn("/" + "e" * 40 + "/desktop/x--light.png)", page)

    def test_changes_after_an_approval_go_in_a_new_comment(self) -> None:
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={approved} -->\n> Approved"
        out, write, pushed = self.propose_with(
            lock_of(approved, X, "phone/y--dark.png"),
            {tag: (200, {"object": {"sha": approved}})},
            [{"id": 7, "user": {"login": BOT}, "body": body}],
            {
                approved: {
                    "desktop/x--light.png": b"approved",
                    "phone/y--dark.png": b"kept",
                }
            },
        )
        self.assertIn("proposed", out)
        self.assertEqual(pushed, ["refs/heads/pending/pr-990"])
        method, path, sent = write.sent[0]
        self.assertEqual(
            (method, path), ("POST", "repos/Bookclerk/bookclerk/issues/990/comments")
        )
        # Only what changed since the approval needs approving again.
        self.assertIn("1 changed, 1 removed", sent["body"])

    def noisy(self, compare: tuple[bytes, list[str]] | None):
        """Proposes over an approved ``X`` whose bytes differ from the recording."""
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        return self.propose_with(
            lock_of(approved),
            {tag: (200, {"object": {"sha": approved}})},
            [],
            {approved: {X: b"same pixels, other bytes"}},
            compare=compare,
        )

    REPORT = (CORPUS / "report__finished").read_bytes()

    def test_byte_noise_the_comparison_passed_is_not_proposed(self) -> None:
        out, write, pushed = self.noisy((self.REPORT, []))
        self.assertIn("matches the approved baselines", out)
        self.assertEqual((write.sent, pushed), ([], []))

    def test_a_screenshot_the_comparison_failed_is_proposed(self) -> None:
        failed = ["app-x--desktop/x--light-actual.png"]
        out, write, _ = self.noisy((self.REPORT, failed))
        self.assertIn("proposed", out)
        self.assertIn("Changed (1)", write.sent[0][2]["body"])

    def test_an_unfinished_comparison_counts_every_byte(self) -> None:
        report = (CORPUS / "report__setup_error").read_bytes()
        out, _, pushed = self.noisy((report, []))
        self.assertIn("proposed", out)
        self.assertEqual(pushed, ["refs/heads/pending/pr-990"])

    def test_a_test_that_failed_before_its_screenshot_counts_every_byte(
        self,
    ) -> None:
        report = json.dumps(
            {"errors": [], "stats": {"expected": 3, "unexpected": 1}}
        ).encode()
        out, _, pushed = self.noisy((report, []))
        self.assertIn("proposed", out)
        self.assertEqual(pushed, ["refs/heads/pending/pr-990"])

    def test_fetch_set_reads_a_removals_only_set_as_empty(self) -> None:
        calls: list = []

        def fake_git(args: list, cwd: Path, env: dict) -> str:
            calls.append(args[0])
            return ""

        with unittest.mock.patch.object(self.bot, "git", fake_git):
            self.assertEqual(self.bot.fetch_set("B/v", "e" * 40, "t"), {})
        self.assertNotIn("--work-tree", calls)

    def test_no_comparison_counts_every_byte(self) -> None:
        out, _, _ = self.noisy(None)
        self.assertIn("proposed", out)

    def test_a_comparison_against_another_lock_counts_every_byte(self) -> None:
        self.assertIsNone(self.bot.run_mismatches(self.root / "missing", {X: "a" * 40}))
        art = self.root / "art"
        (art / "compare" / "test-results").mkdir(parents=True)
        (art / "compare" / "results.json").write_bytes(self.REPORT)
        (art / "compared.lock").write_bytes(lock_of("b" * 40))
        self.assertIsNone(self.bot.run_mismatches(art, {X: "a" * 40}))
        self.assertEqual(self.bot.run_mismatches(art, {X: "b" * 40}), set())

    def test_the_pending_set_holds_only_new_images_and_lists_every_change(self) -> None:
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        out, _, _ = self.propose_with(
            lock_of(approved, X, "phone/gone--dark.png", "phone/same--dark.png"),
            {tag: (200, {"object": {"sha": approved}})},
            [],
            {
                approved: {
                    X: b"old",
                    "phone/gone--dark.png": b"g",
                    "phone/same--dark.png": b"s",
                }
            },
        )
        self.assertIn("proposed", out)
        # The recording holds X only, so the other two read as removed.
        files, message = self.pushed_sets[0]
        self.assertEqual(files, [X])
        self.assertEqual(
            vb.parse_changes(message),
            [
                (X, "changed"),
                ("phone/gone--dark.png", "removed"),
                ("phone/same--dark.png", "removed"),
            ],
        )

    def test_a_rerun_with_other_changes_proposes_again(self) -> None:
        pending = "e" * 40
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={pending} -->\n- [ ] x"
        out, _, pushed = self.propose_with(
            lock_of(None),
            {},
            [{"id": 7, "user": {"login": BOT}, "body": body}],
            shown_changes=[(X, "changed")],
        )
        self.assertIn("proposed", out)
        self.assertEqual(pushed, ["refs/heads/pending/pr-990"])

    def test_approve_refuses_a_branch_lock_it_cannot_read(self) -> None:
        global BRANCH_LOCK
        saved = BRANCH_LOCK
        BRANCH_LOCK = {"sha": "f" * 40, "content": base64.b64encode(b"a" * 40).decode()}
        try:
            # approve_with builds its answers from the module-level lock.
            out, write = self.approve_with("a" * 40, "b" * 40)
        finally:
            BRANCH_LOCK = saved
        self.assertIn("cannot be read", out)
        self.assertNotIn("PUT", [m for m, _, _ in write.sent])

    def test_propose_skips_an_incomplete_set(self) -> None:
        self.env["ARTIFACT_DIR"] = str(self.root)
        self.assertEqual(
            self.bot.propose(self.env, FakeGitHub({}), FakeGitHub({})),
            "no complete recorded set in this run",
        )

    def test_approve_refuses_a_writer_and_unticks(self) -> None:
        body = self.approval_event()
        repo = "repos/Bookclerk/bookclerk"
        send = FakeGitHub(
            {
                ("GET", f"{repo}/collaborators/maint/permission"): (
                    200,
                    {"role_name": "write"},
                )
            }
        )
        write = FakeGitHub({("PATCH", f"{repo}/issues/comments/7"): (200, {})})
        out = self.bot.approve(self.env, send, write, FakeGitHub({}))
        self.assertIn("Not approved", out)
        self.assertEqual(
            write.sent[0][:2],
            ("GET", "orgs/Bookclerk/teams/visual-reviewers/memberships/maint"),
        )
        self.assertTrue(write.sent[-1][2]["body"].startswith(body))
        self.assertNotIn("- [x]", write.sent[-1][2]["body"])

    def test_a_visual_reviewer_without_write_approves_by_reply(self) -> None:
        body = vb.render_comment(
            "a" * 40,
            "b" * 40,
            {},
            [("desktop/x--light.png", "added")],
            "https://r",
            BOT,
        )
        self.event(
            {
                "action": "created",
                "comment": {
                    "id": 9,
                    "user": {"login": "designer", "type": "User"},
                    "body": " @Bookclerk Approve-Visuals \n",
                },
                "issue": {"number": 990, "pull_request": {}},
                "sender": {"login": "designer", "type": "User"},
            }
        )
        repo, base = (
            "repos/Bookclerk/bookclerk",
            "repos/Bookclerk/bookclerk-visual-baselines",
        )
        send = FakeGitHub(
            {
                ("GET", f"{repo}/issues/990/comments?per_page=100&page=1"): (
                    200,
                    [{"id": 7, "user": {"login": BOT}, "body": body}],
                ),
                ("GET", f"{repo}/issues/comments/7"): (200, {"body": body}),
                ("GET", f"{repo}/collaborators/designer/permission"): (
                    200,
                    {"role_name": "read"},
                ),
                ("GET", f"{repo}/pulls/990"): (
                    200,
                    {
                        "state": "open",
                        "head": {
                            "sha": "a" * 40,
                            "ref": "x",
                            "repo": {"full_name": "fork/bookclerk"},
                        },
                    },
                ),
            }
        )
        write = FakeGitHub(
            {
                ("GET", "orgs/Bookclerk/teams/visual-reviewers/memberships/designer"): (
                    200,
                    {"state": "active"},
                ),
                ("PATCH", f"{repo}/issues/comments/7"): (200, {}),
            }
        )
        baselines = FakeGitHub(
            {
                ("GET", f"{base}/git/ref/tags/approved/" + "b" * 40): (
                    200,
                    {"object": {"sha": "b" * 40}},
                ),
                ("GET", f"{base}/git/commits/" + "b" * 40): (200, SET_MESSAGE),
                ("POST", f"{base}/git/refs"): (201, {}),
            }
        )
        out = self.bot.approve(self.env, send, write, baselines)
        self.assertIn("Approved by designer", out)
        self.assertIn("b" * 40, out)  # a fork: the author commits the lock
        self.assertNotIn("PUT", [m for m, _, _ in write.sent])

    def test_a_pending_invitation_does_not_approve(self) -> None:
        self.approval_event("invitee")
        repo = "repos/Bookclerk/bookclerk"
        send = FakeGitHub(
            {
                ("GET", f"{repo}/collaborators/invitee/permission"): (
                    200,
                    {"role_name": "none"},
                )
            }
        )
        write = FakeGitHub(
            {
                ("GET", "orgs/Bookclerk/teams/visual-reviewers/memberships/invitee"): (
                    200,
                    {"state": "pending"},
                ),
                ("PATCH", f"{repo}/issues/comments/7"): (200, {}),
            }
        )
        self.assertIn(
            "not in the visual-reviewers team",
            self.bot.approve(self.env, send, write, FakeGitHub({})),
        )

    def test_approve_by_a_maintainer_tags_and_writes_the_lock(self) -> None:
        self.approval_event()
        repo, base = (
            "repos/Bookclerk/bookclerk",
            "repos/Bookclerk/bookclerk-visual-baselines",
        )
        send = FakeGitHub(
            {
                ("GET", f"{repo}/collaborators/maint/permission"): (
                    200,
                    {"role_name": "maintain"},
                ),
                ("GET", f"{repo}/pulls/990"): (
                    200,
                    {
                        "state": "open",
                        "head": {
                            "sha": "a" * 40,
                            "ref": "feature",
                            "repo": {"full_name": "Bookclerk/bookclerk"},
                        },
                    },
                ),
                ("GET", f"{repo}/contents/ui/visual/baselines.lock?ref=feature"): (
                    200,
                    BRANCH_LOCK,
                ),
            }
        )
        baselines = FakeGitHub(
            {
                ("GET", f"{base}/git/ref/tags/approved/" + "b" * 40): (
                    200,
                    {"object": {"sha": "b" * 40}},
                ),
                ("GET", f"{base}/git/commits/" + "b" * 40): (200, SET_MESSAGE),
                ("POST", f"{base}/git/refs"): (201, {}),
            }
        )
        write = FakeGitHub(
            {
                ("PUT", f"{repo}/contents/ui/visual/baselines.lock"): (200, {}),
                ("PATCH", f"{repo}/issues/comments/7"): (200, {}),
            }
        )
        out = self.bot.approve(self.env, send, write, baselines)
        self.assertIn("Approved by maint", out)
        self.assertEqual(
            baselines.sent[0][2],
            {"ref": "refs/tags/approved/" + "b" * 40, "sha": "b" * 40},
        )
        put = write.sent[0][2]
        self.assertEqual(
            vb.parse_lock(base64.b64decode(put["content"])),
            {X: "b" * 40, "phone/kept--dark.png": "a" * 40},
        )
        self.assertEqual(put["sha"], "f" * 40)
        self.assertEqual(put["branch"], "feature")
        self.assertIn("1 screenshot(s) locked to `bbbbbbbbbbbb`", out)

    def approve_with(self, head: str, pending: str) -> tuple[str, FakeGitHub]:
        self.approval_event()
        repo, base = (
            "repos/Bookclerk/bookclerk",
            "repos/Bookclerk/bookclerk-visual-baselines",
        )
        send = FakeGitHub(
            {
                ("GET", f"{repo}/collaborators/maint/permission"): (
                    200,
                    {"role_name": "admin"},
                ),
                ("GET", f"{repo}/pulls/990"): (
                    200,
                    {
                        "state": "open",
                        "head": {
                            "sha": head,
                            "ref": "feature",
                            "repo": {"full_name": "Bookclerk/bookclerk"},
                        },
                    },
                ),
                ("GET", f"{repo}/contents/ui/visual/baselines.lock?ref=feature"): (
                    200,
                    BRANCH_LOCK,
                ),
            }
        )
        baselines = FakeGitHub(
            {
                ("GET", f"{base}/git/ref/tags/approved/" + pending): (
                    200,
                    {"object": {"sha": pending}},
                ),
                ("GET", f"{base}/git/commits/" + pending): (200, SET_MESSAGE),
                ("POST", f"{base}/git/refs"): (201, {}),
            }
        )
        write = FakeGitHub(
            {
                ("PUT", f"{repo}/contents/ui/visual/baselines.lock"): (200, {}),
                ("PATCH", f"{repo}/issues/comments/7"): (200, {}),
            }
        )
        return self.bot.approve(self.env, send, write, baselines), write

    def test_approve_after_a_push_still_approves_the_shown_set(self) -> None:
        out, write = self.approve_with("9" * 40, "b" * 40)
        self.assertIn("Approved by maint", out)
        self.assertEqual(write.sent[0][0], "PUT")

    def test_approve_refuses_a_set_missing_from_the_baselines(self) -> None:
        out, write = self.approve_with("a" * 40, "c" * 40)
        self.assertIn("no longer in the baselines repository", out)
        self.assertNotIn("PUT", [m for m, _, _ in write.sent])
        self.assertNotIn("- [x]", write.sent[-1][2]["body"])

    def test_git_env_keeps_the_token_off_argv(self) -> None:
        env = self.bot.git_env("secret-token")
        self.assertNotIn("secret-token", env["GIT_CONFIG_VALUE_0"])
        self.assertEqual(
            base64.b64decode(env["GIT_CONFIG_VALUE_0"].split()[-1]),
            b"x-access-token:secret-token",
        )


if __name__ == "__main__":
    unittest.main()
