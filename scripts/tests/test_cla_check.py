#!/usr/bin/env python3
"""CLA check: seed corpus replay, mutation fuzzing and end-to-end runs.

``cla_check.py`` parses GitHub documents that carry contributor-chosen text
(logins, comment bodies, commit metadata) and decides who may skip the CLA,
so every parser is an untrusted-input boundary. Each corpus file is named
``<kind>__<case>``; every corpus input and every mutation of one must be
refused with ``ClaInputError`` or read exactly as the oracle reads it. The
oracle checks shapes field by field by hand instead of using the module's
patterns. The end-to-end tests drive ``run`` over a fake GitHub to check who
must sign, how signatures are recorded, and what the comment may contain.
"""

from __future__ import annotations

import base64
import copy
import importlib.util
import json
import random
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "cla_check_corpus"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


cla = _load("cla_check", ROOT / "scripts" / "cla_check.py")

SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40
CLA_URL = "https://github.com/bookclerk/bookclerk/blob/main/CLA.md"
RUN_URL = "https://github.com/bookclerk/bookclerk/actions/runs/1"


class Refused(Exception):
    """The oracle refuses the input."""


# --- Oracle -----------------------------------------------------------------

ALNUM = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")


def o_json(raw: bytes, limit: int = 4 * 1024 * 1024) -> object:
    """Decodes JSON, refusing oversize, non-UTF-8 and NaN/Infinity documents."""
    if len(raw) > limit:
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


def o_obj(v: object) -> dict:
    if type(v) is not dict:
        raise Refused
    return v


def o_id(v: object) -> int:
    if type(v) is not int or v < 1 or v > 9007199254740991:
        raise Refused
    return v


def o_str(v: object) -> str:
    if type(v) is not str:
        raise Refused
    return v


def o_sha(v: object) -> str:
    s = o_str(v)
    if len(s) != 40 or any(c not in "0123456789abcdef" for c in s):
        raise Refused
    return s


def o_base_login(s: str) -> bool:
    if not 1 <= len(s) <= 39 or any(c not in ALNUM and c != "-" for c in s):
        return False
    return s[0] != "-"


def o_account(v: object) -> tuple[int, str, str]:
    obj = o_obj(v)
    account_id = o_id(obj.get("id"))
    login = o_str(obj.get("login"))
    kind = obj.get("type")
    if kind not in ("User", "Bot", "Organization", "Mannequin"):
        raise Refused
    if kind == "Bot":
        if not login.endswith("[bot]") or not o_base_login(login[:-5]):
            raise Refused
    elif not o_base_login(login):
        raise Refused
    return (account_id, login, kind)


def o_pull(v: object) -> tuple:
    obj = o_obj(v)
    head = o_obj(obj.get("head"))
    commits = obj.get("commits")
    if type(commits) is not int or commits < 0:
        raise Refused
    return (o_id(obj.get("number")), o_account(obj.get("user")), o_sha(head.get("sha")), commits)


def o_comment(v: object) -> tuple:
    obj = o_obj(v)
    return (o_id(obj.get("id")), o_account(obj.get("user")), o_str(obj.get("body")), o_str(obj.get("created_at")))


