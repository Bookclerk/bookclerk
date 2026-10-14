//! Durable per-plugin key/value storage (`plugin_kv`) behind the
//! `[[kv_namespaces]]` binding.
//!
//! One row per `(plugin_key, namespace, key)`, written through the SeaORM
//! entity so every backend (SQLite, PostgreSQL, D1, the adapter proxy) gets
//! its own statement shape. Values are stored as base64 text;
//! expiry is lazy: an expired row reads as absent and is removed by the next
//! write to the same key or by [`LibraryStore::kv_prune_expired`]. Limits
//! (`max_value_bytes`, `max_keys`) come from the manifest row and are
//! enforced here, not by the binding transport, so a native and a workerd
//! guest see the same refusals.

use base64::Engine as _;
use chrono::{DateTime, Utc};

use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveValue::Set, ColumnTrait, Condition, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect,
};

use super::{now_str, LibraryStore};
use crate::entities::plugin_kv;
use crate::error::{LibraryError, Result};

/// Default page size for [`LibraryStore::kv_list`] when the caller passes 0.
pub const KV_LIST_DEFAULT_LIMIT: u32 = 100;

/// One live key/value row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvEntry {
    /// Key within the namespace.
    pub key: String,
    /// Value bytes.
    pub value: Vec<u8>,
    /// Opaque JSON metadata text; empty when none.
    pub metadata: String,
    /// Absolute expiry, when set.
    pub expires_at: Option<DateTime<Utc>>,
}

/// One page of [`LibraryStore::kv_list`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KvListPage {
    /// Keys in lexical order (values are not loaded).
    pub keys: Vec<KvEntry>,
    /// True when no further page exists.
    pub list_complete: bool,
    /// Opaque cursor for the next page; empty when complete.
    pub cursor: String,
}

/// A write plus the namespace limits it must satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvWriteSpec {
    /// Value bytes.
    pub value: Vec<u8>,
    /// Opaque JSON metadata text; empty when none.
    pub metadata: String,
    /// Absolute expiry; `None` never expires.
    pub expires_at: Option<DateTime<Utc>>,
    /// Largest value this namespace accepts.
    pub max_value_bytes: u32,
    /// Most live keys this namespace holds.
    pub max_keys: u32,
}

/// Why a write was refused, so the binding can map it onto a wire code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvPutOutcome {
    /// The row was written.
    Written,
    /// `value` exceeds `max_value_bytes`.
    ValueTooLarge {
        /// Bytes offered.
        len: usize,
        /// Namespace cap.
        max: u32,
    },
    /// The namespace already holds `max_keys` other live keys.
    KeyLimitReached {
        /// Namespace cap.
        max: u32,
    },
}

impl LibraryStore {
    /// Reads one key; `None` when absent or expired.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub async fn kv_get(
        &self,
        plugin_key: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<KvEntry>> {
        let row = plugin_kv::Entity::find_by_id((
            plugin_key.to_string(),
            namespace.to_string(),
            key.to_string(),
        ))
        .one(self.db())
        .await
        .map_err(LibraryError::Orm)?;
        let now = Utc::now();
        match row {
            Some(row) => {
                let entry = row_to_entry(row)?;
                Ok(is_live(entry.expires_at, now).then_some(entry))
            }
            None => Ok(None),
        }
    }

    /// Writes one key, replacing any previous value, metadata, and expiry.
    ///
    /// Enforces `spec.max_value_bytes` and `spec.max_keys` (expired rows do
    /// not count and are reclaimed to make room).
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn kv_put(
        &self,
        plugin_key: &str,
        namespace: &str,
        key: &str,
        spec: KvWriteSpec,
    ) -> Result<KvPutOutcome> {
        if spec.value.len() > spec.max_value_bytes as usize {
            return Ok(KvPutOutcome::ValueTooLarge {
                len: spec.value.len(),
                max: spec.max_value_bytes,
            });
        }
        let exists = self.kv_get(plugin_key, namespace, key).await?.is_some();
        if !exists {
            let live = self.kv_count(plugin_key, namespace).await?;
            if live >= u64::from(spec.max_keys) {
                // The count only saw live keys; reclaim stale rows before
                // refusing so an expired key never holds a slot.
                self.kv_prune_expired_in(plugin_key, namespace).await?;
                if self.kv_count(plugin_key, namespace).await? >= u64::from(spec.max_keys) {
                    return Ok(KvPutOutcome::KeyLimitReached { max: spec.max_keys });
                }
            }
        }
        let model = plugin_kv::ActiveModel {
            plugin_key: Set(plugin_key.to_string()),
            namespace: Set(namespace.to_string()),
            key: Set(key.to_string()),
            value: Set(base64::engine::general_purpose::STANDARD.encode(&spec.value)),
            metadata: Set(spec.metadata),
            expires_at: Set(spec.expires_at.map(|t| t.to_rfc3339()).unwrap_or_default()),
            updated_at: Set(now_str()),
        };
        plugin_kv::Entity::insert(model)
            .on_conflict(
                OnConflict::columns([
                    plugin_kv::Column::PluginKey,
                    plugin_kv::Column::Namespace,
                    plugin_kv::Column::Key,
                ])
                .update_columns([
                    plugin_kv::Column::Value,
                    plugin_kv::Column::Metadata,
                    plugin_kv::Column::ExpiresAt,
                    plugin_kv::Column::UpdatedAt,
                ])
                .to_owned(),
            )
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(KvPutOutcome::Written)
    }

