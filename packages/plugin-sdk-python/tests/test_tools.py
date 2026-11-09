import json
import shutil
from datetime import date, timedelta
from pathlib import Path
from typing import Any

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

# The repository root: the nearest folder above holding the ABI crate. A
# fixed depth would break when mutation testing runs these tests from a copy
# under `mutants/` (docs/contributing/ci.md#python-sdk).
ROOT = next(
    p for p in Path(__file__).resolve().parents if (p / "crates" / "bookclerk-plugin-abi").is_dir()
)
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


def test_check_warns_when_compatibility_date_is_newer_than_pin(tmp_path: Path, capsys):
    from bookclerk_plugin_sdk.tools import WORKERD_PIN_COMPAT_DATE

    # Copy the fixture and set its date to the day after the pin, so the
    # fallback boundary tracks every pin bump.
    newer = (date.fromisoformat(WORKERD_PIN_COMPAT_DATE) + timedelta(days=1)).isoformat()
    plugin = tmp_path / "valid-compat-date-future"
    shutil.copytree(FIXTURES / "valid-compat-date-future", plugin)
    manifest_path = plugin / "plugin.toml"
    lines = manifest_path.read_text(encoding="utf-8").splitlines(keepends=True)
    rewritten = "".join(
        f'compatibility_date = "{newer}"\n' if line.startswith("compatibility_date = ") else line
        for line in lines
    )
    assert newer in rewritten
    manifest_path.write_text(rewritten, encoding="utf-8")
    result = check_plugin(plugin)
    assert result.startswith("ok ")
    assert "Falling back" in capsys.readouterr().err


def test_check_rejects_non_calendar_compatibility_date():
    with pytest.raises(ValueError, match="YYYY-MM-DD"):
        check_plugin(FIXTURES / "invalid-compat-date-shape")


def test_check_rejects_unknown_compatibility_flag():
    with pytest.raises(ValueError, match="not allowed"):
        check_plugin(FIXTURES / "invalid-compat-flag")


def test_check_rejects_experimental_flag():
    with pytest.raises(ValueError, match="host-only"):
        check_plugin(FIXTURES / "invalid-compat-experimental")


def test_check_rejects_python_without_flag_pair():
    with pytest.raises(ValueError, match="must include"):
        check_plugin(FIXTURES / "invalid-python-flags-missing")


def test_check_rejects_python_flags_without_module():
    with pytest.raises(ValueError, match="Python module"):
        check_plugin(FIXTURES / "invalid-flags-without-python")


def test_check_rejects_module_type_mismatch():
    with pytest.raises(ValueError, match="does not match"):
        check_plugin(FIXTURES / "invalid-module-type")


def test_check_accepts_explicit_path_over_py_name():
    msg = check_plugin(FIXTURES / "valid-module-path-wins")
    assert "path_wins_over_py_name" in msg


def test_check_lints_js_main_when_a_python_helper_is_declared():
    msg = check_plugin(FIXTURES / "valid-js-with-python-helper")
    assert "js_main_python_helper" in msg


def test_check_rejects_module_path_when_name_matches_a_different_file():
    with pytest.raises(ValueError, match="not in the workerd load set"):
        check_plugin(FIXTURES / "invalid-module-path")
    with pytest.raises(ValueError, match=r"missing\.js"):
        check_plugin(FIXTURES / "invalid-module-path")


def test_check_accepts_module_name_when_path_is_omitted():
    msg = check_plugin(FIXTURES / "valid-module-name-only")
    assert "echo_workerd_name_only" in msg


def test_check_accepts_producer_that_shares_oauth_name():
    msg = check_plugin(FIXTURES / "valid-producer-shares-oauth-name")
    assert "producer_shares_oauth" in msg


@pytest.mark.parametrize(
    "name",
    [
        "invalid-kv-secret",
        "invalid-kv-work-fs",
        "invalid-kv-oauth-name",
        "invalid-producer-database",
    ],
)
def test_check_rejects_custom_binding_collisions(name: str):
    with pytest.raises(ValueError, match="collides"):
        check_plugin(FIXTURES / name)


def test_check_rejects_kv_oauth_binding():
    with pytest.raises(ValueError, match="collides"):
        check_plugin(FIXTURES / "invalid-kv-oauth")


def test_binding_errors_identify_the_row_not_the_name():
    from bookclerk_plugin_sdk.tools import validate_manifest

    base = {
        "api_version": 3,
        "id": "echo",
        "runtime": "native",
        "command": "./echo",
        "entrypoints": ["cli"],
        "capabilities": {"network": {"mode": "deny"}},
    }
    duplicate = {
        **base,
        "kv_namespaces": [{"binding": "CACHE"}, {"binding": "CACHE"}],
    }
    with pytest.raises(
        ValueError, match=r"\[kv_namespaces\]\[1\] binding collides"
    ) as duplicate_err:
        validate_manifest(duplicate)
    assert "CACHE" not in str(duplicate_err.value)

    reserved = {
        **base,
        "kv_namespaces": [{"binding": "CACHE"}, {"binding": "OAUTH"}],
    }
    with pytest.raises(
        ValueError, match=r"\[kv_namespaces\]\[1\] binding collides"
    ) as reserved_err:
        validate_manifest(reserved)
    assert "OAUTH" not in str(reserved_err.value)

    pattern = {
        **base,
        "events": {
            "producers": [
                {"type": "book_ready", "binding": "EVENTS"},
                {"type": "scan_done", "binding": "not-valid"},
            ]
        },
    }
    with pytest.raises(
        ValueError, match=r"\[events\.producers\]\[1\] binding must be"
    ) as pattern_err:
        validate_manifest(pattern)
    assert "not-valid" not in str(pattern_err.value)


def test_check_rejects_undeclared_python_file():
    with pytest.raises(ValueError, match="undeclared Python file"):
        check_plugin(FIXTURES / "invalid-undeclared-python")


def test_materialize_rejects_module_path_when_name_matches_a_different_file():
    import tomllib

    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config

    manifest = tomllib.loads(
        (FIXTURES / "invalid-module-path" / "plugin.toml").read_text(encoding="utf-8")
    )
    with pytest.raises(ValueError, match=r"missing\.js"):
        materialize_config(
            FIXTURES / "invalid-module-path",
            manifest,
            listen_port=0,
            bridge_token="token",
        )


