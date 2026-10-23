"""Render a Python package's public API as Markdown, one page per module.

``docs-markdown.py`` uses this for ``api/python/``. Modules are read with
:mod:`ast`, never imported, so nothing in the package runs during the build.

A module's public names are its ``__all__`` when it has one, otherwise every
top-level class, function and annotated or upper-case constant whose name
does not start with ``_``. A name re-exported from another public module
links there; one re-exported from a private module (``_abi``) is documented
where it is re-exported.

Google-style docstring sections (``Args:``, ``Returns:``, ``Raises:``,
``Attributes:``, ``Example:`` ...) become labelled lists, reST literals
(````x````) become Markdown code, and cross-reference roles
(``:class:`Foo```) become links when the target is documented.
"""

from __future__ import annotations

import ast
import re
import textwrap
from collections.abc import Iterable, Iterator, Mapping
from dataclasses import dataclass, field
from pathlib import Path

from rustdoc_markdown import slug

SECTION_RE = re.compile(
    r"^(Args|Arguments|Parameters|Params|Keyword Args|Keyword Arguments|Other Parameters|Returns|Return|Yields|Yield|"
    r"Raises|Raise|Except|Exceptions|Attributes|Example|Examples|Note|Notes|Warning|Warnings|See Also|Todo|References)"
    r":\s*$"
)
# Sections whose body is a list of ``name (type): description`` entries.
ENTRY_SECTIONS = frozenset(
    {"Args", "Arguments", "Parameters", "Params", "Keyword Args", "Keyword Arguments", "Other Parameters", "Raises", "Raise", "Except", "Exceptions", "Attributes"}
)
SECTION_TITLES = {
    "Arguments": "Args",
    "Parameters": "Args",
    "Params": "Args",
    "Keyword Arguments": "Keyword args",
    "Keyword Args": "Keyword args",
    "Return": "Returns",
    "Yield": "Yields",
    "Raise": "Raises",
    "Except": "Raises",
    "Exceptions": "Raises",
    "Example": "Example",
    "Note": "Note",
    "Warnings": "Warning",
}
ENTRY_RE = re.compile(r"^(\*{0,2}[A-Za-z_][\w.]*)\s*(?:\(([^)]*)\))?\s*:\s*(.*)$")
ROLE_RE = re.compile(r":(?:py:)?(?:class|func|meth|mod|attr|data|exc|obj|const):`(~?)([^`]+)`")
RST_LITERAL_RE = re.compile(r"``([^`]+?)``")
CODE_BLOCK_RE = re.compile(r"^(\s*)\.\. code-block::\s*(\w*)\s*$")
FENCE_RE = re.compile(r"^\s*(```|~~~)")
DOCTEST_RE = re.compile(r"^\s*>>> ")


@dataclass
class Member:
    """A documented name on a module page.

    Attributes:
        name: The name it is exported under.
        node: Its definition.
        defined_in: Dotted module that defines it.
        doc: The string literal after a constant, which documents it by
            convention, or None.
    """

    name: str
    node: ast.AST
    defined_in: str
    doc: str | None = None


@dataclass
class ModulePage:
    """One public module.

    Attributes:
        module: Dotted module name.
        tree: The parsed module.
        members: Names documented here, in export order.
        links: Names exported here but documented on another page:
            name here -> (dotted module of that page, name there).
    """

    module: str
    tree: ast.Module
    members: list[Member] = field(default_factory=list)
    links: dict[str, tuple[str, str]] = field(default_factory=dict)

    @property
    def file(self) -> str:
        """The page's path under ``api/python``.

        Returns:
            ``<module>.md``.
        """
        return f"{self.module}.md"


def _source_file(src: Path, module: str) -> Path | None:
    """Return the file of a dotted module under ``src``.

    Args:
        src: Directory holding the top-level package.
        module: Dotted name.

    Returns:
        ``.../mod.py`` or ``.../pkg/__init__.py``, or None.
    """
    base = src.joinpath(*module.split("."))
    for path in (base.with_suffix(".py"), base / "__init__.py"):
        if path.is_file():
            return path
    return None


