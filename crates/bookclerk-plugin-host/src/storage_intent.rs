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

    #[test]
    fn features_become_capabilities() {
        let caps = capabilities_from_features(|f| f == bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE);
        assert!(caps.move_object);
        assert!(!caps.delete_many);
    }
}
