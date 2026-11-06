#!/usr/bin/env python3
"""Bounded ClusterFuzzLite artifact lookups: selection, patching and CLI.

``scripts/cflite_artifacts.py`` replaces ClusterFuzzLite's walk through every
artifact in the repository with one request by name. These tests replay and
mutate GitHub API listings against an independent oracle, and drive the
patched filestore with fake ClusterFuzzLite modules to check that a lookup
is exactly one request, whatever the answer.
"""

from __future__ import annotations

import importlib.util
import json
import random
import sys
import unittest
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "cflite_artifacts_corpus"
NAME = "cifuzz-corpus-node_frame"
LISTING = "https://api.github.com/repos/Bookclerk/bookclerk/actions/artifacts"


def _load() -> ModuleType:
    spec = importlib.util.spec_from_file_location("cflite_artifacts", ROOT / "scripts" / "cflite_artifacts.py")
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["cflite_artifacts"] = module
    spec.loader.exec_module(module)
    return module


ca = _load()


def oracle(raw: bytes) -> dict | None:
    """The first entry an exact walk accepts, written without the module."""
    try:
        text = raw.decode("utf-8")
        doc = json.loads(text)
    except ValueError:
        return None
    if type(doc) is not dict or type(doc.get("artifacts")) is not list:
        return None
    for entry in doc["artifacts"]:
        if type(entry) is not dict:
            continue
        ident = entry.get("id")
        if type(ident) is not int or ident <= 0:
            continue
        if type(entry.get("name")) is not str or entry["name"] != NAME:
            continue
        if entry.get("expired") is not False:
            continue
        url = entry.get("archive_download_url")
        if type(url) is not str or url != LISTING + "/" + str(ident) + "/zip":
            continue
        return entry
    return None


def check(case: unittest.TestCase, raw: bytes, label: str) -> None:
    case.assertEqual(ca.newest_artifact(raw, NAME, LISTING), oracle(raw), label)


class ListingTests(unittest.TestCase):
    """The endpoint and the replayed seed corpus."""

    def test_listing_url(self) -> None:
        self.assertEqual(ca.listing_url("https://api.github.com/", "Bookclerk", "bookclerk"), LISTING)
        self.assertEqual(
            ca.listing_url("https://ghe.example/api/v3", "a b", "c/d"),
            "https://ghe.example/api/v3/repos/a%20b/c%2Fd/actions/artifacts",
        )

    def test_corpus_matches_oracle(self) -> None:
        seeds = sorted(CORPUS.iterdir())
        self.assertGreaterEqual(len(seeds), 8)
        for seed in seeds:
            with self.subTest(seed=seed.name):
                self.assertEqual(seed.name.split("__")[0], "artifacts")
                check(self, seed.read_bytes(), seed.name)

    def test_corpus_picks(self) -> None:
        raw = {p.name: p.read_bytes() for p in CORPUS.iterdir()}

        def pick(name: str) -> int | None:
            found = ca.newest_artifact(raw[name], NAME, LISTING)
            return None if found is None else found["id"]

        self.assertEqual(pick("artifacts__two_corpora.json"), 13432161099)
        self.assertEqual(pick("artifacts__expired_first.json"), 13427968894)
        self.assertEqual(pick("artifacts__other_name.json"), 13427968894)
        self.assertEqual(pick("artifacts__mismatched_id.json"), 13427968894)
        for name in (
            "artifacts__empty.json",
            "artifacts__foreign_download.json",
            "artifacts__odd_types.json",
            "artifacts__no_list.json",
            "artifacts__not_object.json",
        ):
            self.assertIsNone(pick(name), name)