def _resolve_relative(module: str, is_package: bool, node: ast.ImportFrom) -> str | None:
    """Return the absolute module an ``ImportFrom`` names.

    Args:
        module: The importing module's dotted name.
        is_package: Whether the importing module is a package ``__init__``.
        node: The import.

    Returns:
        The dotted module, or None when it climbs above the package.
    """
    if node.level == 0:
        return node.module
    parts = module.split(".")
    if not is_package:
        parts = parts[:-1]
    if node.level > 1:
        if node.level - 1 > len(parts):
            return None
        parts = parts[: len(parts) - (node.level - 1)]
    if node.module:
        parts += node.module.split(".")
    return ".".join(parts)


def _module_statements(tree: ast.Module) -> Iterator[ast.stmt]:
    """Yield the statements that run at import time, in order.

    Top-level statements, and those inside top-level ``if``, ``try`` and
    ``with`` blocks (every branch, since which one runs is not known), but
    not function or class bodies.

    Args:
        tree: The parsed module.

    Yields:
        Each statement.
    """
    yield from _walk_block(tree.body)


def _walk_block(body: list[ast.stmt]) -> Iterator[ast.stmt]:
    """Yield a block's statements and those of its nested import-time blocks.

    Args:
        body: Statements of one block.

    Yields:
        Each statement, then the statements of its ``if`` / ``try`` /
        ``with`` branches.
    """
    for node in body:
        yield node
        blocks: list[list[ast.stmt]] = []
        if isinstance(node, ast.If):
            blocks = [node.body, node.orelse]
        elif isinstance(node, (ast.Try, getattr(ast, "TryStar", ast.Try))):
            blocks = [node.body, *(h.body for h in node.handlers), node.orelse, node.finalbody]
        elif isinstance(node, (ast.With, ast.AsyncWith)):
            blocks = [node.body]
        for block in blocks:
            yield from _walk_block(block)


#: ``ast.TypeAlias`` (PEP 695 ``type X = ...``) where this Python has it.
_TYPE_ALIAS: tuple[type, ...] = (ast.TypeAlias,) if hasattr(ast, "TypeAlias") else ()


def _literal_names(value: ast.expr) -> list[str]:
    """Evaluate a list or tuple of strings, or several joined with ``+``.

    Args:
        value: The expression.

    Returns:
        The strings.

    Raises:
        ValueError: The expression is anything else.
    """
    if isinstance(value, ast.BinOp) and isinstance(value.op, ast.Add):
        return _literal_names(value.left) + _literal_names(value.right)
    literal = ast.literal_eval(value)
    if not isinstance(literal, (list, tuple)) or not all(isinstance(v, str) for v in literal):
        raise ValueError("not a list or tuple of strings")
    return list(literal)


def _all_names(tree: ast.Module) -> list[str] | None:
    """Return a module's ``__all__``.

    Handles ``__all__ = [...]`` (or a tuple, or literals joined with ``+``)
    and later ``__all__ += [...]`` / ``__all__.extend([...])`` /
    ``__all__.append("...")``.

    Args:
        tree: The parsed module.

    Returns:
        The names, or None when the module has no ``__all__``.

    Raises:
        ValueError: ``__all__`` is built from something other than literals,
            which this renderer cannot read without importing the module.
    """
    names: list[str] | None = None
    for node in _module_statements(tree):
        value: ast.expr | None = None
        extend = False
        if isinstance(node, (ast.Assign, ast.AnnAssign)):
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            if any(isinstance(t, ast.Name) and t.id == "__all__" for t in targets):
                value = node.value
        elif isinstance(node, ast.AugAssign) and isinstance(node.target, ast.Name) and node.target.id == "__all__":
            value, extend = node.value, True
        elif (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Attribute)
            and isinstance(node.value.func.value, ast.Name)
            and node.value.func.value.id == "__all__"
        ):
            call = node.value
            if call.func.attr == "extend" and len(call.args) == 1 and not call.keywords:
                value, extend = call.args[0], True
            elif call.func.attr == "append" and len(call.args) == 1 and not call.keywords:
                value, extend = ast.List(elts=[call.args[0]], ctx=ast.Load()), True
            else:
                # insert, remove, clear, ...: the result cannot be read safely.
                raise ValueError(f"__all__ is changed with .{call.func.attr}() (line {node.lineno})")
        if value is None:
            continue
        try:
            found = _literal_names(value)
        except ValueError as err:
            raise ValueError(f"__all__ is not built from string literals (line {node.lineno})") from err
        names = (names or []) + found if extend else found
    return names


