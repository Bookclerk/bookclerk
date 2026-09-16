"""SDK code emitters for scripts/gen-plugin-abi.py.

Generates the TypeScript and Python SDK projections from the single ABI
source of truth: ``crates/bookclerk-plugin-abi/schema/plugin.capnp``. Three
layers are produced per language, all from the hand-written schema:

1. Author-facing explicit types: TypeScript ``interface`` / ``type`` unions
   and Python ``TypedDict`` / ``Literal`` unions / ``Protocol`` classes, with
   every schema ``#`` comment preserved as TSDoc / docstrings so IDE hover
   and the generated API reference are complete.
2. ``@internal`` wire codecs (``_``-prefixed in Python) built on the runtime
   Cap'n Proto mini-libraries in ``db-capnp.ts`` / ``db_value.py``. Field
   offsets, data-word counts, pointer counts, and union discriminants come
   from ``schema/plugin.layout.json`` (written by ``tools/abi-layout`` from
   the Cap'n Proto compiler); nothing here derives layout by hand.
3. Product constants and enum ordinal tables.

Emitted artifacts (all carry a GENERATED header; edit the schema and re-run
``scripts/gen-plugin-abi.py --write`` instead of editing them):

- ``packages/plugin-sdk/src/abi.ts`` (constants + enum tables)
- ``packages/plugin-sdk/src/generated.ts`` (types, unions, interfaces)
- ``packages/plugin-sdk/src/generated-wire.ts`` (codecs)
- ``packages/plugin-sdk-python/src/bookclerk_plugin_sdk/_abi.py``
- ``packages/plugin-sdk-python/src/bookclerk_plugin_sdk/abi.py``
- ``packages/plugin-sdk-python/src/bookclerk_plugin_sdk/_wire.py``

Plus in-place constant / error-code rewrites for
``packages/plugin-sdk/embed/bookclerk_plugin.js``.
"""

from __future__ import annotations

import json
import keyword
import re
import textwrap
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import capnp_schema as cs

GENERATED_NOTE = (
    "GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` "
    "after changing crates/bookclerk-plugin-abi/schema/plugin.capnp."
)

# Enums whose values travel on the wire as snake_case `Text` codes rather than
# 16-bit ordinals (`PluginError.code`); either annotation name marks one.
TEXT_ENUM_ANNOTATIONS = frozenset({"textEnum", "jsonEnum"})


# ---------------------------------------------------------------------------
# Compatibility shims used by gen-plugin-abi.py checks
# ---------------------------------------------------------------------------


def parse_capnp_constants(text: str) -> dict[str, int | str]:
    """File-scope ``const`` declarations, camelCase name -> value."""
    schema = cs.parse_schema(text)
    out: dict[str, int | str] = {c.name: c.value for c in schema.consts}
    if not out:
        raise SystemExit("no file-scope constants found in plugin.capnp")
    return out


def parse_capnp_enum(text: str, name: str) -> list[str]:
    """Ordinal-ordered enumerant wire names of one Cap'n Proto enum."""
    schema = cs.parse_schema(text)
    try:
        return schema.enum(name).wire_names()
    except KeyError as exc:
        raise SystemExit(f"enum {name} not found in plugin.capnp") from exc


def snake_wire(camel: str) -> str:
    """Wire string for a ``$textEnum`` enumerant (snake_case)."""
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", camel).lower()


def _screaming(camel: str) -> str:
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", camel).upper()


def _pascal(name: str) -> str:
    return name[:1].upper() + name[1:]


_PY_KEYWORDS = frozenset(keyword.kwlist) | {"self"}


def _py_param(name: str) -> str:
    """Python identifier for a Cap'n parameter (``from`` -> ``from_``)."""
    out = _snake(name)
    return f"{out}_" if out in _PY_KEYWORDS else out


def _snake(name: str) -> str:
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", name).lower()


def enum_table_name(enum_name: str) -> str:
    """Constant table name for an enum (``DbStatementKind`` -> ``DB_STATEMENT_KINDS``)."""
    return _screaming(enum_name) + "S"


def const_name(camel: str) -> str:
    """Exported constant name (``apiVersion`` is the product ABI version)."""
    return "PRODUCT_API_VERSION" if camel == "apiVersion" else _screaming(camel)


def is_text_enum(enum: cs.Enum) -> bool:
    return any(a in TEXT_ENUM_ANNOTATIONS for a in enum.annotations)


def enum_wire_values(enum: cs.Enum) -> list[str]:
    """Wire strings in ordinal order (snake_case for ``$textEnum``)."""
    names = enum.wire_names()
    return [snake_wire(n) for n in names] if is_text_enum(enum) else names


# ---------------------------------------------------------------------------
# Layout (from tools/abi-layout)
# ---------------------------------------------------------------------------


@dataclass
class Layout:
    """Parsed ``plugin.layout.json``."""

    raw: dict[str, Any]

    @property
    def structs(self) -> dict[str, dict[str, Any]]:
        return self.raw["structs"]

    @property
    def enums(self) -> dict[str, dict[str, Any]]:
        return self.raw["enums"]

    @property
    def interfaces(self) -> dict[str, dict[str, Any]]:
        return self.raw["interfaces"]

    def struct(self, name: str) -> dict[str, Any]:
        try:
            return self.structs[name]
        except KeyError as exc:
            raise SystemExit(
                f"layout has no struct {name!r}; run tools/abi-layout to refresh plugin.layout.json"
            ) from exc

    def field(self, struct: str, name: str) -> dict[str, Any]:
        for f in self.struct(struct)["fields"]:
            if f["name"] == name:
                return f
        raise SystemExit(f"layout struct {struct!r} has no field {name!r}")


def load_layout(path: Path) -> Layout:
    """Read ``plugin.layout.json``."""
    return Layout(json.loads(path.read_text(encoding="utf-8")))


def envelope_name(interface: str, method: str, which: str) -> str:
    """Layout name of an implicit method envelope struct."""
    return f"{interface}.{method}${which}"


def envelope_type_name(interface: str, method: str, which: str) -> str:
    """Generated type name of a method envelope (``GuestDatabaseExecuteParams``)."""
    return f"{interface}{_pascal(method)}{which}"


def check_layout(schema: cs.Schema, layout: Layout, file: str = "plugin.capnp") -> list[str]:
    """Cross-check schema text against the compiler layout (stale-layout guard)."""
    errors: list[str] = []
    layout_structs = {n for n, s in layout.structs.items() if s["file"] == file and "$" not in n}
    schema_structs = {s.name for s in schema.structs}
    for name in sorted(schema_structs - layout_structs):
        errors.append(f"struct {name} is in plugin.capnp but not in plugin.layout.json")
    for name in sorted(layout_structs - schema_structs):
        errors.append(f"struct {name} is in plugin.layout.json but not in plugin.capnp")
    for st in schema.structs:
        if st.name not in layout.structs:
            continue
        lf = {f["name"]: f for f in layout.struct(st.name)["fields"]}
        for f in st.fields:
            if f.name not in lf:
                errors.append(f"field {st.name}.{f.name} missing from layout")
                continue
            if f.in_union != (lf[f.name]["discriminantValue"] is not None):
                errors.append(f"field {st.name}.{f.name} union membership differs from layout")
    layout_enums = {n for n, e in layout.enums.items() if e["file"] == file}
    for en in schema.enums:
        if en.name not in layout_enums:
            errors.append(f"enum {en.name} missing from layout")
        elif layout.enums[en.name]["enumerants"] != en.wire_names():
            errors.append(f"enum {en.name} enumerants differ from layout")
    layout_ifaces = {n for n, i in layout.interfaces.items() if i["file"] == file}
    for iface in schema.interfaces:
        if iface.name not in layout_ifaces:
            errors.append(f"interface {iface.name} missing from layout")
            continue
        got_methods = [m["name"] for m in layout.interfaces[iface.name]["methods"]]
        want_methods = [m.name for m in sorted(iface.methods, key=lambda m: m.ordinal)]
        if got_methods != want_methods:
            errors.append(f"interface {iface.name} methods differ from layout")
    names = set(schema.names())
    for iface in schema.interfaces:
        for m in iface.methods:
            for which in ("Params", "Results"):
                if envelope_type_name(iface.name, m.name, which) in names:
                    errors.append(
                        f"struct {envelope_type_name(iface.name, m.name, which)} collides with the "
                        f"generated envelope name of {iface.name}.{m.name}"
                    )
    return errors


_LAYOUT_SCALAR = {
    "Void": "void",
    "Bool": "bool",
    "Int8": "int8",
    "Int16": "int16",
    "Int32": "int32",
    "Int64": "int64",
    "UInt8": "uint8",
    "UInt16": "uint16",
    "UInt32": "uint32",
    "UInt64": "uint64",
    "Float32": "float32",
    "Float64": "float64",
    "Text": "text",
    "Data": "data",
    "AnyPointer": "anyPointer",
}


# ---------------------------------------------------------------------------
# Type resolution
# ---------------------------------------------------------------------------


class Resolver:
    """Maps schema type names onto their declaration kind."""

    def __init__(self, schema: cs.Schema) -> None:
        self.schema = schema
        self.kinds: dict[str, str] = {}
        for d in schema.decls:
            if isinstance(d, cs.Struct):
                self.kinds[d.name] = "struct"
            elif isinstance(d, cs.Enum):
                self.kinds[d.name] = "enum"
            elif isinstance(d, cs.Interface):
                self.kinds[d.name] = "interface"
        for a in schema.aliases:
            target = a.target
            if target in self.kinds:
                self.kinds[a.name] = self.kinds[target]
        self.aliases = {a.name: a.target for a in schema.aliases}

    def canonical(self, name: str) -> str:
        while name in self.aliases:
            name = self.aliases[name]
        return name

    def kind_of(self, ty: cs.TypeRef) -> str:
        if ty.inner is not None:
            return "list"
        if ty.name in cs.SCALARS:
            return ty.name
        name = self.canonical(ty.name)
        try:
            return self.kinds[name]
        except KeyError as exc:
            raise SystemExit(f"unknown type {ty.name!r} in plugin.capnp") from exc


def _check_layout_types(schema: cs.Schema, layout: Layout) -> list[str]:
    res = Resolver(schema)
    errors: list[str] = []

    def want(ty: cs.TypeRef) -> dict[str, Any]:
        if ty.inner is not None:
            return {"kind": "list", "element": want(ty.inner)}
        if ty.name in _LAYOUT_SCALAR:
            return {"kind": _LAYOUT_SCALAR[ty.name]}
        name = res.canonical(ty.name)
        return {"kind": res.kinds[name], "name": name}

    for st in schema.structs:
        if st.name not in layout.structs:
            continue
        lf = {f["name"]: f for f in layout.struct(st.name)["fields"]}
        for f in st.fields:
            if f.name in lf and want(f.type) != lf[f.name]["type"]:
                errors.append(
                    f"field {st.name}.{f.name} type differs: schema={want(f.type)} layout={lf[f.name]['type']}"
                )
    return errors


