#!/usr/bin/env python3
"""Build, check and publish the embedding model packs.

A model pack is the directory the first-party `embedder` plugin reads
(`docs/internals/model-packs.md`): `model.json`, the cut ONNX graph, the
SentencePiece Lite tokenizer and the token rows. This script makes one from a
published model, proves it gives the published model's token ids and vectors,
and publishes it to a container registry as an OCI artifact.

Everything it reads is pinned in `scripts/model-packs/models.json`: the
upstream repository, revision, and the size and SHA-256 of each file it
downloads, and the SHA-256 of the `model.json` and of the OCI manifest the
build must produce. A build that produces anything else fails, so the packs a
node downloads are the ones a reviewer could rebuild.

Commands (each takes `--model <id>`; see `--help`):

    fetch    download the pinned upstream files into the work directory
    build    write the pack and its OCI manifest
    check    compare token ids and vectors with the published model
    pin      compare the built digests with models.json (or rewrite them)
    publish  push the pack's files and manifest to a registry

`fetch`, `pin` and `publish` use only the standard library. `build` and
`check` need the packages in `scripts/model-packs/requirements.txt`, the
upstream `spm_to_fb` tool (`scripts/model-packs/build-spm-to-fb.sh`) and, for
`check`, the plugin's `embed` and `tokens` examples and the pinned ONNX
Runtime library.
"""

from __future__ import annotations

import argparse
import base64
import dataclasses
import hashlib
import json
import math
import os
import re
import struct
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, BinaryIO, Callable, Sequence

#: The pins: upstream sources and the digests a build must produce.
MODELS_FILE = Path(__file__).resolve().parent / "model-packs" / "models.json"
#: Texts the checks embed, in every language the packs are offered for.
CHECK_TEXTS_FILE = Path(__file__).resolve().parent / "model-packs" / "check-texts.json"

#: Layout version written to `model.json` (`PACK_FORMAT` in bookclerk-model-pack).
PACK_FORMAT = 1
#: First bytes of a row pack (`ROWS_MAGIC` in bookclerk-model-pack).
ROWS_MAGIC = b"BCROWS1\0"
#: Names of the files in a model directory.
MANIFEST_NAME = "model.json"
GRAPH_NAME = "core.onnx"
TOKENIZER_NAME = "tokenizer.spm.fb"
BASE_ROWS_NAME = "base.rows"
#: Name of the OCI manifest `build` writes beside the pack's files.
OCI_MANIFEST_NAME = "oci-manifest.json"

#: OCI media types of a model pack artifact.
OCI_MANIFEST_TYPE = "application/vnd.oci.image.manifest.v1+json"
OCI_EMPTY_TYPE = "application/vnd.oci.empty.v1+json"
OCI_ARTIFACT_TYPE = "application/vnd.bookclerk.model-pack.v1"
OCI_FILE_TYPE = "application/vnd.bookclerk.model-pack.file.v1"
#: The empty JSON object every artifact manifest names as its config.
OCI_EMPTY_BLOB = b"{}"

#: Largest difference allowed between a plugin vector and the published
#: model's, in any number. Both run the same graph on the same rows, so the
#: difference is float rounding in pooling (measured: under 4e-7).
VECTOR_TOLERANCE = 1e-5
#: Bytes read at a time when hashing or copying a file.
CHUNK = 1 << 20


class PackError(Exception):
    """A pin, a download or a check did not hold; the message says which."""


# ---------------------------------------------------------------------------
# Pins
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class FilePin:
    """Size and digest of one file.

    Attributes:
        bytes: The file's length in bytes.
        sha256: Its SHA-256, 64 lowercase hexadecimal characters.
    """

    bytes: int
    sha256: str


@dataclasses.dataclass(frozen=True)
class SourcePin:
    """Where one upstream file comes from.

    Attributes:
        repository: Hugging Face repository, `owner/name`.
        revision: Full commit of that repository, 40 hexadecimal characters.
        path: The file's path in the repository.
        pin: The file's size and digest.
    """

    repository: str
    revision: str
    path: str
    pin: FilePin

    @property
    def url(self) -> str:
        """The download address of this exact revision of the file."""
        return f"https://huggingface.co/{self.repository}/resolve/{self.revision}/{urllib.parse.quote(self.path)}"

    @property
    def local_name(self) -> str:
        """File name in the work directory: the upstream base name."""
        return self.path.rsplit("/", 1)[-1]


@dataclasses.dataclass(frozen=True)
class ModelSpec:
    """One model the packs are built for, as `models.json` describes it.

    Attributes:
        model_id: Directory and artifact name; the plugin serves it as
            `@embedder/<model_id>`.
        license: SPDX expression of the published model's license.
        graph: The published ONNX graph.
        graph_data: The graph's external tensor data, when it has any.
        tokenizer: The published `tokenizer.json`.
        dimensions: Numbers in one vector.
        max_tokens: Longest input in tokens, framing included.
        pooling: `mean` when the graph gives one vector per token, `none`
            when it gives the text's vector.
        output: Name of the graph output to read.
        query_prefix: Text put before a search query.
        document_prefix: Text put before a document.
        literal_pieces: `added` when the tokenizer's added tokens must be
            matched before tokenizing (a Metaspace tokenizer), else `none`.
        model_json_sha256: Digest the built `model.json` must have.
        manifest_sha256: Digest the built OCI manifest must have.
    """

    model_id: str
    license: str
    graph: SourcePin
    graph_data: SourcePin | None
    tokenizer: SourcePin
    dimensions: int
    max_tokens: int
    pooling: str
    output: str
    query_prefix: str
    document_prefix: str
    literal_pieces: str
    model_json_sha256: str
    manifest_sha256: str

    def sources(self) -> list[SourcePin]:
        """Every upstream file the build reads."""
        return [pin for pin in (self.graph, self.graph_data, self.tokenizer) if pin is not None]


@dataclasses.dataclass(frozen=True)
class Pins:
    """The whole of `models.json`.

    Attributes:
        source: Repository the artifacts are built from; written into each
            OCI manifest so the registry links the package to it.
        registry: Registry path the packs are published under; a model's
            artifact is `<registry>/<model_id>`.
        models: The models, by id.
    """

    source: str
    registry: str
    models: dict[str, ModelSpec]


def _is_hex(value: object, length: int) -> bool:
    """Whether `value` is `length` lowercase hexadecimal characters."""
    return isinstance(value, str) and len(value) == length and all(c in "0123456789abcdef" for c in value)


