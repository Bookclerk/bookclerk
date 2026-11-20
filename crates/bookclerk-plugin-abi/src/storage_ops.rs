//! Intent-based storage operations on `Destination`: moves, batch deletes
//! and probes, and change feeds.
//!
//! The host states what it wants and the backend chooses how: a local disk
//! renames, an object store copies and deletes. Each operation is optional
//! and negotiated as a named `rpcFeatures` entry (`storage.move`,
//! `storage.moveMany`, `storage.deleteMany`, `storage.headMany`,
//! `storage.changes`, `storage.translateKeys`, and `storage.conditionalWrite` for
//! [`crate::WriteOptions::if_match`]); the host keeps one fallback per
//! operation built from the required primitives.
//!
//! Replies come from a jailed guest, so the host validates every one before
//! using it ([`validate_key_outcomes`], [`validate_move_many_page`],
//! [`validate_head_many`], [`validate_changes_page`],
//! [`validate_translated_keys`]). The checks are
//! sans-IO, and the `storage_reply` fuzz target drives them through
//! [`decode_storage_reply_bytes`].

use serde::{Deserialize, Serialize};

use crate::plugin_capnp::{
    changes_page, changes_request, head_many_result, head_ok, key_outcome, key_pair,
    move_many_page, move_many_request, move_many_target, move_options, move_result, object_change,
    object_info, plugin_error, translate_keys_result, ObjectChangeKind as WireChangeKind,
};
use crate::rpc::{non_empty, non_zero, read_error, strict_text, write_error};
use crate::rpc_types::{ObjectInfo, ObjectMetadata};
use crate::{PluginError, Result};

/// Maximum keys or key pairs in one `moveMany`, `deleteMany` or `headMany`
/// call, and maximum outcomes in one reply.
pub const MAX_BATCH_KEYS: u32 = crate::plugin_capnp::MAX_BATCH_KEYS;

/// Longest key a guest may translate a host key into, in UTF-8 bytes.
/// Generous next to any store's own limit (S3 keys stop at 1024 bytes,
/// file-system paths usually at 4096).
pub const MAX_TRANSLATED_KEY_BYTES: usize = 4096;

/// Maximum changes in one `Destination.changes` page.
pub const MAX_CHANGES_PAGE: u32 = crate::plugin_capnp::MAX_CHANGES_PAGE;

/// Maximum bytes of a `Destination.changes` cursor.
pub const MAX_CHANGES_CURSOR_BYTES: u32 = crate::plugin_capnp::MAX_CHANGES_CURSOR_BYTES;

/// Options for [`crate::Destination::move_object`] and
/// [`crate::Destination::move_many`]. Fields are only ever appended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveOptions {
    /// Fail with `conflict` instead of replacing an object at the target.
    #[serde(default)]
    pub no_overwrite: bool,
    /// Single moves only: move only while the source's etag equals this
    /// value, else fail with `conflict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_source_match: Option<String>,
}

/// How a single move was done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveResult {
    /// True when the backend moved atomically (a rename): no reader saw the
    /// object at both keys or at neither.
    pub atomic: bool,
    /// True when the source was already gone and the target present, as
    /// when a move is repeated after a restart. The move counts as done.
    pub already_moved: bool,
}

/// One source and target key (or prefix).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPair {
    /// Source key or prefix.
    pub from: String,
    /// Target key or prefix.
    pub to: String,
}

/// What a [`MoveManyRequest`] moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum MoveManyTarget {
    /// Explicit, independent moves; at most [`MAX_BATCH_KEYS`].
    Pairs(Vec<KeyPair>),
    /// Every object under `from` moves under `to`, keeping the rest of its
    /// key. Paged through [`MoveManyRequest::cursor`].
    Prefix(KeyPair),
}

/// Argument of [`crate::Destination::move_many`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveManyRequest {
    /// Explicit pairs or a prefix.
    pub target: MoveManyTarget,
    /// Prefix form: [`MoveManyPage::next_cursor`] from the previous call;
    /// `None` starts. Ignored for pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Prefix form: objects to move in this call, clamped to
    /// [`MAX_BATCH_KEYS`]; zero means guest default.
    #[serde(default)]
    pub limit: u32,
    /// Overwrite rule applied to every move (`if_source_match` is ignored).
    #[serde(default)]
    pub options: MoveOptions,
}

/// Outcome for one key of a batch call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyOutcome {
    /// The key this outcome is for (the source key for moves).
    pub key: String,
    /// The failure for this key; `None` when it succeeded.
    pub error: Option<PluginError>,
}

impl KeyOutcome {
    /// A successful outcome for `key`.
    #[must_use]
    pub fn ok(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            error: None,
        }
    }

    /// A failed outcome for `key`.
    #[must_use]
    pub fn failed(key: impl Into<String>, error: PluginError) -> Self {
        Self {
            key: key.into(),
            error: Some(error),
        }
    }
}

/// One page of [`crate::Destination::move_many`] progress.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveManyPage {
    /// Outcomes in request order. A prefix move done in one step may report
    /// a single outcome keyed by the source prefix.
    pub results: Vec<KeyOutcome>,
    /// Prefix form: cursor for the next call; `None` when every object moved.
    pub next_cursor: Option<String>,
    /// True when every move in this page was atomic.
    pub atomic: bool,
    /// Objects moved in this page.
    pub objects_moved: u64,
}

/// Argument of [`crate::Destination::changes`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesRequest {
    /// [`ChangesPage::cursor`] from the previous call; `None` asks for the
    /// current position (a page with a cursor and no changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Requested changes per page, clamped to [`MAX_CHANGES_PAGE`]; zero
    /// means guest default.
    #[serde(default)]
    pub limit: u32,
}

/// What happened to an object in a change feed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectChangeKind {
    /// Something changed; probe the key to learn what. Unknown wire codes
    /// read as this.
    #[default]
    Changed,
    /// The object was created or rewritten.
    Upserted,
    /// The object was removed.
    Removed,
}

/// One entry of a change feed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectChange {
    /// Object key, relative to the destination root.
    pub key: String,
    /// What happened to it.
    pub kind: ObjectChangeKind,
    /// Listing fields after the change when the feed carries them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<ObjectInfo>,
}

/// One page of [`crate::Destination::changes`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesPage {
    /// Changes since the request's cursor, oldest first. A key may repeat.
    pub changes: Vec<ObjectChange>,
    /// Cursor for the next call; never empty.
    pub cursor: String,
    /// True when more changes are ready now.
    #[serde(default)]
    pub more: bool,
    /// False when the feed may have missed changes since the request's
    /// cursor; the host then backs it with a full listing.
    #[serde(default)]
    pub complete: bool,
    /// True when the running guest produces the feed itself (a filesystem
    /// watcher), so stopping it would lose changes. The host keeps such a
    /// guest awake while `complete` stays true, and honours this only from
    /// the first-party storage plugins it starts on demand. Durable feeds
    /// leave it false.
    #[serde(default)]
    pub live_watch: bool,
}

