#!/usr/bin/env python3
"""Tells contributors when commits on their pull request are not signed.

The default branch's ruleset accepts only commits whose signature GitHub
shows as **Verified**. A pull request with an unsigned commit passes CI and
can be approved, but GitHub never lets it into the merge queue, and nothing
on the pull request says why. This check says why, and how to fix it.

The ``Signed commits`` workflow runs this on ``workflow_run`` after the
unprivileged ``Maintainer review trigger`` workflow ran for a pull request
(opened, pushed to, reopened), on **Re-run** of the ``Signed commits`` check
run (``check_run``), and on ``merge_group``. It reads only pull request
metadata through the REST API and never checks out or runs pull request
code.

For every open pull request into the default branch (the one the ruleset
protects) whose head is the commit the event names, it reads the pull
request's commits and GitHub's verification of each one, and:

- reports the ``Signed commits`` check run on the head commit, failing when
  any commit is not verified. Like ``Maintainer review``, the check belongs
  to the commit and passes while any open pull request on it passes; the
  ruleset still judges each pull request's own commits, so this cannot let
  an unsigned commit through.
- keeps one comment on each pull request with an unverified commit, marked
  with ``COMMENT_MARKER`` and edited in place, that lists those commits with
  GitHub's reason and the steps to sign and re-sign them. Once every commit
  is verified, the comment, if there is one, says so. No comment is posted
  on a pull request whose commits were always verified.

Everything that parses bytes from GitHub (the event payload, the pull
request and its commit and comment pages) is sans-IO and fuzzed by
``scripts/tests/test_signed_commits.py``; it shares ``cla_check``'s JSON
readers and ``maintainer_review``'s event and pull request readers, so the
checks refuse the same malformed documents. Any input outside the expected
shape fails closed.

Usage (inside the workflow): ``signed_commits.py`` with ``GITHUB_TOKEN``,
``GITHUB_REPOSITORY``, ``GITHUB_EVENT_NAME``, ``GITHUB_EVENT_PATH`` and
``GITHUB_SERVER_URL`` set, plus ``SIGNED_APP_TOKEN``, an automation App
token that may create check runs and comment on pull requests, and
``SIGNED_APP_SLUG``, that App's slug. Without them the workflow's own token
reports the check run instead, and the comment is skipped with a warning
when that token may not comment. It exits 0 once every check run is reported, whatever the
verdict, and 1 when it could not decide.
"""

from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass
from enum import Enum

from cla_check import (
    BOT_LOGIN,
    MAX_COMMITS,
    MAX_DOCUMENT_BYTES,
    PAGE_SIZE,
    REPOSITORY,
    Account,
    ApiError,
    ClaInputError,
    Comment,
    PullRequest,
    Reporter,
    RunHead,
    _load_json,
    _object,
    _sha,
    _string,
    own_comment,
    parse_comment_page,
    parse_pull_refs,
    parse_pull_request,
    urllib_transport,
)
from maintainer_review import GitHub as ReviewGitHub
from maintainer_review import parse_run_head

#: Name of the check run; the one a ruleset may require.
CHECK_NAME = "Signed commits"

#: First line of the check's own comment, used to find it again.
COMMENT_MARKER = "<!-- bookclerk-signed-commits -->"

#: The check run's title when the check could not decide.
ERROR_DESCRIPTION = "The signed commits check could not decide; see the run."

#: A verification ``reason`` as GitHub spells them: lowercase words joined
#: by underscores. Reasons GitHub adds later are read too, and shown with
#: ``OTHER_REASON``.
REASON = re.compile(r"[a-z][a-z_]{0,63}", re.ASCII)

#: What each verification reason GitHub documents means for the
#: contributor, phrased to follow "GitHub doesn't verify it:".
REASON_TEXT = {
    "unsigned": "it is not signed.",
    "unknown_key": "the signing key is not on the author's GitHub account as a signing key.",
    "not_signing_key": "the key is on the author's GitHub account, but not as a signing key.",
    "no_user": "the commit's email belongs to no GitHub account.",
    "unverified_email": "the commit's email is not verified on the signer's GitHub account.",
    "bad_email": "the commit's email is not one of the signer's GitHub emails.",
    "expired_key": "the signing key has expired.",
    "gpgverify_unavailable": "GitHub could not check the signature just now; press Re-run on the check.",
    "gpgverify_error": "GitHub could not check the signature just now; press Re-run on the check.",
    "ocsp_pending": "GitHub could not check the certificate yet; press Re-run on the check.",
    "ocsp_error": "GitHub could not check the certificate's revocation status.",
    "ocsp_revoked": "the signing certificate was revoked.",
    "bad_cert": "the signing certificate is not valid.",
    "unknown_signature_type": "GitHub does not support this kind of signature.",
    "malformed_signature": "the signature cannot be read.",
    "invalid": "the signature does not match the commit.",
}

