#!/usr/bin/env python3
"""Build the Markdown docs tree published to the docs repository.

The tree holds ``docs/`` as it is, with each generated reference added to
the shelf its readers use (:data:`REFERENCES`):

- ``plugins/sdk/typescript/``: the TypeScript plugin SDK, from the project's
  own TypeDoc with ``typedoc-plugin-markdown`` and its ``typedoc.json``, one
  page per source module;
- ``plugins/sdk/python/``: the Python plugin SDK, read with :mod:`ast` by
  ``python_api_markdown.py`` (nothing in the package is imported), nested
  like the package;
- ``plugins/sdk/rust/``: the Rust plugin SDK crate, from rustdoc JSON (a
  pinned nightly) rendered by ``rustdoc_markdown.py``, one page per module;
- ``internals/crates/``: every other workspace library crate, from the same
  rustdoc JSON, grouped into plugin interface, plugins, host internals and
  development crates;
- ``server/api/``: the daemon's HTTP API, from its committed OpenAPI document
  (``crates/bookclerkd/openapi.json``) rendered by ``openapi_markdown.py``;
- ``contributing/ui/``: the web UI library, like the TypeScript SDK.

A reference's landing page is the folder's hand-written ``README.md`` from
``docs/``, which gets the generated module list and the package or crate
root appended; ``server/api`` and ``internals/crates`` are written whole.
The references are listed in ``generated.json`` at the root of the tree,
which the site reads to tell generated pages from hand-written ones.

The build refuses a hand-written page at a generated page's path, a page
named ``index.md`` (the site serves every page's Markdown copy at
``<page>/index.md``), a folder named ``index``, a page beside a folder of
the same name, and a broken relative link.

`scripts/docs-publish.py` commits the tree to the docs repository.

The output directory is cleared before each build, so it must sit strictly
inside the scratch root (``target/docs-markdown/`` by default) and, when it
already exists, carry the marker file this script writes. Anything else is
refused before a single file is touched. See docs/contributing/automation.md#docs-publishing.
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
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import openapi_markdown  # noqa: E402  (sibling module, needs the path above)
import python_api_markdown  # noqa: E402
import rustdoc_markdown  # noqa: E402

DEFAULT_SCRATCH = ROOT / "target" / "docs-markdown"
DEFAULT_OUT = DEFAULT_SCRATCH / "out"

# Written into the output directory; only a directory carrying it is ever
# cleared. `docs-publish.py` skips it.
OUTPUT_MARKER = ".bookclerk-docs-markdown"

# rustdoc_markdown.py reads rustdoc JSON format 56, which nightlies emitted
# between 2025-09 and 2025-11-21. Bump both together.
RUST_NIGHTLY = "nightly-2025-11-15"
RUST_JSON_FORMAT = 56

# Markdown-theme options for TypeDoc: no GitHub source links (the docs are
# read on the site), no per-page package banner or breadcrumbs, no README (the
# landing page is hand-written), one page per module rather than per symbol,
# and parameters as tables.
TYPEDOC_MARKDOWN_OPTIONS = (
    "--readme",
    "none",
    "--disableSources",
    "--hidePageHeader",
    "--hideBreadcrumbs",
    "--outputFileStrategy",
    "modules",
    "--parametersFormat",
    "table",
    "--enumMembersFormat",
    "table",
    "--typeDeclarationFormat",
    "table",
    "--useCodeBlocks",
)
# Written at the output root: the date each page of docs/ last changed.
LAST_UPDATED_FILE = "last-updated.json"
LAST_UPDATED_VERSION = 1
# Starts each commit's header line in the `git log` output read by
# page_dates. A NUL cannot start a path, and git writes it for `%x00`.
COMMIT_MARK = "\x00commit "
COMMIT_FORMAT = "%x00commit %at"
# The daemon's OpenAPI document, regenerated and checked by bookclerkd's tests.
OPENAPI_DOCUMENT = "crates/bookclerkd/openapi.json"
PYTHON_SDK = "packages/plugin-sdk-python"
PYTHON_PACKAGE = "bookclerk_plugin_sdk"
# Written at the output root: the generated references and their pages.
GENERATED_FILE = "generated.json"
GENERATED_VERSION = 1


@dataclass(frozen=True)
class Reference:
    """One generated reference and where it lands in the tree.

    Attributes:
        folder: Its folder in the tree (``plugins/sdk/python``).
        kind: What generates it: ``typescript`` (TypeDoc), ``python``,
            ``rust-sdk`` (the SDK crate), ``rust-crates`` (every other
            crate) or ``web-api`` (OpenAPI).
        title: What the site calls it.
        source: The repository path it is generated from.
        landing: Whether the folder's ``README.md`` is hand-written in
            ``docs/`` (the build appends the generated part to it) rather
            than written by the build.
        project: For ``typescript``, the TypeDoc project directory.
        root_module: For ``typescript``, the module whose page is the
            package root, folded into the landing page; None when the
            project has no root module.
    """

    folder: str
    kind: str
    title: str
    source: str
    landing: bool
    project: str = ""
    root_module: str | None = None


# The references, in the order the build writes them.
REFERENCES = (
    Reference(
        "plugins/sdk/typescript",
        "typescript",
        "TypeScript plugin SDK",
        "packages/plugin-sdk",
        True,
        project="packages/plugin-sdk",
        root_module="plugin-sdk",
    ),
    Reference("plugins/sdk/python", "python", "Python plugin SDK", PYTHON_SDK, True),
    Reference(
        "plugins/sdk/rust",
        "rust-sdk",
        "Rust plugin SDK",
        "crates/bookclerk-plugin-sdk",
        True,
    ),
    Reference("server/api", "web-api", "Web API", OPENAPI_DOCUMENT, False),
    Reference("internals/crates", "rust-crates", "Rust crates", "Cargo.toml", False),
    Reference(
        "contributing/ui",
        "typescript",
        "Web UI library",
        "ui/src/lib",
        True,
        project="ui",
    ),
)

# Groups of the Rust reference, in index order: (title, description).
# A crate's group comes from RUST_GROUP_OF, else from its directory.
RUST_GROUPS = (
    (
        "Plugin interface",
        "The plugin ABI, the `plugin.toml` manifest and the plugin author CLI.",
    ),
    (
        "Plugins",
        "First-party plugins: databases, destinations, sources and integrations.",
    ),
    ("Host internals", "The crates the hosts are built from. Not a stable API."),
    ("Development and testing", "Build tooling, test suites and examples."),
)
# The Rust plugin SDK itself lands on the plugins shelf (the `rust-sdk`
# reference); every other crate goes under `rust-crates`.
RUST_SDK_CRATE = "bookclerk_plugin_sdk"
RUST_GROUP_OF = {
    "bookclerk_plugin_sdk": "Plugin interface",
    "bookclerk_plugin_abi": "Plugin interface",
    "bookclerk_plugin_manifest": "Plugin interface",
    "bookclerk_plugin_tools": "Plugin interface",
    "bookclerk_dev": "Development and testing",
    "bookclerk_plugin_e2e": "Development and testing",
}

FENCE_RE = re.compile(r"^\s*(```|~~~)")
LINK_RE = re.compile(r"(?<!!)\[(?:[^\[\]]|\[[^\]]*\])*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
# A markdownlint directive comment on one line
# (`<!-- markdownlint-disable-line MD038 -->`) with the spaces before it. The
# published tree leaves these out; one that spans lines stays, so stripping
# never removes a line break.
LINT_COMMENT_RE = re.compile(r"[ \t]*<!--[ \t]*markdownlint-[^\n]*?-->")
# The container markers and indentation that open a line: block quotes
# (`> `), list items (`- `, `1. `) and their continuation indent.
CONTAINER_RE = re.compile(r"(?:[ \t]*(?:>|[-*+](?=[ \t])|\d{1,9}[.)](?=[ \t])) ?)*[ \t]*")
# The block-quote markers that open a line. Inside a fence, list markers are
# code, so a closing fence can follow only these and up to three spaces more
# than the opening fence's container indent.
QUOTE_PREFIX_RE = re.compile(r"(?:[ \t]*> ?)*")
# The spaces at the start of what follows a line's block-quote markers.
INDENT_RE = re.compile(r"[ \t]*")
# A code fence line after its container markers: its backtick or tilde run,
# then the rest of the line.
FENCE_RUN_RE = re.compile(r"(`{3,}|~{3,})(.*)$")
# A code span: a backtick run, its content (which may cross lines within a
# paragraph) and a closing run of the same length.
CODE_SPAN_RE = re.compile(r"(?<!`)(`+)(?!`).*?(?<!`)\1(?!`)", re.DOTALL)


class BuildError(RuntimeError):
    """A build step failed."""


class UnsafeOutputDir(BuildError):
    """The output directory is not one this script may clear."""


class NoHistory(BuildError):
    """The checkout has no complete git history to date the pages from."""


def _forbidden_dirs() -> set[Path]:
    """Return directories that may never be cleared or used as a scratch root.

    Returns:
        The filesystem root, the home directory, the repository root, the
        shared project mount and the system temp directory, resolved.
    """
    dirs = {
        Path("/"),
        Path.home(),
        ROOT,
        Path("/mnt"),
        Path("/tmp"),
        Path("/root"),
        Path("/home"),
    }
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


def strip_outside_code_spans(paragraph: str) -> str:
    """Remove markdownlint directives from a paragraph, sparing code spans.

    A code span opens and closes with backtick runs of the same length, so
    a double-backtick span can hold a single backtick, and it may run over
    several lines of the paragraph. Line breaks are never removed.

    Args:
        paragraph: Lines of prose outside fenced code, joined by newlines.

    Returns:
        The paragraph without directives outside its code spans.
    """
    kept: list[str] = []
    start = 0
    for span in CODE_SPAN_RE.finditer(paragraph):
        kept.append(LINT_COMMENT_RE.sub("", paragraph[start : span.start()]))
        kept.append(span.group(0))
        start = span.end()
    kept.append(LINT_COMMENT_RE.sub("", paragraph[start:]))
    return "".join(kept)


@dataclass(frozen=True)
class Fence:
    """An open fenced code block, as far as finding its closing line needs.

    Attributes:
        run: The opening backtick or tilde run; a closing run starts with it.
        depth: How many block quotes hold the fence.
        indent: Columns between the fence's quote markers and its run (list
            markers and indentation); a closing run may sit at most three
            columns further in.
    """

    run: str
    depth: int
    indent: int


def opening_fence(line: str) -> Fence | None:
    """Say whether a line opens a fenced code block.

    Args:
        line: One line of the page.

    Returns:
        The fence, or ``None`` when the line does not open one (a backtick
        fence's info string cannot hold a backtick).
    """
    prefix = CONTAINER_RE.match(line)
    start = prefix.end() if prefix else 0
    run = FENCE_RUN_RE.match(line, start)
    if run is None or (run.group(1)[0] == "`" and "`" in run.group(2)):
        return None
    quote = QUOTE_PREFIX_RE.match(line)
    quote_end = quote.end() if quote else 0
    return Fence(run.group(1), line[:quote_end].count(">"), start - quote_end)


def closes_fence(line: str, fence: Fence) -> bool | None:
    """Say how a line inside a fenced code block relates to the fence.

    Args:
        line: One line after the fence opened.
        fence: The open fence.

    Returns:
        ``True`` when the line is the closing fence, ``None`` when it leaves
        the fence's block quote (which ends the fence before the line), and
        ``False`` when it is code.
    """
    quote = QUOTE_PREFIX_RE.match(line)
    quote_end = quote.end() if quote else 0
    depth = line[:quote_end].count(">")
    if depth < fence.depth:
        return None
    # A closing fence sits at the fence's own quote depth (deeper markers are
    # code), indented at most three columns past the fence's container, and
    # is a run of the opening character at least as long as the opening run
    # with nothing but spaces after it.
    if depth > fence.depth:
        return False
    indent = INDENT_RE.match(line, quote_end)
    start = indent.end() if indent else quote_end
    if start - quote_end > fence.indent + 3:
        return False
    run = FENCE_RUN_RE.match(line, start)
    return bool(run and run.group(1).startswith(fence.run) and run.group(2).strip() == "")


def strip_lint_comments(text: str) -> str:
    """Remove markdownlint directives from a page's prose.

    The directives are for the ``lint`` check only. GitHub hides them, but
    the site renders the published tree with raw HTML off and serves it as
    Markdown, so they leave the tree here. Code spans and fenced code
    (fences in block quotes and list items included) keep them, so a page
    can still show one as an example; where the page's structure is unclear,
    the filter keeps a directive rather than risk cutting code. A line that
    held nothing but a directive goes, and so does the blank line after it
    when one came before it.

    Args:
        text: The page's Markdown.

    Returns:
        The page without the directives.
    """
    fence: Fence | None = None
    out: list[str] = []
    paragraph: list[str] = []
    dropped = False

    def flush() -> None:
        nonlocal dropped
        if not paragraph:
            return
        kept_lines = strip_outside_code_spans("\n".join(paragraph)).split("\n")
        for line, kept in zip(paragraph, kept_lines, strict=True):
            if kept.strip() == "" and line.strip() != "":
                dropped = True
                continue
            if not (dropped and kept == "" and out and out[-1] == ""):
                out.append(kept)
            dropped = False
        paragraph.clear()

    for line in text.split("\n"):
        if fence is not None:
            closing = closes_fence(line, fence)
            if closing is not None:
                if closing:
                    fence = None
                out.append(line)
                continue
            fence = None
        opened = opening_fence(line)
        if opened is not None:
            flush()
            fence = opened
            dropped = False
            out.append(line)
        elif line.strip() == "":
            # A blank line ends the paragraph, so no code span crosses it.
            paragraph.append(line)
            flush()
        else:
            paragraph.append(line)
    flush()
    return "\n".join(out)


def copy_docs(src: Path, out: Path) -> None:
    """Copy ``docs/`` into the output root.

    Markdown pages lose their markdownlint overrides (see
    :func:`strip_lint_comments`).

    Args:
        src: The repository's ``docs/`` directory.
        out: The output root.
    """
    for path in sorted(src.rglob("*")):
        rel = path.relative_to(src)
        if path.is_dir() or path.is_symlink():
            continue
        dest = out / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        if path.suffix == ".md":
            dest.write_text(strip_lint_comments(path.read_text(encoding="utf-8")), encoding="utf-8")
        else:
            shutil.copyfile(path, dest)


def page_dates(log: str, pages: set[str]) -> dict[str, str]:
    """Date each page from ``git log --name-status`` output over ``docs/``.

    A page's date is the newest commit that changed its content. A pure move
    (a rename git scores 100, the content unchanged) is not an update: the
    page is followed back to its old path instead. A partial rename is an
    update and is followed back too. An add ends the page's history, so a
    path that was deleted and later re-added is not dated by the old file.
    The log must list every commit before its parents (``--topo-order``).
    This repository's history is linear (pull requests are squashed), so
    one rename map is enough; renames on parallel branches are not merged.

    Args:
        log: The output of ``git log --topo-order -M --name-status`` with
            ``--format=COMMIT_FORMAT`` and newest commits first, as
            :func:`git_docs_log` returns it.
        pages: Repository paths of the pages to date (``docs/x.md``).

    Returns:
        ``YYYY-MM-DDTHH:MM:SSZ`` (the author date, in UTC) by repository
        path, for each page some commit touched. The site shows it in the
        reader's own time zone.

    Raises:
        BuildError: A line is not one ``git log`` writes, or names a path
            git had to quote (a page would be left undated without notice).
    """
    # Path in the commit being read -> the page it is now.
    names = {page: page for page in pages}
    dates: dict[str, int] = {}
    when: int | None = None

    def touch(page: str) -> None:
        """Record the current commit as a change to ``page``."""
        assert when is not None  # a status line follows a commit line
        dates[page] = max(dates.get(page, when), when)

    for line in log.split("\n"):
        if not line:
            continue
        if line.startswith(COMMIT_MARK):
            stamp = line[len(COMMIT_MARK) :]
            if not stamp.isdigit():
                raise BuildError(f"git log: bad commit line {line!r}")
            when = int(stamp)
            continue
        fields = line.split("\t")
        status = fields[0]
        if when is None or not status:
            raise BuildError(f"git log: status line outside a commit: {line!r}")
        if any(path.startswith('"') for path in fields[1:]):
            raise BuildError(f"git log: quoted path, rename the file: {line!r}")
        if status[0] == "R" and len(fields) == 3:
            old, new = fields[1], fields[2]
            page = names.get(new)
            if page is None:
                continue
            del names[new]
            names[old] = page
            if status != "R100":
                touch(page)
        elif status in ("A", "M", "D", "T") and len(fields) == 2:
            page = names.get(fields[1])
            if page is None:
                continue
            if status == "D":
                # Deleted here, re-added by a newer commit already read.
                del names[fields[1]]
                continue
            touch(page)
            if status == "A":
                del names[fields[1]]
        else:
            raise BuildError(f"git log: unexpected status line {line!r}")
    return {page: datetime.fromtimestamp(stamp, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ") for page, stamp in dates.items()}


def git_docs_log(repo: Path) -> str:
    """Read the history of ``docs/`` for :func:`page_dates`.

    Args:
        repo: The repository root.

    Returns:
        The ``git log`` output, newest commit first.

    Raises:
        NoHistory: ``repo`` is not a git checkout or its history is shallow,
            so dates would be wrong.
        BuildError: ``git log`` failed (a subclass other than NoHistory, so
            it fails the build with or without ``--require-history``).
    """

    def git(*args: str) -> str:
        """Run git in ``repo`` and return its standard output.

        Raises:
            BuildError: git failed.
        """
        result = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            raise BuildError(f"git {args[0]}: {result.stderr.strip() or f'exited {result.returncode}'}")
        return result.stdout

    try:
        shallow = git("rev-parse", "--is-shallow-repository").strip()
    except BuildError as err:
        raise NoHistory(f"not a git checkout: {err}") from err
    if shallow != "false":
        raise NoHistory("the checkout is shallow; fetch the full history (fetch-depth: 0)")
    return git(
        "-c",
        "core.quotePath=false",
        "log",
        "--topo-order",
        "-M",
        "--name-status",
        f"--format={COMMIT_FORMAT}",
        "--",
        "docs",
    )


def write_last_updated(out: Path, log: str, generated: set[str] = frozenset()) -> int:
    """Write :data:`LAST_UPDATED_FILE`: when each copied page last changed.

    The site shows it as "Last updated". Only pages copied from ``docs/`` are
    dated here; ``docs-publish.py`` adds the generated pages, which it can
    compare with the docs repository.

    Args:
        out: The output root, after :func:`copy_docs`.
        log: The history of ``docs/``, from :func:`git_docs_log`.
        generated: Keys of the generated pages (:attr:`Tree.keys`), left out.

    Returns:
        How many pages got a date.

    Raises:
        BuildError: The log could not be read.
    """
    keys = {
        f"docs/{md.relative_to(out).as_posix()}": md.relative_to(out).with_suffix("").as_posix()
        for md in out.rglob("*.md")
        if md.relative_to(out).with_suffix("").as_posix() not in generated
    }
    dated = page_dates(log, set(keys))
    pages = {keys[path]: date for path, date in sorted(dated.items(), key=lambda item: keys[item[0]])}
    document = {"version": LAST_UPDATED_VERSION, "pages": pages}
    (out / LAST_UPDATED_FILE).write_text(json.dumps(document, indent=1, sort_keys=False) + "\n", encoding="utf-8")
    return len(pages)


def build_rust_json(target_dir: Path) -> Path:
    """Generate rustdoc JSON for every workspace library on the pinned nightly.

    Binaries are left out: they have no API, and a binary named like its
    package's library would overwrite that library's JSON.

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
            "--lib",
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


@dataclass
class RustCrate:
    """A workspace library documented in the Rust reference.

    Attributes:
        name: rustdoc name (``bookclerk_media``).
        package: Cargo package name.
        description: The package's ``description``, or ``""``.
        group: Title from :data:`RUST_GROUPS`.
        manifest_dir: Its package directory, relative to the workspace root.
    """

    name: str
    package: str
    description: str
    group: str
    manifest_dir: str = ""


def rust_group(name: str, manifest_dir: str) -> str:
    """Return the reference group a workspace library belongs to.

    Args:
        name: rustdoc name of the library.
        manifest_dir: Its package directory, relative to the workspace root.

    Returns:
        A title from :data:`RUST_GROUPS`.
    """
    if name in RUST_GROUP_OF:
        return RUST_GROUP_OF[name]
    parts = Path(manifest_dir).parts
    if parts[:1] in (("examples",), ("tools",)):
        return "Development and testing"
    if parts[:2] == ("crates", "bookclerk-plugins"):
        return "Plugins"
    return "Host internals"


def workspace_libraries() -> dict[str, RustCrate]:
    """Return the workspace's library targets, keyed by rustdoc name.

    Returns:
        Library name (``-`` replaced by ``_``, as rustdoc names its JSON) ->
        crate.
    """
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    meta = json.loads(result.stdout)
    root = Path(meta["workspace_root"])
    libs = {}
    for package in meta["packages"]:
        manifest_dir = Path(package["manifest_path"]).parent.relative_to(root).as_posix()
        for target in package["targets"]:
            if set(target["kind"]) & {"lib", "proc-macro", "rlib"}:
                name = target["name"].replace("-", "_")
                group = rust_group(name, manifest_dir)
                libs[name] = RustCrate(
                    name,
                    package["name"],
                    package.get("description") or "",
                    group,
                    manifest_dir,
                )
    return libs


class Tree:
    """The output tree while the references are added to the hand-written pages.

    Args:
        out: The output root, holding the copy of ``docs/``.
    """

    def __init__(self, out: Path) -> None:
        """Start with the hand-written pages already in ``out``."""
        self.out = out
        #: Folder -> keys of the generated pages in it, in the order written.
        self.pages: dict[str, list[str]] = {}
        #: Folder of a generated crate -> its package directory.
        self.sources: dict[str, str] = {}
        #: The references built, in order.
        self.built: list[Reference] = []

    @property
    def keys(self) -> set[str]:
        """Keys of every generated page.

        Returns:
            Tree-relative paths without ``.md``.
        """
        return {key for keys in self.pages.values() for key in keys}

    def place(self, reference: Reference, rel: str, text: str, source: str | None = None) -> None:
        """Write one generated page into a reference's folder.

        Args:
            reference: The reference.
            rel: The page's path under the reference's folder.
            text: Its Markdown.
            source: For a crate page, the crate's package directory.

        Raises:
            BuildError: A hand-written page (or an earlier generated one)
                already sits at that path.
        """
        dest = self.out / reference.folder / rel
        if dest.exists():
            raise BuildError(
                f"{reference.folder}/{rel}: a page is already there; the {reference.title} reference writes this path, so move the hand-written page"
            )
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(text, encoding="utf-8")
        key = (Path(reference.folder) / rel).with_suffix("").as_posix()
        self.pages.setdefault(reference.folder, []).append(key)
        if source is not None:
            self.sources[key.removesuffix("/README")] = source
        if reference not in self.built:
            self.built.append(reference)

    def asset(self, reference: Reference, rel: str, data: bytes) -> None:
        """Write a file a reference carries beside its pages (an image TypeDoc copied).

        It is not a page, so the manifest does not list it.

        Args:
            reference: The reference.
            rel: The file's path under the reference's folder.
            data: Its bytes.

        Raises:
            BuildError: A file already sits at that path.
        """
        dest = self.out / reference.folder / rel
        if dest.exists():
            raise BuildError(f"{reference.folder}/{rel}: a file is already there; the {reference.title} reference writes this path")
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)

    def forget(self, reference: Reference, rel: str) -> None:
        """Drop a page the build deleted again from the manifest.

        Args:
            reference: The reference that wrote it.
            rel: The page's path under the reference's folder.
        """
        key = (Path(reference.folder) / rel).with_suffix("").as_posix()
        self.pages[reference.folder] = [k for k in self.pages.get(reference.folder, []) if k != key]

    def landing(self, reference: Reference, body: str) -> None:
        """Append the generated part of a reference to its hand-written landing page.

        Args:
            reference: A reference whose ``landing`` is true.
            body: Markdown to append: the module list and the package or
                crate root, without an H1.

        Raises:
            BuildError: ``docs/<folder>/README.md`` is missing.
        """
        readme = self.out / reference.folder / "README.md"
        if not readme.is_file():
            raise BuildError(f"docs/{reference.folder}/README.md is missing: the {reference.title} reference needs it as its landing page")
        text = readme.read_text(encoding="utf-8").rstrip()
        readme.write_text(f"{text}\n\n{body.strip()}\n", encoding="utf-8")
        if reference not in self.built:
            self.built.append(reference)


