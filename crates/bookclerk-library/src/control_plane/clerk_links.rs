//! On a clerk: the libraries this install holds a library card for.
//!
//! A clerk is a client app that signs in to a library as its person; it is
//! not a node of that library. Signing in redeems an OAuth code for a
//! library card (`ClerkTokenAnswer`). [`record_clerk_link`] seals the card
//! with this install's DEK into one `encrypted_secrets` row (`kind =
//! clerk_card`, `provider` and `account_id` = the library's cluster id) and
//! writes the `clerk_links` row. The clerk reader loads the card with
//! [`load_clerk_card`] to dial the library's clerk socket, and stores the
//! books it reads with the window books functions
//! (`control_plane::window_books`), keyed by the library's cluster id, so
//! "Your books in other libraries" merges them with those of window links.
//!
//! The card never leaves this module in plain form except as a
//! [`LibraryCard`], a zeroizing buffer whose `Debug` output is redacted.

use chrono::{SecondsFormat, Utc};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::Serialize;
use zeroize::Zeroizing;

use crate::control_plane::{clear_window_books, load_cluster_link, sea_probe, Bump, LiveTopic};
use crate::entities::clerk_links;
use crate::error::{LibraryError, Result};
use crate::secrets::{
    build_sealed_record, delete_secret, get_secret, secret_account_type, secret_kind,
    unseal_secret, upsert_secret,
};
use crate::store::LibraryStore;
use crate::EntityKind;

/// `encrypted_secrets.name` of a clerk link's library card.
pub const CLERK_CARD_NAME: &str = "library_card";

/// Longest library name kept.
const MAX_LIBRARY_NAME_BYTES: usize = 256;

/// Whether this install still holds the card of a clerk link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClerkLinkState {
    /// The card is held; the clerk reader dials the library.
    Active,
    /// The library ended the card (or the person signed out here); the
    /// card is deleted and nothing dials.
    SignedOut,
}

impl ClerkLinkState {
    /// The `clerk_links.state` spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::SignedOut => "signed_out",
        }
    }

    /// Parses a stored state; anything unknown reads as signed out, so a
    /// damaged row never dials.
    fn from_column(raw: &str) -> Self {
        if raw == Self::Active.as_str() {
            Self::Active
        } else {
            Self::SignedOut
        }
    }
}

/// One library this install holds (or held) a library card for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClerkLink {
    /// The library's cluster id.
    pub library_cluster_id: String,
    /// The library's origin as [`bookclerk_node_proto::canonical_origin`]
    /// spells it.
    pub origin: String,
    /// SPKI pin of the library's self-signed certificate, when the person
    /// confirmed one.
    pub spki_pin: Option<String>,
    /// The library's name at sign-in.
    pub library_name: String,
    /// The person's user id on the library.
    pub remote_user_id: i64,
    /// The person's display name on the library, when it has one.
    pub display_name: Option<String>,
    /// The name this install signed in as.
    pub clerk_name: String,
    /// Whether the card is still held.
    pub state: ClerkLinkState,
    /// RFC 3339 sign-in time.
    pub created_at: String,
    /// RFC 3339 time of the last change.
    pub updated_at: String,
}

impl ClerkLink {
    /// True while the clerk reader should dial this library.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state == ClerkLinkState::Active
    }
}

impl From<clerk_links::Model> for ClerkLink {
    fn from(row: clerk_links::Model) -> Self {
        Self {
            library_cluster_id: row.library_cluster_id,
            origin: row.origin,
            spki_pin: row.spki_pin,
            library_name: row.library_name,
            remote_user_id: row.remote_user_id,
            display_name: row.display_name,
            clerk_name: row.clerk_name,
            state: ClerkLinkState::from_column(&row.state),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// A library card in plain form: zeroized on drop, redacted in `Debug`.
#[derive(Clone)]
pub struct LibraryCard(Zeroizing<String>);

impl std::fmt::Debug for LibraryCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LibraryCard(<redacted>)")
    }
}

impl LibraryCard {
    /// Wraps a card a library issued.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when `token` is not a card
    /// ([`bookclerk_node_proto::is_clerk_token`]).
    pub fn new(token: String) -> Result<Self> {
        let token = Zeroizing::new(token);
        if !bookclerk_node_proto::is_clerk_token(&token) {
            return Err(LibraryError::Invalid("not a library card".into()));
        }
        Ok(Self(token))
    }