def _definitions(tree: ast.Module) -> dict[str, ast.AST]:
    """Index a module's top-level definitions by name.

    Args:
        tree: The parsed module.

    Returns:
        Name -> class, function or assignment node.
    """
    out: dict[str, ast.AST] = {}
    for node in _module_statements(tree):
        if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            out[node.name] = node
        elif isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
            out[node.target.id] = node
        elif isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name):
                    out[t.id] = node
        elif isinstance(node, _TYPE_ALIAS):
            out[node.name.id] = node
    return out


def _imports(module: str, is_package: bool, tree: ast.Module) -> dict[str, tuple[str, str]]:
    """Index a module's ``from x import y`` names.

    Args:
        module: The module's dotted name.
        is_package: Whether it is a package ``__init__``.
        tree: The parsed module.

    Returns:
        Local name -> (source module, name there).
    """
    out = {}
    for node in _module_statements(tree):
        if isinstance(node, ast.ImportFrom):
            source = _resolve_relative(module, is_package, node)
            if source is None:
                continue
            for alias in node.names:
                out[alias.asname or alias.name] = (source, alias.name)
    return out


def _is_public_constant(name: str, node: ast.AST) -> bool:
    """Tell whether a top-level assignment is part of the public API.

    Args:
        name: The assigned name.
        node: The assignment.

    Returns:
        True for annotated assignments, upper-case names, ``type`` aliases,
        and CapWords names bound to a type expression (``DbValue =
        Union[...]``, ``Id = str | int``, ``T = TypeVar("T")``).
    """
    if isinstance(node, ast.AnnAssign) or name.isupper() or isinstance(node, _TYPE_ALIAS):
        return True
    return name[:1].isupper() and isinstance(node, ast.Assign) and _is_type_expression(node.value)


#: Calls that create a type rather than a value.
_TYPE_FACTORIES = frozenset({"TypeVar", "ParamSpec", "TypeVarTuple", "NewType", "TypeAliasType"})


def _is_type_expression(value: ast.expr) -> bool:
    """Tell whether an assigned value reads as a type alias.

    Args:
        value: The right-hand side.

    Returns:
        True for a subscripted generic (``Union[...]``, ``dict[str, int]``),
        a ``|`` union of such or of names, and a call to a type factory
        such as ``TypeVar``.
    """
    if isinstance(value, ast.Subscript):
        return True
    if isinstance(value, ast.BinOp) and isinstance(value.op, ast.BitOr):
        return all(isinstance(side, (ast.Name, ast.Attribute, ast.Subscript, ast.Constant, ast.BinOp)) for side in (value.left, value.right))
    if isinstance(value, ast.Call):
        func = value.func
        name = func.id if isinstance(func, ast.Name) else func.attr if isinstance(func, ast.Attribute) else ""
        return name in _TYPE_FACTORIES
    return False


