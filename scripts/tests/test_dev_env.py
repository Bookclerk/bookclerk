#!/usr/bin/env python3
"""Dev Container, provision script and Dockerfile consistency checks.

The Dev Container image installs its tools through scripts/dev-env/provision.sh
so there is one list of what a development environment needs. These tests keep
that list, the CI tool pins, the base-image pins and the two Dev Container
configurations from drifting apart.
"""

from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import sys
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]


def _load(name: str, path: Path):
    """Import a script that is not on the module path."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


tool_versions = _load("tool_versions", ROOT / "scripts" / "tool_versions.py")
PROVISION = ROOT / "scripts" / "dev-env" / "provision.sh"
DEVCONTAINER_DOCKERFILE = ROOT / ".devcontainer" / "Dockerfile"
DEVCONTAINER_DEFAULT = ROOT / ".devcontainer" / "devcontainer.json"
DEVCONTAINER_DESKTOP = ROOT / ".devcontainer" / "devcontainer.linux-desktop.json"
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
DEPENDABOT = ROOT / ".github" / "dependabot.yml"
PUBLISH_WORKFLOW = ROOT / ".github" / "workflows" / "images-publish.yml"
TOOL_VERSIONS = ROOT / ".tool-versions"

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


def _tool_versions() -> dict[str, str]:
    """Return the central tool pins in ``.tool-versions``.

    Returns:
        Tool name to version, as ``scripts/tool_versions.py`` parses them.
    """
    return tool_versions.parse_pins(TOOL_VERSIONS.read_text(encoding="utf-8"))


def _setup_steps(action: str) -> list[tuple[str, dict]]:
    """Return every step that uses an action, across the workflows.

    Args:
        action: Action name without the ref, e.g. ``astral-sh/setup-uv``.

    Returns:
        ``(workflow:job, step)`` for each such step.
    """
    found: list[tuple[str, dict]] = []
    for workflow in sorted((ROOT / ".github" / "workflows").glob("*.yml")):
        jobs = yaml.safe_load(workflow.read_text(encoding="utf-8")).get("jobs", {})
        for name, job in jobs.items():
            for step in job.get("steps", []):
                if str(step.get("uses", "")).split("@", 1)[0] == action:
                    found.append((f"{workflow.name}:{name}", step))
    return found


class ProvisionScriptTests(unittest.TestCase):
    """The provision script is the Dev Container's one tool list."""

    def test_nextest_comes_from_tool_versions(self) -> None:
        """provision.sh and CI both install the ``cargo-nextest`` pin in ``.tool-versions``."""
        script = PROVISION.read_text(encoding="utf-8")
        self.assertIn('awk \'$1 == "cargo-nextest" { print $2 }\' "${repo}/.tool-versions"', script)
        self.assertNotRegex(script, r'NEXTEST_VERSION="[0-9]')
        ci = re.findall(r"tool:\s*cargo-nextest@(.+)$", CI_WORKFLOW.read_text(encoding="utf-8"), re.MULTILINE)
        self.assertTrue(ci, "cargo-nextest install not found in ci.yml")
        self.assertEqual({"${{ steps.pins.outputs.cargo-nextest }}"}, set(ci))
        dockerfile = DEVCONTAINER_DOCKERFILE.read_text(encoding="utf-8")
        self.assertIn("COPY .tool-versions /tmp/bookclerk-dev/.tool-versions", dockerfile)

    def test_tool_versions_parses(self) -> None:
        """``.tool-versions`` holds exactly the pins ``scripts/tool_versions.py`` knows."""
        pins = _tool_versions()
        self.assertEqual(tool_versions.parse_pins(TOOL_VERSIONS.read_text(encoding="utf-8")), pins)
        # actions/setup-node takes the first line its pattern matches,
        # `^(?:node(js)?\s+)?v?(?<version>[^\s]+)$` (multiline), so check it
        # reads our Node.js pin rather than another tool's line or a comment.
        found = re.search(
            r"^(?:node(js)?\s+)?v?(?P<version>[^\s]+)$",
            TOOL_VERSIONS.read_text(encoding="utf-8"),
            re.MULTILINE,
        )
        self.assertIsNotNone(found)
        self.assertEqual(pins["nodejs"], found.group("version"))

    def test_workflows_read_node_and_uv_from_tool_versions(self) -> None:
        """Every setup-node and setup-uv step reads ``.tool-versions``, never a literal."""
        for action, file_key, literal_key in (
            ("actions/setup-node", "node-version-file", "node-version"),
            ("astral-sh/setup-uv", "version-file", "version"),
        ):
            steps = _setup_steps(action)
            self.assertTrue(steps, f"no {action} step found")
            for where, step in steps:
                with self.subTest(action=action, job=where):
                    inputs = step.get("with") or {}
                    self.assertEqual(".tool-versions", inputs.get(file_key))
                    self.assertNotIn(literal_key, inputs)

    def test_images_match_tool_versions(self) -> None:
        """Every Node.js and uv base image is the version ``.tool-versions`` pins."""
        pins = _tool_versions()
        seen: set[str] = set()
        for dockerfile in _tracked_dockerfiles():
            for ref in _base_images(dockerfile):
                if "@" not in ref:
                    continue
                name, tag, _ = _split_ref(ref)
                where = f"{dockerfile.relative_to(ROOT)}: {name}"
                with self.subTest(image=where):
                    if name == "node":
                        seen.add(name)
                        self.assertEqual(pins["nodejs"], tag.split("-", 1)[0].split(".", 1)[0])
                    elif name == "ghcr.io/astral-sh/uv":
                        seen.add(name)
                        self.assertEqual(pins["uv"], tag)
        self.assertEqual({"node", "ghcr.io/astral-sh/uv"}, seen)

    def test_bump_workflow_reads_tools_from_the_script(self) -> None:
        """tool-versions-bump.yml bumps whatever ``tool_versions.py bumpable`` lists."""
        workflow = yaml.safe_load(
            (ROOT / ".github" / "workflows" / "tool-versions-bump.yml").read_text(encoding="utf-8")
        )
        runs = "\n".join(step.get("run", "") for step in workflow["jobs"]["bump"]["steps"])
        self.assertIn("$(python3 scripts/tool_versions.py bumpable)", runs)
        self.assertNotIn("matrix", workflow["jobs"]["bump"].get("strategy", {}))

    def test_dependabot_leaves_tool_versions_pins_alone(self) -> None:
        """Dependabot skips the uv image and Node.js majors, which .tool-versions owns."""
        config = yaml.safe_load(DEPENDABOT.read_text(encoding="utf-8"))
        docker = next(u for u in config["updates"] if u["package-ecosystem"] == "docker")
        ignored = {i["dependency-name"]: i.get("update-types") for i in docker.get("ignore", [])}
        self.assertEqual(None, ignored.get("ghcr.io/astral-sh/uv", "missing"))
        self.assertEqual(["version-update:semver-major"], ignored.get("node"))

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


