from pathlib import Path

import pytest

from bookclerk_plugin_sdk.tools import (
    check_plugin,
    env_properties_for,
    fmt_plugin_toml,
    format_manifest,
    generate_types,
    package_plugin,
    sync_embed,
    validate_plugin_id,
)

ROOT = Path(__file__).resolve().parents[3]
FIXTURES = ROOT / "crates" / "bookclerk-plugin-abi" / "fixtures" / "tools"
ECHO_PY = ROOT / "examples" / "plugins-echo-workerd-python"


def test_check_valid_workerd():
    msg = check_plugin(FIXTURES / "valid-workerd")
    assert "echo_workerd_tools" in msg


def test_check_invalid_outbound():
    with pytest.raises(ValueError, match="domains"):
        check_plugin(FIXTURES / "invalid-outbound-no-domains")


def test_check_valid_logo_url():
    msg = check_plugin(FIXTURES / "valid-logo-url")
    assert "logo_url" in msg


def test_check_valid_logo_path():
    msg = check_plugin(FIXTURES / "valid-logo-path")
    assert "logo_path" in msg


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


@pytest.mark.parametrize("padded", [" echo", "echo "])
def test_validate_plugin_id_rejects_whitespace(padded: str):
    with pytest.raises(ValueError, match="whitespace"):
        validate_plugin_id(padded)


@pytest.mark.parametrize("name", ["valid-native", "valid-workerd"])
def test_fmt_check_gold(name):
    gold = FIXTURES / name / "plugin.fmt.toml"
    assert "ok" in fmt_plugin_toml(gold, check_only=True)


def test_check_echo_python_workerd():
    msg = check_plugin(ECHO_PY)
    assert "echo_workerd_python" in msg


def test_package_python_vendors_sdk_package(tmp_path: Path):
    out = tmp_path / "dist"
    archive = package_plugin(ECHO_PY, out)
    assert archive.is_file()
    import tarfile

    with tarfile.open(archive, "r:gz") as tar:
        names = tar.getnames()
    assert any(n.endswith("bookclerk_plugin_sdk/workerd.py") for n in names)
    assert any(n.endswith("modules/plugin.py") for n in names)


