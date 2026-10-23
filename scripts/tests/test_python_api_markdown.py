#!/usr/bin/env python3
"""python_api_markdown.py: public names, re-exports, docstrings and links (offline).

Each test writes a tiny package into a temporary directory; nothing in it is
imported.
"""

from __future__ import annotations

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import python_api_markdown as pm  # noqa: E402

FILES = {
    "pkg/__init__.py": '''
        """The package.

        Start with :mod:`pkg.worker` and :class:`~pkg.worker.Worker`.
        """

        from ._consts import VERSION
        from .worker import Worker, run
        from .worker import LIMIT as MAX_JOBS
        from .worker import Unlisted

        try:
            from ._fast import encode
        except ImportError:
            from ._consts import encode

        __all__ = ["VERSION", "Worker", "run"] + ["MAX_JOBS"]
        __all__ += ["Unlisted", "encode"]
        ''',
    "pkg/_consts.py": '''
        VERSION: str = "3"
        """ABI version the package speaks."""


        def encode(value: str) -> bytes:
            """Encode a value for the wire."""
        ''',
    "pkg/worker.py": '''
        """Workers and how to run them."""

        import json

        __all__ = ["LIMIT", "Worker", "run"]

        LIMIT = 8
        """Most jobs at once."""

        type Pair = tuple[int, int]
        """Two job ids."""

        __all__.append("Pair")

        _hidden = 1


        class Worker(Base):
            """Does the work.

            Attributes:
                name: Display name.
            """

            name: str = "w"

            def __init__(self) -> None:
                pass

            def handle(self, job: dict[str, int], *, retry: bool = False) -> int:
                """Handle one job.

                Uses ``json``; see :func:`run`.

                Args:
                    job (dict): The job, decoded.
                        Keys are names.
                    retry: Try again on failure.

                Returns:
                    The exit status.

                Raises:
                    ValueError: The job is malformed.

                Example:
                    >>> Worker().handle({})
                    0
                """
                return 0

            def _private(self) -> None:
                """Not shown."""


        async def run(worker: Worker) -> None:
            """Run ``worker`` until it stops.

            The wire looks like this::

                {"op": "run"}
            """


        def _helper() -> None:
            pass


        class Unlisted:
            """Defined here but left out of this module's __all__."""
        ''',
}


class PackageTest(unittest.TestCase):
    """A package renders one page per public module."""

    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = tempfile.TemporaryDirectory(prefix="python-api-markdown-")
        src = Path(cls._tmp.name)
        for rel, text in FILES.items():
            (src / rel).parent.mkdir(parents=True, exist_ok=True)
            (src / rel).write_text(textwrap.dedent(text).lstrip())
        cls.src = src
        cls.pages = dict(pm.render(src, ["pkg", "pkg.worker"]))

    @classmethod
    def tearDownClass(cls) -> None:
        cls._tmp.cleanup()

    def test_pages(self) -> None:
        self.assertEqual(sorted(self.pages), ["pkg.md", "pkg.worker.md"])

    def test_package_page(self) -> None:
        page = self.pages["pkg.md"]
        self.assertTrue(page.startswith("# pkg\n\nThe package.\n\nStart with [`pkg.worker`](pkg.worker.md) and [`Worker`](pkg.worker.md#worker)."))
        # A name from a private module is documented where it is exported.
        self.assertIn('### VERSION\n\n```python\nVERSION: str = \'3\'\n```\n\nABI version the package speaks.', page)
        # A name from a public module links there.
        self.assertIn("- [`Worker`](pkg.worker.md#worker) from `pkg.worker`", page)
        self.assertIn("- [`run`](pkg.worker.md#run) from `pkg.worker`", page)
        # An alias links to the name the item is documented under.
        self.assertIn("- [`MAX_JOBS` (`LIMIT`)](pkg.worker.md#limit) from `pkg.worker`", page)
        # Not documented by its own module, so documented here.
        self.assertIn("### Unlisted\n", page)
        self.assertNotIn("(pkg.worker.md#unlisted)", page)
        # Imported in a try block.
        self.assertIn("### encode\n", page)

    def test_non_literal_all_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory(prefix="python-api-markdown-") as tmp:
            (Path(tmp) / "pkg").mkdir()
            (Path(tmp) / "pkg/__init__.py").write_text("from . import other\n__all__ = other.__all__\n")
            with self.assertRaisesRegex(ValueError, "__all__ is not built from string literals"):
                pm.render(Path(tmp), ["pkg"])

    def test_type_aliases_without_all_are_public(self) -> None:
        with tempfile.TemporaryDirectory(prefix="python-api-markdown-") as tmp:
            (Path(tmp) / "pkg").mkdir()
            (Path(tmp) / "pkg/__init__.py").write_text(
                "from typing import Callable, TypeVar, Union\n"
                "DbValue = Union[int, str]\nId = str | int\nT = TypeVar('T')\n"
                "Hook = Callable[[int], None]\nMode = 3\nhelper = dict()\n"
            )
            page = dict(pm.render(Path(tmp), ["pkg"]))["pkg.md"]
            for name in ("DbValue", "Id", "T", "Hook"):
                self.assertIn(f"### {name}\n", page)
            self.assertNotIn("### Mode", page)
            self.assertNotIn("### helper", page)

    def test_unreadable_all_change_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory(prefix="python-api-markdown-") as tmp:
            (Path(tmp) / "pkg").mkdir()
            (Path(tmp) / "pkg/__init__.py").write_text('__all__ = ["a", "b"]\n__all__.remove("b")\n')
            with self.assertRaisesRegex(ValueError, r"__all__ is changed with \.remove\(\)"):
                pm.render(Path(tmp), ["pkg"])

    def test_module_page(self) -> None:
        page = self.pages["pkg.worker.md"]
        self.assertNotIn("_hidden", page)
        self.assertNotIn("_helper", page)
        self.assertNotIn("_private", page)
        self.assertNotIn("__init__", page)
        self.assertNotIn("json\n", page.split("## Classes")[0].split("| --- |")[0])
        self.assertIn("| [`Worker`](#worker) | class | Does the work. |", page)
        self.assertIn("| [`LIMIT`](#limit) | constant | Most jobs at once. |", page)
        # Added with __all__.append; a `type` alias summarizes its own doc.
        self.assertIn("| [`Pair`](#pair) | constant | Two job ids. |", page)
        self.assertIn("```python\nclass Worker(Base)\n```", page)
        self.assertIn("**Attributes**\n\n- `name`: Display name.", page)
        self.assertIn("#### Worker.handle\n\n```python\ndef handle(self, job: dict[str, int], *, retry: bool=False) -> int\n```", page)
        self.assertIn("Uses `json`; see [`run`](#run).", page)
        self.assertIn("**Args**\n\n- `job` (dict): The job, decoded. Keys are names.\n- `retry`: Try again on failure.", page)
        self.assertIn("**Returns**\n\nThe exit status.", page)
        self.assertIn("**Raises**\n\n- `ValueError`: The job is malformed.", page)
        self.assertIn("**Example**\n\n```python\n>>> Worker().handle({})\n0\n```", page)
        self.assertIn("```python\nasync def run(worker: Worker) -> None\n```", page)
        self.assertIn("The wire looks like this:\n\n```\n{\"op\": \"run\"}\n```", page)
        self.assertNotIn("\n\n\n", page)

    def test_module_summary(self) -> None:
        package = pm.Package(self.src, ["pkg", "pkg.worker"])
        self.assertEqual(package.summary("pkg.worker"), "Workers and how to run them.")
        self.assertEqual(package.summary("pkg.missing"), "")


if __name__ == "__main__":
    unittest.main()