def test_materialize_rejects_disk_only_python_even_with_both_flags():
    import tomllib

    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config
    from bookclerk_plugin_sdk.tools import module_load_key

    assert module_load_key("modules", "./modules/index.js") == "index.js"
    assert module_load_key("./modules", "modules/index.js") == "index.js"
    assert module_load_key("modules", "modules/pkg/./echo.wasm") == "pkg/echo.wasm"
    manifest = tomllib.loads(
        (FIXTURES / "invalid-undeclared-python" / "plugin.toml").read_text(encoding="utf-8")
    )
    with pytest.raises(ValueError, match="undeclared Python file"):
        materialize_config(
            FIXTURES / "invalid-undeclared-python",
            manifest,
            listen_port=0,
            bridge_token="token",
        )
    flagged = tomllib.loads(
        (FIXTURES / "invalid-undeclared-python" / "plugin.toml").read_text(encoding="utf-8")
    )
    flagged["workerd"]["compatibility_flags"] = [
        "python_workers",
        "disable_python_external_sdk",
    ]
    with pytest.raises(ValueError, match="Python module"):
        materialize_config(
            FIXTURES / "invalid-undeclared-python",
            flagged,
            listen_port=0,
            bridge_token="token",
        )


def test_check_rejects_typescript_main():
    with pytest.raises(ValueError, match="not implemented yet"):
        check_plugin(FIXTURES / "invalid-module-ts")


def test_check_accepts_kv_and_queues_declarations():
    assert "valid_kv" in check_plugin(FIXTURES / "valid-kv")
    assert "valid_queues" in check_plugin(FIXTURES / "valid-queues")


def test_check_rejects_plugin_producing_host_event():
    with pytest.raises(ValueError, match="emitted by the host"):
        check_plugin(FIXTURES / "invalid-producer-host-event")


def test_check_rejects_bad_kv_namespace():
    with pytest.raises(ValueError, match="namespace"):
        check_plugin(FIXTURES / "invalid-kv-namespace")


def test_materialize_accepts_kv_fixture():
    import tomllib

    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config

    manifest = tomllib.loads((FIXTURES / "valid-kv" / "plugin.toml").read_text(encoding="utf-8"))
    materialize_config(
        FIXTURES / "valid-kv",
        manifest,
        listen_port=0,
        bridge_token="token",
    )


def test_kv_rows_format_and_type():
    import tomllib

    from bookclerk_plugin_sdk.tools import render_env_types

    manifest = tomllib.loads((FIXTURES / "valid-kv" / "plugin.toml").read_text(encoding="utf-8"))
    rendered = format_manifest(manifest)
    assert 'namespace = "sync.cursors"' in rendered
    assert "max_value_bytes = 4096" in rendered
    assert "max_keys = 500" in rendered
    stub = render_env_types(manifest)
    assert "from bookclerk_plugin_sdk.workerd import KvNamespace" in stub
    assert "    KV: KvNamespace" in stub
    assert "    CURSORS: KvNamespace" in stub
    assert "sync.cursors" in stub


@pytest.mark.parametrize(
    "triggers",
    [
        {"crons": ["*/30 * * * *", "0 3 * * MON"]},
        {"jobs": ["echo.sync"], "crons": ["0 0 * * *"]},
    ],
)
def test_format_keeps_cron_and_job_triggers(triggers: dict[str, list[str]]) -> None:
    import tomllib

    rendered = format_manifest(
        {
            "api_version": 3,
            "id": "echo",
            "runtime": "native",
            "command": "./echo",
            "triggers": triggers,
            "capabilities": {"network": {"mode": "deny"}},
        }
    )
    assert tomllib.loads(rendered)["triggers"] == triggers


def test_format_keeps_queues_declaration():
    text = (FIXTURES / "valid-queues" / "plugin.toml").read_text(encoding="utf-8")
    import tomllib

    rendered = format_manifest(tomllib.loads(text))
    assert "[[queues.producers]]" in rendered
    assert "[[queues.consumers]]" in rendered
    assert 'dead_letter_queue = "jobs-dlq"' in rendered
    assert "not implemented" not in rendered.lower()


def test_validate_rejects_reserved_and_duplicate_queues():
    from bookclerk_plugin_sdk.tools import validate_manifest

    base = {
        "api_version": 3,
        "id": "echo",
        "runtime": "native",
        "command": "./echo",
        "entrypoints": ["cli"],
        "capabilities": {"network": {"mode": "deny"}},
    }
    with pytest.raises(ValueError, match="host-reserved"):
        validate_manifest(
            {**base, "queues": {"producers": [{"queue": "core.events", "binding": "Q"}]}}
        )
    with pytest.raises(ValueError, match="is duplicated"):
        validate_manifest({**base, "queues": {"consumers": [{"queue": "jobs"}, {"queue": "jobs"}]}})
    with pytest.raises(ValueError, match="collides"):
        validate_manifest(
            {**base, "queues": {"producers": [{"queue": "jobs", "binding": "EVENTS"}]}}
        )
    with pytest.raises(ValueError, match="must differ"):
        validate_manifest(
            {**base, "queues": {"consumers": [{"queue": "jobs", "dead_letter_queue": "jobs"}]}}
        )


def test_calendar_date_rejects_unicode_digits_and_unquoted_toml_dates():
    import tomllib

    from bookclerk_plugin_sdk.tools import validate_author_compatibility_date, validate_manifest

    with pytest.raises(ValueError, match="YYYY-MM-DD"):
        validate_author_compatibility_date("２０２６-０８-０１")
    text = """
api_version = 3
id = "echo"
runtime = "workerd"
entrypoints = ["cli"]
[workerd]
compatibility_date = 2026-08-01
main_module = "index.js"
[capabilities.network]
mode = "deny"
"""
    with pytest.raises(ValueError, match="compatibility_date"):
        validate_manifest(tomllib.loads(text))


def test_format_manifest_does_not_insert_python_flags():
    rendered = format_manifest(
        {
            "api_version": 3,
            "id": "echo",
            "runtime": "workerd",
            "entrypoints": ["cli"],
            "workerd": {
                "compatibility_date": "2026-08-01",
                "main_module": "plugin.py",
            },
            "modules": [{"name": "plugin.py", "type": "python"}],
            "capabilities": {"network": {"mode": "deny"}},
        }
    )
    assert "python_workers" not in rendered


def test_format_omits_module_path_when_absent():
    rendered = format_manifest(
        {
            "api_version": 3,
            "id": "echo",
            "runtime": "workerd",
            "entrypoints": ["cli"],
            "workerd": {
                "compatibility_date": "2026-08-01",
                "main_module": "index.js",
            },
            "modules": [{"name": "index.js"}],
        }
    )
    assert 'name = "index.js"' in rendered
    assert "path =" not in rendered


