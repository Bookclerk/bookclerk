"""The Cap'n Proto config and embeds ``materialize_config`` writes.

Each case's config is compared byte for byte with a golden file under
``tests/golden/``, reviewed by hand once, so a change to any service, module
embed, binding or policy field shows. Regenerate them with
``BOOKCLERK_UPDATE_GOLDEN=1`` and review the diff.
"""

import json
import os
import re
import shutil
import tomllib
from pathlib import Path
from typing import Any

import pytest

from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config, package_root

# The repository root: the nearest folder above holding the ABI crate, not a
# fixed depth (docs/contributing/ci.md#python-sdk).
ROOT = next(
    p for p in Path(__file__).resolve().parents if (p / "crates" / "bookclerk-plugin-abi").is_dir()
)
FIXTURES = ROOT / "crates" / "bookclerk-plugin-abi" / "fixtures" / "tools"
GOLDEN = Path(__file__).parent / "golden"
SDK = package_root()


def _plugin(tmp_path: Path, fixture: str, edit: Any = None) -> tuple[Path, dict[str, Any]]:
    """Copy a tools fixture, let ``edit`` change its manifest, and return both."""
    plugin = tmp_path / "plugin"
    shutil.copytree(FIXTURES / fixture, plugin)
    manifest = tomllib.loads((plugin / "plugin.toml").read_text(encoding="utf-8"))
    if edit is not None:
        edit(manifest)
    return plugin, manifest


def _outbound_named(manifest: dict[str, Any]) -> None:
    """Outbound network with an own-agent host, a named entrypoint, two wires."""
    manifest["entrypoints"] = ["cli", "metadata", "unknown"]
    manifest["workerd"]["entrypoint"] = "Main"
    manifest["capabilities"]["network"] = {
        "mode": "outbound",
        "domains": ["api.example.com"],
        "own_user_agent": {"hosts": ["*.Example.com", 7]},
    }


CASES = {
    "workerd-config-js": ("valid-workerd", None),
    "workerd-config-js-python": ("valid-js-with-python-helper", _outbound_named),
}


def _materialize(tmp_path: Path, fixture: str, edit: Any) -> tuple[Path, str]:
    plugin, manifest = _plugin(tmp_path, fixture, edit)
    state = tmp_path / "state"
    state.mkdir()
    generated = materialize_config(
        plugin,
        manifest,
        listen_port=4321,
        bridge_token='tok"en',
        state_dir=state,
    )
    assert generated.config_path == state.resolve() / "workerd-config.capnp"
    assert generated.listen_addr == "127.0.0.1:4321"
    assert generated.state_dir == state.resolve()
    assert generated.import_path == plugin.resolve()
    return state, generated.config_path.read_text(encoding="utf-8")


@pytest.mark.parametrize("case", sorted(CASES))
def test_config_matches_gold(tmp_path: Path, case: str) -> None:
    fixture, edit = CASES[case]
    _, config = _materialize(tmp_path, fixture, edit)
    gold = GOLDEN / f"{case}.capnp"
    if os.environ.get("BOOKCLERK_UPDATE_GOLDEN") == "1":
        gold.write_text(config, encoding="utf-8")
    assert config == gold.read_text(encoding="utf-8")


def test_the_egress_policy_is_compact_json(tmp_path: Path) -> None:
    _, config = _materialize(tmp_path, *CASES["workerd-config-js-python"])
    escaped = re.search(r'name = "EGRESS_POLICY", json = "((?:[^"\\]|\\.)*)"', config)
    assert escaped is not None
    raw = escaped.group(1).encode().decode("unicode_escape")
    assert ", " not in raw and ": " not in raw
    policy = json.loads(raw)
    assert policy["mode"] == "outbound"
    # The declared domain comes first, before the runtime's own hosts.
    assert policy["domains"][0] == "api.example.com"
    assert policy["maxRedirects"] == 10
    assert policy["ownUserAgentHosts"] == ["*.example.com"]


def test_embeds_are_copies_of_the_sdk_sources(tmp_path: Path) -> None:
    state, _ = _materialize(tmp_path, *CASES["workerd-config-js-python"])
    embeds = state / ".bookclerk"
    for name in ("bridge.js", "egress.js"):
        assert (embeds / name).read_bytes() == (SDK / "bridge" / name).read_bytes()
    for embed, source in (
        ("sdk-workerd.py", "workerd.py"),
        ("sdk-product-abi.py", "_abi.py"),
        ("sdk-guest-sql.py", "guest_sql.py"),
        ("sdk-limits.py", "limits.py"),
        ("sdk-db-value.py", "db_value.py"),
    ):
        assert (embeds / embed).read_text(encoding="utf-8") == (SDK / source).read_text(
            encoding="utf-8"
        ), embed
    assert (embeds / "sdk-init.py").is_file()
    assert (embeds / "sdk-workerd.js").is_file()
    assert (embeds / "adapter.js").is_file()


def test_a_js_only_plugin_gets_no_python_embeds(tmp_path: Path) -> None:
    state, config = _materialize(tmp_path, *CASES["workerd-config-js"])
    assert not (state / ".bookclerk" / "sdk-workerd.py").exists()
    assert "pythonModule" not in config
    assert "compatibilityFlags" not in config


def test_config_name_is_a_single_file_name(tmp_path: Path) -> None:
    plugin, manifest = _plugin(tmp_path, "valid-workerd")
    state = tmp_path / "state"
    state.mkdir()
    generated = materialize_config(
        plugin, manifest, listen_port=1, bridge_token="t", state_dir=state, config_name="x.capnp"
    )
    assert generated.config_path.name == "x.capnp"
    with pytest.raises(ValueError):
        materialize_config(
            plugin, manifest, listen_port=1, bridge_token="t", state_dir=state, config_name="../x"
        )


@pytest.mark.parametrize(
    ("edit", "error", "match"),
    [
        (lambda m: m.pop("workerd"), ValueError, r"^missing \[workerd\] table$"),
        (lambda m: None, ValueError, r"^bridge_token is required$"),
    ],
    ids=["no-workerd-table", "no-bridge-token"],
)
def test_materialize_refuses(tmp_path: Path, edit: Any, error: type[Exception], match: str) -> None:
    plugin, manifest = _plugin(tmp_path, "valid-workerd", edit)
    token = "" if "bridge_token" in match else "t"
    with pytest.raises(error, match=match):
        materialize_config(plugin, manifest, listen_port=1, bridge_token=token, state_dir=tmp_path)


def test_missing_modules_and_main_module_are_named(tmp_path: Path) -> None:
    plugin, manifest = _plugin(tmp_path, "valid-workerd")
    (plugin / "modules" / "index.js").unlink()
    with pytest.raises(FileNotFoundError, match=r"^main module missing: .*index\.js$"):
        materialize_config(plugin, manifest, listen_port=1, bridge_token="t", state_dir=tmp_path)
    shutil.rmtree(plugin / "modules")
    with pytest.raises(FileNotFoundError, match=r"^modules dir missing: .*modules$"):
        materialize_config(plugin, manifest, listen_port=1, bridge_token="t", state_dir=tmp_path)


def test_an_undeclared_python_file_is_refused(tmp_path: Path) -> None:
    plugin, manifest = _plugin(tmp_path, "valid-workerd")
    (plugin / "modules" / "stray.py").write_text("x = 1\n", encoding="utf-8")
    with pytest.raises(ValueError):
        materialize_config(plugin, manifest, listen_port=1, bridge_token="t", state_dir=tmp_path)
