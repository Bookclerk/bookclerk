"""Render the daemon's OpenAPI document as Markdown for ``api/web/``.

``docs-markdown.py`` uses this for the web API reference. The input is
``crates/bookclerkd/openapi.json``, the document ``bookclerkd`` generates from
its routes (the same one it serves at ``GET /api/openapi.json``); it is read
as JSON, nothing is executed.

The pages are:

- ``README.md``: the API's description, a quick-start request, how to
  authenticate, and an index of the route groups (OpenAPI tags) with every
  route;
- one page per tag (``<tag slug>.md``) with each operation: who may call it
  (``x-bookclerk-access``), its parameters, request body and responses, and
  an example request (``curl``) and response built from the schemas;
- ``schemas.md``: every component schema, with a table of its fields.

Schema references become links to ``schemas.md``. Anything the renderer does
not understand is shown as ``any JSON`` rather than dropped. Raw HTML is never
emitted, and table cells escape ``|`` and fold newlines, so a description
cannot break the page.
"""

from __future__ import annotations

import json
import re
import urllib.parse
from collections.abc import Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from urllib.parse import quote, urlencode

from rustdoc_markdown import Anchors

# Order of methods within a path, as OpenAPI lists them.
METHODS = ("get", "put", "post", "delete", "options", "head", "patch", "trace")
SCHEMA_PAGE = "schemas.md"
REF_PREFIX = "#/components/schemas/"
# Inline objects are flattened into `parent.child` rows this deep.
MAX_FIELD_DEPTH = 3
# Parameter locations, in the order the tables list them.
PARAM_ORDER = {"path": 0, "query": 1, "header": 2, "cookie": 3}
# The daemon's default listen address, used in example requests.
EXAMPLE_ORIGIN = "http://127.0.0.1:8787"
# The route the index's Quick start calls when the document has a GET for it.
QUICK_START_PATH = "/api/status"
# Example values nest objects and arrays this deep; deeper ones are empty.
MAX_EXAMPLE_DEPTH = 4
# Example strings by `format`.
FORMAT_EXAMPLES = {
    "date-time": "2026-01-01T12:00:00Z",
    "date": "2026-01-01",
    "time": "12:00:00Z",
    "uuid": "3fa85f64-5717-4562-b3fc-2c963f66afa6",
    "uri": "https://example.com/",
    "url": "https://example.com/",
    "email": "reader@example.com",
    "hostname": "bookclerk.example.com",
    "ipv4": "192.0.2.1",
    "ipv6": "2001:db8::1",
}

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


def nested(text: str, level: int) -> str:
    """Normalize a description that sits under a heading of ``level``.

    Descriptions come from doc comments, whose sections (``# Errors``) are
    top-level headings; left as they are they would outrank the operation
    they describe. Every heading outside a code fence moves down by
    ``level`` (capped at six) so the description nests under its section;
    a setext heading (text underlined with ``===`` or ``---``) is written
    as an ATX one on the way.

    Args:
        text: Markdown text from the document.
        level: The level of the heading the text sits under.

    Returns:
        The text, normalized as by :func:`prose`, with headings demoted.
    """
    lines: list[str] = []
    fence = ""
    for line in prose(text).splitlines():
        indent = len(line) - len(line.lstrip(" "))
        stripped = line.lstrip(" ")
        opener = re.match(r"(`{3,}|~{3,})", stripped) if indent < 4 else None
        if fence:
            # A fence closes on a run of its own character at least as long.
            if opener and opener.group(1)[0] == fence[0] and len(opener.group(1)) >= len(fence) and not stripped[len(opener.group(1)):].strip():
                fence = ""
            lines.append(line)
            continue
        if opener:
            fence = opener.group(1)
            lines.append(line)
            continue
        setext = re.fullmatch(r"(=+|-+)[ \t]*", stripped) if indent < 4 else None
        paragraph = _open_paragraph(lines) if setext else 0
        if setext and paragraph:
            # The underline makes the whole paragraph above it the heading.
            depth = 1 if setext.group(1)[0] == "=" else 2
            text = " ".join(line.strip() for line in lines[-paragraph:])
            del lines[-paragraph:]
            lines.append("#" * min(6, depth + level) + " " + text)
            continue
        hashes = len(stripped) - len(stripped.lstrip("#"))
        if indent < 4 and 1 <= hashes <= 6 and stripped[hashes : hashes + 1] in ("", " ", "\t"):
            line = "#" * min(6, hashes + level) + stripped[hashes:]
        lines.append(line)
    return "\n".join(lines)


