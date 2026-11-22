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
- ``propose`` (``workflow_run`` of the visual recheck,
  ``.github/workflows/visual-recheck.yml``): when the default branch moves
  in a way the visual checks see, the recheck run compares each ready
  pull request with auto-merge on, merged into the new commit, before the
  merge queue does; the bot proposes for each one the same way, so images
  two pull requests only produce together get approved on the pull
  request instead of failing a merge group.
- ``recheck-targets`` (the recheck run's first job, which runs no pull
  request code): picks those pull requests and their merge commits.
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
``ARTIFACT_DIR`` (the downloaded ``ui-visual`` artifact, or for a recheck
run a folder holding its ``recheck-targets`` and ``ui-visual-<number>``
artifacts). ``recheck-targets`` reads ``GITHUB_REPOSITORY``,
``GITHUB_TOKEN``, ``DEFAULT_BRANCH``, ``BASE_SHA``, ``CHANGED_PATHS_FILE``
and ``OUT_DIR``.
"""

from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import tempfile
import time
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

#: The visual recheck workflow, as ``workflow_run.path`` names it.
RECHECK_WORKFLOW = ".github/workflows/visual-recheck.yml"

#: The recheck run's artifact listing the pull requests it compared.
RECHECK_TARGETS = "recheck-targets"

#: The file inside ``RECHECK_TARGETS`` (``vb.parse_targets``).
RECHECK_TARGETS_FILE = "targets.json"

#: A recheck run uploads each pull request's ``ui-visual`` artifact under
#: this prefix followed by the pull request number.
RECHECK_ARTIFACT_PREFIX = "ui-visual-"

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


class TooLong(BotError):
    """A listing had more pages than the bot reads."""


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
    return approved_entries(send, baselines, lock)


def approved_entries(
    send: Transport, baselines: str, lock: dict[str, str]
) -> dict[str, str]:
    """Keeps the lock entries whose commit the approval workflow tagged.

    # Errors
    ``BotError`` when a tag cannot be read.
    """
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

    A CI run of a pull request proposes for the open pull requests at its
    head; a visual recheck run (``RECHECK_WORKFLOW``) for each pull request
    its ``recheck-targets`` artifact lists (``propose_rechecked``).

    # Arguments
    * ``env``: the environment described in the module docs.
    * ``send``: reads this repository.
    * ``write``: posts the review comment, as the App.

    # Errors
    ``BotError``, ``VisualBaselinesError``, ``ClaInputError`` or ``ApiError``
    when a step fails.
    """
    repository = env["GITHUB_REPOSITORY"]
    payload = vb._json_object(
        Path(env["GITHUB_EVENT_PATH"]).read_bytes()[: vb.MAX_DOCUMENT_BYTES + 1],
        "event",
    )
    run = payload.get("workflow_run")
    if isinstance(run, dict) and run.get("path") == RECHECK_WORKFLOW:
        return propose_rechecked(env, send, write, payload)
    artifact = Path(env.get("ARTIFACT_DIR", ""))
    if not (artifact / RECORD_COMPLETE).is_file():
        return "no complete recorded set in this run"
    head = cla_check.parse_run_head(run)
    numbers = maintainer_review.GitHub(send, repository).pulls_on(head.sha, head)
    if not numbers:
        return "no open pull request on this head"
    done = []
    for number in numbers:
        lock = compared_lock(
            send, repository, env["BASELINES_REPOSITORY"], number, head.sha
        )
        done.append(propose_for(env, send, write, number, head.sha, artifact, lock))
    return "; ".join(done)


