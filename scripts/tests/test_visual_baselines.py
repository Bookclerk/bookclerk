#!/usr/bin/env python3
"""Visual baselines: seed corpus replay, mutation fuzzing and the CLI.

``scripts/visual_baselines.py`` reads two untrusted inputs: the
``baselines.lock`` a pull request edits, and the screenshot set in a pull
request run's artifact, which the bot commits with a write token, plus the
``edited`` event of the bot's review comment, whose checkbox approves a set,
and the collaborator role GitHub reports for whoever ticked it. Each corpus
file is named ``<kind>__<case>`` (``lock``, ``png``, ``approval``,
``command``, ``membership`` or ``role``);
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


def oracle_lock(raw: bytes) -> str | None | Exception:
    """Independent reading of a lock file: the commit, ``None``, or a refusal."""
    if len(raw) > 128:
        return ValueError()
    if raw[-1:] == b"\n":
        raw = raw[:-1]
    if raw == b"none":
        return None
    if len(raw) != 40:
        return ValueError()
    for b in raw:
        if chr(b) not in HEX:
            return ValueError()
    return raw.decode("ascii")


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
    if w == 0 or h == 0 or w > 8192 or h > 8192:
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
        self.assertIsNone(vb.parse_lock(load("lock__none")))
        self.assertEqual(
            vb.parse_lock(load("lock__sha")), "0123456789abcdef0123456789abcdef01234567"
        )
        for bad in (
            "lock__upper",
            "lock__branch",
            "lock__option",
            "lock__crlf",
            "lock__empty",
        ):
            with self.subTest(bad=bad), self.assertRaises(vb.VisualBaselinesError):
                vb.parse_lock(load(bad))
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

    def test_lock_outputs(self) -> None:
        self.assertEqual(
            self.run_main("lock", str(CORPUS / "lock__none")), (0, "sha=\n")
        )
        code, out = self.run_main("lock", str(CORPUS / "lock__sha"))
        self.assertEqual(
            (code, out), (0, "sha=0123456789abcdef0123456789abcdef01234567\n")
        )
        self.assertEqual(self.run_main("lock", str(CORPUS / "lock__branch"))[0], 1)

    def test_committed_lock_parses(self) -> None:
        code, _ = self.run_main("lock", str(REPO / "ui" / "visual" / "baselines.lock"))
        self.assertEqual(code, 0)

    def test_usage(self) -> None:
        self.assertEqual(self.run_main("nope")[0], 2)


class DiffAndRenderTests(unittest.TestCase):
    """The comment the bot posts and the changes it lists."""

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
            "c" * 40,
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
        body = vb.render_comment(
            "a" * 40, "b" * 40, "c" * 40, changes, "https://r", BOT
        )
        self.assertIn("4 screenshot(s) differ", body)
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
            + "c" * 40
            + "/phone/d--dark.png) |",
            body,
        )
        self.assertNotIn("| new |", body)
        self.assertLess(body.index("- [ ]"), body.index("<details>"))

    def test_a_set_too_large_for_one_comment_keeps_every_name(self) -> None:
        changes = [(f"desktop/story-{i:04}--light.png", "added") for i in range(900)]
        body = vb.render_comment(
            "a" * 40, "b" * 40, None, changes, "https://r" * 5, BOT
        )
        self.assertLessEqual(len(body), vb.MAX_COMMENT_CHARS)
        for path, _ in changes:
            self.assertEqual(body.count(f"`{path[:-4]}`"), 1)
        self.assertGreater(body.count("![proposed]"), 100)


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
            None,
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
        lock = base64.b64encode(b"none\n").decode()
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
        lock = base64.b64encode(b"f" * 40 + b"\n").decode()
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
        self, lock: bytes, extra: dict, comments: list, sets: dict | None = None
    ) -> tuple[str, FakeGitHub, list]:
        """Runs ``propose`` for #990 at head ``d...`` with one recorded image."""
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
                ("POST", f"{repo}/pulls/990/requested_reviewers"): (201, {}),
            }
        )
        pushed: list = []
        image = {"desktop/x--light.png": (CORPUS / "png__valid").read_bytes()}
        with (
            unittest.mock.patch.object(
                self.bot,
                "push_set",
                lambda r, f, ref, m, t: pushed.append(ref) or "e" * 40,
            ),
            unittest.mock.patch.object(
                self.bot, "fetch_set", lambda r, c, t: dict((sets or {}).get(c, image))
            ),
        ):
            out = self.bot.propose(self.env, FakeGitHub(answers), write)
        return out, write, pushed

    def test_propose_without_a_merge_ref_reads_the_head_lock(self) -> None:
        repo = "repos/Bookclerk/bookclerk"
        send = FakeGitHub(
            {
                ("GET", f"{repo}/contents/ui/visual/baselines.lock?ref={'d' * 40}"): (
                    200,
                    {"content": base64.b64encode(b"none\n").decode()},
                )
            }
        )
        self.assertIsNone(
            self.bot.compared_lock(send, "Bookclerk/bookclerk", "B/v", 990, "d" * 40)
        )
        self.assertEqual(
            [p for _, p, _ in send.sent],
            [
                f"{repo}/contents/ui/visual/baselines.lock?ref=refs/pull/990/merge",
                f"{repo}/contents/ui/visual/baselines.lock?ref={'d' * 40}",
            ],
        )

    def test_propose_is_quiet_when_the_base_already_approved_the_images(self) -> None:
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        out, write, pushed = self.propose_with(
            approved.encode() + b"\n", {tag: (200, {"object": {"sha": approved}})}, []
        )
        self.assertIn("matches the approved baselines", out)
        self.assertEqual((write.sent, pushed), ([], []))

    def test_propose_keeps_the_comment_that_approved_the_set(self) -> None:
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={approved} -->\n> Approved"
        out, write, _ = self.propose_with(
            approved.encode() + b"\n",
            {tag: (200, {"object": {"sha": approved}})},
            [{"id": 7, "user": {"login": BOT}, "body": body}],
        )
        self.assertIn("matches", out)
        self.assertEqual(write.sent, [])

    def test_propose_keeps_a_pending_set_a_rerun_recorded_again(self) -> None:
        pending = "e" * 40
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={pending} -->\n- [ ] x"
        out, write, pushed = self.propose_with(
            b"none\n", {}, [{"id": 7, "user": {"login": BOT}, "body": body}]
        )
        self.assertIn("still pending", out)
        self.assertEqual((write.sent, pushed), ([], []))

    def test_propose_updates_the_open_comment_for_new_images(self) -> None:
        pending = "9" * 40
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={pending} -->\n- [ ] x"
        out, write, pushed = self.propose_with(
            b"none\n",
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

    def test_changes_after_an_approval_go_in_a_new_comment(self) -> None:
        approved = "a" * 40
        tag = (
            "GET",
            f"repos/Bookclerk/bookclerk-visual-baselines/git/ref/tags/approved/{approved}",
        )
        body = f"<!-- bookclerk-visual-baselines v1 head={'c' * 40} set={approved} -->\n> Approved"
        out, write, pushed = self.propose_with(
            approved.encode() + b"\n",
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
            None,
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
                    {"sha": "f" * 40},
                ),
            }
        )
        baselines = FakeGitHub(
            {
                ("GET", f"{base}/git/ref/tags/approved/" + "b" * 40): (
                    200,
                    {"object": {"sha": "b" * 40}},
                ),
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
        self.assertEqual(base64.b64decode(put["content"]), ("b" * 40 + "\n").encode())
        self.assertEqual(put["branch"], "feature")

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
                    {"sha": "f" * 40},
                ),
            }
        )
        baselines = FakeGitHub(
            {
                ("GET", f"{base}/git/ref/tags/approved/" + pending): (
                    200,
                    {"object": {"sha": pending}},
                ),
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
