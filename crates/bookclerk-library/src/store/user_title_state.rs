//! One user's own state for a work (`user_title_state`): finished flag,
//! ratings and tags.
//!
//! The state is keyed `(user_id, work_id)`, so members who share a
//! storefront account each keep their own, and one person's state follows
//! the work across every account and store that holds it
//! (`docs/adr/library-data-model.md`). Writes go to the database plugin as
//! one atomic batch per call (an `UPDATE` that keeps fields the caller left
//! out, then `INSERT OR IGNORE`), never an interactive transaction.

use std::collections::BTreeSet;

use bookclerk_plugin_abi::{DbType, DbValue, TypedDbStatement};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::{now_str, parse_dt, LibraryStore};
use crate::column_read::SelectColumnExt;
use crate::control_plane::batch;
use crate::entities::{account_links, portal_identities, user_title_state};
use crate::error::{LibraryError, Result};
use crate::models::UserTitleState;
use crate::store::library_index::facet_key;
use crate::store::UserBookFields;

impl LibraryStore {
    /// Records `fields` as `user_id`'s state for `work_id`.
    ///
    /// Fields left `None` keep their stored value (a new row starts
    /// unfinished, unrated and untagged). New tags also replace the work's
    /// `user_work_tags` rows, which the library page filters by. The write
    /// is one atomic batch.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user (`users.id`).
    /// * `work_id` - The work (`works.id`).
    /// * `fields` - The fields to set.
    ///
    /// # Errors
    ///
    /// Returns the database error, for example when the user or the work
    /// does not exist on an engine that checks foreign keys.
    pub async fn set_user_title_state(
        &self,
        user_id: i64,
        work_id: &str,
        fields: &UserBookFields,
    ) -> Result<()> {
        let mut statements = user_title_state_upsert(user_id, work_id, fields, &now_str()).to_vec();
        if let Some(tags) = fields.tags.as_deref() {
            statements.extend(user_work_tags_replace(user_id, work_id, tags));
        }
        let operation_id = format!("user-title-state-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(())
    }

    /// Applies `edit` to `user_id`'s state for `work_id` and returns the
    /// state as stored afterwards.
    ///
    /// Unlike [`LibraryStore::set_user_title_state`], a field can be
    /// cleared as well as kept or set. New tags also replace the work's
    /// `user_work_tags` rows. The write is one atomic batch, and every
    /// statement in it requires the work to exist, so an unknown work
    /// writes nothing.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user (`users.id`).
    /// * `work_id` - The work (`works.id`).
    /// * `edit` - What to change.
    ///
    /// # Returns
    ///
    /// The stored state, or `None` when no such work exists.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn edit_user_title_state(
        &self,
        user_id: i64,
        work_id: &str,
        edit: &UserStateEdit,
    ) -> Result<Option<UserTitleState>> {
        let mut statements = user_title_state_edit(user_id, work_id, edit, &now_str()).to_vec();
        match &edit.tags {
            FieldEdit::Keep => {}
            FieldEdit::Clear => statements.extend(user_work_tags_replace(user_id, work_id, "")),
            FieldEdit::Set(tags) => {
                statements.extend(user_work_tags_replace(user_id, work_id, tags));
            }
        }
        let operation_id = format!("user-title-state-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        self.user_title_state(user_id, work_id).await
    }

    /// Every work state of the given users, read in chunks.
    ///
    /// # Arguments
    ///
    /// * `user_ids` - Users whose state to read.
    ///
    /// # Returns
    ///
    /// The stored rows, ordered by user then work.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn user_title_states(&self, user_ids: &[i64]) -> Result<Vec<UserTitleState>> {
        let users: Vec<i64> = user_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut out = Vec::new();
        for part in users.chunks(self.lookup_chunk()) {
            out.extend(
                user_title_state::Entity::find()
                    .filter(user_title_state::Column::UserId.is_in(part.iter().copied()))
                    .order_by_asc(user_title_state::Column::UserId)
                    .order_by_asc(user_title_state::Column::WorkId)
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                    .into_iter()
                    .map(map_state),
            );
        }
        Ok(out)
    }

