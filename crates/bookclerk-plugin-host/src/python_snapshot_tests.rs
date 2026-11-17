use super::*;

fn write(dir: &Path, name: &str, body: &[u8]) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, body).expect("write");
}

#[test]
fn the_key_follows_every_file_of_the_install() {
    let install = tempfile::tempdir().expect("tempdir");
    write(install.path(), "plugin.toml", b"id = \"py\"");
    write(install.path(), "modules/plugin.py", b"print(1)");
    let first = install_key(install.path()).expect("key").expect("small");
    assert_eq!(first.len(), 32);
    assert_eq!(
        install_key(install.path()).expect("key").expect("small"),
        first,
        "stable"
    );

    write(install.path(), "modules/plugin.py", b"print(2)");
    let changed = install_key(install.path()).expect("key").expect("small");
    assert_ne!(changed, first, "a changed module is a new key");

    write(install.path(), "modules/extra.py", b"");
    assert_ne!(
        install_key(install.path()).expect("key").expect("small"),
        changed,
        "an added empty file is a new key"
    );
}

#[test]
fn a_moved_byte_between_files_is_a_new_key() {
    let a = tempfile::tempdir().expect("tempdir");
    write(a.path(), "a", b"xy");
    write(a.path(), "b", b"");
    let b = tempfile::tempdir().expect("tempdir");
    write(b.path(), "a", b"x");
    write(b.path(), "b", b"y");
    assert_ne!(
        install_key(a.path()).expect("key"),
        install_key(b.path()).expect("key")
    );
}

#[test]
fn a_first_start_saves_and_the_ready_snapshot_is_promoted() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let slot = SnapshotSlot::create(&root, "k1").expect("slot");
    assert_eq!(slot.read_path(), None, "nothing stored yet");
    assert!(slot.write_path().is_dir());
    let env = slot.env();
    assert_eq!(env.len(), 1);
    assert_eq!(env[0].0, launcher::SAVE_ENV);

    write(slot.write_path(), READY_FILE, b"snap");
    assert!(slot.promote());
    assert!(!slot.write_path().exists(), "staging removed");
    assert_eq!(
        std::fs::read(root.join("k1").join(STORED_FILE)).expect("stored"),
        b"snap"
    );

    let next = SnapshotSlot::create(&root, "k1").expect("slot");
    assert_eq!(next.read_path(), Some(root.join("k1").as_path()));
    let names: Vec<_> = next.env().into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, vec![launcher::SAVE_ENV, launcher::LOAD_ENV]);
}

#[test]
fn a_new_key_drops_the_old_snapshot() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let old = SnapshotSlot::create(&root, "old").expect("slot");
    write(old.write_path(), READY_FILE, b"old");
    assert!(old.promote());

    let new = SnapshotSlot::create(&root, "new").expect("slot");
    assert_eq!(new.read_path(), None);
    write(new.write_path(), READY_FILE, b"new");
    assert!(new.promote());
    assert!(!root.join("old").exists());
    assert!(root.join("new").join(STORED_FILE).exists());
}

#[test]
fn an_unfinished_save_is_not_promoted() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let slot = SnapshotSlot::create(&root, "k").expect("slot");
    write(slot.write_path(), launcher::SAVED_FILE, b"half");
    assert!(!slot.promote());
    assert!(!root.join("k").exists());
    assert!(!slot.write_path().exists(), "staging removed anyway");
}

#[cfg(unix)]
#[test]
fn a_planted_link_is_not_promoted() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let slot = SnapshotSlot::create(&root, "k").expect("slot");
    let outside = state.path().join("secret");
    std::fs::write(&outside, b"host file").expect("write");
    std::os::unix::fs::symlink(&outside, slot.write_path().join(READY_FILE)).expect("link");
    assert!(!slot.promote());
    assert!(!root.join("k").exists());
    assert_eq!(std::fs::read(&outside).expect("kept"), b"host file");
}

#[cfg(unix)]
#[test]
fn the_store_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let slot = SnapshotSlot::create(&root, "k").expect("slot");
    for dir in [root.as_path(), slot.write_path()] {
        let mode = std::fs::metadata(dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
    }
}

#[test]
fn left_behind_staging_directories_are_swept() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let old = SnapshotSlot::create(&root, "k").expect("slot");
    write(&root, "k/snapshot.bin", b"kept");
    sweep_stale_staging(&root, Duration::from_secs(3600));
    assert!(old.write_path().is_dir(), "a fresh staging directory stays");
    sweep_stale_staging(&root, Duration::ZERO);
    assert!(!old.write_path().exists(), "an old one is removed");
    assert!(root.join("k").join(STORED_FILE).exists(), "the store stays");
}