def test_package_refuses_module_symlink_without_outside_bytes(tmp_path: Path):
    import tarfile

    from bookclerk_plugin_sdk.path_guard import copy_tree_no_symlinks

    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    (plugin / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    outside = tmp_path / "outside.txt"
    outside.write_text("SECRET_OUTSIDE_BYTES", encoding="utf-8")
    leak = modules / "leak.txt"
    leak.symlink_to(outside)

    dest = tmp_path / "copy"
    with pytest.raises(ValueError, match="symlink"):
        copy_tree_no_symlinks(modules, dest)

    out = tmp_path / "dist"
    with pytest.raises(ValueError, match="symlink"):
        package_plugin(plugin, out)
    assert not any(out.glob("*.tar.gz"))
    assert "SECRET_OUTSIDE_BYTES" not in "".join(
        p.read_text(encoding="utf-8", errors="ignore")
        for p in out.rglob("*")
        if p.is_file()
    )


def test_package_refuses_intermediate_dir_symlink(tmp_path: Path):
    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    (plugin / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    outside = tmp_path / "outside_mods"
    outside.mkdir()
    (outside / "x.py").write_text("x = 1\n", encoding="utf-8")
    (modules / "vendor").symlink_to(outside)
    out = tmp_path / "dist"
    with pytest.raises(ValueError, match="symlink"):
        package_plugin(plugin, out)


def test_package_refuses_version_path_traversal(tmp_path: Path):
    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    toml = (ECHO_PY / "plugin.toml").read_text(encoding="utf-8")
    toml = toml.replace('version = "1.0.0"', 'version = "../../../victim"', 1)
    (plugin / "plugin.toml").write_text(toml, encoding="utf-8")
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    out = tmp_path / "dist"
    out.mkdir()
    victim = tmp_path / "victim-workerd.tar.gz"
    victim.write_bytes(b"PREEXISTING")
    with pytest.raises(ValueError, match=r"\.\.|escape"):
        package_plugin(plugin, out)
    assert victim.read_bytes() == b"PREEXISTING"


def test_package_allows_symlinked_plugin_root(tmp_path: Path):
    real = tmp_path / "real"
    modules = real / "modules"
    modules.mkdir(parents=True)
    (real / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    link = tmp_path / "link"
    link.symlink_to(real)
    out = tmp_path / "dist"
    archive = package_plugin(link, out)
    assert archive.is_file()


def test_refuse_symlink_allows_symlinked_trusted_root(tmp_path: Path):
    from bookclerk_plugin_sdk.path_guard import refuse_symlink_path, resolve_under

    real = tmp_path / "real"
    real.mkdir()
    (real / "child.txt").write_text("ok\n", encoding="utf-8")
    link = tmp_path / "link"
    link.symlink_to(real)
    child = resolve_under(link, "child.txt")
    # Root may be a symlink; child must not be.
    assert refuse_symlink_path(link, child) == child


def test_refuse_symlink_blocks_bookclerk_dir_link(tmp_path: Path):
    from bookclerk_plugin_sdk.path_guard import refuse_symlink_path, resolve_under
    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config

    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    (plugin / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    outside = tmp_path / "outside_dir"
    outside.mkdir()
    (plugin / ".bookclerk").symlink_to(outside)
    with pytest.raises(ValueError, match="symlink|escape"):
        # realpath containment refuses the symlink escape at resolve_under;
        # refuse_symlink_path remains the suffix guard for non-escaping links.
        bookclerk = resolve_under(plugin, ".bookclerk")
        refuse_symlink_path(plugin, bookclerk)
    # Generated embeds go to a host session dir — plugin `.bookclerk` symlink is ignored.
    generated = materialize_config(
        plugin,
        __import__("tomllib").loads((plugin / "plugin.toml").read_text(encoding="utf-8")),
        listen_port=0,
        bridge_token="token",
    )
    assert not (outside / "bridge.js").exists()
    assert generated.state_dir != plugin
    assert (generated.state_dir / ".bookclerk" / "bridge.js").is_file()
    import shutil

    shutil.rmtree(generated.state_dir, ignore_errors=True)


def test_refuse_symlink_blocks_main_module_link(tmp_path: Path):
    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config
    import tomllib

    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    (plugin / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    outside = tmp_path / "outside_main.py"
    outside.write_text("# outside\n", encoding="utf-8")
    (modules / "plugin.py").symlink_to(outside)
    with pytest.raises(ValueError, match="symlink"):
        materialize_config(
            plugin,
            tomllib.loads((plugin / "plugin.toml").read_text(encoding="utf-8")),
            listen_port=0,
            bridge_token="token",
        )


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


def test_sync_embed_creates_absent_default_modules(tmp_path: Path):
    staging = tmp_path / "plugin"
    staging.mkdir()
    (staging / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    # No modules/ yet — ordinary first-time sync-embed.
    assert not (staging / "modules").exists()
    assert "synced" in sync_embed(staging)
    assert (staging / "modules" / "bookclerk_plugin_sdk" / "workerd.py").is_file()


def test_sync_embed_creates_nested_missing_modules_path(tmp_path: Path):
    staging = tmp_path / "plugin"
    staging.mkdir()
    toml = (ECHO_PY / "plugin.toml").read_text(encoding="utf-8")
    toml = toml.replace('modules_dir = "modules"', 'modules_dir = "mods/nested"')
    if 'modules_dir = "mods/nested"' not in toml:
        # Echo fixture may omit modules_dir (defaults to modules); inject under [workerd].
        toml = toml.replace(
            "[workerd]",
            '[workerd]\nmodules_dir = "mods/nested"',
            1,
        )
    (staging / "plugin.toml").write_text(toml, encoding="utf-8")
    assert "synced" in sync_embed(staging)
    assert (staging / "mods" / "nested" / "bookclerk_plugin_sdk" / "workerd.py").is_file()


def test_sync_embed_refuses_modules_and_leaf_symlinks(tmp_path: Path):
    staging = tmp_path / "plugin"
    staging.mkdir()
    (staging / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "workerd.py").write_text("SECRET", encoding="utf-8")
    modules = staging / "modules"
    modules.symlink_to(outside)
    with pytest.raises(ValueError, match="symlink"):
        sync_embed(staging)

    modules.unlink()
    modules.mkdir()
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    pkg = modules / "bookclerk_plugin_sdk"
    pkg.mkdir()
    (pkg / "workerd.py").symlink_to(outside / "workerd.py")
    with pytest.raises(ValueError, match="symlink"):
        sync_embed(staging)
    assert (outside / "workerd.py").read_text(encoding="utf-8") == "SECRET"


def test_sync_embed_refuses_dangling_package_dir_link(tmp_path: Path):
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
    (modules / "bookclerk_plugin_sdk").symlink_to(staging / "missing-pkg")
    with pytest.raises(ValueError, match="symlink"):
        sync_embed(staging)


def test_sync_embed_allows_symlinked_operator_root(tmp_path: Path):
    real = tmp_path / "real-plugin"
    real.mkdir()
    (real / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    modules = real / "modules"
    modules.mkdir()
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    link = tmp_path / "link-plugin"
    link.symlink_to(real)
    assert "synced" in sync_embed(link)
    assert (modules / "bookclerk_plugin_sdk" / "workerd.py").is_file()


def test_package_refuses_toml_leaf_symlink(tmp_path: Path):
    plugin = tmp_path / "plugin"
    plugin.mkdir()
    outside = tmp_path / "evil.toml"
    outside.write_text(
        (ECHO_PY / "plugin.toml").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    (plugin / "plugin.toml").symlink_to(outside)
    modules = plugin / "modules"
    modules.mkdir()
    (modules / "plugin.py").write_text(
        (ECHO_PY / "modules" / "plugin.py").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="symlink"):
        package_plugin(plugin, tmp_path / "dist")


def test_format_manifest_emits_sealed_and_loopback_tables():
    text = format_manifest(
        {
            "api_version": 3,
            "id": "echo",
            "runtime": "native",
            "command": "./echo",
            "entrypoints": ["cli"],
            "secrets": {"binding": "SECRETS"},
            "oauth": {"binding": "OAUTH"},
            "capabilities": {"network": {"mode": "deny"}},
        }
    )
    assert "\n[secrets]\n" in text
    assert "\n[oauth]\n" in text
    assert 'binding = "SECRETS"' in text
    assert 'binding = "OAUTH"' in text


def test_env_properties_include_sealed_and_loopback_bindings(tmp_path: Path):
    names = [
        name
        for name, _, _ in env_properties_for({"id": "echo", "secrets": {}, "oauth": {}})
    ]
    assert "SECRETS" in names
    assert "OAUTH" in names
    (tmp_path / "plugin.toml").write_text(
        format_manifest(
            {
                "api_version": 3,
                "id": "echo",
                "runtime": "native",
                "command": "./echo",
                "entrypoints": ["cli"],
                "secrets": {"binding": "SECRETS"},
                "oauth": {"binding": "OAUTH"},
                "capabilities": {"network": {"mode": "deny"}},
            }
        ),
        encoding="utf-8",
    )
    msg = generate_types(tmp_path)
    stub = (tmp_path / "bookclerk_configuration.py").read_text(encoding="utf-8")
    assert "wrote" in msg
    assert "SECRETS: dict[str, str]" in stub
    assert "OAUTH: Any" in stub
    assert "[secrets]" in stub
    assert "[oauth]" in stub


def test_write_file_under_refuses_leaf_symlink_and_allows_double_dot(tmp_path: Path):
    from bookclerk_plugin_sdk.path_guard import write_file_under

    root = tmp_path / "state"
    root.mkdir()
    written = write_file_under(root, "edition..2.js", "ok")
    assert written.read_text(encoding="utf-8") == "ok"
    victim = tmp_path / "victim"
    victim.write_text("VICTIM", encoding="utf-8")
    (root / "adapter.js").symlink_to(victim)
    with pytest.raises(ValueError, match="symlink"):
        write_file_under(root, "adapter.js", "NEW")
    assert victim.read_text(encoding="utf-8") == "VICTIM"


def test_materialize_refuses_supplied_state_leaf_symlink(tmp_path: Path):
    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config

    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    (modules / "main.js").write_text("export default class X {}\n", encoding="utf-8")
    state = tmp_path / "state"
    bookclerk = state / ".bookclerk"
    bookclerk.mkdir(parents=True)
    victim = tmp_path / "victim.js"
    victim.write_text("VICTIM", encoding="utf-8")
    (bookclerk / "adapter.js").symlink_to(victim)
    manifest = {
        "api_version": 3,
        "id": "leaf",
        "runtime": "workerd",
        "entrypoints": ["cli"],
        "workerd": {
            "compatibility_date": "2026-08-01",
            "main_module": "main.js",
            "modules_dir": "modules",
            "entrypoint": "default",
        },
        "capabilities": {"network": {"mode": "deny"}},
    }
    with pytest.raises(ValueError, match="symlink"):
        materialize_config(
            plugin,
            manifest,
            listen_port=0,
            bridge_token="token",
            state_dir=state,
        )
    assert victim.read_text(encoding="utf-8") == "VICTIM"


def test_materialize_nested_modules_and_double_dot_name(tmp_path: Path):
    import shutil

    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config

    plugin = tmp_path / "plugin"
    main = plugin / "dist" / "modules" / "nested" / "edition..2.js"
    main.parent.mkdir(parents=True)
    main.write_text("export default class Nested {}\n", encoding="utf-8")
    generated = materialize_config(
        plugin,
        {
            "api_version": 3,
            "id": "nested",
            "runtime": "workerd",
            "entrypoints": ["cli"],
            "workerd": {
                "compatibility_date": "2026-08-01",
                "main_module": "nested/edition..2.js",
                "modules_dir": "dist/modules",
                "entrypoint": "default",
            },
            "capabilities": {"network": {"mode": "deny"}},
        },
        listen_port=0,
        bridge_token="token",
    )
    text = generated.config_path.read_text(encoding="utf-8")
    assert "/dist/modules/nested/edition..2.js" in text
    shutil.rmtree(generated.state_dir, ignore_errors=True)


def test_workerd_cache_and_currency(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    from bookclerk_plugin_sdk.sparse_workerd.ensure import (
        _is_current,
        binary_name,
        default_cache_dir,
        ensure_workerd,
        load_pin,
    )

    cache = tmp_path / "wd-cache"
    monkeypatch.setenv("BOOKCLERK_WORKERD_CACHE", str(cache))
    assert default_cache_dir() == cache.resolve()

    pin = load_pin()
    stamp_dir = tmp_path / "stamped"
    stamp_dir.mkdir()
    (stamp_dir / pin["version_stamp"]).write_text(pin["release_tag"] + "\n", encoding="utf-8")
    missing = stamp_dir / "workerd"
    assert _is_current(missing, pin) is False
    present = stamp_dir / "present-workerd"
    present.write_bytes(b"not-executable-needed")
    assert _is_current(present, pin) is True

    # Symlink to a stamped regular file still counts as current (override path).
    link = stamp_dir / "link-workerd"
    link.symlink_to(present)
    assert _is_current(link, pin) is True

    probe_dir = tmp_path / "probe"
    probe_dir.mkdir()
    script = probe_dir / "fake-workerd"
    script.write_text(
        f"#!/bin/sh\nprintf '%s\\n' '{pin['release_tag']}'\n",
        encoding="utf-8",
    )
    script.chmod(0o755)
    assert _is_current(script, pin) is True
    script_link = probe_dir / "fake-workerd-link"
    script_link.symlink_to(script)
    assert _is_current(script_link, pin) is True

    # Explicit absolute override that is a symlink must be accepted without download.
    monkeypatch.setenv("BOOKCLERK_WORKERD_BIN", str(link))

    def _forbid_download(*_a, **_k):  # pragma: no cover - must not run
        raise AssertionError("ensure_workerd must not download when override matches")

    monkeypatch.setattr(
        "bookclerk_plugin_sdk.sparse_workerd.ensure.urllib.request.urlopen",
        _forbid_download,
    )
    got = ensure_workerd(cache_dir=cache)
    assert Path(got).resolve() == present.resolve()

    # No-stamp --version symlink override.
    monkeypatch.setenv("BOOKCLERK_WORKERD_BIN", str(script_link))
    got2 = ensure_workerd(cache_dir=cache)
    assert Path(got2).resolve() == script.resolve()

    # Mismatched override falls through; managed cache leaf symlink is refused.
    bad = tmp_path / "wrong-workerd"
    bad.write_bytes(b"nope")
    monkeypatch.setenv("BOOKCLERK_WORKERD_BIN", str(bad))
    managed = cache / binary_name()
    cache.mkdir(parents=True, exist_ok=True)
    if managed.exists() or managed.is_symlink():
        managed.unlink()
    outside = tmp_path / "outside-bin"
    outside.write_bytes(b"OUT")
    managed.symlink_to(outside)
    try:
        ensure_workerd(cache_dir=cache)
        raise AssertionError("expected managed cache symlink refusal")
    except ValueError as err:
        assert "symlink" in str(err).lower()
    assert outside.read_bytes() == b"OUT"