//! Splitting a work an automatic merge joined wrongly
//! (`docs/adr/library-data-model.md`: "A wrong automatic merge is undone
//! by an override that splits the work").
//!
//! A split moves some of a work's expressions, with their editions,
//! listings and holdings, to a new work in one atomic batch. The new work
//! copies the old one's title, match key, description, work-level credits
//! and series, and each person's own state and tags for it. The batch also
//! records an `isolate` override on the new work, so the work matcher keeps
//! joining new listings with the same title to the original work, and
//! publishes `edition_updated` for one edition on each side so the library
//! page index and the works search index refresh both works.

use std::collections::BTreeSet;

use bookclerk_plugin_abi::{DbValue, TypedDbStatement};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use uuid::Uuid;

use super::{now_str, LibraryStore};
use crate::control_plane::batch;
use crate::entities::{editions, expressions, store_listings, works};
use crate::error::{LibraryError, Result};

/// Most expressions one split moves: the batch's widest statement binds
/// each id twice, and the minimum database profile allows 32 binds.
pub const MAX_SPLIT_EXPRESSIONS: usize = 12;

/// What [`LibraryStore::split_work`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitOutcome {
    /// The expressions now belong to `work_id`, a new work.
    Split {
        /// The new work.
        work_id: String,
    },
    /// No work has this id.
    UnknownWork,
    /// No expression was named, or more than [`MAX_SPLIT_EXPRESSIONS`].
    BadCount,
    /// These named expressions are not the work's.
    NotOfWork(Vec<String>),
    /// The split would leave the work with no expression.
    WouldEmpty,
}

