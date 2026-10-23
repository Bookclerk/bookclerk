#!/usr/bin/env python3
"""Build the Markdown docs tree published to the docs repository.

The tree holds ``docs/`` as it is, plus the generated API reference converted
to Markdown under ``api/``:

- ``api/rust/``: rustdoc JSON (a pinned nightly) converted by ``cargo-doc-md``,
  with rustdoc's intra-doc links (``[`PluginWorker`]``), which the converter
  leaves as plain text, turned into links;
- ``api/typescript/`` and ``api/ui/``: each project's own TypeDoc with
  ``typedoc-plugin-markdown`` (a dev dependency of both) and its own
  ``typedoc.json``;
- ``api/python/``: ``pydoc-markdown`` over the Python SDK, one page per
  public module.

`scripts/docs-publish.py` commits the tree to the docs repository. Every
relative link in the tree must resolve, or the build fails.

The output directory is cleared before each build, so it must sit strictly
inside the scratch root (``target/docs-markdown/`` by default) and, when it
already exists, carry the marker file this script writes. Anything else is
refused before a single file is touched. See docs/ci.md#docs-publishing.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SCRATCH = ROOT / "target" / "docs-markdown"
DEFAULT_OUT = DEFAULT_SCRATCH / "out"

# Written into the output directory; only a directory carrying it is ever
# cleared. `docs-publish.py` skips it.
OUTPUT_MARKER = ".bookclerk-docs-markdown"

# cargo-doc-md 0.11 reads rustdoc JSON format 56, which nightlies emitted
# between 2025-09 and 2025-11-21. Bump both together.
RUST_NIGHTLY = "nightly-2025-11-15"
RUST_JSON_FORMAT = 56

# TypeDoc projects: (output folder under api/, project directory).
TYPEDOC_PROJECTS = (("typescript", "packages/plugin-sdk"), ("ui", "ui"))
PYTHON_SDK = "packages/plugin-sdk-python"
PYTHON_PACKAGE = "bookclerk_plugin_sdk"

# Item kinds rustdoc JSON reports, mapped to the page prefix rustdoc's HTML
# (docs.rs, doc.rust-lang.org) uses for them.
HTML_KIND_PREFIX = {
    "struct": "struct",
    "enum": "enum",
    "union": "union",
    "trait": "trait",
    "trait_alias": "traitalias",
    "function": "fn",
    "type_alias": "type",
    "constant": "constant",
    "static": "static",
    "macro": "macro",
    "proc_attribute": "attr",
    "proc_derive": "derive",
    "primitive": "primitive",
}
STD_CRATES = frozenset({"std", "core", "alloc", "proc_macro", "test"})

FENCE_RE = re.compile(r"^\s*(```|~~~)")
CODE_SPAN_RE = re.compile(r"(`+)(?:(?!\1).)+?\1")
# A shortcut reference such as [`Foo`] or [Foo::bar] that is not followed by
# a link destination, a reference label or a definition colon.
SHORTCUT_RE = re.compile(r"\[(`[^`\[\]]+`|[A-Za-z_][\w:]*(?:\(\))?)\](?![(\[:])")
# An inline link whose destination is a Rust path, as rustdoc allows:
# [text](crate::Item) or [text](Self::method).
INLINE_PATH_RE = re.compile(r"\[((?:[^\[\]]|\[[^\]]*\])+)\]\(([A-Za-z_]\w*(?:::\w+)*(?:\(\))?)\)")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*#*\s*$")
LINK_RE = re.compile(r"(?<!!)\[(?:[^\[\]]|\[[^\]]*\])*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")


class BuildError(RuntimeError):
    """A build step failed."""


class UnsafeOutputDir(BuildError):
    """The output directory is not one this script may clear."""


def _forbidden_dirs() -> set[Path]:
    """Return directories that may never be cleared or used as a scratch root.

    Returns:
        The filesystem root, the home directory, the repository root, the
        shared project mount and the system temp directory, resolved.
    """
    dirs = {Path("/"), Path.home(), ROOT, Path("/mnt"), Path("/tmp"), Path("/root"), Path("/home")}
    return {d.resolve() for d in dirs}


def check_output_dir(out: Path, scratch_root: Path) -> Path:
    """Decide whether ``out`` is safe to clear, without touching anything.

    ``scratch_root`` must resolve strictly inside the repository's
    ``target/`` or the system temp directory; ``out`` must resolve strictly
    inside ``scratch_root``; neither may be a forbidden directory or an
    ancestor of one; ``out`` may not be a symlink; and an existing ``out``
    must be empty or carry :data:`OUTPUT_MARKER`.

    Args:
        out: The requested output directory.
        scratch_root: The only directory outputs may live under.

    Returns:
        ``out``, resolved.

    Raises:
        UnsafeOutputDir: Any of the conditions above fails.
    """
    if out.is_symlink():
        raise UnsafeOutputDir(f"output directory is a symlink: {out}")
    root = scratch_root.resolve()
    target = out.resolve()
    forbidden = _forbidden_dirs()
    for candidate, what in ((root, "scratch root"), (target, "output directory")):
        for bad in forbidden:
            if candidate == bad or candidate in bad.parents:
                raise UnsafeOutputDir(f"{what} {candidate} is or contains {bad}")
    allowed = ((ROOT / "target").resolve(), Path(tempfile.gettempdir()).resolve())
    if not any(base in root.parents for base in allowed):
        raise UnsafeOutputDir(f"scratch root {root} is not strictly inside {' or '.join(map(str, allowed))}")
    if target == root or root not in target.parents:
        raise UnsafeOutputDir(f"output directory {target} is not strictly inside {root}")
    if target.exists():
        if not target.is_dir():
            raise UnsafeOutputDir(f"output path exists and is not a directory: {target}")
        if not (target / OUTPUT_MARKER).is_file() and any(target.iterdir()):
            raise UnsafeOutputDir(f"{target} is not empty and was not made by this script (no {OUTPUT_MARKER})")
    return target


def prepare_output_dir(out: Path, scratch_root: Path) -> Path:
    """Clear and recreate the output directory after :func:`check_output_dir`.

    Args:
        out: The requested output directory.
        scratch_root: The only directory outputs may live under.

    Returns:
        The empty output directory, resolved, holding only the marker.

    Raises:
        UnsafeOutputDir: ``out`` failed the check; nothing was deleted.
    """
    target = check_output_dir(out, scratch_root)
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True)
    (target / OUTPUT_MARKER).write_text("Built by scripts/docs-markdown.py; safe to delete.\n")
    return target


def run(cmd: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    """Run a build command, failing the build when it fails.

    Args:
        cmd: Command and arguments.
        cwd: Working directory.
        env: Extra environment variables.

    Raises:
        BuildError: The command exited non-zero.
    """
    print(f"==> {' '.join(cmd)}", flush=True)
    merged = {**os.environ, **(env or {})}
    result = subprocess.run(cmd, cwd=cwd, env=merged, check=False)
    if result.returncode != 0:
        raise BuildError(f"{cmd[0]} exited {result.returncode}")


def copy_docs(src: Path, out: Path) -> None:
    """Copy ``docs/`` into the output root, leaving out the local ``docs/api/``.

    ``docs/api/`` holds the generated HTML when someone runs
    ``generate-api-docs.sh`` locally; the published tree has its own ``api/``.

    Args:
        src: The repository's ``docs/`` directory.
        out: The output root.
    """
    for path in sorted(src.rglob("*")):
        rel = path.relative_to(src)
        if rel.parts[0] == "api" or path.is_dir() or path.is_symlink():
            continue
        dest = out / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, dest)


def build_rust_json(target_dir: Path) -> Path:
    """Generate rustdoc JSON for every workspace crate on the pinned nightly.

    ``--ignore-rust-version`` because the pinned nightly can predate the
    workspace's ``rust-version``; only documentation is produced.

    Args:
        target_dir: Cargo target directory for this build.

    Returns:
        The directory holding one ``<crate>.json`` per crate.
    """
    run(
        [
            "cargo",
            f"+{RUST_NIGHTLY}",
            "doc",
            "--workspace",
            "--no-deps",
            "--all-features",
            "--ignore-rust-version",
            "--target-dir",
            str(target_dir),
        ],
        cwd=ROOT,
        env={"RUSTDOCFLAGS": "-Z unstable-options --output-format json"},
    )
    return target_dir / "doc"


def workspace_targets() -> set[str]:
    """Return the rustdoc names of the workspace's library and binary targets.

    Returns:
        Target names with ``-`` replaced by ``_``, as rustdoc names its JSON.
    """
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    names = set()
    for package in json.loads(result.stdout)["packages"]:
        for target in package["targets"]:
            if set(target["kind"]) & {"lib", "bin", "proc-macro", "rlib", "cdylib", "staticlib", "dylib"}:
                names.add(target["name"].replace("-", "_"))
    return names


def convert_rust(json_dir: Path, out: Path, crates: set[str]) -> list[str]:
    """Convert each crate's rustdoc JSON with ``cargo-doc-md``.

    Only files named after a current workspace target are read, so JSON left
    in a reused target directory by a renamed or removed crate is ignored.

    Args:
        json_dir: Directory of ``<crate>.json`` files.
        out: ``api/rust`` in the output tree.
        crates: Output of :func:`workspace_targets`.

    Returns:
        The crate names converted, sorted.

    Raises:
        BuildError: No JSON was found, or a file has an unexpected format.
    """
    files = sorted(p for p in json_dir.glob("*.json") if p.stem in crates)
    if not files:
        raise BuildError(f"no rustdoc JSON in {json_dir}")
    crates = []
    for path in files:
        with path.open() as fh:
            fmt = json.load(fh).get("format_version")
        if fmt != RUST_JSON_FORMAT:
            raise BuildError(f"{path.name}: rustdoc JSON format {fmt}, expected {RUST_JSON_FORMAT}")
        run(["cargo-doc-md", "--json", str(path), "--output", str(out)], cwd=ROOT)
        crates.append(path.stem)
    # Each run rewrites the converter's own master index for one crate;
    # write_rust_index replaces it with one covering every crate.
    (out / "index.md").unlink(missing_ok=True)
    return crates


def slug(heading: str) -> str:
    """Return the GitHub-style anchor for a heading.

    Args:
        heading: Heading text, without the leading ``#`` marks.

    Returns:
        Lowercased text with punctuation other than ``-`` and ``_`` removed
        and spaces turned into ``-``.
    """
    text = re.sub(r"[`*]", "", heading).strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


@dataclass
class HeadingIndex:
    """Where each Rust item and module landed in the converted tree.

    Attributes:
        items: Full path (``crate::module::Item``) -> (file, anchor).
        modules: Full module path -> file.
    """

    items: dict[str, tuple[Path, str]] = field(default_factory=dict)
    modules: dict[str, Path] = field(default_factory=dict)


def index_rust_headings(rust_out: Path) -> HeadingIndex:
    """Index the item headings and module files ``cargo-doc-md`` wrote.

    Items are headed by their full path (``## crate::mux::Mux``). A module
    ``crate::a::b`` is the file ``crate/a/b.md``; a crate root is
    ``crate/index.md``.

    Args:
        rust_out: ``api/rust`` in the output tree.

    Returns:
        The index.
    """
    index = HeadingIndex()
    for md in sorted(rust_out.rglob("*.md")):
        rel = md.relative_to(rust_out)
        if len(rel.parts) < 2:
            continue
        crate = rel.parts[0]
        if rel.name == "index.md" and len(rel.parts) == 2:
            index.modules.setdefault(crate, md)
        else:
            module = "::".join((crate, *rel.with_suffix("").parts[1:]))
            if rel.with_suffix("").parts[1:] == (crate,):
                module = crate
            index.modules.setdefault(module, md)
        in_fence = False
        for line in md.read_text(encoding="utf-8").splitlines():
            if FENCE_RE.match(line):
                in_fence = not in_fence
                continue
            match = None if in_fence else HEADING_RE.match(line)
            if match and "::" in match.group(2) and " " not in match.group(2).strip("`"):
                name = match.group(2).strip("`")
                index.items.setdefault(name, (md, slug(match.group(2))))
    return index


def external_url(path: list[str], kind: str, crate_name: str) -> str | None:
    """Return the rustdoc HTML page of an item outside the workspace.

    Args:
        path: The item's full path, starting with its crate.
        kind: rustdoc JSON item kind.
        crate_name: The crate's name as rustdoc reports it.

    Returns:
        A doc.rust-lang.org URL for the standard library, a docs.rs URL
        otherwise, or None for kinds without their own page.
    """
    base = (
        f"https://doc.rust-lang.org/stable/{'/'.join(path[:-1])}"
        if crate_name in STD_CRATES
        else f"https://docs.rs/{crate_name}/latest/{'/'.join(path[:-1])}"
    )
    if kind == "module":
        return f"{base}/{path[-1]}/index.html"
    prefix = HTML_KIND_PREFIX.get(kind)
    if prefix is None:
        return None
    return f"{base}/{prefix}.{path[-1]}.html"


def rust_link_targets(json_path: Path, workspace_crates: set[str]) -> dict[str, tuple[str, str] | None]:
    """Collect what each intra-doc link text in a crate points at.

    Args:
        json_path: One crate's rustdoc JSON.
        workspace_crates: Crates converted into this tree.

    Returns:
        Link text (as written between the brackets) -> ("local", full path)
        for workspace items, ("url", address) for other crates, or None when
        the text is a link but its target has no page, or names different
        targets in different items.
    """
    with json_path.open() as fh:
        data = json.load(fh)
    paths = data.get("paths", {})
    crates = {str(k): v.get("name", "") for k, v in data.get("external_crates", {}).items()}
    found: dict[str, tuple[str, str] | None] = {}
    for item in data.get("index", {}).values():
        for text, target_id in (item.get("links") or {}).items():
            summary = paths.get(str(target_id))
            resolved: tuple[str, str] | None = None
            if summary:
                full = summary["path"]
                if full[0] in workspace_crates:
                    resolved = ("local", "::".join(full))
                else:
                    name = crates.get(str(summary.get("crate_id")), full[0])
                    url = external_url(full, summary.get("kind", ""), name)
                    resolved = ("url", url) if url else None
            if text in found and found[text] != resolved:
                found[text] = None
            else:
                found[text] = resolved
    return found


def _code_spans(line: str) -> list[tuple[int, int]]:
    """Return the ``(start, end)`` offsets of inline code spans in a line.

    Args:
        line: One Markdown line.

    Returns:
        Offsets, end exclusive.
    """
    return [m.span() for m in CODE_SPAN_RE.finditer(line)]


def rewrite_shortcuts(
    text: str, md_file: Path, targets: dict[str, tuple[str, str] | None], index: HeadingIndex
) -> tuple[str, int]:
    """Turn rustdoc shortcut links in one converted file into Markdown links.

    Handles shortcut links (``[`Foo`]``) and inline links whose destination
    is a Rust path (``[text](crate::Foo)``). Only texts rustdoc itself
    recorded as links are touched, and never inside fenced code or inline
    code. A link whose target has no page (a method, say) keeps its text and
    loses the link.

    Args:
        text: The file's Markdown.
        md_file: The file's path, for relative links.
        targets: Output of :func:`rust_link_targets` for the file's crate.
        index: Output of :func:`index_rust_headings`.

    Returns:
        (new Markdown, number of links rewritten).
    """
    def resolve(label: str) -> str | None:
        target = targets[label]
        if target is None:
            return None
        if target[0] == "url":
            return target[1]
        full = target[1]
        if full in index.items:
            dest, anchor = index.items[full]
            return f"{Path(os.path.relpath(dest, md_file.parent)).as_posix()}#{anchor}"
        if full in index.modules:
            return Path(os.path.relpath(index.modules[full], md_file.parent)).as_posix()
        return None

    out_lines = []
    count = 0
    in_fence = False
    for line in text.splitlines(keepends=True):
        if FENCE_RE.match(line):
            in_fence = not in_fence
            out_lines.append(line)
            continue
        if in_fence:
            out_lines.append(line)
            continue
        spans = _code_spans(line)

        def replace(match: re.Match[str]) -> str:
            nonlocal count
            start = match.start()
            if any(s <= start < e for s, e in spans):
                return match.group(0)
            label = match.group(1)
            if label not in targets:
                return match.group(0)
            count += 1
            href = resolve(label)
            return label if href is None else f"[{label}]({href})"

        def replace_inline(match: re.Match[str]) -> str:
            nonlocal count
            if any(s <= match.start() < e for s, e in spans):
                return match.group(0)
            text, dest = match.group(1), match.group(2)
            if dest not in targets:
                return match.group(0)
            count += 1
            href = resolve(dest)
            return text if href is None else f"[{text}]({href})"

        line = INLINE_PATH_RE.sub(replace_inline, line)
        spans = _code_spans(line)
        out_lines.append(SHORTCUT_RE.sub(replace, line))
    return "".join(out_lines), count


def fix_rust_links(json_dir: Path, rust_out: Path, crates: list[str]) -> int:
    """Rewrite intra-doc links across the converted Rust reference.

    Args:
        json_dir: Directory of ``<crate>.json`` files.
        rust_out: ``api/rust`` in the output tree.
        crates: Crates converted.

    Returns:
        The number of links rewritten.
    """
    index = index_rust_headings(rust_out)
    workspace = set(crates)
    total = 0
    for crate in crates:
        targets = rust_link_targets(json_dir / f"{crate}.json", workspace)
        crate_dir = rust_out / crate
        if not crate_dir.is_dir():
            continue
        for md in sorted(crate_dir.rglob("*.md")):
            text = md.read_text(encoding="utf-8")
            new, count = rewrite_shortcuts(text, md, targets, index)
            if new != text:
                md.write_text(new, encoding="utf-8")
            total += count
    return total


def write_rust_index(rust_out: Path, crates: list[str]) -> None:
    """Write ``api/rust/README.md``, listing every converted crate.

    Args:
        rust_out: ``api/rust`` in the output tree.
        crates: Crates converted.
    """
    lines = [
        "# Rust API reference",
        "",
        "Generated from rustdoc JSON for every crate in the Bookclerk workspace.",
        "Start with [`bookclerk_plugin_sdk`](bookclerk_plugin_sdk/index.md) to write a native plugin.",
        "",
    ]
    for crate in crates:
        if (rust_out / crate / "index.md").is_file():
            lines.append(f"- [`{crate}`]({crate}/index.md)")
    (rust_out / "README.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def build_typedoc(project: Path, out: Path, revision: str | None) -> None:
    """Run TypeDoc's Markdown theme over one TypeScript project.

    The project's own TypeDoc, TypeScript and ``typedoc.json`` are used, so
    the reference matches what ``generate-api-docs.sh`` checks.

    Args:
        project: The TypeScript project directory, dependencies installed.
        out: Its folder under ``api/``.
        revision: Commit for source links, or None for TypeDoc's default.
    """
    cmd = [
        str(project / "node_modules" / ".bin" / "typedoc"),
        "--options",
        "typedoc.json",
        "--plugin",
        "typedoc-plugin-markdown",
        "--out",
        str(out),
    ]
    if revision:
        cmd += ["--gitRevision", revision]
    run(cmd, cwd=project)


def link_media_to_docs(project_out: Path, out: Path) -> int:
    """Point TypeDoc's copies of ``docs/`` pages back at the pages themselves.

    TypeDoc copies every Markdown file a README links to into ``_media/``,
    where that copy's own relative links no longer resolve. A copy identical
    to a page of the docs tree is deleted and links to it are rewritten to
    the page.

    Args:
        project_out: One TypeDoc output folder under ``api/``.
        out: The output root.

    Returns:
        The number of copies replaced.
    """
    media = project_out / "_media"
    if not media.is_dir():
        return 0
    docs = {}
    for path in out.rglob("*.md"):
        rel = path.relative_to(out)
        if rel.parts[0] != "api":
            docs.setdefault(path.read_bytes(), path)
    replaced = {}
    for copy in sorted(media.glob("*.md")):
        page = docs.get(copy.read_bytes())
        if page is not None:
            replaced[copy.resolve()] = page
    if not replaced:
        return 0
    for md in project_out.rglob("*.md"):
        if md.resolve() in replaced:
            continue
        text = md.read_text(encoding="utf-8")

        def relink(match: re.Match[str], md: Path = md) -> str:
            href = match.group(1)
            path, _, anchor = href.partition("#")
            if not path or "://" in path:
                return match.group(0)
            page = replaced.get((md.parent / path).resolve())
            if page is None:
                return match.group(0)
            new = Path(os.path.relpath(page, md.parent)).as_posix() + (f"#{anchor}" if anchor else "")
            return match.group(0).replace(f"({href}", f"({new}", 1)

        new_text = LINK_RE.sub(relink, text)
        if new_text != text:
            md.write_text(new_text, encoding="utf-8")
    for copy in replaced:
        copy.unlink()
    if not any(media.iterdir()):
        media.rmdir()
    return len(replaced)


def python_modules(src: Path, package: str) -> list[str]:
    """List the public modules of a Python package.

    Modules or packages whose name starts with ``_`` (other than
    ``__init__``), ``__main__`` and template directories are left out.

    Args:
        src: The ``src`` directory holding the package.
        package: Package name.

    Returns:
        Dotted module names, sorted.
    """
    modules = []
    for path in sorted((src / package).rglob("*.py")):
        rel = path.relative_to(src).with_suffix("")
        parts = list(rel.parts)
        if parts[-1] == "__init__":
            parts = parts[:-1]
        if any(p.startswith("_") or p.endswith("_template") for p in parts):
            continue
        modules.append(".".join(parts))
    return modules


def build_python(venv_bin: Path, src: Path, out: Path) -> list[str]:
    """Render each public Python SDK module with ``pydoc-markdown``.

    Args:
        venv_bin: ``bin`` of a venv with the ``docs-markdown`` extra.
        src: The SDK's ``src`` directory.
        out: ``api/python`` in the output tree.

    Returns:
        The modules rendered.
    """
    out.mkdir(parents=True, exist_ok=True)
    modules = python_modules(src, PYTHON_PACKAGE)
    for module in modules:
        # JSON is valid YAML, which pydoc-markdown reads.
        config = {
            "loaders": [{"type": "python", "search_path": [str(src)], "modules": [module]}],
            "processors": [{"type": "filter", "skip_empty_modules": True}, {"type": "google"}, {"type": "crossref"}],
            "renderer": {
                "type": "markdown",
                "filename": str(out / f"{module}.md"),
                "render_toc": False,
                "insert_header_anchors": False,
                "descriptive_class_title": False,
                "add_method_class_prefix": True,
                "render_module_header": True,
            },
        }
        config_path = out / f".{module}.pydoc-markdown.yml"
        config_path.write_text(json.dumps(config), encoding="utf-8")
        try:
            run([str(venv_bin / "pydoc-markdown"), str(config_path)], cwd=src)
        finally:
            config_path.unlink(missing_ok=True)
    lines = ["# Python API reference", "", "The `bookclerk-plugin-sdk` package, one page per public module.", ""]
    lines += [f"- [`{m}`]({m}.md)" for m in modules if (out / f"{m}.md").is_file()]
    (out / "README.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return modules


def write_api_index(api_out: Path, sections: Iterable[tuple[str, str, str]]) -> None:
    """Write ``api/README.md``, linking each reference that was built.

    Args:
        api_out: ``api`` in the output tree.
        sections: (folder, title, description) for each candidate reference.
    """
    lines = [
        "# API reference",
        "",
        "Generated from the source at the same version as these docs.",
        "To build the HTML version locally, run `./scripts/generate-api-docs.sh`.",
        "",
    ]
    for folder, title, description in sections:
        if (api_out / folder / "README.md").is_file():
            lines.append(f"- [{title}]({folder}/README.md): {description}")
    (api_out / "README.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def broken_links(out: Path) -> list[str]:
    """Find relative Markdown links that do not resolve to a file in the tree.

    A link inside the tree must name a file that exists. A link from
    ``docs/`` out of the tree (to source files) is allowed, one from ``api/``
    is not. Anchors are not checked: converters slug headings differently
    from the site. Links in fenced code are ignored.

    Args:
        out: The output root.

    Returns:
        ``file: target`` for each broken link.
    """
    broken = []
    for md in sorted(out.rglob("*.md")):
        in_fence = False
        for line in md.read_text(encoding="utf-8").splitlines():
            if FENCE_RE.match(line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            spans = _code_spans(line)
            for match in LINK_RE.finditer(line):
                if any(s <= match.start() < e for s, e in spans):
                    continue
                href = match.group(1)
                if re.match(r"^[a-z][a-z0-9+.-]*:", href) or href.startswith(("#", "/")):
                    continue
                target = href.split("#", 1)[0]
                if not target:
                    continue
                resolved = (md.parent / target).resolve()
                inside = out.resolve() in resolved.parents
                if inside and resolved.exists():
                    continue
                # Links from docs/ to repository files outside docs/ are
                # expected; the site rewrites them to GitHub.
                if not inside and md.relative_to(out).parts[0] != "api":
                    continue
                broken.append(f"{md.relative_to(out)}: {href}")
    return broken


def main(argv: list[str] | None = None) -> int:
    """Run the CLI.

    Args:
        argv: Arguments, without the program name.

    Returns:
        The process exit status.
    """
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="output directory (cleared first)")
    parser.add_argument("--scratch-root", type=Path, default=DEFAULT_SCRATCH, help="directory --out must be inside")
    parser.add_argument("--rust-json-dir", type=Path, help="use existing rustdoc JSON instead of building it")
    parser.add_argument("--python-venv", type=Path, help="venv with the docs-markdown extra (default: build one)")
    parser.add_argument("--revision", help="commit for TypeScript source links")
    parser.add_argument("--skip-rust", action="store_true")
    parser.add_argument("--skip-typescript", action="store_true")
    parser.add_argument("--skip-python", action="store_true")
    args = parser.parse_args(argv)

    try:
        out = prepare_output_dir(args.out, args.scratch_root)
        scratch = args.scratch_root.resolve()
        copy_docs(ROOT / "docs", out)
        api = out / "api"
        api.mkdir()

        if not args.skip_rust:
            json_dir = args.rust_json_dir.resolve() if args.rust_json_dir else build_rust_json(scratch / "cargo-target")
            crates = convert_rust(json_dir, api / "rust", workspace_targets())
            fixed = fix_rust_links(json_dir, api / "rust", crates)
            write_rust_index(api / "rust", crates)
            print(f"==> Rust: {len(crates)} crates, {fixed} intra-doc links rewritten")

        if not args.skip_typescript:
            for folder, project in TYPEDOC_PROJECTS:
                if not (ROOT / project / "node_modules").is_dir():
                    run(["npm", "ci", "--no-audit", "--no-fund"], cwd=ROOT / project)
                build_typedoc(ROOT / project, api / folder, args.revision)
                link_media_to_docs(api / folder, out)

        if not args.skip_python:
            sdk = ROOT / PYTHON_SDK
            venv = args.python_venv.resolve() if args.python_venv else scratch / "python-venv"
            if args.python_venv is None:
                run(
                    ["uv", "sync", "--quiet", "--no-install-project", "--locked", "--extra", "docs-markdown"],
                    cwd=sdk,
                    env={"UV_PROJECT_ENVIRONMENT": str(venv)},
                )
            modules = build_python(venv / "bin", sdk / "src", api / "python")
            print(f"==> Python: {len(modules)} modules")

        write_api_index(
            api,
            (
                ("rust", "Rust", "every workspace crate, including the plugin SDK"),
                ("typescript", "TypeScript plugin SDK", "`@bookclerk/plugin-sdk`"),
                ("ui", "Web UI library", "`ui/src/lib`"),
                ("python", "Python plugin SDK", "`bookclerk-plugin-sdk`"),
            ),
        )
        broken = broken_links(out)
        if broken:
            for line in broken[:50]:
                print(f"broken link: {line}", file=sys.stderr)
            raise BuildError(f"{len(broken)} broken relative links in the API reference")
        files = sum(1 for p in out.rglob("*") if p.is_file())
        print(f"==> wrote {files} files under {out}")
    except BuildError as err:
        print(f"::error::{err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
