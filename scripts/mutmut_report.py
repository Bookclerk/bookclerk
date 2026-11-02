#!/usr/bin/env python3
"""Scopes and summarizes mutmut mutation runs on the Python plugin SDK.

The Python side of mutation testing (``docs/contributing/ci.md#python-sdk``):
``.github/workflows/mutation-python.yml`` runs mutmut each night on the
functions merged since its last run in ``packages/plugin-sdk-python/src`` and then this script,
which writes the Markdown summary and, with ``--issue``, the issue body for
missed mutants. Like the Rust and Worker jobs, it never blocks a merge and
the script exits 0 unless mutmut tested none of the selected mutants: then
it exits 1, so the next daily run covers the same changes.

Two subcommands:

- ``select --diff FILE --root DIR [--base REV] [--skipped FILE]`` prints the mutmut
  mutant-name patterns for the SDK at ``DIR``, one per line, or nothing
  when no source line changed. mutmut names each mutant after its function
  (``<module>.x_<function>__mutmut_<n>``, or
  ``<module>.xǁ<Class>ǁ<method>__mutmut_<n>`` for a method), so a changed
  function becomes ``<module>.x_<function>__mutmut_*``. Added lines are
  placed in the new file; deleted lines are placed in the file at
  ``--base`` (read with ``git show``), so removing a guard, even a
  function's last lines, selects its function when the new file still
  has it. mutmut only mutates
  function bodies, so a changed line outside every function (a constant, an
  import, a class attribute) widens to the whole module (``<module>.x_*``
  and ``<module>.xǁ*``): the tests of the code beside the changed data are
  then checked. Blank lines, comments and docstrings outside functions
  widen nothing. Tests and the generated ABI modules (``do_not_mutate`` in
  the SDK's ``pyproject.toml``) are never selected. Changed functions
  mutmut never mutates (decorated ones, and methods of enum classes) are
  written to ``--skipped`` so the summary can name them.
- ``report --mutants DIR --patterns FILE --root DIR [--skipped FILE]``
  reads the results
  mutmut keeps beside each mutated module (``mutants/src/**/<file>.meta``),
  keeps the mutants the patterns select, appends the summary to
  ``$GITHUB_STEP_SUMMARY`` (``--summary FILE`` overrides) and annotates each
  missed mutant on the run, with the line and change read from mutmut's
  copy of the function. ``--out DIR`` also writes every mutant, one file
  per outcome (``caught.txt``, ``missed.txt``, …), for the artifact.

The diff, the sources and mutmut's files come from the same job; parsing is
still sans-IO and bounded, and ``scripts/tests/test_mutmut_report.py``
fuzzes it against independent readings.
"""

from __future__ import annotations

import argparse
import ast
import difflib
import fnmatch
import json
import os
import re
import subprocess
import sys
import tomllib
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from mutation_report import MAX_LISTED, Outcome, Results, _code, write_issue
from stryker_report import SAFE_PATH, _clean, annotations

#: Largest file read (a source module, a mutated module or a ``.meta``
#: file), in bytes; a larger one is skipped.
MAX_FILE_BYTES = 32 * 1024 * 1024

#: Most ``.meta`` files read from one run.
MAX_META_FILES = 1000

#: mutmut 3's exit codes per mutant, mapped to the Rust job's outcomes, from
#: ``mutmut/stats.py`` at the version the workflow pins. A mutant no test
#: runs ("no tests") is missed, as Stryker's ``NoCoverage`` is. A code that
#: is not listed (not checked, interrupted, skipped, suspicious, a crash)
#: is left out.
EXIT_CODES = {
    1: Outcome.CAUGHT,
    3: Outcome.CAUGHT,
    37: Outcome.CAUGHT,
    0: Outcome.MISSED,
    5: Outcome.MISSED,
    33: Outcome.MISSED,
    36: Outcome.TIMEOUT,
    24: Outcome.TIMEOUT,
    -24: Outcome.TIMEOUT,
    152: Outcome.TIMEOUT,
    255: Outcome.TIMEOUT,
}

#: A dotted module name mutmut can be given: ASCII identifiers only, so the
#: patterns are safe in a step output and on mutmut's command line.
MODULE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]{0,99}(?:\.[A-Za-z_][A-Za-z0-9_]{0,99}){0,19}$")

#: An identifier ``select`` writes into a pattern.
IDENTIFIER = re.compile(r"^[A-Za-z_][A-Za-z0-9_]{0,199}$")

#: A pattern ``select`` writes: one function's mutants, or a whole module's
#: (two patterns, one for functions and one for methods).
PATTERN = re.compile(r"^(?P<module>[A-Za-z0-9_.]{1,2100})\.(?:(?P<base>x(?:_|ǁ)[A-Za-z0-9_ǁ]{1,400})__mutmut_\*|x_\*|xǁ\*)$")