/// True when `key` can name an object: non-empty, without NUL, and at most
/// one ordinary scalar long. Backends apply their own stricter key rules.
fn plausible_key(key: &str) -> bool {
    !key.is_empty() && !key.contains('\0') && key.len() <= crate::MAX_SCALAR_BYTES as usize
}

/// Rejects a batch request the guest is not obliged to serve.
///
/// # Arguments
///
/// * `what` - The operation name, for the error.
/// * `len` - Keys or pairs in the request.
///
/// # Errors
///
/// Returns `invalid_params` for an empty batch and `payload_too_large` for
/// one above [`MAX_BATCH_KEYS`].
pub fn require_batch_len(what: &str, len: usize) -> Result<()> {
    if len == 0 {
        return Err(PluginError::invalid_params(format!(
            "{what} needs at least one key"
        )));
    }
    if u32::try_from(len).map_or(true, |n| n > MAX_BATCH_KEYS) {
        return Err(PluginError::payload_too_large(format!(
            "{what} of {len} keys exceeds {MAX_BATCH_KEYS}"
        )));
    }
    Ok(())
}

/// Checks a guest's per-key outcomes against the keys the host sent.
///
/// # Arguments
///
/// * `what` - The operation name, for the error.
/// * `sent` - Keys the host sent, in order.
/// * `results` - Outcomes the guest returned.
///
/// # Errors
///
/// Returns `internal` unless there is exactly one outcome per key, in the
/// same order and naming the same key.
pub fn validate_key_outcomes(what: &str, sent: &[String], results: &[KeyOutcome]) -> Result<()> {
    if results.len() != sent.len() {
        return Err(PluginError::internal(format!(
            "{what} returned {} outcomes for {} keys",
            results.len(),
            sent.len()
        )));
    }
    if let Some((i, _)) = sent
        .iter()
        .zip(results)
        .enumerate()
        .find(|(_, (key, outcome))| **key != outcome.key)
    {
        return Err(PluginError::internal(format!(
            "{what} outcome {i} names a key the host did not send"
        )));
    }
    Ok(())
}

/// Checks a guest's `moveMany` page against the request it answers.
///
/// Pairs get one outcome per pair, keyed by its source, and no cursor. A
/// prefix page carries at most [`MAX_BATCH_KEYS`] outcomes, each keyed by a
/// source under the prefix (or the prefix itself), and a cursor that is at
/// most [`MAX_CHANGES_CURSOR_BYTES`] and not the one the host sent.
///
/// # Errors
///
/// Returns `internal` describing the first broken rule.
pub fn validate_move_many_page(request: &MoveManyRequest, page: &MoveManyPage) -> Result<()> {
    match &request.target {
        MoveManyTarget::Pairs(pairs) => {
            let sources: Vec<String> = pairs.iter().map(|p| p.from.clone()).collect();
            validate_key_outcomes("moveMany", &sources, &page.results)?;
            if page.next_cursor.is_some() {
                return Err(PluginError::internal(
                    "moveMany returned a cursor for an explicit list of pairs",
                ));
            }
        }
        MoveManyTarget::Prefix(prefix) => {
            if u32::try_from(page.results.len()).map_or(true, |n| n > MAX_BATCH_KEYS) {
                return Err(PluginError::internal(format!(
                    "moveMany returned {} outcomes, more than {MAX_BATCH_KEYS}",
                    page.results.len()
                )));
            }
            if page
                .results
                .iter()
                .any(|r| !r.key.starts_with(&prefix.from) || !plausible_key(&r.key))
            {
                return Err(PluginError::internal(
                    "moveMany returned an outcome outside the source prefix",
                ));
            }
            if let Some(next) = &page.next_cursor {
                if next.len() > MAX_CHANGES_CURSOR_BYTES as usize {
                    return Err(PluginError::internal("moveMany cursor is too long"));
                }
                if request.cursor.as_deref() == Some(next.as_str()) {
                    return Err(PluginError::internal("moveMany cursor did not advance"));
                }
            }
        }
    }
    Ok(())
}

/// Checks a guest's `headMany` probes against the keys the host sent.
///
/// # Errors
///
/// Returns `internal` unless there is one probe per key, in order, and every
/// found object names the key it was asked for.
pub fn validate_head_many(sent: &[String], objects: &[Option<ObjectMetadata>]) -> Result<()> {
    if objects.len() != sent.len() {
        return Err(PluginError::internal(format!(
            "headMany returned {} probes for {} keys",
            objects.len(),
            sent.len()
        )));
    }
    if sent
        .iter()
        .zip(objects)
        .any(|(key, meta)| meta.as_ref().is_some_and(|m| m.key != *key))
    {
        return Err(PluginError::internal(
            "headMany returned a probe for a key the host did not send",
        ));
    }
    Ok(())
}

/// Checks a guest's `translateKeys` answer against the host keys it was sent.
///
/// # Errors
///
/// Returns `internal` unless there is one key per host key and every key is
/// relative (no leading `/`), at most [`MAX_TRANSLATED_KEY_BYTES`] long, free
/// of control characters, and has no empty, `.` or `..` segment between `/`
/// or `\` separators: a key the host can store and read back without
/// leaving the destination.
pub fn validate_translated_keys(sent: &[String], translated: &[String]) -> Result<()> {
    if translated.len() != sent.len() {
        return Err(PluginError::internal(format!(
            "translateKeys returned {} keys for {} host keys",
            translated.len(),
            sent.len()
        )));
    }
    for key in translated {
        if !safe_relative_key(key) {
            return Err(PluginError::internal(format!(
                "translateKeys returned an unusable key ({} bytes)",
                key.len()
            )));
        }
    }
    Ok(())
}

/// True when `key` is a relative key with only ordinary segments; see
/// [`validate_translated_keys`].
fn safe_relative_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_TRANSLATED_KEY_BYTES
        && !key.chars().any(char::is_control)
        && key
            .split(['/', '\\'])
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Checks a guest's change feed page against the cursor the host sent.
///
/// # Errors
///
/// Returns `internal` when the page holds more than [`MAX_CHANGES_PAGE`]
/// changes, a change names an implausible key, the cursor is empty or longer
/// than [`MAX_CHANGES_CURSOR_BYTES`], or the page says more changes are
/// ready without advancing the cursor.
pub fn validate_changes_page(sent: Option<&str>, page: &ChangesPage) -> Result<()> {
    if u32::try_from(page.changes.len()).map_or(true, |n| n > MAX_CHANGES_PAGE) {
        return Err(PluginError::internal(format!(
            "changes page of {} entries exceeds {MAX_CHANGES_PAGE}",
            page.changes.len()
        )));
    }
    if page.cursor.is_empty() || page.cursor.len() > MAX_CHANGES_CURSOR_BYTES as usize {
        return Err(PluginError::internal("changes cursor is empty or too long"));
    }
    if page.more && sent == Some(page.cursor.as_str()) {
        return Err(PluginError::internal("changes cursor did not advance"));
    }
    if page.changes.iter().any(|c| !plausible_key(&c.key)) {
        return Err(PluginError::internal("changes page names an empty key"));
    }
    Ok(())
}

