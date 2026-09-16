"""SDK code emitters for scripts/gen-plugin-abi.py.

Generates the TypeScript and Python SDK projections from the single ABI
source of truth: ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.

Three kinds of declarations are projected:

- File-scope ``const`` values (versions, limits, feature names).
- Database enums (``DbType`` / ``DbStatementKind`` / ``DbResultSelection``)
  as ordinal-ordered wire-name tables.
- The "JSON payload contracts" section: structs and ``$jsonEnum`` enums that
  describe the JSON payloads carried inside ``Text`` fields of the ABI
  (``describe().metadataJson``, role ``paramsJson``, ``cliInvoke``). Doc
  comments and the ``$required`` / ``$jsonValue`` annotations drive the
  emitted types.

Emitted artifacts (all carry a GENERATED header; edit the schema and re-run
``scripts/gen-plugin-abi.py --write`` instead of editing them):

- ``packages/plugin-sdk/src/abi.ts`` (constants + database enum tables)
- ``packages/plugin-sdk/src/generated.ts`` (JSON payload DTO interfaces)
- ``packages/plugin-sdk-python/src/bookclerk_plugin_sdk/_abi.py``
- ``packages/plugin-sdk-python/src/bookclerk_plugin_sdk/abi.py``

Plus in-place constant / error-code rewrites for
``packages/plugin-sdk/embed/bookclerk_plugin.js``.
"""

from __future__ import annotations

import re
import textwrap
from dataclasses import dataclass, field

GENERATED_NOTE = (
    "GENERATED FILE - do not edit. Run `python3 scripts/gen-plugin-abi.py --write` "
    "after changing crates/bookclerk-plugin-abi/schema/plugin.capnp."
)

JSON_SECTION_BEGIN = "# JSON payload contracts"
JSON_SECTION_END = "# End of JSON payload contracts"

# ---------------------------------------------------------------------------
# plugin.capnp parsing
# ---------------------------------------------------------------------------

_CAPNP_CONST_RE = re.compile(
    r'^const (\w+) :(UInt32|Text) = (?:(\d+)|"([^"]*)");', re.MULTILINE
)


def parse_capnp_constants(text: str) -> dict[str, int | str]:
    """File-scope ``const`` declarations, camelCase name -> value."""
    out: dict[str, int | str] = {}
    for name, ty, num, txt in _CAPNP_CONST_RE.findall(text):
        out[name] = int(num) if ty == "UInt32" else txt
    if not out:
        raise SystemExit("no file-scope constants found in plugin.capnp")
    return out


@dataclass
class CapnpField:
    """One struct field in the JSON payload section."""

    name: str
    type: str
    required: bool = False
    json_value: bool = False
    doc: list[str] = field(default_factory=list)


@dataclass
class CapnpStruct:
    """One JSON payload DTO struct."""

    name: str
    doc: list[str] = field(default_factory=list)
    fields: list[CapnpField] = field(default_factory=list)


@dataclass
class CapnpEnum:
    """One enum declaration (ordinal-ordered enumerants)."""

    name: str
    enumerants: list[str] = field(default_factory=list)
    json_enum: bool = False
    doc: list[str] = field(default_factory=list)


@dataclass
class CapnpAlias:
    """A ``using Alias = Target;`` declaration."""

    name: str
    target: str
    doc: list[str] = field(default_factory=list)


@dataclass
class JsonSection:
    """Parsed contents of the JSON payload contracts section, in file order."""

    decls: list[CapnpStruct | CapnpEnum | CapnpAlias] = field(default_factory=list)


_FIELD_RE = re.compile(
    r"^  (\w+) @\d+ :([\w()]+)((?:\s+\$\w+)*);$",
)
_ENUMERANT_RE = re.compile(r"^  (\w+) @(\d+);$")


def _flush_doc(doc: list[str]) -> list[str]:
    out = list(doc)
    doc.clear()
    return out


