#!/usr/bin/env python3
"""Proposes and approves visual baselines from pull request runs.

Run by ``.github/workflows/visual-baselines.yml`` from the default branch;
it never checks out or runs pull request code
(docs/contributing/ui/visual-testing.md#baselines).

- ``propose`` (``workflow_run`` of CI): reads the set the pull request's
  ``ui-visual`` job recorded, validates it, compares it with the approved
  images the ``baselines.lock`` of the pull request merged into its base
  lists (what CI compared with; without a merge ref, the base branch's lock
  with the head's entries over it). When CI's comparison ran to the end
  against that same lock, an image whose bytes differ only counts as
  changed if the comparison failed it, so anti-aliasing noise within
  Playwright's threshold is not proposed. When anything changed it commits
  the added and changed images, with one ``Change:`` line per change in the
  message, to ``pending/pr-<number>`` in the baselines repository, posts one
  review comment with the changes grouped by category and an approval
  checkbox, and requests a review from the ``visual-reviewers`` team. An
  unapproved comment is updated in place; once one is approved, later
  changes get a new comment.
- ``approve`` (``issue_comment`` ``edited``): when a user ticks that
  checkbox, or replies ``@<app> approve-visuals`` (for someone without write
  access), checks that they are a maintainer or in the ``visual-reviewers``
  team and that the pull request is open, tags the set
  ``approved/<commit>`` so it is kept, and on the pull request's branch
  locks the screenshots the set changed to it, leaving every other lock
  entry alone so the approval merges with others (or, for a fork, tells the
  author what to commit). Any other tick is undone.

Every parser lives in ``visual_baselines.py``; this module only moves bytes
between GitHub, git and those parsers.

Environment: ``GITHUB_REPOSITORY``, ``GITHUB_EVENT_PATH``, ``GITHUB_TOKEN``
(reads), ``BASELINES_REPOSITORY`` (``owner/name``), ``BASELINES_TOKEN``
(contents write on the baselines repository), ``PR_TOKEN`` (comments, and
contents write for ``approve``), ``APP_SLUG``, and for ``propose``
``ARTIFACT_DIR`` (the downloaded ``ui-visual`` artifact).
"""

from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import tempfile
import urllib.parse
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import cla_check
import maintainer_review
import visual_baselines as vb

#: The lock file's path in this repository.
LOCK_PATH = "ui/visual/baselines.lock"

#: The folder of the ``ui-visual`` artifact that holds the recorded set.
ARTIFACT_SET = "baselines"

#: The folder of the ``ui-visual`` artifact the comparison run wrote.
ARTIFACT_COMPARE = "compare"

#: The comparison run's Playwright JSON report, inside ``ARTIFACT_COMPARE``.
COMPARE_REPORT = "results.json"

#: The copy of the lock the CI job compared with.
COMPARED_LOCK = "compared.lock"

#: The file the CI job writes once the recording run passed; a set from a
#: run whose recording failed (an axe-core violation) is incomplete.
RECORD_COMPLETE = "record-complete"

#: Who the baselines repository's commits are by.
COMMITTER = ("Bookclerk visual baselines", "contact@bookclerk.app")

Transport = cla_check.Transport


class BotError(RuntimeError):
    """A GitHub or git step failed."""


class NotFound(BotError):
    """GitHub answered 404."""


def call(
    send: Transport,
    method: str,
    path: str,
    body: dict[str, object] | None = None,
    ok: tuple[int, ...] = (200,),
) -> bytes:
    """Sends one REST request and returns the body of an accepted answer.

    # Errors
    ``BotError`` when the status is not in ``ok``.
    """
    status, data = send(method, path, body)
    if status not in ok:
        raise BotError(f"{method} {path}: HTTP {status}")
    return data


def git_env(token: str) -> dict[str, str]:
    """Returns an environment that authenticates git to GitHub with ``token``.

    The token goes in an ``http.extraheader`` set through ``GIT_CONFIG_*``
    variables, never on argv or in a remote URL.
    """
    basic = base64.b64encode(f"x-access-token:{token}".encode()).decode()
    return {
        **os.environ,
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "http.https://github.com/.extraheader",
        "GIT_CONFIG_VALUE_0": f"AUTHORIZATION: basic {basic}",
        "GIT_TERMINAL_PROMPT": "0",
    }


