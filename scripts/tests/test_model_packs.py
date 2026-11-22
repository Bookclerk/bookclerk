#!/usr/bin/env python3
"""Model pack builder: pins, pack layout, tokenizer conversion and publishing.

Covers the parts of ``scripts/model_packs.py`` that need only the standard
library. The parts that need ONNX, the tokenizer converter and the plugin
(`build` and `check`) run in the ``Model packs`` workflow against the real
models.

The registry's token reply and upload address are an untrusted-input
boundary: the token goes into a request header and the address is where the
token is sent next. Every input in ``model_packs_corpus`` and every mutation
of one must give the answer of an oracle that checks characters one by one
instead of using the readers' expressions.

The layout fixtures under ``crates/bookclerk-model-pack/tests/fixtures`` are
shared with that crate's ``builder_fixture`` test: this test checks the
builder writes exactly those bytes, the Rust test that the plugin's readers
accept them. ``UPDATE_MODEL_PACK_FIXTURES=1`` rewrites them.
"""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
import random
import struct
import sys
import tempfile
import unittest
import urllib.request
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
CORPUS = Path(__file__).resolve().parent / "model_packs_corpus"
FIXTURES = ROOT / "crates" / "bookclerk-model-pack" / "tests" / "fixtures"


def _load(name: str, path: Path) -> Any:
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


mp = _load("model_packs", ROOT / "scripts" / "model_packs.py")

HEX_A = "a" * 64
HEX_B = "b" * 64
REVISION = "c" * 40


def source(path: str = "onnx/model.onnx", size: int = 5, digest: str = HEX_A) -> dict[str, Any]:
    """One upstream file entry of a pins document."""
    return {"repository": "owner/name", "revision": REVISION, "path": path, "bytes": size, "sha256": digest}


def pins_document() -> dict[str, Any]:
    """A small valid pins document."""
    return {
        "source": "https://github.com/example/repo",
        "registry": "registry.example/owner/models",
        "models": {
            "tiny": {
                "license": "MIT",
                "graph": source(),
                "graph_data": None,
                "tokenizer": source("tokenizer.json"),
                "dimensions": 4,
                "max_tokens": 16,
                "pooling": "mean",
                "output": "last_hidden_state",
                "query_prefix": "Query: ",
                "document_prefix": "",
                "literal_pieces": "added",
                "pack": {"model_json_sha256": HEX_A, "manifest_sha256": HEX_B},
            }
        },
    }


def tiny_spec() -> Any:
    """The model of `pins_document`."""
    return mp.parse_pins(json.dumps(pins_document())).models["tiny"]


def tokenizer_json(pre_tokenizer: dict[str, Any] | None = None) -> dict[str, Any]:
    """A small byte-fallback BPE `tokenizer.json`."""
    vocab = ["<pad>", "<eos>", "<bos>", "<unk>", "\n", "<unused0>", "<0x41>", "\t", "▁", "a", "b", "ab", "▁ab", "abab"]
    return {
        "added_tokens": [
            {"id": 0, "content": "<pad>", "special": True},
            {"id": 1, "content": "<eos>", "special": True},
            {"id": 2, "content": "<bos>", "special": True},
            {"id": 3, "content": "<unk>", "special": True},
            {"id": 4, "content": "\n", "special": False},
            {"id": 5, "content": "<unused0>", "special": False},
        ],
        "pre_tokenizer": pre_tokenizer or {"type": "Metaspace", "replacement": "▁", "prepend_scheme": "always", "split": False},
        "post_processor": {
            "type": "TemplateProcessing",
            "single": [
                {"SpecialToken": {"id": "<bos>", "type_id": 0}},
                {"Sequence": {"id": "A", "type_id": 0}},
                {"SpecialToken": {"id": "<eos>", "type_id": 0}},
            ],
        },
        "model": {
            "type": "BPE",
            "unk_token": "<unk>",
            "byte_fallback": True,
            "vocab": {piece: index for index, piece in enumerate(vocab)},
            "merges": ["a b", ["▁", "ab"], "ab ab", "a b"],
        },
    }


