#!/usr/bin/env python3
"""Publish the Markdown docs tree to the docs repository through the GitHub API.

`scripts/docs-markdown.py` builds the tree (``docs/`` plus the generated API
reference) in a job with no secrets; this script runs in a second job with the
automation App token and turns that tree into one commit on the docs
repository (``Bookclerk/bookclerk-docs``). Commits go through the Git Data API
rather than ``git push``, so GitHub signs them as the App.

Two modes:

``main``
    Commit the tree onto the docs repository's ``main`` (fast-forward only)
    with a ``Source-Commit:`` trailer naming the Bookclerk commit it was built
    from. An unchanged tree, or a ``main`` that already carries this source
    commit, is a no-op.

``release``
    Tag the docs commit for a release. When a commit on the docs ``main``
    already carries the release's source commit, that commit is tagged;
    otherwise the tree is committed beside ``main`` (parented on its head,
    without moving it) and that commit is tagged. An existing tag is never
    moved.

The token is read from ``DOCS_TOKEN`` and never printed. See docs/ci.md#docs-publishing.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.request
from collections.abc import Callable, Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any

API = "https://api.github.com"
HTTP_TIMEOUT_S = 60

# Marker that `docs-markdown.py` writes into its output directory; it is
# build bookkeeping, not content.
OUTPUT_MARKER = ".bookclerk-docs-markdown"

REPO_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
# Release tags as GitHub accepts them, minus anything that could address a
# different ref or path: `/`-separated segments that each start with a letter
# or digit (so no `..`, no leading `-` or `/`, no empty segment, no `@{`).
TAG_SEGMENT = r"[A-Za-z0-9][A-Za-z0-9._+-]*"
TAG_RE = re.compile(rf"^{TAG_SEGMENT}(?:/{TAG_SEGMENT})*$")
TAG_MAX_LEN = 100
TRAILER_RE = re.compile(r"^Source-Commit: ([0-9a-f]{40})$", re.MULTILINE)

# Upper bound on the JSON payload of one create-tree call. Large trees are
# written as a chain of trees, each based on the previous one.
TREE_CHUNK_BYTES = 4 * 1024 * 1024
# Commits on the docs `main` searched for a matching `Source-Commit:` trailer.
TRAILER_SEARCH_PAGES = 5

# A request function: (method, API path, JSON body or None) -> (status, JSON).
Requester = Callable[[str, str, Any], tuple[int, Any]]


class PublishError(RuntimeError):
    """A publish step failed in a way the workflow should report."""


@dataclass(frozen=True)
class LocalFile:
    """One file of the built tree, as it will be stored in the docs repository.

    Attributes:
        path: Repository path, `/`-separated, relative, without `..`.
        data: File contents.
        sha: Git blob id of ``data`` (SHA-1 of the blob header and contents).
    """

    path: str
    data: bytes
    sha: str


def blob_sha(data: bytes) -> str:
    """Return the git blob id for ``data``.

    Args:
        data: File contents.

    Returns:
        The 40-character hex SHA-1 git uses for a blob with these contents.
    """
    header = f"blob {len(data)}\0".encode()
    return hashlib.sha1(header + data, usedforsecurity=False).hexdigest()


def validate_repo(repo: str) -> str:
    """Check an ``owner/name`` repository argument.

    Args:
        repo: The repository, such as ``Bookclerk/bookclerk-docs``.

    Returns:
        ``repo`` unchanged.

    Raises:
        PublishError: The value is not a plain ``owner/name``.
    """
    if not REPO_RE.fullmatch(repo) or ".." in repo:
        raise PublishError(f"not an owner/name repository: {repo!r}")
    return repo


def validate_sha(sha: str) -> str:
    """Check a full commit id.

    Args:
        sha: A commit id.

    Returns:
        ``sha`` unchanged.

    Raises:
        PublishError: The value is not 40 lowercase hex digits.
    """
    if not SHA_RE.fullmatch(sha):
        raise PublishError(f"not a full commit id: {sha!r}")
    return sha


def validate_tag(tag: str) -> str:
    """Check a release tag name.

    Args:
        tag: The tag, such as ``v1.2.0``.

    Returns:
        ``tag`` unchanged.

    Raises:
        PublishError: The name could address something other than one tag.
    """
    if (
        len(tag) > TAG_MAX_LEN
        or not TAG_RE.fullmatch(tag)
        or ".." in tag
        or any(segment.endswith((".lock", ".")) for segment in tag.split("/"))
    ):
        raise PublishError(f"not a plain release tag: {tag!r}")
    return tag


def read_tree(root: Path) -> list[LocalFile]:
    """Read every file under ``root`` as the content of the docs repository.

    The output marker is skipped. Symlinks are refused rather than followed,
    so the tree cannot pull in files from outside the build output.

    Args:
        root: The build output directory.

    Returns:
        The files, sorted by path.

    Raises:
        PublishError: ``root`` is missing, holds a symlink or a path git
            would not store, or holds no files.
    """
    if not root.is_dir():
        raise PublishError(f"build output is not a directory: {root}")
    files: list[LocalFile] = []
    for path in sorted(root.rglob("*")):
        rel = path.relative_to(root).as_posix()
        if path.is_symlink():
            raise PublishError(f"refusing symlink in build output: {rel}")
        if path.is_dir():
            continue
        if rel == OUTPUT_MARKER:
            continue
        parts = rel.split("/")
        if any(part in ("", ".", "..", ".git") for part in parts):
            raise PublishError(f"refusing path in build output: {rel}")
        data = path.read_bytes()
        files.append(LocalFile(path=rel, data=data, sha=blob_sha(data)))
    if not files:
        raise PublishError(f"build output is empty: {root}")
    return files


def commit_message(source_repo: str, source_commit: str, tag: str | None) -> str:
    """Build the docs commit message, ending in the ``Source-Commit:`` trailer.

    Args:
        source_repo: The repository the docs were built from.
        source_commit: The commit they were built from.
        tag: The release tag, for a release commit.

    Returns:
        The message.
    """
    subject = f"Docs for {tag}" if tag else f"Docs for {source_repo}@{source_commit[:12]}"
    return (
        f"{subject}\n\n"
        f"Built from https://github.com/{source_repo}/commit/{source_commit}.\n\n"
        f"Source-Commit: {source_commit}\n"
    )


def source_commit_of(message: str) -> str | None:
    """Return the ``Source-Commit:`` trailer of a docs commit message.

    Args:
        message: A commit message.

    Returns:
        The source commit id, or None when the message has no such trailer.
    """
    found = TRAILER_RE.findall(message)
    return found[-1] if found else None


class _RefuseRedirects(urllib.request.HTTPRedirectHandler):
    """Turn every redirect into an error, so the token never follows one."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[no-untyped-def]
        """Refuse the redirect.

        Args:
            req: The original request.
            fp: The response body.
            code: The redirect status.
            msg: The status reason.
            headers: The response headers.
            newurl: Where the response points.

        Raises:
            PublishError: Always.
        """
        raise PublishError(f"refusing redirect (HTTP {code}) from {req.full_url} to {newurl}")