class Package:
    """The public modules of one package, parsed.

    Args:
        src: Directory holding the package.
        modules: Dotted public modules to document.
    """

    def __init__(self, src: Path, modules: Iterable[str]) -> None:
        """Parse every module and place its public names."""
        self.src = src
        self.public = list(modules)
        self._trees: dict[str, tuple[ast.Module, bool]] = {}
        self.pages = [self._page(m) for m in self.public]
        self.where: dict[str, tuple[str, str]] = {}
        for page in self.pages:
            for heading, qual in _page_anchors(page):
                self.where.setdefault(qual, (page.file, heading))

    def _tree(self, module: str) -> tuple[ast.Module, bool] | None:
        """Parse a module once.

        Args:
            module: Dotted name.

        Returns:
            (tree, is_package), or None when the file does not exist.
        """
        if module not in self._trees:
            path = _source_file(self.src, module)
            if path is None:
                return None
            self._trees[module] = (ast.parse(path.read_text(encoding="utf-8"), filename=str(path)), path.name == "__init__.py")
        return self._trees[module]

    def _lookup(self, module: str, name: str, depth: int = 0) -> tuple[str, str, ast.AST] | None:
        """Follow imports to the definition of ``module.name``.

        Args:
            module: Dotted module.
            name: Name in it.
            depth: Recursion guard.

        Returns:
            (defining module, name there, node), or None for names defined
            outside the package.
        """
        parsed = self._tree(module)
        if parsed is None or depth > 8:
            return None
        tree, is_package = parsed
        defs = _definitions(tree)
        if name in defs:
            return module, name, defs[name]
        imported = _imports(module, is_package, tree).get(name)
        if imported is None:
            return None
        return self._lookup(imported[0], imported[1], depth + 1)

    def _exported(self, module: str) -> list[str]:
        """List the names a module's page documents or links.

        Args:
            module: Dotted module.

        Returns:
            Its ``__all__``, else its public top-level definitions.
        """
        parsed = self._tree(module)
        if parsed is None:
            return []
        tree, _ = parsed
        names = _all_names(tree)
        if names is not None:
            return names
        defs = _definitions(tree)
        return [n for n, node in defs.items() if not n.startswith("_") and (not isinstance(node, (ast.Assign, ast.AnnAssign)) or _is_public_constant(n, node))]

    def _page(self, module: str) -> ModulePage:
        """Place one module's public names.

        Args:
            module: Dotted module.

        Returns:
            Its page.
        """
        parsed = self._tree(module)
        if parsed is None:
            raise FileNotFoundError(module)
        tree, _ = parsed
        page = ModulePage(module, tree)
        for name in self._exported(module):
            found = self._lookup(module, name)
            if found is None:
                continue
            defined_in, original, node = found
            # Link only to a page that documents the name; otherwise it is
            # documented here.
            if defined_in != module and defined_in in self.public and original in self._exported(defined_in):
                page.links[name] = (defined_in, original)
            else:
                page.members.append(Member(name, node, defined_in, self._constant_doc(defined_in, node)))
        return page

    def _constant_doc(self, module: str, node: ast.AST) -> str | None:
        """Return the string literal that follows a constant's assignment or a ``type`` alias.

        Args:
            module: The module defining it.
            node: The assignment.

        Returns:
            The cleaned string, or None for other nodes.
        """
        parsed = self._tree(module)
        if parsed is None or not isinstance(node, (ast.Assign, ast.AnnAssign, *_TYPE_ALIAS)):
            return None
        body = parsed[0].body
        for i, stmt in enumerate(body[:-1]):
            nxt = body[i + 1]
            if stmt is node and isinstance(nxt, ast.Expr) and isinstance(nxt.value, ast.Constant) and isinstance(nxt.value.value, str):
                return textwrap.dedent(nxt.value.value).strip()
        return None

    def summary(self, module: str) -> str:
        """Return the first paragraph of a module's docstring on one line.

        Args:
            module: Dotted module name.

        Returns:
            The summary, or ``""`` for a module without a docstring or source.
        """
        parsed = self._tree(module)
        return "" if parsed is None else _summary(ast.get_docstring(parsed[0]))

    def render_all(self) -> dict[str, str]:
        """Render every page.

        Returns:
            Page file under ``api/python`` -> Markdown.
        """
        return {page.file: self._render(page) for page in self.pages}

    # -- rendering ---------------------------------------------------------

    def _link(self, page: ModulePage, target: str) -> str | None:
        """Resolve a cross-reference role's target from a page.

        Tries the dotted name as given, then relative to the page's module
        and package, then a unique last-segment match.

        Args:
            page: The page the reference appears on.
            target: The role's target (``Foo``, ``mod.Foo.bar``).

        Returns:
            A relative link, or None.
        """
        package = self.public[0].split(".")[0] if self.public else ""
        candidates = [target, f"{page.module}.{target}", f"{package}.{target}"]
        for qual in candidates:
            if qual in self.where:
                file, anchor = self.where[qual]
                return (f"{file}" if file != page.file else "") + (f"#{anchor}" if anchor else "")
            if qual in self.public:
                return f"{qual}.md"
        tail = target.split(".")[-1]
        matches = {v for k, v in self.where.items() if k.split(".")[-1] == tail}
        if len(matches) == 1:
            file, anchor = matches.pop()
            return (file if file != page.file else "") + (f"#{anchor}" if anchor else "")
        return None

    def _render(self, page: ModulePage) -> str:
        """Render one module page.

        Args:
            page: The page.

        Returns:
            Markdown.
        """
        lines = [f"# {page.module}", ""]
        doc = ast.get_docstring(page.tree)
        if doc:
            lines += [self._docstring(page, doc), ""]
        functions = [m for m in page.members if isinstance(m.node, (ast.FunctionDef, ast.AsyncFunctionDef))]
        classes = [m for m in page.members if isinstance(m.node, ast.ClassDef)]
        data = [m for m in page.members if m not in functions and m not in classes]
        rows = [(m, "class") for m in classes] + [(m, "function") for m in functions] + [(m, "constant") for m in data]
        if len(rows) > 1:
            lines += ["| Name | Kind | Summary |", "| --- | --- | --- |"]
            for m, kind in rows:
                # ast.get_docstring only takes classes, functions and modules;
                # constants and `type` aliases carry their doc on the member.
                doc = ast.get_docstring(m.node) if isinstance(m.node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) else m.doc
                lines.append(f"| [`{m.name}`](#{slug(m.name)}) | {kind} | {_summary(doc)} |")
            lines.append("")
        if page.links:
            lines += ["## Re-exports", ""]
            for name, (target, original) in page.links.items():
                shown = f"`{name}`" if name == original else f"`{name}` (`{original}`)"
                lines.append(f"- [{shown}]({target}.md#{slug(original)}) from `{target}`")
            lines.append("")
        if classes:
            lines += ["## Classes", ""]
            for m in classes:
                lines += self._class(page, m)
        if functions:
            lines += ["## Functions", ""]
            for m in functions:
                lines += self._function(page, m.name, m.node, 3)
        if data:
            lines += ["## Constants", ""]
            for m in data:
                lines += [f"### {m.name}", "", "```python", _assignment(m.name, m.node), "```", ""]
                if m.doc:
                    lines += [self._docstring(page, m.doc), ""]
        return "\n".join(lines).rstrip() + "\n"

    def _class(self, page: ModulePage, member: Member) -> list[str]:
        """Render a class with its public methods and fields.

        Args:
            page: The page.
            member: The class.

        Returns:
            Markdown lines.
        """
        node = member.node
        assert isinstance(node, ast.ClassDef)
        bases = [ast.unparse(b) for b in node.bases] + [ast.unparse(k) for k in node.keywords]
        decorators = [f"@{ast.unparse(d)}" for d in node.decorator_list]
        sig = "\n".join([*decorators, f"class {member.name}" + (f"({', '.join(bases)})" if bases else "")])
        lines = [f"### {member.name}", "", "```python", sig, "```", ""]
        doc = ast.get_docstring(node)
        if doc:
            lines += [self._docstring(page, doc), ""]
        fields = [n for n in node.body if isinstance(n, ast.AnnAssign) and isinstance(n.target, ast.Name) and not n.target.id.startswith("_")]
        if fields and (doc is None or "Attributes:" not in doc):
            lines += ["**Fields**", ""]
            for f in fields:
                assert isinstance(f.target, ast.Name)
                default = f" = {_short(ast.unparse(f.value))}" if f.value is not None else ""
                lines.append(f"- `{f.target.id}: {ast.unparse(f.annotation)}{default}`")
            lines.append("")
        for child in node.body:
            if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)) and (not child.name.startswith("_") or child.name == "__init__"):
                if child.name == "__init__" and ast.get_docstring(child) is None and len(child.args.args) <= 1:
                    continue
                lines += self._function(page, f"{member.name}.{child.name}", child, 4)
        return lines

    def _function(self, page: ModulePage, label: str, node: ast.AST, level: int) -> list[str]:
        """Render a function or method.

        Args:
            page: The page.
            label: Heading text (``name`` or ``Class.name``).
            node: The definition.
            level: Heading level.

        Returns:
            Markdown lines.
        """
        assert isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
        decorators = [f"@{ast.unparse(d)}" for d in node.decorator_list]
        prefix = "async def" if isinstance(node, ast.AsyncFunctionDef) else "def"
        args = ast.unparse(node.args)
        returns = f" -> {ast.unparse(node.returns)}" if node.returns is not None else ""
        line = f"{prefix} {node.name}({args}){returns}"
        if len(line) > 90:
            parts = _split_args(args)
            line = f"{prefix} {node.name}(\n" + "".join(f"    {p},\n" for p in parts) + f"){returns}"
        lines = [f"{'#' * level} {label}", "", "```python", "\n".join([*decorators, line]), "```", ""]
        doc = ast.get_docstring(node)
        if doc:
            lines += [self._docstring(page, doc), ""]
        return lines

    def _docstring(self, page: ModulePage, doc: str) -> str:
        """Render a Google-style docstring as Markdown.

        Args:
            page: The page, for cross-references.
            doc: The cleaned docstring.

        Returns:
            Markdown.
        """
        out: list[str] = []
        lines = doc.splitlines()
        i = 0
        while i < len(lines):
            m = SECTION_RE.match(lines[i].strip()) if not lines[i].startswith((" ", "\t")) else None
            if m is None:
                out.append(lines[i])
                i += 1
                continue
            name = m.group(1)
            body = []
            i += 1
            while i < len(lines) and (not lines[i].strip() or lines[i].startswith((" ", "\t"))):
                body.append(lines[i])
                i += 1
            while body and not body[-1].strip():
                body.pop()
            body_text = textwrap.dedent("\n".join(body))
            title = SECTION_TITLES.get(name, name)
            out += ["", f"**{title}**", ""]
            if name in ENTRY_SECTIONS:
                out += _entries(body_text)
            elif name in ("Example", "Examples") and not FENCE_RE.match(body_text.lstrip().splitlines()[0] if body_text.strip() else ""):
                out += _example(body_text)
            else:
                out += body_text.splitlines()
            out.append("")
        text = re.sub(r"\n{3,}", "\n\n", "\n".join(_code_blocks(out)))
        return self._inline(page, text).strip()

    def _inline(self, page: ModulePage, text: str) -> str:
        """Convert reST literals and roles outside fenced code.

        Args:
            page: The page, for cross-references.
            text: Markdown.

        Returns:
            Markdown.
        """
        out = []
        in_fence = False
        for line in text.splitlines():
            if FENCE_RE.match(line):
                in_fence = not in_fence
                out.append(line)
                continue
            if in_fence:
                out.append(line)
                continue

            def role(m: re.Match[str]) -> str:
                target = m.group(2).strip()
                shown = target.split(".")[-1] if m.group(1) else target
                href = self._link(page, target.lstrip("."))
                return f"[`{shown}`]({href})" if href else f"`{shown}`"

            line = ROLE_RE.sub(role, line)
            line = RST_LITERAL_RE.sub(lambda m: f"`{m.group(1)}`", line)
            out.append(line)
        return "\n".join(out)


