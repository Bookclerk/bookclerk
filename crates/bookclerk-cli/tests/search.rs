//! `bookclerk library search --rebuild-index` rebuilds both search indexes.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use common::{run_bookclerk, stage_sqlite_plugin, stderr, stdout, write_config};

#[test]
fn rebuild_index_rebuilds_the_title_and_works_indexes() {
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
    assert!(text.contains("search index rebuilt: 0 book(s)"), "{text}");
    assert!(
        text.contains("works search index rebuilt: 0 work(s)"),
        "{text}"
    );
    let works = bookclerk_search::WorkSearchIndex::dir_beside(&files.join("search_index"));
    assert!(
        bookclerk_search::WorkSearchIndex::is_complete(&works),
        "the works index is marked complete"
    );
}