def git(args: list[str], cwd: Path, env: dict[str, str]) -> str:
    """Runs git in ``cwd`` and returns its trimmed standard output.

    # Errors
    ``BotError`` when git exits non-zero.
    """
    done = subprocess.run(
        ["git", *args], cwd=cwd, env=env, capture_output=True, text=True, check=False
    )
    if done.returncode != 0:
        raise BotError(f"git {args[0]} failed: {done.stderr.strip()[:300]}")
    return done.stdout.strip()


def fetch_set(repository: str, commit: str, token: str) -> dict[str, bytes]:
    """Reads the images a set commit of the baselines repository holds.

    A proposed set holds only the screenshots it adds or changes, so a set
    of removals alone is empty.

    # Errors
    ``BotError`` when the fetch fails; ``VisualBaselinesError`` when the
    commit holds anything but valid screenshots.
    """
    with tempfile.TemporaryDirectory() as tmp:
        repo, tree = Path(tmp, "repo"), Path(tmp, "tree")
        repo.mkdir()
        tree.mkdir()
        env = git_env(token)
        git(["init", "-q", "--bare"], repo, env)
        git(
            [
                "fetch",
                "-q",
                "--depth",
                "1",
                f"https://github.com/{repository}.git",
                commit,
            ],
            repo,
            env,
        )
        # A set of removals alone has an empty tree, which `checkout -- .`
        # refuses.
        if git(["ls-tree", "--name-only", "FETCH_HEAD"], repo, env):
            git(
                ["--work-tree", str(tree), "checkout", "-q", "FETCH_HEAD", "--", "."],
                repo,
                env,
            )
        files = vb.read_set(tree)
        if files:
            vb.validate_set(files)
        return dict(files)


def approved_images(repository: str, lock: dict[str, str]) -> dict[str, bytes]:
    """Reads the approved image of every screenshot ``lock`` lists.

    # Arguments
    * ``repository``: the public baselines repository, ``owner/name``.
    * ``lock``: approved entries only (``compared_lock``).

    # Errors
    ``VisualBaselinesError`` when a commit is not approved, cannot be
    fetched or lacks a locked image.
    """
    with tempfile.TemporaryDirectory() as tmp:
        vb.assemble(lock, f"https://github.com/{repository}.git", Path(tmp))
        return dict(vb.read_set(Path(tmp)))


def set_changes(send: Transport, baselines: str, commit: str) -> list[tuple[str, str]]:
    """Reads the changes a proposed set commit's message lists.

    # Errors
    ``BotError`` when the commit cannot be read; ``VisualBaselinesError``
    when its message is malformed.
    """
    obj = vb._json_object(
        call(send, "GET", f"repos/{baselines}/git/commits/{commit}"), "set commit"
    )
    message = obj.get("message")
    if not isinstance(message, str):
        raise vb.VisualBaselinesError("set commit has no message")
    return vb.parse_changes(message)


def push_set(
    repository: str, files: dict[str, bytes], ref: str, message: str, token: str
) -> str:
    """Commits ``files`` as the whole tree of a new root commit and force-pushes it to ``ref``.

    ``files`` may be empty (a proposal that only removes screenshots).

    # Returns
    The new commit's id.

    # Errors
    ``BotError`` when git fails.
    """
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        env = {
            **git_env(token),
            "GIT_AUTHOR_NAME": COMMITTER[0],
            "GIT_AUTHOR_EMAIL": COMMITTER[1],
            "GIT_COMMITTER_NAME": COMMITTER[0],
            "GIT_COMMITTER_EMAIL": COMMITTER[1],
        }
        git(["init", "-q"], root, env)
        for path, data in files.items():
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_bytes(data)
        git(["add", "-A"], root, env)
        git(
            ["commit", "-q", "--allow-empty", "--no-gpg-sign", "-m", message], root, env
        )
        commit = git(["rev-parse", "HEAD"], root, env)
        git(
            [
                "push",
                "-q",
                "--force",
                f"https://github.com/{repository}.git",
                f"HEAD:{ref}",
            ],
            root,
            env,
        )
        return commit


