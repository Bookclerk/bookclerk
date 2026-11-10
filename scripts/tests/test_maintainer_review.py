#!/usr/bin/env python3
"""Maintainer review check: seed corpus replay, mutation fuzzing and end-to-end runs.

``maintainer_review.py`` parses GitHub documents that carry
contributor-chosen text (logins, branch names, file paths) and decides
whether an outside pull request may merge, so every parser is an
untrusted-input boundary. Each corpus file is named ``<kind>__<case>``;
every corpus input and every mutation of one must be refused with
``ClaInputError`` or read exactly as the oracle reads it. The oracle checks
shapes field by field by hand instead of using the module's patterns. The
end-to-end tests drive ``run`` over a fake GitHub to check who may merge
without an approval, which approvals count, and what the summary lists.
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
CORPUS = Path(__file__).resolve().parent / "maintainer_review_corpus"


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


# `maintainer_review` imports `cla_check` by name, as it does when the
# workflow runs it from scripts/.
cla = _load("cla_check", ROOT / "scripts" / "cla_check.py")
mr = _load("maintainer_review", ROOT / "scripts" / "maintainer_review.py")

SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40
RUN_URL = "https://github.com/bookclerk/bookclerk/actions/runs/1"


class Refused(Exception):
    """The oracle refuses the input."""


# --- Oracle -----------------------------------------------------------------

ALNUM = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
HEX = set("0123456789abcdef")


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


def o_account(value: object) -> tuple:
    obj = o_obj(value)
    account_id = o_id(obj.get("id"))
    login = o_str(obj.get("login"))
    kind = obj.get("type")
    if kind not in ("User", "Bot", "Organization", "Mannequin"):
        raise Refused
    if kind == "Bot":
        if not login.endswith("[bot]") or not o_login(login[: -len("[bot]")]):
            raise Refused
    elif not o_login(login):
        raise Refused
    return (account_id, login, kind)


def o_path(value: object) -> str:
    path = o_str(value)
    if not 1 <= len(path) <= 4096 or path.startswith("/") or not path.isprintable() or ".." in path.split("/"):
        raise Refused
    return path


def oracle(kind: str, raw: bytes) -> object:
    """Reads ``raw`` as a document of ``kind``, independently of the module."""
    doc = o_json(raw)
    if kind.startswith("event_"):
        payload = o_obj(doc)
        if kind == "event_workflow_run":
            run = o_obj(payload.get("workflow_run"))
            if run.get("event") not in ("pull_request", "pull_request_review"):
                raise Refused
            repo = o_obj(run.get("head_repository"))
            owner = o_str(o_obj(repo.get("owner")).get("login"))
            if not o_login(owner):
                raise Refused
            branch = o_str(run.get("head_branch"))
            if not 1 <= len(branch) <= 255 or not branch.isprintable():
                raise Refused
            return ("run", o_sha(run.get("head_sha")), branch, o_id(repo.get("id")), owner)
        if kind == "event_merge_group":
            return ("group", o_sha(o_obj(payload.get("merge_group")).get("head_sha")))
        run = o_obj(payload.get("check_run"))
        sha = o_sha(run.get("head_sha"))
        slug = o_str(o_obj(run.get("app")).get("slug"))
        if o_str(run.get("name")) != "Maintainer review":
            return ("rerun", None, None)
        return ("rerun", sha, slug)
    if kind == "permission":
        permission = o_str(o_obj(doc).get("permission"))
        if permission not in ("admin", "write", "read", "none"):
            raise Refused
        return permission
    if kind == "reviews":
        if not isinstance(doc, list):
            raise Refused
        out = []
        for entry in doc:
            obj = o_obj(entry)
            state = obj.get("state")
            if state not in ("APPROVED", "CHANGES_REQUESTED", "COMMENTED", "DISMISSED", "PENDING"):
                raise Refused
            commit = obj.get("commit_id")
            commit = None if commit is None else o_sha(commit)
            if obj.get("user") is None:
                continue
            out.append((o_account(obj.get("user")), state, commit))
        return (out, len(doc))
    if kind == "files":
        if not isinstance(doc, list):
            raise Refused
        out = []
        for entry in doc:
            obj = o_obj(entry)
            status = o_str(obj.get("status"))
            if status not in ("added", "removed", "modified", "renamed", "copied", "changed", "unchanged"):
                raise Refused
            previous = obj.get("previous_filename")
            patch = obj.get("patch")
            if patch is not None:
                o_str(patch)
            path = o_path(obj.get("filename"))
            out.append((path, None if previous is None else o_path(previous), status == "removed", patch is not None))
        return out
    raise AssertionError(kind)


def acc(account) -> tuple:
    """An ``Account`` as the oracle's tuple."""
    return (account.id, account.login, account.type.value)


