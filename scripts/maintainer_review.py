#!/usr/bin/env python3
"""Requires a maintainer's approval on pull requests from outside authors.

The ``Maintainer review`` workflow runs this on ``workflow_run`` after the
unprivileged ``Maintainer review trigger`` workflow ran for a pull request
(opened, pushed to, reopened) or a review of one (submitted, dismissed), on
**Re-run** of the ``Maintainer review`` check run (``check_run``), and on
``merge_group``. It reads only pull request metadata through the REST API and
never checks out or runs pull request code.

The rule: a pull request passes when its author can push to the repository
(``write`` or ``admin`` permission) or is one of the bots in
``TRUSTED_BOTS``. Any other author's pull request passes only once someone
with that permission, other than the author, has approved its current head
commit, and no such reviewer's latest verdict asks for changes. A push moves
the head, so every push needs a fresh approval.

A fork pull request runs its own copy of the CI workflows, on the pull
request and again in the merge queue, so a green CI run proves nothing about
a change to those workflows or to anything they execute. For an outside
author the check therefore also lists the changed files that run in CI or at
build time, or that cannot be read as text (``is_sensitive``), in its
summary, and labels the pull request with ``SENSITIVE_LABEL``, so the
approval is given knowing what to read.

The verdict is the ``Maintainer review`` check run on the head commit,
created through the Checks API with the automation App's token so rulesets
can require it from that App. The check belongs to the commit, and what it
vouches for is the commit: it passes when any open pull request with that
head passes, since a maintainer who opened or approved one of them vouched
for exactly that commit. Someone opening another pull request on the same
commit therefore cannot block it.

Everything that parses bytes from GitHub (the event payload, permission
answers, review and file pages) is sans-IO and fuzzed by
``scripts/tests/test_maintainer_review.py``; it shares ``cla_check``'s JSON
readers, so both checks refuse the same malformed documents. Any input
outside the expected shape fails closed.

Usage (inside the workflow): ``maintainer_review.py`` with ``GITHUB_TOKEN``,
``GITHUB_REPOSITORY``, ``GITHUB_EVENT_NAME``, ``GITHUB_EVENT_PATH`` and
``GITHUB_SERVER_URL`` set, plus ``REVIEW_APP_TOKEN``, an automation App token
that may create check runs and label pull requests, and ``REVIEW_APP_SLUG``,
that App's slug. Without them the workflow's own token reports instead. It
exits 0 once every check run is reported, whatever the verdict, and 1 when
it could not decide.
"""

from __future__ import annotations

import os
import sys
import urllib.parse
from dataclasses import dataclass
from enum import Enum

from cla_check import (
    BOT_LOGIN,
    MAX_BRANCH_CHARS,
    MAX_DOCUMENT_BYTES,
    PAGE_SIZE,
    REPOSITORY,
    USER_LOGIN,
    Account,
    AccountType,
    ApiError,
    ClaInputError,
    PullRequest,
    Reporter,
    RunHead,
    Transport,
    _id,
    _load_json,
    _object,
    _sha,
    _string,
    parse_account,
    parse_pull_request,
    parse_pull_refs,
    urllib_transport,
    utc_now,
)

#: Name of the check run; the one to require in the default branch's ruleset.
CHECK_NAME = "Maintainer review"

#: The check run's title when the check could not decide.
ERROR_DESCRIPTION = "The maintainer review check could not decide; see the run."

#: Bot accounts whose pull requests need no approval: Dependabot and the
#: automation App, which open dependency bumps that CI then verifies. Only
#: API accounts of type ``Bot`` match, and ``[bot]`` logins cannot be
#: registered by people.
TRUSTED_BOTS = frozenset({"dependabot[bot]", "bookclerk[bot]"})

#: The collaborator permissions (the API's legacy ``permission`` field, where
#: ``maintain`` reads as ``write``) that may merge, and so may approve.
TRUSTED_PERMISSIONS = frozenset({"admin", "write"})