/// Encodes an [`ObjectInfo`] onto a Cap'n Proto builder.
pub(crate) fn fill_object_info(mut b: object_info::Builder<'_>, info: &ObjectInfo) {
    b.set_key(&info.key);
    b.set_size(info.size);
    if let Some(etag) = &info.etag {
        b.set_etag(etag);
    }
    b.set_modified_at_ms(info.modified_at_ms.unwrap_or(0));
    if let Some(v) = &info.version_id {
        b.set_version_id(v);
    }
    if let Some(sig) = &info.signature {
        b.set_signature(sig);
    }
}

/// Decodes an [`ObjectInfo`] from a Cap'n Proto reader.
///
/// # Errors
///
/// Returns [`PluginError`] when a text field cannot be read.
pub(crate) fn read_object_info(r: object_info::Reader<'_>) -> Result<ObjectInfo> {
    Ok(ObjectInfo {
        key: strict_text(r.get_key())?,
        size: r.get_size(),
        etag: non_empty(strict_text(r.get_etag())?),
        modified_at_ms: non_zero(r.get_modified_at_ms()),
        version_id: non_empty(strict_text(r.get_version_id())?),
        signature: non_empty(strict_text(r.get_signature())?),
    })
}

/// Maps a Cap'n Proto failure onto [`PluginError`].
fn from_capnp(err: impl std::fmt::Display) -> PluginError {
    crate::rpc::from_capnp(err)
}

/// Encodes [`MoveOptions`] onto a Cap'n Proto builder.
pub(crate) fn fill_move_options(mut b: move_options::Builder<'_>, options: &MoveOptions) {
    b.set_no_overwrite(options.no_overwrite);
    if let Some(tag) = &options.if_source_match {
        b.set_if_source_match(tag);
    }
}

/// Decodes [`MoveOptions`].
///
/// # Errors
///
/// Returns [`PluginError`] when `ifSourceMatch` cannot be read, so a
/// precondition is never silently dropped.
pub(crate) fn read_move_options(r: move_options::Reader<'_>) -> Result<MoveOptions> {
    Ok(MoveOptions {
        no_overwrite: r.get_no_overwrite(),
        if_source_match: non_empty(strict_text(r.get_if_source_match())?),
    })
}

/// Encodes a [`MoveResult`].
pub(crate) fn fill_move_result(mut b: move_result::Builder<'_>, result: MoveResult) {
    b.set_atomic(result.atomic);
    b.set_already_moved(result.already_moved);
}

/// Decodes a [`MoveResult`].
pub(crate) fn read_move_result(r: move_result::Reader<'_>) -> MoveResult {
    MoveResult {
        atomic: r.get_atomic(),
        already_moved: r.get_already_moved(),
    }
}

/// Encodes one [`KeyPair`].
fn fill_key_pair(mut b: key_pair::Builder<'_>, pair: &KeyPair) {
    b.set_from(&pair.from);
    b.set_to(&pair.to);
}

/// Decodes one [`KeyPair`].
///
/// # Errors
///
/// Returns [`PluginError`] when a text field cannot be read.
fn read_key_pair(r: key_pair::Reader<'_>) -> Result<KeyPair> {
    Ok(KeyPair {
        from: strict_text(r.get_from())?,
        to: strict_text(r.get_to())?,
    })
}

/// Cap'n Proto list length for `len` items.
///
/// # Errors
///
/// Returns `payload_too_large` when `len` does not fit a list.
fn list_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| PluginError::payload_too_large("list too long to encode"))
}

/// Encodes a [`MoveManyRequest`].
///
/// # Errors
///
/// Returns `payload_too_large` when the pair list cannot be encoded.
pub(crate) fn fill_move_many_request(
    mut b: move_many_request::Builder<'_>,
    request: &MoveManyRequest,
) -> Result<()> {
    {
        let target = b.reborrow().init_target();
        match &request.target {
            MoveManyTarget::Pairs(pairs) => {
                let mut list = target.init_pairs(list_len(pairs.len())?);
                for (i, pair) in pairs.iter().enumerate() {
                    fill_key_pair(list.reborrow().get(list_len(i)?), pair);
                }
            }
            MoveManyTarget::Prefix(prefix) => fill_key_pair(target.init_prefix(), prefix),
        }
    }
    if let Some(c) = &request.cursor {
        b.set_cursor(c);
    }
    b.set_limit(request.limit);
    fill_move_options(b.init_options(), &request.options);
    Ok(())
}

/// Decodes a [`MoveManyRequest`].
///
/// # Errors
///
/// Returns [`PluginError`] when the target cannot be read or holds more
/// than [`MAX_BATCH_KEYS`] pairs.
pub(crate) fn read_move_many_request(r: move_many_request::Reader<'_>) -> Result<MoveManyRequest> {
    let target = r.get_target().map_err(from_capnp)?;
    let target = match target.which().map_err(from_capnp)? {
        move_many_target::Pairs(list) => {
            let list = list.map_err(from_capnp)?;
            require_batch_len("moveMany", list.len() as usize)?;
            let mut pairs = Vec::with_capacity(list.len() as usize);
            for pair in list.iter() {
                pairs.push(read_key_pair(pair)?);
            }
            MoveManyTarget::Pairs(pairs)
        }
        move_many_target::Prefix(pair) => {
            MoveManyTarget::Prefix(read_key_pair(pair.map_err(from_capnp)?)?)
        }
    };
    Ok(MoveManyRequest {
        target,
        cursor: non_empty(strict_text(r.get_cursor())?),
        limit: r.get_limit(),
        options: read_move_options(r.get_options().map_err(from_capnp)?)?,
    })
}

/// Encodes one [`KeyOutcome`].
fn fill_key_outcome(mut b: key_outcome::Builder<'_>, outcome: &KeyOutcome) {
    b.set_key(&outcome.key);
    if let Some(err) = &outcome.error {
        write_error(b.init_error(), err);
    }
}

/// Decodes one [`KeyOutcome`]; an error with an empty code is success, and
/// one whose code is unreadable is an `unknown` failure.
///
/// # Errors
///
/// Returns [`PluginError`] when the key cannot be read.
fn read_key_outcome(r: key_outcome::Reader<'_>) -> Result<KeyOutcome> {
    let key = strict_text(r.get_key())?;
    let error = if r.has_error() {
        let e: plugin_error::Reader<'_> = r.get_error().map_err(from_capnp)?;
        // Only a readable, empty code is success. A code that is not text or
        // not UTF-8 is a failure (reported as `unknown`), never a success.
        let code_is_empty = matches!(e.get_code(), Ok(t) if t.as_bytes().is_empty());
        if code_is_empty {
            None
        } else {
            let mut err = read_error(e);
            if err.wire_str().is_empty() {
                err.wire_code = Some("unknown".to_string());
            }
            Some(err)
        }
    } else {
        None
    };
    Ok(KeyOutcome { key, error })
}