def _is_file_name(value: str) -> bool:
    """Whether `value` is a name the pack layout allows (`PackFileName`)."""
    allowed = "abcdefghijklmnopqrstuvwxyz0123456789._-"
    return 1 <= len(value) <= 64 and all(c in allowed for c in value) and value[0] not in "._-"


def _source(raw: dict[str, Any], where: str) -> SourcePin:
    """Reads one upstream file's pin.

    Args:
        raw: The JSON object.
        where: Path of the object in the file, for error messages.

    Raises:
        PackError: A field is missing or malformed.
    """
    repository, revision, path = raw.get("repository"), raw.get("revision"), raw.get("path")
    size, digest = raw.get("bytes"), raw.get("sha256")
    if not isinstance(repository, str) or repository.count("/") != 1 or ".." in repository:
        raise PackError(f"{where}.repository must be `owner/name`")
    if not _is_hex(revision, 40):
        raise PackError(f"{where}.revision must be a full commit id")
    if not isinstance(path, str) or not path or path.startswith("/") or ".." in path.split("/"):
        raise PackError(f"{where}.path must be a relative path")
    if not isinstance(size, int) or isinstance(size, bool) or size <= 0:
        raise PackError(f"{where}.bytes must be a positive integer")
    if not _is_hex(digest, 64):
        raise PackError(f"{where}.sha256 must be 64 lowercase hexadecimal characters")
    assert isinstance(revision, str) and isinstance(digest, str)
    return SourcePin(repository, revision, path, FilePin(size, digest))


def parse_pins(text: str) -> Pins:
    """Parses and validates `models.json`.

    Args:
        text: The file's contents.

    Returns:
        The pins.

    Raises:
        PackError: The document is not the shape this script reads.
    """
    try:
        raw = json.loads(text)
    except json.JSONDecodeError as error:
        raise PackError(f"models.json is not JSON: {error}") from error
    if not isinstance(raw, dict) or not isinstance(raw.get("models"), dict):
        raise PackError("models.json must be an object with `models`")
    source, registry = raw.get("source"), raw.get("registry")
    if not isinstance(source, str) or not source.startswith("https://"):
        raise PackError("`source` must be an https address")
    if not isinstance(registry, str) or "/" not in registry or "://" in registry:
        raise PackError("`registry` must be `host/path`")
    models: dict[str, ModelSpec] = {}
    for model_id, entry in raw["models"].items():
        where = f"models.{model_id}"
        if not _is_file_name(model_id):
            raise PackError(f"{where}: a model id is lowercase letters, digits, `.`, `_` and `-`")
        if not isinstance(entry, dict):
            raise PackError(f"{where} must be an object")
        for key in ("graph", "tokenizer"):
            if not isinstance(entry.get(key), dict):
                raise PackError(f"{where}.{key} must be an object")
        for key in ("license", "output", "query_prefix", "document_prefix"):
            if not isinstance(entry.get(key), str):
                raise PackError(f"{where}.{key} must be a string")
        for key in ("dimensions", "max_tokens"):
            if not isinstance(entry.get(key), int) or isinstance(entry.get(key), bool) or entry[key] <= 0:
                raise PackError(f"{where}.{key} must be a positive integer")
        if entry.get("pooling") not in ("mean", "none"):
            raise PackError(f"{where}.pooling must be `mean` or `none`")
        if entry.get("literal_pieces") not in ("added", "none"):
            raise PackError(f"{where}.literal_pieces must be `added` or `none`")
        pack = entry.get("pack")
        if not isinstance(pack, dict):
            raise PackError(f"{where}.pack must be an object")
        for key in ("model_json_sha256", "manifest_sha256"):
            if not _is_hex(pack.get(key), 64):
                raise PackError(f"{where}.pack.{key} must be 64 lowercase hexadecimal characters")
        data = entry.get("graph_data")
        if data is not None and not isinstance(data, dict):
            raise PackError(f"{where}.graph_data must be an object or null")
        models[model_id] = ModelSpec(
            model_id=model_id,
            license=entry["license"],
            graph=_source(entry["graph"], f"{where}.graph"),
            graph_data=None if data is None else _source(data, f"{where}.graph_data"),
            tokenizer=_source(entry["tokenizer"], f"{where}.tokenizer"),
            dimensions=entry["dimensions"],
            max_tokens=entry["max_tokens"],
            pooling=entry["pooling"],
            output=entry["output"],
            query_prefix=entry["query_prefix"],
            document_prefix=entry["document_prefix"],
            literal_pieces=entry["literal_pieces"],
            model_json_sha256=pack["model_json_sha256"],
            manifest_sha256=pack["manifest_sha256"],
        )
        names = [pin.local_name for pin in models[model_id].sources()]
        if len(set(names)) != len(names):
            raise PackError(f"{where}: two upstream files share a base name")
    if not models:
        raise PackError("models.json lists no model")
    return Pins(source=source, registry=registry, models=models)


def rewrite_pack_pins(text: str, model_id: str, model_json_sha256: str, manifest_sha256: str) -> str:
    """Returns `models.json` with one model's built digests replaced.

    Args:
        text: The file's current contents.
        model_id: The model whose `pack` object changes.
        model_json_sha256: The new `model.json` digest.
        manifest_sha256: The new OCI manifest digest.

    Returns:
        The new contents, formatted as Prettier formats JSON.

    Raises:
        PackError: The file does not list the model.
    """
    raw = json.loads(text)
    if model_id not in raw.get("models", {}):
        raise PackError(f"models.json does not list `{model_id}`")
    raw["models"][model_id]["pack"] = {
        "model_json_sha256": model_json_sha256,
        "manifest_sha256": manifest_sha256,
    }
    return json.dumps(raw, indent=2, ensure_ascii=False) + "\n"


# ---------------------------------------------------------------------------
# Files and digests
# ---------------------------------------------------------------------------


def sha256_file(path: Path) -> FilePin:
    """Measures a file.

    Args:
        path: The file to read.

    Returns:
        Its length and SHA-256.
    """
    digest, size = hashlib.sha256(), 0
    with path.open("rb") as handle:
        while chunk := handle.read(CHUNK):
            digest.update(chunk)
            size += len(chunk)
    return FilePin(size, digest.hexdigest())


def sha256_bytes(data: bytes) -> FilePin:
    """Measures bytes in memory."""
    return FilePin(len(data), hashlib.sha256(data).hexdigest())