def without_title(page: str) -> str:
    """Drop a generated page's H1 so the page can join a landing page.

    Args:
        page: A page starting with ``# Title``.

    Returns:
        The page after its first line and the blank lines that follow.
    """
    lines = page.splitlines()
    if lines and lines[0].startswith("# "):
        lines = lines[1:]
    return "\n".join(lines).strip() + "\n"


def write_generated(out: Path, tree: Tree) -> None:
    """Write :data:`GENERATED_FILE`: the references built and their pages.

    The site reads it to tell a generated page from a hand-written one (the
    date, the "Generated from" line, the sidebar), to lay the Web API out
    as operations, and to link each crate's source.

    Args:
        out: The output root.
        tree: The tree after every reference was added.
    """
    references = [
        {
            "folder": r.folder,
            "kind": r.kind,
            "title": r.title,
            "source": r.source,
            "landing": r.landing,
            "pages": tree.pages.get(r.folder, []),
        }
        for r in tree.built
    ]
    document = {
        "version": GENERATED_VERSION,
        "references": references,
        "sources": dict(sorted(tree.sources.items())),
    }
    (out / GENERATED_FILE).write_text(json.dumps(document, indent=1) + "\n", encoding="utf-8")


def convert_rust(json_dir: Path, tree: Tree, libraries: dict[str, RustCrate]) -> list[str]:
    """Render the Rust references from rustdoc JSON.

    The SDK crate's pages go to the ``rust-sdk`` reference (its root joins
    the landing page) and every other crate to ``rust-crates``. Only files
    named after a current workspace library are read, so JSON left in a
    reused target directory by a renamed or removed crate, or by a binary,
    is ignored.

    Args:
        json_dir: Directory of ``<crate>.json`` files.
        tree: The output tree.
        libraries: Output of :func:`workspace_libraries`.

    Returns:
        The crate names rendered, sorted.

    Raises:
        BuildError: No JSON was found, a file has an unexpected format, or
            a page is in the way.
    """
    sdk = reference_of("rust-sdk")
    crates_ref = reference_of("rust-crates")
    try:
        crates = rustdoc_markdown.load_crates(json_dir, libraries, RUST_JSON_FORMAT)
    except ValueError as err:
        raise BuildError(str(err)) from err
    if not crates:
        raise BuildError(f"no rustdoc JSON in {json_dir}")
    folders = {name: sdk.folder if name == RUST_SDK_CRATE else f"{crates_ref.folder}/{name}" for name in crates}
    try:
        pages = rustdoc_markdown.Renderer(crates, folders).render_all()
    except ValueError as err:
        raise BuildError(f"Rust: {err}") from err
    landing = f"{sdk.folder}/README.md"
    for file, text in pages.items():
        if file == landing:
            tree.landing(sdk, without_title(text))
            continue
        reference = sdk if file.startswith(f"{sdk.folder}/") else crates_ref
        rel = file[len(reference.folder) + 1 :]
        # A crate's root page carries its package directory for the site's
        # "Generated from" link.
        name = rel[: -len("/README.md")] if reference is crates_ref and rel.count("/") == 1 and rel.endswith("/README.md") else None
        tree.place(
            reference,
            rel,
            text,
            libraries[name].manifest_dir or None if name is not None else None,
        )
    tree.place(
        crates_ref,
        "README.md",
        rust_index([libraries[name] for name in crates], crates_ref, sdk),
    )
    return sorted(crates)