    /// Deletes one key. Returns whether a live value existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn kv_delete(&self, plugin_key: &str, namespace: &str, key: &str) -> Result<bool> {
        let existed = self.kv_get(plugin_key, namespace, key).await?.is_some();
        plugin_kv::Entity::delete_many()
            .filter(plugin_kv::Column::PluginKey.eq(plugin_key))
            .filter(plugin_kv::Column::Namespace.eq(namespace))
            .filter(plugin_kv::Column::Key.eq(key))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(existed)
    }

    /// Lists live keys in lexical order, `limit` per page (0 means
    /// [`KV_LIST_DEFAULT_LIMIT`]). `cursor` is the opaque value a previous
    /// page returned; empty starts from the beginning.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or the cursor is malformed.
    pub async fn kv_list(
        &self,
        plugin_key: &str,
        namespace: &str,
        prefix: &str,
        cursor: &str,
        limit: u32,
    ) -> Result<KvListPage> {
        let limit = if limit == 0 {
            KV_LIST_DEFAULT_LIMIT
        } else {
            limit
        };
        let mut after = decode_cursor(cursor)?;
        let now = Utc::now();
        let mut keys = Vec::with_capacity(limit as usize);
        // Expired rows are skipped after the query, so page until the live
        // keys fill the limit or the namespace runs out.
        loop {
            let rows = plugin_kv::Entity::find()
                .filter(plugin_kv::Column::PluginKey.eq(plugin_key))
                .filter(plugin_kv::Column::Namespace.eq(namespace))
                .filter(plugin_kv::Column::Key.gt(after.clone()))
                .order_by_asc(plugin_kv::Column::Key)
                .limit(u64::from(limit) + 1)
                .all(self.db())
                .await
                .map_err(LibraryError::Orm)?;
            let exhausted = rows.len() <= limit as usize;
            for row in rows {
                after.clone_from(&row.key);
                if !row.key.starts_with(prefix) {
                    if !prefix.is_empty() && row.key.as_str() > prefix {
                        // Lexically past every key with the prefix.
                        return Ok(complete(keys));
                    }
                    continue;
                }
                let expires_at = parse_expiry(&row.expires_at);
                if !is_live(expires_at, now) {
                    continue;
                }
                let key = row.key;
                keys.push(KvEntry {
                    key: key.clone(),
                    value: Vec::new(),
                    metadata: row.metadata,
                    expires_at,
                });
                if keys.len() == limit as usize {
                    return Ok(KvListPage {
                        keys,
                        list_complete: false,
                        cursor: encode_cursor(&key),
                    });
                }
            }
            if exhausted {
                return Ok(complete(keys));
            }
        }
    }

    /// Live keys in one namespace.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub async fn kv_count(&self, plugin_key: &str, namespace: &str) -> Result<u64> {
        let now = Utc::now().to_rfc3339();
        plugin_kv::Entity::find()
            .filter(plugin_kv::Column::PluginKey.eq(plugin_key))
            .filter(plugin_kv::Column::Namespace.eq(namespace))
            .filter(
                Condition::any()
                    .add(plugin_kv::Column::ExpiresAt.eq(""))
                    .add(plugin_kv::Column::ExpiresAt.gt(now)),
            )
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// Removes expired rows of one namespace. Returns the rows removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn kv_prune_expired_in(&self, plugin_key: &str, namespace: &str) -> Result<u64> {
        let now = Utc::now().to_rfc3339();
        let result = plugin_kv::Entity::delete_many()
            .filter(plugin_kv::Column::PluginKey.eq(plugin_key))
            .filter(plugin_kv::Column::Namespace.eq(namespace))
            .filter(plugin_kv::Column::ExpiresAt.ne(""))
            .filter(plugin_kv::Column::ExpiresAt.lte(now))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// Removes every expired row across all plugins and namespaces.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn kv_prune_expired(&self) -> Result<u64> {
        let now = Utc::now().to_rfc3339();
        let result = plugin_kv::Entity::delete_many()
            .filter(plugin_kv::Column::ExpiresAt.ne(""))
            .filter(plugin_kv::Column::ExpiresAt.lte(now))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }

    /// Removes every row of one plugin (retire / uninstall).
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn kv_delete_plugin(&self, plugin_key: &str) -> Result<u64> {
        let result = plugin_kv::Entity::delete_many()
            .filter(plugin_kv::Column::PluginKey.eq(plugin_key))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected)
    }
}

