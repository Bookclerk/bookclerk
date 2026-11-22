//! Serves a [`StorageBackend`]'s intent operations through a storage guest's
//! `Destination` capability.
//!
//! The `local` and `s3` destination plugins each wrap one backend. The
//! helpers here turn the plugin ABI's requests into backend calls and the
//! backend's results into ABI replies, so both guests answer the intent
//! operations the same way and declare exactly the ones their backend does
//! natively (see `docs/plugins/abi.md#intent-operations`).

use bookclerk_plugin_sdk::{
    ChangesPage, ChangesRequest, KeyOutcome, ListOptions, ListPage, MoveManyPage, MoveManyRequest,
    MoveManyTarget, ObjectChange as AbiObjectChange, ObjectInfo as AbiObjectInfo, ObjectMetadata,
    PluginError, WriteOptions, FEATURE_STORAGE_CHANGES, FEATURE_STORAGE_CONDITIONAL_WRITE,
    FEATURE_STORAGE_DELETE_MANY, FEATURE_STORAGE_HEAD_MANY, FEATURE_STORAGE_MOVE,
    FEATURE_STORAGE_MOVE_MANY, FEATURE_STORAGE_TRANSLATE_KEYS, MAX_BATCH_KEYS,
};

use crate::error::StorageError;
use crate::intent::{
    KeyResult, ListRequest, MoveOptions, MoveResult, StorageCapabilities, WriteCondition,
};
use crate::traits::{ObjectInfo, ObjectProbe, StorageBackend};

/// Result alias for guest replies.
type GuestResult<T> = std::result::Result<T, PluginError>;

/// Maps a storage error onto the plugin error a guest answers with.
///
/// A failed precondition is `conflict`, a malformed key `invalidParams`, and
/// a cursor the backend no longer accepts `invalidCursor`, so the host can
/// tell them apart from backend failures.
#[must_use]
pub fn plugin_error(err: StorageError) -> PluginError {
    match err {
        StorageError::NotFound(key) => PluginError::not_found(key),
        StorageError::InvalidKey(msg) => PluginError::invalid_params(msg),
        StorageError::PayloadTooLarge(msg) => PluginError::payload_too_large(msg),
        StorageError::InvalidCursor(msg) => PluginError::invalid_cursor(msg),
        StorageError::Conflict(msg) => PluginError::conflict(msg),
        StorageError::Integrity(msg) => PluginError::internal(format!("integrity: {msg}")),
        StorageError::Io(err) => PluginError::internal(err.to_string()),
        StorageError::S3(msg) | StorageError::Remote(msg) => PluginError::internal(msg),
        StorageError::Other(err) => PluginError::internal(err.to_string()),
    }
}

/// The `storage.*` features to accept in `describe` for a backend with
/// `capabilities`: one per operation the backend does natively.
#[must_use]
pub fn storage_features(capabilities: StorageCapabilities) -> Vec<String> {
    let StorageCapabilities {
        move_object,
        move_many,
        delete_many,
        head_many,
        conditional_write,
        changes,
        translate_keys,
    } = capabilities;
    [
        (move_object, FEATURE_STORAGE_MOVE),
        (move_many, FEATURE_STORAGE_MOVE_MANY),
        (delete_many, FEATURE_STORAGE_DELETE_MANY),
        (head_many, FEATURE_STORAGE_HEAD_MANY),
        (conditional_write, FEATURE_STORAGE_CONDITIONAL_WRITE),
        (changes, FEATURE_STORAGE_CHANGES),
        (translate_keys, FEATURE_STORAGE_TRANSLATE_KEYS),
    ]
    .into_iter()
    .filter(|(native, _)| *native)
    .map(|(_, feature)| feature.to_string())
    .collect()
}

/// The ABI metadata for a backend probe.
#[must_use]
pub fn object_metadata(probe: ObjectProbe) -> ObjectMetadata {
    let sha256 = probe.meta.sha256_hex.as_deref().and_then(|hex| {
        crate::parse_sha256_hex(hex)
            .ok()
            .map(|bytes| bytes.to_vec())
    });
    ObjectMetadata {
        key: probe.key,
        size: probe.size,
        content_type: probe.content_type.or(probe.meta.content_type),
        etag: probe.etag,
        sha256,
        modified_at_ms: probe.modified_at_ms,
        version_id: probe.version_id,
        signature: probe.signature,
    }
}

/// The ABI listing entry for a backend listing entry.
fn object_info(info: ObjectInfo) -> AbiObjectInfo {
    AbiObjectInfo {
        key: info.key,
        size: info.size,
        etag: info.etag,
        modified_at_ms: info.modified_at_ms,
        version_id: info.version_id,
        signature: info.signature,
    }
}

