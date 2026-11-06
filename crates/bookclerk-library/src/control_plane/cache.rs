//! `core.cache`: retention limits for the daemon's on-disk image caches.
//!
//! One cluster document holds the default disk budget for each node's cover
//! and image caches and how long an entry may go unviewed before it is
//! evicted. A node may override the budget in its `node.runtime` document
//! ([`NodeRuntimeSettingsV1::cache_budget_bytes`]).
//!
//! Unlike `core.events` and `core.output` there is no `config.toml` seed:
//! an absent document means the defaults, and the first write creates it.
//! See `docs/server/configuration.md#image-cache`.

use serde::{Deserialize, Serialize};

use super::documents::{
    self, import_if_absent, load_document, replace_document, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::{document, ConfigActor, ConfigurationDocument, NodeRuntimeSettingsV1};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Namespace of the cluster cache document.
pub const CACHE_NAMESPACE: &str = "core.cache";

/// Schema version of [`CacheSettingsV1`].
pub const CACHE_SCHEMA_VERSION: i64 = 1;

/// Default per-node cache budget: 1 GiB.
pub const DEFAULT_CACHE_BUDGET_BYTES: u64 = 1 << 30;

/// Smallest accepted cache budget: 16 MiB, room for a few hundred covers.
pub const MIN_CACHE_BUDGET_BYTES: u64 = 16 << 20;

/// Largest accepted cache budget: 1 TiB.
pub const MAX_CACHE_BUDGET_BYTES: u64 = 1 << 40;

/// Default days an entry may go unviewed before it is evicted.
pub const DEFAULT_CACHE_UNVIEWED_DAYS: u32 = 90;

/// Largest accepted `unviewed_days`, about ten years.
pub const MAX_CACHE_UNVIEWED_DAYS: u32 = 3650;

/// Settings keys the cache document owns, in display order.
pub const CACHE_SETTING_KEYS: &[&str] = &["cache.budget_bytes", "cache.unviewed_days"];

/// True when `key` is one of [`CACHE_SETTING_KEYS`].
///
/// # Arguments
///
/// * `key` - A dotted settings key such as `cache.budget_bytes`.
#[must_use]
pub fn is_cache_setting_key(key: &str) -> bool {
    CACHE_SETTING_KEYS.contains(&key)
}

/// Typed `core.cache` document, schema version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheSettingsV1 {
    /// Disk space, in bytes, each node's image caches may use together
    /// unless the node overrides it (16 MiB to 1 TiB).
    #[serde(default = "default_budget_bytes")]
    #[schema(
        minimum = 16_777_216_u64,
        maximum = 1_099_511_627_776_u64,
        example = 1_073_741_824_u64
    )]
    pub budget_bytes: u64,
    /// Days an entry may go unviewed before the sweep evicts it (1 to 3650).
    #[serde(default = "default_unviewed_days")]
    #[schema(minimum = 1, maximum = 3650, example = 90)]
    pub unviewed_days: u32,
}

/// Serde default for [`CacheSettingsV1::budget_bytes`].
fn default_budget_bytes() -> u64 {
    DEFAULT_CACHE_BUDGET_BYTES
}

/// Serde default for [`CacheSettingsV1::unviewed_days`].
fn default_unviewed_days() -> u32 {
    DEFAULT_CACHE_UNVIEWED_DAYS
}

impl Default for CacheSettingsV1 {
    fn default() -> Self {
        Self {
            budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
            unviewed_days: DEFAULT_CACHE_UNVIEWED_DAYS,
        }
    }
}

impl CacheSettingsV1 {
    /// Rejects values outside the domain bounds.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] when a field is out of range.
    pub fn validate(&self) -> Result<()> {
        validate_budget("cache.budget_bytes", self.budget_bytes)?;
        if !(1..=MAX_CACHE_UNVIEWED_DAYS).contains(&self.unviewed_days) {
            return Err(invalid("cache.unviewed_days must be 1..=3650"));
        }
        Ok(())
    }