#: Shown for a reason ``REASON_TEXT`` does not list.
OTHER_REASON = "the signature is not valid (GitHub's reason: `{reason}`)."

#: Most unverified commits the comment lists; the rest are counted.
MAX_LISTED_COMMITS = 50

#: Most comment pages read when looking for the check's own comment.
MAX_COMMENT_PAGES = 30

#: Characters of a commit SHA-1 the comment shows.
SHORT_SHA = 7

#: Most pages of open pull requests read when a **Re-run** names a commit
#: the commit listing does not tie to any pull request (a fork's).
MAX_OPEN_PULL_PAGES = 5

#: A default branch name the comment may put in a shell command: letters,
#: digits, ``.``, ``_``, ``-`` and ``/``. Any other name fails closed.
DEFAULT_BRANCH = re.compile(r"[A-Za-z0-9._/-]{1,255}", re.ASCII)

#: Longest base branch name accepted, in characters.
MAX_BASE_CHARS = 255


class EventKind(Enum):
    """Which workflow event triggered the run."""

    #: The ``Maintainer review trigger`` workflow ran for a pull request.
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
    #: The head the trigger run was for, from a ``workflow_run`` payload
    #: for a ``pull_request`` run; ``None`` for a review run, which cannot
    #: change any commit.
    run_head: RunHead | None = None
    #: The merge group's head commit, from a ``merge_group`` payload.
    merge_group_sha: str | None = None
    #: The commit of a ``Signed commits`` check run someone asked to re-run,
    #: from a ``check_run`` payload; ``None`` for another check.
    rerun_sha: str | None = None
    #: The slug of the App that owns that check run.
    rerun_app: str | None = None


@dataclass(frozen=True)
class SignedCommit:
    """One pull request commit and GitHub's verification of its signature."""

    #: The commit's full SHA-1.
    sha: str
    #: Whether GitHub shows the commit as **Verified**, which is what the
    #: ruleset requires.
    verified: bool
    #: GitHub's reason, matching ``REASON``, such as ``valid`` or
    #: ``unsigned``.
    reason: str


@dataclass(frozen=True)
class Verdict:
    """Whether one pull request's commits are all verified."""

    #: The pull request number.
    number: int
    #: The pull request author.
    author: Account
    #: The branch it merges into, the repository's default branch (the one
    #: the ruleset protects); matches ``DEFAULT_BRANCH``.
    base: str
    #: How many commits the pull request has.
    total: int
    #: Its commits GitHub does not verify, oldest first.
    unverified: tuple[SignedCommit, ...] = ()

    @property
    def passed(self) -> bool:
        """Whether every commit is verified."""
        return not self.unverified


@dataclass(frozen=True)
class PullTarget:
    """A pull request and the branch it merges into."""

    #: The pull request.
    pull: PullRequest
    #: The name of its base branch.
    base: str
    #: The name of the repository's default branch, which the ruleset that
    #: requires signed commits protects; matches ``DEFAULT_BRANCH``.
    default_branch: str


def parse_pull(raw: bytes) -> PullTarget:
    """Reads a ``GET /pulls/{n}`` answer with its base and default branch.

    # Errors
    ``ClaInputError`` when the pull request has the wrong shape, the base
    name is not 1 to ``MAX_BASE_CHARS`` printable characters, or the default
    branch does not match ``DEFAULT_BRANCH``.
    """
    obj = _object(_load_json(raw), "pull request")
    pull = parse_pull_request(obj)
    base = _object(obj.get("base"), "pull request base")
    ref = _string(base.get("ref"), "base branch")
    if not 1 <= len(ref) <= MAX_BASE_CHARS or not ref.isprintable():
        raise ClaInputError("base branch has a bad name")
    default = _string(_object(base.get("repo"), "base repository").get("default_branch"), "default branch")
    if DEFAULT_BRANCH.fullmatch(default) is None:
        raise ClaInputError("default branch has a bad name")
    return PullTarget(pull=pull, base=ref, default_branch=default)


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
            head = parse_run_head(payload.get("workflow_run"))
            run = _object(payload.get("workflow_run"), "workflow run")
            return Event(kind, run_head=head if run.get("event") == "pull_request" else None)
        case EventKind.MERGE_GROUP:
            group = _object(payload.get("merge_group"), "merge group")
            return Event(kind, merge_group_sha=_sha(group.get("head_sha"), "merge group head"))
        case EventKind.CHECK_RUN:
            check = _object(payload.get("check_run"), "check run")
            sha = _sha(check.get("head_sha"), "check run head")
            app = _string(_object(check.get("app"), "check run app").get("slug"), "check run app slug")
            if _string(check.get("name"), "check run name") != CHECK_NAME:
                return Event(kind)
            return Event(kind, rerun_sha=sha, rerun_app=app)


