"""Render the daemon's OpenAPI document as Markdown for ``api/web/``.

``docs-markdown.py`` uses this for the web API reference. The input is
``crates/bookclerkd/openapi.json``, the document ``bookclerkd`` generates from
its routes (the same one it serves at ``GET /api/openapi.json``); it is read
as JSON, nothing is executed.

The pages are:

- ``README.md``: the API's description, how to authenticate, and an index of
  the route groups (OpenAPI tags) with every route;
- one page per tag (``<tag slug>.md``) with each operation: who may call it
  (``x-bookclerk-access``), its parameters, request body and responses;
- ``schemas.md``: every component schema, with a table of its fields.

Schema references become links to ``schemas.md``. Anything the renderer does
not understand is shown as ``any JSON`` rather than dropped. Raw HTML is never
emitted, and table cells escape ``|`` and fold newlines, so a description
cannot break the page.
"""

from __future__ import annotations

import json
import re
from collections.abc import Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from rustdoc_markdown import Anchors

# Order of methods within a path, as OpenAPI lists them.
METHODS = ("get", "put", "post", "delete", "options", "head", "patch", "trace")
SCHEMA_PAGE = "schemas.md"
REF_PREFIX = "#/components/schemas/"
# Inline objects are flattened into `parent.child` rows this deep.
MAX_FIELD_DEPTH = 3
# Parameter locations, in the order the tables list them.
PARAM_ORDER = {"path": 0, "query": 1, "header": 2, "cookie": 3}

JsonObject = Mapping[str, Any]


class OpenApiError(ValueError):
    """The document is not an OpenAPI 3 document this renderer can read."""


def cell(text: str) -> str:
    """Make text safe for one Markdown table cell.

    Args:
        text: Markdown text, possibly over several lines.

    Returns:
        The text on one line, with ``|`` escaped and paragraphs separated by
        a space.
    """
    return " ".join(text.split()).replace("|", "\\|")


def prose(text: str) -> str:
    """Normalize a description for use as a paragraph.

    Args:
        text: Markdown text from the document.

    Returns:
        The text with trailing spaces and surrounding blank lines removed.
    """
    return "\n".join(line.rstrip() for line in text.strip().splitlines())


def code(text: str) -> str:
    """Return ``text`` as an inline code span that survives backticks in it.

    Args:
        text: Literal text.

    Returns:
        The span, fenced with one more backtick than the longest run inside.
    """
    longest = max((len(run) for run in re.findall(r"`+", text)), default=0)
    fence = "`" * (longest + 1)
    pad = " " if text.startswith("`") or text.endswith("`") else ""
    return f"{fence}{pad}{text}{pad}{fence}"


@dataclass
class Operation:
    """One method on one path."""

    #: Upper-case HTTP method.
    method: str
    #: The path template, such as ``/api/users/{id}``.
    path: str
    #: The operation object from the document.
    spec: JsonObject
    #: Anchor of the operation's heading on its tag page.
    anchor: str = ""

    @property
    def title(self) -> str:
        """The heading text: method and path."""
        return f"{self.method} {self.path}"


@dataclass
class Tag:
    """A route group: one page of the reference."""

    #: The tag name, such as ``Library``.
    name: str
    #: The tag's description, or ``""``.
    description: str
    #: The page file name under ``api/web/``.
    page: str
    #: Operations in document order.
    operations: list[Operation] = field(default_factory=list)