class PinsTest(unittest.TestCase):
    def test_the_committed_pins_parse(self) -> None:
        pins = mp.parse_pins(mp.MODELS_FILE.read_text(encoding="utf-8"))
        self.assertEqual(sorted(pins.models), ["bekko-a8m", "harrier-270m-q4"])
        self.assertEqual(pins.registry, pins.registry.lower())
        for spec in pins.models.values():
            self.assertEqual(spec.license, "MIT")
            for pin in spec.sources():
                self.assertTrue(pin.url.startswith(f"https://huggingface.co/{pin.repository}/resolve/{pin.revision}/"))
        harrier = pins.models["harrier-270m-q4"]
        self.assertIsNotNone(harrier.graph_data)
        self.assertEqual(len(harrier.sources()), 3)
        self.assertEqual(len(pins.models["bekko-a8m"].sources()), 2)

    def test_the_committed_pins_are_formatted_as_the_writer_formats_them(self) -> None:
        text = mp.MODELS_FILE.read_text(encoding="utf-8")
        spec = mp.parse_pins(text).models["bekko-a8m"]
        self.assertEqual(mp.rewrite_pack_pins(text, "bekko-a8m", spec.model_json_sha256, spec.manifest_sha256), text)

    def test_rewriting_changes_one_models_digests(self) -> None:
        text = json.dumps(pins_document())
        rewritten = mp.parse_pins(mp.rewrite_pack_pins(text, "tiny", HEX_B, HEX_A)).models["tiny"]
        self.assertEqual((rewritten.model_json_sha256, rewritten.manifest_sha256), (HEX_B, HEX_A))
        with self.assertRaises(mp.PackError):
            mp.rewrite_pack_pins(text, "other", HEX_B, HEX_A)

    def test_malformed_pins_are_refused(self) -> None:
        def broken(change: Any) -> str:
            document = pins_document()
            change(document)
            return json.dumps(document)

        model = lambda d: d["models"]["tiny"]  # noqa: E731
        cases = [
            "not json",
            "[]",
            json.dumps({"models": []}),
            broken(lambda d: d.update(source="http://example")),
            broken(lambda d: d.update(registry="https://registry.example/x")),
            broken(lambda d: d.update(registry="nohost")),
            broken(lambda d: d.update(models={})),
            broken(lambda d: d["models"].update({"Bad Id": model(d)})),
            broken(lambda d: d["models"].update(tiny=[])),
            broken(lambda d: model(d).update(graph="x")),
            broken(lambda d: model(d).update(license=1)),
            broken(lambda d: model(d).update(dimensions=0)),
            broken(lambda d: model(d).update(max_tokens=True)),
            broken(lambda d: model(d).update(pooling="max")),
            broken(lambda d: model(d).update(literal_pieces="some")),
            broken(lambda d: model(d).update(pack=None)),
            broken(lambda d: model(d)["pack"].update(manifest_sha256="A" * 64)),
            broken(lambda d: model(d).update(graph_data=3)),
            broken(lambda d: model(d).update(graph_data=source("other/tokenizer.json"))),
            broken(lambda d: model(d)["graph"].update(repository="a/b/c")),
            broken(lambda d: model(d)["graph"].update(repository="../b")),
            broken(lambda d: model(d)["graph"].update(revision="main")),
            broken(lambda d: model(d)["graph"].update(path="/etc/passwd")),
            broken(lambda d: model(d)["graph"].update(path="a/../b")),
            broken(lambda d: model(d)["graph"].update(bytes=0)),
            broken(lambda d: model(d)["graph"].update(bytes=True)),
            broken(lambda d: model(d)["graph"].update(sha256="xyz")),
        ]
        for text in cases:
            with self.subTest(text=text[:60]), self.assertRaises(mp.PackError):
                mp.parse_pins(text)


class FetchTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.body = b"hello"
        digest = hashlib.sha256(self.body).hexdigest()
        self.pin = mp.SourcePin("owner/name", REVISION, "onnx/a file.onnx", mp.FilePin(len(self.body), digest))

    def test_a_matching_download_is_kept_and_reused(self) -> None:
        opened: list[str] = []

        def opener(url: str) -> io.BytesIO:
            opened.append(url)
            return io.BytesIO(self.body)

        path = mp.fetch(self.pin, self.dir / "work", opener)
        self.assertEqual(path.read_bytes(), self.body)
        self.assertEqual(path.name, "a file.onnx")
        self.assertEqual(opened, [f"https://huggingface.co/owner/name/resolve/{REVISION}/onnx/a%20file.onnx"])
        self.assertEqual(mp.fetch(self.pin, self.dir / "work", opener), path)
        self.assertEqual(len(opened), 1)

    def test_a_download_that_differs_is_removed(self) -> None:
        for body in (b"hellp", b"hell", b"hello and more"):
            with self.subTest(body=body), self.assertRaises(mp.PackError):
                mp.fetch(self.pin, self.dir, lambda _url, body=body: io.BytesIO(body))
            self.assertEqual(list(self.dir.iterdir()), [])

    def test_a_stale_copy_is_downloaded_again(self) -> None:
        (self.dir / "a file.onnx").write_bytes(b"old")
        self.assertEqual(mp.fetch(self.pin, self.dir, lambda _url: io.BytesIO(self.body)).read_bytes(), self.body)