def test_check_rejects_native_with_domains():
    with pytest.raises(ValueError, match="only valid for runtime"):
        check_plugin(FIXTURES / "invalid-native-with-domains")


@pytest.mark.parametrize("padded", [" echo", "echo "])
def test_validate_plugin_id_rejects_whitespace(padded: str):
    with pytest.raises(ValueError, match="whitespace"):
        validate_plugin_id(padded)


# Fixtures with a formatted gold: the Rust `bookclerk-plugin fmt` output,
# which every SDK's formatter must reproduce byte for byte.
FMT_GOLDS = ["valid-native", "valid-workerd", "fmt-full"]


@pytest.mark.parametrize("name", FMT_GOLDS)
def test_fmt_check_gold(name):
    gold = FIXTURES / name / "plugin.fmt.toml"
    assert "ok" in fmt_plugin_toml(gold, check_only=True)


@pytest.mark.parametrize("name", FMT_GOLDS)
def test_format_manifest_matches_gold(name: str) -> None:
    import tomllib

    source = (FIXTURES / name / "plugin.toml").read_text(encoding="utf-8")
    gold = (FIXTURES / name / "plugin.fmt.toml").read_text(encoding="utf-8")
    assert format_manifest(tomllib.loads(source)) == gold


def test_fmt_rewrites_to_gold(tmp_path: Path) -> None:
    target = tmp_path / "plugin.toml"
    shutil.copy(FIXTURES / "fmt-full" / "plugin.toml", target)
    assert "wrote" in fmt_plugin_toml(target, check_only=False)
    gold = (FIXTURES / "fmt-full" / "plugin.fmt.toml").read_text(encoding="utf-8")
    assert target.read_text(encoding="utf-8") == gold
    assert "ok" in fmt_plugin_toml(target, check_only=True)


