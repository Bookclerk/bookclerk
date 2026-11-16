//! Ignored timing test for the library page index rebuild (#1001).
//!
//! Seeds 10,000 titles (10,000 works) into a fresh SQLite library, then
//! times a full `library_index_rebuild` walk and prints it:
//!
//! ```text
//! cargo test --release -p bookclerk-library --test library_index_timing -- --ignored --nocapture
//! ```
//!
//! Set `BOOKCLERK_INDEX_TIMING_DB` to the path of an already seeded
//! `library.db` to time the rebuild on a copy of it instead of seeding.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::unwrap_used)]

use std::time::Instant;

use bookclerk_library::{LibraryStore, NewBook, ScannedTitle};

/// Titles seeded.
const TITLES: u32 = 10_000;

/// Titles written per call, the size of a large storefront scan page.
const SEED_PAGE: u32 = 200;

#[tokio::test]
#[ignore = "timing: seeds 10,000 titles, then rebuilds the library page index"]
async fn rebuild_of_ten_thousand_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let seeded = std::env::var_os("BOOKCLERK_INDEX_TIMING_DB");
    if let Some(source) = &seeded {
        std::fs::copy(source, &path).unwrap();
    }
    let store = bookclerk_plugin_database_sqlite::open_store(&path)
        .await
        .unwrap_or_else(|err| panic!("open library: {err}"));
    if seeded.is_none() {
        seed(&store).await;
    }

    let started = Instant::now();
    let mut pages = 0u32;
    let mut after: Option<String> = None;
    while let Some(last) = store
        .rebuild_library_index_page(after.as_deref())
        .await
        .unwrap()
    {
        pages += 1;
        after = Some(last);
    }
    store.finish_library_index_build().await.unwrap();
    let elapsed = started.elapsed();
    assert!(store.library_index_built().await.unwrap());
    eprintln!(
        "library index rebuilt: {pages} page(s) in {} ms",
        elapsed.as_millis()
    );
}

/// Writes [`TITLES`] deterministic titles a scan page at a time.
async fn seed(store: &LibraryStore) {
    store
        .upsert_account("audible", "timing", "us", Some("timing"), false, "audible")
        .await
        .unwrap();
    let started = Instant::now();
    for page in 0..TITLES / SEED_PAGE {
        let titles: Vec<ScannedTitle> = (page * SEED_PAGE..(page + 1) * SEED_PAGE)
            .map(|i| {
                let mut book =
                    NewBook::minimal(format!("T{i:05}"), "timing", "us", format!("Title {i:05}"));
                book.authors = Some(format!("Author {}", i % 997));
                book.narrators = Some(format!("Narrator {}", i % 499));
                book.into()
            })
            .collect();
        store.upsert_scanned_titles(&titles).await.unwrap();
    }
    eprintln!(
        "seeded {TITLES} titles in {} ms",
        started.elapsed().as_millis()
    );
}
