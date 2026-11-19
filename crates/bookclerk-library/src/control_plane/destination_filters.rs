//! `core.destination`: which books one storage destination takes.
//!
//! Each storage plugin instance is a destination
//! (`docs/adr/storage-destinations.md`). Its filter is host data, not a
//! plugin setting, so it lives in a host-owned document on the instance's
//! scope, beside the plugin's own `plugin.config` document. An instance with
//! no `core.destination` document takes every book.
//!
//! Writes are operator writes under compare-and-swap, like every other
//! configuration document. A changed filter applies to acquires that start
//! afterwards; files already stored stay where they are.

use std::collections::BTreeMap;

use bookclerk_config::DestinationFilter;
use serde::Serialize;

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument, PLUGIN_INSTANCE_SCOPE_TYPE,
};
use super::{document, ConfigActor, ConfigurationDocument};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Namespace of a destination's host-owned placement document.
pub const DESTINATION_NAMESPACE: &str = "core.destination";

/// Schema version of [`DestinationPlacementV1`].
pub const DESTINATION_SCHEMA_VERSION: i64 = 1;

/// How many times [`set_destination_filter`] retries a lost
/// compare-and-swap before it reports a conflict.
const SET_ATTEMPTS: u32 = 4;

/// The body of a `core.destination` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DestinationPlacementV1 {
    /// Which books the destination takes; no rules take every book.
    pub filter: DestinationFilter,
}

/// The `core.destination` address for one plugin instance.
#[must_use]
pub fn destination_key(instance_id: &str) -> DocumentKey {
    DocumentKey {
        scope_type: PLUGIN_INSTANCE_SCOPE_TYPE.to_string(),
        scope_id: instance_id.to_string(),
        namespace: DESTINATION_NAMESPACE.to_string(),
    }
}

/// Loads one destination's placement document.
///
/// # Returns
///
/// The stored document, or `None` when nobody has written one: the
/// destination then takes every book.
///
/// # Errors
///
/// Returns the read error, or an error when the stored document is invalid
/// or of an unknown schema.
pub async fn load_destination_placement(
    store: &LibraryStore,
    instance_id: &str,
) -> Result<Option<ConfigurationDocument<DestinationPlacementV1>>> {
    load_document(store, &destination_key(instance_id))
        .await?
        .map(parse_placement)
        .transpose()
}

/// One destination's filter: the stored one, or a filter that takes every
/// book.
///
/// # Errors
///
/// Returns the read or parse error.
pub async fn destination_filter(
    store: &LibraryStore,
    instance_id: &str,
) -> Result<DestinationFilter> {
    Ok(load_destination_placement(store, instance_id)
        .await?
        .map(|document| document.body.filter)
        .unwrap_or_default())
}

/// The stored filters of the given destinations, keyed by plugin instance id.
///
/// Each document is read by its primary key, one row per read, so the
/// lookup stays inside the smallest result limits an adapter may advertise
/// however many destinations have stored a filter. Pass the destinations
/// deployed on this host; ids without a document are absent from the map
/// and take every book.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `instance_ids` - The destinations to read, by plugin instance id.
///
/// # Errors
///
/// Returns the read error, or a parse error for any stored document; an
/// acquire fails closed on it rather than writing where a filter said not
/// to.
pub async fn destination_filters<'a>(
    store: &LibraryStore,
    instance_ids: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeMap<String, DestinationFilter>> {
    let mut filters = BTreeMap::new();
    for id in instance_ids {
        if filters.contains_key(id) {
            continue;
        }
        if let Some(document) = load_destination_placement(store, id).await? {
            filters.insert(id.to_string(), document.body.filter);
        }
    }
    Ok(filters)
}