def _open_paragraph(lines: Sequence[str]) -> int:
    """Count the trailing lines that form a paragraph a setext underline could close.

    Args:
        lines: The lines so far, outside any code fence.

    Returns:
        How many of the last lines make up that paragraph; 0 when the last
        line is blank or belongs to something else (an indented code block,
        a list, a quote, a heading or a fence).
    """
    count = 0
    for line in reversed(lines):
        if not line.strip() or re.match(r" {0,3}(#|`{3,}|~{3,})", line):
            break
        count += 1
    first = lines[-count] if count else ""
    if not count or first.startswith("    ") or re.match(r" {0,3}([>*+-]|\d+[.)])", first):
        return 0
    return count


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

    # Examples ------------------------------------------------------------

    def resolve(self, schema: Any) -> tuple[Any, str | None]:
        """Follow a schema's reference, or a nullable one, to its component.

        Args:
            schema: A schema object.

        Returns:
            The component schema and its name, or the schema itself and
            ``None`` when it is not a (nullable) reference.
        """
        if isinstance(schema, Mapping):
            ref = schema.get("$ref")
            if not isinstance(ref, str):
                branches = [p for p in schema.get("oneOf", schema.get("anyOf", [])) if isinstance(p, Mapping)]
                refs = [p for p in branches if isinstance(p.get("$ref"), str)]
                others = [p for p in branches if "$ref" not in p and p.get("type") != "null"]
                if len(refs) == 1 and not others:
                    ref = refs[0]["$ref"]
                allof = schema.get("allOf")
                if ref is None and isinstance(allof, list) and len(allof) == 1 and isinstance(allof[0], Mapping):
                    ref = allof[0].get("$ref")
            if isinstance(ref, str) and ref.startswith(REF_PREFIX):
                name = ref.removeprefix(REF_PREFIX)
                if name in self.schemas:
                    return self.schemas[name], name
        return schema, None

    def example(self, schema: Any, depth: int = 0, seen: tuple[str, ...] = (), minimal: bool = False) -> Any:
        """Build an example value for a schema.

        Uses the schema's own ``example`` / ``examples`` / ``default`` when it
        has one, the first ``enum`` value, a format's typical value, and
        otherwise a placeholder of the right type. Arrays get one item and
        nullable values their non-null form. Below
        :data:`MAX_EXAMPLE_DEPTH` levels an object keeps only its required
        fields, so a deep example stays short but still deserializes.

        Args:
            schema: A schema object.
            depth: How many objects and arrays enclose this value.
            seen: Component names being expanded, to stop at a cycle.
            minimal: Keep only required fields at every level. Request
                examples use this: optional fields are often alternatives
                (a callback's ``code`` or its ``error``) that a request
                must not send together.

        Returns:
            A JSON-compatible value, or :data:`NO_EXAMPLE` when the schema
            says nothing about its shape (``{}``) or is binary.
        """
        if not isinstance(schema, Mapping):
            return NO_EXAMPLE
        target, name = self.resolve(schema)
        if name is not None:
            if name in seen:
                return [] if target.get("type") == "array" or "items" in target else {}
            return self.example(target, depth, (*seen, name), minimal)
        for key in ("example", "default"):
            if key in schema:
                return schema[key]
        if isinstance(schema.get("examples"), list) and schema["examples"]:
            return schema["examples"][0]
        if "const" in schema:
            return schema["const"]
        values = [v for v in schema.get("enum", []) if v is not None] if isinstance(schema.get("enum"), list) else []
        if values:
            return values[0]
        for key in ("oneOf", "anyOf"):
            branches = schema.get(key)
            if isinstance(branches, list):
                for branch in branches:
                    if isinstance(branch, Mapping) and branch.get("type") != "null":
                        return self.example(branch, depth, seen, minimal)
        types = schema.get("type")
        names = [t for t in types if t != "null"] if isinstance(types, list) else [types] if isinstance(types, str) else []
        kind = names[0] if names else None
        if kind is None and ("properties" in schema or "allOf" in schema or "additionalProperties" in schema):
            kind = "object"
        if kind is None and "items" in schema:
            kind = "array"
        if kind == "object":
            # Past the depth limit an object keeps only its required fields,
            # so the example still deserializes; a cycle stops at ``seen``.
            if depth >= 2 * MAX_EXAMPLE_DEPTH:
                return {}
            required_only = minimal or depth >= MAX_EXAMPLE_DEPTH
            out: dict[str, Any] = {}
            for part in schema.get("allOf", []) if isinstance(schema.get("allOf"), list) else []:
                value = self.example(part, depth, seen, minimal)
                if isinstance(value, dict):
                    out.update(value)
            for prop_name, prop, required in self.fields(schema):
                if required_only and not required:
                    continue
                value = self.example(prop, depth + 1, seen, minimal)
                if value is not NO_EXAMPLE:
                    out[prop_name] = value
            extra = schema.get("additionalProperties")
            if not out and not required_only and isinstance(extra, Mapping):
                value = self.example(extra, depth + 1, seen, minimal)
                out["key"] = {} if value is NO_EXAMPLE else value
            return out
        if kind == "array":
            if depth >= MAX_EXAMPLE_DEPTH:
                return []
            item = self.example(schema.get("items"), depth + 1, seen, minimal)
            return [] if item is NO_EXAMPLE else [item]
        if kind == "string":
            fmt = schema.get("format")
            if fmt in ("binary", "byte"):
                return NO_EXAMPLE
            return FORMAT_EXAMPLES.get(str(fmt), "string")
        if kind == "integer":
            return schema.get("minimum", 0)
        if kind == "number":
            return schema.get("minimum", 0)
        if kind == "boolean":
            return True
        return NO_EXAMPLE

    def _credential(self, requirements: Any, write: bool) -> tuple[Credential | None, bool]:
        """The first credential an operation accepts, as a sample sends it.

        Args:
            requirements: The operation's ``security`` list.
            write: Whether the method changes state; a cookie credential
                then also sends the ``Origin`` the daemon checks.

        Returns:
            The credential (none for an open route or when no requirement
            uses a scheme a sample can send; each alternative is tried in
            turn) and whether to send ``Origin``.
        """
        if not isinstance(requirements, Sequence) or isinstance(requirements, str):
            return None, False
        for requirement in requirements:
            if not isinstance(requirement, Mapping) or not requirement:
                return None, False
            name = str(next(iter(requirement)))
            scheme = self.security_schemes.get(name)
            if not isinstance(scheme, Mapping):
                continue
            kind = scheme.get("type")
            if kind == "http" and str(scheme.get("scheme", "")).lower() == "bearer":
                return Credential("header", "Authorization", f"Bearer <{name}>"), False
            if kind == "apiKey" and scheme.get("in") == "header":
                header = str(scheme.get("name", ""))
                # A scheme whose description spells out the header's form,
                # such as `Authorization: Bookclerk-Upload <token>`, keeps it.
                shown = re.search(r"`(" + re.escape(header) + r"): ([^`]+)`", str(scheme.get("description", "")))
                return Credential("header", header, shown.group(2) if shown else f"<{name}>"), False
            if kind == "apiKey" and scheme.get("in") == "cookie":
                return Credential("cookie", str(scheme.get("name", "")), f"<{name}>"), write
        return None, False

    def request_example(self, op: Operation) -> RequestExample:
        """Describe an operation's example request, which each sample renders.

        Args:
            op: The operation.

        Returns:
            The method, URL (path parameters stay as ``<name>`` placeholders;
            required query parameters get example values), credential and
            body. A JSON body sends only required fields, since optional
            ones can be alternatives that must not be sent together.
        """
        spec = op.spec
        path = re.sub(r"\{([^}/]+)\}", r"<\1>", op.path)
        query: list[tuple[str, Any]] = []
        for param in spec_list(spec.get("parameters", []), "parameters"):
            if isinstance(param, Mapping) and param.get("in") == "query" and param.get("required"):
                query.append((str(param.get("name", "")), self.example(param.get("schema", {}), minimal=True)))
        url = EXAMPLE_ORIGIN + path + ("?" + form_encode(query) if query else "")
        credential, origin = self._credential(spec.get("security"), op.method not in ("GET", "HEAD", "OPTIONS"))
        body: RequestBody | None = None
        request_body = spec.get("requestBody")
        content = request_body.get("content", {}) if isinstance(request_body, Mapping) else {}
        if isinstance(content, Mapping) and content:
            media, entry = next(iter(content.items()))
            schema = entry.get("schema", {}) if isinstance(entry, Mapping) else {}
            value = self.example(schema, minimal=True)
            if str(media).startswith("application/json"):
                body = RequestBody("json", "application/json", {} if value is NO_EXAMPLE else value)
            elif media == "application/x-www-form-urlencoded" and isinstance(value, dict):
                # The value holds only required fields (see ``minimal``); a
                # form with none of them sends nothing.
                body = RequestBody("form", str(media), list(value.items())) if value else None
            else:
                body = RequestBody("binary", str(media), None)
        return RequestExample(op.method, url, credential, origin, body)

    def example_request(self, op: Operation) -> list[str]:
        """Render an operation's example request in every sample language.

        Args:
            op: The operation.

        Returns:
            Markdown lines: an ``Example request`` heading, then one fenced
            block per language in SAMPLE_LANGUAGES order (``console`` holds
            the ``curl`` command). The site shows the blocks as a language
            picker; elsewhere they follow one another.
        """
        example = self.request_example(op)
        lines = ["### Example request", ""]
        for language, render in SAMPLE_LANGUAGES.items():
            lines += [f"```{language}", *render(example), "```", ""]
        return lines

    def example_response(self, op: Operation) -> list[str]:
        """Render an example of each of an operation's responses.

        Every status gets a ``Status `<code>`: <description>`` line, then a
        ``json`` block built from its schema, or a sentence saying the
        response has no body or one the schemas do not describe (yet). The
        site shows the statuses as tabs.

        Args:
            op: The operation.

        Returns:
            Markdown lines: an ``Example response`` heading and one entry per
            status; nothing when no status has a JSON example.
        """
        responses = op.spec.get("responses", {})
        if not isinstance(responses, Mapping):
            return []
        entries: list[str] = []
        examples = 0
        for status, response in responses.items():
            if not isinstance(response, Mapping):
                continue
            description = str(response.get("description", "")).strip()
            label = f"Status {code(str(status))}:" + (f" {prose(description)}" if description else "")
            content = response.get("content", {})
            if not isinstance(content, Mapping) or not content:
                entries += [f"{label} No body.", ""]
                continue
            entry = content.get("application/json")
            value = self.example(entry.get("schema", {})) if isinstance(entry, Mapping) else NO_EXAMPLE
            if value is NO_EXAMPLE:
                media = ", ".join(code(str(m)) for m in content)
                note = (
                    "The body is JSON the schemas do not describe yet."
                    if isinstance(entry, Mapping)
                    else f"The body is {media}."
                )
                entries += [f"{label} {note}", ""]
                continue
            examples += 1
            text = json.dumps(value, indent=2, ensure_ascii=False)
            entries += [label, "", "```json", text, "```", ""]
        if examples == 0:
            return []
        return ["### Example response", "", *entries]

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
        # The page keeps the name the docs sidebar gives it; the document's
        # own title is named in the paragraph about the document below.
        lines = ["# Web API", ""]
        if info.get("description"):
            lines += [prose(str(info["description"])), ""]
        version = info.get("version")
        title = f", *{prose(str(info['title']))}*," if info.get("title") else ""
        lines += [
            f"The daemon serves this reference as an OpenAPI 3.1 document{title} at "
            + code("GET /api/openapi.json")
            + (f" (API version {code(str(version))})." if version else ".")
            + " Load it into an OpenAPI viewer such as Scalar or Swagger UI, or a client generator. "
            + "Tools that only read OpenAPI 3.0, such as Cloudflare API Shield's schema validation, "
            + "can use the same API restated as OpenAPI 3.0.3 at "
            + code("GET /api/openapi-3.0.json")
            + ".",
            "",
        ]
        lines += self.quick_start()
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

    def quick_start(self) -> list[str]:
        """Render the index's ``Quick start``: one ready-to-run request.

        The request is :data:`QUICK_START_PATH` when the document has a
        ``GET`` for it, else the first ``GET`` that needs a credential.

        Returns:
            Markdown lines: the heading, a sentence naming the route and its
            credential, the ``curl`` command and a link to the route; nothing
            when no ``GET`` needs a credential.
        """
        pages = [(group, op) for group in self.tags for op in group.operations if op.method == "GET" and op.spec.get("security")]
        chosen = next((pair for pair in pages if pair[1].path == QUICK_START_PATH), pages[0] if pages else None)
        if chosen is None:
            return []
        group, op = chosen
        command = ["```console", *sample_curl(self.request_example(op)), "```", ""]
        requirement = next(iter(spec_list(op.spec["security"], "security")), {})
        credential = f" with the {code(str(next(iter(requirement))))} credential" if isinstance(requirement, Mapping) and requirement else ""
        return [
            "## Quick start",
            "",
            f"The daemon listens on {code(EXAMPLE_ORIGIN)} unless `daemon.listen` says otherwise. "
            + f"This request calls {code(op.title)}{credential}; [Authentication](#authentication) says where to get it.",
            "",
            *command,
            f"[{code(op.title)}]({group.page}#{op.anchor}) describes the response.",
            "",
        ]

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
            lines += [nested(str(spec["description"]), 2), ""]
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
        lines += self.example_request(op)
        lines += self.example_response(op)
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
            target, _name = self.resolve(schema)
            rows = self.field_rows(target, here) if _inline_object(target) else []
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


