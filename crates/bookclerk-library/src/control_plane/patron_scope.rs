//! Patron scopes: which part of the library a Member sees and what they may
//! do with it (see `docs/adr/patron-scopes.md`).
//!
//! A scope is a list of rules. Each rule names a field, one
//! value of that field, and whether it includes or excludes the books that
//! have it. With no include rules every book passes; with any, a book must
//! match at least one. An exclude rule always wins over an include.
//!
//! Owners and Administrators are never scoped. A Member or Host follows the
//! library's default scope (the `core.patron_scope` cluster document) until
//! an Administrator gives them their own (a `patron_scopes` row). Out of the
//! box the default has no rules, so it restricts nothing.
//!
//! The library applies a scope with one filter on its page index
//! ([`crate::WorkScope`]); see `docs/guide/sharing.md#limit-what-a-patron-sees`.

use std::collections::BTreeSet;

use chrono::Utc;
use sea_orm::EntityTrait;
use serde::{Deserialize, Serialize};

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::{document, ConfigActor, ConfigurationDocument, VerifiedPatronAdministrator};
use crate::control_plane::batch::{self, int, text};
use crate::entities::patron_scopes;
use crate::error::{LibraryError, Result};
use crate::models::{UserRecord, UserRole, UserStatus};
use crate::store::LibraryStore;
use crate::{EntityKind, WorkFacet, WorkScope};

/// Namespace of the cluster document that holds the library's default
/// scope.
pub const PATRON_SCOPE_NAMESPACE: &str = "core.patron_scope";

/// Schema version of [`PatronScope`] in the default-scope document and in
/// `patron_scopes.scope_json`.
pub const PATRON_SCOPE_SCHEMA_VERSION: i64 = 1;

/// Most rules one scope may hold.
pub const MAX_SCOPE_RULES: usize = 64;

/// Longest rule value, in UTF-8 bytes.
pub const MAX_SCOPE_VALUE_BYTES: usize = 256;

/// The book field a [`ScopeRule`] matches one value of.
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
pub enum ScopeField {
    /// A genre (store category), such as "Kids".
    Genre,
    /// A series name, such as "Discworld".
    Series,
    /// An author (a show's host, for a podcast).
    Author,
    /// A narrator.
    Narrator,
}

impl ScopeField {
    /// The page index facet whose values the field matches.
    #[must_use]
    pub fn facet(self) -> WorkFacet {
        match self {
            Self::Genre => WorkFacet::Genre,
            Self::Series => WorkFacet::Series,
            Self::Author => WorkFacet::Author,
            Self::Narrator => WorkFacet::Narrator,
        }
    }
}

/// Whether a [`ScopeRule`] lets matching books in or keeps them out.
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
pub enum RuleEffect {
    /// Matching books are visible (once any include rule exists, only books
    /// that match one are).
    Include,
    /// Matching books are hidden, whatever the include rules say.
    Exclude,
}

/// One rule of a scope: a field, one value of it, and its effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeRule {
    /// The field the rule matches.
    pub field: ScopeField,
    /// The value, compared without case and surrounding space (1 to 256
    /// bytes).
    #[schema(example = "Kids")]
    pub value: String,
    /// Include or exclude.
    pub effect: RuleEffect,
}

/// Which part of the library a patron sees.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PatronScope {
    /// Visibility rules, at most 64. Empty means every book is visible.
    #[serde(default)]
    pub rules: Vec<ScopeRule>,
}

