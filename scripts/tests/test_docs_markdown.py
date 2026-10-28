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


class IndexTest(unittest.TestCase):
    """The indexes put the plugin SDKs first, then the web API, then the crates."""

    def test_groups(self) -> None:
        self.assertEqual(dm.rust_group("bookclerk_plugin_abi", "crates/bookclerk-plugin-abi"), "Plugin interface")
        self.assertEqual(dm.rust_group("bookclerk_dev", "crates/bookclerk-dev"), "Development and testing")
        self.assertEqual(
            dm.rust_group("bookclerk_plugin_source_libro", "crates/bookclerk-plugins/optional/source-libro"), "Plugins"
        )
        self.assertEqual(dm.rust_group("echo", "examples/plugins-echo-workerd-rust"), "Development and testing")
        self.assertEqual(dm.rust_group("bookclerk_media", "crates/bookclerk-media"), "Host internals")

    def test_index_lists_groups_in_order(self) -> None:
        with tempfile.TemporaryDirectory(prefix="docs-markdown-rust-") as tmp:
            out = Path(tmp)
            crates = [
                dm.RustCrate("bookclerk_media", "bookclerk-media", "Clear media | remux", "Host internals"),
                dm.RustCrate("bookclerk_plugin_abi", "bookclerk-plugin-abi", "ABI", "Plugin interface"),
                dm.RustCrate("bookclerk_plugin_sdk", "bookclerk-plugin-sdk", "Guest SDK", "Plugin interface"),
            ]
            dm.write_rust_index(out, crates)
            text = (out / "README.md").read_text()
        self.assertLess(text.index("## Plugin interface"), text.index("## Host internals"))
        # The SDK is listed with the other SDKs, not among the crates.
        self.assertNotIn("| [`bookclerk_plugin_sdk`]", text)
        self.assertNotIn("## Plugins", text)
        self.assertIn("| [`bookclerk_media`](bookclerk_media/index.md) | Clear media \\| remux |", text)

    def test_api_index_order_and_missing_references(self) -> None:
        with tempfile.TemporaryDirectory(prefix="docs-markdown-api-") as tmp:
            api = Path(tmp)
            for rel in ("typescript/README.md", "python/README.md", "rust/README.md", "rust/bookclerk_plugin_sdk/index.md", "ui/README.md"):
                (api / rel).parent.mkdir(parents=True, exist_ok=True)
                (api / rel).write_text("# x\n")
            dm.write_api_index(api)
            text = (api / "README.md").read_text()
        order = [text.index(s) for s in ("## Plugin SDKs", "typescript/README.md", "python/README.md", "rust/bookclerk_plugin_sdk/index.md", "## Rust crates", "## Web UI")]
        self.assertEqual(order, sorted(order))
        # No web API reference was built, so there is no section for it.
        self.assertNotIn("## Web API", text)


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
            "\n".join(["```markdown", "```not-a-closing-fence", "<!-- markdownlint-disable -->", "```", "Text."]),
        )

    def test_strip_lint_comments_matches_code_span_runs_by_length(self) -> None:
        line = "Use `` ` <!-- markdownlint-disable MD038 --> `` here. <!-- markdownlint-disable-line MD001 -->"
        self.assertEqual(dm.strip_lint_comments(line), "Use `` ` <!-- markdownlint-disable MD038 --> `` here.")

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

    def test_typedoc_module_list_moves_to_the_readme(self) -> None:
        project = self.base / "out" / "api" / "typescript"
        project.mkdir(parents=True)
        (project / "README.md").write_text("# SDK\n\nIntro.\n")
        (project / "modules.md").write_text("# SDK\n\n## Modules\n\n- [cli](cli.md)\n- [workerd](workerd.md)\n")
        (project / "cli.md").write_text("# cli\n\nBack to [the SDK](modules.md).\n")
        (project / "a" / "b").mkdir(parents=True)
        (project / "a" / "b" / "deep.md").write_text("[up](../../modules.md#cli) [other](../modules.md) [keep](other/modules.md)\n")
        dm.fold_module_index(project)
        self.assertFalse((project / "modules.md").exists())
        self.assertEqual(
            (project / "README.md").read_text(), "# SDK\n\nIntro.\n\n## Modules\n\n- [cli](cli.md)\n- [workerd](workerd.md)\n"
        )
        self.assertIn("(README.md)", (project / "cli.md").read_text())
        # Any depth, with or without an anchor; other files named modules.md stay.
        self.assertEqual(
            (project / "a" / "b" / "deep.md").read_text(), "[up](../../README.md#modules) [other](../modules.md) [keep](other/modules.md)\n"
        )

    def test_broken_links_in_the_tree_and_out_of_the_api_reference(self) -> None:
        out = self.base / "out"
        (out / "api" / "rust").mkdir(parents=True)
        (out / "README.md").write_text("[code](../crates/x/src/lib.rs) [ok](api/rust/a.md) [gone](missing.md)\n")
        (out / "api" / "rust" / "a.md").write_text(
            "[b](b.md) [web](https://example.com) [anchor](#x) [up](../../README.md)\n"
            "```\n[c](c.md)\n```\n`[d](d.md)`\n"
        )
        self.assertEqual(dm.broken_links(out), ["README.md: missing.md", "api/rust/a.md: b.md"])


