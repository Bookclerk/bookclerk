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
import contextlib
import copy
import importlib.util
import io
import json
import random
import sys
import tempfile
import unittest
from unittest import mock
import urllib.error
import urllib.parse
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
SHA_D = "d" * 40
#: The recheck command for the App the tests report as.
RECHECK = "@bookclerk recheck"
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
    state = obj.get("state")
    if type(state) is not str or state not in ("open", "closed"):
        raise Refused
    return (o_id(obj.get("number")), o_account(obj.get("user")), o_sha(head.get("sha")), commits, state == "open")


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
        if name == "workflow_run":
            run = o_obj(payload.get("workflow_run"))
            if run.get("event") != "pull_request":
                raise Refused
            head_repo = o_obj(run.get("head_repository"))
            owner = o_str(o_obj(head_repo.get("owner")).get("login"))
            if not o_base_login(owner):
                raise Refused
            branch = o_str(run.get("head_branch"))
            if not branch or len(branch) > 255 or not all(ch.isprintable() for ch in branch):
                raise Refused
            return ("run", repo, (o_sha(run.get("head_sha")), branch, o_id(head_repo.get("id")), owner))
        if name == "issue_comment":
            issue = o_obj(payload.get("issue"))
            number = o_id(issue.get("number"))
            comment = o_comment(payload.get("comment"))
            return ("comment", repo, number if issue.get("pull_request") is not None else None, comment)
        if name == "check_run":
            run = o_obj(payload.get("check_run"))
            sha = o_sha(run.get("head_sha"))
            app = o_str(o_obj(run.get("app")).get("slug"))
            if o_str(run.get("name")) != "Bookclerk CLA":
                return ("rerun", repo, None, None)
            return ("rerun", repo, sha, app)
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
        out = []
        for e in page:
            user = o_obj(e).get("user")
            try:
                o_account(user)
            except Refused:
                continue
            out.append(o_comment(e))
        return out
    if kind == "pullrefs":
        page = o_json(raw)
        if type(page) is not list:
            raise Refused
        out = []
        for e in page:
            obj = o_obj(e)
            state = obj.get("state")
            if type(state) is not str or state not in ("open", "closed"):
                raise Refused
            head = o_obj(obj.get("head"))
            head_repo = head.get("repo")
            repo_id = None if head_repo is None else o_id(o_obj(head_repo).get("id"))
            out.append((o_id(obj.get("number")), state == "open", o_sha(head.get("sha")), repo_id))
        return out
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
                    login = user.get("login")
                    kind = "Bot" if type(login) is str and login[-5:] == "[bot]" else "User"
                    account = o_account({"id": user.get("databaseId"), "login": login, "type": kind})
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
        places = set()
        for entry in entries:
            obj = o_obj(entry)
            o_str(obj.get("name"))
            account = o_id(obj.get("id"))
            ids.add(account)
            if type(obj.get("repoId")) is int and type(obj.get("pullRequestNo")) is int:
                places.add((account, obj["repoId"], obj["pullRequestNo"]))
        return (len(entries), frozenset(ids), frozenset(places))
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
    if kind == "gitobject":
        return o_sha(o_obj(o_json(raw)).get("sha"))
    if kind == "reaction":
        return o_id(o_obj(o_json(raw)).get("id"))
    if kind == "checkrun":
        return o_id(o_obj(o_json(raw)).get("id"))
    if kind == "combined":
        statuses = o_obj(o_json(raw)).get("statuses")
        if not isinstance(statuses, list) or len(statuses) > 100:
            raise Refused
        return frozenset(o_str(o_obj(e).get("context")) for e in statuses)
    if kind == "branchrules":
        rules = o_json(raw)
        if not isinstance(rules, list) or len(rules) > 200:
            raise Refused
        kinds = set()
        for rule in rules:
            kind_ = o_str(o_obj(rule).get("type"))
            if not kind_ or len(kind_) > 64 or not kind_[0].islower() or any(c not in "abcdefghijklmnopqrstuvwxyz_" for c in kind_):
                raise Refused
            kinds.add(kind_)
        return frozenset(kinds)
    raise AssertionError(kind)


def acc(a) -> tuple[int, str, str]:
    """Normalizes a module ``Account`` to the oracle's tuple."""
    return (a.id, a.login, a.type.value)


