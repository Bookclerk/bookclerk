#!/usr/bin/env python3
"""Proposes and approves visual baselines from pull request runs.

Run by ``.github/workflows/visual-baselines.yml`` from the default branch;
it never checks out or runs pull request code
(docs/contributing/ui/visual-testing.md#baselines).

- ``propose`` (``workflow_run`` of CI): reads the set the pull request's
  ``ui-visual`` job recorded, validates it, compares it with the set the
  ``baselines.lock`` of the pull request merged into its base names (what
  CI compared with), and when they differ commits it to
  ``pending/pr-<number>`` in the baselines repository and posts (or
  updates) one review comment with the approved and proposed images and an
  approval checkbox, continued in more comments when the table is too long
  for one, then requests a review from the ``visual-reviewers`` team.
- ``approve`` (``issue_comment`` ``edited``): when a user ticks that
  checkbox, or replies ``@<app> approve-visuals`` (for someone without write
  access), checks that they are a maintainer or in the ``visual-reviewers``
  team and that the set is still the pull request's pending one, tags the set ``approved/<commit>`` so
  it is kept, and writes the new lock to the pull request's branch (or, for
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
from dataclasses import dataclass
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


@dataclass(frozen=True)
class Comments:
    """The bot's comments on one pull request."""

    #: The review comment's id and body, or ``None`` before one was posted.
    review: tuple[int, str] | None
    #: The continuation comments' ids, by part number from 1.
    parts: dict[int, int]


def find_comments(
    send: Transport, repository: str, number: int, bot_login: str
) -> Comments:
    """Finds the bot's review comment and its continuations on pull request ``number``.

    Only comments by ``bot_login`` count, so a user who pastes a marker
    cannot pose as either.

    # Errors
    ``BotError`` when the listing fails.
    """
    review: tuple[int, str] | None = None
    parts: dict[int, int] = {}
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
            if not (
                isinstance(user, dict)
                and user.get("login") == bot_login
                and isinstance(body, str)
                and type(cid) is int
            ):
                continue
            if review is None and vb.MARKER.match(body):
                review = (cid, body)
            elif part := vb.PART_MARKER.match(body):
                parts.setdefault(int(part.group(1)), cid)
        if len(items) < 100:
            break
    return Comments(review, parts)


def find_comment(
    send: Transport, repository: str, number: int, bot_login: str
) -> int | None:
    """Returns the id of the bot's review comment on pull request ``number``.

    # Errors
    ``BotError`` when the listing fails.
    """
    review = find_comments(send, repository, number, bot_login).review
    return review[0] if review else None


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


def sync_parts(
    send: Transport,
    repository: str,
    number: int,
    parts: dict[int, int],
    bodies: list[str],
) -> None:
    """Makes the continuation comments match ``bodies``.

    Updates the ones that exist, posts the missing ones and deletes any left
    over from a longer table.

    # Arguments
    * ``send``: writes comments, as the App.
    * ``repository``: ``owner/name``.
    * ``number``: the pull request.
    * ``parts``: the existing continuations, from ``find_comments``.
    * ``bodies``: the continuation bodies, part 1 first.

    # Errors
    ``BotError`` when GitHub refuses a write.
    """
    for index, body in enumerate(bodies, start=1):
        post_or_update(send, repository, number, parts.get(index), body)
    for index, cid in sorted(parts.items()):
        if index > len(bodies):
            call(
                send,
                "DELETE",
                f"repos/{repository}/issues/comments/{cid}",
                ok=(204, 404),
            )


def pending_commit(send: Transport, baselines: str, number: int) -> str | None:
    """Returns the commit ``pending/pr-<number>`` names, or ``None`` when absent.

    # Errors
    ``BotError`` when GitHub answers neither the ref nor 404.
    """
    status, data = send(
        "GET", f"repos/{baselines}/git/ref/heads/pending/pr-{number}", None
    )
    if status == 404:
        return None
    if status != 200:
        raise BotError(f"pending ref: HTTP {status}")
    target = vb._json_object(data, "pending ref").get("object")
    sha = target.get("sha") if isinstance(target, dict) else None
    if not isinstance(sha, str) or not vb.COMMIT_ID.fullmatch(sha):
        raise BotError("pending ref has no commit")
    return sha


