//! Destination-specific storage backends for one acquire operation.

use std::collections::BTreeMap;

use bookclerk_config::{DestinationFilter, DestinationId, MultiDestinationMode, PlacementFacts};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{FanoutBackend, StorageBackend};

use crate::error::{AcquireError, Result};

/// One configured output destination for an acquire run.
pub struct AcquireDestination {
    /// The destination's id: its storage plugin instance id.
    pub id: DestinationId,
    /// Constructed storage backend for this destination.
    pub backend: Box<dyn StorageBackend>,
    /// Source download options (quality, chapter prefs, …).
    pub options: DownloadOptions,
    /// Which books the destination takes; no rules take every book.
    pub filter: DestinationFilter,
}

/// Ordered set of acquire destinations built from the deployed destination plugins.
pub struct AcquireDestinations {
    /// Configured acquire destinations in evaluation order.
    pub items: Vec<AcquireDestination>,
    /// The primary destination, whose keys the library records as the
    /// title's storage key.
    pub primary: DestinationId,
    /// Whether multiple destinations are enabled for this run.
    pub multi_destination: MultiDestinationMode,
}

impl AcquireDestinations {
    /// Number of destinations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns true when no destinations are deployed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the primary destination used for library storage keys.
    #[must_use]
    pub fn primary_destination(&self) -> &AcquireDestination {
        self.items
            .iter()
            .find(|dest| dest.id == self.primary)
            .unwrap_or_else(|| &self.items[0])
    }

    /// Returns the destination `id`, if configured.
    #[must_use]
    pub fn destination(&self, id: &DestinationId) -> Option<&AcquireDestination> {
        self.items.iter().find(|dest| &dest.id == id)
    }

    /// Replaces every destination's filter with the one stored for it now.
    ///
    /// A long acquire run calls this before each book's target set is
    /// chosen, so a filter edited mid-run applies to the books that start
    /// afterwards. A destination absent from `filters` takes every book.
    ///
    /// # Arguments
    ///
    /// * `filters` - Stored filters by plugin instance id.
    pub fn set_filters(&mut self, filters: &BTreeMap<String, DestinationFilter>) {
        for dest in &mut self.items {
            dest.filter = filters.get(dest.id.as_str()).cloned().unwrap_or_default();
        }
    }

    /// The destinations that take the book `facts` describes: the target
    /// set an acquire of it writes to, fixed before the acquire starts.
    ///
    /// The primary stays the primary when it takes the book; otherwise the
    /// first destination that does becomes it.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::NoDestinationAccepts`] when no destination's
    /// filter takes the book.
    pub fn for_book(&self, facts: &PlacementFacts) -> Result<AcquireDestinations> {
        let items: Vec<AcquireDestination> = self
            .items
            .iter()
            .filter(|dest| dest.filter.accepts(facts))
            .map(|dest| AcquireDestination {
                id: dest.id.clone(),
                backend: dest.backend.clone_box(),
                options: dest.options.clone(),
                filter: dest.filter.clone(),
            })
            .collect();
        let primary = if items.iter().any(|dest| dest.id == self.primary) {
            self.primary.clone()
        } else {
            items
                .first()
                .map(|dest| dest.id.clone())
                .ok_or(AcquireError::NoDestinationAccepts)?
        };
        Ok(AcquireDestinations {
            items,
            primary,
            multi_destination: self.multi_destination,
        })
    }

    /// Splits the destinations into the ones that answer a listing now and
    /// the ones that do not.
    ///
    /// An acquire run lists every destination before it starts. One that
    /// is offline is left out of the run rather than failing it: the
    /// titles go to the destinations that answer, and the missing copies
    /// are written by a later run once it is back. The primary stays the
    /// primary when it answers; otherwise the first destination that does
    /// becomes it.
    ///
    /// # Returns
    ///
    /// The destinations that answered, and each one that did not with its
    /// listing error.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::NoDestinationReachable`] when none answers.
    pub async fn reachable(&self) -> Result<(AcquireDestinations, Vec<(DestinationId, String)>)> {
        let probes = futures::future::join_all(
            self.items
                .iter()
                .map(|dest| async move { (dest, dest.backend.list_page("", None, 1).await) }),
        )
        .await;
        let mut items = Vec::new();
        let mut unreachable = Vec::new();
        for (dest, probe) in probes {
            match probe {
                Ok(_) => items.push(AcquireDestination {
                    id: dest.id.clone(),
                    backend: dest.backend.clone_box(),
                    options: dest.options.clone(),
                    filter: dest.filter.clone(),
                }),
                Err(err) => unreachable.push((dest.id.clone(), err.to_string())),
            }
        }
        let primary = if items.iter().any(|dest| dest.id == self.primary) {
            self.primary.clone()
        } else {
            items.first().map(|dest| dest.id.clone()).ok_or_else(|| {
                AcquireError::NoDestinationReachable {
                    errors: unreachable
                        .iter()
                        .map(|(id, err)| format!("{id}: {err}"))
                        .collect::<Vec<_>>()
                        .join("; "),
                }
            })?
        };
        Ok((
            AcquireDestinations {
                items,
                primary,
                multi_destination: self.multi_destination,
            },
            unreachable,
        ))
    }