def is_approved(send: Transport, baselines: str, commit: str) -> bool:
    """Whether ``commit`` carries the ``approved/<commit>`` tag in ``baselines``.

    Only the approval workflow creates these tags (a ruleset keeps everyone
    else out), so a lock naming any other commit was never approved.

    # Errors
    ``BotError`` when GitHub answers neither the tag nor 404.
    """
    status, data = send(
        "GET", f"repos/{baselines}/git/ref/tags/approved/{commit}", None
    )
    if status == 404:
        return False
    if status != 200:
        raise BotError(f"approved tag: HTTP {status}")
    target = vb._json_object(data, "tag ref").get("object")
    return isinstance(target, dict) and target.get("sha") == commit


def head_lock(send: Transport, repository: str, sha: str) -> dict[str, str]:
    """Reads the lock at ``sha`` (a commit or ref) of this repository.

    # Errors
    ``NotFound`` when the ref or the file is missing; ``BotError`` when it
    cannot be fetched otherwise; ``VisualBaselinesError`` when it
    is malformed.
    """
    status, raw = send(
        "GET", f"repos/{repository}/contents/{LOCK_PATH}?ref={sha}", None
    )
    if status == 404:
        raise NotFound(f"no lock at {sha}")
    if status != 200:
        raise BotError(f"lock contents: HTTP {status}")
    obj = vb._json_object(raw, "lock contents")
    content = obj.get("content")
    if not isinstance(content, str):
        raise vb.VisualBaselinesError("lock contents have no content")
    try:
        data = base64.b64decode(content, validate=False)
    except ValueError as err:
        raise vb.VisualBaselinesError("lock contents are not base64") from err
    return vb.parse_lock(data)


def bot_comments(
    send: Transport, repository: str, number: int, bot_login: str
) -> list[tuple[int, str]]:
    """Lists the comments ``bot_login`` left on pull request ``number``.

    Only comments by ``bot_login`` count, so a user who pastes a marker
    cannot pose as the bot.

    # Returns
    Each comment's id and body, oldest first.

    # Errors
    ``BotError`` when the listing fails.
    """
    found: list[tuple[int, str]] = []
    for page in range(1, 11):
        raw = call(
            send,
            "GET",
            f"repos/{repository}/issues/{number}/comments?per_page=100&page={page}",
        )
        try:
            items = json.loads(raw)
        except ValueError as err:
            raise BotError("comment listing is not JSON") from err
        if not isinstance(items, list):
            raise BotError("comment listing is not a list")
        for item in items:
            if not isinstance(item, dict):
                continue
            user, body, cid = item.get("user"), item.get("body"), item.get("id")
            if (
                isinstance(user, dict)
                and user.get("login") == bot_login
                and isinstance(body, str)
                and type(cid) is int
            ):
                found.append((cid, body))
        if len(items) < 100:
            break
    return found


def find_comment(
    send: Transport, repository: str, number: int, bot_login: str
) -> tuple[int, str] | None:
    """Returns the bot's latest review comment on pull request ``number``.

    # Returns
    The comment's id and body, or ``None`` before the bot posted one.

    # Errors
    ``BotError`` when the listing fails.
    """
    latest: tuple[int, str] | None = None
    for cid, body in bot_comments(send, repository, number, bot_login):
        if vb.MARKER.match(body):
            latest = (cid, body)
    return latest


def sync_continuations(
    send: Transport,
    write: Transport,
    repository: str,
    number: int,
    bot_login: str,
    replaces: str | None,
    pages: list[str],
) -> None:
    """Brings the continuation comments of a review comment up to date.

    The continuation comments of the set the review comment showed before
    (``replaces``) are edited into ``pages`` in order; any left over are
    deleted and any missing are posted after them. Continuation comments of
    other sets (an approved comment kept as a record) stay as they are.

    # Arguments
    * ``send``: reads this repository.
    * ``write``: edits, posts and deletes comments, as the App.
    * ``replaces``: the set the review comment showed before this update,
      or ``None`` for a new review comment.
    * ``pages``: the continuation bodies from ``render_comments``.

    # Errors
    ``BotError`` when a request fails.
    """
    old: list[tuple[int, int]] = []
    if replaces is not None:
        for cid, body in bot_comments(send, repository, number, bot_login):
            more = vb.MORE_MARKER.match(body)
            if more and more.group(1) == replaces:
                old.append((int(more.group(2)), cid))
    old.sort()
    for i, page in enumerate(pages):
        post_or_update(
            write, repository, number, old[i][1] if i < len(old) else None, page
        )
    for _, cid in old[len(pages) :]:
        call(
            write,
            "DELETE",
            f"repos/{repository}/issues/comments/{cid}",
            ok=(204,),
        )