class LayoutTest(unittest.TestCase):
    def test_a_row_pack_is_magic_count_width_ids_rows(self) -> None:
        data = mp.encode_rows([2, 5, 9], bytes(range(12)), 4)
        self.assertEqual(data[:8], b"BCROWS1\0")
        self.assertEqual(struct.unpack_from("<II", data, 8), (3, 4))
        self.assertEqual(struct.unpack_from("<3I", data, 16), (2, 5, 9))
        self.assertEqual(data[28:], bytes(range(12)))
        self.assertEqual(mp.encode_rows(range(2), b"ab", 1)[16:], struct.pack("<2I", 0, 1) + b"ab")

    def test_row_packs_refuse_bad_input(self) -> None:
        for ids, rows, width in (([1, 1], b"abcd", 2), ([2, 1], b"abcd", 2), ([-1, 0], b"abcd", 2), ([1], b"abc", 2), ([1], b"", 0)):
            with self.subTest(ids=ids), self.assertRaises(mp.PackError):
                mp.encode_rows(ids, rows, width)

    def fixture_pack(self) -> tuple[bytes, bytes]:
        """The model description and row pack the shared fixtures hold."""
        rows = mp.encode_rows(range(6), bytes(range(6 * 6)), 6)
        files = {
            mp.GRAPH_NAME: mp.sha256_bytes(b"graph"),
            mp.TOKENIZER_NAME: mp.sha256_bytes(b"tokenizer"),
            mp.BASE_ROWS_NAME: mp.sha256_bytes(rows),
        }
        tables = [mp.TableInfo("embeddings.weight", "i8", 2), mp.TableInfo("embeddings.scale", "f32", 1)]
        framing = mp.Framing(("\n", "<br>"), ("<bos>",), ("<eos>",))
        return mp.pack_manifest(tiny_spec(), files, tables, framing), rows

    def test_the_builder_writes_the_shared_fixtures(self) -> None:
        manifest, rows = self.fixture_pack()
        expected = {"builder-model.json": manifest, "builder-base.rows": rows}
        if os.environ.get("UPDATE_MODEL_PACK_FIXTURES"):
            FIXTURES.mkdir(parents=True, exist_ok=True)
            for name, data in expected.items():
                (FIXTURES / name).write_bytes(data)
        for name, data in expected.items():
            with self.subTest(name=name):
                self.assertEqual((FIXTURES / name).read_bytes(), data, "run with UPDATE_MODEL_PACK_FIXTURES=1")

    def test_the_model_description_lists_every_file(self) -> None:
        manifest, rows = self.fixture_pack()
        document = json.loads(manifest)
        self.assertEqual(document["format"], 1)
        self.assertEqual(document["model_id"], "tiny")
        self.assertEqual(document["languages"], {})
        self.assertEqual(document["tokenizer"]["query_prefix"], "Query: ")
        self.assertEqual(document["tokenizer"]["literal_pieces"], ["\n", "<br>"])
        self.assertEqual(document["base_rows"], {"file": "base.rows", "bytes": len(rows), "sha256": hashlib.sha256(rows).hexdigest()})
        self.assertEqual([table["width"] for table in document["tables"]], [2, 1])
        with self.assertRaises(mp.PackError):
            mp.file_entry("../escape", mp.sha256_bytes(b""))

    def test_the_oci_manifest_names_each_file_and_has_no_time_stamp(self) -> None:
        files = [("model.json", mp.sha256_bytes(b"m")), ("core.onnx", mp.sha256_bytes(b"graph"))]
        manifest = mp.oci_manifest(tiny_spec(), "https://github.com/example/repo", files)
        self.assertEqual(manifest, mp.oci_manifest(tiny_spec(), "https://github.com/example/repo", files))
        document = json.loads(manifest)
        self.assertEqual(document["artifactType"], "application/vnd.bookclerk.model-pack.v1")
        self.assertEqual(
            document["config"],
            {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                "size": 2,
            },
        )
        self.assertEqual([layer["annotations"]["org.opencontainers.image.title"] for layer in document["layers"]], ["model.json", "core.onnx"])
        self.assertEqual(document["layers"][1]["digest"], "sha256:" + hashlib.sha256(b"graph").hexdigest())
        self.assertEqual(document["annotations"]["org.opencontainers.image.source"], "https://github.com/example/repo")
        self.assertIn(REVISION, document["annotations"]["org.opencontainers.image.description"])
        self.assertNotIn("created", manifest.decode())


