//! Member controls: what a patron may do in the library, beside what they
//! see (see `docs/adr/patron-scopes.md`).
//!
//! Six switches, each on out of the box ([`MemberControl`]). The library's
//! default (the `core.member_controls` cluster document) sets every switch
//! for all patrons; an Administrator can override single switches for one
//! patron (a `patron_controls` row), and the rest keep following the
//! default. Owners and Administrators are never limited, and a user who
//! is not active may do none of it.
//!
//! The scope's visibility rules ([`super::PatronScope`]) are separate: a
//! patron's own rules replace the default rules as a whole, while a
//! control override replaces one switch.

use chrono::Utc;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::{document, ConfigActor, ConfigurationDocument};
use crate::control_plane::batch::{self, int, text};
use crate::entities::patron_controls;
use crate::error::{LibraryError, Result};
use crate::models::{UserRecord, UserRole, UserStatus};
use crate::store::LibraryStore;
use crate::EntityKind;

/// Namespace of the cluster document that holds the library's default
/// member controls.
pub const MEMBER_CONTROLS_NAMESPACE: &str = "core.member_controls";

/// Schema version of [`MemberControls`] in the default document and of
/// [`ControlOverrides`] in `patron_controls.controls_json`.
pub const MEMBER_CONTROLS_SCHEMA_VERSION: i64 = 1;

/// One thing a patron may be allowed or refused.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MemberControl {
    /// Connect a store account, or sign in to one again, here or by
    /// bringing it from another library. Accounts already connected keep
    /// working, and an Administrator can still remove them.
    LinkAccounts,
    /// Take copies of the library's files: downloads to a branch or a
    /// clerk, streaming and library-card sync.
    Download,
    /// Change a book's metadata.
    EditMetadata,
    /// Add books to the library by uploading files.
    Upload,
    /// Ask a branch for a book it listed (interlibrary requests).
    RequestBooks,
    /// See prices and store links in Discover and the wishlist.
    SeePrices,
}

impl MemberControl {
    /// Every control, in the order the editors list them.
    pub const ALL: [Self; 6] = [
        Self::LinkAccounts,
        Self::Download,
        Self::EditMetadata,
        Self::Upload,
        Self::RequestBooks,
        Self::SeePrices,
    ];

    /// The control's wire name (`link_accounts`, `download`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LinkAccounts => "link_accounts",
            Self::Download => "download",
            Self::EditMetadata => "edit_metadata",
            Self::Upload => "upload",
            Self::RequestBooks => "request_books",
            Self::SeePrices => "see_prices",
        }
    }

    /// Parses a wire name.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|control| control.as_str() == raw)
    }
}

/// The serde default of every switch: allowed.
fn allowed() -> bool {
    true
}

/// A value for each control: the library's default, or what applies to
/// one user.
// Read by a strict hand-written reader (module `strict`); the serde
// attributes here shape the schema and match what that reader takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MemberControls {
    /// May connect store accounts ([`MemberControl::LinkAccounts`]).
    #[serde(default = "allowed")]
    #[schema(example = false)]
    pub link_accounts: bool,
    /// May take copies of the library's files ([`MemberControl::Download`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub download: bool,
    /// May change metadata ([`MemberControl::EditMetadata`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub edit_metadata: bool,
    /// May upload books ([`MemberControl::Upload`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub upload: bool,
    /// May ask branches for books ([`MemberControl::RequestBooks`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub request_books: bool,
    /// May see prices and store links ([`MemberControl::SeePrices`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub see_prices: bool,
}

impl Default for MemberControls {
    fn default() -> Self {
        Self::all(true)
    }
}

impl MemberControls {
    /// Every control set to `value`.
    #[must_use]
    pub fn all(value: bool) -> Self {
        Self {
            link_accounts: value,
            download: value,
            edit_metadata: value,
            upload: value,
            request_books: value,
            see_prices: value,
        }
    }