def reference_of(kind: str) -> Reference:
    """Find the reference of a kind that :data:`REFERENCES` lists once.

    Args:
        kind: The reference kind.

    Returns:
        The reference.
    """
    return next(r for r in REFERENCES if r.kind == kind)


def rust_index(crates: list[RustCrate], reference: Reference, sdk: Reference) -> str:
    """Render the crates page, listing the crates by group.

    The plugin SDK crate is left out: the plugins shelf holds it.

    Args:
        crates: Crates rendered.
        reference: The ``rust-crates`` reference.
        sdk: The ``rust-sdk`` reference, linked for plugin authors.

    Returns:
        The page's Markdown.
    """
    to_sdk = os.path.relpath(sdk.folder, reference.folder).replace(os.sep, "/")
    lines = [
        "# Rust crates",
        "",
        "Generated from rustdoc JSON for the Bookclerk workspace's libraries.",
        f"To write a native plugin, use the [Rust plugin SDK]({to_sdk}/README.md).",
        "",
    ]
    for title, description in RUST_GROUPS:
        members = sorted(
            (c for c in crates if c.group == title and c.name != RUST_SDK_CRATE),
            key=lambda c: c.name,
        )
        if not members:
            continue
        lines += [
            f"## {title}",
            "",
            description,
            "",
            "| Crate | Description |",
            "| --- | --- |",
        ]
        for crate in members:
            text = crate.description.replace("|", "\\|")
            lines.append(f"| [`{crate.name}`]({crate.name}/README.md) | {text} |")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def build_typedoc(project: Path, out: Path) -> None:
    """Run TypeDoc's Markdown theme over one TypeScript project.

    The project's own TypeDoc, TypeScript and ``typedoc.json`` are used, so
    the reference matches what ``generate-api-docs.sh`` checks.

    Args:
        project: The TypeScript project directory, dependencies installed.
        out: A scratch folder for its pages.
    """
    cmd = [
        str(project / "node_modules" / ".bin" / "typedoc"),
        "--options",
        "typedoc.json",
        "--plugin",
        "typedoc-plugin-markdown",
        "--out",
        str(out),
        *TYPEDOC_MARKDOWN_OPTIONS,
    ]
    run(cmd, cwd=project)


