//! Intent-based storage operations and the host's fallback for each.
//!
//! The host says what it wants (move these objects, delete these keys, probe
//! these keys, write only if unchanged, what changed since a cursor) and the
//! backend chooses how. A backend that does an operation natively says so in
//! [`StorageCapabilities`] and overrides the [`StorageBackend`] method; every
//! other backend gets the fallback here, built only from the required
//! primitives (`head`, `list_page`, `copy`, `get_stream`, `put_stream`,
//! `delete`). See `docs/plugins/abi.md#intent-operations`.

use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};
use crate::traits::{ListPage, ObjectProbe, PutStreamResult, StorageBackend};

pub use bookclerk_plugin_sdk::{MoveOptions, MoveResult, ObjectChangeKind};

/// Which intent operations a backend does natively.
///
/// A `false` field means the host runs its fallback for that operation. The
/// fallback gives the same result when no one else writes the same keys
/// meanwhile, but it may be slower (a copy and a delete instead of a rename)
/// and is not atomic: a concurrent writer can slip in between its steps (a
/// conditional write checked before writing, a `no_overwrite` target created
/// after the check).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCapabilities {
    /// Single moves ([`StorageBackend::move_object`]).
    pub move_object: bool,
    /// Batch and prefix moves ([`StorageBackend::move_many`],
    /// [`StorageBackend::move_prefix`]).
    pub move_many: bool,
    /// Batch deletes ([`StorageBackend::delete_many`]).
    pub delete_many: bool,
    /// Batch probes ([`StorageBackend::head_many`]).
    pub head_many: bool,
    /// Conditional writes enforced by the backend
    /// ([`StorageBackend::put_stream_conditional`]).
    pub conditional_write: bool,
    /// A change feed ([`StorageBackend::changes`]).
    pub changes: bool,
}

/// Outcome for one key of a batch operation.
#[derive(Debug)]
pub struct KeyResult {
    /// The key (the source key for moves).
    pub key: String,
    /// The failure for this key; `None` when it succeeded.
    pub error: Option<StorageError>,
}

impl KeyResult {
    /// Builds an outcome for `key` from a per-key result.
    #[must_use]
    pub fn from_result(key: impl Into<String>, result: Result<()>) -> Self {
        Self {
            key: key.into(),
            error: result.err(),
        }
    }
}

/// One page of a prefix move.
#[derive(Debug, Default)]
pub struct MovePrefixPage {
    /// Outcomes, one per object attempted (or one for the whole prefix when
    /// the backend moved it in one step).
    pub results: Vec<KeyResult>,
    /// Cursor for the next call; `None` when every object moved.
    pub next_cursor: Option<String>,
    /// True when every move in this page was atomic.
    pub atomic: bool,
    /// Objects moved in this page.
    pub objects_moved: u64,
}

/// A precondition on a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteCondition {
    /// Write only while the stored object's etag equals this value.
    IfMatch(String),
    /// Write only when no object exists at the key.
    IfNoneMatch,
}

/// Result of [`StorageBackend::put_stream_conditional`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConditionalPutResult {
    /// The write itself.
    pub put: PutStreamResult,
    /// True when the backend checked the condition atomically with the
    /// write. False for the fallback, which checks first and then writes, so
    /// a concurrent writer can slip in between (the caller's next listing
    /// sees it as an outside change).
    pub atomic: bool,
}

/// A listing request with hints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListRequest {
    /// Key prefix filter.
    pub prefix: String,
    /// Opaque continuation token from the previous page.
    pub cursor: Option<String>,
    /// Requested page size, clamped to [`crate::MAX_LIST_PAGE`].
    pub limit: u32,
    /// Hint: fill the signature fields of each listed object.
    pub with_signatures: bool,
    /// Hint: only objects modified at or after this UTC unix millisecond.
    pub modified_since_ms: Option<u64>,
}