class Document:
    """An OpenAPI document and the pages rendered from it."""

    def __init__(self, spec: JsonObject) -> None:
        """Index the document's tags, operations and schemas.

        Args:
            spec: The parsed document.

        Raises:
            OpenApiError: The document is not OpenAPI 3, or its paths,
                tags or schemas are malformed.
        """
        version = spec.get("openapi")
        if not isinstance(version, str) or not version.startswith("3."):
            raise OpenApiError(f"not an OpenAPI 3 document (openapi = {version!r})")
        self.spec = spec
        components = _object(spec.get("components", {}), "components")
        self.schemas: JsonObject = _object(components.get("schemas", {}), "components.schemas")
        self.security_schemes: JsonObject = _object(components.get("securitySchemes", {}), "components.securitySchemes")
        self.schema_anchors = self._plan_schema_anchors()
        self.tags = self._collect_tags()

    def _plan_schema_anchors(self) -> dict[str, str]:
        """Assign each component schema its heading anchor on the schema page."""
        anchors = Anchors()
        anchors.take("Schemas")
        return {name: anchors.take(name) for name in self.schema_names()}

    def schema_names(self) -> list[str]:
        """Component schema names in page order: alphabetical, ignoring case."""
        return sorted(self.schemas, key=str.casefold)

    def _collect_tags(self) -> list[Tag]:
        """Group operations by their first tag, in the document's tag order."""
        tags: dict[str, Tag] = {}
        pages = Anchors()
        pages.take("README")
        pages.take("schemas")

        def tag(name: str, description: str = "") -> Tag:
            if name not in tags:
                tags[name] = Tag(name, description, f"{pages.take(name)}.md")
            return tags[name]

        for entry in spec_list(self.spec.get("tags", []), "tags"):
            entry = _object(entry, "tags[]")
            tag(str(entry.get("name", "")), str(entry.get("description", "")))
        for path, item in _object(self.spec.get("paths", {}), "paths").items():
            item = _object(item, f"paths.{path}")
            for method in METHODS:
                if method not in item:
                    continue
                op = _object(item[method], f"{method} {path}")
                op = _with_path_parameters(op, item, f"{method} {path}")
                names = spec_list(op.get("tags", []), f"{method} {path} tags")
                tag(str(names[0]) if names else "Other").operations.append(Operation(method.upper(), path, op))
        for group in tags.values():
            anchors = Anchors()
            anchors.take(group.name)
            for op in group.operations:
                op.anchor = anchors.take(op.title)
        return [group for group in tags.values() if group.operations]

    # Types ---------------------------------------------------------------

    def ref_link(self, ref: str, here: str) -> str:
        """Link a ``$ref`` to its schema.

        Args:
            ref: The reference, ``#/components/schemas/<name>``.
            here: The page the link appears on.

        Returns:
            A link to the schema's heading, or the name in code when the
            reference points elsewhere.
        """
        name = ref.removeprefix(REF_PREFIX)
        anchor = self.schema_anchors.get(name)
        if anchor is None:
            return code(name)
        target = "" if here == SCHEMA_PAGE else SCHEMA_PAGE
        return f"[{code(name)}]({target}#{anchor})"

    def type_of(self, schema: Any, here: str) -> str:
        """Describe a schema's type in a few words of Markdown.

        Args:
            schema: A schema object (or ``True`` / ``False``).
            here: The page the text appears on, for links.

        Returns:
            Text such as ``string``, ``array of [`Book`](...)``,
            ``integer or null`` or ``any JSON``.
        """
        if schema is True or not isinstance(schema, Mapping):
            return "any JSON"
        if "$ref" in schema:
            return self.ref_link(str(schema["$ref"]), here)
        for key, joiner in (("oneOf", " or "), ("anyOf", " or "), ("allOf", " and ")):
            if key in schema:
                parts = [self.type_of(part, here) for part in spec_list(schema[key], key)]
                return joiner.join(_unique(parts)) or "any JSON"
        if "const" in schema:
            return code(json.dumps(schema["const"]))
        if "enum" in schema:
            values = [code(json.dumps(v) if not isinstance(v, str) else v) for v in spec_list(schema["enum"], "enum")]
            if len(values) == 1:
                return values[0]
            return "one of " + ", ".join(values) if values else "any JSON"
        types = schema.get("type")
        names = [str(t) for t in types] if isinstance(types, list) else [str(types)] if types else []
        if not names:
            if "properties" in schema or "additionalProperties" in schema:
                names = ["object"]
            elif "items" in schema:
                names = ["array"]
            else:
                return "any JSON"
        return " or ".join(_unique(self._one_type(name, schema, here) for name in names))

    def _one_type(self, name: str, schema: JsonObject, here: str) -> str:
        """Describe one entry of a schema's ``type``."""
        if name == "array":
            items = schema.get("items")
            return f"array of {self.type_of(items, here)}" if items is not None else "array"
        if name == "object":
            extra = schema.get("additionalProperties")
            if isinstance(extra, Mapping) and not schema.get("properties"):
                return f"map of string to {self.type_of(extra, here)}"
            return "object"
        fmt = schema.get("format")
        if name == "string" and fmt == "binary":
            return "binary"
        return f"{name} ({fmt})" if isinstance(fmt, str) and fmt else name

    def constraints(self, schema: Any) -> str:
        """Summarize a schema's bounds and defaults.

        Args:
            schema: A schema object.

        Returns:
            Text such as ``at least 0; default `10```, or ``""``.
        """
        if not isinstance(schema, Mapping):
            return ""
        notes = []
        for key, label in (("minimum", "at least"), ("maximum", "at most"), ("minLength", "at least {} characters"),
                           ("maxLength", "at most {} characters"), ("minItems", "at least {} items"),
                           ("maxItems", "at most {} items")):
            if key in schema:
                value = schema[key]
                notes.append(label.format(value) if "{}" in label else f"{label} {value}")
        if "pattern" in schema:
            notes.append(f"matches {code(str(schema['pattern']))}")
        if schema.get("uniqueItems"):
            notes.append("no duplicates")
        if "default" in schema:
            notes.append(f"default {code(json.dumps(schema['default']))}")
        if schema.get("deprecated"):
            notes.append("deprecated")
        return "; ".join(notes)

    def describe(self, schema: Any) -> str:
        """Return a schema's description followed by its constraints.

        Args:
            schema: A schema object.

        Returns:
            The text for a table's description column.
        """
        if not isinstance(schema, Mapping):
            return ""
        text = str(schema.get("description", ""))
        if not text and "$ref" not in schema:
            for key in ("oneOf", "anyOf"):
                parts = [p for p in spec_list(schema.get(key, []), key) if isinstance(p, Mapping)]
                text = next((str(p["description"]) for p in parts if p.get("description")), "")
                if text:
                    break
        extra = self.constraints(schema)
        if not (text and extra):
            return text or extra
        text = text.rstrip()
        if text.endswith("."):
            return f"{text} {extra[0].upper()}{extra[1:]}."
        return f"{text} ({extra})"

    def field_rows(self, schema: Any, here: str, prefix: str = "", depth: int = 0) -> list[str]:
        """Table rows for an object's fields, flattening inline objects.

        Args:
            schema: An object schema (``allOf`` parts are merged).
            here: The page the rows appear on.
            prefix: Dotted name of the enclosing field, or ``""``.
            depth: How many inline objects enclose this one.

        Returns:
            ``| field | type | required | description |`` rows.
        """
        rows = []
        for name, prop, required in self.fields(schema):
            full = f"{prefix}{name}"
            rows.append(
                f"| {code(full)} | {cell(self.type_of(prop, here))} | {'yes' if required else 'no'} | {cell(self.describe(prop))} |"
            )
            if depth + 1 < MAX_FIELD_DEPTH and _inline_object(prop):
                rows += self.field_rows(prop, here, f"{full}.", depth + 1)
            elif depth + 1 < MAX_FIELD_DEPTH and isinstance(prop, Mapping) and _inline_object(prop.get("items")):
                rows += self.field_rows(prop["items"], here, f"{full}[].", depth + 1)
        return rows

    def fields(self, schema: Any) -> Iterator[tuple[str, Any, bool]]:
        """Yield an object's fields with whether each is required.

        Args:
            schema: An object schema; inline ``allOf`` parts contribute their
                fields, referenced parts are listed by :meth:`type_of`.

        Yields:
            ``(name, schema, required)`` in declaration order.
        """
        if not isinstance(schema, Mapping):
            return
        for part in spec_list(schema.get("allOf", []), "allOf"):
            if isinstance(part, Mapping) and "$ref" not in part:
                yield from self.fields(part)
        required = set(spec_list(schema.get("required", []), "required"))
        props = schema.get("properties", {})
        if isinstance(props, Mapping):
            for name, prop in props.items():
                yield str(name), prop, name in required

    # Pages ---------------------------------------------------------------

    def render_all(self) -> dict[str, str]:
        """Render every page.

        Returns:
            Page file name under ``api/web/`` mapped to its Markdown.
        """
        pages = {"README.md": self.render_index()}
        for group in self.tags:
            pages[group.page] = self.render_tag(group)
        pages[SCHEMA_PAGE] = self.render_schemas()
        return pages

    def render_index(self) -> str:
        """Render ``README.md``: description, authentication and route index."""
        info = _object(self.spec.get("info", {}), "info")
        lines = [f"# {info.get('title', 'Web API')}", ""]
        if info.get("description"):
            lines += [prose(str(info["description"])), ""]
        version = info.get("version")
        lines += [
            "The daemon serves this reference as an OpenAPI document at "
            + code("GET /api/openapi.json")
            + (f" (API version {code(str(version))})." if version else "."),
            "",
        ]
        if self.security_schemes:
            lines += [
                "## Authentication",
                "",
                "Each route lists the credentials it accepts and who may call it. The credentials are:",
                "",
                "| Name | Kind | Description |",
                "| --- | --- | --- |",
            ]
            for name, scheme in self.security_schemes.items():
                lines.append(f"| {code(str(name))} | {cell(self._scheme_kind(scheme))} | {cell(str(_get(scheme, 'description')))} |")
            lines.append("")
        lines += ["## Routes", ""]
        for group in self.tags:
            lines += [f"### [{group.name}]({group.page})", ""]
            if group.description:
                lines += [group.description, ""]
            lines += ["| Route | Summary |", "| --- | --- |"]
            for op in group.operations:
                mark = " (deprecated)" if op.spec.get("deprecated") else ""
                lines.append(f"| [{code(op.title)}]({group.page}#{op.anchor}){mark} | {cell(str(op.spec.get('summary', '')))} |")
            lines.append("")
        lines += ["## Schemas", "", f"Request and response bodies are described in [Schemas]({SCHEMA_PAGE}).", ""]
        return _join(lines)

    def _scheme_kind(self, scheme: Any) -> str:
        """Describe how a security scheme is sent."""
        if not isinstance(scheme, Mapping):
            return ""
        kind = scheme.get("type")
        if kind == "http":
            return f"HTTP {scheme.get('scheme', '')}".strip()
        if kind == "apiKey":
            return f"{scheme.get('in', '')} {code(str(scheme.get('name', '')))}".strip()
        return str(kind or "")

    def render_tag(self, group: Tag) -> str:
        """Render one tag's page.

        Args:
            group: The tag.

        Returns:
            The page's Markdown.
        """
        lines = [f"# {group.name}", ""]
        if group.description:
            lines += [prose(group.description), ""]
        lines += ["Back to the [web API reference](README.md).", ""]
        for op in group.operations:
            lines += self.render_operation(op, group.page)
        return _join(lines)

    def render_operation(self, op: Operation, here: str) -> list[str]:
        """Render one operation's section.

        Args:
            op: The operation.
            here: Its page.

        Returns:
            Markdown lines.
        """
        spec = op.spec
        lines = [f"## {code(op.title)}", ""]
        if spec.get("deprecated"):
            lines += ["**Deprecated.**", ""]
        if spec.get("summary"):
            lines += [prose(str(spec["summary"])), ""]
        if spec.get("description"):
            lines += [prose(str(spec["description"])), ""]
        access = spec.get("x-bookclerk-access")
        if access:
            lines += [f"**Access:** {' '.join(str(access).split())}", ""]
        security = self._security(spec.get("security"))
        if security:
            lines += [f"**Credentials:** {security}", ""]

        params = [p for p in spec_list(spec.get("parameters", []), "parameters") if isinstance(p, Mapping)]
        if params:
            lines += ["### Parameters", "", "| Name | In | Type | Required | Description |", "| --- | --- | --- | --- | --- |"]
            for param in sorted(params, key=lambda p: PARAM_ORDER.get(str(p.get("in")), 9)):
                schema = param.get("schema", {})
                required = bool(param.get("required")) or param.get("in") == "path"
                description = str(param.get("description", "")) or self.describe(schema)
                lines.append(
                    f"| {code(str(param.get('name', '')))} | {param.get('in', '')} | {cell(self.type_of(schema, here))} | "
                    f"{'yes' if required else 'no'} | {cell(description)} |"
                )
            lines.append("")

        body = spec.get("requestBody")
        if isinstance(body, Mapping):
            lines += ["### Request body", ""]
            optional = "" if body.get("required") else " Optional."
            if body.get("description"):
                lines += [prose(str(body["description"])) + optional, ""]
            elif optional:
                lines += [optional.strip(), ""]
            lines += self._content(body.get("content", {}), here)

        responses = _object(spec.get("responses", {}), f"{op.title} responses")
        if responses:
            lines += ["### Responses", "", "| Status | Description | Body |", "| --- | --- | --- |"]
            for status, response in responses.items():
                response = response if isinstance(response, Mapping) else {}
                lines.append(f"| {status} | {cell(str(response.get('description', '')))} | {cell(self._body_summary(response, here))} |")
            lines.append("")
        return lines

    def _security(self, requirements: Any) -> str:
        """List the credentials an operation accepts, or ``none``."""
        if not isinstance(requirements, Sequence) or isinstance(requirements, str):
            return ""
        options = []
        for requirement in requirements:
            if not isinstance(requirement, Mapping):
                continue
            names = [code(str(name)) for name in requirement]
            options.append(" and ".join(names) if names else "none")
        if not requirements:
            return "none"
        if not options:
            return ""
        return " or ".join(_unique(options))

    def _content(self, content: Any, here: str) -> list[str]:
        """Render a request body's media types, with field tables for objects."""
        lines: list[str] = []
        if not isinstance(content, Mapping):
            return lines
        for media, entry in content.items():
            schema = entry.get("schema", {}) if isinstance(entry, Mapping) else {}
            lines += [f"{code(str(media))}: {self.type_of(schema, here)}", ""]
            rows = self.field_rows(schema, here) if _inline_object(schema) else []
            if rows:
                lines += ["| Field | Type | Required | Description |", "| --- | --- | --- | --- |", *rows, ""]
        return lines

    def _body_summary(self, response: JsonObject, here: str) -> str:
        """Summarize a response's body as ``type (media)`` entries."""
        content = response.get("content")
        if not isinstance(content, Mapping) or not content:
            return ""
        parts = []
        for media, entry in content.items():
            schema = entry.get("schema", {}) if isinstance(entry, Mapping) else {}
            parts.append(f"{self.type_of(schema, here)} ({code(str(media))})")
        return "; ".join(parts)

    def render_schemas(self) -> str:
        """Render ``schemas.md``: every component schema."""
        lines = [
            "# Schemas",
            "",
            "The request and response bodies of the [web API](README.md), in alphabetical order.",
            "",
        ]
        for name in self.schema_names():
            schema = self.schemas[name]
            lines += [f"## {name}", ""]
            if isinstance(schema, Mapping) and schema.get("description"):
                lines += [prose(str(schema["description"])), ""]
            rows = self.field_rows(schema, SCHEMA_PAGE)
            refs = [p for p in spec_list(schema.get("allOf", []), "allOf") if isinstance(p, Mapping) and "$ref" in p] if isinstance(schema, Mapping) else []
            if refs:
                lines += ["Includes every field of " + ", ".join(self.ref_link(str(p["$ref"]), SCHEMA_PAGE) for p in refs) + ".", ""]
            if rows:
                lines += ["| Field | Type | Required | Description |", "| --- | --- | --- | --- |", *rows, ""]
            elif not refs:
                lines += [f"Type: {self.type_of(schema, SCHEMA_PAGE)}", ""]
                extra = self.constraints(schema)
                if extra:
                    lines += [f"Constraints: {extra}", ""]
        return _join(lines)