def module(kind: str, raw: bytes) -> object:
    """Reads ``raw`` with the module, normalized to the oracle's shape."""
    def pull(p) -> tuple:
        return (p.number, acc(p.author), p.head_sha, p.commits, p.open)

    def comment(c) -> tuple:
        return (c.id, acc(c.author), c.body, c.created_at)

    if kind == "pull":
        return pull(cla.parse_pull_request(cla._load_json(raw)))
    if kind.startswith("event_"):
        ev = cla.parse_event(kind[len("event_"):], raw)
        if ev.kind is cla.EventKind.WORKFLOW_RUN:
            assert ev.pull_number is None and ev.comment is None
            h = ev.run_head
            return ("run", ev.repo_id, (h.sha, h.branch, h.repo_id, h.owner))
        if ev.kind is cla.EventKind.ISSUE_COMMENT:
            return ("comment", ev.repo_id, ev.pull_number, comment(ev.comment))
        if ev.kind is cla.EventKind.CHECK_RUN:
            assert ev.pull_number is None and ev.comment is None and ev.merge_group_sha is None
            return ("rerun", ev.repo_id, ev.rerun_sha, ev.rerun_app)
        return ("merge", ev.repo_id, ev.merge_group_sha)
    if kind == "commits":
        return [(c.sha, None if c.author is None else acc(c.author)) for c in cla.parse_commit_page(raw)]
    if kind == "comments":
        kept, entries = cla.parse_comment_page(raw)
        assert entries == len(cla._load_json(raw))
        return [comment(c) for c in kept]
    if kind == "pullrefs":
        return [(r.number, r.open, r.head_sha, r.head_repo_id) for r in cla.parse_pull_refs(raw)]
    if kind == "coauthors":
        page = cla.parse_coauthor_page(raw)
        commits = {sha: [acc(a) for a in accounts] for sha, accounts in page.commits.items()}
        return (commits, page.next_cursor, page.assistant_ids)
    if kind == "signatures":
        book = cla.parse_signatures(raw)
        return (len(book.entries), book.signed_ids, book.signed_on)
    if kind == "contents":
        return cla.parse_contents(raw)
    if kind == "gitobject":
        return cla.parse_git_object(raw)
    if kind == "reaction":
        return cla.parse_reaction(raw)
    if kind == "checkrun":
        return cla.parse_check_run(raw)
    if kind == "combined":
        return cla.parse_status_contexts(raw)
    if kind == "branchrules":
        return cla.parse_branch_rules(raw)
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
            "event_workflow_run__not_object", "event_workflow_run__invalid_utf8", "event_workflow_run__push",
            "event_workflow_run__no_repository", "event_workflow_run__bad_owner", "event_workflow_run__bad_branch",
            "event_workflow_run__long_branch", "pullrefs__bad_head_repository",
            "commits__not_list", "commits__missing_sha", "commits__bad_author", "comments__null_body",
            "signatures__missing_key", "signatures__string_id", "signatures__no_name", "signatures__zero_id",
            "signatures__deep", "contents__dir", "contents__bad_base64", "contents__none_encoding",
            "coauthors__errors", "coauthors__too_many", "coauthors__count_mismatch", "coauthors__repeat",
            "coauthors__no_cursor", "coauthors__bad_bot_login", "coauthors__null_pull",
            "coauthors__email_not_string", "gitobject__no_sha", "gitobject__upper_sha", "gitobject__not_object",
            "reaction__no_id", "reaction__string_id", "reaction__not_object",
            "checkrun__no_id", "checkrun__string_id", "checkrun__not_object", "checkrun__negative_id",
            "combined__no_statuses", "combined__context_not_string", "combined__not_object",
            "event_check_run__no_app", "event_check_run__bad_sha",
            "branchrules__not_list", "branchrules__upper_type", "branchrules__no_type",
        ):
            kind = name.partition("__")[0]
            self.assertEqual(outcome(module, kind, load(name), cla.ClaInputError), ("refused",), name)

    def test_expected_reads(self) -> None:
        self.assertEqual(module("commits", load("commits__mixed")), [(SHA_A, (101, "alice", "User")), (SHA_B, None)])
        self.assertEqual(module("signatures", load("signatures__seed")), (0, frozenset(), frozenset()))
        # Some older accounts have logins new accounts cannot take.
        self.assertEqual(module("pull", load("pull__login_double_dash"))[1], (1, "al--ice", "User"))
        self.assertEqual(module("pull", load("pull__login_trailing_dash"))[1], (1, "alice-", "User"))
        self.assertEqual(module("signatures", load("signatures__two")), (2, frozenset({101, 2}), frozenset({(101, 555, 1)})))
        self.assertEqual(module("gitobject", load("gitobject__commit")), SHA_B)
        self.assertEqual(module("reaction", load("reaction__eyes")), 31)
        self.assertEqual(module("checkrun", load("checkrun__created")), 4242)
        self.assertEqual(module("combined", load("combined__legacy")), frozenset({"Bookclerk CLA", "codecov/patch"}))
        self.assertEqual(module("branchrules", load("branchrules__all")), cla.SIGNATURE_BRANCH_RULES)
        data, blob = cla.parse_contents(load("contents__seed"))
        self.assertEqual(data, load("signatures__seed"))
        self.assertEqual(blob, SHA_A)
        self.assertEqual(module("event_issue_comment", load("event_issue_comment__plain_issue"))[2], None)
        self.assertEqual(
            module("event_workflow_run", load("event_workflow_run__opened")),
            ("run", 555, (SHA_A, "feature", 700, "alice")),
        )
        self.assertEqual(
            module("pullrefs", load("pullrefs__head_repository")),
            [(7, True, SHA_C, 700), (8, False, SHA_C, None), (9, True, SHA_A, None)],
        )
        self.assertEqual(
            module("coauthors", load("coauthors__basic")),
            ({SHA_A: [(101, "alice", "User"), (102, "bob", "User")], SHA_B: []}, None, frozenset()),
        )
        self.assertEqual(module("coauthors", load("coauthors__next_page"))[1], "Y3Vyc29yOnYyOpHOAAAAAQ==")
        # GraphQL links a bot's commit to a `User` whose login ends in `[bot]`.
        self.assertEqual(
            module("coauthors", load("coauthors__bot_author")),
            ({SHA_A: [(49699333, "dependabot[bot]", "Bot")]}, None, frozenset()),
        )
        self.assertEqual(
            module("coauthors", load("coauthors__assistant")),
            ({SHA_A: [(101, "alice", "User")]}, None, frozenset({900001})),
        )

    def test_unknown_event_is_refused(self) -> None:
        with self.assertRaises(cla.ClaInputError):
            cla.parse_event("pull_request_target", load("event_workflow_run__opened"))


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
        # Co-authors come from GraphQL, so the commit page parser never sets
        # them; draw them (and the assistant flag) at random instead.
        pool = [
            cla.parse_account(a)
            for a in (ALICE, BOB, DEPENDABOT, account("carol", 103), account("evil[bot]", 104, "Bot"))
        ]
        assistant = cla.parse_account(account("assistant", 105))
        allowed_bots = {"dependabot[bot]", "bookclerk[bot]", "github-actions[bot]"}
        rng = random.Random(491)
        checked = 0
        for _ in range(self.ITERATIONS):
            try:
                parsed = cla.parse_commit_page(mutate(rng, rng.choice(pages)))
            except cla.ClaInputError:
                continue
            commits = []
            for c in parsed:
                # An assistant-authored commit credits the (unsigned) assistant
                # account, so a wrongly required assistant shows up as unsigned.
                by_assistant = c.author is not None and rng.random() < 0.2
                commits.append(
                    cla.Commit(
                        sha=c.sha,
                        author=assistant if by_assistant else c.author,
                        coauthors=tuple(rng.sample(pool, rng.randint(0, 3))),
                        assistant_author=by_assistant,
                    )
                )
            checked += 1
            verdict = cla.evaluate(pull, commits, book)
            linked = [pull.author] + [
                a
                for c in commits
                for a in ((None if c.assistant_author else c.author), *c.coauthors)
                if a is not None
            ]
            # Independent of cla.is_exempt: only allowlisted Bot accounts skip.
            required = {a.id for a in linked if not (a.type.value == "Bot" and a.login in allowed_bots)}
            self.assertEqual({a.id for a in verdict.unsigned}, required - book.signed_ids)
            self.assertEqual(set(verdict.unlinked), {c.sha for c in commits if c.author is None})
            self.assertEqual(verdict.passed, not verdict.unsigned and not verdict.unlinked)
            assert_safe_comment(self, cla.render_comment(verdict, CLA_URL), verdict)
            self.assertLessEqual(len(cla.check_title(verdict)), 140)
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
APP = account("bookclerk[bot]", 900, "Bot")