def post_or_update(
    send: Transport, repository: str, number: int, comment_id: int | None, body: str
) -> None:
    """Updates the review comment, or posts it when there is none yet."""
    if comment_id is None:
        call(
            send,
            "POST",
            f"repos/{repository}/issues/{number}/comments",
            {"body": body},
            ok=(201,),
        )
    else:
        call(
            send,
            "PATCH",
            f"repos/{repository}/issues/comments/{comment_id}",
            {"body": body},
        )


def compared_lock(
    send: Transport, repository: str, baselines: str, number: int, head: str
) -> dict[str, str]:
    """Returns the approved lock entries pull request ``number`` is compared with.

    CI checks out the pull request merged into its base, so the lock it
    used is the merge ref's: a pull request inherits every entry the base
    branch's lock has, and screenshots approved elsewhere and merged stop
    showing as changes here. Without a merge ref (the pull request
    conflicts with its base), the base branch's lock stands in, with the
    head's entries (those approved on this pull request) over it; a head
    lock that cannot be read is left out. Entries whose commit the
    approval workflow never tagged are dropped, so those screenshots need
    approval.

    # Errors
    ``BotError`` when a read fails; ``VisualBaselinesError`` when the lock
    is malformed.
    """
    try:
        lock = head_lock(send, repository, f"refs/pull/{number}/merge")
    except NotFound:
        lock = head_lock(send, repository, pull_base(send, repository, number))
        try:
            lock.update(head_lock(send, repository, head))
        except (NotFound, vb.VisualBaselinesError):
            pass
    approved = {c for c in set(lock.values()) if is_approved(send, baselines, c)}
    return {path: commit for path, commit in lock.items() if commit in approved}


def run_mismatches(artifact: Path, lock: dict[str, str]) -> set[str] | None:
    """Returns the screenshots the run's comparison found different.

    Only a comparison that ran to the end (``vb.compare_finished``) against
    the same lock the bot compares with (the ``COMPARED_LOCK`` CI copied),
    and in which every failed test left a failed screenshot, says which
    byte differences are real. Otherwise, such as when a test failed before
    its screenshot, every byte difference counts.

    # Arguments
    * ``artifact``: the downloaded ``ui-visual`` artifact.
    * ``lock``: the lock from ``compared_lock``.

    # Returns
    The mismatched paths, or ``None`` when the comparison cannot be used.
    """
    compare = artifact / ARTIFACT_COMPARE
    results = compare / "test-results"
    try:
        report = (compare / COMPARE_REPORT).read_bytes()[: vb.MAX_REPORT_BYTES + 1]
        used = vb.parse_lock(
            (artifact / COMPARED_LOCK).read_bytes()[: vb.MAX_LOCK_BYTES + 1]
        )
        names = [
            p.relative_to(results).as_posix() for p in results.rglob("*-actual.png")
        ]
    except (OSError, ValueError):
        return None
    failed = vb.compare_finished(report)
    if used != lock or failed is None:
        return None
    mismatched = vb.compare_mismatches(names)
    if failed > len(mismatched):
        return None
    return mismatched


def pull_base(send: Transport, repository: str, number: int) -> str:
    """Returns the name of the branch pull request ``number`` merges into.

    # Errors
    ``BotError`` when the pull request cannot be read or names no base.
    """
    status, raw = send("GET", f"repos/{repository}/pulls/{number}", None)
    if status != 200:
        raise BotError(f"pull request #{number}: HTTP {status}")
    base = vb._json_object(raw, "pull request").get("base")
    ref = base.get("ref") if isinstance(base, dict) else None
    if not isinstance(ref, str) or not ref:
        raise BotError(f"pull request #{number} names no base branch")
    return urllib.parse.quote(ref, safe="")