def parse_json_section(text: str) -> JsonSection:
    """Parse structs, enums, and aliases between the JSON section markers."""
    begin = text.find(JSON_SECTION_BEGIN)
    end = text.find(JSON_SECTION_END)
    if begin < 0 or end < 0 or end <= begin:
        raise SystemExit("JSON payload contracts section markers not found in plugin.capnp")
    section = JsonSection()
    doc: list[str] = []
    current: CapnpStruct | CapnpEnum | None = None
    for raw in text[begin:end].splitlines()[1:]:
        line = raw.rstrip()
        if not line:
            doc.clear()
            continue
        stripped = line.strip()
        if stripped.startswith("#####"):
            doc.clear()
            continue
        if stripped.startswith("#"):
            doc.append(stripped.lstrip("#").strip())
            continue
        if stripped.startswith("annotation "):
            doc.clear()
            continue
        m = re.match(r"^struct (\w+) \{$", line)
        if m:
            current = CapnpStruct(name=m.group(1), doc=_flush_doc(doc))
            section.decls.append(current)
            continue
        m = re.match(r"^enum (\w+)( \$jsonEnum)? \{$", line)
        if m:
            current = CapnpEnum(
                name=m.group(1), json_enum=bool(m.group(2)), doc=_flush_doc(doc)
            )
            section.decls.append(current)
            continue
        m = re.match(r"^using (\w+) = (\w+);$", line)
        if m:
            section.decls.append(
                CapnpAlias(name=m.group(1), target=m.group(2), doc=_flush_doc(doc))
            )
            continue
        if line == "}":
            current = None
            doc.clear()
            continue
        if isinstance(current, CapnpStruct):
            m = _FIELD_RE.match(line)
            if not m:
                raise SystemExit(f"unparsed struct line in JSON section: {line!r}")
            annos = m.group(3) or ""
            current.fields.append(
                CapnpField(
                    name=m.group(1),
                    type=m.group(2),
                    required="$required" in annos,
                    json_value="$jsonValue" in annos,
                    doc=_flush_doc(doc),
                )
            )
            continue
        if isinstance(current, CapnpEnum):
            m = _ENUMERANT_RE.match(line)
            if not m:
                raise SystemExit(f"unparsed enum line in JSON section: {line!r}")
            current.enumerants.append(m.group(1))
            doc.clear()
            continue
        raise SystemExit(f"unparsed line in JSON payload section: {line!r}")
    if not section.decls:
        raise SystemExit("JSON payload contracts section is empty")
    return section


def parse_capnp_enum(text: str, name: str) -> list[str]:
    """Ordinal-ordered enumerant wire names of one Cap'n Proto enum."""
    match = re.search(rf"enum {name}(?: \$\w+)? \{{(.*?)\}}", text, re.DOTALL)
    if not match:
        raise SystemExit(f"enum {name} not found in plugin.capnp")
    entries = re.findall(r"(\w+)\s*@(\d+);", match.group(1))
    entries.sort(key=lambda e: int(e[1]))
    return [n for n, _ in entries]


def snake_wire(camel: str) -> str:
    """JSON wire string for a ``$jsonEnum`` enumerant (snake_case)."""
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", camel).lower()


def _screaming(camel: str) -> str:
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", camel).upper()