    /// Whether `control` is allowed.
    #[must_use]
    pub fn allows(&self, control: MemberControl) -> bool {
        match control {
            MemberControl::LinkAccounts => self.link_accounts,
            MemberControl::Download => self.download,
            MemberControl::EditMetadata => self.edit_metadata,
            MemberControl::Upload => self.upload,
            MemberControl::RequestBooks => self.request_books,
            MemberControl::SeePrices => self.see_prices,
        }
    }

    /// Sets `control` to `value`.
    pub fn set(&mut self, control: MemberControl, value: bool) {
        let slot = match control {
            MemberControl::LinkAccounts => &mut self.link_accounts,
            MemberControl::Download => &mut self.download,
            MemberControl::EditMetadata => &mut self.edit_metadata,
            MemberControl::Upload => &mut self.upload,
            MemberControl::RequestBooks => &mut self.request_books,
            MemberControl::SeePrices => &mut self.see_prices,
        };
        *slot = value;
    }
}

/// One patron's overrides: a value for some controls, and the library's
/// default for the rest (an absent or `null` field).
// Read by a strict hand-written reader (module `strict`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlOverrides {
    /// Overrides [`MemberControls::link_accounts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub link_accounts: Option<bool>,
    /// Overrides [`MemberControls::download`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub download: Option<bool>,
    /// Overrides [`MemberControls::edit_metadata`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub edit_metadata: Option<bool>,
    /// Overrides [`MemberControls::upload`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub upload: Option<bool>,
    /// Overrides [`MemberControls::request_books`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub request_books: Option<bool>,
    /// Overrides [`MemberControls::see_prices`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub see_prices: Option<bool>,
}

impl ControlOverrides {
    /// The override of `control`, if any.
    #[must_use]
    pub fn get(&self, control: MemberControl) -> Option<bool> {
        match control {
            MemberControl::LinkAccounts => self.link_accounts,
            MemberControl::Download => self.download,
            MemberControl::EditMetadata => self.edit_metadata,
            MemberControl::Upload => self.upload,
            MemberControl::RequestBooks => self.request_books,
            MemberControl::SeePrices => self.see_prices,
        }
    }

    /// Sets or clears (`None`) the override of `control`.
    pub fn set(&mut self, control: MemberControl, value: Option<bool>) {
        let slot = match control {
            MemberControl::LinkAccounts => &mut self.link_accounts,
            MemberControl::Download => &mut self.download,
            MemberControl::EditMetadata => &mut self.edit_metadata,
            MemberControl::Upload => &mut self.upload,
            MemberControl::RequestBooks => &mut self.request_books,
            MemberControl::SeePrices => &mut self.see_prices,
        };
        *slot = value;
    }

    /// True when no control is overridden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        MemberControl::ALL
            .into_iter()
            .all(|control| self.get(control).is_none())
    }

    /// `defaults` with these overrides applied.
    #[must_use]
    pub fn apply(&self, defaults: MemberControls) -> MemberControls {
        let mut controls = defaults;
        for control in MemberControl::ALL {
            if let Some(value) = self.get(control) {
                controls.set(control, value);
            }
        }
        controls
    }
}

/// A user's controls: the library's default, their overrides, and what
/// applies to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignedControls {
    /// The library's default.
    pub defaults: MemberControls,
    /// The user's own overrides.
    pub overrides: ControlOverrides,
    /// What applies to the user: everything for an active Owner or
    /// Administrator, nothing for a user who is not active, and the
    /// default with the overrides applied for an active Member.
    pub effective: MemberControls,
}

/// Loads the library's default member controls document.
///
/// # Returns
///
/// The stored document, or `None` when nobody has written one: every
/// control is then allowed.
///
/// # Errors
///
/// Returns the read error, or an error when the stored document is invalid
/// or of an unknown schema.
pub async fn load_default_member_controls(
    store: &LibraryStore,
) -> Result<Option<ConfigurationDocument<MemberControls>>> {
    load_document(store, &DocumentKey::cluster(MEMBER_CONTROLS_NAMESPACE))
        .await?
        .map(parse_default)
        .transpose()
}