def propose(env: dict[str, str], send: Transport, write: Transport) -> str:
    """Runs ``propose`` and returns what it did, for the log.

    # Arguments
    * ``env``: the environment described in the module docs.
    * ``send``: reads this repository.
    * ``write``: posts the review comment, as the App.

    # Errors
    ``BotError``, ``VisualBaselinesError``, ``ClaInputError`` or ``ApiError``
    when a step fails.
    """
    repository, baselines = env["GITHUB_REPOSITORY"], env["BASELINES_REPOSITORY"]
    bot_login = f"{env['APP_SLUG']}[bot]"
    artifact = Path(env.get("ARTIFACT_DIR", ""))
    if not (artifact / RECORD_COMPLETE).is_file():
        return "no complete recorded set in this run"
    payload = vb._json_object(
        Path(env["GITHUB_EVENT_PATH"]).read_bytes()[: vb.MAX_DOCUMENT_BYTES + 1],
        "event",
    )
    head = cla_check.parse_run_head(payload.get("workflow_run"))
    numbers = maintainer_review.GitHub(send, repository).pulls_on(head.sha, head)
    if not numbers:
        return "no open pull request on this head"
    files = vb.read_set(artifact / ARTIFACT_SET)
    vb.validate_set(files)
    proposed = dict(files)
    raw_base = f"https://raw.githubusercontent.com/{baselines}"
    done = []
    for number in numbers:
        lock = compared_lock(send, repository, baselines, number, head.sha)
        approved = approved_images(baselines, lock)
        changes = vb.diff_sets(approved, proposed, run_mismatches(artifact, lock))
        # The pending set holds only the images this proposal adds or
        # changes; the approval locks just those (``vb.apply_changes``).
        images = {path: proposed[path] for path, kind in changes if kind != "removed"}
        latest = find_comment(send, repository, number, bot_login)
        shown = vb.MARKER.match(latest[1]) if latest else None
        # An approved comment is a record: later changes go in a new comment.
        # An unapproved one is the open question, so it is updated in place.
        open_id = (
            latest[0]
            if latest is not None
            and shown is not None
            and not is_approved(send, baselines, shown.group(2))
            else None
        )
        replaces = shown.group(2) if open_id is not None and shown is not None else None
        if not changes:
            if open_id is not None:
                note = f"<!-- bookclerk-visual-baselines v1 head={head.sha} set={'0' * 40} -->\n"
                note += f"No visual changes left to approve at `{head.sha[:12]}`.\n"
                post_or_update(write, repository, number, open_id, note)
                sync_continuations(
                    send, write, repository, number, bot_login, replaces, []
                )
            done.append(f"#{number}: matches the approved baselines")
            continue
        # A rerun that recorded the same changes keeps the open comment as it
        # is, so a tick in progress is not lost.
        if (
            open_id is not None
            and shown is not None
            and shown.group(2) != "0" * 40
            and set_changes(send, baselines, shown.group(2)) == changes
            and fetch_set(baselines, shown.group(2), env["BASELINES_TOKEN"]) == images
        ):
            done.append(
                f"#{number}: {shown.group(2)} still pending ({len(changes)} changes)"
            )
            continue
        commit = push_set(
            baselines,
            images,
            f"refs/heads/pending/pr-{number}",
            f"Proposed baselines for {repository}#{number} at {head.sha}\n\n"
            + vb.render_changes(changes),
            env["BASELINES_TOKEN"],
        )
        review, *pages = vb.render_comments(
            head.sha, commit, lock, changes, raw_base, bot_login
        )
        post_or_update(write, repository, number, open_id, review)
        sync_continuations(send, write, repository, number, bot_login, replaces, pages)
        done.append(
            f"#{number}: proposed {commit} ({len(changes)} changes)"
            + request_review(write, repository, number)
        )
    return "; ".join(done)


def request_review(write: Transport, repository: str, number: int) -> str:
    """Asks the ``APPROVER_TEAM`` team to review pull request ``number``.

    Best effort: the proposal stands without it, so a refusal (the team
    missing, or lacking access to the repository) is only logged. GitHub
    keeps one request per team, so asking again on each proposal is
    harmless.

    # Arguments
    * ``write``: requests reviewers, as the App.
    * ``repository``: ``owner/name``.
    * ``number``: the pull request.

    # Returns
    An empty string, or a note for the log when the request failed.
    """
    status, _ = write(
        "POST",
        f"repos/{repository}/pulls/{number}/requested_reviewers",
        {"team_reviewers": [vb.APPROVER_TEAM]},
    )
    if status in (200, 201):
        return ""
    return f", review request for {vb.APPROVER_TEAM} refused (HTTP {status})"