def spec_list(value: Any, where: str) -> list[Any]:
    """Return a JSON array from the document.

    Args:
        value: The value.
        where: Where it was read, for the error.

    Returns:
        The array as a list.

    Raises:
        OpenApiError: The value is not an array.
    """
    if not isinstance(value, list):
        raise OpenApiError(f"{where}: expected an array")
    return value


def _with_path_parameters(op: JsonObject, item: JsonObject, where: str) -> JsonObject:
    """Merge a path item's shared parameters into one of its operations.

    Args:
        op: The operation.
        item: The path item holding it.
        where: Where the operation is, for error messages.

    Returns:
        The operation with the path's parameters it does not override, keyed by
        ``(name, in)``; ``$ref`` entries are kept as they are.
    """
    shared = spec_list(item.get("parameters", []), f"{where} path parameters")
    if not shared:
        return op
    keyed: dict[tuple[str, str], Any] = {}
    unkeyed: list[Any] = []
    for param in [*shared, *spec_list(op.get("parameters", []), f"{where} parameters")]:
        name = param.get("name") if isinstance(param, Mapping) else None
        location = param.get("in") if isinstance(param, Mapping) else None
        if isinstance(name, str) and isinstance(location, str) and "$ref" not in param:
            keyed[(name, location)] = param
        else:
            unkeyed.append(param)
    return {**op, "parameters": [*keyed.values(), *unkeyed]}


