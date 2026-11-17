//! Intent storage operations on a storage guest.
//!
//! [`crate::rpc_session::PluginStorage`] forwards a move, a batch delete or
//! probe, a conditional write, a hinted listing or a change-feed read to the
//! guest's `Destination` when the guest declared the matching `storage.*`
//! feature, and otherwise runs the host fallback from
//! [`bookclerk_storage::intent`]. Guest replies are validated by the ABI
//! client ([`bookclerk_plugin_sdk::DestinationClient`]) before they reach
//! this module.

use bookclerk_plugin_sdk::{
    ChangesPage, ChangesRequest, Destination, KeyOutcome, KeyPair, MoveManyPage, MoveManyRequest,
    MoveManyTarget, MoveOptions, MoveResult, ObjectMetadata, MAX_BATCH_KEYS,
};
use bookclerk_storage::{
    ChangeFeedPage, KeyResult, MovePrefixPage, ObjectChange, ObjectInfo, StorageCapabilities,
    StorageError,
};

/// One intent operation the vat forwards to the guest's `Destination`.
pub(crate) enum StorageOp {
    /// `Destination.move`.
    Move {
        /// Source key.
        from: String,
        /// Target key.
        to: String,
        /// Overwrite and source preconditions.
        options: MoveOptions,
    },
    /// `Destination.moveMany`.
    MoveMany(MoveManyRequest),
    /// `Destination.deleteMany`.
    DeleteMany(Vec<String>),
    /// `Destination.headMany`.
    HeadMany(Vec<String>),
    /// `Destination.changes`.
    Changes(ChangesRequest),
}

/// The guest's answer to a [`StorageOp`], already validated.
pub(crate) enum StorageOpReply {
    /// Answer to [`StorageOp::Move`].
    Move(MoveResult),
    /// Answer to [`StorageOp::MoveMany`].
    MoveMany(MoveManyPage),
    /// Answer to [`StorageOp::DeleteMany`].
    DeleteMany(Vec<KeyOutcome>),
    /// Answer to [`StorageOp::HeadMany`].
    HeadMany(Vec<Option<ObjectMetadata>>),
    /// Answer to [`StorageOp::Changes`].
    Changes(ChangesPage),
}

/// Runs `op` against the guest's `Destination` on the vat thread.
///
/// # Errors
///
/// Returns the guest's typed error, or a validation error when its reply
/// breaks the operation's rules.
pub(crate) async fn run_storage_op(
    dest: &dyn Destination,
    op: StorageOp,
) -> std::result::Result<StorageOpReply, bookclerk_plugin_sdk::PluginError> {
    match op {
        StorageOp::Move { from, to, options } => dest
            .move_object(&from, &to, options)
            .await
            .map(StorageOpReply::Move),
        StorageOp::MoveMany(request) => dest.move_many(request).await.map(StorageOpReply::MoveMany),
        StorageOp::DeleteMany(keys) => dest.delete_many(keys).await.map(StorageOpReply::DeleteMany),
        StorageOp::HeadMany(keys) => dest.head_many(keys).await.map(StorageOpReply::HeadMany),
        StorageOp::Changes(request) => dest.changes(request).await.map(StorageOpReply::Changes),
    }
}

/// The storage operations a guest declared, from its negotiated features.
///
/// # Arguments
///
/// * `has` - True when the guest accepted the named feature.
pub(crate) fn capabilities_from_features(has: impl Fn(&str) -> bool) -> StorageCapabilities {
    StorageCapabilities {
        move_object: has(bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE),
        move_many: has(bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE_MANY),
        delete_many: has(bookclerk_plugin_sdk::FEATURE_STORAGE_DELETE_MANY),
        head_many: has(bookclerk_plugin_sdk::FEATURE_STORAGE_HEAD_MANY),
        conditional_write: has(bookclerk_plugin_sdk::FEATURE_STORAGE_CONDITIONAL_WRITE),
        changes: has(bookclerk_plugin_sdk::FEATURE_STORAGE_CHANGES),
    }
}

