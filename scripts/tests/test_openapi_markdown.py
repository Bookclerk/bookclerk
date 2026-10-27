#!/usr/bin/env python3
"""openapi_markdown.py: pages, types, anchors and escaping (offline).

Most tests render a small hand-written document; one renders the daemon's
committed document and checks that every link on every page lands on a
heading that exists.
"""

from __future__ import annotations

import itertools
import json
import re
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import openapi_markdown as om  # noqa: E402
from rustdoc_markdown import Anchors, slug  # noqa: E402

DOCUMENT = {
    "openapi": "3.1.0",
    "info": {"title": "Test API", "description": "What it is.", "version": "1.2.3"},
    "tags": [{"name": "Books", "description": "The books."}, {"name": "Empty"}],
    "paths": {
        "/books/{id}": {
            "get": {
                "tags": ["Books"],
                "summary": "Reads a book.",
                "x-bookclerk-access": "Any\n  session.",
                "security": [{"token": []}, {"cookie": []}],
                "parameters": [
                    {"name": "full", "in": "query", "schema": {"type": "boolean", "default": False}},
                    {"name": "id", "in": "path", "required": True, "description": "Book id.", "schema": {"type": "integer"}},
                ],
                "responses": {
                    "200": {"description": "The book.", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/Book"}}}},
                    "404": {"description": "No | such book."},
                },
            },
            "delete": {"tags": ["Books"], "summary": "Deletes it.", "deprecated": True, "security": [{}], "responses": {"204": {"description": "Gone."}}},
        },
        "/books": {
            "post": {
                "tags": ["Books"],
                "summary": "Adds a book.",
                "requestBody": {
                    "required": True,
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "required": ["title"],
                                "properties": {
                                    "title": {"type": "string", "description": "Shown\non two lines."},
                                    "meta": {"type": "object", "properties": {"pages": {"type": ["integer", "null"], "minimum": 1}}},
                                },
                            }
                        }
                    },
                },
                "responses": {"201": {"description": "Added.", "content": {"image/png": {"schema": {"type": "string", "format": "binary"}}}}},
            }
        },
        "/shelves/{shelf}": {
            "parameters": [
                {"name": "shelf", "in": "path", "required": True, "description": "Shared.", "schema": {"type": "string"}},
                {"name": "lang", "in": "query", "description": "Path level.", "schema": {"type": "string"}},
            ],
            "get": {
                "tags": ["Books"],
                "summary": "Lists a shelf.",
                "security": [],
                "parameters": [{"name": "lang", "in": "query", "description": "Operation level.", "schema": {"type": "string"}}],
                "responses": {"200": {"description": "The shelf."}},
            },
        },
        "/untagged": {"get": {"summary": "Odd one.", "responses": {"200": {"description": "Anything.", "content": {"application/json": {"schema": {}}}}}}},
    },
    "components": {
        "securitySchemes": {
            "token": {"type": "http", "scheme": "bearer", "description": "A token."},
            "cookie": {"type": "apiKey", "in": "cookie", "name": "sid", "description": "A cookie."},
        },
        "schemas": {
            "Book": {
                "type": "object",
                "description": "A book.",
                "required": ["id", "status"],
                "properties": {
                    "id": {"type": "integer", "format": "int64", "description": "Its id."},
                    "status": {"$ref": "#/components/schemas/Status"},
                    "tags": {"type": "array", "items": {"type": "string"}, "uniqueItems": True},
                    "extra": {"type": "object", "additionalProperties": {"$ref": "#/components/schemas/Status"}},
                    "count": {"oneOf": [{"type": "integer", "minimum": 0}, {"type": "string", "enum": ["all"]}], "description": "How many."},
                },
            },
            "Wrapped": {"allOf": [{"$ref": "#/components/schemas/Book"}, {"type": "object", "properties": {"note": {"type": "string"}}}]},
            "Status": {"type": "string", "enum": ["new", "old"], "description": "Book state."},
        },
    },
}