    /// The value of one settings key, as text.
    ///
    /// # Arguments
    ///
    /// * `key` - One of [`CACHE_SETTING_KEYS`].
    ///
    /// # Returns
    ///
    /// The value, or `None` for a key this document does not own.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "cache.budget_bytes" => Some(self.budget_bytes.to_string()),
            "cache.unviewed_days" => Some(self.unviewed_days.to_string()),
            _ => None,
        }
    }

    /// Parses `value` into the field `key` names and validates the result.
    ///
    /// # Arguments
    ///
    /// * `key` - One of [`CACHE_SETTING_KEYS`].
    /// * `value` - The new value as text: a whole number.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for an unknown key, a value that is
    /// not a whole number, or one outside the field's range.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let value = value.trim();
        match key {
            "cache.budget_bytes" => {
                self.budget_bytes = value
                    .parse()
                    .map_err(|_| invalid("cache.budget_bytes must be a whole number of bytes"))?;
            }
            "cache.unviewed_days" => {
                self.unviewed_days = value
                    .parse()
                    .map_err(|_| invalid("cache.unviewed_days must be a whole number of days"))?;
            }
            other => return Err(invalid(&format!("unknown cache setting: {other}"))),
        }
        self.validate()
    }

    /// The budget that applies on a node: its own override, else the
    /// cluster budget.
    ///
    /// # Arguments
    ///
    /// * `node` - The node's runtime document.
    #[must_use]
    pub fn budget_for(&self, node: &NodeRuntimeSettingsV1) -> u64 {
        node.cache_budget_bytes.unwrap_or(self.budget_bytes)
    }
}

/// Rejects a cache budget outside 16 MiB..=1 TiB.
///
/// # Arguments
///
/// * `name` - Field name for the error message.
/// * `bytes` - The budget.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] when `bytes` is out of range.
pub(super) fn validate_budget(name: &str, bytes: u64) -> Result<()> {
    if (MIN_CACHE_BUDGET_BYTES..=MAX_CACHE_BUDGET_BYTES).contains(&bytes) {
        Ok(())
    } else {
        Err(invalid(&format!(
            "{name} must be between {MIN_CACHE_BUDGET_BYTES} (16 MiB) and \
             {MAX_CACHE_BUDGET_BYTES} (1 TiB) bytes"
        )))
    }
}

/// Loads the cluster cache document.
///
/// # Returns
///
/// The stored document, or `None` when nobody has written one yet: the
/// defaults of [`CacheSettingsV1::default`] then apply.
///
/// # Errors
///
/// Returns an error when the database read fails or the stored document is
/// unsupported or invalid.
pub async fn load_cache_settings(
    store: &LibraryStore,
) -> Result<Option<ConfigurationDocument<CacheSettingsV1>>> {
    load_document(store, &DocumentKey::cluster(CACHE_NAMESPACE))
        .await?
        .map(parse_cache)
        .transpose()
}

/// Applies `edit` to the cluster cache document under compare-and-swap,
/// creating it with the defaults when absent and retrying a few times on a
/// concurrent edit. Nodes pick the change up at their next heartbeat.
///
/// # Arguments
///
/// * `store` - The library database.
/// * `actor` - Who is writing, for authorization and the audit log.
/// * `edit` - Changes the body; an error aborts without writing.
///
/// # Errors
///
/// Returns the error `edit` returned, an error when the edited body is
/// invalid or the actor may not write, or [`LibraryError::Conflict`] when
/// the document keeps changing underneath.
pub async fn update_cache_settings(
    store: &LibraryStore,
    actor: &ConfigActor,
    edit: impl Fn(&mut CacheSettingsV1) -> Result<()>,
) -> Result<ConfigurationDocument<CacheSettingsV1>> {
    let key = DocumentKey::cluster(CACHE_NAMESPACE);
    for attempt in 0..4 {
        let current = match load_cache_settings(store).await? {
            Some(document) => document,
            None => {
                let json = to_json(&CacheSettingsV1::default())?;
                parse_cache(
                    import_if_absent(
                        store,
                        actor,
                        &key,
                        CACHE_SCHEMA_VERSION,
                        &json,
                        "core-cache-default",
                    )
                    .await?,
                )?
            }
        };
        let mut body = current.body.clone();
        edit(&mut body)?;
        body.validate()?;
        if body == current.body {
            return Ok(current);
        }
        let operation_id = format!(
            "core-cache-{}-{attempt}-{}",
            current.revision,
            uuid::Uuid::new_v4().simple()
        );
        match replace_document(
            store,
            actor,
            &key,
            CACHE_SCHEMA_VERSION,
            current.revision,
            &to_json(&body)?,
            &operation_id,
        )
        .await?
        {
            ReplaceOutcome::Applied(stored) => return parse_cache(stored),
            ReplaceOutcome::Conflict { .. } | ReplaceOutcome::Replayed { .. } => {}
        }
    }
    Err(LibraryError::Conflict(
        "core.cache kept changing; retry".into(),
    ))
}