#: Hunk header of a unified diff: each side's first line and length.
HUNK_HEADER = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")

#: Enum base classes whose methods mutmut 3 never mutates
#: (``ENUM_BASE_NAMES`` in ``mutmut/mutation/file_mutation.py``).
ENUM_BASES = frozenset({"Enum", "IntEnum", "StrEnum", "Flag", "IntFlag"})

#: A mutant name mutmut writes: module, then the mangled function and number.
MUTANT = re.compile(r"^(?P<module>[A-Za-z0-9_.]{1,2100})\.(?P<base>x(?:_|ǁ)[^.\s]{1,400})__mutmut_(?P<n>[0-9]{1,9})$")


def generated_modules(pyproject: str) -> frozenset[str]:
    """Read the files mutmut must never mutate from the SDK's ``pyproject.toml``.

    Args:
        pyproject: The file's text.

    Returns:
        ``[tool.mutmut].do_not_mutate`` as paths relative to the SDK; empty
        when the table or key is missing or the file does not parse.
    """
    try:
        data = tomllib.loads(pyproject)
    except tomllib.TOMLDecodeError:
        return frozenset()
    tool = data.get("tool", {})
    mutmut = tool.get("mutmut", {}) if isinstance(tool, dict) else {}
    paths = mutmut.get("do_not_mutate", []) if isinstance(mutmut, dict) else []
    return frozenset(p for p in paths if isinstance(p, str)) if isinstance(paths, list) else frozenset()


def is_source(path: str, generated: frozenset[str]) -> bool:
    """Say whether an SDK-relative path is a module mutmut should mutate.

    Args:
        path: The path relative to the SDK's directory.
        generated: The generated modules, from ``generated_modules``: paths
            or globs, matched as mutmut matches ``do_not_mutate``.

    Returns:
        True for a ``.py`` file under ``src/`` that is not generated and not
        in a test folder.
    """
    parts = PurePosixPath(path).parts
    if len(parts) < 2 or parts[0] != "src" or not parts[-1].endswith(".py"):
        return False
    if any(fnmatch.fnmatch(path, pattern) for pattern in generated):
        return False
    return not any(part in {"tests", "test", "__pycache__"} for part in parts[1:-1])


@dataclass
class Change:
    """How the diff changes one SDK source file."""

    #: The lines the diff adds, on the new side, 1-based.
    added: set[int]
    #: The lines the diff deletes, on the old side, 1-based.
    deleted: set[int]
    #: The file's path relative to the SDK before the change (it differs
    #: from the new one for a rename).
    old_path: str


def changed_points(diff: str, root: str, generated: frozenset[str]) -> dict[str, Change]:
    """Collect the changed lines of the SDK's sources from a unified diff.

    Args:
        diff: The diff, as ``git diff`` writes it.
        root: The SDK's directory, relative to the repository root.
        generated: The generated modules, from ``generated_modules``.

    Returns:
        For each source file under ``root`` (by its new path relative to
        it, matching ``SAFE_PATH`` and kept by ``is_source``), the lines the
        diff adds (new side) and deletes (old side). Deleted lines are
        placed against the old file, since the new one no longer holds
        them.
    """
    prefix = root.rstrip("/") + "/"
    changes: dict[str, Change] = {}
    current: Change | None = None
    old_path: str | None = None
    old_line = new_line = 0
    old_remaining = new_remaining = 0

    def relative(path: str, side: str) -> str | None:
        if not path.startswith(side + "/" + prefix):
            return None
        rest = path[len(side) + 1 + len(prefix) :]
        if SAFE_PATH.match(rest) and is_source(rest, generated) and ".." not in PurePosixPath(rest).parts:
            return rest
        return None

    for text in diff.splitlines():
        if text.startswith("diff --git "):
            current, old_path, old_remaining, new_remaining = None, None, 0, 0
            continue
        if old_remaining == 0 and new_remaining == 0 and text.startswith("--- "):
            old_path = relative(text[4:], "a")
            continue
        if old_remaining == 0 and new_remaining == 0 and text.startswith("+++ "):
            new_path = relative(text[4:], "b")
            current = None
            if new_path is not None:
                current = changes.setdefault(new_path, Change(set(), set(), old_path or new_path))
            continue
        match = HUNK_HEADER.match(text)
        if match:
            old_line = int(match.group(1))
            old_remaining = int(match.group(2)) if match.group(2) is not None else 1
            new_line = int(match.group(3))
            new_remaining = int(match.group(4)) if match.group(4) is not None else 1
            # A side of length 0 names the line before the hunk.
            old_line += old_remaining == 0
            new_line += new_remaining == 0
            continue
        if (old_remaining <= 0 and new_remaining <= 0) or not text:
            continue
        if text[0] == "+" and new_remaining > 0:
            if current is not None:
                current.added.add(new_line)
            new_line += 1
            new_remaining -= 1
        elif text[0] == "-" and old_remaining > 0:
            if current is not None:
                current.deleted.add(old_line)
            old_line += 1
            old_remaining -= 1
        elif text[0] == " ":
            old_line += 1
            new_line += 1
            old_remaining -= 1
            new_remaining -= 1
    return changes