#: Every value the legacy ``permission`` field may take.
KNOWN_PERMISSIONS = frozenset({"admin", "write", "read", "none"})

#: The label an outside pull request carries while it changes files that
#: ``is_sensitive`` flags.
SENSITIVE_LABEL = "touches-ci-or-build"

#: Path prefixes whose files run in CI, in the Dev Container or at build
#: time: workflows and actions, scripts the workflows call, Cargo and npm
#: configuration, packaging and the Dev Container.
SENSITIVE_PREFIXES = (
    ".github/",
    ".cargo/",
    ".devcontainer/",
    "scripts/",
    "packaging/",
    "tools/lint/",
    "fuzz/corpus/",
)

#: File names that run code or choose dependencies wherever they are: build
#: scripts, manifests and lockfiles (a new dependency or ``proc-macro``
#: crate, an npm lifecycle script), toolchain and container files.
SENSITIVE_NAMES = frozenset(
    {
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "package-lock.json",
        "pyproject.toml",
        "uv.lock",
        "requirements.txt",
        "Dockerfile",
        "Containerfile",
        "wrangler.toml",
        "rust-toolchain.toml",
        ".tool-versions",
        ".npmrc",
        ".gitattributes",
        ".gitmodules",
    }
)

#: Most changed files the pull request files endpoint lists; beyond it the
#: list is incomplete and the summary says so.
MAX_LISTED_FILES = 3000

#: Most review pages read; a pull request with more fails closed.
MAX_REVIEW_PAGES = 10

#: Longest changed file path accepted, in characters.
MAX_PATH_CHARS = 4096

#: Most flagged paths the summary names; the rest are counted.
MAX_SUMMARY_PATHS = 50

#: Most open pull requests that may share one head commit; more fails closed.
MAX_SHARED_PULLS = 10


class EventKind(Enum):
    """Which workflow event triggered the run."""

    #: The ``Maintainer review trigger`` workflow ran for a pull request or
    #: one of its reviews.
    WORKFLOW_RUN = "workflow_run"
    #: A merge queue built a merge group.
    MERGE_GROUP = "merge_group"
    #: Someone pressed **Re-run** on a check run.
    CHECK_RUN = "check_run"


@dataclass(frozen=True)
class Event:
    """The parts of a workflow event payload the check acts on."""

    #: The event that triggered the run.
    kind: EventKind
    #: The head the trigger run was for, from a ``workflow_run`` payload.
    run_head: RunHead | None = None
    #: The merge group's head commit, from a ``merge_group`` payload.
    merge_group_sha: str | None = None
    #: The commit of a ``Maintainer review`` check run someone asked to
    #: re-run, from a ``check_run`` payload; ``None`` for another check.
    rerun_sha: str | None = None
    #: The slug of the App that owns that check run.
    rerun_app: str | None = None


class ReviewState(Enum):
    """A review's ``state`` as the REST API reports it."""

    #: The reviewer approved the commit the review is on.
    APPROVED = "APPROVED"
    #: The reviewer asked for changes.
    CHANGES_REQUESTED = "CHANGES_REQUESTED"
    #: A comment-only review, which is no verdict.
    COMMENTED = "COMMENTED"
    #: A review that was dismissed, which withdraws the reviewer's verdict.
    DISMISSED = "DISMISSED"
    #: A review not submitted yet, visible only to its author.
    PENDING = "PENDING"


@dataclass(frozen=True)
class Review:
    """One pull request review."""

    #: Who submitted it.
    author: Account
    #: Its state.
    state: ReviewState
    #: The commit it was submitted on, or ``None`` when the API leaves it out.
    commit_id: str | None


@dataclass(frozen=True)
class ChangedFile:
    """One entry of a pull request's changed files."""

    #: The file's path after the change, repository-relative.
    path: str
    #: The path before a rename, or ``None``.
    previous_path: str | None
    #: Whether the file was deleted.
    removed: bool
    #: Whether the API shows a text diff; binary files and very large diffs
    #: have none.
    has_patch: bool