def fetch(pin: SourcePin, directory: Path, opener: Callable[[str], BinaryIO] | None = None) -> Path:
    """Downloads one pinned upstream file, or keeps the copy already there.

    The file is written under a temporary name and renamed only once its
    size and digest match the pin, so a partial or altered download is never
    left where the build would read it. Each call writes under a name of
    its own.

    Args:
        pin: The file and what it must hash to.
        directory: The work directory.
        opener: Opens an address for reading; `urllib` when omitted.

    Returns:
        The file's path in the work directory.

    Raises:
        PackError: The download does not match the pin.
    """
    target = directory / pin.local_name
    if target.is_file() and sha256_file(target) == pin.pin:
        return target
    directory.mkdir(parents=True, exist_ok=True)
    open_url = opener or (lambda url: urllib.request.urlopen(url, timeout=120))  # noqa: S310 - https only, pinned digest
    digest, size = hashlib.sha256(), 0
    # A name of this call's own, so two fetches of one file cannot write
    # through each other.
    descriptor, name = tempfile.mkstemp(prefix=target.name + ".", suffix=".part", dir=directory)
    partial = Path(name)
    try:
        with open_url(pin.url) as response, os.fdopen(descriptor, "wb") as handle:
            while chunk := response.read(CHUNK):
                size += len(chunk)
                if size > pin.pin.bytes:
                    break
                digest.update(chunk)
                handle.write(chunk)
        if FilePin(size, digest.hexdigest()) != pin.pin:
            raise PackError(f"{pin.url} does not match its pin")
        partial.replace(target)
    finally:
        partial.unlink(missing_ok=True)
    return target


# ---------------------------------------------------------------------------
# Pack layout (the writer's side of bookclerk-model-pack)
# ---------------------------------------------------------------------------


def encode_rows(ids: Sequence[int], rows: bytes, row_bytes: int) -> bytes:
    """Encodes one row pack.

    Args:
        ids: Token ids, strictly ascending.
        rows: The rows in the order of `ids`, `row_bytes` each.
        row_bytes: Length of one row.

    Returns:
        The file: magic, count, row length, ids, rows (little-endian).

    Raises:
        PackError: The ids are not ascending or the lengths disagree.
    """
    if any(b <= a for a, b in zip(ids, ids[1:])) or (ids and ids[0] < 0):
        raise PackError("row pack ids must be strictly ascending")
    if row_bytes <= 0 or len(rows) != row_bytes * len(ids):
        raise PackError("row pack rows do not match the id count")
    header = ROWS_MAGIC + struct.pack("<II", len(ids), row_bytes)
    return header + struct.pack(f"<{len(ids)}I", *ids) + rows


def file_entry(name: str, pin: FilePin) -> dict[str, Any]:
    """One file of the pack as `model.json` lists it (`PackFile`)."""
    if not _is_file_name(name):
        raise PackError(f"`{name}` is not a pack file name")
    return {"file": name, "bytes": pin.bytes, "sha256": pin.sha256}


@dataclasses.dataclass(frozen=True)
class TableInfo:
    """One vocabulary table the cut graph takes as an input.

    Attributes:
        name: The graph input's name.
        element: `i8`, `u8` or `f32`.
        width: Elements in one token's row of this table.
    """

    name: str
    element: str
    width: int


@dataclasses.dataclass(frozen=True)
class Framing:
    """What the tokenizer puts around and matches inside a text.

    Attributes:
        literal_pieces: Pieces matched before tokenizing.
        prefix_pieces: Pieces put before every text (`<bos>`).
        suffix_pieces: Pieces put after every text (`<eos>`).
    """

    literal_pieces: tuple[str, ...]
    prefix_pieces: tuple[str, ...]
    suffix_pieces: tuple[str, ...]


def pack_manifest(
    spec: ModelSpec,
    files: dict[str, FilePin],
    tables: Sequence[TableInfo],
    framing: Framing,
) -> bytes:
    """Writes `model.json` (`PackManifest` in bookclerk-model-pack).

    Args:
        spec: The model.
        files: Size and digest of the graph, tokenizer and base rows, by
            file name.
        tables: The tables, in the order a row concatenates them.
        framing: The tokenizer's framing.

    Returns:
        The document's bytes; the same inputs always give the same bytes.
    """
    document = {
        "format": PACK_FORMAT,
        "model_id": spec.model_id,
        "dimensions": spec.dimensions,
        "max_tokens": spec.max_tokens,
        "graph": {
            "model": file_entry(GRAPH_NAME, files[GRAPH_NAME]),
            "ids_input": "input_ids",
            "mask_input": "attention_mask",
            "output": spec.output,
            "pooling": spec.pooling,
        },
        "tokenizer": {
            "model": file_entry(TOKENIZER_NAME, files[TOKENIZER_NAME]),
            "literal_pieces": list(framing.literal_pieces),
            "prefix_pieces": list(framing.prefix_pieces),
            "suffix_pieces": list(framing.suffix_pieces),
            "document_prefix": spec.document_prefix,
            "query_prefix": spec.query_prefix,
        },
        "tables": [{"input": t.name, "element": t.element, "width": t.width} for t in tables],
        "base_rows": file_entry(BASE_ROWS_NAME, files[BASE_ROWS_NAME]),
        "languages": {},
    }
    return (json.dumps(document, indent=2, ensure_ascii=False) + "\n").encode()


def oci_descriptor(media_type: str, pin: FilePin, title: str | None = None) -> dict[str, Any]:
    """One blob as an OCI manifest names it."""
    descriptor: dict[str, Any] = {"mediaType": media_type, "digest": f"sha256:{pin.sha256}", "size": pin.bytes}
    if title is not None:
        descriptor["annotations"] = {"org.opencontainers.image.title": title}
    return descriptor


def oci_manifest(spec: ModelSpec, source: str, files: Sequence[tuple[str, FilePin]]) -> bytes:
    """Writes the OCI manifest of a pack.

    The artifact has one layer per file, named by an
    `org.opencontainers.image.title` annotation, `model.json` first. It
    carries no time stamp, so the same pack always has the same digest.

    Args:
        spec: The model.
        source: Repository the pack is built from.
        files: Name, size and digest of each file, in layer order.

    Returns:
        The manifest's bytes.
    """
    upstream = f"https://huggingface.co/{spec.graph.repository}/tree/{spec.graph.revision}"
    document = {
        "schemaVersion": 2,
        "mediaType": OCI_MANIFEST_TYPE,
        "artifactType": OCI_ARTIFACT_TYPE,
        "config": oci_descriptor(OCI_EMPTY_TYPE, sha256_bytes(OCI_EMPTY_BLOB)),
        "layers": [oci_descriptor(OCI_FILE_TYPE, pin, name) for name, pin in files],
        "annotations": {
            "org.opencontainers.image.title": spec.model_id,
            "org.opencontainers.image.source": source,
            "org.opencontainers.image.licenses": spec.license,
            "org.opencontainers.image.description": f"Bookclerk model pack built from {upstream}",
        },
    }
    return json.dumps(document, separators=(",", ":"), ensure_ascii=False).encode()