# Presentation docs for the generated constants (values come from the schema).
_CONST_DOCS = {
    "PRODUCT_API_VERSION": "Product ABI version (`apiVersion` / `plugin.toml` `api_version`).",
    "MAX_SCALAR_BYTES": "Maximum decoded size of an ordinary RPC scalar value (not a stream window).",
    "MAX_STREAM_WINDOW_BYTES": "Maximum bytes returned by one `ByteSource.pull` (flow-control window).",
    "MAX_LIST_PAGE": "Maximum objects in one `Destination.list` page.",
    "MAX_CHECKPOINT_BYTES": "Maximum job / event checkpoint payload size (bytes).",
    "MAX_IDENTIFIER_BYTES": "Maximum plugin / account identifier length (bytes).",
    "MAX_CONFIG_PAYLOAD_BYTES": "Maximum granted config payload size (bytes).",
    "MAX_EVENT_PAYLOAD_BYTES": "Maximum decoded size of a domain-event scalar payload (not a stream).",
    "MAX_PLUGIN_MIGRATION_OPS": "Maximum already-separated operations in one plugin-owned migration.",
    "MAX_PLUGIN_MIGRATION_TOTAL_OPS": "Maximum already-separated operations across one `databaseMigrations` registration.",
    "MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES": "Maximum aggregate UTF-8 bytes of plugin migration ids plus SQL in one `databaseMigrations` registration.",
    "FEATURE_SCALAR_LIMITS": "Guest honors scalar / stream-window / list-page caps.",
    "FEATURE_STREAMS": "Media moves through transferred `ByteRange` / `ByteSource` streams.",
    "FEATURE_STORAGE_COPY": "Guest implements server-side `Destination.copy`.",
}

_ENUM_TABLES = (
    # (constant name, capnp enum, doc)
    (
        "DB_STATEMENT_KINDS",
        "DbStatementKind",
        "Ordinal-ordered `DbStatementKind` wire names (index = Cap'n Proto ordinal).",
    ),
    (
        "DB_RESULT_SELECTIONS",
        "DbResultSelection",
        "Ordinal-ordered `DbResultSelection` wire names (index = Cap'n Proto ordinal).",
    ),
    (
        "DB_COLUMN_TYPES",
        "DbType",
        "Ordinal-ordered `DbType` column-type wire names (index = Cap'n Proto ordinal).",
    ),
)

_ENUM_TYPE_NAMES = {
    "DB_STATEMENT_KINDS": "DbStatementKind",
    "DB_RESULT_SELECTIONS": "DbResultSelection",
    "DB_COLUMN_TYPES": "DbColumnType",
}


def _const_entries(consts: dict[str, int | str]) -> list[tuple[str, int | str]]:
    entries: list[tuple[str, int | str]] = []
    for camel, value in consts.items():
        name = "PRODUCT_API_VERSION" if camel == "apiVersion" else _screaming(camel)
        entries.append((name, value))
    return entries


def emit_ts_abi(capnp_text: str) -> str:
    """`packages/plugin-sdk/src/abi.ts` — product constants + enum tables."""
    consts = parse_capnp_constants(capnp_text)
    lines = [
        "/**",
        f" * {GENERATED_NOTE}",
        " *",
        " * TypeScript projection of the product ABI constants and database enum",
        " * ordinal tables declared in `crates/bookclerk-plugin-abi/schema/plugin.capnp`.",
        " */",
        "",
    ]
    for name, value in _const_entries(consts):
        doc = _CONST_DOCS.get(name, name)
        lines.append(f"/** {doc} */")
        if isinstance(value, int):
            lines.append(f"export const {name} = {value} as const;")
        else:
            lines.append(f'export const {name} = "{value}" as const;')
        lines.append("")
    for name, enum, doc in _ENUM_TABLES:
        values = parse_capnp_enum(capnp_text, enum)
        rendered = ", ".join(f'"{v}"' for v in values)
        ty = _ENUM_TYPE_NAMES[name]
        lines.append(f"/** {doc} */")
        lines.append(f"export const {name} = [{rendered}] as const;")
        lines.append("")
        lines.append(f"/** Union of `{name}` wire names. */")
        lines.append(f"export type {ty} = (typeof {name})[number];")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def emit_py_product_abi(capnp_text: str) -> str:
    """`packages/plugin-sdk-python/.../_abi.py` — product constants + enum tables."""
    consts = parse_capnp_constants(capnp_text)
    lines = [
        f'"""{GENERATED_NOTE}',
        "",
        "Python projection of the product ABI constants and database enum ordinal",
        "tables declared in ``crates/bookclerk-plugin-abi/schema/plugin.capnp``.",
        '"""',
        "",
        "from __future__ import annotations",
        "",
    ]
    names: list[str] = []
    for name, value in _const_entries(consts):
        doc = _CONST_DOCS.get(name, name)
        if isinstance(value, int):
            lines.append(f"{name}: int = {value}")
        else:
            lines.append(f'{name}: str = "{value}"')
        lines.append(f'"""{doc}"""')
        lines.append("")
        names.append(name)
    for name, enum, doc in _ENUM_TABLES:
        values = parse_capnp_enum(capnp_text, enum)
        rendered = ", ".join(f'"{v}"' for v in values)
        lines.append(f"{name}: tuple[str, ...] = ({rendered})")
        lines.append(f'"""{doc}"""')
        lines.append("")
        names.append(name)
    exported = ",\n".join(f'    "{n}"' for n in names)
    lines.append(f"__all__ = [\n{exported},\n]")
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# Embed rewriting (packages/plugin-sdk/embed/bookclerk_plugin.js)
# ---------------------------------------------------------------------------


