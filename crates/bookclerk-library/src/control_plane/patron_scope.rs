//! Patron scopes: which part of the library a Member sees and what they may
//! do with it (see `docs/adr/patron-scopes.md`).
//!
//! A scope is a list of rules. Each rule names a field, one
//! value of that field, and whether it includes or excludes the books that
//! have it. With no include rules every book passes; with any, a book must
//! match at least one. An exclude rule always wins over an include.
//!
//! Owners and Administrators are never scoped. A Member follows the
//! library's default scope (the `core.patron_scope` library document) until
//! an Administrator gives them their own (a `patron_scopes` row). Out of the
//! box the default has no rules, so it restricts nothing.
//!
//! The library applies a scope with one filter on its page index
//! ([`crate::WorkScope`]); see `docs/guide/sharing.md#limit-what-a-patron-sees`.

use std::collections::BTreeSet;

use chrono::Utc;
use sea_orm::EntityTrait;
use serde::Serialize;

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

/// Namespace of the library document that holds the library's default
/// scope.
pub const PATRON_SCOPE_NAMESPACE: &str = "core.patron_scope";

/// Schema version of [`PatronScope`] in the default-scope document and in
/// `patron_scopes.scope_json`.
pub const PATRON_SCOPE_SCHEMA_VERSION: i64 = 1;

/// Most rules one scope may hold.
pub const MAX_SCOPE_RULES: usize = 16;

/// Longest rule value, in UTF-8 bytes.
pub const MAX_SCOPE_VALUE_BYTES: usize = 256;

/// Largest scope as compact JSON, in bytes: one cell of the smallest reply
/// the host accepts from a database plugin
/// ([`HOST_MIN_CELL_BYTES`](bookclerk_plugin_abi::db_execute::HOST_MIN_CELL_BYTES)),
/// so a stored scope can be read back on every supported database. Sixteen
/// rules fit when their values are short.
pub const MAX_SCOPE_JSON_BYTES: usize = 1024;

/// The book field a [`ScopeRule`] matches one value of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
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
    /// A library tag: one an Administrator set on the work, or one read
    /// from a sidecar file.
    Tag,
    /// A collection: a named part of the library, defined by storage key
    /// prefixes (`core.collections`). A book with no file is in none.
    Collection,
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
            Self::Tag => WorkFacet::Tag,
            Self::Collection => WorkFacet::Collection,
        }
    }
}

/// Whether a [`ScopeRule`] lets matching books in or keeps them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    /// Matching books are visible (once any include rule exists, only books
    /// that match one are).
    Include,
    /// Matching books are hidden, whatever the include rules say.
    Exclude,
}

/// Whether a patron sees books marked as explicit (adult) content.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExplicitContent {
    /// Explicit books are visible (the default: explicit flags are
    /// incomplete, so hiding is a choice an Administrator makes).
    #[default]
    Allow,
    /// Books a store or metadata file marks explicit are hidden, whatever
    /// the include rules say; books with no flag stay visible.
    Hide,
}

impl ExplicitContent {
    /// True for [`Self::Allow`], which a stored scope leaves out.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        *self == Self::Allow
    }
}

/// The page index value a work marked explicit carries under
/// [`WorkFacet::Explicit`].
pub const EXPLICIT_FACET_KEY: &str = "explicit";

/// One rule of a scope: a field, one value of it, and its effect.
// Read by a strict hand-written reader (module `strict`); the serde
// attributes here shape the schema and match what that reader takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
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
// Read by a strict hand-written reader (module `strict`); the serde
// attributes here shape the schema and match what that reader takes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PatronScope {
    /// Visibility rules, at most 16 and 1,024 bytes as JSON. Empty means
    /// every book is visible.
    #[serde(default)]
    #[schema(max_items = 16)]
    pub rules: Vec<ScopeRule>,
    /// Whether books marked explicit are visible; left out means `allow`.
    #[serde(default, skip_serializing_if = "ExplicitContent::is_allow")]
    pub explicit: ExplicitContent,
}

