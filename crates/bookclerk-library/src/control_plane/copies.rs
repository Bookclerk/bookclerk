//! A reader takes the copy this library already holds of an edition
//! (`copy.grant`, then the download endpoint).
//!
//! A branch whose owner has a book in their own store account would
//! otherwise download and process an edition this library already holds.
//! Instead it asks for the library's copy:
//!
//! 1. [`plan_copy`] decides whether the asker may have it and what its
//!    files are. Every rule lives there ([`visible_to`] is the one place a
//!    person's right to an edition is decided), and the answer for "you
//!    may not" and "there is no copy" is the same [`CopyPlanned::NotFound`].
//! 2. The daemon picks the destination it can read the files from and
//!    sizes them, then [`open_copy`] records the grant and mints the
//!    token. Only the token's hash is stored.
//! 3. Each download calls [`authorize_copy`], which runs [`plan_copy`]
//!    again: a branch whose trust ended, a person no longer active, an
//!    edition no longer visible or files replaced since the grant all stop
//!    the next request, whatever the token's remaining life.
//!
//! Nothing here reads a file: the rows of `edition_files` say what the
//! copy is, and the daemon streams the bytes from the destination.

use std::collections::BTreeMap;

use bookclerk_node_proto::{
    CopyFile, CopyRole, UploadProbe, WindowCopy, MAX_COPY_FILES, MAX_COPY_NAME_BYTES,
    MAX_PROBE_BITRATE_KBPS,
};
use chrono::{DateTime, Duration, Utc};
use rand::RngCore as _;
use sea_orm::sea_query::Query;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use zeroize::Zeroizing;

use super::catalog_lookup::{is_trusted_branch, read_paged};
use super::window_books::{copies_of_editions, titles_within_bytes};
use crate::entities::{
    copy_grants, edition_acquisitions, edition_files, holdings, nodes, store_listings,
};
use crate::error::{LibraryError, Result};
use crate::models::AcquireStatus;
use crate::store::holdings::find_holdings;
use crate::store::{AcquireTitle, FileRole, LibraryStore};

/// Seconds a copy token is accepted after its grant.
pub const COPY_TOKEN_TTL_SECS: i64 = 60 * 60;

/// Most holdings of one store product [`plan_copy`] reads: one per account
/// that holds it, so far more than any library has.
const MAX_PRODUCT_HOLDINGS: u64 = 64;

/// Who asks for a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyHolder<'a> {
    /// A branch node, on its node socket; the person is its owner.
    Node(&'a str),
}

impl CopyHolder<'_> {
    /// The stored `copy_grants.holder_kind`.
    fn kind(self) -> &'static str {
        match self {
            Self::Node(_) => "node",
        }
    }

    /// The stored `copy_grants.holder_id`.
    fn id(&self) -> &str {
        match self {
            Self::Node(id) => id,
        }
    }
}

/// The store listing a reader names an edition by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyListing<'a> {
    /// Store id as the library's page states it (the store's alias).
    pub store: &'a str,
    /// Lowercase marketplace; empty for a store without marketplaces.
    pub marketplace: &'a str,
    /// The store's product id.
    pub product_id: &'a str,
}

/// One file of a planned copy, on one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPlanFile {
    /// Storage key on the destination.
    pub key: String,
    /// The file as the answer frame lists it, its size not yet known
    /// (zero): the daemon reads sizes from the destination.
    pub file: CopyFile,
}

/// The copy of one edition a holder may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPlan {
    /// The person the copy is for.
    pub person: i64,
    /// The edition.
    pub edition_id: String,
    /// What the copy is, as the library's page states it.
    pub copy: WindowCopy,
    /// The edition's files on each destination that holds its primary
    /// audio, by destination id; each list has the primary audio first,
    /// then the rest in key order. A file's place in its list is its
    /// download index.
    pub destinations: BTreeMap<String, Vec<CopyPlanFile>>,
    /// Per destination, the digest of the file rows the list was made
    /// from ([`open_copy`] stores it, [`authorize_copy`] compares it).
    digests: BTreeMap<String, String>,
}