def parse_commit_page(raw: bytes) -> list[SignedCommit]:
    """Reads one page of ``GET /pulls/{n}/commits``.

    Only the SHA-1 and ``commit.verification`` are read; author names,
    emails and messages are ignored.

    # Errors
    ``ClaInputError`` when the page or an entry has the wrong shape:
    ``verified`` must be a boolean and ``reason`` must match ``REASON``.
    """
    page = _load_json(raw)
    if not isinstance(page, list):
        raise ClaInputError("commit page is not a list")
    commits = []
    for entry in page:
        obj = _object(entry, "commit")
        sha = _sha(obj.get("sha"), "commit")
        verification = _object(_object(obj.get("commit"), "git commit").get("verification"), "verification")
        verified = verification.get("verified")
        if type(verified) is not bool:
            raise ClaInputError("verification is not a boolean")
        reason = _string(verification.get("reason"), "verification reason")
        if REASON.fullmatch(reason) is None:
            raise ClaInputError("verification reason is not a reason")
        commits.append(SignedCommit(sha=sha, verified=verified, reason=reason))
    return commits


def evaluate(pull: PullRequest, commits: list[SignedCommit], base: str = "main") -> Verdict:
    """Decides one pull request from its commits.

    # Arguments
    * ``pull``: the pull request.
    * ``commits``: all of its commits, oldest first.
    * ``base``: the default branch it merges into.

    # Returns
    The verdict: every commit GitHub does not show as verified is listed,
    whoever wrote it, maintainers and bots included, since the ruleset
    exempts nobody.
    """
    return Verdict(
        number=pull.number,
        author=pull.author,
        base=base,
        total=len(commits),
        unverified=tuple(c for c in commits if not c.verified),
    )


def reason_text(reason: str) -> str:
    """Returns the contributor-facing explanation of a verification reason."""
    text = REASON_TEXT.get(reason)
    return text if text is not None else OTHER_REASON.format(reason=reason)


def check_title(verdict: Verdict) -> str:
    """Returns the check run's one-line title for ``verdict``."""
    if verdict.passed:
        return "Every commit is signed and verified."
    noun = "commit" if verdict.total == 1 else "commits"
    if len(verdict.unverified) == 1:
        return f"1 of {verdict.total} {noun} is not verified."
    return f"{len(verdict.unverified)} of {verdict.total} {noun} are not verified."


def render_comment(verdict: Verdict, clone_url: str = "https://github.com/bookclerk/bookclerk.git") -> str:
    """Returns the check's comment on the pull request.

    Everything in it comes from validated fields: commit SHA-1s, GitHub's
    reason (lowercase letters and underscores), the commit count, the
    author's login and the default branch name, so contributor-chosen text
    cannot add links or formatting.

    # Arguments
    * ``verdict``: the pull request's verdict.
    * ``clone_url``: the repository's clone URL, which the re-sign steps
      fetch the base branch from.
    """
    if verdict.passed:
        return "\n".join(
            [
                COMMENT_MARKER,
                f"Every commit on this pull request is now signed and verified. Thanks, @{verdict.author.login}!",
            ]
        )
    count = len(verdict.unverified)
    lines = [
        COMMENT_MARKER,
        f"@{verdict.author.login}, `{verdict.base}` only accepts commits that GitHub shows as **Verified**, and "
        + ("1 commit" if count == 1 else f"{count} commits")
        + " on this pull request "
        + ("isn't" if count == 1 else "aren't")
        + ". Until "
        + ("it is" if count == 1 else "they are")
        + ", GitHub won't let this pull request into the merge queue, even once it's approved and CI is green.",
        "",
        "| Commit | Why GitHub doesn't verify it |",
        "| --- | --- |",
    ]
    for commit in verdict.unverified[:MAX_LISTED_COMMITS]:
        text = reason_text(commit.reason)
        lines.append(f"| `{commit.sha[:SHORT_SHA]}` | {text[0].upper()}{text[1:]} |")
    hidden = count - MAX_LISTED_COMMITS
    if hidden > 0:
        lines.append(f"| … | and {hidden} more |")
    lines += [
        "",
        "To fix it:",
        "",
        "1. Set up commit signing. An SSH key you already use with GitHub works "
        "([GitHub's guide](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification)):",
        "",
        "   ```sh",
        "   git config gpg.format ssh",
        "   git config user.signingkey ~/.ssh/id_ed25519.pub",
        "   git config commit.gpgsign true",
        "   ```",
        "",
        "   Then add the same key on GitHub as a **Signing key** (**Settings → SSH and GPG keys**), and make sure "
        "the email in your commits is a verified email on your account.",
        "2. Re-sign this pull request's commits without changing their content, and force-push. The rebase "
        f"starts where the branch left `{verdict.base}`, so it touches only your commits (any merge commits "
        "are flattened away):",
        "",
        "   ```sh",
        f"   git fetch {clone_url} {verdict.base}",
        "   git rebase --exec 'git commit --amend --no-edit --no-verify -S' \"$(git merge-base HEAD FETCH_HEAD)\"",
        "   git push --force-with-lease",
        "   ```",
        "",
        "This comment updates when you push. Once every commit shows **Verified**, the `Signed commits` check passes.",
    ]
    return "\n".join(lines)


