"""Path containment helpers for authoring tools and sparse workerd launchers."""

from __future__ import annotations

import os
from pathlib import Path


def _is_under(root_s: str, resolved_s: str) -> bool:
    """True when ``resolved_s`` is ``root_s`` or a descendant.

    Uses ``os.path.relpath`` + ``startswith("..")`` (CodeQL-recognized shape)
    and an explicit ``startswith(root + sep)`` prefix match.
    """
    try:
        rel = os.path.relpath(resolved_s, root_s)
    except ValueError:
        return False
    if rel == ".." or rel.startswith(".." + os.sep) or os.path.isabs(rel):
        return False
    if resolved_s == root_s:
        return True
    prefix = root_s if root_s.endswith(os.sep) else root_s + os.sep
    return resolved_s.startswith(prefix)


def resolve_under(root: Path | str, *parts: str | Path) -> Path:
    """Join ``parts`` under ``root`` and require the result stay inside ``root``.

    Lexical ``abspath`` / ``normpath`` + ``relpath`` / ``startswith`` barrier
    (no ``realpath`` before the check — that is itself a path sink under local
    threat modeling). Callers that write under a plugin tree must also use
    :func:`refuse_symlink_path` so a ``.bookclerk -> /outside`` link cannot
    redirect generated output.

    Args:
        root: Trusted directory (user-selected plugin/output root, or cache).
        *parts: Relative segments to join, or one absolute path to validate
            under ``root``.

    Returns:
        Absolute normalized path under ``root``.

    Raises:
        ValueError: When a part contains ``..`` / NUL or the result escapes
            ``root``.
    """
    root_s = os.path.abspath(os.path.normpath(os.fspath(root)))
    if "\0" in root_s:
        raise ValueError(f"root path contains NUL: {root}")

    if len(parts) == 1 and os.path.isabs(os.fspath(parts[0])):
        resolved_s = os.path.abspath(os.path.normpath(os.fspath(parts[0])))
    else:
        for part in parts:
            part_s = os.fspath(part)
            if "\0" in part_s:
                raise ValueError(f"path contains NUL: {part}")
            # Component-level check: allow names like ``edition..2``.
            for seg in Path(part_s).parts:
                if seg == "..":
                    raise ValueError(f"path must not contain '..': {part}")
        joined = Path(root_s).joinpath(*parts) if parts else Path(root_s)
        resolved_s = os.path.abspath(os.path.normpath(os.fspath(joined)))

    if "\0" in resolved_s:
        raise ValueError(f"path contains NUL: {resolved_s}")

    if not _is_under(root_s, resolved_s):
        raise ValueError(f"path {resolved_s} escapes root {root_s}")
    return Path(resolved_s)


def ensure_dir_under(root: Path | str, rel: str | Path) -> Path:
    """Create ``root`` / ``rel`` after resolve + ``startswith`` (check before mkdir)."""
    root_s = os.path.abspath(os.path.normpath(os.fspath(root)))
    resolved = resolve_under(root_s, rel)
    resolved_s = os.fspath(resolved)
    if not _is_under(root_s, resolved_s):
        raise ValueError(f"path {resolved} escapes root {root_s}")
    # Inspect existing components before recursive mkdir so a symlink prefix
    # cannot redirect creation outside ``root``.
    refuse_symlink_existing_components(root_s, resolved_s)
    # mkdir only the barriered child — never the raw operator root.
    os.makedirs(resolved_s, exist_ok=True)
    return Path(resolved_s)


