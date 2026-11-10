#!/usr/bin/env python3
"""Signed commits check: seed corpus replay, mutation fuzzing and end-to-end runs.

``signed_commits.py`` parses GitHub documents that carry contributor-chosen
text (logins, branch names, commit messages it must ignore) and turns them
into a check run and a pull request comment, so every parser is an
untrusted-input boundary. Each corpus file is named ``<kind>__<case>``;
every corpus input and every mutation of one must be refused with
``ClaInputError`` or read exactly as the oracle reads it. The oracle checks
shapes field by field by hand instead of using the module's patterns. The
fuzzer also checks that the comment rendered from any page it reads lists
exactly the unverified commits and carries no text from outside the
validated fields. The end-to-end tests drive ``main`` over a fake GitHub to
check the verdict, that the comment is posted once and edited in place, and
that a passing pull request gets no new comment.
"""

from __future__ import annotations

import contextlib
import copy
import importlib.util
import io
import json
import random
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "signed_commits_corpus"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


# `signed_commits` imports `cla_check` and `maintainer_review` by name, as it
# does when the workflow runs it from scripts/.
cla = _load("cla_check", ROOT / "scripts" / "cla_check.py")
mr = _load("maintainer_review", ROOT / "scripts" / "maintainer_review.py")
sc = _load("signed_commits", ROOT / "scripts" / "signed_commits.py")

SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40


class Refused(Exception):
    """The oracle refuses the input."""


# --- Oracle -----------------------------------------------------------------

ALNUM = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
HEX = set("0123456789abcdef")
LOWER = set("abcdefghijklmnopqrstuvwxyz")


def o_json(raw: bytes) -> object:
    """Decodes JSON, refusing oversize, non-UTF-8 and NaN/Infinity documents."""
    if len(raw) > 4 * 1024 * 1024:
        raise Refused
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        raise Refused

    def bad(_: str) -> object:
        raise Refused

    try:
        return json.loads(text, parse_constant=bad)
    except (ValueError, RecursionError):
        raise Refused


def o_obj(value: object) -> dict:
    if not isinstance(value, dict):
        raise Refused
    return value


def o_str(value: object) -> str:
    if not isinstance(value, str):
        raise Refused
    return value


def o_id(value: object) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not 1 <= value <= 2**53 - 1:
        raise Refused
    return value


def o_sha(value: object) -> str:
    sha = o_str(value)
    if len(sha) != 40 or any(c not in HEX for c in sha):
        raise Refused
    return sha


def o_login(login: str) -> bool:
    """A person's or organization's login: 1 to 39 of ASCII letters, digits and hyphens, not starting with a hyphen."""
    return 1 <= len(login) <= 39 and login[0] in ALNUM and all(c in ALNUM or c == "-" for c in login)


def o_reason(value: object) -> str:
    """A verification reason: a lowercase letter, then up to 63 lowercase letters or underscores."""
    reason = o_str(value)
    if not 1 <= len(reason) <= 64 or reason[0] not in LOWER or any(c not in LOWER and c != "_" for c in reason):
        raise Refused
    return reason