class FakeGitHub:
    """An in-memory GitHub REST API for one pull request."""

    def __init__(self, author: dict, commits: list[dict], signed: list[dict] | None = None) -> None:
        self.author = author
        self.commit_list = commits
        #: Other pull requests by number: ``{"author", "commits", "head",
        #: "state", "label", "repo"}``; ``label`` is the head's ``owner:branch``.
        self.others: dict[int, dict] = {}
        #: Pull request 7's head commit and state.
        self.head = SHA_C
        self.state = "open"
        self.file = json.dumps({"signedContributors": signed or []}).encode()
        self.blob = 1
        self.comments: list[dict] = []
        #: Each ``Bookclerk CLA`` check run created, oldest first, as ``(sha,
        #: {"state", "context", "description", "target_url"})``; ``state`` is
        #: ``pending`` for an in-progress check run, else its conclusion. The
        #: newest one per commit is what GitHub shows.
        self.statuses: list[tuple[str, dict]] = []
        #: Commits that carry a ``Bookclerk CLA`` commit status from before
        #: the check became a check run, and each status posted to mirror a
        #: check there, as ``(sha, state, creator login)``.
        self.legacy: set[str] = set()
        self.legacy_posts: list[tuple[str, str, str]] = []
        #: Check runs by id: ``(sha, status, creator login)``.
        self.check_runs: dict[int, tuple[str, str, str]] = {}
        #: The body of each check run created, in order.
        self.check_bodies: list[dict] = []
        #: Who posted each of ``statuses``, in order.
        self.status_creators: list[dict] = []
        self.conflicts = 0
        self.requests: list[tuple[str, str]] = []
        #: Runs once, just before the next status is recorded, to stand in
        #: for a concurrent run.
        self.before_status = None
        #: True while a request comes through ``write``, the signature
        #: token's transport; only those may change the signature file.
        self.writing = False
        #: True while a request comes through ``comment``, the App's comment
        #: transport; comments made then are the App's.
        self.commenting = False
        #: Whether ``cla-signatures`` exists, and holds the signature file.
        self.branch = True
        self.has_file = True
        #: Git objects created through the API, by SHA, and the tree the
        #: branch was started with, by path.
        self.objects: dict[str, dict] = {}
        self.branch_tree: dict[str, dict] = {}
        #: The message of the commit that started the branch, and of each
        #: signature written through the contents API.
        self.branch_message: str | None = None
        self.put_messages: list[str] = []
        #: The commits that changed ``CLA.md``, newest first.
        self.cla_commits = [SHA_D]
        #: Reactions on comments by id: ``(comment id, content, login)``, and
        #: the ones removed, in order.
        self.reactions: dict[int, tuple[int, str, str]] = {}
        self.removed_reactions: list[tuple[int, str, str]] = []
        #: The ruleset rule types that apply to ``cla-signatures``.
        self.rules: set[str] = set(cla.SIGNATURE_BRANCH_RULES)
        #: Runs once, just before the branch is created, to stand in for a
        #: concurrent run that created it first.
        self.before_ref = None

    def pull(self) -> dict:
        head = {"sha": self.head, "label": "alice:feature", "repo": {"id": 700}}
        return {"number": 7, "user": self.author, "head": head, "state": self.state, "commits": len(self.commit_list)}

    def pull_of(self, number: int) -> dict | None:
        if number == 7:
            return self.pull()
        other = self.others.get(number)
        if other is None:
            return None
        return {
            "number": number,
            "user": other["author"],
            "head": {
                "sha": other.get("head", SHA_C),
                "label": other.get("label", f"bob:topic-{number}"),
                "repo": {"id": other.get("repo", 800 + number)},
            },
            "state": other.get("state", "open"),
            "commits": len(other["commits"]),
        }

    def commits_of(self, number: int) -> list[dict]:
        return self.commit_list if number == 7 else self.others[number]["commits"]

    def blob_sha(self) -> str:
        return f"{self.blob:040x}"

    def graphql(self, body: dict) -> tuple[int, bytes]:
        variables = body["variables"]
        assert (variables["owner"], variables["name"]) == ("bookclerk", "bookclerk")
        commit_list = self.commits_of(variables["number"])
        start = int(variables["after"] or 0)
        nodes = []
        for c in commit_list[start:start + 100]:
            # GraphQL links bot authors too, as a `User` with the bot's login.
            primary = c["author"] if c["author"] and c["author"]["type"] in ("User", "Bot") else None
            users = [primary, *c.get("coauthors", [])]
            user_nodes = [
                {"email": (u or {}).get("email", "x@example.com"), "user": None if u is None else {"databaseId": u["id"], "login": u["login"]}}
                for u in users
            ]
            nodes.append({"commit": {"oid": c["sha"], "authors": {"totalCount": len(user_nodes), "nodes": user_nodes}}})
        more = start + 100 < len(commit_list)
        info = {"hasNextPage": more, "endCursor": str(start + 100) if more else None}
        page = {"data": {"repository": {"pullRequest": {"commits": {"pageInfo": info, "nodes": nodes}}}}}
        return 200, json.dumps(page).encode()

    def write(self, method: str, path: str, body: dict | None) -> tuple[int, bytes]:
        self.writing = True
        try:
            return self.send(method, path, body)
        finally:
            self.writing = False

    def comment(self, method: str, path: str, body: dict | None) -> tuple[int, bytes]:
        self.commenting = True
        try:
            return self.send(method, path, body)
        finally:
            self.commenting = False

    def commenter_account(self) -> dict:
        return APP if self.commenting else ACTIONS

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

        parts = route.split("/")
        if method == "GET" and route == "pulls":
            assert params["state"] == "open"
            label = urllib.parse.unquote(params["head"])
            listed = [self.pull_of(n) for n in (7, *self.others)]
            return 200, json.dumps([p for p in listed if p["head"]["label"] == label and p["state"] == "open"]).encode()
        if method == "GET" and len(parts) == 2 and parts[0] == "pulls" and self.pull_of(int(parts[1])) is not None:
            return 200, json.dumps(self.pull_of(int(parts[1]))).encode()
        if method == "GET" and len(parts) == 3 and parts[0] == "pulls" and parts[2] == "commits":
            return 200, page_of(self.commits_of(int(parts[1])))
        if method == "GET" and len(parts) == 3 and parts[0] == "commits" and parts[2] == "pulls":
            refs = [{"number": 7, "state": self.state, "head": {"sha": self.head}}]
            for n, o in self.others.items():
                refs.append({"number": n, "state": o.get("state", "open"), "head": {"sha": o.get("head", SHA_C)}})
            return 200, json.dumps(refs).encode()
        if method == "GET" and route == "commits" and params.get("path") == "CLA.md":
            return 200, json.dumps([{"sha": sha, "author": None} for sha in self.cla_commits]).encode()
        if method == "GET" and route == "issues/7/comments":
            return 200, page_of(self.comments)
        if method == "GET" and len(parts) == 3 and parts[0] == "issues" and int(parts[1]) in self.others:
            return 200, page_of(self.others[int(parts[1])].setdefault("comments", []))
        if method == "GET" and route == "contents/signatures/cla.json":
            assert params["ref"] == "cla-signatures"
            if not (self.branch and self.has_file):
                return 404, b'{"message": "Not Found"}'
            content = base64.encodebytes(self.file).decode()
            return 200, json.dumps({"type": "file", "encoding": "base64", "sha": self.blob_sha(), "content": content}).encode()
        if method == "GET" and route == "rules/branches/cla-signatures":
            return 200, json.dumps([{"type": t, "ruleset_id": 1} for t in sorted(self.rules)]).encode()
        if method == "GET" and route == "git/ref/heads/cla-signatures":
            return (200, b"{}") if self.branch else (404, b"{}")
        if method == "POST" and route in ("git/trees", "git/commits", "git/refs"):
            assert self.writing, "the signature branch must be created with the signature token"
            assert body is not None
            if route == "git/refs":
                if self.before_ref is not None:
                    hook, self.before_ref = self.before_ref, None
                    hook()
                assert body["ref"] == "refs/heads/cla-signatures"
                if self.branch:
                    return 422, b'{"message": "Reference already exists"}'
                commit_obj = self.objects[body["sha"]]
                assert commit_obj["parents"] == [], "the branch must start with no history"
                tree = {e["path"]: e for e in self.objects[commit_obj["tree"]]["tree"]}
                assert sorted(tree) == ["README.md", "setup-rulesets.sh", "signatures/cla.json"], sorted(tree)
                assert tree["setup-rulesets.sh"]["mode"] == "100755"
                self.branch_tree = tree
                self.branch_message = commit_obj["message"]
                self.file = tree["signatures/cla.json"]["content"].encode("utf-8")
                self.branch = self.has_file = True
                self.blob += 1
                return 201, b"{}"
            sha = f"{len(self.objects) + 0xb00:040x}"
            self.objects[sha] = body
            return 201, json.dumps({"sha": sha}).encode()
        if method == "PUT" and route == "contents/signatures/cla.json":
            assert self.writing, "the signature file must be written with the signature token"
            assert body is not None and body["branch"] == "cla-signatures"
            self.put_messages.append(body["message"])
            if not self.branch:
                return 404, b'{"message": "Branch not found"}'
            if not self.has_file:
                assert "sha" not in body
                self.file = base64.b64decode(body["content"])
                self.has_file = True
                self.blob += 1
                return 201, b"{}"
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
            self.comments.append({"id": 1000 + len(self.comments), "user": self.commenter_account(), "body": body["body"], "created_at": "t"})
            return 201, b"{}"
        if method == "POST" and len(parts) == 3 and parts[0] == "issues" and int(parts[1]) in self.others:
            thread = self.others[int(parts[1])].setdefault("comments", [])
            thread.append({"id": 2000 + len(thread), "user": self.commenter_account(), "body": body["body"], "created_at": "t"})
            return 201, b"{}"
        if len(parts) >= 4 and parts[:2] == ["issues", "comments"] and parts[3] == "reactions":
            comment_id = int(parts[2])
            if method == "POST":
                assert body is not None
                reaction_id = 5000 + len(self.reactions) + len(self.removed_reactions)
                self.reactions[reaction_id] = (comment_id, body["content"], self.commenter_account()["login"])
                return 201, json.dumps({"id": reaction_id, "content": body["content"]}).encode()
            if method == "DELETE" and len(parts) == 5:
                reaction_id = int(parts[4])
                assert self.reactions[reaction_id][2] == self.commenter_account()["login"]
                self.removed_reactions.append(self.reactions.pop(reaction_id))
                return 204, b""
        if method == "PATCH" and route.startswith("issues/comments/"):
            target = int(route.rsplit("/", 1)[1])
            for c in self.comments:
                if c["id"] == target:
                    assert c["user"] == self.commenter_account(), "only a comment's author may edit it"
                    c["body"] = body["body"]
            return 200, b"{}"
        if method == "GET" and route.startswith("commits/") and route.endswith("/status"):
            sha = route.split("/")[1]
            contexts = ["Bookclerk CLA"] if sha in self.legacy else []
            return 200, json.dumps({"state": "pending", "statuses": [{"context": c, "state": "failure"} for c in ["CI", *contexts]]}).encode()
        if method == "POST" and route.startswith("statuses/"):
            assert body is not None and body["context"] == "Bookclerk CLA"
            self.legacy_posts.append((route.rsplit("/", 1)[1], body["state"], self.commenter_account()["login"]))
            return 201, b"{}"
        if method in ("POST", "PATCH") and route.split("/", 1)[0] == "check-runs":
            assert body is not None and body["name"] == "Bookclerk CLA"
            assert body["output"]["title"] and body["output"]["summary"]
            assert body["started_at"] <= body.get("completed_at", body["started_at"])
            state = "pending" if body["status"] == "in_progress" else body.get("conclusion")
            assert (body["status"], state) in (("in_progress", "pending"), ("completed", "success"), ("completed", "failure"))
            if method == "PATCH":
                # Completing an older run changes nothing GitHub shows: the
                # newest run of a name is the one that counts.
                run_id = int(route.rsplit("/", 1)[1])
                sha, status, creator = self.check_runs[run_id]
                assert "head_sha" not in body and state != "pending"
                assert status == "in_progress", "only an in-progress check is completed in place"
                assert creator == self.commenter_account()["login"], "only a check run's App may update it"
                self.check_runs[run_id] = (sha, "completed", creator)
                return 200, json.dumps({"id": run_id, "status": "completed"}).encode()
            if self.before_status is not None and state != "pending":
                hook, self.before_status = self.before_status, None
                hook()
            run_id, sha = 7000 + len(self.check_runs), body["head_sha"]
            self.check_runs[run_id] = (sha, body["status"], self.commenter_account()["login"])
            self.statuses.append((sha, {"state": state, "context": body["name"], "description": body["output"]["title"], "target_url": body["details_url"]}))
            self.check_bodies.append(body)
            self.status_creators.append(self.commenter_account())
            return 201, json.dumps({"id": run_id, "status": body["status"]}).encode()
        return 404, b"{}"

    def signed_ids(self) -> set[int]:
        return {e["id"] for e in json.loads(self.file)["signedContributors"]}

    def final_statuses(self) -> list[tuple[str, dict]]:
        """The statuses posted, without the ``pending`` ones a run starts with."""
        return [(sha, body) for sha, body in self.statuses if body["state"] != "pending"]

    def last_status(self) -> str:
        sha, body = self.statuses[-1]
        assert sha == SHA_C and body["context"] == "Bookclerk CLA"
        return body["state"]