/// Maps a guest error carried inside a successful batch reply onto a
/// [`StorageError`], by its wire code.
pub(crate) fn storage_error_from_abi(err: &bookclerk_plugin_sdk::PluginError) -> StorageError {
    let message = err.message.clone();
    match err.wire_str() {
        "not_found" => StorageError::NotFound(message),
        "conflict" => StorageError::Conflict(message),
        "payload_too_large" => StorageError::PayloadTooLarge(message),
        "invalid_cursor" => StorageError::InvalidCursor(message),
        "invalid_params" => StorageError::InvalidKey(message),
        code => StorageError::Other(anyhow::anyhow!("{code}: {message}")),
    }
}

/// Converts guest outcomes into storage outcomes.
pub(crate) fn key_results(outcomes: Vec<KeyOutcome>) -> Vec<KeyResult> {
    outcomes
        .into_iter()
        .map(|o| KeyResult {
            error: o.error.as_ref().map(storage_error_from_abi),
            key: o.key,
        })
        .collect()
}

/// Splits `items` into wire batches of at most [`MAX_BATCH_KEYS`].
pub(crate) fn batches<T>(items: &[T]) -> impl Iterator<Item = &[T]> {
    // MAX_BATCH_KEYS is a small schema constant (256); it always fits usize.
    let size = usize::try_from(MAX_BATCH_KEYS).unwrap_or(256).max(1);
    items.chunks(size)
}

/// The `moveMany` request for one batch of explicit pairs.
pub(crate) fn pairs_request(pairs: &[(String, String)], options: &MoveOptions) -> MoveManyRequest {
    MoveManyRequest {
        target: MoveManyTarget::Pairs(
            pairs
                .iter()
                .map(|(from, to)| KeyPair {
                    from: from.clone(),
                    to: to.clone(),
                })
                .collect(),
        ),
        cursor: None,
        limit: 0,
        options: MoveOptions {
            if_source_match: None,
            ..options.clone()
        },
    }
}

/// The `moveMany` request for one page of a prefix move.
pub(crate) fn prefix_request(
    from_prefix: &str,
    to_prefix: &str,
    cursor: Option<&str>,
    limit: u32,
    options: &MoveOptions,
) -> MoveManyRequest {
    MoveManyRequest {
        target: MoveManyTarget::Prefix(KeyPair {
            from: from_prefix.to_string(),
            to: to_prefix.to_string(),
        }),
        cursor: cursor.map(str::to_string),
        limit: limit.min(MAX_BATCH_KEYS),
        options: MoveOptions {
            if_source_match: None,
            ..options.clone()
        },
    }
}

/// Converts a guest prefix page into a storage page.
pub(crate) fn prefix_page(page: MoveManyPage) -> MovePrefixPage {
    MovePrefixPage {
        results: key_results(page.results),
        next_cursor: page.next_cursor,
        atomic: page.atomic,
        objects_moved: page.objects_moved,
    }
}

/// Converts a guest listing entry into a storage listing entry.
pub(crate) fn object_info(o: bookclerk_plugin_sdk::ObjectInfo) -> ObjectInfo {
    ObjectInfo {
        key: o.key,
        size: o.size,
        etag: o.etag,
        modified_at_ms: o.modified_at_ms,
        version_id: o.version_id,
        signature: o.signature,
    }
}

