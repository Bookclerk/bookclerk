#!/usr/bin/env python3
"""Dev Container, provision script and Dockerfile consistency checks.

The Dev Container image installs its tools through scripts/dev-env/provision.sh
so there is one list of what a development environment needs. These tests keep
that list, the CI tool pins, the base-image pins and the two Dev Container
configurations from drifting apart.
"""

from __future__ import annotations

import json
import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROVISION = ROOT / "scripts" / "dev-env" / "provision.sh"
DEVCONTAINER_DOCKERFILE = ROOT / ".devcontainer" / "Dockerfile"
DEVCONTAINER_DEFAULT = ROOT / ".devcontainer" / "devcontainer.json"
DEVCONTAINER_DESKTOP = ROOT / ".devcontainer" / "devcontainer.linux-desktop.json"
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
DEPENDABOT = ROOT / ".github" / "dependabot.yml"

# Keys the Linux desktop configuration must share verbatim with the default
# one; it only adds mounts, runArgs and desktop environment variables.
SHARED_DEVCONTAINER_KEYS = (
    "build",
    "workspaceFolder",
    "remoteUser",
    "updateRemoteUserUID",
    "forwardPorts",
    "portsAttributes",
    "postCreateCommand",
    "postStartCommand",
    "customizations",
)

# `FROM image:tag@sha256:<64 hex>` with an optional `AS name`.
PINNED_FROM = re.compile(r"^FROM\s+(?P<image>[^\s:@]+(?::\d+)?(?:/[^\s:@]+)*):(?P<tag>[^\s@]+)@sha256:[0-9a-f]{64}(?:\s+AS\s+\S+)?$", re.IGNORECASE)


def _tracked_dockerfiles() -> list[Path]:
    """Return every tracked Dockerfile in the repository.

    Returns:
        Absolute paths of tracked files named ``Dockerfile``.
    """
    out = subprocess.run(
        ["git", "ls-files", "*Dockerfile"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return [ROOT / line for line in out.splitlines() if line.endswith("Dockerfile")]


def _from_lines(dockerfile: Path) -> list[str]:
    """Return the FROM instructions of a Dockerfile.

    Args:
        dockerfile: Dockerfile to read.

    Returns:
        Each FROM line, stripped.
    """
    return [
        line.strip()
        for line in dockerfile.read_text(encoding="utf-8").splitlines()
        if line.strip().upper().startswith("FROM ")
    ]


def _is_build_arg_from(line: str) -> bool:
    """Tell whether a FROM line names its image through a build argument.

    Args:
        line: A FROM instruction.

    Returns:
        True when the image is ``${ARG}``, which Dependabot cannot pin.
    """
    return line.split()[1].startswith("$")


class ProvisionScriptTests(unittest.TestCase):
    """The provision script is the Dev Container's one tool list."""

    def test_nextest_version_matches_ci_pin(self) -> None:
        """cargo-nextest in the Dev Container is the version CI runs."""
        script = PROVISION.read_text(encoding="utf-8")
        ours = re.search(r'^NEXTEST_VERSION="([^"]+)"$', script, re.MULTILINE)
        self.assertIsNotNone(ours, "NEXTEST_VERSION not found in provision.sh")
        ci = re.findall(r"tool:\s*cargo-nextest@(\S+)", CI_WORKFLOW.read_text(encoding="utf-8"))
        self.assertTrue(ci, "cargo-nextest pin not found in ci.yml")
        self.assertEqual({ours.group(1)}, set(ci))

    def test_dockerfile_installs_through_provision_script(self) -> None:
        """The Dev Container Dockerfile delegates every package install."""
        dockerfile = DEVCONTAINER_DOCKERFILE.read_text(encoding="utf-8")
        self.assertIn("scripts/dev-env/provision.sh", dockerfile)
        for tool in ("apt-get install", "cargo install", "pip install", "rustup component add"):
            self.assertNotIn(tool, dockerfile, f"{tool} belongs in scripts/dev-env/provision.sh")


class BaseImageTests(unittest.TestCase):
    """Base images are tag-and-digest pinned, aligned, and tracked."""

    def test_every_from_is_tag_and_digest_pinned(self) -> None:
        """Dependabot needs the tag to track and the digest to pin."""
        for dockerfile in _tracked_dockerfiles():
            for line in _from_lines(dockerfile):
                if _is_build_arg_from(line):
                    continue
                with self.subTest(dockerfile=str(dockerfile.relative_to(ROOT)), line=line):
                    self.assertRegex(line, PINNED_FROM)

    def test_shared_images_use_one_digest(self) -> None:
        """An image used in several Dockerfiles is pinned to the same digest."""
        seen: dict[str, tuple[str, str]] = {}
        for dockerfile in _tracked_dockerfiles():
            for line in _from_lines(dockerfile):
                if _is_build_arg_from(line):
                    continue
                ref = line.split()[1]
                name_tag, digest = ref.split("@", 1)
                rel = str(dockerfile.relative_to(ROOT))
                if name_tag in seen:
                    other_digest, other_file = seen[name_tag]
                    with self.subTest(image=name_tag):
                        self.assertEqual(
                            digest,
                            other_digest,
                            f"{name_tag} pinned differently in {rel} and {other_file}",
                        )
                else:
                    seen[name_tag] = (digest, rel)

    def test_dependabot_covers_every_dockerfile_directory(self) -> None:
        """Every directory with a pinnable Dockerfile is in the docker ecosystem."""
        config = DEPENDABOT.read_text(encoding="utf-8")
        block = config.split("package-ecosystem: docker", 1)
        self.assertEqual(len(block), 2, "no docker ecosystem in dependabot.yml")
        listed = set(re.findall(r'^\s+-\s+"(/[^"]*)"\s*$', block[1].split("package-ecosystem:", 1)[0], re.MULTILINE))
        for dockerfile in _tracked_dockerfiles():
            if all(_is_build_arg_from(line) for line in _from_lines(dockerfile)):
                continue
            directory = "/" + str(dockerfile.parent.relative_to(ROOT))
            with self.subTest(directory=directory):
                self.assertIn(directory, listed)


class DevcontainerConfigTests(unittest.TestCase):
    """The Linux desktop configuration only adds to the default one."""

    def test_desktop_config_shares_default_keys(self) -> None:
        """Shared keys stay identical and default env vars carry over."""
        default = json.loads(DEVCONTAINER_DEFAULT.read_text(encoding="utf-8"))
        desktop = json.loads(DEVCONTAINER_DESKTOP.read_text(encoding="utf-8"))
        for key in SHARED_DEVCONTAINER_KEYS:
            with self.subTest(key=key):
                self.assertEqual(default.get(key), desktop.get(key))
        for section in ("containerEnv", "remoteEnv"):
            for name, value in default.get(section, {}).items():
                with self.subTest(section=section, name=name):
                    self.assertEqual(desktop.get(section, {}).get(name), value)


if __name__ == "__main__":
    unittest.main()