impl PatronScope {
    /// A scope that restricts nothing: no rules.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            rules: Vec::new(),
            explicit: ExplicitContent::Allow,
        }
    }

    /// True when the scope hides no book: no rules, and explicit books
    /// allowed.
    #[must_use]
    pub fn sees_everything(&self) -> bool {
        self.rules.is_empty() && self.explicit.is_allow()
    }

    /// The rules with the given effect, as `(facet, facet key)` pairs for
    /// the page index. Hiding explicit books adds an exclude pair on
    /// [`WorkFacet::Explicit`].
    #[must_use]
    pub fn facet_keys(&self, effect: RuleEffect) -> Vec<(WorkFacet, String)> {
        let mut keys: Vec<(WorkFacet, String)> = self
            .rules
            .iter()
            .filter(|rule| rule.effect == effect)
            .map(|rule| (rule.field.facet(), crate::facet_key(&rule.value)))
            .collect();
        if effect == RuleEffect::Exclude && self.explicit == ExplicitContent::Hide {
            keys.push((WorkFacet::Explicit, EXPLICIT_FACET_KEY.to_string()));
        }
        keys
    }

    /// Checks the scope's bounds.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for more than [`MAX_SCOPE_RULES`]
    /// rules, an empty or over-long value, a rule given twice (the same
    /// field, value and effect, compared without case), or a scope longer
    /// than [`MAX_SCOPE_JSON_BYTES`] as JSON.
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
        let size = serde_json::to_vec(&self.clone().normalized())
            .map_err(|err| invalid(&format!("the scope could not be written: {err}")))?
            .len();
        if size > MAX_SCOPE_JSON_BYTES {
            return Err(invalid(&format!(
                "a scope is at most {MAX_SCOPE_JSON_BYTES} bytes as JSON"
            )));
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

/// `PUT /api/users/{id}/scope` body.
// Read by a strict hand-written reader (module `strict`); the serde
// attribute here shapes the schema and matches what that reader takes.
#[derive(Debug, Clone, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutUserScope {
    /// The patron's own scope, or `null` to return them to the library's
    /// default.
    pub scope: Option<PatronScope>,
}

/// Parses the body of `PUT /api/users/default-scope` from its raw bytes,
/// sans IO, the way the route reads it: JSON into [`PatronScope`], then
/// trimmed ([`PatronScope::normalized`]) and checked
/// ([`PatronScope::validate`]) as [`set_default_patron_scope`] does.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the scope's JSON
/// shape (unknown or repeated fields, wrong types, a missing rule field) or
/// a scope that fails [`PatronScope::validate`].
pub fn parse_patron_scope(body: &[u8]) -> Result<PatronScope> {
    let scope = serde_json::from_slice::<PatronScope>(body)
        .map_err(|err| invalid(&format!("scope body: {err}")))?
        .normalized();
    scope.validate()?;
    Ok(scope)
}

/// Parses the body of `PUT /api/users/{id}/scope` from its raw bytes, sans
/// IO, the way the route reads it: JSON into [`PutUserScope`], then the
/// scope (when one is given) trimmed and checked as [`set_patron_scope`]
/// does.
///
/// # Returns
///
/// The patron's own scope, or `None` (`"scope": null`, or no `scope` key)
/// to return them to the default.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape or a scope that fails [`PatronScope::validate`].
pub fn parse_user_scope_request(body: &[u8]) -> Result<Option<PatronScope>> {
    let body = serde_json::from_slice::<PutUserScope>(body)
        .map_err(|err| invalid(&format!("user scope body: {err}")))?;
    body.scope
        .map(|scope| {
            let scope = scope.normalized();
            scope.validate().map(|()| scope)
        })
        .transpose()
}

/// Strict JSON readers for the scope types.
///
/// A derived reader would also take a struct written as an array
/// (`["genre", "Kids", "include"]`) and a unit variant written as an object
/// (`{"genre": null}`). Scopes come from Administrators over the web API,
/// so the readers here take only the documented shape: objects for the
/// structs, with no unknown, repeated or (for a rule) missing keys, and
/// strings for the field and effect names. The `patron_scope_request` fuzz
/// target checks them against an independent reading.
mod strict {
    use std::fmt;

    use serde::de::{self, Deserializer, MapAccess, Visitor};
    use serde::Deserialize;

    use super::{ExplicitContent, PatronScope, PutUserScope, RuleEffect, ScopeField, ScopeRule};