def layout_errors(schema: cs.Schema, layout: Layout) -> list[str]:
    """All schema/layout consistency errors."""
    errors = check_layout(schema, layout)
    errors.extend(_check_layout_types(schema, layout))
    return errors


# ---------------------------------------------------------------------------
# Doc helpers
# ---------------------------------------------------------------------------


def _ts_doc(doc: list[str], indent: str = "", extra: list[str] | None = None) -> list[str]:
    body = [line for line in doc]
    if extra:
        if body:
            body.append("")
        body.extend(extra)
    if not body:
        return []
    if len(body) == 1 and len(body[0]) + len(indent) <= 90 and not extra:
        return [f"{indent}/** {body[0]} */"]
    out = [f"{indent}/**"]
    for line in body:
        out.append(f"{indent} * {line}".rstrip())
    out.append(f"{indent} */")
    return out


def _py_docstring(doc: list[str], indent: str, attrs: list[tuple[str, list[str]]] | None = None) -> list[str]:
    text = list(doc) or ["(undocumented)"]
    lines = [f'{indent}"""{text[0]}']
    for line in text[1:]:
        lines.append(f"{indent}{line}".rstrip())
    if attrs:
        lines.append("")
        lines.append(f"{indent}Attributes:")
        for name, adoc in attrs:
            joined = " ".join(adoc) if adoc else name
            lines.extend(
                textwrap.wrap(
                    f"{name}: {joined}",
                    width=88,
                    initial_indent=f"{indent}    ",
                    subsequent_indent=f"{indent}        ",
                )
            )
    if len(lines) == 1:
        lines[0] += '"""'
    else:
        lines.append(f'{indent}"""')
    return lines


# ---------------------------------------------------------------------------
# TypeScript: abi.ts (constants + enum tables)
# ---------------------------------------------------------------------------


def emit_ts_abi(capnp_text: str) -> str:
    """`packages/plugin-sdk/src/abi.ts` — product constants + enum tables."""
    schema = cs.parse_schema(capnp_text)
    lines = [
        "/**",
        f" * {GENERATED_NOTE}",
        " *",
        " * TypeScript projection of the product ABI constants and enum ordinal",
        " * tables declared in `crates/bookclerk-plugin-abi/schema/plugin.capnp`.",
        " */",
        "",
    ]
    for c in schema.consts:
        lines.extend(_ts_doc(c.doc or [c.name]))
        name = const_name(c.name)
        if isinstance(c.value, int):
            lines.append(f"export const {name} = {c.value} as const;")
        else:
            lines.append(f'export const {name} = "{c.value}" as const;')
        lines.append("")
    for en in schema.enums:
        table = enum_table_name(en.name)
        rendered = ", ".join(f'"{v}"' for v in enum_wire_values(en))
        note = (
            f"Wire strings of `{en.name}` in ordinal order (snake_case `Text` codes)."
            if is_text_enum(en)
            else f"Ordinal-ordered `{en.name}` wire names (index = Cap'n Proto ordinal)."
        )
        lines.extend(_ts_doc(en.doc, extra=[note]))
        lines.append(f"export const {table} = [{rendered}] as const;")
        lines.append("")
        lines.append(f"/** Union of `{table}` wire names. */")
        lines.append(f"export type {en.name} = (typeof {table})[number];")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Python: _abi.py (constants + enum tables)
# ---------------------------------------------------------------------------


def emit_py_product_abi(capnp_text: str) -> str:
    """`packages/plugin-sdk-python/.../_abi.py` — product constants + enum tables."""
    schema = cs.parse_schema(capnp_text)
    lines = [
        f'"""{GENERATED_NOTE}',
        "",
        "Python projection of the product ABI constants and enum ordinal tables",
        "declared in ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.",
        '"""',
        "",
        "from __future__ import annotations",
        "",
    ]
    names: list[str] = []
    for c in schema.consts:
        name = const_name(c.name)
        if isinstance(c.value, int):
            lines.append(f"{name}: int = {c.value}")
        else:
            lines.append(f'{name}: str = "{c.value}"')
        lines.extend(_py_docstring(c.doc or [c.name], ""))
        lines.append("")
        names.append(name)
    for en in schema.enums:
        table = enum_table_name(en.name)
        rendered = ", ".join(f'"{v}"' for v in enum_wire_values(en))
        note = (
            f"Wire strings of ``{en.name}`` in ordinal order (snake_case ``Text`` codes)."
            if is_text_enum(en)
            else f"Ordinal-ordered ``{en.name}`` wire names (index = Cap'n Proto ordinal)."
        )
        lines.append(f"{table}: tuple[str, ...] = ({rendered})")
        lines.extend(_py_docstring([*en.doc, "", note] if en.doc else [note], ""))
        lines.append("")
        names.append(table)
    exported = ",\n".join(f'    "{n}"' for n in names)
    lines.append(f"__all__ = [\n{exported},\n]")
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Embed rewriting (packages/plugin-sdk/embed/bookclerk_plugin.js)
# ---------------------------------------------------------------------------


def sync_embed_constants(embed_text: str, capnp_text: str) -> str:
    """Rewrite constants and the error-code set in the embed JS from the schema."""
    schema = cs.parse_schema(capnp_text)
    consts = {const_name(c.name): c.value for c in schema.consts}
    out = embed_text
    for match in re.finditer(r'^export const (\w+) = (?:\d+|"[^"]*");$', out, re.MULTILINE):
        name = match.group(1)
        if name not in consts:
            raise SystemExit(
                f"embed constant `{name}` is not declared in plugin.capnp; "
                "declare it in the schema or rename it"
            )
        value = consts[name]
        rendered = str(value) if isinstance(value, int) else f'"{value}"'
        out = out.replace(match.group(0), f"export const {name} = {rendered};")
    codes = enum_wire_values(schema.enum("PluginErrorCode"))
    rendered_codes = "\n".join(f'  "{code}",' for code in codes)
    out, n = re.subn(
        r"const KNOWN_ERROR_CODES = new Set\(\[\n(?:  \"[a-z_]+\",\n)+\]\);",
        f"const KNOWN_ERROR_CODES = new Set([\n{rendered_codes}\n]);",
        out,
    )
    if n != 1:
        raise SystemExit("KNOWN_ERROR_CODES set not found in embed bookclerk_plugin.js")
    return out


# ---------------------------------------------------------------------------
# Type mapping
# ---------------------------------------------------------------------------

_TS_SCALAR = {
    "Void": "void",
    "Bool": "boolean",
    "Int8": "number",
    "Int16": "number",
    "Int32": "number",
    "Int64": "bigint",
    "UInt8": "number",
    "UInt16": "number",
    "UInt32": "number",
    "UInt64": "number",
    "Float32": "number",
    "Float64": "number",
    "Text": "string",
    "Data": "Uint8Array",
    "AnyPointer": "unknown",
}

_PY_SCALAR = {
    "Void": "None",
    "Bool": "bool",
    "Int8": "int",
    "Int16": "int",
    "Int32": "int",
    "Int64": "int",
    "UInt8": "int",
    "UInt16": "int",
    "UInt32": "int",
    "UInt64": "int",
    "Float32": "float",
    "Float64": "float",
    "Text": "str",
    "Data": "bytes",
    "AnyPointer": "object",
}


def ts_type(ty: cs.TypeRef, res: Resolver) -> str:
    """Author-facing TypeScript type for a schema type."""
    if ty.inner is not None:
        inner = ts_type(ty.inner, res)
        return f"{inner}[]"
    if ty.name in _TS_SCALAR:
        return _TS_SCALAR[ty.name]
    return ty.name


def py_type(ty: cs.TypeRef, res: Resolver) -> str:
    """Author-facing Python type for a schema type."""
    if ty.inner is not None:
        return f"list[{py_type(ty.inner, res)}]"
    if ty.name in _PY_SCALAR:
        return _PY_SCALAR[ty.name]
    return ty.name


# `$optional` marks a scalar field whose zero value (empty `Text` / `Data`,
# numeric `0`) means "absent". Author-facing projections surface it as
# optional and the codecs map the zero value both ways.
OPTIONAL_ANNOTATION = "optional"

# Scalar kinds that may carry `$optional` (bool, enum, struct, list, and
# interface fields have no zero-means-absent convention).
_OPTIONAL_KINDS = frozenset(
    {"text", "data", "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32", "uint64", "float32", "float64"}
)


def field_is_optional(f: cs.Field) -> bool:
    return OPTIONAL_ANNOTATION in f.annotations


def check_optional_fields(schema: cs.Schema) -> list[str]:
    """`$optional` is only meaningful on zero-defaulting scalars."""
    errors: list[str] = []
    for st in schema.structs:
        for f in st.fields:
            if not field_is_optional(f):
                continue
            kind = _LAYOUT_SCALAR.get(f.type.name)
            if f.type.inner is not None or kind not in _OPTIONAL_KINDS:
                errors.append(f"{st.name}.{f.name}: $optional requires a Text, Data, or numeric field")
            if f in st.union_fields:
                errors.append(f"{st.name}.{f.name}: $optional is not allowed on union members")
    return errors


RUST_SECTION_START = "# --- rust-generated: begin ---"
RUST_SECTION_END = "# --- rust-generated: end ---"


def rust_generated_decls(capnp_text: str) -> list[cs.Decl]:
    """Structs and enums declared between the ``rust-generated`` markers.

    Rust DTOs and Cap'n codecs for these are emitted into
    ``crates/bookclerk-plugin-abi/src/generated.rs``; everything else in the
    schema keeps its hand-written Rust projection in ``rpc_types.rs`` /
    ``rpc.rs``.
    """
    lines = capnp_text.splitlines()
    start = next((i for i, l in enumerate(lines, 1) if l.strip() == RUST_SECTION_START), None)
    end = next((i for i, l in enumerate(lines, 1) if l.strip() == RUST_SECTION_END), None)
    if start is None or end is None:
        raise SystemExit("plugin.capnp is missing the rust-generated section markers")
    schema = cs.parse_schema(capnp_text)
    return [d for d in schema.decls if start < d.line < end and isinstance(d, (cs.Struct, cs.Enum))]


# ---------------------------------------------------------------------------
# TypeScript: generated.ts (author-facing types)
# ---------------------------------------------------------------------------


def _ts_union_member(f: cs.Field, res: Resolver) -> str:
    if f.type.name == "Void" and f.type.inner is None:
        return f'{{ kind: "{f.name}" }}'
    return f'{{ kind: "{f.name}"; value: {ts_type(f.type, res)} }}'


