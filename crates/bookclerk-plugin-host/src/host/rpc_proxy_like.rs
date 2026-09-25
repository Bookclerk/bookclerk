//! Production `RpcDatabaseProxy` → `PluginSession` → Cap'n → adapter LIKE vector.
//!
//! Complements `tests/canonical_adapter_boundary.rs`, which copies the SeaORM
//! frontend onto a recording proxy. These tests spawn a first-party guest and
//! send leftover SQL over the real session.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::{Path, PathBuf};

use bookclerk_config::{Config, Isolation, Paths};
use sea_orm::{ConnectionTrait, Statement, Value};
use tempfile::TempDir;

use super::database::{capture_outbound_adapter_sql, ExternalDatabase};
use crate::consent::{consent_request, PluginGrantStore};
use crate::discover::DiscoveredPlugin;
use crate::Entrypoint;

const LIKE_SQL: &str = "SELECT 1 AS n WHERE 'rowcap-keep' LIKE ?";

struct StagedGuest {
    /// Files-dir used for grants and plugin state (`HOME` / `TMPDIR`).
    files: TempDir,
    /// Install directory containing `plugin.toml` and the guest binary.
    _install: TempDir,
    plugin: DiscoveredPlugin,
}

fn plugin_crate_dir(id: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    match id {
        "sqlite" => root.join("crates/bookclerk-plugins/platform/database-sqlite"),
        "postgres" => root.join("crates/bookclerk-plugins/optional/database-postgres"),
        other => root.join(format!(
            "crates/bookclerk-plugins/optional/database-{other}"
        )),
    }
}

fn plugin_bin_name(id: &str) -> String {
    let base = format!("bookclerk-plugin-database-{id}");
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base
    }
}

fn find_plugin_binary(id: &str) -> Option<PathBuf> {
    let name = plugin_bin_name(id);
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(&name));
            candidates.push(dir.join("..").join(&name));
        }
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    candidates.push(target.join(profile).join(&name));
    candidates.into_iter().find(|path| path.is_file())
}

fn copy_plugin_toml_and_assets(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    std::fs::copy(src.join("plugin.toml"), dest.join("plugin.toml"))?;
    let assets = src.join("assets");
    if assets.is_dir() {
        let dest_assets = dest.join("assets");
        std::fs::create_dir_all(&dest_assets)?;
        if let Ok(entries) = std::fs::read_dir(&assets) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(name) = path.file_name() {
                        std::fs::copy(&path, dest_assets.join(name))?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn stage_files_dir() -> Option<TempDir> {
    // Cargo TMPDIR is workspace `.tmp`. Nested plugin-state paths overflow
    // Linux sockaddr_un (~108 bytes) on GitHub Actions; keep staging under a
    // similarly long prefix so native-behind-workerd cannot regress to
    // pathname sockets.
    let base = std::env::temp_dir().join("gha-sunlen").join("x".repeat(48));
    std::fs::create_dir_all(&base).ok()?;
    tempfile::Builder::new().prefix("td").tempdir_in(base).ok()
}

fn stage_first_party_guest(id: &str) -> Option<StagedGuest> {
    let src = plugin_crate_dir(id);
    if !src.join("plugin.toml").is_file() {
        return None;
    }
    let binary = find_plugin_binary(id)?;
    let install = TempDir::new().ok()?;
    copy_plugin_toml_and_assets(&src, install.path()).ok()?;
    let dest_bin = install.path().join(plugin_bin_name(id));
    std::fs::copy(&binary, &dest_bin).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest_bin).ok()?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest_bin, perms).ok()?;
    }
    let toml = std::fs::read_to_string(install.path().join("plugin.toml")).ok()?;
    let manifest = bookclerk_plugin_manifest::parse(&toml).ok()?;
    let files = stage_files_dir()?;
    if id == "sqlite" {
        let version = manifest.version.as_deref().unwrap_or("0.0.0");
        bookclerk_plugin_catalog::stamp_platform_receipt(
            install.path(),
            files.path(),
            "bookclerk-plugin-database-sqlite",
            &manifest,
            version,
        )
        .ok()?;
    }
    let mut plugin = DiscoveredPlugin::try_new(
        manifest,
        install.path().to_path_buf(),
        dest_bin,
        Some(files.path()),
    )
    .ok()?;
    if matches!(id, "postgres" | "d1") {
        let package = format!("bookclerk-plugin-database-{id}");
        let key = bookclerk_plugin_catalog::PluginKey::platform(&package, id).ok()?;
        plugin.identity.plugin_key = key.clone();
        plugin.identity.artifact.plugin_key = key;
        plugin.identity.provenance = bookclerk_plugin_catalog::PluginProvenance::VerifiedInstalled;
    }
    Some(StagedGuest {
        files,
        plugin,
        _install: install,
    })
}