def test_fmt_check_rejects_unformatted() -> None:
    with pytest.raises(ValueError, match="would reformat"):
        fmt_plugin_toml(FIXTURES / "fmt-full" / "plugin.toml", check_only=True)


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
        p.read_text(encoding="utf-8", errors="ignore") for p in out.rglob("*") if p.is_file()
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
        (ECHO_PY / "plugin.toml")
        .read_text(encoding="utf-8")
        .replace('[[kv_namespaces]]\nbinding = "KV"\n', ""),
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
    import tomllib

    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config

    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    (plugin / "plugin.toml").write_text(
        (ECHO_PY / "plugin.toml")
        .read_text(encoding="utf-8")
        .replace('[[kv_namespaces]]\nbinding = "KV"\n', ""),
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
    names = [name for name, _, _ in env_properties_for({"id": "echo", "secrets": {}, "oauth": {}})]
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


def test_materialize_falls_back_when_compatibility_date_is_newer_than_pin(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
):
    from bookclerk_plugin_sdk.sparse_workerd.config import materialize_config
    from bookclerk_plugin_sdk.tools import WORKERD_PIN_COMPAT_DATE

    # One day past the pin, so the boundary tracks every pin bump.
    newer = (date.fromisoformat(WORKERD_PIN_COMPAT_DATE) + timedelta(days=1)).isoformat()
    plugin = tmp_path / "plugin"
    modules = plugin / "modules"
    modules.mkdir(parents=True)
    (modules / "index.js").write_text("export default class X {}\n", encoding="utf-8")
    generated = materialize_config(
        plugin,
        {
            "api_version": 3,
            "id": "echo",
            "runtime": "workerd",
            "entrypoints": ["cli"],
            "workerd": {
                "compatibility_date": newer,
                "main_module": "index.js",
                "modules_dir": "modules",
                "entrypoint": "default",
            },
            "capabilities": {"network": {"mode": "deny"}},
        },
        listen_port=0,
        bridge_token="token",
    )
    text = generated.config_path.read_text(encoding="utf-8")
    assert f'compatibilityDate = "{WORKERD_PIN_COMPAT_DATE}"' in text
    assert newer not in text
    assert "Falling back" in capsys.readouterr().err


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


def test_manifest_capabilities_report_queues():
    from bookclerk_plugin_sdk.sparse_workerd.config import manifest_capabilities

    caps = manifest_capabilities(
        {
            "id": "q",
            "entrypoints": ["cli"],
            "queues": {
                "producers": [
                    {"queue": "jobs", "binding": "JOBS"},
                    {"queue": "jobs", "binding": "JOBS_AGAIN"},
                ],
                "consumers": [{"queue": "notify"}],
            },
        }
    )
    assert caps["queueConsumers"] == ["notify"]
    assert caps["queueProducers"] == ["jobs"]
    assert caps["bindings"] == ["JOBS", "JOBS_AGAIN"]


# The `types` stub for the every-section fixture, reviewed by hand once and
# compared byte for byte so a change to any binding, type or doc line shows.
ENV_GOLD = Path(__file__).parent / "golden" / "fmt-full.env.txt"


def test_render_env_types_matches_gold():
    import tomllib

    from bookclerk_plugin_sdk.tools import render_env_types

    manifest = tomllib.loads((FIXTURES / "fmt-full" / "plugin.toml").read_text(encoding="utf-8"))
    assert render_env_types(manifest) == ENV_GOLD.read_text(encoding="utf-8")


def test_render_env_types_minimal_manifest_imports_nothing_extra():
    from bookclerk_plugin_sdk.tools import render_env_types

    stub = render_env_types({"id": "bare"})
    assert "    CONFIG: dict[str, Any]\n" in stub
    assert "DatabaseBinding" not in stub
    assert "bookclerk_plugin_sdk.workerd" not in stub
    assert [name for name, _, _ in env_properties_for({"id": "bare", "vars": {}})] == ["CONFIG"]


def test_env_properties_default_binding_names():
    manifest = {
        "id": "defaults",
        "secrets": {},
        "work_fs": {},
        "kv_namespaces": [{}],
        "oauth": {},
        "events": {"producers": [{"type": "a_done"}, {"type": "b_done"}]},
    }
    props = {name: (type_, doc) for name, type_, doc in env_properties_for(manifest)}
    assert list(props) == ["CONFIG", "SECRETS", "EVENTS", "WORK_FS", "KV", "OAUTH"]
    assert props["EVENTS"][1] == "`[[events.producers]]` outbox publisher (a_done, b_done)."


def test_env_properties_var_types():
    manifest = {"id": "v", "vars": {"t": {"k": 1}, "d": date(2026, 1, 1)}}
    config = env_properties_for(manifest)[0][1]
    assert config == 'dict[str, Any]  # {"d": Any, "t": dict[str, Any]}'


@pytest.mark.parametrize("binding", ["CONFIG", "SECRETS", "EVENTS", "WORK_FS", "KV", "OAUTH"])
def test_env_properties_database_cannot_take_a_host_binding(binding: str):
    with pytest.raises(ValueError, match="collides with a Bookclerk binding"):
        env_properties_for({"id": "x", "databases": [{"binding": binding}]})


def test_env_properties_refuse_a_binding_declared_twice():
    manifest = {"id": "x", "databases": [{"binding": "DB"}], "kv_namespaces": [{"binding": "DB"}]}
    with pytest.raises(ValueError, match="declared twice"):
        env_properties_for(manifest)


def _native_plugin(tmp_path: Path, extra: str = "") -> Path:
    """Copy the native fixture beside a stand-in binary and return its root."""
    plugin = tmp_path / "native"
    plugin.mkdir()
    toml = (FIXTURES / "valid-native" / "plugin.toml").read_text(encoding="utf-8")
    (plugin / "plugin.toml").write_text(toml + extra, encoding="utf-8")
    binary = plugin / "echo-tools"
    binary.write_bytes(b"\x7fELF stand-in")
    binary.chmod(0o600)
    return plugin


def _members(archive: Path) -> dict[str, tuple[int, bytes]]:
    """Each regular file in an archive: its mode bits and bytes."""
    import tarfile

    with tarfile.open(archive, "r:gz") as tar:
        return {
            info.name: (info.mode & 0o777, tar.extractfile(info).read())  # type: ignore[union-attr]
            for info in tar.getmembers()
            if info.isfile()
        }


def test_package_native_archive_layout_and_checksum(tmp_path: Path):
    import hashlib

    from bookclerk_plugin_sdk.tools import _host_target

    plugin = _native_plugin(tmp_path)
    out = tmp_path / "dist" / "nested"
    archive = package_plugin(plugin, out)

    assert archive == out / f"bookclerk-plugin-echo_tools-0.1.0-{_host_target()}.tar.gz"
    members = _members(archive)
    assert set(members) == {"./plugin.toml", "./echo-tools"}
    assert members["./plugin.toml"][1] == (plugin / "plugin.toml").read_bytes()
    assert members["./echo-tools"] == (0o755, b"\x7fELF stand-in")
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert (out / "SHA256SUMS").read_text(encoding="utf-8") == f"{digest}  {archive.name}\n"
    # Only the archive and its checksums stay behind: no staging or attempt dirs.
    assert sorted(p.name for p in out.iterdir()) == sorted([archive.name, "SHA256SUMS"])


def test_package_native_replaces_its_own_checksum_line(tmp_path: Path):
    import hashlib

    plugin = _native_plugin(tmp_path)
    out = tmp_path / "dist"
    out.mkdir()
    (out / "SHA256SUMS").write_text("abc  other.tar.gz\n\n", encoding="utf-8")
    package_plugin(plugin, out)
    (plugin / "echo-tools").write_bytes(b"rebuilt")
    archive = package_plugin(plugin, out)
    lines = (out / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert lines == ["abc  other.tar.gz", f"{digest}  {archive.name}"]


def test_package_native_reuses_a_stale_staging_dir(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    out = tmp_path / "dist"
    stale = out / ".staging-echo_tools"
    stale.mkdir(parents=True)
    (stale / "leftover").write_text("old", encoding="utf-8")
    archive = package_plugin(plugin, out)
    assert "./leftover" not in _members(archive)
    assert not stale.exists()


def test_package_native_without_version_uses_zero(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    (plugin / "plugin.toml").write_text(toml.replace('version = "0.1.0"\n', ""), encoding="utf-8")
    assert "-0.0.0-" in package_plugin(plugin, tmp_path / "dist").name


def test_package_native_absolute_command(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    built = tmp_path / "build" / "echo-tools-bin"
    built.parent.mkdir()
    built.write_bytes(b"absolute")
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    toml = toml.replace('command = "./echo-tools"', f"command = {str(built)!r}")
    (plugin / "plugin.toml").write_text(toml, encoding="utf-8")
    members = _members(package_plugin(plugin, tmp_path / "dist"))
    assert members["./echo-tools-bin"] == (0o755, b"absolute")


def test_package_native_missing_binary(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    (plugin / "echo-tools").unlink()
    with pytest.raises(FileNotFoundError, match="native binary not found for package"):
        package_plugin(plugin, tmp_path / "dist")


def test_package_native_refuses_a_binary_symlink(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    real = tmp_path / "real-binary"
    real.write_bytes(b"outside")
    (plugin / "echo-tools").unlink()
    (plugin / "echo-tools").symlink_to(real)
    with pytest.raises((ValueError, FileNotFoundError)):
        package_plugin(plugin, tmp_path / "dist")


def test_package_copies_an_embedded_logo(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    toml = toml.replace('runtime = "native"', 'logo = "assets/logo.png"\nruntime = "native"')
    (plugin / "plugin.toml").write_text(toml, encoding="utf-8")
    (plugin / "assets").mkdir()
    (plugin / "assets" / "logo.png").write_bytes(b"\x89PNG")
    members = _members(package_plugin(plugin, tmp_path / "dist"))
    assert members["./assets/logo.png"][1] == b"\x89PNG"


def test_package_refuses_a_missing_embedded_logo(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    toml = toml.replace('runtime = "native"', 'logo = "assets/logo.png"\nruntime = "native"')
    (plugin / "plugin.toml").write_text(toml, encoding="utf-8")
    (plugin / "assets").mkdir()
    with pytest.raises(FileNotFoundError, match="embedded logo missing for package"):
        package_plugin(plugin, tmp_path / "dist")


def test_package_skips_a_url_logo(tmp_path: Path):
    plugin = _native_plugin(tmp_path)
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    toml = toml.replace(
        'runtime = "native"', 'logo = "https://example.com/logo.png"\nruntime = "native"'
    )
    (plugin / "plugin.toml").write_text(toml, encoding="utf-8")
    assert set(_members(package_plugin(plugin, tmp_path / "dist"))) == {
        "./plugin.toml",
        "./echo-tools",
    }


def test_validate_accepts_and_rejects_cron_triggers():
    from bookclerk_plugin_sdk.tools import MAX_CRON_TRIGGERS, validate_manifest

    base = {
        "api_version": 3,
        "id": "echo",
        "runtime": "native",
        "command": "./echo",
        "capabilities": {"network": {"mode": "deny"}},
    }
    # Crons alone satisfy "declare at least one entrypoint or trigger".
    validate_manifest({**base, "triggers": {"crons": ["*/30 * * * *", "0 3 * * MON-FRI"]}})
    with pytest.raises(ValueError, match="array of strings"):
        validate_manifest({**base, "triggers": {"crons": "* * * * *"}})
    with pytest.raises(ValueError, match="array of strings"):
        validate_manifest({**base, "entrypoints": ["cli"], "triggers": {"crons": False}})
    with pytest.raises(ValueError, match="at most"):
        validate_manifest(
            {
                **base,
                "triggers": {"crons": [f"{m} * * * *" for m in range(MAX_CRON_TRIGGERS + 1)]},
            }
        )
    with pytest.raises(ValueError, match="ASCII string"):
        validate_manifest({**base, "triggers": {"crons": [5]}})
    with pytest.raises(ValueError, match="needs 5 fields"):
        validate_manifest({**base, "triggers": {"crons": ["* * * *"]}})
    with pytest.raises(ValueError, match="is not valid cron"):
        validate_manifest({**base, "triggers": {"crons": ["@daily * * * *"]}})
    with pytest.raises(ValueError, match="is duplicated"):
        validate_manifest({**base, "triggers": {"crons": ["0 * * * *", "0  *  * * *"]}})
    for registry in ("storefront", "storage", "databaseAdapter"):
        with pytest.raises(ValueError, match="cannot be combined"):
            validate_manifest(
                {**base, "entrypoints": [registry], "triggers": {"crons": ["0 0 * * *"]}}
            )


def test_smoke_health_probe_covers_every_health_entrypoint() -> None:
    from bookclerk_plugin_sdk.sparse_workerd.smoke import _health_ifaces

    assert _health_ifaces(["cli", "metadata", "storefront"]) == [
        "ContentSource",
        "MetadataProvider",
    ]
    assert _health_ifaces(["remoteLibrary"]) == ["RemoteLibrary"]
    assert _health_ifaces(["cli"]) == []
    assert _health_ifaces([]) == []


def test_smoke_health_fails_when_not_ready() -> None:
    from bookclerk_plugin_sdk.sparse_workerd.smoke import _require_healthy

    ready = {"ok": True, "detail": ""}
    assert _require_healthy("MetadataProvider", ready) is ready
    with pytest.raises(RuntimeError, match="not ready: catalog down"):
        _require_healthy("MetadataProvider", {"ok": False, "detail": "catalog down"})
    with pytest.raises(RuntimeError, match="not ready: no detail"):
        _require_healthy("ContentSource", {"ok": False})


def test_smoke_health_codecs_cover_every_health_interface() -> None:
    from bookclerk_plugin_sdk import _wire
    from bookclerk_plugin_sdk.sparse_workerd.smoke import _HEALTH_INTERFACES, _health_codecs

    for _, iface in _HEALTH_INTERFACES:
        params, results = _health_codecs(iface)
        assert params is not None and results is not None
    assert _health_codecs("CoverFetcher") == (
        _wire._cover_fetcher_health_params_codec,
        _wire._cover_fetcher_health_results_codec,
    )
    assert _health_codecs("MetadataProvider") == (
        _wire._metadata_provider_health_params_codec,
        _wire._metadata_provider_health_results_codec,
    )
    with pytest.raises(ValueError, match="no health probe"):
        _health_codecs("Storage")


# Shared with the Rust and TypeScript checkers: every SDK must classify each
# logo the same way and give the same error text.
LOGO_CASES = json.loads((FIXTURES / "logo-cases.json").read_text(encoding="utf-8"))
COMPAT_DATES = json.loads((FIXTURES / "compat-date-cases.json").read_text(encoding="utf-8"))


def _plain_id(text: str) -> str:
    """Spell a case input as a test id that pytest accepts back on its command line.

    pytest escapes backslashes and non-ASCII characters in the ids it reports,
    and a node id written that way is not found again, so mutmut, which runs
    tests by node id, cannot select the case. Every other character is written
    as ``U+XXXX``.

    Args:
        text: The case's input.

    Returns:
        A printable-ASCII id without backslashes (``empty`` for ``""``).
    """
    if not text:
        return "empty"
    return "".join(c if " " <= c <= "~" and c != "\\" else f"U+{ord(c):04X}" for c in text)


@pytest.mark.parametrize("case", LOGO_CASES, ids=lambda c: _plain_id(c["input"]))
def test_validate_logo_matches_shared_cases(case: dict[str, str]) -> None:
    from bookclerk_plugin_sdk.tools import validate_logo

    if "error" in case:
        with pytest.raises(ValueError) as err:
            validate_logo(case["input"])
        assert str(err.value) == case["error"]
    else:
        kind = "remote" if "remote" in case else "embedded"
        assert validate_logo(case["input"]) == (kind, case[kind])


@pytest.mark.parametrize("date_text", COMPAT_DATES["valid"])
def test_compatibility_date_accepts_shared_valid(date_text: str) -> None:
    from bookclerk_plugin_sdk.tools import validate_author_compatibility_date

    validate_author_compatibility_date(date_text)


@pytest.mark.parametrize("date_text", COMPAT_DATES["invalid"], ids=_plain_id)
def test_compatibility_date_rejects_shared_invalid(date_text: str) -> None:
    from bookclerk_plugin_sdk.tools import validate_author_compatibility_date

    with pytest.raises(
        ValueError,
        match=r"^plugin\.toml: workerd\.compatibility_date must be a calendar YYYY-MM-DD$",
    ):
        validate_author_compatibility_date(date_text)


MANIFEST_CASES = json.loads((FIXTURES / "manifest-cases.json").read_text(encoding="utf-8"))


@pytest.mark.parametrize("case", MANIFEST_CASES["cases"], ids=lambda c: c["name"])
def test_validate_manifest_matches_shared_cases(case: dict[str, Any]) -> None:
    import tomllib

    from bookclerk_plugin_sdk.tools import validate_manifest

    text = (
        MANIFEST_CASES["prelude"]
        + case.get("top", MANIFEST_CASES["top"])
        + "\n"
        + case.get("base_tables", MANIFEST_CASES["base_tables"])
        + "\n"
        + case.get("tables", "")
    )
    manifest = tomllib.loads(text)
    if case["error"] is None:
        validate_manifest(manifest)
    else:
        with pytest.raises(ValueError) as err:
            validate_manifest(manifest)
        assert str(err.value) == case["error"]


MAIN_MODULE_CASES = json.loads((FIXTURES / "main-module-cases.json").read_text(encoding="utf-8"))


@pytest.mark.parametrize("case", MAIN_MODULE_CASES, ids=lambda c: c["name"])
def test_check_main_module_source_matches_shared_cases(case: dict[str, Any]) -> None:
    from bookclerk_plugin_sdk.tools import check_main_module_source

    main = "index.js" if case["language"] == "js" else "main.py"
    args = (main, case["src"], case["entrypoints"], case["language"])
    if case["error"] is None:
        check_main_module_source(*args)
    else:
        with pytest.raises(ValueError) as err:
            check_main_module_source(*args)
        assert str(err.value) == case["error"]


def _python_workerd_plugin(tmp_path: Path) -> Path:
    staging = tmp_path / "plugin"
    staging.mkdir()
    shutil.copyfile(ECHO_PY / "plugin.toml", staging / "plugin.toml")
    return staging


def test_sync_embed_writes_the_sdk_package(tmp_path: Path) -> None:
    from bookclerk_plugin_sdk.tools import _sdk_workerd_embed_src

    staging = _python_workerd_plugin(tmp_path)
    pkg = staging / "modules" / "bookclerk_plugin_sdk"
    assert sync_embed(staging) == f"synced {pkg / 'workerd.py'}"
    assert (pkg / "workerd.py").read_bytes() == _sdk_workerd_embed_src().read_bytes()
    assert (pkg / "__init__.py").read_text(encoding="utf-8") == (
        '"""Bookclerk plugin SDK (vendored for workerd). Prefer .workerd."""\n'
    )
    assert sorted(p.name for p in pkg.iterdir()) == ["__init__.py", "workerd.py"]


def test_sync_embed_keeps_an_existing_init(tmp_path: Path) -> None:
    staging = _python_workerd_plugin(tmp_path)
    pkg = staging / "modules" / "bookclerk_plugin_sdk"
    pkg.mkdir(parents=True)
    (pkg / "__init__.py").write_text("# the author's own\n", encoding="utf-8")
    sync_embed(staging)
    assert (pkg / "__init__.py").read_text(encoding="utf-8") == "# the author's own\n"


def test_sync_embed_refuses_a_native_plugin(tmp_path: Path) -> None:
    staging = tmp_path / "plugin"
    shutil.copytree(FIXTURES / "valid-native", staging)
    with pytest.raises(ValueError) as err:
        sync_embed(staging)
    assert str(err.value) == 'sync-embed requires runtime = "workerd"'
    assert not (staging / "modules").exists()


def test_sync_embed_refuses_a_javascript_plugin(tmp_path: Path) -> None:
    staging = tmp_path / "plugin"
    shutil.copytree(FIXTURES / "valid-workerd", staging)
    with pytest.raises(ValueError) as err:
        sync_embed(staging)
    assert str(err.value) == (
        "sync-embed (Python SDK): main_module must end with .py (got 'index.js')"
    )
    assert not (staging / "modules" / "bookclerk_plugin_sdk").exists()


@pytest.mark.parametrize(
    ("sysname", "machine", "target"),
    [
        ("linux", "x86_64", "linux-x64-gnu"),
        ("linux", "AMD64", "linux-x64-gnu"),
        ("linux", "aarch64", "linux-arm64"),
        ("linux", "ARM64", "linux-arm64"),
        ("darwin", "arm64", "macos-arm64"),
        ("darwin", "aarch64", "macos-arm64"),
        ("darwin", "x86_64", "macos-x64"),
        ("darwin", "amd64", "macos-x64"),
        ("win32", "AMD64", "windows-x64"),
        ("win32", "x86_64", "windows-x64"),
        ("win32", "ARM64", "win32-arm64"),
        ("linux", "riscv64", "linux-riscv64"),
        ("freebsd14", "amd64", "freebsd14-amd64"),
        ("darwinish", "arm64", "darwinish-arm64"),
    ],
)
def test_host_target_names_each_platform(
    monkeypatch: pytest.MonkeyPatch, sysname: str, machine: str, target: str
) -> None:
    import platform
    import sys

    from bookclerk_plugin_sdk.tools import _host_target

    monkeypatch.setattr(sys, "platform", sysname)
    monkeypatch.setattr(platform, "machine", lambda: machine)
    assert _host_target() == target


def _copy_fixture(tmp_path: Path, name: str) -> Path:
    staging = tmp_path / name
    shutil.copytree(FIXTURES / name, staging)
    return staging


def _rewrite(path: Path, old: str, new: str) -> None:
    text = path.read_text(encoding="utf-8")
    assert old in text
    path.write_text(text.replace(old, new), encoding="utf-8")


def test_check_plugin_reports_every_entrypoint(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-workerd")
    _rewrite(staging / "plugin.toml", 'entrypoints = ["cli"]', 'entrypoints = ["cli", "storage"]')
    _rewrite(
        staging / "modules" / "index.js",
        "export class Cli",
        "export class Storage extends StorageEntrypoint {}\nexport class Cli",
    )
    assert (
        check_plugin(staging) == "ok id=echo_workerd_tools entrypoints=cli,storage runtime=workerd"
    )


def test_check_plugin_runs_the_author_model_check_on_js(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-workerd")
    _rewrite(staging / "modules" / "index.js", "export class Cli", "class Cli")
    with pytest.raises(ValueError) as err:
        check_plugin(staging)
    assert str(err.value) == (
        'index.js: entrypoints = ["cli"] requires `export class Cli extends CliEntrypoint`'
    )


def test_check_plugin_runs_the_author_model_check_on_mjs(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-workerd")
    _rewrite(staging / "plugin.toml", 'main_module = "index.js"', 'main_module = "index.mjs"')
    main = staging / "modules" / "index.mjs"
    (staging / "modules" / "index.js").rename(main)
    _rewrite(main, "export class Cli", "class Cli")
    with pytest.raises(ValueError) as err:
        check_plugin(staging)
    assert str(err.value).startswith('index.mjs: entrypoints = ["cli"] requires `export class')


def test_check_plugin_names_a_missing_modules_dir(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-workerd")
    shutil.rmtree(staging / "modules")
    with pytest.raises(FileNotFoundError) as err:
        check_plugin(staging)
    assert str(err.value) == f"workerd modules_dir missing: {staging / 'modules'}"


def test_check_plugin_names_a_missing_main_module(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-workerd")
    (staging / "modules" / "index.js").unlink()
    with pytest.raises(FileNotFoundError) as err:
        check_plugin(staging)
    assert str(err.value) == f"workerd main_module missing: {staging / 'modules' / 'index.js'}"


def test_check_plugin_names_a_missing_embedded_logo(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-logo-path")
    (staging / "assets" / "logo.png").unlink()
    with pytest.raises(FileNotFoundError) as err:
        check_plugin(staging)
    assert str(err.value) == f"embedded logo missing: {staging / 'assets' / 'logo.png'}"


def test_check_plugin_does_not_look_for_a_remote_logo(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-logo-url")
    assert check_plugin(staging).startswith("ok id=")


def test_check_plugin_native_binary_is_optional_by_default(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-native")
    assert check_plugin(staging) == "ok id=echo_tools entrypoints=cli runtime=native"


def test_check_plugin_require_binary_refuses_a_missing_command(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-native")
    (staging / ".require-binary").touch()
    with pytest.raises(FileNotFoundError) as err:
        check_plugin(staging)
    assert str(err.value) == f"native command not found: {staging / 'echo-tools'}"
    (staging / "echo-tools").touch()
    assert check_plugin(staging) == "ok id=echo_tools entrypoints=cli runtime=native"


def test_check_plugin_require_binary_resolves_an_absolute_command(tmp_path: Path) -> None:
    staging = _copy_fixture(tmp_path, "valid-native")
    binary = tmp_path / "elsewhere" / "echo-tools"
    _rewrite(
        staging / "plugin.toml", 'command = "./echo-tools"', f'command = "{binary.as_posix()}"'
    )
    (staging / ".require-binary").touch()
    with pytest.raises(FileNotFoundError) as err:
        check_plugin(staging)
    assert str(err.value) == f"native command not found: {binary}"
    binary.parent.mkdir()
    binary.touch()
    assert check_plugin(staging).startswith("ok id=echo_tools")


@pytest.mark.parametrize(
    ("host", "valid"),
    [
        ("api.example.com", True),
        ("*.Libro.fm.", True),
        ("*.bücher.example", True),
        ("*.", False),
        (" * ", False),
        ("api.example.com:443", False),
        ("https://api.example.com", False),
        ("a b.example", False),
        ("x%41.example", False),
        ("bad\u200e.example", False),
        ("a,b.example", False),
        ("a.example\uff0cb.example", False),
        ("xn--.b\u00fccher.example", False),
        ("xn--.example", False),
        ("xn--99999999.example", False),
        ("xn--bcher-kva.example", True),
        ("*.api.123", True),
    ],
    ids=[
        "exact",
        "wildcard-trailing-dot",
        "unicode-wildcard",
        "bare-wildcard-dot",
        "padded-star",
        "port",
        "scheme",
        "space",
        "percent",
        "disallowed-idna",
        "comma",
        "fullwidth-comma",
        "unicode-with-empty-punycode",
        "empty-punycode",
        "bad-punycode",
        "a-label",
        "numeric-last-label",
    ],
)
def test_own_user_agent_hosts_match_the_host_rules(host: str, valid: bool) -> None:
    from bookclerk_plugin_sdk.tools import _is_own_user_agent_host

    assert _is_own_user_agent_host(host) is valid


@pytest.mark.parametrize(
    ("own", "message"),
    [
        ("not a table", "capabilities.network.own_user_agent must be a table"),
        ({"hosts": [], "reason": "r"}, "hosts must list between 1 and 32 hosts"),
        ({"hosts": ["a.example"] * 33, "reason": "r"}, "hosts must list between 1 and 32 hosts"),
        ({"hosts": "a.example", "reason": "r"}, "hosts must list between 1 and 32 hosts"),
        ({"hosts": ["a.example"], "reason": None}, "reason must be one line"),
        ({"hosts": [7], "reason": "r"}, "hosts entry `7` is not a valid hostname"),
        ({"hosts": ["a.example"], "reason": "x" * 301}, "reason must be one line"),
        ({"hosts": ["a.example"], "reason": "two\nlines"}, "reason must be one line"),
        (
            {"hosts": ["a.example"], "reason": "r", "extra": True},
            "unknown field `extra`",
        ),
    ],
)
def test_own_user_agent_table_is_validated(own: Any, message: str) -> None:
    from bookclerk_plugin_sdk.tools import _validate_own_user_agent

    with pytest.raises(ValueError, match=message):
        _validate_own_user_agent({"mode": "outbound", "own_user_agent": own})


def test_own_user_agent_needs_outbound_mode() -> None:
    from bookclerk_plugin_sdk.tools import _validate_own_user_agent

    _validate_own_user_agent({"mode": "outbound"})
    _validate_own_user_agent(
        {"mode": "outbound", "own_user_agent": {"hosts": ["a.example"], "reason": "App API."}}
    )
    with pytest.raises(ValueError, match='needs mode = "outbound"'):
        _validate_own_user_agent(
            {"mode": "deny", "own_user_agent": {"hosts": ["a.example"], "reason": "r"}}
        )


def test_own_user_agent_hosts_are_put_in_ascii_form() -> None:
    from bookclerk_plugin_sdk.sparse_workerd.config import own_user_agent_hosts_for

    assert own_user_agent_hosts_for(
        [" *.Bücher.example. ", "API.example.com", 7, "\u200e", ""]
    ) == ["*.xn--bcher-kva.example", "api.example.com"]
    # UTS 46 keeps `ß` (IDNA 2003 would map it to `ss`), as Node and the host do.
    assert own_user_agent_hosts_for(["*.Faß.de", "xn--.example"]) == ["*.xn--fa-hia.de"]
    # Case and compatibility forms map first, as the host's IDNA step does.
    assert own_user_agent_hosts_for(["*.FAẞ.de", "*.ｆａß.de", "*.ＥＸＡＭＰＬＥ．com"]) == [
        "*.xn--fa-hia.de",
        "*.xn--fa-hia.de",
        "*.example.com",
    ]
    # Characters UTS 46 ignores are dropped; a joiner outside a virama is refused.
    assert own_user_agent_hosts_for(["*.fa\u00adß.de", "x\ufe0fy.example", "a\u200db.example"]) == [
        "*.xn--fa-hia.de",
        "xy.example",
    ]


def test_format_manifest_writes_the_defaults_a_manifest_leaves_out():
    import tomllib

    rendered = format_manifest(
        {
            "api_version": 3,
            "id": "echo",
            "workerd": {
                "compatibility_date": "2026-08-01",
                "main_module": "index.js",
                "limits": {},
            },
            "modules": [{"name": "index.js"}],
            "events": {"consumers": [{"type": "book.added", "filter": {}}]},
            "cli": {"commands": [{"name": "greet", "args": [{"name": "who"}]}]},
            "oidc": {
                "clients": [
                    {
                        "client_id": "echo",
                        "callback_path": "/cb",
                        "origin_config_key": "origin",
                    }
                ]
            },
        }
    )
    parsed = tomllib.loads(rendered)
    assert parsed["runtime"] == "native"
    assert "args" not in parsed
    workerd = parsed["workerd"]
    assert "compatibility_flags" not in workerd
    assert "limits" not in workerd
    assert workerd["modules_dir"] == "modules"
    assert workerd["entrypoint"] == "default"
    assert parsed["modules"] == [{"name": "index.js", "type": "js"}]
    consumer = parsed["events"]["consumers"][0]
    assert consumer["schema_versions"] == [1]
    assert consumer["resource_class"] == "network"
    assert "filter" not in consumer
    assert parsed["capabilities"]["network"] == {"mode": "deny"}
    assert parsed["cli"]["commands"][0]["args"][0]["kind"] == "string"
    client = parsed["oidc"]["clients"][0]
    assert client["public_client"] is True
    assert client["issue_refresh_token"] is True
    assert rendered.endswith("\n") and not rendered.endswith("\n\n")


def test_format_manifest_keeps_the_values_a_manifest_gives():
    import tomllib

    def limits_of(limits: dict[str, int]) -> dict[str, Any]:
        rendered = format_manifest(
            {
                "api_version": 3,
                "id": "echo",
                "runtime": "workerd",
                "args": ["--serve", "--quiet"],
                "workerd": {
                    "compatibility_date": "2026-08-01",
                    "compatibility_flags": ["nodejs_compat"],
                    "main_module": "index.js",
                    "limits": limits,
                },
                "events": {
                    "consumers": [
                        {
                            "type": "book.added",
                            "resource_class": "cpu",
                            "filter": {"source": "libro"},
                        }
                    ]
                },
                "oidc": {
                    "clients": [
                        {
                            "client_id": "echo",
                            "callback_path": "/cb",
                            "public_client": False,
                            "issue_refresh_token": False,
                            "origin_config_key": "origin",
                        }
                    ]
                },
            }
        )
        return tomllib.loads(rendered)

    parsed = limits_of({"subrequests": 5})
    assert parsed["args"] == ["--serve", "--quiet"]
    assert parsed["workerd"]["compatibility_flags"] == ["nodejs_compat"]
    assert parsed["workerd"]["limits"] == {"subrequests": 5}
    assert limits_of({"cpu_ms": 50})["workerd"]["limits"] == {"cpu_ms": 50}
    consumer = parsed["events"]["consumers"][0]
    assert consumer["resource_class"] == "cpu"
    assert consumer["filter"] == {"source": "libro"}
    client = parsed["oidc"]["clients"][0]
    assert client["public_client"] is False
    assert client["issue_refresh_token"] is False


def _echo_python_copy(tmp_path: Path, modules_dir: str | None) -> Path:
    """Copy the Python workerd example with its modules under ``modules_dir``.

    ``None`` leaves ``modules_dir`` out of the manifest, so the default applies.
    """
    plugin = tmp_path / "py"
    shutil.copytree(ECHO_PY, plugin)
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    if modules_dir is None:
        toml = toml.replace('modules_dir = "modules"\n', "")
    else:
        toml = toml.replace('modules_dir = "modules"', f"modules_dir = {modules_dir!r}")
        (plugin / "modules").rename(plugin / modules_dir)
    (plugin / "plugin.toml").write_text(toml, encoding="utf-8")
    return plugin


@pytest.mark.parametrize("modules_dir", [None, "src"])
def test_package_python_workerd_vendors_the_sdk_under_its_modules(
    tmp_path: Path, modules_dir: str | None
) -> None:
    from bookclerk_plugin_sdk.tools import _sdk_workerd_embed_src

    plugin = _echo_python_copy(tmp_path, modules_dir)
    archive = package_plugin(plugin, tmp_path / "dist")
    assert archive.name == "bookclerk-plugin-echo_workerd_python-1.0.0-workerd.tar.gz"
    members = _members(archive)
    folder = modules_dir or "modules"
    assert members["./plugin.toml"][1] == (plugin / "plugin.toml").read_bytes()
    assert members[f"./{folder}/plugin.py"][1] == (plugin / folder / "plugin.py").read_bytes()
    assert members[f"./{folder}/bookclerk_plugin_sdk/__init__.py"][1] == (
        b'"""Bookclerk plugin SDK (vendored for workerd)."""\n'
    )
    assert (
        members[f"./{folder}/bookclerk_plugin_sdk/workerd.py"][1]
        == _sdk_workerd_embed_src().read_bytes()
    )


def test_package_python_workerd_replaces_an_authored_sdk_folder(tmp_path: Path) -> None:
    plugin = _echo_python_copy(tmp_path, None)
    (plugin / "modules" / "bookclerk_plugin_sdk").mkdir()
    (plugin / "modules" / "bookclerk_plugin_sdk" / "__init__.py").write_text(
        "stale\n", encoding="utf-8"
    )
    members = _members(package_plugin(plugin, tmp_path / "dist"))
    assert members["./modules/bookclerk_plugin_sdk/__init__.py"][1].startswith(b'"""Bookclerk')


def test_package_treats_a_manifest_without_runtime_as_native(tmp_path: Path) -> None:
    plugin = _native_plugin(tmp_path)
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    (plugin / "plugin.toml").write_text(toml.replace('runtime = "native"\n', ""), encoding="utf-8")
    members = _members(package_plugin(plugin, tmp_path / "dist"))
    assert set(members) == {"./plugin.toml", "./echo-tools"}


@pytest.mark.parametrize("logo", ["logo.png", "assets/img/logo.png"])
def test_package_copies_an_embedded_logo_at_any_depth(tmp_path: Path, logo: str) -> None:
    plugin = _native_plugin(tmp_path)
    toml = (plugin / "plugin.toml").read_text(encoding="utf-8")
    toml = toml.replace('runtime = "native"', f'logo = {logo!r}\nruntime = "native"')
    (plugin / "plugin.toml").write_text(toml, encoding="utf-8")
    (plugin / logo).parent.mkdir(parents=True, exist_ok=True)
    (plugin / logo).write_bytes(b"\x89PNG")
    members = _members(package_plugin(plugin, tmp_path / "dist"))
    assert members[f"./{logo}"][1] == b"\x89PNG"


def test_package_writes_the_archive_readable_but_not_executable(tmp_path: Path) -> None:
    import os

    umask = os.umask(0)
    os.umask(umask)
    archive = package_plugin(_native_plugin(tmp_path), tmp_path / "dist")
    assert archive.stat().st_mode & 0o777 == 0o644 & ~umask