def http_requester(token: str) -> Requester:
    """Return a requester that calls the GitHub REST API with ``token``.

    Redirects are refused rather than followed, so the Authorization header
    is only ever sent to ``API``.

    Args:
        token: An installation token with contents write on the docs repository.

    Returns:
        A function performing one API request. Non-2xx responses return their
        status and decoded body (or None) instead of raising; network errors,
        timeouts and undecodable success bodies raise :class:`PublishError`.
    """
    opener = urllib.request.build_opener(_RefuseRedirects)

    def request(method: str, path: str, body: Any) -> tuple[int, Any]:
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(f"{API}{path}", data=data, method=method)
        req.add_header("Accept", "application/vnd.github+json")
        req.add_header("Authorization", f"Bearer {token}")
        req.add_header("X-GitHub-Api-Version", "2022-11-28")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with opener.open(req, timeout=HTTP_TIMEOUT_S) as resp:
                raw = resp.read()
                return resp.status, json.loads(raw) if raw else None
        except urllib.error.HTTPError as err:
            raw = err.read()
            try:
                parsed = json.loads(raw) if raw else None
            except json.JSONDecodeError:
                parsed = None
            return err.code, parsed
        except json.JSONDecodeError as err:
            raise PublishError(f"{method} {path}: response is not JSON ({err})") from err
        except (urllib.error.URLError, TimeoutError, OSError) as err:
            raise PublishError(f"{method} {path}: {err}") from err

    return request