def _method_result_ts(m: cs.Method, res: Resolver) -> str:
    if not m.results:
        return "void"
    if len(m.results) == 1:
        return ts_type(m.results[0].type, res)
    inner = "; ".join(f"{r.name}: {ts_type(r.type, res)}" for r in m.results)
    return f"{{ {inner} }}"


def _ts_returns_doc(m: cs.Method, res: Resolver) -> str:
    """TSDoc `@returns` text: the schema comment, else a link to the result type."""
    if len(m.results) == 1:
        r = m.results[0]
        if r.doc:
            return " ".join(r.doc)
        if r.type.inner is None and r.type.name not in _TS_SCALAR:
            return f"{{@link {r.type.name}}}"
        return f"`{r.name}` ({ts_type(r.type, res)})"
    return ", ".join(f"`{r.name}`" for r in m.results)


def _py_returns_doc(m: cs.Method, res: Resolver) -> str:
    """Docstring ``Returns:`` text: the schema comment, else the result type."""
    if len(m.results) == 1:
        r = m.results[0]
        if r.doc:
            return " ".join(r.doc)
        return f"``{py_type(r.type, res)}``"
    return ", ".join(f"``{r.name}``" for r in m.results)


def emit_ts_generated(capnp_text: str) -> str:
    """`packages/plugin-sdk/src/generated.ts` — explicit ABI types with TSDoc."""
    schema = cs.parse_schema(capnp_text)
    res = Resolver(schema)
    enum_names = [e.name for e in schema.enums]
    lines = [
        "/**",
        f" * {GENERATED_NOTE}",
        " *",
        " * Author-facing TypeScript projection of every struct, union, and",
        " * interface in `crates/bookclerk-plugin-abi/schema/plugin.capnp`. Field",
        " * names are the schema names (camelCase). Cap'n Proto unions become",
        " * discriminated `{ kind, value }` unions; `Void` members carry no",
        " * `value`. `Int64` is `bigint` (exact SQL integers); `UInt64` counters,",
        " * sizes, and unix-millisecond timestamps are `number`. `Data` is",
        " * `Uint8Array`. Interfaces are capabilities: their methods return",
        " * promises and the wire codecs carry them through a transport cap table.",
        " *",
        " * @module",
        " */",
        "",
        "import type {",
    ]
    for name in enum_names:
        lines.append(f"  {name},")
    lines.append('} from "./abi.js";')
    lines.append("")
    lines.append("export type {")
    for name in enum_names:
        lines.append(f"  {name},")
    lines.append("};")
    lines.append("")

    for decl in schema.decls:
        if isinstance(decl, cs.Alias) and not decl.is_import:
            lines.extend(_ts_doc(decl.doc))
            lines.append(f"export type {decl.name} = {decl.target};")
            lines.append("")
        elif isinstance(decl, cs.Struct):
            if decl.has_union:
                if decl.plain_fields:
                    raise SystemExit(
                        f"struct {decl.name} mixes union and plain fields; the emitter supports whole-struct unions only"
                    )
                lines.extend(_ts_doc(decl.doc))
                members = [_ts_union_member(f, res) for f in decl.union_fields]
                if len(members) == 1:
                    lines.append(f"export type {decl.name} = {members[0]};")
                else:
                    lines.append(f"export type {decl.name} =")
                    for i, member in enumerate(members):
                        member_doc = " ".join(decl.union_fields[i].doc)
                        comment = f" // {member_doc}" if member_doc else ""
                        end = ";" if i == len(members) - 1 else ""
                        lines.append(f"  | {member}{end}{comment}")
                lines.append("")
            else:
                lines.extend(_ts_doc(decl.doc))
                lines.append(f"export interface {decl.name} {{")
                for f in decl.fields:
                    fdoc = list(f.doc)
                    if field_is_optional(f):
                        fdoc.append("Omitted when absent (wire zero value).")
                    lines.extend(_ts_doc(fdoc, "  "))
                    opt = "?" if field_is_optional(f) else ""
                    lines.append(f"  {f.name}{opt}: {ts_type(f.type, res)};")
                lines.append("}")
                lines.append("")
        elif isinstance(decl, cs.Interface):
            lines.extend(_ts_doc(decl.doc))
            lines.append(f"export interface {decl.name} {{")
            for m in sorted(decl.methods, key=lambda m: m.ordinal):
                extra = [f"@param {p.name} - {' '.join(p.doc) or p.name}" for p in m.params]
                if m.results:
                    extra.append(f"@returns {_ts_returns_doc(m, res)}")
                lines.extend(_ts_doc(m.doc, "  ", extra))
                params = ", ".join(f"{p.name}: {ts_type(p.type, res)}" for p in m.params)
                lines.append(f"  {m.name}({params}): Promise<{_method_result_ts(m, res)}>;")
            lines.append("}")
            lines.append("")
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Python: abi.py (author-facing types)
# ---------------------------------------------------------------------------


def _py_union_member_name(struct: str, member: str) -> str:
    return f"{struct}{_pascal(member)}"


def _method_result_py(m: cs.Method, res: Resolver) -> str:
    if not m.results:
        return "None"
    if len(m.results) == 1:
        return py_type(m.results[0].type, res)
    return "dict[str, object]"


def emit_py_abi(capnp_text: str) -> str:
    """`packages/plugin-sdk-python/.../abi.py` — explicit ABI types with docstrings."""
    schema = cs.parse_schema(capnp_text)
    res = Resolver(schema)
    enum_names = [e.name for e in schema.enums]
    lines = [
        f'"""{GENERATED_NOTE}',
        "",
        "Author-facing Python projection of every struct, union, enum, and",
        "interface in ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.",
        "TypedDict keys are the schema names (camelCase). Cap'n Proto unions",
        "become ``{\"kind\": ..., \"value\": ...}`` TypedDict unions; ``Void``",
        "members carry no ``value``. 64-bit integers are Python ``int``; ``Data``",
        "is ``bytes``. Interfaces are ``Protocol`` classes with ``async`` methods.",
        '"""',
        "",
        "from __future__ import annotations",
        "",
        "from typing import Any, Literal, NotRequired, Protocol, TypedDict, Union",
        "",
    ]
    exported: list[str] = []
    for en in schema.enums:
        values = enum_wire_values(en)
        rendered = ", ".join(f'"{v}"' for v in values)
        if len(en.name) + len(rendered) <= 76:
            lines.append(f"{en.name} = Literal[{rendered}]")
        else:
            lines.append(f"{en.name} = Literal[")
            lines.extend(f'    "{v}",' for v in values)
            lines.append("]")
        lines.extend(_py_docstring(en.doc or [f"``{en.name}`` wire names."], ""))
        lines.append("")
        exported.append(en.name)
    lines.append("")

    for decl in schema.decls:
        if isinstance(decl, cs.Alias) and not decl.is_import:
            lines.append(f"{decl.name} = {decl.target}")
            lines.extend(_py_docstring(decl.doc, ""))
            lines.append("")
            lines.append("")
            exported.append(decl.name)
        elif isinstance(decl, cs.Struct):
            if decl.has_union:
                if decl.plain_fields:
                    raise SystemExit(
                        f"struct {decl.name} mixes union and plain fields; the emitter supports whole-struct unions only"
                    )
                member_names: list[str] = []
                for f in decl.union_fields:
                    mname = _py_union_member_name(decl.name, f.name)
                    member_names.append(mname)
                    lines.append(f"class {mname}(TypedDict):")
                    attrs = [("kind", [f'Always ``"{f.name}"``.'])]
                    if not (f.type.name == "Void" and f.type.inner is None):
                        attrs.append(("value", f.doc or [f"``{f.type.render()}`` payload."]))
                    lines.extend(
                        _py_docstring(
                            [f"``{decl.name}`` member ``{f.name}``.", *([""] + f.doc if f.doc else [])],
                            "    ",
                            attrs,
                        )
                    )
                    lines.append("")
                    lines.append(f'    kind: Literal["{f.name}"]')
                    if not (f.type.name == "Void" and f.type.inner is None):
                        lines.append(f"    value: {py_type(f.type, res)}")
                    lines.append("")
                    lines.append("")
                if len(member_names) == 1:
                    lines.append(f"{decl.name} = {member_names[0]}")
                else:
                    lines.append(f"{decl.name} = Union[")
                    lines.extend(f"    {n}," for n in member_names)
                    lines.append("]")
                lines.extend(_py_docstring(decl.doc or [f"``{decl.name}`` union."], ""))
                lines.append("")
                lines.append("")
                exported.extend(member_names)
                exported.append(decl.name)
            else:
                lines.append(f"class {decl.name}(TypedDict):")
                attr_docs = [
                    (
                        f.name,
                        [*f.doc, "Omitted when absent (wire zero value)."]
                        if field_is_optional(f)
                        else f.doc,
                    )
                    for f in decl.fields
                ]
                lines.extend(_py_docstring(decl.doc, "    ", attr_docs))
                lines.append("")
                if not decl.fields:
                    lines.append("    pass")
                for f in decl.fields:
                    fty = py_type(f.type, res)
                    if field_is_optional(f):
                        fty = f"NotRequired[{fty}]"
                    lines.append(f"    {f.name}: {fty}")
                lines.append("")
                lines.append("")
                exported.append(decl.name)
        elif isinstance(decl, cs.Interface):
            lines.append(f"class {decl.name}(Protocol):")
            lines.extend(_py_docstring(decl.doc or [f"``{decl.name}`` capability."], "    "))
            lines.append("")
            for m in sorted(decl.methods, key=lambda m: m.ordinal):
                params = "".join(f", {_py_param(p.name)}: {py_type(p.type, res)}" for p in m.params)
                lines.append(
                    f"    async def {_snake(m.name)}(self{params}) -> {_method_result_py(m, res)}:"
                )
                arg_doc = [(_py_param(p.name), p.doc) for p in m.params]
                mdoc = list(m.doc) or [f"``{decl.name}.{m.name}``."]
                dlines = [f'        """{mdoc[0]}']
                dlines.extend(f"        {line}".rstrip() for line in mdoc[1:])
                if arg_doc:
                    dlines.append("")
                    dlines.append("        Args:")
                    for name, pdoc in arg_doc:
                        dlines.extend(
                            textwrap.wrap(
                                f"{name}: {' '.join(pdoc) or name}",
                                width=88,
                                initial_indent="            ",
                                subsequent_indent="                ",
                            )
                        )
                if m.results:
                    dlines.append("")
                    dlines.append("        Returns:")
                    rdoc = _py_returns_doc(m, res)
                    dlines.extend(
                        textwrap.wrap(
                            rdoc, width=88, initial_indent="            ", subsequent_indent="            "
                        )
                    )
                dlines.append('        """')
                lines.extend(dlines)
                lines.append("        ...")
                lines.append("")
            lines.append("")
            exported.append(decl.name)
    body = "\n".join(lines)
    exported_lines = ",\n".join(f'    "{n}"' for n in exported)
    body = body.rstrip() + f"\n\n\n__all__ = [\n{exported_lines},\n]\n"
    body = re.sub(r"\n{4,}", "\n\n\n", body)
    return body


