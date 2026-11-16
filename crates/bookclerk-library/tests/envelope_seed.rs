//! Ignored fixture for the 1 vCPU / 1 GiB envelope harness.
//!
//! Deletes `library.db` (and its wal/shm) and the local object root under
//! `BOOKCLERK_FILES_DIR`, then writes 10,000 books, builds the library page
//! index over their works, and writes 100,001 one-byte objects.
//! Do not point this at a files directory you still need.
//!
//! ```text
//! BOOKCLERK_FILES_DIR="$PWD/BookclerkFiles/envelope" \
//!   cargo test -p bookclerk-library --test envelope_seed -- --ignored --nocapture
//! ```

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Instant;

use bookclerk_library::{AcquireStatus, LibraryStore, NewBook, ScannedTitle};

/// Titles written per call, the size of a large storefront scan page.
const SEED_PAGE: u32 = 200;

#[tokio::test]
#[ignore = "writes BOOKCLERK_FILES_DIR; 10k books and 100001 one-byte objects"]
async fn seed_envelope_files_dir() {
    let files = std::env::var("BOOKCLERK_FILES_DIR").unwrap_or_else(|_| {
        panic!("set BOOKCLERK_FILES_DIR to a dedicated directory such as BookclerkFiles/envelope")
    });
    let files = PathBuf::from(files);
    std::fs::create_dir_all(&files).unwrap();
    for name in ["library.db", "library.db-wal", "library.db-shm"] {
        let path = files.join(name);
        if path.exists() {
            std::fs::remove_file(&path)
                .unwrap_or_else(|err| panic!("remove {}: {err}", path.display()));
        }
    }
    let objects = files.join("objects");
    if objects.exists() {
        std::fs::remove_dir_all(&objects)
            .unwrap_or_else(|err| panic!("remove {}: {err}", objects.display()));
    }
    std::fs::write(
        files.join("config.toml"),
        r#"[library]
auto_acquire = false

[discovery]
embeddings_enabled = false

[jobs]
temp_quota_bytes = 2147483648

[jobs.concurrency]
network = 1

[media]
workers = 0

[output.local]
enabled = true
root = "objects"
"#,
    )
    .unwrap();

    let store = bookclerk_plugin_database_sqlite::open_store(&files.join("library.db"))
        .await
        .unwrap_or_else(|err| panic!("open library: {err}"));
    seed_books(&store).await;
    build_library_index(&store).await;
    seed_objects(&objects);

    let db_bytes = std::fs::metadata(files.join("library.db")).unwrap().len();
    let wal = files.join("library.db-wal");
    let wal_bytes = std::fs::metadata(&wal).map(|meta| meta.len()).unwrap_or(0);
    eprintln!(
        "envelope seed ready files={} library.db={db_bytes} wal={wal_bytes} objects={}",
        files.display(),
        objects.display()
    );
}

/// Inserts 10,000 deterministic books. A second upsert of the first id stays one row.
async fn seed_books(store: &LibraryStore) {
    store
        .upsert_account(
            "audible",
            "envelope-a",
            "us",
            Some("envelope"),
            false,
            "audible",
        )
        .await
        .unwrap();
    store
        .upsert_account(
            "audible",
            "envelope-b",
            "us",
            Some("envelope"),
            false,
            "audible",
        )
        .await
        .unwrap();
    // Written a page at a time, as a library scan writes them: one title
    // per write took about 2 minutes of the CI envelope job on its one vCPU.
    let started = Instant::now();
    for page in 0..10_000u32 / SEED_PAGE {
        let titles: Vec<ScannedTitle> = (page * SEED_PAGE..(page + 1) * SEED_PAGE)
            .map(|i| {
                let account = if i < 8_000 {
                    "envelope-a"
                } else {
                    "envelope-b"
                };
                NewBook::minimal(format!("B{i:05}"), account, "us", format!("Title {i:05}")).into()
            })
            .collect();
        store.upsert_scanned_titles(&titles).await.unwrap();
        let done = (page + 1) * SEED_PAGE;
        if done.is_multiple_of(1_000) {
            eprintln!(
                "upserted {done} books in {} ms",
                started.elapsed().as_millis()
            );
        }
    }
    let again = NewBook::minimal("B00000", "envelope-a", "us", "Title 00000");
    store.upsert_book(&again).await.unwrap();
    let backend = store.db().get_database_backend();
    sea_orm::ConnectionTrait::execute_raw(
        store.db(),
        sea_orm::Statement::from_string(
            backend,
            "UPDATE books SET acquire_status = CASE \
                WHEN (CAST(substr(product_id, 2) AS INTEGER) % 10) = 0 THEN 'error' \
                WHEN (CAST(substr(product_id, 2) AS INTEGER) % 10) = 1 THEN 'not_acquired' \
                ELSE 'acquired' END"
                .to_string(),
        ),
    )
    .await
    .unwrap();
    // The works index reads acquire state per edition, as an acquire job
    // records it with `set_acquire_status`; one statement here instead of
    // 10,000 calls.
    let recorded = sea_orm::ConnectionTrait::execute_raw(
        store.db(),
        sea_orm::Statement::from_string(
            backend,
            "INSERT OR IGNORE INTO edition_acquisitions \
                (edition_id, status, storage_key, error_message, holding_id, updated_at) \
             SELECT l.edition_id, b.acquire_status, NULL, NULL, \
                    (SELECT h.id FROM holdings h WHERE h.listing_id = l.id \
                       AND h.plugin_key = b.plugin_key AND h.account_id = b.account_id), \
                    b.updated_at \
             FROM books b JOIN store_listings l \
               ON l.plugin_key = b.plugin_key AND l.product_id = b.product_id"
                .to_string(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        recorded.rows_affected(),
        10_000,
        "every seeded title's edition records its acquire state"
    );
    let total = store.count_books(None).await.unwrap();
    assert_eq!(total, 10_000, "re-seeding the same ids must be idempotent");
    let first = store
        .get_book("B00000", "envelope-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.acquire_status, AcquireStatus::Error);
    assert_eq!(first.title, "Title 00000");
    eprintln!("books ready in {} ms", started.elapsed().as_millis());
}

/// Builds the library page index the works routes read, as the daemon's
/// `library_index_rebuild` job would, so the envelope times the routes
/// rather than a first build racing its load.
async fn build_library_index(store: &LibraryStore) {
    let started = Instant::now();
    let mut after: Option<String> = None;
    while let Some(last) = store
        .rebuild_library_index_page(after.as_deref())
        .await
        .unwrap()
    {
        after = Some(last);
    }
    store.finish_library_index_build().await.unwrap();
    assert!(store.library_index_built().await.unwrap());
    eprintln!(
        "library index built in {} ms",
        started.elapsed().as_millis()
    );
}

/// Writes 100,000 `fNNNNNN.bin` bodies plus `nest/deep/z.bin`.
fn seed_objects(root: &std::path::Path) {
    std::fs::create_dir_all(root).unwrap();
    let started = Instant::now();
    for i in 0..100_000u32 {
        std::fs::write(root.join(format!("f{i:06}.bin")), b"x").unwrap();
    }
    let nested = root.join("nest").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("z.bin"), b"z").unwrap();
    eprintln!(
        "created 100001 objects in {} ms",
        started.elapsed().as_millis()
    );
}