impl PatronScope {
    /// A scope that restricts nothing: no rules.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self { rules: Vec::new() }
    }

    /// True when the scope hides no book.
    #[must_use]
    pub fn sees_everything(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rules with the given effect, as `(facet, facet key)` pairs for
    /// the page index.
    #[must_use]
    pub fn facet_keys(&self, effect: RuleEffect) -> Vec<(WorkFacet, String)> {
        self.rules
            .iter()
            .filter(|rule| rule.effect == effect)
            .map(|rule| (rule.field.facet(), crate::facet_key(&rule.value)))
            .collect()
    }

    /// Checks the scope's bounds.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for more than [`MAX_SCOPE_RULES`]
    /// rules, an empty or over-long value, or a rule given twice (the same
    /// field, value and effect, compared without case).
    pub fn validate(&self) -> Result<()> {
        if self.rules.len() > MAX_SCOPE_RULES {
            return Err(invalid(&format!(
                "a scope holds at most {MAX_SCOPE_RULES} rules"
            )));
        }
        let mut seen = BTreeSet::new();
        for rule in &self.rules {
            let value = rule.value.trim();
            if value.is_empty() {
                return Err(invalid("a scope rule needs a value"));
            }
            if value.len() > MAX_SCOPE_VALUE_BYTES || value.contains('\u{0000}') {
                return Err(invalid(&format!(
                    "a scope rule value is at most {MAX_SCOPE_VALUE_BYTES} bytes"
                )));
            }
            if !seen.insert((rule.field, crate::facet_key(value), rule.effect)) {
                return Err(invalid(&format!("the rule `{value}` is given twice")));
            }
        }
        Ok(())
    }

    /// The scope with each value trimmed, as it is stored.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        for rule in &mut self.rules {
            rule.value = rule.value.trim().to_string();
        }
        self
    }
}

/// A Member's scope and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedScope {
    /// The scope that applies.
    pub scope: PatronScope,
    /// True when an Administrator gave the patron this scope; false when the
    /// patron follows the library's default.
    pub own: bool,
}

/// Whether a role is ever scoped: Members and Hosts are patrons, while
/// Owners and Administrators always see the whole library.
#[must_use]
pub fn role_is_scoped(role: UserRole) -> bool {
    match role {
        UserRole::Member | UserRole::Host => true,
        UserRole::Owner | UserRole::Administrator => false,
    }
}

/// Checks that `user_id` is an active Owner or Administrator and returns
/// the actor that may write the library's default scope.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] for anyone else (including an unknown
/// user), and the read error when the lookup fails.
pub async fn verify_patron_administrator(
    store: &LibraryStore,
    user_id: i64,
) -> Result<ConfigActor> {
    match store.get_user(user_id).await? {
        Some(user)
            if user.status == UserStatus::Active
                && matches!(user.role, UserRole::Owner | UserRole::Administrator) =>
        {
            Ok(ConfigActor::PatronAdministrator(
                VerifiedPatronAdministrator {
                    user_id,
                    audit: format!("user:{user_id}"),
                },
            ))
        }
        _ => Err(LibraryError::Forbidden(format!(
            "user {user_id} is not an active administrator"
        ))),
    }
}

/// Loads the library's default scope document.
///
/// # Returns
///
/// The stored document, or `None` when nobody has written one: the default
/// is then [`PatronScope::unrestricted`].
///
/// # Errors
///
/// Returns the read error, or an error when the stored document is invalid
/// or of an unknown schema.
pub async fn load_default_patron_scope(
    store: &LibraryStore,
) -> Result<Option<ConfigurationDocument<PatronScope>>> {
    load_document(store, &DocumentKey::cluster(PATRON_SCOPE_NAMESPACE))
        .await?
        .map(parse_default)
        .transpose()
}

/// The library's default scope: the stored document, or no restriction.
///
/// # Errors
///
/// Returns the read or parse error.
pub async fn default_patron_scope(store: &LibraryStore) -> Result<PatronScope> {
    Ok(load_default_patron_scope(store)
        .await?
        .map(|document| document.body)
        .unwrap_or_default())
}