def render_summary(verdict: Verdict) -> str:
    """Returns the Markdown the check run's page shows for one pull request."""
    lines = [f"#{verdict.number}: {check_title(verdict)}"]
    if verdict.passed:
        return lines[0]
    lines += ["", "The pull request's comment from this check says how to sign and re-sign them.", ""]
    for commit in verdict.unverified[:MAX_LISTED_COMMITS]:
        lines.append(f"- `{commit.sha[:SHORT_SHA]}`: {reason_text(commit.reason)}")
    hidden = len(verdict.unverified) - MAX_LISTED_COMMITS
    if hidden > 0:
        lines.append(f"- and {hidden} more")
    return "\n".join(lines)


class GitHub(ReviewGitHub):
    """The REST calls the check makes, over a ``Transport``.

    Shares the maintainer review check's reads, its listing of the open pull
    requests on a commit and its check run reporting.
    """

    #: The check run this class reports.
    check_name = CHECK_NAME

    #: The repository's clone URL, for the re-sign steps; ``main`` sets it
    #: from ``GITHUB_SERVER_URL`` and ``GITHUB_REPOSITORY``.
    clone_url = "https://github.com/bookclerk/bookclerk.git"

    def commits(self, pull: PullRequest) -> list[SignedCommit]:
        """Fetches every commit of ``pull`` with its verification.

        # Errors
        ``ApiError`` when it has more than ``MAX_COMMITS`` commits (the
        endpoint lists no more), or the pages disagree with its commit
        count or list a commit twice (a push between page reads).
        """
        if pull.commits > MAX_COMMITS:
            raise ApiError(f"pull request has more than {MAX_COMMITS} commits")
        commits: list[SignedCommit] = []
        page = 1
        while True:
            batch = parse_commit_page(self.call(f"pulls/{pull.number}/commits?per_page={PAGE_SIZE}&page={page}"))
            commits += batch
            if len(batch) < PAGE_SIZE or len(commits) >= MAX_COMMITS:
                break
            page += 1
        if len(commits) != pull.commits or len({c.sha for c in commits}) != len(commits):
            raise ApiError("commit pages do not match the pull request's commit count")
        return commits

    def open_pulls_with_head(self, sha: str) -> list[int]:
        """Lists the open pull requests whose head commit is ``sha`` by reading every open one.

        The fallback for **Re-run**, whose event names only the commit: the
        listing of pull requests containing a commit leaves out fork pull
        requests, and a re-run carries no branch to look them up by.

        # Errors
        ``ApiError`` when there are more than ``MAX_OPEN_PULL_PAGES`` pages
        of open pull requests.
        """
        numbers: list[int] = []
        for page in range(1, MAX_OPEN_PULL_PAGES + 1):
            refs = parse_pull_refs(self.call(f"pulls?state=open&per_page={PAGE_SIZE}&page={page}"))
            numbers += [r.number for r in refs if r.open and r.head_sha == sha]
            if len(refs) < PAGE_SIZE:
                return sorted(set(numbers))
        raise ApiError("too many open pull requests to look the commit up in")

    def comments(self, number: int) -> list[Comment]:
        """Fetches the comments on pull request ``number``, oldest first.

        # Errors
        ``ApiError`` when it has more than ``MAX_COMMENT_PAGES`` pages.
        """
        comments: list[Comment] = []
        for page in range(1, MAX_COMMENT_PAGES + 1):
            batch, entries = parse_comment_page(self.call(f"issues/{number}/comments?per_page={PAGE_SIZE}&page={page}"))
            comments += batch
            if entries < PAGE_SIZE:
                return comments
        raise ApiError(f"pull request has more than {MAX_COMMENT_PAGES * PAGE_SIZE} comments")

    def decide_signatures(self, number: int, sha: str) -> Verdict | None:
        """Reads and decides pull request ``number`` at head ``sha``.

        # Returns
        The verdict, or ``None`` when the pull request is closed, its head
        moved on from ``sha`` (the newer push's run decides it), or it does
        not merge into the default branch, the one the ruleset protects.
        """
        target = parse_pull(self.call(f"pulls/{number}"))
        pull = target.pull
        if not pull.open or pull.head_sha != sha or target.base != target.default_branch:
            return None
        return evaluate(pull, self.commits(pull), target.default_branch)

    def comment(self, verdict: Verdict) -> None:
        """Posts, edits or leaves the check's comment for ``verdict``.

        A pull request with an unverified commit gets the comment, posted
        once and then edited in place. A passing pull request's comment, if
        there is one, is edited to say so; none is posted.

        The comment only explains the check, so a refusal or network failure
        is printed as a warning and never fails the check.
        """
        try:
            existing = own_comment(self.comments(verdict.number), self.reporter.login, COMMENT_MARKER)
            body = render_comment(verdict, self.clone_url)
            if existing is not None:
                if existing.body == body:
                    return
                status, _ = self.reporter.send("PATCH", f"{self.base}/issues/comments/{existing.id}", {"body": body})
            elif verdict.passed:
                return
            else:
                status, _ = self.reporter.send("POST", f"{self.base}/issues/{verdict.number}/comments", {"body": body})
        except (ApiError, ClaInputError, OSError) as err:
            print(f"::warning::cannot update the signed commits comment on #{verdict.number} ({err})")
            return
        if status not in (200, 201):
            print(f"::warning::cannot update the signed commits comment on #{verdict.number} (HTTP {status})")