def place_typedoc(scratch: Path, tree: Tree, reference: Reference) -> list[str]:
    """Move TypeDoc's pages into the tree and build the landing page's tail.

    Without a README, TypeDoc writes the module list as ``README.md``. That
    list goes to the landing page under "Modules", with the root module's
    page (its intro, the few items declared outside the other modules, and
    its re-exports as links) after it; every other module is a page.

    Args:
        scratch: Where :func:`build_typedoc` wrote.
        tree: The output tree.
        reference: The ``typescript`` reference.

    Returns:
        The module names placed as pages, sorted.

    Raises:
        BuildError: TypeDoc wrote no module list, the root module has no
            page, or a page is in the way.
    """
    index = scratch / "README.md"
    if not index.is_file():
        raise BuildError(f"{reference.title}: TypeDoc wrote no README.md")
    modules = [line for line in index.read_text(encoding="utf-8").splitlines() if line.startswith("- [")]
    root = ""
    if reference.root_module is not None:
        page = scratch / f"{reference.root_module}.md"
        if not page.is_file():
            raise BuildError(f"{reference.title}: no page for the root module {reference.root_module!r} (its `@module` tag names it)")
        root = without_title(page.read_text(encoding="utf-8"))
        page.unlink()
        modules = [m for m in modules if not m.endswith(f"]({reference.root_module}.md)")]
    intro, _, sections = root.partition("\n## ")
    body = "## Modules\n\n" + (f"{intro.strip()}\n\n" if intro.strip() else "") + "\n".join(modules) + "\n"
    if sections:
        body += f"\n## {sections}"
    tree.landing(reference, body)
    index.unlink()
    placed = []
    # The root module's page is now the landing page, so links to it
    # (`plugin-sdk.md#pluginerror` from a sibling) point at README.md.
    root_link = re.compile(rf"(\]\()(?:\./)?{re.escape(reference.root_module)}\.md(?=[#)])") if reference.root_module else None
    for md in sorted(scratch.rglob("*")):
        if not md.is_file():
            continue
        rel = md.relative_to(scratch).as_posix()
        if not rel.endswith(".md"):
            tree.asset(reference, rel, md.read_bytes())
            continue
        text = md.read_text(encoding="utf-8")
        if root_link is not None and "/" not in rel:
            text = root_link.sub(r"\1README.md", text)
        tree.place(reference, rel, text)
        placed.append(rel[: -len(".md")])
    return sorted(placed)