class RenderTest(unittest.TestCase):
    """Pages rendered from a small document."""

    def setUp(self) -> None:
        self.pages = om.Document(DOCUMENT).render_all()

    def test_pages(self) -> None:
        # A tag without operations gets no page; an untagged route goes under Other.
        self.assertEqual(sorted(self.pages), ["README.md", "books.md", "other.md", "schemas.md"])

    def test_index_lists_credentials_and_routes(self) -> None:
        index = self.pages["README.md"]
        self.assertIn("# Test API", index)
        self.assertIn("(API version `1.2.3`)", index)
        self.assertIn("| `token` | HTTP bearer | A token. |", index)
        self.assertIn("| `cookie` | cookie `sid` | A cookie. |", index)
        self.assertIn("| [`GET /books/{id}`](books.md#get-booksid) | Reads a book. |", index)
        self.assertIn("| [`DELETE /books/{id}`](books.md#delete-booksid) (deprecated) | Deletes it. |", index)
        self.assertLess(index.index("GET /books/{id}"), index.index("DELETE /books/{id}"))
        self.assertNotIn("Empty", index)

    def test_operation_sections(self) -> None:
        page = self.pages["books.md"]
        self.assertIn("## `GET /books/{id}`", page)
        self.assertIn("**Access:** Any session.", page)
        self.assertIn("**Credentials:** `token` or `cookie`", page)
        # Path parameters come first and are always required.
        self.assertLess(page.index("| `id` | path |"), page.index("| `full` | query |"))
        self.assertIn("| `id` | path | integer | yes | Book id. |", page)
        self.assertIn("| `full` | query | boolean | no | default `false` |", page)
        self.assertIn("| 200 | The book. | [`Book`](schemas.md#book) (`application/json`) |", page)
        self.assertIn("| 404 | No \\| such book. |  |", page)
        self.assertIn("**Deprecated.**", page)
        self.assertIn("**Credentials:** none", page)

    def test_path_parameters_merge_and_open_security(self) -> None:
        shelf = self.pages["books.md"].split("## `GET /shelves/{shelf}`")[1]
        self.assertIn("| `shelf` | path | string | yes | Shared. |", shelf)
        # The operation's own parameter replaces the path's one of the same name and place.
        self.assertIn("| `lang` | query | string | no | Operation level. |", shelf)
        self.assertNotIn("Path level.", shelf)
        # An empty security list means the route is open.
        self.assertIn("**Credentials:** none", shelf)

    def test_request_body_fields_flatten_inline_objects(self) -> None:
        page = self.pages["books.md"]
        self.assertIn("`application/json`: object", page)
        self.assertIn("| `title` | string | yes | Shown on two lines. |", page)
        self.assertIn("| `meta.pages` | integer or null | no | at least 1 |", page)
        self.assertIn("| 201 | Added. | binary (`image/png`) |", page)
        self.assertNotIn("Optional.", page.split("## `POST /books`")[1])

    def test_example_request(self) -> None:
        get = self.pages["books.md"].split("## `GET /books/{id}`")[1].split("\n## ")[0]
        self.assertIn(
            "### Example request\n\n```console\n$ curl 'http://127.0.0.1:8787/books/<id>' \\\n    -H 'Authorization: Bearer <token>'\n```",
            get,
        )
        post = self.pages["books.md"].split("## `POST /books`")[1]
        self.assertIn("    -X POST \\\n    -H 'Content-Type: application/json' \\\n    -d '{\n  \"title\": \"string\",", post)
        # An open route sends no credential.
        shelf = self.pages["books.md"].split("## `GET /shelves/{shelf}`")[1]
        self.assertIn("$ curl 'http://127.0.0.1:8787/shelves/<shelf>'\n```", shelf)

    def test_example_response(self) -> None:
        get = self.pages["books.md"].split("## `GET /books/{id}`")[1].split("## `DELETE")[0]
        self.assertIn("### Example response\n\nStatus `200`:\n\n```json\n", get)
        body = json.loads(get.split("```json\n")[1].split("\n```")[0])
        self.assertEqual(body, {"id": 0, "status": "new", "tags": ["string"], "extra": {"key": "new"}, "count": 0})
        # Untyped and binary bodies get no example.
        self.assertNotIn("Example response", self.pages["other.md"])
        self.assertNotIn("Example response", self.pages["books.md"].split("## `POST /books`")[1])

    def test_examples_follow_references_and_stop_at_cycles(self) -> None:
        doc = om.Document(
            {
                **DOCUMENT,
                "components": {
                    "schemas": {
                        "Node": {"type": "object", "properties": {"next": {"oneOf": [{"$ref": "#/components/schemas/Node"}, {"type": "null"}]}, "at": {"type": "string", "format": "date-time"}}}
                    }
                },
            }
        )
        self.assertEqual(doc.example({"$ref": "#/components/schemas/Node"}), {"next": {}, "at": "2026-01-01T12:00:00Z"})
        self.assertIs(doc.example({}), om.NO_EXAMPLE)
        self.assertEqual(doc.example({"type": "string", "example": "x", "enum": ["y"]}), "x")

    def test_shell_quote(self) -> None:
        self.assertEqual(om.shell_quote("it's"), "'it'\\''s'")

    def test_unknown_schemas_are_any_json(self) -> None:
        self.assertIn("any JSON (`application/json`)", self.pages["other.md"])

    def test_schema_page(self) -> None:
        schemas = self.pages["schemas.md"]
        self.assertIn("| `id` | integer (int64) | yes | Its id. |", schemas)
        self.assertIn("| `status` | [`Status`](#status) | yes |  |", schemas)
        self.assertIn("| `tags` | array of string | no | no duplicates |", schemas)
        self.assertIn("| `extra` | map of string to [`Status`](#status) | no |  |", schemas)
        self.assertIn("| `count` | integer or `all` | no | How many. |", schemas)
        self.assertIn("Type: one of `new`, `old`", schemas)
        self.assertIn("Includes every field of [`Book`](#book).", schemas)
        self.assertIn("| `note` | string | no |  |", schemas)
        # Alphabetical, whatever order the document lists them in.
        self.assertLess(schemas.index("## Status"), schemas.index("## Wrapped"))

    def test_no_raw_html(self) -> None:
        for name, text in self.pages.items():
            outside_code = re.sub(r"`[^`]*`", "", text)
            self.assertNotRegex(outside_code, r"<[a-zA-Z/]", name)