# ---------------------------------------------------------------------------
# Codec emission helpers
# ---------------------------------------------------------------------------

# Data-section setters/getters by layout kind: (ts setter, ts getter, py setter, py getter)
_DATA_ACCESSORS = {
    "bool": ("setBool", "getBool", "set_bool", "get_bool"),
    "int32": ("setInt32", "getInt32", "set_i32", "get_i32"),
    "uint16": ("setUint16", "getUint16", "set_u16", "get_u16"),
    "uint32": ("setUint32", "getUint32", "set_u32", "get_u32"),
    "int64": ("setInt64", "getInt64", "set_i64", "get_i64"),
    "uint64": ("setUint64", "getUint64", "set_u64", "get_u64"),
    "float32": ("setFloat32", "getFloat32", "set_f32", "get_f32"),
    "float64": ("setFloat64", "getFloat64", "set_f64", "get_f64"),
}

_UNSUPPORTED_LIST_ELEMENTS = {"void", "list", "anyPointer", "int8", "int16", "uint8", "uint16", "int32", "uint32", "int64", "uint64", "float32", "float64", "enum", "interface"}


@dataclass
class _Ctx:
    schema: cs.Schema
    layout: Layout
    res: Resolver

    def is_optional(self, layout_name: str, field: str) -> bool:
        """True when the schema marks ``layout_name.field`` with ``$optional``."""
        if "$" in layout_name:
            return False
        st = self.schema.struct(layout_name)
        return any(f.name == field and field_is_optional(f) for f in st.fields)

    def codec_name_ts(self, struct: str) -> str:
        return f"{self.type_name(struct)}Codec"

    def codec_name_py(self, struct: str) -> str:
        return f"_{_snake(self.type_name(struct))}_codec"

    def type_name(self, layout_name: str) -> str:
        if "$" in layout_name:
            iface, rest = layout_name.split(".", 1)
            method, which = rest.split("$", 1)
            return envelope_type_name(iface, method, which)
        return layout_name

    def struct_dims(self, name: str) -> tuple[int, int]:
        s = self.layout.struct(name)
        return s["dataWords"], s["pointerCount"]

    def enum_table(self, name: str) -> str:
        return enum_table_name(name)


def _ts_write_expr(ctx: _Ctx, ty: dict[str, Any], off: int, value: str, indent: str) -> list[str]:
    kind = ty["kind"]
    if kind == "void":
        return []
    if kind in _DATA_ACCESSORS:
        setter = _DATA_ACCESSORS[kind][0]
        if kind == "uint64":
            return [f"{indent}s.{setter}({off}, BigInt({value}));"]
        return [f"{indent}s.{setter}({off}, {value});"]
    if kind == "text":
        return [f"{indent}s.setText({off}, {value});"]
    if kind == "data":
        return [f"{indent}s.setData({off}, {value});"]
    if kind == "enum":
        return [f'{indent}s.setUint16({off}, ord(A.{ctx.enum_table(ty["name"])}, {value}, "{ty["name"]}"));']
    if kind == "struct":
        dw, pc = ctx.struct_dims(ty["name"])
        return [f"{indent}{ctx.codec_name_ts(ty['name'])}.write(s.initStruct({off}, {dw}, {pc}), {value}, caps);"]
    if kind == "interface":
        return [f"{indent}s.setCap({off}, caps.exportCap({value}));"]
    if kind == "list":
        el = ty["element"]
        ek = el["kind"]
        if ek == "text":
            return [f"{indent}s.setTextList({off}, {value});"]
        if ek == "data":
            return [f"{indent}s.setDataList({off}, {value});"]
        if ek == "bool":
            return [f"{indent}s.setBoolList({off}, {value});"]
        if ek == "struct":
            dw, pc = ctx.struct_dims(el["name"])
            codec = ctx.codec_name_ts(el["name"])
            return [
                f"{indent}{{",
                f"{indent}  const items = s.initStructList({off}, {value}.length, {dw}, {pc});",
                f"{indent}  for (let i = 0; i < items.length; i++) {{",
                f"{indent}    {codec}.write(items[i]!, {value}[i]!, caps);",
                f"{indent}  }}",
                f"{indent}}}",
            ]
        raise SystemExit(f"codec emitter: unsupported list element type {ek}")
    raise SystemExit(f"codec emitter: unsupported field type {kind}")


def _ts_read_expr(ctx: _Ctx, ty: dict[str, Any], off: int) -> str:
    kind = ty["kind"]
    if kind == "void":
        return "undefined"
    if kind in _DATA_ACCESSORS:
        getter = _DATA_ACCESSORS[kind][1]
        if kind == "uint64":
            return f"Number(s.{getter}({off}))"
        return f"s.{getter}({off})"
    if kind == "text":
        return f"s.getText({off})"
    if kind == "data":
        return f"s.getData({off})"
    if kind == "enum":
        return f'fromOrd(A.{ctx.enum_table(ty["name"])}, s.getUint16({off}), "{ty["name"]}")'
    if kind == "struct":
        dw, pc = ctx.struct_dims(ty["name"])
        return f"{ctx.codec_name_ts(ty['name'])}.read(s.getStruct({off}, {dw}, {pc}), caps)"
    if kind == "interface":
        return f"caps.importCap(s.getCapIndex({off})) as T.{ty['name']}"
    if kind == "list":
        el = ty["element"]
        ek = el["kind"]
        if ek == "text":
            return f"s.getTextList({off})"
        if ek == "data":
            return f"s.getDataList({off})"
        if ek == "bool":
            return f"s.getBoolList({off})"
        if ek == "struct":
            dw, pc = ctx.struct_dims(el["name"])
            return f"s.getStructList({off}, {dw}, {pc}).map((item) => {ctx.codec_name_ts(el['name'])}.read(item, caps))"
        raise SystemExit(f"codec emitter: unsupported list element type {ek}")
    raise SystemExit(f"codec emitter: unsupported field type {kind}")


def _field_uses_caps(ty: dict[str, Any]) -> bool:
    """True when encoding this type consults the capability table."""
    kind = ty["kind"]
    if kind in ("struct", "interface"):
        return True
    return kind == "list" and _field_uses_caps(ty["element"])


def _ts_absent_check(ty: dict[str, Any], expr: str) -> str:
    """TS expression that is true when a `$optional` wire value means absent."""
    kind = ty["kind"]
    if kind == "text":
        return f'{expr} === ""'
    if kind == "data":
        return f"{expr}.length === 0"
    if kind == "int64":
        return f"{expr} === 0n"
    return f"{expr} === 0"


def _py_absent_check(ty: dict[str, Any], expr: str) -> str:
    """Python expression that is true when a `$optional` wire value means absent."""
    kind = ty["kind"]
    if kind == "text":
        return f'{expr} == ""'
    if kind == "data":
        return f"len({expr}) == 0"
    return f"{expr} == 0"


def _struct_names_for_codecs(ctx: _Ctx, file: str = "plugin.capnp") -> list[str]:
    names = [s.name for s in ctx.schema.structs]
    for iface in ctx.schema.interfaces:
        for m in sorted(iface.methods, key=lambda m: m.ordinal):
            names.append(envelope_name(iface.name, m.name, "Params"))
            names.append(envelope_name(iface.name, m.name, "Results"))
    return names


def _ts_struct_codec(ctx: _Ctx, layout_name: str) -> list[str]:
    st = ctx.layout.struct(layout_name)
    tname = ctx.type_name(layout_name)
    cname = ctx.codec_name_ts(layout_name)
    dw, pc = st["dataWords"], st["pointerCount"]
    fields = sorted(st["fields"], key=lambda f: f["codeOrder"])
    union = [f for f in fields if f["discriminantValue"] is not None]
    plain = [f for f in fields if f["discriminantValue"] is None]
    is_envelope = "$" in layout_name
    doc = f"Wire codec for `{tname}` ({dw} data words, {pc} pointers)."
    if is_envelope:
        iface, rest = layout_name.split(".", 1)
        method, which = rest.split("$", 1)
        doc = f"Wire codec for the `{iface}.{method}` {which.lower()} envelope ({dw} data words, {pc} pointers)."
    out = ["/**", f" * {doc}", " *", " * @internal", " */"]
    out.append(f"export const {cname}: StructCodec<{'T.' + tname if not is_envelope else tname}> = {{")
    out.append(f"  dataWords: {dw},")
    out.append(f"  pointerCount: {pc},")
    if union and plain:
        raise SystemExit(f"struct {layout_name} mixes union and plain fields")
    uses_caps = any(_field_uses_caps(f["type"]) for f in fields)
    caps_sink = [] if uses_caps else ["    void caps;"]
    if union:
        disc = st["discriminantOffset"]
        out.append("  write(s, v, caps) {")
        out.extend(caps_sink)
        out.append("    switch (v.kind) {")
        for f in union:
            out.append(f'      case "{f["name"]}":')
            out.append(f"        s.setUint16({disc}, {f['discriminantValue']});")
            if f["type"]["kind"] != "void":
                out.extend(_ts_write_expr(ctx, f["type"], f["offset"], "v.value", "        "))
            out.append("        break;")
        out.append("      default:")
        out.append(f'        throw unknownUnion("{tname}", (v as {{ kind: string }}).kind);')
        out.append("    }")
        out.append("  },")
        out.append("  read(s, caps) {")
        out.extend(caps_sink)
        out.append(f"    const disc = s.getUint16({disc});")
        out.append("    switch (disc) {")
        for f in union:
            out.append(f"      case {f['discriminantValue']}:")
            if f["type"]["kind"] == "void":
                out.append(f'        return {{ kind: "{f["name"]}" }};')
            else:
                out.append(
                    f'        return {{ kind: "{f["name"]}", value: {_ts_read_expr(ctx, f["type"], f["offset"])} }};'
                )
        out.append("      default:")
        out.append(f'        throw unknownUnion("{tname}", disc);')
        out.append("    }")
        out.append("  },")
    else:
        out.append("  write(s, v, caps) {")
        body: list[str] = []
        for f in plain:
            if ctx.is_optional(layout_name, f["name"]):
                body.append(f"    if (v.{f['name']} !== undefined) {{")
                body.extend(_ts_write_expr(ctx, f["type"], f["offset"], f"v.{f['name']}", "      "))
                body.append("    }")
            else:
                body.extend(_ts_write_expr(ctx, f["type"], f["offset"], f"v.{f['name']}", "    "))
        if not body:
            out.append("    void s;")
            out.append("    void v;")
        out.extend(caps_sink)
        out.extend(body)
        out.append("  },")
        out.append("  read(s, caps) {")
        out.extend(caps_sink)
        optional = [f for f in plain if ctx.is_optional(layout_name, f["name"])]
        if plain:
            if optional:
                out.append(f"    const out: T.{tname} = {{")
            else:
                out.append("    return {")
            for f in plain:
                if f in optional:
                    continue
                out.append(f"      {f['name']}: {_ts_read_expr(ctx, f['type'], f['offset'])},")
            out.append("    };")
            for f in optional:
                # Field names may be JS reserved words (`default`); the local
                # temporary gets a suffix so the emitted code stays valid.
                local = f"{f['name']}Value"
                out.append(f"    const {local} = {_ts_read_expr(ctx, f['type'], f['offset'])};")
                out.append(f"    if (!({_ts_absent_check(f['type'], local)})) {{")
                out.append(f"      out.{f['name']} = {local};")
                out.append("    }")
            if optional:
                out.append("    return out;")
        else:
            out.append("    void s;")
            out.append("    return {};")
        out.append("  },")
    out.append("};")
    out.append("")
    return out