@dataclass(frozen=True)
class Verdict:
    """Whether one pull request may merge, and what the check says about it."""

    #: The pull request number.
    number: int
    #: The pull request author.
    author: Account
    #: Whether the author needs no approval.
    trusted_author: bool
    #: Maintainers whose latest verdict approves the head commit.
    approved_by: tuple[Account, ...] = ()
    #: Maintainers whose latest verdict asks for changes.
    changes_requested_by: tuple[Account, ...] = ()
    #: Changed paths ``is_sensitive`` flags, in listing order.
    sensitive: tuple[str, ...] = ()
    #: Whether the pull request changes more files than the API lists.
    files_truncated: bool = False

    @property
    def passed(self) -> bool:
        """Whether the pull request may merge."""
        if self.trusted_author:
            return True
        return bool(self.approved_by) and not self.changes_requested_by


def _path(value: object, what: str) -> str:
    """Returns ``value`` as a repository-relative path.

    # Errors
    ``ClaInputError`` when it is not a string of 1 to ``MAX_PATH_CHARS``
    printable characters, starts with ``/`` or holds a ``..`` segment.
    """
    path = _string(value, what)
    if not 1 <= len(path) <= MAX_PATH_CHARS or not path.isprintable() or path.startswith("/"):
        raise ClaInputError(f"{what} is not a repository path")
    if ".." in path.split("/"):
        raise ClaInputError(f"{what} is not a repository path")
    return path


def parse_run_head(value: object) -> RunHead:
    """Reads the ``workflow_run`` object of a ``workflow_run`` payload.

    # Errors
    ``ClaInputError`` when the run was not for a ``pull_request`` or
    ``pull_request_review`` event, or a field is missing or malformed.
    """
    obj = _object(value, "workflow run")
    if obj.get("event") not in ("pull_request", "pull_request_review"):
        raise ClaInputError("workflow run is not for a pull request")
    repo = _object(obj.get("head_repository"), "head repository")
    owner = _string(_object(repo.get("owner"), "head repository owner").get("login"), "head repository owner")
    if USER_LOGIN.fullmatch(owner) is None:
        raise ClaInputError("head repository owner is not a login")
    branch = _string(obj.get("head_branch"), "head branch")
    if not 1 <= len(branch) <= MAX_BRANCH_CHARS or not branch.isprintable():
        raise ClaInputError("head branch has a bad name")
    return RunHead(
        sha=_sha(obj.get("head_sha"), "run head"),
        branch=branch,
        repo_id=_id(repo.get("id"), "head repository id"),
        owner=owner,
    )


def parse_event(name: str, raw: bytes) -> Event:
    """Reads the workflow event payload.

    # Arguments
    * ``name``: ``GITHUB_EVENT_NAME``.
    * ``raw``: the contents of ``GITHUB_EVENT_PATH``.

    # Returns
    The event, with the run head, merge group or re-run commit it carries.

    # Errors
    ``ClaInputError`` for any other event name or a payload of the wrong
    shape.
    """
    try:
        kind = EventKind(name)
    except ValueError as err:
        raise ClaInputError("unsupported event") from err
    payload = _object(_load_json(raw), "event")
    match kind:
        case EventKind.WORKFLOW_RUN:
            return Event(kind, run_head=parse_run_head(payload.get("workflow_run")))
        case EventKind.MERGE_GROUP:
            group = _object(payload.get("merge_group"), "merge group")
            return Event(kind, merge_group_sha=_sha(group.get("head_sha"), "merge group head"))
        case EventKind.CHECK_RUN:
            run = _object(payload.get("check_run"), "check run")
            sha = _sha(run.get("head_sha"), "check run head")
            app = _string(_object(run.get("app"), "check run app").get("slug"), "check run app slug")
            if _string(run.get("name"), "check run name") != CHECK_NAME:
                return Event(kind)
            return Event(kind, rerun_sha=sha, rerun_app=app)