    /// Clone destination backends into a listing/match storage handle.
    ///
    /// Reuses the backends acquire already built instead of spawning the
    /// destination guests a second time.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn listing_backend(&self) -> Result<Box<dyn StorageBackend>> {
        let backends = self
            .items
            .iter()
            .map(|dest| dest.backend.clone_box())
            .collect::<Vec<_>>();
        if backends.len() == 1 {
            let mut backends = backends;
            return Ok(backends.remove(0));
        }
        Ok(Box::new(FanoutBackend::new(backends)?))
    }

    /// Consumes this set and returns a listing backend over the primary destination.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn into_listing_backend(self) -> Result<Box<dyn StorageBackend>> {
        let backends = self
            .items
            .into_iter()
            .map(|dest| dest.backend)
            .collect::<Vec<_>>();
        if backends.len() == 1 {
            let mut backends = backends;
            return Ok(backends.remove(0));
        }
        Ok(Box::new(FanoutBackend::new(backends)?))
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_config::{
        FilterEffect, FilterField, FilterRule, PlacementFacts, PlacementMediaKind,
    };

    use super::*;

    /// A destination named `id` that takes books under `filter`.
    fn destination(
        root: &std::path::Path,
        id: &str,
        filter: DestinationFilter,
    ) -> AcquireDestination {
        AcquireDestination {
            id: DestinationId::new(id).unwrap(),
            backend: Box::new(bookclerk_storage::LocalFsBackend::new(root.join(id)).unwrap()),
            options: DownloadOptions::default(),
            filter,
        }
    }

    /// A filter of one media kind rule.
    fn kind(value: &str, effect: FilterEffect) -> DestinationFilter {
        DestinationFilter {
            rules: vec![FilterRule {
                field: FilterField::MediaKind,
                value: value.into(),
                effect,
            }],
        }
    }

    /// A podcast episode held by user 2.
    fn podcast() -> PlacementFacts {
        PlacementFacts {
            patrons: vec![2],
            storefront: "audible".into(),
            account: "acct".into(),
            media_kind: PlacementMediaKind::Podcast,
        }
    }

    #[test]
    fn the_target_set_keeps_the_destinations_that_take_the_book() {
        let tmp = tempfile::tempdir().unwrap();
        let all = AcquireDestinations {
            items: vec![
                destination(tmp.path(), "books", kind("book", FilterEffect::Include)),
                destination(tmp.path(), "pods", kind("podcast", FilterEffect::Include)),
                destination(tmp.path(), "mirror", DestinationFilter::default()),
            ],
            primary: DestinationId::new("books").unwrap(),
            multi_destination: MultiDestinationMode::SyncMissing,
        };
        let placed = all.for_book(&podcast()).unwrap();
        let ids: Vec<&str> = placed.items.iter().map(|dest| dest.id.as_str()).collect();
        assert_eq!(ids, ["pods", "mirror"]);
        assert_eq!(
            placed.primary.as_str(),
            "pods",
            "the first taker is primary"
        );
        assert_eq!(placed.multi_destination, MultiDestinationMode::SyncMissing);
        assert_eq!(placed.len(), 2);

        let mirror = AcquireDestinations {
            primary: DestinationId::new("mirror").unwrap(),
            ..all
        };
        let placed = mirror.for_book(&podcast()).unwrap();
        assert_eq!(placed.primary.as_str(), "mirror", "a taking primary stays");
    }

    #[test]
    fn an_empty_target_set_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let all = AcquireDestinations {
            items: vec![destination(
                tmp.path(),
                "books",
                kind("podcast", FilterEffect::Exclude),
            )],
            primary: DestinationId::new("books").unwrap(),
            multi_destination: MultiDestinationMode::RefetchAll,
        };
        let err = all.for_book(&podcast()).err().unwrap();
        assert!(matches!(err, AcquireError::NoDestinationAccepts));
        assert_eq!(err.to_string(), "no destination accepts this book");
    }

    #[tokio::test]
    async fn a_destination_that_cannot_be_listed_is_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        let all = AcquireDestinations {
            items: vec![
                destination(tmp.path(), "local", DestinationFilter::default()),
                destination(tmp.path(), "second", DestinationFilter::default()),
            ],
            primary: DestinationId::new("local").unwrap(),
            multi_destination: MultiDestinationMode::SyncMissing,
        };
        let (reachable, unreachable) = all.reachable().await.unwrap();
        assert_eq!(reachable.len(), 2);
        assert!(unreachable.is_empty());

        // The primary's root is a file now, so listing it fails.
        std::fs::remove_dir_all(tmp.path().join("local")).unwrap();
        std::fs::write(tmp.path().join("local"), b"not a folder").unwrap();
        let (reachable, unreachable) = all.reachable().await.unwrap();
        let ids: Vec<&str> = reachable.items.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["second"]);
        assert_eq!(reachable.primary.as_str(), "second");
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0].0.as_str(), "local");

        std::fs::remove_dir_all(tmp.path().join("second")).unwrap();
        std::fs::write(tmp.path().join("second"), b"not a folder").unwrap();
        let err = all.reachable().await.err().expect("nothing answers");
        assert!(
            matches!(err, AcquireError::NoDestinationReachable { .. }),
            "{err}"
        );
    }

    #[test]
    fn set_filters_replaces_every_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut all = AcquireDestinations {
            items: vec![
                destination(tmp.path(), "books", kind("podcast", FilterEffect::Exclude)),
                destination(tmp.path(), "pods", DestinationFilter::default()),
            ],
            primary: DestinationId::new("books").unwrap(),
            multi_destination: MultiDestinationMode::RefetchAll,
        };
        let fresh = BTreeMap::from([("pods".to_string(), kind("book", FilterEffect::Include))]);
        all.set_filters(&fresh);
        assert!(all.items[0].filter.takes_everything(), "a cleared filter");
        assert_eq!(all.items[1].filter, kind("book", FilterEffect::Include));
        let placed = all.for_book(&podcast()).unwrap();
        let ids: Vec<&str> = placed.items.iter().map(|dest| dest.id.as_str()).collect();
        assert_eq!(ids, ["books"]);
    }
}
