#!/usr/bin/env python3
"""Checks that everyone behind a pull request has accepted the Bookclerk CLA.

The ``CLA`` workflow runs this on ``workflow_run`` after the unprivileged
``CLA trigger`` workflow ran for a pull request, on signature and ``recheck``
comments, and on ``merge_group``. It reads only pull request metadata through
the REST API and never checks out pull request code.

Who must sign: the pull request author, the GitHub account linked to every
commit, and every account a commit credits with a ``Co-authored-by:`` line,
maintainers and organization members included. People are matched by
numeric account id, never by name. The only exemptions are tools: the bot
accounts in ``BOT_ALLOWLIST``, recognized by their API account type and
login, which no human account can take, and the accounts GitHub links to an
AI coding assistant's address in ``ASSISTANT_EMAILS``. A commit not linked to any account fails the
check, since nobody can sign for it. A co-author whose email matches no
account does not: the pull request author is the one authenticated party,
and their signature covers what they submit (CLA section 4).

The verdict is the ``CLA`` commit status on the pull request's head commit.
Signatures live in ``signatures/cla.json`` on the ``cla-signatures`` branch,
in the format CLA Assistant Lite used, and are written through the contents
API with the automation App's token; the first signature creates the branch
as a single commit with no history. One comment, marked with
``COMMENT_MARKER``, says who still has to sign and is edited in place.

Everything that parses bytes from GitHub (the event payload, pull requests,
commit and comment pages, the signature file) is sans-IO and fuzzed by
``scripts/tests/test_cla_check.py``. Any input outside the expected shape
fails closed.

Usage (inside the workflow): ``cla_check.py`` with ``GITHUB_TOKEN``,
``GITHUB_REPOSITORY``, ``GITHUB_EVENT_NAME``, ``GITHUB_EVENT_PATH`` and
``GITHUB_SERVER_URL`` set, plus ``CLA_SIGNATURE_TOKEN``, an automation App
token that may write to ``cla-signatures`` and comment on pull requests, and
``CLA_APP_SLUG``, that App's slug. Only that token records signatures, so the
branch can refuse every other writer and each signature commit is signed by
GitHub for the App. The check's comment is posted as the App too, or as the
workflow when no App is configured; the ``CLA`` status always comes from the
workflow, the source the default branch's ruleset expects. It exits 0 once the status is posted, whatever the
verdict, and 1 when it could not decide.
"""

from __future__ import annotations

import base64
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from enum import Enum
from typing import Callable

#: The comment a contributor posts to accept the CLA, matched exactly once
#: surrounding whitespace is trimmed.
SIGN_PHRASE = "I have read the Bookclerk CLA and I agree to it."

#: The comment that re-runs the check without signing.
RECHECK_PHRASE = "recheck"

#: First line of the check's own comment, used to find it again.
COMMENT_MARKER = "<!-- bookclerk-cla-check -->"

#: Commit status context the check reports; the one to require in rulesets.
STATUS_CONTEXT = "CLA"

#: The branch that holds the signature file.
SIGNATURE_BRANCH = "cla-signatures"

#: Path of the signature file on ``SIGNATURE_BRANCH``.
SIGNATURE_PATH = "signatures/cla.json"

#: Files the first signature writes beside the signature file, so the branch
#: explains itself: name on the branch, file mode. Their text comes from
#: ``scripts/cla_branch/`` on the default branch.
BRANCH_FILES = (("README.md", "100644"), ("setup-rulesets.sh", "100755"))

#: Ruleset rule types that must apply to ``SIGNATURE_BRANCH``: only the App
#: may create or update it, and nobody may delete it, force-push, merge
#: into it or push unsigned commits. Each run warns about missing ones.
SIGNATURE_BRANCH_RULES = frozenset(
    {"creation", "update", "deletion", "non_fast_forward", "required_linear_history", "required_signatures"}
)

#: Most rules a branch rules answer may list.
MAX_BRANCH_RULES = 200

#: A ruleset rule type, such as ``non_fast_forward``.
RULE_TYPE = re.compile(r"[a-z][a-z_]{0,63}", re.ASCII)

#: Bot accounts exempt from signing. Only API accounts of type ``Bot`` match,
#: and ``[bot]`` logins cannot be registered by people.
BOT_ALLOWLIST = frozenset({"dependabot[bot]", "bookclerk[bot]", "github-actions[bot]"})

#: Commit author addresses of AI coding assistants. A commit may be authored
#: by an assistant or credit one with a ``Co-authored-by:`` line; the account
#: GitHub links to such an address is a tool, not a person, so like a bot it
#: does not sign (the pull request author still does).
#: Matching on the address is safe because only the assistant's own account
#: can own it, so no person's account is ever exempted this way.
ASSISTANT_EMAILS = frozenset({"noreply@anthropic.com"})

#: The account the workflow's own token comments as, when no automation App
#: token is configured to comment instead.
WORKFLOW_COMMENT_AUTHOR = "github-actions[bot]"

#: Most commits the pull request commits endpoint returns; larger pull
#: requests fail closed rather than be checked in part.
MAX_COMMITS = 250

#: Most authors one commit may credit; a commit with more fails closed.
MAX_COAUTHORS = 25

#: Longest GraphQL page cursor accepted, in characters.
MAX_CURSOR_CHARS = 1024

#: Reads the accounts GitHub links to each commit's authors, co-authors
#: included, one page of commits at a time.
COAUTHOR_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      commits(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { commit { oid authors(first: 25) { totalCount nodes { email user { databaseId login } } } } }
      }
    }
  }
}
"""

#: Page size for every paginated request.
PAGE_SIZE = 100

#: Largest JSON document read from the API or the event file, in bytes.
MAX_DOCUMENT_BYTES = 4 * 1024 * 1024

#: Largest signature file read, in bytes (the contents API inlines at most 1 MiB).
MAX_SIGNATURE_BYTES = 1024 * 1024

#: Times a signature write is retried after another run changed the file.
SIGNATURE_WRITE_ATTEMPTS = 3

#: Times a check decides and posts again because the signature file changed
#: while it ran.
STATUS_ATTEMPTS = 3

#: The version of ``CLA.md`` a signature accepts, recorded with each entry.
#: Bump it together with the version line in ``CLA.md``.
CLA_VERSION = "1.0"

#: Most open pull requests that may share one head commit. The ``CLA``
#: status belongs to the commit, so each of them is checked before it passes;
#: more than this fails closed.
MAX_SHARED_PULLS = 10

#: A GitHub user or organization login. New logins cannot end in a hyphen
#: or repeat one, but some older accounts do, so only the first character
#: is restricted beyond the alphabet.
USER_LOGIN = re.compile(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}", re.ASCII)

#: A GitHub App's bot login.
BOT_LOGIN = re.compile(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}\[bot\]", re.ASCII)

#: A full commit SHA-1.
COMMIT_SHA = re.compile(r"[0-9a-f]{40}", re.ASCII)

#: An ``owner/name`` repository.
#: Longest branch name accepted from a ``workflow_run`` payload, in characters.
MAX_BRANCH_CHARS = 255

REPOSITORY = re.compile(r"[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}", re.ASCII)

#: Largest id GitHub could hand out that still round-trips through JSON numbers.
MAX_ID = 2**53 - 1


class ClaInputError(ValueError):
    """A document from GitHub is not in the shape the check expects."""


class ApiError(RuntimeError):
    """A GitHub API request failed."""


class AccountType(Enum):
    """The kind of GitHub account, from the API's ``type`` field."""

    #: A person's account.
    USER = "User"
    #: A GitHub App's bot account.
    BOT = "Bot"
    #: An organization, which can own repositories but not author commits.
    ORGANIZATION = "Organization"
    #: A placeholder for an account imported from elsewhere.
    MANNEQUIN = "Mannequin"


