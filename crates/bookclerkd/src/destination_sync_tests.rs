//! Tests for [`super`]: following outside changes at a destination.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use bookclerk_library::{CopyState, NewBook};
use bookclerk_storage::{
    ChangeFeedPage, LocalFsBackend, ObjectChange, ObjectInfo, ObjectMeta, StorageCapabilities,
    StorageError,
};

use super::*;

/// An in-memory library.
async fn test_store() -> LibraryStore {
    LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    )
}

/// Writes `key` under `backend`.
async fn put(backend: &LocalFsBackend, key: &str) {
    backend
        .put(
            key,
            bytes::Bytes::from_static(b"audio"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
}

/// An edition `store` holds with a stored copy on `destination` at `key`.
async fn stored_edition(store: &LibraryStore, destination: &str, key: &str) -> String {
    store
        .upsert_account("audible", "user-1", "us", None, true, "audible")
        .await
        .unwrap();
    store
        .upsert_book(&NewBook::minimal("B00TEST", "user-1", "us", "Test"))
        .await
        .unwrap();
    let edition = store
        .acquire_titles_named(None, "B00TEST")
        .await
        .unwrap()
        .remove(0)
        .edition_id;
    store
        .set_copy_stored(&edition, destination, key)
        .await
        .unwrap();
    edition
}

/// The stored state of `edition`'s copy on `destination`.
async fn copy_state(store: &LibraryStore, edition: &str, destination: &str) -> CopyState {
    store
        .copies_of_editions(&[edition.to_owned()])
        .await
        .unwrap()
        .into_iter()
        .find(|copy| copy.destination == destination)
        .unwrap()
        .state
}

/// Makes `destination`'s scheduled listing due now.
async fn make_listing_due(store: &LibraryStore, destination: &str) {
    let mut feed = store.destination_feed(destination).await.unwrap().unwrap();
    feed.next_listing_at = Some(Utc::now() - chrono::Duration::seconds(1));
    store.save_destination_feed(&feed).await.unwrap();
}

/// Without a change feed: the first pass lists everything, a pass before
/// the next listing is due reads nothing, and a later listing notices a
/// deleted file, whose stored copy turns pending.
#[tokio::test]
async fn a_listing_notices_a_deleted_file_and_its_copy_is_written_again() {
    let store = test_store().await;
    let dir = tempfile::tempdir().unwrap();
    let local = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
    for key in ["A/Test.m4b", "A/cover.jpg", "B/Other.m4b"] {
        put(&local, key).await;
    }
    let edition = stored_edition(&store, "local", "A/Test.m4b").await;

    let first = sync_destination(&store, "local", &local, None)
        .await
        .unwrap();
    assert!(first.listing_finished);
    assert!(first.changes >= 3, "{first:?}");
    let feed = store.destination_feed("local").await.unwrap().unwrap();
    assert_eq!(feed.feed_state, FeedState::Off);
    assert!(feed.objects >= 3);
    assert!(feed.listed_at.is_some());
    assert!(feed.next_listing_at.unwrap() > Utc::now() + chrono::Duration::hours(5));
    assert_eq!(feed.checked_seq, feed.seq);

    let quiet = sync_destination(&store, "local", &local, None)
        .await
        .unwrap();
    assert_eq!(quiet, SyncReport::default(), "nothing due");

    local.delete("A/Test.m4b").await.unwrap();
    make_listing_due(&store, "local").await;
    let after = sync_destination(&store, "local", &local, None)
        .await
        .unwrap();
    assert!(after.listing_finished);
    assert_eq!(after.lost_copies, 1, "{after:?}");
    assert_eq!(after.held_back, 0);
    assert_eq!(
        copy_state(&store, &edition, "local").await,
        CopyState::Pending
    );
}

/// A listing that misses most of what the last one saw is held back: no
/// copies change, and the destination's health says why.
#[tokio::test]
async fn a_listing_that_misses_most_files_is_held_back() {
    let store = test_store().await;
    let dir = tempfile::tempdir().unwrap();
    let local = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
    for i in 0..20 {
        put(&local, &format!("A/{i:02}.m4b")).await;
    }
    let edition = stored_edition(&store, "local", "A/00.m4b").await;
    sync_destination(&store, "local", &local, None)
        .await
        .unwrap();

    // The folder is empty now (an unmounted disk looks like this).
    let empty = tempfile::tempdir().unwrap();
    let unmounted = LocalFsBackend::new(empty.path().to_path_buf()).unwrap();
    make_listing_due(&store, "local").await;
    let report = sync_destination(&store, "local", &unmounted, None)
        .await
        .unwrap();
    assert!(report.held_back >= 20, "{report:?}");
    assert_eq!(report.lost_copies, 0);
    assert_eq!(
        copy_state(&store, &edition, "local").await,
        CopyState::Stored
    );
    let feed = store.destination_feed("local").await.unwrap().unwrap();
    assert!(feed
        .listing_error
        .as_deref()
        .is_some_and(|error| error.contains("removals were not applied")));

    // Back again: nothing changed, and the error clears.
    make_listing_due(&store, "local").await;
    let back = sync_destination(&store, "local", &local, None)
        .await
        .unwrap();
    assert_eq!(back.held_back, 0);
    assert_eq!(back.changes, 0, "{back:?}");
    let feed = store.destination_feed("local").await.unwrap().unwrap();
    assert_eq!(feed.listing_error, None);
}

/// A destination double whose change feed answers from a script, over a
/// local folder for everything else.
struct ScriptedFeed {
    /// The folder the double stores in.
    inner: LocalFsBackend,
    /// Answers to `changes`, oldest first; an empty script answers an
    /// empty complete page.
    pages: Mutex<VecDeque<bookclerk_storage::Result<ChangeFeedPage>>>,
    /// Cursors `changes` was called with.
    cursors: Mutex<Vec<Option<String>>>,
}

impl ScriptedFeed {
    /// A double over `root` that answers `pages` in order.
    fn new(root: &Path, pages: Vec<bookclerk_storage::Result<ChangeFeedPage>>) -> Self {
        Self {
            inner: LocalFsBackend::new(root.to_path_buf()).unwrap(),
            pages: Mutex::new(pages.into()),
            cursors: Mutex::new(Vec::new()),
        }
    }
}

/// A feed page with `changes`, ending at `cursor`.
fn page(changes: Vec<ObjectChange>, cursor: &str, complete: bool) -> ChangeFeedPage {
    ChangeFeedPage {
        changes,
        cursor: cursor.into(),
        more: false,
        complete,
        live_watch: false,
    }
}

#[async_trait::async_trait]
impl StorageBackend for ScriptedFeed {
    fn name(&self) -> &'static str {
        "scripted-feed"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(Self {
            inner: self.inner.clone(),
            pages: Mutex::new(VecDeque::new()),
            cursors: Mutex::new(Vec::new()),
        })
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            changes: true,
            ..StorageCapabilities::default()
        }
    }

    async fn changes(
        &self,
        cursor: Option<&str>,
        _limit: u32,
    ) -> bookclerk_storage::Result<Option<ChangeFeedPage>> {
        self.cursors.lock().unwrap().push(cursor.map(str::to_owned));
        let next = self.pages.lock().unwrap().pop_front();
        match next {
            Some(answer) => answer.map(Some),
            None => Ok(Some(page(Vec::new(), cursor.unwrap_or("c0"), true))),
        }
    }

    async fn put(
        &self,
        key: &str,
        data: bytes::Bytes,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        self.inner.put(key, data, meta).await
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        self.inner.put_file(key, path, meta).await
    }

    async fn get(&self, key: &str) -> bookclerk_storage::Result<bytes::Bytes> {
        self.inner.get(key).await
    }

    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        self.inner.exists(key).await
    }

    async fn list(&self, prefix: &str) -> bookclerk_storage::Result<Vec<ObjectInfo>> {
        self.inner.list(prefix).await
    }

    async fn probe(&self, key: &str) -> bookclerk_storage::Result<bookclerk_storage::ObjectProbe> {
        self.inner.probe(key).await
    }

    async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        self.inner.copy(from, to).await
    }

    async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
        self.inner.delete(key).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> bookclerk_storage::Result<bookclerk_storage::ListPage> {
        self.inner.list_page(prefix, cursor, limit).await
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<bookclerk_storage::ByteRange>,
    ) -> bookclerk_storage::Result<(
        bookclerk_storage::ObjectProbe,
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    )> {
        self.inner.get_stream(key, range).await
    }

    async fn put_stream(
        &self,
        key: &str,
        body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<bookclerk_storage::PutStreamResult> {
        self.inner.put_stream(key, body, meta).await
    }
}