/// The final page of a listing.
fn complete(keys: Vec<KvEntry>) -> KvListPage {
    KvListPage {
        keys,
        list_complete: true,
        cursor: String::new(),
    }
}

/// True when `expires_at` is unset or still in the future.
fn is_live(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    expires_at.is_none_or(|t| t > now)
}

/// Parses the stored expiry text; empty or malformed means never.
fn parse_expiry(text: &str) -> Option<DateTime<Utc>> {
    if text.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Maps one `plugin_kv` row onto a [`KvEntry`].
fn row_to_entry(row: plugin_kv::Model) -> Result<KvEntry> {
    let value = base64::engine::general_purpose::STANDARD
        .decode(row.value.as_bytes())
        .map_err(|err| {
            LibraryError::Other(anyhow::anyhow!("plugin_kv value is not base64: {err}"))
        })?;
    Ok(KvEntry {
        expires_at: parse_expiry(&row.expires_at),
        key: row.key,
        value,
        metadata: row.metadata,
    })
}

/// Opaque list cursor: the last key of the page, base64-encoded.
fn encode_cursor(last_key: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(last_key.as_bytes())
}

/// Decodes a list cursor; empty means the first page.
fn decode_cursor(cursor: &str) -> Result<String> {
    if cursor.is_empty() {
        return Ok(String::new());
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("malformed kv list cursor")))?;
    String::from_utf8(bytes)
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("malformed kv list cursor")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLUGIN: &str = "path:file:///tmp/kv-plugin#kv";

    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        )
    }

    fn spec(value: &[u8]) -> KvWriteSpec {
        KvWriteSpec {
            value: value.to_vec(),
            metadata: String::new(),
            expires_at: None,
            max_value_bytes: 1024,
            max_keys: 3,
        }
    }

    #[tokio::test]
    async fn put_get_delete_round_trip_with_metadata_and_expiry() {
        let store = memory_store().await;
        assert!(store.kv_get(PLUGIN, "ns", "a").await.unwrap().is_none());
        let outcome = store
            .kv_put(
                PLUGIN,
                "ns",
                "a",
                KvWriteSpec {
                    metadata: r#"{"page":1}"#.into(),
                    ..spec(b"one")
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, KvPutOutcome::Written);
        let entry = store.kv_get(PLUGIN, "ns", "a").await.unwrap().expect("row");
        assert_eq!(entry.value, b"one");
        assert_eq!(entry.metadata, r#"{"page":1}"#);
        assert!(entry.expires_at.is_none());
        // Same key in another namespace or plugin is a different row.
        assert!(store.kv_get(PLUGIN, "other", "a").await.unwrap().is_none());
        assert!(store
            .kv_get("other-plugin", "ns", "a")
            .await
            .unwrap()
            .is_none());
        // Overwrite replaces value and metadata.
        store.kv_put(PLUGIN, "ns", "a", spec(b"two")).await.unwrap();
        let entry = store.kv_get(PLUGIN, "ns", "a").await.unwrap().expect("row");
        assert_eq!(entry.value, b"two");
        assert!(entry.metadata.is_empty());
        // Expired rows read as absent and do not count.
        store
            .kv_put(
                PLUGIN,
                "ns",
                "stale",
                KvWriteSpec {
                    expires_at: Some(Utc::now() - chrono::Duration::seconds(1)),
                    ..spec(b"x")
                },
            )
            .await
            .unwrap();
        assert!(store.kv_get(PLUGIN, "ns", "stale").await.unwrap().is_none());
        assert_eq!(store.kv_count(PLUGIN, "ns").await.unwrap(), 1);
        assert!(store.kv_delete(PLUGIN, "ns", "a").await.unwrap());
        assert!(!store.kv_delete(PLUGIN, "ns", "a").await.unwrap());
        assert_eq!(store.kv_prune_expired().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn limits_refuse_oversize_values_and_too_many_keys() {
        let store = memory_store().await;
        let big = store
            .kv_put(PLUGIN, "ns", "big", spec(&[0u8; 1025]))
            .await
            .unwrap();
        assert_eq!(
            big,
            KvPutOutcome::ValueTooLarge {
                len: 1025,
                max: 1024
            }
        );
        for key in ["a", "b", "c"] {
            assert_eq!(
                store.kv_put(PLUGIN, "ns", key, spec(b"v")).await.unwrap(),
                KvPutOutcome::Written
            );
        }
        assert_eq!(
            store.kv_put(PLUGIN, "ns", "d", spec(b"v")).await.unwrap(),
            KvPutOutcome::KeyLimitReached { max: 3 }
        );
        // Rewriting an existing key never counts against the cap.
        assert_eq!(
            store.kv_put(PLUGIN, "ns", "a", spec(b"v2")).await.unwrap(),
            KvPutOutcome::Written
        );
        // An expired row is reclaimed to make room.
        store
            .kv_put(
                PLUGIN,
                "ns",
                "a",
                KvWriteSpec {
                    expires_at: Some(Utc::now() - chrono::Duration::seconds(1)),
                    ..spec(b"v")
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store.kv_put(PLUGIN, "ns", "d", spec(b"v")).await.unwrap(),
            KvPutOutcome::Written
        );
        assert_eq!(store.kv_count(PLUGIN, "ns").await.unwrap(), 3);
    }

    /// Two hosts, two connections, one library: the cursor an instance wrote
    /// on host A is what the same instance reads after moving to host B.
    #[tokio::test]
    async fn cursor_written_on_one_node_reads_back_on_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("library.db");
        let node_a = {
            let db = bookclerk_plugin_database_sqlite::open(&path)
                .await
                .expect("open a");
            crate::host_schema::apply_host_schema(&db)
                .await
                .expect("schema");
            LibraryStore::from_connection(db)
        };
        node_a
            .kv_put(
                PLUGIN,
                "sync.cursors",
                "audible:acct-1",
                KvWriteSpec {
                    metadata: r#"{"host":"a"}"#.into(),
                    ..spec(b"page-42")
                },
            )
            .await
            .expect("write on host a");
        drop(node_a);
        let node_b = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open(&path)
                .await
                .expect("open b"),
        );
        let entry = node_b
            .kv_get(PLUGIN, "sync.cursors", "audible:acct-1")
            .await
            .expect("read on host b")
            .expect("cursor follows the instance");
        assert_eq!(entry.value, b"page-42");
        assert_eq!(entry.metadata, r#"{"host":"a"}"#);
        assert!(
            node_b
                .kv_get("other-plugin", "sync.cursors", "audible:acct-1")
                .await
                .expect("read")
                .is_none(),
            "another plugin key never sees it"
        );
    }

    #[tokio::test]
    async fn list_pages_in_order_by_prefix_and_skips_expired() {
        let store = memory_store().await;
        let mut big = spec(b"v");
        big.max_keys = 100;
        for key in ["b2", "a1", "a3", "a2", "c1"] {
            store.kv_put(PLUGIN, "ns", key, big.clone()).await.unwrap();
        }
        store
            .kv_put(
                PLUGIN,
                "ns",
                "a0",
                KvWriteSpec {
                    expires_at: Some(Utc::now() - chrono::Duration::seconds(1)),
                    ..big.clone()
                },
            )
            .await
            .unwrap();
        let page = store.kv_list(PLUGIN, "ns", "a", "", 2).await.unwrap();
        assert_eq!(
            page.keys.iter().map(|k| k.key.as_str()).collect::<Vec<_>>(),
            ["a1", "a2"]
        );
        assert!(!page.list_complete);
        let next = store
            .kv_list(PLUGIN, "ns", "a", &page.cursor, 2)
            .await
            .unwrap();
        assert_eq!(
            next.keys.iter().map(|k| k.key.as_str()).collect::<Vec<_>>(),
            ["a3"]
        );
        assert!(next.list_complete);
        let all = store.kv_list(PLUGIN, "ns", "", "", 0).await.unwrap();
        assert_eq!(all.keys.len(), 5);
        assert!(all.list_complete);
        assert!(store
            .kv_list(PLUGIN, "ns", "", "not-a-cursor!", 2)
            .await
            .is_err());
        assert!(store
            .kv_list(PLUGIN, "empty", "", "", 2)
            .await
            .unwrap()
            .keys
            .is_empty());
        assert_eq!(store.kv_delete_plugin(PLUGIN).await.unwrap(), 6);
    }
}