@dataclass(frozen=True)
class Account:
    """A GitHub account as the REST API describes it.

    # Invariants
    ``login`` matches ``BOT_LOGIN`` exactly when ``type`` is ``BOT``, and
    ``USER_LOGIN`` otherwise; ``id`` is in ``1..=MAX_ID``.
    """

    #: The account's numeric id, which never changes.
    id: int
    #: The account's current login, safe to print and to mention.
    login: str
    #: The kind of account.
    type: AccountType


@dataclass(frozen=True)
class Commit:
    """One pull request commit and the account GitHub links its author to."""

    #: The commit's full SHA-1.
    sha: str
    #: The linked author account, or ``None`` when the author email matches
    #: no account. The git author name is never read.
    author: Account | None
    #: The user accounts linked to the commit's authors and
    #: ``Co-authored-by:`` lines, as GitHub resolves them; authors whose
    #: email matches no user are left out.
    coauthors: tuple[Account, ...] = ()
    #: Whether ``author`` is an AI coding assistant's account, which does
    #: not sign (see ``ASSISTANT_EMAILS``).
    assistant_author: bool = False


@dataclass(frozen=True)
class CoauthorPage:
    """One page of the co-author query."""

    #: Each commit's linked author accounts, by SHA-1, without assistants.
    commits: dict[str, tuple[Account, ...]]
    #: Ids of the accounts linked to an ``ASSISTANT_EMAILS`` address.
    assistant_ids: frozenset[int]
    #: The cursor of the next page, or ``None`` on the last one.
    next_cursor: str | None


@dataclass(frozen=True)
class PullRequest:
    """The pull request fields the check uses."""

    #: The pull request number.
    number: int
    #: The account that opened it.
    author: Account
    #: The SHA-1 of its head commit, where the status goes.
    head_sha: str
    #: How many commits it has.
    commits: int
    #: Whether it is open; a closed pull request gets no status of its own.
    open: bool = True


@dataclass(frozen=True)
class PullRef:
    """One entry of the pull requests that contain a commit."""

    #: The pull request number.
    number: int
    #: Whether it is open.
    open: bool
    #: The SHA-1 of its head commit.
    head_sha: str
    #: The id of the repository its head branch lives in; ``None`` when the
    #: listing leaves it out or that repository was deleted.
    head_repo_id: int | None = None


@dataclass(frozen=True)
class RunHead:
    """The head a ``CLA trigger`` run checked out, from a ``workflow_run`` payload.

    GitHub sets every field from the run itself, so a pull request cannot
    choose them; the check still matches them against the pull requests the
    API lists instead of trusting a number.
    """

    #: The SHA-1 of the head commit the run was for.
    sha: str
    #: The head branch's name.
    branch: str
    #: The id of the repository the head branch lives in (a fork, or this one).
    repo_id: int
    #: The login of that repository's owner, for the ``head`` filter.
    owner: str


@dataclass(frozen=True)
class Comment:
    """An issue comment on the pull request."""

    #: The comment's numeric id.
    id: int
    #: Who posted it.
    author: Account
    #: Its Markdown body.
    body: str
    #: When it was posted, as the API's ISO 8601 text.
    created_at: str


class EventKind(Enum):
    """Which workflow event triggered the run."""

    #: The ``CLA trigger`` workflow ran for a pull request that was opened,
    #: pushed to, reopened, retargeted or closed.
    WORKFLOW_RUN = "workflow_run"
    #: A comment was posted on an issue or pull request.
    ISSUE_COMMENT = "issue_comment"
    #: A merge queue built a merge group.
    MERGE_GROUP = "merge_group"


@dataclass(frozen=True)
class Event:
    """The parts of a workflow event payload the check acts on."""

    #: The event that triggered the run.
    kind: EventKind
    #: The id of the repository the workflow runs in.
    repo_id: int
    #: The pull request number from an ``issue_comment`` payload; ``None``
    #: for the other events and for a comment on a plain issue.
    pull_number: int | None
    #: The new comment from an ``issue_comment`` payload.
    comment: Comment | None
    #: The merge group's head commit from a ``merge_group`` payload.
    merge_group_sha: str | None
    #: The head the ``CLA trigger`` run was for, from a ``workflow_run`` payload.
    run_head: RunHead | None = None


@dataclass(frozen=True)
class SignatureBook:
    """The parsed signature file.

    ``entries`` keeps every entry exactly as read, so writing it back changes
    only what is appended.
    """

    #: The raw entries of ``signedContributors``, in file order.
    entries: tuple[dict[str, object], ...]
    #: The account ids that have signed.
    signed_ids: frozenset[int]


@dataclass(frozen=True)
class Verdict:
    """Who still has to do something before the check passes."""

    #: Accounts that must sign and have not, in first-seen order.
    unsigned: tuple[Account, ...]
    #: Commits linked to no account, by SHA-1.
    unlinked: tuple[str, ...]

    @property
    def passed(self) -> bool:
        """Whether everyone has signed and every commit is linked."""
        return not self.unsigned and not self.unlinked


def _load_json(raw: bytes, limit: int = MAX_DOCUMENT_BYTES) -> object:
    """Decodes a JSON document no larger than ``limit`` bytes.

    # Errors
    ``ClaInputError`` when it is too large, not UTF-8, not JSON, or uses
    constants JSON does not define (``NaN``, ``Infinity``).
    """
    if len(raw) > limit:
        raise ClaInputError("document too large")

    def no_constant(name: str) -> object:
        raise ClaInputError(f"invalid JSON constant {name}")

    try:
        return json.loads(raw.decode("utf-8"), parse_constant=no_constant)
    except (UnicodeDecodeError, json.JSONDecodeError, RecursionError) as err:
        raise ClaInputError("not a JSON document") from err


def _object(value: object, what: str) -> dict[str, object]:
    """Returns ``value`` as a JSON object, or raises ``ClaInputError`` naming ``what``."""
    if not isinstance(value, dict):
        raise ClaInputError(f"{what} is not an object")
    return value