/// The ABI outcome for one key of a backend batch.
fn key_outcome(result: KeyResult) -> KeyOutcome {
    match result.error {
        None => KeyOutcome::ok(result.key),
        Some(err) => KeyOutcome::failed(result.key, plugin_error(err)),
    }
}

/// The write precondition in `options`, if any.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when both `ifMatch` and
/// `ifNoneMatch` are set, or when a precondition is combined with
/// `stageOnly` (a staged object is published by `commit`, which carries no
/// precondition).
pub fn write_condition(options: &WriteOptions) -> GuestResult<Option<WriteCondition>> {
    let condition = match (&options.if_match, options.if_none_match) {
        (Some(_), true) => {
            return Err(PluginError::invalid_params(
                "a write takes ifMatch or ifNoneMatch, not both",
            ));
        }
        (Some(etag), false) => Some(WriteCondition::IfMatch(etag.clone())),
        (None, true) => Some(WriteCondition::IfNoneMatch),
        (None, false) => None,
    };
    if condition.is_some() && options.stage_only {
        return Err(PluginError::invalid_params(
            "a conditional write cannot be staged",
        ));
    }
    Ok(condition)
}

/// One listing page, honouring the hints the backend supports.
///
/// # Errors
///
/// Returns the backend's listing error as a plugin error.
pub async fn list<B: StorageBackend + ?Sized>(
    backend: &B,
    options: ListOptions,
) -> GuestResult<ListPage> {
    let hinted = backend
        .list_hinted(&ListRequest {
            prefix: options.prefix,
            cursor: options.cursor,
            limit: options.limit,
            with_signatures: options.with_signatures,
            modified_since_ms: options.modified_since_ms,
        })
        .await
        .map_err(plugin_error)?;
    Ok(ListPage {
        objects: hinted.page.objects.into_iter().map(object_info).collect(),
        next_cursor: hinted.page.next_cursor,
        signatures_included: hinted.signatures_included,
        modified_since_applied: hinted.modified_since_applied,
    })
}

/// Moves one object.
///
/// # Errors
///
/// Returns the backend's error as a plugin error (`conflict` for a failed
/// precondition).
pub async fn move_object<B: StorageBackend + ?Sized>(
    backend: &B,
    from: &str,
    to: &str,
    options: MoveOptions,
) -> GuestResult<MoveResult> {
    backend
        .move_object(from, to, &options)
        .await
        .map_err(plugin_error)
}

/// Moves explicit pairs, or one page of a prefix.
///
/// Pairs ignore `ifSourceMatch`, which names one object's etag, the same as
/// the host's fallback does.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] for more than
/// [`MAX_BATCH_KEYS`] pairs, and prefix errors (overlapping prefixes, a
/// stale cursor, listing failures) as plugin errors. Per-object failures
/// are in the page.
pub async fn move_many<B: StorageBackend + ?Sized>(
    backend: &B,
    request: MoveManyRequest,
) -> GuestResult<MoveManyPage> {
    let limit = request.limit.clamp(1, MAX_BATCH_KEYS);
    match request.target {
        MoveManyTarget::Pairs(pairs) => {
            if pairs.len() > usize::try_from(MAX_BATCH_KEYS).unwrap_or(usize::MAX) {
                return Err(PluginError::invalid_params(format!(
                    "moveMany takes at most {MAX_BATCH_KEYS} pairs"
                )));
            }
            let options = MoveOptions {
                if_source_match: None,
                ..request.options
            };
            let mut page = MoveManyPage {
                results: Vec::with_capacity(pairs.len()),
                next_cursor: None,
                atomic: true,
                objects_moved: 0,
            };
            for pair in pairs {
                match backend.move_object(&pair.from, &pair.to, &options).await {
                    Ok(moved) => {
                        page.atomic &= moved.atomic;
                        page.objects_moved = page.objects_moved.saturating_add(1);
                        page.results.push(KeyOutcome::ok(pair.from));
                    }
                    Err(err) => page
                        .results
                        .push(KeyOutcome::failed(pair.from, plugin_error(err))),
                }
            }
            Ok(page)
        }
        MoveManyTarget::Prefix(prefixes) => {
            let page = backend
                .move_prefix(
                    &prefixes.from,
                    &prefixes.to,
                    request.cursor.as_deref(),
                    limit,
                    &request.options,
                )
                .await
                .map_err(plugin_error)?;
            Ok(MoveManyPage {
                results: page.results.into_iter().map(key_outcome).collect(),
                next_cursor: page.next_cursor,
                atomic: page.atomic,
                objects_moved: page.objects_moved,
            })
        }
    }
}