/// Replaces one destination's filter under compare-and-swap, creating the
/// document when absent and retrying a few times on a concurrent edit.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - Who is writing, for authorization and the audit log.
/// * `instance_id` - The destination's plugin instance id.
/// * `filter` - The new filter; no rules take every book.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for a filter that fails
/// [`DestinationFilter::validate`], an error when the actor may not write,
/// or [`LibraryError::Conflict`] when the document keeps changing
/// underneath.
pub async fn set_destination_filter(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &str,
    filter: DestinationFilter,
) -> Result<ConfigurationDocument<DestinationPlacementV1>> {
    let filter = filter.normalized();
    filter
        .validate()
        .map_err(|err| LibraryError::Invalid(err.to_string()))?;
    let body = DestinationPlacementV1 { filter };
    let key = destination_key(instance_id);
    for attempt in 0..SET_ATTEMPTS {
        let current = match load_destination_placement(store, instance_id).await? {
            Some(document) => document,
            None => parse_placement(
                import_if_absent(
                    store,
                    actor,
                    &key,
                    DESTINATION_SCHEMA_VERSION,
                    &to_json(&DestinationPlacementV1::default())?,
                    &format!("core-destination-{instance_id}-default"),
                )
                .await?,
            )?,
        };
        if current.body == body {
            return Ok(current);
        }
        let operation_id = format!(
            "core-destination-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            DESTINATION_SCHEMA_VERSION,
            current.revision,
            &to_json(&body)?,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => return parse_placement(stored),
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(format!(
        "the filter of destination {instance_id} kept changing; retry"
    )))
}

/// Parses a stored `core.destination` row, failing closed on an unknown
/// schema or a filter that does not validate.
fn parse_placement(
    stored: StoredDocument,
) -> Result<ConfigurationDocument<DestinationPlacementV1>> {
    if stored.schema_version != DESTINATION_SCHEMA_VERSION {
        return Err(documents::unsupported(
            DESTINATION_NAMESPACE,
            stored.schema_version,
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Invalid(format!("invalid {DESTINATION_NAMESPACE}: {err}")))?;
    let filter_json = value
        .get("filter")
        .map(serde_json::Value::to_string)
        .unwrap_or_else(|| "{}".to_string());
    let filter = bookclerk_config::parse_destination_filter(filter_json.as_bytes())
        .map_err(|err| LibraryError::Invalid(format!("invalid {DESTINATION_NAMESPACE}: {err}")))?;
    Ok(document(stored, DestinationPlacementV1 { filter }))
}

/// Serializes a body for storage.
fn to_json(body: &DestinationPlacementV1) -> Result<String> {
    serde_json::to_string(body)
        .map_err(|err| LibraryError::Invalid(format!("{DESTINATION_NAMESPACE} body: {err}")))
}

#[cfg(test)]
mod tests {
    use bookclerk_config::{
        FilterEffect, FilterField, FilterRule, MAX_FILTER_RULES, MAX_FILTER_VALUE_BYTES,
    };

    use super::*;

    /// An operator actor for writes.
    fn operator() -> ConfigActor {
        ConfigActor::Operator {
            id: "operator:test".into(),
        }
    }

    /// A one-rule filter.
    fn storefront(value: &str) -> DestinationFilter {
        DestinationFilter {
            rules: vec![FilterRule {
                field: FilterField::Storefront,
                value: value.into(),
                effect: FilterEffect::Include,
            }],
        }
    }

    #[tokio::test]
    async fn an_unwritten_destination_takes_every_book() {
        let store = crate::control_plane::tests::memory_store().await;
        assert!(load_destination_placement(&store, "a")
            .await
            .unwrap()
            .is_none());
        assert!(destination_filter(&store, "a")
            .await
            .unwrap()
            .takes_everything());
        assert!(destination_filters(&store, ["a"]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn filters_are_stored_per_instance_and_replaced() {
        let store = crate::control_plane::tests::memory_store().await;
        let first = set_destination_filter(&store, &operator(), "a", storefront(" audible "))
            .await
            .unwrap();
        assert_eq!(first.body.filter, storefront("audible"));
        let again = set_destination_filter(&store, &operator(), "a", storefront("audible"))
            .await
            .unwrap();
        assert_eq!(
            again.revision, first.revision,
            "an unchanged filter is not rewritten"
        );
        set_destination_filter(&store, &operator(), "b", storefront("libro"))
            .await
            .unwrap();
        let cleared =
            set_destination_filter(&store, &operator(), "a", DestinationFilter::default())
                .await
                .unwrap();
        assert!(cleared.revision > first.revision);
        let all = destination_filters(&store, ["a", "b", "c", "a"])
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert!(all["a"].takes_everything());
        assert_eq!(all["b"], storefront("libro"));
    }

    #[tokio::test]
    async fn a_member_may_not_write_and_bad_filters_are_refused() {
        let store = crate::control_plane::tests::memory_store().await;
        let member = ConfigActor::Member {
            id: "user:9".into(),
        };
        assert!(
            set_destination_filter(&store, &member, "a", storefront("audible"))
                .await
                .is_err()
        );
        let err = set_destination_filter(&store, &operator(), "a", storefront("  "))
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
        assert!(destination_filters(&store, ["a"]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_largest_filter_fits_a_document() {
        let store = crate::control_plane::tests::memory_store().await;
        let filter = DestinationFilter {
            rules: (0..MAX_FILTER_RULES)
                .map(|n| FilterRule {
                    field: FilterField::Account,
                    value: format!("{n:0>width$}", width = MAX_FILTER_VALUE_BYTES),
                    effect: FilterEffect::Exclude,
                })
                .collect(),
        };
        let stored = set_destination_filter(&store, &operator(), "a", filter.clone())
            .await
            .unwrap();
        assert_eq!(stored.body.filter, filter);
    }

    #[tokio::test]
    async fn an_unknown_schema_fails_closed() {
        let store = crate::control_plane::tests::memory_store().await;
        import_if_absent(
            &store,
            &operator(),
            &destination_key("a"),
            DESTINATION_SCHEMA_VERSION + 1,
            r#"{"filter":{}}"#,
            "op-1",
        )
        .await
        .unwrap();
        assert!(destination_filters(&store, ["a"]).await.is_err());
        assert!(
            destination_filters(&store, ["b"]).await.unwrap().is_empty(),
            "only the destinations asked for are read"
        );
        assert!(destination_filter(&store, "a").await.is_err());
    }

    /// Several stored filters are read within every profile's limits,
    /// including the minimum's one row per statement and no streamed reads.
    #[tokio::test]
    async fn many_filters_load_under_every_database_profile() {
        for profile in crate::test_support::Profile::ALL {
            let name = profile.name();
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            let ids = ["a", "b", "c", "d"];
            for id in ids {
                set_destination_filter(store, &operator(), id, storefront(id))
                    .await
                    .unwrap_or_else(|err| panic!("{name}: {err}"));
            }
            let all = destination_filters(store, ids.into_iter().chain(["e"]))
                .await
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            assert_eq!(all.len(), 4, "{name}");
            for id in ids {
                assert_eq!(all[id], storefront(id), "{name}");
            }
            assert_eq!(kit.refused_begins(), 0, "{name}");
        }
    }
}