def settle(api: GitHub, sha: str, run_url: str, head: RunHead | None = None) -> None:
    """Decides every open pull request on ``sha``, comments, and reports the shared check.

    The check passes when any of them passes (see the module documentation).
    Does nothing when no open pull request into the default branch has
    ``sha`` as its head. Without ``head`` (a **Re-run**), a commit the
    commit listing ties to no pull request is looked up among all open
    ones, so a fork's pull request is found too.

    # Errors
    ``ApiError`` or ``ClaInputError`` when the check cannot decide.
    """
    api.head_sha = sha
    verdicts = []
    numbers = api.pulls_on(sha, head)
    if not numbers and head is None:
        numbers = api.open_pulls_with_head(sha)
    for number in numbers:
        verdict = api.decide_signatures(number, sha)
        if verdict is not None:
            verdicts.append(verdict)
    if not verdicts:
        return
    for verdict in verdicts:
        api.comment(verdict)
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
            # Every pull request in the group entered the queue through the
            # ruleset, which already required verified commits.
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
        print("signed_commits: GITHUB_REPOSITORY is not owner/name", file=sys.stderr)
        return 1
    server = env.get("GITHUB_SERVER_URL", "https://github.com")
    api_url = env.get("GITHUB_API_URL", "https://api.github.com")
    send = urllib_transport(env.get("GITHUB_TOKEN", ""), api_url)
    reporter = None
    app_token = env.get("SIGNED_APP_TOKEN", "")
    app_slug = env.get("SIGNED_APP_SLUG", "")
    if app_token and app_slug:
        login = f"{app_slug}[bot]"
        if BOT_LOGIN.fullmatch(login) is None:
            print("signed_commits: SIGNED_APP_SLUG is not an App slug", file=sys.stderr)
            return 1
        reporter = Reporter(urllib_transport(app_token, api_url), login)
    api = GitHub(send, repository, reporter)
    api.clone_url = f"{server}/{repository}.git"
    run_url = f"{server}/{repository}/actions/runs/{env.get('GITHUB_RUN_ID', '')}"
    try:
        with open(env.get("GITHUB_EVENT_PATH", ""), "rb") as handle:
            event = parse_event(env.get("GITHUB_EVENT_NAME", ""), handle.read(MAX_DOCUMENT_BYTES + 1))
    except (OSError, ClaInputError) as err:
        print(f"signed_commits: cannot read the event: {err}", file=sys.stderr)
        return 1
    try:
        run(event, api, run_url)
    except (ApiError, ClaInputError, OSError) as err:
        print(f"signed_commits: {err}", file=sys.stderr)
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