def oracle(kind: str, raw: bytes) -> object:
    """Reads ``raw`` as a document of ``kind``, independently of the module."""
    doc = o_json(raw)
    if kind.startswith("event_"):
        payload = o_obj(doc)
        if kind == "event_workflow_run":
            run = o_obj(payload.get("workflow_run"))
            event = run.get("event")
            if event not in ("pull_request", "pull_request_review"):
                raise Refused
            repo = o_obj(run.get("head_repository"))
            owner = o_str(o_obj(repo.get("owner")).get("login"))
            if not o_login(owner):
                raise Refused
            branch = o_str(run.get("head_branch"))
            if not 1 <= len(branch) <= 255 or not branch.isprintable():
                raise Refused
            head = ("run", o_sha(run.get("head_sha")), branch, o_id(repo.get("id")), owner)
            return head if event == "pull_request" else ("run", None)
        if kind == "event_merge_group":
            return ("group", o_sha(o_obj(payload.get("merge_group")).get("head_sha")))
        run = o_obj(payload.get("check_run"))
        sha = o_sha(run.get("head_sha"))
        slug = o_str(o_obj(run.get("app")).get("slug"))
        if o_str(run.get("name")) != "Signed commits":
            return ("rerun", None, None)
        return ("rerun", sha, slug)
    if kind == "commits":
        if not isinstance(doc, list):
            raise Refused
        out = []
        for entry in doc:
            obj = o_obj(entry)
            sha = o_sha(obj.get("sha"))
            verification = o_obj(o_obj(obj.get("commit")).get("verification"))
            verified = verification.get("verified")
            if verified is not True and verified is not False:
                raise Refused
            out.append((sha, verified, o_reason(verification.get("reason"))))
        return out
    raise AssertionError(kind)


def module(kind: str, raw: bytes) -> object:
    """Reads ``raw`` with the module, shaped like the oracle's answer."""
    if kind.startswith("event_"):
        event = sc.parse_event(kind.removeprefix("event_"), raw)
        if kind == "event_workflow_run":
            h = event.run_head
            return ("run", None) if h is None else ("run", h.sha, h.branch, h.repo_id, h.owner)
        if kind == "event_merge_group":
            return ("group", event.merge_group_sha)
        return ("rerun", event.rerun_sha, event.rerun_app)
    if kind == "commits":
        return [(c.sha, c.verified, c.reason) for c in sc.parse_commit_page(raw)]
    raise AssertionError(kind)


def outcome(kind: str, raw: bytes, reader) -> tuple:
    """Runs ``reader`` and folds a refusal into ``("refused",)``."""
    try:
        return ("read", reader(kind, raw))
    except (Refused, cla.ClaInputError):
        return ("refused",)


def corpus() -> list[tuple[str, str, bytes]]:
    cases = []
    for path in sorted(CORPUS.iterdir()):
        kind, _, case = path.name.partition("__")
        cases.append((kind, case, path.read_bytes()))
    return cases


def load(name: str) -> bytes:
    return (CORPUS / name).read_bytes()