def sync_embed_constants(embed_text: str, capnp_text: str) -> str:
    """Rewrite constants and the error-code set in the embed JS from the schema."""
    consts = dict(_const_entries(parse_capnp_constants(capnp_text)))
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
    codes = [snake_wire(e) for e in parse_capnp_enum(capnp_text, "PluginErrorCode")]
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
# JSON payload DTO section -> TypeScript generated.ts
# ---------------------------------------------------------------------------

_SCALAR_TS = {
    "Text": "string",
    "Bool": "boolean",
    "UInt32": "number",
    "UInt64": "number",
    "Int32": "number",
    "Int64": "number",
    "Float32": "number",
    "Float64": "number",
}

_SCALAR_PY = {
    "Text": "str",
    "Bool": "bool",
    "UInt32": "int",
    "UInt64": "int",
    "Int32": "int",
    "Int64": "int",
    "Float32": "float",
    "Float64": "float",
}


def _field_type(field_decl: CapnpField, scalars: dict[str, str], list_fmt: str) -> str:
    if field_decl.json_value:
        return "JsonValue"
    ty = field_decl.type
    m = re.match(r"^List\((\w+)\)$", ty)
    if m:
        inner = scalars.get(m.group(1), m.group(1))
        return list_fmt.format(inner)
    return scalars.get(ty, ty)


def _ts_doc(doc: list[str], indent: str = "") -> list[str]:
    if not doc:
        return []
    text = " ".join(doc)
    if len(text) + len(indent) <= 90:
        return [f"{indent}/** {text} */"]
    body = textwrap.wrap(text, width=94 - len(indent) - 3)
    return [f"{indent}/**", *[f"{indent} * {line}" for line in body], f"{indent} */"]


def emit_ts_generated(capnp_text: str) -> str:
    """`packages/plugin-sdk/src/generated.ts` — JSON payload DTO projection."""
    section = parse_json_section(capnp_text)
    lines = [
        "/**",
        f" * {GENERATED_NOTE}",
        " *",
        " * TypeScript projection of the JSON payload contracts declared in",
        " * `crates/bookclerk-plugin-abi/schema/plugin.capnp` (the payloads carried",
        " * inside `Text` fields of the Cap'n Proto ABI: `describe().metadataJson`,",
        " * role `paramsJson`, and `cliInvoke` params/results). Field names are the",
        " * literal JSON keys (camelCase).",
        " */",
        "",
        "/** Arbitrary JSON value carried inside a payload field. */",
        "export type JsonValue = unknown;",
        "",
        "/** Loose JSON object used for config blobs and structured payloads. */",
        "export type JsonObject = Record<string, unknown>;",
        "",
    ]
    for decl in section.decls:
        lines.extend(_ts_doc(decl.doc))
        if isinstance(decl, CapnpEnum):
            wire = [snake_wire(e) for e in decl.enumerants]
            table = _screaming(decl.name) + "S"
            rendered = ", ".join(f'"{v}"' for v in wire)
            lines.append(f"export const {table} = [{rendered}] as const;")
            lines.append("")
            lines.append(f"/** Union of known `{decl.name}` wire strings. */")
            lines.append(f"export type {decl.name} = (typeof {table})[number];")
        elif isinstance(decl, CapnpAlias):
            lines.append(f"export type {decl.name} = {decl.target};")
        else:
            lines.append(f"export interface {decl.name} {{")
            for f in decl.fields:
                lines.extend(_ts_doc(f.doc, "  "))
                opt = "" if f.required else "?"
                lines.append(f"  {f.name}{opt}: {_field_type(f, _SCALAR_TS, '{}[]')};")
            lines.append("}")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


