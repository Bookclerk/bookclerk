//! `bookclerk library search` reads the works index, which `--rebuild-index`
//! builds first.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{run_bookclerk, stage_sqlite_plugin, stderr, stdout, write_config};

#[test]
fn rebuild_index_builds_the_works_index_that_search_reads() {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let out = run_bookclerk(
        files,
        &[
            "library",
            "search",
            "--rebuild-index",
            "--limit",
            "0",
            "Title",
        ],
        &[],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("works search index rebuilt: 0 work(s)"),
        "{text}"
    );
    let works = files.join("search_index_works");
    assert!(
        bookclerk_search::WorkSearchIndex::is_complete(&works),
        "the works index is marked complete"
    );
    assert!(
        !files.join("search_index").join("meta.json").exists(),
        "no per-title index is written"
    );
}

#[test]
fn a_search_before_the_index_is_built_says_how_to_build_it() {
    let files_dir = tempfile::tempdir().unwrap();
    let files = files_dir.path();
    write_config(files);
    stage_sqlite_plugin(files);
    let approved = run_bookclerk(files, &["plugins", "approve", "sqlite", "--yes"], &[]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    let out = run_bookclerk(files, &["library", "search", "Title"], &[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--rebuild-index"), "{}", stderr(&out));
}