def write_file_under(root: Path | str, name: str, contents: str | bytes) -> Path:
    """Write ``contents`` to ``root`` / ``name`` after resolve + ``startswith``.

    Assumes ``root`` already exists (session/cache dirs are created first).
    Refuses a symlink at the destination, including a leaf link out of ``root``.
    Filenames may contain ``..`` (``edition..2.js``); ``.`` and ``..`` components
    are rejected.
    """
    if (
        not name
        or "\0" in name
        or "/" in name
        or "\\" in name
        or name in {".", ".."}
    ):
        raise ValueError(f"file name must be a single path component: {name}")
    root_s = os.path.abspath(os.path.normpath(os.fspath(root)))
    resolved_s = os.path.abspath(os.path.join(root_s, name))
    try:
        rel = os.path.relpath(resolved_s, root_s)
    except ValueError as err:
        raise ValueError(f"path {resolved_s} escapes root {root_s}") from err
    if rel == ".." or rel.startswith(".." + os.sep) or os.path.isabs(rel):
        raise ValueError(f"path {resolved_s} escapes root {root_s}")
    if resolved_s != root_s and not resolved_s.startswith(root_s + os.sep):
        raise ValueError(f"path {resolved_s} escapes root {root_s}")
    refuse_symlink_path(root_s, resolved_s)
    if isinstance(contents, str):
        with open(resolved_s, "w", encoding="utf-8") as fh:
            fh.write(contents)
    else:
        with open(resolved_s, "wb") as fh:
            fh.write(contents)
    return Path(resolved_s)


def copy_file_under(root: Path | str, name: str, src: Path | str) -> Path:
    """Copy ``src`` to ``root`` / ``name`` after resolve + ``startswith`` on dest."""
    import shutil

    if (
        not name
        or "\0" in name
        or "/" in name
        or "\\" in name
        or name in {".", ".."}
    ):
        raise ValueError(f"file name must be a single path component: {name}")
    root_s = os.path.abspath(os.path.normpath(os.fspath(root)))
    resolved_s = os.path.abspath(os.path.join(root_s, name))
    if not _is_under(root_s, resolved_s):
        raise ValueError(f"path {resolved_s} escapes root {root_s}")
    refuse_symlink_path(root_s, resolved_s)
    shutil.copy2(src, resolved_s, follow_symlinks=False)
    return Path(resolved_s)


def refuse_symlink_existing_components(trusted_root: Path | str, path: Path | str) -> Path:
    """Like :func:`refuse_symlink_path`, but any trailing missing components are OK.

    Use before ``os.makedirs(..., exist_ok=True)`` so a symlink intermediate
    cannot redirect recursive directory creation outside ``trusted_root``.
    """
    root_lex = os.path.abspath(os.path.normpath(os.fspath(trusted_root)))
    candidate_lex = os.path.abspath(os.path.normpath(os.fspath(path)))

    if not _is_under(root_lex, candidate_lex):
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}")

    try:
        root = os.path.realpath(root_lex)
    except OSError as err:
        raise ValueError(f"cannot resolve trusted root {root_lex}: {err}") from err

    if _is_under(root_lex, candidate_lex):
        suffix = os.path.relpath(candidate_lex, root_lex)
        if suffix in {"", "."}:
            candidate = root
        else:
            candidate = os.path.abspath(os.path.join(root, suffix))
    else:
        candidate = candidate_lex

    if not _is_under(root, candidate) and candidate != root:
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}")

    try:
        rel = os.path.relpath(candidate, root)
    except ValueError as err:
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}") from err
    if rel == ".." or rel.startswith(".." + os.sep) or os.path.isabs(rel):
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}")

    cur = root
    parts = [] if rel in {"", "."} else rel.split(os.sep)
    for part in parts:
        cur = os.path.join(cur, part)
        if not _is_under(root, cur) and cur != root:
            raise ValueError(f"path {cur} escapes root {root_lex}")
        if os.path.islink(cur):
            raise ValueError(f"refusing symlink in path: {cur}")
        if not os.path.lexists(cur):
            # Missing from here on: safe to create under the last existing prefix.
            break
    return Path(candidate_lex)