/// Encodes a list of [`KeyOutcome`]s.
///
/// # Errors
///
/// Returns `payload_too_large` when the list cannot be encoded.
fn fill_outcomes(
    mut list: capnp::struct_list::Builder<'_, key_outcome::Owned>,
    outcomes: &[KeyOutcome],
) -> Result<()> {
    for (i, outcome) in outcomes.iter().enumerate() {
        fill_key_outcome(list.reborrow().get(list_len(i)?), outcome);
    }
    Ok(())
}

/// Decodes at most [`MAX_BATCH_KEYS`] outcomes.
///
/// # Errors
///
/// Returns `payload_too_large` above the cap, or [`PluginError`] when an
/// entry cannot be read.
fn read_outcomes(
    list: capnp::struct_list::Reader<'_, key_outcome::Owned>,
) -> Result<Vec<KeyOutcome>> {
    if list.len() > MAX_BATCH_KEYS {
        return Err(PluginError::payload_too_large(format!(
            "{} outcomes exceed {MAX_BATCH_KEYS}",
            list.len()
        )));
    }
    let mut out = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        out.push(read_key_outcome(item)?);
    }
    Ok(out)
}

/// Encodes a [`MoveManyPage`].
///
/// # Errors
///
/// Returns `payload_too_large` when the outcomes cannot be encoded.
pub(crate) fn fill_move_many_page(
    mut b: move_many_page::Builder<'_>,
    page: &MoveManyPage,
) -> Result<()> {
    fill_outcomes(
        b.reborrow().init_results(list_len(page.results.len())?),
        &page.results,
    )?;
    if let Some(c) = &page.next_cursor {
        b.set_next_cursor(c);
    }
    b.set_atomic(page.atomic);
    b.set_objects_moved(page.objects_moved);
    Ok(())
}

/// Decodes a [`MoveManyPage`].
///
/// # Errors
///
/// Returns [`PluginError`] when the page cannot be read or exceeds a cap.
pub(crate) fn read_move_many_page(r: move_many_page::Reader<'_>) -> Result<MoveManyPage> {
    Ok(MoveManyPage {
        results: read_outcomes(r.get_results().map_err(from_capnp)?)?,
        next_cursor: non_empty(strict_text(r.get_next_cursor())?),
        atomic: r.get_atomic(),
        objects_moved: r.get_objects_moved(),
    })
}

/// Encodes per-key outcomes into a `DeleteManyResult`.
///
/// # Errors
///
/// Returns `payload_too_large` when the outcomes cannot be encoded.
pub(crate) fn fill_delete_many_result(
    b: crate::plugin_capnp::delete_many_result::Builder<'_>,
    results: &[KeyOutcome],
) -> Result<()> {
    fill_outcomes(b.init_results(list_len(results.len())?), results)
}

/// Decodes a `DeleteManyResult`.
///
/// # Errors
///
/// Returns [`PluginError`] when the result cannot be read or exceeds a cap.
pub(crate) fn read_delete_many_result(
    r: crate::plugin_capnp::delete_many_result::Reader<'_>,
) -> Result<Vec<KeyOutcome>> {
    read_outcomes(r.get_results().map_err(from_capnp)?)
}

/// Encodes one probe per key into a `HeadManyResult`.
///
/// # Errors
///
/// Returns `payload_too_large` when the list cannot be encoded.
pub(crate) fn fill_head_many_result(
    b: head_many_result::Builder<'_>,
    objects: &[Option<ObjectMetadata>],
) -> Result<()> {
    let mut list = b.init_objects(list_len(objects.len())?);
    for (i, meta) in objects.iter().enumerate() {
        let mut item: head_ok::Builder<'_> = list.reborrow().get(list_len(i)?);
        if let Some(meta) = meta {
            item.set_found(true);
            crate::rpc::fill_metadata(item.init_meta(), meta);
        }
    }
    Ok(())
}

/// Decodes a `HeadManyResult`.
///
/// # Errors
///
/// Returns [`PluginError`] when the result cannot be read or holds more than
/// [`MAX_BATCH_KEYS`] probes.
pub(crate) fn read_head_many_result(
    r: head_many_result::Reader<'_>,
) -> Result<Vec<Option<ObjectMetadata>>> {
    let list = r.get_objects().map_err(from_capnp)?;
    if list.len() > MAX_BATCH_KEYS {
        return Err(PluginError::payload_too_large(format!(
            "{} probes exceed {MAX_BATCH_KEYS}",
            list.len()
        )));
    }
    let mut out = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        out.push(if item.get_found() {
            Some(crate::rpc::read_metadata(
                item.get_meta().map_err(from_capnp)?,
            )?)
        } else {
            None
        });
    }
    Ok(out)
}

/// Encodes one translated key per host key into a `TranslateKeysResult`.
///
/// # Errors
///
/// Returns `payload_too_large` when the list cannot be encoded.
pub(crate) fn fill_translate_keys_result(
    b: translate_keys_result::Builder<'_>,
    keys: &[String],
) -> Result<()> {
    let mut list = b.init_keys(list_len(keys.len())?);
    for (i, key) in keys.iter().enumerate() {
        list.set(list_len(i)?, key);
    }
    Ok(())
}

/// Decodes a `TranslateKeysResult`.
///
/// # Errors
///
/// Returns [`PluginError`] when the result cannot be read, holds more than
/// [`MAX_BATCH_KEYS`] keys, or a key is not valid UTF-8.
pub(crate) fn read_translate_keys_result(
    r: translate_keys_result::Reader<'_>,
) -> Result<Vec<String>> {
    let list = r.get_keys().map_err(from_capnp)?;
    if list.len() > MAX_BATCH_KEYS {
        return Err(PluginError::payload_too_large(format!(
            "{} translated keys exceed {MAX_BATCH_KEYS}",
            list.len()
        )));
    }
    let mut out = Vec::with_capacity(list.len() as usize);
    for key in list.iter() {
        out.push(strict_text(key)?);
    }
    Ok(out)
}

/// Encodes a [`ChangesRequest`].
pub(crate) fn fill_changes_request(mut b: changes_request::Builder<'_>, request: &ChangesRequest) {
    if let Some(c) = &request.cursor {
        b.set_cursor(c);
    }
    b.set_limit(request.limit);
}

/// Decodes a [`ChangesRequest`].
///
/// # Errors
///
/// Returns [`PluginError`] when the cursor cannot be read or is longer than
/// [`MAX_CHANGES_CURSOR_BYTES`].
pub(crate) fn read_changes_request(r: changes_request::Reader<'_>) -> Result<ChangesRequest> {
    let cursor = non_empty(strict_text(r.get_cursor())?);
    if cursor
        .as_ref()
        .is_some_and(|c| c.len() > MAX_CHANGES_CURSOR_BYTES as usize)
    {
        return Err(PluginError::invalid_cursor("changes cursor is too long"));
    }
    Ok(ChangesRequest {
        cursor,
        limit: r.get_limit(),
    })
}