def _ts_envelope_type(ctx: _Ctx, layout_name: str) -> list[str]:
    """Internal TS interface for a method envelope (params/results struct)."""
    st = ctx.layout.struct(layout_name)
    tname = ctx.type_name(layout_name)
    iface, rest = layout_name.split(".", 1)
    method, which = rest.split("$", 1)
    out = [
        "/**",
        f" * {which} envelope of `{iface}.{method}`.",
        " *",
        " * @internal",
        " */",
        f"export interface {tname} {{",
    ]
    for f in sorted(st["fields"], key=lambda f: f["codeOrder"]):
        out.append(f"  {f['name']}: {_ts_type_from_layout(ctx, f['type'])};")
    out.append("}")
    out.append("")
    return out


def _ts_type_from_layout(ctx: _Ctx, ty: dict[str, Any]) -> str:
    kind = ty["kind"]
    if kind == "list":
        return f"{_ts_type_from_layout(ctx, ty['element'])}[]"
    scalar = {v: k for k, v in _LAYOUT_SCALAR.items()}
    if kind in scalar:
        return _TS_SCALAR[scalar[kind]]
    if kind == "enum":
        return f"A.{ty['name']}"
    return f"T.{ty['name']}"


def emit_ts_wire(capnp_text: str, layout: Layout) -> str:
    """`packages/plugin-sdk/src/generated-wire.ts` — `@internal` Cap'n codecs."""
    schema = cs.parse_schema(capnp_text)
    ctx = _Ctx(schema, layout, Resolver(schema))
    lines = [
        "/**",
        f" * {GENERATED_NOTE}",
        " *",
        " * Unpacked single-segment Cap'n Proto codecs for every struct and method",
        " * envelope in `plugin.capnp`. Layout numbers come from",
        " * `schema/plugin.layout.json` (Cap'n Proto compiler output), not from",
        " * hand-derived tables. Pointer targets are allocated in declaration order,",
        " * depth first, which matches the `capnpc-rust` builder so Rust, TypeScript,",
        " * and Python produce byte-identical messages for the same value.",
        " *",
        " * @internal",
        " * @module",
        " */",
        "",
        'import * as A from "./abi.js";',
        'import type * as T from "./generated.js";',
        "import {",
        "  CapnpMessage,",
        "  CapnpReader,",
        "  NO_CAPS,",
        "  type CapTable,",
        "  type CapnpStruct,",
        "  type StructReader,",
        '} from "./db-capnp.js";',
        "",
        "/**",
        " * Struct codec over the runtime Cap'n builders/readers.",
        " *",
        " * @internal",
        " */",
        "export interface StructCodec<V> {",
        "  /** Data section size in 64-bit words. */",
        "  readonly dataWords: number;",
        "  /** Pointer section size in pointers. */",
        "  readonly pointerCount: number;",
        "  /** Writes `value` into an allocated struct of this codec's size. */",
        "  write(s: CapnpStruct, value: V, caps: CapTable): void;",
        "  /** Reads a value from a struct at least this codec's size. */",
        "  read(s: StructReader, caps: CapTable): V;",
        "}",
        "",
        "/**",
        " * Encodes `value` as a standalone unpacked Cap'n message.",
        " *",
        " * @param codec - Root struct codec.",
        " * @param value - Value to encode.",
        " * @param caps - Transport capability table (required only for interface-typed fields).",
        " * @returns Unpacked Cap'n stream bytes.",
        " * @internal",
        " */",
        "export function encodeMessage<V>(codec: StructCodec<V>, value: V, caps: CapTable = NO_CAPS): Uint8Array {",
        "  const msg = new CapnpMessage();",
        "  codec.write(msg.initRoot(codec.dataWords, codec.pointerCount), value, caps);",
        "  return msg.finish();",
        "}",
        "",
        "/**",
        " * Decodes a standalone unpacked Cap'n message.",
        " *",
        " * @param codec - Root struct codec.",
        " * @param bytes - Unpacked Cap'n stream.",
        " * @param caps - Transport capability table (required only for interface-typed fields).",
        " * @returns Decoded value.",
        " * @internal",
        " */",
        "export function decodeMessage<V>(codec: StructCodec<V>, bytes: Uint8Array, caps: CapTable = NO_CAPS): V {",
        "  return codec.read(new CapnpReader(bytes).root(codec.dataWords, codec.pointerCount), caps);",
        "}",
        "",
        "function ord(table: readonly string[], name: string, enumName: string): number {",
        "  const i = table.indexOf(name);",
        "  if (i < 0) {",
        "    throw new Error(`unknown ${enumName} value: ${name}`);",
        "  }",
        "  return i;",
        "}",
        "",
        "function fromOrd<S extends string>(table: readonly S[], ordinal: number, enumName: string): S {",
        "  const v = table[ordinal];",
        "  if (v === undefined) {",
        "    throw new Error(`unknown ${enumName} ordinal: ${ordinal}`);",
        "  }",
        "  return v;",
        "}",
        "",
        "function unknownUnion(struct: string, member: string | number): Error {",
        "  return new Error(`unknown ${struct} union member: ${String(member)}`);",
        "}",
        "",
    ]
    names = _struct_names_for_codecs(ctx)
    for name in names:
        if "$" in name:
            lines.extend(_ts_envelope_type(ctx, name))
    for name in names:
        lines.extend(_ts_struct_codec(ctx, name))
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Python: _wire.py (codecs)
# ---------------------------------------------------------------------------


def _py_write_expr(ctx: _Ctx, ty: dict[str, Any], off: int, value: str, indent: str) -> list[str]:
    kind = ty["kind"]
    if kind == "void":
        return []
    if kind in _DATA_ACCESSORS:
        return [f"{indent}s.{_DATA_ACCESSORS[kind][2]}({off}, {value})"]
    if kind == "text":
        return [f"{indent}s.set_text({off}, {value})"]
    if kind == "data":
        return [f"{indent}s.set_data({off}, {value})"]
    if kind == "enum":
        return [f'{indent}s.set_u16({off}, _ord(A.{ctx.enum_table(ty["name"])}, {value}, "{ty["name"]}"))']
    if kind == "struct":
        dw, pc = ctx.struct_dims(ty["name"])
        return [f"{indent}{ctx.codec_name_py(ty['name'])}.write(s.init_struct({off}, {dw}, {pc}), {value}, caps)"]
    if kind == "interface":
        return [f"{indent}s.set_cap({off}, caps.export_cap({value}))"]
    if kind == "list":
        el = ty["element"]
        ek = el["kind"]
        if ek == "text":
            return [f"{indent}s.set_text_list({off}, {value})"]
        if ek == "data":
            return [f"{indent}s.set_data_list({off}, {value})"]
        if ek == "bool":
            return [f"{indent}s.set_bool_list({off}, {value})"]
        if ek == "struct":
            dw, pc = ctx.struct_dims(el["name"])
            codec = ctx.codec_name_py(el["name"])
            return [
                f"{indent}items = s.init_struct_list({off}, len({value}), {dw}, {pc})",
                f"{indent}for item, elem in zip(items, {value}, strict=True):",
                f"{indent}    {codec}.write(item, elem, caps)",
            ]
        raise SystemExit(f"codec emitter: unsupported list element type {ek}")
    raise SystemExit(f"codec emitter: unsupported field type {kind}")


def _py_read_expr(ctx: _Ctx, ty: dict[str, Any], off: int) -> str:
    kind = ty["kind"]
    if kind == "void":
        return "None"
    if kind in _DATA_ACCESSORS:
        return f"s.{_DATA_ACCESSORS[kind][3]}({off})"
    if kind == "text":
        return f"s.get_text({off})"
    if kind == "data":
        return f"s.get_data({off})"
    if kind == "enum":
        return f'_from_ord(A.{ctx.enum_table(ty["name"])}, s.get_u16({off}), "{ty["name"]}")'
    if kind == "struct":
        dw, pc = ctx.struct_dims(ty["name"])
        return f"{ctx.codec_name_py(ty['name'])}.read(s.get_struct({off}, {dw}, {pc}), caps)"
    if kind == "interface":
        return f"caps.import_cap(s.get_cap_index({off}))"
    if kind == "list":
        el = ty["element"]
        ek = el["kind"]
        if ek == "text":
            return f"s.get_text_list({off})"
        if ek == "data":
            return f"s.get_data_list({off})"
        if ek == "bool":
            return f"s.get_bool_list({off})"
        if ek == "struct":
            dw, pc = ctx.struct_dims(el["name"])
            return f"[{ctx.codec_name_py(el['name'])}.read(item, caps) for item in s.get_struct_list({off}, {dw}, {pc})]"
        raise SystemExit(f"codec emitter: unsupported list element type {ek}")
    raise SystemExit(f"codec emitter: unsupported field type {kind}")


