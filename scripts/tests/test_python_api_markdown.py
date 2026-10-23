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

        __all__ = ["VERSION", "Worker", "run"]
        ''',
    "pkg/_consts.py": '''
        VERSION: str = "3"
        """ABI version the package speaks."""
        ''',
    "pkg/worker.py": '''
        """Workers and how to run them."""

        import json

        LIMIT = 8
        """Most jobs at once."""

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

    def test_module_page(self) -> None:
        page = self.pages["pkg.worker.md"]
        self.assertNotIn("_hidden", page)
        self.assertNotIn("_helper", page)
        self.assertNotIn("_private", page)
        self.assertNotIn("__init__", page)
        self.assertNotIn("json\n", page.split("## Classes")[0].split("| --- |")[0])
        self.assertIn("| [`Worker`](#worker) | class | Does the work. |", page)
        self.assertIn("| [`LIMIT`](#limit) | constant | Most jobs at once. |", page)
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
        self.assertEqual(pm.module_summary(self.src, "pkg.worker"), "Workers and how to run them.")
        self.assertEqual(pm.module_summary(self.src, "pkg.missing"), "")


if __name__ == "__main__":
    unittest.main()