def link_media_to_docs(project_out: Path, out: Path, generated: set[str]) -> list[str]:
    """Point TypeDoc's copies of ``docs/`` pages back at the pages themselves.

    TypeDoc copies every Markdown file a doc comment links to into
    ``_media/``, where that copy's own relative links no longer resolve. A
    copy identical to a hand-written page of the docs tree is deleted and
    links to it are rewritten to the page.

    Args:
        project_out: One TypeDoc reference's folder in the tree.
        out: The output root.
        generated: Keys of the generated pages, which are not candidates.

    Returns:
        The deleted copies' paths under ``project_out``, for the caller to
        drop from the manifest.
    """
    media = project_out / "_media"
    if not media.is_dir():
        return 0
    docs = {}
    for path in out.rglob("*.md"):
        rel = path.relative_to(out)
        if rel.with_suffix("").as_posix() not in generated and media not in path.parents:
            docs.setdefault(path.read_bytes(), path)
    replaced = {}
    for copy in sorted(media.glob("*.md")):
        page = docs.get(copy.read_bytes())
        if page is not None:
            replaced[copy.resolve()] = page
    if not replaced:
        return []
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
    return sorted(copy.relative_to(project_out.resolve()).as_posix() for copy in replaced)


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


def build_python(src: Path, tree: Tree, reference: Reference) -> list[str]:
    """Render each public Python SDK module with :mod:`python_api_markdown`.

    The package's own page (its docstring, constants and re-exports) joins
    the landing page after a table of the modules.

    Args:
        src: The SDK's ``src`` directory.
        tree: The output tree.
        reference: The ``python`` reference.

    Returns:
        The modules rendered.

    Raises:
        BuildError: A module's ``__all__`` cannot be read without importing
            it, or a page is in the way.
    """
    modules = python_modules(src, PYTHON_PACKAGE)
    try:
        package = python_api_markdown.Package(src, modules)
        pages = package.render_all()
    except ValueError as err:
        raise BuildError(f"Python SDK: {err}") from err
    lines = ["## Modules", "", "| Module | Summary |", "| --- | --- |"]
    for module in modules:
        file = package.files.get(module)
        if file is not None and file != "README.md":
            lines.append(f"| [`{module}`]({file}) | {package.summary(module)} |")
    root = pages.get("README.md", "")
    body = "\n".join(lines) + "\n" + (f"\n{without_title(root)}" if root else "")
    tree.landing(reference, body)
    for rel, text in pages.items():
        if rel != "README.md":
            tree.place(reference, rel, text)
    return modules