def parse_permission(raw: bytes) -> str:
    """Reads a ``GET /collaborators/{login}/permission`` answer.

    # Returns
    The legacy ``permission`` field, one of ``KNOWN_PERMISSIONS``.

    # Errors
    ``ClaInputError`` when the answer has the wrong shape or an unknown
    permission.
    """
    obj = _object(_load_json(raw), "permission")
    permission = _string(obj.get("permission"), "permission")
    if permission not in KNOWN_PERMISSIONS:
        raise ClaInputError("unknown permission")
    return permission


def parse_review_page(raw: bytes) -> tuple[list[Review], int]:
    """Reads one page of ``GET /pulls/{n}/reviews``.

    Reviews whose author's account was deleted (``user`` is ``null``) are
    left out: nobody can be asked about them, and they cannot approve.

    # Returns
    The reviews, oldest first, and how many entries the page held, so
    pagination does not stop early when some are left out.

    # Errors
    ``ClaInputError`` when the page or an entry has the wrong shape.
    """
    page = _load_json(raw)
    if not isinstance(page, list):
        raise ClaInputError("review page is not a list")
    reviews = []
    for entry in page:
        obj = _object(entry, "review")
        try:
            state = ReviewState(obj.get("state"))
        except ValueError as err:
            raise ClaInputError("unknown review state") from err
        commit = obj.get("commit_id")
        commit_id = None if commit is None else _sha(commit, "review commit")
        user = obj.get("user")
        if user is None:
            continue
        reviews.append(Review(author=parse_account(user), state=state, commit_id=commit_id))
    return reviews, len(page)


def parse_file_page(raw: bytes) -> list[ChangedFile]:
    """Reads one page of ``GET /pulls/{n}/files``.

    # Errors
    ``ClaInputError`` when the page or an entry has the wrong shape.
    """
    page = _load_json(raw)
    if not isinstance(page, list):
        raise ClaInputError("file page is not a list")
    files = []
    for entry in page:
        obj = _object(entry, "changed file")
        status = _string(obj.get("status"), "file status")
        if status not in ("added", "removed", "modified", "renamed", "copied", "changed", "unchanged"):
            raise ClaInputError("unknown file status")
        previous = obj.get("previous_filename")
        patch = obj.get("patch")
        if patch is not None:
            _string(patch, "file patch")
        files.append(
            ChangedFile(
                path=_path(obj.get("filename"), "file name"),
                previous_path=None if previous is None else _path(previous, "previous file name"),
                removed=status == "removed",
                has_patch=patch is not None,
            )
        )
    return files


def is_sensitive_path(path: str) -> bool:
    """Tells whether a change to ``path`` can run code in CI or at build time.

    # Arguments
    * ``path``: a repository-relative path.
    """
    if path.startswith(SENSITIVE_PREFIXES):
        return True
    return path.rsplit("/", 1)[-1] in SENSITIVE_NAMES


def is_sensitive(file: ChangedFile) -> bool:
    """Tells whether a changed file needs reading before an approval.

    A file is flagged when its new or old path is ``is_sensitive_path``, or
    when it is added or changed without a text diff (a binary file, such as
    a test fixture, or a diff too large to show), since nobody can review
    what the diff does not show.
    """
    if is_sensitive_path(file.path):
        return True
    if file.previous_path is not None and is_sensitive_path(file.previous_path):
        return True
    return not file.removed and not file.has_patch


def is_trusted_bot(account: Account) -> bool:
    """Tells whether ``account`` is one of ``TRUSTED_BOTS``."""
    return account.type is AccountType.BOT and account.login in TRUSTED_BOTS


