#!/usr/bin/env python3
"""Scopes and summarizes mutmut mutation runs on the Python plugin SDK.

The Python side of mutation testing (``docs/contributing/ci.md#python-sdk``):
``.github/workflows/mutation-python.yml`` runs mutmut on the functions a pull
request changes in ``packages/plugin-sdk-python/src`` and then this script,
which writes the Markdown summary. Like the Rust and Worker jobs, the trial
is report-only and the script always exits 0.

Two subcommands:

- ``select --diff FILE --root DIR`` prints the mutmut mutant-name patterns
  for the SDK at ``DIR``, one per line, or nothing when no source line
  changed. mutmut names each mutant after its function
  (``<module>.x_<function>__mutmut_<n>``, or
  ``<module>.xǁ<Class>ǁ<method>__mutmut_<n>`` for a method), so a changed
  function becomes ``<module>.x_<function>__mutmut_*``. mutmut only mutates
  function bodies, so a changed line outside every function (a constant, an
  import, a class attribute) widens to the whole module, ``<module>.x*``:
  the tests of the code beside the changed data are then checked. Blank
  lines, comments and docstrings outside functions widen nothing. Tests and
  the generated ABI modules (``do_not_mutate`` in the SDK's
  ``pyproject.toml``) are never selected.
- ``report --mutants DIR --patterns FILE --root DIR`` reads the results
  mutmut keeps beside each mutated module (``mutants/src/**/<file>.meta``),
  keeps the mutants the patterns select, appends the summary to
  ``$GITHUB_STEP_SUMMARY`` (``--summary FILE`` overrides) and annotates each
  missed mutant on the diff, with the line and change read from mutmut's
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
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from mutation_report import MAX_LINE, MAX_LISTED, Outcome, Results, _code
from stryker_report import SAFE_PATH, annotations, changed_lines

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

#: A pattern ``select`` writes: one function's mutants, or a whole module's.
PATTERN = re.compile(r"^(?P<module>[A-Za-z0-9_.]{1,2100})\.(?:(?P<base>x(?:_|ǁ)[A-Za-z0-9_ǁ]{1,400})__mutmut_\*|x\*)$")

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
        generated: The generated modules, from ``generated_modules``.

    Returns:
        True for a ``.py`` file under ``src/`` that is not generated and not
        in a test folder.
    """
    parts = PurePosixPath(path).parts
    if len(parts) < 2 or parts[0] != "src" or not parts[-1].endswith(".py") or path in generated:
        return False
    return not any(part in {"tests", "test", "__pycache__"} for part in parts[1:-1])


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


def targets(source: str, lines: set[int]) -> tuple[list[str], bool]:
    """Find the functions of one module that the changed lines fall in.

    Args:
        source: The module's new text.
        lines: The lines the pull request adds, 1-based.

    Returns:
        The mangled names mutmut gives the changed functions (``x_<name>``
        for a module-level function, ``xǁ<Class>ǁ<name>`` for a method of a
        module-level class), sorted, and whether a changed line falls in
        code outside every function, so the whole module should be
        mutated. A module that does not parse, or a changed function whose
        name is not a plain ASCII identifier, widens too.
    """
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError, RecursionError, MemoryError):
        return [], True
    source_lines = source.splitlines()
    names: set[str] = set()
    widen = False

    def hit(start: int, end: int) -> bool:
        return any(start <= line <= end for line in lines)

    def function(name: str, start: int, end: int) -> None:
        nonlocal widen
        if hit(start, end):
            if all(IDENTIFIER.match(part) for part in name.split("ǁ")):
                names.add("x" + ("ǁ" if "ǁ" in name else "_") + name)
            else:
                widen = True

    for node in tree.body:
        start, end = _span(node)
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            function(node.name, start, end)
        elif isinstance(node, ast.ClassDef):
            covered: list[tuple[int, int]] = []
            for item in node.body:
                item_start, item_end = _span(item)
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    function(f"{node.name}ǁ{item.name}", item_start, item_end)
                    covered.append((item_start, item_end))
                elif _is_docstring(item):
                    covered.append((item_start, item_end))
            # Class-level code outside its methods and docstring: the class
            # line, decorators, bases and attributes.
            rest = {line for line in lines if start <= line <= end and not any(a <= line <= b for a, b in covered)}
            if any(_code_line(source_lines, line) for line in rest):
                widen = True
        elif not _is_docstring(node) and hit(start, end) and any(_code_line(source_lines, line) for line in lines if start <= line <= end):
            widen = True
    return sorted(names), widen


def _code_line(lines: list[str], line: int) -> bool:
    """Say whether a 1-based line holds code, not only blanks or a comment."""
    if not 0 < line <= len(lines):
        return False
    text = lines[line - 1].strip()
    return bool(text) and not text.startswith("#")


def patterns_for(module: str, names: list[str], widen: bool) -> list[str]:
    """Write the mutmut patterns for one module's changed functions.

    Args:
        module: The dotted module name.
        names: The mangled function names, from ``targets``.
        widen: Whether the whole module is mutated instead.

    Returns:
        ``<module>.x*`` when widened, otherwise one
        ``<module>.<name>__mutmut_*`` per function.
    """
    if widen:
        return [f"{module}.x*"]
    return [f"{module}.{name}__mutmut_*" for name in names]