def _py_struct_codec(ctx: _Ctx, layout_name: str) -> list[str]:
    st = ctx.layout.struct(layout_name)
    tname = ctx.type_name(layout_name)
    cname = ctx.codec_name_py(layout_name)
    dw, pc = st["dataWords"], st["pointerCount"]
    fields = sorted(st["fields"], key=lambda f: f["codeOrder"])
    union = [f for f in fields if f["discriminantValue"] is not None]
    plain = [f for f in fields if f["discriminantValue"] is None]
    if union and plain:
        raise SystemExit(f"struct {layout_name} mixes union and plain fields")
    wname = f"_write_{_snake(tname)}"
    rname = f"_read_{_snake(tname)}"
    out: list[str] = []
    out.append(f"def {wname}(s: _CapnpStruct, v: Any, caps: _CapTable) -> None:")
    if union:
        disc = st["discriminantOffset"]
        out.append('    kind = v["kind"]')
        first = True
        for f in union:
            kw = "if" if first else "elif"
            first = False
            out.append(f'    {kw} kind == "{f["name"]}":')
            out.append(f"        s.set_u16({disc}, {f['discriminantValue']})")
            if f["type"]["kind"] != "void":
                out.extend(_py_write_expr(ctx, f["type"], f["offset"], 'v["value"]', "        "))
        out.append("    else:")
        out.append(f'        raise ValueError(f"unknown {tname} union member: {{kind}}")')
    else:
        body: list[str] = []
        for f in plain:
            if ctx.is_optional(layout_name, f["name"]):
                body.append(f'    if v.get("{f["name"]}") is not None:')
                body.extend(
                    _py_write_expr(ctx, f["type"], f["offset"], f'v["{f["name"]}"]', "        ")
                )
            else:
                body.extend(
                    _py_write_expr(ctx, f["type"], f["offset"], f'v["{f["name"]}"]', "    ")
                )
        out.extend(body or ["    pass"])
    out.append("")
    out.append("")
    out.append(f"def {rname}(s: _StructReader, caps: _CapTable) -> Any:")
    if union:
        disc = st["discriminantOffset"]
        out.append(f"    disc = s.get_u16({disc})")
        first = True
        for f in union:
            kw = "if" if first else "elif"
            first = False
            out.append(f"    {kw} disc == {f['discriminantValue']}:")
            if f["type"]["kind"] == "void":
                out.append(f'        return {{"kind": "{f["name"]}"}}')
            else:
                out.append(
                    f'        return {{"kind": "{f["name"]}", "value": {_py_read_expr(ctx, f["type"], f["offset"])}}}'
                )
        out.append(f'    raise ValueError(f"unknown {tname} union member: {{disc}}")')
    else:
        optional = [f for f in plain if ctx.is_optional(layout_name, f["name"])]
        if not plain:
            out.append("    return {}")
        else:
            out.append("    out: dict[str, Any] = {" if optional else "    return {")
            for f in plain:
                if f in optional:
                    continue
                out.append(f'        "{f["name"]}": {_py_read_expr(ctx, f["type"], f["offset"])},')
            out.append("    }")
            for f in optional:
                out.append(f"    {_snake(f['name'])} = {_py_read_expr(ctx, f['type'], f['offset'])}")
                out.append(f"    if not ({_py_absent_check(f['type'], _snake(f['name']))}):")
                out.append(f'        out["{f["name"]}"] = {_snake(f["name"])}')
            if optional:
                out.append("    return out")
    out.append("")
    out.append("")
    out.append(f"{cname} = _Codec({dw}, {pc}, {wname}, {rname})")
    out.append(f'"""Wire codec for ``{tname}`` ({dw} data words, {pc} pointers)."""')
    out.append("")
    out.append("")
    return out


def emit_py_wire(capnp_text: str, layout: Layout) -> str:
    """`packages/plugin-sdk-python/.../_wire.py` — private Cap'n codecs."""
    schema = cs.parse_schema(capnp_text)
    ctx = _Ctx(schema, layout, Resolver(schema))
    lines = [
        f'"""{GENERATED_NOTE}',
        "",
        "Unpacked single-segment Cap'n Proto codecs for every struct and method",
        "envelope in ``plugin.capnp``. Layout numbers come from",
        "``schema/plugin.layout.json`` (Cap'n Proto compiler output), not from",
        "hand-derived tables. Pointer targets are allocated in declaration order,",
        "depth first, matching the ``capnpc-rust`` builder so Rust, TypeScript, and",
        "Python produce byte-identical messages for the same value.",
        '"""',
        "",
        "from __future__ import annotations",
        "",
        "from typing import Any",
        "",
        "from . import _abi as A",
        "from ._capnp import (",
        "    _CapnpMessage,",
        "    _CapnpReader,",
        "    _CapnpStruct,",
        "    _CapTable,",
        "    _StructReader,",
        "    NO_CAPS,",
        ")",
        "",
        "",
        "class _Codec:",
        '    """Struct codec over the runtime Cap\'n builders/readers."""',
        "",
        "    def __init__(self, data_words: int, pointer_count: int, write: Any, read: Any) -> None:",
        "        self.data_words = data_words",
        "        self.pointer_count = pointer_count",
        "        self.write = write",
        "        self.read = read",
        "",
        "",
        "def _encode_message(codec: _Codec, value: Any, caps: _CapTable = NO_CAPS) -> bytes:",
        '    """Encode ``value`` as a standalone unpacked Cap\'n message."""',
        "    msg = _CapnpMessage()",
        "    codec.write(msg.init_root(codec.data_words, codec.pointer_count), value, caps)",
        "    return msg.finish()",
        "",
        "",
        "def _decode_message(codec: _Codec, data: bytes, caps: _CapTable = NO_CAPS) -> Any:",
        '    """Decode a standalone unpacked Cap\'n message."""',
        "    return codec.read(_CapnpReader(data).root(codec.data_words, codec.pointer_count), caps)",
        "",
        "",
        "def _ord(table: tuple[str, ...], name: str, enum_name: str) -> int:",
        "    try:",
        "        return table.index(name)",
        "    except ValueError as exc:",
        '        raise ValueError(f"unknown {enum_name} value: {name}") from exc',
        "",
        "",
        "def _from_ord(table: tuple[str, ...], ordinal: int, enum_name: str) -> str:",
        "    if ordinal < 0 or ordinal >= len(table):",
        '        raise ValueError(f"unknown {enum_name} ordinal: {ordinal}")',
        "    return table[ordinal]",
        "",
        "",
    ]
    for name in _struct_names_for_codecs(ctx):
        lines.extend(_py_struct_codec(ctx, name))
    body = "\n".join(lines).rstrip() + "\n"
    return re.sub(r"\n{4,}", "\n\n\n", body)


# ---------------------------------------------------------------------------
# Rust: crates/bookclerk-plugin-abi/src/generated.rs (DTOs + Cap'n codecs)
# ---------------------------------------------------------------------------

_RS_SCALAR = {
    "Bool": "bool",
    "Int8": "i8",
    "Int16": "i16",
    "Int32": "i32",
    "Int64": "i64",
    "UInt8": "u8",
    "UInt16": "u16",
    "UInt32": "u32",
    "UInt64": "u64",
    "Float32": "f32",
    "Float64": "f64",
    "Text": "String",
    "Data": "Vec<u8>",
}

_RS_FLOATS = frozenset({"Float32", "Float64"})

# Hand-written Rust projections the generated section may reference:
# name -> (Rust type, writer fn, reader fn). Writers take `(Builder, &T)` and
# return `()`; readers take a `Reader` and return `T` infallibly.
_RS_EXTERNAL = {
    "PluginError": ("crate::PluginError", "crate::rpc::write_error", "crate::rpc::read_error"),
    "ExtensibleConfig": (
        "crate::ExtensibleConfig",
        "crate::rpc::write_extensible_config",
        "crate::rpc::read_extensible_config",
    ),
}

_RS_KEYWORDS = frozenset(
    "as break const continue crate else enum extern false fn for if impl in let loop match mod "
    "move mut pub ref return self Self static struct super trait true type unsafe use where while "
    "async await dyn abstract become box do final macro override priv typeof unsized virtual yield "
    "try gen".split()
)


def _rs_field(name: str) -> str:
    """Rust field identifier for a Cap'n field (raw identifier for keywords)."""
    snake = _snake(name)
    return f"r#{snake}" if snake in _RS_KEYWORDS else snake


def _rs_accessor(name: str) -> str:
    """Suffix capnpc-rust uses for `get_`/`set_`/`init_` accessors of a field."""
    out: list[str] = []
    for i, ch in enumerate(name):
        if ch.isupper() and i > 0:
            out.append("_")
        out.append(ch.lower())
    return "".join(out)


def _rs_module(struct: str) -> str:
    """capnpc-rust module name of a struct (`LoginParams` -> `login_params`)."""
    name = _rs_accessor(struct)
    return f"{name}_" if name in _RS_KEYWORDS else name


def _rs_doc(doc: list[str], indent: str = "") -> list[str]:
    """Rustdoc lines; square brackets are escaped so config-table names such as
    `[sources.<id>]` are not parsed as intra-doc links."""
    out: list[str] = []
    for line in doc:
        escaped = line.replace("[", "\\[").replace("]", "\\]")
        out.append(f"{indent}/// {escaped}".rstrip())
    return out


class _RsCtx:
    """Resolution state for the Rust emitter."""

    def __init__(self, schema: cs.Schema, decls: list[cs.Decl]) -> None:
        self.schema = schema
        self.res = Resolver(schema)
        self.structs = {d.name: d for d in decls if isinstance(d, cs.Struct)}
        self.enums = {d.name: d for d in decls if isinstance(d, cs.Enum)}
        self._eq_memo: dict[str, bool] = {}

    def is_result_union(self, st: cs.Struct) -> bool:
        """`{ ok :T; err :PluginError }` unions project to `Result<T, PluginError>`."""
        if not st.has_union or st.plain_fields or len(st.union_fields) != 2:
            return False
        ok, err = st.union_fields
        return ok.name == "ok" and err.name == "err" and err.type.name == "PluginError"

    def rust_type(self, ty: cs.TypeRef) -> str:
        if ty.inner is not None:
            return f"Vec<{self.rust_type(ty.inner)}>"
        if ty.name in _RS_SCALAR:
            return _RS_SCALAR[ty.name]
        name = self.res.canonical(ty.name)
        if name in _RS_EXTERNAL:
            return _RS_EXTERNAL[name][0]
        if name in self.structs:
            st = self.structs[name]
            if self.is_result_union(st):
                ok = st.union_fields[0]
                return f"Result<{self.rust_type(ok.type)}, crate::PluginError>"
            return name
        if name in self.enums:
            return name
        raise SystemExit(
            f"rust emitter: `{ty.name}` is referenced from the rust-generated section but is "
            "neither declared there nor listed in _RS_EXTERNAL"
        )

    def field_type(self, f: cs.Field) -> str:
        inner = self.rust_type(f.type)
        return f"Option<{inner}>" if field_is_optional(f) else inner

    def is_eq(self, name: str) -> bool:
        """True when the Rust projection can derive `Eq` (no floats reachable)."""
        if name in self._eq_memo:
            return self._eq_memo[name]
        self._eq_memo[name] = True  # cycles are impossible in Cap'n structs by value
        if name in self.enums or name in _RS_EXTERNAL:
            return True
        st = self.structs[name]
        ok = all(self._type_eq(f.type) for f in st.fields)
        self._eq_memo[name] = ok
        return ok

    def _type_eq(self, ty: cs.TypeRef) -> bool:
        if ty.inner is not None:
            return self._type_eq(ty.inner)
        if ty.name in _RS_FLOATS:
            return False
        if ty.name in _RS_SCALAR:
            return True
        return self.is_eq(self.res.canonical(ty.name))