def _page_anchors(page: ModulePage) -> list[tuple[str, str]]:
    """List a page's headings and the qualified names they document.

    Args:
        page: The page.

    Returns:
        (anchor, qualified name) pairs.
    """
    out = []
    for m in page.members:
        qual = f"{page.module}.{m.name}"
        out.append((slug(m.name), qual))
        if m.defined_in != page.module:
            out.append((slug(m.name), f"{m.defined_in}.{m.name}"))
        if isinstance(m.node, ast.ClassDef):
            for child in m.node.body:
                if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)) and not child.name.startswith("_"):
                    out.append((slug(f"{m.name}.{child.name}"), f"{qual}.{child.name}"))
    return out


def _summary(doc: str | None) -> str:
    """Return a docstring's first paragraph on one line.

    Args:
        doc: The docstring, or None.

    Returns:
        The summary with reST literals converted, ``|`` escaped.
    """
    if not doc:
        return ""
    para = []
    for line in doc.strip().splitlines():
        if not line.strip():
            break
        para.append(line.strip())
    text = RST_LITERAL_RE.sub(lambda m: f"`{m.group(1)}`", " ".join(para))
    text = ROLE_RE.sub(lambda m: f"`{m.group(2).split('.')[-1] if m.group(1) else m.group(2)}`", text)
    return text.replace("|", "\\|")