# ---------------------------------------------------------------------------
# Tokenizer: tokenizer.json to a SentencePiece model
# ---------------------------------------------------------------------------

#: SentencePiece piece types (`ModelProto.SentencePiece.Type`).
PIECE_NORMAL, PIECE_UNKNOWN, PIECE_CONTROL, PIECE_USER_DEFINED, PIECE_BYTE = 1, 2, 3, 4, 6


@dataclasses.dataclass(frozen=True)
class TokenizerPlan:
    """A `tokenizer.json` BPE vocabulary as SentencePiece describes it.

    Attributes:
        pieces: `(piece, type, score)` for every id in order.
        add_dummy_prefix: Whether a space is put before the text.
        split_by_whitespace: Whether a piece may not cross a space.
        byte_fallback: Whether unknown characters become byte pieces.
        unk_id: Id of the unknown piece.
        bos_id: Id of `<bos>`, or -1.
        eos_id: Id of `<eos>`, or -1.
        pad_id: Id of `<pad>`, or -1.
        framing: The framing the pack's `model.json` records.
    """

    pieces: tuple[tuple[str, int, float], ...]
    add_dummy_prefix: bool
    split_by_whitespace: bool
    byte_fallback: bool
    unk_id: int
    bos_id: int
    eos_id: int
    pad_id: int
    framing: Framing


def _is_byte_piece(piece: str) -> bool:
    """Whether `piece` is spelled `<0xNN>`."""
    return len(piece) == 6 and piece.startswith("<0x") and piece.endswith(">")


def _template_pieces(tokenizer: dict[str, Any]) -> tuple[tuple[str, ...], tuple[str, ...]]:
    """Reads the special tokens a tokenizer's template puts around one text.

    Args:
        tokenizer: The parsed `tokenizer.json`.

    Returns:
        The pieces before and after the text.

    Raises:
        PackError: The post-processor is not a single-sequence template.
    """
    processor = tokenizer.get("post_processor") or {}
    if processor.get("type") != "TemplateProcessing" or not isinstance(processor.get("single"), list):
        raise PackError("tokenizer.json: only a TemplateProcessing post-processor is supported")
    before: list[str] = []
    after: list[str] = []
    seen_text = False
    for item in processor["single"]:
        if "Sequence" in item:
            seen_text = True
        elif "SpecialToken" in item:
            (after if seen_text else before).append(item["SpecialToken"]["id"])
        else:
            raise PackError("tokenizer.json: unknown template item")
    if not seen_text:
        raise PackError("tokenizer.json: the template has no text")
    return tuple(before), tuple(after)


def tokenizer_plan(tokenizer: dict[str, Any], literal_pieces: str) -> TokenizerPlan:
    """Describes a byte-fallback BPE `tokenizer.json` as a SentencePiece model.

    A piece's score is minus the rank of the first merge that makes it, so
    SentencePiece applies merges in the published order. The published
    tokenizer matches its added tokens before anything else. With
    `literal_pieces` `added` the plugin does that itself (a Metaspace
    tokenizer also puts a space after each match, which SentencePiece
    cannot express), so they become control pieces; with `none` they become
    user-defined pieces, which SentencePiece matches first. Special tokens
    (`<bos>`, `<eos>`, `<pad>`) are always control pieces, so text cannot
    spell them.

    Args:
        tokenizer: The parsed `tokenizer.json`.
        literal_pieces: `added` or `none` (`ModelSpec.literal_pieces`).

    Returns:
        The plan.

    Raises:
        PackError: The tokenizer is not one this conversion understands.
    """
    model = tokenizer.get("model") or {}
    if model.get("type") != "BPE" or not isinstance(model.get("vocab"), dict):
        raise PackError("tokenizer.json: only a BPE model is supported")
    vocab: dict[str, int] = model["vocab"]
    by_id = sorted(vocab.items(), key=lambda item: item[1])
    if [index for _, index in by_id] != list(range(len(by_id))):
        raise PackError("tokenizer.json: vocabulary ids are not 0..n-1")
    rank: dict[str, int] = {}
    for position, merge in enumerate(model.get("merges") or []):
        left, right = merge.split(" ", 1) if isinstance(merge, str) else merge
        rank.setdefault(left + right, position)
    added = {entry["id"]: entry for entry in tokenizer.get("added_tokens") or []}
    unknown = model.get("unk_token") or "<unk>"
    if unknown not in vocab:
        raise PackError("tokenizer.json: the unknown token is not in the vocabulary")
    pieces: list[tuple[str, int, float]] = []
    for piece, index in by_id:
        if piece == "\t" and "<0x09>" not in vocab:
            # The Gemma 2 vocabulary lists the tab byte as a plain piece.
            pieces.append(("<0x09>", PIECE_BYTE, 0.0))
        elif piece == unknown:
            pieces.append((piece, PIECE_UNKNOWN, 0.0))
        elif index in added:
            literal = literal_pieces == "none" and not added[index].get("special")
            pieces.append((piece, PIECE_USER_DEFINED if literal else PIECE_CONTROL, 0.0))
        elif _is_byte_piece(piece):
            pieces.append((piece, PIECE_BYTE, 0.0))
        else:
            pieces.append((piece, PIECE_NORMAL, -float(rank[piece]) if piece in rank else 0.0))
    pre = tokenizer.get("pre_tokenizer") or {}
    metaspace = pre.get("type") == "Metaspace"
    before, after = _template_pieces(tokenizer)
    literals: tuple[str, ...] = ()
    if literal_pieces == "added":
        literals = tuple(
            entry["content"]
            for entry in tokenizer.get("added_tokens") or []
            if not entry.get("special") and not entry["content"].startswith("<unused")
        )
    return TokenizerPlan(
        pieces=tuple(pieces),
        add_dummy_prefix=metaspace and pre.get("prepend_scheme") in ("always", "first"),
        split_by_whitespace=metaspace and bool(pre.get("split", True)),
        byte_fallback=bool(model.get("byte_fallback")),
        unk_id=vocab[unknown],
        bos_id=vocab.get("<bos>", -1),
        eos_id=vocab.get("<eos>", -1),
        pad_id=vocab.get("<pad>", -1),
        framing=Framing(literals, before, after),
    )