/// Parses a stored `core.cache` row, failing closed on an unknown schema.
fn parse_cache(stored: StoredDocument) -> Result<ConfigurationDocument<CacheSettingsV1>> {
    if stored.schema_version != CACHE_SCHEMA_VERSION {
        return Err(documents::unsupported(
            CACHE_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: CacheSettingsV1 = serde_json::from_str(&stored.document_json)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))?;
    body.validate()?;
    Ok(document(stored, body))
}

/// Serializes a body for storage.
fn to_json(body: &CacheSettingsV1) -> Result<String> {
    serde_json::to_string(body)
        .map_err(|err| LibraryError::Invalid(format!("invalid configuration: {err}")))
}

/// Error for a cache value outside the domain bounds.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Invalid(format!("invalid configuration: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::tests::{memory_store, operator};
    use crate::control_plane::update_node_runtime;

    #[test]
    fn defaults_are_one_gibibyte_and_ninety_days() {
        let body = CacheSettingsV1::default();
        assert_eq!(body.budget_bytes, 1_073_741_824);
        assert_eq!(body.unviewed_days, 90);
        assert!(body.validate().is_ok());
        let parsed: CacheSettingsV1 = serde_json::from_str("{}").expect("empty document");
        assert_eq!(parsed, body);
    }

    #[test]
    fn set_parses_and_bounds_each_key() {
        let mut body = CacheSettingsV1::default();
        body.set("cache.budget_bytes", " 33554432 ")
            .expect("32 MiB");
        assert_eq!(body.get("cache.budget_bytes").as_deref(), Some("33554432"));
        body.set("cache.unviewed_days", "7").expect("a week");
        assert_eq!(body.get("cache.unviewed_days").as_deref(), Some("7"));
        for (key, value) in [
            ("cache.budget_bytes", "1024"),
            ("cache.budget_bytes", "1099511627777"),
            ("cache.budget_bytes", "lots"),
            ("cache.unviewed_days", "0"),
            ("cache.unviewed_days", "3651"),
            ("cache.unviewed_days", "-1"),
            ("cache.other", "1"),
        ] {
            let mut copy = CacheSettingsV1::default();
            assert!(copy.set(key, value).is_err(), "{key}={value} accepted");
        }
        assert_eq!(body.get("cache.other"), None);
        assert!(is_cache_setting_key("cache.unviewed_days"));
        assert!(!is_cache_setting_key("cache.other"));
    }

    #[test]
    fn unknown_fields_are_refused() {
        let parsed: std::result::Result<CacheSettingsV1, _> =
            serde_json::from_str(r#"{"budget_bytes":16777216,"surprise":1}"#);
        assert!(parsed.is_err());
    }

    #[test]
    fn a_node_override_wins_over_the_cluster_budget() {
        let cluster = CacheSettingsV1::default();
        let mut node = NodeRuntimeSettingsV1::new();
        assert_eq!(cluster.budget_for(&node), DEFAULT_CACHE_BUDGET_BYTES);
        node.cache_budget_bytes = Some(MIN_CACHE_BUDGET_BYTES);
        assert_eq!(cluster.budget_for(&node), MIN_CACHE_BUDGET_BYTES);
    }

    #[tokio::test]
    async fn an_absent_document_reads_as_none_and_the_first_update_creates_it() {
        let store = memory_store().await;
        assert!(load_cache_settings(&store).await.expect("load").is_none());

        let doc = update_cache_settings(&store, &operator(), |body| {
            body.set("cache.unviewed_days", "30")
        })
        .await
        .expect("update");
        assert_eq!(doc.body.unviewed_days, 30);
        assert_eq!(doc.body.budget_bytes, DEFAULT_CACHE_BUDGET_BYTES);
        assert_eq!(doc.namespace, CACHE_NAMESPACE);

        let loaded = load_cache_settings(&store)
            .await
            .expect("load")
            .expect("stored");
        assert_eq!(loaded, doc);

        let unchanged = update_cache_settings(&store, &operator(), |_| Ok(()))
            .await
            .expect("no-op");
        assert_eq!(unchanged.revision, doc.revision);
    }

    #[tokio::test]
    async fn a_rejected_edit_writes_nothing() {
        let store = memory_store().await;
        let err = update_cache_settings(&store, &operator(), |body| {
            body.set("cache.budget_bytes", "1")
        })
        .await
        .expect_err("too small");
        assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
        let doc = load_cache_settings(&store).await.expect("load");
        // The default document may exist; the rejected value never does.
        assert!(doc.is_none_or(|doc| doc.body == CacheSettingsV1::default()));
    }

    #[tokio::test]
    async fn the_node_override_round_trips_through_node_runtime() {
        let store = memory_store().await;
        let doc = update_node_runtime(&store, &operator(), "node-a", |body| {
            body.cache_budget_bytes = Some(64 << 20);
        })
        .await
        .expect("override");
        assert_eq!(doc.body.cache_budget_bytes, Some(64 << 20));
        let err = update_node_runtime(&store, &operator(), "node-a", |body| {
            body.cache_budget_bytes = Some(1);
        })
        .await
        .expect_err("too small");
        assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
    }
}