impl LibraryStore {
    /// Moves `expression_ids` out of `work_id` into a new work, as one
    /// atomic batch.
    ///
    /// Every write requires the new work, and the new work is inserted only
    /// while every named expression still belongs to `work_id` and another
    /// stays behind, so a concurrent change writes nothing rather than half
    /// a split.
    ///
    /// # Arguments
    ///
    /// * `work_id` - The work to split (`works.id`).
    /// * `expression_ids` - The expressions to move, 1 to
    ///   [`MAX_SPLIT_EXPRESSIONS`]; duplicates count once.
    /// * `user_id` - The Administrator (`users.id`); `None` for the operator.
    ///
    /// # Returns
    ///
    /// What the split did. [`SplitOutcome::Split`] is returned only when
    /// the expressions were moved.
    ///
    /// # Errors
    ///
    /// Returns the database error of a failed read or write, or
    /// [`LibraryError::Conflict`] when the work changed between the read
    /// and the write.
    pub async fn split_work(
        &self,
        work_id: &str,
        expression_ids: &[String],
        user_id: Option<i64>,
    ) -> Result<SplitOutcome> {
        let moved: Vec<String> = expression_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if moved.is_empty() || moved.len() > MAX_SPLIT_EXPRESSIONS {
            return Ok(SplitOutcome::BadCount);
        }
        let known = works::Entity::find_by_id(work_id.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some();
        if !known {
            return Ok(SplitOutcome::UnknownWork);
        }
        let owner = [work_id.to_string()];
        let own: BTreeSet<String> = super::library_model::find_chunked_paged::<
            expressions::Entity,
            _,
        >(self, &owner, |part| {
            expressions::Entity::find()
                .filter(expressions::Column::WorkId.is_in(part.iter().cloned()))
        })
        .await?
        .into_iter()
        .map(|x| x.id)
        .collect();
        let foreign: Vec<String> = moved
            .iter()
            .filter(|id| !own.contains(*id))
            .cloned()
            .collect();
        if !foreign.is_empty() {
            return Ok(SplitOutcome::NotOfWork(foreign));
        }
        let kept: Vec<String> = own
            .iter()
            .filter(|id| !moved.contains(id))
            .cloned()
            .collect();
        if kept.is_empty() {
            return Ok(SplitOutcome::WouldEmpty);
        }
        let moved_edition = self.edition_to_publish(&moved).await?;
        let kept_edition = self.edition_to_publish(&kept).await?;

        let new_id = Uuid::new_v4().to_string();
        let revision = Uuid::new_v4().to_string();
        let statements = split_statements(&SplitWrite {
            work_id,
            new_id: &new_id,
            moved: &moved,
            user_id,
            revision: &revision,
            now: &now_str(),
            published: [moved_edition, kept_edition]
                .into_iter()
                .flatten()
                .collect(),
        });
        let operation_id = format!("split-work-{}", Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        let created = works::Entity::find_by_id(new_id.clone())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some();
        if !created {
            return Err(LibraryError::Conflict(format!(
                "work {work_id} changed while it was being split; try again"
            )));
        }
        Ok(SplitOutcome::Split { work_id: new_id })
    }

    /// An edition of `expression_ids` to publish `edition_updated` for,
    /// with the storefront to name as the event's source: one with a
    /// listing when there is one (its store's PluginKey), else any edition
    /// (source `host`), else `None`.
    async fn edition_to_publish(
        &self,
        expression_ids: &[String],
    ) -> Result<Option<(String, String)>> {
        let found: Vec<String> = super::library_model::find_chunked_paged::<editions::Entity, _>(
            self,
            expression_ids,
            |part| {
                editions::Entity::find()
                    .filter(editions::Column::ExpressionId.is_in(part.iter().cloned()))
            },
        )
        .await?
        .into_iter()
        .map(|e| e.id)
        .collect();
        let Some(first) = found.first().cloned() else {
            return Ok(None);
        };
        let listed = store_listings::Entity::find()
            .filter(store_listings::Column::EditionId.is_in(found.iter().take(16).cloned()))
            .limit(1)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(Some(match listed {
            Some(listing) => (listing.edition_id, listing.plugin_key),
            None => (first, "host".to_string()),
        }))
    }
}

/// Everything one split writes.
struct SplitWrite<'a> {
    /// The work split.
    work_id: &'a str,
    /// The new work's id.
    new_id: &'a str,
    /// The expressions moved, sorted and distinct.
    moved: &'a [String],
    /// The Administrator; `None` for the operator.
    user_id: Option<i64>,
    /// This split's revision (a fresh UUID), in the override and the
    /// events' dedup keys.
    revision: &'a str,
    /// RFC 3339 write time.
    now: &'a str,
    /// Editions to publish `edition_updated` for, with each event's source.
    published: Vec<(String, String)>,
}

/// The statements of one split; see [`LibraryStore::split_work`].
fn split_statements(p: &SplitWrite<'_>) -> Vec<TypedDbStatement> {
    let list = vec!["?"; p.moved.len()].join(", ");
    let ids = || p.moved.iter().map(|id| batch::text(id));
    let created = "EXISTS (SELECT 1 FROM works WHERE id = ?)";
    let count = i64::try_from(p.moved.len()).unwrap_or(i64::MAX);

    let mut insert_binds: Vec<DbValue> = vec![
        batch::text(p.new_id),
        batch::text(p.now),
        batch::text(p.now),
        batch::text(p.work_id),
        batch::text(p.work_id),
    ];
    insert_binds.extend(ids());
    insert_binds.push(batch::int(count));
    insert_binds.push(batch::text(p.work_id));
    insert_binds.extend(ids());

    let mut move_binds: Vec<DbValue> = vec![
        batch::text(p.new_id),
        batch::text(p.now),
        batch::text(p.work_id),
    ];
    move_binds.extend(ids());
    move_binds.push(batch::text(p.new_id));

    let copy = |sql: &str| {
        batch::exec(
            &format!("{sql} AND {created}"),
            vec![
                batch::text(p.new_id),
                batch::text(p.now),
                batch::text(p.work_id),
                batch::text(p.new_id),
            ],
        )
    };

    let mut out = vec![
        batch::exec(
            &format!(
                "INSERT INTO works \
                 (id, kind, title, match_key, parent_work_id, description, subjects, categories, \
                  cover_url, created_at, updated_at) \
                 SELECT ?, kind, title, match_key, parent_work_id, description, subjects, \
                  categories, cover_url, ?, ? FROM works \
                 WHERE id = ? \
                   AND (SELECT COUNT(*) FROM expressions WHERE work_id = ? AND id IN ({list})) = ? \
                   AND EXISTS (SELECT 1 FROM expressions WHERE work_id = ? AND id NOT IN ({list}))"
            ),
            insert_binds,
        ),
        copy(
            "INSERT OR IGNORE INTO credits \
             (target_kind, target_id, contributor_id, role, position, created_at) \
             SELECT 'work', ?, contributor_id, role, position, ? FROM credits \
             WHERE target_kind = 'work' AND target_id = ?",
        ),
        copy(
            "INSERT OR IGNORE INTO series_entries \
             (series_id, work_id, position, sort_position, created_at) \
             SELECT series_id, ?, position, sort_position, ? FROM series_entries \
             WHERE work_id = ?",
        ),
        copy(
            "INSERT OR IGNORE INTO user_title_state \
             (user_id, work_id, is_finished, rating_overall, rating_performance, rating_story, \
              tags, updated_at) \
             SELECT user_id, ?, is_finished, rating_overall, rating_performance, rating_story, \
              tags, ? FROM user_title_state WHERE work_id = ?",
        ),
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO user_work_tags (user_id, work_id, tag_key, tag) \
                 SELECT user_id, ?, tag_key, tag FROM user_work_tags WHERE work_id = ? AND {created}"
            ),
            vec![
                batch::text(p.new_id),
                batch::text(p.work_id),
                batch::text(p.new_id),
            ],
        ),
        batch::exec(
            &format!(
                "UPDATE expressions SET work_id = ?, updated_at = ? \
                 WHERE work_id = ? AND id IN ({list}) AND {created}"
            ),
            move_binds,
        ),
        batch::exec(
            &format!(
                "INSERT OR IGNORE INTO resolution_overrides \
                 (subject_kind, subject_id, action, target_id, created_by_user_id, created_at, revision) \
                 SELECT 'work', ?, 'isolate', NULL, ?, ?, ? WHERE {created}"
            ),
            vec![
                batch::text(p.new_id),
                batch::int_opt(p.user_id),
                batch::text(p.now),
                batch::text(p.revision),
                batch::text(p.new_id),
            ],
        ),
    ];
    for (edition_id, source) in &p.published {
        out.push(split_event(p, edition_id, source));
    }
    out
}

