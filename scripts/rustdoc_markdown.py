"""Render rustdoc JSON as Markdown pages, one per public module.

``docs-markdown.py`` uses this for ``api/rust/``. The layout follows a
crate's public API rather than its source files: every item reachable from
the crate root is documented on the page of the public module it is
reachable from, re-exports from private modules included, and re-exports
from other crates link to the crate that defines them.

Each page has a single H1 (the module path). Doc comments keep their own
structure, demoted under the item they belong to, so ``# Errors`` in a
function's docs becomes a heading under that function. Signatures are
rebuilt from the JSON types, intra-doc links come from rustdoc's own
``links`` table, and hidden doctest lines (``# use ...``) are dropped.

The module is sans-IO apart from :func:`load_crates`: rendering works on the
decoded JSON, so tests drive it with small hand-written crates.
"""

from __future__ import annotations

import json
import os
import re
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# rustdoc JSON value: a decoded object, list, string, number, bool or null.
Json = Any

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

# Sections of a module page, in order: (inner kind, heading, singular).
SECTIONS = (
    ("module", "Modules", "module"),
    ("trait", "Traits", "trait"),
    ("struct", "Structs", "struct"),
    ("enum", "Enums", "enum"),
    ("union", "Unions", "union"),
    ("function", "Functions", "function"),
    ("type_alias", "Type aliases", "type alias"),
    ("constant", "Constants", "constant"),
    ("static", "Statics", "static"),
    ("macro", "Macros", "macro"),
    ("proc_macro", "Procedural macros", "procedural macro"),
)
# Compiler-internal marker traits left out of "Implements" lists.
HIDDEN_TRAITS = frozenset({"StructuralPartialEq", "StructuralEq"})

# The attribute that declares each rustdoc MacroKind of procedural macro.
PROC_MACRO_ATTRIBUTES = {"bang": "proc_macro", "attr": "proc_macro_attribute", "derive": "proc_macro_derive"}
SECTION_KINDS = frozenset(kind for kind, _, _ in SECTIONS)

FENCE_RE = re.compile(r"^(\s*)(```+|~~~+)(.*)$")
CODE_SPAN_RE = re.compile(r"(`+)(?:(?!\1).)+?\1")
HEADING_RE = re.compile(r"^(#{1,6})(\s+.*)$")
# [label] not followed by a destination, a reference label or a definition colon.
SHORTCUT_RE = re.compile(r"\[(`[^`\[\]]+`|[A-Za-z_][\w:]*(?:\(\))?|[A-Za-z_][\w:]*!)\](?![(\[:])")
# [text](dest), where dest may be a Rust path.
INLINE_RE = re.compile(r"\[((?:[^\[\]]|\[[^\]]*\])+)\]\(([^)\s]+)\)")
# [text][label]
REFERENCE_RE = re.compile(r"\[((?:[^\[\]]|\[[^\]]*\])+)\]\[([^\]]+)\]")
# [label]: dest
DEFINITION_RE = re.compile(r"^\s{0,3}\[([^\]]+)\]:\s*(\S+)\s*$")
# Fence info words rustdoc treats as Rust code.
RUST_FENCE_WORDS = frozenset(
    {"rust", "ignore", "no_run", "should_panic", "compile_fail", "test_harness", "edition2015", "edition2018", "edition2021", "edition2024"}
)


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


class Anchors:
    """Hands out heading anchors for one page, numbering repeats as GitHub does."""

    def __init__(self) -> None:
        """Start with no headings seen."""
        self._seen: dict[str, int] = {}

    def take(self, heading: str) -> str:
        """Record a heading and return its anchor.

        Args:
            heading: Heading text, without the leading ``#`` marks.

        Returns:
            The heading's slug, with ``-1``, ``-2`` ... for repeats.
        """
        base = slug(heading)
        count = self._seen.get(base, 0)
        self._seen[base] = count + 1
        return base if count == 0 else f"{base}-{count}"


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


# ---------------------------------------------------------------------------
# Types and signatures


def _path_name(path: str) -> str:
    """Return the last segment of a path as rustdoc shows it.

    Args:
        path: A path as written, such as ``crate::error::Result``.

    Returns:
        ``Result``.
    """
    return path.rsplit("::", 1)[-1]


def render_args(args: Json) -> str:
    """Render generic arguments (``<T, Item = U>`` or ``(A) -> B``).

    Args:
        args: A rustdoc ``GenericArgs`` value, or None.

    Returns:
        The arguments including their brackets, or ``""``.
    """
    if not args:
        return ""
    if isinstance(args, str):
        return "(..)" if args == "return_type_notation" else ""
    if "angle_bracketed" in args:
        inner = args["angle_bracketed"]
        parts = [render_arg(a) for a in inner.get("args", [])]
        for c in inner.get("constraints", []):
            name = c["name"] + render_args(c.get("args"))
            binding = c.get("binding", {})
            if "equality" in binding:
                term = binding["equality"]
                value = render_type(term["type"]) if "type" in term else term.get("constant", {}).get("expr", "_")
                parts.append(f"{name} = {value}")
            elif "constraint" in binding:
                parts.append(f"{name}: {render_bounds(binding['constraint'])}")
        return f"<{', '.join(parts)}>" if parts else ""
    if "parenthesized" in args:
        inner = args["parenthesized"]
        out = f"({', '.join(render_type(t) for t in inner.get('inputs', []))})"
        if inner.get("output") is not None:
            out += f" -> {render_type(inner['output'])}"
        return out
    return ""


def render_arg(arg: Json) -> str:
    """Render one generic argument.

    Args:
        arg: A rustdoc ``GenericArg``.

    Returns:
        The lifetime, type or const expression.
    """
    if arg == "infer":
        return "_"
    if "lifetime" in arg:
        return arg["lifetime"]
    if "type" in arg:
        return render_type(arg["type"])
    if "const" in arg:
        return arg["const"].get("expr", "_")
    return "_"