def module_name(path: str) -> str | None:
    """Turn an SDK-relative source path into the dotted name mutmut uses.

    Args:
        path: A path such as ``src/bookclerk_plugin_sdk/tools.py``.

    Returns:
        ``bookclerk_plugin_sdk.tools`` (a package's ``__init__.py`` is the
        package itself), or ``None`` when the path is not under ``src/`` or
        the name is not plain ASCII identifiers.
    """
    parts = list(PurePosixPath(path).parts)
    if len(parts) < 2 or parts[0] != "src" or not parts[-1].endswith(".py"):
        return None
    parts = parts[1:]
    parts[-1] = parts[-1][: -len(".py")]
    if parts[-1] == "__init__":
        parts.pop()
    name = ".".join(parts)
    return name if MODULE.match(name) else None


def _span(node: ast.stmt) -> tuple[int, int]:
    """Return a statement's first and last line, decorators included."""
    decorators = getattr(node, "decorator_list", [])
    start = min([node.lineno, *(d.lineno for d in decorators)])
    return start, node.end_lineno or node.lineno


def _is_docstring(node: ast.stmt) -> bool:
    """Say whether a statement is a bare string, as a docstring is."""
    return isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant) and isinstance(node.value.value, str)


def _mutmut_skips(node: ast.FunctionDef | ast.AsyncFunctionDef) -> bool:
    """Say whether mutmut 3 leaves a function alone because of its decorators.

    mutmut skips every decorated function except one with a single bare
    ``@staticmethod`` or ``@classmethod``.
    """
    decorators = node.decorator_list
    if not decorators:
        return False
    return not (len(decorators) == 1 and isinstance(decorators[0], ast.Name) and decorators[0].id in {"staticmethod", "classmethod"})


def _is_enum(node: ast.ClassDef) -> bool:
    """Say whether a class derives directly from an enum base mutmut skips."""
    for base in node.bases:
        name = base.attr if isinstance(base, ast.Attribute) else base.id if isinstance(base, ast.Name) else None
        if name in ENUM_BASES:
            return True
    return False


@dataclass
class Targets:
    """What one module's changed lines select."""

    #: The mangled names mutmut gives the changed functions it mutates
    #: (``x_<name>``, or ``xǁ<Class>ǁ<name>`` for a method), sorted.
    names: list[str]
    #: Whether a changed line falls in code outside every function, so the
    #: whole module is mutated.
    widen: bool
    #: The changed functions mutmut never mutates (``<name>`` or
    #: ``<Class>.<name>``), sorted.
    skipped: list[str]


def targets(source: str, lines: set[int]) -> Targets:
    """Find the functions of one module that the changed lines fall in.

    Args:
        source: The module's new text.
        lines: The changed lines, 1-based.

    Returns:
        The selected functions and whether to widen. A module that does not
        parse, or a changed function whose name is not a plain ASCII
        identifier, widens too.
    """
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        return Targets([], True, [])
    source_lines = source.splitlines()
    names: set[str] = set()
    skipped: set[str] = set()
    widen = False

    def hit(start: int, end: int) -> bool:
        return any(start <= line <= end for line in lines)

    def function(node: ast.FunctionDef | ast.AsyncFunctionDef, owner: ast.ClassDef | None) -> None:
        nonlocal widen
        start, end = _span(node)
        if not hit(start, end):
            return
        parts = [owner.name, node.name] if owner is not None else [node.name]
        if not all(IDENTIFIER.match(part) for part in parts):
            widen = True
        elif _mutmut_skips(node) or (owner is not None and _is_enum(owner)):
            skipped.add(".".join(parts))
        else:
            names.add("xǁ" + "ǁ".join(parts) if owner is not None else "x_" + node.name)

    for node in tree.body:
        start, end = _span(node)
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            function(node, None)
        elif isinstance(node, ast.ClassDef):
            covered: list[tuple[int, int]] = []
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    function(item, node)
                    covered.append(_span(item))
                elif _is_docstring(item):
                    covered.append(_span(item))
            # Class-level code outside its methods and docstring: the class
            # line, decorators, bases and attributes.
            rest = {line for line in lines if start <= line <= end and not any(a <= line <= b for a, b in covered)}
            if any(_code_line(source_lines, line) for line in rest):
                widen = True
        elif not _is_docstring(node) and any(_code_line(source_lines, line) for line in lines if start <= line <= end):
            widen = True
    return Targets(sorted(names), widen, sorted(skipped))