class FuzzTests(unittest.TestCase):
    """Seeded mutation fuzzing: never raises, always agrees with the oracle."""

    VALUES: list[object] = [None, True, False, 0, 1, -1, 1.5, "", "false", NAME, LISTING + "/1/zip", [], {}, [1]]

    def mutate_json(self, rng: random.Random, node: object) -> object:
        if isinstance(node, dict) and node and rng.random() < 0.7:
            key = rng.choice(list(node))
            out = dict(node)
            if rng.random() < 0.2:
                del out[key]
            else:
                out[key] = self.mutate_json(rng, node[key])
            return out
        if isinstance(node, list) and node and rng.random() < 0.7:
            out = list(node)
            i = rng.randrange(len(out))
            out[i] = self.mutate_json(rng, out[i])
            return out
        return rng.choice(self.VALUES)

    def mutate_bytes(self, rng: random.Random, raw: bytes) -> bytes:
        data = bytearray(raw)
        for _ in range(rng.randint(1, 4)):
            pos = rng.randint(0, len(data))
            op = rng.random()
            if op < 0.4:
                data[pos:pos] = bytes([rng.choice(b'{}[]",:0aA \\\xff')])
            elif op < 0.7 and data:
                del data[min(pos, len(data) - 1)]
            elif data:
                data[min(pos, len(data) - 1)] = rng.randrange(256)
        return bytes(data)

    def test_structural_mutations(self) -> None:
        rng = random.Random(20261106)
        seeds = [json.loads(p.read_bytes()) for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            node = rng.choice(seeds)
            for _ in range(rng.randint(1, 3)):
                node = self.mutate_json(rng, node)
            raw = json.dumps(node).encode()
            check(self, raw, f"iteration {i}: {raw!r}")

    def test_byte_mutations(self) -> None:
        rng = random.Random(4840)
        seeds = [p.read_bytes() for p in sorted(CORPUS.iterdir())]
        for i in range(3000):
            raw = self.mutate_bytes(rng, rng.choice(seeds))
            check(self, raw, f"iteration {i}: {raw!r}")


class FilestoreError(Exception):
    """Stands in for ClusterFuzzLite's ``filestore.FilestoreError``."""


def fake_modules(status: int, body: bytes) -> tuple[Any, Any, Any, list[tuple[str, dict[str, Any]]]]:
    """Fake ``filestore.github_actions``, ``filestore`` and ``github_api``.

    The stock lookup pages forever; calling it fails the test.
    """
    calls: list[tuple[str, dict[str, Any]]] = []

    class Store:
        def __init__(self) -> None:
            self.config = SimpleNamespace(project_repo_owner="Bookclerk", project_repo_name="bookclerk")
            self.github_api_http_headers = {"Authorization": "token t"}

        def _find_artifact(self, name: str) -> None:
            raise AssertionError("stock lookup used")

        def _list_artifacts(self) -> None:
            raise AssertionError("stock listing used")

    def get(url: str, **kwargs: Any) -> Any:
        calls.append((url, kwargs))
        return SimpleNamespace(status_code=status, content=body)

    gha = SimpleNamespace(GithubActionsFilestore=Store)
    filestore = SimpleNamespace(FilestoreError=FilestoreError)
    github_api = SimpleNamespace(_do_get_request=get)
    return gha, filestore, github_api, calls


class InstallTests(unittest.TestCase):
    """The patched filestore makes one request per lookup."""

    def test_one_request_finds_the_newest(self) -> None:
        body = (CORPUS / "artifacts__two_corpora.json").read_bytes()
        gha, filestore, github_api, calls = fake_modules(200, body)
        ca.install(gha, filestore, github_api)
        found = gha.GithubActionsFilestore()._find_artifact(NAME)
        self.assertEqual(found["id"], 13432161099)
        self.assertEqual(len(calls), 1)
        url, kwargs = calls[0]
        self.assertEqual(url, LISTING)
        self.assertEqual(kwargs["params"], {"name": NAME, "per_page": "100"})
        self.assertEqual(kwargs["headers"], {"Authorization": "token t"})
        self.assertEqual(kwargs["timeout"], ca.REQUEST_TIMEOUT_SECONDS)

    def test_missing_artifact_is_one_request(self) -> None:
        """The case that walked 4,840 pages: a target with no corpus yet."""
        body = (CORPUS / "artifacts__empty.json").read_bytes()
        gha, filestore, github_api, calls = fake_modules(200, body)
        ca.install(gha, filestore, github_api)
        self.assertIsNone(gha.GithubActionsFilestore()._find_artifact("cifuzz-corpus-cover_wire"))
        self.assertEqual(len(calls), 1)

    def test_failed_request_raises_the_filestore_error(self) -> None:
        gha, filestore, github_api, calls = fake_modules(403, b'{"message": "API rate limit exceeded"}')
        ca.install(gha, filestore, github_api)
        with self.assertRaises(FilestoreError):
            gha.GithubActionsFilestore()._find_artifact(NAME)
        self.assertEqual(len(calls), 1)

    def test_full_listing_is_disabled(self) -> None:
        gha, filestore, github_api, calls = fake_modules(200, b"{}")
        ca.install(gha, filestore, github_api)
        with self.assertRaises(FilestoreError):
            gha.GithubActionsFilestore()._list_artifacts()
        self.assertEqual(calls, [])

    def test_fails_closed_without_the_hooks(self) -> None:
        gha, filestore, github_api, _ = fake_modules(200, b"{}")
        for broken in (
            (SimpleNamespace(), filestore, github_api),
            (SimpleNamespace(GithubActionsFilestore=object), filestore, github_api),
            (gha, SimpleNamespace(), github_api),
            (gha, SimpleNamespace(FilestoreError="no"), github_api),
            (gha, filestore, SimpleNamespace()),
        ):
            with self.subTest(broken=broken), self.assertRaises(RuntimeError):
                ca.install(*broken)

    def test_api_url_from_the_runner(self) -> None:
        gha, filestore, github_api, calls = fake_modules(200, b'{"artifacts": []}')
        ca.install(gha, filestore, github_api)
        old = ca.os.environ.get("GITHUB_API_URL")
        ca.os.environ["GITHUB_API_URL"] = "https://ghe.example/api/v3"
        try:
            gha.GithubActionsFilestore()._find_artifact(NAME)
        finally:
            if old is None:
                del ca.os.environ["GITHUB_API_URL"]
            else:
                ca.os.environ["GITHUB_API_URL"] = old
        self.assertEqual(calls[0][0], "https://ghe.example/api/v3/repos/Bookclerk/bookclerk/actions/artifacts")


class MainTests(unittest.TestCase):
    """Outside the run-fuzzers image the entrypoint refuses to fuzz."""

    def test_refuses_without_clusterfuzzlite(self) -> None:
        if importlib.util.find_spec("filestore") is not None:
            self.skipTest("a filestore module is importable here")
        self.assertEqual(ca.main(), 1)


if __name__ == "__main__":
    unittest.main()