/// A listing page that says how far the hints were honoured.
#[derive(Debug, Clone, Default)]
pub struct HintedListPage {
    /// The page.
    pub page: ListPage,
    /// True when every entry carries the signature fields the backend has.
    pub signatures_included: bool,
    /// True when objects modified before the requested time were left out;
    /// false means the page is unfiltered and the caller compares.
    pub modified_since_applied: bool,
}

/// One change in a change feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectChange {
    /// Object key.
    pub key: String,
    /// What happened to it.
    pub kind: ObjectChangeKind,
    /// Listing fields after the change, when the feed carries them.
    pub info: Option<crate::ObjectInfo>,
}

/// One page of a change feed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeFeedPage {
    /// Changes since the cursor, oldest first.
    pub changes: Vec<ObjectChange>,
    /// Cursor for the next call.
    pub cursor: String,
    /// True when more changes are ready now.
    pub more: bool,
    /// False when the feed may have missed changes: back it with a listing.
    pub complete: bool,
}

/// Moves one object with the required primitives: probe, copy (server-side
/// when the backend supports it, else streamed), then delete the source.
///
/// A repeated move whose source is gone and whose target exists succeeds
/// with `already_moved`.
///
/// Concurrent writers are handled as far as the primitives allow:
///
/// - With `no_overwrite` on a backend that enforces conditional writes, the
///   target is written with `IfNoneMatch`, so an object created at the
///   target after the probe is never replaced. Elsewhere the check runs
///   just before the copy, and the result's `atomic: false` says so.
/// - The source is probed again after the copy and deleted only when it is
///   provably still the object that was copied (same etag, else same size
///   and modification time). A source replaced during the copy, or one whose
///   probes cannot prove it, is kept and the move fails with `Conflict`. The
///   ABI has no conditional delete, so a rewrite between that probe and the
///   delete can still be lost.
///
/// # Errors
///
/// Returns [`StorageError::NotFound`] when neither key exists,
/// [`StorageError::Conflict`] when a precondition in `options` fails or the
/// source changed during the copy, and backend errors otherwise.
pub async fn fallback_move<B: StorageBackend + ?Sized>(
    backend: &B,
    from: &str,
    to: &str,
    options: &MoveOptions,
) -> Result<MoveResult> {
    let Some(source) = backend.head(from).await? else {
        if from != to && backend.head(to).await?.is_some() {
            return Ok(MoveResult {
                atomic: false,
                already_moved: true,
            });
        }
        return Err(StorageError::NotFound(from.to_string()));
    };
    if let Some(expected) = &options.if_source_match {
        if source.etag.as_deref() != Some(expected.as_str()) {
            return Err(StorageError::Conflict(format!(
                "{from} changed since it was read"
            )));
        }
    }
    if from == to {
        return Ok(MoveResult {
            atomic: true,
            already_moved: false,
        });
    }
    if options.no_overwrite && backend.capabilities().conditional_write {
        copy_streamed(backend, &source, to, Some(&WriteCondition::IfNoneMatch)).await?;
    } else {
        if options.no_overwrite && backend.head(to).await?.is_some() {
            return Err(StorageError::Conflict(format!("{to} already exists")));
        }
        if backend.supports_server_copy() {
            backend.copy(from, to).await?;
        } else {
            copy_streamed(backend, &source, to, None).await?;
        }
    }
    match backend.head(from).await? {
        // Someone else already removed the source; the copy stands.
        None => {}
        Some(now) if same_object(&source, &now) => backend.delete(from).await?,
        Some(_) => {
            return Err(StorageError::Conflict(format!(
                "{from} changed while it was copied to {to}; the source was kept"
            )));
        }
    }
    Ok(MoveResult {
        atomic: false,
        already_moved: false,
    })
}