def _code_line(lines: list[str], line: int) -> bool:
    """Say whether a 1-based line holds code, not only blanks or a comment."""
    if not 0 < line <= len(lines):
        return False
    text = lines[line - 1].strip()
    return bool(text) and not text.startswith("#")


def patterns_for(module: str, found: Targets) -> list[str]:
    """Write the mutmut patterns for one module's changed functions.

    Args:
        module: The dotted module name.
        found: The module's selection, from ``targets``.

    Returns:
        ``<module>.x_*`` and ``<module>.xǁ*`` (every function and method)
        when widened, otherwise one ``<module>.<name>__mutmut_*`` per
        function. mutmut matches these as globs, so a whole-module pattern
        also reaches a submodule named ``x_…``; ``selected`` is exact.
    """
    if found.widen:
        return [f"{module}.x_*", f"{module}.xǁ*"]
    return [f"{module}.{name}__mutmut_*" for name in found.names]


def _defined(source: str) -> dict[str, bool]:
    """Index a module's top-level functions and methods by mangled name.

    Returns:
        Whether mutmut mutates each one (``False`` for decorated functions
        and enum methods); the first definition of a name wins.
    """
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        return {}
    names: dict[str, bool] = {}
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            names.setdefault("x_" + node.name, not _mutmut_skips(node))
        elif isinstance(node, ast.ClassDef):
            enum = _is_enum(node)
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    names.setdefault(f"xǁ{node.name}ǁ{item.name}", not enum and not _mutmut_skips(item))
    return names


def _display(mangled: str) -> str:
    """Turn mutmut's mangled ``x_name`` or ``xǁClassǁname`` into ``name`` or ``Class.name``."""
    return mangled[2:] if mangled.startswith("x_") else mangled[2:].replace("ǁ", ".")


def _mangled(display: str) -> str:
    """Turn ``name`` or ``Class.name`` into mutmut's mangled function name."""
    return "xǁ" + display.replace(".", "ǁ") if "." in display else "x_" + display


def select(
    diff: str,
    root: str,
    read: dict[str, str],
    generated: frozenset[str],
    read_old: dict[str, str] | None = None,
) -> tuple[list[str], list[str]]:
    """Build the mutmut patterns for a diff's changes to the SDK.

    Added lines are placed in the new file. Deleted lines are placed in the
    old one: the function they were in is selected when the new file still
    has it, and deleted code outside every function widens the module.

    Args:
        diff: The unified diff of the code being mutated.
        root: The SDK's directory, relative to the repository root.
        read: Each changed source file's new text, keyed by its path
            relative to ``root``; a file missing here (deleted) is skipped.
        generated: The generated modules, from ``generated_modules``.
        read_old: Each changed file's text before the change, keyed by its
            new path; deleted lines of a file missing here select nothing.

    Returns:
        The patterns, one module after another in path order, and the
        changed functions mutmut never mutates, as ``<module>.<name>``.
    """
    read_old = read_old or {}
    patterns: list[str] = []
    skipped: list[str] = []
    for path, change in sorted(changed_points(diff, root, generated).items()):
        module = module_name(path)
        if module is None or path not in read:
            continue
        found = targets(read[path], change.added)
        if change.deleted and path in read_old:
            # The old file says which function held the deleted lines; the
            # new one says whether it still exists and whether mutmut
            # mutates it now (removing a decorator makes it mutable).
            before = targets(read_old[path], change.deleted)
            now = _defined(read[path])
            held = set(before.names) | {_mangled(name) for name in before.skipped}
            names, unmutable = set(found.names), set(found.skipped)
            for mangled in sorted(held):
                if now.get(mangled) is True:
                    names.add(mangled)
                elif now.get(mangled) is False:
                    unmutable.add(_display(mangled))
            found = Targets(sorted(names), found.widen or before.widen, sorted(unmutable))
        patterns += patterns_for(module, found)
        skipped += [f"{module}.{name}" for name in found.skipped]
    return patterns, skipped


def parse_patterns(text: str) -> list[tuple[str, str | None]]:
    """Read the patterns ``select`` wrote back into module and function.

    Args:
        text: The patterns file, one per line.

    Returns:
        ``(module, mangled function)`` per pattern, the function ``None``
        for a whole module (listed once for its two patterns); lines that
        are not a pattern ``select`` writes are left out.
    """
    out: list[tuple[str, str | None]] = []
    for line in text.splitlines()[:10000]:
        match = PATTERN.match(line.strip())
        if match and (match.group("module"), match.group("base")) not in out:
            out.append((match.group("module"), match.group("base")))
    return out


