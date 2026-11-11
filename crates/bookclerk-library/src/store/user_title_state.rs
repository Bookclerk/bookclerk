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
use crate::store::UserBookFields;

impl LibraryStore {
    /// Records `fields` as `user_id`'s state for `work_id`.
    ///
    /// Fields left `None` keep their stored value (a new row starts
    /// unfinished, unrated and untagged). The write is one atomic batch.
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
        let statements = user_title_state_upsert(user_id, work_id, fields, &now_str());
        let operation_id = format!("user-title-state-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements.to_vec()))
            .await?;
        Ok(())
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

/// Maps a stored row; `is_finished` is stored as 0/1.
fn map_state(m: user_title_state::Model) -> UserTitleState {
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
                &book.uuid,
                "shared",
                &UserBookFields {
                    is_finished: Some(true),
                    ..UserBookFields::default()
                },
            )
            .await
            .unwrap();
        let work = store.work_id_for_book(&book.uuid).await.unwrap().unwrap();
        let states = store.user_title_states(&[ana.id, ben.id]).await.unwrap();
        assert_eq!(states.len(), 1, "{states:?}");
        assert_eq!(states[0].user_id, ana.id);
        assert_eq!(states[0].work_id, work);
        assert!(states[0].is_finished);
    }
}