class _NoExample:
    """Marks a schema an example cannot be built for (untyped or binary)."""


#: The one :class:`_NoExample` value.
NO_EXAMPLE = _NoExample()


@dataclass(frozen=True)
class Credential:
    """How a sample request authenticates."""

    # ``header`` sends ``name: value``; ``cookie`` sends the cookie ``name=value``.
    kind: str
    # The header or cookie name.
    name: str
    # Its value, with ``<scheme>`` placeholders for the secret.
    value: str


@dataclass(frozen=True)
class RequestBody:
    """A sample request's body."""

    # ``json`` (``value`` is the JSON value), ``form`` (``value`` is the
    # ``(name, example)`` pairs) or ``binary`` (a file of ``media``).
    kind: str
    # The ``Content-Type`` sent.
    media: str
    # See ``kind``.
    value: Any


@dataclass(frozen=True)
class RequestExample:
    """An operation's example request, which every sample language renders."""

    # The HTTP method, upper-case.
    method: str
    # The full URL, with ``<name>`` placeholders for path parameters.
    url: str
    # The credential sent, if the route needs one a sample can send.
    credential: Credential | None
    # Whether to send the daemon's ``Origin`` check (a cookie credential on a write).
    origin: bool
    # The body, if the operation takes one.
    body: RequestBody | None