/// The library's default member controls: the stored document, or every
/// control allowed.
///
/// # Errors
///
/// Returns the read or parse error.
pub async fn default_member_controls(store: &LibraryStore) -> Result<MemberControls> {
    Ok(load_default_member_controls(store)
        .await?
        .map(|document| document.body)
        .unwrap_or_default())
}

/// Replaces the library's default member controls under compare-and-swap,
/// creating the document when absent and retrying a few times on a
/// concurrent edit.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - Who is writing, for authorization and the audit log.
/// * `controls` - The new default.
///
/// # Errors
///
/// Returns an error when the actor may not write, or
/// [`LibraryError::Conflict`] when the document keeps changing underneath.
pub async fn set_default_member_controls(
    store: &LibraryStore,
    actor: &ConfigActor,
    controls: MemberControls,
) -> Result<ConfigurationDocument<MemberControls>> {
    let key = DocumentKey::cluster(MEMBER_CONTROLS_NAMESPACE);
    for attempt in 0..4 {
        let current = match load_default_member_controls(store).await? {
            Some(document) => document,
            None => parse_default(
                import_if_absent(
                    store,
                    actor,
                    &key,
                    MEMBER_CONTROLS_SCHEMA_VERSION,
                    &to_json(&MemberControls::default())?,
                    "core-member-controls-default",
                )
                .await?,
            )?,
        };
        if current.body == controls {
            return Ok(current);
        }
        let operation_id = format!(
            "core-member-controls-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            MEMBER_CONTROLS_SCHEMA_VERSION,
            current.revision,
            &to_json(&controls)?,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => return parse_default(stored),
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(
        "core.member_controls kept changing; retry".into(),
    ))
}

/// The overrides stored for `user_id` (none when there is no row).
///
/// # Errors
///
/// Returns the read error, or an error when the stored row is invalid.
pub async fn member_control_overrides(
    store: &LibraryStore,
    user_id: i64,
) -> Result<ControlOverrides> {
    patron_controls::Entity::find_by_id(user_id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map_or_else(|| Ok(ControlOverrides::default()), |row| parse_row(&row))
}

/// The controls of `user`: the library's default, their overrides, and
/// what applies to them.
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub async fn assigned_member_controls(
    store: &LibraryStore,
    user: &UserRecord,
) -> Result<AssignedControls> {
    let defaults = default_member_controls(store).await?;
    let overrides = member_control_overrides(store, user.id).await?;
    let effective = effective_controls(user, defaults, overrides);
    Ok(AssignedControls {
        defaults,
        overrides,
        effective,
    })
}

/// Whether member controls ever limit a role: Members, Hosts and
/// Administrators can be limited; Owners never are.
///
/// An Administrator's controls are set only by an Owner or the operator,
/// because an Administrator may manage Members alone.
#[must_use]
pub fn role_has_controls(role: UserRole) -> bool {
    match role {
        UserRole::Member | UserRole::Host | UserRole::Administrator => true,
        UserRole::Owner => false,
    }
}

/// Whether a role follows the library's default controls: Members and
/// Hosts do. An Administrator may do everything except the controls set
/// for them alone, so a default meant for patrons never limits staff.
#[must_use]
pub fn role_follows_default_controls(role: UserRole) -> bool {
    match role {
        UserRole::Member | UserRole::Host => true,
        UserRole::Administrator | UserRole::Owner => false,
    }
}

/// What applies to `user` given the library's default and their
/// overrides: nothing for a user who is not active, everything for an
/// active Owner, the overrides over "everything allowed" for an active
/// Administrator, and the overrides over the default for an active Member.
#[must_use]
pub fn effective_controls(
    user: &UserRecord,
    defaults: MemberControls,
    overrides: ControlOverrides,
) -> MemberControls {
    if user.status != UserStatus::Active {
        MemberControls::all(false)
    } else if role_follows_default_controls(user.role) {
        overrides.apply(defaults)
    } else if role_has_controls(user.role) {
        overrides.apply(MemberControls::all(true))
    } else {
        MemberControls::all(true)
    }
}

/// Whether `user_id` may do `control` here. An unknown user may not.
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub async fn user_may(store: &LibraryStore, user_id: i64, control: MemberControl) -> Result<bool> {
    let Some(user) = store.get_user(user_id).await? else {
        return Ok(false);
    };
    Ok(assigned_member_controls(store, &user)
        .await?
        .effective
        .allows(control))
}

/// Replaces the overrides of `user_id`; with none, the user follows the
/// library's default for every control again.
///
/// The caller has already checked that the actor may manage the user. The
/// write is guarded on the role read here ([`set_member_control_overrides_for`]).
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] for an unknown user, or the errors
/// of [`set_member_control_overrides_for`].
pub async fn set_member_control_overrides(
    store: &LibraryStore,
    user_id: i64,
    overrides: ControlOverrides,
) -> Result<AssignedControls> {
    let Some(user) = store.get_user(user_id).await? else {
        return Err(LibraryError::not_found(EntityKind::User, user_id));
    };
    set_member_control_overrides_for(store, &user, overrides).await
}

/// Replaces the overrides of `user`, as the caller read them when it
/// checked that the actor may manage them; with none, the user follows the
/// library's default for every control again.
///
/// One atomic batch updates the stored row, inserts it when there is none,
/// or deletes it when no control is overridden. Every statement is guarded
/// on the user still having the role in `user`, so a role change made after
/// the caller authorized the write (an Owner promoting a Member to
/// Administrator, say) stops it rather than letting it through under the
/// old authorization.
///
/// # Errors
///
/// Returns [`LibraryError::Conflict`] when the user's role changed since
/// `user` was read, even when the stored overrides already matched (the
/// caller authorized against a role the user no longer has, so it reads
/// again), [`LibraryError::NotFound`] when
/// the user is gone, or the read or write error.
pub async fn set_member_control_overrides_for(
    store: &LibraryStore,
    user: &UserRecord,
    overrides: ControlOverrides,
) -> Result<AssignedControls> {
    let user_id = user.id;
    let role = user.role.as_str();
    let same_role = "EXISTS (SELECT 1 FROM users WHERE id = ? AND role = ?)";
    let now = Utc::now().to_rfc3339();
    let operation_id = format!(
        "patron-controls-{user_id}-{}",
        uuid::Uuid::new_v4().simple()
    );
    let statements = if overrides.is_empty() {
        vec![batch::exec(
            &format!("DELETE FROM patron_controls WHERE user_id = ? AND {same_role}"),
            vec![int(user_id), int(user_id), text(role)],
        )]
    } else {
        let json = serde_json::to_string(&overrides).map_err(|err| invalid(&err.to_string()))?;
        vec![
            batch::exec(
                &format!(
                    "UPDATE patron_controls SET controls_json = ?, updated_at = ? \
                     WHERE user_id = ? AND {same_role}"
                ),
                vec![
                    text(&json),
                    text(&now),
                    int(user_id),
                    int(user_id),
                    text(role),
                ],
            ),
            batch::exec(
                &format!(
                    "INSERT OR IGNORE INTO patron_controls (user_id, controls_json, updated_at) \
                     SELECT ?, ?, ? WHERE {same_role}"
                ),
                vec![
                    int(user_id),
                    text(&json),
                    text(&now),
                    int(user_id),
                    text(role),
                ],
            ),
        ]
    };
    store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    let Some(current) = store.get_user(user_id).await? else {
        return Err(LibraryError::not_found(EntityKind::User, user_id));
    };
    if current.role != user.role {
        return Err(LibraryError::Conflict(format!(
            "user {user_id} became {} while their controls were being set; read them again",
            current.role.as_str()
        )));
    }
    assigned_member_controls(store, &current).await
}

/// The stored state one decision on a control was made from, which a host
/// batch re-checks so that a change made while the batch was being built
/// stops it ([`control_guard`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlGuard {
    /// The user (`users.id`).
    user_id: i64,
    /// The user's stored role.
    role: String,
    /// The user's stored overrides, or empty when they have no row.
    overrides_json: String,
    /// Revision of the default document, or 0 when there is none.
    default_revision: i64,
}

