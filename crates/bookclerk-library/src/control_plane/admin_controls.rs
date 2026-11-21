//! Administrator switches: what an Administrator may do in the library,
//! set only by an Owner or the operator (see
//! `docs/adr/owner-and-operator.md`).
//!
//! Seven switches, each on out of the box ([`AdminControl`]). The library's
//! default (the `core.admin_controls` library document) sets every switch
//! for all Administrators; an Owner or the operator can override single
//! switches for one Administrator (an `admin_controls` row), and the rest
//! keep following the default. An Owner is never limited, a user who is
//! not active may do none of it, and Members never had these powers.

use chrono::Utc;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::{document, ConfigActor, ConfigurationDocument};
use crate::control_plane::batch::{self, int, text};
use crate::entities::admin_controls;
use crate::error::{LibraryError, Result};
use crate::models::{UserRecord, UserRole, UserStatus};
use crate::store::LibraryStore;
use crate::EntityKind;

/// Namespace of the library document that holds the library's default
/// Administrator switches.
pub const ADMIN_CONTROLS_NAMESPACE: &str = "core.admin_controls";

/// Schema version of [`AdminControls`] in the default document and of
/// [`AdminControlOverrides`] in `admin_controls.controls_json`.
pub const ADMIN_CONTROLS_SCHEMA_VERSION: i64 = 1;

/// One power an Administrator may be allowed or refused.
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
pub enum AdminControl {
    /// Invite Members, change their details, disable or delete them, reset
    /// their passwords and mint claim tickets. Off, the Administrator still
    /// sees the user list.
    ManageMembers,
    /// Set patron scopes and member controls, for one Member and the
    /// library's defaults. Off, the Administrator still reads them.
    SetPatronLimits,
    /// Scan and acquire for every store account, plus the legacy `/scan`
    /// and `/acquire`. Off, the Administrator scans and acquires only with
    /// the store accounts linked to them, like a Member.
    LibraryScan,
    /// Cancel jobs; retry, acknowledge, cancel or resume event deliveries;
    /// run a listening sync. Off, the Administrator still watches jobs and
    /// queues.
    RunJobs,
    /// Pin listings, split works and resolve conflicts.
    FixCatalog,
    /// Mint enrollment links for their own new nodes. Nodes they already
    /// own keep running.
    InviteBranches,
    /// Approve interlibrary requests from branches and Members. Off,
    /// requests wait for an Owner.
    ApproveRequests,
}

impl AdminControl {
    /// Every switch, in the order the editors list them.
    pub const ALL: [Self; 7] = [
        Self::ManageMembers,
        Self::SetPatronLimits,
        Self::LibraryScan,
        Self::RunJobs,
        Self::FixCatalog,
        Self::InviteBranches,
        Self::ApproveRequests,
    ];

    /// The switch's wire name (`manage_members`, `library_scan`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManageMembers => "manage_members",
            Self::SetPatronLimits => "set_patron_limits",
            Self::LibraryScan => "library_scan",
            Self::RunJobs => "run_jobs",
            Self::FixCatalog => "fix_catalog",
            Self::InviteBranches => "invite_branches",
            Self::ApproveRequests => "approve_requests",
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

/// What a change does to one switch of one Administrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AdminSwitchChoice {
    /// Allow the switch for this Administrator, whatever the default says.
    On,
    /// Refuse the switch for this Administrator, whatever the default says.
    Off,
    /// Follow the library's default again.
    Default,
}

impl AdminSwitchChoice {
    /// The stored override: `Some(true)`, `Some(false)`, or `None` to
    /// follow the default.
    #[must_use]
    pub fn value(self) -> Option<bool> {
        match self {
            Self::On => Some(true),
            Self::Off => Some(false),
            Self::Default => None,
        }
    }
}

/// Changes to some of the library's default switches: a new value for
/// each named switch; an absent switch keeps its stored value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultSwitchChanges {
    /// New default for [`AdminControl::ManageMembers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub manage_members: Option<bool>,
    /// New default for [`AdminControl::SetPatronLimits`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub set_patron_limits: Option<bool>,
    /// New default for [`AdminControl::LibraryScan`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub library_scan: Option<bool>,
    /// New default for [`AdminControl::RunJobs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub run_jobs: Option<bool>,
    /// New default for [`AdminControl::FixCatalog`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub fix_catalog: Option<bool>,
    /// New default for [`AdminControl::InviteBranches`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub invite_branches: Option<bool>,
    /// New default for [`AdminControl::ApproveRequests`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub approve_requests: Option<bool>,
}

