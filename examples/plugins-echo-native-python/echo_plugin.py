#!/usr/bin/env python3
"""Historical native Python stdio entry.

This example now validates workerd (`modules/plugin.py`, `runtime = "workerd"`).
There is no Python Cap'n Proto guest stack. See README.md.
"""

from __future__ import annotations

import sys

if __name__ == "__main__":
    sys.stderr.write(
        "echo_native_python is a workerd guest (modules/plugin.py). "
        "Native Python stdio is not the product ABI.\n"
    )
    raise SystemExit(1)
