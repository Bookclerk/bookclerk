#!/usr/bin/env python3
"""Build the Markdown docs tree published to the docs repository.

The tree holds ``docs/`` as it is, plus the generated API reference under
``api/``, ordered for plugin authors first:

- ``api/typescript/``: the TypeScript plugin SDK, from the project's own
  TypeDoc with ``typedoc-plugin-markdown`` and its ``typedoc.json``;
- ``api/python/``: the Python plugin SDK, read with :mod:`ast` by
  ``python_api_markdown.py`` (nothing in the package is imported);
- ``api/rust/``: every workspace library crate, from rustdoc JSON (a pinned
  nightly) rendered by ``rustdoc_markdown.py``, grouped into the plugin SDK,
  plugins, host internals and development crates;
- ``api/web/``: the daemon's HTTP API, from its committed OpenAPI document
  (``crates/bookclerkd/openapi.json``) rendered by ``openapi_markdown.py``;
- ``api/ui/``: the web UI library, like the TypeScript SDK.

`scripts/docs-publish.py` commits the tree to the docs repository. Every
relative link in the tree must resolve, or the build fails.

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

# TypeDoc projects: (output folder under api/, project directory).
TYPEDOC_PROJECTS = (("typescript", "packages/plugin-sdk"), ("ui", "ui"))
# Markdown-theme options for both: no GitHub source links (the docs are read
# on the site), no per-page package banner or breadcrumbs, one page per
# module rather than per symbol, and parameters as tables.
TYPEDOC_MARKDOWN_OPTIONS = (
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
# The daemon's OpenAPI document, regenerated and checked by bookclerkd's tests.
OPENAPI_DOCUMENT = "crates/bookclerkd/openapi.json"
PYTHON_SDK = "packages/plugin-sdk-python"
PYTHON_PACKAGE = "bookclerk_plugin_sdk"

# Groups of the Rust reference, in index order: (title, description).
# A crate's group comes from RUST_GROUP_OF, else from its directory.
RUST_GROUPS = (
    ("Plugin interface", "The plugin ABI, the `plugin.toml` manifest and the plugin author CLI."),
    ("Plugins", "First-party plugins: databases, destinations, sources and integrations."),
    ("Host internals", "The crates the hosts are built from. Not a stable API."),
    ("Development and testing", "Build tooling, test suites and examples."),
)
# The Rust plugin SDK itself is listed with the other SDKs (RUST_SDK_CRATE).
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
# A link to TypeDoc's modules.md, at any depth, with an optional anchor.
MODULES_LINK_RE = re.compile(r"\]\(((?:\.\./)*)modules\.md(#[^)\s]*)?\)")
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
    """

    name: str
    package: str
    description: str
    group: str


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
                libs[name] = RustCrate(name, package["name"], package.get("description") or "", group)
    return libs


def convert_rust(json_dir: Path, out: Path, libraries: dict[str, RustCrate]) -> list[str]:
    """Render the Rust reference from rustdoc JSON.

    Only files named after a current workspace library are read, so JSON
    left in a reused target directory by a renamed or removed crate, or by a
    binary, is ignored.

    Args:
        json_dir: Directory of ``<crate>.json`` files.
        out: ``api/rust`` in the output tree.
        libraries: Output of :func:`workspace_libraries`.

    Returns:
        The crate names rendered, sorted.

    Raises:
        BuildError: No JSON was found, or a file has an unexpected format.
    """
    try:
        crates = rustdoc_markdown.load_crates(json_dir, libraries, RUST_JSON_FORMAT)
    except ValueError as err:
        raise BuildError(str(err)) from err
    if not crates:
        raise BuildError(f"no rustdoc JSON in {json_dir}")
    pages = rustdoc_markdown.Renderer(crates).render_all()
    for rel, text in pages.items():
        dest = out / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(text, encoding="utf-8")
    write_rust_index(out, [libraries[name] for name in crates])
    return sorted(crates)


def write_rust_index(rust_out: Path, crates: list[RustCrate]) -> None:
    """Write ``api/rust/README.md``, listing the crates by group.

The plugin SDK crate is left out: ``api/README.md`` lists it with the other
plugin SDKs.

    Args:
        rust_out: ``api/rust`` in the output tree.
        crates: Crates rendered.
    """
    lines = [
        "# Rust API reference",
        "",
        "Generated from rustdoc JSON for the Bookclerk workspace's libraries.",
        "To write a native plugin, use the [Rust plugin SDK](bookclerk_plugin_sdk/index.md).",
        "",
    ]
    for title, description in RUST_GROUPS:
        members = sorted((c for c in crates if c.group == title and c.name != RUST_SDK_CRATE), key=lambda c: c.name)
        if not members:
            continue
        lines += [f"## {title}", "", description, "", "| Crate | Description |", "| --- | --- |"]
        for crate in members:
            text = crate.description.replace("|", "\\|")
            lines.append(f"| [`{crate.name}`]({crate.name}/index.md) | {text} |")
        lines.append("")
    (rust_out / "README.md").write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