fn guest_config(staged: &StagedGuest, plugin_id: &str, postgres_url: Option<String>) -> Config {
    let mut config = Config {
        paths: Some(Paths::from_files_dir(staged.files.path().to_path_buf())),
        ..Config::default()
    };
    // The leak under test is leftover SQL on the Cap'n execute path, not jail
    // policy. `Isolation::Off` keeps this vector independent of `bookclerk-jail`.
    config.plugins.isolation = Isolation::Off;
    config.database.plugin = plugin_id.to_string();
    if let Some(url) = postgres_url {
        config.database.postgres.url = Some(url);
    }
    config
}

fn approve_guest(config: &Config, plugin: &DiscoveredPlugin) {
    let files = &config.paths().files_dir;
    let mut grants = PluginGrantStore::load(files).expect("load grants");
    let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
    crate::consent::overlay_host_implied_network(
        &mut grant,
        plugin,
        config,
        std::slice::from_ref(plugin),
    );
    grants.upsert(grant);
    grants.save(files).expect("save grants");
}

fn postgres_plugin_tests_enabled() -> bool {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if url.is_some() {
        return true;
    }
    assert!(
        std::env::var("BOOKCLERK_REQUIRE_POSTGRES_TESTS")
            .ok()
            .as_deref()
            != Some("1"),
        "BOOKCLERK_TEST_POSTGRES_URL is required when BOOKCLERK_REQUIRE_POSTGRES_TESTS=1"
    );
    false
}

fn postgres_url_with_db(url: &str, db_name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let trimmed = base.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(slash) => {
            let head = &trimmed[..slash];
            match query {
                Some(q) => format!("{head}/{db_name}?{q}"),
                None => format!("{head}/{db_name}"),
            }
        }
        None => match query {
            Some(q) => format!("{trimmed}/{db_name}?{q}"),
            None => format!("{trimmed}/{db_name}"),
        },
    }
}

async fn create_disposable_postgres_url() -> String {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").expect("BOOKCLERK_TEST_POSTGRES_URL");
    let db_name = format!("rpc_like_{}", uuid::Uuid::new_v4().as_simple());
    let admin = sea_orm::Database::connect(url.as_str())
        .await
        .expect("connect BOOKCLERK_TEST_POSTGRES_URL");
    let backend = sea_orm::ConnectionTrait::get_database_backend(&admin);
    sea_orm::ConnectionTrait::execute_raw(
        &admin,
        Statement::from_string(backend, format!("CREATE DATABASE {db_name}")),
    )
    .await
    .expect("create disposable postgres database");
    postgres_url_with_db(&url, &db_name)
}

fn assert_canonical_like_boundary(sqls: &[String]) {
    let like = sqls
        .iter()
        .filter(|sql| sql.contains("LIKE") || sql.contains("GLOB"))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        !like.is_empty(),
        "production proxy sent no LIKE/GLOB SQL over Cap'n: {sqls:?}"
    );
    for sql in &like {
        assert!(sql.contains("LIKE"), "boundary must keep LIKE: {sql}");
        assert!(!sql.contains("GLOB"), "GLOB must not cross Cap'n: {sql}");
        assert!(sql.contains('?'), "canonical placeholders stay ?: {sql}");
        assert!(
            !sql.contains("$1"),
            "physical $n must not cross Cap'n: {sql}"
        );
    }
}