def select(diff: str, root: str, read: dict[str, str], generated: frozenset[str]) -> list[str]:
    """Build the mutmut patterns for a pull request's changes to the SDK.

    Args:
        diff: The pull request's unified diff.
        root: The SDK's directory, relative to the repository root.
        read: Each changed source file's new text, keyed by its path
            relative to ``root``; a file missing here (deleted) is skipped.
        generated: The generated modules, from ``generated_modules``.

    Returns:
        The patterns, one module after another in path order.
    """
    out: list[str] = []
    for path, runs in changed_lines(diff, root, lambda p: is_source(p, generated)).items():
        module = module_name(path)
        if module is None or path not in read:
            continue
        lines = {line for start, end in runs for line in range(start, end + 1)}
        names, widen = targets(read[path], lines)
        out += patterns_for(module, names, widen)
    return out


def parse_patterns(text: str) -> list[tuple[str, str | None]]:
    """Read the patterns ``select`` wrote back into module and function.

    Args:
        text: The patterns file, one per line.

    Returns:
        ``(module, mangled function)`` per pattern, the function ``None``
        for a whole module; lines that are not a pattern ``select`` writes
        are left out.
    """
    out = []
    for line in text.splitlines()[:10000]:
        match = PATTERN.match(line.strip())
        if match:
            out.append((match.group("module"), match.group("base")))
    return out


def selected(key: str, patterns: list[tuple[str, str | None]]) -> bool:
    """Say whether a mutant name is one the patterns select.

    The check is exact: a whole-module pattern selects that module's
    functions only, not a submodule's whose name starts with ``x``.
    """
    match = MUTANT.match(key)
    if not match:
        return False
    return any(match.group("module") == module and (base is None or match.group("base") == base) for module, base in patterns)


def parse_meta(text: str) -> dict[str, int | None]:
    """Read one of mutmut's ``.meta`` files.

    Args:
        text: The file's JSON.

    Returns:
        Each mutant's exit code (``None`` when mutmut did not test it),
        keyed by mutant name; empty when the file is not mutmut's.
    """
    try:
        data = json.loads(text)
    except (ValueError, RecursionError):
        return {}
    codes = data.get("exit_code_by_key") if isinstance(data, dict) else None
    if not isinstance(codes, dict):
        return {}
    return {k: v if type(v) is int else None for k, v in codes.items() if isinstance(k, str)}


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
            original: The module's text as the pull request has it.
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


def _clean(text: str) -> str:
    """Make text printable and bounded."""
    return "".join(c if c.isprintable() else "?" for c in text)[:MAX_LINE]


def collect(
    metas: dict[str, str],
    modules: dict[str, tuple[str, str]],
    patterns: list[tuple[str, str | None]],
    root: str,
) -> tuple[Results, list[tuple[str, int, str]]]:
    """Gather the selected mutants' outcomes from mutmut's files.

    Args:
        metas: Each ``.meta`` file's text, keyed by the source path it
            belongs to, relative to the SDK (``src/…/tools.py``).
        modules: ``(original, mutated)`` text per source path, for
            describing missed mutants; a path missing here leaves them
            undescribed.
        patterns: The selection, from ``parse_patterns``.
        root: The SDK's directory, prefixed to paths so they are
            repository-relative.

    Returns:
        The mutants by outcome, each described as ``path:line: old → new``,
        and ``(path, line, description)`` for each missed one with a line.
    """
    results = Results()
    survivors: list[tuple[str, int, str]] = []
    prefix = root.rstrip("/") + "/"
    for source_path in sorted(metas):
        codes = parse_meta(metas[source_path])
        keys = [k for k in codes if selected(k, patterns)]
        if not keys:
            continue
        indexed = Module.read(*modules[source_path]) if source_path in modules else None
        path = _clean(prefix + source_path)
        for key in sorted(keys, key=_natural):
            code = codes[key]
            outcome = EXIT_CODES.get(code) if code is not None else None
            if outcome is None:
                continue
            line, change = indexed.change(key) if indexed is not None and outcome is Outcome.MISSED else (0, "")
            name = _clean(key.rsplit(".", 1)[-1])
            description = _clean(f"{path}:{line}: {change} ({name})" if change else f"{path}: {name}")
            results.mutants[outcome].append(description)
            if outcome is Outcome.MISSED and line:
                survivors.append((path, line, description))
    return results, survivors


def _natural(key: str) -> tuple[str, int]:
    """Sort mutant names by function, then by number."""
    match = MUTANT.match(key)
    return (match.group("base"), int(match.group("n"))) if match else (key, 0)


