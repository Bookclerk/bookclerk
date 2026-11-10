"""The SDK modules injected into a Python isolate cover what the SDK imports.

A Python workerd isolate only holds the modules the launcher injects, so an
SDK import that reaches a module missing from that list fails with
``ModuleNotFoundError`` inside the isolate (#748). These tests walk the
in-package imports of ``workerd.py`` with :mod:`ast` and compare the closure
with the lists in the Python sparse config, the TypeScript sparse config and
the launcher (``bookclerk-workerd``).
"""

from __future__ import annotations

import ast
import re
from pathlib import Path

import bookclerk_plugin_sdk
from bookclerk_plugin_sdk.sparse_workerd.config import (
    SDK_PY_INIT_MODULE,
    SDK_PY_SIBLING_MODULES,
    SDK_PY_WORKERD_MODULE,
    is_legacy_sdk_embed,
)

# Find the repository by a folder its root holds, never by a fixed depth
# (docs/contributing/ci.md#python-sdk).
REPO_ROOT = next(
    p for p in Path(__file__).resolve().parents if (p / "crates" / "bookclerk-plugin-abi").is_dir()
)
PACKAGE_DIR = Path(bookclerk_plugin_sdk.__file__).resolve().parent
PACKAGE = "bookclerk_plugin_sdk"


def _is_type_checking(node: ast.If) -> bool:
    """Whether ``node`` is ``if TYPE_CHECKING:``, whose body never runs."""
    test = node.test
    return (isinstance(test, ast.Name) and test.id == "TYPE_CHECKING") or (
        isinstance(test, ast.Attribute) and test.attr == "TYPE_CHECKING"
    )


def _runtime_imports(source: str) -> set[str]:
    """In-package modules ``source`` imports at run time, as isolate paths.

    Lazy imports inside functions count; imports under ``if TYPE_CHECKING:``
    do not.

    Args:
        source: Python module source.

    Returns:
        Paths such as ``bookclerk_plugin_sdk/limits.py``.
    """
    found: set[str] = set()

    def visit(node: ast.AST) -> None:
        if isinstance(node, ast.If) and _is_type_checking(node):
            for child in node.orelse:
                visit(child)
            return
        if isinstance(node, ast.ImportFrom):
            if node.level == 1 and node.module:
                found.add(f"{PACKAGE}/{node.module.split('.')[0]}.py")
            elif node.level == 1:
                found.update(f"{PACKAGE}/{alias.name}.py" for alias in node.names)
            elif node.level == 0 and (node.module or "").startswith(f"{PACKAGE}."):
                found.add(f"{PACKAGE}/{node.module.split('.')[1]}.py")
        elif isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name.startswith(f"{PACKAGE}."):
                    found.add(f"{PACKAGE}/{alias.name.split('.')[1]}.py")
        for child in ast.iter_child_nodes(node):
            visit(child)

    visit(ast.parse(source))
    return found


def _closure() -> set[str]:
    """Every SDK module reachable from ``workerd.py``, except ``__init__.py``.

    The isolate's ``__init__.py`` is a stub, not the package's own.
    """
    pending = [SDK_PY_WORKERD_MODULE]
    reached: set[str] = set()
    while pending:
        module = pending.pop()
        if module in reached:
            continue
        reached.add(module)
        source = (PACKAGE_DIR / module.removeprefix(f"{PACKAGE}/")).read_text(encoding="utf-8")
        pending.extend(_runtime_imports(source))
    return reached


def _injected() -> set[str]:
    """Every module the Python sparse config injects."""
    return {
        SDK_PY_WORKERD_MODULE,
        SDK_PY_INIT_MODULE,
        *(mod_name for mod_name, _, _ in SDK_PY_SIBLING_MODULES),
    }


def test_runtime_imports_reads_every_form() -> None:
    source = (
        "from typing import TYPE_CHECKING\n"
        "from .limits import X\n"
        "from . import _abi as A\n"
        "import bookclerk_plugin_sdk._wire\n"
        "def f():\n"
        "    from bookclerk_plugin_sdk.db_value import Y\n"
        "if TYPE_CHECKING:\n"
        "    from .abi import Z\n"
        "else:\n"
        "    from ._capnp import Q\n"
    )
    assert _runtime_imports(source) == {
        "bookclerk_plugin_sdk/limits.py",
        "bookclerk_plugin_sdk/_abi.py",
        "bookclerk_plugin_sdk/_wire.py",
        "bookclerk_plugin_sdk/db_value.py",
        "bookclerk_plugin_sdk/_capnp.py",
    }


def test_injected_modules_cover_the_import_closure() -> None:
    closure = _closure()
    # The modules #748 found missing are reached, so the walk is real.
    assert {
        "bookclerk_plugin_sdk/_capnp.py",
        "bookclerk_plugin_sdk/_wire.py",
        "bookclerk_plugin_sdk/plugin_migration.py",
    } <= closure
    assert closure - _injected() == set()


def test_injected_sources_exist_and_are_not_vendored_twice() -> None:
    for mod_name, src_name, _embed in SDK_PY_SIBLING_MODULES:
        assert mod_name == f"{PACKAGE}/{src_name}"
        assert (PACKAGE_DIR / src_name).is_file()
        assert is_legacy_sdk_embed(mod_name)
    assert is_legacy_sdk_embed(SDK_PY_WORKERD_MODULE)
    assert not is_legacy_sdk_embed(f"{PACKAGE}/cli.py")


def test_typescript_sparse_config_injects_the_same_modules() -> None:
    source = (REPO_ROOT / "packages/plugin-sdk/src/sparse-workerd/config.ts").read_text(
        encoding="utf-8"
    )
    block = source.split("const SDK_PY_SIBLING_MODULES", 1)[1].split("];", 1)[0]
    entries = set(re.findall(r'\["([^"]+)", "([^"]+)", "([^"]+)"\]', block))
    assert entries == set(SDK_PY_SIBLING_MODULES)


def test_launcher_injects_the_same_modules() -> None:
    source = (REPO_ROOT / "crates/bookclerk-workerd/src/config.rs").read_text(encoding="utf-8")
    consts = dict(re.findall(r'pub const (SDK_PY_\w+_MODULE): &str = "([^"]+)";', source))
    block = source.split("const SDK_PY_MODULES: &[InjectedPyModule] = &[", 1)[1].split("];", 1)[0]
    injected = {consts[name] for name in re.findall(r"module: (SDK_PY_\w+_MODULE),", block)}
    assert injected == _injected()
