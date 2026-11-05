"""Collection checks shared by every test module of the Python SDK."""

from __future__ import annotations

import pytest


def pytest_collection_modifyitems(items: list[pytest.Item]) -> None:
    """Refuse test ids that cannot be passed back to pytest by node id.

    pytest escapes backslashes and non-ASCII characters in the node ids it
    reports, and such an id is then not found on its command line. The
    nightly mutation run (``.github/workflows/mutation-python.yml``) selects
    tests by node id, so one such id fails its whole baseline run.

    Args:
        items: The collected tests.

    Raises:
        pytest.UsageError: A test id holds a backslash or a non-ASCII character.
    """
    bad = [item.nodeid for item in items if "\\" in item.nodeid or not item.nodeid.isascii()]
    if bad:
        raise pytest.UsageError(
            "test ids must be ASCII without backslashes (mutmut selects tests by node id): "
            + ", ".join(bad)
        )
