//! `AdapterDatabaseSession.execute` round-trip latency for the sqlite guest on
//! both transports, so the cost of the workerd front door stays measured.
//!
//! Ignored by default (needs the built `bookclerk-plugin-database-sqlite`,
//! `bookclerk-workerd`, and pinned `workerd` beside the test binary). Run with:
//!
//! ```text
//! cargo test -p bookclerk-plugin-host --test transport_latency -- --ignored --nocapture
//! ```
//!
//! Each `SELECT 1` goes through the production `RpcDatabaseProxy`, i.e.
//! [`PluginSession::db_execute_request`] with host proof stamping, over a
//! shared Cap'n Proto session; only the launcher tree differs between runs.
//!
//! [`PluginSession::db_execute_request`]: bookclerk_plugin_host::PluginSession::db_execute_request

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, DiscoveredPlugin, ExternalDatabase, PluginGrantStore, SessionServices,
    SpawnTransport,
};
use sea_orm::ConnectionTrait;
use tempfile::TempDir;

/// Number of `SELECT 1` round trips per transport.
const ROUND_TRIPS: usize = 500;

/// Staged sqlite guest: install dir + files dir kept alive for the run.
struct StagedGuest {
    files: TempDir,
    _install: TempDir,
    plugin: DiscoveredPlugin,
}

fn sqlite_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "bookclerk-plugin-database-sqlite.exe"
    } else {
        "bookclerk-plugin-database-sqlite"
    };
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(name));
            candidates.push(dir.join("..").join(name));
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
    candidates.push(target.join(profile).join(name));
    candidates.into_iter().find(|path| path.is_file())
}

fn stage_sqlite_guest() -> Option<StagedGuest> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("crates/bookclerk-plugins/platform/database-sqlite");
    let binary = sqlite_binary()?;
    let install = TempDir::new().ok()?;
    std::fs::copy(src.join("plugin.toml"), install.path().join("plugin.toml")).ok()?;
    let dest_bin = install
        .path()
        .join(binary.file_name().expect("binary file name"));
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
    Some(StagedGuest {
        files: TempDir::new().ok()?,
        plugin: DiscoveredPlugin {
            manifest,
            root: install.path().to_path_buf(),
            command: dest_bin,
        },
        _install: install,
    })
}

fn guest_config(files: &Path) -> Config {
    let mut config = Config {
        paths: Some(Paths::from_files_dir(files.to_path_buf())),
        ..Config::default()
    };
    // The comparison is transport cost, not jail cost: both runs skip the jail.
    config.plugins.isolation = Isolation::Off;
    config.database.plugin = "sqlite".to_string();
    config
}

fn approve(config: &Config, staged: &StagedGuest) {
    let files = &config.paths().files_dir;
    let mut grants = PluginGrantStore::load(files).expect("load grants");
    grants.upsert(consent_request(&staged.plugin.manifest));
    grants.save(files).expect("save grants");
}

/// Sorted-sample percentile (nearest rank).
fn percentile(sorted: &[Duration], pct: f64) -> Duration {
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

async fn measure(
    config: &Config,
    staged: &StagedGuest,
    transport: SpawnTransport,
) -> Vec<Duration> {
    let services = SessionServices {
        event_outbox: None,
        spawn_transport: transport,
    };
    let spawn_started = Instant::now();
    let ext = ExternalDatabase::spawn_with(&staged.plugin, config, services)
        .await
        .unwrap_or_else(|err| panic!("spawn sqlite over {transport:?}: {err}"));
    let (db, _caps) = ext
        .connect_without_migrate(config)
        .await
        .unwrap_or_else(|err| panic!("connect sqlite over {transport:?}: {err}"));
    eprintln!(
        "{transport:?}: spawn + open + capabilities in {:?}",
        spawn_started.elapsed()
    );

    let statement =
        || bookclerk_db_exec::canonical_statement("SELECT 1", Vec::<sea_orm::Value>::new());
    // Warm-up so first-call page faults / JIT do not skew the percentiles.
    for _ in 0..20 {
        db.query_one_raw(statement())
            .await
            .expect("warm-up SELECT 1");
    }
    let mut samples = Vec::with_capacity(ROUND_TRIPS);
    for _ in 0..ROUND_TRIPS {
        let started = Instant::now();
        db.query_one_raw(statement()).await.expect("SELECT 1");
        samples.push(started.elapsed());
    }
    samples.sort();
    samples
}

fn report(label: &str, samples: &[Duration]) {
    let total: Duration = samples.iter().sum();
    eprintln!(
        "{label}: N={} p50={:?} p95={:?} p99={:?} min={:?} max={:?} mean={:?}",
        samples.len(),
        percentile(samples, 50.0),
        percentile(samples, 95.0),
        percentile(samples, 99.0),
        samples[0],
        samples[samples.len() - 1],
        total / samples.len() as u32,
    );
}

/// Prints p50/p95 for direct native and native-behind-workerd on one host.
#[tokio::test]
#[ignore = "latency benchmark; needs bookclerk-plugin-database-sqlite, bookclerk-workerd and workerd built"]
async fn sqlite_execute_round_trip_direct_vs_front_door() {
    let staged = stage_sqlite_guest().expect(
        "build bookclerk-plugin-database-sqlite (cargo build -p bookclerk-plugin-database-sqlite)",
    );
    let config = guest_config(staged.files.path());
    approve(&config, &staged);

    let direct = measure(&config, &staged, SpawnTransport::DirectNativeDiagnostic).await;
    let fronted = measure(&config, &staged, SpawnTransport::WorkerdFrontDoor).await;

    report("direct native (diagnostic)", &direct);
    report("native-behind-workerd (front door)", &fronted);
    assert_eq!(direct.len(), ROUND_TRIPS);
    assert_eq!(fronted.len(), ROUND_TRIPS);
}