impl DefaultSwitchChanges {
    /// The change named for `control`, if any.
    #[must_use]
    pub fn get(&self, control: AdminControl) -> Option<bool> {
        match control {
            AdminControl::ManageMembers => self.manage_members,
            AdminControl::SetPatronLimits => self.set_patron_limits,
            AdminControl::LibraryScan => self.library_scan,
            AdminControl::RunJobs => self.run_jobs,
            AdminControl::FixCatalog => self.fix_catalog,
            AdminControl::InviteBranches => self.invite_branches,
            AdminControl::ApproveRequests => self.approve_requests,
        }
    }

    /// Names (or, with `None`, stops naming) a change for `control`.
    pub fn set(&mut self, control: AdminControl, value: Option<bool>) {
        let slot = match control {
            AdminControl::ManageMembers => &mut self.manage_members,
            AdminControl::SetPatronLimits => &mut self.set_patron_limits,
            AdminControl::LibraryScan => &mut self.library_scan,
            AdminControl::RunJobs => &mut self.run_jobs,
            AdminControl::FixCatalog => &mut self.fix_catalog,
            AdminControl::InviteBranches => &mut self.invite_branches,
            AdminControl::ApproveRequests => &mut self.approve_requests,
        };
        *slot = value;
    }

    /// `control` changed to `value`, for building a change.
    #[must_use]
    pub fn with(mut self, control: AdminControl, value: bool) -> Self {
        self.set(control, Some(value));
        self
    }
}

/// Changes to some of one Administrator's overrides: `on` or `off` for
/// them alone, or `default` to follow the library's default again; an
/// absent switch keeps what it had.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OverrideSwitchChanges {
    /// Change to [`AdminControl::ManageMembers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub manage_members: Option<AdminSwitchChoice>,
    /// Change to [`AdminControl::SetPatronLimits`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub set_patron_limits: Option<AdminSwitchChoice>,
    /// Change to [`AdminControl::LibraryScan`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "off")]
    pub library_scan: Option<AdminSwitchChoice>,
    /// Change to [`AdminControl::RunJobs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub run_jobs: Option<AdminSwitchChoice>,
    /// Change to [`AdminControl::FixCatalog`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub fix_catalog: Option<AdminSwitchChoice>,
    /// Change to [`AdminControl::InviteBranches`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub invite_branches: Option<AdminSwitchChoice>,
    /// Change to [`AdminControl::ApproveRequests`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub approve_requests: Option<AdminSwitchChoice>,
}

impl OverrideSwitchChanges {
    /// The change named for `control`, if any.
    #[must_use]
    pub fn get(&self, control: AdminControl) -> Option<AdminSwitchChoice> {
        match control {
            AdminControl::ManageMembers => self.manage_members,
            AdminControl::SetPatronLimits => self.set_patron_limits,
            AdminControl::LibraryScan => self.library_scan,
            AdminControl::RunJobs => self.run_jobs,
            AdminControl::FixCatalog => self.fix_catalog,
            AdminControl::InviteBranches => self.invite_branches,
            AdminControl::ApproveRequests => self.approve_requests,
        }
    }

    /// Names (or, with `None`, stops naming) a change for `control`.
    pub fn set(&mut self, control: AdminControl, value: Option<AdminSwitchChoice>) {
        let slot = match control {
            AdminControl::ManageMembers => &mut self.manage_members,
            AdminControl::SetPatronLimits => &mut self.set_patron_limits,
            AdminControl::LibraryScan => &mut self.library_scan,
            AdminControl::RunJobs => &mut self.run_jobs,
            AdminControl::FixCatalog => &mut self.fix_catalog,
            AdminControl::InviteBranches => &mut self.invite_branches,
            AdminControl::ApproveRequests => &mut self.approve_requests,
        };
        *slot = value;
    }

    /// `control` changed to `value`, for building a change.
    #[must_use]
    pub fn with(mut self, control: AdminControl, value: AdminSwitchChoice) -> Self {
        self.set(control, Some(value));
        self
    }
}