/// The answer of [`plan_copy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyPlanned {
    /// The asker is not one this library gives copies to (frame code
    /// `refused`).
    Refused,
    /// The person cannot take that edition, or the library holds no copy
    /// of it (frame code `not_found` for both).
    NotFound,
    /// The copy and its files.
    Planned(CopyPlan),
}

/// The titles among `titles` that `person` may take a copy of: the single
/// place a person's right to an edition is decided, for the grant and for
/// every download.
///
/// A library's members see every book on its store accounts, as its own
/// Library page lists them, so today every listed title passes. Patron
/// scopes (`docs/adr/patron-scopes.md`) narrow this here and nowhere
/// else: a scoped Member's include and exclude rules and the Download
/// permission apply to the titles that are not the person's own, and a
/// person's own books always pass.
fn visible_to(_person: i64, titles: Vec<AcquireTitle>) -> Vec<AcquireTitle> {
    titles
}

/// The person `holder` asks for, when this library gives it copies: a
/// trusted branch's owner.
///
/// # Errors
///
/// Returns the read error.
async fn person_of(store: &LibraryStore, holder: CopyHolder<'_>) -> Result<Option<i64>> {
    match holder {
        CopyHolder::Node(node_id) => {
            let Some(node) = nodes::Entity::find_by_id(node_id.to_string())
                .one(store.db())
                .await
                .map_err(LibraryError::Orm)?
            else {
                return Ok(None);
            };
            if !is_trusted_branch(store, &node).await? {
                return Ok(None);
            }
            Ok(node.owner_user_id)
        }
    }
}

/// The library's titles of the store product `listing` names, as the
/// books page lists them: every account holding of a listing with that
/// product id whose store and marketplace are the ones named.
///
/// # Errors
///
/// Returns the read error.
async fn titles_of_listing(
    store: &LibraryStore,
    listing: CopyListing<'_>,
) -> Result<Vec<AcquireTitle>> {
    let listings = Query::select()
        .column(store_listings::Column::Id)
        .from(store_listings::Entity)
        .and_where(store_listings::Column::ProductId.eq(listing.product_id))
        .to_owned();
    let held = read_paged(
        store,
        find_holdings()
            .filter(holdings::Column::HolderKind.eq("account"))
            .filter(holdings::Column::ListingId.in_subquery(listings)),
        Some(MAX_PRODUCT_HOLDINGS),
    )
    .await?;
    Ok(titles_within_bytes(store, held)
        .await?
        .into_iter()
        .filter(|title| {
            title.product_id == listing.product_id
                && title.source.trim().eq_ignore_ascii_case(listing.store)
                && title
                    .marketplace
                    .trim()
                    .eq_ignore_ascii_case(listing.marketplace)
        })
        .collect())
}

/// What a planned file is, from its row: the edition's primary key is its
/// audio, and any other row recorded as audio is a part.
fn role_of(row: &edition_files::Model, primary: &str) -> CopyRole {
    if row.storage_key == primary {
        return CopyRole::Audio;
    }
    match FileRole::parse(&row.role) {
        FileRole::Audio | FileRole::Part => CopyRole::Part,
        FileRole::Chapter => CopyRole::Chapter,
        FileRole::Pdf => CopyRole::Pdf,
        FileRole::Cover => CopyRole::Cover,
        FileRole::Sidecar => CopyRole::Sidecar,
        FileRole::Transcript => CopyRole::Transcript,
        FileRole::Epub => CopyRole::Epub,
    }
}

/// The folder a storage key lies in, with its trailing `/`; empty for a
/// key at the destination's root.
fn folder_of(key: &str) -> &str {
    key.rfind('/').map_or("", |at| &key[..=at])
}