def selected(key: str, patterns: list[tuple[str, str | None]]) -> bool:
    """Say whether a mutant name is one the patterns select.

    The check is exact: a whole-module pattern selects that module's
    functions only, not a submodule's.
    """
    match = MUTANT.match(key)
    if not match:
        return False
    return any(match.group("module") == module and (base is None or match.group("base") == base) for module, base in patterns)


#: One mutant's line in a ``.meta`` file, which mutmut writes with
#: ``json.dump(indent=4)``.
#: ``json.dump`` escapes every non-ASCII character, so a method's key
#: (``xǁClassǁname``) holds ``\u01c1`` escapes.
META_LINE = re.compile(r'^\s*(?P<key>"(?:[^"\\]|\\.){1,2600}"): (?P<code>-?[0-9]{1,9}|null),?$')


def parse_meta(text: str) -> dict[str, int | None]:
    """Read one of mutmut's ``.meta`` files.

    mutmut rewrites the file after every mutant, so a run stopped at the time
    limit can leave it cut short. When the JSON does not parse, the
    ``exit_code_by_key`` object is read line by line up to where it ends or
    the file stops.

    Args:
        text: The file's text.

    Returns:
        Each mutant's exit code (``None`` when mutmut did not test it),
        keyed by mutant name; empty when the file is not mutmut's.
    """
    try:
        data = json.loads(text)
    except (ValueError, RecursionError):
        return _salvage_meta(text)
    codes = data.get("exit_code_by_key") if isinstance(data, dict) else None
    if not isinstance(codes, dict):
        return {}
    return {k: v if type(v) is int else None for k, v in codes.items() if isinstance(k, str)}


def _salvage_meta(text: str) -> dict[str, int | None]:
    """Read the ``exit_code_by_key`` lines of a ``.meta`` file cut short."""
    codes: dict[str, int | None] = {}
    inside = False
    for line in text.splitlines():
        if not inside:
            inside = line.strip() == '"exit_code_by_key": {'
            continue
        match = META_LINE.match(line)
        if not match:
            break
        try:
            key = json.loads(match.group("key"))
        except ValueError:
            break
        code = match.group("code")
        codes[key] = None if code == "null" else int(code)
    return codes


def _functions(source: str) -> dict[str, ast.FunctionDef | ast.AsyncFunctionDef]:
    """Index a module's functions, at any depth, by name (first one wins)."""
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        return {}
    found: dict[str, ast.FunctionDef | ast.AsyncFunctionDef] = {}
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            found.setdefault(node.name, node)
    return found


def _original(tree: ast.Module, base: str) -> ast.FunctionDef | ast.AsyncFunctionDef | None:
    """Find the function a mangled name stands for in the original module."""
    if base.startswith("x_"):
        name, owner = base[2:], None
    elif base.startswith("xǁ") and base.count("ǁ") == 2:
        _, owner, name = base.split("ǁ")
    else:
        return None
    body = tree.body
    if owner is not None:
        classes = [n for n in body if isinstance(n, ast.ClassDef) and n.name == owner]
        if not classes:
            return None
        body = classes[0].body
    for node in body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == name:
            return node
    return None