def _entries(body: str) -> list[str]:
    """Render ``name (type): description`` entries as a list.

    Args:
        body: The section's dedented body.

    Returns:
        Markdown list lines; text that is not an entry is kept as is.
    """
    out: list[str] = []
    for line in body.splitlines():
        if not line.strip():
            continue
        m = ENTRY_RE.match(line) if not line.startswith((" ", "\t")) else None
        if m:
            name, kind, text = m.groups()
            kind_text = f" ({kind})" if kind else ""
            out.append(f"- `{name}`{kind_text}: {text}".rstrip(": ").rstrip() if text else f"- `{name}`{kind_text}")
        elif out:
            out[-1] += " " + line.strip()
        else:
            out.append(line.strip())
    return out


def _example(body: str) -> list[str]:
    """Render an ``Example:`` section, fencing doctest or indented code.

    Args:
        body: The section's dedented body.

    Returns:
        Markdown lines.
    """
    lines = body.splitlines()
    if any(DOCTEST_RE.match(line) for line in lines) or all(not line or line.startswith(" ") for line in lines):
        return ["```python", *textwrap.dedent(body).splitlines(), "```"]
    return lines


def _code_blocks(lines: list[str]) -> list[str]:
    """Turn ``.. code-block::`` directives and ``::`` literal blocks into fences.

    Args:
        lines: Markdown lines.

    Returns:
        Lines with indented code fenced.
    """
    out: list[str] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        m = CODE_BLOCK_RE.match(line)
        literal = line.rstrip().endswith("::") and not line.strip().startswith("..")
        if not (m or literal):
            out.append(line)
            i += 1
            continue
        lang = m.group(2) if m else ""
        indent = len(m.group(1)) if m else len(line) - len(line.lstrip())
        if literal:
            out.append(line.rstrip()[:-1])
        i += 1
        block = []
        while i < len(lines) and (not lines[i].strip() or len(lines[i]) - len(lines[i].lstrip()) > indent):
            block.append(lines[i])
            i += 1
        while block and not block[-1].strip():
            block.pop()
        while block and not block[0].strip():
            block.pop(0)
        if block:
            out += ["", f"```{lang}", *textwrap.dedent("\n".join(block)).splitlines(), "```", ""]
    return out