/// The answer-frame entry of `row`, named relative to `folder`; `None`
/// for a row outside the folder or one a frame cannot carry.
fn plan_file(row: &edition_files::Model, primary: &str, folder: &str) -> Option<CopyPlanFile> {
    let name = row.storage_key.strip_prefix(folder)?;
    let role = role_of(row, primary);
    let probed = row.probed_at.is_some()
        && matches!(role, CopyRole::Audio | CopyRole::Part | CopyRole::Chapter);
    let file = CopyFile {
        name: name.to_string(),
        role,
        size: 0,
        content_type: row.content_type.clone().filter(|_| probed),
        probe: probed.then(|| UploadProbe {
            bitrate_kbps: row
                .bitrate_kbps
                .and_then(|kbps| u32::try_from(kbps).ok())
                .filter(|kbps| (1..=MAX_PROBE_BITRATE_KBPS).contains(kbps)),
            lossless: row.lossless != 0,
            source_store: row.source_store.clone(),
        }),
    };
    (name.len() <= MAX_COPY_NAME_BYTES && file.is_well_formed()).then(|| CopyPlanFile {
        key: row.storage_key.clone(),
        file,
    })
}

/// The digest of `rows` in order: what the files were when the list was
/// made. Any write of a key changes its `written_at`.
fn digest_of(rows: &[&edition_files::Model]) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    for row in rows {
        for part in [
            row.storage_key.as_str(),
            row.written_at.as_str(),
            row.writer.as_str(),
            row.sha256.as_deref().unwrap_or(""),
        ] {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

/// The files of `edition_id` on each destination that holds its primary
/// audio `primary`, with each list's digest.
///
/// A destination whose list would pass [`MAX_COPY_FILES`] is left out: the
/// answer frame could not carry it.
///
/// # Errors
///
/// Returns the read error.
async fn files_of(
    store: &LibraryStore,
    edition_id: &str,
    primary: &str,
) -> Result<(
    BTreeMap<String, Vec<CopyPlanFile>>,
    BTreeMap<String, String>,
)> {
    let cap = u64::try_from(MAX_COPY_FILES)
        .unwrap_or(u64::MAX)
        .saturating_mul(8);
    let rows = read_paged(
        store,
        edition_files::Entity::find()
            .filter(edition_files::Column::OwnerKind.eq("edition"))
            .filter(edition_files::Column::OwnerId.eq(edition_id)),
        Some(cap),
    )
    .await?;
    let mut by_destination: BTreeMap<&str, Vec<&edition_files::Model>> = BTreeMap::new();
    for row in &rows {
        by_destination
            .entry(row.destination.as_str())
            .or_default()
            .push(row);
    }
    let folder = folder_of(primary);
    let mut files = BTreeMap::new();
    let mut digests = BTreeMap::new();
    for (destination, mut rows) in by_destination {
        if !rows.iter().any(|row| row.storage_key == primary) {
            continue;
        }
        rows.sort_by(|a, b| {
            (a.storage_key != primary, &a.storage_key)
                .cmp(&(b.storage_key != primary, &b.storage_key))
        });
        let listed: Vec<&edition_files::Model> = rows
            .into_iter()
            .filter(|row| plan_file(row, primary, folder).is_some())
            .collect();
        if listed.len() > MAX_COPY_FILES
            || listed.first().is_none_or(|row| row.storage_key != primary)
        {
            continue;
        }
        digests.insert(destination.to_string(), digest_of(&listed));
        files.insert(
            destination.to_string(),
            listed
                .into_iter()
                .filter_map(|row| plan_file(row, primary, folder))
                .collect(),
        );
    }
    Ok((files, digests))
}

/// Decides whether `holder` may take this library's copy of the edition
/// `listing` names, and lists the copy's files.
///
/// The holder must be one this library gives copies to ([`CopyHolder`]);
/// the edition must be one the person may take (`visible_to`) and one
/// this library has acquired, with its primary audio recorded on at least
/// one destination.
///
/// # Errors
///
/// Returns the read error.
pub async fn plan_copy(
    store: &LibraryStore,
    holder: CopyHolder<'_>,
    listing: CopyListing<'_>,
) -> Result<CopyPlanned> {
    let Some(person) = person_of(store, holder).await? else {
        return Ok(CopyPlanned::Refused);
    };
    let titles = visible_to(person, titles_of_listing(store, listing).await?);
    let mut editions: Vec<String> = titles.into_iter().map(|title| title.edition_id).collect();
    editions.sort();
    editions.dedup();
    for edition_id in editions {
        if let Some(plan) = edition_plan(store, person, &edition_id).await? {
            return Ok(CopyPlanned::Planned(plan));
        }
    }
    Ok(CopyPlanned::NotFound)
}

/// The library's titles of edition `edition_id`, as the books page lists
/// them: every account holding of one of its listings.
///
/// # Errors
///
/// Returns the read error.
async fn titles_of_edition(store: &LibraryStore, edition_id: &str) -> Result<Vec<AcquireTitle>> {
    let listings = Query::select()
        .column(store_listings::Column::Id)
        .from(store_listings::Entity)
        .and_where(store_listings::Column::EditionId.eq(edition_id))
        .to_owned();
    let held = read_paged(
        store,
        find_holdings()
            .filter(holdings::Column::HolderKind.eq("account"))
            .filter(holdings::Column::ListingId.in_subquery(listings)),
        Some(MAX_PRODUCT_HOLDINGS),
    )
    .await?;
    Ok(titles_within_bytes(store, held)
        .await?
        .into_iter()
        .filter(|title| title.edition_id == edition_id)
        .collect())
}

/// The copy this library holds of `edition_id` and its files, for
/// `person`; `None` when it has not acquired the edition or no
/// destination records its primary audio. Whether the person may take it
/// is the caller's check ([`visible_to`]).
///
/// # Errors
///
/// Returns the read error.
async fn edition_plan(
    store: &LibraryStore,
    person: i64,
    edition_id: &str,
) -> Result<Option<CopyPlan>> {
    let ids = [edition_id.to_string()];
    let Some(copy) = copies_of_editions(store, &ids).await?.remove(edition_id) else {
        return Ok(None);
    };
    let Some(primary) = edition_acquisitions::Entity::find()
        .filter(edition_acquisitions::Column::EditionId.eq(edition_id))
        .filter(edition_acquisitions::Column::Status.eq(AcquireStatus::Acquired.as_str()))
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .and_then(|row| row.storage_key)
    else {
        return Ok(None);
    };
    let (destinations, digests) = files_of(store, edition_id, &primary).await?;
    Ok((!destinations.is_empty()).then(|| CopyPlan {
        person,
        edition_id: edition_id.to_string(),
        copy,
        destinations,
        digests,
    }))
}

/// A grant just opened by [`open_copy`].
pub struct OpenedCopy {
    /// The copy id.
    pub copy_id: String,
    /// The bearer token; shown to the holder once and never stored.
    pub token: Zeroizing<String>,
    /// Seconds the token is accepted.
    pub expires_in_secs: u64,
}

impl Drop for OpenedCopy {
    /// The token leaves the redaction registry ([`open_copy`] registered
    /// it) and its copy here is zeroized with the grant. The holder's
    /// later requests carry it in a `Bookclerk-Copy` header, which the
    /// redaction patterns cover.
    fn drop(&mut self) {
        bookclerk_config::unregister_secret(self.token.as_str());
    }
}

impl std::fmt::Debug for OpenedCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedCopy")
            .field("copy_id", &self.copy_id)
            .field("token", &"<redacted>")
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// A fresh bearer token: 32 random bytes, base64url.
fn mint_token() -> Zeroizing<String> {
    use base64::Engine as _;
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(bytes.as_mut_slice());
    Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes.as_slice()))
}