def sample_curl(example: RequestExample) -> list[str]:
    """Render a request as a ``curl`` command in a ``console`` block.

    Args:
        example: The request.

    Returns:
        The command's lines: a ``$ `` prompt line and one flag per
        continuation line, with a JSON body on one line (the site's Copy
        button joins continuation lines only after a trailing backslash,
        not inside quotes).
    """
    flags = [] if example.method == "GET" else [f"-X {example.method}"]
    credential = example.credential
    if credential is not None and credential.kind == "header":
        flags.append("-H " + shell_quote(f"{credential.name}: {credential.value}"))
    elif credential is not None:
        flags.append("-b " + shell_quote(f"{credential.name}={credential.value}"))
    if example.origin:
        flags.append("-H " + shell_quote(f"Origin: {EXAMPLE_ORIGIN}"))
    body = example.body
    if body is not None and body.kind == "json":
        flags.append("-H " + shell_quote("Content-Type: application/json"))
        flags.append("-d " + shell_quote(json.dumps(body.value, ensure_ascii=False)))
    elif body is not None and body.kind == "form":
        flags.append("-d " + shell_quote(form_encode(body.value)))
    elif body is not None:
        flags.append("-H " + shell_quote(f"Content-Type: {body.media}"))
        flags.append("--data-binary @file")
    command = "$ curl " + shell_quote(example.url)
    if flags:
        command += " \\\n" + " \\\n".join(f"    {flag}" for flag in flags)
    return command.split("\n")