def write_sentencepiece_model(plan: TokenizerPlan, path: Path) -> None:
    """Writes the plan as a SentencePiece `.model` file.

    Args:
        plan: The vocabulary and its options.
        path: Where to write the model.
    """
    from sentencepiece import sentencepiece_model_pb2 as pb  # type: ignore[import-untyped]

    proto = pb.ModelProto()
    for piece, kind, score in plan.pieces:
        entry = proto.pieces.add()
        entry.piece, entry.type, entry.score = piece, kind, score
    normalizer = proto.normalizer_spec
    normalizer.name = "identity"
    normalizer.add_dummy_prefix = plan.add_dummy_prefix
    normalizer.remove_extra_whitespaces = False
    normalizer.escape_whitespaces = True
    trainer = proto.trainer_spec
    trainer.model_type = 2  # BPE
    trainer.vocab_size = len(plan.pieces)
    trainer.byte_fallback = plan.byte_fallback
    trainer.split_by_whitespace = plan.split_by_whitespace
    trainer.unk_id, trainer.bos_id, trainer.eos_id, trainer.pad_id = plan.unk_id, plan.bos_id, plan.eos_id, plan.pad_id
    path.write_bytes(proto.SerializeToString(deterministic=True))


# ---------------------------------------------------------------------------
# Graph: vocabulary tables become graph inputs
# ---------------------------------------------------------------------------

#: `model.json` element names by NumPy type name.
ELEMENTS = {"int8": "i8", "uint8": "u8", "float32": "f32"}


def cut_graph(source: Path, target: Path) -> tuple[list[TableInfo], bytes, int]:
    """Turns every vocabulary table a graph reads by token id into an input.

    A table is an initializer with one row per vocabulary entry that a node
    reading `input_ids` also reads. It is removed from the graph and declared
    as a graph input with no shape, so the caller passes one row per token
    and numbers `input_ids` from 0.

    Args:
        source: The published ONNX graph (external data beside it, if any).
        target: Where to write the cut graph, as one file.

    Returns:
        The tables in row order, every token's row concatenated in id order,
        and the vocabulary size.

    Raises:
        PackError: The graph has no such table, or one of a type the pack
            layout has no name for.
    """
    import numpy as np
    import onnx
    from onnx import helper, numpy_helper

    model = onnx.load(str(source))
    graph = model.graph
    initializers = {tensor.name: tensor for tensor in graph.initializer}
    vocabulary = max((tensor.dims[0] for tensor in graph.initializer if tensor.dims), default=0)
    found: list[tuple[TableInfo, Any]] = []
    for node in graph.node:
        if "input_ids" not in node.input or node.op_type == "Shape":
            continue
        for name in node.input:
            tensor = initializers.get(name)
            if tensor is None or not tensor.dims or tensor.dims[0] != vocabulary:
                continue
            if any(table.name == name for table, _ in found):
                continue
            array = numpy_helper.to_array(tensor, base_dir=str(source.parent))
            element = ELEMENTS.get(str(array.dtype))
            if element is None:
                raise PackError(f"table `{name}` has unsupported type {array.dtype}")
            rows = np.ascontiguousarray(array.reshape(vocabulary, -1))
            if rows.dtype.byteorder == ">":
                rows = rows.astype(rows.dtype.newbyteorder("<"))
            found.append((TableInfo(name, element, int(rows.shape[1])), rows))
            graph.initializer.remove(tensor)
            value = helper.make_tensor_value_info(name, tensor.data_type, None)
            value.type.tensor_type.ClearField("shape")
            graph.input.append(value)
    if not found:
        raise PackError("the graph reads no vocabulary table by `input_ids`")
    target.write_bytes(model.SerializeToString(deterministic=True))
    parts = [rows.view(np.uint8).reshape(vocabulary, -1) for _, rows in found]
    return [table for table, _ in found], np.concatenate(parts, axis=1).tobytes(), vocabulary


# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Built:
    """What a build produced.

    Attributes:
        directory: The model directory.
        model_json: Size and digest of `model.json`.
        manifest: Size and digest of the OCI manifest.
        files: Name, size and digest of every file, in layer order.
        manifest_bytes: The OCI manifest itself, the bytes `manifest`
            measures, so what is pushed is what was measured.
    """

    directory: Path
    model_json: FilePin
    manifest: FilePin
    files: tuple[tuple[str, FilePin], ...]
    manifest_bytes: bytes


def verify_sources(spec: ModelSpec, work: Path) -> None:
    """Checks every fetched upstream file against its pin, before any is parsed.

    Args:
        spec: The model.
        work: Directory holding the fetched files.

    Raises:
        PackError: A file is missing or is not the pinned one.
    """
    for pin in spec.sources():
        path = work / pin.local_name
        if not path.is_file() or sha256_file(path) != pin.pin:
            raise PackError(f"{path} is missing or does not match its pin; run `fetch` first")


def build(spec: ModelSpec, source: str, work: Path, out: Path, spm_to_fb: Path) -> Built:
    """Builds one model's pack from the fetched upstream files.

    Args:
        spec: The model.
        source: Repository written into the OCI manifest.
        work: Directory holding the fetched files.
        out: Directory to write `<model_id>/` and its manifest under.
        spm_to_fb: The upstream SentencePiece Lite converter.

    Returns:
        The digests of what was written.

    Raises:
        PackError: An upstream file is missing or the graph or tokenizer is
            not one the build understands.
    """
    verify_sources(spec, work)
    directory = out / spec.model_id
    directory.mkdir(parents=True, exist_ok=True)

    tables, rows, vocabulary = cut_graph(work / spec.graph.local_name, directory / GRAPH_NAME)
    row_bytes = len(rows) // vocabulary
    (directory / BASE_ROWS_NAME).write_bytes(encode_rows(range(vocabulary), rows, row_bytes))

    plan = tokenizer_plan(json.loads((work / spec.tokenizer.local_name).read_text(encoding="utf-8")), spec.literal_pieces)
    if len(plan.pieces) != vocabulary:
        raise PackError(f"the tokenizer has {len(plan.pieces)} pieces, the graph's tables {vocabulary} rows")
    sentencepiece_model = work / f"{spec.model_id}.spm.model"
    write_sentencepiece_model(plan, sentencepiece_model)
    subprocess.run(
        [str(spm_to_fb), f"--model={sentencepiece_model}", f"--output={directory / TOKENIZER_NAME}"],
        check=True,
    )

    measured = {name: sha256_file(directory / name) for name in (GRAPH_NAME, TOKENIZER_NAME, BASE_ROWS_NAME)}
    model_json = pack_manifest(spec, measured, tables, plan.framing)
    (directory / MANIFEST_NAME).write_bytes(model_json)
    files = ((MANIFEST_NAME, sha256_bytes(model_json)), *measured.items())
    manifest = oci_manifest(spec, source, files)
    (out / f"{spec.model_id}.{OCI_MANIFEST_NAME}").write_bytes(manifest)
    return Built(directory, sha256_bytes(model_json), sha256_bytes(manifest), tuple(files), manifest)


