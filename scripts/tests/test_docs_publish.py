#!/usr/bin/env python3
"""docs-publish.py against an in-memory stand-in for the GitHub Git Data API (offline)."""

from __future__ import annotations

import base64
import hashlib
import importlib.util
import json
import os
import re
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from types import ModuleType
from typing import Any
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
REPO = "Bookclerk/bookclerk-docs"
SRC = "Bookclerk/bookclerk"
SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40


def _load() -> ModuleType:
    path = ROOT / "scripts" / "docs-publish.py"
    spec = importlib.util.spec_from_file_location("docs_publish", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["docs_publish"] = module
    spec.loader.exec_module(module)
    return module


dp = _load()


def _sha(kind: str, payload: Any) -> str:
    return hashlib.sha1(f"{kind}:{json.dumps(payload, sort_keys=True)}".encode()).hexdigest()


class FakeGitHub:
    """Just enough of the GitHub REST API for docs-publish.py.

    Trees are flat path -> (mode, blob id) maps; commits keep message, tree
    and parents. Every call is recorded.
    """

    def __init__(self) -> None:
        self.blobs: dict[str, bytes] = {}
        self.trees: dict[str, dict[str, tuple[str, str]]] = {}
        self.commits: dict[str, dict[str, Any]] = {}
        self.refs: dict[str, str] = {}
        # Annotated tag objects: tag object id -> commit id.
        self.tag_objects: dict[str, str] = {}
        self.calls: list[tuple[str, str]] = []

    def _blob(self, data: bytes) -> str:
        sha = dp.blob_sha(data)
        self.blobs[sha] = data
        return sha

    def _tree(self, entries: dict[str, tuple[str, str]]) -> str:
        sha = _sha("tree", sorted(entries.items()))
        self.trees[sha] = dict(entries)
        return sha

    def _commit(self, message: str, tree: str, parents: list[str]) -> str:
        sha = _sha("commit", [message, tree, parents, len(self.commits)])
        self.commits[sha] = {"message": message, "tree": tree, "parents": parents}
        return sha

    def files(self, ref: str) -> dict[str, bytes]:
        """Return the files at a branch or tag."""
        tree = self.trees[self.commits[self.refs[ref]]["tree"]]
        return {path: self.blobs[sha] for path, (_mode, sha) in tree.items()}

    def _is_ancestor(self, old: str, new: str) -> bool:
        todo = [new]
        while todo:
            sha = todo.pop()
            if sha == old:
                return True
            todo.extend(self.commits[sha]["parents"])
        return False

    def __call__(self, method: str, path: str, body: Any) -> tuple[int, Any]:
        self.calls.append((method, path))
        prefix = f"/repos/{REPO}"
        assert path.startswith(prefix), path
        rest = path[len(prefix) :]
        if method == "GET" and (m := re.fullmatch(r"/git/ref/(heads|tags)/(.+)", rest)):
            ref = f"{m.group(1)}/{m.group(2)}"
            if not self.refs:
                return 409, {"message": "Git Repository is empty."}
            if ref not in self.refs:
                return 404, {"message": "Not Found"}
            sha = self.refs[ref]
            return 200, {"object": {"sha": sha, "type": "tag" if sha in self.tag_objects else "commit"}}
        if method == "GET" and (m := re.fullmatch(r"/git/tags/([0-9a-f]{40})", rest)):
            return 200, {"object": {"sha": self.tag_objects[m.group(1)], "type": "commit"}}
        if method == "PUT" and rest == "/contents/README.md":
            assert not self.refs, "contents API used on a non-empty repository"
            blob = self._blob(base64.b64decode(body["content"]))
            commit = self._commit(body["message"], self._tree({"README.md": ("100644", blob)}), [])
            self.refs[f"heads/{body['branch']}"] = commit
            return 201, {"commit": {"sha": commit}}
        if method == "GET" and (m := re.fullmatch(r"/git/commits/([0-9a-f]{40})", rest)):
            commit = self.commits[m.group(1)]
            return 200, {"sha": m.group(1), "message": commit["message"], "tree": {"sha": commit["tree"]}}
        if method == "GET" and (m := re.fullmatch(r"/git/trees/([0-9a-f]{40})\?recursive=1", rest)):
            entries = self.trees[m.group(1)]
            listing = [{"path": p, "mode": mode, "type": "blob", "sha": sha} for p, (mode, sha) in entries.items()]
            return 200, {"tree": listing, "truncated": False}
        if method == "POST" and rest == "/git/blobs":
            return 201, {"sha": self._blob(base64.b64decode(body["content"]))}
        if method == "POST" and rest == "/git/trees":
            entries = dict(self.trees[body["base_tree"]])
            for entry in body["tree"]:
                if "content" in entry:
                    entries[entry["path"]] = (entry["mode"], self._blob(entry["content"].encode()))
                elif entry["sha"] is None:
                    del entries[entry["path"]]
                else:
                    assert entry["sha"] in self.blobs
                    entries[entry["path"]] = (entry["mode"], entry["sha"])
            return 201, {"sha": self._tree(entries)}
        if method == "POST" and rest == "/git/commits":
            assert "author" not in body and "committer" not in body
            return 201, {"sha": self._commit(body["message"], body["tree"], body["parents"])}
        if method == "PATCH" and (m := re.fullmatch(r"/git/refs/heads/(.+)", rest)):
            ref = f"heads/{m.group(1)}"
            assert body["force"] is False
            if not self._is_ancestor(self.refs[ref], body["sha"]):
                return 422, {"message": "Update is not a fast forward"}
            self.refs[ref] = body["sha"]
            return 200, {"object": {"sha": body["sha"]}}
        if method == "POST" and rest == "/git/refs":
            ref = body["ref"].removeprefix("refs/")
            if ref in self.refs:
                return 422, {"message": "Reference already exists"}
            if body["sha"] not in self.commits:
                return 422, {"message": "Object does not exist"}
            self.refs[ref] = body["sha"]
            return 201, {"ref": body["ref"]}
        if method == "GET" and (m := re.fullmatch(r"/commits\?sha=([^&]+)&per_page=(\d+)&page=(\d+)", rest)):
            sha = self.refs.get(f"heads/{m.group(1)}")
            history = []
            while sha:
                history.append({"sha": sha, "commit": {"message": self.commits[sha]["message"]}})
                parents = self.commits[sha]["parents"]
                sha = parents[0] if parents else None
            per_page, page = int(m.group(2)), int(m.group(3))
            return 200, history[(page - 1) * per_page : page * per_page]
        raise AssertionError(f"unexpected call {method} {path}")


class PublishTest(unittest.TestCase):
    """Main mirroring and release tagging."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="docs-publish-")
        self.out = Path(self._tmp.name) / "out"
        self.gh = FakeGitHub()
        self.repo = dp.DocsRepo(REPO, self.gh)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write(self, files: dict[str, bytes | str]) -> list[Any]:
        if self.out.exists():
            for path in sorted(self.out.rglob("*"), reverse=True):
                path.unlink() if path.is_file() else path.rmdir()
        self.out.mkdir(exist_ok=True)
        (self.out / dp.OUTPUT_MARKER).write_text("marker")
        for rel, data in files.items():
            path = self.out / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data if isinstance(data, bytes) else data.encode())
        return dp.read_tree(self.out)

    def test_first_publish_initializes_and_replaces_the_placeholder(self) -> None:
        files = self.write({"README.md": "# Docs\n", "api/rust/README.md": "# Rust\n"})
        summary = dp.publish_main(self.repo, "main", files, SRC, SHA_A)
        self.assertIn("main ->", summary)
        self.assertEqual(self.gh.files("heads/main"), {"README.md": b"# Docs\n", "api/rust/README.md": b"# Rust\n"})
        head = self.gh.commits[self.gh.refs["heads/main"]]
        self.assertEqual(dp.source_commit_of(head["message"]), SHA_A)
        self.assertEqual(len(head["parents"]), 1)

    def test_rerun_and_unchanged_builds_add_no_commit(self) -> None:
        files = self.write({"README.md": "# Docs\n"})
        dp.publish_main(self.repo, "main", files, SRC, SHA_A)
        head = self.gh.refs["heads/main"]
        self.assertIn("already carries", dp.publish_main(self.repo, "main", files, SRC, SHA_A))
        self.assertIn("no changes", dp.publish_main(self.repo, "main", files, SRC, SHA_B))
        self.assertEqual(self.gh.refs["heads/main"], head)

    def test_changes_and_deletions_and_binary_files(self) -> None:
        dp.publish_main(self.repo, "main", self.write({"README.md": "# Docs\n", "old.md": "gone\n"}), SRC, SHA_A)
        files = self.write({"README.md": "# Docs v2\n", "img/logo.png": b"\x89PNG\x00\xff"})
        dp.publish_main(self.repo, "main", files, SRC, SHA_B)
        self.assertEqual(self.gh.files("heads/main"), {"README.md": b"# Docs v2\n", "img/logo.png": b"\x89PNG\x00\xff"})
        self.assertIn(("POST", f"/repos/{REPO}/git/blobs"), self.gh.calls)

    def test_release_tags_the_matching_main_commit(self) -> None:
        files = self.write({"README.md": "# Docs\n"})
        dp.publish_main(self.repo, "main", files, SRC, SHA_A)
        tagged = self.gh.refs["heads/main"]
        dp.publish_main(self.repo, "main", self.write({"README.md": "# Later\n"}), SRC, SHA_B)
        summary = dp.publish_release(self.repo, "main", files, SRC, SHA_A, "v0.1.0")
        self.assertIn("existing main commit", summary)
        self.assertEqual(self.gh.refs["tags/v0.1.0"], tagged)
        self.assertIn("already tagged", dp.publish_release(self.repo, "main", files, SRC, SHA_A, "v0.1.0"))

    def test_release_without_a_main_commit_commits_beside_main(self) -> None:
        dp.publish_main(self.repo, "main", self.write({"README.md": "# Main\n"}), SRC, SHA_A)
        head = self.gh.refs["heads/main"]
        files = self.write({"README.md": "# Release\n"})
        summary = dp.publish_release(self.repo, "main", files, SRC, SHA_C, "v1.0.0")
        self.assertIn("new commit beside main", summary)
        self.assertEqual(self.gh.refs["heads/main"], head)
        self.assertEqual(self.gh.files("tags/v1.0.0"), {"README.md": b"# Release\n"})
        tag_commit = self.gh.commits[self.gh.refs["tags/v1.0.0"]]
        self.assertEqual(tag_commit["parents"], [head])
        self.assertEqual(dp.source_commit_of(tag_commit["message"]), SHA_C)

    def test_an_existing_tag_is_never_moved(self) -> None:
        files = self.write({"README.md": "# Docs\n"})
        dp.publish_main(self.repo, "main", files, SRC, SHA_A)
        self.gh.refs["tags/v0.1.0"] = self.gh.refs["heads/main"]
        dp.publish_main(self.repo, "main", self.write({"README.md": "# Later\n"}), SRC, SHA_B)
        with self.assertRaises(dp.PublishError):
            dp.publish_release(self.repo, "main", files, SRC, SHA_B, "v0.1.0")

    def test_an_annotated_tag_on_the_same_commit_counts_as_tagged(self) -> None:
        files = self.write({"README.md": "# Docs\n"})
        dp.publish_main(self.repo, "main", files, SRC, SHA_A)
        tag_object = "d" * 40
        self.gh.tag_objects[tag_object] = self.gh.refs["heads/main"]
        self.gh.refs["tags/v0.1.0"] = tag_object
        self.assertIn("already tagged", dp.publish_release(self.repo, "main", files, SRC, SHA_A, "v0.1.0"))

    def test_a_tag_refused_for_another_reason_reports_that_reason(self) -> None:
        dp.publish_main(self.repo, "main", self.write({"README.md": "# Docs\n"}), SRC, SHA_A)
        with self.assertRaisesRegex(dp.PublishError, "Object does not exist"):
            self.repo.tag_commit("v0.1.0", "e" * 40)

    def test_truncated_tree_listing_is_refused(self) -> None:
        dp.publish_main(self.repo, "main", self.write({"README.md": "# Docs\n"}), SRC, SHA_A)

        def truncated(method: str, path: str, body: Any) -> tuple[int, Any]:
            status, data = self.gh(method, path, body)
            if "/git/trees/" in path and method == "GET":
                data = {**data, "truncated": True}
            return status, data

        with self.assertRaises(dp.PublishError):
            dp.publish_main(dp.DocsRepo(REPO, truncated), "main", self.write({"README.md": "x\n"}), SRC, SHA_B)

    def test_chunks_split_large_trees(self) -> None:
        entries = [{"path": f"f{i}.md", "content": "x" * 50} for i in range(10)]
        batches = list(dp.chunks(entries, limit=200))
        self.assertGreater(len(batches), 1)
        self.assertEqual([e for b in batches for e in b], entries)
        self.assertEqual(list(dp.chunks([{"path": "big", "content": "x" * 500}], limit=10)), [[{"path": "big", "content": "x" * 500}]])


class InputTest(unittest.TestCase):
    """Arguments and the build tree are validated before any request."""

    def test_validators(self) -> None:
        for tag in ("v0.1.0", "v1.2.3-rc.1", "release-2026", "release/v1.0", "sdk/ts/v0.2.0"):
            self.assertEqual(dp.validate_tag(tag), tag)
        bad = ("", "../main", "v1..2", "-x", "/v1", "v1/", "a//b", "a/.b", "a/-b", "v1.lock", "a.lock/b", "a@{0}")
        for tag in (*bad, "x" * 101):
            with self.subTest(tag=tag), self.assertRaises(dp.PublishError):
                dp.validate_tag(tag)
        for repo in ("bookclerk", "a/b/c", "../x", "a/.."):
            with self.subTest(repo=repo), self.assertRaises(dp.PublishError):
                dp.validate_repo(repo)
        with self.assertRaises(dp.PublishError):
            dp.validate_sha("abc")

    def test_read_tree_refuses_symlinks_git_paths_and_empty_trees(self) -> None:
        with tempfile.TemporaryDirectory(prefix="docs-publish-tree-") as tmp:
            root = Path(tmp)
            (root / dp.OUTPUT_MARKER).write_text("marker")
            with self.assertRaises(dp.PublishError):
                dp.read_tree(root)
            (root / "a.md").write_text("a")
            self.assertEqual([f.path for f in dp.read_tree(root)], ["a.md"])
            (root / "link.md").symlink_to(root / "a.md")
            with self.assertRaises(dp.PublishError):
                dp.read_tree(root)
            (root / "link.md").unlink()
            (root / ".git").mkdir()
            (root / ".git" / "config").write_text("x")
            with self.assertRaises(dp.PublishError):
                dp.read_tree(root)

    def test_blob_sha_matches_git(self) -> None:
        # `printf 'hello\n' | git hash-object --stdin`
        self.assertEqual(dp.blob_sha(b"hello\n"), "ce013625030ba8dba906f756967f9e9ca394464a")

    def test_trailer_round_trip(self) -> None:
        message = dp.commit_message(SRC, SHA_A, None)
        self.assertTrue(message.endswith(f"\n\nSource-Commit: {SHA_A}\n"))
        self.assertEqual(dp.source_commit_of(message), SHA_A)
        self.assertIsNone(dp.source_commit_of("Docs\n\nSource-Commit: nope\n"))

    def test_redirects_are_refused(self) -> None:
        handler = dp._RefuseRedirects()
        req = urllib.request.Request(f"{dp.API}/repos/{REPO}/git/refs")
        with self.assertRaises(dp.PublishError):
            handler.redirect_request(req, None, 302, "Found", {}, "https://elsewhere.example/")

    def test_network_errors_and_bad_bodies_become_publish_errors(self) -> None:
        class Opener:
            def __init__(self, error: BaseException | None, body: bytes = b"") -> None:
                self.error, self.body = error, body

            def open(self, req: Any, timeout: float) -> Any:
                if self.error is not None:
                    raise self.error
                body = self.body

                class Response:
                    status = 200

                    def __enter__(self) -> Any:
                        return self

                    def __exit__(self, *exc: object) -> None:
                        return None

                    def read(self) -> bytes:
                        return body

                return Response()

        for opener in (
            Opener(urllib.error.URLError("no route")),
            Opener(TimeoutError("timed out")),
            Opener(None, b"<html>not json</html>"),
        ):
            with self.subTest(error=opener.error), mock.patch("urllib.request.build_opener", return_value=opener):
                with self.assertRaises(dp.PublishError):
                    dp.http_requester("token")("GET", "/x", None)

    def test_main_requires_a_token(self) -> None:
        with tempfile.TemporaryDirectory(prefix="docs-publish-main-") as tmp:
            (Path(tmp) / "a.md").write_text("a")
            argv = ["main", "--dir", tmp, "--repo", REPO, "--source-repo", SRC, "--source-commit", SHA_A]
            with mock.patch.dict(os.environ, {"DOCS_TOKEN": ""}), mock.patch("sys.stderr"):
                self.assertEqual(dp.main(argv), 1)


if __name__ == "__main__":
    unittest.main()