    /// One user's stored state for each of `work_ids`, read in chunks
    /// that fit the database's limits.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user (`users.id`).
    /// * `work_ids` - Works whose state to read; duplicates are read once.
    ///
    /// # Returns
    ///
    /// The stored rows, one per work the user has state for, ordered by
    /// work.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn user_title_states_of_works(
        &self,
        user_id: i64,
        work_ids: &[String],
    ) -> Result<Vec<UserTitleState>> {
        let ids: Vec<String> = work_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // `(user_id, work_id)` is the key, so each work names at most one row.
        Ok(
            super::library_model::find_chunked::<user_title_state::Entity, _>(self, &ids, |part| {
                user_title_state::Entity::find()
                    .filter(user_title_state::Column::UserId.eq(user_id))
                    .filter(user_title_state::Column::WorkId.is_in(part.iter().cloned()))
                    .order_by_asc(user_title_state::Column::WorkId)
            })
            .await?
            .into_iter()
            .map(map_state)
            .collect(),
        )
    }

    /// The users linked to a storefront account: the users of the portal
    /// identities whose account links name it.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The account's plugin.
    /// * `account_id` - The account id at that plugin.
    ///
    /// # Returns
    ///
    /// Distinct user ids, ascending; empty when no linked identity has a
    /// user.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn users_linked_to_account(
        &self,
        plugin_key: &str,
        account_id: &str,
    ) -> Result<Vec<i64>> {
        let identities: Vec<i64> = account_links::Entity::find()
            .filter(account_links::Column::PluginKey.eq(plugin_key))
            .filter(account_links::Column::AccountId.eq(account_id))
            .into_column::<i64, _>(account_links::Column::IdentityId)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut users = BTreeSet::new();
        for part in identities.chunks(self.lookup_chunk()) {
            for identity in portal_identities::Entity::find()
                .filter(portal_identities::Column::Id.is_in(part.iter().copied()))
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?
            {
                users.extend(identity.user_id);
            }
        }
        Ok(users.into_iter().collect())
    }
}

/// Real bind, or a typed NULL when unknown.
fn real_opt(value: Option<f32>) -> DbValue {
    match value {
        Some(value) => DbValue::Float64(f64::from(value)),
        None => DbValue::Null(DbType::Float64),
    }
}

/// One field of a [`UserStateEdit`].
#[derive(Debug, Clone, PartialEq, Default)]
pub enum FieldEdit<T> {
    /// Leave the stored value as it is.
    #[default]
    Keep,
    /// Remove the stored value.
    Clear,
    /// Store this value.
    Set(T),
}

impl<T> FieldEdit<T> {
    /// The bind pair for `col = CASE WHEN ? = 1 THEN ? ELSE col END`:
    /// whether to write, and the value written (NULL when clearing).
    ///
    /// # Arguments
    ///
    /// * `value` - Maps a set value to its bind.
    /// * `null` - The typed NULL for the column.
    fn binds(&self, value: impl Fn(&T) -> DbValue, null: DbValue) -> (DbValue, DbValue) {
        match self {
            Self::Keep => (batch::int(0), null),
            Self::Clear => (batch::int(1), null),
            Self::Set(v) => (batch::int(1), value(v)),
        }
    }
}

/// A change to one user's state for one work, applied by
/// [`LibraryStore::edit_user_title_state`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserStateEdit {
    /// The finished flag to store; `None` keeps it (a new row starts
    /// unfinished).
    pub is_finished: Option<bool>,
    /// Overall rating, 0 to 5.
    pub rating_overall: FieldEdit<f64>,
    /// Narration rating, 0 to 5.
    pub rating_performance: FieldEdit<f64>,
    /// Story rating, 0 to 5.
    pub rating_story: FieldEdit<f64>,
    /// Tags, whitespace-separated.
    pub tags: FieldEdit<String>,
}