/// True when two probes of one key describe the same stored object as far as
/// the probes can tell: equal etags when both have one, else equal size and a
/// modification time both report and agree on. The size-and-time branch is a
/// heuristic (a same-size rewrite within the same millisecond passes it);
/// backends that need a strict move declare `storage.move`. Probes that carry
/// neither count as different, so the source is kept.
fn same_object(before: &ObjectProbe, after: &ObjectProbe) -> bool {
    match (&before.etag, &after.etag) {
        (Some(a), Some(b)) => a == b,
        _ => {
            before.size == after.size
                && before.modified_at_ms.is_some()
                && before.modified_at_ms == after.modified_at_ms
        }
    }
}

/// Copies `source` to `to` through a streamed read and write, under
/// `condition` when one is given.
///
/// # Errors
///
/// Returns backend read or write errors.
async fn copy_streamed<B: StorageBackend + ?Sized>(
    backend: &B,
    source: &ObjectProbe,
    to: &str,
    condition: Option<&WriteCondition>,
) -> Result<()> {
    let (opened, body) = backend.get_stream(&source.key, None).await?;
    let mut meta = opened.meta.clone();
    meta.content_length = Some(opened.size);
    meta.commit_token = None;
    match condition {
        Some(condition) => {
            backend
                .put_stream_conditional(to, body, meta, condition)
                .await?;
        }
        None => {
            backend.put_stream(to, body, meta).await?;
        }
    }
    Ok(())
}

/// Moves each pair in turn with [`StorageBackend::move_object`]. One pair's
/// failure does not stop the others.
pub async fn fallback_move_many<B: StorageBackend + ?Sized>(
    backend: &B,
    pairs: &[(String, String)],
    options: &MoveOptions,
) -> Vec<KeyResult> {
    let options = MoveOptions {
        if_source_match: None,
        ..options.clone()
    };
    let mut out = Vec::with_capacity(pairs.len());
    for (from, to) in pairs {
        let result = backend.move_object(from, to, &options).await.map(|_| ());
        out.push(KeyResult::from_result(from.clone(), result));
    }
    out
}

/// Rejects a prefix move whose namespaces overlap.
///
/// A target inside the source would move objects it just moved; a source
/// inside the target lets one moved object overwrite another that has not
/// moved yet (`a/b/` to `a/` sends `a/b/b/x` onto `a/b/x`). Prefixes are
/// compared after dropping `.` and empty path segments, with `\` read as
/// `/`, so `./a/sub/`, `a//sub/` and `a\sub/` count as `a/sub/`.
///
/// # Errors
///
/// Returns [`StorageError::InvalidKey`] for an empty source prefix or
/// overlapping prefixes.
pub fn require_disjoint_prefixes(from_prefix: &str, to_prefix: &str) -> Result<()> {
    let from = normalized_prefix(from_prefix);
    let to = normalized_prefix(to_prefix);
    if from.is_empty() {
        return Err(StorageError::InvalidKey(
            "a prefix move needs a non-empty source prefix".into(),
        ));
    }
    if to.starts_with(&from) || from.starts_with(&to) {
        return Err(StorageError::InvalidKey(format!(
            "target prefix {to_prefix} overlaps source prefix {from_prefix}"
        )));
    }
    Ok(())
}

/// `prefix` with `.` and empty segments removed. A trailing separator is kept,
/// so `a/` and `ab` stay disjoint while `a` and `ab` overlap as raw prefixes
/// do. `\` counts as a separator too, because the local backend treats it as
/// one on Windows; elsewhere that only refuses a few moves that were safe.
fn normalized_prefix(prefix: &str) -> String {
    let parts: Vec<&str> = prefix
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let mut out = parts.join("/");
    if prefix.ends_with(['/', '\\']) && !out.is_empty() {
        out.push('/');
    }
    out
}