def read_built(spec: ModelSpec, out: Path) -> Built:
    """Measures a pack an earlier `build` wrote.

    Args:
        spec: The model.
        out: The directory `build` wrote under.

    Returns:
        The digests of the files on disk.

    Raises:
        PackError: A file is missing, or the OCI manifest does not describe
            the files beside it.
    """
    directory = out / spec.model_id
    manifest_path = out / f"{spec.model_id}.{OCI_MANIFEST_NAME}"
    if not manifest_path.is_file():
        raise PackError(f"{manifest_path} is missing; run `build` first")
    manifest = manifest_path.read_bytes()
    files: list[tuple[str, FilePin]] = []
    for layer in json.loads(manifest)["layers"]:
        name = layer["annotations"]["org.opencontainers.image.title"]
        if not _is_file_name(name) or not (directory / name).is_file():
            raise PackError(f"{directory / name} is missing")
        pin = sha256_file(directory / name)
        if layer["digest"] != f"sha256:{pin.sha256}" or layer["size"] != pin.bytes:
            raise PackError(f"{directory / name} is not the file the manifest lists")
        files.append((name, pin))
    if not files or files[0][0] != MANIFEST_NAME:
        raise PackError("the manifest's first layer must be model.json")
    return Built(directory, files[0][1], sha256_bytes(manifest), tuple(files), manifest)


def pin_report(spec: ModelSpec, built: Built) -> list[str]:
    """Compares a build with the model's pins.

    Args:
        spec: The model and its pinned digests.
        built: What the build produced.

    Returns:
        One line per digest that differs; empty when the build is the pinned
        one.
    """
    problems = []
    if built.model_json.sha256 != spec.model_json_sha256:
        problems.append(f"model.json is {built.model_json.sha256}, pinned {spec.model_json_sha256}")
    if built.manifest.sha256 != spec.manifest_sha256:
        problems.append(f"OCI manifest is {built.manifest.sha256}, pinned {spec.manifest_sha256}")
    return problems


# ---------------------------------------------------------------------------
# Checks against the published model
# ---------------------------------------------------------------------------


def load_check_texts(path: Path = CHECK_TEXTS_FILE) -> list[dict[str, Any]]:
    """Reads the check texts.

    Args:
        path: The JSON file: an array of `{"text", "query"}` objects.

    Returns:
        The texts.

    Raises:
        PackError: The file is not that shape.
    """
    texts = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(texts, list) or not texts:
        raise PackError("check-texts.json must be a non-empty array")
    for item in texts:
        if not isinstance(item, dict) or not isinstance(item.get("text"), str) or not isinstance(item.get("query"), bool):
            raise PackError("check-texts.json: every item needs `text` and `query`")
    return texts


def compare_ids(expected: Sequence[Sequence[int]], found: Sequence[Sequence[int]]) -> list[int]:
    """Lists the texts whose token ids differ.

    Args:
        expected: Ids from the published tokenizer, one list per text.
        found: Ids from the plugin's tokenizer.

    Returns:
        Indexes of the texts that differ.

    Raises:
        PackError: The two lists cover different numbers of texts.
    """
    if len(expected) != len(found):
        raise PackError(f"{len(expected)} texts were tokenized by one side and {len(found)} by the other")
    return [index for index, (a, b) in enumerate(zip(expected, found)) if list(a) != list(b)]


def largest_difference(expected: Sequence[Sequence[float]], found: Sequence[Sequence[float]]) -> float:
    """The largest absolute difference between two sets of vectors.

    Args:
        expected: Vectors from the published model, one per text.
        found: Vectors from the plugin.

    Returns:
        The largest difference in any number.

    Raises:
        PackError: The sets differ in shape, or a number is not finite.
    """
    if len(expected) != len(found) or any(len(a) != len(b) for a, b in zip(expected, found)):
        raise PackError("the plugin's vectors are not the shape of the published model's")
    differences = [abs(x - y) for a, b in zip(expected, found) for x, y in zip(a, b)]
    # `max` would pass over a NaN that follows a finite number.
    if not all(math.isfinite(difference) for difference in differences):
        raise PackError("a vector holds a number that is not finite")
    return max(differences, default=0.0)