def oracle(kind: str, raw: bytes) -> object:
    """Reads ``raw`` as a document of ``kind``, independently of the module."""
    if kind == "pull":
        return o_pull(o_json(raw))
    if kind.startswith("event_"):
        name = kind[len("event_"):]
        payload = o_obj(o_json(raw))
        repo = o_id(o_obj(payload.get("repository")).get("id"))
        if name == "pull_request_target":
            return ("pr", repo, o_pull(payload.get("pull_request")))
        if name == "issue_comment":
            issue = o_obj(payload.get("issue"))
            number = o_id(issue.get("number"))
            comment = o_comment(payload.get("comment"))
            return ("comment", repo, number if issue.get("pull_request") is not None else None, comment)
        return ("merge", repo, o_sha(o_obj(payload.get("merge_group")).get("head_sha")))
    if kind == "commits":
        page = o_json(raw)
        if type(page) is not list:
            raise Refused
        out = []
        for entry in page:
            obj = o_obj(entry)
            author = obj.get("author")
            out.append((o_sha(obj.get("sha")), None if author is None else o_account(author)))
        return out
    if kind == "comments":
        page = o_json(raw)
        if type(page) is not list:
            raise Refused
        return [o_comment(e) for e in page]
    if kind == "coauthors":
        doc = o_obj(o_json(raw))
        if doc.get("errors") is not None:
            raise Refused
        commits = o_obj(o_obj(o_obj(o_obj(doc.get("data")).get("repository")).get("pullRequest")).get("commits"))
        info = o_obj(commits.get("pageInfo"))
        if type(info.get("hasNextPage")) is not bool:
            raise Refused
        cursor = None
        if info["hasNextPage"]:
            cursor = o_str(info.get("endCursor"))
            if cursor == "" or len(cursor) > 1024:
                raise Refused
        nodes = commits.get("nodes")
        if type(nodes) is not list:
            raise Refused
        out = {}
        assistants = set()
        for n in nodes:
            commit = o_obj(o_obj(n).get("commit"))
            sha = o_sha(commit.get("oid"))
            if sha in out:
                raise Refused
            authors = o_obj(commit.get("authors"))
            total = authors.get("totalCount")
            entries = authors.get("nodes")
            if type(total) is not int or total < 0 or total > 25 or type(entries) is not list or len(entries) != total:
                raise Refused
            linked = []
            for e in entries:
                e = o_obj(e)
                email = e.get("email")
                if email is not None and type(email) is not str:
                    raise Refused
                user = e.get("user")
                if user is not None:
                    user = o_obj(user)
                    account = o_account({"id": user.get("databaseId"), "login": user.get("login"), "type": "User"})
                    if email is not None and email.casefold() == "noreply@anthropic.com":
                        assistants.add(account[0])
                    else:
                        linked.append(account)
            out[sha] = linked
        return (out, cursor, frozenset(assistants))
    if kind == "signatures":
        doc = o_obj(o_json(raw, 1024 * 1024))
        entries = doc.get("signedContributors")
        if type(entries) is not list:
            raise Refused
        ids = set()
        for entry in entries:
            obj = o_obj(entry)
            o_str(obj.get("name"))
            ids.add(o_id(obj.get("id")))
        return (len(entries), frozenset(ids))
    if kind == "contents":
        obj = o_obj(o_json(raw))
        if obj.get("type") != "file" or obj.get("encoding") != "base64":
            raise Refused
        sha = o_sha(obj.get("sha"))
        text = "".join(o_str(obj.get("content")).split())
        alphabet = set("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=")
        if any(c not in alphabet for c in text):
            raise Refused
        try:
            data = base64.b64decode(text, validate=True)
        except ValueError:
            raise Refused
        if len(data) > 1024 * 1024:
            raise Refused
        return (data, sha)
    raise AssertionError(kind)


def acc(a) -> tuple[int, str, str]:
    """Normalizes a module ``Account`` to the oracle's tuple."""
    return (a.id, a.login, a.type.value)


def module(kind: str, raw: bytes) -> object:
    """Reads ``raw`` with the module, normalized to the oracle's shape."""
    def pull(p) -> tuple:
        return (p.number, acc(p.author), p.head_sha, p.commits)

    def comment(c) -> tuple:
        return (c.id, acc(c.author), c.body, c.created_at)

    if kind == "pull":
        return pull(cla.parse_pull_request(cla._load_json(raw)))
    if kind.startswith("event_"):
        ev = cla.parse_event(kind[len("event_"):], raw)
        if ev.kind is cla.EventKind.PULL_REQUEST:
            assert ev.pull_number == ev.pull_request.number
            return ("pr", ev.repo_id, pull(ev.pull_request))
        if ev.kind is cla.EventKind.ISSUE_COMMENT:
            return ("comment", ev.repo_id, ev.pull_number, comment(ev.comment))
        return ("merge", ev.repo_id, ev.merge_group_sha)
    if kind == "commits":
        return [(c.sha, None if c.author is None else acc(c.author)) for c in cla.parse_commit_page(raw)]
    if kind == "comments":
        return [comment(c) for c in cla.parse_comment_page(raw)]
    if kind == "coauthors":
        page = cla.parse_coauthor_page(raw)
        commits = {sha: [acc(a) for a in accounts] for sha, accounts in page.commits.items()}
        return (commits, page.next_cursor, page.assistant_ids)
    if kind == "signatures":
        book = cla.parse_signatures(raw)
        return (len(book.entries), book.signed_ids)
    if kind == "contents":
        return cla.parse_contents(raw)
    raise AssertionError(kind)