/// The two statements that apply `edit` to one user's state for one work,
/// each guarded so nothing is written for an unknown work.
///
/// # Arguments
///
/// * `user_id` - The user.
/// * `work_id` - The work.
/// * `edit` - What to change.
/// * `now` - The write time (RFC 3339).
fn user_title_state_edit(
    user_id: i64,
    work_id: &str,
    edit: &UserStateEdit,
    now: &str,
) -> [TypedDbStatement; 2] {
    let real = |v: &f64| DbValue::Float64(*v);
    let null_real = || DbValue::Null(DbType::Float64);
    let (overall_set, overall) = edit.rating_overall.binds(real, null_real());
    let (performance_set, performance) = edit.rating_performance.binds(real, null_real());
    let (story_set, story) = edit.rating_story.binds(real, null_real());
    let (tags_set, tags) = edit
        .tags
        .binds(|v: &String| batch::text(v), DbValue::Null(DbType::Text));
    let finished = batch::int_opt(edit.is_finished.map(i64::from));
    [
        batch::exec(
            "UPDATE user_title_state SET \
             is_finished = COALESCE(?, is_finished), \
             rating_overall = CASE WHEN ? = 1 THEN ? ELSE rating_overall END, \
             rating_performance = CASE WHEN ? = 1 THEN ? ELSE rating_performance END, \
             rating_story = CASE WHEN ? = 1 THEN ? ELSE rating_story END, \
             tags = CASE WHEN ? = 1 THEN ? ELSE tags END, \
             updated_at = ? \
             WHERE user_id = ? AND work_id = ?",
            vec![
                finished.clone(),
                overall_set,
                overall.clone(),
                performance_set,
                performance.clone(),
                story_set,
                story.clone(),
                tags_set,
                tags.clone(),
                batch::text(now),
                batch::int(user_id),
                batch::text(work_id),
            ],
        ),
        batch::exec(
            "INSERT OR IGNORE INTO user_title_state \
             (user_id, work_id, is_finished, rating_overall, rating_performance, rating_story, \
              tags, updated_at) \
             SELECT ?, ?, COALESCE(?, 0), ?, ?, ?, ?, ? \
             WHERE EXISTS (SELECT 1 FROM works WHERE id = ?)",
            vec![
                batch::int(user_id),
                batch::text(work_id),
                finished,
                overall,
                performance,
                story,
                tags,
                batch::text(now),
                batch::text(work_id),
            ],
        ),
    ]
}

/// The two statements that store one user's state for one work.
///
/// `UPDATE` first, keeping each stored field the caller left out with
/// `COALESCE`, then `INSERT OR IGNORE` (canonical SQL-v1 has no
/// `ON CONFLICT … DO UPDATE`): an existing row is updated and the insert is
/// ignored; a new row is missed by the update and inserted.
///
/// # Arguments
///
/// * `user_id` - The user.
/// * `work_id` - The work.
/// * `fields` - The fields to set.
/// * `now` - `updated_at` for the write (RFC 3339).
fn user_title_state_upsert(
    user_id: i64,
    work_id: &str,
    fields: &UserBookFields,
    now: &str,
) -> [TypedDbStatement; 2] {
    let finished = batch::int_opt(fields.is_finished.map(i64::from));
    [
        batch::exec(
            "UPDATE user_title_state SET \
             is_finished = COALESCE(?, is_finished), \
             rating_overall = COALESCE(?, rating_overall), \
             rating_performance = COALESCE(?, rating_performance), \
             rating_story = COALESCE(?, rating_story), \
             tags = COALESCE(?, tags), \
             updated_at = ? \
             WHERE user_id = ? AND work_id = ?",
            vec![
                finished.clone(),
                real_opt(fields.rating_overall),
                real_opt(fields.rating_performance),
                real_opt(fields.rating_story),
                batch::text_opt(fields.tags.as_deref()),
                batch::text(now),
                batch::int(user_id),
                batch::text(work_id),
            ],
        ),
        batch::exec(
            "INSERT OR IGNORE INTO user_title_state \
             (user_id, work_id, is_finished, rating_overall, rating_performance, rating_story, \
              tags, updated_at) \
             VALUES (?, ?, COALESCE(?, 0), ?, ?, ?, ?, ?)",
            vec![
                batch::int(user_id),
                batch::text(work_id),
                finished,
                real_opt(fields.rating_overall),
                real_opt(fields.rating_performance),
                real_opt(fields.rating_story),
                batch::text_opt(fields.tags.as_deref()),
                batch::text(now),
            ],
        ),
    ]
}

