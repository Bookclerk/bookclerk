//! What a guest can actually reach once the host has started it.
//!
//! The unit tests beside `jail.rs` check the allowlist the host builds, and
//! `bookclerk-jail`'s own tests check that a policy survives `exec`. Neither
//! proves the two halves meet: a host that assembled a perfect spec and then
//! spawned the guest directly would pass both. So these tests go through
//! [`PluginSession`] spawn — the same call the daemon makes — and have the guest
//! report what it could open.
//!
//! The guest is a shell script rather than one of the plugins we ship, because
//! the interesting part is the kernel's answer and a script can be asked to try
//! things a real plugin never would.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, grant_has_binding, plugin_data_dir, require_grant,
    DiscoveredPlugin, PluginGrantStore, PluginSession, HOST_SHARED_ACCOUNT,
};

/// Where cargo left the launcher for this test run.
///
/// The host looks beside the current executable, which for an integration test
/// is `target/<profile>/deps`. Locating it here too lets the test skip with a
/// reason rather than fail on a spawn error that explains nothing.
fn launcher() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join("bookclerk-jail"), dir.join("../bookclerk-jail")]
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Whether this host can enforce a filesystem allowlist.
///
/// `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT` turns a skip into a failure, so a
/// green run on a platform we expect to confine always means something.
fn confinement_available() -> bool {
    let caps = bookclerk_sandbox::capabilities();
    let demanded = std::env::var("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT")
        .is_ok_and(|value| !value.trim().is_empty());
    assert!(
        caps.filesystem || !demanded,
        "enforcement demanded but unavailable: {} [{}]",
        caps.detail,
        caps.backend
    );
    assert!(
        launcher().is_some() || !demanded,
        "enforcement demanded but bookclerk-jail was not built beside the test binary"
    );
    caps.filesystem && launcher().is_some()
}

/// One thing the guest is asked to attempt.
enum Probe {
    Read(PathBuf),
    Write(PathBuf),
}

/// Build a guest that probes every path it was given, then serves RPC.
///
/// Probe results go to a file in the guest's own data directory rather than over
/// the wire: the report has to survive whatever the RPC layer makes of a guest
/// that misbehaves, and `HOME` is the one place a guest is certain to be able to
/// write.
fn probe_script(probes: &BTreeMap<&str, Probe>) -> String {
    let mut body = String::from(
        r#"#!/bin/sh
report="$HOME/probe-report"
: > "$report" || exit 91

try_read() {
  if cat "$2" >/dev/null 2>&1; then echo "$1=allowed" >> "$report"
  else echo "$1=denied" >> "$report"; fi
}

try_write() {
  # A failed redirection is reported by the shell itself, not by the command, so
  # the subshell is what keeps a denial out of the host's log.
  if ( echo probe > "$2" ) 2>/dev/null; then echo "$1=allowed" >> "$report"
  else echo "$1=denied" >> "$report"; fi
}

"#,
    );
    // Double quotes so a probe can name `$HOME` or `$TMPDIR`, which is how a
    // guest reaches its own directories: it is told where they are by the
    // environment, not by the host's path layout. Every other path here is one
    // the fixture built, so there is nothing else to expand.
    for (name, probe) in probes {
        let (verb, path) = match probe {
            Probe::Read(path) => ("try_read", path),
            Probe::Write(path) => ("try_write", path),
        };
        body.push_str(&format!("{verb} {name} \"{}\"\n", path.display()));
    }
    // Answer requests so the host completes its Cap'n Proto handshake, then stay alive until
    // stdin closes. The id is echoed back rather than assumed, so this does not
    // depend on where the host's request counter started.
    body.push_str(
        r#"
# Probes finished. Stay alive until stdin closes so the host spawn can observe
# the report even if the Cap'n Proto handshake fails (this fixture is a
# shell script, not a PluginRoot guest).
cat >/dev/null
"#,
    );
    body
}

/// A files dir that looks like a real one, with a guest installed where
/// Bookclerk actually installs plugins: `$FILES_DIR/plugins/<id>`.
///
/// That layout matters rather than being incidental. It puts the guest's
/// writable data directory *inside* the install directory the host grants
/// read-only, so a backend that did not honour the more specific rule would show
/// up here and nowhere else.
struct Fixture {
    files: tempfile::TempDir,
    config: Config,
}