/// The `edition_updated` outbox insert for `edition_id`, published only
/// when the split created its work.
fn split_event(p: &SplitWrite<'_>, edition_id: &str, source: &str) -> TypedDbStatement {
    let dedup_key = format!("edition_updated:{edition_id}:{}", p.revision);
    let payload = serde_json::json!({ "editionId": edition_id }).to_string();
    batch::exec(
        "INSERT OR IGNORE INTO domain_events (\
            id, event_type, schema_version, occurred_at, account_id, source, \
            correlation_id, causation_id, dedup_key, payload, ordering_key, \
            dispatch_state, created_at, wake_pending\
         ) SELECT ?, 'edition_updated', 1, ?, '', ?, ?, '', ?, ?, ?, 'pending', ?, 1 \
         WHERE NOT EXISTS (\
            SELECT 1 FROM domain_events \
            WHERE queue_name = 'core.events' AND account_id = '' AND source = ? \
              AND event_type = 'edition_updated' AND dedup_key = ?\
         ) AND EXISTS (SELECT 1 FROM works WHERE id = ?)",
        vec![
            batch::text(&Uuid::new_v4().to_string()),
            batch::text(p.now),
            batch::text(source),
            batch::text(p.work_id),
            batch::text(&dedup_key),
            batch::text(&payload),
            batch::text(edition_id),
            batch::text(p.now),
            batch::text(source),
            batch::text(&dedup_key),
            batch::text(p.new_id),
        ],
    )
}

