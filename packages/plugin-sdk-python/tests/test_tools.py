from pathlib import Path

import pytest

from bookclerk_plugin_sdk.tools import (
    check_plugin,
    fmt_plugin_toml,
    package_plugin,
    sync_embed,
)

ROOT = Path(__file__).resolve().parents[3]
FIXTURES = ROOT / "crates" / "bookclerk-plugin-abi" / "fixtures" / "tools"
ECHO_PY = ROOT / "examples" / "plugins-echo-workerd-python"


def test_check_valid_workerd():
    msg = check_plugin(FIXTURES / "valid-workerd")
    assert "echo-workerd-tools" in msg


def test_check_invalid_outbound():
    with pytest.raises(ValueError, match="domains"):
        check_plugin(FIXTURES / "invalid-outbound-no-domains")


def test_check_valid_logo_url():
    msg = check_plugin(FIXTURES / "valid-logo-url")
    assert "logo-url" in msg


def test_check_valid_logo_path():
    msg = check_plugin(FIXTURES / "valid-logo-path")
    assert "logo-path" in msg


def test_check_rejects_logo_javascript():
    with pytest.raises(ValueError, match="logo"):
        check_plugin(FIXTURES / "invalid-logo-javascript")


def test_check_rejects_logo_vbscript():
    with pytest.raises(ValueError, match="logo"):
        check_plugin(FIXTURES / "invalid-logo-vbscript")


def test_check_rejects_logo_parent():
    with pytest.raises(ValueError, match="logo"):
        check_plugin(FIXTURES / "invalid-logo-parent")


def test_check_rejects_native_with_domains():
    with pytest.raises(ValueError, match="only valid for runtime"):
        check_plugin(FIXTURES / "invalid-native-with-domains")


@pytest.mark.parametrize("name", ["valid-native", "valid-workerd"])
def test_fmt_check_gold(name):
    gold = FIXTURES / name / "plugin.fmt.toml"
    assert "ok" in fmt_plugin_toml(gold, check_only=True)


def test_check_echo_python_workerd():
    msg = check_plugin(ECHO_PY)
    assert "echo-workerd-python" in msg


def test_package_python_vendors_sdk_package(tmp_path: Path):
    out = tmp_path / "dist"
    archive = package_plugin(ECHO_PY, out)
    assert archive.is_file()
    import tarfile

    with tarfile.open(archive, "r:gz") as tar:
        names = tar.getnames()
    assert any(n.endswith("bookclerk_plugin_sdk/workerd.py") for n in names)
    assert any(n.endswith("modules/plugin.py") for n in names)


def test_sync_embed_optional_vendor(tmp_path: Path):
    staging = tmp_path / "plugin"
    staging.mkdir()
    (staging / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    modules = staging / "modules"
    modules.mkdir()
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    assert "synced" in sync_embed(staging)
    assert (modules / "bookclerk_plugin_sdk" / "workerd.py").is_file()
    assert "ok" in check_plugin(staging)