async fn assert_like_through_production_proxy(config: &Config, plugin: &DiscoveredPlugin) {
    approve_guest(config, plugin);
    assert!(plugin.manifest.has_entrypoint(Entrypoint::DatabaseAdapter));
    let ext = ExternalDatabase::spawn(plugin, config)
        .await
        .unwrap_or_else(|err| panic!("spawn {}: {err}", plugin.manifest.id));
    // Skip host schema apply: this vector only needs leftover LIKE across Cap'n.
    // Full `connect()` re-inserts catalog companions (`UNIQUE` on
    // `bookclerk_sql_catalog`).
    let (db, _caps) = ext
        .connect_without_migrate(config)
        .await
        .unwrap_or_else(|err| {
            let err = err.to_string();
            if err.contains("maxFunctionArgs 0") {
                panic!(
                    "connect {}: {err} (rebuild bookclerk-plugin-database-{} — \
                     cargo test -p bookclerk-plugin-host does not rebuild guest bins)",
                    plugin.manifest.id, plugin.manifest.id
                );
            }
            panic!("connect {}: {err}", plugin.manifest.id)
        });

    let (query, sqls) = capture_outbound_adapter_sql(|| async {
        ConnectionTrait::query_all_raw(
            &db,
            bookclerk_db_exec::canonical_statement(LIKE_SQL, [Value::from("rowcap-%")]),
        )
        .await
    })
    .await;
    query.unwrap_or_else(|err| {
        panic!(
            "LIKE query through RpcDatabaseProxy/PluginSession must succeed (GLOB would fail on Postgres): {err}"
        )
    });
    eprintln!(
        "production RpcDatabaseProxy/Cap'n leftover SQL ({}): {sqls:?}",
        plugin.manifest.id
    );
    assert_canonical_like_boundary(&sqls);
}

/// Skips when a first-party guest binary is missing, unless
/// `BOOKCLERK_REQUIRE_TEST_GUESTS=1` (CI) demands it be built.
fn missing_guest(package: &str) {
    let hint = format!("build {package} (cargo build -p {package})");
    if std::env::var("BOOKCLERK_REQUIRE_TEST_GUESTS").is_ok_and(|v| v == "1") {
        panic!("BOOKCLERK_REQUIRE_TEST_GUESTS=1 but guest binary is missing: {hint}");
    }
    eprintln!("skipping: {hint}");
}

#[tokio::test]
async fn production_rpc_proxy_keeps_like_through_sqlite_guest() {
    let Some(staged) = stage_first_party_guest("sqlite") else {
        missing_guest("bookclerk-plugin-database-sqlite");
        return;
    };
    let config = guest_config(&staged, "sqlite", None);
    assert_like_through_production_proxy(&config, &staged.plugin).await;
}

/// Describe-only: no live Postgres. Catches workerd/native spawn regressions
/// that the ignored LIKE vector only runs in the postgres CI job.
#[tokio::test]
async fn postgres_guest_describes_through_workerd() {
    let Some(staged) = stage_first_party_guest("postgres") else {
        missing_guest("bookclerk-plugin-database-postgres");
        return;
    };
    eprintln!(
        "postgres PluginKey={} command={}",
        staged.plugin.plugin_key().canonical(),
        staged.plugin.command.display()
    );
    let config = guest_config(&staged, "postgres", None);
    approve_guest(&config, &staged.plugin);
    let _ext = ExternalDatabase::spawn(&staged.plugin, &config)
        .await
        .unwrap_or_else(|err| panic!("spawn postgres describe: {err}"));
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and bookclerk-plugin-database-postgres"]
async fn production_rpc_proxy_keeps_like_through_postgres_guest() {
    if !postgres_plugin_tests_enabled() {
        return;
    }
    let staged = stage_first_party_guest("postgres").unwrap_or_else(|| {
        panic!("postgres plugin binary missing; cargo build -p bookclerk-plugin-database-postgres")
    });
    let url = create_disposable_postgres_url().await;
    let config = guest_config(&staged, "postgres", Some(url));
    assert_like_through_production_proxy(&config, &staged.plugin).await;
}