    /// The card, for the `Authorization: Bookclerk-Clerk` header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// What a sign-in returned, to record with [`record_clerk_link`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClerkLink {
    /// The library's cluster id, as both its info and its token answer
    /// named it.
    pub library_cluster_id: String,
    /// The origin the sign-in went to.
    pub origin: String,
    /// SPKI pin the person confirmed, for a self-signed library.
    pub spki_pin: Option<String>,
    /// The library's name from its info.
    pub library_name: String,
    /// The person's user id from the token answer.
    pub remote_user_id: i64,
    /// The person's display name from the token answer.
    pub display_name: Option<String>,
    /// The name this install signed in as.
    pub clerk_name: String,
}

/// Seals `card` and records the link as active, replacing an earlier
/// sign-in to the same library.
///
/// The card row is written first, so a link row never names a card that
/// is not stored; a card row left by a failed write is replaced by the
/// next sign-in.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a malformed cluster id, origin,
/// pin, name or user id, [`LibraryError::Conflict`] when this install is
/// already an active branch or window of that library (it reads that
/// library's books through the link instead), an error when no master key
/// is configured, and the write error.
pub async fn record_clerk_link(
    store: &LibraryStore,
    link: &NewClerkLink,
    card: &LibraryCard,
) -> Result<ClerkLink> {
    if !bookclerk_node_proto::is_cluster_id(&link.library_cluster_id) {
        return Err(LibraryError::Invalid("not a library cluster id".into()));
    }
    let origin = bookclerk_node_proto::canonical_origin(&link.origin)
        .ok_or_else(|| LibraryError::Invalid("library origin must be an http(s) origin".into()))?;
    if let Some(pin) = link.spki_pin.as_deref() {
        crate::control_plane::decode_spki_pin(pin)?;
    }
    let library_name = link.library_name.trim();
    if library_name.len() > MAX_LIBRARY_NAME_BYTES || library_name.chars().any(char::is_control) {
        return Err(LibraryError::Invalid(
            "library name is not displayable".into(),
        ));
    }
    if !bookclerk_node_proto::is_clerk_name(&link.clerk_name) {
        return Err(LibraryError::Invalid(
            "clerk name is not displayable".into(),
        ));
    }
    if link.remote_user_id <= 0 {
        return Err(LibraryError::Invalid(
            "remote user id must be positive".into(),
        ));
    }
    if link.display_name.as_deref().is_some_and(|name| {
        name.len() > bookclerk_node_proto::MAX_CLERK_DISPLAY_NAME_BYTES
            || name.chars().any(char::is_control)
    }) {
        return Err(LibraryError::Invalid(
            "display name is not displayable".into(),
        ));
    }
    if load_cluster_link(store, &link.library_cluster_id)
        .await?
        .is_some_and(|cluster| cluster.is_active())
    {
        return Err(LibraryError::Conflict(
            "this install already joined that library; its books are read through that link".into(),
        ));
    }
    let record = build_sealed_record(
        card.expose().as_bytes(),
        secret_kind::CLERK_CARD,
        &link.library_cluster_id,
        secret_account_type::OPERATOR,
        &link.library_cluster_id,
        CLERK_CARD_NAME,
    )?;
    upsert_secret(store.db(), &record).await?;
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let row = clerk_links::ActiveModel {
        library_cluster_id: Set(link.library_cluster_id.clone()),
        origin: Set(origin),
        spki_pin: Set(link.spki_pin.clone()),
        library_name: Set(library_name.to_string()),
        remote_user_id: Set(link.remote_user_id),
        display_name: Set(link.display_name.clone()),
        clerk_name: Set(link.clerk_name.clone()),
        state: Set(ClerkLinkState::Active.as_str().into()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    };
    let insert = clerk_links::Entity::insert(row).on_conflict(
        OnConflict::column(clerk_links::Column::LibraryClusterId)
            .update_columns([
                clerk_links::Column::Origin,
                clerk_links::Column::SpkiPin,
                clerk_links::Column::LibraryName,
                clerk_links::Column::RemoteUserId,
                clerk_links::Column::DisplayName,
                clerk_links::Column::ClerkName,
                clerk_links::Column::State,
                clerk_links::Column::CreatedAt,
                clerk_links::Column::UpdatedAt,
            ])
            .to_owned(),
    );
    let previous = load_clerk_link(store, &link.library_cluster_id).await?;
    store
        .exec_bumped(insert, LiveTopic::Clusters, Bump::Always)
        .await?;
    // A sign-in as another person must not show the previous person's
    // books, even for the moment before the reader replaces them.
    if previous.is_some_and(|prev| prev.remote_user_id != link.remote_user_id) {
        clear_window_books(store, &link.library_cluster_id).await?;
    }
    load_clerk_link(store, &link.library_cluster_id)
        .await?
        .ok_or_else(|| LibraryError::not_found(EntityKind::ClerkLink, &link.library_cluster_id))
}

/// Every clerk link, oldest sign-in first.
///
/// # Errors
///
/// Returns the read error.
pub async fn list_clerk_links(store: &LibraryStore) -> Result<Vec<ClerkLink>> {
    // Paged, so a database that returns few rows per read still lists
    // every link.
    let select = clerk_links::Entity::find().order_by_asc(clerk_links::Column::CreatedAt);
    Ok(
        crate::control_plane::catalog_lookup::read_paged(store, select, None)
            .await?
            .into_iter()
            .map(ClerkLink::from)
            .collect(),
    )
}

/// One clerk link.
///
/// # Errors
///
/// Returns the read error.
pub async fn load_clerk_link(
    store: &LibraryStore,
    library_cluster_id: &str,
) -> Result<Option<ClerkLink>> {
    Ok(
        clerk_links::Entity::find_by_id(library_cluster_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(ClerkLink::from),
    )
}

/// Unseals the card of an active `link`.
///
/// The caller registers the card with the log redactor before using it.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] when the link is signed out,
/// [`LibraryError::NotFound`] when the card row is missing, and
/// [`LibraryError::Invalid`] when it does not hold a card.
pub async fn load_clerk_card(store: &LibraryStore, link: &ClerkLink) -> Result<LibraryCard> {
    if !link.is_active() {
        return Err(LibraryError::Forbidden(format!(
            "the library card for {} was signed out",
            link.library_cluster_id
        )));
    }
    let record = get_secret(
        store.db(),
        secret_kind::CLERK_CARD,
        Some(&link.library_cluster_id),
        secret_account_type::OPERATOR,
        Some(&link.library_cluster_id),
        CLERK_CARD_NAME,
    )
    .await?
    .ok_or_else(|| LibraryError::not_found(EntityKind::ClerkCard, &link.library_cluster_id))?;
    let plaintext = Zeroizing::new(unseal_secret(&record)?);
    let token = std::str::from_utf8(&plaintext)
        .map_err(|_| LibraryError::Invalid("stored library card is not text".into()))?
        .to_string();
    LibraryCard::new(token)
}

/// Marks a link signed out, deletes its card and drops the books read
/// through it. Answers the link as it now stands, or `None` when there is
/// no such link.
///
/// Used when the library ends the card (close 4401 or a 401 answer) and
/// when the person signs out here.
///
/// # Errors
///
/// Returns the read or write error.
pub async fn sign_out_clerk_link(
    store: &LibraryStore,
    library_cluster_id: &str,
) -> Result<Option<ClerkLink>> {
    let Some(mut link) = load_clerk_link(store, library_cluster_id).await? else {
        return Ok(None);
    };
    let signed_out = ClerkLinkState::SignedOut.as_str();
    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
    let update = clerk_links::Entity::update_many()
        .col_expr(clerk_links::Column::State, Expr::value(signed_out))
        .col_expr(clerk_links::Column::UpdatedAt, Expr::value(now.clone()))
        .filter(clerk_links::Column::LibraryClusterId.eq(library_cluster_id));
    let probe = clerk_links::Entity::find()
        .filter(clerk_links::Column::LibraryClusterId.eq(library_cluster_id))
        .filter(clerk_links::Column::State.ne(signed_out));
    // State first: a signed-out link is never dialed, even if a delete
    // below fails.
    store
        .exec_bumped(update, LiveTopic::Clusters, Bump::IfAny(sea_probe(&probe)?))
        .await?;
    delete_secret(
        store.db(),
        secret_kind::CLERK_CARD,
        Some(library_cluster_id),
        secret_account_type::OPERATOR,
        Some(library_cluster_id),
        CLERK_CARD_NAME,
    )
    .await?;
    // A window link to the same library keeps its own rows; only a clerk
    // link with no active cluster link owns them.
    if !load_cluster_link(store, library_cluster_id)
        .await?
        .is_some_and(|cluster| cluster.is_active())
    {
        clear_window_books(store, library_cluster_id).await?;
    }
    if link.is_active() {
        link.state = ClerkLinkState::SignedOut;
        link.updated_at = now;
    }
    Ok(Some(link))
}

/// Deletes a signed-out link's row, so it leaves the list. Answers false
/// when there was no such link.
///
/// # Errors
///
/// Returns [`LibraryError::Conflict`] while the link is active (sign out
/// first), and the write error.
pub async fn forget_clerk_link(store: &LibraryStore, library_cluster_id: &str) -> Result<bool> {
    let Some(link) = load_clerk_link(store, library_cluster_id).await? else {
        return Ok(false);
    };
    if link.is_active() {
        return Err(LibraryError::Conflict(
            "sign out of the library before removing it".into(),
        ));
    }
    let delete = clerk_links::Entity::delete_many()
        .filter(clerk_links::Column::LibraryClusterId.eq(library_cluster_id))
        .filter(clerk_links::Column::State.eq(ClerkLinkState::SignedOut.as_str()));
    let removed = store
        .exec_bumped(delete, LiveTopic::Clusters, Bump::Always)
        .await?;
    Ok(removed == 1)
}

#[cfg(test)]
#[path = "clerk_links_tests.rs"]
mod tests;