class TokenizerPlanTest(unittest.TestCase):
    def test_a_metaspace_tokenizer_keeps_added_tokens_for_the_plugin(self) -> None:
        plan = mp.tokenizer_plan(tokenizer_json(), "added")
        kinds = {piece: kind for piece, kind, _ in plan.pieces}
        self.assertEqual(kinds["<pad>"], mp.PIECE_CONTROL)
        self.assertEqual(kinds["<unk>"], mp.PIECE_UNKNOWN)
        self.assertEqual(kinds["\n"], mp.PIECE_CONTROL)
        self.assertEqual(kinds["<0x41>"], mp.PIECE_BYTE)
        self.assertEqual(kinds["<0x09>"], mp.PIECE_BYTE)
        self.assertNotIn("\t", kinds)
        scores = {piece: score for piece, _, score in plan.pieces}
        self.assertEqual((scores["a"], scores["ab"], scores["▁ab"], scores["abab"]), (0.0, -0.0, -1.0, -2.0))
        self.assertTrue(plan.add_dummy_prefix)
        self.assertFalse(plan.split_by_whitespace)
        self.assertTrue(plan.byte_fallback)
        self.assertEqual((plan.unk_id, plan.bos_id, plan.eos_id, plan.pad_id), (3, 2, 1, 0))
        self.assertEqual(plan.framing, mp.Framing(("\n",), ("<bos>",), ("<eos>",)))

    def test_a_plain_tokenizer_lets_sentencepiece_match_added_tokens(self) -> None:
        split = {"type": "Split", "pattern": {"String": " "}, "behavior": "MergedWithPrevious", "invert": False}
        plan = mp.tokenizer_plan(tokenizer_json(split), "none")
        kinds = {piece: kind for piece, kind, _ in plan.pieces}
        self.assertEqual(kinds["\n"], mp.PIECE_USER_DEFINED)
        self.assertEqual(kinds["<unused0>"], mp.PIECE_USER_DEFINED)
        self.assertEqual(kinds["<bos>"], mp.PIECE_CONTROL)
        self.assertFalse(plan.add_dummy_prefix)
        self.assertFalse(plan.split_by_whitespace)
        self.assertEqual(plan.framing.literal_pieces, ())

    def test_tokenizers_the_conversion_does_not_understand_are_refused(self) -> None:
        def broken(change: Any) -> dict[str, Any]:
            document = tokenizer_json()
            change(document)
            return document

        cases = [
            broken(lambda d: d["model"].update(type="Unigram")),
            broken(lambda d: d["model"].update(vocab={"a": 1})),
            broken(lambda d: d["model"].update(unk_token="<missing>")),
            broken(lambda d: d.update(post_processor={"type": "BertProcessing"})),
            broken(lambda d: d["post_processor"].update(single=[{"SpecialToken": {"id": "<bos>"}}])),
            broken(lambda d: d["post_processor"].update(single=[{"Other": {}}])),
        ]
        for document in cases:
            with self.subTest(document=str(document)[:60]), self.assertRaises(mp.PackError):
                mp.tokenizer_plan(document, "added")


class BuiltTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.out = Path(self.tmp.name)
        self.spec = tiny_spec()
        directory = self.out / "tiny"
        directory.mkdir()
        self.contents = {"model.json": b"{}\n", "core.onnx": b"graph", "base.rows": b"rows"}
        for name, data in self.contents.items():
            (directory / name).write_bytes(data)
        files = [(name, mp.sha256_bytes(data)) for name, data in self.contents.items()]
        self.manifest = mp.oci_manifest(self.spec, "https://github.com/example/repo", files)
        (self.out / "tiny.oci-manifest.json").write_bytes(self.manifest)

    def test_a_built_pack_is_measured_from_disk(self) -> None:
        built = mp.read_built(self.spec, self.out)
        self.assertEqual(built.model_json, mp.sha256_bytes(b"{}\n"))
        self.assertEqual(built.manifest, mp.sha256_bytes(self.manifest))
        self.assertEqual([name for name, _ in built.files], list(self.contents))

    def test_a_changed_or_missing_file_is_refused(self) -> None:
        (self.out / "tiny" / "core.onnx").write_bytes(b"other")
        with self.assertRaises(mp.PackError):
            mp.read_built(self.spec, self.out)
        (self.out / "tiny" / "core.onnx").unlink()
        with self.assertRaises(mp.PackError):
            mp.read_built(self.spec, self.out)
        (self.out / "tiny.oci-manifest.json").unlink()
        with self.assertRaises(mp.PackError):
            mp.read_built(self.spec, self.out)

    def test_a_manifest_that_does_not_start_with_the_description_is_refused(self) -> None:
        files = [("core.onnx", mp.sha256_bytes(b"graph"))]
        (self.out / "tiny.oci-manifest.json").write_bytes(mp.oci_manifest(self.spec, "https://github.com/example/repo", files))
        with self.assertRaises(mp.PackError):
            mp.read_built(self.spec, self.out)

    def test_the_pin_report_names_each_digest_that_differs(self) -> None:
        built = mp.read_built(self.spec, self.out)
        self.assertEqual(len(mp.pin_report(self.spec, built)), 2)
        text = mp.rewrite_pack_pins(json.dumps(pins_document()), "tiny", built.model_json.sha256, built.manifest.sha256)
        self.assertEqual(mp.pin_report(mp.parse_pins(text).models["tiny"], built), [])