# Unix times of 2025-10-01 and 2025-10-02, 12:00 UTC.
DAY1 = 1_759_320_000
DAY2 = DAY1 + 86_400


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
        self.assertEqual(dm.page_dates(log, {"docs/a.md", "docs/b.md"}), {"docs/a.md": "2025-10-02", "docs/b.md": "2025-10-01"})

    def test_a_pure_move_is_followed_not_counted(self) -> None:
        log = self.log((DAY2, ["R100\tdocs/old.md\tdocs/new.md"]), (DAY1, ["A\tdocs/old.md"]))
        self.assertEqual(dm.page_dates(log, {"docs/new.md"}), {"docs/new.md": "2025-10-01"})

    def test_a_move_with_edits_counts_and_is_followed(self) -> None:
        log = self.log(
            (DAY2, ["R087\tdocs/old.md\tdocs/new.md"]),
            (DAY1 + 3600, ["M\tdocs/old.md"]),
            (DAY1, ["A\tdocs/old.md"]),
        )
        self.assertEqual(dm.page_dates(log, {"docs/new.md"}), {"docs/new.md": "2025-10-02"})

    def test_a_readded_path_ignores_the_deleted_file(self) -> None:
        log = self.log(
            (DAY1 + 3600, ["A\tdocs/a.md"]),
            (DAY1 + 1800, ["D\tdocs/a.md"]),
            (DAY2 + 86_400, ["M\tdocs/a.md"]),  # out of order, and of the old file
        )
        self.assertEqual(dm.page_dates(log, {"docs/a.md"}), {"docs/a.md": "2025-10-01"})

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
        subprocess.run(["git", "-c", "commit.gpgsign=false", *args], cwd=cwd or self.repo, env=env, check=True, capture_output=True)

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
        (out / "api").mkdir()
        (out / "api" / "README.md").write_text("# API\n")
        self.assertEqual(dm.write_last_updated(out, dm.git_docs_log(self.repo)), 2)
        document = json.loads((out / dm.LAST_UPDATED_FILE).read_text())
        self.assertEqual(document, {"version": 1, "pages": {"README": "2025-10-02", "guide/moved": "2025-10-01"}})

    def test_refuses_a_shallow_or_missing_history(self) -> None:
        for when in (DAY1, DAY2):
            (self.repo / "docs" / "README.md").write_text(f"# Docs {when}\n")
            self.commit(when)
        shallow = Path(self._tmp.name) / "shallow"
        self.git("clone", "-q", "--depth", "1", f"file://{self.repo}", str(shallow), cwd=Path(self._tmp.name))
        with self.assertRaises(dm.NoHistory):
            dm.git_docs_log(shallow)
        with self.assertRaises(dm.NoHistory):
            dm.git_docs_log(Path(self._tmp.name))


if __name__ == "__main__":
    os.environ.setdefault("PYTHONDONTWRITEBYTECODE", "1")
    unittest.main()
