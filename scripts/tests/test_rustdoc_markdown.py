#!/usr/bin/env python3
"""rustdoc_markdown.py: types, doc comments, placement and links (offline).

The crates here are small hand-written rustdoc JSON documents in format 56's
shape, so the tests need no toolchain.
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import rustdoc_markdown as rm  # noqa: E402

NO_GENERICS: dict[str, Any] = {"params": [], "where_predicates": []}
HEADER = {"is_const": False, "is_unsafe": False, "is_async": False, "abi": "Rust"}


def path(p: str, item_id: int, args: Any = None) -> dict[str, Any]:
    """A resolved path type."""
    return {"resolved_path": {"path": p, "id": item_id, "args": args}}


def angle(*types: Any) -> dict[str, Any]:
    """Angle-bracketed generic arguments."""
    return {"angle_bracketed": {"args": [{"type": t} for t in types], "constraints": []}}


def item(item_id: int, name: str | None, inner: dict[str, Any], docs: str | None = None, **extra: Any) -> dict[str, Any]:
    """A public item."""
    return {"id": item_id, "crate_id": 0, "name": name, "visibility": "public", "docs": docs, "links": {}, "inner": inner, **extra}


def fn(sig_inputs: list[Any], output: Any = None, generics: Any = None, header: Any = None) -> dict[str, Any]:
    """A function inner."""
    return {
        "function": {
            "sig": {"inputs": sig_inputs, "output": output, "is_c_variadic": False},
            "generics": generics or NO_GENERICS,
            "header": header or HEADER,
            "has_body": True,
        }
    }


def demo_crate() -> dict[str, Any]:
    """A crate ``demo``: a struct re-exported from a private module, a public
    module with a trait, an enum, a function, and a re-export from ``other``."""
    index = {
        "0": item(
            0,
            "demo",
            {"module": {"is_crate": True, "items": [1, 2, 3, 4, 5, 40], "is_stripped": False}},
            docs="Demo crate.\n\n# Overview\n\nBuild a [`Mux`] with [`open`].",
            links={"`Mux`": 10, "`open`": 20},
        ),
        # pub use inner::Mux;
        "1": item(1, None, {"use": {"source": "inner::Mux", "name": "Mux", "id": 10, "is_glob": False}}),
        "2": item(2, "open", {}, links={}),
        "3": item(3, "proto", {"module": {"is_crate": False, "items": [30, 31], "is_stripped": False}}, docs="Wire protocol."),
        "4": item(4, None, {"use": {"source": "other::Thing", "name": "Thing", "id": 50, "is_glob": False}}),
        "5": item(5, "Mode", {"enum": {"generics": NO_GENERICS, "has_stripped_variants": False, "variants": [6, 7], "impls": []}}, docs="How to open."),
        "6": item(6, "Read", {"variant": {"kind": "plain", "discriminant": None}}, docs="Read only."),
        "7": item(7, "Write", {"variant": {"kind": {"tuple": [8]}, "discriminant": None}}),
        "8": item(8, "0", {"struct_field": {"primitive": "u32"}}),
        "10": item(
            10,
            "Mux",
            {
                "struct": {
                    "kind": {"plain": {"fields": [11, 12], "has_stripped_fields": True}},
                    "generics": {"params": [{"name": "T", "kind": {"type": {"bounds": [], "default": None, "is_synthetic": False}}}], "where_predicates": []},
                    "impls": [13, 15, 16],
                }
            },
            docs="A multiplexer.\n\n# Errors\n\nNever.\n\n```\n# use demo::Mux;\nlet m = Mux::new();\n```",
        ),
        "11": item(11, "streams", {"struct_field": path("std::vec::Vec", 90, angle({"generic": "T"}))}, docs="Open streams."),
        "12": {**item(12, "secret", {"struct_field": {"primitive": "u8"}}), "visibility": "crate"},
        "13": item(13, None, {"impl": {"is_unsafe": False, "generics": NO_GENERICS, "provided_trait_methods": [], "trait": None, "for": {"generic": "Mux"}, "items": [14], "is_negative": False, "is_synthetic": False, "blanket_impl": None}}),
        "14": item(
            14,
            "send",
            fn([["self", {"borrowed_ref": {"lifetime": None, "is_mutable": True, "type": {"generic": "Self"}}}], ["data", {"impl_trait": [{"trait_bound": {"trait": {"path": "AsRef", "id": 91, "args": angle({"slice": {"primitive": "u8"}})}, "generic_params": [], "modifier": "none"}}]}]], output=path("crate::Result", 92, angle({"tuple": []}))),
            docs="Send bytes; see [`Frame`].",
            links={"`Frame`": 30},
        ),
        "15": item(15, None, {"impl": {"is_unsafe": False, "generics": NO_GENERICS, "provided_trait_methods": [], "trait": {"path": "Clone", "id": 93, "args": None}, "for": {"generic": "Mux"}, "items": [], "is_negative": False, "is_synthetic": False, "blanket_impl": None}}),
        "16": item(16, None, {"impl": {"is_unsafe": False, "generics": NO_GENERICS, "provided_trait_methods": [], "trait": {"path": "Send", "id": 94, "args": None}, "for": {"generic": "Mux"}, "items": [], "is_negative": False, "is_synthetic": True, "blanket_impl": None}}),
        "20": item(
            20,
            "open",
            fn([["path", {"borrowed_ref": {"lifetime": None, "is_mutable": False, "type": path("std::path::Path", 95)}}]], output=path("Mux", 10, angle({"primitive": "u8"})), header={**HEADER, "is_async": True}),
            docs="Open a [`Mux`] at a [`Path`], or [fail](crate::proto::Frame).",
            links={"`Mux`": 10, "`Path`": 95, "crate::proto::Frame": 30},
        ),
        "30": item(30, "Frame", {"struct": {"kind": "unit", "generics": NO_GENERICS, "impls": []}}, docs="One frame."),
        "31": item(31, "Codec", {"trait": {"is_auto": False, "is_unsafe": False, "is_dyn_compatible": True, "items": [32], "generics": NO_GENERICS, "bounds": [{"trait_bound": {"trait": {"path": "Send", "id": 94, "args": None}, "generic_params": [], "modifier": "none"}}], "implementations": []}}),
        "32": {**item(32, "decode", fn([["self", {"borrowed_ref": {"lifetime": None, "is_mutable": False, "type": {"generic": "Self"}}}]], output={"primitive": "bool"})), "visibility": "default"},
        "40": item(40, "LIMIT", {"constant": {"type": {"primitive": "usize"}, "const": {"expr": "64", "value": None, "is_literal": True}}}),
    }
    # `pub use` of an item defined in this crate under a private module: the
    # root's id 2 is not a use, so drop it to keep the fixture honest.
    del index["2"]
    index["0"]["inner"]["module"]["items"].remove(2)
    index["0"]["inner"]["module"]["items"].append(20)
    return {
        "root": 0,
        "format_version": 56,
        "index": index,
        "paths": {
            "10": {"crate_id": 0, "path": ["demo", "inner", "Mux"], "kind": "struct"},
            "30": {"crate_id": 0, "path": ["demo", "proto", "Frame"], "kind": "struct"},
            "50": {"crate_id": 1, "path": ["other", "Thing"], "kind": "struct"},
            "95": {"crate_id": 2, "path": ["std", "path", "Path"], "kind": "struct"},
        },
        "external_crates": {"1": {"name": "other"}, "2": {"name": "std"}},
    }


def other_crate() -> dict[str, Any]:
    """A crate ``other``: a struct, a union, a struct whose field docs have a
    heading, an empty unsafe trait, a constant and two procedural macros."""
    return {
        "root": 0,
        "format_version": 56,
        "index": {
            "0": item(0, "other", {"module": {"is_crate": True, "items": [1, 2, 5, 7, 8, 9, 10], "is_stripped": False}}),
            "1": item(1, "Thing", {"struct": {"kind": "unit", "generics": NO_GENERICS, "impls": []}}, docs="A thing."),
            "2": item(2, "Pair", {"union": {"generics": NO_GENERICS, "has_stripped_fields": False, "fields": [3, 4], "impls": []}}, docs="Two views."),
            "3": item(3, "word", {"struct_field": {"primitive": "u32"}}, docs="As a word."),
            "4": item(4, "bytes", {"struct_field": {"array": {"type": {"primitive": "u8"}, "len": "4"}}}),
            "5": item(5, "Gauge", {"struct": {"kind": {"plain": {"fields": [6], "has_stripped_fields": False}}, "generics": NO_GENERICS, "impls": []}}, docs="A gauge."),
            "6": item(6, "level", {"struct_field": {"primitive": "u8"}}, docs="Level.\n\n# Limit\n\nAt most [`LIMIT`]."),
            "7": item(7, "Marker", {"trait": {"is_auto": False, "is_unsafe": True, "is_dyn_compatible": True, "items": [], "generics": NO_GENERICS, "bounds": [], "implementations": []}}),
            "8": item(8, "LIMIT", {"constant": {"type": {"primitive": "u8"}, "const": {"expr": "10", "value": None, "is_literal": True}}}),
            "9": item(9, "make", {"proc_macro": {"kind": "bang", "helpers": []}}),
            "10": item(10, "tagged", {"proc_macro": {"kind": "attr", "helpers": []}}),
        },
        "paths": {"1": {"crate_id": 0, "path": ["other", "Thing"], "kind": "struct"}},
        "external_crates": {},
    }


class TypeTest(unittest.TestCase):
    """Types and signatures read like Rust source."""

    def test_types(self) -> None:
        self.assertEqual(rm.render_type(path("std::sync::Arc", 1, angle({"dyn_trait": {"traits": [{"trait": {"path": "Fn", "id": 2, "args": {"parenthesized": {"inputs": [{"primitive": "str"}], "output": None}}}, "generic_params": []}], "lifetime": "'static"}}))), "Arc<dyn Fn(str) + 'static>")
        self.assertEqual(rm.render_type({"borrowed_ref": {"lifetime": "'a", "is_mutable": True, "type": {"slice": {"primitive": "u8"}}}}), "&'a mut [u8]")
        self.assertEqual(rm.render_type({"tuple": [{"primitive": "u8"}]}), "(u8,)")
        self.assertEqual(rm.render_type({"array": {"type": {"primitive": "u8"}, "len": "32"}}), "[u8; 32]")
        self.assertEqual(rm.render_type({"raw_pointer": {"is_mutable": False, "type": {"primitive": "u8"}}}), "*const u8")
        qualified = {"qualified_path": {"name": "Item", "args": None, "self_type": {"generic": "I"}, "trait": {"path": "Iterator", "id": 3, "args": None}}}
        self.assertEqual(rm.render_type(qualified), "<I as Iterator>::Item")

    def test_function_signature(self) -> None:
        generics = {
            "params": [
                {"name": "impl AsRef<str>", "kind": {"type": {"bounds": [], "default": None, "is_synthetic": True}}},
                {"name": "'a", "kind": {"lifetime": {"outlives": []}}},
            ],
            "where_predicates": [{"bound_predicate": {"type": {"generic": "Self"}, "bounds": [{"trait_bound": {"trait": {"path": "Sized", "id": 4, "args": None}, "generic_params": [], "modifier": "none"}}], "generic_params": []}}],
        }
        sig = fn([["self", {"generic": "Self"}], ["name", {"impl_trait": [{"trait_bound": {"trait": {"path": "AsRef", "id": 5, "args": angle({"primitive": "str"})}, "generic_params": [], "modifier": "none"}}]}]], output=None, generics=generics, header={**HEADER, "is_const": True})
        self.assertEqual(rm.render_fn("rename", sig["function"], True), "pub const fn rename<'a>(self, name: impl AsRef<str>)\nwhere\n    Self: Sized,")


class DocsTest(unittest.TestCase):
    """Doc comments are demoted, linked and cleaned."""

    def ctx(self, links: dict[str, int], demote: int = 3) -> rm.DocContext:
        targets = {1: "mux.md#mux", 2: None}
        return rm.DocContext(links, lambda i: targets.get(i), demote)

    def test_headings_move_under_the_item(self) -> None:
        out = rm.rewrite_docs("Intro.\n\n# Errors\n\n## Detail\n\n#### Deep", self.ctx({}))
        self.assertEqual(out, "Intro.\n\n#### Errors\n\n##### Detail\n\n###### Deep")

    def test_rust_blocks_get_a_fence_and_lose_hidden_lines(self) -> None:
        out = rm.rewrite_docs("```\n# use x::Y;\n#\nlet y = Y;\n## not hidden\n```\n\n```text\n# kept\n```", self.ctx({}))
        self.assertEqual(out, "```rust\nlet y = Y;\n# not hidden\n```\n\n```text\n# kept\n```")

    def test_links(self) -> None:
        links = {"`Mux`": 1, "crate::Mux": 1, "`gone`": 2, "label": 1}
        text = "Use [`Mux`], [the mux](crate::Mux), [`gone`], [ref][label], `[`Mux`]` and [unknown].\n\n[label]: crate::x"
        out = rm.rewrite_docs(text, self.ctx({**links, "crate::x": 1}))
        self.assertEqual(
            out,
            "Use [`Mux`](mux.md#mux), [the mux](mux.md#mux), `gone`, [ref](mux.md#mux), `[`Mux`]` and [unknown].\n\n[label]: mux.md#mux",
        )

    def test_code_blocks_keep_their_brackets(self) -> None:
        text = "```toml\n[dependencies]\n[`Mux`]\n```"
        self.assertEqual(rm.rewrite_docs(text, self.ctx({"`Mux`": 1, "dependencies": 1})), text)

    def test_summary(self) -> None:
        self.assertEqual(rm.summary("Open a [`Mux`] [here](x) | now.\nSecond line.\n\nMore."), "Open a `Mux` here \\| now. Second line.")
        self.assertEqual(rm.summary(None), "")

    def test_anchors_number_repeats(self) -> None:
        anchors = rm.Anchors()
        self.assertEqual([anchors.take(h) for h in ("Mux", "`Mux`", "Mux::new", "Errors", "Errors")], ["mux", "mux-1", "muxnew", "errors", "errors-1"])
        self.assertEqual(rm.slug("Module: `mux` & more"), "module-mux--more")

    def test_external_urls(self) -> None:
        self.assertEqual(rm.external_url(["zeroize", "ZeroizeOnDrop"], "trait", "zeroize"), "https://docs.rs/zeroize/latest/zeroize/trait.ZeroizeOnDrop.html")
        self.assertEqual(rm.external_url(["std", "sync", "Arc"], "struct", "std"), "https://doc.rust-lang.org/stable/std/sync/struct.Arc.html")
        self.assertEqual(rm.external_url(["tokio", "net"], "module", "tokio"), "https://docs.rs/tokio/latest/tokio/net/index.html")
        self.assertIsNone(rm.external_url(["serde", "Serialize", "serialize"], "assoc_type", "serde"))


class RenderTest(unittest.TestCase):
    """Pages follow the public API and link within and across crates."""

    @classmethod
    def setUpClass(cls) -> None:
        crates = {"demo": rm.Crate("demo", demo_crate()), "other": rm.Crate("other", other_crate())}
        cls.pages = rm.Renderer(crates).render_all()

    def test_one_page_per_public_module(self) -> None:
        self.assertEqual(sorted(self.pages), ["demo/index.md", "demo/proto.md", "other/index.md"])

    def test_root_page(self) -> None:
        page = self.pages["demo/index.md"]
        self.assertEqual(page.count("\n# "), 0)
        self.assertTrue(page.startswith("# demo\n\nDemo crate.\n\n## Overview\n\nBuild a [`Mux`](#mux) with [`open`](#open)."))
        # The struct from the private module is documented on the root page.
        self.assertIn("### Mux\n\n```rust\npub struct Mux<T> {\n    pub streams: Vec<T>,\n    /* private fields */\n}\n```", page)
        self.assertNotIn("secret", page)
        self.assertIn("#### Errors\n\nNever.\n\n```rust\nlet m = Mux::new();\n```", page)
        self.assertIn("- `streams`: Open streams.", page)
        self.assertIn("#### Mux::send\n\n```rust\npub fn send(&mut self, data: impl AsRef<[u8]>) -> Result<()>\n```", page)
        self.assertIn("Send bytes; see [`Frame`](proto.md#frame).", page)
        # Synthetic (auto trait) impls are left out.
        self.assertIn("Implements: `Clone`\n", page)
        self.assertIn("pub async fn open(path: &Path) -> Mux<u8>", page)
        self.assertIn("[`Path`](https://doc.rust-lang.org/stable/std/path/struct.Path.html)", page)
        self.assertIn("[fail](proto.md#frame)", page)
        self.assertIn("pub enum Mode {\n    Read,\n    Write(u32),\n}", page)
        self.assertIn("**Variants**\n\n- `Read`: Read only.", page)
        self.assertIn("pub const LIMIT: usize = 64;", page)
        self.assertIn("- [`proto`](proto.md): Wire protocol.", page)
        self.assertIn("- [`other::Thing`](../other/index.md#thing)", page)
        self.assertIn("| [`Mux`](#mux) | struct | A multiplexer. |", page)

    def test_section_order(self) -> None:
        page = self.pages["demo/index.md"]
        order = [page.index(f"\n## {s}\n") for s in ("Re-exports", "Modules", "Structs", "Enums", "Functions", "Constants")]
        self.assertEqual(order, sorted(order))

    def test_trait_page(self) -> None:
        page = self.pages["demo/proto.md"]
        self.assertIn("pub trait Codec: Send {\n    fn decode(&self) -> bool;\n}", page)
        self.assertIn("#### Codec::decode", page)
        self.assertIn("pub struct Frame;", page)

    def test_unions_traits_and_macros(self) -> None:
        page = self.pages["other/index.md"]
        self.assertIn("pub union Pair {\n    pub word: u32,\n    pub bytes: [u8; 4],\n}", page)
        self.assertIn("**Fields**\n\n- `word`: As a word.", page)
        self.assertIn("pub unsafe trait Marker {}", page)
        self.assertIn("#[proc_macro] make", page)
        self.assertIn("#[proc_macro_attribute] tagged", page)

    def test_field_doc_headings_are_counted(self) -> None:
        # The field's "Limit" heading comes first on the page, so the
        # constant is the second "limit" anchor.
        page = self.pages["other/index.md"]
        self.assertLess(page.index("Limit"), page.index("### LIMIT"))
        self.assertIn("| [`LIMIT`](#limit-1) |", page)

    def test_load_crates_checks_the_format(self) -> None:
        with tempfile.TemporaryDirectory(prefix="rustdoc-markdown-") as tmp:
            d = Path(tmp)
            (d / "demo.json").write_text(json.dumps(demo_crate()))
            (d / "old.json").write_text(json.dumps({**other_crate(), "format_version": 55}))
            self.assertEqual(list(rm.load_crates(d, ["demo", "missing"], 56)), ["demo"])
            with self.assertRaises(ValueError):
                rm.load_crates(d, ["old"], 56)


if __name__ == "__main__":
    unittest.main()