class CheckHelpersTest(unittest.TestCase):
    def test_the_check_texts_cover_the_offered_languages(self) -> None:
        texts = mp.load_check_texts()
        self.assertGreaterEqual(len(texts), 50)
        self.assertTrue(any(item["query"] for item in texts))
        joined = "\n".join(item["text"] for item in texts)
        for sample in ("lighthouse", "Buchhandlung", "librairie", "novela negra", "commissario", "livraria", "喫茶店"):
            self.assertIn(sample, joined)

    def test_malformed_check_texts_are_refused(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "texts.json"
            for content in ("[]", "{}", '[{"text": "a"}]', '[{"text": 1, "query": false}]', '["a"]'):
                path.write_text(content, encoding="utf-8")
                with self.subTest(content=content), self.assertRaises(mp.PackError):
                    mp.load_check_texts(path)

    def test_token_ids_are_compared_text_by_text(self) -> None:
        self.assertEqual(mp.compare_ids([[1, 2], [3]], [[1, 2], [3]]), [])
        self.assertEqual(mp.compare_ids([[1, 2], [3], [4]], [[1, 2], [3, 9], []]), [1, 2])
        with self.assertRaises(mp.PackError):
            mp.compare_ids([[1]], [])

    def test_the_largest_vector_difference_is_found(self) -> None:
        self.assertEqual(mp.largest_difference([[0.0, 1.0]], [[0.0, 1.0]]), 0.0)
        self.assertAlmostEqual(mp.largest_difference([[0.0, 1.0], [0.5, 0.5]], [[0.0, 1.0], [0.5, 0.25]]), 0.25)
        for found in ([], [[0.0]]):
            with self.subTest(found=found), self.assertRaises(mp.PackError):
                mp.largest_difference([[0.0, 1.0]], found)


class FakeRegistry:
    """An in-memory registry speaking the calls the client makes."""

    def __init__(self) -> None:
        self.blobs: dict[str, bytes] = {}
        self.manifests: dict[str, bytes] = {}
        self.log: list[tuple[str, str]] = []
        self.token_status = 200
        self.refuse: set[str] = set()
        self.stored_digest: str | None = None

    def __call__(self, request: urllib.request.Request) -> tuple[int, dict[str, str], bytes]:
        method, url = request.get_method(), request.full_url
        self.log.append((method, url))
        if "/token?" in url:
            assert request.get_header("Authorization", "").startswith("Basic ")
            return self.token_status, {}, json.dumps({"token": "t0ken"}).encode()
        assert request.get_header("Authorization") == "Bearer t0ken"
        if method in self.refuse:
            return 403, {}, b""
        data = request.data.read() if hasattr(request.data, "read") else request.data
        if method == "HEAD":
            return (200 if url.rsplit("/", 1)[-1] in self.blobs else 404), {}, b""
        if method == "POST":
            return 202, {"location": "/v2/owner/models/tiny/blobs/uploads/1?state=x"}, b""
        if "/blobs/uploads/" in url:
            digest = url.rsplit("digest=", 1)[-1]
            assert digest == "sha256:" + hashlib.sha256(data).hexdigest()
            assert request.get_header("Content-length") == str(len(data))
            self.blobs[digest] = data
            return 201, {}, b""
        self.manifests[url.rsplit("/", 1)[-1]] = data
        digest = self.stored_digest or "sha256:" + hashlib.sha256(data).hexdigest()
        return 201, {"docker-content-digest": digest}, b""


class RegistryTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.out = Path(self.tmp.name)
        self.spec = tiny_spec()
        (self.out / "tiny").mkdir()
        self.contents = {"model.json": b"{}\n", "core.onnx": b"graph"}
        for name, data in self.contents.items():
            (self.out / "tiny" / name).write_bytes(data)
        files = [(name, mp.sha256_bytes(data)) for name, data in self.contents.items()]
        self.manifest = mp.oci_manifest(self.spec, "https://github.com/example/repo", files)
        (self.out / "tiny.oci-manifest.json").write_bytes(self.manifest)
        self.built = mp.read_built(self.spec, self.out)
        self.fake = FakeRegistry()
        self.registry = mp.Registry("registry.example/owner/models/tiny", "user", "secret", self.fake)

    def test_a_publish_sends_every_blob_then_the_manifest(self) -> None:
        result = mp.publish(self.spec, self.built, self.registry)
        digest = "sha256:" + hashlib.sha256(self.manifest).hexdigest()
        self.assertEqual(result, {"name": "registry.example/owner/models/tiny", "digest": digest, "tag": "sha256-" + digest[7:19], "uploaded": 3})
        self.assertEqual(sorted(self.fake.blobs.values()), sorted([b"{}", b"{}\n", b"graph"]))
        self.assertEqual(self.fake.manifests, {result["tag"]: self.manifest})
        self.assertEqual(sum(1 for _, url in self.fake.log if "/token?" in url), 1)
        self.assertIn("scope=repository:owner/models/tiny:pull,push", self.fake.log[0][1])
        self.assertFalse(any("secret" in url for _, url in self.fake.log))

    def test_publishing_again_sends_no_blob(self) -> None:
        mp.publish(self.spec, self.built, self.registry)
        again = mp.publish(self.spec, self.built, self.registry)
        self.assertEqual(again["uploaded"], 0)

    def test_refusals_are_reported(self) -> None:
        self.fake.token_status = 401
        with self.assertRaises(mp.PackError):
            self.registry.has_blob(mp.sha256_bytes(b"x"))
        self.fake.token_status = 200
        for method in ("POST", "PUT"):
            self.fake.refuse = {method}
            with self.subTest(method=method), self.assertRaises(mp.PackError):
                self.registry.push_blob(mp.sha256_bytes(b"x"), b"x")
        with self.assertRaises(mp.PackError):
            self.registry.push_manifest(self.manifest, "tag")
        self.fake.refuse = set()
        self.fake.stored_digest = "sha256:" + HEX_A
        with self.assertRaises(mp.PackError):
            self.registry.push_manifest(self.manifest, "tag")

    def test_an_upload_address_that_is_not_https_is_refused(self) -> None:
        def transport(request: urllib.request.Request) -> tuple[int, dict[str, str], bytes]:
            if request.get_method() == "POST":
                return 202, {"location": "http://registry.example/upload"}, b""
            return self.fake(request)

        registry = mp.Registry("registry.example/owner/models/tiny", "user", "secret", transport)
        with self.assertRaises(mp.PackError):
            registry.push_blob(mp.sha256_bytes(b"x"), b"x")

    def test_a_reference_must_be_host_and_lowercase_path(self) -> None:
        for reference in ("registry.example", "https://registry.example/a", "registry.example/Owner/x", "/x"):
            with self.subTest(reference=reference), self.assertRaises(mp.PackError):
                mp.Registry(reference, "user", "secret")


#: Fragments mutations splice in: JSON, address, header and shell syntax.
FRAGMENTS = [
    '"', "{", "}", "[", "]", ":", ",", "\\n", "\n", "\r\n", " ", "/", "//", "?", "#", "&", "=", "@", "%", "\\", "$", "`",
    "https://", "http://", "ghcr.io", "evil.example", '"token":', "null", "true", "1", "a", "Z", "é", "\u0000", "\x00",
]
TOKEN_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._~+/=-"
PATH_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._~!$&'()*+,;=:@%/?-"
HOST = "ghcr.io"
DIGEST = "d" * 64


def expected_token(raw: bytes) -> str | None:
    """Independent oracle: the token a reply holds, or ``None``."""
    if len(raw) > mp.MAX_REPLY_BYTES:
        return None
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError):
        return None
    token = value.get("token") if type(value) is dict else None
    if type(token) is not str or not 1 <= len(token) <= 8192:
        return None
    return token if all(c in TOKEN_CHARS for c in token) else None


