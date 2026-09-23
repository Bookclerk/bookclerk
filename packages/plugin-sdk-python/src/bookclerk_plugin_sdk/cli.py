"""CLI entry: ``bookclerk-plugin`` / ``python -m bookclerk_plugin_sdk``.

Dispatches authoring helpers (``check``, ``fmt``, ``types``, ``sync-embed``,
``package``, ``smoke``) that mirror the Rust/TypeScript plugin toolchains.
"""

from __future__ import annotations

import sys
from pathlib import Path

from .path_guard import cli_user_path
from .sparse_workerd import run_smoke
from .tools import check_plugin, fmt_plugin_toml, generate_types, package_plugin, sync_embed


def main(argv: list[str] | None = None) -> int:
    """Run the ``bookclerk-plugin`` CLI.

    Args:
        argv: Argument vector excluding the program name. Defaults to
            ``sys.argv[1:]``.

    Returns:
        Process exit code (``0`` success, ``1`` command failure, ``2`` usage).

    Examples:
        >>> # python -m bookclerk_plugin_sdk check ./my-plugin
        >>> # main(["check", "./my-plugin"])
    """
    args = list(sys.argv[1:] if argv is None else argv)
    if not args or args[0] in {"-h", "--help", "help"}:
        print(
            "bookclerk-plugin — Bookclerk plugin authoring helpers\n\n"
            "Usage:\n"
            "  bookclerk-plugin check [dir]\n"
            "  bookclerk-plugin fmt [--check] [plugin.toml]\n"
            "  bookclerk-plugin types [dir] [--out <file>]\n"
            "  bookclerk-plugin sync-embed [dir]\n"
            "  bookclerk-plugin package --out <dir> [plugin-dir]\n"
            "  bookclerk-plugin smoke [dir]\n",
            file=sys.stderr,
        )
        return 0 if args else 2
    cmd = args[0]
    try:
        if cmd == "check":
            directory = cli_user_path(args[1] if len(args) > 1 else ".")
            print(check_plugin(directory))
            return 0
        if cmd == "fmt":
            check_only = False
            path = Path("plugin.toml")
            for a in args[1:]:
                if a == "--check":
                    check_only = True
                elif not a.startswith("-"):
                    path = Path(a)
                else:
                    raise SystemExit(f"unknown fmt flag: {a}")
            print(fmt_plugin_toml(cli_user_path(path), check_only=check_only))
            return 0
        if cmd == "types":
            out = None
            directory = Path(".")
            i = 1
            while i < len(args):
                if args[i] == "--out":
                    i += 1
                    out = Path(args[i])
                elif not args[i].startswith("-"):
                    directory = Path(args[i])
                else:
                    raise SystemExit(f"unknown types flag: {args[i]}")
                i += 1
            directory = cli_user_path(directory)
            out_resolved = cli_user_path(out) if out is not None else None
            print(generate_types(directory, out_resolved))
            return 0
        if cmd == "sync-embed":
            directory = cli_user_path(args[1] if len(args) > 1 else ".")
            print(sync_embed(directory))
            return 0
        if cmd == "package":
            out = None
            directory = Path(".")
            i = 1
            while i < len(args):
                if args[i] == "--out":
                    i += 1
                    out = Path(args[i])
                elif not args[i].startswith("-"):
                    directory = Path(args[i])
                else:
                    raise SystemExit(f"unknown package flag: {args[i]}")
                i += 1
            if out is None:
                raise SystemExit("package requires --out <dir>")
            directory = cli_user_path(directory)
            out_dir = cli_user_path(out)
            archive = package_plugin(directory, out_dir)
            print(f"packed {archive}")
            return 0
        if cmd == "smoke":
            directory = cli_user_path(args[1] if len(args) > 1 else ".")
            print(run_smoke(directory))
            return 0
        print(f"unknown command: {cmd}", file=sys.stderr)
        return 2
    except Exception as err:  # noqa: BLE001
        print(f"{cmd} failed: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