def outcome(fn, kind: str, raw: bytes, refusal: type[Exception]) -> object:
    try:
        return ("ok", fn(kind, raw))
    except refusal:
        return ("refused",)


def assert_matches(case: unittest.TestCase, kind: str, raw: bytes, label: str) -> None:
    case.assertEqual(outcome(module, kind, raw, cla.ClaInputError), outcome(oracle, kind, raw, Refused), label)


def corpus() -> list[tuple[str, str, bytes]]:
    out = []
    for path in sorted(CORPUS.iterdir()):
        if path.is_file():
            kind, _, case = path.name.partition("__")
            out.append((kind, case, path.read_bytes()))
    return out


def load(name: str) -> bytes:
    return (CORPUS / name).read_bytes()


# --- Corpus -------------------------------------------------------------------


class CorpusTests(unittest.TestCase):
    """Replays the seed corpus against the oracle."""

    def test_corpus_matches_oracle(self) -> None:
        cases = corpus()
        self.assertGreater(len(cases), 0)
        for kind, case, raw in cases:
            assert_matches(self, kind, raw, f"{kind}__{case}")

    def test_expected_refusals(self) -> None:
        for name in (
            "pull__bot_login_as_user", "pull__user_login_as_bot", "pull__short_sha", "pull__upper_sha",
            "pull__bool_id", "pull__negative_commits", "pull__float_number", "pull__login_too_long", "pull__nan",
            "event_issue_comment__no_comment", "event_merge_group__bad_sha",
            "event_pull_request_target__not_object", "event_pull_request_target__invalid_utf8",
            "commits__not_list", "commits__missing_sha", "commits__bad_author", "comments__null_body",
            "signatures__missing_key", "signatures__string_id", "signatures__no_name", "signatures__zero_id",
            "signatures__deep", "contents__dir", "contents__bad_base64", "contents__none_encoding",
            "coauthors__errors", "coauthors__too_many", "coauthors__count_mismatch", "coauthors__repeat",
            "coauthors__no_cursor", "coauthors__bot_login_as_user", "coauthors__null_pull",
            "coauthors__email_not_string",
        ):
            kind = name.partition("__")[0]
            self.assertEqual(outcome(module, kind, load(name), cla.ClaInputError), ("refused",), name)

    def test_expected_reads(self) -> None:
        self.assertEqual(module("commits", load("commits__mixed")), [(SHA_A, (101, "alice", "User")), (SHA_B, None)])
        self.assertEqual(module("signatures", load("signatures__seed")), (0, frozenset()))
        # Some older accounts have logins new accounts cannot take.
        self.assertEqual(module("pull", load("pull__login_double_dash"))[1], (1, "al--ice", "User"))
        self.assertEqual(module("pull", load("pull__login_trailing_dash"))[1], (1, "alice-", "User"))
        self.assertEqual(module("signatures", load("signatures__two")), (2, frozenset({101, 2})))
        data, blob = cla.parse_contents(load("contents__seed"))
        self.assertEqual(data, load("signatures__seed"))
        self.assertEqual(blob, SHA_A)
        self.assertEqual(module("event_issue_comment", load("event_issue_comment__plain_issue"))[2], None)
        self.assertEqual(
            module("coauthors", load("coauthors__basic")),
            ({SHA_A: [(101, "alice", "User"), (102, "bob", "User")], SHA_B: []}, None, frozenset()),
        )
        self.assertEqual(module("coauthors", load("coauthors__next_page"))[1], "Y3Vyc29yOnYyOpHOAAAAAQ==")
        self.assertEqual(
            module("coauthors", load("coauthors__assistant")),
            ({SHA_A: [(101, "alice", "User")]}, None, frozenset({900001})),
        )

    def test_unknown_event_is_refused(self) -> None:
        with self.assertRaises(cla.ClaInputError):
            cla.parse_event("pull_request", load("event_pull_request_target__opened"))


# --- Fuzzing -------------------------------------------------------------------

