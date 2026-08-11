"""Download / refresh the pinned Cloudflare ``workerd`` binary (mirrors ensure.rs).

Resolves the platform asset from ``workerd-pin.json``, verifies sha256, and
caches under ``~/.cache/bookclerk/workerd`` (or ``BOOKCLERK_WORKERD_CACHE``).
"""

from __future__ import annotations

import gzip
import hashlib
import os
import platform
import shutil
import subprocess
import urllib.request
from pathlib import Path
from typing import Any

_PKG = Path(__file__).resolve().parent.parent  # bookclerk_plugin_sdk/


def package_root() -> Path:
    """Return the ``bookclerk_plugin_sdk`` package directory.

    Returns:
        Absolute path containing ``workerd.py``, ``bridge/``, and the pin file.
    """
    return _PKG


def load_pin(root: Path | None = None) -> dict[str, Any]:
    """Load the pinned workerd release metadata.

    Args:
        root: Package root containing ``workerd-pin.json`` (defaults to this SDK).

    Returns:
        Parsed pin dictionary (release tag, assets, version stamp).

    Raises:
        FileNotFoundError: If the pin file is missing.
        json.JSONDecodeError: If the pin file is not valid JSON.
    """
    pin_path = (root or package_root()) / "workerd-pin.json"
    import json

    return json.loads(pin_path.read_text(encoding="utf-8"))


def platform_key(
    system: str | None = None,
    machine: str | None = None,
) -> str | None:
    """Map OS/arch to a pin asset key such as ``linux-x86_64``.

    Args:
        system: OS name override (defaults to ``platform.system()``).
        machine: CPU arch override (defaults to ``platform.machine()``).

    Returns:
        Pin key string, or ``None`` when the platform is unsupported.
    """
    system = (system or platform.system()).lower()
    machine = (machine or platform.machine()).lower()
    os_map = {"linux": "linux", "darwin": "macos", "windows": "windows"}
    arch_map = {
        "x86_64": "x86_64",
        "amd64": "x86_64",
        "aarch64": "aarch64",
        "arm64": "aarch64",
    }
    os_name = os_map.get(system)
    arch = arch_map.get(machine)
    if not os_name or not arch:
        return None
    return f"{os_name}-{arch}"


def binary_name() -> str:
    """Return the on-disk workerd binary filename for this OS.

    Returns:
        ``workerd.exe`` on Windows, otherwise ``workerd``.
    """
    return "workerd.exe" if platform.system().lower() == "windows" else "workerd"


def download_url(pin: dict[str, Any], artifact: str) -> str:
    """Build the GitHub release download URL for a workerd artifact.

    Args:
        pin: Loaded pin metadata containing ``release_tag``.
        artifact: Asset filename from the pin ``assets`` table.

    Returns:
        Absolute HTTPS URL to the Cloudflare workerd release asset.
    """
    tag = pin["release_tag"]
    return f"https://github.com/cloudflare/workerd/releases/download/{tag}/{artifact}"


def default_cache_dir() -> Path:
    """Resolve the workerd binary cache directory.

    Honors ``BOOKCLERK_WORKERD_CACHE`` when set; otherwise uses
    ``~/.cache/bookclerk/workerd``.

    Returns:
        Path to the cache directory (may not exist yet).
    """
    if env := os.environ.get("BOOKCLERK_WORKERD_CACHE"):
        return Path(env)
    home = Path.home()
    return home / ".cache" / "bookclerk" / "workerd"


def _is_current(bin_path: Path, pin: dict[str, Any]) -> bool:
    stamp = bin_path.parent / pin["version_stamp"]
    if stamp.is_file() and stamp.read_text(encoding="utf-8").strip() == pin["release_tag"]:
        return True
    try:
        proc = subprocess.run(
            [str(bin_path), "--version"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return False
    if proc.returncode != 0:
        return False
    combined = f"{proc.stdout}{proc.stderr}"
    tag = pin["release_tag"]
    bare = tag.lstrip("v")
    return tag in combined or bare in combined


def ensure_workerd(
    cache_dir: Path | None = None,
    root: Path | None = None,
) -> Path:
    """Ensure a pinned ``workerd`` binary is available locally.

    Reuses ``BOOKCLERK_WORKERD_BIN`` or a cached binary when the version stamp
    matches the pin; otherwise downloads, verifies sha256, and installs.

    Args:
        cache_dir: Override cache directory (defaults to :func:`default_cache_dir`).
        root: SDK root for loading ``workerd-pin.json``.

    Returns:
        Path to an executable workerd binary matching the pin.

    Raises:
        RuntimeError: If no asset exists for this platform or the download hash
            mismatches.
        OSError: If the binary cannot be written or executed.
    """
    pin = load_pin(root)
    override = os.environ.get("BOOKCLERK_WORKERD_BIN")
    if override:
        path = Path(override)
        if path.is_file() and _is_current(path, pin):
            return path

    cache = cache_dir or default_cache_dir()
    cache.mkdir(parents=True, exist_ok=True)
    dest = cache / binary_name()
    if dest.is_file() and _is_current(dest, pin):
        return dest

    key = platform_key()
    assets = pin.get("assets") or {}
    if not key or key not in assets:
        raise RuntimeError(
            f"no pinned workerd asset for {platform.system()}-{platform.machine()}"
        )
    asset = assets[key]
    url = download_url(pin, asset["artifact"])
    print(f"bookclerk-plugin: fetching {url}", flush=True)
    with urllib.request.urlopen(url) as resp:  # noqa: S310 — pinned GitHub release URL
        compressed = resp.read()
    got = hashlib.sha256(compressed).hexdigest()
    if got != asset["sha256_hex"]:
        raise RuntimeError(
            f"workerd download sha256 mismatch: got {got}, expected {asset['sha256_hex']}"
        )

    tmp = cache / f"{binary_name()}.tmp"
    with gzip.GzipFile(fileobj=__import__("io").BytesIO(compressed)) as gz, tmp.open(
        "wb"
    ) as out:
        shutil.copyfileobj(gz, out)
    if platform.system().lower() != "windows":
        tmp.chmod(0o755)
    tmp.replace(dest)
    (cache / pin["version_stamp"]).write_text(f"{pin['release_tag']}\n", encoding="utf-8")
    print(f"bookclerk-plugin: installed {pin['release_tag']} → {dest}", flush=True)
    return dest
