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
/// A `false` field means the host runs its fallback for that operation, which
/// is always correct but may be slower (a copy and a delete instead of a
/// rename) or not atomic (a conditional write checked before writing).
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
/// # Errors
///
/// Returns [`StorageError::NotFound`] when neither key exists,
/// [`StorageError::Conflict`] when a precondition in `options` fails, and
/// backend errors otherwise.
pub async fn fallback_move<B: StorageBackend + ?Sized>(
    backend: &B,
    from: &str,
    to: &str,
    options: &MoveOptions,
) -> Result<MoveResult> {
    if from == to {
        return Ok(MoveResult {
            atomic: true,
            already_moved: false,
        });
    }
    let Some(source) = backend.head(from).await? else {
        if backend.head(to).await?.is_some() {
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
    if options.no_overwrite && backend.head(to).await?.is_some() {
        return Err(StorageError::Conflict(format!("{to} already exists")));
    }
    if backend.supports_server_copy() {
        backend.copy(from, to).await?;
    } else {
        copy_streamed(backend, &source, to).await?;
    }
    backend.delete(from).await?;
    Ok(MoveResult {
        atomic: false,
        already_moved: false,
    })
}

/// Copies `source` to `to` through a streamed read and write.
///
/// # Errors
///
/// Returns backend read or write errors.
async fn copy_streamed<B: StorageBackend + ?Sized>(
    backend: &B,
    source: &ObjectProbe,
    to: &str,
) -> Result<()> {
    let (opened, body) = backend.get_stream(&source.key, None).await?;
    let mut meta = opened.meta.clone();
    meta.content_length = Some(opened.size);
    meta.commit_token = None;
    backend.put_stream(to, body, meta).await?;
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

/// Rejects a prefix move whose target lies inside its source (it would move
/// objects it just moved).
///
/// # Errors
///
/// Returns [`StorageError::InvalidKey`] for an empty source prefix or a
/// target under the source.
pub fn require_disjoint_prefixes(from_prefix: &str, to_prefix: &str) -> Result<()> {
    if from_prefix.is_empty() {
        return Err(StorageError::InvalidKey(
            "a prefix move needs a non-empty source prefix".into(),
        ));
    }
    if to_prefix.starts_with(from_prefix) {
        return Err(StorageError::InvalidKey(format!(
            "target prefix {to_prefix} lies inside source prefix {from_prefix}"
        )));
    }
    Ok(())
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
        assert!(require_disjoint_prefixes("a/", "b/").is_ok());
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
}
