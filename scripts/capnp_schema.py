"""Parser for the hand-written Bookclerk plugin ABI schema (`plugin.capnp`).

The schema is the single source of truth for the plugin ABI. This module
reads the *text* (declarations, ordinals, annotations, and every ``#`` doc
comment) so the SDK emitters can project explicit types with documentation.
Wire layout (data words, pointer counts, field offsets, discriminants) is
never derived here; it comes from ``schema/plugin.layout.json`` written by
``tools/abi-layout`` from the Cap'n Proto compiler.

Supported subset: file id, ``using`` aliases and imports, ``annotation``
declarations, ``const``, ``struct`` (fields and one anonymous ``union``),
``enum``, and ``interface`` with methods whose parameter / result lists may
span several lines and carry inline doc comments. Groups, nested named types,
generics, and default values are rejected because the ABI does not use them.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Iterator

# ---------------------------------------------------------------------------
# Tokenizer
# ---------------------------------------------------------------------------

_TOKEN_RE = re.compile(
    r"""
    (?P<ws>[ \t]+)
  | (?P<nl>\n)
  | (?P<comment>\#[^\n]*)
  | (?P<arrow>->)
  | (?P<string>"(?:[^"\\]|\\.)*")
  | (?P<hex>0x[0-9a-fA-F]+)
  | (?P<number>\d+)
  | (?P<ident>[A-Za-z_][A-Za-z0-9_]*)
  | (?P<punct>[{}():;=@,$.])
    """,
    re.VERBOSE,
)


@dataclass
class Token:
    """One lexical token with its 1-based source line."""

    kind: str
    text: str
    line: int


def tokenize(text: str) -> list[Token]:
    """Split schema text into tokens (whitespace dropped, newlines kept)."""
    out: list[Token] = []
    line = 1
    pos = 0
    while pos < len(text):
        m = _TOKEN_RE.match(text, pos)
        if not m:
            raise SystemExit(f"plugin.capnp: unexpected character {text[pos]!r} on line {line}")
        kind = m.lastgroup or ""
        if kind == "nl":
            out.append(Token("nl", "\n", line))
            line += 1
        elif kind != "ws":
            out.append(Token(kind, m.group(0), line))
        pos = m.end()
    out.append(Token("eof", "", line))
    return out


# ---------------------------------------------------------------------------
# AST
# ---------------------------------------------------------------------------


@dataclass
class TypeRef:
    """A type expression: scalar, named type, or ``List(inner)``."""

    name: str
    inner: TypeRef | None = None

    @property
    def is_list(self) -> bool:
        return self.inner is not None

    def render(self) -> str:
        if self.inner is not None:
            return f"List({self.inner.render()})"
        return self.name


SCALARS = {
    "Void",
    "Bool",
    "Int8",
    "Int16",
    "Int32",
    "Int64",
    "UInt8",
    "UInt16",
    "UInt32",
    "UInt64",
    "Float32",
    "Float64",
    "Text",
    "Data",
    "AnyPointer",
}


@dataclass
class Field:
    """One struct field; ``in_union`` marks anonymous-union members."""

    name: str
    ordinal: int
    type: TypeRef
    doc: list[str] = field(default_factory=list)
    annotations: list[str] = field(default_factory=list)
    in_union: bool = False
    line: int = 0


@dataclass
class Struct:
    """One ``struct`` declaration."""

    name: str
    doc: list[str] = field(default_factory=list)
    fields: list[Field] = field(default_factory=list)
    annotations: list[str] = field(default_factory=list)
    line: int = 0

    @property
    def union_fields(self) -> list[Field]:
        return [f for f in self.fields if f.in_union]

    @property
    def plain_fields(self) -> list[Field]:
        return [f for f in self.fields if not f.in_union]

    @property
    def has_union(self) -> bool:
        return any(f.in_union for f in self.fields)


@dataclass
class Enumerant:
    """One enum member."""

    name: str
    ordinal: int
    doc: list[str] = field(default_factory=list)
    line: int = 0


@dataclass
class Enum:
    """One ``enum`` declaration."""

    name: str
    doc: list[str] = field(default_factory=list)
    enumerants: list[Enumerant] = field(default_factory=list)
    annotations: list[str] = field(default_factory=list)
    line: int = 0

    def wire_names(self) -> list[str]:
        """Enumerants in ordinal order."""
        return [e.name for e in sorted(self.enumerants, key=lambda e: e.ordinal)]


@dataclass
class Param:
    """One method parameter or result."""

    name: str
    type: TypeRef
    doc: list[str] = field(default_factory=list)


@dataclass
class Method:
    """One interface method."""

    name: str
    ordinal: int
    params: list[Param] = field(default_factory=list)
    results: list[Param] = field(default_factory=list)
    doc: list[str] = field(default_factory=list)
    line: int = 0


@dataclass
class Interface:
    """One ``interface`` declaration."""

    name: str
    doc: list[str] = field(default_factory=list)
    methods: list[Method] = field(default_factory=list)
    line: int = 0


@dataclass
class Const:
    """One file-scope ``const``."""

    name: str
    type: TypeRef
    value: int | str
    doc: list[str] = field(default_factory=list)
    line: int = 0


@dataclass
class Alias:
    """``using Name = Target;`` (imports are recorded with ``is_import``)."""

    name: str
    target: str
    doc: list[str] = field(default_factory=list)
    is_import: bool = False
    line: int = 0


@dataclass
class Annotation:
    """``annotation name @id (targets) :Type;``."""

    name: str
    doc: list[str] = field(default_factory=list)
    line: int = 0


Decl = Struct | Enum | Interface | Const | Alias | Annotation


@dataclass
class Schema:
    """Parsed schema file in declaration order."""

    file_id: str = ""
    header: list[str] = field(default_factory=list)
    decls: list[Decl] = field(default_factory=list)

    @property
    def structs(self) -> list[Struct]:
        return [d for d in self.decls if isinstance(d, Struct)]

    @property
    def enums(self) -> list[Enum]:
        return [d for d in self.decls if isinstance(d, Enum)]

    @property
    def interfaces(self) -> list[Interface]:
        return [d for d in self.decls if isinstance(d, Interface)]

    @property
    def consts(self) -> list[Const]:
        return [d for d in self.decls if isinstance(d, Const)]

    @property
    def aliases(self) -> list[Alias]:
        return [d for d in self.decls if isinstance(d, Alias) and not d.is_import]

    def struct(self, name: str) -> Struct:
        for s in self.structs:
            if s.name == name:
                return s
        raise KeyError(name)

    def enum(self, name: str) -> Enum:
        for e in self.enums:
            if e.name == name:
                return e
        raise KeyError(name)

    def names(self) -> dict[str, Decl]:
        return {d.name: d for d in self.decls}


# ---------------------------------------------------------------------------
# Parser
# ---------------------------------------------------------------------------


class _Parser:
    def __init__(self, tokens: list[Token]) -> None:
        self.toks = tokens
        self.i = 0
        # Pending doc-comment block: (lines, line number of the last comment).
        self.pending: list[str] = []
        self.pending_last_line = -10

    # -- token helpers ---------------------------------------------------

    def peek(self, k: int = 0) -> Token:
        j = self.i + k
        return self.toks[j] if j < len(self.toks) else self.toks[-1]

    def next(self) -> Token:
        tok = self.peek()
        self.i += 1
        return tok

    def expect(self, kind: str, text: str | None = None) -> Token:
        tok = self.next()
        if tok.kind != kind or (text is not None and tok.text != text):
            want = text if text is not None else kind
            raise SystemExit(
                f"plugin.capnp:{tok.line}: expected {want!r}, found {tok.text!r}"
            )
        return tok

    def accept(self, kind: str, text: str | None = None) -> Token | None:
        tok = self.peek()
        if tok.kind == kind and (text is None or tok.text == text):
            self.i += 1
            return tok
        return None

    def skip_layout(self) -> None:
        """Consume newlines and comments, accumulating doc comments.

        A blank line (two consecutive newlines) or a ``#####`` divider resets
        the pending doc block so section banners never attach to declarations.
        """
        newlines = 0
        while True:
            tok = self.peek()
            if tok.kind == "nl":
                newlines += 1
                if newlines >= 2:
                    self.pending.clear()
                self.i += 1
                continue
            if tok.kind == "comment":
                newlines = 0
                body = tok.text[1:]
                if body.startswith("####"):
                    self.pending.clear()
                    self.pending_last_line = -10
                elif tok.line == self.pending_last_line + 1 or not self.pending:
                    self.pending.append(body.strip())
                    self.pending_last_line = tok.line
                else:
                    self.pending = [body.strip()]
                    self.pending_last_line = tok.line
                self.i += 1
                continue
            return

    def take_doc(self, decl_line: int) -> list[str]:
        """Doc block for a declaration starting on ``decl_line``."""
        if self.pending and self.pending_last_line == decl_line - 1:
            doc = list(self.pending)
        else:
            doc = []
        self.pending.clear()
        return doc

    def trailing_comment(self) -> list[str]:
        """Comment on the same line as the token just consumed, if any."""
        tok = self.peek()
        prev_line = self.toks[self.i - 1].line
        if tok.kind == "comment" and tok.line == prev_line:
            self.i += 1
            return [tok.text[1:].strip()]
        return []

    # -- grammar ---------------------------------------------------------

    def parse_type(self) -> TypeRef:
        name = self.expect("ident").text
        while self.accept("punct", "."):
            # Imported namespace qualifier (`Plugin.Foo`): the plugin ABI is a
            # single namespace, so keep only the final component.
            name = self.expect("ident").text
        if name == "List":
            self.expect("punct", "(")
            inner = self.parse_type()
            self.expect("punct", ")")
            return TypeRef("List", inner)
        return TypeRef(name)

    def parse_annotations(self) -> list[str]:
        out: list[str] = []
        while self.accept("punct", "$"):
            out.append(self.expect("ident").text)
            if self.accept("punct", "("):
                depth = 1
                while depth:
                    t = self.next()
                    if t.kind == "punct" and t.text == "(":
                        depth += 1
                    elif t.kind == "punct" and t.text == ")":
                        depth -= 1
                    elif t.kind == "eof":
                        raise SystemExit("unterminated annotation arguments")
        return out

    def parse_file(self) -> Schema:
        schema = Schema()
        # Header: comment block before the file id.
        self.skip_layout()
        if self.pending:
            schema.header = list(self.pending)
            self.pending.clear()
        while True:
            self.skip_layout()
            tok = self.peek()
            if tok.kind == "eof":
                break
            if tok.kind == "punct" and tok.text == "@":
                self.next()
                schema.file_id = self.expect("hex").text
                self.expect("punct", ";")
                self.pending.clear()
                continue
            if tok.kind != "ident":
                raise SystemExit(f"plugin.capnp:{tok.line}: unexpected {tok.text!r}")
            if tok.text == "const":
                schema.decls.append(self.parse_const())
            elif tok.text == "using":
                schema.decls.append(self.parse_using())
            elif tok.text == "annotation":
                schema.decls.append(self.parse_annotation())
            elif tok.text == "struct":
                schema.decls.append(self.parse_struct())
            elif tok.text == "enum":
                schema.decls.append(self.parse_enum())
            elif tok.text == "interface":
                schema.decls.append(self.parse_interface())
            else:
                raise SystemExit(
                    f"plugin.capnp:{tok.line}: unsupported declaration {tok.text!r}"
                )
        return schema

    def parse_const(self) -> Const:
        kw = self.expect("ident", "const")
        doc = self.take_doc(kw.line)
        name = self.expect("ident").text
        self.expect("punct", ":")
        ty = self.parse_type()
        self.expect("punct", "=")
        val = self.next()
        if val.kind == "number":
            value: int | str = int(val.text)
        elif val.kind == "string":
            value = val.text[1:-1]
        else:
            raise SystemExit(f"plugin.capnp:{val.line}: unsupported const value {val.text!r}")
        self.expect("punct", ";")
        doc = doc or self.trailing_comment()
        return Const(name=name, type=ty, value=value, doc=doc, line=kw.line)

    def parse_using(self) -> Alias:
        kw = self.expect("ident", "using")
        doc = self.take_doc(kw.line)
        name = self.expect("ident").text
        self.expect("punct", "=")
        if self.peek().kind == "ident" and self.peek().text == "import":
            self.next()
            target = self.expect("string").text[1:-1]
            self.expect("punct", ";")
            return Alias(name=name, target=target, doc=doc, is_import=True, line=kw.line)
        target = self.parse_type().render()
        self.expect("punct", ";")
        doc = doc or self.trailing_comment()
        return Alias(name=name, target=target, doc=doc, line=kw.line)

    def parse_annotation(self) -> Annotation:
        kw = self.expect("ident", "annotation")
        doc = self.take_doc(kw.line)
        name = self.expect("ident").text
        while not self.accept("punct", ";"):
            if self.next().kind == "eof":
                raise SystemExit("unterminated annotation declaration")
        return Annotation(name=name, doc=doc, line=kw.line)

    def parse_struct(self) -> Struct:
        kw = self.expect("ident", "struct")
        doc = self.take_doc(kw.line)
        name = self.expect("ident").text
        annos = self.parse_annotations()
        self.expect("punct", "{")
        st = Struct(name=name, doc=doc, annotations=annos, line=kw.line)
        self.parse_struct_body(st, in_union=False)
        return st

    def parse_struct_body(self, st: Struct, in_union: bool) -> None:
        while True:
            self.skip_layout()
            tok = self.peek()
            if tok.kind == "punct" and tok.text == "}":
                self.next()
                self.pending.clear()
                return
            if tok.kind == "ident" and tok.text == "union":
                if in_union:
                    raise SystemExit(f"plugin.capnp:{tok.line}: nested unions are not supported")
                self.next()
                self.expect("punct", "{")
                self.pending.clear()
                self.parse_struct_body(st, in_union=True)
                continue
            if tok.kind == "ident" and tok.text in ("struct", "enum", "group", "interface"):
                raise SystemExit(
                    f"plugin.capnp:{tok.line}: nested {tok.text} in struct {st.name} is not supported"
                )
            fname = self.expect("ident")
            doc = self.take_doc(fname.line)
            self.expect("punct", "@")
            ordinal = int(self.expect("number").text)
            self.expect("punct", ":")
            ty = self.parse_type()
            annos = self.parse_annotations()
            if self.accept("punct", "="):
                raise SystemExit(
                    f"plugin.capnp:{fname.line}: default values are not supported ({st.name}.{fname.text})"
                )
            self.expect("punct", ";")
            doc = doc or self.trailing_comment()
            st.fields.append(
                Field(
                    name=fname.text,
                    ordinal=ordinal,
                    type=ty,
                    doc=doc,
                    annotations=annos,
                    in_union=in_union,
                    line=fname.line,
                )
            )

    def parse_enum(self) -> Enum:
        kw = self.expect("ident", "enum")
        doc = self.take_doc(kw.line)
        name = self.expect("ident").text
        annos = self.parse_annotations()
        self.expect("punct", "{")
        en = Enum(name=name, doc=doc, annotations=annos, line=kw.line)
        while True:
            self.skip_layout()
            tok = self.peek()
            if tok.kind == "punct" and tok.text == "}":
                self.next()
                self.pending.clear()
                return en
            ename = self.expect("ident")
            edoc = self.take_doc(ename.line)
            self.expect("punct", "@")
            ordinal = int(self.expect("number").text)
            self.expect("punct", ";")
            edoc = edoc or self.trailing_comment()
            en.enumerants.append(Enumerant(name=ename.text, ordinal=ordinal, doc=edoc, line=ename.line))

    def parse_param_list(self) -> list[Param]:
        """``( name :Type, ... )`` with optional per-parameter doc comments."""
        self.expect("punct", "(")
        out: list[Param] = []
        while True:
            self.skip_layout()
            if self.accept("punct", ")"):
                self.pending.clear()
                return out
            pname = self.expect("ident")
            doc = self.take_doc(pname.line)
            self.expect("punct", ":")
            ty = self.parse_type()
            self.accept("punct", ",")
            doc = doc or self.trailing_comment()
            out.append(Param(name=pname.text, type=ty, doc=doc))

    def parse_interface(self) -> Interface:
        kw = self.expect("ident", "interface")
        doc = self.take_doc(kw.line)
        name = self.expect("ident").text
        self.expect("punct", "{")
        iface = Interface(name=name, doc=doc, line=kw.line)
        while True:
            self.skip_layout()
            tok = self.peek()
            if tok.kind == "punct" and tok.text == "}":
                self.next()
                self.pending.clear()
                return iface
            mname = self.expect("ident")
            mdoc = self.take_doc(mname.line)
            self.expect("punct", "@")
            ordinal = int(self.expect("number").text)
            params = self.parse_param_list()
            results: list[Param] = []
            while self.peek().kind == "nl":
                self.next()
            if self.accept("arrow"):
                results = self.parse_param_list()
            self.expect("punct", ";")
            mdoc = mdoc or self.trailing_comment()
            iface.methods.append(
                Method(
                    name=mname.text,
                    ordinal=ordinal,
                    params=params,
                    results=results,
                    doc=mdoc,
                    line=mname.line,
                )
            )


def parse_schema(text: str) -> Schema:
    """Parse one schema file."""
    return _Parser(tokenize(text)).parse_file()


# ---------------------------------------------------------------------------
# Doc-comment lint
# ---------------------------------------------------------------------------


def missing_docs(schema: Schema) -> Iterator[str]:
    """Yield ``line: what`` for every declaration without a doc comment.

    Every struct, field, union member, enum, enumerant, interface, method,
    method parameter, const, and alias must carry a ``#`` comment on the
    line(s) immediately above it (or trailing on the same line). Generated
    TSDoc / docstrings are only as complete as the schema.
    """
    for decl in schema.decls:
        if isinstance(decl, Annotation):
            continue
        if isinstance(decl, Alias) and decl.is_import:
            continue
        if not decl.doc:
            yield f"{decl.line}: {type(decl).__name__.lower()} {decl.name}"
        if isinstance(decl, Struct):
            for f in decl.fields:
                if not f.doc:
                    yield f"{f.line}: field {decl.name}.{f.name}"
        elif isinstance(decl, Enum):
            for e in decl.enumerants:
                if not e.doc:
                    yield f"{e.line}: enumerant {decl.name}.{e.name}"
        elif isinstance(decl, Interface):
            for m in decl.methods:
                if not m.doc:
                    yield f"{m.line}: method {decl.name}.{m.name}"
                for p in m.params:
                    if not p.doc:
                        yield f"{m.line}: param {decl.name}.{m.name}({p.name})"