class HelpersTest(unittest.TestCase):
    """Escaping and type descriptions."""

    def test_code_spans_survive_backticks(self) -> None:
        self.assertEqual(om.code("a"), "`a`")
        self.assertEqual(om.code("a`b"), "``a`b``")
        self.assertEqual(om.code("`x"), "`` `x ``")

    def test_cell_folds_lines_and_escapes_pipes(self) -> None:
        self.assertEqual(om.cell("a |\n\n b"), "a \\| b")

    def test_nested_demotes_headings_outside_fences(self) -> None:
        text = "Intro.\n\n# Errors\n\n```\n# comment\n```\n\n##### Deep\n#hashtag"
        self.assertEqual(
            om.nested(text, 2),
            "Intro.\n\n### Errors\n\n```\n# comment\n```\n\n###### Deep\n#hashtag",
        )

    def test_description_joins_constraints(self) -> None:
        document = om.Document(DOCUMENT)
        self.assertEqual(document.describe({"description": "Size.", "maximum": 3}), "Size. At most 3.")
        self.assertEqual(document.describe({"description": "Size", "maximum": 3}), "Size (at most 3)")

    def test_rejects_documents_it_cannot_read(self) -> None:
        for bad in ({"swagger": "2.0"}, {"openapi": "3.0.0", "paths": []}, {"openapi": "3.0.0", "tags": {}}):
            with self.subTest(bad=bad), self.assertRaises(om.OpenApiError):
                om.Document(bad)

    def test_render_reports_invalid_json(self) -> None:
        with tempfile.TemporaryDirectory(prefix="openapi-md-") as tmp:
            path = Path(tmp) / "bad.json"
            path.write_text("{", encoding="utf-8")
            with self.assertRaises(om.OpenApiError):
                om.render(path, Path(tmp) / "out")


HEADING_RE = re.compile(r"^#{1,6} (.+)$", re.MULTILINE)
LINK_RE = re.compile(r"\]\(([^)\s]*)\)")


class DaemonDocumentTest(unittest.TestCase):
    """The committed document renders, and every link resolves to a heading."""

    def test_links_resolve(self) -> None:
        spec = json.loads((ROOT / "crates" / "bookclerkd" / "openapi.json").read_text(encoding="utf-8"))
        pages = om.Document(spec).render_all()
        anchors = {}
        for name, text in pages.items():
            seen = Anchors()
            anchors[name] = {seen.take(heading) for heading in HEADING_RE.findall(text)}
        operations = 0
        for name, text in pages.items():
            operations += text.count("\n## `") if name not in ("README.md", "schemas.md") else 0
            for href in LINK_RE.findall(text):
                page, _, anchor = href.partition("#")
                page = page or name
                with self.subTest(page=name, href=href):
                    self.assertIn(page, pages)
                    if anchor:
                        self.assertIn(anchor, anchors[page])
        routes = sum(1 for item in spec["paths"].values() for method in om.METHODS if method in item)
        self.assertEqual(operations, routes)
        self.assertEqual(slug("`GET /api/openapi.json`"), "get-apiopenapijson")

    def test_route_pages_keep_heading_order(self) -> None:
        spec = json.loads((ROOT / "crates" / "bookclerkd" / "openapi.json").read_text(encoding="utf-8"))
        for name, text in om.Document(spec).render_all().items():
            levels = [len(m) for m in re.findall(r"^(#{1,6}) ", re.sub(r"```.*?```", "", text, flags=re.DOTALL), re.MULTILINE)]
            with self.subTest(page=name):
                self.assertEqual(levels[:1], [1])
                self.assertNotIn(1, levels[1:])
                for before, after in itertools.pairwise(levels):
                    self.assertLessEqual(after, before + 1)


if __name__ == "__main__":
    unittest.main()