class DocsRepo:
    """The docs repository, reached through a :data:`Requester`."""

    def __init__(self, repo: str, request: Requester) -> None:
        """Wrap ``repo``.

        Args:
            repo: ``owner/name`` of the docs repository.
            request: Function performing API requests.
        """
        self.repo = validate_repo(repo)
        self._request = request

    def _call(self, method: str, path: str, body: Any = None, ok: tuple[int, ...] = (200, 201)) -> Any:
        status, data = self._request(method, f"/repos/{self.repo}{path}", body)
        if status not in ok:
            detail = data.get("message") if isinstance(data, dict) else None
            raise PublishError(f"{method} {path}: HTTP {status} {detail or ''}".rstrip())
        return data

    def branch_head(self, branch: str) -> str | None:
        """Return the commit a branch points at.

        Args:
            branch: Branch name.

        Returns:
            The commit id, or None when the branch (or the whole repository)
            does not exist yet.
        """
        status, data = self._request("GET", f"/repos/{self.repo}/git/ref/heads/{branch}", None)
        # 404: no such branch; 409: the repository has no commits at all.
        if status in (404, 409):
            return None
        if status != 200 or not isinstance(data, dict):
            raise PublishError(f"reading heads/{branch}: HTTP {status}")
        return validate_sha(data["object"]["sha"])

    def initialize(self, branch: str) -> str:
        """Create the first commit of an empty repository.

        The Git Data API refuses to write to a repository with no commits, but
        the contents API can create one. The placeholder README it writes is
        replaced by the first published tree.

        Args:
            branch: The branch to create.

        Returns:
            The new commit id.
        """
        content = b"# Bookclerk docs\n\nPublished by the Bookclerk docs workflow.\n"
        data = self._call(
            "PUT",
            "/contents/README.md",
            {
                "message": "Initialize the docs repository",
                "content": base64.b64encode(content).decode(),
                "branch": branch,
            },
        )
        return validate_sha(data["commit"]["sha"])

    def commit(self, sha: str) -> dict[str, Any]:
        """Return a commit object.

        Args:
            sha: Commit id.

        Returns:
            The Git Data API commit (``tree.sha``, ``message``, ``parents``).
        """
        return self._call("GET", f"/git/commits/{validate_sha(sha)}")

    def tree_blobs(self, tree_sha: str) -> dict[str, tuple[str, str]]:
        """Return every blob in a tree.

        Args:
            tree_sha: Tree id.

        Returns:
            Path -> (mode, blob id), for blobs only.

        Raises:
            PublishError: GitHub truncated the listing, so a diff against it
                would delete files it did not list.
        """
        data = self._call("GET", f"/git/trees/{validate_sha(tree_sha)}?recursive=1")
        if data.get("truncated"):
            raise PublishError("tree listing truncated; refusing to diff against a partial tree")
        return {
            entry["path"]: (entry["mode"], entry["sha"])
            for entry in data.get("tree", [])
            if entry.get("type") == "blob"
        }

    def create_blob(self, data: bytes) -> str:
        """Upload a binary file.

        Args:
            data: File contents.

        Returns:
            The blob id.
        """
        out = self._call("POST", "/git/blobs", {"content": base64.b64encode(data).decode(), "encoding": "base64"})
        return validate_sha(out["sha"])

    def create_tree(self, base_tree: str, entries: list[dict[str, Any]]) -> str:
        """Create a tree from ``base_tree`` plus ``entries``.

        Args:
            base_tree: Tree id the entries apply to.
            entries: Create-tree entries (additions, changes, deletions).

        Returns:
            The new tree id.
        """
        out = self._call("POST", "/git/trees", {"base_tree": base_tree, "tree": entries})
        return validate_sha(out["sha"])

    def create_commit(self, message: str, tree: str, parents: list[str]) -> str:
        """Create a commit. No author is passed, so GitHub signs it as the App.

        Args:
            message: Commit message.
            tree: Tree id.
            parents: Parent commit ids.

        Returns:
            The new commit id.
        """
        out = self._call("POST", "/git/commits", {"message": message, "tree": tree, "parents": parents})
        return validate_sha(out["sha"])

    def fast_forward(self, branch: str, sha: str) -> None:
        """Move a branch to ``sha``; GitHub refuses anything but a fast-forward.

        Args:
            branch: Branch name.
            sha: New commit id, a descendant of the current head.
        """
        self._call("PATCH", f"/git/refs/heads/{branch}", {"sha": sha, "force": False})

    def tag_target(self, tag: str) -> str | None:
        """Return the commit a tag points at, following annotated tags.

        Args:
            tag: Tag name.

        Returns:
            The commit id, or None when the tag does not exist.

        Raises:
            PublishError: Reading the tag failed for another reason.
        """
        status, data = self._request("GET", f"/repos/{self.repo}/git/ref/tags/{validate_tag(tag)}", None)
        # 404: no such tag; 409: the repository has no commits at all.
        if status in (404, 409):
            return None
        if status != 200 or not isinstance(data, dict):
            raise PublishError(f"reading tags/{tag}: HTTP {status}")
        target = data["object"]
        # An annotated tag points at a tag object; follow it to the commit.
        while target.get("type") == "tag":
            target = self._call("GET", f"/git/tags/{validate_sha(target['sha'])}")["object"]
        return validate_sha(target["sha"])

    def tag_commit(self, tag: str, sha: str) -> bool:
        """Create a lightweight tag, never moving an existing one.

        Args:
            tag: Tag name.
            sha: Commit to tag.

        Returns:
            True when the tag was created, False when it already pointed at ``sha``.

        Raises:
            PublishError: The tag exists and points elsewhere, or creating it
                failed for another reason.
        """
        status, data = self._request(
            "POST", f"/repos/{self.repo}/git/refs", {"ref": f"refs/tags/{validate_tag(tag)}", "sha": sha}
        )
        if status == 201:
            return True
        detail = data.get("message") if isinstance(data, dict) else None
        failure = f"creating tag {tag}: HTTP {status} {detail or ''}".rstrip()
        if status != 422:
            raise PublishError(failure)
        # 422 is also what an invalid sha gets, so only an existing tag
        # explains it; otherwise report the original error.
        target = self.tag_target(tag)
        if target is None:
            raise PublishError(failure)
        if target == sha:
            return False
        raise PublishError(f"tag {tag} already points at {target}, not {sha}")

    def find_source_commit(self, branch: str, source_commit: str) -> str | None:
        """Find the commit on ``branch`` whose trailer names ``source_commit``.

        Args:
            branch: Branch to search from its head.
            source_commit: Bookclerk commit id.

        Returns:
            The docs commit id, or None when none of the newest
            ``TRAILER_SEARCH_PAGES`` pages of commits matches.
        """
        for page in range(1, TRAILER_SEARCH_PAGES + 1):
            commits = self._call("GET", f"/commits?sha={branch}&per_page=100&page={page}")
            for item in commits:
                if source_commit_of(item["commit"]["message"]) == source_commit:
                    return validate_sha(item["sha"])
            if len(commits) < 100:
                break
        return None