def commit(sha: str, author: dict | None, git_name: str = "someone", coauthors: list | None = None) -> dict:
    """A REST commit entry; ``coauthors`` (user dicts, or ``None`` for an
    unlinked email) only feeds the fake GraphQL answer."""
    entry = {"sha": sha, "author": author, "commit": {"author": {"name": git_name, "email": "x@example.com"}}}
    if coauthors is not None:
        entry["coauthors"] = coauthors
    return entry


def run_event(fake: FakeGitHub, number: int = 7, sha: str | None = None, repo: int | None = None) -> object:
    """The ``workflow_run`` event of a ``CLA trigger`` run for pull request
    ``number``, by default on its current head."""
    pull = fake.pull_of(number)
    owner, branch = pull["head"]["label"].split(":", 1)
    raw = json.dumps({"repository": {"id": 555}, "workflow_run": {
        "event": "pull_request",
        "head_sha": sha or pull["head"]["sha"],
        "head_branch": branch,
        "head_repository": {"id": repo or pull["head"]["repo"]["id"], "owner": {"login": owner}},
    }}).encode()
    return cla.parse_event("workflow_run", raw)


def pr_event(fake: FakeGitHub) -> object:
    return run_event(fake)


def comment_event(user: dict, body: str, comment_id: int = 900) -> object:
    raw = json.dumps({
        "repository": {"id": 555},
        "issue": {"number": 7, "pull_request": {"url": "u"}},
        "comment": {"id": comment_id, "user": user, "body": body, "created_at": "2026-10-27T17:00:00Z"},
    }).encode()
    return cla.parse_event("issue_comment", raw)