#[cfg(test)]
mod tests {
    use bookclerk_resolve as resolve;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    use super::*;
    use crate::entities::{credits, domain_events, resolution_overrides, series_entries};
    use crate::models::UserRole;
    use crate::test_support::{sqlite, Profile};
    use crate::{FieldEdit, UserStateEdit};

    /// A catalog listing of "Dune" by Frank Herbert in series "Dune": an
    /// unabridged audiobook (`abridged` false) or an abridged one, each
    /// with its own ASIN, so one work with two expressions.
    fn dune(product: &str, abridged: bool) -> resolve::Listing {
        resolve::Listing::from_draft(&resolve::ListingDraft {
            plugin_key: "plugin:audible".into(),
            marketplace: Some("us".into()),
            product_id: product.into(),
            work_kind: Some(resolve::WorkKind::Book),
            expression_kind: Some(resolve::ExpressionKind::Narration),
            media_kind: Some(resolve::MediaKind::Audio),
            language: Some("en".into()),
            abridgement: Some(if abridged {
                resolve::Abridgement::Abridged
            } else {
                resolve::Abridgement::Unabridged
            }),
            title: "Dune".into(),
            identifiers: vec![(resolve::IdentifierScheme::Asin, product.into(), None)],
            credits: vec![("Frank Herbert".into(), resolve::CreditRole::Author, None)],
            series: vec![("Dune".into(), Some("1".into()))],
            ..resolve::ListingDraft::default()
        })
        .unwrap()
    }

    /// The work, unabridged expression and abridged expression of two
    /// recorded Dune listings.
    async fn merged(store: &LibraryStore) -> (String, String, String) {
        let full = store
            .record_catalog_listing(&dune("FULL", false))
            .await
            .unwrap();
        let short = store
            .record_catalog_listing(&dune("SHORT", true))
            .await
            .unwrap();
        let full_edition = editions::Entity::find_by_id(full.listing.edition_id.clone())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let short_edition = editions::Entity::find_by_id(short.listing.edition_id.clone())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(full_edition.expression_id, short_edition.expression_id);
        let work = store
            .work_of_edition(&full_edition.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .work_of_edition(&short_edition.id)
                .await
                .unwrap()
                .as_deref(),
            Some(work.as_str()),
            "one work before the split"
        );
        (
            work,
            full_edition.expression_id,
            short_edition.expression_id,
        )
    }

    /// The work `expression_id` belongs to.
    async fn work_of(store: &LibraryStore, expression_id: &str) -> String {
        expressions::Entity::find_by_id(expression_id.to_string())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .work_id
    }

    #[tokio::test]
    async fn a_split_moves_expressions_to_an_isolated_copy() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let (work, full, short) = merged(store).await;
            let ana = store
                .create_user(UserRole::Owner, Some("Ana"), None)
                .await
                .unwrap();
            store
                .edit_user_title_state(
                    ana.id,
                    &work,
                    &UserStateEdit {
                        is_finished: Some(true),
                        tags: FieldEdit::Set("classic".into()),
                        ..UserStateEdit::default()
                    },
                )
                .await
                .unwrap();

            let outcome = store
                .split_work(&work, &[short.clone(), short.clone()], Some(ana.id))
                .await
                .unwrap();
            let SplitOutcome::Split { work_id: new } = outcome else {
                panic!("{name}: {outcome:?}");
            };
            assert_eq!(work_of(store, &short).await, new, "{name}");
            assert_eq!(work_of(store, &full).await, work, "{name}");