def _sample_headers(example: RequestExample) -> list[tuple[str, str]]:
    """The headers a non-``curl`` sample sends, in order: credential (a cookie as ``Cookie``), then ``Origin``."""
    headers: list[tuple[str, str]] = []
    credential = example.credential
    if credential is not None and credential.kind == "header":
        headers.append((credential.name, credential.value))
    elif credential is not None:
        headers.append(("Cookie", f"{credential.name}={credential.value}"))
    if example.origin:
        headers.append(("Origin", EXAMPLE_ORIGIN))
    return headers


def powershell_quote(text: str) -> str:
    """Quote text for PowerShell in single quotes, doubling each ``'``."""
    return "'" + text.replace("'", "''") + "'"


def sample_powershell(example: RequestExample) -> list[str]:
    """Render a request as an ``Invoke-RestMethod`` call.

    A cookie credential goes in a ``WebRequestSession`` passed as
    ``-WebSession``, since Windows PowerShell 5.1 ignores a ``Cookie``
    entry in ``-Headers``; the ``Origin`` header stays in ``-Headers``.
    """
    lines: list[str] = []
    parts = [f"-Method {example.method.capitalize()}"] if example.method != "GET" else []
    headers = _sample_headers(example)
    credential = example.credential
    if credential is not None and credential.kind == "cookie":
        host = urllib.parse.urlsplit(example.url).hostname or "localhost"
        lines += [
            "$session = New-Object Microsoft.PowerShell.Commands.WebRequestSession",
            "$session.Cookies.Add((New-Object System.Net.Cookie("
            f"{powershell_quote(credential.name)}, {powershell_quote(credential.value)}, '/', {powershell_quote(host)})))",
        ]
        parts.append("-WebSession $session")
        headers = [(name, value) for name, value in headers if name != "Cookie"]
    if headers:
        parts.append("-Headers @{ " + "; ".join(f"{name} = {powershell_quote(value)}" for name, value in headers) + " }")
    body = example.body
    if body is not None:
        parts.append(f"-ContentType {powershell_quote(body.media)}")
        if body.kind == "json":
            parts.append("-Body " + powershell_quote(json.dumps(body.value, ensure_ascii=False)))
        elif body.kind == "form":
            parts.append("-Body " + powershell_quote(form_encode(body.value)))
        else:
            parts.append("-InFile 'file'")
    command = f"Invoke-RestMethod -Uri {powershell_quote(example.url)}"
    if parts:
        command += " `\n" + " `\n".join(f"    {part}" for part in parts)
    return lines + command.split("\n")