impl ControlGuard {
    /// SQL condition, true while the user is active and their role, their
    /// overrides and the library's default are as they were read. Bind
    /// [`ControlGuard::values`] to it in order.
    pub(crate) const SQL: &'static str =
        "EXISTS (SELECT 1 FROM users WHERE id = ? AND status = 'active' AND role = ?) \
         AND COALESCE((SELECT controls_json FROM patron_controls WHERE user_id = ?), '') = ? \
         AND COALESCE((SELECT revision FROM configuration_documents \
         WHERE scope_type = ? AND scope_id = ? AND namespace = ?), 0) = ?";

    /// The values [`ControlGuard::SQL`] binds.
    pub(crate) fn values(&self) -> Vec<bookclerk_plugin_abi::DbValue> {
        vec![
            int(self.user_id),
            text(&self.role),
            int(self.user_id),
            text(&self.overrides_json),
            text(documents::CLUSTER_SCOPE_TYPE),
            text(documents::CLUSTER_SCOPE_ID),
            text(MEMBER_CONTROLS_NAMESPACE),
            int(self.default_revision),
        ]
    }
}

/// Reads whether `user_id` may do `control` and the state that decision
/// rests on.
///
/// # Returns
///
/// The guard when the user may, to re-check inside the write that acts on
/// it; `None` when they may not (an unknown or inactive user, or a
/// control turned off for them).
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub(crate) async fn control_guard(
    store: &LibraryStore,
    user_id: i64,
    control: MemberControl,
) -> Result<Option<ControlGuard>> {
    let Some(user) = store.get_user(user_id).await? else {
        return Ok(None);
    };
    if user.status != UserStatus::Active {
        return Ok(None);
    }
    let default = load_default_member_controls(store).await?;
    let row = patron_controls::Entity::find_by_id(user_id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let overrides = row
        .as_ref()
        .map_or_else(|| Ok(ControlOverrides::default()), parse_row)?;
    let defaults = default
        .as_ref()
        .map(|document| document.body)
        .unwrap_or_default();
    let allowed = effective_controls(&user, defaults, overrides).allows(control);
    Ok(allowed.then(|| ControlGuard {
        user_id,
        role: user.role.as_str().to_string(),
        overrides_json: row.map(|row| row.controls_json).unwrap_or_default(),
        default_revision: default.map_or(0, |document| document.revision),
    }))
}

/// Parses a stored default document, failing closed on an unknown schema.
fn parse_default(stored: StoredDocument) -> Result<ConfigurationDocument<MemberControls>> {
    if stored.schema_version != MEMBER_CONTROLS_SCHEMA_VERSION {
        return Err(documents::unsupported(
            MEMBER_CONTROLS_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: MemberControls =
        serde_json::from_str(&stored.document_json).map_err(|err| invalid(&err.to_string()))?;
    Ok(document(stored, body))
}

/// Parses a stored patron's overrides.
fn parse_row(row: &patron_controls::Model) -> Result<ControlOverrides> {
    serde_json::from_str(&row.controls_json).map_err(|err| invalid(&err.to_string()))
}

/// Serializes the default for storage.
fn to_json(controls: &MemberControls) -> Result<String> {
    serde_json::to_string(controls).map_err(|err| invalid(&err.to_string()))
}

/// Error for a document or row that does not parse.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid member controls: {detail}"))
}

/// `PUT /api/users/{id}/controls` body.
// Read by a strict hand-written reader (module `strict`); the serde
// attribute here shapes the schema and matches what that reader takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutUserControls {
    /// The controls set for this patron alone; an absent or `null`
    /// control follows the library's default, so `{}` returns the patron
    /// to the default for all of them.
    pub overrides: ControlOverrides,
}

/// Parses the body of `PUT /api/users/default-controls` from its raw
/// bytes, sans IO, the way the route reads it.
///
/// # Returns
///
/// The new default, with an absent control allowed.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not one JSON
/// object of known controls with boolean values and no repeated key.
pub fn parse_member_controls(body: &[u8]) -> Result<MemberControls> {
    serde_json::from_slice(body).map_err(|err| invalid(&format!("controls body: {err}")))
}

/// Parses the body of `PUT /api/users/{id}/controls` from its raw bytes,
/// sans IO, the way the route reads it.
///
/// # Returns
///
/// The patron's overrides, with an absent or `null` control following
/// the default.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not one JSON
/// object with exactly the key `overrides`, itself an object of known
/// controls with boolean or `null` values, with no repeated key.
pub fn parse_user_controls_request(body: &[u8]) -> Result<ControlOverrides> {
    serde_json::from_slice::<PutUserControls>(body)
        .map(|body| body.overrides)
        .map_err(|err| invalid(&format!("user controls body: {err}")))
}

/// Strict JSON readers for the controls types.
///
/// A derived reader would also take a struct written as an array
/// (`[true, false, true, true, true, true]`). Controls come from
/// Administrators over the web API and decide what patrons may do, so the
/// readers here take only objects, with no unknown or repeated keys. The
/// `member_controls_request` fuzz target checks them against an
/// independent reading.
mod strict {
    use std::fmt;
    use std::marker::PhantomData;

    use serde::de::{value::MapAccessDeserializer, Deserializer, MapAccess, Visitor};
    use serde::Deserialize;

    use super::{allowed, ControlOverrides, MemberControls, PutUserControls};

    /// Reads a JSON object, and only an object, as `T`.
    struct Object<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }

    /// [`MemberControls`] as the derived reader takes it from an object.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawControls {
        /// See [`MemberControls::link_accounts`].
        #[serde(default = "allowed")]
        link_accounts: bool,
        /// See [`MemberControls::download`].
        #[serde(default = "allowed")]
        download: bool,
        /// See [`MemberControls::edit_metadata`].
        #[serde(default = "allowed")]
        edit_metadata: bool,
        /// See [`MemberControls::upload`].
        #[serde(default = "allowed")]
        upload: bool,
        /// See [`MemberControls::request_books`].
        #[serde(default = "allowed")]
        request_books: bool,
        /// See [`MemberControls::see_prices`].
        #[serde(default = "allowed")]
        see_prices: bool,
    }

    impl<'de> Deserialize<'de> for MemberControls {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            let raw: RawControls = de.deserialize_map(Object(PhantomData))?;
            Ok(Self {
                link_accounts: raw.link_accounts,
                download: raw.download,
                edit_metadata: raw.edit_metadata,
                upload: raw.upload,
                request_books: raw.request_books,
                see_prices: raw.see_prices,
            })
        }
    }

    /// [`ControlOverrides`] as the derived reader takes it from an object.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawOverrides {
        /// See [`ControlOverrides::link_accounts`].
        #[serde(default)]
        link_accounts: Option<bool>,
        /// See [`ControlOverrides::download`].
        #[serde(default)]
        download: Option<bool>,
        /// See [`ControlOverrides::edit_metadata`].
        #[serde(default)]
        edit_metadata: Option<bool>,
        /// See [`ControlOverrides::upload`].
        #[serde(default)]
        upload: Option<bool>,
        /// See [`ControlOverrides::request_books`].
        #[serde(default)]
        request_books: Option<bool>,
        /// See [`ControlOverrides::see_prices`].
        #[serde(default)]
        see_prices: Option<bool>,
    }

    impl<'de> Deserialize<'de> for ControlOverrides {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            let raw: RawOverrides = de.deserialize_map(Object(PhantomData))?;
            Ok(Self {
                link_accounts: raw.link_accounts,
                download: raw.download,
                edit_metadata: raw.edit_metadata,
                upload: raw.upload,
                request_books: raw.request_books,
                see_prices: raw.see_prices,
            })
        }
    }

    /// [`PutUserControls`] as the derived reader takes it from an object.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawPutUserControls {
        /// See [`PutUserControls::overrides`].
        overrides: ControlOverrides,
    }

    impl<'de> Deserialize<'de> for PutUserControls {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            let raw: RawPutUserControls = de.deserialize_map(Object(PhantomData))?;
            Ok(Self {
                overrides: raw.overrides,
            })
        }
    }
}

#[cfg(test)]
#[path = "member_controls_tests.rs"]
mod tests;