/// The serde default of every switch: allowed.
fn allowed() -> bool {
    true
}

/// A value for each switch: the library's default, or what applies to one
/// Administrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminControls {
    /// May manage Members ([`AdminControl::ManageMembers`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub manage_members: bool,
    /// May set patron limits ([`AdminControl::SetPatronLimits`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub set_patron_limits: bool,
    /// May scan and acquire library-wide ([`AdminControl::LibraryScan`]).
    #[serde(default = "allowed")]
    #[schema(example = false)]
    pub library_scan: bool,
    /// May run the job queue ([`AdminControl::RunJobs`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub run_jobs: bool,
    /// May fix the catalog ([`AdminControl::FixCatalog`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub fix_catalog: bool,
    /// May invite branches ([`AdminControl::InviteBranches`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub invite_branches: bool,
    /// May approve interlibrary requests ([`AdminControl::ApproveRequests`]).
    #[serde(default = "allowed")]
    #[schema(example = true)]
    pub approve_requests: bool,
}

impl Default for AdminControls {
    fn default() -> Self {
        Self::all(true)
    }
}

impl AdminControls {
    /// Every switch set to `value`.
    #[must_use]
    pub fn all(value: bool) -> Self {
        Self {
            manage_members: value,
            set_patron_limits: value,
            library_scan: value,
            run_jobs: value,
            fix_catalog: value,
            invite_branches: value,
            approve_requests: value,
        }
    }

    /// Whether `control` is allowed.
    #[must_use]
    pub fn allows(&self, control: AdminControl) -> bool {
        match control {
            AdminControl::ManageMembers => self.manage_members,
            AdminControl::SetPatronLimits => self.set_patron_limits,
            AdminControl::LibraryScan => self.library_scan,
            AdminControl::RunJobs => self.run_jobs,
            AdminControl::FixCatalog => self.fix_catalog,
            AdminControl::InviteBranches => self.invite_branches,
            AdminControl::ApproveRequests => self.approve_requests,
        }
    }

    /// Sets `control` to `value`.
    pub fn set(&mut self, control: AdminControl, value: bool) {
        let slot = match control {
            AdminControl::ManageMembers => &mut self.manage_members,
            AdminControl::SetPatronLimits => &mut self.set_patron_limits,
            AdminControl::LibraryScan => &mut self.library_scan,
            AdminControl::RunJobs => &mut self.run_jobs,
            AdminControl::FixCatalog => &mut self.fix_catalog,
            AdminControl::InviteBranches => &mut self.invite_branches,
            AdminControl::ApproveRequests => &mut self.approve_requests,
        };
        *slot = value;
    }
}

/// One Administrator's overrides: a value for some switches, and the
/// library's default for the rest (an absent or `null` field).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminControlOverrides {
    /// Overrides [`AdminControls::manage_members`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub manage_members: Option<bool>,
    /// Overrides [`AdminControls::set_patron_limits`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub set_patron_limits: Option<bool>,
    /// Overrides [`AdminControls::library_scan`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub library_scan: Option<bool>,
    /// Overrides [`AdminControls::run_jobs`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub run_jobs: Option<bool>,
    /// Overrides [`AdminControls::fix_catalog`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub fix_catalog: Option<bool>,
    /// Overrides [`AdminControls::invite_branches`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub invite_branches: Option<bool>,
    /// Overrides [`AdminControls::approve_requests`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub approve_requests: Option<bool>,
}

impl AdminControlOverrides {
    /// The override of `control`, if any.
    #[must_use]
    pub fn get(&self, control: AdminControl) -> Option<bool> {
        match control {
            AdminControl::ManageMembers => self.manage_members,
            AdminControl::SetPatronLimits => self.set_patron_limits,
            AdminControl::LibraryScan => self.library_scan,
            AdminControl::RunJobs => self.run_jobs,
            AdminControl::FixCatalog => self.fix_catalog,
            AdminControl::InviteBranches => self.invite_branches,
            AdminControl::ApproveRequests => self.approve_requests,
        }
    }