def propose_rechecked(
    env: dict[str, str],
    send: Transport,
    write: Transport,
    payload: dict[str, object],
) -> str:
    """Proposes for each pull request a visual recheck run compared.

    The run must be a ``push`` (or manual) run of ``RECHECK_WORKFLOW`` on the default
    branch of this repository, so its targets come from the default branch's
    code. Each target is proposed for only while its pull request is open,
    still at the head the run planned with and has auto-merge on; its
    images are compared with the approved entries of the merge commit's
    lock, which is what the run compared with.

    # Arguments
    * ``env``: the environment described in the module docs;
      ``ARTIFACT_DIR`` holds one folder per downloaded artifact.
    * ``send``: reads this repository.
    * ``write``: posts the review comments, as the App.
    * ``payload``: the ``workflow_run`` event.

    # Errors
    ``BotError`` when the run is not a recheck of the default branch or a
    step fails; ``VisualBaselinesError`` when the targets are malformed.
    """
    repository = env["GITHUB_REPOSITORY"]
    run = payload.get("workflow_run")
    repo = payload.get("repository")
    if not isinstance(run, dict) or not isinstance(repo, dict):
        raise BotError("recheck event is malformed")
    head_repo = run.get("head_repository")
    default = repo.get("default_branch")
    if (
        run.get("event") not in ("push", "workflow_dispatch")
        or not isinstance(default, str)
        or run.get("head_branch") != default
        or repo.get("full_name") != repository
        or not isinstance(head_repo, dict)
        or head_repo.get("full_name") != repository
    ):
        raise BotError("recheck run is not a push to this repository's default branch")
    artifact = Path(env.get("ARTIFACT_DIR", ""))
    try:
        raw = (artifact / RECHECK_TARGETS / RECHECK_TARGETS_FILE).read_bytes()
    except OSError:
        return "no recheck targets in this run"
    base, targets = vb.parse_targets(raw[: vb.MAX_DOCUMENT_BYTES + 1])
    if base != run.get("head_sha"):
        raise BotError("recheck targets were planned for another commit")
    done = []
    for target in targets:
        number = target.number
        status, data = send("GET", f"repos/{repository}/pulls/{number}", None)
        if status != 200:
            raise BotError(f"pull request #{number}: HTTP {status}")
        pull = vb._json_object(data, "pull request")
        pull_head = pull.get("head")
        if (
            pull.get("state") != "open"
            or not isinstance(pull_head, dict)
            or pull_head.get("sha") != target.head
            or pull.get("auto_merge") is None
        ):
            done.append(f"#{number}: moved on since the recheck started")
            continue
        recorded = artifact / f"{RECHECK_ARTIFACT_PREFIX}{number}"
        if not (recorded / RECORD_COMPLETE).is_file():
            done.append(f"#{number}: no complete recorded set")
            continue
        lock = approved_entries(
            send,
            env["BASELINES_REPOSITORY"],
            head_lock(send, repository, target.merge),
        )
        done.append(propose_for(env, send, write, number, target.head, recorded, lock))
    return "; ".join(done) or "nothing to recheck"


def propose_for(
    env: dict[str, str],
    send: Transport,
    write: Transport,
    number: int,
    head: str,
    artifact: Path,
    lock: dict[str, str],
) -> str:
    """Proposes one recorded set for pull request ``number``.

    # Arguments
    * ``env``: the environment described in the module docs.
    * ``send``: reads this repository.
    * ``write``: posts the review comment, as the App.
    * ``number``: the pull request.
    * ``head``: its head commit, which the comment names for approval.
    * ``artifact``: the run's ``ui-visual`` artifact for it.
    * ``lock``: the approved lock entries the run compared with.

    # Returns
    What it did, for the log.

    # Errors
    ``BotError``, ``VisualBaselinesError`` or ``ApiError`` when a step fails.
    """
    repository, baselines = env["GITHUB_REPOSITORY"], env["BASELINES_REPOSITORY"]
    bot_login = f"{env['APP_SLUG']}[bot]"
    files = vb.read_set(artifact / ARTIFACT_SET)
    vb.validate_set(files)
    proposed = dict(files)
    raw_base = f"https://raw.githubusercontent.com/{baselines}"
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
            note = (
                f"<!-- bookclerk-visual-baselines v1 head={head} set={'0' * 40} -->\n"
            )
            note += f"No visual changes left to approve at `{head[:12]}`.\n"
            post_or_update(write, repository, number, open_id, note)
            sync_continuations(send, write, repository, number, bot_login, replaces, [])
        return f"#{number}: matches the approved baselines"
    # A rerun that recorded the same changes keeps the open comment as it
    # is, so a tick in progress is not lost.
    if (
        open_id is not None
        and shown is not None
        and shown.group(2) != "0" * 40
        and set_changes(send, baselines, shown.group(2)) == changes
        and fetch_set(baselines, shown.group(2), env["BASELINES_TOKEN"]) == images
    ):
        return f"#{number}: {shown.group(2)} still pending ({len(changes)} changes)"
    commit = push_set(
        baselines,
        images,
        f"refs/heads/pending/pr-{number}",
        f"Proposed baselines for {repository}#{number} at {head}\n\n"
        + vb.render_changes(changes),
        env["BASELINES_TOKEN"],
    )
    review, *pages = vb.render_comments(
        head, commit, lock, changes, raw_base, bot_login
    )
    post_or_update(write, repository, number, open_id, review)
    sync_continuations(send, write, repository, number, bot_login, replaces, pages)
    return f"#{number}: proposed {commit} ({len(changes)} changes)" + request_review(
        write, repository, number
    )


#: Most pages of open pull requests, and of one pull request's files, read.
MAX_LIST_PAGES = 30

#: How many times a pull request's merge commit is read while GitHub
#: rebuilds it on the new default branch commit, and the pause between.
MERGE_TRIES = 12
MERGE_PAUSE_SECONDS = 10.0