def _object(value: Any, where: str) -> JsonObject:
    """Return a JSON object from the document, or raise :class:`OpenApiError`."""
    if not isinstance(value, Mapping):
        raise OpenApiError(f"{where}: expected an object")
    return value


def _get(value: Any, key: str) -> Any:
    """Read ``key`` from a JSON object, or ``""`` when absent or not an object."""
    return value.get(key, "") if isinstance(value, Mapping) else ""


def _inline_object(schema: Any) -> bool:
    """Whether a schema is an object defined in place (not a ``$ref``)."""
    return isinstance(schema, Mapping) and "$ref" not in schema and bool(schema.get("properties") or schema.get("allOf"))


def _unique(items: Iterator[str] | list[str]) -> list[str]:
    """Drop repeats, keeping the first of each."""
    seen: dict[str, None] = {}
    for item in items:
        seen.setdefault(item, None)
    return list(seen)


def _join(lines: list[str]) -> str:
    """Join page lines, ending with exactly one newline."""
    return "\n".join(lines).rstrip() + "\n"


def render(path: Path, out: Path) -> int:
    """Render the document at ``path`` into ``out``.

    Args:
        path: The OpenAPI JSON file.
        out: The output directory (``api/web``); created if missing.

    Returns:
        The number of operations rendered.

    Raises:
        OpenApiError: The file is not valid JSON or not a readable document.
    """
    try:
        spec = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as err:
        raise OpenApiError(f"{path}: {err}") from err
    document = Document(_object(spec, "document"))
    out.mkdir(parents=True, exist_ok=True)
    for page, text in document.render_all().items():
        (out / page).write_text(text, encoding="utf-8")
    return sum(len(group.operations) for group in document.tags)


__all__ = ["Document", "OpenApiError", "render"]