@dataclass
class Module:
    """One mutated module: mutmut's copy and the original, indexed once."""

    #: The original module's lines.
    original_lines: list[str]
    #: The original module's syntax tree, or ``None`` when it does not parse.
    original_tree: ast.Module | None
    #: mutmut's copy of the module, by line.
    mutated_lines: list[str]
    #: The functions in mutmut's copy, by name.
    mutated_functions: dict[str, ast.FunctionDef | ast.AsyncFunctionDef]

    @classmethod
    def read(cls, original: str, mutated: str) -> Module:
        """Index an original module and mutmut's copy of it.

        Args:
            original: The module's text after the diff.
            mutated: mutmut's copy, holding every mutant's function.

        Returns:
            The indexed pair.
        """
        try:
            tree: ast.Module | None = ast.parse(original)
        except (SyntaxError, ValueError, RecursionError, MemoryError):
            tree = None
        return cls(original.splitlines(), tree, mutated.splitlines(), _functions(mutated))

    def _lines(self, name: str) -> list[str] | None:
        """Return a function's lines in mutmut's copy, its own name blanked."""
        node = self.mutated_functions.get(name)
        if node is None or node.end_lineno is None:
            return None
        lines = self.mutated_lines[node.lineno - 1 : node.end_lineno]
        if lines:
            lines[0] = lines[0].replace(name, "", 1)
        return lines

    def change(self, key: str) -> tuple[int, str]:
        """Describe what one mutant changes.

        Args:
            key: The mutant's name.

        Returns:
            The original line the change starts on (0 when it cannot be
            placed) and the change as ``old → new``, each side its first
            changed line, stripped; empty when mutmut's copy does not hold
            the mutant.
        """
        match = MUTANT.match(key)
        if not match:
            return 0, ""
        base = match.group("base")
        orig = self._lines(f"{base}__mutmut_orig")
        mutant = self._lines(f"{base}__mutmut_{match.group('n')}")
        if orig is None or mutant is None:
            return 0, ""
        opcodes = [op for op in difflib.SequenceMatcher(a=orig, b=mutant, autojunk=False).get_opcodes() if op[0] != "equal"]
        if not opcodes:
            return 0, ""
        _, i1, i2, j1, j2 = opcodes[0]
        old = orig[i1].strip() if i1 < i2 else ""
        new = mutant[j1].strip() if j1 < j2 else ""
        line = 0
        node = _original(self.original_tree, base) if self.original_tree is not None else None
        if node is not None and node.end_lineno is not None and node.end_lineno - node.lineno + 1 == len(orig):
            line = node.lineno + i1
        return line, f"{old or '(nothing)'} → {new or '(removed)'}"


def collect(
    codes_by_path: dict[str, dict[str, int | None]],
    load: Callable[[str], tuple[str, str] | None],
    patterns: list[tuple[str, str | None]],
    root: str,
) -> tuple[Results, list[tuple[str, int, str]], int]:
    """Gather the selected mutants' outcomes from mutmut's results.

    Args:
        codes_by_path: Each ``.meta`` file's exit codes (``parse_meta``),
            keyed by the source path it belongs to, relative to the SDK
            (``src/…/tools.py``).
        load: Returns ``(original, mutated)`` text for a source path, for
            describing missed mutants, or ``None`` to leave them
            undescribed; called only for paths with a missed mutant.
        patterns: The selection, from ``parse_patterns``.
        root: The SDK's directory, prefixed to paths so they are
            repository-relative.

    Returns:
        The mutants by outcome, each described as
        ``path:line: old → new (name)``, ``(path, line, description)``
        for each missed one with a line, and how many mutants mutmut ran
        that ended with a code ``EXIT_CODES`` does not count (a crash, a
        skip, a suspicious result).
    """
    results = Results()
    other = 0
    survivors: list[tuple[str, int, str]] = []
    prefix = root.rstrip("/") + "/"
    for source_path in sorted(codes_by_path):
        codes = codes_by_path[source_path]
        keys = sorted((k for k in codes if selected(k, patterns)), key=_natural)
        path = _clean(prefix + source_path)
        indexed: Module | None = None
        loaded = False
        for key in keys:
            code = codes[key]
            if code is None:
                continue
            outcome = EXIT_CODES.get(code)
            if outcome is None:
                other += 1
                continue
            line, change = 0, ""
            if outcome is Outcome.MISSED:
                if not loaded:
                    texts = load(source_path)
                    indexed = Module.read(*texts) if texts is not None else None
                    loaded = True
                if indexed is not None:
                    line, change = indexed.change(key)
            name = _clean(key.rsplit(".", 1)[-1])
            description = _clean(f"{path}:{line}: {change} ({name})" if change else f"{path}: {name}")
            results.mutants[outcome].append(description)
            if outcome is Outcome.MISSED and line:
                survivors.append((path, line, description))
    return results, survivors, other


def progress(codes_by_path: dict[str, dict[str, int | None]], patterns: list[tuple[str, str | None]]) -> tuple[int, int]:
    """Count the selected mutants, and those mutmut has tested.

    Args:
        codes_by_path: Each ``.meta`` file's exit codes, by source path.
        patterns: The selection, from ``parse_patterns``.

    Returns:
        ``(selected, tested)``: mutmut writes every mutant with no exit
        code before testing any, so selected mutants none of which has a
        code mean the run stopped before testing them.
    """
    chosen = [code for codes in codes_by_path.values() for key, code in codes.items() if selected(key, patterns)]
    return len(chosen), sum(code is not None for code in chosen)


def _natural(key: str) -> tuple[str, int]:
    """Sort mutant names by function, then by number."""
    match = MUTANT.match(key)
    return (match.group("base"), int(match.group("n"))) if match else (key, 0)