def _id(value: object, what: str) -> int:
    """Returns ``value`` as a positive JSON integer id.

    # Errors
    ``ClaInputError`` when it is not an integer (booleans included) in
    ``1..=MAX_ID``.
    """
    if type(value) is not int or not 1 <= value <= MAX_ID:
        raise ClaInputError(f"{what} is not a valid id")
    return value


def _string(value: object, what: str) -> str:
    """Returns ``value`` as a JSON string, or raises ``ClaInputError`` naming ``what``."""
    if not isinstance(value, str):
        raise ClaInputError(f"{what} is not a string")
    return value


def _sha(value: object, what: str) -> str:
    """Returns ``value`` as a full lowercase commit SHA-1.

    # Errors
    ``ClaInputError`` when it is anything else.
    """
    sha = _string(value, what)
    if COMMIT_SHA.fullmatch(sha) is None:
        raise ClaInputError(f"{what} is not a commit SHA")
    return sha


def parse_account(value: object) -> Account:
    """Reads a REST API account object (``user``, ``author``).

    # Arguments
    * ``value``: the decoded JSON value.

    # Returns
    The account, with its login checked against its type.

    # Errors
    ``ClaInputError`` when a field is missing or malformed, the type is
    unknown, or the login does not fit the type.
    """
    obj = _object(value, "account")
    account_id = _id(obj.get("id"), "account id")
    login = _string(obj.get("login"), "account login")
    try:
        kind = AccountType(obj.get("type"))
    except ValueError as err:
        raise ClaInputError("unknown account type") from err
    pattern = BOT_LOGIN if kind is AccountType.BOT else USER_LOGIN
    if pattern.fullmatch(login) is None:
        raise ClaInputError("account login does not fit its type")
    return Account(id=account_id, login=login, type=kind)


def parse_pull_request(value: object) -> PullRequest:
    """Reads a REST API pull request object.

    # Arguments
    * ``value``: the decoded JSON value, from an event or ``GET /pulls/{n}``.

    # Returns
    The pull request's number, author, head commit and commit count.

    # Errors
    ``ClaInputError`` when a field is missing or malformed.
    """
    obj = _object(value, "pull request")
    head = _object(obj.get("head"), "pull request head")
    commits = obj.get("commits")
    if type(commits) is not int or commits < 0:
        raise ClaInputError("pull request commit count is not a count")
    state = obj.get("state")
    if state not in ("open", "closed"):
        raise ClaInputError("pull request state is not open or closed")
    return PullRequest(
        number=_id(obj.get("number"), "pull request number"),
        author=parse_account(obj.get("user")),
        head_sha=_sha(head.get("sha"), "pull request head"),
        commits=commits,
        open=state == "open",
    )


def parse_pull_refs(raw: bytes) -> list[PullRef]:
    """Reads a list of pull requests (``GET /commits/{sha}/pulls`` or ``GET /pulls``).

    # Errors
    ``ClaInputError`` when the page or an entry has the wrong shape.
    """
    page = _load_json(raw)
    if not isinstance(page, list):
        raise ClaInputError("pull request list is not a list")
    refs = []
    for entry in page:
        obj = _object(entry, "pull request")
        state = obj.get("state")
        if state not in ("open", "closed"):
            raise ClaInputError("pull request state is not open or closed")
        head = _object(obj.get("head"), "pull request head")
        repo = head.get("repo")
        refs.append(
            PullRef(
                number=_id(obj.get("number"), "pull request number"),
                open=state == "open",
                head_sha=_sha(head.get("sha"), "pull request head"),
                head_repo_id=None if repo is None else _id(_object(repo, "head repository").get("id"), "head repository id"),
            )
        )
    return refs


def parse_comment(value: object) -> Comment:
    """Reads a REST API issue comment object.

    # Errors
    ``ClaInputError`` when a field is missing or malformed.
    """
    obj = _object(value, "comment")
    return Comment(
        id=_id(obj.get("id"), "comment id"),
        author=parse_account(obj.get("user")),
        body=_string(obj.get("body"), "comment body"),
        created_at=_string(obj.get("created_at"), "comment time"),
    )


def parse_event(name: str, raw: bytes) -> Event:
    """Reads the workflow event payload.

    # Arguments
    * ``name``: ``GITHUB_EVENT_NAME``.
    * ``raw``: the contents of ``GITHUB_EVENT_PATH``.

    # Returns
    The event, with the run head, comment or merge group it carries.

    # Errors
    ``ClaInputError`` for any other event name, a ``workflow_run`` for
    anything but a ``pull_request`` run, or a payload of the wrong shape.
    """
    try:
        kind = EventKind(name)
    except ValueError as err:
        raise ClaInputError("unsupported event") from err
    payload = _object(_load_json(raw), "event")
    repo_id = _id(_object(payload.get("repository"), "repository").get("id"), "repository id")
    if kind is EventKind.WORKFLOW_RUN:
        return Event(kind, repo_id, None, None, None, parse_run_head(payload.get("workflow_run")))
    if kind is EventKind.ISSUE_COMMENT:
        issue = _object(payload.get("issue"), "issue")
        number = _id(issue.get("number"), "issue number")
        is_pull = issue.get("pull_request") is not None
        comment = parse_comment(payload.get("comment"))
        return Event(kind, repo_id, number if is_pull else None, comment, None)
    group = _object(payload.get("merge_group"), "merge group")
    return Event(kind, repo_id, None, None, _sha(group.get("head_sha"), "merge group head"))


def parse_run_head(value: object) -> RunHead:
    """Reads the ``workflow_run`` object of a ``workflow_run`` payload.

    # Errors
    ``ClaInputError`` when the run was not for a ``pull_request`` event, or
    a field is missing or malformed.
    """
    obj = _object(value, "workflow run")
    if obj.get("event") != "pull_request":
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


def parse_commit_page(raw: bytes) -> list[Commit]:
    """Reads one page of ``GET /pulls/{n}/commits``.

    Only the top-level ``author``, the account GitHub linked the commit to,
    is read; the git author name and email are ignored.

    # Errors
    ``ClaInputError`` when the page or an entry has the wrong shape.
    """
    page = _load_json(raw)
    if not isinstance(page, list):
        raise ClaInputError("commit page is not a list")
    commits = []
    for entry in page:
        obj = _object(entry, "commit")
        author = obj.get("author")
        commits.append(Commit(sha=_sha(obj.get("sha"), "commit"), author=None if author is None else parse_account(author)))
    return commits