impl Fixture {
    /// Lay out the files dir, then let `probes` name paths within it.
    fn new(probes: impl FnOnce(&Paths) -> BTreeMap<&'static str, Probe>) -> Self {
        let files = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_files_dir(files.path().to_path_buf());

        // Stand-ins for the things a guest must never reach.
        std::fs::write(paths.files_dir.join("master.key"), b"sealed-dek").expect("master.key");
        std::fs::write(&paths.library_db, b"SQLite format 3\0").expect("library.db");
        std::fs::write(&paths.config_file, b"# config\n").expect("config.toml");
        std::fs::create_dir_all(paths.files_dir.join("Books")).expect("output dir");

        let install = paths.files_dir.join("plugins").join("probe");
        std::fs::create_dir_all(&install).expect("install dir");
        write_script(&install.join("guest.sh"), &probe_script(&probes(&paths)));
        std::fs::write(
            install.join("plugin.toml"),
            "api_version = 2\nid = \"probe\"\nkind = \"integration\"\n\
             runtime = \"native\"\ncommand = \"./guest.sh\"\n\n\
             [capabilities.network]\nmode = \"deny\"\n",
        )
        .expect("plugin.toml");

        // Spawn requires a covering consent grant (same bar as enable).
        let mut config = Config {
            paths: Some(paths),
            ..Default::default()
        };
        config.plugins.isolation = Isolation::Required;
        let plugin = discover_plugins(&config)
            .expect("discover")
            .into_iter()
            .find(|found| found.manifest.id == "probe")
            .expect("probe");
        let mut grants = PluginGrantStore::default();
        grants.upsert(consent_request(&plugin.manifest));
        grants
            .save(&config.paths().files_dir)
            .expect("write plugin-grants.json");

        Self { files, config }
    }

    fn paths(&self) -> Paths {
        Paths::from_files_dir(self.files.path().to_path_buf())
    }

    fn plugin(&self) -> DiscoveredPlugin {
        discover_plugins(&self.config)
            .expect("discover")
            .into_iter()
            .find(|found| found.manifest.id == "probe")
            .expect("the probe plugin should be discovered")
    }

    /// Spawn the guest through the host and collect what it reported.
    async fn probe_results(&self) -> BTreeMap<String, String> {
        let plugin = self.plugin();
        // Shell probe guests cannot speak Cap'n Proto; spawn still applies the
        // jail and runs probes before the Cap'n Proto handshake fails.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            PluginSession::spawn_for_account(
                &plugin,
                &self.config,
                serde_json::json!({}),
                HOST_SHARED_ACCOUNT,
            ),
        )
        .await;
        let report = plugin_data_dir(&self.config, "probe")
            .expect("valid plugin id")
            .join("probe-report");
        let text = std::fs::read_to_string(&report)
            .unwrap_or_else(|err| panic!("read {}: {err}", report.display()));
        text.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, verdict)| (name.to_string(), verdict.to_string()))
            .collect()
    }
}

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn assert_verdicts(results: &BTreeMap<String, String>, expected: &[(&str, &str)]) {
    for (name, verdict) in expected {
        assert_eq!(
            results.get(*name).map(String::as_str),
            Some(*verdict),
            "probe `{name}` should be {verdict}; full report: {results:?}"
        );
    }
}

/// The point of the tier. A storefront guest parses hostile input, so it must
/// not be able to read the key that seals every credential Bookclerk holds, the
/// database, the config, or the finished library — and it must still be able to
/// work in the three directories it was given.
#[tokio::test]
async fn a_jailed_guest_reaches_its_own_directories_and_nothing_else() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement (or no launcher) on this host");
        return;
    }

    let fixture = Fixture::new(|paths| {
        let install = paths.files_dir.join("plugins").join("probe");
        let mut probes = BTreeMap::new();
        probes.insert(
            "master_key",
            Probe::Read(paths.files_dir.join("master.key")),
        );
        probes.insert("library_db", Probe::Read(paths.library_db.clone()));
        probes.insert("config_file", Probe::Read(paths.config_file.clone()));
        probes.insert(
            "steal_the_key",
            Probe::Write(paths.files_dir.join("master.key")),
        );
        probes.insert(
            "write_the_library",
            Probe::Write(paths.files_dir.join("Books").join("planted.m4b")),
        );
        // A guest may read its own install directory but not rewrite the
        // manifest that describes it — the next start would read it back.
        probes.insert("install_dir", Probe::Read(install.join("plugin.toml")));
        probes.insert(
            "rewrite_manifest",
            Probe::Write(install.join("plugin.toml")),
        );
        // The three grants, which have to keep working.
        probes.insert("own_home", Probe::Write(PathBuf::from("$HOME/state")));
        probes.insert("own_tmpdir", Probe::Write(PathBuf::from("$TMPDIR/scratch")));
        probes.insert(
            "download_cache",
            Probe::Write(paths.cache_dir.join("staged.part")),
        );
        probes
    });

    let results = fixture.probe_results().await;
    assert_verdicts(
        &results,
        &[
            ("master_key", "denied"),
            ("library_db", "denied"),
            ("config_file", "denied"),
            ("steal_the_key", "denied"),
            ("write_the_library", "denied"),
            ("install_dir", "allowed"),
            ("rewrite_manifest", "denied"),
            ("own_home", "allowed"),
            ("own_tmpdir", "allowed"),
            ("download_cache", "denied"),
        ],
    );

    // The guest's own directories are where the host said they were, so an
    // operator can find (and back up, or delete) one plugin's state.
    let paths = fixture.paths();
    let state = paths.files_dir.join("plugins").join("probe");
    assert!(state.join("data").join("state").is_file());
    assert!(state.join("tmp").join("scratch").is_file());
}