def render_path(path: Json) -> str:
    """Render a resolved path with its arguments.

    Args:
        path: A rustdoc ``Path`` (``path``, ``id``, ``args``).

    Returns:
        The last segment and its generic arguments.
    """
    return _path_name(path["path"]) + render_args(path.get("args"))


def render_bound(bound: Json) -> str:
    """Render one generic bound.

    Args:
        bound: A rustdoc ``GenericBound``.

    Returns:
        ``?Sized``, ``for<'a> Fn(&'a str)``, ``'static`` or ``use<..>``.
    """
    if "trait_bound" in bound:
        tb = bound["trait_bound"]
        prefix = {"maybe": "?", "maybe_const": "~const "}.get(tb.get("modifier", "none"), "")
        hrtb = _for_lifetimes(tb.get("generic_params", []))
        return f"{hrtb}{prefix}{render_path(tb['trait'])}"
    if "outlives" in bound:
        return bound["outlives"]
    if "use" in bound:
        names = [p if isinstance(p, str) else p.get("lifetime", p.get("param", "")) for p in bound["use"]]
        return f"use<{', '.join(names)}>"
    return "?"


def render_bounds(bounds: Iterable[Json]) -> str:
    """Render bounds joined by ``+``.

    Args:
        bounds: rustdoc ``GenericBound`` values.

    Returns:
        ``Clone + Send``.
    """
    return " + ".join(render_bound(b) for b in bounds)


def _for_lifetimes(params: list[Json]) -> str:
    """Render a higher-ranked ``for<'a>`` prefix.

    Args:
        params: rustdoc generic parameter definitions.

    Returns:
        ``for<'a, 'b> `` or ``""``.
    """
    names = [p["name"] for p in params if "lifetime" in p.get("kind", {})]
    return f"for<{', '.join(names)}> " if names else ""


def render_type(ty: Json) -> str:
    """Render a rustdoc ``Type`` as Rust source.

    Args:
        ty: The type.

    Returns:
        The type as it would be written, paths shortened to their last
        segment.
    """
    if ty is None:
        return "()"
    (kind, value), = ty.items()
    if kind == "resolved_path":
        return render_path(value)
    if kind == "generic":
        return value
    if kind == "primitive":
        return value
    if kind == "tuple":
        inner = ", ".join(render_type(t) for t in value)
        return f"({inner},)" if len(value) == 1 else f"({inner})"
    if kind == "slice":
        return f"[{render_type(value)}]"
    if kind == "array":
        return f"[{render_type(value['type'])}; {value['len']}]"
    if kind == "pat":
        return render_type(value["type"])
    if kind == "infer":
        return "_"
    if kind == "raw_pointer":
        return f"*{'mut' if value['is_mutable'] else 'const'} {render_type(value['type'])}"
    if kind == "borrowed_ref":
        lifetime = f"{value['lifetime']} " if value.get("lifetime") else ""
        mutability = "mut " if value["is_mutable"] else ""
        return f"&{lifetime}{mutability}{render_type(value['type'])}"
    if kind == "impl_trait":
        return f"impl {render_bounds(value)}"
    if kind == "dyn_trait":
        traits = [_for_lifetimes(t.get("generic_params", [])) + render_path(t["trait"]) for t in value["traits"]]
        if value.get("lifetime"):
            traits.append(value["lifetime"])
        return f"dyn {' + '.join(traits)}"
    if kind == "qualified_path":
        self_type = render_type(value["self_type"])
        name = value["name"] + render_args(value.get("args"))
        trait = value.get("trait")
        if trait is None:
            return f"{self_type}::{name}"
        if self_type == "Self" or value["self_type"] == {"generic": "Self"}:
            return f"Self::{name}"
        return f"<{self_type} as {render_path(trait)}>::{name}"
    if kind == "function_pointer":
        header = _header(value.get("header", {}))
        sig = value["sig"]
        inputs = ", ".join(render_type(t) for _, t in sig["inputs"])
        out = f" -> {render_type(sig['output'])}" if sig.get("output") is not None else ""
        return f"{_for_lifetimes(value.get('generic_params', []))}{header}fn({inputs}){out}"
    return "_"


def _header(header: Json) -> str:
    """Render a function header's qualifiers.

    Args:
        header: rustdoc ``FunctionHeader``.

    Returns:
        ``const async unsafe extern "C" `` style prefix (possibly empty).
    """
    out = ""
    if header.get("is_const"):
        out += "const "
    if header.get("is_async"):
        out += "async "
    if header.get("is_unsafe"):
        out += "unsafe "
    abi = header.get("abi", "Rust")
    if abi != "Rust":
        name = next(iter(abi)) if isinstance(abi, dict) else abi
        out += f'extern "{name}" '
    return out


def render_generics(generics: Json) -> tuple[str, str]:
    """Render generic parameters and the where clause.

    Synthetic parameters (from ``impl Trait`` arguments) are left out; the
    argument itself shows ``impl Trait``.

    Args:
        generics: rustdoc ``Generics``.

    Returns:
        (``<T: Clone>`` or ``""``, ``where ...`` clause or ``""``).
    """
    params = []
    for p in generics.get("params", []):
        kind = p.get("kind", {})
        if "lifetime" in kind:
            outlives = kind["lifetime"].get("outlives", [])
            params.append(p["name"] + (f": {' + '.join(outlives)}" if outlives else ""))
        elif "type" in kind:
            t = kind["type"]
            if t.get("is_synthetic"):
                continue
            text = p["name"]
            if t.get("bounds"):
                text += f": {render_bounds(t['bounds'])}"
            if t.get("default") is not None:
                text += f" = {render_type(t['default'])}"
            params.append(text)
        elif "const" in kind:
            c = kind["const"]
            text = f"const {p['name']}: {render_type(c['type'])}"
            if c.get("default"):
                text += f" = {c['default']}"
            params.append(text)
    preds = []
    for w in generics.get("where_predicates", []):
        if "bound_predicate" in w:
            bp = w["bound_predicate"]
            preds.append(f"{_for_lifetimes(bp.get('generic_params', []))}{render_type(bp['type'])}: {render_bounds(bp['bounds'])}")
        elif "lifetime_predicate" in w:
            lp = w["lifetime_predicate"]
            preds.append(f"{lp['lifetime']}: {' + '.join(lp['outlives'])}")
        elif "eq_predicate" in w:
            ep = w["eq_predicate"]
            rhs = ep["rhs"]
            value = render_type(rhs["type"]) if "type" in rhs else rhs.get("constant", {}).get("expr", "_")
            preds.append(f"{render_type(ep['lhs'])} = {value}")
    head = f"<{', '.join(params)}>" if params else ""
    where = f"\nwhere\n    {(',' + chr(10) + '    ').join(preds)}," if preds else ""
    return head, where