/// Records that `holder` may download `plan`'s files from `destination`
/// on node `serving_node_id`, and mints the token.
///
/// # Returns
///
/// `None` when `plan` has no files on `destination`.
///
/// # Errors
///
/// Returns the write error.
pub async fn open_copy(
    store: &LibraryStore,
    holder: CopyHolder<'_>,
    plan: &CopyPlan,
    destination: &str,
    serving_node_id: &str,
) -> Result<Option<OpenedCopy>> {
    let Some(digest) = plan.digests.get(destination) else {
        return Ok(None);
    };
    let token = mint_token();
    // Registered before anything can fail or log with it in scope.
    bookclerk_config::register_secret(token.as_str());
    let opened = OpenedCopy {
        copy_id: uuid::Uuid::new_v4().to_string(),
        token,
        expires_in_secs: u64::try_from(COPY_TOKEN_TTL_SECS).unwrap_or(0),
    };
    let now = Utc::now();
    copy_grants::Entity::insert(copy_grants::ActiveModel {
        copy_id: Set(opened.copy_id.clone()),
        token_hash: Set(crate::hash_token(&opened.token)),
        holder_kind: Set(holder.kind().to_string()),
        holder_id: Set(holder.id().to_string()),
        user_id: Set(plan.person),
        edition_id: Set(plan.edition_id.clone()),
        serving_node_id: Set(serving_node_id.to_string()),
        destination: Set(destination.to_string()),
        files_digest: Set(digest.clone()),
        created_at: Set(now.to_rfc3339()),
        expires_at: Set((now + Duration::seconds(COPY_TOKEN_TTL_SECS)).to_rfc3339()),
    })
    .exec_without_returning(store.db())
    .await
    .map_err(LibraryError::Orm)?;
    Ok(Some(opened))
}