def planner_selects_visual(paths: list[str]) -> bool:
    """Whether CI's planner runs the visual checks for ``paths``.

    Runs ``scripts/ci-plan.py`` from this checkout (the default branch's);
    no paths select nothing.

    # Errors
    ``BotError`` when the planner fails or answers something unexpected.
    """
    if not paths:
        return False
    planner = Path(__file__).resolve().parent / "ci-plan.py"
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
        f.write("\n".join(paths) + "\n")
        listing = f.name
    try:
        done = subprocess.run(
            [sys.executable, str(planner), "--paths-file", listing, "--format", "json"],
            capture_output=True,
            text=True,
            check=False,
        )
    finally:
        os.unlink(listing)
    if done.returncode != 0:
        raise BotError(f"ci-plan.py failed: {done.stderr.strip()[:300]}")
    try:
        jobs = json.loads(done.stdout).get("jobs")
    except (ValueError, AttributeError) as err:
        raise BotError("ci-plan.py printed no plan") from err
    selected = jobs.get("ui-visual") if isinstance(jobs, dict) else None
    if type(selected) is not bool:
        raise BotError("ci-plan.py plan has no ui-visual job")
    return selected


def _pages(send: Transport, path: str) -> list[dict[str, object]]:
    """Reads every page of a REST listing of objects, up to ``MAX_LIST_PAGES``.

    # Errors
    ``BotError`` when a page cannot be read or is not a list of objects;
    ``TooLong`` when the listing is longer than ``MAX_LIST_PAGES`` pages.
    """
    out: list[dict[str, object]] = []
    sep = "&" if "?" in path else "?"
    for page in range(1, MAX_LIST_PAGES + 1):
        data = call(send, "GET", f"{path}{sep}per_page=100&page={page}")
        try:
            items = json.loads(data)
        except ValueError as err:
            raise BotError(f"{path}: not JSON") from err
        if not isinstance(items, list) or not all(isinstance(i, dict) for i in items):
            raise BotError(f"{path}: not a list of objects")
        out.extend(items)
        if len(items) < 100:
            return out
    raise TooLong(f"{path}: more than {MAX_LIST_PAGES} pages")


def _sha_of(value: object) -> str | None:
    """``value["sha"]`` when ``value`` is an object holding a commit id."""
    sha = value.get("sha") if isinstance(value, dict) else None
    return sha if isinstance(sha, str) and vb.COMMIT_ID.fullmatch(sha) else None


def fresh_merge(
    send: Transport,
    repository: str,
    number: int,
    base: str,
    head: str,
    sleep,
) -> str | None:
    """Returns pull request ``number``'s merge commit of ``head`` into ``base``.

    GitHub rebuilds a pull request's merge commit some time after its base
    branch moves, so it is read up to ``MERGE_TRIES`` times.

    # Arguments
    * ``sleep``: pauses between tries (``time.sleep``; tests pass a stub).

    # Returns
    The merge commit id; ``None`` when the pull request conflicts with
    ``base``, moved to another head, or its merge commit never caught up.

    # Errors
    ``BotError`` when a read fails.
    """
    for attempt in range(MERGE_TRIES):
        if attempt:
            sleep(MERGE_PAUSE_SECONDS)
        pull = vb._json_object(
            call(send, "GET", f"repos/{repository}/pulls/{number}"), "pull request"
        )
        if _sha_of(pull.get("head")) != head or pull.get("mergeable") is False:
            return None
        merge = pull.get("merge_commit_sha")
        if pull.get("mergeable") is not True or not isinstance(merge, str):
            continue
        if vb.COMMIT_ID.fullmatch(merge) is None:
            return None
        commit = vb._json_object(
            call(send, "GET", f"repos/{repository}/commits/{merge}"), "commit"
        )
        parents = commit.get("parents")
        if isinstance(parents, list) and [_sha_of(p) for p in parents] == [base, head]:
            return merge
    return None