#: Values mutations put in place of a JSON value.
VALUES: list[object] = [
    None, True, False, 0, 1, -1, 7.0, 2**53, 2**53 - 1, 101, 49699333, "", "x", SHA_A, SHA_A.upper(), SHA_A[:39],
    "alice", "alice-", "-alice", "al--ice", "a" * 39, "a" * 40, "dependabot[bot]", "github-actions[bot]",
    "evil[bot]", "[bot]", "ålice", "alice\n", "User", "Bot", "Organization", "Mannequin", "user",
    "file", "dir", "base64", "@everyone", "<!-- bookclerk-cla-check -->", cla.SIGN_PHRASE, [], {}, [1], {"id": 1},
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
            pass  # Not JSON, or too deep to rebuild: mutate the bytes only.
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
        rng = random.Random(20261027)
        for n in range(self.ITERATIONS):
            kind, case, raw = rng.choice(cases)
            mutated = mutate(rng, raw)
            assert_matches(self, kind, mutated, f"iteration {n} from {kind}__{case}: {mutated[:200]!r}")

    def test_fuzzed_verdicts_keep_invariants(self) -> None:
        """Verdicts and comments built from mutated commit pages stay safe."""
        pages = [raw for kind, _, raw in corpus() if kind == "commits"]
        pull = cla.parse_pull_request(cla._load_json(load("pull__basic")))
        book = cla.parse_signatures(load("signatures__two"))
        rng = random.Random(491)
        checked = 0
        for _ in range(self.ITERATIONS):
            try:
                commits = cla.parse_commit_page(mutate(rng, rng.choice(pages)))
            except cla.ClaInputError:
                continue
            checked += 1
            verdict = cla.evaluate(pull, commits, book)
            linked = [pull.author] + [a for c in commits for a in (c.author, *c.coauthors) if a is not None]
            required = {a.id for a in linked if not cla.is_exempt(a)}
            self.assertEqual({a.id for a in verdict.unsigned}, required - book.signed_ids)
            self.assertEqual(set(verdict.unlinked), {c.sha for c in commits if c.author is None})
            self.assertEqual(verdict.passed, not verdict.unsigned and not verdict.unlinked)
            assert_safe_comment(self, cla.render_comment(verdict, CLA_URL), verdict)
            self.assertLessEqual(len(cla.status_description(verdict)), 140)
        self.assertGreater(checked, 100)


def assert_safe_comment(case: unittest.TestCase, body: str, verdict) -> None:
    """The comment mentions only unsigned logins and links only to the CLA."""
    case.assertTrue(body.startswith(cla.COMMENT_MARKER + "\n"))
    rest = body[len(cla.COMMENT_MARKER):]
    case.assertNotIn("<", rest)
    mentions = set()
    for i, ch in enumerate(rest):
        if ch == "@":
            j = i + 1
            while j < len(rest) and (rest[j] in ALNUM or rest[j] in "-[]"):
                j += 1
            mentions.add(rest[i + 1:j])
    case.assertEqual(mentions, {a.login for a in verdict.unsigned})
    case.assertEqual(rest.count("]("), 1 if not verdict.passed else 0)
    if not verdict.passed:
        case.assertIn(f"]({CLA_URL})", rest)


# --- End to end ----------------------------------------------------------------


def account(login: str, account_id: int, kind: str = "User") -> dict[str, object]:
    return {"login": login, "id": account_id, "type": kind}


ALICE = account("alice", 101)
BOB = account("bob", 102)
DEPENDABOT = account("dependabot[bot]", 49699333, "Bot")
ACTIONS = account("github-actions[bot]", 41898282, "Bot")


class FakeGitHub:
    """An in-memory GitHub REST API for one pull request."""

    def __init__(self, author: dict, commits: list[dict], signed: list[dict] | None = None) -> None:
        self.author = author
        self.commit_list = commits
        self.file = json.dumps({"signedContributors": signed or []}).encode()
        self.blob = 1
        self.comments: list[dict] = []
        self.statuses: list[tuple[str, dict]] = []
        self.conflicts = 0
        self.requests: list[tuple[str, str]] = []

    def pull(self) -> dict:
        return {"number": 7, "user": self.author, "head": {"sha": SHA_C}, "commits": len(self.commit_list)}

    def blob_sha(self) -> str:
        return f"{self.blob:040x}"

    def graphql(self, body: dict) -> tuple[int, bytes]:
        variables = body["variables"]
        assert (variables["owner"], variables["name"], variables["number"]) == ("bookclerk", "bookclerk", 7)
        start = int(variables["after"] or 0)
        nodes = []
        for c in self.commit_list[start:start + 100]:
            primary = c["author"] if c["author"] and c["author"]["type"] == "User" else None
            users = [primary, *c.get("coauthors", [])]
            user_nodes = [
                {"email": (u or {}).get("email", "x@example.com"), "user": None if u is None else {"databaseId": u["id"], "login": u["login"]}}
                for u in users
            ]
            nodes.append({"commit": {"oid": c["sha"], "authors": {"totalCount": len(user_nodes), "nodes": user_nodes}}})
        more = start + 100 < len(self.commit_list)
        info = {"hasNextPage": more, "endCursor": str(start + 100) if more else None}
        page = {"data": {"repository": {"pullRequest": {"commits": {"pageInfo": info, "nodes": nodes}}}}}
        return 200, json.dumps(page).encode()

    def send(self, method: str, path: str, body: dict | None) -> tuple[int, bytes]:
        self.requests.append((method, path))
        if (method, path) == ("POST", "graphql"):
            assert body is not None
            return self.graphql(body)
        prefix = "repos/bookclerk/bookclerk/"
        assert path.startswith(prefix), path
        path = path[len(prefix):]
        route, _, query = path.partition("?")
        params = dict(p.split("=") for p in query.split("&")) if query else {}
        page = int(params.get("page", "1"))
        size = int(params.get("per_page", "30"))

        def page_of(items: list) -> bytes:
            return json.dumps(items[(page - 1) * size: page * size]).encode()

        if method == "GET" and route == "pulls/7":
            return 200, json.dumps(self.pull()).encode()
        if method == "GET" and route == "pulls/7/commits":
            return 200, page_of(self.commit_list)
        if method == "GET" and route == "issues/7/comments":
            return 200, page_of(self.comments)
        if method == "GET" and route == "contents/signatures/cla.json":
            assert params["ref"] == "cla-signatures"
            content = base64.encodebytes(self.file).decode()
            return 200, json.dumps({"type": "file", "encoding": "base64", "sha": self.blob_sha(), "content": content}).encode()
        if method == "PUT" and route == "contents/signatures/cla.json":
            assert body is not None and body["branch"] == "cla-signatures"
            if self.conflicts:
                self.conflicts -= 1
                self.blob += 1
                return 409, b"{}"
            if body["sha"] != self.blob_sha():
                return 409, b"{}"
            self.file = base64.b64decode(body["content"])
            self.blob += 1
            return 200, b"{}"
        if method == "POST" and route == "issues/7/comments":
            self.comments.append({"id": 1000 + len(self.comments), "user": ACTIONS, "body": body["body"], "created_at": "t"})
            return 201, b"{}"
        if method == "PATCH" and route.startswith("issues/comments/"):
            target = int(route.rsplit("/", 1)[1])
            for c in self.comments:
                if c["id"] == target:
                    c["body"] = body["body"]
            return 200, b"{}"
        if method == "POST" and route.startswith("statuses/"):
            self.statuses.append((route.rsplit("/", 1)[1], body))
            return 201, b"{}"
        return 404, b"{}"

    def signed_ids(self) -> set[int]:
        return {e["id"] for e in json.loads(self.file)["signedContributors"]}

    def last_status(self) -> str:
        sha, body = self.statuses[-1]
        assert sha == SHA_C and body["context"] == "CLA"
        return body["state"]


def commit(sha: str, author: dict | None, git_name: str = "someone", coauthors: list | None = None) -> dict:
    """A REST commit entry; ``coauthors`` (user dicts, or ``None`` for an
    unlinked email) only feeds the fake GraphQL answer."""
    entry = {"sha": sha, "author": author, "commit": {"author": {"name": git_name, "email": "x@example.com"}}}
    if coauthors is not None:
        entry["coauthors"] = coauthors
    return entry


def pr_event(fake: FakeGitHub) -> object:
    raw = json.dumps({"repository": {"id": 555}, "pull_request": fake.pull()}).encode()
    return cla.parse_event("pull_request_target", raw)


def comment_event(user: dict, body: str, comment_id: int = 900) -> object:
    raw = json.dumps({
        "repository": {"id": 555},
        "issue": {"number": 7, "pull_request": {"url": "u"}},
        "comment": {"id": comment_id, "user": user, "body": body, "created_at": "2026-10-27T17:00:00Z"},
    }).encode()
    return cla.parse_event("issue_comment", raw)


def drive(fake: FakeGitHub, event: object) -> None:
    cla.run(event, cla.GitHub(fake.send, "bookclerk/bookclerk"), CLA_URL, RUN_URL)


class EndToEndTests(unittest.TestCase):
    """Drives ``run`` over a fake GitHub."""

    def test_unsigned_author_fails_then_signs(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        self.assertEqual(len(fake.comments), 1)
        self.assertIn("@alice", fake.comments[0]["body"])
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(fake.last_status(), "success")
        self.assertEqual(len(fake.comments), 1)
        self.assertNotIn("@alice", fake.comments[0]["body"])
        entry = json.loads(fake.file)["signedContributors"][0]
        self.assertEqual(entry, {"name": "alice", "id": 101, "comment_id": 900, "created_at": "2026-10-27T17:00:00Z", "repoId": 555, "pullRequestNo": 7})

    def test_signed_pull_request_posts_no_comment(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "old-login", "id": 101}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "success")
        self.assertEqual(fake.comments, [])

    def test_maintainers_are_not_exempt(self) -> None:
        fake = FakeGitHub(account("fritz-fritz", 5), [commit(SHA_A, account("fritz-fritz", 5))])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")

    def test_bots_are_exempt(self) -> None:
        fake = FakeGitHub(DEPENDABOT, [commit(SHA_A, DEPENDABOT)])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "success")

    def test_unknown_bot_must_sign(self) -> None:
        other = account("someones-app[bot]", 77, "Bot")
        fake = FakeGitHub(other, [commit(SHA_A, other)])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")

    def test_git_name_cannot_claim_a_bot(self) -> None:
        fake = FakeGitHub(DEPENDABOT, [commit(SHA_A, None, git_name="dependabot[bot]")])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        self.assertIn(SHA_A, fake.comments[0]["body"])
        self.assertNotIn("someone", fake.comments[0]["body"])

    def test_pull_request_author_must_sign_when_commits_are_a_bots(self) -> None:
        fake = FakeGitHub(BOB, [commit(SHA_A, DEPENDABOT)])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        self.assertIn("@bob", fake.comments[0]["body"])

    def test_every_commit_author_must_sign(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE), commit(SHA_B, BOB)], [{"name": "alice", "id": 101}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        body = fake.comments[0]["body"]
        self.assertIn("@bob", body)
        self.assertNotIn("@alice", body)

    def test_linked_co_authors_must_sign(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE, coauthors=[BOB])], [{"name": "alice", "id": 101}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        self.assertIn("@bob", fake.comments[0]["body"])
        drive(fake, comment_event(BOB, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {101, 102})
        self.assertEqual(fake.last_status(), "success")

    def test_unlinked_co_authors_are_covered_by_the_author(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE, coauthors=[None])], [{"name": "alice", "id": 101}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "success")

    def test_assistant_accounts_do_not_sign(self) -> None:
        assistant = {**account("assistant-app", 900001), "email": "NoReply@Anthropic.com"}
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE, coauthors=[assistant])], [{"name": "alice", "id": 101}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "success")

    def test_assistant_author_does_not_sign_but_the_pull_request_author_does(self) -> None:
        assistant = account("assistant-app", 900001)
        fake = FakeGitHub(BOB, [commit(SHA_A, assistant)])
        real = fake.graphql
        fake.graphql = lambda body: (200, real(body)[1].replace(b'"x@example.com"', b'"noreply@anthropic.com"', 1))
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        body = fake.comments[0]["body"]
        self.assertIn("@bob", body)
        self.assertNotIn("assistant-app", body)
        self.assertNotIn(SHA_A, body)

    def test_a_person_cannot_borrow_the_assistant_exemption(self) -> None:
        """An assistant address on another commit exempts only its own account."""
        assistant = {**account("assistant-app", 900001), "email": "noreply@anthropic.com"}
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE, coauthors=[assistant]), commit(SHA_B, BOB)], [{"name": "alice", "id": 101}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        self.assertIn("@bob", fake.comments[0]["body"])

    def test_co_author_query_must_list_the_same_commits(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        real = fake.graphql
        fake.graphql = lambda body: (200, real(body)[1].replace(SHA_A.encode(), SHA_B.encode()))
        with self.assertRaises(cla.ApiError):
            drive(fake, pr_event(fake))
        self.assertEqual(fake.statuses, [])

    def test_signatures_match_by_id_not_login(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 999}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")

    def test_bystander_cannot_sign_on_someone_elses_pull_request(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, comment_event(BOB, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), set())

    def test_inexact_phrase_does_not_sign(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE + " but not really"))
        self.assertEqual(fake.signed_ids(), set())
        self.assertEqual(fake.last_status(), "failure")

    def test_lost_signing_events_are_recovered(self) -> None:
        """A signature whose run was replaced in the queue counts on the next run."""
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE), commit(SHA_B, BOB)])
        for i, user in enumerate((ALICE, BOB, ALICE)):
            fake.comments.append({"id": 50 + i, "user": user, "body": cla.SIGN_PHRASE, "created_at": f"t{i}"})
        drive(fake, comment_event(BOB, cla.RECHECK_PHRASE, comment_id=60))
        self.assertEqual(fake.signed_ids(), {101, 102})
        entries = json.loads(fake.file)["signedContributors"]
        self.assertEqual([e["comment_id"] for e in entries], [50, 51])
        self.assertEqual(fake.last_status(), "success")

    def test_signature_write_retries_after_conflict(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "carol", "id": 3}])
        fake.conflicts = 2
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {3, 101})
        self.assertEqual(fake.last_status(), "success")

    def test_signature_write_gives_up(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.conflicts = cla.SIGNATURE_WRITE_ATTEMPTS
        with self.assertRaises(cla.ApiError):
            drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))

    def test_commits_are_paginated(self) -> None:
        commits = [commit(f"{i:040x}", BOB if i == 150 else ALICE) for i in range(1, 201)]
        commits[180]["coauthors"] = [account("carol", 103)]
        fake = FakeGitHub(ALICE, commits, [{"name": "alice", "id": 101}])
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        self.assertIn("@bob", fake.comments[0]["body"])
        self.assertIn("@carol", fake.comments[0]["body"])

    def test_too_many_commits_fail_closed(self) -> None:
        commits = [commit(f"{i:040x}", ALICE) for i in range(1, cla.MAX_COMMITS + 2)]
        fake = FakeGitHub(ALICE, commits, [{"name": "alice", "id": 101}])
        with self.assertRaises(cla.ApiError):
            drive(fake, pr_event(fake))
        self.assertEqual(fake.statuses, [])

    def test_own_comment_is_found_past_the_first_page(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.comments = [{"id": i, "user": BOB, "body": "hi", "created_at": "t"} for i in range(1, 151)]
        drive(fake, pr_event(fake))
        drive(fake, pr_event(fake))
        ours = [c for c in fake.comments if c["user"] == ACTIONS]
        self.assertEqual(len(ours), 1)

    def test_spoofed_marker_comment_is_not_edited(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.comments = [{"id": 1, "user": BOB, "body": cla.COMMENT_MARKER + "\nmine", "created_at": "t"}]
        drive(fake, pr_event(fake))
        self.assertEqual(fake.comments[0]["body"], cla.COMMENT_MARKER + "\nmine")
        self.assertEqual(len(fake.comments), 2)

    def test_merge_group_reports_on_its_head(self) -> None:
        fake = FakeGitHub(ALICE, [])
        drive(fake, cla.parse_event("merge_group", load("event_merge_group__queued")))
        self.assertEqual(fake.statuses, [(SHA_B, {"state": "success", "context": "CLA", "description": "Checked on each pull request in the group.", "target_url": RUN_URL})])

    def test_plain_issue_comment_does_nothing(self) -> None:
        fake = FakeGitHub(ALICE, [])
        drive(fake, cla.parse_event("issue_comment", load("event_issue_comment__plain_issue")))
        self.assertEqual(fake.requests, [])

    def test_existing_entries_are_kept_verbatim(self) -> None:
        old = {"name": "carol", "id": 3, "comment_id": 1, "created_at": "t", "repoId": 555, "pullRequestNo": 1, "extra": [1]}
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [old])
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(json.loads(fake.file)["signedContributors"][0], old)


class MainTests(unittest.TestCase):
    """The workflow entry point refuses a bad environment before any request."""

    def test_bad_repository(self) -> None:
        self.assertEqual(cla.main({"GITHUB_REPOSITORY": "../evil", "GITHUB_TOKEN": "t"}), 1)

    def test_unreadable_event(self) -> None:
        env = {"GITHUB_REPOSITORY": "bookclerk/bookclerk", "GITHUB_TOKEN": "t", "GITHUB_EVENT_NAME": "push",
               "GITHUB_EVENT_PATH": str(CORPUS / "pull__basic")}
        self.assertEqual(cla.main(env), 1)


if __name__ == "__main__":
    unittest.main()
