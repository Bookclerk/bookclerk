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