def build_web(document: Path, scratch: Path, tree: Tree, reference: Reference) -> int:
    """Render the Web API reference and place its pages.

    Args:
        document: The OpenAPI JSON file.
        scratch: A scratch folder for the rendered pages.
        tree: The output tree.
        reference: The ``web-api`` reference.

    Returns:
        The number of operations rendered.

    Raises:
        BuildError: The document cannot be read, or a page is in the way.
    """
    try:
        operations = openapi_markdown.render(document, scratch)
    except (OSError, openapi_markdown.OpenApiError) as err:
        raise BuildError(f"web API: {err}") from err
    for page in sorted(scratch.rglob("*.md")):
        tree.place(
            reference,
            page.relative_to(scratch).as_posix(),
            page.read_text(encoding="utf-8"),
        )
    return operations


def index_pages(out: Path) -> list[str]:
    """Find pages the site could not serve.

    The site serves a folder's ``README.md`` at the folder's own URL and
    every page's Markdown copy at ``<page>/index.md``, so a page named
    ``index.md``, or any page inside a folder named ``index``, cannot be
    reached.

    Args:
        out: The output root.

    Returns:
        The path of each such page under the root, relative to it.
    """
    found = []
    for page in out.rglob("*.md"):
        rel = page.relative_to(out)
        if rel.name == "index.md" or "index" in rel.parts[:-1]:
            found.append(str(rel))
    return sorted(found)