def module(kind: str, raw: bytes) -> object:
    """Reads ``raw`` with the module, shaped like the oracle's answer."""
    if kind.startswith("event_"):
        event = mr.parse_event(kind.removeprefix("event_"), raw)
        if kind == "event_workflow_run":
            h = event.run_head
            return ("run", h.sha, h.branch, h.repo_id, h.owner)
        if kind == "event_merge_group":
            return ("group", event.merge_group_sha)
        return ("rerun", event.rerun_sha, event.rerun_app)
    if kind == "permission":
        return mr.parse_permission(raw)
    if kind == "reviews":
        reviews, entries = mr.parse_review_page(raw)
        return ([(acc(r.author), r.state.value, r.commit_id) for r in reviews], entries)
    if kind == "files":
        return [(f.path, f.previous_path, f.removed, f.has_patch) for f in mr.parse_file_page(raw)]
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
            "event_workflow_run__push", "event_workflow_run__bad_owner", "event_workflow_run__bad_branch",
            "event_workflow_run__long_branch", "event_workflow_run__upper_sha", "event_workflow_run__not_object",
            "event_workflow_run__invalid_utf8", "event_merge_group__bad_sha", "event_check_run__no_app",
            "permission__maintain", "permission__missing", "permission__not_object",
            "reviews__unknown_state", "reviews__bad_commit", "reviews__bot_login_as_user", "reviews__not_list",
            "files__absolute", "files__dotdot", "files__unknown_status", "files__patch_not_string",
            "files__newline", "files__not_list",
        ):
            kind = name.partition("__")[0]
            self.assertEqual(outcome(kind, load(name), module), ("refused",), name)

    def test_expected_reads(self) -> None:
        self.assertEqual(module("event_workflow_run", load("event_workflow_run__review")), ("run", SHA_A, "feature", 700, "alice"))
        self.assertEqual(module("event_merge_group", load("event_merge_group__basic")), ("group", SHA_B))
        self.assertEqual(module("event_check_run", load("event_check_run__ours")), ("rerun", SHA_A, "bookclerk"))
        self.assertEqual(module("event_check_run", load("event_check_run__other")), ("rerun", None, None))
        self.assertEqual(module("permission", load("permission__write")), "write")
        reviews, entries = module("reviews", load("reviews__mixed"))
        # The review by a deleted account is left out but still counted.
        self.assertEqual(entries, 6)
        self.assertEqual(len(reviews), 5)
        files = mr.parse_file_page(load("files__mixed"))
        self.assertEqual(
            [f.path for f in files if mr.is_sensitive(f)],
            [".github/workflows/ci.yml", "crates/x/build.rs", "tests/fixtures/sample.m4b", "old/name.rs"],
        )

    def test_unknown_event_is_refused(self) -> None:
        with self.assertRaises(cla.ClaInputError):
            mr.parse_event("pull_request_target", load("event_workflow_run__pull_request"))


# --- Fuzzing -------------------------------------------------------------------