def pr_event_for(fake: FakeGitHub, number: int) -> object:
    return run_event(fake, number)


#: The files that start the signature branch, as the workflow checks them out.
BRANCH_FILES = cla.load_branch_files(str(Path(cla.__file__).resolve().parent / "cla_branch"))


def drive(fake: FakeGitHub, event: object) -> None:
    reporter = cla.Reporter(fake.comment, APP["login"])
    cla.run(event, cla.GitHub(fake.send, "bookclerk/bookclerk", fake.write, BRANCH_FILES, reporter), CLA_URL, RUN_URL)


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
        self.assertNotIn("Still to sign", fake.comments[0]["body"])
        self.assertIn("Signed on this pull request: @alice.", fake.comments[0]["body"])
        entry = json.loads(fake.file)["signedContributors"][0]
        self.assertEqual(entry, {"name": "alice", "id": 101, "comment_id": 900, "created_at": "2026-10-27T17:00:00Z", "repoId": 555, "pullRequestNo": 7, "claUrl": f"https://github.com/bookclerk/bookclerk/blob/{SHA_D}/CLA.md"})
        # The agreement's history is read once per run, not per signature.
        self.assertEqual(sum(1 for m, p in fake.requests if p.startswith("repos/bookclerk/bookclerk/commits?path=CLA.md")), 1)

    def test_a_missing_branch_counts_as_nobody_signed(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.branch = False
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "failure")
        self.assertFalse(fake.branch)
        self.assertEqual(fake.objects, {})

    def test_the_first_signature_creates_the_branch_without_history(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE, coauthors=[BOB])])
        fake.branch = False
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertTrue(fake.branch)
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(fake.last_status(), "failure")
        # The branch explains itself with the default branch's templates,
        # in a commit that names no signer; the signature is the next one.
        for name, _ in cla.BRANCH_FILES:
            self.assertEqual(fake.branch_tree[name]["content"].encode(), BRANCH_FILES[name])
        self.assertEqual(json.loads(fake.branch_tree["signatures/cla.json"]["content"]), {"signedContributors": []})
        self.assertEqual(fake.branch_message, cla.BRANCH_START_MESSAGE)
        self.assertNotIn("alice", cla.BRANCH_START_MESSAGE)
        self.assertEqual(fake.put_messages, ["chore(cla): record @alice's signature from #7"])
        # Later signatures append to it through the contents API.
        objects = len(fake.objects)
        drive(fake, comment_event(BOB, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {101, 102})
        self.assertEqual(len(fake.objects), objects)
        self.assertEqual(fake.last_status(), "success")

    def test_a_branch_created_by_a_concurrent_run_is_appended_to(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE, coauthors=[BOB])])
        fake.branch = False

        def bob_signs_first() -> None:
            fake.branch = fake.has_file = True
            fake.file = json.dumps({"signedContributors": [{"name": "bob", "id": 102}]}).encode()
            fake.blob += 1

        fake.before_ref = bob_signs_first
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {101, 102})
        self.assertEqual(fake.last_status(), "success")

    def test_a_branch_without_the_file_gets_it_created(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.has_file = False
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(fake.objects, {})

    def test_the_branch_is_not_started_without_its_readme(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.branch = False
        api = cla.GitHub(fake.send, "bookclerk/bookclerk", fake.write, {"README.md": b"x"})
        with self.assertRaises(cla.ApiError):
            cla.run(comment_event(ALICE, cla.SIGN_PHRASE), api, CLA_URL, RUN_URL)
        self.assertFalse(fake.branch)

    def test_a_refused_branch_is_reported_not_retried(self) -> None:
        # A ruleset refuses the new ref (422) and the branch stays missing.
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.branch = False
        real_send = fake.send

        def refuse_refs(method: str, path: str, body: dict | None) -> tuple[int, bytes]:
            if path.endswith("/git/refs"):
                return 422, b'{"message": "Repository rule violations found"}'
            return real_send(method, path, body)

        fake.send = refuse_refs  # type: ignore[method-assign]
        with self.assertRaises(cla.ApiError) as raised:
            drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertIn("does not exist", str(raised.exception))
        self.assertEqual(len(fake.objects), 2)

    def test_missing_branch_rules_are_warned_about(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        api = cla.GitHub(fake.send, "bookclerk/bookclerk")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            cla.warn_about_branch_rules(api)
        self.assertEqual(out.getvalue(), "")
        fake.rules -= {"update", "required_signatures"}
        with contextlib.redirect_stdout(out):
            cla.warn_about_branch_rules(api)
        self.assertIn("::warning::cla-signatures is missing ruleset rules: required_signatures, update.", out.getvalue())

    def test_without_a_signature_token_a_signature_fails_closed(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        api = cla.GitHub(fake.send, "bookclerk/bookclerk")
        with self.assertRaises(cla.ApiError):
            cla.run(comment_event(ALICE, cla.SIGN_PHRASE), api, CLA_URL, RUN_URL)
        self.assertEqual(fake.signed_ids(), set())
        self.assertNotIn(("PUT", "repos/bookclerk/bookclerk/contents/signatures/cla.json"), fake.requests)
        self.assertEqual(fake.final_statuses(), [])

    def test_a_shared_head_passes_only_when_every_pull_request_passes(self) -> None:
        # Bob opens #8 on alice's signed head; a recheck on #7 must not
        # turn the shared status green while bob has not signed.
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        fake.others[8] = {"author": BOB, "commits": [commit(SHA_A, ALICE)]}
        drive(fake, comment_event(ALICE, RECHECK))
        self.assertEqual(fake.last_status(), "failure")
        self.assertIn("#8", fake.statuses[-1][1]["description"])
        self.assertLessEqual(len(fake.statuses[-1][1]["description"]), 140)
        # The comment on #7 says why it is red although everyone there signed.
        self.assertEqual(len(fake.comments), 1)
        body = fake.comments[0]["body"]
        self.assertIn("#8", body)
        self.assertNotIn("@alice", body)
        self.assertNotIn("@bob", body)
        # Once #8 is closed, its closing run lets #7 decide alone.
        fake.others[8]["state"] = "closed"
        drive(fake, pr_event_for(fake, 8))
        self.assertEqual(fake.last_status(), "success")

    def test_a_signature_recorded_by_a_concurrent_run_is_not_overwritten(self) -> None:
        # A trigger run reads the file before alice's comment run records her
        # signature and posts success; the trigger run's stale failure lands
        # after it, and its re-read must put success back.
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])

        def comment_run() -> None:
            drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))

        fake.before_status = comment_run
        drive(fake, run_event(fake))
        self.assertEqual([b["state"] for _, b in fake.final_statuses()], ["success", "failure", "success"])
        self.assertEqual(len(fake.comments), 1)
        self.assertNotIn("Still to sign", fake.comments[0]["body"])
        self.assertIn("Signed on this pull request: @alice.", fake.comments[0]["body"])

    def test_a_stale_run_re_decides_its_old_head(self) -> None:
        # #7 was pushed to a new head before its run started; the old head,
        # which signed #8 still has, is decided by #8 alone.
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        fake.head = SHA_B
        fake.others[8] = {"author": ALICE, "commits": [commit(SHA_A, ALICE)]}
        drive(fake, run_event(fake, 7, sha=SHA_C))
        self.assertEqual([(sha, b["state"]) for sha, b in fake.final_statuses()], [(SHA_C, "success")])
        self.assertEqual(fake.comments, [])

    def test_a_same_named_branch_in_another_repository_does_not_count(self) -> None:
        # The run's head repository must match: #7's branch name and head
        # from another repository only re-decide the commit's shared status.
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, run_event(fake, 7, repo=999))
        self.assertEqual(fake.last_status(), "failure")
        self.assertEqual(fake.comments, [])
        drive(fake, run_event(fake, 7))
        self.assertEqual(len(fake.comments), 1)

    def test_closed_pull_requests_on_a_reused_branch_do_not_count(self) -> None:
        # A fork that opens every pull request from its `main` piles up
        # closed ones with that label; they must not fill the listing.
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        for n in range(8, 8 + 2 * cla.MAX_SHARED_PULLS):
            fake.others[n] = {"author": ALICE, "commits": [commit(SHA_A, ALICE)], "label": "alice:feature", "repo": 700, "state": "closed"}
        drive(fake, run_event(fake))
        self.assertEqual(fake.last_status(), "success")

    def test_too_many_pull_requests_from_one_head_fail_closed(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        for n in range(8, 8 + cla.MAX_SHARED_PULLS):
            fake.others[n] = {"author": ALICE, "commits": [commit(SHA_A, ALICE)], "label": "alice:feature", "repo": 700}
        with self.assertRaises(cla.ApiError):
            drive(fake, run_event(fake))
        self.assertEqual(fake.final_statuses(), [])

    def test_a_closed_pull_request_never_passes_a_shared_head_alone(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        fake.state = "closed"
        fake.others[8] = {"author": BOB, "commits": [commit(SHA_A, ALICE)]}
        drive(fake, comment_event(ALICE, RECHECK))
        self.assertEqual(fake.last_status(), "failure")
        self.assertEqual(fake.comments, [])

    def test_a_closed_pull_request_alone_gets_no_status(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.state = "closed"
        drive(fake, pr_event(fake))
        self.assertEqual(fake.final_statuses(), [])
        self.assertEqual(fake.comments, [])

    def test_closed_or_moved_pull_requests_do_not_hold_a_head(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        fake.others[8] = {"author": BOB, "commits": [commit(SHA_A, ALICE)], "state": "closed"}
        fake.others[9] = {"author": BOB, "commits": [commit(SHA_A, ALICE)], "head": SHA_B}
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "success")
        self.assertNotIn(("GET", "repos/bookclerk/bookclerk/pulls/8"), fake.requests)

    def test_a_signed_shared_head_passes(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}, {"name": "bob", "id": 102}])
        fake.others[8] = {"author": BOB, "commits": [commit(SHA_A, ALICE)]}
        drive(fake, pr_event(fake))
        self.assertEqual(fake.last_status(), "success")

    def test_too_many_pull_requests_on_one_head_fail_closed(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        for n in range(8, 8 + cla.MAX_SHARED_PULLS + 1):
            fake.others[n] = {"author": ALICE, "commits": [commit(SHA_A, ALICE)]}
        with self.assertRaises(cla.ApiError):
            drive(fake, pr_event(fake))
        self.assertEqual(fake.final_statuses(), [])

    def test_odd_comment_authors_do_not_stop_the_check(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.comments += [
            {"id": 1, "user": None, "body": cla.SIGN_PHRASE, "created_at": "t"},
            {"id": 2, "user": account("Copilot", 198982749, "Bot"), "body": "Summary", "created_at": "t"},
        ]
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE + "\n"))
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(fake.last_status(), "success")

    def test_odd_comment_authors_do_not_end_pagination_early(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.comments += [{"id": n, "user": None, "body": "ghost", "created_at": "t"} for n in range(1, 101)]
        fake.comments.append({"id": 500, "user": ALICE, "body": cla.SIGN_PHRASE, "created_at": "t"})
        drive(fake, comment_event(BOB, RECHECK))
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(fake.last_status(), "success")

    def test_without_the_agreements_history_a_signature_fails_closed(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.cla_commits = []
        with self.assertRaises(cla.ApiError):
            drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), set())

    def test_a_failed_comment_run_still_knows_the_head(self) -> None:
        # main() reports ``error`` on ``api.head_sha``; a comment event
        # carries no head, so run() must record it before anything can fail.
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.file = b"not json"
        api = cla.GitHub(fake.send, "bookclerk/bookclerk")
        with self.assertRaises(cla.ClaInputError):
            cla.run(comment_event(ALICE, cla.SIGN_PHRASE), api, CLA_URL, RUN_URL)
        self.assertEqual(api.head_sha, SHA_C)

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
        self.assertEqual(fake.final_statuses(), [])

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
        drive(fake, comment_event(BOB, RECHECK, comment_id=60))
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
        self.assertEqual(fake.final_statuses(), [])

    def test_own_comment_is_found_past_the_first_page(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.comments = [{"id": i, "user": BOB, "body": "hi", "created_at": "t"} for i in range(1, 151)]
        drive(fake, pr_event(fake))
        drive(fake, pr_event(fake))
        ours = [c for c in fake.comments if c["user"] == APP]
        self.assertEqual(len(ours), 1)

    def test_the_comment_is_posted_as_the_app(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, pr_event(fake))
        self.assertEqual([c["user"] for c in fake.comments], [APP])
        self.assertTrue(fake.comments[0]["body"].startswith(cla.COMMENT_MARKER))

    def test_a_signature_comment_is_reacted_to_as_it_is_recorded(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        event = comment_event(ALICE, cla.SIGN_PHRASE)
        drive(fake, event)
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(list(fake.reactions.values()), [(event.comment.id, "+1", "bookclerk[bot]")])
        self.assertEqual(fake.removed_reactions, [(event.comment.id, "eyes", "bookclerk[bot]")])
        # An already recorded signature is not reacted to again.
        drive(fake, event)
        self.assertEqual(len(fake.reactions), 1)

    def test_refused_reactions_do_not_stop_the_signature(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        real_send = fake.send

        def refuse_reactions(method: str, path: str, body: dict | None) -> tuple[int, bytes]:
            if "/reactions" in path:
                return 403, b"{}"
            return real_send(method, path, body)

        fake.send = refuse_reactions  # type: ignore[method-assign]
        with contextlib.redirect_stdout(io.StringIO()) as out:
            drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(fake.last_status(), "success")
        self.assertIn("::warning::cannot add the eyes reaction", out.getvalue())

    def test_reaction_network_failures_do_not_stop_the_signature(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        real_send = fake.send

        def drop_reactions(method: str, path: str, body: dict | None) -> tuple[int, bytes]:
            if "/reactions" in path:
                raise urllib.error.URLError("timed out")
            return real_send(method, path, body)

        fake.send = drop_reactions  # type: ignore[method-assign]
        with contextlib.redirect_stdout(io.StringIO()) as out:
            drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.signed_ids(), {101})
        self.assertEqual(fake.last_status(), "success")
        self.assertIn("timed out", out.getvalue())

    def test_the_status_is_posted_as_the_app(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, pr_event(fake))
        drive(fake, cla.parse_event("merge_group", load("event_merge_group__queued")))
        self.assertEqual([b["state"] for _, b in fake.statuses], ["pending", "failure", "success"])
        self.assertEqual(fake.status_creators, [APP, APP, APP])

    def test_a_check_shows_progress_before_its_verdict(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, pr_event(fake))
        (sha, first), (_, last) = fake.statuses[0], fake.statuses[-1]
        self.assertEqual((sha, first["state"], first["description"]), (SHA_C, "pending", cla.CHECKING_DESCRIPTION))
        self.assertEqual((last["state"], last["description"]), ("failure", "1 still to sign."))
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual(fake.statuses[-1][1]["description"], "Everyone has signed.")

    def test_a_verdict_is_a_new_check_run_and_completes_the_one_in_progress(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, pr_event(fake))
        self.assertEqual([r for r in fake.requests if "check-runs" in r[1]], [
            ("POST", "repos/bookclerk/bookclerk/check-runs"),
            ("POST", "repos/bookclerk/bookclerk/check-runs"),
            ("PATCH", "repos/bookclerk/bookclerk/check-runs/7000"),
        ])
        self.assertEqual(fake.check_runs, {7000: (SHA_C, "completed", APP["login"]), 7001: (SHA_C, "completed", APP["login"])})
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertEqual({status for _, status, _ in fake.check_runs.values()}, {"completed"})
        self.assertEqual(fake.last_status(), "success")

    def test_the_comment_names_only_who_signed_on_this_pull_request(self) -> None:
        elsewhere = {"name": "bob", "id": 102, "repoId": 555, "pullRequestNo": 3}
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE), commit(SHA_B, BOB)], [elsewhere])
        drive(fake, pr_event(fake))
        self.assertNotIn("Signed on this pull request", fake.comments[0]["body"])
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        body = fake.comments[0]["body"]
        self.assertIn("Signed on this pull request: @alice.", body)
        self.assertNotIn("@bob", body)

    def test_re_run_checks_the_pull_requests_on_that_commit(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        drive(fake, cla.parse_event("check_run", load("event_check_run__rerequested")))
        self.assertEqual(fake.last_status(), "success")

    def test_re_run_of_another_check_or_app_does_nothing(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, cla.parse_event("check_run", load("event_check_run__other_check")))
        raw = load("event_check_run__rerequested").replace(b'"slug": "bookclerk"', b'"slug": "impostor"')
        drive(fake, cla.parse_event("check_run", raw))
        self.assertEqual(fake.requests, [])

    def test_the_check_page_says_who_signed_and_who_is_left(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE), commit(SHA_B, BOB)])
        drive(fake, pr_event(fake))
        summary = fake.check_bodies[-1]["output"]["summary"]
        self.assertIn("Still to sign: @alice, @bob.", summary)
        self.assertIn(cla.SIGN_PHRASE, summary)
        self.assertNotIn(cla.COMMENT_MARKER, summary)
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        self.assertIn("Signed on this pull request: @alice.", fake.check_bodies[-1]["output"]["summary"])

    def test_a_verdict_starts_when_the_check_started(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        times = iter(["2026-10-28T21:57:29Z", "2026-10-28T21:57:41Z"])
        reporter = cla.Reporter(fake.comment, APP["login"])
        api = cla.GitHub(fake.send, "bookclerk/bookclerk", fake.write, BRANCH_FILES, reporter)
        api.clock = lambda: next(times)
        cla.run(pr_event(fake), api, CLA_URL, RUN_URL)
        first, last = fake.check_bodies[0], fake.check_bodies[-1]
        self.assertEqual((first["started_at"], "completed_at" in first), ("2026-10-28T21:57:29Z", False))
        self.assertEqual((last["started_at"], last["completed_at"]), ("2026-10-28T21:57:29Z", "2026-10-28T21:57:41Z"))

    def test_a_status_left_from_before_the_check_run_gets_the_same_verdict(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.legacy.add(SHA_C)
        drive(fake, pr_event(fake))
        drive(fake, comment_event(ALICE, cla.SIGN_PHRASE))
        app = APP["login"]
        self.assertEqual(fake.legacy_posts, [(SHA_C, "pending", app), (SHA_C, "failure", app), (SHA_C, "pending", app), (SHA_C, "success", app)])
        self.assertEqual(fake.last_status(), "success")

    def test_no_status_is_posted_where_there_was_none(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        drive(fake, pr_event(fake))
        drive(fake, cla.parse_event("merge_group", load("event_merge_group__queued")))
        self.assertEqual(fake.legacy_posts, [])
        self.assertEqual([b["state"] for _, b in fake.final_statuses()], ["success", "success"])

    def test_a_refused_completion_of_the_run_in_progress_only_warns(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        send = fake.comment

        def comment(method: str, path: str, body: dict | None) -> tuple[int, bytes]:
            return (502, b"") if method == "PATCH" and "check-runs/" in path else send(method, path, body)

        api = cla.GitHub(fake.send, "bookclerk/bookclerk", fake.write, BRANCH_FILES, cla.Reporter(comment, APP["login"]))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            cla.run(pr_event(fake), api, CLA_URL, RUN_URL)
        self.assertIn("cannot complete check run 7000 (HTTP 502)", out.getvalue())
        self.assertEqual(fake.last_status(), "failure")

    def test_an_unreadable_check_run_answer_fails_closed(self) -> None:
        api = cla.GitHub(lambda m, p, b: (201, b"[]"), "bookclerk/bookclerk")
        with self.assertRaises(cla.ClaInputError):
            api.set_check(SHA_C, cla.CheckState.CHECKING, cla.CHECKING_DESCRIPTION, RUN_URL)
        self.assertEqual(api.open_checks, {})

    def test_without_an_app_the_workflow_reports(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        api = cla.GitHub(fake.send, "bookclerk/bookclerk", fake.write, BRANCH_FILES)
        cla.run(pr_event(fake), api, CLA_URL, RUN_URL)
        cla.run(pr_event(fake), api, CLA_URL, RUN_URL)
        self.assertEqual([c["user"] for c in fake.comments], [ACTIONS])
        self.assertEqual(set(map(str, fake.status_creators)), {str(ACTIONS)})

    def test_the_workflows_old_comment_is_left_for_the_apps(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        old = cla.COMMENT_MARKER + "\nold"
        fake.comments = [{"id": 1, "user": ACTIONS, "body": old, "created_at": "t"}]
        drive(fake, pr_event(fake))
        self.assertEqual(fake.comments[0]["body"], old)
        self.assertEqual([c["user"] for c in fake.comments], [ACTIONS, APP])

    def test_spoofed_marker_comment_is_not_edited(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        fake.comments = [{"id": 1, "user": BOB, "body": cla.COMMENT_MARKER + "\nmine", "created_at": "t"}]
        drive(fake, pr_event(fake))
        self.assertEqual(fake.comments[0]["body"], cla.COMMENT_MARKER + "\nmine")
        self.assertEqual(len(fake.comments), 2)

    def test_merge_group_reports_on_its_head(self) -> None:
        fake = FakeGitHub(ALICE, [])
        drive(fake, cla.parse_event("merge_group", load("event_merge_group__queued")))
        self.assertEqual(fake.statuses, [(SHA_B, {"state": "success", "context": "Bookclerk CLA", "description": "Checked on each pull request in the group.", "target_url": RUN_URL})])

    def test_only_the_bots_recheck_command_runs_the_check(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)], [{"name": "alice", "id": 101}])
        for body in ("recheck", "please recheck this", "@someone-else recheck", "@bookclerk recheck now"):
            drive(fake, comment_event(BOB, body))
        self.assertEqual(fake.requests, [])
        drive(fake, comment_event(BOB, "  @Bookclerk RECHECK\n"))
        self.assertEqual(fake.last_status(), "success")

    def test_the_workflow_filter_matches_the_checker(self) -> None:
        workflow = (ROOT / ".github" / "workflows" / "cla.yml").read_text(encoding="utf-8")
        self.assertIn(f"contains(github.event.comment.body, '{cla.SIGN_TRIGGER}')", workflow)
        self.assertIn(f"contains(github.event.comment.body, '{cla.RECHECK_WORD}')", workflow)
        self.assertIn(f"github.event.check_run.name == '{cla.CHECK_NAME}'", workflow)
        self.assertIn("types: [rerequested]", workflow)
        self.assertTrue(cla.SIGN_PHRASE.startswith(cla.SIGN_TRIGGER))

    def test_the_comment_names_the_recheck_command(self) -> None:
        fake = FakeGitHub(ALICE, [commit(SHA_A, ALICE)])
        drive(fake, pr_event(fake))
        self.assertIn("Comment `@bookclerk recheck` to run the check again.", fake.comments[0]["body"])
        self.assertEqual(cla.recheck_command("github-actions[bot]"), "@github-actions recheck")

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

    def test_a_refused_app_status_still_leaves_an_error_on_a_merge_group(self) -> None:
        sent: list[tuple[str, str, str]] = []

        def transport(token: str, api_url: str):
            def send(method: str, path: str, body: dict | None) -> tuple[int, bytes]:
                sent.append((token, method, path))
                return (403, b"{}") if token == "app" else (201, b"{}")
            return send

        with tempfile.TemporaryDirectory() as folder:
            event_path = Path(folder) / "event.json"
            event_path.write_bytes(load("event_merge_group__queued"))
            env = {"GITHUB_REPOSITORY": "bookclerk/bookclerk", "GITHUB_TOKEN": "workflow", "CLA_SIGNATURE_TOKEN": "app",
                   "CLA_APP_SLUG": "bookclerk", "GITHUB_EVENT_NAME": "merge_group", "GITHUB_EVENT_PATH": str(event_path)}
            with mock.patch.object(cla, "urllib_transport", transport), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(cla.main(env), 1)
        self.assertIn(("workflow", "POST", "repos/bookclerk/bookclerk/check-runs"), sent)

    def test_bad_app_slug(self) -> None:
        env = {"GITHUB_REPOSITORY": "bookclerk/bookclerk", "GITHUB_TOKEN": "t", "CLA_SIGNATURE_TOKEN": "s",
               "CLA_APP_SLUG": "evil/../x"}
        self.assertEqual(cla.main(env), 1)


if __name__ == "__main__":
    unittest.main()