#[test]
fn an_install_past_the_limits_gets_no_key() {
    let install = tempfile::tempdir().expect("tempdir");
    write(install.path(), "a", b"12");
    write(install.path(), "b", b"3");
    assert!(install_key_within(install.path(), 2, 3)
        .expect("key")
        .is_some());
    assert_eq!(install_key_within(install.path(), 1, 3).expect("key"), None);
    assert_eq!(install_key_within(install.path(), 2, 2).expect("key"), None);
    assert!(install_key(&install.path().join("missing")).is_err());
}

fn plugin(root: &Path, toml: &str) -> DiscoveredPlugin {
    write(root, "plugin.toml", toml.as_bytes());
    write(root, "plugin.py", b"print(1)");
    write(root, "main.js", b"export default {}");
    let manifest = crate::PluginManifest::parse(toml).expect("manifest");
    DiscoveredPlugin::try_new(manifest, root.to_path_buf(), root.join("plugin.py"), None)
        .expect("plugin")
}

const PYTHON: &str = r#"
api_version = 3
id = "py"
version = "1.0.0"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
compatibility_flags = ["python_workers", "disable_python_external_sdk"]
main_module = "plugin.py"

[[modules]]
name = "plugin.py"
path = "plugin.py"
type = "python"

[capabilities.network]
mode = "deny"
"#;

const SCRIPT: &str = r#"
api_version = 3
id = "js"
version = "1.0.0"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
main_module = "main.js"

[capabilities.network]
mode = "deny"
"#;

#[test]
fn only_a_python_plugin_gets_a_slot() {
    let state = tempfile::tempdir().expect("tempdir");
    let js = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        SnapshotSlot::plan(state.path(), &plugin(js.path(), SCRIPT)),
        None
    );

    let py = tempfile::tempdir().expect("tempdir");
    let slot = SnapshotSlot::plan(state.path(), &plugin(py.path(), PYTHON)).expect("slot");
    assert!(slot
        .write_path()
        .starts_with(state.path().join(SNAPSHOT_DIR)));
    assert_eq!(slot.read_path(), None);
}

#[test]
fn a_slot_that_cannot_be_created_is_skipped() {
    let state = tempfile::tempdir().expect("tempdir");
    // A file where the snapshot directory belongs.
    write(state.path(), SNAPSHOT_DIR, b"");
    let py = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        SnapshotSlot::plan(state.path(), &plugin(py.path(), PYTHON)),
        None
    );
}

#[test]
fn the_grants_follow_whether_a_snapshot_is_stored() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let first = SnapshotSlot::create(&root, "k").expect("slot");
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    first.grant(&mut reads, &mut writes);
    assert!(reads.is_empty());
    assert_eq!(writes, vec![first.write_path().to_path_buf()]);

    write(first.write_path(), READY_FILE, b"snap");
    assert!(first.promote());
    let next = SnapshotSlot::create(&root, "k").expect("slot");
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    next.grant(&mut reads, &mut writes);
    assert_eq!(reads, vec![root.join("k")]);
    assert_eq!(writes, vec![next.write_path().to_path_buf()]);
}

#[test]
fn an_oversized_snapshot_is_refused() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let slot = SnapshotSlot::create(&root, "k").expect("slot");
    let file = std::fs::File::create(slot.write_path().join(READY_FILE)).expect("create");
    // Sparse, so the test writes no real data.
    file.set_len(MAX_SNAPSHOT_BYTES + 1).expect("grow");
    drop(file);
    assert!(!slot.promote());
    assert!(!root.join("k").exists());
    assert!(!slot.write_path().exists());
}

#[test]
fn a_promotion_drops_stray_files_and_keeps_other_staging() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let other = SnapshotSlot::create(&root, "k").expect("slot");
    let slot = SnapshotSlot::create(&root, "k").expect("slot");
    write(&root, "stray", b"x");
    write(slot.write_path(), READY_FILE, b"snap");
    assert!(slot.promote());
    assert!(!root.join("stray").exists());
    assert!(other.write_path().is_dir(), "another start's staging stays");
}

#[tokio::test]
async fn the_watcher_promotes_once_the_snapshot_is_ready() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let slot = SnapshotSlot::create(&root, "k").expect("slot");
    let staging = slot.write_path().to_path_buf();
    slot.promote_when_ready();
    tokio::time::sleep(Duration::from_millis(50)).await;
    write(&staging, READY_FILE, b"snap");
    let stored = root.join("k").join(STORED_FILE);
    for _ in 0..100 {
        if stored.exists() && !staging.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(&stored).expect("stored"), b"snap");
    assert!(!staging.exists());
}

#[tokio::test]
async fn the_watcher_stops_once_a_start_loaded_the_stored_snapshot() {
    let state = tempfile::tempdir().expect("tempdir");
    let root = state.path().join(SNAPSHOT_DIR);
    let slot = SnapshotSlot::create(&root, "k").expect("slot");
    let staging = slot.write_path().to_path_buf();
    write(&staging, LOADED_FILE, b"");
    slot.promote_when_ready();
    for _ in 0..100 {
        if !staging.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!staging.exists());
    assert!(!root.join("k").exists());
}