/// Deletes up to [`MAX_BATCH_KEYS`] keys, one outcome per key in order.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] for too many keys, and a backend
/// error that failed the whole batch.
pub async fn delete_many<B: StorageBackend + ?Sized>(
    backend: &B,
    keys: Vec<String>,
) -> GuestResult<Vec<KeyOutcome>> {
    require_batch(&keys, "deleteMany")?;
    let results = backend.delete_many(&keys).await.map_err(plugin_error)?;
    Ok(results.into_iter().map(key_outcome).collect())
}

/// Probes up to [`MAX_BATCH_KEYS`] keys, one entry per key in order.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] for too many keys, and the first
/// probe failure other than absence.
pub async fn head_many<B: StorageBackend + ?Sized>(
    backend: &B,
    keys: Vec<String>,
) -> GuestResult<Vec<Option<ObjectMetadata>>> {
    require_batch(&keys, "headMany")?;
    let probes = backend.head_many(&keys).await.map_err(plugin_error)?;
    Ok(probes
        .into_iter()
        .map(|probe| probe.map(object_metadata))
        .collect())
}

/// Translates up to [`MAX_BATCH_KEYS`] host keys into the keys the backend
/// stores them under, one per key in order.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] for too many keys or a key that
/// breaks the host key syntax ([`crate::key_syntax::HostKey`]), and the
/// backend's failure.
pub async fn translate_keys<B: StorageBackend + ?Sized>(
    backend: &B,
    keys: Vec<String>,
) -> GuestResult<Vec<String>> {
    require_batch(&keys, "translateKeys")?;
    let parsed = keys
        .iter()
        .map(|key| crate::key_syntax::HostKey::parse(key))
        .collect::<crate::Result<Vec<_>>>()
        .map_err(plugin_error)?;
    backend.translate_keys(&parsed).await.map_err(plugin_error)
}

/// Serves `changes` from the backend's change feed.
///
/// A backend without a feed (for the `local` plugin, an instance with
/// `watch` off) answers with an empty page that says changes may have been
/// missed, so the host lists instead.
///
/// # Errors
///
/// Returns `invalidCursor` for a cursor the feed does not accept, and the
/// feed's other failures.
pub async fn changes<B: StorageBackend + ?Sized>(
    backend: &B,
    request: ChangesRequest,
) -> GuestResult<ChangesPage> {
    let page = backend
        .changes(request.cursor.as_deref(), request.limit)
        .await
        .map_err(plugin_error)?;
    let Some(page) = page else {
        return Ok(ChangesPage {
            changes: Vec::new(),
            cursor: NO_FEED_CURSOR.into(),
            more: false,
            complete: false,
            live_watch: false,
        });
    };
    Ok(ChangesPage {
        changes: page
            .changes
            .into_iter()
            .map(|change| AbiObjectChange {
                key: change.key,
                kind: change.kind,
                info: change.info.map(object_info),
            })
            .collect(),
        cursor: page.cursor,
        more: page.more,
        complete: page.complete,
        live_watch: page.live_watch,
    })
}

/// The cursor of a page from a backend with no change feed; the host only
/// hands it back, and it always reads as incomplete.
const NO_FEED_CURSOR: &str = "none";