/// Wire enum for an [`ObjectChangeKind`].
fn kind_to_wire(kind: ObjectChangeKind) -> WireChangeKind {
    match kind {
        ObjectChangeKind::Changed => WireChangeKind::Changed,
        ObjectChangeKind::Upserted => WireChangeKind::Upserted,
        ObjectChangeKind::Removed => WireChangeKind::Removed,
    }
}

/// Encodes a [`ChangesPage`].
///
/// # Errors
///
/// Returns `payload_too_large` when the change list cannot be encoded.
pub(crate) fn fill_changes_page(
    mut b: changes_page::Builder<'_>,
    page: &ChangesPage,
) -> Result<()> {
    {
        let mut list = b.reborrow().init_changes(list_len(page.changes.len())?);
        for (i, change) in page.changes.iter().enumerate() {
            let mut item: object_change::Builder<'_> = list.reborrow().get(list_len(i)?);
            item.set_key(&change.key);
            item.set_kind(kind_to_wire(change.kind));
            if let Some(info) = &change.info {
                fill_object_info(item.init_info(), info);
            }
        }
    }
    b.set_cursor(&page.cursor);
    b.set_more(page.more);
    b.set_complete(page.complete);
    b.set_live_watch(page.live_watch);
    Ok(())
}

/// Decodes a [`ChangesPage`]. Unknown change kinds read as
/// [`ObjectChangeKind::Changed`].
///
/// # Errors
///
/// Returns [`PluginError`] when the page cannot be read or holds more than
/// [`MAX_CHANGES_PAGE`] changes.
pub(crate) fn read_changes_page(r: changes_page::Reader<'_>) -> Result<ChangesPage> {
    let list = r.get_changes().map_err(from_capnp)?;
    if list.len() > MAX_CHANGES_PAGE {
        return Err(PluginError::payload_too_large(format!(
            "changes page of {} entries exceeds {MAX_CHANGES_PAGE}",
            list.len()
        )));
    }
    let mut changes = Vec::with_capacity(list.len() as usize);
    for item in list.iter() {
        let kind = match item.get_kind() {
            Ok(WireChangeKind::Upserted) => ObjectChangeKind::Upserted,
            Ok(WireChangeKind::Removed) => ObjectChangeKind::Removed,
            Ok(WireChangeKind::Changed) | Err(_) => ObjectChangeKind::Changed,
        };
        let info = if item.has_info() {
            Some(read_object_info(item.get_info().map_err(from_capnp)?)?)
        } else {
            None
        };
        changes.push(ObjectChange {
            key: strict_text(item.get_key())?,
            kind,
            info,
        });
    }
    Ok(ChangesPage {
        changes,
        cursor: strict_text(r.get_cursor())?,
        more: r.get_more(),
        complete: r.get_complete(),
        live_watch: r.get_live_watch(),
    })
}

/// A guest reply the `storage_reply` fuzz target decodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageReply {
    /// A `moveMany` page.
    MoveMany(MoveManyPage),
    /// `deleteMany` outcomes.
    DeleteMany(Vec<KeyOutcome>),
    /// `headMany` probes.
    HeadMany(Vec<Option<ObjectMetadata>>),
    /// A `changes` page.
    Changes(ChangesPage),
    /// A `list` page.
    List(crate::ListPage),
    /// `translateKeys` keys.
    TranslateKeys(Vec<String>),
}

/// Which reply [`decode_storage_reply_bytes`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageReplyKind {
    /// `MoveManyPage`.
    MoveMany,
    /// `DeleteManyResult`.
    DeleteMany,
    /// `HeadManyResult`.
    HeadMany,
    /// `ChangesPage`.
    Changes,
    /// `ListPage`.
    List,
    /// `TranslateKeysResult`.
    TranslateKeys,
}

/// Decodes one guest storage reply from an unpacked Cap'n Proto message,
/// under the same caps the host applies to a live reply.
///
/// # Arguments
///
/// * `kind` - Which reply struct the message holds.
/// * `bytes` - The message.
///
/// # Returns
///
/// The decoded reply.
///
/// # Errors
///
/// Returns [`PluginError`] when the framing, the codec or a cap rejects it.
pub fn decode_storage_reply_bytes(kind: StorageReplyKind, bytes: &[u8]) -> Result<StorageReply> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    let reader = capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)
        .map_err(from_capnp)?;
    match kind {
        StorageReplyKind::MoveMany => Ok(StorageReply::MoveMany(read_move_many_page(
            reader.get_root().map_err(from_capnp)?,
        )?)),
        StorageReplyKind::DeleteMany => Ok(StorageReply::DeleteMany(read_delete_many_result(
            reader.get_root().map_err(from_capnp)?,
        )?)),
        StorageReplyKind::HeadMany => Ok(StorageReply::HeadMany(read_head_many_result(
            reader.get_root().map_err(from_capnp)?,
        )?)),
        StorageReplyKind::Changes => Ok(StorageReply::Changes(read_changes_page(
            reader.get_root().map_err(from_capnp)?,
        )?)),
        StorageReplyKind::List => Ok(StorageReply::List(crate::rpc::decode_list_page(
            reader.get_root().map_err(from_capnp)?,
            crate::MAX_LIST_PAGE,
        )?)),
        StorageReplyKind::TranslateKeys => Ok(StorageReply::TranslateKeys(
            read_translate_keys_result(reader.get_root().map_err(from_capnp)?)?,
        )),
    }
}