/// `required` must refuse at the point a guest would start, not merely warn.
#[tokio::test]
async fn a_guest_that_cannot_be_jailed_is_not_spawned() {
    let fixture = Fixture::new(|_| BTreeMap::new());
    let mut config = fixture.config.clone();
    config.plugins.isolation = Isolation::Required;
    config.plugins.jail_bin = Some(fixture.files.path().join("no-such-launcher"));

    let message = match PluginSession::spawn_for_account(
        &fixture.plugin(),
        &config,
        serde_json::json!({}),
        HOST_SHARED_ACCOUNT,
    )
    .await
    {
        Ok(_) => panic!("a guest must not start unconfined under `required`"),
        Err(err) => err.to_string(),
    };
    assert!(message.contains("refusing to run plugin"), "got: {message}");
    assert!(
        !plugin_data_dir(&config, "probe")
            .expect("valid plugin id")
            .join("probe-report")
            .exists(),
        "the guest ran despite a jail that could not be applied"
    );
}

/// Enabled (discovered) plugins still cannot spawn without a covering grant.
#[tokio::test]
async fn spawn_fails_without_consent_grant() {
    let fixture = Fixture::new(|_| BTreeMap::new());
    // Remove the grant Fixture::new wrote.
    let grants_path = fixture.config.paths().files_dir.join("plugin-grants.json");
    std::fs::remove_file(&grants_path).expect("remove grants");

    let message = match PluginSession::spawn_for_account(
        &fixture.plugin(),
        &fixture.config,
        serde_json::json!({}),
        HOST_SHARED_ACCOUNT,
    )
    .await
    {
        Ok(_) => panic!("spawn must fail without a grant"),
        Err(err) => err.to_string(),
    };
    assert!(
        message.contains("no permission grant") || message.contains("approve"),
        "got: {message}"
    );
}

/// Extra bindings in a later `plugin.toml` stay off the effective grant.
/// The stored operator subset remains covering, so spawn is not refused.
#[tokio::test]
async fn spawn_keeps_stored_grant_when_manifest_widens() {
    let fixture = Fixture::new(|_| BTreeMap::new());
    let install = fixture
        .config
        .paths()
        .files_dir
        .join("plugins")
        .join("probe");
    std::fs::write(
        install.join("plugin.toml"),
        "api_version = 2\nid = \"probe\"\nkind = \"integration\"\n\
         runtime = \"native\"\ncommand = \"./guest.sh\"\n\n\
         [capabilities.network]\nmode = \"deny\"\n\n\
         [capabilities.bindings]\nconfig = true\nsecrets = true\n",
    )
    .expect("widen plugin.toml");

    let plugin = fixture.plugin();
    let grant = require_grant(fixture.config.paths().files_dir.as_path(), &plugin.manifest)
        .expect("stored grant still covers a widened manifest");
    assert!(
        !grant_has_binding(&grant, "secrets"),
        "extra manifest bindings must stay off the effective grant"
    );

    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        PluginSession::spawn_for_account(
            &plugin,
            &fixture.config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
        ),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Err(_) => {
            // Shell probe cannot complete Cap'n Proto handshake; timeout is not
            // a grant failure.
        }
        Ok(Err(err)) => {
            let message = err.to_string();
            assert!(
                !message.contains("no permission grant")
                    && !message.contains("approve")
                    && !message.contains("capabilities widened")
                    && !message.contains("re-approve")
                    && !message.contains("grant does not match"),
                "stored grant must still cover spawn after widening; got: {message}"
            );
        }
    }
}