    /// Sets or clears (`None`) the override of `control`.
    pub fn set(&mut self, control: AdminControl, value: Option<bool>) {
        let slot = match control {
            AdminControl::ManageMembers => &mut self.manage_members,
            AdminControl::SetPatronLimits => &mut self.set_patron_limits,
            AdminControl::LibraryScan => &mut self.library_scan,
            AdminControl::RunJobs => &mut self.run_jobs,
            AdminControl::FixCatalog => &mut self.fix_catalog,
            AdminControl::InviteBranches => &mut self.invite_branches,
            AdminControl::ApproveRequests => &mut self.approve_requests,
        };
        *slot = value;
    }

    /// True when no switch is overridden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        AdminControl::ALL
            .into_iter()
            .all(|control| self.get(control).is_none())
    }

    /// `defaults` with these overrides applied.
    #[must_use]
    pub fn apply(&self, defaults: AdminControls) -> AdminControls {
        let mut controls = defaults;
        for control in AdminControl::ALL {
            if let Some(value) = self.get(control) {
                controls.set(control, value);
            }
        }
        controls
    }
}

/// A user's switches: the library's default, their overrides, and what
/// applies to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignedAdminControls {
    /// The library's default.
    pub defaults: AdminControls,
    /// The user's own overrides.
    pub overrides: AdminControlOverrides,
    /// What applies to the user: everything for an active Owner, the
    /// default with the overrides applied for an active Administrator, and
    /// nothing for anyone else.
    pub effective: AdminControls,
}

/// Loads the library's default Administrator switches document.
///
/// # Returns
///
/// The stored document, or `None` when nobody has written one: every
/// switch is then on.
///
/// # Errors
///
/// Returns the read error, or an error when the stored document is invalid
/// or of an unknown schema.
pub async fn load_default_admin_controls(
    store: &LibraryStore,
) -> Result<Option<ConfigurationDocument<AdminControls>>> {
    load_document(store, &DocumentKey::library(ADMIN_CONTROLS_NAMESPACE))
        .await?
        .map(parse_default)
        .transpose()
}

/// The library's default Administrator switches: the stored document, or
/// every switch on.
///
/// # Errors
///
/// Returns the read or parse error.
pub async fn default_admin_controls(store: &LibraryStore) -> Result<AdminControls> {
    Ok(load_default_admin_controls(store)
        .await?
        .map(|document| document.body)
        .unwrap_or_default())
}

/// Replaces the library's default Administrator switches under
/// compare-and-swap, creating the document when absent and retrying a few
/// times on a concurrent edit.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - Who is writing: the operator or a verified Owner
///   ([`super::verify_library_owner`]).
/// * `controls` - The new default.
///
/// # Errors
///
/// Returns an error when the actor may not write, or
/// [`LibraryError::Conflict`] when the document keeps changing underneath.
pub async fn set_default_admin_controls(
    store: &LibraryStore,
    actor: &ConfigActor,
    controls: AdminControls,
) -> Result<ConfigurationDocument<AdminControls>> {
    write_default(store, actor, |_| controls).await
}

/// Changes only the named default switches under compare-and-swap: each
/// attempt applies `changes` to the stored default it read, so a
/// concurrent edit of another switch is kept rather than overwritten.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - Who is writing: the operator or a verified Owner
///   ([`super::verify_library_owner`]).
/// * `changes` - The switches to change and their new values.
///
/// # Errors
///
/// Returns an error when the actor may not write, or
/// [`LibraryError::Conflict`] when the document keeps changing underneath.
pub async fn change_default_admin_controls(
    store: &LibraryStore,
    actor: &ConfigActor,
    changes: &DefaultSwitchChanges,
) -> Result<ConfigurationDocument<AdminControls>> {
    write_default(store, actor, |mut controls| {
        for control in AdminControl::ALL {
            if let Some(value) = changes.get(control) {
                controls.set(control, value);
            }
        }
        controls
    })
    .await
}