/// Encodes one storage reply as an unpacked single-segment message, the
/// inverse of [`decode_storage_reply_bytes`].
///
/// # Errors
///
/// Returns [`PluginError`] when a list cannot be encoded.
pub fn encode_storage_reply_bytes(reply: &StorageReply) -> Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    match reply {
        StorageReply::MoveMany(page) => fill_move_many_page(message.init_root(), page)?,
        StorageReply::DeleteMany(results) => fill_delete_many_result(message.init_root(), results)?,
        StorageReply::HeadMany(objects) => fill_head_many_result(message.init_root(), objects)?,
        StorageReply::Changes(page) => fill_changes_page(message.init_root(), page)?,
        StorageReply::List(page) => {
            crate::rpc::fill_list_page(message.init_root(), page)?;
        }
        StorageReply::TranslateKeys(keys) => {
            fill_translate_keys_result(message.init_root(), keys)?;
        }
    }
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn translated_keys_must_be_one_safe_key_per_host_key() {
        let sent = keys(&["a/b.m4b", "c.jpg"]);
        assert!(validate_translated_keys(&sent, &keys(&["a/b.m4b", "C_.jpg"])).is_ok());
        assert!(validate_translated_keys(&sent, &keys(&["a/b.m4b"])).is_err());
        for bad in [
            "", "/abs", "a//b", "a/./b", "../up", "a\\..\\b", "nul\0", "tab\tkey",
        ] {
            assert!(
                validate_translated_keys(&sent[..1], &keys(&[bad])).is_err(),
                "{bad:?}"
            );
        }
        let long = "k".repeat(MAX_TRANSLATED_KEY_BYTES + 1);
        assert!(validate_translated_keys(&sent[..1], &[long]).is_err());
    }

    #[test]
    fn outcomes_must_match_the_sent_keys_in_order() {
        let sent = keys(&["a", "b"]);
        let good = vec![KeyOutcome::ok("a"), KeyOutcome::ok("b")];
        assert!(validate_key_outcomes("deleteMany", &sent, &good).is_ok());
        let swapped = vec![KeyOutcome::ok("b"), KeyOutcome::ok("a")];
        assert!(validate_key_outcomes("deleteMany", &sent, &swapped).is_err());
        assert!(validate_key_outcomes("deleteMany", &sent, &good[..1]).is_err());
    }

    /// A `PluginError` whose `code` pointer is a struct, not text.
    fn outcome_with_struct_code() -> capnp::message::Builder<capnp::message::HeapAllocator> {
        // `MoveManyRequest` has a struct at pointer 0, where `PluginError`
        // keeps its `code` text: read as a `PluginError`, the code is unreadable.
        let mut fake = capnp::message::Builder::new_default();
        fake.init_root::<move_many_request::Builder<'_>>()
            .init_target()
            .init_pairs(0);
        let fake_error = fake
            .get_root_as_reader::<plugin_error::Reader<'_>>()
            .expect("struct root");
        let mut msg = capnp::message::Builder::new_default();
        let mut outcome = msg.init_root::<key_outcome::Builder<'_>>();
        outcome.set_key("a");
        outcome.set_error(fake_error).expect("copy");
        msg
    }

    #[test]
    fn an_unreadable_error_code_is_a_failure_never_success() {
        let msg = outcome_with_struct_code();
        let reader = msg
            .get_root_as_reader::<key_outcome::Reader<'_>>()
            .expect("root");
        let outcome = read_key_outcome(reader).expect("key reads");
        let err = outcome.error.expect("an unreadable code is a failure");
        assert_eq!(err.wire_str(), "unknown");
    }

    #[test]
    fn invalid_utf8_in_a_cursor_or_precondition_is_refused() {
        let mut msg = capnp::message::Builder::new_default();
        let mut page = msg.init_root::<move_many_page::Builder<'_>>();
        page.reborrow().init_results(0);
        page.reborrow()
            .init_next_cursor(2)
            .as_bytes_mut()
            .copy_from_slice(&[0xff, 0xfe]);
        let reader = msg
            .get_root_as_reader::<move_many_page::Reader<'_>>()
            .expect("root");
        assert!(
            read_move_many_page(reader).is_err(),
            "an undecodable continuation must not read as a finished move"
        );

        let mut msg = capnp::message::Builder::new_default();
        let mut opts = msg.init_root::<move_options::Builder<'_>>();
        opts.reborrow()
            .init_if_source_match(1)
            .as_bytes_mut()
            .copy_from_slice(&[0xff]);
        let reader = msg
            .get_root_as_reader::<move_options::Reader<'_>>()
            .expect("root");
        assert!(read_move_options(reader).is_err());
    }

    #[test]
    fn batch_length_is_bounded() {
        assert!(require_batch_len("x", 0).is_err());
        assert!(require_batch_len("x", 1).is_ok());
        assert!(require_batch_len("x", MAX_BATCH_KEYS as usize).is_ok());
        assert!(require_batch_len("x", MAX_BATCH_KEYS as usize + 1).is_err());
    }

    #[test]
    fn prefix_pages_stay_under_the_prefix_and_advance() {
        let request = MoveManyRequest {
            target: MoveManyTarget::Prefix(KeyPair {
                from: "Old/".into(),
                to: "New/".into(),
            }),
            cursor: Some("c1".into()),
            limit: 0,
            options: MoveOptions::default(),
        };
        let mut page = MoveManyPage {
            results: vec![KeyOutcome::ok("Old/a.m4b")],
            next_cursor: Some("c2".into()),
            atomic: false,
            objects_moved: 1,
        };
        assert!(validate_move_many_page(&request, &page).is_ok());
        page.next_cursor = Some("c1".into());
        assert!(validate_move_many_page(&request, &page).is_err());
        page.next_cursor = None;
        page.results = vec![KeyOutcome::ok("Elsewhere/a.m4b")];
        assert!(validate_move_many_page(&request, &page).is_err());
    }

    #[test]
    fn changes_cursor_must_be_set_and_advance_when_more() {
        let mut page = ChangesPage {
            changes: vec![ObjectChange {
                key: "a".into(),
                kind: ObjectChangeKind::Upserted,
                info: None,
            }],
            cursor: "c2".into(),
            more: true,
            complete: true,
            live_watch: false,
        };
        assert!(validate_changes_page(Some("c1"), &page).is_ok());
        assert!(validate_changes_page(Some("c2"), &page).is_err());
        page.cursor.clear();
        assert!(validate_changes_page(None, &page).is_err());
    }

    #[test]
    fn head_many_probes_name_their_keys() {
        let sent = keys(&["a", "b"]);
        let found = ObjectMetadata {
            key: "a".into(),
            ..ObjectMetadata::default()
        };
        assert!(validate_head_many(&sent, &[Some(found.clone()), None]).is_ok());
        assert!(validate_head_many(&sent, &[None, Some(found)]).is_err());
    }

    #[test]
    fn replies_round_trip() {
        let replies = vec![
            StorageReply::MoveMany(MoveManyPage {
                results: vec![
                    KeyOutcome::ok("a"),
                    KeyOutcome::failed("b", PluginError::conflict("target exists")),
                ],
                next_cursor: Some("next".into()),
                atomic: true,
                objects_moved: 1,
            }),
            StorageReply::DeleteMany(vec![KeyOutcome::ok("x")]),
            StorageReply::HeadMany(vec![
                None,
                Some(ObjectMetadata {
                    key: "k".into(),
                    size: 3,
                    etag: Some("e".into()),
                    modified_at_ms: Some(5),
                    version_id: Some("v".into()),
                    signature: Some("s".into()),
                    ..ObjectMetadata::default()
                }),
            ]),
            StorageReply::Changes(ChangesPage {
                changes: vec![ObjectChange {
                    key: "k".into(),
                    kind: ObjectChangeKind::Removed,
                    info: Some(ObjectInfo {
                        key: "k".into(),
                        size: 1,
                        ..ObjectInfo::default()
                    }),
                }],
                cursor: "c".into(),
                more: false,
                complete: true,
                live_watch: false,
            }),
            StorageReply::List(crate::ListPage {
                objects: vec![ObjectInfo {
                    key: "k".into(),
                    size: 9,
                    signature: Some("sig".into()),
                    ..ObjectInfo::default()
                }],
                next_cursor: None,
                signatures_included: true,
                modified_since_applied: false,
            }),
            StorageReply::TranslateKeys(vec!["a/b.m4b".into(), "c_.jpg".into()]),
        ];
        for reply in replies {
            let kind = match &reply {
                StorageReply::MoveMany(_) => StorageReplyKind::MoveMany,
                StorageReply::DeleteMany(_) => StorageReplyKind::DeleteMany,
                StorageReply::HeadMany(_) => StorageReplyKind::HeadMany,
                StorageReply::Changes(_) => StorageReplyKind::Changes,
                StorageReply::List(_) => StorageReplyKind::List,
                StorageReply::TranslateKeys(_) => StorageReplyKind::TranslateKeys,
            };
            let bytes = encode_storage_reply_bytes(&reply).unwrap();
            assert_eq!(decode_storage_reply_bytes(kind, &bytes).unwrap(), reply);
        }
    }
}