/// What a download request may read ([`authorize_copy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyDownload {
    /// The node that granted the copy and serves its files.
    pub serving_node_id: String,
    /// The destination the files are read from.
    pub destination: String,
    /// The files, in download order.
    pub files: Vec<CopyPlanFile>,
}

/// The files `token` may download for copy `copy_id`, or `None`.
///
/// The token's hash must match and it must not have expired; and, read
/// afresh on every call, the holder must still be one this library gives
/// copies to for the same person, the person must still be allowed the
/// edition (`visible_to`), and the edition's files on the granted
/// destination must be the ones the grant listed. Every refusal is the
/// same `None`.
///
/// # Errors
///
/// Returns the read error.
pub async fn authorize_copy(
    store: &LibraryStore,
    copy_id: &str,
    token: &str,
) -> Result<Option<CopyDownload>> {
    if token.is_empty() || token.len() > 128 || !bookclerk_node_proto::is_copy_id(copy_id) {
        return Ok(None);
    }
    let Some(row) = copy_grants::Entity::find_by_id(copy_id.to_string())
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    else {
        return Ok(None);
    };
    let unexpired = DateTime::parse_from_rfc3339(&row.expires_at)
        .is_ok_and(|expires| expires.with_timezone(&Utc) > Utc::now());
    if row.token_hash != crate::hash_token(token) || !unexpired {
        return Ok(None);
    }
    let holder = match row.holder_kind.as_str() {
        "node" => CopyHolder::Node(row.holder_id.as_str()),
        _ => return Ok(None),
    };
    if person_of(store, holder).await? != Some(row.user_id) {
        return Ok(None);
    }
    if visible_to(
        row.user_id,
        titles_of_edition(store, &row.edition_id).await?,
    )
    .is_empty()
    {
        return Ok(None);
    }
    let Some(plan) = edition_plan(store, row.user_id, &row.edition_id).await? else {
        return Ok(None);
    };
    if plan.digests.get(&row.destination) != Some(&row.files_digest) {
        return Ok(None);
    }
    let Some(files) = plan.destinations.get(&row.destination).cloned() else {
        return Ok(None);
    };
    Ok(Some(CopyDownload {
        serving_node_id: row.serving_node_id,
        destination: row.destination,
        files,
    }))
}

#[cfg(test)]
#[path = "copies_tests.rs"]
mod tests;
