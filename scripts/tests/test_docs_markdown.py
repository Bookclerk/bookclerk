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
import subprocess
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
        for out in (
            Path("/"),
            Path.home(),
            ROOT,
            Path("/mnt"),
            Path("/mnt/project-files"),
            Path("/tmp"),
        ):
            with self.subTest(out=out), self.assertRaises(dm.UnsafeOutputDir):
                dm.check_output_dir(out, self.scratch)

    def test_refuses_dangerous_scratch_roots(self) -> None:
        for root in (
            Path("/"),
            Path.home(),
            ROOT,
            Path("/mnt"),
            Path("/tmp"),
            Path("/home"),
        ):
            with self.subTest(root=root), self.assertRaises(dm.UnsafeOutputDir):
                dm.check_output_dir(root / "docs-markdown-out", root)

    def test_refuses_scratch_roots_outside_target_and_temp(self) -> None:
        # Paths only: check_output_dir never touches the filesystem beyond
        # resolving, and none of these is created.
        for root in (
            Path("/etc/docs-markdown"),
            ROOT / "docs" / "scratch",
            Path.home() / "docs-markdown",
        ):
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


class IndexTest(unittest.TestCase):
    """The crates page groups the crates; each reference has a shelf folder."""

    def test_groups(self) -> None:
        self.assertEqual(
            dm.rust_group("bookclerk_plugin_abi", "crates/bookclerk-plugin-abi"),
            "Plugin interface",
        )
        self.assertEqual(
            dm.rust_group("bookclerk_dev", "crates/bookclerk-dev"),
            "Development and testing",
        )
        self.assertEqual(
            dm.rust_group(
                "bookclerk_plugin_source_libro",
                "crates/bookclerk-plugins/optional/source-libro",
            ),
            "Plugins",
        )
        self.assertEqual(
            dm.rust_group("echo", "examples/plugins-echo-workerd-rust"),
            "Development and testing",
        )
        self.assertEqual(dm.rust_group("bookclerk_media", "crates/bookclerk-media"), "Host internals")

    def test_crates_page_lists_groups_in_order(self) -> None:
        crates = [
            dm.RustCrate(
                "bookclerk_media",
                "bookclerk-media",
                "Clear media | remux",
                "Host internals",
            ),
            dm.RustCrate(
                "bookclerk_plugin_abi",
                "bookclerk-plugin-abi",
                "ABI",
                "Plugin interface",
            ),
            dm.RustCrate(
                "bookclerk_plugin_sdk",
                "bookclerk-plugin-sdk",
                "Guest SDK",
                "Plugin interface",
            ),
        ]
        text = dm.rust_index(crates, dm.reference_of("rust-crates"), dm.reference_of("rust-sdk"))
        self.assertLess(text.index("## Plugin interface"), text.index("## Host internals"))
        # The SDK sits on the plugins shelf, not among the crates.
        self.assertNotIn("| [`bookclerk_plugin_sdk`]", text)
        self.assertIn("[Rust plugin SDK](../../plugins/sdk/rust/README.md)", text)
        self.assertNotIn("## Plugins", text)
        self.assertIn(
            "| [`bookclerk_media`](bookclerk_media/README.md) | Clear media \\| remux |",
            text,
        )

    def test_references_land_on_shelves(self) -> None:
        folders = [r.folder for r in dm.REFERENCES]
        self.assertEqual(len(folders), len(set(folders)))
        for reference in dm.REFERENCES:
            self.assertNotIn("index", reference.folder.split("/"))
        self.assertEqual(dm.reference_of("web-api").folder, "server/api")
        self.assertTrue(dm.reference_of("python").landing)
        self.assertFalse(dm.reference_of("web-api").landing)