def expected_target(location: str) -> str | None:
    """Independent oracle: the upload address for a location, or ``None``."""
    prefix = "https://" + HOST
    path = location
    if location[: len(prefix) + 1] == prefix + "/":
        path = location[len(prefix) :]
    if not 1 <= len(path) <= 4096 or path[0] != "/" or path[:2] == "//":
        return None
    if not all(c in PATH_CHARS for c in path):
        return None
    return prefix + path + ("&" if "?" in path else "?") + "digest=sha256:" + DIGEST


def run_token(raw: bytes) -> str | None:
    """Reads a token, mapping a refusal to ``None``."""
    try:
        return mp.parse_token_reply(raw)
    except mp.PackError:
        return None


def run_target(location: str) -> str | None:
    """Builds an upload address, mapping a refusal to ``None``."""
    try:
        return mp.upload_target(HOST, location, DIGEST)
    except mp.PackError:
        return None


def assert_safe(case: unittest.TestCase, kind: str, raw: bytes, label: str) -> None:
    """Checks one reader against its oracle and the output invariants."""
    if kind == "token":
        got = run_token(raw)
        case.assertEqual(got, expected_token(raw), label)
        if got is not None:
            case.assertRegex(got, r"\A[\x21-\x7e]+\Z", label)
    else:
        # A header value reaches the client as text; bytes that are not
        # UTF-8 arrive as Latin-1.
        try:
            location = raw.decode("utf-8")
        except UnicodeDecodeError:
            location = raw.decode("latin-1")
        got = run_target(location)
        case.assertEqual(got, expected_target(location), label)
        if got is not None:
            case.assertTrue(got.startswith(f"https://{HOST}/"), label)
            case.assertFalse(got.startswith(f"https://{HOST}//"), label)
            case.assertRegex(got, r"\A[\x21-\x7e]+\Z", label)
            case.assertTrue(got.endswith("digest=sha256:" + DIGEST), label)


