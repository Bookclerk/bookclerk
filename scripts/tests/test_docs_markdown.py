#!/usr/bin/env python3
"""docs-markdown.py: output-directory guard, Rust link rewriting, link checks (offline).

The guard tests never point a deleting call at a real directory: refusals of
`/`, the home directory, the repository and the like go through
`check_output_dir`, which only inspects paths, and every directory
`prepare_output_dir` is allowed to clear is a fresh temporary one.
"""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType

ROOT = Path(__file__).resolve().parents[2]


def _load() -> ModuleType:
    path = ROOT / "scripts" / "docs-markdown.py"
    spec = importlib.util.spec_from_file_location("docs_markdown", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["docs_markdown"] = module
    spec.loader.exec_module(module)
    return module


dm = _load()


class OutputGuardTest(unittest.TestCase):
    """The output directory may only be cleared inside the scratch root."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="docs-markdown-guard-")
        self.scratch = Path(self._tmp.name) / "scratch"
        self.scratch.mkdir()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_refuses_dangerous_outputs_without_touching_them(self) -> None:
        for out in (Path("/"), Path.home(), ROOT, Path("/mnt"), Path("/mnt/project-files"), Path("/tmp")):
            with self.subTest(out=out), self.assertRaises(dm.UnsafeOutputDir):
                dm.check_output_dir(out, self.scratch)

    def test_refuses_dangerous_scratch_roots(self) -> None:
        for root in (Path("/"), Path.home(), ROOT, Path("/mnt"), Path("/tmp"), Path("/home")):
            with self.subTest(root=root), self.assertRaises(dm.UnsafeOutputDir):
                dm.check_output_dir(root / "docs-markdown-out", root)

    def test_refuses_scratch_roots_outside_target_and_temp(self) -> None:
        # Paths only: check_output_dir never touches the filesystem beyond
        # resolving, and none of these is created.
        for root in (Path("/etc/docs-markdown"), ROOT / "docs" / "scratch", Path.home() / "docs-markdown"):
            with self.subTest(root=root), self.assertRaises(dm.UnsafeOutputDir):
                dm.check_output_dir(root / "out", root)

    def test_refuses_the_scratch_root_itself_and_paths_outside_it(self) -> None:
        sibling = self.scratch.parent / "sibling"
        escape = self.scratch / "a" / ".." / ".."
        for out in (self.scratch, sibling, escape, self.scratch.parent):
            with self.subTest(out=out), self.assertRaises(dm.UnsafeOutputDir):
                dm.check_output_dir(out, self.scratch)

    def test_refuses_a_symlink_out_of_the_scratch_root(self) -> None:
        elsewhere = Path(self._tmp.name) / "elsewhere"
        elsewhere.mkdir()
        (elsewhere / "keep.txt").write_text("keep")
        link = self.scratch / "out"
        link.symlink_to(elsewhere, target_is_directory=True)
        with self.assertRaises(dm.UnsafeOutputDir):
            dm.prepare_output_dir(link, self.scratch)
        self.assertTrue((elsewhere / "keep.txt").exists())

    def test_refuses_an_unmarked_directory_and_keeps_its_files(self) -> None:
        out = self.scratch / "out"
        out.mkdir()
        (out / "keep.txt").write_text("keep")
        with self.assertRaises(dm.UnsafeOutputDir):
            dm.prepare_output_dir(out, self.scratch)
        self.assertEqual((out / "keep.txt").read_text(), "keep")

    def test_refuses_a_file(self) -> None:
        out = self.scratch / "out"
        out.write_text("not a directory")
        with self.assertRaises(dm.UnsafeOutputDir):
            dm.prepare_output_dir(out, self.scratch)
        self.assertTrue(out.is_file())

    def test_clears_its_own_output(self) -> None:
        out = self.scratch / "nested" / "out"
        first = dm.prepare_output_dir(out, self.scratch)
        self.assertTrue((first / dm.OUTPUT_MARKER).is_file())
        (first / "old.md").write_text("old")
        second = dm.prepare_output_dir(out, self.scratch)
        self.assertEqual(sorted(p.name for p in second.iterdir()), [dm.OUTPUT_MARKER])

    def test_accepts_an_empty_directory(self) -> None:
        out = self.scratch / "out"
        out.mkdir()
        self.assertEqual(dm.prepare_output_dir(out, self.scratch), out.resolve())

    def test_default_output_is_inside_the_default_scratch_root(self) -> None:
        self.assertIn(dm.DEFAULT_SCRATCH.resolve(), dm.DEFAULT_OUT.resolve().parents)
        self.assertIn(ROOT / "target", dm.DEFAULT_SCRATCH.resolve().parents)


class RustLinkTest(unittest.TestCase):
    """rustdoc intra-doc links become Markdown links."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="docs-markdown-rust-")
        self.rust = Path(self._tmp.name) / "rust"
        (self.rust / "demo" / "mux").mkdir(parents=True)
        (self.rust / "demo" / "index.md").write_text("# demo\n")
        (self.rust / "demo" / "mux.md").write_text(
            "# Module: mux\n\n## demo::mux::Mux\n\n*Struct*\n\n```rust\n## demo::not::AHeading\n```\n"
        )
        (self.rust / "demo" / "mux" / "frame.md").write_text("# Module: frame\n")
        self.index = dm.index_rust_headings(self.rust)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_indexes_items_and_modules(self) -> None:
        self.assertEqual(self.index.items["demo::mux::Mux"], (self.rust / "demo" / "mux.md", "demomuxmux"))
        self.assertNotIn("demo::not::AHeading", self.index.items)
        self.assertEqual(self.index.modules["demo"], self.rust / "demo" / "index.md")
        self.assertEqual(self.index.modules["demo::mux"], self.rust / "demo" / "mux.md")
        self.assertEqual(self.index.modules["demo::mux::frame"], self.rust / "demo" / "mux" / "frame.md")

    def test_slug_matches_github(self) -> None:
        self.assertEqual(dm.slug("demo::mux::Mux"), "demomuxmux")
        self.assertEqual(dm.slug("Module: `mux` & more"), "module-mux--more")

    def rewrite(self, text: str, targets: dict[str, tuple[str, str] | None]) -> tuple[str, int]:
        return dm.rewrite_shortcuts(text, self.rust / "demo" / "index.md", targets, self.index)

    def test_rewrites_shortcut_and_inline_links(self) -> None:
        targets = {
            "`Mux`": ("local", "demo::mux::Mux"),
            "crate::mux": ("local", "demo::mux"),
            "`Arc`": ("url", "https://doc.rust-lang.org/stable/std/sync/struct.Arc.html"),
        }
        text = "Use [`Mux`] in [the module](crate::mux), shared in an [`Arc`].\n"
        out, count = self.rewrite(text, targets)
        self.assertEqual(
            out,
            "Use [`Mux`](mux.md#demomuxmux) in [the module](mux.md), shared in an "
            "[`Arc`](https://doc.rust-lang.org/stable/std/sync/struct.Arc.html).\n",
        )
        self.assertEqual(count, 3)

    def test_unresolvable_links_lose_only_the_link(self) -> None:
        targets = {"`Self::open`": None, "Self::close": ("local", "demo::mux::Mux::close")}
        out, count = self.rewrite("Call [`Self::open`], then [close it](Self::close).\n", targets)
        self.assertEqual(out, "Call `Self::open`, then close it.\n")
        self.assertEqual(count, 2)

    def test_leaves_code_and_unknown_brackets_alone(self) -> None:
        targets = {"`Mux`": ("local", "demo::mux::Mux"), "dependencies": ("local", "demo::mux")}
        text = "```toml\n[dependencies]\n[`Mux`]\n```\nSee `[`Mux`]` and [features] and `&[T]`.\n"
        out, count = self.rewrite(text, targets)
        self.assertEqual(out, text)
        self.assertEqual(count, 0)

    def test_external_urls(self) -> None:
        self.assertEqual(
            dm.external_url(["zeroize", "ZeroizeOnDrop"], "trait", "zeroize"),
            "https://docs.rs/zeroize/latest/zeroize/trait.ZeroizeOnDrop.html",
        )
        self.assertEqual(
            dm.external_url(["std", "sync", "Arc"], "struct", "std"),
            "https://doc.rust-lang.org/stable/std/sync/struct.Arc.html",
        )
        self.assertEqual(
            dm.external_url(["tokio", "net"], "module", "tokio"), "https://docs.rs/tokio/latest/tokio/net/index.html"
        )
        self.assertIsNone(dm.external_url(["serde", "Serialize", "serialize"], "assoc_type", "serde"))

    def test_link_targets_from_rustdoc_json(self) -> None:
        data = {
            "index": {
                "1": {"links": {"`Mux`": 10, "`Arc`": 11, "`Gone`": 12}},
                "2": {"links": {"`Mux`": 10, "`Thing`": 13}},
                "3": {"links": {"`Thing`": 14}},
            },
            "paths": {
                "10": {"crate_id": 0, "path": ["demo", "mux", "Mux"], "kind": "struct"},
                "11": {"crate_id": 5, "path": ["alloc", "sync", "Arc"], "kind": "struct"},
                "13": {"crate_id": 0, "path": ["demo", "A"], "kind": "struct"},
                "14": {"crate_id": 0, "path": ["demo", "B"], "kind": "struct"},
            },
            "external_crates": {"5": {"name": "alloc"}},
        }
        path = Path(self._tmp.name) / "demo.json"
        path.write_text(json.dumps(data))
        targets = dm.rust_link_targets(path, {"demo"})
        self.assertEqual(targets["`Mux`"], ("local", "demo::mux::Mux"))
        self.assertEqual(targets["`Arc`"], ("url", "https://doc.rust-lang.org/stable/alloc/sync/struct.Arc.html"))
        self.assertIsNone(targets["`Gone`"])
        # One text naming two targets is ambiguous.
        self.assertIsNone(targets["`Thing`"])


class TreeTest(unittest.TestCase):
    """Docs copy, Python module list, TypeDoc media and link checks."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="docs-markdown-tree-")
        self.base = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_copy_docs_leaves_out_the_local_api_folder(self) -> None:
        src = self.base / "docs"
        (src / "api" / "rust").mkdir(parents=True)
        (src / "api" / "README.md").write_text("generated")
        (src / "adr").mkdir()
        (src / "README.md").write_text("# Docs")
        (src / "adr" / "one.md").write_text("# One")
        out = self.base / "out"
        out.mkdir()
        dm.copy_docs(src, out)
        self.assertEqual(sorted(p.relative_to(out).as_posix() for p in out.rglob("*") if p.is_file()), ["README.md", "adr/one.md"])

    def test_python_modules_skip_private_and_templates(self) -> None:
        pkg = self.base / "src" / "pkg"
        for rel in ("__init__.py", "__main__.py", "_abi.py", "abi.py", "sub/__init__.py", "sub/x.py", "a_template/t.py"):
            (pkg / rel).parent.mkdir(parents=True, exist_ok=True)
            (pkg / rel).write_text("")
        self.assertEqual(dm.python_modules(self.base / "src", "pkg"), ["pkg", "pkg.abi", "pkg.sub", "pkg.sub.x"])

    def test_typedoc_media_copies_link_back_to_the_docs(self) -> None:
        out = self.base / "out"
        project = out / "api" / "ui"
        (project / "_media").mkdir(parents=True)
        (out / "gui.md").write_text("# GUI\n\nSee [plugins](plugins.md).\n")
        (out / "plugins.md").write_text("# Plugins\n")
        (project / "_media" / "gui.md").write_text("# GUI\n\nSee [plugins](plugins.md).\n")
        (project / "_media" / "other.md").write_text("# Not a doc\n")
        (project / "README.md").write_text("See [the GUI](_media/gui.md#top) and [other](_media/other.md).\n")
        self.assertEqual(dm.link_media_to_docs(project, out), 1)
        self.assertEqual(
            (project / "README.md").read_text(), "See [the GUI](../../gui.md#top) and [other](_media/other.md).\n"
        )
        self.assertFalse((project / "_media" / "gui.md").exists())
        self.assertTrue((project / "_media" / "other.md").exists())

    def test_broken_links_in_the_tree_and_out_of_the_api_reference(self) -> None:
        out = self.base / "out"
        (out / "api" / "rust").mkdir(parents=True)
        (out / "README.md").write_text("[code](../crates/x/src/lib.rs) [ok](api/rust/a.md) [gone](missing.md)\n")
        (out / "api" / "rust" / "a.md").write_text(
            "[b](b.md) [web](https://example.com) [anchor](#x) [up](../../README.md)\n"
            "```\n[c](c.md)\n```\n`[d](d.md)`\n"
        )
        self.assertEqual(dm.broken_links(out), ["README.md: missing.md", "api/rust/a.md: b.md"])


if __name__ == "__main__":
    os.environ.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    unittest.main()