/// A healthy feed reports a removal that turns the copy pending, keeps its
/// cursor, and stretches the listing interval; a feed that reports a gap
/// or fails sends the next pass to a listing at once.
#[tokio::test]
async fn a_healthy_feed_reports_changes_and_a_gap_or_failure_lists_at_once() {
    let store = test_store().await;
    let dir = tempfile::tempdir().unwrap();
    let removed = ObjectChange {
        key: "A/Test.m4b".into(),
        kind: ObjectChangeKind::Removed,
        info: None,
    };
    let probed = ObjectChange {
        key: "B/New.m4b".into(),
        kind: ObjectChangeKind::Changed,
        info: None,
    };
    let backend = ScriptedFeed::new(
        dir.path(),
        vec![
            Ok(page(Vec::new(), "c1", true)),
            Ok(page(vec![removed, probed], "c2", true)),
            Ok(page(Vec::new(), "c3", false)),
            Err(StorageError::Other(anyhow::anyhow!("queue unreachable"))),
        ],
    );
    put(&backend.inner, "A/Test.m4b").await;
    let edition = stored_edition(&store, "s3", "A/Test.m4b").await;

    // First pass: the feed starts and the first listing runs.
    let first = sync_destination(&store, "s3", &backend, None)
        .await
        .unwrap();
    assert!(first.listing_finished);
    let feed = store.destination_feed("s3").await.unwrap().unwrap();
    assert_eq!(feed.feed_state, FeedState::Healthy);
    assert_eq!(feed.feed_cursor.as_deref(), Some("c1"));
    assert!(feed.next_listing_at.unwrap() > Utc::now() + chrono::Duration::hours(23));

    // Second pass: the feed reports the removal and a new file, no listing.
    backend.inner.delete("A/Test.m4b").await.unwrap();
    put(&backend.inner, "B/New.m4b").await;
    let second = sync_destination(&store, "s3", &backend, None)
        .await
        .unwrap();
    assert!(!second.listing_finished);
    assert_eq!(second.listing_pages, 0);
    assert_eq!(second.changes, 2, "{second:?}");
    assert_eq!(second.lost_copies, 1);
    assert_eq!(copy_state(&store, &edition, "s3").await, CopyState::Pending);
    assert_eq!(
        backend.cursors.lock().unwrap().last().cloned().flatten(),
        Some("c1".into()),
        "the feed is read from the saved cursor"
    );

    // Third pass: a gap lists at once and then counts as healthy again.
    let third = sync_destination(&store, "s3", &backend, None)
        .await
        .unwrap();
    assert!(third.listing_finished, "{third:?}");
    let feed = store.destination_feed("s3").await.unwrap().unwrap();
    assert_eq!(feed.feed_state, FeedState::Healthy);

    // Fourth pass: a failing feed lists at once and says why.
    let fourth = sync_destination(&store, "s3", &backend, None)
        .await
        .unwrap();
    assert!(fourth.listing_finished);
    let feed = store.destination_feed("s3").await.unwrap().unwrap();
    assert_eq!(feed.feed_state, FeedState::Failing);
    assert!(feed
        .feed_error
        .as_deref()
        .is_some_and(|error| error.contains("queue unreachable")));
    assert_eq!(feed.feed_cursor.as_deref(), Some("c3"), "cursor kept");
}
