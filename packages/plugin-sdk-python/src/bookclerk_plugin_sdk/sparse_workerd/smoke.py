"""Out-of-tree workerd plugin smoke: ensure → materialize → handshake + health."""

from __future__ import annotations

import json
import os
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


def _wait_for_health(base: str, timeout_s: float = 15.0) -> None:
    url = f"{base}/health"
    deadline = time.monotonic() + timeout_s
    while True:
        try:
            with urllib.request.urlopen(url, timeout=1.0) as resp:  # noqa: S310 — loopback
                if 200 <= resp.status < 300:
                    return
        except (urllib.error.URLError, TimeoutError, OSError):
            pass
        if time.monotonic() > deadline:
            raise TimeoutError(f"timeout waiting for {url}")
        time.sleep(0.05)


def _post_rpc(rpc_url: str, body: dict[str, Any]) -> Any:
    data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(
        rpc_url,
        data=data,
        headers={"content-type": "application/json"},
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
    if value.get("error"):
        err = value["error"]
        raise RuntimeError(
            f"RPC {body['method']} failed: {err.get('code', 'internal')}: "
            f"{err.get('message', 'bridge error')}"
        )
    return value.get("result")


def run_smoke(plugin_dir: Path) -> str:
    """Smoke a workerd plugin without the Rust ``bookclerk-workerd`` binary."""
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
    config_path, listen_addr = materialize_config(
        root, manifest, listen_port=port, notify_addr=None
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
        _wait_for_health(base)
        rpc_url = f"{base}/rpc"
        handshake = _post_rpc(
            rpc_url,
            {"id": 1, "method": "handshake", "params": {"apiVersion": 1, "config": {}}},
        )
        health = _post_rpc(rpc_url, {"id": 2, "method": "health", "params": {}})
        detail = {
            "plugin": manifest["id"],
            "listen": listen_addr,
            "handshake": handshake,
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