def render(
    parsed: tuple[Results, list[tuple[str, int, str]]] | None,
    *,
    patterns: list[tuple[str, str | None]],
    stopped: bool = False,
) -> str:
    """Write the Markdown summary of the SDK's run.

    Args:
        parsed: The collected outcomes, or ``None`` when mutmut left no
            results.
        patterns: The selection; empty when no source line changed, so
            mutmut did not run.
        stopped: Whether the workflow stopped mutmut at its time limit, so
            only the mutants tested before then are counted.

    Returns:
        The summary, ending with a newline.
    """
    lines = ["## Mutation testing (Python SDK)", ""]
    widened = sorted({module for module, base in patterns if base is None})
    if not patterns:
        lines.append("No mutants to test: the change touches no source line of the SDK.")
    elif parsed is None:
        lines.append("mutmut did not finish; check the job log.")
    else:
        results = parsed[0]
        if widened:
            names = ", ".join(_code(m) for m in widened)
            lines += [f"Changed lines outside any function in {names}, so all of their functions were mutated.", ""]
        if stopped:
            lines += ["mutmut reached the time limit, so only the mutants tested before it are counted.", ""]
        score = results.score()
        if score is None:
            lines.append("mutmut found no mutant in the changed functions.")
        else:
            missed = results.count(Outcome.MISSED)
            lines.append(f"The tests caught **{score:.0f}%** of the {results.tested} mutants of the changed functions ({missed} missed).")
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
    lines += ["", "Report only: missed mutants do not fail the pull request yet."]
    return "\n".join(lines) + "\n"


def _read(path: Path) -> str | None:
    """Read a text file no larger than ``MAX_FILE_BYTES``, or return ``None``."""
    try:
        if not path.is_file() or path.stat().st_size > MAX_FILE_BYTES:
            return None
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def select_main(argv: list[str]) -> int:
    """Print the mutmut patterns for the SDK's changed functions."""
    parser = argparse.ArgumentParser(prog="mutmut_report.py select")
    parser.add_argument("--diff", type=Path, required=True, help="the pull request's diff")
    parser.add_argument("--root", required=True, help="the SDK's directory, relative to the repository root")
    args = parser.parse_args(argv)
    root = Path(args.root)
    generated = generated_modules(_read(root / "pyproject.toml") or "")
    diff = args.diff.read_text(encoding="utf-8", errors="replace")
    read = {}
    for path in changed_lines(diff, args.root, lambda p: is_source(p, generated)):
        text = _read(root / path)
        if text is not None:
            read[path] = text
    patterns = select(diff, args.root, read, generated)
    sys.stdout.write("".join(p + "\n" for p in patterns))
    return 0


def report_main(argv: list[str]) -> int:
    """Write the SDK's summary and annotations."""
    parser = argparse.ArgumentParser(prog="mutmut_report.py report")
    parser.add_argument("--mutants", type=Path, required=True, help="mutmut's mutants/ folder")
    parser.add_argument("--patterns", type=Path, required=True, help="the patterns `select` printed")
    parser.add_argument("--root", required=True, help="the SDK's directory, relative to the repository root")
    parser.add_argument("--stopped", action="store_true", help="the workflow stopped mutmut at its time limit")
    parser.add_argument("--out", type=Path, help="folder to write <outcome>.txt into, one mutant per line, for the artifact")
    parser.add_argument("--summary", default=os.environ.get("GITHUB_STEP_SUMMARY", ""), help="file to append the summary to (none when empty)")
    args = parser.parse_args(argv)
    patterns = parse_patterns(_read(args.patterns) or "")
    parsed = None
    if patterns and (args.mutants / "src").is_dir():
        metas: dict[str, str] = {}
        modules: dict[str, tuple[str, str]] = {}
        for meta in sorted((args.mutants / "src").rglob("*.py.meta"))[:MAX_META_FILES]:
            source_path = meta.relative_to(args.mutants).as_posix()[: -len(".meta")]
            if not SAFE_PATH.match(source_path):
                continue
            text = _read(meta)
            if text is None:
                continue
            metas[source_path] = text
            original, mutated = _read(Path(args.root) / source_path), _read(args.mutants / source_path)
            if original is not None and mutated is not None:
                modules[source_path] = (original, mutated)
        if any(parse_meta(text) for text in metas.values()):
            parsed = collect(metas, modules, patterns, args.root)
    markdown = render(parsed, patterns=patterns, stopped=args.stopped)
    sys.stdout.write(markdown)
    if parsed is not None:
        sys.stdout.write(annotations(parsed[1]))
        if args.out is not None:
            args.out.mkdir(parents=True, exist_ok=True)
            for outcome in Outcome:
                (args.out / f"{outcome.value}.txt").write_text("".join(m + "\n" for m in parsed[0].mutants[outcome]), encoding="utf-8")
    if args.summary:
        with Path(args.summary).open("a", encoding="utf-8") as handle:
            handle.write(markdown)
    return 0


def main(argv: list[str] | None = None) -> int:
    """Run ``select`` or ``report``.

    Args:
        argv: Command-line arguments; ``sys.argv`` when ``None``.

    Returns:
        0: the trial never fails the workflow.
    """
    argv = sys.argv[1:] if argv is None else argv
    commands = {"select": select_main, "report": report_main}
    if not argv or argv[0] not in commands:
        sys.stderr.write("usage: mutmut_report.py {select,report} ...\n")
        return 2
    return commands[argv[0]](argv[1:])


if __name__ == "__main__":
    sys.exit(main())
