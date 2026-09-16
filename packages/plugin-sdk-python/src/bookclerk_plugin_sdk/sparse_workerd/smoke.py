"""Out-of-tree workerd plugin smoke: ensure → materialize → describe + health.

Spawns the pinned Cloudflare ``workerd`` with a materialized Cap'n Proto config
and POSTs ``describe`` (plus a ``POST /invoke`` ``health`` call for storefront /
remoteLibrary guests) to the HTTP bridge. Does not require the Rust
``bookclerk-workerd`` binary.
"""

from __future__ import annotations

import json
import os
import secrets
import socket
import subprocess
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

from ..tools import validate_manifest
from .config import materialize_config
from .ensure import default_cache_dir, ensure_workerd


def _free_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _wait_for_health(base: str, token: str, timeout_s: float = 15.0) -> None:
    url = f"{base}/health"
    deadline = time.monotonic() + timeout_s
    while True:
        try:
            req = urllib.request.Request(
                url,
                headers={"Authorization": f"Bearer {token}"},
                method="GET",
            )
            with urllib.request.urlopen(req, timeout=1.0) as resp:  # noqa: S310 — loopback
                if 200 <= resp.status < 300:
                    return
        except (urllib.error.URLError, TimeoutError, OSError):
            pass
        if time.monotonic() > deadline:
            raise TimeoutError(f"timeout waiting for {url}")
        time.sleep(0.05)


def _invoke_health(base: str, iface: str, token: str) -> Any:
    """``POST /invoke`` health probe for ``ContentSource`` / ``RemoteLibrary``.

    Args:
        base: Bridge base URL.
        iface: Cap'n interface name (``ContentSource`` or ``RemoteLibrary``).
        token: Bridge bearer token.

    Returns:
        The ``HealthOk`` dict from the reply union.

    Raises:
        RuntimeError: On a non-200 transport answer or an ``err`` reply.
    """
    from .. import _wire

    if iface == "ContentSource":
        params = _wire._content_source_health_params_codec
        results = _wire._content_source_health_results_codec
    else:
        params = _wire._remote_library_health_params_codec
        results = _wire._remote_library_health_results_codec
    req = urllib.request.Request(
        f"{base}/invoke",
        data=_wire._encode_message(params, {}, _wire.NO_CAPS),
        headers={
            "content-type": "application/x-capnp",
            "Authorization": f"Bearer {token}",
            "x-bookclerk-interface": iface,
            "x-bookclerk-method": "health",
            "x-bookclerk-context": json.dumps({"invocation": {"id": "smoke"}}),
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=30.0) as resp:  # noqa: S310 — loopback
            body = resp.read()
    except urllib.error.HTTPError as err:
        text = err.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"bridge HTTP {err.code}: {text}") from err
    reply = _wire._decode_message(results, body)["result"]
    if reply.get("kind") == "err":
        failure = reply.get("value") or {}
        raise RuntimeError(
            f"{iface}.health failed: {failure.get('code', 'internal')}: "
            f"{failure.get('message', 'plugin error')}"
        )
    return reply.get("value")


def _post_json(url: str, body: dict[str, Any], token: str) -> Any:
    data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={
            "content-type": "application/json",
            "Authorization": f"Bearer {token}",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=30.0) as resp:  # noqa: S310 — loopback
            text = resp.read().decode("utf-8")
            status = resp.status
    except urllib.error.HTTPError as err:
        text = err.read().decode("utf-8", errors="replace")
        status = err.code
        if status != 400:
            raise RuntimeError(f"bridge HTTP {status}: {text}") from err
    value = json.loads(text)
    if isinstance(value, dict) and value.get("error"):
        err = value["error"]
        raise RuntimeError(
            f"POST {url} failed: {err.get('code', 'internal')}: "
            f"{err.get('message', 'bridge error')}"
        )
    return value


def run_smoke(plugin_dir: Path) -> str:
    """Smoke a workerd plugin without the Rust ``bookclerk-workerd`` binary.

    Args:
        plugin_dir: Path to a ``runtime = "workerd"`` plugin root.

    Returns:
        Multi-line status including plugin id and JSON describe/health detail.

    Raises:
        FileNotFoundError: If ``plugin.toml`` is missing.
        ValueError: If the manifest is invalid or not a workerd plugin.
        TimeoutError: If the bridge health endpoint never becomes ready.
        RuntimeError: If the describe/health bridge call fails.

    Examples:
        >>> # print(run_smoke(Path("./my-workerd-plugin")))
    """
    root = plugin_dir.resolve()
    toml_path = root / "plugin.toml"
    if not toml_path.is_file():
        raise FileNotFoundError(f"missing plugin.toml in {root}")
    manifest = tomllib.loads(toml_path.read_text(encoding="utf-8"))
    validate_manifest(manifest)
    runtime = manifest.get("runtime") or "native"
    if runtime != "workerd":
        raise ValueError(f'smoke requires runtime = "workerd" (got {runtime!r})')

    workerd_bin = ensure_workerd(default_cache_dir())
    port = _free_loopback_port()
    bridge_token = secrets.token_hex(32)
    config_path, listen_addr = materialize_config(
        root,
        manifest,
        listen_port=port,
        bridge_token=bridge_token,
    )
    base = f"http://{listen_addr}"

    env = {**os.environ, "BOOKCLERK_PLUGIN_ROOT": str(root)}
    proc = subprocess.Popen(
        [str(workerd_bin), "serve", str(config_path)],
        cwd=str(root),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        text=True,
    )
    try:
        _wait_for_health(base, bridge_token)
        describe = _post_json(f"{base}/describe", {}, bridge_token)
        # `health` is a method of the storefront and remoteLibrary entrypoints;
        # the default entrypoint (event/job triggers) has no health probe.
        entrypoints = [str(e) for e in (manifest.get("entrypoints") or [])]
        if "storefront" in entrypoints:
            health_iface: str | None = "ContentSource"
        elif "remoteLibrary" in entrypoints:
            health_iface = "RemoteLibrary"
        else:
            health_iface = None
        health = (
            _invoke_health(base, health_iface, bridge_token) if health_iface else None
        )
        detail = {
            "plugin": manifest["id"],
            "listen": listen_addr,
            "describe": describe,
            "health": health,
        }
        return f"smoke ok {manifest['id']}\n{json.dumps(detail, indent=2)}"
    except Exception:
        try:
            out, err = proc.communicate(timeout=0.2)
            if out:
                print(out, flush=True)
            if err:
                print(err, flush=True)
        except Exception:  # noqa: BLE001
            pass
        raise
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=3)