def may_approve(
    env: dict[str, str], send: Transport, members: Transport, login: str
) -> str | None:
    """Decides whether ``login`` may approve a baselines set.

    # Arguments
    * ``env``: the environment; ``GITHUB_REPOSITORY`` names the org.
    * ``send``: reads this repository's collaborator permission.
    * ``members``: reads org team membership, as the App.
    * ``login``: who approved.

    # Returns
    ``None`` when they may, else the reason they may not.

    # Errors
    ``BotError`` when GitHub refuses a read.
    """
    repository = env["GITHUB_REPOSITORY"]
    quoted = urllib.parse.quote(login, safe="")
    role = vb.parse_role(
        call(send, "GET", f"repos/{repository}/collaborators/{quoted}/permission")
    )
    if role in vb.APPROVER_ROLES:
        return None
    org = repository.split("/")[0]
    status, data = members(
        "GET", f"orgs/{org}/teams/{vb.APPROVER_TEAM}/memberships/{quoted}", None
    )
    if status == 200 and vb.parse_membership(data):
        return None
    if status not in (200, 404):
        raise BotError(f"team membership: HTTP {status}")
    return (
        f"Not approved: {login} is not in the {vb.APPROVER_TEAM} team or a maintainer."
    )


def approve(
    env: dict[str, str], send: Transport, write: Transport, baselines_write: Transport
) -> str:
    """Runs ``approve`` and returns what it did, for the log.

    A tick of the bot's checkbox (``edited``) or an ``approve_command`` reply
    (``created``) approves the set the bot's comment proposes, once the
    approver passes ``may_approve`` and the pull request is open (pushes
    since the comment do not matter).

    # Arguments
    * ``env``: the environment described in the module docs.
    * ``send``: reads this repository (pull request, permission).
    * ``write``: edits the comment and writes the lock, as the App; also
      reads team membership.
    * ``baselines_write``: tags the approved set, as the App.

    # Errors
    ``BotError`` or ``VisualBaselinesError`` when a step fails.
    """
    repository, baselines = env["GITHUB_REPOSITORY"], env["BASELINES_REPOSITORY"]
    bot_login = f"{env['APP_SLUG']}[bot]"
    raw = Path(env["GITHUB_EVENT_PATH"]).read_bytes()[: vb.MAX_DOCUMENT_BYTES + 1]
    approval = vb.parse_approval(raw, bot_login)
    if approval is not None:
        body = str(vb._json_object(raw, "event")["comment"]["body"])  # type: ignore[index]
        restore = body.replace(
            vb.checkbox(approval.head, True), vb.checkbox(approval.head, False)
        )
    else:
        command = vb.parse_command(raw, bot_login)
        if command is None:
            return "not an approval"
        number, sender = command
        latest = find_comment(send, repository, number, bot_login)
        if latest is None:
            return "no proposed set to approve"
        comment_id, body = latest
        match = vb.MARKER.match(body)
        if match is None or vb.checkbox(match.group(1), False) not in body:
            return "no proposed set to approve"
        approval = vb.Approval(
            number, comment_id, sender, match.group(1), match.group(2)
        )
        restore = body

    def refuse(reason: str) -> str:
        call(
            write,
            "PATCH",
            f"repos/{repository}/issues/comments/{approval.comment_id}",
            {"body": f"{restore}\n> {reason}\n"},
        )
        return reason

    denied = may_approve(env, send, write, approval.sender)
    if denied:
        return refuse(denied)
    pull = vb._json_object(
        call(send, "GET", f"repos/{repository}/pulls/{approval.number}"), "pull request"
    )
    pull_head = pull.get("head")
    # The approval is of the images the comment shows, not of a commit: a
    # push since then does not void it. The set commit comes from the bot's
    # own marker (an edit may only tick the box), and the run after the lock
    # lands compares the new head with it, so screenshots that changed since
    # get a new comment and the rest stay approved.
    if pull.get("state") != "open" or not isinstance(pull_head, dict):
        return refuse("Not approved: the pull request is no longer open.")
    call(
        baselines_write,
        "POST",
        f"repos/{baselines}/git/refs",
        {
            "ref": f"refs/tags/approved/{approval.set_commit}",
            "sha": approval.set_commit,
        },
        ok=(201, 422),
    )
    if not is_approved(baselines_write, baselines, approval.set_commit):
        return refuse(
            "Not approved: the proposed set is no longer in the baselines repository."
        )
    changes = set_changes(baselines_write, baselines, approval.set_commit)
    locked = sum(1 for _, kind in changes if kind != "removed")
    dropped = len(changes) - locked
    summary = f"{locked} screenshot(s) locked to `{approval.set_commit[:12]}`" + (
        f", {dropped} removed" if dropped else ""
    )
    head_repo = pull_head.get("repo")
    branch = pull_head.get("ref")
    if (
        isinstance(head_repo, dict)
        and head_repo.get("full_name") == repository
        and isinstance(branch, str)
    ):
        current = vb._json_object(
            call(
                send,
                "GET",
                f"repos/{repository}/contents/{LOCK_PATH}?ref={urllib.parse.quote(branch, safe='')}",
            ),
            "lock contents",
        )
        content = current.get("content")
        try:
            lock = vb.parse_lock(base64.b64decode(str(content), validate=False))
        except (ValueError, vb.VisualBaselinesError):
            return refuse(
                f"Not approved: `{LOCK_PATH}` on this branch cannot be read; "
                "merge the base branch into it and try again."
            )
        lock = vb.apply_changes(lock, approval.set_commit, changes)
        put = {
            "message": f"Approve visual baselines {approval.set_commit[:12]}\n\nApproved by {approval.sender} in #{approval.number}.",
            "content": base64.b64encode(vb.render_lock(lock).encode()).decode(),
            "sha": current.get("sha"),
            "branch": branch,
        }
        call(write, "PUT", f"repos/{repository}/contents/{LOCK_PATH}", put)
        outcome = f"Approved by {approval.sender}; on this branch's lock, {summary}."
    else:
        outcome = (
            f"Approved by {approval.sender}. In `{LOCK_PATH}` on this pull request's "
            f"branch, lock the screenshots the set commit `{approval.set_commit}` adds "
            "or changes to it and drop the ones it removes (its message lists them)."
        )
    call(
        write,
        "PATCH",
        f"repos/{repository}/issues/comments/{approval.comment_id}",
        {"body": f"{body}\n> {outcome}\n"},
    )
    return outcome