def python_literal(value: Any) -> str:
    """Write a JSON value as a Python literal on one line."""
    if value is True or value is False or value is None:
        return {True: "True", False: "False", None: "None"}[value]
    if isinstance(value, list):
        return "[" + ", ".join(python_literal(item) for item in value) + "]"
    if isinstance(value, Mapping):
        return "{" + ", ".join(f"{json.dumps(str(k), ensure_ascii=False)}: {python_literal(v)}" for k, v in value.items()) + "}"
    return json.dumps(value, ensure_ascii=False)


def sample_python(example: RequestExample) -> list[str]:
    """Render a request with the ``requests`` library."""
    method = example.method.lower()
    call = f"requests.{method}" if method in ("get", "post", "put", "patch", "delete", "head", "options") else "requests.request"
    args = [json.dumps(example.method)] if call == "requests.request" else []
    args.append(json.dumps(example.url, ensure_ascii=False))
    headers = _sample_headers(example)
    body = example.body
    if body is not None and body.kind == "binary":
        headers.append(("Content-Type", body.media))
    if headers:
        args.append("headers=" + python_literal(dict(headers)))
    if body is not None and body.kind == "json":
        args.append("json=" + python_literal(body.value))
    elif body is not None and body.kind == "form":
        # ``requests`` sends a list value as repeated fields, like curl does.
        form: dict[str, Any] = {}
        for name, text in form_pairs(body.value):
            if name in form:
                form[name] = [*(form[name] if isinstance(form[name], list) else [form[name]]), text]
            else:
                form[name] = text
        args.append("data=" + python_literal(form))
    elif body is not None:
        args.append('data=open("file", "rb")')
    lines = ["import requests", ""]
    if len(args) == 1:
        lines.append(f"response = {call}({args[0]})")
    else:
        lines += [f"response = {call}(", *(f"    {arg}," for arg in args), ")"]
    lines.append("print(response.status_code, response.text)")
    return lines