def render(
    parsed: tuple[Results, list[tuple[str, int, str]], int] | None,
    *,
    patterns: list[tuple[str, str | None]],
    skipped: list[str] | None = None,
    stopped: bool = False,
) -> str:
    """Write the Markdown summary of the SDK's run.

    Args:
        parsed: The collected outcomes, or ``None`` when mutmut tested none
            of the selected mutants (it failed or was stopped first).
        patterns: The selection; empty when no changed function can be
            mutated, so mutmut did not run.
        skipped: Changed functions mutmut never mutates, as
            ``<module>.<name>``.
        stopped: Whether the workflow stopped mutmut at its time limit, so
            only the mutants tested before then are counted.

    Returns:
        The summary, ending with a newline.
    """
    lines = ["## Mutation testing (Python SDK)", ""]
    widened = sorted({module for module, base in patterns if base is None})
    skipped = skipped or []
    if not patterns and not skipped:
        lines.append("No mutants to test: the change touches no source line of the SDK.")
    elif not patterns:
        lines.append("No mutants to test: mutmut cannot mutate any of the changed functions.")
    elif parsed is None:
        reason = "it reached the time limit first" if stopped else "check the job log"
        lines.append(f"mutmut did not test any mutant of the changed functions; {reason}.")
    else:
        results, other = parsed[0], parsed[2]
        if widened:
            names = ", ".join(_code(m) for m in widened)
            lines += [f"Changed lines outside any function in {names}, so all of their mutable functions were mutated.", ""]
        if stopped:
            lines += ["mutmut reached the time limit, so only the mutants tested before it are counted.", ""]
        score = results.score()
        if score is None and other:
            lines.append(f"None of the {other} mutants mutmut ran ended with a result it counts (a crash, a skip or a suspicious result); check the job log.")
        elif score is None:
            lines.append("mutmut found no mutant in the changed functions.")
        else:
            missed = results.count(Outcome.MISSED)
            lines.append(f"The tests caught **{score:.0f}%** of the {results.tested} mutants of the changed functions ({missed} missed).")
            if other:
                lines += ["", f"{other} more ended without a result mutmut counts (a crash, a skip or a suspicious result) and are left out."]
        counts = (results.count(o) for o in (Outcome.CAUGHT, Outcome.MISSED, Outcome.TIMEOUT))
        lines += [
            "",
            "| Caught | Missed | Timed out |",
            "| ---: | ---: | ---: |",
            "| " + " | ".join(str(c) for c in counts) + " |",
        ]
        missed_list = results.mutants[Outcome.MISSED]
        if missed_list:
            lines += ["", "Missed mutants: the tests still pass with each of these changes.", ""]
            lines += [f"- {_code(m)}" for m in missed_list[:MAX_LISTED]]
            if len(missed_list) > MAX_LISTED:
                lines.append(f"- … and {len(missed_list) - MAX_LISTED} more (see the `mutants-python-sdk` artifact).")
    if skipped:
        lines += ["", "mutmut never mutates decorated functions (other than a bare `@staticmethod` or `@classmethod`) or enum methods, so these changed ones were not tested:", ""]
        lines += [f"- {_code(_clean(name))}" for name in skipped[:MAX_LISTED]]
        if len(skipped) > MAX_LISTED:
            lines.append(f"- … and {len(skipped) - MAX_LISTED} more.")
    lines += ["", "Daily run: missed mutants are filed as an issue and never block a merge."]
    return "\n".join(lines) + "\n"


def _read(path: Path) -> str | None:
    """Read a text file no larger than ``MAX_FILE_BYTES``, or return ``None``."""
    try:
        if not path.is_file() or path.stat().st_size > MAX_FILE_BYTES:
            return None
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def _git_show(revision: str, path: str) -> str | None:
    """Return a file's text at a git revision, or ``None`` when git cannot show it."""
    if not re.match(r"^[A-Za-z0-9_^~./-]{1,100}$", revision) or revision.startswith("-") or not SAFE_PATH.match(path) or ".." in PurePosixPath(path).parts:
        return None
    try:
        result = subprocess.run(["git", "show", f"{revision}:{path}"], capture_output=True, check=False, timeout=60)
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0 or len(result.stdout) > MAX_FILE_BYTES:
        return None
    return result.stdout.decode("utf-8", errors="replace")