    /// Reads one of the names of a unit enum from a JSON string.
    struct Name<T: 'static> {
        /// What the reader expects, for errors.
        what: &'static str,
        /// The accepted names and their values.
        names: &'static [(&'static str, T)],
        /// The accepted names alone, for errors.
        expected: &'static [&'static str],
    }

    impl<T: Copy> Visitor<'_> for Name<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.what)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
            self.names
                .iter()
                .find(|(name, _)| *name == value)
                .map(|(_, found)| *found)
                .ok_or_else(|| E::unknown_variant(value, self.expected))
        }
    }

    impl<'de> Deserialize<'de> for ScopeField {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_str(Name {
                what: "a scope field name",
                names: &[
                    ("genre", ScopeField::Genre),
                    ("series", ScopeField::Series),
                    ("author", ScopeField::Author),
                    ("narrator", ScopeField::Narrator),
                    ("tag", ScopeField::Tag),
                    ("collection", ScopeField::Collection),
                ],
                expected: &["genre", "series", "author", "narrator", "tag", "collection"],
            })
        }
    }

    impl<'de> Deserialize<'de> for ExplicitContent {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_str(Name {
                what: "allow or hide",
                names: &[
                    ("allow", ExplicitContent::Allow),
                    ("hide", ExplicitContent::Hide),
                ],
                expected: &["allow", "hide"],
            })
        }
    }

    impl<'de> Deserialize<'de> for RuleEffect {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_str(Name {
                what: "include or exclude",
                names: &[
                    ("include", RuleEffect::Include),
                    ("exclude", RuleEffect::Exclude),
                ],
                expected: &["include", "exclude"],
            })
        }
    }

    /// Stores one key's value, refusing a key written twice.
    fn once<'de, A: MapAccess<'de>, T: Deserialize<'de>>(
        map: &mut A,
        slot: &mut Option<T>,
        key: &'static str,
    ) -> Result<(), A::Error> {
        if slot.is_some() {
            return Err(de::Error::duplicate_field(key));
        }
        *slot = Some(map.next_value()?);
        Ok(())
    }

    /// Reads a [`ScopeRule`] from an object.
    struct RuleVisitor;

    impl<'de> Visitor<'de> for RuleVisitor {
        type Value = ScopeRule;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a scope rule object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ScopeRule, A::Error> {
            const KEYS: &[&str] = &["field", "value", "effect"];
            let (mut field, mut value, mut effect) = (None, None, None);
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "field" => once(&mut map, &mut field, "field")?,
                    "value" => once(&mut map, &mut value, "value")?,
                    "effect" => once(&mut map, &mut effect, "effect")?,
                    other => return Err(de::Error::unknown_field(other, KEYS)),
                }
            }
            Ok(ScopeRule {
                field: field.ok_or_else(|| de::Error::missing_field("field"))?,
                value: value.ok_or_else(|| de::Error::missing_field("value"))?,
                effect: effect.ok_or_else(|| de::Error::missing_field("effect"))?,
            })
        }
    }

    impl<'de> Deserialize<'de> for ScopeRule {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_map(RuleVisitor)
        }
    }

    /// Reads a [`PatronScope`] from an object.
    struct ScopeVisitor;

    impl<'de> Visitor<'de> for ScopeVisitor {
        type Value = PatronScope;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a patron scope object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PatronScope, A::Error> {
            const KEYS: &[&str] = &["rules", "explicit"];
            let (mut rules, mut explicit) = (None, None);
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "rules" => once(&mut map, &mut rules, "rules")?,
                    "explicit" => once(&mut map, &mut explicit, "explicit")?,
                    other => return Err(de::Error::unknown_field(other, KEYS)),
                }
            }
            Ok(PatronScope {
                rules: rules.unwrap_or_default(),
                explicit: explicit.unwrap_or_default(),
            })
        }
    }

    impl<'de> Deserialize<'de> for PatronScope {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_map(ScopeVisitor)
        }
    }

    /// Reads a [`PutUserScope`] from an object.
    struct PutVisitor;

    impl<'de> Visitor<'de> for PutVisitor {
        type Value = PutUserScope;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an object with an optional `scope`")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<PutUserScope, A::Error> {
            let mut scope: Option<Option<PatronScope>> = None;
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "scope" => once(&mut map, &mut scope, "scope")?,
                    other => return Err(de::Error::unknown_field(other, &["scope"])),
                }
            }
            Ok(PutUserScope {
                scope: scope.flatten(),
            })
        }
    }

    impl<'de> Deserialize<'de> for PutUserScope {
        fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
            de.deserialize_map(PutVisitor)
        }
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