def _split_args(args: str) -> list[str]:
    """Split an unparsed argument list at top-level commas.

    Args:
        args: ``ast.unparse`` of an ``arguments`` node.

    Returns:
        One string per argument.
    """
    parts, depth, cur = [], 0, ""
    for ch in args:
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur.strip())
            cur = ""
            continue
        cur += ch
    if cur.strip():
        parts.append(cur.strip())
    return parts


def _short(value: str, limit: int = 80) -> str:
    """Shorten a long value expression.

    Args:
        value: Source text.
        limit: Maximum length.

    Returns:
        The text, or its start followed by ``...``.
    """
    value = " ".join(value.split())
    return value if len(value) <= limit else value[: limit - 3] + "..."


def _assignment(name: str, node: ast.AST) -> str:
    """Render a module-level constant.

    Args:
        name: Its name.
        node: The assignment.

    Returns:
        ``NAME: type = value``.
    """
    if isinstance(node, ast.AnnAssign):
        value = f" = {_short(ast.unparse(node.value))}" if node.value is not None else ""
        return f"{name}: {ast.unparse(node.annotation)}{value}"
    if isinstance(node, ast.Assign):
        return f"{name} = {_short(ast.unparse(node.value))}"
    return _short(ast.unparse(node))



def render(src: Path, modules: Iterable[str]) -> Mapping[str, str]:
    """Render the named modules of the package under ``src``.

    Args:
        src: Directory holding the package.
        modules: Dotted public modules.

    Returns:
        Page file -> Markdown.
    """
    return Package(src, modules).render_all()

