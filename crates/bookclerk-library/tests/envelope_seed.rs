//! Ignored fixture for the 1 vCPU / 1 GiB envelope harness.
//!
//! Deletes `library.db` (and its wal/shm) and the local object root under
//! `BOOKCLERK_FILES_DIR`, then writes 10,000 books and 100,001 one-byte objects.
//! Do not point this at a files directory you still need.
//!
//! ```text
//! BOOKCLERK_FILES_DIR="$PWD/BookclerkFiles/envelope" \
//!   cargo test -p bookclerk-library --test envelope_seed -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Instant;

use bookclerk_library::{AcquireStatus, LibraryStore, NewBook};

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
        .upsert_account("envelope-a", "us", Some("envelope"), false, "audible")
        .await
        .unwrap();
    store
        .upsert_account("envelope-b", "us", Some("envelope"), false, "audible")
        .await
        .unwrap();
    let started = Instant::now();
    for i in 0..10_000u32 {
        let account = if i < 8_000 {
            "envelope-a"
        } else {
            "envelope-b"
        };
        let book = NewBook::minimal(format!("B{i:05}"), account, "us", format!("Title {i:05}"));
        store.upsert_book(&book).await.unwrap();
        if i > 0 && i % 1_000 == 0 {
            eprintln!("upserted {i} books in {} ms", started.elapsed().as_millis());
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