class ImagePublishWorkflowTests(unittest.TestCase):
    """Registry write and signing rights stay in the jobs that need them."""

    @classmethod
    def setUpClass(cls) -> None:
        """Load the publish workflow once (PyYAML from scripts/tests/requirements.txt)."""
        import yaml

        cls.workflow = yaml.safe_load(PUBLISH_WORKFLOW.read_text(encoding="utf-8"))
        cls.jobs = cls.workflow["jobs"]

    def test_permissions_are_per_job(self) -> None:
        """Only push jobs write packages; only the attest job mints OIDC tokens."""
        self.assertEqual(self.workflow["permissions"], {"contents": "read"})
        for name, job in self.jobs.items():
            perms = job.get("permissions", {})
            with self.subTest(job=name):
                self.assertEqual(perms.get("contents"), "read")
                if name in ("build", "publish", "attest"):
                    self.assertEqual(perms.get("packages"), "write")
                else:
                    self.assertNotEqual(perms.get("packages"), "write")
                if name == "attest":
                    self.assertEqual(perms.get("id-token"), "write")
                    self.assertEqual(perms.get("attestations"), "write")
                else:
                    self.assertNotIn("id-token", perms)
                    self.assertNotIn("attestations", perms)

    def test_release_tag_goes_through_the_parser(self) -> None:
        """The release tag reaches the registry only through the fuzzed parser."""
        plan = next(step for step in self.jobs["plan"]["steps"] if step.get("id") == "plan")
        self.assertIn("python3 scripts/image_release_tags.py", plan["run"])
        self.assertNotIn("BASH_REMATCH", plan["run"])
        self.assertFalse(self.jobs["plan"]["steps"][0]["with"]["persist-credentials"])
        self.assertIn("scripts/image_release_tags.py", self.workflow[True]["pull_request"]["paths"])

    def test_pull_requests_never_push(self) -> None:
        """A pull request run builds and checks but cannot publish."""
        plan = next(step for step in self.jobs["plan"]["steps"] if step.get("id") == "plan")["run"]
        self.assertIn("pull_request) push=false", plan)
        # A run by hand from another branch must not publish unreviewed code.
        self.assertIn('if [[ "$EVENT" != release && "$REF" != refs/heads/main ]]; then\n  push=false', plan)
        gate = "needs.plan.outputs.push == 'true'"
        self.assertIn(gate, self.jobs["publish"]["if"])
        self.assertIn(gate, self.jobs["attest"]["if"])
        self.assertIn(gate, self.jobs["build"]["if"])
        dry_run = self.jobs["dry-run"]
        self.assertIn("needs.plan.outputs.push == 'false'", dry_run["if"])
        self.assertEqual(dry_run["permissions"], {"contents": "read"})
        for name in ("dry-run", "build"):
            with self.subTest(job=name):
                pushes = [s for s in self.jobs[name]["steps"] if "docker push" in s.get("run", "")]
                self.assertEqual(len(pushes), 1 if name == "build" else 0)

    def test_dry_run_builds_what_publishes(self) -> None:
        """The pull request dry run covers every image and architecture the build job pushes."""
        for name in ("dry-run", "build"):
            with self.subTest(job=name):
                uses = [s.get("uses") for s in self.jobs[name]["steps"]]
                self.assertIn("./.github/actions/image-build", uses)
        self.assertEqual(
            self.jobs["dry-run"]["strategy"]["matrix"],
            self.jobs["build"]["strategy"]["matrix"],
        )

    def test_rerun_repairs_missing_attestation(self) -> None:
        """A re-run attests an index an earlier run published but failed to attest."""
        digest = self.jobs["attest"]["steps"][0]["run"]
        self.assertIn('gh api "repos/$GH_REPO/attestations/$digest"', digest)
        self.assertIn("has no attestation, attesting it", digest)

    def test_architectures(self) -> None:
        """Production and the Dev Container ship arm64 as well as amd64, each built natively."""
        built = {(e["image"], e["arch"]): e["runner"] for e in self.jobs["build"]["strategy"]["matrix"]["include"]}
        for image in ("bookclerk", "bookclerk-dev"):
            with self.subTest(image=image):
                self.assertEqual(built.get((image, "amd64")), "ubuntu-latest")
                self.assertTrue(built.get((image, "arm64"), "").endswith("-arm"))
        joined = {e["image"]: set(e["arches"].split()) for e in self.jobs["publish"]["strategy"]["matrix"]["include"]}
        for image, arches in joined.items():
            with self.subTest(image=image):
                self.assertEqual(arches, {a for (i, a) in built if i == image})


if __name__ == "__main__":
    unittest.main()