def parse_coauthor_page(raw: bytes) -> CoauthorPage:
    """Reads one page of the ``COAUTHOR_QUERY`` GraphQL response.

    GraphQL types every linked author as a user, an App's bot included, so
    an account is read as a ``BOT`` when its login has the ``[bot]`` suffix
    and as a ``USER`` otherwise. Accounts linked to an ``ASSISTANT_EMAILS``
    address are reported separately.

    # Errors
    ``ClaInputError`` when the response has errors or the wrong shape, a
    commit repeats, a commit credits more than ``MAX_COAUTHORS`` authors, or
    the next-page cursor is missing or too long.
    """
    doc = _object(_load_json(raw), "GraphQL response")
    if doc.get("errors") is not None:
        raise ClaInputError("GraphQL response has errors")
    data = _object(doc.get("data"), "GraphQL data")
    pull = _object(_object(data.get("repository"), "repository").get("pullRequest"), "pull request")
    commits = _object(pull.get("commits"), "commits")
    info = _object(commits.get("pageInfo"), "page info")
    has_next = info.get("hasNextPage")
    if type(has_next) is not bool:
        raise ClaInputError("hasNextPage is not a boolean")
    cursor = None
    if has_next:
        cursor = _string(info.get("endCursor"), "page cursor")
        if not 1 <= len(cursor) <= MAX_CURSOR_CHARS:
            raise ClaInputError("page cursor has a bad length")
    nodes = commits.get("nodes")
    if not isinstance(nodes, list):
        raise ClaInputError("commit nodes are not a list")
    out: dict[str, tuple[Account, ...]] = {}
    assistants: set[int] = set()
    for node in nodes:
        commit = _object(_object(node, "commit node").get("commit"), "commit")
        sha = _sha(commit.get("oid"), "commit")
        if sha in out:
            raise ClaInputError("commit repeats")
        authors = _object(commit.get("authors"), "commit authors")
        total = authors.get("totalCount")
        if type(total) is not int or not 0 <= total <= MAX_COAUTHORS:
            raise ClaInputError("commit credits too many authors")
        entries = authors.get("nodes")
        if not isinstance(entries, list) or len(entries) != total:
            raise ClaInputError("author nodes do not match their count")
        linked = []
        for entry in entries:
            author = _object(entry, "author")
            email = author.get("email")
            if email is not None:
                _string(email, "author email")
            user = author.get("user")
            if user is not None:
                obj = _object(user, "user")
                # GraphQL types a commit's linked account as ``User`` even for
                # an App's bot (Dependabot's commits link `dependabot[bot]`),
                # so the type comes from the login: GitHub gives the `[bot]`
                # suffix only to App bots and no user login can carry it.
                login = obj.get("login")
                kind = "Bot" if isinstance(login, str) and login.endswith("[bot]") else "User"
                account = parse_account({"id": obj.get("databaseId"), "login": login, "type": kind})
                if isinstance(email, str) and email.lower() in ASSISTANT_EMAILS:
                    assistants.add(account.id)
                else:
                    linked.append(account)
        out[sha] = tuple(linked)
    return CoauthorPage(commits=out, next_cursor=cursor, assistant_ids=frozenset(assistants))


def parse_comment_page(raw: bytes) -> tuple[list[Comment], int]:
    """Reads one page of ``GET /issues/{n}/comments``.

    A comment whose author is missing or is an account this check does not
    model (a deleted user, an app login without ``[bot]``) is left out: it
    can neither sign nor be the check's own comment, and one such comment
    must not stop the check for good.

    # Returns
    The comments kept, and how many entries the page had before any were
    left out (which decides whether another page follows).

    # Errors
    ``ClaInputError`` when the page or a comment's other fields have the
    wrong shape.
    """
    page = _load_json(raw)
    if not isinstance(page, list):
        raise ClaInputError("comment page is not a list")
    comments = []
    for entry in page:
        obj = _object(entry, "comment")
        try:
            parse_account(obj.get("user"))
        except ClaInputError:
            continue
        comments.append(parse_comment(obj))
    return comments, len(page)


def parse_signatures(raw: bytes) -> SignatureBook:
    """Reads the signature file.

    # Arguments
    * ``raw``: the file's bytes.

    # Returns
    Its entries and the set of account ids that signed.

    # Errors
    ``ClaInputError`` when it is too large, is not an object whose
    ``signedContributors`` is a list, or an entry lacks a string ``name`` or a
    valid ``id``.
    """
    doc = _object(_load_json(raw, MAX_SIGNATURE_BYTES), "signature file")
    entries = doc.get("signedContributors")
    if not isinstance(entries, list):
        raise ClaInputError("signedContributors is not a list")
    ids = set()
    for entry in entries:
        obj = _object(entry, "signature")
        _string(obj.get("name"), "signature name")
        ids.add(_id(obj.get("id"), "signature id"))
    return SignatureBook(entries=tuple(entries), signed_ids=frozenset(ids))


def parse_contents(raw: bytes) -> tuple[bytes, str]:
    """Reads ``GET /contents/{path}`` for a file.

    # Returns
    The file's bytes and its blob SHA, needed to replace it.

    # Errors
    ``ClaInputError`` when the response is not a base64 file of at most
    ``MAX_SIGNATURE_BYTES``.
    """
    obj = _object(_load_json(raw), "contents")
    if obj.get("type") != "file" or obj.get("encoding") != "base64":
        raise ClaInputError("contents is not a base64 file")
    blob = _sha(obj.get("sha"), "contents sha")
    text = _string(obj.get("content"), "contents")
    try:
        data = base64.b64decode("".join(text.split()).encode("ascii"), validate=True)
    except (UnicodeEncodeError, ValueError) as err:
        raise ClaInputError("contents is not base64") from err
    if len(data) > MAX_SIGNATURE_BYTES:
        raise ClaInputError("signature file too large")
    return data, blob


def parse_git_object(raw: bytes) -> str:
    """Reads the response to creating a git tree or commit.

    # Returns
    The new object's SHA-1.

    # Errors
    ``ClaInputError`` when the response is not an object with a ``sha``.
    """
    return _sha(_object(_load_json(raw), "git object").get("sha"), "git object sha")


def parse_branch_rules(raw: bytes) -> frozenset[str]:
    """Reads ``GET /rules/branches/{branch}``: the active rules for a branch.

    # Returns
    The rule types that apply.

    # Errors
    ``ClaInputError`` when the answer is not a list of at most
    ``MAX_BRANCH_RULES`` objects, each with a lowercase ``type``.
    """
    rules = _load_json(raw)
    if not isinstance(rules, list) or len(rules) > MAX_BRANCH_RULES:
        raise ClaInputError("branch rules are not a list of at most 200")
    types = set()
    for rule in rules:
        kind = _string(_object(rule, "branch rule").get("type"), "rule type")
        if RULE_TYPE.fullmatch(kind) is None:
            raise ClaInputError("rule type is malformed")
        types.add(kind)
    return frozenset(types)


def is_exempt(account: Account) -> bool:
    """Whether ``account`` is one of the bots that need not sign."""
    return account.type is AccountType.BOT and account.login in BOT_ALLOWLIST


def required_signers(pull: PullRequest, commits: list[Commit]) -> list[Account]:
    """Lists the accounts that must sign, once each, in first-seen order.

    The pull request author comes first, then each commit's linked author
    and co-authors. Exempt bots and AI coding assistants are left out.
    """
    seen: set[int] = set()
    signers = []
    accounts = [pull.author]
    for commit in commits:
        author = None if commit.assistant_author else commit.author
        accounts += [a for a in (author, *commit.coauthors) if a is not None]
    for account in accounts:
        if account.id not in seen and not is_exempt(account):
            seen.add(account.id)
            signers.append(account)
    return signers


