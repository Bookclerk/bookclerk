#!/usr/bin/env python3
"""Proposes and approves visual baselines from pull request runs.

Run by ``.github/workflows/visual-baselines.yml`` from the default branch;
it never checks out or runs pull request code
(docs/contributing/ui/visual-testing.md#baselines).

- ``propose`` (``workflow_run`` of CI): reads the set the pull request's
  ``ui-visual`` job recorded, validates it, compares it with the set the
  ``baselines.lock`` of the pull request merged into its base names (what
  CI compared with; the base branch's own lock when the pull request
  conflicts and has no approved set of its own), and when they differ commits it to
  ``pending/pr-<number>`` in the baselines repository and posts one review
  comment with the changes grouped by category and an approval checkbox,
  then requests a review from the ``visual-reviewers`` team. An unapproved
  comment is updated in place; once one is approved, later changes get a
  new comment.
- ``approve`` (``issue_comment`` ``edited``): when a user ticks that
  checkbox, or replies ``@<app> approve-visuals`` (for someone without write
  access), checks that they are a maintainer or in the ``visual-reviewers``
  team and that the pull request is open, tags the set
  ``approved/<commit>`` so it is kept, and writes the new lock to the pull request's branch (or, for
  a fork, tells the author the line to commit). Any other tick is undone.

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
    """Reads the approved set at ``commit`` of the baselines repository.

    # Errors
    ``BotError`` when the fetch fails; ``VisualBaselinesError`` when the
    commit does not hold a valid set.
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
        git(
            ["--work-tree", str(tree), "checkout", "-q", "FETCH_HEAD", "--", "."],
            repo,
            env,
        )
        files = vb.read_set(tree)
        vb.validate_set(files)
        return dict(files)


def push_set(
    repository: str, files: dict[str, bytes], ref: str, message: str, token: str
) -> str:
    """Commits ``files`` as the whole tree of a new root commit and force-pushes it to ``ref``.

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
        git(["commit", "-q", "--no-gpg-sign", "-m", message], root, env)
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


def head_lock(send: Transport, repository: str, sha: str) -> str | None:
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


def find_comment(
    send: Transport, repository: str, number: int, bot_login: str
) -> tuple[int, str] | None:
    """Returns the bot's latest review comment on pull request ``number``.

    Only comments by ``bot_login`` count, so a user who pastes the marker
    cannot pose as one.

    # Returns
    The comment's id and body, or ``None`` before the bot posted one.

    # Errors
    ``BotError`` when the listing fails.
    """
    latest: tuple[int, str] | None = None
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
                and vb.MARKER.match(body)
            ):
                latest = (cid, body)
        if len(items) < 100:
            break
    return latest


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
) -> str | None:
    """Returns the approved set pull request ``number`` is compared with.

    CI checks out the pull request merged into its base, so the lock it
    used is the merge ref's: a pull request that leaves the lock alone
    inherits the base branch's, and one approved elsewhere and merged stops
    showing as a change here. Without a merge ref (the pull request
    conflicts with its base), the head's lock stands in when it names an
    approved set (one approved on this pull request), and otherwise the
    base branch's: a branch from before the first approval still carries
    ``none``, and comparing with nothing would show every image as new. A
    lock the approval workflow never tagged reads as ``None``, so every
    image needs approval.

    # Errors
    ``BotError`` when a read fails; ``VisualBaselinesError`` when the lock
    is malformed.
    """
    try:
        lock = head_lock(send, repository, f"refs/pull/{number}/merge")
    except NotFound:
        lock = head_lock(send, repository, head)
        if not (lock and is_approved(send, baselines, lock)):
            lock = head_lock(send, repository, pull_base(send, repository, number))
    if lock and not is_approved(send, baselines, lock):
        return None
    return lock


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
        approved = fetch_set(baselines, lock, env["BASELINES_TOKEN"]) if lock else {}
        changes = vb.diff_sets(approved, proposed)
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
        if not changes:
            if open_id is not None:
                note = f"<!-- bookclerk-visual-baselines v1 head={head.sha} set={lock or '0' * 40} -->\n"
                note += f"No visual changes left to approve at `{head.sha[:12]}`.\n"
                post_or_update(write, repository, number, open_id, note)
            done.append(f"#{number}: matches the approved baselines")
            continue
        # A rerun that recorded the same images keeps the open comment as it
        # is, so a tick in progress is not lost.
        if (
            open_id is not None
            and shown is not None
            and shown.group(2) != "0" * 40
            and fetch_set(baselines, shown.group(2), env["BASELINES_TOKEN"]) == proposed
        ):
            done.append(
                f"#{number}: {shown.group(2)} still pending ({len(changes)} changes)"
            )
            continue
        commit = push_set(
            baselines,
            proposed,
            f"refs/heads/pending/pr-{number}",
            f"Proposed baselines for {repository}#{number} at {head.sha}",
            env["BASELINES_TOKEN"],
        )
        post_or_update(
            write,
            repository,
            number,
            open_id,
            vb.render_comment(head.sha, commit, lock, changes, raw_base, bot_login),
        )
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
        put = {
            "message": f"Approve visual baselines {approval.set_commit[:12]}\n\nApproved by {approval.sender} in #{approval.number}.",
            "content": base64.b64encode(f"{approval.set_commit}\n".encode()).decode(),
            "sha": current.get("sha"),
            "branch": branch,
        }
        call(write, "PUT", f"repos/{repository}/contents/{LOCK_PATH}", put)
        outcome = f"Approved by {approval.sender}; the lock on this branch now names `{approval.set_commit[:12]}`."
    else:
        outcome = (
            f"Approved by {approval.sender}. Set `{LOCK_PATH}` to `{approval.set_commit}` "
            "in a commit on this pull request's branch."
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
