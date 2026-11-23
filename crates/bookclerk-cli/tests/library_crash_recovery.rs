//! Startup after a crash in the middle of a database write, through the real
//! `bookclerk` binary and the jailed sqlite guest.
//!
//! A process killed mid-write leaves a hot `library.db-journal`. The jailed
//! guest may write that file but not delete anything in the files directory,
//! so recovery must roll the journal back without unlinking it (issue #1349).

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::fs;
use std::path::Path;

use common::{run_bookclerk, stage_sqlite_plugin, stderr, write_config};

/// Leaves `library.db` and `library.db-journal` in `files` as a process
/// killed in the middle of a write would: the database file holds pages of an
/// uncommitted transaction and the journal holds what they replaced.
///
/// The live write runs on a copy so the test's own connection never holds a
/// lock on the files the guest opens next.
fn plant_hot_journal(files: &Path) {
    let scratch = tempfile::tempdir().unwrap();
    let live = scratch.path().join("library.db");
    fs::copy(files.join("library.db"), &live).unwrap();
    let conn = rusqlite::Connection::open(&live).unwrap();
    // A tiny page cache makes SQLite spill dirty pages into the database file
    // before COMMIT, which is what makes the journal hot.
    conn.execute_batch(
        "PRAGMA journal_mode = TRUNCATE;
         PRAGMA cache_size = 2;
         BEGIN IMMEDIATE;
         CREATE TABLE crash_probe (id INTEGER PRIMARY KEY, v TEXT NOT NULL);",
    )
    .unwrap();
    for id in 0..500 {
        conn.execute(
            "INSERT INTO crash_probe (id, v) VALUES (?1, ?2)",
            (id, "x".repeat(400)),
        )
        .unwrap();
    }
    fs::copy(&live, files.join("library.db")).unwrap();
    fs::copy(
        scratch.path().join("library.db-journal"),
        files.join("library.db-journal"),
    )
    .unwrap();
    drop(conn);
    let journal = fs::read(files.join("library.db-journal")).unwrap();
    assert!(
        journal.first().is_some_and(|byte| *byte != 0),
        "the planted journal must be hot"
    );
}

#[test]
fn startup_after_a_crash_mid_write_rolls_back_without_unlinking_the_journal() {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(
        approved.status.success(),
        "approve sqlite: {}",
        stderr(&approved)
    );
    let enrolled = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(
        enrolled.status.success(),
        "first enrollment: {}",
        stderr(&enrolled)
    );

    plant_hot_journal(files);

    let restarted = run_bookclerk(files, &["config", "get", "events.retention_days"], &[]);
    assert!(
        restarted.status.success(),
        "startup after a crash mid-write must recover: {}",
        stderr(&restarted)
    );
    assert!(
        String::from_utf8_lossy(&restarted.stdout).contains('7'),
        "{}",
        String::from_utf8_lossy(&restarted.stdout)
    );
    assert!(
        files.join("library.db-journal").exists(),
        "recovery must truncate the journal, not unlink it"
    );
    let conn = rusqlite::Connection::open(files.join("library.db")).unwrap();
    let probe: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'crash_probe'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(probe, 0, "the uncommitted write must be rolled back");
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
}
