#!/usr/bin/env python3
"""Publisher workflow selects bookclerk-plugin-tools from Cargo metadata."""

from __future__ import annotations

import json
import subprocess
import unittest
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parents[2]
WORKFLOW = REPO / ".github" / "workflows" / "plugin-publisher-reusable.yml"


def detector_source() -> str:
    """Python that the pack step runs after YAML indentation is removed."""
    doc = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
    run = next(
        step["run"]
        for job in doc["jobs"].values()
        for step in job.get("steps") or []
        if step.get("id") == "pack"
    )
    start = run.index("cat <<'PY'\n") + len("cat <<'PY'\n")
    end = run.index("\nPY\n", start)
    return run[start:end] + "\n"


def detect(metadata: dict) -> int:
    proc = subprocess.run(
        ["python3", "-c", detector_source()],
        input=json.dumps(metadata),
        text=True,
        check=False,
    )
    return proc.returncode


class PublisherToolDetectionTests(unittest.TestCase):
    def test_sdk_without_tools_uses_archive_fallback(self) -> None:
        self.assertEqual(detect({"packages": [{"name": "bookclerk-plugin-sdk", "targets": []}]}), 1)

    def test_tools_package_and_binary_use_tools_packaging(self) -> None:
        metadata = {
            "packages": [
                {
                    "name": "bookclerk-plugin-tools",
                    "targets": [{"name": "bookclerk-plugin", "kind": ["bin"]}],
                }
            ]
        }
        self.assertEqual(detect(metadata), 0)

    def test_dependency_name_is_not_tool_detection(self) -> None:
        metadata = {
            "packages": [
                {
                    "name": "bookclerk-plugin-sdk",
                    "dependencies": [{"name": "bookclerk-plugin-tools"}],
                    "targets": [{"name": "bookclerk-plugin", "kind": ["bin"]}],
                }
            ]
        }
        self.assertEqual(detect(metadata), 1)


if __name__ == "__main__":
    unittest.main()