def main(argv: list[str], env: dict[str, str]) -> int:
    """Runs ``propose`` or ``approve`` from the workflow environment."""
    if len(argv) != 1 or argv[0] not in ("propose", "approve"):
        print("usage: visual_baselines_bot.py propose|approve", file=sys.stderr)
        return 2
    for key in ("GITHUB_REPOSITORY", "BASELINES_REPOSITORY"):
        if cla_check.REPOSITORY.fullmatch(env.get(key, "")) is None:
            print(f"visual_baselines_bot: {key} is not owner/name", file=sys.stderr)
            return 1
    if cla_check.BOT_LOGIN.fullmatch(f"{env.get('APP_SLUG', '')}[bot]") is None:
        print("visual_baselines_bot: APP_SLUG is not an App slug", file=sys.stderr)
        return 1
    api = env.get("GITHUB_API_URL", "https://api.github.com")
    send = cla_check.urllib_transport(env.get("GITHUB_TOKEN", ""), api)
    write = cla_check.urllib_transport(env.get("PR_TOKEN", ""), api)
    try:
        if argv[0] == "propose":
            print(propose(env, send, write))
        else:
            print(
                approve(
                    env,
                    send,
                    write,
                    cla_check.urllib_transport(env.get("BASELINES_TOKEN", ""), api),
                )
            )
    except (
        BotError,
        vb.VisualBaselinesError,
        cla_check.ClaInputError,
        cla_check.ApiError,
        OSError,
    ) as err:
        print(f"visual_baselines_bot: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], dict(os.environ)))