def evaluate(pull: PullRequest, commits: list[Commit], book: SignatureBook) -> Verdict:
    """Decides whether the pull request passes the CLA check.

    # Arguments
    * ``pull``: the pull request.
    * ``commits``: all of its commits.
    * ``book``: the signatures recorded so far.

    # Returns
    The accounts that still have to sign and the commits linked to no
    account; the check passes when both are empty.
    """
    unsigned = tuple(a for a in required_signers(pull, commits) if a.id not in book.signed_ids)
    unlinked = tuple(dict.fromkeys(c.sha for c in commits if c.author is None))
    return Verdict(unsigned=unsigned, unlinked=unlinked)


def add_signature(book: SignatureBook, comment: Comment, repo_id: int, pull_number: int) -> bytes:
    """Returns the signature file with the commenter's signature appended.

    The entry uses CLA Assistant Lite's fields, so the file stays readable by
    either tool, plus ``claVersion``, the version of ``CLA.md`` accepted.
    Existing entries are written back unchanged.
    """
    entry: dict[str, object] = {
        "name": comment.author.login,
        "id": comment.author.id,
        "comment_id": comment.id,
        "created_at": comment.created_at,
        "repoId": repo_id,
        "pullRequestNo": pull_number,
        "claVersion": CLA_VERSION,
    }
    doc = {"signedContributors": [*book.entries, entry]}
    return (json.dumps(doc, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def render_comment(verdict: Verdict, cla_url: str, blocked_by: int | None = None) -> str:
    """Renders the check's comment on the pull request.

    Only validated logins, validated SHAs and ``cla_url`` reach the body; no
    git author name or other text from the pull request does.

    # Arguments
    * ``verdict``: the current verdict.
    * ``cla_url``: the link to the agreement.
    * ``blocked_by``: another open pull request with the same head commit
      that still fails, when this one passes.

    # Returns
    The Markdown body, starting with ``COMMENT_MARKER``.
    """
    lines = [COMMENT_MARKER]
    if verdict.passed and blocked_by is None:
        lines.append("Everyone on this pull request has accepted the Bookclerk CLA. Thank you!")
        return "\n".join(lines) + "\n"
    if verdict.passed:
        lines += [
            (
                "Everyone on this pull request has accepted the Bookclerk CLA, but the check stays red:"
                f" #{blocked_by} has the same head commit, and the check's status belongs to the commit."
            ),
            "",
            f"Once #{blocked_by} is closed, pushed to, or signed, comment `{RECHECK_PHRASE}` here.",
        ]
        return "\n".join(lines) + "\n"
    lines.append(
        "Thank you for your pull request. Before it can be merged, everyone who wrote"
        f" it needs to accept the [Bookclerk Contributor License Agreement]({cla_url}) once."
    )
    if verdict.unsigned:
        lines += ["", "Still to sign: " + ", ".join(f"@{a.login}" for a in verdict.unsigned) + "."]
        lines += ["", "To sign, read the agreement and post this exact comment:", "", "```text", SIGN_PHRASE, "```"]
    if verdict.unlinked:
        lines += [
            "",
            "These commits are not linked to a GitHub account, so nobody can sign for them. Add the"
            " author email to your GitHub account, or amend the commits with an email it already has:",
            "",
            *(f"- `{sha}`" for sha in verdict.unlinked),
        ]
    lines += ["", f"Comment `{RECHECK_PHRASE}` to run the check again."]
    return "\n".join(lines) + "\n"


def status_description(verdict: Verdict) -> str:
    """Returns the commit status description, at most 140 characters."""
    if verdict.passed:
        return "Everyone has accepted the CLA."
    parts = []
    if verdict.unsigned:
        parts.append(f"{len(verdict.unsigned)} still to sign")
    if verdict.unlinked:
        parts.append(f"{len(verdict.unlinked)} unlinked commit(s)")
    return ("CLA: " + ", ".join(parts) + ".")[:140]


#: Sends one API request: ``(method, path, JSON body or None)`` to
#: ``(HTTP status, response bytes)``.
Transport = Callable[[str, str, "dict[str, object] | None"], "tuple[int, bytes]"]


@dataclass(frozen=True)
class Commenter:
    """The bot account that posts the check's comment, and how to reach it."""

    #: Sends that bot's requests, with its token.
    send: Transport
    #: The bot's login, such as ``bookclerk[bot]``; matches ``BOT_LOGIN``.
    login: str


class GitHub:
    """The few REST calls the check makes, over a ``Transport``."""

    def __init__(
        self,
        send: Transport,
        repository: str,
        write: Transport | None = None,
        branch_files: dict[str, bytes] | None = None,
        commenter: Commenter | None = None,
    ) -> None:
        """Wraps ``send`` for ``repository`` (``owner/name``).

        # Arguments
        * ``send``: performs every request except signature writes.
        * ``repository``: ``owner/name``.
        * ``write``: performs signature writes, with the automation App's
          token, or ``None`` when none is configured (recording a signature
          then fails closed).
        * ``branch_files``: the text of each of ``BRANCH_FILES`` by name,
          written when the first signature creates the branch.
        * ``commenter``: who posts and edits the check's comment, or ``None``
          to comment through ``send`` as ``WORKFLOW_COMMENT_AUTHOR``.
        """
        #: The function that performs requests.
        self.send = send
        #: The function that writes the signature file, if any.
        self.write = write
        #: The files that start the signature branch beside the signatures.
        self.branch_files = branch_files or {}
        #: Who posts and edits the check's comment.
        self.commenter = commenter or Commenter(send, WORKFLOW_COMMENT_AUTHOR)
        #: ``repos/<owner>/<name>``, the prefix of every REST path.
        self.base = f"repos/{repository}"
        #: The repository's owner and name, for GraphQL.
        self.owner, self.name = repository.split("/", 1)
        #: The head commit of the pull request being checked, once known, so
        #: a failed run can still report ``error`` there.
        self.head_sha: str | None = None

    def call(self, method: str, path: str, body: dict[str, object] | None = None, ok: tuple[int, ...] = (200, 201)) -> bytes:
        """Sends a request and returns the body of a successful response.

        # Errors
        ``ApiError`` when the status is not in ``ok``.
        """
        status, data = self.send(method, f"{self.base}/{path}", body)
        if status not in ok:
            raise ApiError(f"{method} {path}: HTTP {status}")
        return data

    def pull_request(self, number: int) -> PullRequest:
        """Fetches a pull request."""
        return parse_pull_request(_load_json(self.call("GET", f"pulls/{number}")))

    def commits(self, pull: PullRequest) -> list[Commit]:
        """Fetches every commit of ``pull`` with its linked co-authors.

        # Errors
        ``ApiError`` when it has more than ``MAX_COMMITS`` commits, or the
        pages disagree with its commit count or with each other.
        """
        if pull.commits > MAX_COMMITS:
            raise ApiError(f"pull request has more than {MAX_COMMITS} commits")
        commits: list[Commit] = []
        page = 1
        while True:
            batch = parse_commit_page(self.call("GET", f"pulls/{pull.number}/commits?per_page={PAGE_SIZE}&page={page}"))
            commits += batch
            if len(batch) < PAGE_SIZE or len(commits) >= MAX_COMMITS:
                break
            page += 1
        if len(commits) != pull.commits:
            raise ApiError("commit pages do not match the pull request's commit count")
        coauthors, assistants = self.coauthors(pull)
        if set(coauthors) != {c.sha for c in commits}:
            raise ApiError("the co-author query and the commit pages list different commits")
        return [
            Commit(
                sha=c.sha,
                author=c.author,
                assistant_author=c.author is not None and c.author.id in assistants,
                coauthors=tuple(a for a in coauthors[c.sha] if a.id not in assistants),
            )
            for c in commits
        ]

    def coauthors(self, pull: PullRequest) -> tuple[dict[str, tuple[Account, ...]], frozenset[int]]:
        """Fetches the linked authors and co-authors of every commit of ``pull``.

        # Returns
        Each commit's accounts by SHA-1, and the ids of assistant accounts.
        """
        out: dict[str, tuple[Account, ...]] = {}
        assistants: set[int] = set()
        cursor: str | None = None
        for _ in range(MAX_COMMITS // PAGE_SIZE + 1):
            variables = {"owner": self.owner, "name": self.name, "number": pull.number, "after": cursor}
            status, data = self.send("POST", "graphql", {"query": COAUTHOR_QUERY, "variables": variables})
            if status != 200:
                raise ApiError(f"POST graphql: HTTP {status}")
            page = parse_coauthor_page(data)
            if set(page.commits) & set(out):
                raise ApiError("co-author pages repeat a commit")
            out.update(page.commits)
            assistants |= page.assistant_ids
            cursor = page.next_cursor
            if cursor is None:
                return out, frozenset(assistants)
        raise ApiError(f"pull request has more than {MAX_COMMITS} commits")

    def open_pulls_on(self, sha: str) -> list[int]:
        """Lists the open pull requests whose head commit is ``sha``.

        # Errors
        ``ApiError`` when more than ``MAX_SHARED_PULLS`` have it.
        """
        refs = parse_pull_refs(self.call("GET", f"commits/{sha}/pulls?per_page={PAGE_SIZE}"))
        if len(refs) >= PAGE_SIZE:
            raise ApiError("too many pull requests contain the commit")
        numbers = sorted({r.number for r in refs if r.open and r.head_sha == sha})
        if len(numbers) > MAX_SHARED_PULLS:
            raise ApiError(f"more than {MAX_SHARED_PULLS} pull requests share the head commit")
        return numbers

    def pulls_from(self, head: RunHead) -> list[int]:
        """Lists the open pull requests whose head is ``head``.

        Matches the listing's head commit and head repository against the
        run's, so a branch name reused by another fork or a later push does
        not count. Closed pull requests are left out: one only hands its
        head's shared status to the open ones, which the caller does when
        nothing matches, and a branch reused for many pull requests (a
        fork's ``main``) would otherwise fill the page with closed ones.

        # Errors
        ``ApiError`` when the listing fills a page or more than
        ``MAX_SHARED_PULLS`` match.
        """
        label = f"{urllib.parse.quote(head.owner, safe='')}:{urllib.parse.quote(head.branch, safe='')}"
        refs = parse_pull_refs(self.call("GET", f"pulls?state=open&head={label}&per_page={PAGE_SIZE}"))
        if len(refs) >= PAGE_SIZE:
            raise ApiError("too many pull requests come from the head branch")
        numbers = sorted({r.number for r in refs if r.head_sha == head.sha and r.head_repo_id == head.repo_id})
        if len(numbers) > MAX_SHARED_PULLS:
            raise ApiError(f"more than {MAX_SHARED_PULLS} pull requests come from the head commit")
        return numbers

    def comments(self, number: int) -> list[Comment]:
        """Fetches every comment on pull request ``number``, oldest first."""
        comments: list[Comment] = []
        page = 1
        while True:
            batch, entries = parse_comment_page(self.call("GET", f"issues/{number}/comments?per_page={PAGE_SIZE}&page={page}"))
            comments += batch
            if entries < PAGE_SIZE:
                return comments
            page += 1

    def signatures(self) -> tuple[SignatureBook, str | None]:
        """Reads the signature file and its blob SHA from ``SIGNATURE_BRANCH``.

        # Returns
        The signatures and the file's blob SHA, or an empty book and ``None``
        when the branch or the file does not exist yet. Nobody has signed
        then, so every check that needs a signature fails.
        """
        status, raw = self.send("GET", f"{self.base}/contents/{SIGNATURE_PATH}?ref={SIGNATURE_BRANCH}", None)
        if status == 404:
            return SignatureBook(entries=(), signed_ids=frozenset()), None
        if status != 200:
            raise ApiError(f"GET contents/{SIGNATURE_PATH}: HTTP {status}")
        data, blob = parse_contents(raw)
        return parse_signatures(data), blob

    def create_signature_branch(self, write: Transport, data: bytes, message: str) -> bool:
        """Creates ``SIGNATURE_BRANCH`` as one commit with no parents.

        The commit holds the signature file and ``BRANCH_FILES``, which say
        what the branch is and how to set the check up.

        The tree, commit and branch are created with ``write``, the
        automation App's transport, so the commit is the App's and GitHub
        signs it.

        # Returns
        ``False`` when another run created the branch first.

        # Errors
        ``ApiError`` when GitHub refuses a step.
        """
        entries: list[dict[str, object]] = []
        for name, mode in BRANCH_FILES:
            text = self.branch_files.get(name)
            if text is None:
                raise ApiError(f"scripts/cla_branch/{name} is missing, so the signature branch cannot be started")
            entries.append({"path": name, "mode": mode, "type": "blob", "content": text.decode("utf-8")})
        entries.append({"path": SIGNATURE_PATH, "mode": "100644", "type": "blob", "content": data.decode("utf-8")})
        status, raw = write("POST", f"{self.base}/git/trees", {"tree": entries})
        if status != 201:
            raise ApiError(f"POST git/trees: HTTP {status}")
        tree = parse_git_object(raw)
        status, raw = write("POST", f"{self.base}/git/commits", {"message": message, "tree": tree, "parents": []})
        if status != 201:
            raise ApiError(f"POST git/commits: HTTP {status}")
        commit = parse_git_object(raw)
        status, _ = write("POST", f"{self.base}/git/refs", {"ref": f"refs/heads/{SIGNATURE_BRANCH}", "sha": commit})
        if status == 422:
            # Either another run created the branch first, or a ruleset
            # refused the new ref; only the first is worth retrying.
            exists, _ = self.send("GET", f"{self.base}/git/ref/heads/{SIGNATURE_BRANCH}", None)
            if exists == 200:
                return False
            raise ApiError(f"POST git/refs: HTTP 422 and {SIGNATURE_BRANCH} does not exist (refused by a ruleset?)")
        if status != 201:
            raise ApiError(f"POST git/refs: HTTP {status}")
        return True

    def record(self, comment: Comment, repo_id: int, pull_number: int) -> SignatureBook:
        """Appends the commenter's signature unless it is already there.

        Retries when another run replaced the file between the read and the
        write. The write goes through ``self.write``, so the commit is made,
        and signed, by the automation App. The first signature creates the
        file, and the branch too when it does not exist.

        # Errors
        ``ApiError`` when no signature token is configured, the write is
        refused, or the file keeps changing.

        # Returns
        The signature book as written.
        """
        for _ in range(SIGNATURE_WRITE_ATTEMPTS):
            book, blob = self.signatures()
            if comment.author.id in book.signed_ids:
                return book
            write = self.write
            if write is None:
                raise ApiError("CLA_SIGNATURE_TOKEN is not set, so signatures cannot be recorded")
            data = add_signature(book, comment, repo_id, pull_number)
            message = f"chore(cla): record @{comment.author.login}'s signature from #{pull_number}"
            if blob is None:
                branch, _ = self.send("GET", f"{self.base}/git/ref/heads/{SIGNATURE_BRANCH}", None)
                if branch == 404:
                    if self.create_signature_branch(write, data, message):
                        return parse_signatures(data)
                    continue
                if branch != 200:
                    raise ApiError(f"GET git/ref/heads/{SIGNATURE_BRANCH}: HTTP {branch}")
            body: dict[str, object] = {
                "message": message,
                "content": base64.b64encode(data).decode("ascii"),
                "branch": SIGNATURE_BRANCH,
            }
            if blob is not None:
                body["sha"] = blob
            status, _ = write("PUT", f"{self.base}/contents/{SIGNATURE_PATH}", body)
            if status in (200, 201):
                return parse_signatures(data)
            if status not in (409, 422):
                raise ApiError(f"PUT contents/{SIGNATURE_PATH}: HTTP {status}")
        raise ApiError("the signature file kept changing; try recheck")

    def branch_rules(self) -> frozenset[str]:
        """Reads the rule types that apply to ``SIGNATURE_BRANCH``."""
        return parse_branch_rules(self.call("GET", f"rules/branches/{SIGNATURE_BRANCH}?per_page={PAGE_SIZE}"))

    def set_status(self, sha: str, state: str, description: str, target_url: str) -> None:
        """Posts the ``CLA`` commit status on ``sha``."""
        self.call("POST", f"statuses/{sha}", {"state": state, "context": STATUS_CONTEXT, "description": description, "target_url": target_url})

    def upsert_comment(self, number: int, comments: list[Comment], body: str, create: bool) -> None:
        """Edits the check's comment among ``comments``, or posts one when ``create`` is true."""
        existing = own_comment(comments, self.commenter.login)
        if existing is not None:
            if existing.body != body:
                self.comment_call("PATCH", f"issues/comments/{existing.id}", {"body": body})
        elif create:
            self.comment_call("POST", f"issues/{number}/comments", {"body": body})

    def comment_call(self, method: str, path: str, body: dict[str, object]) -> None:
        """Sends one comment request through the commenter's transport.

        # Errors
        ``ApiError`` when the response is not a success.
        """
        status, _ = self.commenter.send(method, f"{self.base}/{path}", body)
        if status not in (200, 201):
            raise ApiError(f"{method} {path}: HTTP {status}")


def run(event: Event, api: GitHub, cla_url: str, run_url: str) -> None:
    """Handles one workflow event.

    # Arguments
    * ``event``: the parsed event.
    * ``api``: the GitHub client.
    * ``cla_url``: the link to the agreement, for the comment.
    * ``run_url``: the workflow run, for the status link.

    # Errors
    ``ApiError`` or ``ClaInputError`` when the check cannot decide; the
    caller then reports an ``error`` status.
    """
    if event.merge_group_sha is not None:
        # Every pull request in the group passed the required CLA status to
        # enter the queue; the queue run needs the context reported too.
        api.set_status(event.merge_group_sha, "success", "Checked on each pull request in the group.", run_url)
        return
    if event.run_head is not None:
        numbers = api.pulls_from(event.run_head)
        if not numbers:
            # The pull request was closed or moved on (a later push), or its
            # branch is gone; the open pull requests still on that commit
            # decide it alone.
            book, _ = api.signatures()
            settle(api, event.run_head.sha, book, run_url)
        for number in numbers:
            check_pull(event, api, number, cla_url, run_url)
        return
    if event.pull_number is not None:
        check_pull(event, api, event.pull_number, cla_url, run_url)


def check_pull(event: Event, api: GitHub, number: int, cla_url: str, run_url: str) -> None:
    """Decides pull request ``number`` and reports its comment and status.

    # Errors
    ``ApiError`` or ``ClaInputError`` when the check cannot decide.
    """
    # Always re-read the pull request: the event can predate a later push,
    # and its commit count must match the commit pages read next.
    pull = api.pull_request(number)
    if pull.open:
        api.head_sha = pull.head_sha
    book, _ = api.signatures()
    if not pull.open:
        # A closed pull request no longer holds its head; the open ones that
        # share it decide the status alone.
        settle(api, pull.head_sha, book, run_url)
        return
    commits = api.commits(pull)
    verdict = evaluate(pull, commits, book)
    comments = api.comments(pull.number)
    if event.comment is not None and all(c.id != event.comment.id for c in comments):
        # The listing can lag behind the event that started the run.
        comments.append(event.comment)
    for comment in signing_comments(comments, verdict):
        book = api.record(comment, event.repo_id, pull.number)
    for _ in range(STATUS_ATTEMPTS):
        verdict = evaluate(pull, commits, book)
        blocked_by = blocking_pull(api, pull, book) if verdict.passed else None
        passed = verdict.passed and blocked_by is None
        api.upsert_comment(pull.number, comments, render_comment(verdict, cla_url, blocked_by), create=not passed)
        if blocked_by is not None:
            description = f"CLA: #{blocked_by}, which shares this commit, still needs signatures."
        else:
            description = status_description(verdict)
        api.set_status(pull.head_sha, "success" if passed else "failure", description, run_url)
        # A run for a signature comment (another concurrency group) may have
        # recorded a signature after this run read the file, and posted its
        # status before this one. Reading the file again after posting means
        # that, of two such runs, the one that posts last has seen every
        # signature recorded before it posted.
        latest, _ = api.signatures()
        if latest.signed_ids == book.signed_ids:
            return
        book = latest
        comments = api.comments(pull.number)


def blocking_pull(api: GitHub, pull: PullRequest, book: SignatureBook) -> int | None:
    """Finds another open pull request on ``pull``'s head that fails.

    The ``CLA`` status belongs to the commit, which other pull requests
    (another author or base) can share, so it passes only when all of them
    pass.

    # Returns
    The first failing pull request's number, or ``None``.
    """
    for number in api.open_pulls_on(pull.head_sha):
        if number == pull.number:
            continue
        other = api.pull_request(number)
        if not other.open or other.head_sha != pull.head_sha:
            continue
        if not evaluate(other, api.commits(other), book).passed:
            return number
    return None


def settle(api: GitHub, sha: str, book: SignatureBook, run_url: str) -> None:
    """Re-decides the shared status of ``sha`` from the open pull requests on it.

    Does nothing when no open pull request has ``sha`` as its head.
    """
    verdicts = []
    for number in api.open_pulls_on(sha):
        pull = api.pull_request(number)
        if pull.open and pull.head_sha == sha:
            verdicts.append((number, evaluate(pull, api.commits(pull), book)))
    if not verdicts:
        return
    failing = [number for number, verdict in verdicts if not verdict.passed]
    if failing:
        api.set_status(sha, "failure", f"CLA: #{failing[0]}, which has this commit, still needs signatures.", run_url)
    else:
        api.set_status(sha, "success", status_description(verdicts[0][1]), run_url)


def own_comment(comments: list[Comment], author: str) -> Comment | None:
    """Finds the check's earlier comment, which only the bot ``author`` can post.

    A comment another bot posted (the workflow's, from before an App was
    configured) is left alone, since only its author may edit it.
    """
    for comment in comments:
        if comment.author.login == author and comment.author.type is AccountType.BOT and comment.body.startswith(COMMENT_MARKER):
            return comment
    return None


def signing_comments(comments: list[Comment], verdict: Verdict) -> list[Comment]:
    """Picks the comments that sign for someone who has not signed yet.

    Each unsigned account's first comment that is exactly ``SIGN_PHRASE``
    (ignoring surrounding whitespace)
    counts. Reading the whole history, not only the comment that started the
    run, recovers signatures whose run was replaced in the concurrency queue.
    A comment from anyone the pull request does not need a signature from is
    ignored.
    """
    unsigned = {a.id for a in verdict.unsigned}
    picked: dict[int, Comment] = {}
    for comment in comments:
        if comment.body.strip() == SIGN_PHRASE and comment.author.id in unsigned and comment.author.id not in picked:
            picked[comment.author.id] = comment
    return list(picked.values())


def urllib_transport(token: str, api_url: str) -> Transport:
    """Returns a ``Transport`` that calls ``api_url`` with ``token``."""

    def send(method: str, path: str, body: dict[str, object] | None) -> tuple[int, bytes]:
        data = None if body is None else json.dumps(body).encode("utf-8")
        request = urllib.request.Request(f"{api_url}/{path}", data=data, method=method)
        request.add_header("Authorization", f"Bearer {token}")
        request.add_header("Accept", "application/vnd.github+json")
        request.add_header("X-GitHub-Api-Version", "2022-11-28")
        if data is not None:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return response.status, response.read(MAX_DOCUMENT_BYTES + 1)
        except urllib.error.HTTPError as err:
            return err.code, err.read(MAX_DOCUMENT_BYTES + 1)

    return send


def load_branch_files(folder: str) -> dict[str, bytes]:
    """Reads ``BRANCH_FILES`` from ``folder``, leaving out any that cannot be read."""
    files: dict[str, bytes] = {}
    for name, _ in BRANCH_FILES:
        try:
            with open(os.path.join(folder, name), "rb") as handle:
                files[name] = handle.read(MAX_SIGNATURE_BYTES)
        except OSError:
            continue
    return files


def warn_about_branch_rules(api: GitHub) -> None:
    """Prints a workflow warning naming each of ``SIGNATURE_BRANCH_RULES`` that does not apply.

    The verdict does not depend on it: a missing rule only means people
    other than the App could change the signatures.
    """
    try:
        missing = sorted(SIGNATURE_BRANCH_RULES - api.branch_rules())
    except (ApiError, ClaInputError, OSError) as err:
        print(f"::warning::cannot read the rules for {SIGNATURE_BRANCH}: {err}")
        return
    if missing:
        print(
            f"::warning::{SIGNATURE_BRANCH} is missing ruleset rules: {', '.join(missing)}. "
            f"See README.md on that branch, or scripts/cla_branch/README.md, to set them up."
        )


def main(env: dict[str, str]) -> int:
    """Runs the check from the workflow environment.

    # Arguments
    * ``env``: the process environment.

    # Returns
    0 when the status was posted, 1 when the check could not decide.
    """
    repository = env.get("GITHUB_REPOSITORY", "")
    if REPOSITORY.fullmatch(repository) is None:
        print("cla_check: GITHUB_REPOSITORY is not owner/name", file=sys.stderr)
        return 1
    server = env.get("GITHUB_SERVER_URL", "https://github.com")
    api_url = env.get("GITHUB_API_URL", "https://api.github.com")
    signature_token = env.get("CLA_SIGNATURE_TOKEN", "")
    write = urllib_transport(signature_token, api_url) if signature_token else None
    commenter = None
    app_slug = env.get("CLA_APP_SLUG", "")
    if write is not None and app_slug:
        login = f"{app_slug}[bot]"
        if BOT_LOGIN.fullmatch(login) is None:
            print("cla_check: CLA_APP_SLUG is not an App slug", file=sys.stderr)
            return 1
        commenter = Commenter(write, login)
    branch_files = load_branch_files(os.path.join(os.path.dirname(os.path.abspath(__file__)), "cla_branch"))
    api = GitHub(urllib_transport(env["GITHUB_TOKEN"], api_url), repository, write, branch_files, commenter)
    run_url = f"{server}/{repository}/actions/runs/{env.get('GITHUB_RUN_ID', '')}"
    # `HEAD` names the default branch, whatever it is called.
    cla_url = f"{server}/{repository}/blob/HEAD/CLA.md"
    try:
        with open(env["GITHUB_EVENT_PATH"], "rb") as handle:
            event = parse_event(env.get("GITHUB_EVENT_NAME", ""), handle.read(MAX_DOCUMENT_BYTES + 1))
    except (OSError, ClaInputError) as err:
        print(f"cla_check: cannot read the event: {err}", file=sys.stderr)
        return 1
    if event.kind is not EventKind.MERGE_GROUP:
        warn_about_branch_rules(api)
    try:
        run(event, api, cla_url, run_url)
    except (ApiError, ClaInputError) as err:
        print(f"cla_check: {err}", file=sys.stderr)
        sha = api.head_sha
        if sha is None and event.run_head is not None:
            sha = event.run_head.sha
        if sha is not None:
            try:
                api.set_status(sha, "error", "The CLA check could not decide; see the run.", run_url)
            except ApiError:
                pass
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(dict(os.environ)))