def refuse_symlink_path(trusted_root: Path | str, path: Path | str) -> Path:
    """Require ``path`` under ``trusted_root`` with no symlink suffix components.

    Resolves the trusted root identity once (a symlinked operator/plugin root is
    allowed). Only components *below* that identity are inspected for symlinks.
    Missing final components are allowed (for create); intermediate missing
    parents raise.

    Args:
        trusted_root: Original operator/plugin root (not a promoted child).
        path: Candidate path previously produced by :func:`resolve_under`.

    Returns:
        The validated ``path`` as a :class:`~pathlib.Path`.

    Raises:
        ValueError: When a suffix component is a symlink or escapes ``trusted_root``.
        FileNotFoundError: When an intermediate parent is missing.
    """
    root_lex = os.path.abspath(os.path.normpath(os.fspath(trusted_root)))
    candidate_lex = os.path.abspath(os.path.normpath(os.fspath(path)))

    if not _is_under(root_lex, candidate_lex):
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}")

    # Allow the operator-selected root itself to be a symlink; constrain children
    # under the resolved identity (matches prior realpath containment).
    try:
        root = os.path.realpath(root_lex)
    except OSError as err:
        raise ValueError(f"cannot resolve trusted root {root_lex}: {err}") from err

    # Map lexical candidate onto the realpath root when the root is a symlink.
    if _is_under(root_lex, candidate_lex):
        suffix = os.path.relpath(candidate_lex, root_lex)
        if suffix in {"", "."}:
            candidate = root
        else:
            candidate = os.path.abspath(os.path.join(root, suffix))
    else:
        candidate = candidate_lex

    if not _is_under(root, candidate) and candidate != root:
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}")

    try:
        rel = os.path.relpath(candidate, root)
    except ValueError as err:
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}") from err
    if rel == ".." or rel.startswith(".." + os.sep) or os.path.isabs(rel):
        raise ValueError(f"path {candidate_lex} escapes root {root_lex}")

    cur = root
    parts = [] if rel in {"", "."} else rel.split(os.sep)
    for i, part in enumerate(parts):
        cur = os.path.join(cur, part)
        if not _is_under(root, cur) and cur != root:
            raise ValueError(f"path {cur} escapes root {root_lex}")
        if os.path.islink(cur):
            raise ValueError(f"refusing symlink in path: {cur}")
        if not os.path.lexists(cur):
            if i < len(parts) - 1:
                raise FileNotFoundError(f"missing path component: {cur}")
            break
    return Path(candidate_lex)


def cli_user_path(raw: str | Path) -> Path:
    """Accept a CLI path argument (any user-chosen location; no cwd jail).

    Authoring CLIs take absolute or relative paths without forcing containment
    under ``Path.cwd()``. Reject NUL only; callers apply
    :func:`resolve_under` for manifest-derived children beneath the opened
    plugin/output root.

    Args:
        raw: Operator-selected path (absolute, relative, or ``~``-prefixed).

    Returns:
        Normalized absolute path.

    Raises:
        ValueError: When the path contains an interior NUL.
    """
    p = Path(raw).expanduser()
    s = os.fspath(p)
    if "\0" in s:
        raise ValueError("path contains NUL")
    if not os.path.isabs(s):
        s = os.path.normpath(os.path.join(os.getcwd(), s))
    else:
        s = os.path.normpath(s)
    return Path(s)


def copy_tree_no_symlinks(src: Path, dst: Path) -> None:
    """Copy a directory tree, refusing any symlink entries.

    Unlike ``shutil.copytree`` (which follows links by default), this never
    embeds outside bytes through ``modules/leak -> /outside``.

    Args:
        src: Source directory (must not itself be a symlink).
        dst: Destination directory to create.

    Raises:
        ValueError: When a symlink or unsupported file type is encountered.
        OSError: On filesystem failures.
    """
    import shutil

    if src.is_symlink():
        raise ValueError(f"refusing symlink package source: {src}")
    if not src.is_dir():
        raise ValueError(f"package source is not a directory: {src}")
    dst.mkdir(parents=True, exist_ok=True)
    for entry in sorted(src.iterdir(), key=lambda p: p.name):
        target = dst / entry.name
        if entry.is_symlink():
            raise ValueError(f"refusing symlink in package source: {entry}")
        if entry.is_dir():
            copy_tree_no_symlinks(entry, target)
        elif entry.is_file():
            shutil.copy2(entry, target, follow_symlinks=False)
        else:
            raise ValueError(f"refusing unsupported package source type: {entry}")