/// Whether a role is ever scoped: Members are patrons, while
/// Owners and Administrators always see the whole library.
#[must_use]
pub fn role_is_scoped(role: UserRole) -> bool {
    match role {
        UserRole::Member => true,
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
    load_document(store, &DocumentKey::library(PATRON_SCOPE_NAMESPACE))
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
    let key = DocumentKey::library(PATRON_SCOPE_NAMESPACE);
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
            ReplaceOutcome::Applied(stored) => {
                // Open Library pages of patrons who follow the default
                // reload to show what it now allows.
                store
                    .bump_live_topic(crate::control_plane::LiveTopic::Library)
                    .await?;
                return parse_default(stored);
            }
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
/// The filter names the user as its owner, so the books held by every
/// store account linked to them, connected or not, are the patron's own
/// and always pass.
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
    Ok(Some(WorkScope {
        include: scope.facet_keys(RuleEffect::Include),
        exclude: scope.facet_keys(RuleEffect::Exclude),
        owner: Some(user.id),
    }))
}

/// The page index filter for a caller who is not a library user, such as a
/// session whose identity is linked to nobody: the library default, with
/// no store accounts of their own. `None` when the default hides nothing.
///
/// # Errors
///
/// Returns a read or parse error; callers fail closed on it.
pub async fn default_work_scope(store: &LibraryStore) -> Result<Option<WorkScope>> {
    let scope = default_patron_scope(store).await?;
    if scope.sees_everything() {
        return Ok(None);
    }
    Ok(Some(WorkScope {
        include: scope.facet_keys(RuleEffect::Include),
        exclude: scope.facet_keys(RuleEffect::Exclude),
        owner: None,
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
    // The patron's open Library pages reload to show the new scope.
    let mut statements = statements;
    statements.push(crate::control_plane::bump(
        crate::control_plane::LiveTopic::Library,
    ));
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
            explicit: Default::default(),
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
            explicit: ExplicitContent::Allow,
        };
        assert!(many.validate().is_err(), "too many rules");
        assert_eq!(
            scope.clone().normalized().rules[0].value,
            "Kids",
            "values are trimmed"
        );
    }

    /// A scope of the most rules, as long as the JSON limit allows.
    fn largest_scope() -> PatronScope {
        PatronScope {
            rules: (0..MAX_SCOPE_RULES)
                .map(|n| rule(ScopeField::Genre, &format!("g{n:02}"), RuleEffect::Include))
                .collect(),
            explicit: ExplicitContent::Allow,
        }
    }

    #[test]
    fn validation_bounds_the_written_size() {
        let largest = largest_scope();
        largest.validate().unwrap();
        assert!(serde_json::to_vec(&largest).unwrap().len() <= MAX_SCOPE_JSON_BYTES);
        let longest = PatronScope {
            rules: (0..MAX_SCOPE_RULES)
                .map(|n| {
                    rule(
                        ScopeField::Genre,
                        &format!("{n:03}{}", "x".repeat(MAX_SCOPE_VALUE_BYTES - 3)),
                        RuleEffect::Include,
                    )
                })
                .collect(),
            explicit: ExplicitContent::Allow,
        };
        assert!(longest.validate().is_err(), "the most values of 256 bytes");
        assert_eq!(
            Some(MAX_SCOPE_JSON_BYTES),
            usize::try_from(bookclerk_plugin_abi::db_execute::HOST_MIN_CELL_BYTES).ok(),
            "a stored scope fits the smallest cell a database plugin may return"
        );
    }

    /// A scope of the most rules whose compact JSON is exactly `bytes` long.
    fn scope_of_json_length(bytes: usize) -> PatronScope {
        let mut scope = largest_scope();
        let short = serde_json::to_vec(&scope).unwrap().len();
        let last = scope.rules.last_mut().unwrap();
        last.value.push_str(&"x".repeat(bytes - short));
        assert_eq!(serde_json::to_vec(&scope).unwrap().len(), bytes);
        scope
    }

    #[tokio::test]
    async fn a_scope_at_the_json_limit_round_trips() {
        use crate::test_support::{sqlite, Profile};
        for profile in Profile::ALL {
            let name = profile.name();
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let sam = store
                .create_user(UserRole::Member, Some("Sam"), None)
                .await
                .unwrap();
            let admin = store
                .create_user(UserRole::Administrator, Some("Ada"), None)
                .await
                .unwrap();
            let actor = verify_patron_administrator(store, admin.id).await.unwrap();
            let at_limit = scope_of_json_length(MAX_SCOPE_JSON_BYTES);
            set_default_patron_scope(store, &actor, at_limit.clone())
                .await
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(
                default_patron_scope(store).await.unwrap(),
                at_limit,
                "{name}"
            );
            set_patron_scope(store, sam.id, Some(at_limit.clone()))
                .await
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(
                assigned_patron_scope(store, sam.id).await.unwrap().scope,
                at_limit,
                "{name}"
            );
            let over = scope_of_json_length(MAX_SCOPE_JSON_BYTES + 1);
            assert!(
                set_default_patron_scope(store, &actor, over.clone())
                    .await
                    .is_err(),
                "{name}"
            );
            assert!(
                set_patron_scope(store, sam.id, Some(over)).await.is_err(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn scope_writes_reload_open_library_pages() {
        use crate::control_plane::{live_revisions, LiveRevisions, LiveTopic};
        use crate::test_support::{sqlite, Profile};
        let library = |revisions: &LiveRevisions| {
            revisions
                .iter()
                .find(|(topic, _)| *topic == LiveTopic::Library)
                .map(|(_, revision)| revision.0)
                .unwrap()
        };
        let kit = sqlite::memory(Profile::D1).await;
        let store = kit.store();
        let sam = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let admin = store
            .create_user(UserRole::Administrator, Some("Ada"), None)
            .await
            .unwrap();
        let actor = verify_patron_administrator(store, admin.id).await.unwrap();
        let kids = PatronScope {
            rules: vec![rule(ScopeField::Genre, "Kids", RuleEffect::Include)],
            explicit: Default::default(),
        };
        let before = library(&live_revisions(store).await.unwrap());
        set_patron_scope(store, sam.id, Some(kids.clone()))
            .await
            .unwrap();
        let after_own = library(&live_revisions(store).await.unwrap());
        assert!(after_own > before, "a patron's own scope");
        set_default_patron_scope(store, &actor, kids).await.unwrap();
        assert!(
            library(&live_revisions(store).await.unwrap()) > after_own,
            "the default"
        );
    }

    #[tokio::test]
    async fn the_largest_default_scope_is_stored() {
        use crate::test_support::{sqlite, Profile};
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let admin = store
                .create_user(UserRole::Administrator, Some("Ada"), None)
                .await
                .unwrap();
            let actor = verify_patron_administrator(store, admin.id).await.unwrap();
            set_default_patron_scope(store, &actor, largest_scope())
                .await
                .unwrap_or_else(|err| panic!("{}: {err}", profile.name()));
            assert_eq!(
                default_patron_scope(store).await.unwrap(),
                largest_scope(),
                "{}",
                profile.name()
            );
        }
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
                explicit: Default::default(),
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
                    exclude: Vec::new(),
                    owner: Some(sam.id),
                }),
                "{name}"
            );
            assert_eq!(
                default_work_scope(store).await.unwrap(),
                Some(WorkScope {
                    include: vec![(WorkFacet::Genre, "kids".into())],
                    ..WorkScope::default()
                }),
                "{name}: the default for a caller with no user"
            );

            // Their own scope, replaced, then removed.
            let own = PatronScope {
                rules: vec![rule(ScopeField::Series, "Discworld", RuleEffect::Exclude)],
                explicit: Default::default(),
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
                explicit: Default::default(),
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
                    .map(|scope| scope.owner),
                Some(Some(sam.id)),
                "{name}"
            );
        }
    }

    #[test]
    fn scopes_read_only_from_the_documented_shape() {
        assert!(parse_patron_scope(b"{}").unwrap().rules.is_empty());
        let rule = br#"{"rules":[{"field":"genre","value":" Kids ","effect":"include"}]}"#;
        assert_eq!(parse_patron_scope(rule).unwrap().rules[0].value, "Kids");
        for refused in [
            &b"[]"[..],
            br#"[[]]"#,
            br#"{"rules":[["genre","Kids","include"]]}"#,
            br#"{"rules":[{"field":{"genre":null},"value":"Kids","effect":"include"}]}"#,
            br#"{"rules":null}"#,
            br#"{"rules":[],"rules":[]}"#,
            br#"{"rules":[{"field":"genre","value":"Kids"}]}"#,
            br#"{"rules":[{"field":"genre","field":"genre","value":"Kids","effect":"include"}]}"#,
            br#"{"explicit":"maybe"}"#,
            br#"{"explicit":true}"#,
            br#"{"explicit":"hide","explicit":"hide"}"#,
        ] {
            assert!(
                matches!(parse_patron_scope(refused), Err(LibraryError::Invalid(_))),
                "{}",
                String::from_utf8_lossy(refused)
            );
        }
        assert_eq!(parse_user_scope_request(b"{}").unwrap(), None);
        assert_eq!(
            parse_user_scope_request(br#"{"scope":null}"#).unwrap(),
            None
        );
        assert!(parse_user_scope_request(br#"[null]"#).is_err());
        assert!(parse_user_scope_request(br#"{"scope":null,"scope":null}"#).is_err());
        // What the reader takes, the writer gives back.
        let scope = parse_patron_scope(rule).unwrap();
        let json = serde_json::to_vec(&scope).unwrap();
        assert_eq!(parse_patron_scope(&json).unwrap(), scope);
        assert!(
            !json.windows(8).any(|w| w == b"explicit"),
            "allow is left out"
        );
        let hide = parse_patron_scope(br#"{"explicit":"hide"}"#).unwrap();
        assert_eq!(hide.explicit, ExplicitContent::Hide);
        let json = serde_json::to_vec(&hide).unwrap();
        assert_eq!(parse_patron_scope(&json).unwrap(), hide);
        assert_eq!(
            parse_patron_scope(br#"{"explicit":"allow"}"#).unwrap(),
            PatronScope::unrestricted()
        );
    }

    #[test]
    fn hiding_explicit_books_excludes_the_explicit_facet() {
        let hide = PatronScope {
            rules: vec![rule(ScopeField::Genre, "Kids", RuleEffect::Include)],
            explicit: ExplicitContent::Hide,
        };
        assert!(!hide.sees_everything());
        assert_eq!(
            hide.facet_keys(RuleEffect::Exclude),
            [(WorkFacet::Explicit, EXPLICIT_FACET_KEY.to_string())]
        );
        assert_eq!(
            hide.facet_keys(RuleEffect::Include),
            [(WorkFacet::Genre, "kids".to_string())]
        );
        let only_hide = PatronScope {
            explicit: ExplicitContent::Hide,
            ..PatronScope::default()
        };
        assert!(!only_hide.sees_everything());
    }

    #[tokio::test]
    async fn a_scope_hides_works_a_store_marks_explicit_under_every_profile() {
        use crate::store::library_index::WorkPageQuery;
        use crate::store::{NewBook, ScannedTitle};
        use crate::test_support::{sqlite, Profile};
        for profile in Profile::ALL {
            let name = profile.name();
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            store
                .upsert_account("audible", "acct", "us", None, true, "audible")
                .await
                .unwrap();
            let title = |asin: &str, label: &str, is_explicit: Option<bool>| ScannedTitle {
                is_explicit,
                ..ScannedTitle::from(NewBook::minimal(asin, "acct", "us", label))
            };
            store
                .upsert_scanned_titles(&[
                    title("B0ADULT01", "After Dark", Some(true)),
                    title("B0CLEAN01", "Gruffalo", Some(false)),
                    title("B0UNKNOWN", "Dune", None),
                ])
                .await
                .unwrap();
            let mut ids = Vec::new();
            for asin in ["B0ADULT01", "B0CLEAN01", "B0UNKNOWN"] {
                let held = store.held_title(asin, "acct").await.unwrap().unwrap();
                ids.push(
                    store
                        .work_of_holding(&held.holding_id)
                        .await
                        .unwrap()
                        .unwrap(),
                );
            }
            store.refresh_library_works(&ids).await.unwrap();
            let hide = PatronScope {
                explicit: ExplicitContent::Hide,
                ..PatronScope::default()
            };
            let mut query = WorkPageQuery {
                limit: 10,
                scope: Some(WorkScope {
                    include: hide.facet_keys(RuleEffect::Include),
                    exclude: hide.facet_keys(RuleEffect::Exclude),
                    owner: None,
                }),
                ..WorkPageQuery::default()
            };
            let mut titles = Vec::new();
            loop {
                let page = store.library_works_page(&query).await.unwrap();
                titles.extend(page.works.into_iter().map(|w| w.title));
                match page.next {
                    Some(next) => query.after = Some(next),
                    None => break,
                }
            }
            titles.sort();
            assert_eq!(titles, ["Dune", "Gruffalo"], "{name}");
        }
    }

    #[test]
    fn only_patrons_are_scoped() {
        assert!(role_is_scoped(UserRole::Member));
        assert!(!role_is_scoped(UserRole::Administrator));
        assert!(!role_is_scoped(UserRole::Owner));
    }
}
