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

import yaml

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

# `image:tag@sha256:<64 hex>`; the registry may carry a port.
PINNED_REF = re.compile(r"^[^\s:@/]+(?::\d+)?(?:/[^\s:@/]+)*:[^\s@:/]+@sha256:[0-9a-f]{64}$")

# Debian release codenames that may appear in base-image tags.
DEBIAN_CODENAMES = ("bullseye", "bookworm", "trixie", "forky")


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


def _base_images(dockerfile: Path) -> list[str]:
    """Return the external base images a Dockerfile builds from.

    Flags such as ``--platform`` are skipped, and FROM lines that name an
    earlier stage or a build argument are left out: Dependabot can pin
    neither, and neither pulls an image of its own.

    Args:
        dockerfile: Dockerfile to read.

    Returns:
        Each external image reference (``name:tag@sha256:...`` when pinned).
    """
    images: list[str] = []
    stages: set[str] = set()
    for raw in dockerfile.read_text(encoding="utf-8").splitlines():
        tokens = raw.split()
        if not tokens or tokens[0].upper() != "FROM":
            continue
        args = [t for t in tokens[1:] if not t.startswith("--")]
        if not args:
            continue
        image = args[0]
        if not image.startswith("$") and image.lower() not in stages:
            images.append(image)
        if len(args) >= 3 and args[1].upper() == "AS":
            stages.add(args[2].lower())
    return images


def _split_ref(ref: str) -> tuple[str, str, str]:
    """Split a pinned image reference into name, tag and digest.

    Args:
        ref: ``name:tag@sha256:<hex>``, as accepted by ``PINNED_REF``.

    Returns:
        The image name, tag and ``sha256:<hex>`` digest.
    """
    name_tag, digest = ref.split("@", 1)
    name, tag = name_tag.rsplit(":", 1)
    return name, tag, digest


def _action_inputs(action: str, key: str) -> list[str]:
    """Return one input of every step that uses an action, across the workflows.

    Args:
        action: Action name without the ref, e.g. ``astral-sh/setup-uv``.
        key: Input under the step's ``with:`` mapping.

    Returns:
        The input's value for each such step; a step without it yields ``""``
        so a missing pin fails the comparison instead of being skipped.
    """
    found: list[str] = []
    for workflow in sorted((ROOT / ".github" / "workflows").glob("*.yml")):
        jobs = yaml.safe_load(workflow.read_text(encoding="utf-8")).get("jobs", {})
        for job in jobs.values():
            for step in job.get("steps", []):
                if str(step.get("uses", "")).split("@", 1)[0] == action:
                    found.append(str((step.get("with") or {}).get(key, "")))
    return found


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

    def test_image_tools_match_ci_versions(self) -> None:
        """uv and the Node.js major in the Dev Container are the ones CI sets up."""
        images = {_split_ref(ref)[0]: _split_ref(ref)[1] for ref in _base_images(DEVCONTAINER_DOCKERFILE)}
        uv = _action_inputs("astral-sh/setup-uv", "version")
        node = _action_inputs("actions/setup-node", "node-version")
        self.assertTrue(uv and node, "no setup-uv or setup-node step found")
        self.assertEqual({images["ghcr.io/astral-sh/uv"]}, set(uv))
        self.assertEqual({images["node"].split("-", 1)[0]}, set(node))

    def test_session_setup_finds_node_and_uv_images(self) -> None:
        """session-setup.sh copies Node.js and uv out of these two FROM lines."""
        names = {_split_ref(ref)[0] for ref in _base_images(DEVCONTAINER_DOCKERFILE)}
        self.assertIn("node", names)
        self.assertIn("ghcr.io/astral-sh/uv", names)

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
            for ref in _base_images(dockerfile):
                with self.subTest(dockerfile=str(dockerfile.relative_to(ROOT)), ref=ref):
                    self.assertRegex(ref, PINNED_REF)

    def test_shared_images_use_one_tag_and_digest(self) -> None:
        """An image used in several Dockerfiles is pinned identically."""
        seen: dict[str, tuple[str, str]] = {}
        for dockerfile in _tracked_dockerfiles():
            rel = str(dockerfile.relative_to(ROOT))
            for ref in _base_images(dockerfile):
                name, tag, digest = _split_ref(ref)
                if name not in seen:
                    seen[name] = (f"{tag}@{digest}", rel)
                    continue
                with self.subTest(image=name):
                    self.assertEqual(
                        f"{tag}@{digest}",
                        seen[name][0],
                        f"{name} pinned differently in {rel} and {seen[name][1]}",
                    )

    def test_one_debian_release(self) -> None:
        """Every Debian-based image tag names the same release."""
        releases: dict[str, str] = {}
        for dockerfile in _tracked_dockerfiles():
            for ref in _base_images(dockerfile):
                _, tag, _ = _split_ref(ref)
                for codename in DEBIAN_CODENAMES:
                    if codename in tag:
                        releases[f"{dockerfile.relative_to(ROOT)}: {ref.split('@', 1)[0]}"] = codename
        self.assertTrue(releases, "no Debian-based base image found")
        self.assertEqual(len(set(releases.values())), 1, releases)

    def test_dependabot_covers_every_dockerfile_directory(self) -> None:
        """Every directory with a pinnable Dockerfile is in the docker ecosystem."""
        config = DEPENDABOT.read_text(encoding="utf-8")
        block = config.split("package-ecosystem: docker", 1)
        self.assertEqual(len(block), 2, "no docker ecosystem in dependabot.yml")
        listed = set(re.findall(r'^\s+-\s+"(/[^"]*)"\s*$', block[1].split("package-ecosystem:", 1)[0], re.MULTILINE))
        for dockerfile in _tracked_dockerfiles():
            if not _base_images(dockerfile):
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