def tree_entries(files: list[LocalFile], remote: dict[str, tuple[str, str]], repo: DocsRepo) -> list[dict[str, Any]]:
    """Diff the built tree against the remote tree.

    Text files are sent inline; other files are uploaded as blobs first.

    Args:
        files: The built tree.
        remote: Path -> (mode, blob id) of the current remote tree.
        repo: Used to upload binary blobs.

    Returns:
        Create-tree entries: changed or new files, and deletions (``sha``
        None) for remote files the build no longer has.
    """
    entries: list[dict[str, Any]] = []
    local_paths = set()
    for f in files:
        local_paths.add(f.path)
        current = remote.get(f.path)
        if current == ("100644", f.sha):
            continue
        entry: dict[str, Any] = {"path": f.path, "mode": "100644", "type": "blob"}
        try:
            entry["content"] = f.data.decode("utf-8")
        except UnicodeDecodeError:
            entry["sha"] = repo.create_blob(f.data)
        entries.append(entry)
    for path, (mode, _sha) in sorted(remote.items()):
        if path not in local_paths:
            entries.append({"path": path, "mode": mode, "type": "blob", "sha": None})
    return entries


def chunks(entries: list[dict[str, Any]], limit: int = TREE_CHUNK_BYTES) -> Iterator[list[dict[str, Any]]]:
    """Split create-tree entries into payloads of at most ``limit`` bytes.

    Args:
        entries: Create-tree entries.
        limit: Approximate JSON size bound per chunk; a single larger entry
            gets a chunk of its own.

    Yields:
        Lists of entries, in order.
    """
    batch: list[dict[str, Any]] = []
    size = 0
    for entry in entries:
        entry_size = len(json.dumps(entry))
        if batch and size + entry_size > limit:
            yield batch
            batch, size = [], 0
        batch.append(entry)
        size += entry_size
    if batch:
        yield batch


def write_tree(repo: DocsRepo, base_commit: str, files: list[LocalFile]) -> tuple[str, bool]:
    """Write the built tree on top of ``base_commit``'s tree.

    Args:
        repo: The docs repository.
        base_commit: Commit whose tree is replaced.
        files: The built tree.

    Returns:
        (new tree id, whether anything changed).
    """
    base_tree = repo.commit(base_commit)["tree"]["sha"]
    entries = tree_entries(files, repo.tree_blobs(base_tree), repo)
    tree = base_tree
    for batch in chunks(entries):
        tree = repo.create_tree(tree, batch)
    return tree, bool(entries)