def _run_example(command: Sequence[str], texts: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Runs one of the plugin's examples over the texts and parses its reply."""
    done = subprocess.run(list(command), input=json.dumps(texts).encode(), capture_output=True, check=False)
    if done.returncode != 0:
        raise PackError(f"{command[0]} failed: {done.stderr.decode(errors='replace')[-2000:]}")
    reply = json.loads(done.stdout)
    if not isinstance(reply, dict):
        raise PackError(f"{command[0]} printed no object")
    return reply


def check(spec: ModelSpec, work: Path, out: Path, examples: Path, library: Path) -> dict[str, Any]:
    """Checks a built pack against the published model.

    Every check text is tokenized by the published `tokenizer.json` and by
    the plugin's tokenizer from the pack, and embedded by the published ONNX
    graph and by the plugin from the pack.

    Args:
        spec: The model.
        work: Directory holding the fetched upstream files.
        out: Directory `build` wrote under.
        examples: Directory holding the plugin's `embed` and `tokens`
            example executables.
        library: The pinned ONNX Runtime shared library.

    Returns:
        A summary: text count, largest vector difference, revision.

    Raises:
        PackError: An upstream file is not the pinned one, a token id
            differs, a text fell back to single characters, or a vector
            differs by more than `VECTOR_TOLERANCE`.
    """
    import numpy as np
    import onnxruntime
    from tokenizers import Tokenizer

    verify_sources(spec, work)
    texts = load_check_texts()
    tokenizer = Tokenizer.from_file(str(work / spec.tokenizer.local_name))
    tokenizer.no_padding()
    tokenizer.enable_truncation(spec.max_tokens)
    prefixed = [(spec.query_prefix if item["query"] else spec.document_prefix) + item["text"] for item in texts]
    expected_ids = [tokenizer.encode(text).ids for text in prefixed]

    tokens = _run_example([str(examples / "tokens"), str(out), spec.model_id], texts)
    differing = compare_ids(expected_ids, tokens["ids"])
    if differing:
        raise PackError(f"token ids differ from the published tokenizer for texts {differing}")

    options = onnxruntime.SessionOptions()
    options.intra_op_num_threads = 1
    session = onnxruntime.InferenceSession(str(work / spec.graph.local_name), options, providers=["CPUExecutionProvider"])
    outputs = [output.name for output in session.get_outputs()]
    expected: list[list[float]] = []
    for ids in expected_ids:
        feed = {"input_ids": np.array([ids], np.int64), "attention_mask": np.ones((1, len(ids)), np.int64)}
        result = session.run([spec.output], feed)[0][0] if spec.output in outputs else None
        if result is None:
            raise PackError(f"the published graph has no output `{spec.output}`")
        vector = result.mean(axis=0) if spec.pooling == "mean" else result
        expected.append((vector / np.linalg.norm(vector)).astype(np.float32).tolist())

    reply = _run_example([str(examples / "embed"), str(out), str(library), spec.model_id], texts)
    if reply["fallbacks"]:
        raise PackError(f"texts {reply['fallbacks']} fell back to single characters; the pack lacks rows")
    difference = largest_difference(expected, reply["vectors"])
    if not difference <= VECTOR_TOLERANCE:
        raise PackError(f"vectors differ from the published model by {difference:.3g} (allowed {VECTOR_TOLERANCE:g})")
    return {"model": spec.model_id, "texts": len(texts), "largest_difference": difference, "revision": reply["revision"]}


# ---------------------------------------------------------------------------
# Registry (OCI distribution API)
# ---------------------------------------------------------------------------

#: Longest registry reply body the client reads as JSON.
MAX_REPLY_BYTES = 64 * 1024
#: A bearer token: the characters RFC 6750 allows, at most 8192 of them.
_TOKEN = re.compile(r"[A-Za-z0-9._~+/=-]{1,8192}\Z")
#: An upload path and query: URL characters only, so no space, control
#: character, quote, backslash or fragment reaches a request line.
_UPLOAD_PATH = re.compile(r"/[A-Za-z0-9._~!$&'()*+,;=:@%/?-]{0,4095}\Z")


def parse_token_reply(body: bytes) -> str:
    """Reads the bearer token from a registry's token reply.

    The reply comes from the network and the token goes into a request
    header, so only a token made of header-safe characters is accepted.

    Args:
        body: The reply body.

    Returns:
        The token.

    Raises:
        PackError: The body is not a JSON object with a usable `token`.
    """
    if len(body) > MAX_REPLY_BYTES:
        raise PackError("the registry's token reply is too large")
    try:
        reply = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError, RecursionError) as error:
        raise PackError("the registry's token reply is not JSON") from error
    token = reply.get("token") if isinstance(reply, dict) else None
    if not isinstance(token, str) or not _TOKEN.match(token):
        raise PackError("the registry's token reply holds no usable token")
    return token


def upload_target(host: str, location: str, digest_hex: str) -> str:
    """Builds the address a blob is sent to from a registry's `Location`.

    The registry answers an upload request with where to send the bytes.
    The bearer token goes with them, so the address must stay on the same
    host and use https.

    Args:
        host: The registry host the upload was started on.
        location: The `Location` header: a path, or an https address on
            `host`.
        digest_hex: The blob's SHA-256 in hexadecimal.

    Returns:
        The address, with the blob's digest added to its query.

    Raises:
        PackError: The location is on another host, not https, or holds a
            character that does not belong in an address.
    """
    origin = f"https://{host}"
    path = location[len(origin) :] if location.startswith(origin + "/") else location
    if not _UPLOAD_PATH.match(path) or path.startswith("//"):
        raise PackError("the registry sent an upload address this client will not use")
    separator = "&" if "?" in path else "?"
    return f"{origin}{path}{separator}digest=sha256:{digest_hex}"