/// Round trips through a real Cap'n Proto capability: the
/// [`crate::DestinationServer`] dispatch, the [`crate::DestinationClient`]
/// codecs and the reply validation it runs.
#[cfg(test)]
// capnp servers are `!Send`; the test runs on one thread in a `LocalSet`.
#[allow(
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arc_with_non_send_sync
)]
mod rpc_tests {
    use std::cell::RefCell;
    use std::pin::Pin;
    use std::sync::Arc;

    use tokio::io::AsyncRead;

    use super::*;
    use crate::{
        ByteRange, CopyResult, Destination, DestinationClient, DestinationServer, ListOptions,
        ListPage, PutResult, ReadResult, WriteOptions,
    };

    /// How the stub guest answers.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// Answers every request correctly.
        Honest,
        /// Breaks each operation's reply rules.
        Broken,
    }

    /// A guest that implements every intent operation.
    struct Stub {
        /// Honest or broken replies.
        mode: Mode,
        /// The last list options and write options it saw.
        seen: RefCell<(Option<ListOptions>, Option<WriteOptions>)>,
    }

    #[async_trait::async_trait(?Send)]
    impl Destination for Stub {
        async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>> {
            Ok(Some(ObjectMetadata {
                key: key.into(),
                size: 1,
                modified_at_ms: Some(7),
                version_id: Some("v1".into()),
                signature: Some("sig".into()),
                ..ObjectMetadata::default()
            }))
        }
        async fn list(&self, options: ListOptions) -> Result<crate::ListPage> {
            let hinted = options.with_signatures;
            self.seen.borrow_mut().0 = Some(options);
            Ok(ListPage {
                objects: vec![ObjectInfo {
                    key: "a/1".into(),
                    size: 1,
                    etag: Some("e1".into()),
                    modified_at_ms: Some(9),
                    version_id: None,
                    signature: Some("sig".into()),
                }],
                next_cursor: None,
                signatures_included: hinted,
                modified_since_applied: false,
            })
        }
        async fn get(&self, _key: &str, _range: Option<ByteRange>) -> Result<ReadResult> {
            Err(PluginError::unsupported("get"))
        }
        async fn put(
            &self,
            key: &str,
            _body: Pin<Box<dyn AsyncRead + Send>>,
            options: WriteOptions,
        ) -> Result<PutResult> {
            let conflict = options.if_none_match;
            self.seen.borrow_mut().1 = Some(options);
            if conflict {
                return Err(PluginError::conflict("exists"));
            }
            Ok(PutResult {
                key: key.into(),
                bytes_written: 0,
                signature: Some("sig2".into()),
                ..PutResult::default()
            })
        }
        async fn copy(&self, _from: &str, _to: &str) -> Result<CopyResult> {
            Err(PluginError::unsupported("copy"))
        }
        async fn delete(&self, _key: &str) -> Result<()> {
            Ok(())
        }
        async fn move_object(
            &self,
            from: &str,
            _to: &str,
            options: MoveOptions,
        ) -> Result<MoveResult> {
            if options.if_source_match.as_deref() == Some("stale") {
                return Err(PluginError::conflict("source changed"));
            }
            Ok(MoveResult {
                atomic: options.no_overwrite,
                already_moved: from == "gone",
            })
        }
        async fn move_many(&self, request: MoveManyRequest) -> Result<MoveManyPage> {
            let results = match (&request.target, self.mode) {
                (MoveManyTarget::Pairs(pairs), Mode::Honest) => pairs
                    .iter()
                    .map(|p| KeyOutcome::ok(p.from.clone()))
                    .collect(),
                (MoveManyTarget::Prefix(prefix), Mode::Honest) => {
                    vec![KeyOutcome::ok(format!("{}x", prefix.from))]
                }
                (_, Mode::Broken) => vec![KeyOutcome::ok("elsewhere")],
            };
            let next_cursor = match &request.target {
                MoveManyTarget::Prefix(_) => Some("c2".to_string()),
                MoveManyTarget::Pairs(_) => None,
            };
            Ok(MoveManyPage {
                objects_moved: u64::try_from(results.len()).unwrap(),
                results,
                next_cursor,
                atomic: true,
            })
        }
        async fn delete_many(&self, keys: Vec<String>) -> Result<Vec<KeyOutcome>> {
            if self.mode == Mode::Broken {
                return Ok(keys.into_iter().rev().map(KeyOutcome::ok).collect());
            }
            Ok(keys
                .into_iter()
                .map(|k| {
                    if k == "busy" {
                        KeyOutcome::failed(k, PluginError::conflict("busy"))
                    } else {
                        KeyOutcome::ok(k)
                    }
                })
                .collect())
        }
        async fn head_many(&self, keys: Vec<String>) -> Result<Vec<Option<ObjectMetadata>>> {
            if self.mode == Mode::Broken {
                return Ok(vec![None]);
            }
            Ok(keys
                .into_iter()
                .map(|k| {
                    (k != "absent").then(|| ObjectMetadata {
                        key: k,
                        size: 2,
                        ..ObjectMetadata::default()
                    })
                })
                .collect())
        }
        async fn changes(&self, request: ChangesRequest) -> Result<ChangesPage> {
            let cursor = match self.mode {
                Mode::Honest => "c2".to_string(),
                Mode::Broken => request.cursor.clone().unwrap_or_default(),
            };
            Ok(ChangesPage {
                changes: vec![ObjectChange {
                    key: "a/1".into(),
                    kind: ObjectChangeKind::Removed,
                    info: None,
                }],
                cursor,
                more: true,
                complete: true,
                live_watch: false,
            })
        }
    }

    /// A guest that implements only the required methods.
    struct Bare;

    #[async_trait::async_trait(?Send)]
    impl Destination for Bare {
        async fn head(&self, _key: &str) -> Result<Option<ObjectMetadata>> {
            Ok(None)
        }
        async fn list(&self, _options: ListOptions) -> Result<crate::ListPage> {
            Ok(ListPage::default())
        }
        async fn get(&self, _key: &str, _range: Option<ByteRange>) -> Result<ReadResult> {
            Err(PluginError::unsupported("get"))
        }
        async fn put(
            &self,
            _key: &str,
            _body: Pin<Box<dyn AsyncRead + Send>>,
            _options: WriteOptions,
        ) -> Result<PutResult> {
            Err(PluginError::unsupported("put"))
        }
        async fn copy(&self, _from: &str, _to: &str) -> Result<CopyResult> {
            Err(PluginError::unsupported("copy"))
        }
        async fn delete(&self, _key: &str) -> Result<()> {
            Ok(())
        }
    }

    /// A client for `dest` over a local capability.
    fn client(dest: Arc<dyn Destination>) -> DestinationClient {
        DestinationClient::new(
            capnp_rpc::new_client(DestinationServer::new(dest, 4096)),
            4096,
        )
    }

    /// An honest or broken stub guest.
    fn stub(mode: Mode) -> Arc<Stub> {
        Arc::new(Stub {
            mode,
            seen: RefCell::new((None, None)),
        })
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn pairs_request(pairs: &[(&str, &str)]) -> MoveManyRequest {
        MoveManyRequest {
            target: MoveManyTarget::Pairs(
                pairs
                    .iter()
                    .map(|(f, t)| KeyPair {
                        from: (*f).into(),
                        to: (*t).into(),
                    })
                    .collect(),
            ),
            cursor: None,
            limit: 0,
            options: MoveOptions::default(),
        }
    }

    fn prefix_request(cursor: Option<&str>) -> MoveManyRequest {
        MoveManyRequest {
            target: MoveManyTarget::Prefix(KeyPair {
                from: "a/".into(),
                to: "b/".into(),
            }),
            cursor: cursor.map(str::to_string),
            limit: 10,
            options: MoveOptions {
                no_overwrite: true,
                if_source_match: None,
            },
        }
    }

    async fn run<F: std::future::Future<Output = ()>>(f: F) {
        tokio::task::LocalSet::new().run_until(f).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn honest_guest_round_trips_every_operation() {
        run(async {
            let guest = stub(Mode::Honest);
            let dest = client(guest.clone());

            let moved = dest
                .move_object(
                    "gone",
                    "b",
                    MoveOptions {
                        no_overwrite: true,
                        if_source_match: Some("e1".into()),
                    },
                )
                .await
                .unwrap();
            assert!(moved.atomic && moved.already_moved);
            let stale = dest
                .move_object(
                    "a",
                    "b",
                    MoveOptions {
                        no_overwrite: false,
                        if_source_match: Some("stale".into()),
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(stale.wire_str(), "conflict");

            let page = dest
                .move_many(pairs_request(&[("x", "y"), ("z", "w")]))
                .await
                .unwrap();
            assert_eq!(page.results.len(), 2);
            assert!(page.next_cursor.is_none());
            let prefix = dest.move_many(prefix_request(Some("c1"))).await.unwrap();
            assert_eq!(prefix.next_cursor.as_deref(), Some("c2"));
            assert_eq!(prefix.results[0].key, "a/x");

            let deleted = dest.delete_many(keys(&["ok", "busy"])).await.unwrap();
            assert!(deleted[0].error.is_none());
            assert_eq!(deleted[1].error.as_ref().unwrap().wire_str(), "conflict");

            let heads = dest.head_many(keys(&["here", "absent"])).await.unwrap();
            assert_eq!(heads[0].as_ref().unwrap().size, 2);
            assert!(heads[1].is_none());

            let feed = dest
                .changes(ChangesRequest {
                    cursor: Some("c1".into()),
                    limit: 5,
                })
                .await
                .unwrap();
            assert_eq!(feed.cursor, "c2");
            assert_eq!(feed.changes[0].kind, ObjectChangeKind::Removed);

            let listed = dest
                .list(ListOptions {
                    with_signatures: true,
                    modified_since_ms: Some(5),
                    ..ListOptions::default()
                })
                .await
                .unwrap();
            assert!(listed.signatures_included);
            assert_eq!(listed.objects[0].signature.as_deref(), Some("sig"));
            let seen = guest.seen.borrow().0.clone().unwrap();
            assert!(seen.with_signatures);
            assert_eq!(seen.modified_since_ms, Some(5));

            let probe = dest.head("k").await.unwrap().unwrap();
            assert_eq!(probe.version_id.as_deref(), Some("v1"));
            assert_eq!(probe.modified_at_ms, Some(7));

            let written = dest
                .put(
                    "k",
                    Box::pin(std::io::Cursor::new(Vec::new())),
                    WriteOptions {
                        if_match: Some("e1".into()),
                        ..WriteOptions::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(written.signature.as_deref(), Some("sig2"));
            assert_eq!(
                guest.seen.borrow().1.as_ref().unwrap().if_match.as_deref(),
                Some("e1")
            );
            let refused = dest
                .put(
                    "k",
                    Box::pin(std::io::Cursor::new(Vec::new())),
                    WriteOptions {
                        if_none_match: true,
                        ..WriteOptions::default()
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(refused.wire_str(), "conflict");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broken_replies_are_refused() {
        run(async {
            let dest = client(stub(Mode::Broken));
            assert!(dest.move_many(pairs_request(&[("x", "y")])).await.is_err());
            assert!(dest.move_many(prefix_request(None)).await.is_err());
            assert!(dest.delete_many(keys(&["a", "b"])).await.is_err());
            assert!(dest.head_many(keys(&["a", "b"])).await.is_err());
            assert!(dest
                .changes(ChangesRequest {
                    cursor: Some("c1".into()),
                    limit: 0,
                })
                .await
                .is_err());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_and_empty_batches_never_reach_the_guest() {
        run(async {
            let dest = client(stub(Mode::Honest));
            assert!(dest.delete_many(Vec::new()).await.is_err());
            let many: Vec<String> = (0..=MAX_BATCH_KEYS).map(|i| format!("k{i}")).collect();
            assert!(dest.head_many(many.clone()).await.is_err());
            let pairs: Vec<(String, String)> = many
                .iter()
                .map(|k| (k.clone(), format!("{k}.new")))
                .collect();
            let refs: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect();
            assert!(dest.move_many(pairs_request(&refs)).await.is_err());
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn undeclared_operations_answer_unsupported() {
        run(async {
            let dest = client(Arc::new(Bare));
            let code = |r: Result<()>| r.unwrap_err().wire_str().to_string();
            assert_eq!(
                code(
                    dest.move_object("a", "b", MoveOptions::default())
                        .await
                        .map(drop)
                ),
                "unsupported"
            );
            assert_eq!(
                code(dest.move_many(pairs_request(&[("a", "b")])).await.map(drop)),
                "unsupported"
            );
            assert_eq!(
                code(dest.delete_many(keys(&["a"])).await.map(drop)),
                "unsupported"
            );
            assert_eq!(
                code(dest.head_many(keys(&["a"])).await.map(drop)),
                "unsupported"
            );
            assert_eq!(
                code(dest.changes(ChangesRequest::default()).await.map(drop)),
                "unsupported"
            );
        })
        .await;
    }
}