/// Moves one listing page under `from_prefix` to `to_prefix`.
///
/// The cursor is the backend's own listing cursor. Moved objects leave the
/// source prefix, so a backend whose listing cursor does not survive deletes
/// answers [`StorageError::InvalidCursor`]; the caller then starts over with
/// `None`, which is safe because everything already moved is gone.
///
/// # Errors
///
/// Returns [`StorageError::InvalidKey`] for overlapping prefixes and listing
/// errors; per-object failures are in the page.
pub async fn fallback_move_prefix<B: StorageBackend + ?Sized>(
    backend: &B,
    from_prefix: &str,
    to_prefix: &str,
    cursor: Option<&str>,
    limit: u32,
    options: &MoveOptions,
) -> Result<MovePrefixPage> {
    require_disjoint_prefixes(from_prefix, to_prefix)?;
    let page = backend.list_page(from_prefix, cursor, limit).await?;
    let pairs: Vec<(String, String)> = page
        .objects
        .iter()
        .filter_map(|o| {
            o.key
                .strip_prefix(from_prefix)
                .map(|rest| (o.key.clone(), format!("{to_prefix}{rest}")))
        })
        .collect();
    let results = backend.move_many(&pairs, options).await?;
    let moved = results.iter().filter(|r| r.error.is_none()).count();
    Ok(MovePrefixPage {
        results,
        next_cursor: page.next_cursor,
        atomic: false,
        objects_moved: u64::try_from(moved).unwrap_or(u64::MAX),
    })
}

/// Deletes each key in turn. One key's failure does not stop the others.
pub async fn fallback_delete_many<B: StorageBackend + ?Sized>(
    backend: &B,
    keys: &[String],
) -> Vec<KeyResult> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        out.push(KeyResult::from_result(
            key.clone(),
            backend.delete(key).await,
        ));
    }
    out
}

/// Probes each key in turn.
///
/// # Errors
///
/// Returns the first probe failure other than absence.
pub async fn fallback_head_many<B: StorageBackend + ?Sized>(
    backend: &B,
    keys: &[String],
) -> Result<Vec<Option<ObjectProbe>>> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        out.push(backend.head(key).await?);
    }
    Ok(out)
}

/// Checks `condition` with a probe, then writes. Not atomic: the result says
/// so.
///
/// # Errors
///
/// Returns [`StorageError::Conflict`] when the condition fails, and backend
/// errors otherwise.
pub async fn fallback_put_conditional<B: StorageBackend + ?Sized>(
    backend: &B,
    key: &str,
    body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    meta: crate::ObjectMeta,
    condition: &WriteCondition,
) -> Result<ConditionalPutResult> {
    let current = backend.head(key).await?;
    match (condition, &current) {
        (WriteCondition::IfNoneMatch, Some(_)) => {
            return Err(StorageError::Conflict(format!("{key} already exists")));
        }
        (WriteCondition::IfMatch(expected), probe) => {
            if probe.as_ref().and_then(|p| p.etag.as_deref()) != Some(expected.as_str()) {
                return Err(StorageError::Conflict(format!(
                    "{key} changed since it was read"
                )));
            }
        }
        (WriteCondition::IfNoneMatch, None) => {}
    }
    let put = backend.put_stream(key, body, meta).await?;
    Ok(ConditionalPutResult { put, atomic: false })
}