def _self_param(ty: Json) -> str | None:
    """Return ``self``, ``&self`` or ``&mut self`` for a receiver type.

    Args:
        ty: The first input's type.

    Returns:
        The short receiver form, or None when it is written out (``self: Box<Self>``).
    """
    if ty == {"generic": "Self"}:
        return "self"
    if "borrowed_ref" in ty and ty["borrowed_ref"]["type"] == {"generic": "Self"}:
        ref = ty["borrowed_ref"]
        lifetime = f"{ref['lifetime']} " if ref.get("lifetime") else ""
        return f"&{lifetime}{'mut ' if ref['is_mutable'] else ''}self"
    return None


def render_fn(name: str, function: Json, public: bool) -> str:
    """Render a function signature.

    Args:
        name: The function's name.
        function: rustdoc ``Function``.
        public: Whether to write ``pub`` (not for trait items).

    Returns:
        The signature, without a body.
    """
    sig = function["sig"]
    inputs = []
    for i, (arg, ty) in enumerate(sig["inputs"]):
        receiver = _self_param(ty) if i == 0 and arg == "self" else None
        inputs.append(receiver or f"{arg}: {render_type(ty)}")
    if sig.get("is_c_variadic"):
        inputs.append("...")
    head, where = render_generics(function.get("generics", {}))
    output = f" -> {render_type(sig['output'])}" if sig.get("output") is not None else ""
    prefix = ("pub " if public else "") + _header(function.get("header", {}))
    one_line = f"{prefix}fn {name}{head}({', '.join(inputs)}){output}"
    if len(one_line) > 90 and inputs:
        args = "".join(f"\n    {a}," for a in inputs)
        one_line = f"{prefix}fn {name}{head}({args}\n){output}"
    return one_line + where


# ---------------------------------------------------------------------------
# Doc comments


def _rust_fence(info: str) -> bool:
    """Tell whether a fence's info string marks Rust code, as rustdoc reads it.

    Args:
        info: Text after the opening fence.

    Returns:
        True for an empty info string or one made of rustdoc's Rust words.
    """
    words = [w for w in re.split(r"[,\s]+", info.strip()) if w]
    return not words or all(w in RUST_FENCE_WORDS for w in words)


def code_spans(line: str) -> list[tuple[int, int]]:
    """Return the ``(start, end)`` offsets of inline code spans in a line.

    Args:
        line: One Markdown line.

    Returns:
        Offsets, end exclusive.
    """
    return [m.span() for m in CODE_SPAN_RE.finditer(line)]


@dataclass
class DocContext:
    """What rewriting one doc comment needs.

    Attributes:
        links: rustdoc's ``links`` for the item: link text -> item id.
        resolve: Maps an item id to a link, relative to the current page,
            or None when it has no page.
        demote: How many levels to push the comment's headings down.
    """

    links: Mapping[str, int]
    resolve: Any
    demote: int


def rewrite_docs(docs: str, ctx: DocContext) -> str:
    """Prepare a doc comment for a page.

    Headings move down ``ctx.demote`` levels (at most to H6), intra-doc links
    become Markdown links (or plain text when the target has no page),
    Rust code blocks get a ``rust`` fence and lose hidden ``# `` lines.

    Args:
        docs: The comment's Markdown.
        ctx: Links and demotion for this item.

    Returns:
        The rewritten Markdown.
    """
    out: list[str] = []
    fence: str | None = None
    rust_block = False
    for line in docs.splitlines():
        m = FENCE_RE.match(line)
        if fence is None and m:
            fence = m.group(2)
            rust_block = fence.startswith("`") and _rust_fence(m.group(3))
            out.append(f"{m.group(1)}{fence}{'rust' if rust_block else m.group(3)}")
            continue
        if fence is not None:
            if m and m.group(2).startswith(fence) and not m.group(3).strip():
                fence = None
                out.append(line)
                continue
            if rust_block:
                stripped = line.strip()
                if stripped == "#" or stripped.startswith("# "):
                    continue
                if stripped.startswith("##"):
                    line = line.replace("##", "#", 1)
            out.append(line)
            continue
        h = HEADING_RE.match(line)
        if h:
            level = min(6, len(h.group(1)) + ctx.demote)
            line = "#" * level + h.group(2)
        out.append(_rewrite_links(line, ctx))
    return "\n".join(out).strip()


def _rewrite_links(line: str, ctx: DocContext) -> str:
    """Rewrite the intra-doc links on one line outside code.

    Args:
        line: One line of a doc comment.
        ctx: Links for this item.

    Returns:
        The line with links resolved.
    """
    def target(label: str) -> tuple[bool, str | None]:
        if label not in ctx.links:
            return False, None
        return True, ctx.resolve(ctx.links[label])

    d = DEFINITION_RE.match(line)
    if d:
        known, href = target(d.group(2))
        if known:
            return f"[{d.group(1)}]: {href}" if href else ""
        return line

    def inline(m: re.Match[str]) -> str:
        if any(s <= m.start() < e for s, e in spans):
            return m.group(0)
        known, href = target(m.group(2))
        if not known:
            return m.group(0)
        return f"[{m.group(1)}]({href})" if href else m.group(1)

    def reference(m: re.Match[str]) -> str:
        if any(s <= m.start() < e for s, e in spans):
            return m.group(0)
        known, href = target(m.group(2))
        if not known:
            return m.group(0)
        return f"[{m.group(1)}]({href})" if href else m.group(1)

    def shortcut(m: re.Match[str]) -> str:
        if any(s <= m.start() < e for s, e in spans):
            return m.group(0)
        known, href = target(m.group(1))
        if not known:
            return m.group(0)
        return f"[{m.group(1)}]({href})" if href else m.group(1)

    spans = code_spans(line)
    line = INLINE_RE.sub(inline, line)
    spans = code_spans(line)
    line = REFERENCE_RE.sub(reference, line)
    spans = code_spans(line)
    return SHORTCUT_RE.sub(shortcut, line)