def publish_main(repo: DocsRepo, branch: str, files: list[LocalFile], source_repo: str, source_commit: str) -> str:
    """Commit the built tree onto the docs ``branch``.

    Args:
        repo: The docs repository.
        branch: The branch that mirrors Bookclerk's main.
        files: The built tree.
        source_repo: Repository the docs were built from.
        source_commit: Commit they were built from.

    Returns:
        A one-line summary of what happened.
    """
    head = repo.branch_head(branch) or repo.initialize(branch)
    head_commit = repo.commit(head)
    if source_commit_of(head_commit.get("message", "")) == source_commit:
        return f"{branch} already carries {source_commit}"
    tree, changed = write_tree(repo, head, files)
    if not changed:
        return f"no changes for {source_commit}; {branch} stays at {head}"
    new = repo.create_commit(commit_message(source_repo, source_commit, None), tree, [head])
    repo.fast_forward(branch, new)
    return f"{branch} -> {new} (Source-Commit {source_commit})"


def publish_release(
    repo: DocsRepo, branch: str, files: list[LocalFile], source_repo: str, source_commit: str, tag: str
) -> str:
    """Tag the docs commit for a release, creating it when ``branch`` has none.

    Args:
        repo: The docs repository.
        branch: The branch that mirrors Bookclerk's main.
        files: The tree built from the release tag.
        source_repo: Repository the docs were built from.
        source_commit: The commit the release tag points at.
        tag: The release tag.

    Returns:
        A one-line summary of what happened.

    Raises:
        PublishError: The tag already exists on a commit built from another
            source commit.
    """
    validate_tag(tag)
    # A rerun finds the tag from the first run, which may be on a commit
    # beside main; accept it when it was built from the same source.
    existing = repo.tag_target(tag)
    if existing is not None:
        existing_source = source_commit_of(repo.commit(existing).get("message", ""))
        if existing_source != source_commit:
            raise PublishError(
                f"tag {tag} already points at {existing}, built from {existing_source}, not {source_commit}"
            )
        return f"{tag} already tagged at {existing} (Source-Commit {source_commit})"
    head = repo.branch_head(branch) or repo.initialize(branch)
    found = repo.find_source_commit(branch, source_commit)
    if found is None:
        tree, _changed = write_tree(repo, head, files)
        found = repo.create_commit(commit_message(source_repo, source_commit, tag), tree, [head])
        how = "new commit beside main"
    else:
        how = f"existing {branch} commit"
    created = repo.tag_commit(tag, found)
    state = "tagged" if created else "already tagged"
    return f"{tag} {state} at {found} ({how}, Source-Commit {source_commit})"


def main(argv: list[str] | None = None) -> int:
    """Run the CLI.

    Args:
        argv: Arguments, without the program name.

    Returns:
        The process exit status.
    """
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("mode", choices=("main", "release"))
    parser.add_argument("--dir", required=True, type=Path, help="build output of docs-markdown.py")
    parser.add_argument("--repo", required=True, help="docs repository, owner/name")
    parser.add_argument("--branch", default="main")
    parser.add_argument("--source-repo", required=True, help="repository the docs were built from")
    parser.add_argument("--source-commit", required=True, help="full commit id the docs were built from")
    parser.add_argument("--tag", help="release tag (release mode)")
    args = parser.parse_args(argv)

    token = os.environ.get("DOCS_TOKEN", "")
    try:
        if not token:
            raise PublishError("DOCS_TOKEN is not set")
        validate_repo(args.source_repo)
        validate_sha(args.source_commit)
        if not re.fullmatch(r"[A-Za-z0-9._/-]+", args.branch) or ".." in args.branch:
            raise PublishError(f"not a plain branch name: {args.branch!r}")
        files = read_tree(args.dir)
        repo = DocsRepo(args.repo, http_requester(token))
        if args.mode == "main":
            summary = publish_main(repo, args.branch, files, args.source_repo, args.source_commit)
        else:
            if not args.tag:
                raise PublishError("release mode needs --tag")
            summary = publish_release(repo, args.branch, files, args.source_repo, args.source_commit, args.tag)
    except PublishError as err:
        print(f"::error::{err}", file=sys.stderr)
        return 1
    print(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