#: Values mutations put in place of a JSON value.
VALUES: list[object] = [
    None, True, False, 0, 1, -1, 7.0, 2**53, 2**53 - 1, 101, "", "x", SHA_A, SHA_A.upper(), SHA_A[:39],
    "alice", "-alice", "a" * 40, "dependabot[bot]", "evil[bot]", "[bot]", "User", "Bot", "Organization",
    "APPROVED", "CHANGES_REQUESTED", "DISMISSED", "COMMENTED", "PENDING", "approved", "admin", "write",
    "maintain", "read", "none", "added", "removed", "renamed", ".github/workflows/ci.yml", "build.rs",
    "/abs", "a/../b", "a\nb", "pull_request", "pull_request_review", "push", "Maintainer review",
    "a`b", [], {}, [1], {"id": 1},
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


def o_sensitive(path: str, previous: str | None, removed: bool, has_patch: bool) -> bool:
    """An independent reading of which changed files need reading first."""
    prefixes = [".github/", ".cargo/", ".devcontainer/", "scripts/", "packaging/", "tools/lint/", "fuzz/corpus/"]
    names = {
        "build.rs", "Cargo.toml", "Cargo.lock", "package.json", "package-lock.json", "pyproject.toml", "uv.lock",
        "requirements.txt", "Dockerfile", "Containerfile", "wrangler.toml", "rust-toolchain.toml", ".tool-versions",
        ".npmrc", ".gitattributes", ".gitmodules",
    }

    def flagged(p: str) -> bool:
        return any(p[: len(x)] == x for x in prefixes) or p.split("/")[-1] in names

    return flagged(path) or (previous is not None and flagged(previous)) or (not removed and not has_patch)


class MutationFuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 6000

    def test_mutations_match_oracle(self) -> None:
        cases = corpus()
        rng = random.Random(20261109)
        for n in range(self.ITERATIONS):
            kind, case, raw = rng.choice(cases)
            mutated = mutate(rng, raw)
            assert_matches(self, kind, mutated, f"iteration {n} from {kind}__{case}: {mutated[:200]!r}")

    def test_fuzzed_files_are_flagged_as_the_oracle_says(self) -> None:
        """The flagged paths and the summary agree with an independent reading."""
        pages = [raw for kind, _, raw in corpus() if kind == "files"]
        pull = cla.PullRequest(number=1, author=OUTSIDER, head_sha=SHA_A, commits=1)
        rng = random.Random(1109)
        for n in range(2000):
            raw = mutate(rng, rng.choice(pages))
            try:
                files = mr.parse_file_page(raw)
            except cla.ClaInputError:
                continue
            verdict = mr.evaluate(pull, "read", [], {}, files)
            expected = tuple(f.path for f in files if o_sensitive(f.path, f.previous_path, f.removed, f.has_patch))
            self.assertEqual(verdict.sensitive, expected, f"iteration {n}")
            self.assertFalse(verdict.passed, f"iteration {n}")
            summary = mr.render_summary(verdict)
            # Paths sit in code spans that a path cannot close.
            for line in summary.splitlines():
                if line.startswith("- `"):
                    self.assertEqual(line.count("`"), 2, line)


# --- End to end ----------------------------------------------------------------


def account(login: str, account_id: int, kind: str = "User") -> dict:
    return {"login": login, "id": account_id, "type": kind}


OWNER = account("fritz-fritz", 5253894)
OTHER_MAINTAINER = account("maint", 300)
OUTSIDER_DOC = account("alice", 101)
OUTSIDER = cla.parse_account(OUTSIDER_DOC)
TRIAGER = account("tri", 200)
DEPENDABOT = account("dependabot[bot]", 49699333, "Bot")
IMPOSTOR_BOT = account("evil[bot]", 666, "Bot")


class FakeGitHub:
    """A GitHub REST API in memory, enough for the check."""

    def __init__(self) -> None:
        self.pulls: dict[int, dict] = {}
        self.reviews: dict[int, list[dict]] = {}
        self.files: dict[int, list[dict]] = {}
        self.permissions = {"fritz-fritz": "admin", "maint": "write", "tri": "read", "alice": "read"}
        self.checks: list[dict] = []
        self.labels: dict[int, set[str]] = {}
        self.calls: list[tuple[str, str]] = []
        self.fail_paths: set[str] = set()
        self.label_status = 200

    def add_pull(self, number: int, author: dict, sha: str = SHA_A, owner: str = "alice", repo_id: int = 700, branch: str = "feature") -> None:
        self.pulls[number] = {
            "number": number,
            "state": "open",
            "user": author,
            "commits": 1,
            "head": {"sha": sha, "ref": branch, "repo": {"id": repo_id}, "label": f"{owner}:{branch}"},
        }
        self.reviews.setdefault(number, [])
        self.files.setdefault(number, [{"filename": "crates/x/src/lib.rs", "status": "modified", "patch": "@@"}])

    def review(self, number: int, who: dict, state: str, sha: str = SHA_A) -> None:
        self.reviews[number].append({"id": len(self.reviews[number]) + 1, "user": who, "state": state, "commit_id": sha})

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
        if method == "GET" and parts[0] == "pulls" and parts[2] in ("reviews", "files"):
            items = (self.reviews if parts[2] == "reviews" else self.files)[int(parts[1])]
            page, size = int(params.get("page", "1")), int(params["per_page"])
            return 200, json.dumps(items[(page - 1) * size: page * size]).encode()
        if method == "GET" and parts[0] == "commits" and parts[2] == "pulls":
            # Like GitHub, the commit listing leaves out fork pull requests.
            refs = [p for p in self.pulls.values() if p["head"]["sha"] == parts[1] and p["head"]["repo"]["id"] == 555]
            return 200, json.dumps(refs).encode()
        if method == "GET" and parts[0] == "collaborators":
            permission = self.permissions.get(parts[1], "none")
            return 200, json.dumps({"permission": permission, "role_name": permission}).encode()
        if method == "POST" and route == "check-runs":
            self.checks.append(body)
            return 201, json.dumps({"id": len(self.checks)}).encode()
        if parts[0] == "issues" and parts[2] == "labels":
            number = int(parts[1])
            if self.label_status != 200:
                return self.label_status, b"{}"
            if method == "POST":
                self.labels.setdefault(number, set()).update(body["labels"])
                return 200, b"[]"
            if method == "DELETE":
                if parts[3] not in self.labels.get(number, set()):
                    return 404, b"{}"
                self.labels[number].discard(parts[3])
                return 200, b"[]"
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
        env |= {"REVIEW_APP_TOKEN": "app", "REVIEW_APP_SLUG": "bookclerk"}
    original = mr.urllib_transport
    mr.urllib_transport = lambda token, url: gh.send
    try:
        with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
            return mr.main(env)
    finally:
        mr.urllib_transport = original
        Path(handle.name).unlink()


def trigger(gh: FakeGitHub, sha: str = SHA_A, owner: str = "alice", repo_id: int = 700, event: str = "pull_request") -> int:
    run = {"event": event, "head_sha": sha, "head_branch": "feature", "head_repository": {"id": repo_id, "owner": {"login": owner}}}
    return run_event(gh, "workflow_run", {"workflow_run": run})


class EndToEndTests(unittest.TestCase):
    """Runs the check over a fake GitHub."""

    def last(self, gh: FakeGitHub) -> dict:
        self.assertTrue(gh.checks)
        return gh.checks[-1]

    def test_an_outside_pull_request_fails_until_a_maintainer_approves(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        self.assertEqual(trigger(gh), 0)
        self.assertEqual(self.last(gh)["conclusion"], "failure")
        self.assertEqual(self.last(gh)["name"], "Maintainer review")
        self.assertEqual(self.last(gh)["head_sha"], SHA_A)
        gh.review(1, OWNER, "APPROVED")
        self.assertEqual(trigger(gh, event="pull_request_review"), 0)
        self.assertEqual(self.last(gh)["conclusion"], "success")
        self.assertIn("@fritz-fritz", self.last(gh)["output"]["summary"])

    def test_an_approval_of_an_older_commit_does_not_count(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC, sha=SHA_B)
        gh.review(1, OWNER, "APPROVED", sha=SHA_A)
        trigger(gh, sha=SHA_B)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_approvals_without_write_access_do_not_count(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.review(1, TRIAGER, "APPROVED")
        gh.review(1, IMPOSTOR_BOT, "APPROVED")
        gh.review(1, account("stranger", 400), "APPROVED")
        trigger(gh)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_the_author_cannot_approve_their_own_pull_request(self) -> None:
        gh = FakeGitHub()
        gh.permissions["alice"] = "read"
        gh.add_pull(1, OUTSIDER_DOC)
        gh.review(1, OUTSIDER_DOC, "APPROVED")
        trigger(gh)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_a_maintainers_request_for_changes_blocks_another_approval(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.review(1, OTHER_MAINTAINER, "CHANGES_REQUESTED")
        gh.review(1, OWNER, "APPROVED")
        trigger(gh)
        self.assertEqual(self.last(gh)["conclusion"], "failure")
        self.assertEqual(self.last(gh)["output"]["title"], "A maintainer asked for changes.")
        # A later approval from the same reviewer replaces their request.
        gh.review(1, OTHER_MAINTAINER, "APPROVED")
        trigger(gh)
        self.assertEqual(self.last(gh)["conclusion"], "success")

    def test_a_dismissed_approval_no_longer_counts(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.review(1, OWNER, "APPROVED")
        gh.review(1, OWNER, "DISMISSED")
        gh.review(1, OWNER, "COMMENTED")
        trigger(gh)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_authors_with_write_access_and_trusted_bots_pass(self) -> None:
        for author in (OWNER, OTHER_MAINTAINER, DEPENDABOT, account("bookclerk[bot]", 9, "Bot")):
            gh = FakeGitHub()
            gh.add_pull(1, author, owner="bookclerk", repo_id=555)
            trigger(gh, owner="bookclerk", repo_id=555)
            self.assertEqual(self.last(gh)["conclusion"], "success", author)
            self.assertFalse(any("reviews" in p or "files" in p for _, p in gh.calls), author)

    def test_an_unknown_bot_is_an_outside_author(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, IMPOSTOR_BOT)
        trigger(gh)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_sensitive_files_are_listed_and_labelled(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.files[1] = json.loads(load("files__mixed"))
        trigger(gh)
        check = self.last(gh)
        self.assertEqual(check["output"]["title"], "Needs a maintainer's approval; changes CI or build files.")
        self.assertIn("`.github/workflows/ci.yml`", check["output"]["summary"])
        self.assertNotIn("covers.rs", check["output"]["summary"])
        self.assertEqual(gh.labels[1], {mr.SENSITIVE_LABEL})
        # Once the change leaves those files alone, the label goes.
        gh.files[1] = [{"filename": "docs/a.md", "status": "modified", "patch": "@@"}]
        trigger(gh)
        self.assertEqual(gh.labels[1], set())

    def test_a_label_refusal_does_not_stop_the_check(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.label_status = 403
        self.assertEqual(trigger(gh), 0)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_more_files_than_listed_are_called_out(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.files[1] = [{"filename": f"docs/{i}.md", "status": "modified", "patch": "@@"} for i in range(mr.MAX_LISTED_FILES)]
        trigger(gh)
        self.assertIn(f"more than {mr.MAX_LISTED_FILES} files", self.last(gh)["output"]["summary"])

    def test_a_shared_head_passes_when_any_pull_request_on_it_passes(self) -> None:
        # Another pull request on a maintainer's commit cannot block it: the
        # maintainer vouched for exactly that commit.
        gh = FakeGitHub()
        gh.add_pull(1, OWNER, owner="bookclerk", repo_id=555)
        gh.add_pull(2, OUTSIDER_DOC, owner="bookclerk", repo_id=555, branch="other")
        trigger(gh, owner="bookclerk", repo_id=555)
        self.assertEqual(self.last(gh)["conclusion"], "success")
        self.assertTrue(self.last(gh)["output"]["title"].startswith("#1: "))
        self.assertIn("#2 by @alice", self.last(gh)["output"]["summary"])
        gh.pulls[1]["state"] = "closed"
        trigger(gh, owner="bookclerk", repo_id=555)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_another_forks_branch_of_the_same_name_does_not_count(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC, repo_id=701)
        trigger(gh)
        self.assertEqual(gh.checks, [])

    def test_merge_groups_pass(self) -> None:
        gh = FakeGitHub()
        self.assertEqual(run_event(gh, "merge_group", {"merge_group": {"head_sha": SHA_C}}), 0)
        self.assertEqual(self.last(gh)["head_sha"], SHA_C)
        self.assertEqual(self.last(gh)["conclusion"], "success")

    def test_rerun_answers_only_its_own_check(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC, owner="bookclerk", repo_id=555)
        gh.review(1, OWNER, "APPROVED")
        ours = {"name": "Maintainer review", "head_sha": SHA_A, "app": {"slug": "bookclerk"}}
        self.assertEqual(run_event(gh, "check_run", {"check_run": ours}), 0)
        self.assertEqual(self.last(gh)["conclusion"], "success")
        gh.checks.clear()
        for other in ({**ours, "app": {"slug": "someone-else"}}, {**ours, "name": "CI Gate"}):
            run_event(gh, "check_run", {"check_run": other})
        self.assertEqual(gh.checks, [])

    def test_a_failed_read_reports_an_error_on_the_head(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.fail_paths.add("pulls/1/reviews")
        self.assertEqual(trigger(gh), 1)
        self.assertEqual(self.last(gh)["conclusion"], "failure")
        self.assertEqual(self.last(gh)["output"]["title"], mr.ERROR_DESCRIPTION)

    def test_too_many_reviews_fail_closed(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        for _ in range(mr.MAX_REVIEW_PAGES * cla.PAGE_SIZE):
            gh.review(1, TRIAGER, "COMMENTED")
        self.assertEqual(trigger(gh), 1)

    def test_a_closed_pull_request_gets_no_check(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        gh.pulls[1]["state"] = "closed"
        self.assertEqual(trigger(gh), 0)
        self.assertEqual(gh.checks, [])

    def test_the_workflow_token_reports_without_the_app(self) -> None:
        gh = FakeGitHub()
        gh.add_pull(1, OUTSIDER_DOC)
        run = {"event": "pull_request", "head_sha": SHA_A, "head_branch": "feature", "head_repository": {"id": 700, "owner": {"login": "alice"}}}
        self.assertEqual(run_event(gh, "workflow_run", {"workflow_run": run}, app=False), 0)
        self.assertEqual(self.last(gh)["conclusion"], "failure")

    def test_bad_environment_and_events_fail(self) -> None:
        gh = FakeGitHub()
        self.assertEqual(run_event(gh, "pull_request_target", {}), 1)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(mr.main({"GITHUB_REPOSITORY": "nope"}), 1)
            self.assertEqual(
                mr.main({"GITHUB_REPOSITORY": "bookclerk/bookclerk", "REVIEW_APP_TOKEN": "x", "REVIEW_APP_SLUG": "bad slug"}),
                1,
            )


if __name__ == "__main__":
    unittest.main()