/// Writes the default that `next` makes of the stored one, under
/// compare-and-swap, creating the document when absent and retrying a few
/// times on a concurrent edit.
async fn write_default(
    store: &LibraryStore,
    actor: &ConfigActor,
    next: impl Fn(AdminControls) -> AdminControls,
) -> Result<ConfigurationDocument<AdminControls>> {
    let key = DocumentKey::library(ADMIN_CONTROLS_NAMESPACE);
    for attempt in 0..4 {
        let current = match load_default_admin_controls(store).await? {
            Some(document) => document,
            None => parse_default(
                import_if_absent(
                    store,
                    actor,
                    &key,
                    ADMIN_CONTROLS_SCHEMA_VERSION,
                    &to_json(&AdminControls::default())?,
                    "core-admin-controls-default",
                )
                .await?,
            )?,
        };
        let controls = next(current.body);
        if current.body == controls {
            return Ok(current);
        }
        let operation_id = format!(
            "core-admin-controls-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            ADMIN_CONTROLS_SCHEMA_VERSION,
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
        "core.admin_controls kept changing; retry".into(),
    ))
}

/// The overrides stored for `user_id` (none when there is no row).
///
/// # Errors
///
/// Returns the read error, or an error when the stored row is invalid.
pub async fn admin_control_overrides(
    store: &LibraryStore,
    user_id: i64,
) -> Result<AdminControlOverrides> {
    admin_controls::Entity::find_by_id(user_id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
        .map_or_else(
            || Ok(AdminControlOverrides::default()),
            |row| parse_row(&row),
        )
}

/// The switches of `user`: the library's default, their overrides, and
/// what applies to them.
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub async fn assigned_admin_controls(
    store: &LibraryStore,
    user: &UserRecord,
) -> Result<AssignedAdminControls> {
    let defaults = default_admin_controls(store).await?;
    let overrides = admin_control_overrides(store, user.id).await?;
    let effective = effective_admin_controls(user, defaults, overrides);
    Ok(AssignedAdminControls {
        defaults,
        overrides,
        effective,
    })
}

/// What applies to `user` given the library's default and their
/// overrides: everything for an active Owner, the overrides over the
/// default for an active Administrator, and nothing for anyone else (a
/// user who is not active or a Member).
#[must_use]
pub fn effective_admin_controls(
    user: &UserRecord,
    defaults: AdminControls,
    overrides: AdminControlOverrides,
) -> AdminControls {
    if user.status != UserStatus::Active {
        return AdminControls::all(false);
    }
    match user.role {
        UserRole::Owner => AdminControls::all(true),
        UserRole::Administrator => overrides.apply(defaults),
        UserRole::Member => AdminControls::all(false),
    }
}

/// Whether `user_id` may use `control` here. An unknown user may not.
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub async fn admin_may(store: &LibraryStore, user_id: i64, control: AdminControl) -> Result<bool> {
    let Some(user) = store.get_user(user_id).await? else {
        return Ok(false);
    };
    Ok(assigned_admin_controls(store, &user)
        .await?
        .effective
        .allows(control))
}

/// Whether an Administrator's switch lets `user_id` do `control`, for an
/// action other roles are allowed or refused by their own rules: true for
/// any role but Administrator (and for an active Administrator whose
/// switch is on), false for an unknown user.
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub async fn administrator_switch_allows(
    store: &LibraryStore,
    user_id: i64,
    control: AdminControl,
) -> Result<bool> {
    let Some(user) = store.get_user(user_id).await? else {
        return Ok(false);
    };
    match user.role {
        UserRole::Administrator => Ok(assigned_admin_controls(store, &user)
            .await?
            .effective
            .allows(control)),
        UserRole::Owner | UserRole::Member => Ok(true),
    }
}

/// Replaces the overrides of `user_id`; with none, the Administrator
/// follows the library's default for every switch again.
///
/// The caller has already checked that the actor is the operator or an
/// Owner. One atomic batch updates the stored row, inserts it when there is
/// none, or deletes it when no switch is overridden.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] for an unknown user, or the read or
/// write error.
pub async fn set_admin_control_overrides(
    store: &LibraryStore,
    user_id: i64,
    overrides: AdminControlOverrides,
) -> Result<AssignedAdminControls> {
    let Some(user) = store.get_user(user_id).await? else {
        return Err(LibraryError::not_found(EntityKind::User, user_id));
    };
    let now = Utc::now().to_rfc3339();
    let operation_id = format!("admin-controls-{user_id}-{}", uuid::Uuid::new_v4().simple());
    let statements = if overrides.is_empty() {
        vec![batch::exec(
            "DELETE FROM admin_controls WHERE user_id = ?",
            vec![int(user_id)],
        )]
    } else {
        let json = serde_json::to_string(&overrides).map_err(|err| invalid(&err.to_string()))?;
        vec![
            batch::exec(
                "UPDATE admin_controls SET controls_json = ?, updated_at = ? WHERE user_id = ?",
                vec![text(&json), text(&now), int(user_id)],
            ),
            batch::exec(
                "INSERT OR IGNORE INTO admin_controls (user_id, controls_json, updated_at) \
                 VALUES (?, ?, ?)",
                vec![int(user_id), text(&json), text(&now)],
            ),
        ]
    };
    store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    assigned_admin_controls(store, &user).await
}

/// Changes only the named overrides of `user_id`: each attempt applies
/// `changes` to the row it read and writes it back only if the row is
/// still what it read, so a concurrent edit of another switch is kept
/// rather than overwritten.
///
/// The caller has already checked that the actor is the operator or an
/// Owner. With no override left, the row is deleted and the Administrator
/// follows the library's default for every switch.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] for an unknown user,
/// [`LibraryError::Conflict`] when the row keeps changing underneath, or
/// the read or write error.
pub async fn change_admin_control_overrides(
    store: &LibraryStore,
    user_id: i64,
    changes: &OverrideSwitchChanges,
) -> Result<AssignedAdminControls> {
    let Some(user) = store.get_user(user_id).await? else {
        return Err(LibraryError::not_found(EntityKind::User, user_id));
    };
    for attempt in 0..4 {
        let row = admin_controls::Entity::find_by_id(user_id)
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let current = row.as_ref().map(parse_row).transpose()?.unwrap_or_default();
        let mut next = current;
        for control in AdminControl::ALL {
            if let Some(choice) = changes.get(control) {
                next.set(control, choice.value());
            }
        }
        if next == current {
            return assigned_admin_controls(store, &user).await;
        }
        let now = Utc::now().to_rfc3339();
        let json = serde_json::to_string(&next).map_err(|err| invalid(&err.to_string()))?;
        let statement = match (&row, next.is_empty()) {
            (Some(old), true) => batch::exec(
                "DELETE FROM admin_controls WHERE user_id = ? AND controls_json = ?",
                vec![int(user_id), text(&old.controls_json)],
            ),
            (Some(old), false) => batch::exec(
                "UPDATE admin_controls SET controls_json = ?, updated_at = ? \
                 WHERE user_id = ? AND controls_json = ?",
                vec![
                    text(&json),
                    text(&now),
                    int(user_id),
                    text(&old.controls_json),
                ],
            ),
            (None, _) => batch::exec(
                "INSERT OR IGNORE INTO admin_controls (user_id, controls_json, updated_at) \
                 VALUES (?, ?, ?)",
                vec![int(user_id), text(&json), text(&now)],
            ),
        };
        let operation_id = format!(
            "admin-controls-{user_id}-{attempt}-{}",
            uuid::Uuid::new_v4().simple()
        );
        store
            .execute_host_batch(batch::request(&operation_id, vec![statement]))
            .await?;
        if admin_control_overrides(store, user_id).await? == next {
            return assigned_admin_controls(store, &user).await;
        }
    }
    Err(LibraryError::Conflict(
        "the administrator's switches kept changing; retry".into(),
    ))
}

/// Parses a stored default document, failing closed on an unknown schema.
fn parse_default(stored: StoredDocument) -> Result<ConfigurationDocument<AdminControls>> {
    if stored.schema_version != ADMIN_CONTROLS_SCHEMA_VERSION {
        return Err(documents::unsupported(
            ADMIN_CONTROLS_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: AdminControls =
        serde_json::from_str(&stored.document_json).map_err(|err| invalid(&err.to_string()))?;
    Ok(document(stored, body))
}

/// Parses a stored Administrator's overrides.
fn parse_row(row: &admin_controls::Model) -> Result<AdminControlOverrides> {
    serde_json::from_str(&row.controls_json).map_err(|err| invalid(&err.to_string()))
}

/// Serializes the default for storage.
fn to_json(controls: &AdminControls) -> Result<String> {
    serde_json::to_string(controls).map_err(|err| invalid(&err.to_string()))
}

/// Error for a document or row that does not parse.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid administrator switches: {detail}"))
}

#[cfg(test)]
#[path = "admin_controls_tests.rs"]
mod tests;