/// Lists one page and reports that no hint was honoured: no signatures
/// beyond what the plain listing carries, no time filter.
///
/// # Errors
///
/// Returns listing errors.
pub async fn fallback_list_hinted<B: StorageBackend + ?Sized>(
    backend: &B,
    request: &ListRequest,
) -> Result<HintedListPage> {
    let page = backend
        .list_page(&request.prefix, request.cursor.as_deref(), request.limit)
        .await?;
    Ok(HintedListPage {
        page,
        signatures_included: false,
        modified_since_applied: false,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::LocalFsBackend;
    use bytes::Bytes;

    async fn backend() -> (tempfile::TempDir, LocalFsBackend) {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        (dir, backend)
    }

    async fn put(b: &LocalFsBackend, key: &str, body: &'static [u8]) {
        b.put(key, Bytes::from_static(body), crate::ObjectMeta::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fallback_move_moves_and_repeats_safely() {
        let (_dir, b) = backend().await;
        put(&b, "a/one.m4b", b"1").await;
        let first = fallback_move(&b, "a/one.m4b", "b/one.m4b", &MoveOptions::default())
            .await
            .unwrap();
        assert!(!first.already_moved);
        assert!(!b.exists("a/one.m4b").await.unwrap());
        assert_eq!(b.get("b/one.m4b").await.unwrap().as_ref(), b"1");
        let again = fallback_move(&b, "a/one.m4b", "b/one.m4b", &MoveOptions::default())
            .await
            .unwrap();
        assert!(again.already_moved);
    }

    #[tokio::test]
    async fn fallback_move_honours_no_overwrite() {
        let (_dir, b) = backend().await;
        put(&b, "a", b"1").await;
        put(&b, "b", b"2").await;
        let options = MoveOptions {
            no_overwrite: true,
            ..MoveOptions::default()
        };
        let err = fallback_move(&b, "a", "b", &options).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        assert_eq!(b.get("b").await.unwrap().as_ref(), b"2");
    }

    #[tokio::test]
    async fn fallback_move_missing_source_is_not_found() {
        let (_dir, b) = backend().await;
        let err = fallback_move(&b, "x", "y", &MoveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn fallback_prefix_move_pages_until_done() {
        let (_dir, b) = backend().await;
        for key in ["Old/1.m4b", "Old/2.m4b", "Old/sub/3.m4b", "Other/4.m4b"] {
            put(&b, key, b"x").await;
        }
        let mut cursor = None;
        let mut moved = 0;
        loop {
            let page = fallback_move_prefix(
                &b,
                "Old/",
                "New/",
                cursor.as_deref(),
                2,
                &MoveOptions::default(),
            )
            .await;
            let page = match page {
                Ok(page) => page,
                Err(StorageError::InvalidCursor(_)) => {
                    cursor = None;
                    continue;
                }
                Err(err) => panic!("{err}"),
            };
            assert!(page.results.iter().all(|r| r.error.is_none()));
            moved += page.objects_moved;
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(moved, 3);
        assert!(b.exists("New/sub/3.m4b").await.unwrap());
        assert!(b.exists("Other/4.m4b").await.unwrap());
        assert!(!b.exists("Old/1.m4b").await.unwrap());
    }

    #[test]
    fn prefixes_must_not_nest() {
        assert!(require_disjoint_prefixes("", "x/").is_err());
        assert!(require_disjoint_prefixes("a/", "a/b/").is_err());
        assert!(require_disjoint_prefixes("a/b/", "a/").is_err());
        assert!(require_disjoint_prefixes("a/", "./a/sub/").is_err());
        assert!(require_disjoint_prefixes("a//b/", "a/b/c/").is_err());
        assert!(require_disjoint_prefixes("a/", "a\\b/").is_err());
        assert!(require_disjoint_prefixes("a\\b\\", "a/").is_err());
        assert!(require_disjoint_prefixes("./", "x/").is_err());
        assert!(require_disjoint_prefixes("a/", "b/").is_ok());
        assert!(require_disjoint_prefixes("a/", "ab/").is_ok());
    }

    /// A local backend where another writer rewrites `a` while it is being
    /// copied (right after the streamed write lands).
    #[derive(Clone)]
    struct RewritesSourceDuringCopy(LocalFsBackend);

    #[async_trait::async_trait]
    impl StorageBackend for RewritesSourceDuringCopy {
        fn name(&self) -> &'static str {
            "racing"
        }
        fn clone_box(&self) -> Box<dyn StorageBackend> {
            Box::new(self.clone())
        }
        async fn put(&self, key: &str, data: Bytes, meta: crate::ObjectMeta) -> Result<()> {
            self.0.put(key, data, meta).await
        }
        async fn get(&self, key: &str) -> Result<Bytes> {
            self.0.get(key).await
        }
        async fn exists(&self, key: &str) -> Result<bool> {
            self.0.exists(key).await
        }
        async fn probe(&self, key: &str) -> Result<ObjectProbe> {
            self.0.probe(key).await
        }
        async fn copy(&self, from: &str, to: &str) -> Result<()> {
            self.0.copy(from, to).await
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.0.delete(key).await
        }
        async fn list_page(
            &self,
            prefix: &str,
            cursor: Option<&str>,
            limit: u32,
        ) -> Result<ListPage> {
            self.0.list_page(prefix, cursor, limit).await
        }
        async fn get_stream(
            &self,
            key: &str,
            range: Option<crate::ByteRange>,
        ) -> Result<(
            ObjectProbe,
            std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        )> {
            self.0.get_stream(key, range).await
        }
        async fn put_stream(
            &self,
            key: &str,
            body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            meta: crate::ObjectMeta,
        ) -> Result<PutStreamResult> {
            let put = self.0.put_stream(key, body, meta).await?;
            self.0
                .put(
                    "a",
                    Bytes::from_static(b"rewritten"),
                    crate::ObjectMeta::default(),
                )
                .await?;
            Ok(put)
        }
    }

    #[tokio::test]
    async fn fallback_move_keeps_a_source_rewritten_during_the_copy() {
        let (_dir, local) = backend().await;
        put(&local, "a", b"1").await;
        let b = RewritesSourceDuringCopy(local);
        assert!(
            !b.supports_server_copy(),
            "the test needs the streamed copy"
        );
        let err = fallback_move(&b, "a", "b", &MoveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        assert_eq!(b.get("a").await.unwrap(), Bytes::from_static(b"rewritten"));
        assert_eq!(b.get("b").await.unwrap(), Bytes::from_static(b"1"));
    }

    #[tokio::test]
    async fn an_overlapping_prefix_move_changes_nothing() {
        let (_dir, b) = backend().await;
        put(&b, "a/0", b"zero").await;
        put(&b, "a/b/0", b"nested").await;
        for (from, to) in [("a/", "a\\b/"), ("a/b/", "a/"), ("a\\b\\", "a/")] {
            let err = fallback_move_prefix(&b, from, to, None, 10, &MoveOptions::default())
                .await
                .unwrap_err();
            assert!(
                matches!(err, StorageError::InvalidKey(_)),
                "{from} -> {to}: {err}"
            );
        }
        assert_eq!(b.get("a/0").await.unwrap(), Bytes::from_static(b"zero"));
        assert_eq!(b.get("a/b/0").await.unwrap(), Bytes::from_static(b"nested"));
    }

    #[test]
    fn same_object_prefers_etags_then_size_and_time() {
        let probe = |etag: Option<&str>, size: u64, at: u64| ObjectProbe {
            key: "k".into(),
            size,
            content_type: None,
            meta: crate::ObjectMeta::default(),
            etag: etag.map(str::to_string),
            modified_at_ms: Some(at),
            version_id: None,
            signature: None,
        };
        assert!(same_object(
            &probe(Some("e"), 1, 1),
            &probe(Some("e"), 2, 2)
        ));
        assert!(!same_object(
            &probe(Some("e"), 1, 1),
            &probe(Some("f"), 1, 1)
        ));
        assert!(same_object(&probe(None, 1, 1), &probe(None, 1, 1)));
        assert!(!same_object(&probe(None, 1, 1), &probe(Some("e"), 1, 2)));
        let blind = ObjectProbe {
            modified_at_ms: None,
            ..probe(None, 1, 1)
        };
        assert!(
            !same_object(&blind, &blind.clone()),
            "probes with no etag and no time cannot prove identity"
        );
    }

    #[tokio::test]
    async fn fallback_delete_many_reports_each_key() {
        let (_dir, b) = backend().await;
        put(&b, "a", b"1").await;
        let results = fallback_delete_many(&b, &["a".into(), "missing".into()]).await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.error.is_none()));
        assert!(!b.exists("a").await.unwrap());
    }

    #[tokio::test]
    async fn fallback_conditional_put_checks_first() {
        let (_dir, b) = backend().await;
        put(&b, "a", b"1").await;
        let body = || -> std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>> {
            Box::pin(std::io::Cursor::new(b"2".to_vec()))
        };
        let err = fallback_put_conditional(
            &b,
            "a",
            body(),
            crate::ObjectMeta::default(),
            &WriteCondition::IfNoneMatch,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
        let done = fallback_put_conditional(
            &b,
            "new",
            body(),
            crate::ObjectMeta::default(),
            &WriteCondition::IfNoneMatch,
        )
        .await
        .unwrap();
        assert!(!done.atomic);
    }
    #[tokio::test]
    async fn fallback_move_checks_the_source_etag_and_skips_self_moves() {
        let (_dir, b) = backend().await;
        put(&b, "a", b"1").await;
        let same = fallback_move(&b, "a", "a", &MoveOptions::default())
            .await
            .unwrap();
        assert!(same.atomic && !same.already_moved);
        let stale = MoveOptions {
            if_source_match: Some("not-the-etag".into()),
            ..MoveOptions::default()
        };
        let err = fallback_move(&b, "a", "b", &stale).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        let err = fallback_move(&b, "a", "a", &stale).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "{err}");
        let err = fallback_move(&b, "gone", "gone", &MoveOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)), "{err}");
        assert!(b.exists("a").await.unwrap());
    }

    #[tokio::test]
    async fn fallback_move_many_reports_each_pair() {
        let (_dir, b) = backend().await;
        put(&b, "a", b"1").await;
        let pairs = [
            ("a".to_string(), "b".to_string()),
            ("missing".to_string(), "c".to_string()),
        ];
        let results = fallback_move_many(&b, &pairs, &MoveOptions::default()).await;
        assert!(results[0].error.is_none());
        assert!(matches!(results[1].error, Some(StorageError::NotFound(_))));
        assert!(b.exists("b").await.unwrap());
    }

    #[tokio::test]
    async fn fallback_head_many_and_hinted_list_report_no_hints() {
        let (_dir, b) = backend().await;
        put(&b, "d/a", b"1").await;
        let probes = fallback_head_many(&b, &["d/a".into(), "d/missing".into()])
            .await
            .unwrap();
        assert_eq!(probes[0].as_ref().unwrap().size, 1);
        assert!(probes[1].is_none());
        let page = fallback_list_hinted(
            &b,
            &ListRequest {
                prefix: "d/".into(),
                limit: 10,
                with_signatures: true,
                modified_since_ms: Some(1),
                ..ListRequest::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.page.objects.len(), 1);
        assert!(!page.signatures_included && !page.modified_since_applied);
    }

    #[tokio::test]
    async fn fallback_conditional_put_if_match_compares_the_etag() {
        let (_dir, b) = backend().await;
        put(&b, "a", b"1").await;
        let body = || -> std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>> {
            Box::pin(std::io::Cursor::new(b"2".to_vec()))
        };
        let err = fallback_put_conditional(
            &b,
            "a",
            body(),
            crate::ObjectMeta::default(),
            &WriteCondition::IfMatch("not-the-etag".into()),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
        if let Some(etag) = b.head("a").await.unwrap().and_then(|p| p.etag) {
            fallback_put_conditional(
                &b,
                "a",
                body(),
                crate::ObjectMeta::default(),
                &WriteCondition::IfMatch(etag),
            )
            .await
            .unwrap();
            assert_eq!(b.get("a").await.unwrap().as_ref(), b"2");
        }
    }
}