def evaluate(
    pull: PullRequest,
    author_permission: str | None,
    reviews: list[Review],
    permissions: dict[int, str],
    files: list[ChangedFile],
    files_truncated: bool = False,
) -> Verdict:
    """Decides one pull request.

    # Arguments
    * ``pull``: the pull request.
    * ``author_permission``: the author's permission, or ``None`` for a bot
      (bots have no collaborator permission).
    * ``reviews``: its reviews, oldest first.
    * ``permissions``: each reviewer's permission by account id; a reviewer
      missing from it counts as having none.
    * ``files``: its changed files.
    * ``files_truncated``: whether ``files`` is incomplete.

    # Returns
    The verdict. Only each reviewer's latest approval, request for changes
    or dismissal counts; comments and pending reviews are no verdict. An
    approval counts only on the current head commit, and the author's own
    reviews never count.
    """
    trusted = is_trusted_bot(pull.author) or (
        pull.author.type is AccountType.USER and author_permission in TRUSTED_PERMISSIONS
    )
    if trusted:
        return Verdict(number=pull.number, author=pull.author, trusted_author=True)
    latest: dict[int, Review] = {}
    for review in reviews:
        if review.state in (ReviewState.COMMENTED, ReviewState.PENDING):
            continue
        if review.author.id == pull.author.id or review.author.type is not AccountType.USER:
            continue
        latest[review.author.id] = review
    approved = []
    changes = []
    for review in latest.values():
        if permissions.get(review.author.id) not in TRUSTED_PERMISSIONS:
            continue
        match review.state:
            case ReviewState.APPROVED:
                if review.commit_id == pull.head_sha:
                    approved.append(review.author)
            case ReviewState.CHANGES_REQUESTED:
                changes.append(review.author)
            case ReviewState.DISMISSED | ReviewState.COMMENTED | ReviewState.PENDING:
                pass
    return Verdict(
        number=pull.number,
        author=pull.author,
        trusted_author=False,
        approved_by=tuple(approved),
        changes_requested_by=tuple(changes),
        sensitive=tuple(f.path for f in files if is_sensitive(f)),
        files_truncated=files_truncated,
    )


def check_title(verdict: Verdict) -> str:
    """Returns the check run's one-line title for ``verdict``."""
    if verdict.trusted_author:
        return "The author can merge; no approval needed."
    if verdict.changes_requested_by:
        return "A maintainer asked for changes."
    if verdict.approved_by:
        return "A maintainer approved the head commit."
    if verdict.sensitive:
        return "Needs a maintainer's approval; changes CI or build files."
    return "Needs a maintainer's approval of the head commit."


def render_summary(verdict: Verdict) -> str:
    """Returns the Markdown the check run's page shows for one pull request.

    It names logins only (no profile links), and file paths in code spans
    with backticks removed, so contributor-chosen text cannot add links or
    formatting.
    """
    lines = [f"#{verdict.number} by @{verdict.author.login}: {check_title(verdict)}", ""]
    if verdict.trusted_author:
        return "\n".join(lines).strip()
    lines.append(
        "Pull requests from authors without write access need a maintainer's approval "
        "on their current head commit; every push needs a fresh one."
    )
    if verdict.approved_by:
        lines.append("")
        lines.append("Approved by " + ", ".join(f"@{a.login}" for a in verdict.approved_by) + ".")
    if verdict.changes_requested_by:
        lines.append("")
        lines.append("Changes requested by " + ", ".join(f"@{a.login}" for a in verdict.changes_requested_by) + ".")
    if verdict.sensitive or verdict.files_truncated:
        lines += [
            "",
            "**Read these before approving.** They run in CI or at build time, or have no text "
            "diff. A fork's pull request runs its own copy of the workflows, so a green CI run "
            "says nothing about them.",
            "",
        ]
        for path in verdict.sensitive[:MAX_SUMMARY_PATHS]:
            lines.append(f"- `{path.replace('`', '')}`")
        hidden = len(verdict.sensitive) - MAX_SUMMARY_PATHS
        if hidden > 0:
            lines.append(f"- and {hidden} more")
        if verdict.files_truncated:
            lines.append(f"- more than {MAX_LISTED_FILES} files changed, so this list is incomplete")
    return "\n".join(lines)