/// The statements that replace one user's `user_work_tags` rows for one
/// work with `tags`: whitespace-separated, each distinct [`facet_key`] once,
/// its first spelling kept. Nothing is inserted for an unknown work.
///
/// # Arguments
///
/// * `user_id` - The user.
/// * `work_id` - The work.
/// * `tags` - The tags as stored in `user_title_state.tags`.
fn user_work_tags_replace(user_id: i64, work_id: &str, tags: &str) -> Vec<TypedDbStatement> {
    let mut statements = vec![batch::exec(
        "DELETE FROM user_work_tags WHERE user_id = ? AND work_id = ?",
        vec![batch::int(user_id), batch::text(work_id)],
    )];
    let mut seen = BTreeSet::new();
    for tag in tags.split_whitespace() {
        let key = facet_key(tag);
        if seen.insert(key.clone()) {
            statements.push(batch::exec(
                "INSERT OR IGNORE INTO user_work_tags (user_id, work_id, tag_key, tag) \
                 SELECT ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM works WHERE id = ?)",
                vec![
                    batch::int(user_id),
                    batch::text(work_id),
                    batch::text(&key),
                    batch::text(tag),
                    batch::text(work_id),
                ],
            ));
        }
    }
    statements
}

/// Maps a stored row; `is_finished` is stored as 0/1.
pub(super) fn map_state(m: user_title_state::Model) -> UserTitleState {
    UserTitleState {
        user_id: m.user_id,
        work_id: m.work_id,
        is_finished: m.is_finished != 0,
        rating_overall: m.rating_overall,
        rating_performance: m.rating_performance,
        rating_story: m.rating_story,
        tags: m.tags,
        updated_at: parse_dt(&m.updated_at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UserRole;
    use crate::test_support::{sqlite, Profile};

    /// A catalog work for `title`.
    async fn work(store: &LibraryStore, title: &str) -> String {
        store
            .work_for_wish(&crate::TitleQuery {
                title,
                authors: Some("Ursula K. Le Guin"),
                ..crate::TitleQuery::default()
            })
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn an_edit_keeps_clears_and_sets_fields_and_skips_unknown_works() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ana = store
                .create_user(UserRole::Owner, Some("Ana"), None)
                .await
                .unwrap();
            let left_hand = work(store, "The Left Hand of Darkness").await;

            let first = store
                .edit_user_title_state(
                    ana.id,
                    &left_hand,
                    &UserStateEdit {
                        is_finished: Some(true),
                        rating_overall: FieldEdit::Set(4.5),
                        rating_story: FieldEdit::Set(5.0),
                        tags: FieldEdit::Set("classic reread".into()),
                        ..UserStateEdit::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert!(first.is_finished, "{name}");
            assert_eq!(first.rating_overall, Some(4.5), "{name}");
            assert_eq!(first.rating_performance, None, "{name}");
            assert_eq!(first.rating_story, Some(5.0), "{name}");
            assert_eq!(first.tags.as_deref(), Some("classic reread"), "{name}");

            let second = store
                .edit_user_title_state(
                    ana.id,
                    &left_hand,
                    &UserStateEdit {
                        rating_overall: FieldEdit::Clear,
                        rating_performance: FieldEdit::Set(3.0),
                        tags: FieldEdit::Clear,
                        ..UserStateEdit::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert!(second.is_finished, "{name}: kept");
            assert_eq!(second.rating_overall, None, "{name}: cleared");
            assert_eq!(second.rating_performance, Some(3.0), "{name}: set");
            assert_eq!(second.rating_story, Some(5.0), "{name}: kept");
            assert_eq!(second.tags, None, "{name}: cleared");
            let tag_rows = crate::entities::user_work_tags::Entity::find()
                .filter(crate::entities::user_work_tags::Column::UserId.eq(ana.id))
                .all(store.db())
                .await
                .unwrap();
            assert!(tag_rows.is_empty(), "{name}: tag rows cleared");

            let missing = store
                .edit_user_title_state(
                    ana.id,
                    "no-such-work",
                    &UserStateEdit {
                        is_finished: Some(true),
                        tags: FieldEdit::Set("ghost".into()),
                        ..UserStateEdit::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(missing, None, "{name}: unknown work writes nothing");
            let tag_rows = crate::entities::user_work_tags::Entity::find()
                .filter(crate::entities::user_work_tags::Column::UserId.eq(ana.id))
                .all(store.db())
                .await
                .unwrap();
            assert!(
                tag_rows.is_empty(),
                "{name}: no tag row for an unknown work"
            );
        }
    }

    #[tokio::test]
    async fn each_user_keeps_their_own_state_and_unset_fields_are_kept() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ana = store
                .create_user(UserRole::Owner, Some("Ana"), None)
                .await
                .unwrap();
            let ben = store
                .create_user(UserRole::Member, Some("Ben"), None)
                .await
                .unwrap();
            let left_hand = work(store, "The Left Hand of Darkness").await;
            let dispossessed = work(store, "The Dispossessed").await;

            store
                .set_user_title_state(
                    ana.id,
                    &left_hand,
                    &UserBookFields {
                        rating_overall: Some(5.0),
                        tags: Some("classics".into()),
                        ..UserBookFields::default()
                    },
                )
                .await
                .unwrap();
            // A later write of one field keeps the others.
            store
                .set_user_title_state(
                    ana.id,
                    &left_hand,
                    &UserBookFields {
                        is_finished: Some(true),
                        ..UserBookFields::default()
                    },
                )
                .await
                .unwrap();
            store
                .set_user_title_state(
                    ben.id,
                    &left_hand,
                    &UserBookFields {
                        rating_overall: Some(2.0),
                        rating_performance: Some(3.0),
                        rating_story: Some(1.5),
                        ..UserBookFields::default()
                    },
                )
                .await
                .unwrap();
            store
                .set_user_title_state(ana.id, &dispossessed, &UserBookFields::default())
                .await
                .unwrap();

            let states = store
                .user_title_states(&[ana.id, ben.id, ana.id])
                .await
                .unwrap();
            assert_eq!(states.len(), 3, "{name}: {states:?}");
            let of = |user: i64, work: &str| {
                states
                    .iter()
                    .find(|s| s.user_id == user && s.work_id == work)
                    .unwrap_or_else(|| panic!("{name}: {user} {work}"))
                    .clone()
            };
            let ana_left = of(ana.id, &left_hand);
            assert!(ana_left.is_finished, "{name}");
            assert_eq!(ana_left.rating_overall, Some(5.0), "{name}");
            assert_eq!(ana_left.tags.as_deref(), Some("classics"), "{name}");
            let ben_left = of(ben.id, &left_hand);
            assert!(!ben_left.is_finished, "{name}: Ben's state is his own");
            assert_eq!(ben_left.rating_overall, Some(2.0), "{name}");
            assert_eq!(ben_left.rating_performance, Some(3.0), "{name}");
            assert_eq!(ben_left.rating_story, Some(1.5), "{name}");
            let fresh = of(ana.id, &dispossessed);
            assert!(!fresh.is_finished && fresh.rating_overall.is_none() && fresh.tags.is_none());
            assert!(store.user_title_states(&[]).await.unwrap().is_empty());

            // Deleting a user deletes their state.
            store.delete_user(ben.id).await.unwrap();
            let left: Vec<i64> = store
                .user_title_states(&[ana.id, ben.id])
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.user_id)
                .collect();
            assert_eq!(left, vec![ana.id, ana.id], "{name}");
            assert_eq!(kit.refused_begins(), 0, "{name}");
        }
    }

    #[tokio::test]
    async fn imported_user_fields_reach_the_user_linked_to_the_account() {
        let kit = sqlite::memory(Profile::ALL[0]).await;
        let store = kit.store();
        store
            .upsert_account("audible", "shared", "us", None, true, "audible")
            .await
            .unwrap();
        let book = store
            .upsert_book(&crate::NewBook::minimal(
                "B00SHARED1",
                "shared",
                "us",
                "Shared",
            ))
            .await
            .unwrap();
        let ana = store
            .create_user(UserRole::Owner, Some("Ana"), None)
            .await
            .unwrap();
        let ben = store
            .create_user(UserRole::Member, Some("Ben"), None)
            .await
            .unwrap();
        let identity = store
            .ensure_local_portal_identity(ana.id, Some("Ana"))
            .await
            .unwrap();
        store
            .ensure_local_portal_identity(ben.id, Some("Ben"))
            .await
            .unwrap();
        store
            .link_account(identity.id, "audible", "shared", "audible")
            .await
            .unwrap();
        assert_eq!(
            store
                .users_linked_to_account("audible", "shared")
                .await
                .unwrap(),
            vec![ana.id]
        );

        store
            .update_user_fields(
                &book.holding_id,
                "shared",
                &UserBookFields {
                    is_finished: Some(true),
                    ..UserBookFields::default()
                },
            )
            .await
            .unwrap();
        let work = store
            .work_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        let states = store.user_title_states(&[ana.id, ben.id]).await.unwrap();
        assert_eq!(states.len(), 1, "{states:?}");
        assert_eq!(states[0].user_id, ana.id);
        assert_eq!(states[0].work_id, work);
        assert!(states[0].is_finished);
    }

    /// One person's state for chosen works reads under every profile, the
    /// one-row Minimum included, and leaves out other people's rows and
    /// works they never touched.
    #[tokio::test]
    async fn one_persons_state_for_chosen_works_reads_under_every_profile() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let ana = store
                .create_user(UserRole::Owner, Some("Ana"), None)
                .await
                .unwrap();
            let ben = store
                .create_user(UserRole::Member, Some("Ben"), None)
                .await
                .unwrap();
            let titles = [
                "The Left Hand of Darkness",
                "The Dispossessed",
                "The Lathe of Heaven",
            ];
            let mut works = Vec::new();
            for title in titles {
                works.push(work(store, title).await);
            }
            for (user, finished) in [(ana.id, true), (ben.id, false)] {
                for work in &works[..2] {
                    store
                        .set_user_title_state(
                            user,
                            work,
                            &UserBookFields {
                                is_finished: Some(finished),
                                tags: Some(format!("tag-{user}")),
                                ..UserBookFields::default()
                            },
                        )
                        .await
                        .unwrap();
                }
            }
            let mut asked = works.clone();
            asked.push(works[0].clone());
            let states = store
                .user_title_states_of_works(ana.id, &asked)
                .await
                .unwrap();
            assert_eq!(states.len(), 2, "{name}");
            assert!(
                states
                    .iter()
                    .all(|state| state.user_id == ana.id && state.is_finished),
                "{name}"
            );
            let mut read: Vec<&str> = states.iter().map(|s| s.work_id.as_str()).collect();
            read.sort_unstable();
            let mut expected: Vec<&str> = works[..2].iter().map(String::as_str).collect();
            expected.sort_unstable();
            assert_eq!(read, expected, "{name}");
            assert!(store
                .user_title_states_of_works(ana.id, &[])
                .await
                .unwrap()
                .is_empty());
        }
    }

    /// A store account belongs to one identity (`account_links` is
    /// exclusive), so the lookup reads at most one link and one identity and
    /// fits every profile, including the minimum's one row per statement.
    #[tokio::test]
    async fn the_linked_user_is_found_under_every_profile() {
        for profile in Profile::ALL {
            let name = profile.name();
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            store
                .upsert_account("audible", "shared", "us", None, true, "audible")
                .await
                .unwrap();
            let mut users = Vec::new();
            let mut identities = Vec::new();
            for who in ["Ana", "Ben"] {
                let user = store
                    .create_user(UserRole::Member, Some(who), None)
                    .await
                    .unwrap();
                identities.push(
                    store
                        .ensure_local_portal_identity(user.id, Some(who))
                        .await
                        .unwrap()
                        .id,
                );
                users.push(user.id);
            }
            store
                .link_account(identities[0], "audible", "shared", "audible")
                .await
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(
                store
                    .link_account(identities[1], "audible", "shared", "audible")
                    .await
                    .is_err(),
                "{name}: a second identity may not link the same account"
            );
            assert_eq!(
                store
                    .users_linked_to_account("audible", "shared")
                    .await
                    .unwrap_or_else(|err| panic!("{name}: {err}")),
                vec![users[0]],
                "{name}"
            );
            assert!(store
                .users_linked_to_account("audible", "other")
                .await
                .unwrap()
                .is_empty());
            assert_eq!(kit.refused_begins(), 0, "{name}");
        }
    }
}