def summary(docs: str | None) -> str:
    """Return the first paragraph of a doc comment on one line, links unwrapped.

    Args:
        docs: The comment, or None.

    Returns:
        The summary, or ``""``.
    """
    if not docs:
        return ""
    para = []
    for line in docs.strip().splitlines():
        if not line.strip() or line.lstrip().startswith(("#", "```", "~~~")):
            break
        para.append(line.strip())
    text = " ".join(para)
    text = INLINE_RE.sub(lambda m: m.group(1), text)
    text = REFERENCE_RE.sub(lambda m: m.group(1), text)
    text = re.sub(r"\[(`[^`]+`)\]", r"\1", text)
    return text.replace("|", "\\|")


# ---------------------------------------------------------------------------
# Crates and placement


@dataclass
class Crate:
    """One crate's rustdoc JSON.

    Attributes:
        name: The crate's rustdoc name (``bookclerk_media``).
        data: The decoded JSON.
    """

    name: str
    data: Json

    def item(self, item_id: int | str) -> Json | None:
        """Return an item of this crate's index.

        Args:
            item_id: The item's id.

        Returns:
            The item, or None when the index does not have it.
        """
        return self.data["index"].get(str(item_id))

    def path_of(self, item_id: int | str) -> list[str] | None:
        """Return the canonical path rustdoc records for an id.

        Args:
            item_id: An id used in this crate's JSON.

        Returns:
            The path, starting with the defining crate, or None.
        """
        summary_ = self.data.get("paths", {}).get(str(item_id))
        return summary_["path"] if summary_ else None


def kind_of(item: Json) -> str:
    """Return an item's inner kind (``struct``, ``function``, ...).

    Args:
        item: A rustdoc item.

    Returns:
        The kind.
    """
    return next(iter(item["inner"]))


def _variant_field_ids(variant: Json) -> list[int]:
    """Return the field ids of a tuple or struct enum variant.

    Args:
        variant: A rustdoc variant item.

    Returns:
        The ids in declaration order; stripped (hidden) tuple fields are
        left out, and a unit variant has none.
    """
    k = variant["inner"]["variant"].get("kind", "plain")
    if isinstance(k, dict) and "struct" in k:
        return list(k["struct"]["fields"])
    if isinstance(k, dict) and "tuple" in k:
        return [fid for fid in k["tuple"] if fid is not None]
    return []


@dataclass
class Entry:
    """An item documented on a module page.

    Attributes:
        name: The name it is reachable under (a re-export may rename it).
        item: The rustdoc item.
    """

    name: str
    item: Json


@dataclass
class ReExport:
    """A re-export of an item this crate does not document itself.

    Attributes:
        name: The name it is reachable under.
        target_id: The id the ``use`` points at (this crate's JSON ids).
        source: The ``use`` source as written.
    """

    name: str
    target_id: int | None
    source: str


@dataclass
class Page:
    """One public module and what it documents.

    Attributes:
        crate: The crate's name.
        path: Module path from the crate root (empty for the root).
        module: The module item.
        entries: Items documented here, by kind.
        reexports: Items from other crates reachable here.
    """

    crate: str
    path: tuple[str, ...]
    module: Json
    entries: dict[str, list[Entry]] = field(default_factory=dict)
    reexports: list[ReExport] = field(default_factory=list)

    @property
    def file(self) -> str:
        """The page's path under ``api/rust``.

        Returns:
            ``<crate>/index.md`` or ``<crate>/<a>/<b>.md``.
        """
        if not self.path:
            return f"{self.crate}/index.md"
        return f"{self.crate}/{'/'.join(self.path)}.md"

    @property
    def title(self) -> str:
        """The page's H1 text.

        Returns:
            ``crate`` or ``crate::a::b``.
        """
        return "::".join((self.crate, *self.path))


class _PageWalk:
    """State of one breadth-first walk over a crate's public modules.

    Args:
        crate: The crate being walked.
    """

    def __init__(self, crate: Crate) -> None:
        """Queue the crate root."""
        self.crate = crate
        root = crate.item(crate.data["root"])
        self.queue: list[tuple[tuple[str, ...], Json]] = [((), root)]
        self.seen_modules: set[int] = {root["id"]}
        self.placed: set[int] = set()

    def page(self, path: tuple[str, ...], module: Json) -> Page:
        """Fill the page of one module, queueing its public submodules.

        Args:
            path: Module path from the crate root.
            module: The module item.

        Returns:
            The page.
        """
        page = Page(self.crate.name, path, module)
        self._visit(page, set(), module["inner"]["module"]["items"], 0)
        return page

    def _add(self, page: Page, names: set[tuple[str, str]], name: str, item: Json) -> None:
        """Place one item on a page unless it is already documented.

        Args:
            page: The page.
            names: (kind, name) pairs already on the page.
            name: The name the item is reachable under.
            item: The item.
        """
        kind = kind_of(item)
        if kind == "module":
            if item["id"] not in self.seen_modules:
                self.seen_modules.add(item["id"])
                self.queue.append(((*page.path, name), item))
                page.entries.setdefault("module", []).append(Entry(name, item))
            return
        if kind not in SECTION_KINDS or item["id"] in self.placed or (kind, name) in names:
            return
        self.placed.add(item["id"])
        names.add((kind, name))
        page.entries.setdefault(kind, []).append(Entry(name, item))

    def _visit(self, page: Page, names: set[tuple[str, str]], ids: Iterable[int], depth: int) -> None:
        """Place a module's public items, following ``use`` and glob re-exports.

        Args:
            page: The page.
            names: (kind, name) pairs already on the page.
            ids: Item ids of the module.
            depth: Glob nesting, bounded so a glob cycle ends.
        """
        for item_id in ids:
            item = self.crate.item(item_id)
            if item is None or item.get("visibility") != "public":
                continue
            if kind_of(item) != "use":
                self._add(page, names, item["name"], item)
                continue
            use = item["inner"]["use"]
            target = self.crate.item(use["id"]) if use.get("id") is not None else None
            if use.get("is_glob"):
                if target is not None and kind_of(target) == "module" and depth < 8:
                    self._visit(page, names, target["inner"]["module"]["items"], depth + 1)
                continue
            if target is not None and kind_of(target) != "use":
                self._add(page, names, use["name"], target)
            else:
                page.reexports.append(ReExport(use["name"], use.get("id"), use["source"]))