/// Refuses a batch over [`MAX_BATCH_KEYS`] keys.
fn require_batch(keys: &[String], op: &str) -> GuestResult<()> {
    if keys.len() > usize::try_from(MAX_BATCH_KEYS).unwrap_or(usize::MAX) {
        return Err(PluginError::invalid_params(format!(
            "{op} takes at most {MAX_BATCH_KEYS} keys"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::{LocalFsBackend, ObjectMeta};
    use bookclerk_plugin_sdk::{KeyPair, PluginErrorCode};
    use bytes::Bytes;

    fn local() -> (tempfile::TempDir, LocalFsBackend) {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalFsBackend::new(dir.path().to_path_buf()).unwrap();
        (dir, backend)
    }

    async fn put(b: &LocalFsBackend, key: &str) {
        b.put(key, Bytes::from_static(b"x"), ObjectMeta::default())
            .await
            .unwrap();
    }

    #[test]
    fn a_failed_precondition_is_a_conflict_and_a_bad_key_invalid_params() {
        assert_eq!(
            plugin_error(StorageError::Conflict("c".into())).code,
            PluginErrorCode::Conflict
        );
        assert_eq!(
            plugin_error(StorageError::InvalidKey("k".into())).code,
            PluginErrorCode::InvalidParams
        );
        assert_eq!(
            plugin_error(StorageError::InvalidCursor("c".into())).code,
            PluginErrorCode::InvalidCursor
        );
        assert_eq!(
            plugin_error(StorageError::NotFound("k".into())).code,
            PluginErrorCode::NotFound
        );
    }

    #[test]
    fn features_name_exactly_the_native_operations() {
        assert!(storage_features(StorageCapabilities::default()).is_empty());
        let local = storage_features(LocalFsBackend::CAPABILITIES);
        assert_eq!(
            local,
            [
                FEATURE_STORAGE_MOVE,
                FEATURE_STORAGE_MOVE_MANY,
                FEATURE_STORAGE_DELETE_MANY,
                FEATURE_STORAGE_HEAD_MANY,
                FEATURE_STORAGE_CONDITIONAL_WRITE,
                FEATURE_STORAGE_TRANSLATE_KEYS,
            ]
        );
    }

    #[test]
    fn write_conditions_are_read_and_contradictions_refused() {
        let mut options = WriteOptions::default();
        assert_eq!(write_condition(&options).unwrap(), None);
        options.if_none_match = true;
        assert_eq!(
            write_condition(&options).unwrap(),
            Some(WriteCondition::IfNoneMatch)
        );
        options.if_match = Some("e".into());
        assert!(write_condition(&options).is_err());
        options.if_none_match = false;
        assert_eq!(
            write_condition(&options).unwrap(),
            Some(WriteCondition::IfMatch("e".into()))
        );
        options.stage_only = true;
        assert!(write_condition(&options).is_err());
    }

    #[tokio::test]
    async fn pairs_answer_per_key_and_report_atomic_renames() {
        let (_dir, b) = local();
        put(&b, "a").await;
        let page = move_many(
            &b,
            MoveManyRequest {
                target: MoveManyTarget::Pairs(vec![
                    KeyPair {
                        from: "a".into(),
                        to: "b".into(),
                    },
                    KeyPair {
                        from: "missing".into(),
                        to: "c".into(),
                    },
                ]),
                cursor: None,
                limit: 0,
                options: MoveOptions {
                    if_source_match: Some("ignored for pairs".into()),
                    ..MoveOptions::default()
                },
            },
        )
        .await
        .unwrap();
        assert!(page.atomic);
        assert_eq!(page.objects_moved, 1);
        assert_eq!(page.results[0].key, "a");
        assert!(page.results[0].error.is_none());
        assert_eq!(
            page.results[1].error.as_ref().map(|e| e.code),
            Some(PluginErrorCode::NotFound)
        );
    }

    #[tokio::test]
    async fn a_prefix_page_moves_and_batches_over_the_limit_are_refused() {
        let (_dir, b) = local();
        put(&b, "from/1").await;
        put(&b, "from/2").await;
        let page = move_many(
            &b,
            MoveManyRequest {
                target: MoveManyTarget::Prefix(KeyPair {
                    from: "from/".into(),
                    to: "to/".into(),
                }),
                cursor: None,
                limit: 10,
                options: MoveOptions::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(page.objects_moved, 2);
        assert!(page.atomic && page.next_cursor.is_none());

        let too_many: Vec<String> = (0..=MAX_BATCH_KEYS).map(|i| i.to_string()).collect();
        assert!(delete_many(&b, too_many.clone()).await.is_err());
        assert!(head_many(&b, too_many.clone()).await.is_err());
        let pairs = too_many
            .into_iter()
            .map(|k| KeyPair {
                from: k.clone(),
                to: k,
            })
            .collect();
        let err = move_many(
            &b,
            MoveManyRequest {
                target: MoveManyTarget::Pairs(pairs),
                cursor: None,
                limit: 0,
                options: MoveOptions::default(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn batch_deletes_and_probes_answer_in_order() {
        let (_dir, b) = local();
        put(&b, "x").await;
        let probes = head_many(&b, vec!["x".into(), "y".into()]).await.unwrap();
        assert_eq!(probes[0].as_ref().map(|m| m.key.as_str()), Some("x"));
        assert!(probes[0].as_ref().and_then(|m| m.etag.as_ref()).is_some());
        assert!(probes[1].is_none());
        let outcomes = delete_many(&b, vec!["x".into(), "y".into()]).await.unwrap();
        assert!(outcomes.iter().all(|o| o.error.is_none()));
        assert!(!b.exists("x").await.unwrap());
    }

    #[tokio::test]
    async fn a_listing_passes_the_hints_through() {
        let (_dir, b) = local();
        put(&b, "p/a").await;
        let page = list(
            &b,
            ListOptions {
                prefix: "p/".into(),
                limit: 10,
                with_signatures: true,
                ..ListOptions::default()
            },
        )
        .await
        .unwrap();
        assert!(page.signatures_included && !page.modified_since_applied);
        assert!(page.objects[0].signature.is_some());
    }
}