def _rs_write_field(ctx: _RsCtx, ty: cs.TypeRef, acc: str, value: str, indent: str) -> list[str]:
    """Statements writing `value` (an expression of the field's Rust type) into `b`."""
    kind = ctx.res.kind_of(ty)
    if ty.inner is not None:
        el = ty.inner
        ek = ctx.res.kind_of(el)
        lines = [
            f"{indent}{{",
            f"{indent}    let mut items = b.reborrow().init_{acc}(list_len({value}.len())?);",
            f"{indent}    for (i, item) in {value}.iter().enumerate() {{",
        ]
        if ek in ("Text", "Data"):
            lines.append(f"{indent}        items.set(list_len(i)?, item);")
        elif ek == "struct":
            lines.append(
                f"{indent}        {_rs_write_call(ctx, el, 'items.reborrow().get(list_len(i)?)', 'item')}"
            )
        else:
            raise SystemExit(f"rust emitter: unsupported list element {el.render()}")
        lines.extend([f"{indent}    }}", f"{indent}}}"])
        return lines
    if kind in ("Text", "Data"):
        return [f"{indent}b.set_{acc}({value});"]
    if kind in _RS_SCALAR:
        return [f"{indent}b.set_{acc}({value});"]
    if kind == "enum":
        return [f"{indent}b.set_{acc}((*{value}).into());"]
    if kind == "struct":
        return [f"{indent}{_rs_write_call(ctx, ty, f'b.reborrow().init_{acc}()', value)}"]
    raise SystemExit(f"rust emitter: unsupported field type {ty.render()}")


def _rs_write_call(ctx: _RsCtx, ty: cs.TypeRef, builder: str, value: str) -> str:
    """Statement encoding struct `value` into `builder` (external writers are infallible)."""
    name = ctx.res.canonical(ty.name)
    if name in _RS_EXTERNAL:
        return f"{_RS_EXTERNAL[name][1]}({builder}, {value});"
    return f"write_{_snake(name)}({builder}, {value})?;"


def _rs_read_call(ctx: _RsCtx, ty: cs.TypeRef, reader: str) -> str:
    """Expression decoding a struct from `reader` (a `capnp::Result<Reader>`)."""
    name = ctx.res.canonical(ty.name)
    if name in _RS_EXTERNAL:
        return f"{_RS_EXTERNAL[name][2]}({reader}?)"
    return f"read_{_snake(name)}({reader}?)?"


def _rs_read_map(ctx: _RsCtx, ty: cs.TypeRef) -> str:
    """Closure mapping struct-list items (plain `Reader`s) to `capnp::Result<T>`."""
    name = ctx.res.canonical(ty.name)
    if name in _RS_EXTERNAL:
        return f"|item| Ok({_RS_EXTERNAL[name][2]}(item))"
    return f"|item| read_{_snake(name)}(item)"


def _rs_read_expr(ctx: _RsCtx, f: cs.Field) -> str:
    """Expression producing the field's Rust value from reader `r`."""
    ty = f.type
    acc = _rs_accessor(f.name)
    optional = field_is_optional(f)
    if ty.inner is not None:
        el = ty.inner
        ek = ctx.res.kind_of(el)
        if ek == "Text":
            return f"read_text_list(r.get_{acc}()?)?"
        if ek == "Data":
            return f"r.get_{acc}()?.iter().map(|d| d.map(<[u8]>::to_vec)).collect::<capnp::Result<Vec<_>>>()?"
        if ek == "struct":
            return f"r.get_{acc}()?.iter().map({_rs_read_map(ctx, el)}).collect::<capnp::Result<Vec<_>>>()?"
        raise SystemExit(f"rust emitter: unsupported list element {el.render()}")
    kind = ctx.res.kind_of(ty)
    if kind == "Text":
        return f"opt_text(r.get_{acc}()?)?" if optional else f"r.get_{acc}()?.to_string()?"
    if kind == "Data":
        return f"opt_data(r.get_{acc}()?)" if optional else f"r.get_{acc}()?.to_vec()"
    if kind in _RS_SCALAR:
        return f"opt_num(r.get_{acc}())" if optional else f"r.get_{acc}()"
    if kind == "enum":
        return f"r.get_{acc}()?.into()"
    if kind == "struct":
        return _rs_read_call(ctx, ty, f"r.get_{acc}()")
    raise SystemExit(f"rust emitter: unsupported field type {ty.render()}")


def _rs_enum(ctx: _RsCtx, en: cs.Enum) -> list[str]:
    names = en.wire_names()
    variants = [_pascal(n) for n in names]
    rename = "snake_case" if is_text_enum(en) else "camelCase"
    out = _rs_doc(en.doc)
    out.append("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]")
    out.append(f'#[serde(rename_all = "{rename}")]')
    out.append(f"pub enum {en.name} {{")
    members = sorted(en.enumerants, key=lambda e: e.ordinal)
    for i, (m, v) in enumerate(zip(members, variants, strict=True)):
        out.extend(_rs_doc(m.doc, "    "))
        if i == 0:
            out.append("    #[default]")
        out.append(f"    {v},")
    out.append("}")
    out.append("")
    wire = enum_wire_values(en)
    out.append(f"impl {en.name} {{")
    out.append("    /// Wire name of this enumerant (matches the TypeScript / Python SDK literal).")
    out.append("    #[must_use]")
    out.append("    pub fn wire_name(self) -> &'static str {")
    out.append("        match self {")
    for v, w in zip(variants, wire, strict=True):
        out.append(f'            Self::{v} => "{w}",')
    out.append("        }")
    out.append("    }")
    out.append("")
    out.append("    /// Parse a wire name; `None` for unknown strings.")
    out.append("    #[must_use]")
    out.append("    pub fn from_wire_name(name: &str) -> Option<Self> {")
    out.append("        match name {")
    for v, w in zip(variants, wire, strict=True):
        out.append(f'            "{w}" => Some(Self::{v}),')
    out.append("            _ => None,")
    out.append("        }")
    out.append("    }")
    out.append("}")
    out.append("")
    out.append(f"impl From<{en.name}> for plugin_capnp::{en.name} {{")
    out.append(f"    fn from(value: {en.name}) -> Self {{")
    out.append("        match value {")
    for v in variants:
        out.append(f"            {en.name}::{v} => Self::{v},")
    out.append("        }")
    out.append("    }")
    out.append("}")
    out.append("")
    out.append(f"impl From<plugin_capnp::{en.name}> for {en.name} {{")
    out.append(f"    fn from(value: plugin_capnp::{en.name}) -> Self {{")
    out.append("        match value {")
    for v in variants:
        out.append(f"            plugin_capnp::{en.name}::{v} => Self::{v},")
    out.append("        }")
    out.append("    }")
    out.append("}")
    out.append("")
    return out


def _rs_struct_type(ctx: _RsCtx, st: cs.Struct) -> list[str]:
    derives = ["Debug", "Clone", "PartialEq"]
    if ctx.is_eq(st.name):
        derives.append("Eq")
    derives.extend(["Default", "Serialize", "Deserialize"])
    out = _rs_doc(st.doc)
    out.append(f"#[derive({', '.join(derives)})]")
    out.append('#[serde(rename_all = "camelCase")]')
    out.append(f"pub struct {st.name} {{")
    for f in st.fields:
        fdoc = list(f.doc)
        if field_is_optional(f):
            fdoc.append("`None` when absent (wire zero value).")
        out.extend(_rs_doc(fdoc, "    "))
        # JSON projections carry Cap'n `Data` as base64 text (never number
        # arrays) so the transport-private JSON bridges stay compact.
        is_data = f.type.inner is None and f.type.name == "Data"
        if field_is_optional(f):
            attrs = ["default", 'skip_serializing_if = "Option::is_none"']
            if is_data:
                attrs.append('with = "crate::json_bytes::opt_b64"')
        else:
            attrs = ["default"]
            if is_data:
                attrs.append('with = "crate::json_bytes::b64"')
        out.append(f"    #[serde({', '.join(attrs)})]")
        out.append(f"    pub {_rs_field(f.name)}: {ctx.field_type(f)},")
    out.append("}")
    out.append("")
    return out


def _rs_union_type(ctx: _RsCtx, st: cs.Struct) -> list[str]:
    out = _rs_doc(st.doc)
    derives = ["Debug", "Clone", "PartialEq"]
    if ctx.is_eq(st.name):
        derives.append("Eq")
    derives.extend(["Serialize", "Deserialize"])
    out.append(f"#[derive({', '.join(derives)})]")
    out.append('#[serde(tag = "kind", content = "value", rename_all = "camelCase")]')
    out.append(f"pub enum {st.name} {{")
    for f in st.union_fields:
        out.extend(_rs_doc(f.doc, "    "))
        if f.type.name == "Void" and f.type.inner is None:
            out.append(f"    {_pascal(f.name)},")
        else:
            out.append(f"    {_pascal(f.name)}({ctx.rust_type(f.type)}),")
    out.append("}")
    out.append("")
    return out