            let copy = works::Entity::find_by_id(new.clone())
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let original = works::Entity::find_by_id(work.clone())
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(copy.title, original.title, "{name}");
            assert_eq!(copy.match_key, original.match_key, "{name}");
            let authors = credits::Entity::find()
                .filter(credits::Column::TargetKind.eq("work"))
                .filter(credits::Column::TargetId.eq(new.as_str()))
                .all(&store.db)
                .await
                .unwrap();
            assert_eq!(authors.len(), 1, "{name}: the author is copied");
            let series = series_entries::Entity::find()
                .filter(series_entries::Column::WorkId.eq(new.as_str()))
                .all(&store.db)
                .await
                .unwrap();
            assert_eq!(series.len(), 1, "{name}: the series entry is copied");
            let state = store.user_title_state(ana.id, &new).await.unwrap().unwrap();
            assert!(state.is_finished, "{name}: own state is copied");
            assert_eq!(state.tags.as_deref(), Some("classic"), "{name}");
            let isolated = resolution_overrides::Entity::find()
                .filter(resolution_overrides::Column::SubjectKind.eq("work"))
                .filter(resolution_overrides::Column::SubjectId.eq(new.as_str()))
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(isolated.action, "isolate", "{name}");
            assert_eq!(isolated.created_by_user_id, Some(ana.id), "{name}");
            let updated = domain_events::Entity::find()
                .filter(domain_events::Column::EventType.eq("edition_updated"))
                .filter(domain_events::Column::DedupKey.ends_with(isolated.revision.as_str()))
                .all(&store.db)
                .await
                .unwrap();
            assert_eq!(updated.len(), 2, "{name}: one edition on each side");

            // A new listing with the same title joins the original work,
            // not the isolated copy.
            let again = store
                .record_catalog_listing(&dune("THIRD", false))
                .await
                .unwrap();
            assert_eq!(
                store
                    .work_of_edition(&again.listing.edition_id)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(work.as_str()),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn a_split_refuses_what_it_cannot_do() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let (work, full, short) = merged(store).await;

            assert_eq!(
                store
                    .split_work("nope", std::slice::from_ref(&short), None)
                    .await
                    .unwrap(),
                SplitOutcome::UnknownWork,
                "{name}"
            );
            assert_eq!(
                store.split_work(&work, &[], None).await.unwrap(),
                SplitOutcome::BadCount,
                "{name}"
            );
            let many: Vec<String> = (0..=MAX_SPLIT_EXPRESSIONS)
                .map(|n| format!("x{n}"))
                .collect();
            assert_eq!(
                store.split_work(&work, &many, None).await.unwrap(),
                SplitOutcome::BadCount,
                "{name}"
            );
            assert_eq!(
                store
                    .split_work(&work, &[short.clone(), "elsewhere".into()], None)
                    .await
                    .unwrap(),
                SplitOutcome::NotOfWork(vec!["elsewhere".into()]),
                "{name}"
            );
            assert_eq!(
                store
                    .split_work(&work, &[short.clone(), full.clone()], None)
                    .await
                    .unwrap(),
                SplitOutcome::WouldEmpty,
                "{name}"
            );
            assert_eq!(work_of(store, &short).await, work, "{name}: nothing moved");

            // The batch itself writes nothing once the work no longer has
            // another expression to keep.
            let statements = split_statements(&SplitWrite {
                work_id: &work,
                new_id: "new-work",
                moved: &[full.clone(), short.clone()],
                user_id: None,
                revision: "r1",
                now: &now_str(),
                published: Vec::new(),
            });
            store
                .execute_host_batch(batch::request("split-all", statements))
                .await
                .unwrap();
            assert!(
                works::Entity::find_by_id("new-work".to_string())
                    .one(&store.db)
                    .await
                    .unwrap()
                    .is_none(),
                "{name}"
            );
            assert_eq!(work_of(store, &full).await, work, "{name}");
        }
    }
}