# ---------------------------------------------------------------------------
# JSON payload DTO section -> Python abi.py
# ---------------------------------------------------------------------------


def _py_attr_doc(fields: list[CapnpField]) -> list[str]:
    attr_lines: list[str] = []
    for f in fields:
        text = " ".join(f.doc) if f.doc else f.name
        attr_lines.extend(
            textwrap.wrap(
                f"{f.name}: {text}",
                width=76,
                initial_indent="        ",
                subsequent_indent="            ",
            )
        )
    if not attr_lines:
        return []
    return ["", "    Attributes:", *attr_lines]


def emit_py_abi(capnp_text: str) -> str:
    """`packages/plugin-sdk-python/.../abi.py` — JSON payload DTO projection."""
    section = parse_json_section(capnp_text)
    lines = [
        f'"""{GENERATED_NOTE}',
        "",
        "Python projection of the JSON payload contracts declared in",
        "``crates/bookclerk-plugin-abi/schema/plugin.capnp`` (the payloads carried",
        "inside ``Text`` fields of the Cap'n Proto ABI: ``describe().metadataJson``,",
        "role ``paramsJson``, and ``cliInvoke`` params/results). TypedDict keys are",
        "the literal JSON keys (camelCase).",
        '"""',
        "",
        "from __future__ import annotations",
        "",
        "from typing import Any, Literal, NotRequired, TypedDict",
        "",
        "JsonValue = Any",
        '"""Arbitrary JSON value carried inside a payload field."""',
        "",
        "JsonObject = dict[str, Any]",
        '"""Loose JSON object used for config blobs and structured payloads."""',
        "",
    ]
    exported = ["JsonValue", "JsonObject"]
    for decl in section.decls:
        desc = " ".join(decl.doc) if decl.doc else decl.name
        exported.append(decl.name)
        if isinstance(decl, CapnpEnum):
            wire = [snake_wire(e) for e in decl.enumerants]
            table = _screaming(decl.name) + "S"
            exported.append(table)
            rendered = ", ".join(f'"{v}"' for v in wire)
            lines.append(f"{table}: tuple[str, ...] = ({rendered})")
            lines.append(f'"""{desc}"""')
            lines.append("")
            if len(decl.name) + len(rendered) <= 76:
                lines.append(f"{decl.name} = Literal[{rendered}]")
            else:
                lines.append(f"{decl.name} = Literal[")
                lines.extend(f'    "{v}",' for v in wire)
                lines.append("]")
            lines.append(f'"""Union of known `{decl.name}` wire strings."""')
        elif isinstance(decl, CapnpAlias):
            lines.append(f"{decl.name} = {decl.target}")
            lines.append(f'"""{desc}"""')
        else:
            lines.append(f"class {decl.name}(TypedDict):")
            lines.append(f'    """{desc}')
            lines.extend(_py_attr_doc(decl.fields))
            lines.append('    """')
            lines.append("")
            if not decl.fields:
                lines.append("    pass")
            for f in decl.fields:
                ty = _field_type(f, _SCALAR_PY, "list[{}]")
                if not f.required:
                    ty = f"NotRequired[{ty}]"
                lines.append(f"    {f.name}: {ty}")
        lines.append("")
        lines.append("")
    body = "\n".join(lines)
    exported_lines = ",\n".join(f'    "{n}"' for n in sorted(exported))
    body += f"__all__ = [\n{exported_lines},\n]\n"
    body = re.sub(r"\n{4,}", "\n\n\n", body)
    return body