def select_main(argv: list[str]) -> int:
    """Print the mutmut patterns for the SDK's changed functions."""
    parser = argparse.ArgumentParser(prog="mutmut_report.py select")
    parser.add_argument("--diff", type=Path, required=True, help="the diff of the code merged since the last run")
    parser.add_argument("--root", required=True, help="the SDK's directory, relative to the repository root")
    parser.add_argument("--skipped", type=Path, help="file to write the changed functions mutmut never mutates to")
    parser.add_argument("--base", help="the git revision the diff starts from, to place deleted lines (HEAD^1 in CI)")
    args = parser.parse_args(argv)
    root = Path(args.root)
    generated = generated_modules(_read(root / "pyproject.toml") or "")
    diff = args.diff.read_text(encoding="utf-8", errors="replace")
    read: dict[str, str] = {}
    read_old: dict[str, str] = {}
    for path, change in changed_points(diff, args.root, generated).items():
        text = _read(root / path)
        if text is not None:
            read[path] = text
        if args.base and change.deleted:
            old = _git_show(args.base, f"{args.root.rstrip('/')}/{change.old_path}")
            if old is not None:
                read_old[path] = old
    patterns, skipped = select(diff, args.root, read, generated, read_old)
    sys.stdout.write("".join(p + "\n" for p in patterns))
    if args.skipped is not None:
        args.skipped.write_text("".join(name + "\n" for name in skipped), encoding="utf-8")
    return 0


def report_main(argv: list[str]) -> int:
    """Write the SDK's summary and annotations."""
    parser = argparse.ArgumentParser(prog="mutmut_report.py report")
    parser.add_argument("--mutants", type=Path, required=True, help="mutmut's mutants/ folder")
    parser.add_argument("--patterns", type=Path, required=True, help="the patterns `select` printed")
    parser.add_argument("--skipped", type=Path, help="the changed functions `select` found mutmut never mutates")
    parser.add_argument("--root", required=True, help="the SDK's directory, relative to the repository root")
    parser.add_argument("--stopped", action="store_true", help="the workflow stopped mutmut at its time limit")
    parser.add_argument("--out", type=Path, help="folder to write <outcome>.txt into, one mutant per line, for the artifact")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY", ""), help="file to append the summary to (none when empty)")
    parser.add_argument("--issue", type=Path, help="file to write the summary to when a mutant was missed, for the issue the workflow files")
    args = parser.parse_args(argv)
    patterns = parse_patterns(_read(args.patterns) or "")
    skipped = [line.strip() for line in (_read(args.skipped) or "").splitlines()[:10000] if line.strip()] if args.skipped else []
    parsed = None
    if patterns and (args.mutants / "src").is_dir():
        codes_by_path: dict[str, dict[str, int | None]] = {}
        for meta in sorted((args.mutants / "src").rglob("*.py.meta"))[:MAX_META_FILES]:
            source_path = meta.relative_to(args.mutants).as_posix()[: -len(".meta")]
            text = _read(meta) if SAFE_PATH.match(source_path) else None
            if text is not None:
                codes_by_path[source_path] = parse_meta(text)

        def load(source_path: str) -> tuple[str, str] | None:
            original, mutated = _read(Path(args.root) / source_path), _read(args.mutants / source_path)
            return (original, mutated) if original is not None and mutated is not None else None

        chosen, tested = progress(codes_by_path, patterns)
        # Selected mutants exist but none has a result: mutmut failed or
        # was stopped before testing any. No selected mutant at all is a
        # finished run: the changed functions have nothing to mutate.
        if tested or (codes_by_path and not chosen):
            parsed = collect(codes_by_path, load, patterns, args.root)
    markdown = render(parsed, patterns=patterns, skipped=skipped, stopped=args.stopped)
    sys.stdout.write(markdown)
    write_issue(args.issue, markdown, parsed[0] if parsed is not None else None)
    if parsed is not None:
        sys.stdout.write(annotations(parsed[1]))
        if args.out is not None:
            args.out.mkdir(parents=True, exist_ok=True)
            for outcome in Outcome:
                (args.out / f"{outcome.value}.txt").write_text("".join(m + "\n" for m in parsed[0].mutants[outcome]), encoding="utf-8")
    if args.summary:
        with Path(args.summary).open("a", encoding="utf-8") as handle:
            handle.write(markdown)
    # The next daily run starts from the last successful one's commit, so a
    # run that selected mutants but tested none fails the workflow to keep
    # the changes in the next run's range.
    return 1 if patterns and parsed is None else 0


def main(argv: list[str] | None = None) -> int:
    """Run ``select`` or ``report``.

    Args:
        argv: Command-line arguments; ``sys.argv`` when ``None``.

    Returns:
        0, including when mutants were missed (they become an issue); 1 when
        ``report`` finds mutmut tested none of the selected mutants, so the
        workflow fails and the next run covers the same changes.
    """
    argv = sys.argv[1:] if argv is None else argv
    commands = {"select": select_main, "report": report_main}
    if not argv or argv[0] not in commands:
        sys.stderr.write("usage: mutmut_report.py {select,report} ...\n")
        return 2
    return commands[argv[0]](argv[1:])


if __name__ == "__main__":
    sys.exit(main())
