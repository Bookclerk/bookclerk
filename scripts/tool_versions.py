#!/usr/bin/env python3
"""Reads and bumps the repository's central tool pins in ``.tool-versions``.

``.tool-versions`` is the one pin for every tool version that crosses a
boundary between workflows, images and scripts (see
``docs/contributing/ci.md#tool-versions``). ``actions/setup-node`` and
``astral-sh/setup-uv`` read it themselves; other consumers go through this
script or, in shell, through ``scripts/dev-env/provision.sh``.

The scheduled ``tool-versions-bump.yml`` workflow feeds this script a GitHub
releases listing (bytes the host did not produce) and a registry digest, so
both are parsed here, sans-IO, before anything reaches a file or
``$GITHUB_OUTPUT``.

Usage:

``tool_versions.py github-output``
    Prints ``<tool>=<version>`` for each pin, for ``$GITHUB_OUTPUT``.
``tool_versions.py bumpable``
    Prints, one per line, each tool the bump workflow moves.
``tool_versions.py repo <tool>``
    Prints the tool's release repository. Exits 1 for a tool that is bumped
    by hand.
``tool_versions.py image-ref <tool> <version>``
    Prints ``<image>:<tag>`` for that version of the tool's image, or nothing
    when the tool has none.
``tool_versions.py npm-dir <tool>``
    Prints the directory whose ``package.json`` pins the tool's npm packages,
    or nothing when it has none. The bump workflow regenerates that
    directory's lockfile after ``apply``.
``tool_versions.py select <tool> --releases FILE [--cooldown-days N]``
    Prints the newest release older than the cooldown that is newer than the
    pin. Exits 2 when there is none.
``tool_versions.py prune-rust``
    Uninstalls every rustup toolchain except the pinned ``rust`` one. CI
    runs it after installing the pin: runner images preinstall other
    toolchains, and the Rust cache key hashes every installed one, so an
    image that ships a different set would otherwise change the key.
``tool_versions.py apply <tool> <version> [--digest sha256:<hex>]``
    Writes the new pin; the tool's image reference in every Dockerfile that
    builds from it and every workflow ``image:`` that runs in it
    (``--digest`` is then required); its exact npm pins in its
    ``package.json``; and, for ``rust``, the ``channel`` in
    ``rust-toolchain.toml``.

Exit codes: 0 success, 1 error, 2 nothing to bump.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOL_VERSIONS = ROOT / ".tool-versions"

#: rustup's toolchain file. rustup cannot read ``.tool-versions``, so its
#: ``channel`` repeats the ``rust`` pin and ``apply rust`` rewrites both.
RUST_TOOLCHAIN = ROOT / "rust-toolchain.toml"

#: Largest releases listing read, in bytes (100 releases are well under it).
MAX_RELEASES_BYTES = 16 * 1024 * 1024

#: Default supply-chain cooldown: a release must be at least this many days old.
DEFAULT_COOLDOWN_DAYS = 7

#: A pinned exact version: three ASCII numbers without leading zeros.
EXACT_VERSION = re.compile(r"(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})\.(0|[1-9][0-9]{0,8})", re.ASCII)

#: A pinned major version, for tools that follow a major line (Node.js).
MAJOR_VERSION = re.compile(r"[1-9][0-9]{0,3}", re.ASCII)

#: An image digest as a registry reports it.
DIGEST = re.compile(r"sha256:[0-9a-f]{64}", re.ASCII)

#: A release's ``published_at``: RFC 3339 in UTC, as the GitHub API writes it.
PUBLISHED_AT = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", re.ASCII)


@dataclass(frozen=True)
class Tool:
    """One pinned tool and how it is bumped.

    Attributes:
        name: The tool's name in ``.tool-versions``.
        version: Pattern a pin must match in full.
        repo: GitHub ``owner/name`` whose releases the bump workflow reads, or
            ``None`` when the pin is bumped by hand.
        tag_prefix: Text before the version in the repository's release tags.
        image: Registry image tagged with the pinned version, or ``None``.
        image_tag: The image's tag for a version, with ``{version}`` standing
            for the pin.
        npm_dir: Directory, relative to the repository root, whose
            ``package.json`` pins ``npm_packages``, or ``None``.
        npm_packages: npm packages pinned to exactly the tool's version in
            ``npm_dir``'s ``package.json`` (as dependencies or overrides).
    """

    name: str
    version: re.Pattern[str]
    repo: str | None
    tag_prefix: str
    image: str | None
    image_tag: str = "{version}"
    npm_dir: str | None = None
    npm_packages: tuple[str, ...] = ()

    def image_ref(self, version: str) -> str | None:
        """Names the tool's image at ``version``.

        Args:
            version: An exact version of the tool.

        Returns:
            ``<image>:<tag>``, or ``None`` when the tool has no image.
        """
        if self.image is None:
            return None
        return f"{self.image}:{self.image_tag.format(version=version)}"


#: Every pin ``.tool-versions`` may hold, in the order the file lists them.
#: ``nodejs`` stays first: ``actions/setup-node`` reads the first line that
#: looks like a version. Node.js majors are a decision, so they move by hand.
#: Playwright's image only ships its own release's browsers, so the image the
#: UI visual jobs run in, ``@playwright/test`` and the ``playwright-core``
#: override in ``ui/`` move together.
TOOLS = (
    Tool("nodejs", MAJOR_VERSION, None, "", None),
    Tool("uv", EXACT_VERSION, "astral-sh/uv", "", "ghcr.io/astral-sh/uv"),
    Tool("cargo-nextest", EXACT_VERSION, "nextest-rs/nextest", "cargo-nextest-", None),
    Tool("rust", EXACT_VERSION, "rust-lang/rust", "", None),
    Tool(
        "playwright",
        EXACT_VERSION,
        "microsoft/playwright",
        "v",
        "mcr.microsoft.com/playwright",
        image_tag="v{version}-noble",
        npm_dir="ui",
        npm_packages=("@playwright/test", "playwright-core"),
    ),
)

TOOLS_BY_NAME = {tool.name: tool for tool in TOOLS}


class ToolVersionsError(ValueError):
    """A pin file, releases listing or digest is not what this script accepts."""


def parse_pins(text: str) -> dict[str, str]:
    """Parses ``.tool-versions`` into tool name → version.

    Args:
        text: The file's contents.

    Returns:
        Each pin, in file order.

    Raises:
        ToolVersionsError: A line is not ``<tool> <version>``, names an unknown
            or repeated tool, or the tools are not exactly ``TOOLS`` in order.
    """
    pins: dict[str, str] = {}
    for number, line in enumerate(text.splitlines(), 1):
        fields = line.split(" ")
        if len(fields) != 2:
            raise ToolVersionsError(f"line {number}: expected `<tool> <version>`")
        name, version = fields
        tool = TOOLS_BY_NAME.get(name)
        if tool is None:
            raise ToolVersionsError(f"line {number}: unknown tool {name!r}")
        if name in pins:
            raise ToolVersionsError(f"line {number}: {name} pinned twice")
        if tool.version.fullmatch(version) is None:
            raise ToolVersionsError(f"line {number}: bad {name} version {version!r}")
        pins[name] = version
    if list(pins) != [tool.name for tool in TOOLS]:
        raise ToolVersionsError("pins must be " + ", ".join(tool.name for tool in TOOLS) + ", in that order")
    return pins


def render_pins(pins: dict[str, str]) -> str:
    """Formats pins as ``.tool-versions`` text, in ``TOOLS`` order.

    Args:
        pins: Tool name → version, one per tool in ``TOOLS``.

    Returns:
        The file's contents, one ``<tool> <version>`` line each.
    """
    return "".join(f"{tool.name} {pins[tool.name]}\n" for tool in TOOLS)


def _version_key(version: str) -> tuple[int, ...]:
    """Orders exact versions numerically."""
    return tuple(int(part) for part in version.split("."))


def select_release(tool: Tool, raw: bytes, current: str, now: datetime, cooldown_days: int) -> str | None:
    """Picks the version to bump ``tool`` to from a GitHub releases listing.

    Only published, non-prerelease releases whose tag is the tool's prefix and
    an exact version, published at least ``cooldown_days`` before ``now``,
    count. Anything else in the listing is skipped; a listing that is not the
    expected shape is refused.

    Args:
        tool: The tool being bumped; its ``repo`` must be set.
        raw: The ``GET /repos/{repo}/releases`` response body.
        current: The pinned version.
        now: The current time, timezone-aware.
        cooldown_days: Minimum release age in days.

    Returns:
        The newest eligible version above ``current``, or ``None``.

    Raises:
        ToolVersionsError: The listing is too large, not UTF-8 JSON, not a list
            of objects, or an eligible-looking release has malformed fields.
    """
    if len(raw) > MAX_RELEASES_BYTES:
        raise ToolVersionsError("releases listing too large")
    try:
        releases = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as err:
        raise ToolVersionsError("releases listing is not UTF-8 JSON") from err
    if not isinstance(releases, list):
        raise ToolVersionsError("releases listing is not a list")
    cutoff = now - timedelta(days=cooldown_days)
    best: str | None = None
    for release in releases:
        if not isinstance(release, dict):
            raise ToolVersionsError("release is not an object")
        tag = release.get("tag_name")
        if not isinstance(tag, str) or not tag.startswith(tool.tag_prefix):
            continue
        version = tag[len(tool.tag_prefix) :]
        if EXACT_VERSION.fullmatch(version) is None:
            continue
        if release.get("draft") is not False or release.get("prerelease") is not False:
            continue
        published = release.get("published_at")
        if not isinstance(published, str) or PUBLISHED_AT.fullmatch(published) is None:
            raise ToolVersionsError(f"release {tag!r} has no valid published_at")
        try:
            published_at = datetime.strptime(published, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
        except ValueError as err:
            raise ToolVersionsError(f"release {tag!r} has no valid published_at") from err
        if published_at > cutoff:
            continue
        if best is None or _version_key(version) > _version_key(best):
            best = version
    if best is None or _version_key(best) <= _version_key(current):
        return None
    return best


#: Largest digest read, in bytes; a sha256 digest is 71.
MAX_DIGEST_BYTES = 256


def parse_digest(raw: bytes) -> str:
    """Reads an image digest as the registry tool printed it.

    One trailing newline is allowed; anything else must be exactly
    ``sha256:`` and 64 lowercase hex digits.

    Args:
        raw: The digest bytes.

    Returns:
        The ``sha256:<hex>`` digest.

    Raises:
        ToolVersionsError: The input is too long, not ASCII, or not a sha256
            digest.
    """
    if len(raw) > MAX_DIGEST_BYTES:
        raise ToolVersionsError("digest too long")
    if raw.endswith(b"\n"):
        raw = raw[:-1]
    try:
        text = raw.decode("ascii")
    except UnicodeDecodeError as err:
        raise ToolVersionsError("digest is not ASCII") from err
    if DIGEST.fullmatch(text) is None:
        raise ToolVersionsError("digest must be sha256:<64 lowercase hex>")
    return text


def rewrite_image(text: str, image: str, tag: str, digest: str) -> str:
    """Points every ``FROM <image>:<tag>@<digest>`` line at a new tag and digest.

    Args:
        text: A Dockerfile's contents.
        image: The image name, e.g. ``ghcr.io/astral-sh/uv``.
        tag: The new tag.
        digest: The new ``sha256:<hex>`` digest.

    Returns:
        The rewritten contents; unchanged when no line builds from ``image``.
    """
    pattern = re.compile(rf"^(FROM\s+(?:--\S+\s+)*){re.escape(image)}:[^\s@]+@sha256:[0-9a-f]{{64}}(?=\s|$)", re.MULTILINE)
    return pattern.sub(lambda m: f"{m.group(1)}{image}:{tag}@{digest}", text)


def rewrite_workflow_image(text: str, image: str, tag: str, digest: str) -> str:
    """Points every workflow ``image: <image>:<tag>@<digest>`` line at a new tag and digest.

    Args:
        text: A workflow's contents.
        image: The image name, e.g. ``mcr.microsoft.com/playwright``.
        tag: The new tag.
        digest: The new ``sha256:<hex>`` digest.

    Returns:
        The rewritten contents; unchanged when no job runs in ``image``.
    """
    pattern = re.compile(rf"^(\s*image:\s+){re.escape(image)}:[^\s@]+@sha256:[0-9a-f]{{64}}(?=\s|$)", re.MULTILINE)
    return pattern.sub(lambda m: f"{m.group(1)}{image}:{tag}@{digest}", text)


def rewrite_npm_pins(text: str, packages: tuple[str, ...], version: str) -> str:
    """Points every exact ``"<package>": "<version>"`` entry of a ``package.json`` at a new version.

    Dependencies and ``overrides`` entries are rewritten alike; ranges such
    as ``^1.2.3`` are not pins, so they don't count.

    Args:
        text: The ``package.json`` contents.
        packages: The npm package names to move.
        version: The new exact version.

    Returns:
        The rewritten contents.

    Raises:
        ToolVersionsError: A package has no exact pin in the file.
    """
    for package in packages:
        pattern = re.compile(rf'(?P<key>"{re.escape(package)}":\s*")(?:{EXACT_VERSION.pattern})(?=")')
        text, count = pattern.subn(lambda m: f"{m.group('key')}{version}", text)
        if count == 0:
            raise ToolVersionsError(f"package.json has no exact {package} pin")
    return text


#: The ``channel = "..."`` line of ``rust-toolchain.toml``'s ``[toolchain]`` table.
TOOLCHAIN_CHANNEL = re.compile(r'^channel = "([^"\n]*)"$', re.MULTILINE)


def toolchain_channel(text: str) -> str:
    """Reads the ``channel`` that ``rust-toolchain.toml`` asks rustup for.

    Args:
        text: The toolchain file's contents.

    Returns:
        The channel, e.g. ``1.100.0``.

    Raises:
        ToolVersionsError: The file has no ``channel = "..."`` line, or more
            than one.
    """
    found = TOOLCHAIN_CHANNEL.findall(text)
    if len(found) != 1:
        raise ToolVersionsError('rust-toolchain.toml needs exactly one `channel = "..."` line')
    return found[0]


def rewrite_toolchain_channel(text: str, version: str) -> str:
    """Points ``rust-toolchain.toml``'s ``channel`` at a new Rust release.

    Args:
        text: The toolchain file's contents.
        version: The new exact Rust version.

    Returns:
        The rewritten contents.

    Raises:
        ToolVersionsError: The file does not have exactly one ``channel`` line.
    """
    toolchain_channel(text)
    return TOOLCHAIN_CHANNEL.sub(f'channel = "{version}"', text)


def extra_toolchains(listing: str, pin: str) -> list[str]:
    """Picks the toolchains in a ``rustup toolchain list`` output that aren't the pin.

    rustup names a pinned install ``<version>-<host triple>`` and writes one
    toolchain per line, optionally followed by a parenthesized note such as
    ``(default)`` or ``(active, default)``. Any other name (``stable-...``,
    another version, a nightly) is extra.

    Args:
        listing: The command's standard output.
        pin: The pinned ``rust`` version, such as ``1.100.0``.

    Returns:
        The extra toolchain names, in listing order.

    Raises:
        ToolVersionsError: When no listed toolchain is the pin, so pruning
            would leave cargo without the pinned compiler.
    """
    names = [line.split()[0] for line in listing.splitlines() if line.strip()]
    pinned = [name for name in names if name == pin or name.startswith(f"{pin}-")]
    if not pinned:
        raise ToolVersionsError(f"rust {pin} is not installed; install it before pruning")
    return [name for name in names if name not in pinned]


def _tracked(*pathspecs: str) -> list[Path]:
    """Lists the files git tracks that match ``pathspecs``."""
    out = subprocess.run(
        ["git", "ls-files", "-z", "--", *pathspecs], cwd=ROOT, capture_output=True, check=True
    ).stdout.decode("utf-8")
    return [ROOT / name for name in out.split("\0") if name]


def _tracked_dockerfiles() -> list[Path]:
    """Lists the Dockerfiles git tracks (``*Dockerfile``, as the bump workflow commits)."""
    return _tracked("*Dockerfile")


def _tracked_workflows() -> list[Path]:
    """Lists the workflows git tracks (``.github/workflows/*.yml``, as the bump workflow commits)."""
    return _tracked(".github/workflows/*.yml")


def _cooldown(text: str) -> int:
    """Parses ``--cooldown-days``: a whole number of days, at least 1."""
    if not text.isascii() or not text.isdigit() or int(text) < 1:
        raise argparse.ArgumentTypeError("cooldown must be a whole number of days, at least 1")
    return int(text)


def _tool(name: str) -> Tool:
    """Looks up a tool by name, as an argparse type."""
    if name not in TOOLS_BY_NAME:
        raise argparse.ArgumentTypeError(f"unknown tool {name!r}")
    return TOOLS_BY_NAME[name]


def main(argv: list[str]) -> int:
    """Runs one subcommand; see the module docstring.

    Args:
        argv: Command-line arguments, without the program name.

    Returns:
        The exit code.
    """
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("github-output", help="print <tool>=<version> lines")
    commands.add_parser("bumpable", help="list the tools the bump workflow moves")
    commands.add_parser("prune-rust", help="uninstall every toolchain but the pinned rust")
    for name in ("repo", "npm-dir"):
        command = commands.add_parser(name, help=f"print a tool's {name}")
        command.add_argument("tool", type=_tool)
    image_ref = commands.add_parser("image-ref", help="print <image>:<tag> for a version of a tool's image")
    image_ref.add_argument("tool", type=_tool)
    image_ref.add_argument("version")
    select = commands.add_parser("select", help="pick an eligible release")
    select.add_argument("tool", type=_tool)
    select.add_argument("--releases", type=Path, required=True, help="GitHub releases listing (JSON)")
    select.add_argument("--cooldown-days", type=_cooldown, default=DEFAULT_COOLDOWN_DAYS)
    apply = commands.add_parser("apply", help="write a new pin")
    apply.add_argument("tool", type=_tool)
    apply.add_argument("version")
    apply.add_argument("--digest", help="the tool image's sha256 digest for the new version")
    try:
        args = parser.parse_args(argv)
    except SystemExit as exit_:
        # argparse exits 2 on a usage error, which callers read as "nothing
        # to bump"; report it as an error instead.
        return 0 if exit_.code == 0 else 1

    try:
        pins = parse_pins(TOOL_VERSIONS.read_text(encoding="utf-8"))
        if args.command == "github-output":
            for name, version in pins.items():
                print(f"{name}={version}")
            return 0
        if args.command == "bumpable":
            for tool in TOOLS:
                if tool.repo is not None:
                    print(tool.name)
            return 0
        if args.command == "prune-rust":
            listing = subprocess.run(
                ["rustup", "toolchain", "list"], check=True, capture_output=True, text=True
            ).stdout
            for name in extra_toolchains(listing, pins["rust"]):
                print(f"removing toolchain {name}", file=sys.stderr)
                subprocess.run(["rustup", "toolchain", "uninstall", name], check=True)
            return 0
        tool: Tool = args.tool
        if args.command in ("repo", "select", "apply") and tool.repo is None:
            raise ToolVersionsError(f"{tool.name} is bumped by hand")
        if args.command == "repo":
            print(tool.repo)
            return 0
        if args.command == "npm-dir":
            print(tool.npm_dir or "")
            return 0
        if args.command == "image-ref":
            if tool.version.fullmatch(args.version) is None:
                raise ToolVersionsError(f"bad {tool.name} version {args.version!r}")
            print(tool.image_ref(args.version) or "")
            return 0
        if args.command == "select":
            with args.releases.open("rb") as handle:
                raw = handle.read(MAX_RELEASES_BYTES + 1)
            chosen = select_release(tool, raw, pins[tool.name], datetime.now(timezone.utc), args.cooldown_days)
            if chosen is None:
                print(f"{tool.name} {pins[tool.name]} is current (cooldown {args.cooldown_days}d)", file=sys.stderr)
                return 2
            print(chosen)
            return 0
        if tool.version.fullmatch(args.version) is None:
            raise ToolVersionsError(f"bad {tool.name} version {args.version!r}")
        if tool.image is not None and args.digest is None:
            raise ToolVersionsError(f"{tool.name} needs --digest sha256:<64 hex> for {tool.image}")
        if tool.image is None and args.digest is not None:
            raise ToolVersionsError(f"{tool.name} has no image to take a digest")
        # Work out every new file first, then write them, restoring the
        # originals if a write fails, so a failed apply changes nothing.
        pins[tool.name] = args.version
        writes = {TOOL_VERSIONS: render_pins(pins)}
        if tool.image is not None:
            digest = parse_digest(args.digest.encode("utf-8"))
            tag = tool.image_tag.format(version=args.version)
            for paths, rewrite in ((_tracked_dockerfiles(), rewrite_image), (_tracked_workflows(), rewrite_workflow_image)):
                for path in paths:
                    text = path.read_text(encoding="utf-8")
                    updated = rewrite(text, tool.image, tag, digest)
                    if updated != text:
                        writes[path] = updated
        if tool.npm_dir is not None:
            manifest = ROOT / tool.npm_dir / "package.json"
            writes[manifest] = rewrite_npm_pins(manifest.read_text(encoding="utf-8"), tool.npm_packages, args.version)
        if tool.name == "rust":
            text = RUST_TOOLCHAIN.read_text(encoding="utf-8")
            writes[RUST_TOOLCHAIN] = rewrite_toolchain_channel(text, args.version)
        originals = {path: path.read_text(encoding="utf-8") for path in writes}
        try:
            for path, text in writes.items():
                path.write_text(text, encoding="utf-8")
        except OSError:
            for path, text in originals.items():
                path.write_text(text, encoding="utf-8")
            raise
        for path in writes:
            print(f"updated {path.relative_to(ROOT)}", file=sys.stderr)
        return 0
    except (OSError, subprocess.CalledProcessError, ToolVersionsError) as err:
        print(f"tool_versions: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