/// Converts a guest change-feed page into a storage page.
pub(crate) fn change_feed_page(page: ChangesPage) -> ChangeFeedPage {
    ChangeFeedPage {
        changes: page
            .changes
            .into_iter()
            .map(|c| ObjectChange {
                key: c.key,
                kind: c.kind,
                info: c.info.map(object_info),
            })
            .collect(),
        cursor: page.cursor,
        more: page.more,
        complete: page.complete,
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn batches_respect_the_wire_cap() {
        let keys: Vec<u32> = (0..600).collect();
        let sizes: Vec<usize> = batches(&keys).map(<[u32]>::len).collect();
        assert_eq!(sizes, vec![256, 256, 88]);
    }

    #[test]
    fn guest_codes_map_to_storage_errors() {
        let conflict = bookclerk_plugin_sdk::PluginError::conflict("exists");
        assert!(matches!(
            storage_error_from_abi(&conflict),
            StorageError::Conflict(_)
        ));
        let missing = bookclerk_plugin_sdk::PluginError::new(
            bookclerk_plugin_sdk::PluginErrorCode::NotFound,
            "gone",
        );
        assert!(matches!(
            storage_error_from_abi(&missing),
            StorageError::NotFound(_)
        ));
    }

    /// A guest answering every intent operation with a fixed reply.
    struct Echo;

    #[async_trait::async_trait(?Send)]
    impl Destination for Echo {
        async fn head(
            &self,
            _key: &str,
        ) -> std::result::Result<Option<ObjectMetadata>, bookclerk_plugin_sdk::PluginError>
        {
            Ok(None)
        }
        async fn list(
            &self,
            _options: bookclerk_plugin_sdk::ListOptions,
        ) -> std::result::Result<bookclerk_plugin_sdk::ListPage, bookclerk_plugin_sdk::PluginError>
        {
            Ok(bookclerk_plugin_sdk::ListPage::default())
        }
        async fn get(
            &self,
            _key: &str,
            _range: Option<bookclerk_plugin_sdk::ByteRange>,
        ) -> std::result::Result<bookclerk_plugin_sdk::ReadResult, bookclerk_plugin_sdk::PluginError>
        {
            Err(bookclerk_plugin_sdk::PluginError::unsupported("get"))
        }
        async fn put(
            &self,
            _key: &str,
            _body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
            _options: bookclerk_plugin_sdk::WriteOptions,
        ) -> std::result::Result<bookclerk_plugin_sdk::PutResult, bookclerk_plugin_sdk::PluginError>
        {
            Err(bookclerk_plugin_sdk::PluginError::unsupported("put"))
        }
        async fn copy(
            &self,
            _from: &str,
            _to: &str,
        ) -> std::result::Result<bookclerk_plugin_sdk::CopyResult, bookclerk_plugin_sdk::PluginError>
        {
            Err(bookclerk_plugin_sdk::PluginError::unsupported("copy"))
        }
        async fn delete(
            &self,
            _key: &str,
        ) -> std::result::Result<(), bookclerk_plugin_sdk::PluginError> {
            Ok(())
        }
        async fn move_object(
            &self,
            _from: &str,
            _to: &str,
            options: MoveOptions,
        ) -> std::result::Result<MoveResult, bookclerk_plugin_sdk::PluginError> {
            Ok(MoveResult {
                atomic: options.no_overwrite,
                already_moved: false,
            })
        }
        async fn move_many(
            &self,
            request: MoveManyRequest,
        ) -> std::result::Result<MoveManyPage, bookclerk_plugin_sdk::PluginError> {
            Ok(MoveManyPage {
                results: vec![KeyOutcome::ok("a/1")],
                next_cursor: request.cursor.map(|c| format!("{c}+")),
                atomic: true,
                objects_moved: 1,
            })
        }
        async fn delete_many(
            &self,
            keys: Vec<String>,
        ) -> std::result::Result<Vec<KeyOutcome>, bookclerk_plugin_sdk::PluginError> {
            Ok(keys.into_iter().map(KeyOutcome::ok).collect())
        }
        async fn head_many(
            &self,
            keys: Vec<String>,
        ) -> std::result::Result<Vec<Option<ObjectMetadata>>, bookclerk_plugin_sdk::PluginError>
        {
            Ok(keys.into_iter().map(|_| None).collect())
        }
        async fn changes(
            &self,
            _request: ChangesRequest,
        ) -> std::result::Result<ChangesPage, bookclerk_plugin_sdk::PluginError> {
            Ok(ChangesPage {
                changes: vec![bookclerk_plugin_sdk::ObjectChange {
                    key: "a/1".into(),
                    kind: bookclerk_plugin_sdk::ObjectChangeKind::Upserted,
                    info: Some(bookclerk_plugin_sdk::ObjectInfo {
                        key: "a/1".into(),
                        size: 4,
                        etag: Some("e".into()),
                        ..bookclerk_plugin_sdk::ObjectInfo::default()
                    }),
                }],
                cursor: "c2".into(),
                more: false,
                complete: true,
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_op_reaches_the_guest_and_comes_back_typed() {
        let dest = Echo;
        let options = MoveOptions {
            no_overwrite: true,
            if_source_match: Some("e".into()),
        };
        let moved = run_storage_op(
            &dest,
            StorageOp::Move {
                from: "a".into(),
                to: "b".into(),
                options: options.clone(),
            },
        )
        .await
        .expect("move");
        assert!(matches!(moved, StorageOpReply::Move(m) if m.atomic));

        let pairs = [("a/1".to_string(), "b/1".to_string())];
        let request = pairs_request(&pairs, &options);
        assert!(
            request.options.if_source_match.is_none(),
            "batch moves drop ifSourceMatch"
        );
        assert!(request.options.no_overwrite);
        let StorageOpReply::MoveMany(page) = run_storage_op(&dest, StorageOp::MoveMany(request))
            .await
            .expect("moveMany")
        else {
            panic!("moveMany reply");
        };
        assert_eq!(key_results(page.results)[0].key, "a/1");

        let request = prefix_request("a/", "b/", Some("c1"), MAX_BATCH_KEYS + 9, &options);
        assert_eq!(request.limit, MAX_BATCH_KEYS);
        assert!(request.options.if_source_match.is_none());
        let StorageOpReply::MoveMany(page) = run_storage_op(&dest, StorageOp::MoveMany(request))
            .await
            .expect("prefix")
        else {
            panic!("prefix reply");
        };
        let page = prefix_page(page);
        assert_eq!(page.next_cursor.as_deref(), Some("c1+"));
        assert!(page.atomic);
        assert_eq!(page.objects_moved, 1);

        let StorageOpReply::DeleteMany(outcomes) =
            run_storage_op(&dest, StorageOp::DeleteMany(vec!["x".into()]))
                .await
                .expect("deleteMany")
        else {
            panic!("deleteMany reply");
        };
        assert!(key_results(outcomes)[0].error.is_none());

        let StorageOpReply::HeadMany(probes) =
            run_storage_op(&dest, StorageOp::HeadMany(vec!["x".into()]))
                .await
                .expect("headMany")
        else {
            panic!("headMany reply");
        };
        assert!(probes[0].is_none());

        let StorageOpReply::Changes(feed) =
            run_storage_op(&dest, StorageOp::Changes(ChangesRequest::default()))
                .await
                .expect("changes")
        else {
            panic!("changes reply");
        };
        let feed = change_feed_page(feed);
        assert_eq!(feed.cursor, "c2");
        assert!(feed.complete && !feed.more);
        let info = feed.changes[0].info.as_ref().expect("info");
        assert_eq!(info.etag.as_deref(), Some("e"));
        assert_eq!(info.size, 4);
    }

    #[test]
    fn failed_outcomes_keep_their_code() {
        let results = key_results(vec![
            KeyOutcome::ok("a"),
            KeyOutcome::failed("b", bookclerk_plugin_sdk::PluginError::conflict("busy")),
        ]);
        assert!(results[0].error.is_none());
        assert!(matches!(results[1].error, Some(StorageError::Conflict(_))));
    }

    #[test]
    fn every_known_code_has_a_storage_error() {
        use bookclerk_plugin_sdk::{PluginError, PluginErrorCode};
        let cases = [
            (PluginErrorCode::PayloadTooLarge, "payload_too_large"),
            (PluginErrorCode::InvalidCursor, "invalid_cursor"),
            (PluginErrorCode::InvalidParams, "invalid_params"),
            (PluginErrorCode::Internal, "internal"),
        ];
        for (code, wire) in cases {
            let mapped = storage_error_from_abi(&PluginError::new(code, "x"));
            let ok = match wire {
                "payload_too_large" => matches!(mapped, StorageError::PayloadTooLarge(_)),
                "invalid_cursor" => matches!(mapped, StorageError::InvalidCursor(_)),
                "invalid_params" => matches!(mapped, StorageError::InvalidKey(_)),
                _ => matches!(mapped, StorageError::Other(_)),
            };
            assert!(ok, "{wire}");
        }
    }

    #[test]
    fn features_become_capabilities() {
        let caps = capabilities_from_features(|f| f == bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE);
        assert!(caps.move_object);
        assert!(!caps.delete_many);
    }
}