def sibling_pages(out: Path) -> list[str]:
    """Find a page beside a folder of the same name.

    ``x.md`` and ``x/README.md`` would both be served at ``/…/x``; a page
    with children is its folder's ``README.md``.

    Args:
        out: The output root.

    Returns:
        The path of each such page under the root, relative to it.
    """
    found = []
    for page in out.rglob("*.md"):
        if page.with_suffix("").is_dir():
            found.append(page.relative_to(out).as_posix())
    return sorted(found)


def broken_links(out: Path, generated: set[str] = frozenset()) -> list[str]:
    """Find relative Markdown links that do not resolve to a file in the tree.

    A link inside the tree must name a file that exists. A link from a
    hand-written page out of the tree (to source files) is allowed, one from
    a generated page is not. Anchors are not checked: converters slug
    headings differently from the site. Links in fenced code are ignored.

    Args:
        out: The output root.
        generated: Keys of the generated pages.

    Returns:
        ``file: target`` for each broken link.
    """
    broken = []
    for md in sorted(out.rglob("*.md")):
        in_fence = False
        key = md.relative_to(out).with_suffix("").as_posix()
        for line in md.read_text(encoding="utf-8").splitlines():
            if FENCE_RE.match(line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            spans = rustdoc_markdown.code_spans(line)
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
                # Links from hand-written pages to repository files outside
                # docs/ are expected; the site rewrites them to GitHub.
                if not inside and key not in generated:
                    continue
                broken.append(f"{md.relative_to(out)}: {href}")
    return broken


def check_tree(out: Path, tree: Tree) -> None:
    """Refuse a tree the site could not serve as it is.

    Args:
        out: The output root.
        tree: The tree after every reference was added.

    Raises:
        BuildError: A page is named ``index.md`` or sits in an ``index``
            folder, a page sits beside a folder of its name, or a relative
            link is broken.
    """
    indexes = index_pages(out)
    if indexes:
        raise BuildError(f"pages the site cannot serve at a URL (name them README.md): {', '.join(indexes)}")
    siblings = sibling_pages(out)
    if siblings:
        raise BuildError(f"pages beside a folder of the same name (make them the folder's README.md): {', '.join(siblings)}")
    broken = broken_links(out, tree.keys)
    if broken:
        for line in broken[:50]:
            print(f"broken link: {line}", file=sys.stderr)
        raise BuildError(f"{len(broken)} broken relative links in the tree")


def main(argv: list[str] | None = None) -> int:
    """Run the CLI.

    Args:
        argv: Arguments, without the program name.

    Returns:
        The process exit status.
    """
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="output directory (cleared first)")
    parser.add_argument(
        "--scratch-root",
        type=Path,
        default=DEFAULT_SCRATCH,
        help="directory --out must be inside",
    )
    parser.add_argument(
        "--rust-json-dir",
        type=Path,
        help="use existing rustdoc JSON instead of building it",
    )
    parser.add_argument("--skip-rust", action="store_true")
    parser.add_argument("--skip-typescript", action="store_true")
    parser.add_argument("--skip-python", action="store_true")
    parser.add_argument("--skip-web", action="store_true")
    parser.add_argument(
        "--require-history",
        action="store_true",
        help=f"fail instead of leaving out {LAST_UPDATED_FILE} when the git history is missing or shallow",
    )
    args = parser.parse_args(argv)

    try:
        out = prepare_output_dir(args.out, args.scratch_root)
        scratch = args.scratch_root.resolve()
        copy_docs(ROOT / "docs", out)
        tree = Tree(out)

        if not args.skip_typescript:
            for reference in (r for r in REFERENCES if r.kind == "typescript"):
                project = ROOT / reference.project
                if not (project / "node_modules").is_dir():
                    run(["npm", "ci", "--no-audit", "--no-fund"], cwd=project)
                with tempfile.TemporaryDirectory(prefix="typedoc-", dir=scratch) as tmp:
                    build_typedoc(project, Path(tmp))
                    modules = place_typedoc(Path(tmp), tree, reference)
                for copy in link_media_to_docs(out / reference.folder, out, tree.keys):
                    tree.forget(reference, copy)
                print(f"==> {reference.title}: {len(modules)} modules")

        if not args.skip_python:
            modules = build_python(ROOT / PYTHON_SDK / "src", tree, reference_of("python"))
            print(f"==> Python: {len(modules)} modules")

        if not args.skip_web:
            with tempfile.TemporaryDirectory(prefix="openapi-", dir=scratch) as tmp:
                operations = build_web(ROOT / OPENAPI_DOCUMENT, Path(tmp), tree, reference_of("web-api"))
            print(f"==> Web API: {operations} operations")

        if not args.skip_rust:
            json_dir = args.rust_json_dir.resolve() if args.rust_json_dir else build_rust_json(scratch / "cargo-target")
            crates = convert_rust(json_dir, tree, workspace_libraries())
            print(f"==> Rust: {len(crates)} crates")

        write_generated(out, tree)
        try:
            dated = write_last_updated(out, git_docs_log(ROOT), tree.keys)
            print(f"==> Last updated: {dated} pages")
        except NoHistory as err:
            if args.require_history:
                raise
            print(f"::warning::no {LAST_UPDATED_FILE}: {err}", file=sys.stderr)
        check_tree(out, tree)
        files = sum(1 for p in out.rglob("*") if p.is_file())
        print(f"==> wrote {files} files under {out}")
    except BuildError as err:
        print(f"::error::{err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