def recheck_targets(
    env: dict[str, str],
    send: Transport,
    selects_visual,
    sleep,
) -> tuple[list[vb.RecheckTarget], list[str]]:
    """Picks the pull requests a visual recheck run compares.

    When the default branch commit ``BASE_SHA`` changed what the visual
    checks see (``CHANGED_PATHS_FILE``, per the planner), these are the open
    pull requests into ``DEFAULT_BRANCH`` that are ready, have auto-merge on
    (they head for the merge queue without another CI run) and whose
    changes the visual checks see, each with its merge commit into
    ``BASE_SHA``; at most ``vb.MAX_RECHECK_TARGETS``, lowest numbers first.

    # Arguments
    * ``env``: ``GITHUB_REPOSITORY``, ``DEFAULT_BRANCH``, ``BASE_SHA`` and
      ``CHANGED_PATHS_FILE`` (one path per line).
    * ``send``: reads this repository.
    * ``selects_visual``: whether the planner runs the visual checks for a
      list of paths (``planner_selects_visual``).
    * ``sleep``: passed to ``fresh_merge``.

    # Returns
    The targets, and a note for the log on each pull request left out.

    # Errors
    ``BotError`` when a read fails or an input is malformed.
    """
    repository, base = env["GITHUB_REPOSITORY"], env["BASE_SHA"]
    if vb.COMMIT_ID.fullmatch(base) is None:
        raise BotError("BASE_SHA is not a commit id")
    changed = Path(env["CHANGED_PATHS_FILE"]).read_text(encoding="utf-8").split()
    if not selects_visual(changed):
        return [], ["the visual checks see nothing this commit changed"]
    branch = urllib.parse.quote(env["DEFAULT_BRANCH"], safe="")
    targets: list[vb.RecheckTarget] = []
    notes: list[str] = []
    pulls = _pages(send, f"repos/{repository}/pulls?state=open&base={branch}")
    candidates: list[tuple[int, str]] = []
    for pull in pulls:
        number, head = pull.get("number"), _sha_of(pull.get("head"))
        if (
            type(number) is int
            and 1 <= number <= vb.MAX_PULL_NUMBER
            and head is not None
            and pull.get("draft") is False
            and pull.get("auto_merge") is not None
        ):
            candidates.append((number, head))
    for number, head in sorted(candidates):
        if len(targets) == vb.MAX_RECHECK_TARGETS:
            notes.append(f"#{number}: over the limit of {vb.MAX_RECHECK_TARGETS}")
            continue
        try:
            files = _pages(send, f"repos/{repository}/pulls/{number}/files")
        except TooLong:
            # GitHub lists at most 3,000 files; so large a change is
            # rechecked rather than guessed about.
            files = None
        paths = [
            f[key]
            for f in files or []
            for key in ("filename", "previous_filename")
            if isinstance(f.get(key), str)
        ]
        if files is not None and not selects_visual(paths):
            continue
        merge = fresh_merge(send, repository, number, base, head, sleep)
        if merge is None:
            notes.append(f"#{number}: no merge commit with {base[:12]}")
            continue
        targets.append(vb.RecheckTarget(number, head, merge))
    return targets, notes


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
    """Runs ``propose``, ``approve`` or ``recheck-targets`` from the workflow."""
    commands = ("propose", "approve", "recheck-targets")
    if len(argv) != 1 or argv[0] not in commands:
        print(f"usage: visual_baselines_bot.py {'|'.join(commands)}", file=sys.stderr)
        return 2
    if cla_check.REPOSITORY.fullmatch(env.get("GITHUB_REPOSITORY", "")) is None:
        print(
            "visual_baselines_bot: GITHUB_REPOSITORY is not owner/name", file=sys.stderr
        )
        return 1
    api = env.get("GITHUB_API_URL", "https://api.github.com")
    send = cla_check.urllib_transport(env.get("GITHUB_TOKEN", ""), api)
    if argv[0] == "recheck-targets":
        return main_recheck_targets(env, send)
    if cla_check.REPOSITORY.fullmatch(env.get("BASELINES_REPOSITORY", "")) is None:
        print(
            "visual_baselines_bot: BASELINES_REPOSITORY is not owner/name",
            file=sys.stderr,
        )
        return 1
    if cla_check.BOT_LOGIN.fullmatch(f"{env.get('APP_SLUG', '')}[bot]") is None:
        print("visual_baselines_bot: APP_SLUG is not an App slug", file=sys.stderr)
        return 1
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


def main_recheck_targets(env: dict[str, str], send: Transport) -> int:
    """Runs ``recheck-targets``.

    Writes ``RECHECK_TARGETS_FILE`` into ``OUT_DIR`` and prints the
    ``matrix`` (a JSON list of ``{number, head, merge}``) and ``count``
    lines for ``$GITHUB_OUTPUT``; the notes go to standard error.

    # Returns
    The exit status.
    """
    try:
        targets, notes = recheck_targets(env, send, planner_selects_visual, time.sleep)
        base = env["BASE_SHA"]
        out = Path(env["OUT_DIR"])
        out.mkdir(parents=True, exist_ok=True)
        document = vb.render_targets(base, targets)
        # What the propose job reads must be what this job means.
        if vb.parse_targets(document.encode()) != (
            base,
            sorted(targets, key=lambda t: t.number),
        ):
            raise BotError("rendered recheck targets do not read back")
        (out / RECHECK_TARGETS_FILE).write_text(document, encoding="utf-8")
    except (BotError, vb.VisualBaselinesError, KeyError, OSError) as err:
        print(f"visual_baselines_bot: {err}", file=sys.stderr)
        return 1
    for note in notes:
        print(f"visual_baselines_bot: {note}", file=sys.stderr)
    matrix = json.loads(document)["targets"]
    print(f"matrix={json.dumps(matrix, separators=(',', ':'))}")
    print(f"count={len(matrix)}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], dict(os.environ)))