def corpus(kind: str) -> list[Path]:
    """The seed files for one reader."""
    return sorted(p for p in (CORPUS / kind).iterdir() if p.is_file())


class RegistryReplyCorpusTest(unittest.TestCase):
    """Replays the seed corpus of registry replies."""

    def test_corpus_matches_oracle(self) -> None:
        for kind in ("token", "location"):
            paths = corpus(kind)
            self.assertGreater(len(paths), 10)
            for path in paths:
                assert_safe(self, kind, path.read_bytes(), f"{kind}/{path.name}")

    def test_expected_outcomes(self) -> None:
        accepted = {"token": {"typical", "with_other_fields", "duplicate_key"}, "location": {"relative", "relative_no_query", "absolute_same_host", "percent"}}
        for kind, names in accepted.items():
            for path in corpus(kind):
                raw = path.read_bytes()
                got = run_token(raw) if kind == "token" else run_target(raw.decode("utf-8", "replace"))
                self.assertEqual(got is not None, path.name in names, f"{kind}/{path.name}")
        self.assertEqual(run_token((CORPUS / "token" / "duplicate_key").read_bytes()), "second")
        self.assertEqual(
            run_target((CORPUS / "location" / "relative").read_bytes().decode()),
            f"https://{HOST}/v2/bookclerk/models/bekko-a8m/blobs/uploads/0f3c?state=abc.DEF-123_~&digest=sha256:{DIGEST}",
        )
        self.assertEqual(run_target("/v2/upload"), f"https://{HOST}/v2/upload?digest=sha256:{DIGEST}")

    def test_oversized_and_deep_replies_are_refused(self) -> None:
        self.assertIsNone(run_token((CORPUS / "token" / "typical").read_bytes() + b" " * mp.MAX_REPLY_BYTES))
        self.assertIsNone(run_token(b'{"token":"' + b"a" * 8193 + b'"}'))
        self.assertIsNone(run_token(b"[" * 30_000 + b"]" * 30_000))
        self.assertIsNone(run_target("/" + "a" * 4096))


class RegistryReplyFuzzTest(unittest.TestCase):
    """Seeded mutation fuzzing over the corpus, replayable from the seed."""

    ITERATIONS = 4000

    def mutate(self, rng: random.Random, text: str) -> bytes:
        for _ in range(rng.randint(1, 4)):
            pos = rng.randint(0, len(text))
            op = rng.random()
            if op < 0.5:
                text = text[:pos] + rng.choice(FRAGMENTS) + text[pos:]
            elif op < 0.75:
                text = text[:pos] + text[pos + rng.randint(1, 3) :]
            else:
                text = text[:pos] + chr(rng.randint(0, 0x2FF)) + text[pos:]
        raw = text.encode("utf-8", "surrogatepass")
        if rng.random() < 0.05 and raw:
            cut = rng.randint(0, len(raw) - 1)
            raw = raw[:cut] + bytes([rng.randint(0x80, 0xFF)]) + raw[cut + 1 :]
        return raw

    def test_mutations_match_oracle(self) -> None:
        for kind, seed in (("token", 20261122), ("location", 1294)):
            seeds = [p.read_bytes().decode("utf-8", "replace") for p in corpus(kind)]
            rng = random.Random(seed)
            accepted = 0
            for n in range(self.ITERATIONS):
                raw = self.mutate(rng, rng.choice(seeds))
                assert_safe(self, kind, raw, f"{kind} iteration {n}: {raw!r}")
                accepted += (run_token(raw) if kind == "token" else run_target(raw.decode("utf-8", "replace"))) is not None
            # The mutations reach both outcomes.
            self.assertGreater(accepted, 50, kind)
            self.assertLess(accepted, self.ITERATIONS - 50, kind)

    def test_generated_values_match_oracle(self) -> None:
        rng = random.Random(8192)
        alphabet = TOKEN_CHARS + " \n\"é#\\"
        for n in range(self.ITERATIONS):
            body = "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 40)))
            raw = json.dumps({rng.choice(["token", "Token", "access_token"]): rng.choice([body, body, None, 7, [body]])}).encode()
            assert_safe(self, "token", raw, f"generated token {n}: {raw!r}")
            prefix = rng.choice(["/", "/", "//", "", f"https://{HOST}/", f"https://{HOST}", "https://evil.example/", f"http://{HOST}/"])
            assert_safe(self, "location", (prefix + body).encode(), f"generated location {n}: {prefix + body!r}")


class CommandLineTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.models = self.dir / "models.json"
        self.models.write_text(json.dumps(pins_document(), indent=2) + "\n", encoding="utf-8")
        self.saved = mp.MODELS_FILE
        mp.MODELS_FILE = self.models
        self.addCleanup(setattr, mp, "MODELS_FILE", self.saved)
        self.env = dict(os.environ)
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(self.env)))
        os.environ.pop("REGISTRY_PASSWORD", None)
        (self.dir / "out" / "tiny").mkdir(parents=True)
        (self.dir / "out" / "tiny" / "model.json").write_bytes(b"{}\n")
        files = [("model.json", mp.sha256_bytes(b"{}\n"))]
        (self.dir / "out" / "tiny.oci-manifest.json").write_bytes(mp.oci_manifest(tiny_spec(), "https://github.com/example/repo", files))

    def run_main(self, *args: str) -> tuple[int, str]:
        out, err = io.StringIO(), io.StringIO()
        saved = sys.stdout, sys.stderr
        sys.stdout, sys.stderr = out, err
        try:
            status = mp.main([*args, "--out", str(self.dir / "out"), "--work", str(self.dir / "work")])
        finally:
            sys.stdout, sys.stderr = saved
        return status, out.getvalue() + err.getvalue()

    def test_list_prints_the_model_ids(self) -> None:
        self.assertEqual(self.run_main("list"), (0, '["tiny"]\n'))

    def test_an_unknown_model_and_missing_options_fail(self) -> None:
        for args in (("pin", "--model", "other"), ("build", "--model", "tiny"), ("check", "--model", "tiny"), ("fetch",)):
            with self.subTest(args=args):
                status, output = self.run_main(*args)
                self.assertEqual(status, 1)
                self.assertIn("error:", output)

    def test_pin_fails_on_a_difference_and_write_records_it(self) -> None:
        status, output = self.run_main("pin", "--model", "tiny")
        self.assertEqual(status, 1)
        self.assertIn("not the pinned one", output)
        self.assertEqual(self.run_main("pin", "--model", "tiny", "--write")[0], 0)
        status, output = self.run_main("pin", "--model", "tiny")
        self.assertEqual((status, output), (0, "tiny matches its pins\n"))

    def test_publish_refuses_an_unpinned_build_and_needs_credentials(self) -> None:
        status, output = self.run_main("publish", "--model", "tiny")
        self.assertEqual(status, 1)
        self.assertIn("refusing to publish", output)
        self.run_main("pin", "--model", "tiny", "--write")
        status, output = self.run_main("publish", "--model", "tiny")
        self.assertEqual(status, 1)
        self.assertIn("REGISTRY_USERNAME", output)

    def test_a_build_without_its_upstream_files_fails(self) -> None:
        status, output = self.run_main("build", "--model", "tiny", "--spm-to-fb", "/nonexistent")
        self.assertEqual(status, 1)
        self.assertIn("run `fetch` first", output)


class ConverterPinsTest(unittest.TestCase):
    def test_the_converter_build_pins_the_vendored_commits(self) -> None:
        vendor = (ROOT / "scripts" / "vendor-sentencepiece-lite.sh").read_text(encoding="utf-8")
        build = (ROOT / "scripts" / "model-packs" / "build-spm-to-fb.sh").read_text(encoding="utf-8")
        flatbuffers = vendor.split('FLATBUFFERS_COMMIT="', 1)[1].split('"', 1)[0]
        self.assertIn(f'[flatbuffers-src]="{flatbuffers}"', build)
        self.assertIn("pin SENTENCEPIECE_COMMIT", build)


if __name__ == "__main__":
    unittest.main()