class GitHub:
    """The REST calls the check makes, over a ``Transport``."""

    #: The check run ``set_check`` reports; the signed commits check
    #: (``signed_commits.py``) reuses this class under its own name.
    check_name = CHECK_NAME

    def __init__(self, send: Transport, repository: str, reporter: Reporter | None = None) -> None:
        """Wraps ``send`` for ``repository`` (``owner/name``).

        # Arguments
        * ``send``: performs every read.
        * ``repository``: ``owner/name``.
        * ``reporter``: who posts the check run and labels, or ``None`` to
          post them through ``send`` as ``github-actions[bot]``.
        """
        #: The function that performs reads.
        self.send = send
        #: Who posts the check run and the label.
        self.reporter = reporter or Reporter(send, "github-actions[bot]")
        #: ``repos/<owner>/<name>``, the prefix of every REST path.
        self.base = f"repos/{repository}"
        #: Permissions read so far, by login.
        self.permissions: dict[str, str] = {}
        #: The head commit being decided, once known, so a failed run can
        #: still report its error there.
        self.head_sha: str | None = None

    def call(self, path: str) -> bytes:
        """Sends a ``GET`` and returns the body of a successful response.

        # Errors
        ``ApiError`` when the status is not 200.
        """
        status, data = self.send("GET", f"{self.base}/{path}", None)
        if status != 200:
            raise ApiError(f"GET {path}: HTTP {status}")
        return data

    def pull_request(self, number: int) -> PullRequest:
        """Fetches a pull request."""
        return parse_pull_request(_load_json(self.call(f"pulls/{number}")))

    def permission(self, account: Account) -> str:
        """Fetches ``account``'s permission on the repository, once per login."""
        if account.login not in self.permissions:
            login = urllib.parse.quote(account.login, safe="")
            self.permissions[account.login] = parse_permission(self.call(f"collaborators/{login}/permission"))
        return self.permissions[account.login]

    def reviews(self, number: int) -> list[Review]:
        """Fetches every review of pull request ``number``, oldest first.

        # Errors
        ``ApiError`` when it has more than ``MAX_REVIEW_PAGES`` pages.
        """
        reviews: list[Review] = []
        for page in range(1, MAX_REVIEW_PAGES + 1):
            batch, entries = parse_review_page(self.call(f"pulls/{number}/reviews?per_page={PAGE_SIZE}&page={page}"))
            reviews += batch
            if entries < PAGE_SIZE:
                return reviews
        raise ApiError(f"pull request has more than {MAX_REVIEW_PAGES * PAGE_SIZE} reviews")

    def files(self, number: int) -> tuple[list[ChangedFile], bool]:
        """Fetches the changed files of pull request ``number``.

        # Returns
        The files, and whether the listing stopped at ``MAX_LISTED_FILES``.
        """
        files: list[ChangedFile] = []
        for page in range(1, MAX_LISTED_FILES // PAGE_SIZE + 1):
            batch = parse_file_page(self.call(f"pulls/{number}/files?per_page={PAGE_SIZE}&page={page}"))
            files += batch
            if len(batch) < PAGE_SIZE:
                return files, False
        return files, True

    def pulls_on(self, sha: str, head: RunHead | None = None) -> list[int]:
        """Lists the open pull requests whose head commit is ``sha``.

        Pull requests from a fork are found through ``head``'s branch,
        matched by head commit and head repository, since the listing of
        pull requests that contain a commit can leave them out.

        # Errors
        ``ApiError`` when a listing fills a page or more than
        ``MAX_SHARED_PULLS`` match.
        """
        refs = parse_pull_refs(self.call(f"commits/{sha}/pulls?per_page={PAGE_SIZE}"))
        numbers = {r.number for r in refs if r.open and r.head_sha == sha}
        full = len(refs) >= PAGE_SIZE
        if head is not None:
            label = f"{urllib.parse.quote(head.owner, safe='')}:{urllib.parse.quote(head.branch, safe='')}"
            branch_refs = parse_pull_refs(self.call(f"pulls?state=open&head={label}&per_page={PAGE_SIZE}"))
            numbers |= {r.number for r in branch_refs if r.open and r.head_sha == sha and r.head_repo_id == head.repo_id}
            full = full or len(branch_refs) >= PAGE_SIZE
        if full:
            raise ApiError("too many pull requests to list for the commit")
        if len(numbers) > MAX_SHARED_PULLS:
            raise ApiError(f"more than {MAX_SHARED_PULLS} pull requests share the head commit")
        return sorted(numbers)

    def decide(self, number: int) -> Verdict | None:
        """Reads and decides pull request ``number``.

        # Returns
        The verdict, or ``None`` when the pull request is closed.
        """
        pull = self.pull_request(number)
        if not pull.open:
            return None
        author_permission = self.permission(pull.author) if pull.author.type is AccountType.USER else None
        if is_trusted_bot(pull.author) or (pull.author.type is AccountType.USER and author_permission in TRUSTED_PERMISSIONS):
            return evaluate(pull, author_permission, [], {}, [])
        reviews = self.reviews(number)
        permissions = {}
        for review in reviews:
            if review.author.type is AccountType.USER and review.author.id != pull.author.id:
                permissions[review.author.id] = self.permission(review.author)
        files, truncated = self.files(number)
        return evaluate(pull, author_permission, reviews, permissions, files, truncated)

    def set_check(self, sha: str, passed: bool, title: str, summary: str, run_url: str) -> None:
        """Reports a completed ``check_name`` check run on ``sha``.

        GitHub shows, and rulesets judge, the newest check run of a name.

        # Errors
        ``ApiError`` when GitHub refuses it.
        """
        now = utc_now()
        body: dict[str, object] = {
            "name": self.check_name,
            "head_sha": sha,
            "details_url": run_url,
            "status": "completed",
            "conclusion": "success" if passed else "failure",
            "started_at": now,
            "completed_at": now,
            "output": {"title": title[:140], "summary": summary[:60000] or title},
        }
        status, _ = self.reporter.send("POST", f"{self.base}/check-runs", body)
        if status not in (200, 201):
            raise ApiError(f"POST check-runs: HTTP {status}")

    def label(self, verdict: Verdict) -> None:
        """Adds ``SENSITIVE_LABEL`` to an outside pull request that needs it, or removes it.

        Labels only point reviewers at the summary, so a refusal or network
        failure is printed as a warning and never fails the check.
        """
        if verdict.trusted_author:
            return
        try:
            if verdict.sensitive or verdict.files_truncated:
                status, _ = self.reporter.send("POST", f"{self.base}/issues/{verdict.number}/labels", {"labels": [SENSITIVE_LABEL]})
                ok: tuple[int, ...] = (200,)
            else:
                status, _ = self.reporter.send("DELETE", f"{self.base}/issues/{verdict.number}/labels/{SENSITIVE_LABEL}", None)
                ok = (200, 404)
        except OSError as err:
            print(f"::warning::cannot update the {SENSITIVE_LABEL} label on #{verdict.number} ({err})")
            return
        if status not in ok:
            print(f"::warning::cannot update the {SENSITIVE_LABEL} label on #{verdict.number} (HTTP {status})")


def settle(api: GitHub, sha: str, run_url: str, head: RunHead | None = None) -> None:
    """Decides every open pull request on ``sha`` and reports the shared check.

    The check passes when any of them passes (see the module documentation).

    Does nothing when no open pull request has ``sha`` as its head (the
    check then stays as it was, and a closed pull request's commit needs
    none).

    # Errors
    ``ApiError`` or ``ClaInputError`` when the check cannot decide.
    """
    api.head_sha = sha
    verdicts = []
    for number in api.pulls_on(sha, head):
        verdict = api.decide(number)
        if verdict is not None:
            verdicts.append(verdict)
    if not verdicts:
        return
    for verdict in verdicts:
        api.label(verdict)
    passing = [v for v in verdicts if v.passed]
    first = passing[0] if passing else verdicts[0]
    title = check_title(first)
    if len(verdicts) > 1:
        title = f"#{first.number}: {title}"
    summary = "\n\n---\n\n".join(render_summary(v) for v in verdicts)
    api.set_check(sha, bool(passing), title, summary, run_url)


def run(event: Event, api: GitHub, run_url: str) -> None:
    """Handles one workflow event.

    # Errors
    ``ApiError`` or ``ClaInputError`` when the check cannot decide; the
    caller then reports a failed check.
    """
    match event.kind:
        case EventKind.MERGE_GROUP:
            # Every pull request in the group passed the required check to
            # enter the queue; the queue run needs the check reported too.
            if event.merge_group_sha is not None:
                api.set_check(event.merge_group_sha, True, "Checked on each pull request in the group.", "", run_url)
        case EventKind.CHECK_RUN:
            # Only **Re-run** on this App's own check runs is ours to answer.
            if event.rerun_sha is not None and f"{event.rerun_app}[bot]" == api.reporter.login:
                settle(api, event.rerun_sha, run_url)
        case EventKind.WORKFLOW_RUN:
            if event.run_head is not None:
                settle(api, event.run_head.sha, run_url, event.run_head)


def main(env: dict[str, str]) -> int:
    """Runs the check from the workflow environment.

    # Arguments
    * ``env``: the process environment.

    # Returns
    0 when every check run was reported, 1 when the check could not decide.
    """
    repository = env.get("GITHUB_REPOSITORY", "")
    if REPOSITORY.fullmatch(repository) is None:
        print("maintainer_review: GITHUB_REPOSITORY is not owner/name", file=sys.stderr)
        return 1
    server = env.get("GITHUB_SERVER_URL", "https://github.com")
    api_url = env.get("GITHUB_API_URL", "https://api.github.com")
    send = urllib_transport(env.get("GITHUB_TOKEN", ""), api_url)
    reporter = None
    app_token = env.get("REVIEW_APP_TOKEN", "")
    app_slug = env.get("REVIEW_APP_SLUG", "")
    if app_token and app_slug:
        login = f"{app_slug}[bot]"
        if BOT_LOGIN.fullmatch(login) is None:
            print("maintainer_review: REVIEW_APP_SLUG is not an App slug", file=sys.stderr)
            return 1
        reporter = Reporter(urllib_transport(app_token, api_url), login)
    api = GitHub(send, repository, reporter)
    run_url = f"{server}/{repository}/actions/runs/{env.get('GITHUB_RUN_ID', '')}"
    try:
        with open(env.get("GITHUB_EVENT_PATH", ""), "rb") as handle:
            event = parse_event(env.get("GITHUB_EVENT_NAME", ""), handle.read(MAX_DOCUMENT_BYTES + 1))
    except (OSError, ClaInputError) as err:
        print(f"maintainer_review: cannot read the event: {err}", file=sys.stderr)
        return 1
    try:
        run(event, api, run_url)
    except (ApiError, ClaInputError, OSError) as err:
        print(f"maintainer_review: {err}", file=sys.stderr)
        sha = api.head_sha or event.merge_group_sha or event.rerun_sha
        if sha is None and event.run_head is not None:
            sha = event.run_head.sha
        if sha is not None:
            try:
                api.set_check(sha, False, ERROR_DESCRIPTION, "", run_url)
            except (ApiError, OSError):
                pass
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(dict(os.environ)))