/// Replaces the library's default scope under compare-and-swap, creating the
/// document when absent and retrying a few times on a concurrent edit.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - Who is writing, for authorization and the audit log.
/// * `scope` - The new default.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a scope that fails
/// [`PatronScope::validate`], an error when the actor may not write, or
/// [`LibraryError::Conflict`] when the document keeps changing underneath.
pub async fn set_default_patron_scope(
    store: &LibraryStore,
    actor: &ConfigActor,
    scope: PatronScope,
) -> Result<ConfigurationDocument<PatronScope>> {
    let scope = scope.normalized();
    scope.validate()?;
    let key = DocumentKey::cluster(PATRON_SCOPE_NAMESPACE);
    for attempt in 0..4 {
        let current = match load_default_patron_scope(store).await? {
            Some(document) => document,
            None => parse_default(
                import_if_absent(
                    store,
                    actor,
                    &key,
                    PATRON_SCOPE_SCHEMA_VERSION,
                    &to_json(&PatronScope::unrestricted())?,
                    "core-patron-scope-default",
                )
                .await?,
            )?,
        };
        if current.body == scope {
            return Ok(current);
        }
        let operation_id = format!(
            "core-patron-scope-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            PATRON_SCOPE_SCHEMA_VERSION,
            current.revision,
            &to_json(&scope)?,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => return parse_default(stored),
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(
        "core.patron_scope kept changing; retry".into(),
    ))
}

/// The scope a user has: their own, or the library's default.
///
/// The role is not considered; see [`scope_for_user`] for the scope that
/// applies to a signed-in user.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] for an unknown user, or a read or
/// parse error.
pub async fn assigned_patron_scope(store: &LibraryStore, user_id: i64) -> Result<AssignedScope> {
    if store.get_user(user_id).await?.is_none() {
        return Err(LibraryError::not_found(EntityKind::User, user_id));
    }
    match patron_scopes::Entity::find_by_id(user_id)
        .one(store.db())
        .await
        .map_err(LibraryError::Orm)?
    {
        Some(row) => Ok(AssignedScope {
            scope: parse_row(&row)?,
            own: true,
        }),
        None => Ok(AssignedScope {
            scope: default_patron_scope(store).await?,
            own: false,
        }),
    }
}

/// The scope that applies to `user`: `None` for an Owner or Administrator,
/// who see everything, and the assigned scope for a Member.
///
/// # Errors
///
/// Returns a read or parse error.
pub async fn scope_for_user(
    store: &LibraryStore,
    user: &UserRecord,
) -> Result<Option<PatronScope>> {
    if !role_is_scoped(user.role) {
        return Ok(None);
    }
    Ok(Some(assigned_patron_scope(store, user.id).await?.scope))
}

/// The page index filter for `user`: `None` when nothing is hidden from
/// them (an Owner or Administrator, or a patron whose scope has no rules).
///
/// The filter carries every store account linked to the user, connected
/// or not, because the books those accounts hold are the patron's own and
/// always pass.
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub async fn work_scope_for_user(
    store: &LibraryStore,
    user: &UserRecord,
) -> Result<Option<WorkScope>> {
    let Some(scope) = scope_for_user(store, user).await? else {
        return Ok(None);
    };
    if scope.sees_everything() {
        return Ok(None);
    }
    let mut own_accounts = BTreeSet::new();
    for identity in store.list_portal_identities_for_user(user.id).await? {
        for link in store.list_account_links(identity.id).await? {
            own_accounts.insert((link.plugin_key, link.account_id));
        }
    }
    Ok(Some(WorkScope {
        include: scope.facet_keys(RuleEffect::Include),
        exclude: scope.facet_keys(RuleEffect::Exclude),
        own_accounts: own_accounts.into_iter().collect(),
    }))
}

/// Gives a user their own scope, or with `None` returns them to the
/// library's default.
///
/// The caller has already checked that the actor may manage the user. One
/// atomic batch updates the stored row, or inserts it when there is none.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] for an unknown user,
/// [`LibraryError::Invalid`] for a scope that fails
/// [`PatronScope::validate`], or the write error.
pub async fn set_patron_scope(
    store: &LibraryStore,
    user_id: i64,
    scope: Option<PatronScope>,
) -> Result<AssignedScope> {
    if store.get_user(user_id).await?.is_none() {
        return Err(LibraryError::not_found(EntityKind::User, user_id));
    }
    let now = Utc::now().to_rfc3339();
    let operation_id = format!("patron-scope-{user_id}-{}", uuid::Uuid::new_v4().simple());
    let statements = match &scope {
        Some(scope) => {
            let scope = scope.clone().normalized();
            scope.validate()?;
            let json = to_json(&scope)?;
            vec![
                batch::exec(
                    "UPDATE patron_scopes SET scope_json = ?, updated_at = ? WHERE user_id = ?",
                    vec![text(&json), text(&now), int(user_id)],
                ),
                batch::exec(
                    "INSERT OR IGNORE INTO patron_scopes (user_id, scope_json, updated_at) \
                     VALUES (?, ?, ?)",
                    vec![int(user_id), text(&json), text(&now)],
                ),
            ]
        }
        None => vec![batch::exec(
            "DELETE FROM patron_scopes WHERE user_id = ?",
            vec![int(user_id)],
        )],
    };
    store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    assigned_patron_scope(store, user_id).await
}

/// Parses a stored default-scope document, failing closed on an unknown
/// schema.
fn parse_default(stored: StoredDocument) -> Result<ConfigurationDocument<PatronScope>> {
    if stored.schema_version != PATRON_SCOPE_SCHEMA_VERSION {
        return Err(documents::unsupported(
            PATRON_SCOPE_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: PatronScope =
        serde_json::from_str(&stored.document_json).map_err(|err| invalid(&err.to_string()))?;
    body.validate()?;
    Ok(document(stored, body))
}

/// Parses a stored patron's scope.
fn parse_row(row: &patron_scopes::Model) -> Result<PatronScope> {
    let scope: PatronScope =
        serde_json::from_str(&row.scope_json).map_err(|err| invalid(&err.to_string()))?;
    scope.validate()?;
    Ok(scope)
}

/// Serializes a scope for storage.
fn to_json(scope: &PatronScope) -> Result<String> {
    serde_json::to_string(scope).map_err(|err| invalid(&err.to_string()))
}

/// Error for a scope outside its bounds.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid patron scope: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(field: ScopeField, value: &str, effect: RuleEffect) -> ScopeRule {
        ScopeRule {
            field,
            value: value.into(),
            effect,
        }
    }

    #[test]
    fn an_empty_document_restricts_nothing() {
        let scope: PatronScope = serde_json::from_str("{}").unwrap();
        assert_eq!(scope, PatronScope::unrestricted());
        assert!(scope.sees_everything());
        assert!(serde_json::from_str::<PatronScope>(r#"{"other":1}"#).is_err());
    }

    #[test]
    fn validation_bounds_rules() {
        let mut scope = PatronScope {
            rules: vec![
                rule(ScopeField::Genre, " Kids ", RuleEffect::Include),
                rule(ScopeField::Genre, "kids", RuleEffect::Exclude),
            ],
        };
        scope.validate().unwrap();
        scope
            .rules
            .push(rule(ScopeField::Genre, "KIDS", RuleEffect::Include));
        assert!(scope.validate().is_err(), "duplicate without case");
        scope.rules.truncate(2);
        scope
            .rules
            .push(rule(ScopeField::Series, "  ", RuleEffect::Include));
        assert!(scope.validate().is_err(), "blank value");
        scope.rules.truncate(2);
        scope.rules.push(rule(
            ScopeField::Author,
            &"x".repeat(MAX_SCOPE_VALUE_BYTES + 1),
            RuleEffect::Include,
        ));
        assert!(scope.validate().is_err(), "long value");
        let many = PatronScope {
            rules: (0..=MAX_SCOPE_RULES)
                .map(|n| rule(ScopeField::Author, &format!("a{n}"), RuleEffect::Include))
                .collect(),
        };
        assert!(many.validate().is_err(), "too many rules");
        assert_eq!(
            scope.clone().normalized().rules[0].value,
            "Kids",
            "values are trimmed"
        );
    }

    #[tokio::test]
    async fn members_follow_the_default_until_given_their_own() {
        use crate::test_support::{sqlite, Profile};
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let sam = store
                .create_user(UserRole::Member, Some("Sam"), None)
                .await
                .unwrap();
            let admin = store
                .create_user(UserRole::Administrator, Some("Ada"), None)
                .await
                .unwrap();

            let assigned = assigned_patron_scope(store, sam.id).await.unwrap();
            assert_eq!(assigned.scope, PatronScope::unrestricted(), "{name}");
            assert!(!assigned.own, "{name}");
            assert_eq!(work_scope_for_user(store, &sam).await.unwrap(), None);

            // An Administrator writes the default; a Member may not.
            let actor = verify_patron_administrator(store, admin.id).await.unwrap();
            assert!(verify_patron_administrator(store, sam.id).await.is_err());
            let kids = PatronScope {
                rules: vec![rule(ScopeField::Genre, " Kids ", RuleEffect::Include)],
            };
            set_default_patron_scope(store, &actor, kids.clone())
                .await
                .unwrap();
            let expected = kids.clone().normalized();
            assert_eq!(default_patron_scope(store).await.unwrap(), expected);
            assert_eq!(
                scope_for_user(store, &sam).await.unwrap(),
                Some(expected.clone()),
                "{name}: a Member follows the default"
            );
            assert_eq!(scope_for_user(store, &admin).await.unwrap(), None);
            assert_eq!(
                work_scope_for_user(store, &sam).await.unwrap(),
                Some(WorkScope {
                    include: vec![(WorkFacet::Genre, "kids".into())],
                    ..WorkScope::default()
                }),
                "{name}"
            );

            // Their own scope, replaced, then removed.
            let own = PatronScope {
                rules: vec![rule(ScopeField::Series, "Discworld", RuleEffect::Exclude)],
            };
            let set = set_patron_scope(store, sam.id, Some(own.clone()))
                .await
                .unwrap();
            assert!(set.own, "{name}");
            assert_eq!(set.scope, own, "{name}");
            let set = set_patron_scope(store, sam.id, Some(kids.clone()))
                .await
                .unwrap();
            assert_eq!(set.scope, expected, "{name}: replaced");
            let back = set_patron_scope(store, sam.id, None).await.unwrap();
            assert!(!back.own, "{name}");
            assert_eq!(back.scope, expected, "{name}: the default again");

            // Bad input and unknown users are refused.
            let blank = PatronScope {
                rules: vec![rule(ScopeField::Author, " ", RuleEffect::Include)],
            };
            assert!(set_patron_scope(store, sam.id, Some(blank)).await.is_err());
            assert!(set_patron_scope(store, 9999, None).await.is_err());
            assert!(assigned_patron_scope(store, 9999).await.is_err());

            // The patron's linked accounts ride along as their own books.
            store
                .upsert_account("audible", "acct-1", "us", None, true, "audible")
                .await
                .unwrap();
            let identity = store
                .ensure_local_portal_identity(sam.id, None)
                .await
                .unwrap();
            store
                .link_account(identity.id, "audible", "acct-1", "audible")
                .await
                .unwrap();
            assert_eq!(
                work_scope_for_user(store, &sam)
                    .await
                    .unwrap()
                    .map(|scope| scope.own_accounts),
                Some(vec![("audible".to_string(), "acct-1".to_string())]),
                "{name}"
            );
        }
    }

    #[test]
    fn only_patrons_are_scoped() {
        assert!(role_is_scoped(UserRole::Member));
        assert!(role_is_scoped(UserRole::Host));
        assert!(!role_is_scoped(UserRole::Administrator));
        assert!(!role_is_scoped(UserRole::Owner));
    }
}