def collect_pages(crate: Crate) -> list[Page]:
    """Walk a crate's public modules and place every reachable item.

    An item is documented once, on the first page it is reachable from
    (breadth-first, so the shortest path wins). Glob re-exports of local
    modules are expanded.

    Args:
        crate: The crate.

    Returns:
        Pages in breadth-first order, the root first.
    """
    walk = _PageWalk(crate)
    pages = []
    while walk.queue:
        path, module = walk.queue.pop(0)
        pages.append(walk.page(path, module))
    return pages


@dataclass
class Placement:
    """Where every documented item landed across all crates.

    Attributes:
        by_id: (crate, item id) -> (page file, anchor or ``""``).
        by_path: canonical path -> (page file, anchor or ``""``).
    """

    by_id: dict[tuple[str, int], tuple[str, str]] = field(default_factory=dict)
    by_path: dict[tuple[str, ...], tuple[str, str]] = field(default_factory=dict)


# ---------------------------------------------------------------------------
# Rendering


class Renderer:
    """Renders every crate's pages, linking across them.

    Args:
        crates: Every crate converted into this tree, by name.
    """

    def __init__(self, crates: Mapping[str, Crate]) -> None:
        """Collect pages for every crate and plan their anchors."""
        self.crates = dict(crates)
        self.pages = {name: collect_pages(crate) for name, crate in self.crates.items()}
        self.placement = Placement()
        for name, pages in self.pages.items():
            for page in pages:
                self._plan(self.crates[name], page)

    # -- planning ----------------------------------------------------------

    def _record(self, crate: Crate, item_id: int, where: tuple[str, str]) -> None:
        """Remember where an item is documented.

        Args:
            crate: The crate whose JSON the id belongs to.
            item_id: The item's id.
            where: (page file, anchor).
        """
        self.placement.by_id.setdefault((crate.name, item_id), where)
        path = crate.path_of(item_id)
        if path:
            self.placement.by_path.setdefault(tuple(path), where)

    def _plan(self, crate: Crate, page: Page) -> None:
        """Assign anchors on one page in the order :meth:`render_page` writes them.

        Args:
            crate: The page's crate.
            page: The page.
        """
        anchors = Anchors()
        anchors.take(page.title)
        for heading in _doc_headings(page.module.get("docs")):
            anchors.take(heading)
        self._record(crate, page.module["id"], (page.file, ""))
        if page.reexports:
            anchors.take("Re-exports")
        for kind, title, _ in SECTIONS:
            entries = page.entries.get(kind, [])
            if not entries:
                continue
            anchors.take(title)
            if kind == "module":
                continue
            for entry in entries:
                anchor = anchors.take(entry.name)
                self._record(crate, entry.item["id"], (page.file, anchor))
                for heading in _doc_headings(entry.item.get("docs")):
                    anchors.take(heading)
                # Fields and variants have no heading of their own: a link
                # to one lands on the item that lists it.
                for member_id in self._field_ids(crate, entry.item):
                    self._record(crate, member_id, (page.file, anchor))
                # Field and variant docs come next on the page (_fields).
                for documented in self._documented_fields(crate, entry)[1]:
                    for heading in _doc_headings(documented.get("docs")):
                        anchors.take(heading)
                    for nested in self._variant_fields(crate, documented):
                        for heading in _doc_headings(nested["docs"]):
                            anchors.take(heading)
                self._plan_members(crate, page, entry, anchors)

    def _plan_members(self, crate: Crate, page: Page, entry: Entry, anchors: Anchors) -> None:
        """Assign anchors to an item's fields, variants and methods.

        Args:
            crate: The page's crate.
            page: The page.
            entry: The item.
            anchors: The page's anchors so far.
        """
        for member_id, label in self._members(crate, entry):
            member = crate.item(member_id)
            if member is None:
                continue
            anchor = anchors.take(label)
            self._record(crate, member_id, (page.file, anchor))
            for heading in _doc_headings(member.get("docs")):
                anchors.take(heading)

    def _members(self, crate: Crate, entry: Entry) -> list[tuple[int, str]]:
        """List the members of an item that get their own heading.

        Args:
            crate: The item's crate.
            entry: The item.

        Returns:
            (member id, heading text) for methods, associated items and
            trait items, in page order.
        """
        item = entry.item
        kind = kind_of(item)
        out: list[tuple[int, str]] = []
        if kind == "trait":
            for member_id in item["inner"]["trait"]["items"]:
                member = crate.item(member_id)
                if member is not None:
                    out.append((member_id, f"{entry.name}::{member['name']}"))
            return out
        if kind in ("struct", "enum", "union"):
            for impl_id in item["inner"][kind].get("impls", []):
                impl = crate.item(impl_id)
                if impl is None or impl["inner"]["impl"].get("trait") is not None:
                    continue
                for member_id in impl["inner"]["impl"]["items"]:
                    member = crate.item(member_id)
                    if member is not None and member.get("visibility") == "public":
                        out.append((member_id, f"{entry.name}::{member['name']}"))
        return out

    # -- links -------------------------------------------------------------

    def resolver(self, crate: Crate, page_file: str) -> Any:
        """Return a function that links an id of ``crate`` from ``page_file``.

        Args:
            crate: The crate whose ids will be resolved.
            page_file: The page the links appear on.

        Returns:
            ``resolve(item_id) -> str | None``.
        """
        def resolve(item_id: int) -> str | None:
            where = self.placement.by_id.get((crate.name, item_id))
            if where is None:
                path = crate.path_of(item_id)
                if path:
                    where = self.placement.by_path.get(tuple(path))
                    if where is None:
                        return self._external(crate, item_id, path)
            if where is None:
                return None
            file, anchor = where
            rel = "" if file == page_file else os.path.relpath(file, os.path.dirname(page_file) or ".").replace(os.sep, "/")
            if anchor:
                return f"{rel}#{anchor}"
            return rel or "#"

        return resolve

    def _external(self, crate: Crate, item_id: int, path: list[str]) -> str | None:
        """Link an item outside the converted crates.

        Args:
            crate: The crate whose JSON names the id.
            item_id: The id.
            path: Its canonical path.

        Returns:
            A docs.rs or doc.rust-lang.org URL, or None for workspace items
            with no page (private items, say).
        """
        if path[0] in self.crates:
            return None
        summary_ = crate.data["paths"][str(item_id)]
        names = {str(k): v.get("name", "") for k, v in crate.data.get("external_crates", {}).items()}
        name = names.get(str(summary_.get("crate_id")), path[0])
        return external_url(path, summary_.get("kind", ""), name)

    # -- pages -------------------------------------------------------------

    def render_all(self) -> dict[str, str]:
        """Render every page.

        Returns:
            Page file under ``api/rust`` -> Markdown.
        """
        out = {}
        for name, pages in self.pages.items():
            for page in pages:
                out[page.file] = self.render_page(self.crates[name], page)
        return out

    def render_page(self, crate: Crate, page: Page) -> str:
        """Render one module page.

        Args:
            crate: The page's crate.
            page: The page.

        Returns:
            The page's Markdown.
        """
        resolve = self.resolver(crate, page.file)
        lines = [f"# {page.title}", ""]
        docs = page.module.get("docs")
        if docs:
            lines += [rewrite_docs(docs, DocContext(page.module.get("links", {}), resolve, 1)), ""]
        lines += self._contents(page)
        if page.reexports:
            lines += ["## Re-exports", ""]
            for r in page.reexports:
                href = resolve(r.target_id) if r.target_id is not None else None
                label = f"`{r.source}`" + (f" as `{r.name}`" if _path_name(r.source) != r.name else "")
                lines.append(f"- [{label}]({href})" if href else f"- {label}")
            lines.append("")
        for kind, title, _ in SECTIONS:
            entries = page.entries.get(kind, [])
            if not entries:
                continue
            lines += [f"## {title}", ""]
            if kind == "module":
                for entry in entries:
                    file = f"{page.crate}/{'/'.join((*page.path, entry.name))}.md"
                    href = os.path.relpath(file, os.path.dirname(page.file)).replace(os.sep, "/")
                    text = summary(entry.item.get("docs"))
                    lines.append(f"- [`{entry.name}`]({href})" + (f": {text}" if text else ""))
                lines.append("")
                continue
            for entry in entries:
                lines += self._item(crate, entry, resolve)
        return "\n".join(lines).rstrip() + "\n"

    def _contents(self, page: Page) -> list[str]:
        """Render the page's table of contents (items other than modules).

        Args:
            page: The page.

        Returns:
            Markdown lines, or none for a page with nothing but modules.
        """
        rows = []
        for kind, _, singular in SECTIONS:
            if kind == "module":
                continue
            for entry in page.entries.get(kind, []):
                where = self.placement.by_id.get((page.crate, entry.item["id"]))
                anchor = where[1] if where else slug(entry.name)
                rows.append(f"| [`{entry.name}`](#{anchor}) | {singular} | {summary(entry.item.get('docs'))} |")
        if len(rows) < 2:
            return []
        return ["| Item | Kind | Summary |", "| --- | --- | --- |", *rows, ""]

    def _item(self, crate: Crate, entry: Entry, resolve: Any) -> list[str]:
        """Render one item: heading, signature, docs and members.

        Args:
            crate: The item's crate.
            entry: The item.
            resolve: Link resolver for the page.

        Returns:
            Markdown lines.
        """
        item = entry.item
        lines = [f"### {entry.name}", "", "```rust", self.declaration(crate, entry), "```", ""]
        lines += _deprecation(item)
        docs = item.get("docs")
        if docs:
            lines += [rewrite_docs(docs, DocContext(item.get("links", {}), resolve, 3)), ""]
        lines += self._fields(crate, entry, resolve)
        lines += self._methods(crate, entry, resolve)
        lines += self._trait_impls(crate, entry)
        return lines

    def declaration(self, crate: Crate, entry: Entry) -> str:
        """Render an item's declaration.

        Args:
            crate: The item's crate.
            entry: The item.

        Returns:
            Rust source for the item without bodies or private fields.
        """
        item = entry.item
        kind = kind_of(item)
        inner = item["inner"][kind]
        name = entry.name
        if kind == "function":
            return render_fn(name, inner, True)
        if kind in ("struct", "union"):
            head, where = render_generics(inner.get("generics", {}))
            # A union has its fields directly; only a struct has a kind.
            k = {"plain": {"fields": inner.get("fields", [])}} if kind == "union" else inner.get("kind", "unit")
            if k == "unit":
                return f"pub {kind} {name}{head}{where};"
            if "tuple" in k:
                fields = []
                for fid in k["tuple"]:
                    f = crate.item(fid) if fid is not None else None
                    fields.append(f"pub {render_type(f['inner']['struct_field'])}" if f and f.get("visibility") == "public" else "/* private */")
                return f"pub struct {name}{head}({', '.join(fields)}){where};"
            fields = k["plain"]["fields"]
            stripped = (k.get("plain") or {}).get("has_stripped_fields", inner.get("has_stripped_fields", False))
            body = []
            for fid in fields:
                f = crate.item(fid)
                if f is not None and f.get("visibility") == "public":
                    body.append(f"    pub {f['name']}: {render_type(f['inner']['struct_field'])},")
            if stripped:
                body.append("    /* private fields */")
            if not body:
                return f"pub {kind} {name}{head}{where} {{}}"
            return f"pub {kind} {name}{head}{where} {{\n" + "\n".join(body) + "\n}"
        if kind == "enum":
            head, where = render_generics(inner.get("generics", {}))
            body = []
            for vid in inner["variants"]:
                v = crate.item(vid)
                if v is not None:
                    body.append(f"    {self._variant(crate, v)},")
            if inner.get("has_stripped_variants"):
                body.append("    /* private variants */")
            return f"pub enum {name}{head}{where} {{\n" + "\n".join(body) + "\n}" if body else f"pub enum {name}{head}{where} {{}}"
        if kind == "trait":
            head, where = render_generics(inner.get("generics", {}))
            bounds = f": {render_bounds(inner['bounds'])}" if inner.get("bounds") else ""
            unsafe = "unsafe " if inner.get("is_unsafe") else ""
            auto = "auto " if inner.get("is_auto") else ""
            body = []
            for mid in inner["items"]:
                m = crate.item(mid)
                if m is not None:
                    body.append("    " + self._assoc(m, trait_item=True).replace("\n", "\n    ") + ";")
            decl = f"pub {unsafe}{auto}trait {name}{head}{bounds}{where}"
            return f"{decl} {{\n" + "\n".join(body) + "\n}" if body else f"{decl} {{}}"
        if kind == "type_alias":
            head, where = render_generics(inner.get("generics", {}))
            return f"pub type {name}{head}{where} = {render_type(inner['type'])};"
        if kind == "constant":
            return f"pub const {name}: {render_type(inner['type'])} = {inner['const'].get('expr', '_')};"
        if kind == "static":
            mutability = "mut " if inner.get("is_mutable") else ""
            return f"pub static {mutability}{name}: {render_type(inner['type'])};"
        if kind == "macro":
            return inner if isinstance(inner, str) else f"macro_rules! {name} {{ ... }}"
        if kind == "proc_macro":
            attribute = PROC_MACRO_ATTRIBUTES.get(inner.get("kind", "derive"), "proc_macro_derive")
            return f"#[{attribute}] {name}"
        return name

    def _variant(self, crate: Crate, variant: Json) -> str:
        """Render one enum variant.

        Args:
            crate: The enum's crate.
            variant: The variant item.

        Returns:
            ``Name``, ``Name(T)`` or ``Name { a: T }``, with a discriminant.
        """
        inner = variant["inner"]["variant"]
        k = inner.get("kind", "plain")
        text = variant["name"]
        if isinstance(k, dict) and "tuple" in k:
            parts = []
            for fid in k["tuple"]:
                f = crate.item(fid) if fid is not None else None
                parts.append(render_type(f["inner"]["struct_field"]) if f else "_")
            text += f"({', '.join(parts)})"
        elif isinstance(k, dict) and "struct" in k:
            parts = []
            for fid in k["struct"]["fields"]:
                f = crate.item(fid)
                if f is not None:
                    parts.append(f"{f['name']}: {render_type(f['inner']['struct_field'])}")
            if k["struct"].get("has_stripped_fields"):
                parts.append("..")
            text += " { " + ", ".join(parts) + " }"
        if inner.get("discriminant"):
            text += f" = {inner['discriminant'].get('expr', '')}"
        return text

    def _assoc(self, member: Json, trait_item: bool) -> str:
        """Render an associated item's declaration.

        Args:
            member: A function, associated constant or associated type.
            trait_item: Whether it belongs to a trait (no ``pub``).

        Returns:
            The declaration, without a body or trailing ``;``.
        """
        kind = kind_of(member)
        inner = member["inner"][kind]
        if kind == "function":
            return render_fn(member["name"], inner, not trait_item)
        if kind == "assoc_const":
            value = f" = {inner['value']}" if inner.get("value") and not trait_item else ""
            return f"{'' if trait_item else 'pub '}const {member['name']}: {render_type(inner['type'])}{value}"
        if kind == "assoc_type":
            head, where = render_generics(inner.get("generics", {}))
            bounds = f": {render_bounds(inner['bounds'])}" if inner.get("bounds") else ""
            value = f" = {render_type(inner['type'])}" if inner.get("type") is not None else ""
            return f"type {member['name']}{head}{bounds}{value}{where}"
        return member.get("name") or ""

    def _fields(self, crate: Crate, entry: Entry, resolve: Any) -> list[str]:
        """Render documented public fields and variants as a list.

        Args:
            crate: The item's crate.
            entry: The item.
            resolve: Link resolver for the page.

        Returns:
            Markdown lines (none when nothing is documented).
        """
        title, fields = self._documented_fields(crate, entry)
        rows = []
        for f in fields:
            text = rewrite_docs(f.get("docs") or "", DocContext(f.get("links", {}), resolve, 5)).replace("\n", "\n  ")
            rows.append(f"- `{f['name']}`: {text}" if text else f"- `{f['name']}`")
            for nested in self._variant_fields(crate, f):
                inner = rewrite_docs(nested["docs"], DocContext(nested.get("links", {}), resolve, 5)).replace("\n", "\n    ")
                rows.append(f"  - `{nested['name']}`: {inner}")
        return [f"**{title}**", "", *rows, ""] if rows else []

    def _field_ids(self, crate: Crate, item: Json) -> list[int]:
        """List the ids of an item's fields and variants, documented or not.

        Args:
            crate: The item's crate.
            item: A struct, union or enum.

        Returns:
            Field ids of a struct or union; variant ids of an enum followed by
            the fields of its tuple and struct variants. Empty for other kinds.
        """
        kind = kind_of(item)
        if kind == "struct":
            k = item["inner"]["struct"].get("kind", "unit")
            if isinstance(k, dict) and "plain" in k:
                return list(k["plain"]["fields"])
            if isinstance(k, dict) and "tuple" in k:
                return [fid for fid in k["tuple"] if fid is not None]
            return []
        if kind == "union":
            return list(item["inner"]["union"].get("fields", []))
        if kind != "enum":
            return []
        out: list[int] = []
        for vid in item["inner"]["enum"]["variants"]:
            out.append(vid)
            variant = crate.item(vid)
            if variant is not None:
                out += _variant_field_ids(variant)
        return out

    def _variant_fields(self, crate: Crate, variant: Json) -> list[Json]:
        """List the documented fields of a tuple or struct enum variant.

        Args:
            crate: The variant's crate.
            variant: A variant item (any other item yields nothing).

        Returns:
            The fields that carry docs, in declaration order.
        """
        if kind_of(variant) != "variant":
            return []
        out = []
        for fid in _variant_field_ids(variant):
            f = crate.item(fid)
            if f is not None and f.get("docs"):
                out.append(f)
        return out

    def _documented_fields(self, crate: Crate, entry: Entry) -> tuple[str, list[Json]]:
        """List the fields or variants :meth:`_fields` renders.

        Args:
            crate: The item's crate.
            entry: The item.

        Returns:
            ``Fields`` or ``Variants``, and the documented public fields of a
            struct or union, or the variants of an enum that carry docs or
            have documented fields, in order.
        """
        item = entry.item
        kind = kind_of(item)
        ids: list[int] = []
        title = "Fields"
        if kind == "struct":
            k = item["inner"]["struct"].get("kind", "unit")
            if isinstance(k, dict) and "plain" in k:
                ids = k["plain"]["fields"]
        elif kind == "union":
            ids = item["inner"]["union"].get("fields", [])
        elif kind == "enum":
            ids = item["inner"]["enum"]["variants"]
            title = "Variants"
        out = []
        for fid in ids:
            f = crate.item(fid)
            if f is None or (kind != "enum" and f.get("visibility") != "public"):
                continue
            # A variant without docs of its own still lists its documented fields.
            if f.get("docs") or self._variant_fields(crate, f):
                out.append(f)
        return title, out

    def _methods(self, crate: Crate, entry: Entry, resolve: Any) -> list[str]:
        """Render inherent methods, or a trait's items.

        Args:
            crate: The item's crate.
            entry: The item.
            resolve: Link resolver for the page.

        Returns:
            Markdown lines.
        """
        lines: list[str] = []
        trait = kind_of(entry.item) == "trait"
        for member_id, label in self._members(crate, entry):
            member = crate.item(member_id)
            if member is None:
                continue
            lines += [f"#### {label}", "", "```rust", self._assoc(member, trait), "```", ""]
            lines += _deprecation(member)
            if member.get("docs"):
                lines += [rewrite_docs(member["docs"], DocContext(member.get("links", {}), resolve, 4)), ""]
        return lines

    def _trait_impls(self, crate: Crate, entry: Entry) -> list[str]:
        """List the traits a type implements, or a trait's implementors.

        Auto traits and blanket implementations are left out.

        Args:
            crate: The item's crate.
            entry: The item.

        Returns:
            One Markdown line, or none.
        """
        item = entry.item
        kind = kind_of(item)
        names: list[str] = []
        if kind in ("struct", "enum", "union"):
            for impl_id in item["inner"][kind].get("impls", []):
                impl = crate.item(impl_id)
                if impl is None:
                    continue
                inner = impl["inner"]["impl"]
                if inner.get("trait") is None or inner.get("is_synthetic") or inner.get("blanket_impl") is not None:
                    continue
                if _path_name(inner["trait"]["path"]) in HIDDEN_TRAITS:
                    continue
                neg = "!" if inner.get("is_negative") else ""
                names.append(f"`{neg}{render_path(inner['trait'])}`")
            label = "Implements"
        elif kind == "trait":
            for impl_id in item["inner"]["trait"].get("implementations", []):
                impl = crate.item(impl_id)
                if impl is None or impl["inner"]["impl"].get("blanket_impl") is not None:
                    continue
                names.append(f"`{render_type(impl['inner']['impl']['for'])}`")
            label = "Implemented by"
        else:
            return []
        names = sorted(set(names), key=str.lower)
        return [f"{label}: {', '.join(names)}", ""] if names else []


