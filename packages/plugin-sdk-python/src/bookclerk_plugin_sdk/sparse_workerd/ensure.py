"""Download / refresh the pinned Cloudflare ``workerd`` binary (mirrors ensure.rs).

Resolves the platform asset from ``workerd-pin.json``, verifies sha256, and
caches under the operator-selected directory (``BOOKCLERK_WORKERD_CACHE`` or
``~/.cache/bookclerk/workerd``).
"""

from __future__ import annotations

import gzip
import hashlib
import os
import platform
import shutil
import time
import urllib.request
from pathlib import Path
from typing import Any

from ..path_guard import resolve_under, write_file_under

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
    pin_path = resolve_under(root or package_root(), "workerd-pin.json")
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

    Honors ``BOOKCLERK_WORKERD_CACHE`` as the selected install root (any
    writable location, including paths outside ``$HOME`` and this package).
    Otherwise uses ``~/.cache/bookclerk/workerd``. Derived files stay under
    that root.

    Returns:
        Path to the cache directory (may not exist yet).
    """
    env = os.environ.get("BOOKCLERK_WORKERD_CACHE")
    if env and "\0" not in env:
        return Path(os.path.abspath(os.path.expanduser(env)))
    home = os.path.expanduser("~")
    return Path(os.path.abspath(os.path.join(home, ".cache", "bookclerk", "workerd")))


def validate_fetch_url(url: str) -> str:
    """Validate a URL before ``urlopen`` / download (request-forgery guard).

    Allows ``https:`` anywhere, or ``http:`` only to loopback hosts.

    Args:
        url: Absolute URL string.

    Returns:
        The same URL when it passes scheme/host checks.

    Raises:
        ValueError: When the URL is invalid or uses a disallowed scheme/host.
    """
    from urllib.parse import urlparse

    parsed = urlparse(url)
    if parsed.scheme == "https" and parsed.netloc:
        return url
    if parsed.scheme == "http":
        host = (parsed.hostname or "").lower()
        if host in {"127.0.0.1", "localhost", "::1"}:
            return url
    raise ValueError(
        f"refusing non-HTTPS (or non-loopback HTTP) URL: {parsed.scheme}://{parsed.netloc}"
    )


def validate_spawn_executable(
    bin_path: Path | str,
    trusted_root: Path | str | None = None,
) -> Path:
    """Validate a workerd (or helper) binary path before ``subprocess`` spawn.

    Requires an absolute path with no NUL bytes. When ``trusted_root`` is set,
    requires the binary to stay under that root via :func:`resolve_under`.

    Args:
        bin_path: Candidate executable path.
        trusted_root: Optional directory the binary must remain under.

    Returns:
        Absolute validated :class:`~pathlib.Path`.

    Raises:
        ValueError: When the path is relative, contains NUL, or escapes
            ``trusted_root``.
    """
    path = Path(bin_path)
    s = os.fspath(path)
    if not s or "\0" in s:
        raise ValueError("spawn executable path is empty or contains NUL")
    if not path.is_absolute():
        raise ValueError(f"spawn executable must be absolute: {path}")
    if trusted_root is not None:
        return resolve_under(trusted_root, path)
    return Path(os.path.abspath(s))


def _stamp_file_name(pin: dict[str, Any]) -> str:
    stamp = pin.get("version_stamp")
    if not isinstance(stamp, str) or not stamp:
        raise ValueError("invalid workerd version_stamp")
    if (
        "\0" in stamp
        or "/" in stamp
        or "\\" in stamp
        or stamp in {".", ".."}
    ):
        raise ValueError(f"invalid workerd version_stamp: {stamp}")
    return stamp


def _is_current(bin_path: Path, pin: dict[str, Any]) -> bool:
    """True when ``bin_path`` is a file matching the pin.

    A sibling stamp is enough when the binary exists. A stamp without a binary
    is not current. When no Bookclerk stamp matches, probe ``--version`` on the
    absolute path with fixed argv (no ``PATH`` lookup).

    Follows leaf symlinks so an explicit ``BOOKCLERK_WORKERD_BIN`` override that
    points at a pinned executable still matches. Managed-cache callers must
    refuse symlink leaves before invoking this helper.
    """
    # Follow symlinks: operator overrides may be links to a pinned binary.
    if not bin_path.is_file():
        return False
    root_s = os.path.abspath(os.fspath(bin_path.parent))
    name = _stamp_file_name(pin)
    stamp_s = os.path.abspath(os.path.join(root_s, name))
    try:
        rel = os.path.relpath(stamp_s, root_s)
    except ValueError:
        rel = ".."
    stamp_ok = (
        rel != ".."
        and not rel.startswith(".." + os.sep)
        and not os.path.isabs(rel)
        and (stamp_s == root_s or stamp_s.startswith(root_s + os.sep))
    )
    # Symlink stamps are not a pin match (fall through to --version).
    if stamp_ok and not os.path.islink(stamp_s):
        try:
            with open(stamp_s, encoding="utf-8") as fh:
                if fh.read().strip() == pin["release_tag"]:
                    return True
        except OSError:
            pass
    return _version_probe_matches(bin_path, pin)


def _version_probe_matches(bin_path: Path, pin: dict[str, Any]) -> bool:
    import subprocess

    validated = validate_spawn_executable(bin_path)
    try:
        out = subprocess.run(
            [os.fspath(validated), "--version"],
            check=False,
            capture_output=True,
            text=True,
            shell=False,
        )
    except OSError:
        return False
    if out.returncode != 0:
        return False
    combined = (out.stdout or "") + (out.stderr or "")
    tag = str(pin["release_tag"])
    bare = tag[1:] if tag.startswith("v") else tag
    return tag in combined or bare in combined


def ensure_workerd(
    cache_dir: Path | None = None,
    root: Path | None = None,
) -> Path:
    """Ensure a pinned ``workerd`` binary is available locally.

    Reuses ``BOOKCLERK_WORKERD_BIN`` or a cached binary when that file exists
    and the version stamp or ``--version`` output matches the pin. The selected
    cache directory is the trusted install root (not this package directory).
    A stamp without a binary is a cache miss.

    Args:
        cache_dir: Override cache directory (defaults to :func:`default_cache_dir`).
        root: SDK root for loading ``workerd-pin.json``.

    Returns:
        Path to an executable workerd binary matching the pin.

    Raises:
        RuntimeError: If no asset exists for this platform or the download hash
            mismatches.
        OSError: If the binary cannot be written or executed.
        ValueError: If a resolved binary path fails spawn validation.
    """
    pin = load_pin(root)

    override = os.environ.get("BOOKCLERK_WORKERD_BIN")
    if override and "\0" not in override:
        override_path = Path(override)
        if (
            override_path.is_absolute()
            and override_path.is_file()
            and _is_current(override_path, pin)
        ):
            return validate_spawn_executable(override_path)

    if cache_dir is None:
        cache_dir = default_cache_dir()
    cache_s = os.path.abspath(os.path.expanduser(os.fspath(cache_dir)))
    if "\0" in cache_s:
        raise ValueError("workerd cache path contains NUL")
    os.makedirs(cache_s, exist_ok=True)
    cache = Path(cache_s)

    dest = resolve_under(cache, binary_name())
    if dest.is_symlink():
        raise ValueError(f"refusing managed workerd symlink: {dest}")
    if dest.is_file() and _is_current(dest, pin):
        return validate_spawn_executable(dest, cache)

    key = platform_key()
    assets = pin.get("assets") or {}
    if not key or key not in assets:
        raise RuntimeError(
            f"no pinned workerd asset for {platform.system()}-{platform.machine()}"
        )
    asset = assets[key]
    url = validate_fetch_url(download_url(pin, asset["artifact"]))
    print(f"bookclerk-plugin: fetching {url}", flush=True)
    # HTTPS (or loopback HTTP) URL validated above.
    with urllib.request.urlopen(url) as resp:  # noqa: S310 — pinned GitHub release URL
        compressed = resp.read()
    got = hashlib.sha256(compressed).hexdigest()
    if got != asset["sha256_hex"]:
        raise RuntimeError(
            f"workerd download sha256 mismatch: got {got}, expected {asset['sha256_hex']}"
        )

    tmp_name = f".{binary_name()}.tmp-{os.getpid()}-{time.time_ns()}"
    if (
        "/" in tmp_name
        or "\\" in tmp_name
        or tmp_name in {".", ".."}
    ):
        raise ValueError(f"invalid temp binary name: {tmp_name}")
    tmp_s = os.path.abspath(os.path.join(cache_s, tmp_name))
    try:
        rel = os.path.relpath(tmp_s, cache_s)
    except ValueError as err:
        raise ValueError(f"temp path escapes cache: {tmp_s}") from err
    if rel == ".." or rel.startswith(".." + os.sep) or os.path.isabs(rel):
        raise ValueError(f"temp path escapes cache: {tmp_s}")
    if tmp_s != cache_s and not tmp_s.startswith(cache_s + os.sep):
        raise ValueError(f"temp path escapes cache: {tmp_s}")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    fd = os.open(tmp_s, flags, 0o755 if platform.system().lower() != "windows" else 0o644)
    owned_tmp = True
    try:
        with os.fdopen(fd, "wb") as out, gzip.GzipFile(
            fileobj=__import__("io").BytesIO(compressed)
        ) as gz:
            shutil.copyfileobj(gz, out)
        if platform.system().lower() != "windows":
            os.chmod(tmp_s, 0o755)
        dest_s = os.fspath(dest)
        if dest_s != cache_s and not dest_s.startswith(cache_s + os.sep):
            raise ValueError(f"dest path escapes cache: {dest_s}")
        if os.path.islink(dest_s):
            raise ValueError(f"refusing managed workerd symlink: {dest_s}")
        os.replace(tmp_s, dest_s)
        owned_tmp = False
    finally:
        if owned_tmp and os.path.lexists(tmp_s) and not os.path.islink(tmp_s):
            try:
                os.unlink(tmp_s)
            except OSError:
                pass
    write_file_under(cache, _stamp_file_name(pin), f"{pin['release_tag']}\n")
    print(f"bookclerk-plugin: installed {pin['release_tag']} → {dest}", flush=True)
    return validate_spawn_executable(dest, cache)