def sample_javascript(example: RequestExample) -> list[str]:
    """Render a request with ``fetch``."""
    options: list[str] = []
    if example.method != "GET":
        options.append(f"method: {json.dumps(example.method)}")
    headers = _sample_headers(example)
    body = example.body
    if body is not None and body.kind != "form":
        headers.append(("Content-Type", body.media))
    if headers:
        pairs = ", ".join(f"{name if name.isalnum() else json.dumps(name)}: {json.dumps(value, ensure_ascii=False)}" for name, value in headers)
        options.append(f"headers: {{ {pairs} }}")
    if body is not None and body.kind == "json":
        options.append(f"body: JSON.stringify({json.dumps(body.value, ensure_ascii=False)})")
    elif body is not None and body.kind == "form":
        options.append(f"body: new URLSearchParams({json.dumps(form_encode(body.value))})")
    elif body is not None:
        options.append('body: await readFile("file")')
    lines = ['import { readFile } from "node:fs/promises";', ""] if body is not None and body.kind == "binary" else []
    url = json.dumps(example.url, ensure_ascii=False)
    if options:
        lines += [f"const response = await fetch({url}, {{", *(f"  {option}," for option in options), "});"]
    else:
        lines.append(f"const response = await fetch({url});")
    lines.append("console.log(response.status, await response.text());")
    return lines


def sample_rust(example: RequestExample) -> list[str]:
    """Render a request with ``reqwest``."""
    method = example.method.lower()
    url = json.dumps(example.url, ensure_ascii=False)
    headers = _sample_headers(example)
    body = example.body
    if body is not None and body.kind == "binary":
        headers.append(("Content-Type", body.media))
    if example.method == "GET" and not headers and body is None:
        return [f"let response = reqwest::get({url}).await?;", 'println!("{} {}", response.status(), response.text().await?);']
    calls = [f".{method}({url})" if method in ("get", "post", "put", "patch", "delete", "head") else f".request(reqwest::Method::{example.method}, {url})"]
    calls += [f".header({json.dumps(name)}, {json.dumps(value, ensure_ascii=False)})" for name, value in headers]
    if body is not None and body.kind == "json":
        calls.append(f".json(&serde_json::json!({json.dumps(body.value, ensure_ascii=False)}))")
    elif body is not None and body.kind == "form":
        pairs = ", ".join(f"({json.dumps(name)}, {json.dumps(text, ensure_ascii=False)})" for name, text in form_pairs(body.value))
        calls.append(f".form(&[{pairs}])")
    elif body is not None:
        calls.append('.body(std::fs::read("file")?)')
    return [
        "let client = reqwest::Client::new();",
        "let response = client",
        *(f"    {call}" for call in calls),
        "    .send()",
        "    .await?;",
        'println!("{} {}", response.status(), response.text().await?);',
    ]


# Each sample language's fence name and renderer, in the order the page lists them.
SAMPLE_LANGUAGES = {
    "console": sample_curl,
    "powershell": sample_powershell,
    "python": sample_python,
    "javascript": sample_javascript,
    "rust": sample_rust,
}


def form_pairs(pairs: Sequence[tuple[str, Any]]) -> list[tuple[str, str]]:
    """Flatten example values into the fields of a form, as text.

    Every sample language sends a form through this, so they all send the
    same body: an array repeats its name once per item, an object is sent as
    JSON text, a boolean or number as its JSON text, and a value with no
    example as empty.

    Args:
        pairs: Names and example values.

    Returns:
        ``(name, text)`` fields in order.
    """
    flat: list[tuple[str, str]] = []
    for name, value in pairs:
        for item in value if isinstance(value, list) else [value]:
            if item is NO_EXAMPLE:
                text = ""
            elif isinstance(item, str):
                text = item
            else:
                text = json.dumps(item, ensure_ascii=False)
            flat.append((name, text))
    return flat


def form_encode(pairs: Sequence[tuple[str, Any]]) -> str:
    """Encode example values as a query string or form body.

    Args:
        pairs: Names and example values, flattened by :func:`form_pairs`.

    Returns:
        ``name=value`` pairs joined by ``&``, percent-encoded.
    """
    return urlencode(form_pairs(pairs), quote_via=quote)


def shell_quote(text: str) -> str:
    """Quote text for a POSIX shell in single quotes.

    Args:
        text: Any text.

    Returns:
        The text in single quotes, with each ``'`` written as ``'\\''``.
    """
    return "'" + text.replace("'", "'\\''") + "'"


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
