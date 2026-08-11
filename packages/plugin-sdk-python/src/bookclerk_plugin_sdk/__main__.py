"""Module entry point for ``python -m bookclerk_plugin_sdk``.

Delegates to :func:`bookclerk_plugin_sdk.cli.main` so the package CLI matches
the ``bookclerk-plugin`` console script.
"""

from .cli import main

raise SystemExit(main())