def build_typedoc(project: Path, out: Path) -> None:
    """Run TypeDoc's Markdown theme over one TypeScript project.

    The project's own TypeDoc, TypeScript and ``typedoc.json`` are used, so
    the reference matches what ``generate-api-docs.sh`` checks.

    Args:
        project: The TypeScript project directory, dependencies installed.
        out: Its folder under ``api/``.
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


def fold_module_index(project_out: Path) -> None:
    """Put TypeDoc's module list on the project's landing page.

    With a project README, TypeDoc writes the README as ``README.md`` and the
    module list as ``modules.md``, which nothing on the landing page links
    to. The list is appended to the README under "Modules" and links to
    ``modules.md`` are pointed at the README.

    Args:
        project_out: One TypeDoc output folder under ``api/``.
    """
    readme = project_out / "README.md"
    modules = project_out / "modules.md"
    if not readme.is_file() or not modules.is_file():
        return
    entries = [line for line in modules.read_text(encoding="utf-8").splitlines() if line.startswith("- [")]
    text = readme.read_text(encoding="utf-8").rstrip()
    readme.write_text(text + "\n\n## Modules\n\n" + "\n".join(entries) + "\n", encoding="utf-8")
    modules.unlink()
    target = modules.resolve()
    for md in project_out.rglob("*.md"):
        page = md.read_text(encoding="utf-8")

        def point_at_readme(match: re.Match[str], md: Path = md) -> str:
            rel, anchor = match.group(1), match.group(2)
            if (md.parent / f"{rel}modules.md").resolve() != target:
                return match.group(0)
            return f"]({rel}README.md{'#modules' if anchor else ''})"

        new = MODULES_LINK_RE.sub(point_at_readme, page)
        if new != page:
            md.write_text(new, encoding="utf-8")


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


def build_python(src: Path, out: Path) -> list[str]:
    """Render each public Python SDK module with :mod:`python_api_markdown`.

    Args:
        src: The SDK's ``src`` directory.
        out: ``api/python`` in the output tree.

    Returns:
        The modules rendered.

    Raises:
        BuildError: A module's ``__all__`` cannot be read without importing it.
    """
    out.mkdir(parents=True, exist_ok=True)
    modules = python_modules(src, PYTHON_PACKAGE)
    try:
        package = python_api_markdown.Package(src, modules)
        pages = package.render_all()
    except ValueError as err:
        raise BuildError(f"Python SDK: {err}") from err
    for rel, text in pages.items():
        (out / rel).write_text(text, encoding="utf-8")
    lines = [
        "# Python plugin SDK",
        "",
        "The `bookclerk-plugin-sdk` package. Start with",
        f"[`{PYTHON_PACKAGE}.workerd`]({PYTHON_PACKAGE}.workerd.md), which holds the entrypoint classes a plugin extends.",
        "",
        "| Module | Summary |",
        "| --- | --- |",
    ]
    for module in modules:
        page = out / f"{module}.md"
        if page.is_file():
            lines.append(f"| [`{module}`]({module}.md) | {package.summary(module)} |")
    (out / "README.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return modules


# api/README.md, in order: (section, [(page under api/, title, description)]).
# A reference that was not built is left out, and so is an empty section.
API_INDEX = (
    (
        "Plugin SDKs",
        (
            ("typescript/README.md", "TypeScript", "`@bookclerk/plugin-sdk`, for plugins that run in workerd"),
            ("python/README.md", "Python", "`bookclerk-plugin-sdk`, for Python plugins that run in workerd"),
            (f"rust/{RUST_SDK_CRATE}/index.md", "Rust", f"`{RUST_SDK_CRATE}`, for native plugins"),
        ),
    ),
    ("Web API", (("web/README.md", "bookclerkd HTTP API", "the endpoints the daemon serves"),)),
    (
        "Rust crates",
        (("rust/README.md", "Rust crates", "the plugin ABI and manifest, first-party plugins, and the host's crates"),),
    ),
    ("Web UI", (("ui/README.md", "Web UI library", "`ui/src/lib`, the web UI's API client and helpers"),)),
)


def write_api_index(api_out: Path) -> None:
    """Write ``api/README.md``, linking each reference that was built.

    Args:
        api_out: ``api`` in the output tree.
    """
    lines = [
        "# API reference",
        "",
        "Generated from the source at the same version as these docs.",
        "Writing a plugin? Start with the SDK for its language; [Plugins](../plugins/README.md) explains the model.",
        "",
    ]
    for section, pages in API_INDEX:
        built = [(page, title, description) for page, title, description in pages if (api_out / page).is_file()]
        if not built:
            continue
        lines += [f"## {section}", ""]
        lines += [f"- [{title}]({page}): {description}" for page, title, description in built]
        lines.append("")
    (api_out / "README.md").write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


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
    parser.add_argument("--skip-rust", action="store_true")
    parser.add_argument("--skip-typescript", action="store_true")
    parser.add_argument("--skip-python", action="store_true")
    parser.add_argument("--skip-web", action="store_true")
    args = parser.parse_args(argv)

    try:
        out = prepare_output_dir(args.out, args.scratch_root)
        scratch = args.scratch_root.resolve()
        copy_docs(ROOT / "docs", out)
        api = out / "api"
        api.mkdir()

        if not args.skip_typescript:
            for folder, project in TYPEDOC_PROJECTS:
                if not (ROOT / project / "node_modules").is_dir():
                    run(["npm", "ci", "--no-audit", "--no-fund"], cwd=ROOT / project)
                build_typedoc(ROOT / project, api / folder)
                fold_module_index(api / folder)
                link_media_to_docs(api / folder, out)

        if not args.skip_python:
            modules = build_python(ROOT / PYTHON_SDK / "src", api / "python")
            print(f"==> Python: {len(modules)} modules")

        if not args.skip_web:
            try:
                operations = openapi_markdown.render(ROOT / OPENAPI_DOCUMENT, api / "web")
            except (OSError, openapi_markdown.OpenApiError) as err:
                raise BuildError(f"web API: {err}") from err
            print(f"==> Web API: {operations} operations")

        if not args.skip_rust:
            json_dir = args.rust_json_dir.resolve() if args.rust_json_dir else build_rust_json(scratch / "cargo-target")
            crates = convert_rust(json_dir, api / "rust", workspace_libraries())
            print(f"==> Rust: {len(crates)} crates")

        write_api_index(api)
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