def compared_lock(
    send: Transport, repository: str, baselines: str, number: int, head: str
) -> str | None:
    """Returns the approved set pull request ``number`` is compared with.

    CI checks out the pull request merged into its base, so the lock it
    used is the merge ref's: a pull request that leaves the lock alone
    inherits the base branch's, and one approved elsewhere and merged stops
    showing as a change here. Without a merge ref (a conflict), the head's
    lock stands in. A lock the approval workflow never tagged reads as
    ``None``, so every image needs approval.

    # Errors
    ``BotError`` when a read fails; ``VisualBaselinesError`` when the lock
    is malformed.
    """
    try:
        lock = head_lock(send, repository, f"refs/pull/{number}/merge")
    except NotFound:
        lock = head_lock(send, repository, head)
    if lock and not is_approved(send, baselines, lock):
        return None
    return lock


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
        comments = find_comments(send, repository, number, bot_login)
        shown = vb.MARKER.match(comments.review[1]) if comments.review else None
        if not changes:
            # Leave a comment that already names this set alone (it carries
            # the approval note); otherwise say the earlier proposal is moot.
            if comments.review is not None and (
                shown is None or shown.group(2) != (lock or "0" * 40)
            ):
                note = f"<!-- bookclerk-visual-baselines v1 head={head.sha} set={lock or '0' * 40} -->\n"
                note += f"No visual changes left to approve at `{head.sha[:12]}`.\n"
                post_or_update(write, repository, number, comments.review[0], note)
                sync_parts(write, repository, number, comments.parts, [])
            done.append(f"#{number}: matches the approved baselines")
            continue
        # A rerun that recorded the same images keeps the pending commit and
        # the comment as they are, so a tick in progress is not lost.
        commit = pending_commit(send, baselines, number)
        if (
            commit is not None
            and shown is not None
            and shown.group(2) == commit
            and fetch_set(baselines, commit, env["BASELINES_TOKEN"]) == proposed
        ):
            done.append(f"#{number}: {commit} still pending ({len(changes)} changes)")
            continue
        commit = push_set(
            baselines,
            proposed,
            f"refs/heads/pending/pr-{number}",
            f"Proposed baselines for {repository}#{number} at {head.sha}",
            env["BASELINES_TOKEN"],
        )
        bodies = vb.render_comments(
            head.sha, commit, lock, changes, raw_base, bot_login
        )
        post_or_update(
            write,
            repository,
            number,
            comments.review[0] if comments.review else None,
            bodies[0],
        )
        sync_parts(write, repository, number, comments.parts, bodies[1:])
        done.append(
            f"#{number}: proposed {commit} ({len(changes)} changes in {len(bodies)} comment(s))"
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
    approver passes ``may_approve``, the pull request is open and the set is
    still its pending one (pushes since the comment do not matter).

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
        comment_id = find_comment(send, repository, number, bot_login)
        if comment_id is None:
            return "no proposed set to approve"
        comment = vb._json_object(
            call(send, "GET", f"repos/{repository}/issues/comments/{comment_id}"),
            "comment",
        )
        body = str(comment.get("body", ""))
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
    # The approval is of the images shown, not of a commit: a push since the
    # comment does not void it. The pending ref still naming the set proves
    # no newer recording replaced it, and CI re-checks the head against the
    # approved set after the lock lands.
    if pull.get("state") != "open" or not isinstance(pull_head, dict):
        return refuse("Not approved: the pull request is no longer open.")
    if (
        pending_commit(baselines_write, baselines, approval.number)
        != approval.set_commit
    ):
        return refuse(
            "Not approved: a newer recording replaced this set; approve the updated comment."
        )
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