class TreeTest(unittest.TestCase):
    """Docs copy, Python module list, TypeDoc media and link checks."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="docs-markdown-tree-")
        self.base = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_copy_docs_copies_every_page(self) -> None:
        src = self.base / "docs"
        (src / "adr").mkdir(parents=True)
        (src / "README.md").write_text("# Docs")
        (src / "adr" / "one.md").write_text("# One")
        out = self.base / "out"
        out.mkdir()
        dm.copy_docs(src, out)
        self.assertEqual(
            sorted(p.relative_to(out).as_posix() for p in out.rglob("*") if p.is_file()),
            ["README.md", "adr/one.md"],
        )

    def test_copy_docs_strips_markdownlint_overrides_from_pages_only(self) -> None:
        src = self.base / "docs"
        src.mkdir()
        page = "# Title <!-- markdownlint-disable-line MD026 -->\n\nText. <!-- markdownlint-disable-line MD038 -->\n"
        (src / "page.md").write_text(page)
        (src / "data.json").write_text('{"note": "<!-- markdownlint-disable -->"}')
        out = self.base / "out"
        out.mkdir()
        dm.copy_docs(src, out)
        self.assertEqual((out / "page.md").read_text(), "# Title\n\nText.\n")
        self.assertEqual((out / "data.json").read_text(), '{"note": "<!-- markdownlint-disable -->"}')

    def test_strip_lint_comments_keeps_code_and_other_comments(self) -> None:
        source = "\n".join(
            [
                "Intro.",
                "",
                "<!-- markdownlint-disable-next-line MD001 -->",
                "",
                "### Deep",
                "",
                "Use `<!-- markdownlint-disable-line MDxxx -->` on the line.",
                "<!-- a note for editors -->",
                "",
                "```markdown",
                "Text. <!-- markdownlint-disable-line MD038 -->",
                "```",
                "",
                "~~~~",
                "```",
                "<!-- markdownlint-disable -->",
                "~~~~",
                "",
            ]
        )
        self.assertEqual(
            dm.strip_lint_comments(source),
            "\n".join(
                [
                    "Intro.",
                    "",
                    "### Deep",
                    "",
                    "Use `<!-- markdownlint-disable-line MDxxx -->` on the line.",
                    "<!-- a note for editors -->",
                    "",
                    "```markdown",
                    "Text. <!-- markdownlint-disable-line MD038 -->",
                    "```",
                    "",
                    "~~~~",
                    "```",
                    "<!-- markdownlint-disable -->",
                    "~~~~",
                    "",
                ]
            ),
        )

    def test_strip_lint_comments_needs_a_bare_closing_fence(self) -> None:
        source = "\n".join(
            [
                "```markdown",
                "```not-a-closing-fence",
                "<!-- markdownlint-disable -->",
                "```",
                "Text. <!-- markdownlint-disable-line MD001 -->",
            ]
        )
        self.assertEqual(
            dm.strip_lint_comments(source),
            "\n".join(
                [
                    "```markdown",
                    "```not-a-closing-fence",
                    "<!-- markdownlint-disable -->",
                    "```",
                    "Text.",
                ]
            ),
        )

    def test_strip_lint_comments_matches_code_span_runs_by_length(self) -> None:
        line = "Use `` ` <!-- markdownlint-disable MD038 --> `` here. <!-- markdownlint-disable-line MD001 -->"
        self.assertEqual(
            dm.strip_lint_comments(line),
            "Use `` ` <!-- markdownlint-disable MD038 --> `` here.",
        )

    def test_strip_lint_comments_keeps_code_spans_across_lines(self) -> None:
        source = "Use `a\n<!-- markdownlint-disable -->\nb` here.\n\n`x\n\n<!-- markdownlint-disable -->\n"
        self.assertEqual(
            dm.strip_lint_comments(source),
            "Use `a\n<!-- markdownlint-disable -->\nb` here.\n\n`x\n",
        )

    def test_strip_lint_comments_ignores_backtick_fences_with_backticks_in_the_info_string(
        self,
    ) -> None:
        source = "```foo`bar\nText. <!-- markdownlint-disable-line MD001 -->\n"
        self.assertEqual(dm.strip_lint_comments(source), "```foo`bar\nText.\n")

    def test_strip_lint_comments_keeps_block_quoted_fences(self) -> None:
        source = "\n".join(
            [
                "> ```markdown",
                "> <!-- markdownlint-disable -->",
                "> ```",
                "",
                "> ~~~",
                "> <!-- markdownlint-disable -->",
                "Text. <!-- markdownlint-disable-line MD001 -->",
            ]
        )
        self.assertEqual(
            dm.strip_lint_comments(source),
            "\n".join(
                [
                    "> ```markdown",
                    "> <!-- markdownlint-disable -->",
                    "> ```",
                    "",
                    "> ~~~",
                    "> <!-- markdownlint-disable -->",
                    "Text.",
                ]
            ),
        )

    def test_strip_lint_comments_keeps_line_breaks(self) -> None:
        source = "Text.\n<!--\nmarkdownlint-disable MD013 -->\nMore. <!-- markdownlint-disable-line MD001 -->\n"
        self.assertEqual(
            dm.strip_lint_comments(source),
            "Text.\n<!--\nmarkdownlint-disable MD013 -->\nMore.\n",
        )

    def test_strip_lint_comments_keeps_list_item_fences(self) -> None:
        source = "- ```markdown\n  <!-- markdownlint-disable MD013 -->\n  ```\n\nText. <!-- markdownlint-disable-line MD001 -->\n"
        self.assertEqual(
            dm.strip_lint_comments(source),
            "- ```markdown\n  <!-- markdownlint-disable MD013 -->\n  ```\n\nText.\n",
        )

    def test_strip_lint_comments_keeps_container_markers_inside_fences(self) -> None:
        source = "````markdown\n- ````\n> ````\n<!-- markdownlint-disable -->\n````\nText. <!-- markdownlint-disable-line MD001 -->\n"
        self.assertEqual(
            dm.strip_lint_comments(source),
            "````markdown\n- ````\n> ````\n<!-- markdownlint-disable -->\n````\nText.\n",
        )

    def test_strip_lint_comments_closes_fences_only_within_three_spaces(self) -> None:
        source = "```markdown\n    ```\n<!-- markdownlint-disable MD013 -->\n```\n- ```\n  <!-- markdownlint-disable -->\n      ```\n  ```\nText. <!-- markdownlint-disable-line MD001 -->\n"
        self.assertEqual(
            dm.strip_lint_comments(source),
            "```markdown\n    ```\n<!-- markdownlint-disable MD013 -->\n```\n- ```\n  <!-- markdownlint-disable -->\n      ```\n  ```\nText.\n",
        )

    def test_python_modules_skip_private_and_templates(self) -> None:
        pkg = self.base / "src" / "pkg"
        for rel in (
            "__init__.py",
            "__main__.py",
            "_abi.py",
            "abi.py",
            "sub/__init__.py",
            "sub/x.py",
            "a_template/t.py",
        ):
            (pkg / rel).parent.mkdir(parents=True, exist_ok=True)
            (pkg / rel).write_text("")
        self.assertEqual(
            dm.python_modules(self.base / "src", "pkg"),
            ["pkg", "pkg.abi", "pkg.sub", "pkg.sub.x"],
        )

    def test_a_project_without_media_copies_deletes_nothing(self) -> None:
        out = self.base / "out"
        project = out / "contributing" / "ui"
        project.mkdir(parents=True)
        (project / "README.md").write_text("# UI\n")
        self.assertEqual(dm.link_media_to_docs(project, out, set()), [])
        self.assertEqual((project / "README.md").read_text(), "# UI\n")

    def test_typedoc_media_copies_link_back_to_the_docs(self) -> None:
        out = self.base / "out"
        project = out / "contributing" / "ui"
        (project / "_media").mkdir(parents=True)
        (out / "gui.md").write_text("# GUI\n\nSee [plugins](plugins.md).\n")
        (out / "plugins.md").write_text("# Plugins\n")
        (project / "_media" / "gui.md").write_text("# GUI\n\nSee [plugins](plugins.md).\n")
        (project / "_media" / "other.md").write_text("# Not a doc\n")
        (project / "README.md").write_text("See [the GUI](_media/gui.md#top) and [other](_media/other.md).\n")
        self.assertEqual(dm.link_media_to_docs(project, out, set()), ["_media/gui.md"])
        self.assertEqual(
            (project / "README.md").read_text(),
            "See [the GUI](../../gui.md#top) and [other](_media/other.md).\n",
        )
        self.assertFalse((project / "_media" / "gui.md").exists())
        self.assertTrue((project / "_media" / "other.md").exists())

    def test_typedoc_pages_join_the_landing_page(self) -> None:
        out = self.base / "out"
        reference = dm.reference_of("typescript")
        landing = out / reference.folder / "README.md"
        landing.parent.mkdir(parents=True)
        landing.write_text("# TypeScript SDK\n\nThe guide.\n")
        scratch = self.base / "scratch"
        scratch.mkdir()
        (scratch / "README.md").write_text(
            "# @bookclerk/plugin-sdk\n\n## Modules\n\n- [cli](cli.md)\n- [plugin-sdk](plugin-sdk.md)\n- [workerd](workerd.md)\n"
        )
        (scratch / "plugin-sdk.md").write_text(
            "# plugin-sdk\n\nThe root.\n\n## Classes\n\n### PluginError\n\n## References\n\n- [cliArgs](workerd.md#cliargs)\n"
        )
        (scratch / "cli.md").write_text("# cli\n")
        (scratch / "workerd.md").write_text(
            "# workerd\n\nThrows [PluginError](plugin-sdk.md#pluginerror); see [plugin-sdk](./plugin-sdk.md) and `plugin-sdk.md`.\n"
        )
        (scratch / "_media").mkdir()
        (scratch / "_media" / "logo.png").write_bytes(b"\x89PNG")
        (scratch / "_media" / "guide.md").write_text("# A copied doc\n")
        tree = dm.Tree(out)
        self.assertEqual(dm.place_typedoc(scratch, tree, reference), ["_media/guide", "cli", "workerd"])
        # An image is written beside the pages but is not one; a deleted copy leaves the manifest.
        self.assertTrue((out / reference.folder / "_media" / "logo.png").is_file())
        self.assertNotIn(f"{reference.folder}/_media/logo", tree.keys)
        tree.forget(reference, "_media/guide.md")
        self.assertNotIn(f"{reference.folder}/_media/guide", tree.keys)
        self.assertIn(f"{reference.folder}/cli", tree.keys)
        # Links to the root module's page now point at the landing page.
        self.assertEqual(
            (out / reference.folder / "workerd.md").read_text(),
            "# workerd\n\nThrows [PluginError](README.md#pluginerror); see [plugin-sdk](README.md) and `plugin-sdk.md`.\n",
        )
        self.assertEqual(
            landing.read_text(),
            "# TypeScript SDK\n\nThe guide.\n\n## Modules\n\nThe root.\n\n- [cli](cli.md)\n- [workerd](workerd.md)\n\n## Classes\n\n### PluginError\n\n## References\n\n- [cliArgs](workerd.md#cliargs)\n",
        )
        self.assertEqual(
            tree.pages,
            {
                reference.folder: [
                    f"{reference.folder}/cli",
                    f"{reference.folder}/workerd",
                ]
            },
        )
        self.assertEqual(tree.built, [reference])
        self.assertFalse((scratch / "README.md").exists())

    def test_a_landing_page_must_be_hand_written(self) -> None:
        tree = dm.Tree(self.base)
        with self.assertRaisesRegex(dm.BuildError, "docs/plugins/sdk/python/README.md is missing"):
            tree.landing(dm.reference_of("python"), "## Modules\n")

    def test_a_generated_page_never_overwrites_a_page(self) -> None:
        out = self.base / "out"
        reference = dm.reference_of("rust-sdk")
        (out / reference.folder).mkdir(parents=True)
        (out / reference.folder / "net.md").write_text("# Hand-written\n")
        tree = dm.Tree(out)
        tree.place(reference, "http.md", "# http\n")
        with self.assertRaisesRegex(dm.BuildError, "plugins/sdk/rust/net.md: a page is already there"):
            tree.place(reference, "net.md", "# net\n")
        with self.assertRaisesRegex(dm.BuildError, "plugins/sdk/rust/http.md: a page is already there"):
            tree.place(reference, "http.md", "# again\n")
        self.assertEqual((out / reference.folder / "net.md").read_text(), "# Hand-written\n")

    def test_generated_manifest_lists_references_and_crate_sources(self) -> None:
        out = self.base / "out"
        out.mkdir()
        tree = dm.Tree(out)
        crates = dm.reference_of("rust-crates")
        tree.place(
            crates,
            "bookclerk_media/README.md",
            "# bookclerk_media\n",
            "crates/bookclerk-media",
        )
        tree.place(crates, "bookclerk_media/mp3.md", "# mp3\n")
        tree.place(crates, "README.md", "# Rust crates\n")
        dm.write_generated(out, tree)
        document = json.loads((out / dm.GENERATED_FILE).read_text())
        self.assertEqual(
            document,
            {
                "version": 1,
                "references": [
                    {
                        "folder": "internals/crates",
                        "kind": "rust-crates",
                        "title": "Rust crates",
                        "source": "Cargo.toml",
                        "landing": False,
                        "pages": [
                            "internals/crates/bookclerk_media/README",
                            "internals/crates/bookclerk_media/mp3",
                            "internals/crates/README",
                        ],
                    }
                ],
                "sources": {"internals/crates/bookclerk_media": "crates/bookclerk-media"},
            },
        )
        self.assertEqual(
            tree.keys,
            {
                "internals/crates/bookclerk_media/README",
                "internals/crates/bookclerk_media/mp3",
                "internals/crates/README",
            },
        )

    def test_python_package_page_joins_the_landing_page(self) -> None:
        src = self.base / "src" / "bookclerk_plugin_sdk"
        src.mkdir(parents=True)
        (src / "__init__.py").write_text('"""The package.\n\nSee :mod:`bookclerk_plugin_sdk.workerd`."""\n\nVERSION = "3"\n"""ABI version."""\n')
        (src / "workerd.py").write_text('"""Workerd runtime."""\n\n\ndef run() -> None:\n    """Run."""\n')
        out = self.base / "out"
        reference = dm.reference_of("python")
        (out / reference.folder).mkdir(parents=True)
        (out / reference.folder / "README.md").write_text("# Python SDK\n\nThe guide.\n")
        tree = dm.Tree(out)
        self.assertEqual(
            dm.build_python(self.base / "src", tree, reference),
            ["bookclerk_plugin_sdk", "bookclerk_plugin_sdk.workerd"],
        )
        landing = (out / reference.folder / "README.md").read_text()
        self.assertTrue(
            landing.startswith(
                "# Python SDK\n\nThe guide.\n\n## Modules\n\n| Module | Summary |\n| --- | --- |\n| [`bookclerk_plugin_sdk.workerd`](workerd.md) | Workerd runtime. |\n\nThe package.\n\nSee [`bookclerk_plugin_sdk.workerd`](workerd.md)."
            ),
            landing,
        )
        self.assertNotIn("\n# ", landing)
        self.assertIn("### VERSION", landing)
        self.assertEqual(tree.pages, {reference.folder: [f"{reference.folder}/workerd"]})

    def test_sibling_pages_are_found(self) -> None:
        out = self.base / "out"
        for rel in (
            "plugins/sdk/rust/http.md",
            "plugins/sdk/rust/http/client.md",
            "plugins/sdk/rust/net.md",
            "plugins/README.md",
        ):
            (out / rel).parent.mkdir(parents=True, exist_ok=True)
            (out / rel).write_text("# x\n")
        self.assertEqual(dm.sibling_pages(out), ["plugins/sdk/rust/http.md"])

    def test_broken_links_in_the_tree_and_out_of_the_api_reference(self) -> None:
        out = self.base / "out"
        (out / "internals" / "crates").mkdir(parents=True)
        (out / "README.md").write_text("[code](../crates/x/src/lib.rs) [ok](internals/crates/a.md) [gone](missing.md)\n")
        (out / "internals" / "crates" / "a.md").write_text(
            "[b](b.md) [web](https://example.com) [anchor](#x) [up](../../README.md) [out](../../../Cargo.toml)\n```\n[c](c.md)\n```\n`[d](d.md)`\n"
        )
        self.assertEqual(
            dm.broken_links(out, {"internals/crates/a"}),
            [
                "README.md: missing.md",
                "internals/crates/a.md: b.md",
                "internals/crates/a.md: ../../../Cargo.toml",
            ],
        )
        # A hand-written page may link out of the tree.
        self.assertEqual(
            dm.broken_links(out),
            ["README.md: missing.md", "internals/crates/a.md: b.md"],
        )

    def test_index_pages_are_found_anywhere_in_the_tree(self) -> None:
        out = self.base / "out"
        for rel in (
            "README.md",
            "internals/crates/a/README.md",
            "internals/crates/b/index.md",
            "plugins/sdk/typescript/index.md",
            "plugins/sdk/typescript/index/README.md",
            "contributing/ui/indexes.md",
            "contributing/ui/index.txt",
        ):
            (out / rel).parent.mkdir(parents=True, exist_ok=True)
            (out / rel).write_text("# x\n")
        self.assertEqual(
            dm.index_pages(out),
            [
                "internals/crates/b/index.md",
                "plugins/sdk/typescript/index.md",
                "plugins/sdk/typescript/index/README.md",
            ],
        )

    def test_relative_file_links_in_rust_doc_comments(self) -> None:
        src = self.base / "crates" / "x" / "src"
        (src / "bin").mkdir(parents=True)
        (src / "lib.rs").write_text(
            "//! [adr](../../../docs/adr/a.md) [item](crate::a::B) [web](https://example.com/x.md)\n"
            "//! [site](/docs/x) [anchor](#x) `[code](c.md)` [readme](README.md#top)\n"
            "//! ```text\n//! [fenced](f.md)\n//! ```\n"
            "//! ````text\n//! ```\n//! [nested](n.md)\n//! ````\n"
            "//! [angle](<https://example.com/x.md>) [rel](<../y.md>)\n"
            "/// [def]: ../other.rs\n"
            "//// [not a doc comment](n.md)\n"
            "// [plain comment](p.md)\n"
        )
        (src / "main.rs").write_text("//! [binary](../README.md)\n")
        (src / "bin" / "tool.rs").write_text("//! [binary](../../README.md)\n")
        self.assertEqual(
            dm.source_doc_links([src], self.base),
            [
                "crates/x/src/lib.rs:1: ../../../docs/adr/a.md",
                "crates/x/src/lib.rs:2: README.md#top",
                "crates/x/src/lib.rs:10: ../y.md",
                "crates/x/src/lib.rs:11: ../other.rs",
            ],
        )


class WorkspaceDocCommentTest(unittest.TestCase):
    """The real workspace: no Rust library doc comment links a file relatively.

    docs-publish (main only) fails on such a link; this runs on every pull
    request, before the change lands.
    """

    def test_workspace_libraries_have_no_relative_file_links(self) -> None:
        self.assertEqual(dm.source_doc_links(dm.library_source_dirs()), [])


# Unix times of 2025-10-01 and 2025-10-02, 12:00 UTC.
DAY1 = 1_759_320_000
DAY2 = DAY1 + 86_400
# The same instants as last-updated.json writes them.
T1 = "2025-10-01T12:00:00Z"
T2 = "2025-10-02T12:00:00Z"


class PageDatesTest(unittest.TestCase):
    """Dating pages from the history of docs/."""

    def log(self, *commits: tuple[int, list[str]]) -> str:
        """Build ``git log`` output: (time, status lines), newest first."""
        out = []
        for when, lines in commits:
            out.append(f"{dm.COMMIT_MARK}{when}\n")
            out.append("\n" + "".join(f"{line}\n" for line in lines))
        return "".join(out)

    def test_newest_content_change_wins(self) -> None:
        log = self.log((DAY2, ["M\tdocs/a.md"]), (DAY1, ["A\tdocs/a.md", "A\tdocs/b.md"]))
        self.assertEqual(
            dm.page_dates(log, {"docs/a.md", "docs/b.md"}),
            {"docs/a.md": T2, "docs/b.md": T1},
        )

    def test_a_pure_move_is_followed_not_counted(self) -> None:
        log = self.log((DAY2, ["R100\tdocs/old.md\tdocs/new.md"]), (DAY1, ["A\tdocs/old.md"]))
        self.assertEqual(dm.page_dates(log, {"docs/new.md"}), {"docs/new.md": T1})

    def test_a_move_with_edits_counts_and_is_followed(self) -> None:
        log = self.log(
            (DAY2, ["R087\tdocs/old.md\tdocs/new.md"]),
            (DAY1 + 3600, ["M\tdocs/old.md"]),
            (DAY1, ["A\tdocs/old.md"]),
        )
        self.assertEqual(dm.page_dates(log, {"docs/new.md"}), {"docs/new.md": T2})

    def test_a_readded_path_ignores_the_deleted_file(self) -> None:
        log = self.log(
            (DAY1 + 3600, ["A\tdocs/a.md"]),
            (DAY1 + 1800, ["D\tdocs/a.md"]),
            (DAY2 + 86_400, ["M\tdocs/a.md"]),  # out of order, and of the old file
        )
        self.assertEqual(dm.page_dates(log, {"docs/a.md"}), {"docs/a.md": "2025-10-01T13:00:00Z"})

    def test_other_paths_and_undated_pages_are_left_out(self) -> None:
        log = self.log((DAY1, ["M\tdocs/other.md", "R100\tdocs/x.md\tdocs/y.md"]))
        self.assertEqual(dm.page_dates(log, {"docs/a.md"}), {})

    def test_rejects_lines_git_does_not_write(self) -> None:
        for log in (
            "M\tdocs/a.md\n",
            f"{dm.COMMIT_MARK}soon\n",
            self.log((DAY1, ["X\tdocs/a.md"])),
            self.log((DAY1, ["M\ta\tb"])),
            self.log((DAY1, ["C100\tdocs/a.md\tdocs/b.md"])),
            self.log((DAY1, ['M\t"docs/tab\\there.md"'])),
        ):
            with self.subTest(log=log), self.assertRaises(dm.BuildError):
                dm.page_dates(log, {"docs/a.md"})


class GitHistoryTest(unittest.TestCase):
    """Reading the history of a real repository."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="docs-markdown-git-")
        self.repo = Path(self._tmp.name) / "repo"
        (self.repo / "docs" / "guide").mkdir(parents=True)
        self.git("init", "-q", "-b", "main")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def git(self, *args: str, when: int = DAY1, cwd: Path | None = None) -> None:
        """Run git with fixed identities and dates."""
        env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.invalid",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.invalid",
            "GIT_AUTHOR_DATE": f"@{when} +0000",
            # The committer date is what a history rewrite changes; pages
            # are dated by the author date.
            "GIT_COMMITTER_DATE": f"@{DAY2 + 86_400 * 30} +0000",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
        }
        subprocess.run(
            ["git", "-c", "commit.gpgsign=false", *args],
            cwd=cwd or self.repo,
            env=env,
            check=True,
            capture_output=True,
        )

    def commit(self, when: int) -> None:
        """Commit everything with author date ``when``."""
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "change", when=when)

    def test_writes_dates_for_copied_pages(self) -> None:
        body = "\n".join(f"Line {i} of a page long enough for rename detection." for i in range(40))
        (self.repo / "docs" / "moved.md").write_text(f"# Moved\n\n{body}\n")
        (self.repo / "docs" / "README.md").write_text("# Docs\n")
        self.commit(DAY1)
        self.git("mv", "docs/moved.md", "docs/guide/moved.md")
        (self.repo / "docs" / "README.md").write_text("# Docs\n\nMore.\n")
        self.commit(DAY2)

        out = Path(self._tmp.name) / "out"
        dm.copy_docs(self.repo / "docs", out)
        (out / "server" / "api").mkdir(parents=True)
        (out / "server" / "api" / "README.md").write_text("# API\n")
        self.assertEqual(
            dm.write_last_updated(out, dm.git_docs_log(self.repo), {"server/api/README"}),
            2,
        )
        document = json.loads((out / dm.LAST_UPDATED_FILE).read_text())
        self.assertEqual(document, {"version": 1, "pages": {"README": T2, "guide/moved": T1}})

    def test_refuses_a_shallow_or_missing_history(self) -> None:
        for when in (DAY1, DAY2):
            (self.repo / "docs" / "README.md").write_text(f"# Docs {when}\n")
            self.commit(when)
        shallow = Path(self._tmp.name) / "shallow"
        self.git(
            "clone",
            "-q",
            "--depth",
            "1",
            f"file://{self.repo}",
            str(shallow),
            cwd=Path(self._tmp.name),
        )
        with self.assertRaises(dm.NoHistory):
            dm.git_docs_log(shallow)
        with self.assertRaises(dm.NoHistory):
            dm.git_docs_log(Path(self._tmp.name))


if __name__ == "__main__":
    os.environ.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    unittest.main()