#: Sends one request and returns status, headers and body. Injected in tests.
Transport = Callable[[urllib.request.Request], tuple[int, dict[str, str], bytes]]


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuses every redirect, so a bearer token is never sent on to
    whatever address a reply names."""

    def redirect_request(self, *_args: Any, **_kwargs: Any) -> None:
        """Returns no follow-up request; the 3xx reply is the answer."""
        return None


def _urllib_transport(request: urllib.request.Request) -> tuple[int, dict[str, str], bytes]:
    """Sends a request with `urllib`, returning error statuses as values.

    Redirects are not followed: the reply's own status comes back. No more
    of a body is read than one byte past `MAX_REPLY_BYTES`, enough for the
    token parser to refuse an oversized reply.
    """
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        with opener.open(request, timeout=600) as response:
            return response.status, {k.lower(): v for k, v in response.headers.items()}, response.read(MAX_REPLY_BYTES + 1)
    except urllib.error.HTTPError as error:
        return error.code, {k.lower(): v for k, v in error.headers.items()}, error.read(MAX_REPLY_BYTES + 1)


class Registry:
    """A client for the few OCI distribution calls a publish needs.

    Attributes:
        host: Registry host name.
        repository: Repository path on that host.
    """

    def __init__(self, reference: str, username: str, password: str, transport: Transport | None = None) -> None:
        """Creates a client.

        Args:
            reference: `host/path` of the repository to push to.
            username: Account the token is requested for.
            password: That account's token. Never logged or put on a
                command line.
            transport: Sends requests; `urllib` when omitted.

        Raises:
            PackError: The reference is not `host/path` in lowercase.
        """
        host, _, repository = reference.partition("/")
        if not re.fullmatch(r"[a-z0-9.-]+(:[0-9]+)?/[a-z0-9._/-]+", reference):
            raise PackError("a registry reference is `host/path` in lowercase")
        self.host = host
        self.repository = repository
        self._basic = base64.b64encode(f"{username}:{password}".encode()).decode()
        self._transport = transport or _urllib_transport
        self._token: str | None = None

    def _request(self, method: str, url: str, headers: dict[str, str] | None = None, data: bytes | BinaryIO | None = None) -> tuple[int, dict[str, str], bytes]:
        """Sends one authorized request to the registry."""
        if self._token is None:
            scope = urllib.parse.quote(f"repository:{self.repository}:pull,push", safe=":,/")
            token_request = urllib.request.Request(
                f"https://{self.host}/token?service={self.host}&scope={scope}",
                headers={"Authorization": f"Basic {self._basic}"},
            )
            status, _, body = self._transport(token_request)
            if status != 200:
                raise PackError(f"the registry refused a token (HTTP {status})")
            self._token = parse_token_reply(body)
        request = urllib.request.Request(url, method=method, data=data)  # noqa: S310 - https only
        request.add_header("Authorization", f"Bearer {self._token}")
        for key, value in (headers or {}).items():
            request.add_header(key, value)
        return self._transport(request)

    def _url(self, path: str) -> str:
        """Address of a path under this repository."""
        return f"https://{self.host}/v2/{self.repository}/{path}"

    def has_blob(self, pin: FilePin) -> bool:
        """Whether the repository already holds a blob."""
        status, _, _ = self._request("HEAD", self._url(f"blobs/sha256:{pin.sha256}"))
        return status == 200

    def push_blob(self, pin: FilePin, data: bytes | Path) -> bool:
        """Uploads one blob unless the repository already holds it.

        Args:
            pin: The blob's size and digest.
            data: The blob, in memory or as a file.

        Returns:
            True when it was uploaded, false when it was already there.

        Raises:
            PackError: The registry refused the upload.
        """
        if self.has_blob(pin):
            return False
        status, headers, _ = self._request("POST", self._url("blobs/uploads/"), {"Content-Length": "0"})
        location = headers.get("location")
        if status != 202 or not location:
            raise PackError(f"the registry refused to start an upload (HTTP {status})")
        target = upload_target(self.host, location, pin.sha256)
        upload_headers = {"Content-Type": "application/octet-stream", "Content-Length": str(pin.bytes)}
        if isinstance(data, Path):
            with data.open("rb") as handle:
                status, _, _ = self._request("PUT", target, upload_headers, handle)
        else:
            status, _, _ = self._request("PUT", target, upload_headers, data)
        if status != 201:
            raise PackError(f"the registry refused blob sha256:{pin.sha256} (HTTP {status})")
        return True

    def push_manifest(self, manifest: bytes, tag: str) -> str:
        """Stores a manifest under a tag.

        Args:
            manifest: The manifest's bytes.
            tag: The tag to point at it.

        Returns:
            The manifest's digest, `sha256:<hex>`.

        Raises:
            PackError: The registry refused it or stored other bytes.
        """
        digest = f"sha256:{sha256_bytes(manifest).sha256}"
        status, headers, _ = self._request("PUT", self._url(f"manifests/{tag}"), {"Content-Type": OCI_MANIFEST_TYPE}, manifest)
        if status != 201:
            raise PackError(f"the registry refused the manifest (HTTP {status})")
        stored = headers.get("docker-content-digest")
        if stored is not None and stored != digest:
            raise PackError(f"the registry stored {stored}, expected {digest}")
        return digest


def publish(spec: ModelSpec, built: Built, registry: Registry) -> dict[str, Any]:
    """Pushes a built pack to a registry.

    Blobs the repository already holds are not sent again, and the tag is
    derived from the manifest's digest, so publishing the same pack twice
    changes nothing.

    Args:
        spec: The model.
        built: The pack on disk.
        registry: The repository to push to.

    Returns:
        The artifact's name, digest and tag, and how many blobs were sent.
    """
    uploaded = int(registry.push_blob(sha256_bytes(OCI_EMPTY_BLOB), OCI_EMPTY_BLOB))
    for name, pin in built.files:
        uploaded += int(registry.push_blob(pin, built.directory / name))
    tag = f"sha256-{built.manifest.sha256[:12]}"
    digest = registry.push_manifest(built.manifest_bytes, tag)
    return {"name": f"{registry.host}/{registry.repository}", "digest": digest, "tag": tag, "uploaded": uploaded}


# ---------------------------------------------------------------------------
# Command line
# ---------------------------------------------------------------------------


def _write_output(values: dict[str, Any]) -> None:
    """Prints `key=value` lines, the form a workflow appends to its outputs."""
    for key, value in values.items():
        print(f"{key}={value}")


def main(argv: Sequence[str] | None = None) -> int:
    """Runs one command.

    Args:
        argv: Arguments; `sys.argv` when omitted.

    Returns:
        The process exit status.
    """
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("command", choices=["fetch", "build", "check", "pin", "publish", "list"])
    parser.add_argument("--model", help="model id from models.json")
    parser.add_argument("--work", type=Path, default=Path("target/model-packs/work"), help="directory for upstream files")
    parser.add_argument("--out", type=Path, default=Path("target/model-packs/out"), help="directory for built packs")
    parser.add_argument("--spm-to-fb", type=Path, help="upstream SentencePiece Lite converter (build)")
    parser.add_argument("--examples", type=Path, help="directory of the plugin's example executables (check)")
    parser.add_argument("--onnxruntime", type=Path, help="pinned ONNX Runtime shared library (check)")
    parser.add_argument("--write", action="store_true", help="pin: rewrite models.json with the built digests")
    parser.add_argument("--registry", help="publish: registry path to push under (default: models.json)")
    args = parser.parse_args(argv)
    try:
        pins = parse_pins(MODELS_FILE.read_text(encoding="utf-8"))
        if args.command == "list":
            print(json.dumps(sorted(pins.models)))
            return 0
        if args.model not in pins.models:
            raise PackError(f"--model must be one of {sorted(pins.models)}")
        spec = pins.models[args.model]
        work = args.work / spec.model_id
        if args.command == "fetch":
            for pin in spec.sources():
                print(f"fetched {fetch(pin, work)}")
        elif args.command == "build":
            if args.spm_to_fb is None:
                raise PackError("build needs --spm-to-fb")
            built = build(spec, pins.source, work, args.out, args.spm_to_fb)
            for name, pin in built.files:
                print(f"{name}\t{pin.bytes}\t{pin.sha256}")
            _write_output({"model_json_sha256": built.model_json.sha256, "manifest_sha256": built.manifest.sha256})
        elif args.command == "check":
            if args.examples is None or args.onnxruntime is None:
                raise PackError("check needs --examples and --onnxruntime")
            print(json.dumps(check(spec, work, args.out, args.examples, args.onnxruntime)))
        elif args.command == "pin":
            built = read_built(spec, args.out)
            problems = pin_report(spec, built)
            if problems and args.write:
                text = MODELS_FILE.read_text(encoding="utf-8")
                MODELS_FILE.write_text(rewrite_pack_pins(text, spec.model_id, built.model_json.sha256, built.manifest.sha256), encoding="utf-8")
                print(f"rewrote the pins of {spec.model_id}")
            elif problems:
                raise PackError("the build is not the pinned one: " + "; ".join(problems))
            else:
                print(f"{spec.model_id} matches its pins")
        else:
            built = read_built(spec, args.out)
            problems = pin_report(spec, built)
            if problems:
                raise PackError("refusing to publish a build that is not the pinned one: " + "; ".join(problems))
            password = os.environ.get("REGISTRY_PASSWORD", "")
            username = os.environ.get("REGISTRY_USERNAME", "")
            if not password or not username:
                raise PackError("publish reads REGISTRY_USERNAME and REGISTRY_PASSWORD from the environment")
            reference = f"{args.registry or pins.registry}/{spec.model_id}"
            _write_output(publish(spec, built, Registry(reference, username, password)))
    except PackError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