def _rs_struct_codec(ctx: _RsCtx, st: cs.Struct) -> list[str]:
    module = _rs_module(st.name)
    snake = _snake(st.name)
    out: list[str] = []
    if ctx.is_result_union(st):
        ok = st.union_fields[0]
        ok_ty = ctx.rust_type(ok.type)
        rs_ty = f"Result<{ok_ty}, crate::PluginError>"
        out.append(f"/// Encode a `{st.name}` union from a typed result.")
        out.append("///")
        out.append("/// # Errors")
        out.append("///")
        out.append("/// Returns a Cap'n Proto error when a nested field cannot be encoded.")
        out.append(
            f"pub fn write_{snake}(b: plugin_capnp::{module}::Builder<'_>, v: &{rs_ty}) -> capnp::Result<()> {{"
        )
        out.append("    match v {")
        ok_kind = ctx.res.kind_of(ok.type)
        if ok_kind == "struct":
            out.append(f"        Ok(ok) => write_{_snake(ctx.res.canonical(ok.type.name))}(b.init_ok(), ok),")
        else:
            raise SystemExit(f"rust emitter: result union {st.name} needs a struct `ok` member")
        out.append("        Err(err) => {")
        out.append("            crate::rpc::write_error(b.init_err(), err);")
        out.append("            Ok(())")
        out.append("        }")
        out.append("    }")
        out.append("}")
        out.append("")
        out.append(f"/// Decode a `{st.name}` union into a typed result.")
        out.append("///")
        out.append("/// # Errors")
        out.append("///")
        out.append("/// Returns a Cap'n Proto error when the union is unset or a field is malformed.")
        out.append(
            f"pub fn read_{snake}(r: plugin_capnp::{module}::Reader<'_>) -> capnp::Result<{rs_ty}> {{"
        )
        out.append("    match r.which()? {")
        out.append(f"        plugin_capnp::{module}::Ok(ok) => Ok(Ok({_rs_read_call(ctx, ok.type, 'ok')})),")
        out.append(f"        plugin_capnp::{module}::Err(err) => Ok(Err(crate::rpc::read_error(err?))),")
        out.append("    }")
        out.append("}")
        out.append("")
        return out
    if st.has_union:
        out.append(f"/// Encode a `{st.name}` union.")
        out.append("///")
        out.append("/// # Errors")
        out.append("///")
        out.append("/// Returns a Cap'n Proto error when a nested field cannot be encoded.")
        out.append(
            f"pub fn write_{snake}(mut b: plugin_capnp::{module}::Builder<'_>, v: &{st.name}) -> capnp::Result<()> {{"
        )
        out.append("    match v {")
        for f in st.union_fields:
            acc = _rs_accessor(f.name)
            variant = _pascal(f.name)
            if f.type.name == "Void" and f.type.inner is None:
                out.append(f"        {st.name}::{variant} => b.set_{acc}(()),")
                continue
            kind = ctx.res.kind_of(f.type)
            if kind == "struct":
                out.append(f"        {st.name}::{variant}(value) => {{")
                out.append(f"            {_rs_write_call(ctx, f.type, f'b.init_{acc}()', 'value')}")
                out.append("        }")
            else:
                out.append(f"        {st.name}::{variant}(value) => {{")
                out.extend(_rs_write_field(ctx, f.type, acc, "value", "            "))
                out.append("        }")
        out.append("    }")
        out.append("    Ok(())")
        out.append("}")
        out.append("")
        out.append(f"/// Decode a `{st.name}` union.")
        out.append("///")
        out.append("/// # Errors")
        out.append("///")
        out.append("/// Returns a Cap'n Proto error when the union is unset or a field is malformed.")
        out.append(
            f"pub fn read_{snake}(r: plugin_capnp::{module}::Reader<'_>) -> capnp::Result<{st.name}> {{"
        )
        out.append("    Ok(match r.which()? {")
        for f in st.union_fields:
            variant = _pascal(f.name)
            if f.type.name == "Void" and f.type.inner is None:
                out.append(f"        plugin_capnp::{module}::{variant}(()) => {st.name}::{variant},")
                continue
            kind = ctx.res.kind_of(f.type)
            if kind == "struct":
                out.append(
                    f"        plugin_capnp::{module}::{variant}(value) => {st.name}::{variant}({_rs_read_call(ctx, f.type, 'value')}),"
                )
            elif kind == "Text":
                out.append(
                    f"        plugin_capnp::{module}::{variant}(value) => {st.name}::{variant}(value?.to_string()?),"
                )
            elif kind == "Data":
                out.append(
                    f"        plugin_capnp::{module}::{variant}(value) => {st.name}::{variant}(value?.to_vec()),"
                )
            elif kind == "enum":
                out.append(
                    f"        plugin_capnp::{module}::{variant}(value) => {st.name}::{variant}(value?.into()),"
                )
            elif kind in _RS_SCALAR:
                out.append(f"        plugin_capnp::{module}::{variant}(value) => {st.name}::{variant}(value),")
            else:
                raise SystemExit(f"rust emitter: unsupported union member {st.name}.{f.name}")
        out.append("    })")
        out.append("}")
        out.append("")
        return out
    out.append(f"/// Encode a [`{st.name}`] onto a Cap'n Proto builder.")
    out.append("///")
    out.append("/// # Errors")
    out.append("///")
    out.append("/// Returns a Cap'n Proto error when a list is too long or a nested field cannot be encoded.")
    out.append(
        f"pub fn write_{snake}(mut b: plugin_capnp::{module}::Builder<'_>, v: &{st.name}) -> capnp::Result<()> {{"
    )
    for f in st.fields:
        acc = _rs_accessor(f.name)
        field = f"v.{_rs_field(f.name)}"
        if field_is_optional(f):
            kind = ctx.res.kind_of(f.type)
            if kind in ("Text", "Data"):
                out.append(f"    if let Some(value) = &{field} {{")
                out.append(f"        b.set_{acc}(value);")
            else:
                out.append(f"    if let Some(value) = {field} {{")
                out.append(f"        b.set_{acc}(value);")
            out.append("    }")
            continue
        kind = ctx.res.kind_of(f.type)
        if kind in _RS_SCALAR and f.type.inner is None and kind not in ("Text", "Data"):
            out.append(f"    b.set_{acc}({field});")
        elif kind in ("Text", "Data"):
            out.append(f"    b.set_{acc}(&{field});")
        elif kind == "enum":
            out.append(f"    b.set_{acc}({field}.into());")
        elif kind == "struct":
            out.append(f"    {_rs_write_call(ctx, f.type, f'b.reborrow().init_{acc}()', f'&{field}')}")
        else:
            out.extend(_rs_write_field(ctx, f.type, acc, field, "    "))
    out.append("    Ok(())")
    out.append("}")
    out.append("")
    out.append(f"/// Decode a [`{st.name}`] from a Cap'n Proto reader.")
    out.append("///")
    out.append("/// # Errors")
    out.append("///")
    out.append("/// Returns a Cap'n Proto error when a text field is not UTF-8 or a pointer is malformed.")
    out.append(
        f"pub fn read_{snake}(r: plugin_capnp::{module}::Reader<'_>) -> capnp::Result<{st.name}> {{"
    )
    out.append(f"    Ok({st.name} {{")
    for f in st.fields:
        out.append(f"        {_rs_field(f.name)}: {_rs_read_expr(ctx, f)},")
    out.append("    })")
    out.append("}")
    out.append("")
    return out


def emit_rust_generated(capnp_text: str) -> str:
    """`crates/bookclerk-plugin-abi/src/generated.rs` — typed DTOs and Cap'n codecs.

    Covers every struct and enum between the ``rust-generated`` markers in
    ``plugin.capnp``. Plain structs become owned Rust structs (``$optional``
    scalars are ``Option<T>``); ``{ ok, err :PluginError }`` unions become
    ``Result<T, PluginError>`` codec pairs; other unions become Rust enums.
    Serde derives keep the JSON bridge to the workerd isolate typed on both
    ends until that transport is Cap'n bytes.
    """
    schema = cs.parse_schema(capnp_text)
    decls = rust_generated_decls(capnp_text)
    ctx = _RsCtx(schema, decls)
    lines = [
        f"//! {GENERATED_NOTE}",
        "//!",
        "//! Rust projection of the `rust-generated` section of `plugin.capnp`: owned",
        "//! DTOs for every typed method payload plus `write_*` / `read_*` codecs over",
        "//! the capnpc builders and readers. `$optional` scalars are `Option<T>`",
        "//! (wire zero value = `None`); `{ ok, err }` reply unions are",
        "//! `Result<T, PluginError>`.",
        "",
        "#![allow(clippy::too_many_lines)]",
        "",
        "use serde::{Deserialize, Serialize};",
        "",
        "use crate::plugin_capnp;",
        "",
        "/// Cap'n Proto list lengths are `u32`.",
        "///",
        "/// # Errors",
        "///",
        "/// Returns when `len` exceeds `u32::MAX`.",
        "fn list_len(len: usize) -> capnp::Result<u32> {",
        "    u32::try_from(len).map_err(|_| capnp::Error::failed(format!(\"list length {len} exceeds UInt32\")))",
        "}",
        "",
        "/// `$optional` text: empty means absent.",
        "///",
        "/// # Errors",
        "///",
        "/// Returns when the Cap'n Proto text is not valid UTF-8.",
        "fn opt_text(t: capnp::text::Reader<'_>) -> capnp::Result<Option<String>> {",
        "    let s = t.to_str()?;",
        "    Ok(if s.is_empty() { None } else { Some(s.to_owned()) })",
        "}",
        "",
        "/// `$optional` data: empty means absent.",
        "fn opt_data(d: &[u8]) -> Option<Vec<u8>> {",
        "    if d.is_empty() {",
        "        None",
        "    } else {",
        "        Some(d.to_vec())",
        "    }",
        "}",
        "",
        "/// `$optional` number: zero means absent.",
        "fn opt_num<T: Default + PartialEq>(v: T) -> Option<T> {",
        "    if v == T::default() {",
        "        None",
        "    } else {",
        "        Some(v)",
        "    }",
        "}",
        "",
        "/// Owned strings of a `List(Text)`.",
        "///",
        "/// # Errors",
        "///",
        "/// Returns when an element is not valid UTF-8.",
        "fn read_text_list(list: capnp::text_list::Reader<'_>) -> capnp::Result<Vec<String>> {",
        "    list.iter().map(|t| Ok(t?.to_string()?)).collect()",
        "}",
        "",
    ]
    for d in decls:
        if isinstance(d, cs.Enum):
            lines.extend(_rs_enum(ctx, d))
    for d in decls:
        if isinstance(d, cs.Struct):
            if ctx.is_result_union(d):
                continue
            if d.has_union:
                if d.plain_fields:
                    raise SystemExit(f"struct {d.name} mixes union and plain fields")
                lines.extend(_rs_union_type(ctx, d))
            else:
                lines.extend(_rs_struct_type(ctx, d))
    for d in decls:
        if isinstance(d, cs.Struct):
            lines.extend(_rs_struct_codec(ctx, d))
    return _rustfmt("\n".join(lines).rstrip() + "\n")


def _rustfmt(source: str) -> str:
    """Format generated Rust with the pinned toolchain's ``rustfmt``.

    The drift check compares byte-for-byte, so the checked-in file must be
    exactly what ``cargo fmt`` would accept. ``rust-toolchain.toml`` pins the
    channel, which keeps the output deterministic across machines.
    """
    import subprocess

    try:
        proc = subprocess.run(
            ["rustfmt", "--edition", "2021", "--emit", "stdout"],
            input=source,
            capture_output=True,
            text=True,
            check=False,
        )
    except FileNotFoundError as exc:
        raise SystemExit("rustfmt is required to generate generated.rs") from exc
    if proc.returncode != 0:
        raise SystemExit(f"rustfmt failed on generated Rust:\n{proc.stderr}")
    return proc.stdout