def _deprecation(item: Json) -> list[str]:
    """Render an item's deprecation note.

    Args:
        item: A rustdoc item.

    Returns:
        Markdown lines, or none.
    """
    dep = item.get("deprecation")
    if not dep:
        return []
    note = dep.get("note") or ""
    since = f" since {dep['since']}" if dep.get("since") else ""
    return [f"> **Deprecated{since}.** {note}".rstrip(), ""]


def _doc_headings(docs: str | None) -> list[str]:
    """List the headings a doc comment contributes to a page, in order.

    Args:
        docs: The comment, or None.

    Returns:
        Heading texts outside code blocks.
    """
    out = []
    fence: str | None = None
    for line in (docs or "").splitlines():
        m = FENCE_RE.match(line)
        if fence is None and m:
            fence = m.group(2)
            continue
        if fence is not None:
            if m and m.group(2).startswith(fence) and not m.group(3).strip():
                fence = None
            continue
        h = HEADING_RE.match(line)
        if h:
            out.append(h.group(2).strip().rstrip("#").strip())
    return out


def load_crates(json_dir: Path, names: Iterable[str], expected_format: int) -> dict[str, Crate]:
    """Read the rustdoc JSON of the named crates.

    Args:
        json_dir: Directory of ``<crate>.json`` files.
        names: Crate names to read; missing files are skipped.
        expected_format: The rustdoc JSON ``format_version`` this module reads.

    Returns:
        Crate name -> crate.

    Raises:
        ValueError: A file has another format version.
    """
    crates = {}
    for name in sorted(names):
        path = json_dir / f"{name}.json"
        if not path.is_file():
            continue
        with path.open(encoding="utf-8") as fh:
            data = json.load(fh)
        if data.get("format_version") != expected_format:
            raise ValueError(f"{path.name}: rustdoc JSON format {data.get('format_version')}, expected {expected_format}")
        crates[name] = Crate(name, data)
    return crates