def assert_matches(test: unittest.TestCase, kind: str, raw: bytes, label: str) -> None:
    test.assertEqual(outcome(kind, raw, module), outcome(kind, raw, oracle), label)


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus against the oracle."""

    def test_corpus_matches_oracle(self) -> None:
        cases = corpus()
        self.assertGreater(len(cases), 0)
        for kind, case, raw in cases:
            assert_matches(self, kind, raw, f"{kind}__{case}")

    def test_expected_refusals(self) -> None:
        for name in (
            "event_workflow_run__push", "event_workflow_run__bad_owner", "event_workflow_run__upper_sha",
            "event_workflow_run__not_object", "event_workflow_run__invalid_utf8", "event_merge_group__bad_sha",
            "event_check_run__no_app", "commits__verified_string", "commits__reason_markdown",
            "commits__reason_upper", "commits__no_verification", "commits__bad_sha", "commits__not_list",
        ):
            kind = name.partition("__")[0]
            self.assertEqual(outcome(kind, load(name), module), ("refused",), name)

    def test_expected_reads(self) -> None:
        self.assertEqual(module("event_workflow_run", load("event_workflow_run__pull_request")), ("run", SHA_A, "feature", 700, "alice"))
        # A review cannot change the commits, so it names no head to check.
        self.assertEqual(module("event_workflow_run", load("event_workflow_run__review")), ("run", None))
        self.assertEqual(module("event_merge_group", load("event_merge_group__basic")), ("group", SHA_B))
        self.assertEqual(module("event_check_run", load("event_check_run__ours")), ("rerun", SHA_A, "bookclerk"))
        self.assertEqual(module("event_check_run", load("event_check_run__other")), ("rerun", None, None))
        self.assertEqual(
            module("commits", load("commits__mixed")),
            [(SHA_A, True, "valid"), (SHA_B, False, "unsigned"), (SHA_C, False, "unknown_key")],
        )
        self.assertEqual(module("commits", load("commits__new_reason")), [(SHA_A, False, "some_future_reason")])
        self.assertEqual(module("commits", load("commits__empty")), [])

    def test_unknown_event_is_refused(self) -> None:
        with self.assertRaises(cla.ClaInputError):
            sc.parse_event("pull_request_target", load("event_workflow_run__pull_request"))

    def test_every_documented_reason_has_text(self) -> None:
        for reason, text in sc.REASON_TEXT.items():
            self.assertIsNotNone(sc.REASON.fullmatch(reason), reason)
            self.assertTrue(text.endswith("."), reason)
        self.assertIn("`some_future_reason`", sc.reason_text("some_future_reason"))


# --- Fuzzing -------------------------------------------------------------------

#: Values mutations put in place of a JSON value.
VALUES: list[object] = [
    None, True, False, 0, 1, -1, 7.0, 2**53, 2**53 - 1, "", "x", SHA_A, SHA_A.upper(), SHA_A[:39],
    "alice", "-alice", "evil[bot]", "User", "Bot", "valid", "unsigned", "unknown_key", "Unsigned", "a b",
    "a|b", "a`b", "_x", "x" * 65, "x" * 64, "true", "pull_request", "pull_request_review", "push",
    "Signed commits", "Maintainer review", "[x](https://example.com)", [], {}, [1], {"id": 1},
]

#: Byte fragments the raw mutator splices in.
FRAGMENTS = [b"{", b"}", b"[", b"]", b",", b":", b'"', b"\\", b"\\u0000", b"NaN", b"Infinity", b"1e999", b"\xff", b"\x00", b" "]


def mutate_value(rng: random.Random, value: object, depth: int = 0) -> object:
    """Returns a structurally mutated copy of a decoded JSON value."""
    if isinstance(value, dict) and value and rng.random() < 0.8:
        key = rng.choice(list(value))
        op = rng.random()
        if op < 0.2:
            del value[key]
        elif op < 0.5 or depth > 6:
            value[key] = copy.deepcopy(rng.choice(VALUES))
        else:
            value[key] = mutate_value(rng, value[key], depth + 1)
        return value
    if isinstance(value, list) and value and rng.random() < 0.8:
        i = rng.randrange(len(value))
        op = rng.random()
        if op < 0.2:
            del value[i]
        elif op < 0.3:
            value.append(copy.deepcopy(value[i]))
        elif op < 0.5 or depth > 6:
            value[i] = copy.deepcopy(rng.choice(VALUES))
        else:
            value[i] = mutate_value(rng, value[i], depth + 1)
        return value
    return copy.deepcopy(rng.choice(VALUES))


def mutate(rng: random.Random, raw: bytes) -> bytes:
    """Mutates a corpus document structurally, then sometimes byte by byte."""
    if rng.random() < 0.85:
        try:
            doc = json.loads(raw)
            for _ in range(rng.randint(1, 3)):
                doc = mutate_value(rng, doc)
            raw = json.dumps(doc, ensure_ascii=rng.random() < 0.5).encode("utf-8")
        except (ValueError, RecursionError):
            pass  # Not JSON: mutate the bytes only.
    if rng.random() < 0.3:
        for _ in range(rng.randint(1, 3)):
            pos = rng.randint(0, len(raw))
            raw = raw[:pos] + rng.choice(FRAGMENTS) + raw[pos + rng.randint(0, 2):]
    return raw


class MutationFuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 6000

    def test_mutations_match_oracle(self) -> None:
        cases = corpus()
        rng = random.Random(20261110)
        for n in range(self.ITERATIONS):
            kind, case, raw = rng.choice(cases)
            mutated = mutate(rng, raw)
            assert_matches(self, kind, mutated, f"iteration {n} from {kind}__{case}: {mutated[:200]!r}")

    def test_fuzzed_comments_list_exactly_the_unverified_commits(self) -> None:
        """The verdict and comment agree with an independent reading of each page."""
        pages = [raw for kind, _, raw in corpus() if kind == "commits"]
        rng = random.Random(1110)
        for n in range(2000):
            raw = mutate(rng, rng.choice(pages))
            try:
                read = oracle("commits", raw)
            except Refused:
                continue
            commits = sc.parse_commit_page(raw)
            pull = cla.PullRequest(number=1, author=OUTSIDER, head_sha=SHA_A, commits=len(commits))
            verdict = sc.evaluate(pull, commits)
            unverified = [(sha, reason) for sha, verified, reason in read if not verified]
            self.assertEqual([(c.sha, c.reason) for c in verdict.unverified], unverified, f"iteration {n}")
            self.assertEqual(verdict.passed, not unverified, f"iteration {n}")
            body = sc.render_comment(verdict)
            self.assertTrue(body.startswith(sc.COMMENT_MARKER), f"iteration {n}")
            rows = [line for line in body.splitlines() if line.startswith("| `")]
            self.assertEqual([r[3:10] for r in rows], [sha[:7] for sha, _ in unverified], f"iteration {n}")
            for row in rows:
                # Each row has exactly its two cells, so no reason can add a
                # column, a link or a code span.
                self.assertEqual(row.count("|"), 3, row)
                self.assertNotIn("](", row, row)
            # Only GitHub's documentation is linked.
            self.assertEqual(body.count("]("), 0 if verdict.passed else 1, f"iteration {n}")


# --- End to end ----------------------------------------------------------------


def account(login: str, account_id: int, kind: str = "User") -> dict:
    return {"login": login, "id": account_id, "type": kind}


OWNER = account("fritz-fritz", 5253894)
OUTSIDER_DOC = account("alice", 101)
OUTSIDER = cla.parse_account(OUTSIDER_DOC)
APP = account("bookclerk[bot]", 9, "Bot")
ACTIONS = account("github-actions[bot]", 41898282, "Bot")


def commit(sha: str, verified: bool = True, reason: str | None = None) -> dict:
    return {"sha": sha, "commit": {"verification": {"verified": verified, "reason": reason or ("valid" if verified else "unsigned")}}}


class FakeGitHub:
    """A GitHub REST API in memory, enough for the check."""

    def __init__(self) -> None:
        self.pulls: dict[int, dict] = {}
        self.commits: dict[int, list[dict]] = {}
        self.comments: dict[int, list[dict]] = {}
        self.checks: list[dict] = []
        self.calls: list[tuple[str, str]] = []
        self.fail_paths: set[str] = set()
        self.comment_status = 201
        self.next_comment = 1000

    def add_pull(self, number: int, commits: list[dict], author: dict = OUTSIDER_DOC, sha: str = SHA_A, owner: str = "alice", repo_id: int = 700, branch: str = "feature") -> None:
        self.pulls[number] = {
            "number": number,
            "state": "open",
            "user": author,
            "commits": len(commits),
            "head": {"sha": sha, "ref": branch, "repo": {"id": repo_id}, "label": f"{owner}:{branch}"},
        }
        self.commits[number] = commits
        self.comments.setdefault(number, [])

    def add_comment(self, number: int, who: dict, body: str) -> None:
        self.next_comment += 1
        self.comments[number].append({"id": self.next_comment, "user": who, "body": body, "created_at": "2026-11-10T00:00:00Z"})

    def ours(self, number: int) -> list[dict]:
        return [c for c in self.comments[number] if c["user"] == APP and c["body"].startswith(sc.COMMENT_MARKER)]

    def send(self, method: str, path: str, body: dict | None) -> tuple[int, bytes]:
        self.calls.append((method, path))
        prefix = "repos/bookclerk/bookclerk/"
        assert path.startswith(prefix), path
        rest = path[len(prefix):]
        route, _, query = rest.partition("?")
        params = dict(p.split("=", 1) for p in query.split("&") if p)
        if route in self.fail_paths:
            return 500, b"{}"
        parts = route.split("/")
        if method == "GET" and parts[0] == "pulls" and len(parts) == 1:
            head = params["head"].replace("%3A", ":")
            refs = [p for p in self.pulls.values() if p["head"]["label"] == head and p["state"] == "open"]
            return 200, json.dumps(refs).encode()
        if method == "GET" and parts[0] == "pulls" and len(parts) == 2:
            pull = self.pulls.get(int(parts[1]))
            return (200, json.dumps(pull).encode()) if pull else (404, b"{}")
        if method == "GET" and parts[0] == "pulls" and parts[2] == "commits":
            items = self.commits[int(parts[1])]
            page, size = int(params.get("page", "1")), int(params["per_page"])
            return 200, json.dumps(items[(page - 1) * size: page * size]).encode()
        if method == "GET" and parts[0] == "commits" and parts[2] == "pulls":
            # Like GitHub, the commit listing leaves out fork pull requests.
            refs = [p for p in self.pulls.values() if p["head"]["sha"] == parts[1] and p["head"]["repo"]["id"] == 555]
            return 200, json.dumps(refs).encode()
        if method == "GET" and parts[0] == "issues" and parts[2] == "comments":
            items = self.comments[int(parts[1])]
            page, size = int(params.get("page", "1")), int(params["per_page"])
            return 200, json.dumps(items[(page - 1) * size: page * size]).encode()
        if method == "POST" and parts[0] == "issues" and parts[2] == "comments":
            if self.comment_status != 201:
                return self.comment_status, b"{}"
            self.add_comment(int(parts[1]), APP, body["body"])
            return 201, b"{}"
        if method == "PATCH" and route.startswith("issues/comments/"):
            for comments in self.comments.values():
                for comment in comments:
                    if comment["id"] == int(parts[2]):
                        comment["body"] = body["body"]
                        return 200, b"{}"
            return 404, b"{}"
        if method == "POST" and route == "check-runs":
            self.checks.append(body)
            return 201, json.dumps({"id": len(self.checks)}).encode()
        raise AssertionError((method, path))


def run_event(gh: FakeGitHub, kind: str, payload: dict, app: bool = True) -> int:
    """Runs ``main`` for one event and returns its exit code."""
    with tempfile.NamedTemporaryFile("wb", suffix=".json", delete=False) as handle:
        handle.write(json.dumps({"repository": {"id": 555}, **payload}).encode())
    env = {
        "GITHUB_REPOSITORY": "bookclerk/bookclerk",
        "GITHUB_EVENT_NAME": kind,
        "GITHUB_EVENT_PATH": handle.name,
        "GITHUB_RUN_ID": "1",
        "GITHUB_TOKEN": "t",
    }
    if app:
        env |= {"SIGNED_APP_TOKEN": "app", "SIGNED_APP_SLUG": "bookclerk"}
    original = sc.urllib_transport
    sc.urllib_transport = lambda token, url: gh.send
    try:
        with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
            return sc.main(env)
    finally:
        sc.urllib_transport = original
        Path(handle.name).unlink()


def trigger(gh: FakeGitHub, sha: str = SHA_A, owner: str = "alice", repo_id: int = 700, event: str = "pull_request") -> int:
    run = {"event": event, "head_sha": sha, "head_branch": "feature", "head_repository": {"id": repo_id, "owner": {"login": owner}}}
    return run_event(gh, "workflow_run", {"workflow_run": run})


class EndToEndTests(unittest.TestCase):
    """Runs the check over a fake GitHub."""

    def last(self, gh: FakeGitHub) -> dict:
        self.assertTrue(gh.checks)
        return gh.checks[-1]

    def test_an_unsigned_commit_fails_and_gets_one_comment_that_clears(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B), commit(SHA_C, False)])
        self.assertEqual(trigger(gh), 0)
        check = self.last(gh)
        self.assertEqual((check["name"], check["head_sha"], check["conclusion"]), ("Signed commits", SHA_A, "failure"))
        self.assertEqual(check["output"]["title"], "1 of 2 commits is not verified.")
        [comment] = gh.ours(1)
        self.assertIn("| `ccccccc` | It is not signed. |", comment["body"])
        self.assertNotIn("`bbbbbbb`", comment["body"])
        self.assertIn("HEAD~2", comment["body"])
        self.assertIn("@alice", comment["body"])
        # Running again with nothing new neither posts nor edits.
        calls = len(gh.calls)
        trigger(gh)
        self.assertFalse(any(m in ("POST", "PATCH") and "comments" in p for m, p in gh.calls[calls:]))
        # A push that signs everything passes and edits the same comment.
        gh.add_pull(1, [commit(SHA_B), commit(SHA_C)])
        trigger(gh)
        self.assertEqual(self.last(gh)["conclusion"], "success")
        [comment] = gh.ours(1)
        self.assertIn("now signed and verified", comment["body"])
        self.assertEqual(len(gh.comments[1]), 1)

    def test_a_pull_request_that_was_always_signed_gets_no_comment(self) -> None:
        for author in (OUTSIDER_DOC, OWNER, account("dependabot[bot]", 49699333, "Bot")):
            gh = FakeGitHub()
            gh.add_pull(1, [commit(SHA_B)], author=author)
            trigger(gh)
            self.assertEqual(self.last(gh)["conclusion"], "success")
            self.assertEqual(self.last(gh)["output"]["title"], "Every commit is signed and verified.")
            self.assertEqual(gh.comments[1], [])

    def test_maintainers_need_verified_commits_too(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B, False, "unverified_email")], author=OWNER, owner="bookclerk", repo_id=555)
        trigger(gh, owner="bookclerk", repo_id=555)
        self.assertEqual(self.last(gh)["conclusion"], "failure")
        self.assertIn("not verified on the signer's GitHub account", gh.ours(1)[0]["body"])

    def test_a_marked_comment_from_someone_else_is_ignored(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B, False)])
        gh.add_comment(1, OUTSIDER_DOC, sc.COMMENT_MARKER + "\nAll good!")
        gh.add_comment(1, ACTIONS, sc.COMMENT_MARKER + "\nAll good!")
        trigger(gh)
        self.assertEqual(len(gh.ours(1)), 1)
        self.assertEqual(gh.comments[1][0]["body"], sc.COMMENT_MARKER + "\nAll good!")

    def test_many_unverified_commits_are_counted(self) -> None:
        gh = FakeGitHub()
        shas = [f"{i:040x}" for i in range(1, sc.MAX_LISTED_COMMITS + 6)]
        gh.add_pull(1, [commit(s, False) for s in shas])
        trigger(gh)
        body = gh.ours(1)[0]["body"]
        self.assertIn("and 5 more", body)
        self.assertEqual(sum(1 for line in body.splitlines() if line.startswith("| `")), sc.MAX_LISTED_COMMITS)
        self.assertIn("and 5 more", self.last(gh)["output"]["summary"])

    def test_a_comment_refusal_does_not_stop_the_check(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B, False)])
        gh.comment_status = 403
        self.assertEqual(trigger(gh), 0)
        self.assertEqual(self.last(gh)["conclusion"], "failure")
        gh.comment_status = 201
        gh.fail_paths.add("issues/1/comments")
        self.assertEqual(trigger(gh), 0)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_a_shared_head_passes_when_any_pull_request_on_it_passes(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_A)], author=OWNER, owner="bookclerk", repo_id=555)
        gh.add_pull(2, [commit(SHA_B, False), commit(SHA_A)], author=OWNER, owner="bookclerk", repo_id=555, branch="other")
        trigger(gh, owner="bookclerk", repo_id=555)
        self.assertEqual(self.last(gh)["conclusion"], "success")
        self.assertTrue(self.last(gh)["output"]["title"].startswith("#1: "))
        self.assertIn("#2: 1 of 2", self.last(gh)["output"]["summary"])
        self.assertEqual(gh.comments[1], [])
        self.assertEqual(len(gh.ours(2)), 1)

    def test_reviews_and_other_forks_are_ignored(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B, False)], repo_id=701)
        trigger(gh)
        trigger(gh, repo_id=701, event="pull_request_review")
        self.assertEqual(gh.checks, [])

    def test_merge_groups_pass(self) -> None:
        gh = FakeGitHub()
        self.assertEqual(run_event(gh, "merge_group", {"merge_group": {"head_sha": SHA_C}}), 0)
        self.assertEqual((self.last(gh)["head_sha"], self.last(gh)["conclusion"]), (SHA_C, "success"))

    def test_rerun_answers_only_its_own_check(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_A)], owner="bookclerk", repo_id=555)
        ours = {"name": "Signed commits", "head_sha": SHA_A, "app": {"slug": "bookclerk"}}
        self.assertEqual(run_event(gh, "check_run", {"check_run": ours}), 0)
        self.assertEqual(self.last(gh)["conclusion"], "success")
        gh.checks.clear()
        for other in ({**ours, "app": {"slug": "someone-else"}}, {**ours, "name": "Maintainer review"}):
            run_event(gh, "check_run", {"check_run": other})
        self.assertEqual(gh.checks, [])

    def test_failed_reads_and_mismatched_counts_report_an_error(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B)])
        gh.fail_paths.add("pulls/1/commits")
        self.assertEqual(trigger(gh), 1)
        self.assertEqual(self.last(gh)["output"]["title"], sc.ERROR_DESCRIPTION)
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B)])
        gh.pulls[1]["commits"] = 2
        self.assertEqual(trigger(gh), 1)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_too_many_commits_fail_closed(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(f"{i:040x}") for i in range(1, cla.MAX_COMMITS + 2)])
        self.assertEqual(trigger(gh), 1)
        self.assertEqual(self.last(gh)["output"]["title"], sc.ERROR_DESCRIPTION)

    def test_commits_are_read_across_pages(self) -> None:
        gh = FakeGitHub()
        shas = [f"{i:040x}" for i in range(1, cla.PAGE_SIZE + 3)]
        gh.add_pull(1, [commit(s) for s in shas[:-1]] + [commit(shas[-1], False)])
        trigger(gh)
        self.assertEqual(self.last(gh)["output"]["title"], f"1 of {len(shas)} commits is not verified.")

    def test_a_closed_pull_request_gets_no_check(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B, False)])
        gh.pulls[1]["state"] = "closed"
        self.assertEqual(trigger(gh), 0)
        self.assertEqual(gh.checks, [])

    def test_the_workflow_token_reports_without_the_app(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, [commit(SHA_B, False)])
        run = {"event": "pull_request", "head_sha": SHA_A, "head_branch": "feature", "head_repository": {"id": 700, "owner": {"login": "alice"}}}
        self.assertEqual(run_event(gh, "workflow_run", {"workflow_run": run}, app=False), 0)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_bad_environment_and_events_fail(self) -> None:
        gh = FakeGitHub()
        self.assertEqual(run_event(gh, "pull_request_target", {}), 1)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(sc.main({"GITHUB_REPOSITORY": "nope"}), 1)
            self.assertEqual(
                sc.main({"GITHUB_REPOSITORY": "bookclerk/bookclerk", "SIGNED_APP_TOKEN": "x", "SIGNED_APP_SLUG": "bad slug"}),
                1,
            )


if __name__ == "__main__":
    unittest.main()
