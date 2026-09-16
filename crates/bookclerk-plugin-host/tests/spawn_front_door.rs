//! The workerd front door is the only product transport.
//!
//! A `runtime = "native"` manifest is launched as `bookclerk-workerd` with the
//! native executable exported as `BOOKCLERK_NATIVE_BACKEND`; only the
//! diagnostic transport yields the raw command. When `bookclerk-workerd` (or
//! the pinned `workerd`) cannot be found, spawn refuses in every isolation
//! mode instead of falling back to direct native.

use std::path::{Path, PathBuf};

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, DiscoveredPlugin, ExecutorIdentity, GuestRuntimeKind,
    PluginGrantStore, PluginSession, SessionServices, SpawnPlan, SpawnTransport, WorkerdFrontDoor,
    HOST_SHARED_ACCOUNT, WORKERD_LAUNCHER_ENV,
};

/// Files dir with a native `probe` plugin installed and granted.
struct Fixture {
    _files: tempfile::TempDir,
    config: Config,
}

impl Fixture {
    fn new() -> Self {
        let files = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_files_dir(files.path().to_path_buf());
        let install = paths.files_dir.join("plugins").join("probe");
        std::fs::create_dir_all(&install).expect("install dir");
        write_executable(&install.join("guest.sh"), "#!/bin/sh\ncat >/dev/null\n");
        std::fs::write(
            install.join("plugin.toml"),
            "api_version = 3\nid = \"probe\"\nentrypoints = [\"remoteLibrary\"]\n\
             runtime = \"native\"\ncommand = \"./guest.sh\"\n\n\
             [capabilities.network]\nmode = \"deny\"\n",
        )
        .expect("plugin.toml");
        let config = Config {
            paths: Some(paths),
            ..Default::default()
        };
        let plugin = find_probe(&config);
        let mut grants = PluginGrantStore::default();
        grants.upsert(consent_request(&plugin.manifest));
        grants
            .save(&config.paths().files_dir)
            .expect("write plugin-grants.json");
        Self {
            _files: files,
            config,
        }
    }

    fn plugin(&self) -> DiscoveredPlugin {
        find_probe(&self.config)
    }
}

fn find_probe(config: &Config) -> DiscoveredPlugin {
    discover_plugins(config)
        .expect("discover")
        .into_iter()
        .find(|found| found.manifest.id == "probe")
        .expect("probe plugin")
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Fake `bookclerk-workerd` + `workerd` files so planning resolves without a build.
fn fake_front_door(dir: &Path) -> WorkerdFrontDoor {
    let suffix = std::env::consts::EXE_SUFFIX;
    std::fs::write(dir.join(format!("bookclerk-workerd{suffix}")), b"").expect("launcher");
    std::fs::write(dir.join(format!("workerd{suffix}")), b"").expect("workerd");
    WorkerdFrontDoor::locate_in(dir).expect("fake front door")
}

/// Pure planning: no process is spawned.
#[test]
fn default_transport_wraps_a_native_manifest_in_workerd() {
    let fixture = Fixture::new();
    let helpers = tempfile::tempdir().expect("tempdir");
    let front_door = fake_front_door(helpers.path());
    let plugin = fixture.plugin();

    let plan =
        SpawnPlan::resolve_with(
            &plugin,
            SpawnTransport::default(),
            || Ok(front_door.clone()),
        )
        .expect("front-door plan");
    assert_eq!(plan.launcher, front_door.launcher);
    assert_eq!(plan.native_backend, Some(plugin.command.clone()));
    assert_eq!(plan.workerd_bin, Some(front_door.workerd_bin.clone()));
    assert_eq!(plan.runtime, GuestRuntimeKind::NativeBehindWorkerd);
    assert!(plan.args.is_empty());

    let identity = ExecutorIdentity::from_plugin(&plugin, HOST_SHARED_ACCOUNT);
    assert_eq!(identity.runtime_backend, "native-behind-workerd");
    assert!(identity.session_key().contains("native-behind-workerd"));

    let direct = SpawnPlan::resolve_with(&plugin, SpawnTransport::DirectNativeDiagnostic, || {
        panic!("direct native never resolves the front door")
    })
    .expect("direct plan");
    assert_eq!(direct.launcher, plugin.command);
    assert_eq!(direct.native_backend, None);
    assert_eq!(direct.runtime, GuestRuntimeKind::NativeDirect);
    assert_eq!(
        ExecutorIdentity::from_plugin_on(
            &plugin,
            HOST_SHARED_ACCOUNT,
            SpawnTransport::DirectNativeDiagnostic
        )
        .runtime_backend,
        "native-direct"
    );
}

/// `required` with no `bookclerk-workerd` refuses before any process starts.
///
/// `BOOKCLERK_PLUGIN_WORKERD` is process-global, so this is the only test in
/// this binary that resolves the real front door.
#[tokio::test]
async fn required_isolation_refuses_when_the_front_door_is_missing() {
    let fixture = Fixture::new();
    let mut config = fixture.config.clone();
    config.plugins.isolation = Isolation::Required;
    let missing = tempfile::tempdir().expect("tempdir");
    std::env::set_var(
        WORKERD_LAUNCHER_ENV,
        missing.path().join("no-such-bookclerk-workerd"),
    );

    let message = match PluginSession::spawn_with(
        &fixture.plugin(),
        &config,
        serde_json::json!({}),
        HOST_SHARED_ACCOUNT,
        &[],
        SessionServices::default(),
    )
    .await
    {
        Ok(_) => panic!("a native guest must not start without the workerd front door"),
        Err(err) => err.to_string(),
    };
    std::env::remove_var(WORKERD_LAUNCHER_ENV);
    assert!(
        message.contains("refusing to start plugin `probe`"),
        "got: {message}"
    );
    assert!(
        message.contains("bookclerk-workerd front door"),
        "got: {message}"
    );
    assert!(message.contains("diagnostic-only"), "got: {message}");
    // The plan is resolved before the jail is planned, so not even the guest's
    // state directories were created.
    let state: PathBuf = config
        .paths()
        .files_dir
        .join("plugins")
        .join("probe")
        .join("data");
    assert!(
        !state.exists(),
        "spawn progressed past plan resolution despite a missing front door"
    );
}
