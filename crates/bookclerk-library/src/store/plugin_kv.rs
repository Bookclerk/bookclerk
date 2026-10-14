//! Durable per-plugin key/value storage (`plugin_kv`) behind the
//! `[[kv_namespaces]]` binding.
//!
//! One row per `(plugin_key, namespace, key)`. Values are stored as base64
//! text so every backend (SQLite, PostgreSQL, D1) carries them identically;
//! expiry is lazy: an expired row reads as absent and is removed by the next
//! write to the same key or by [`LibraryStore::kv_prune_expired`]. Limits
//! (`max_value_bytes`, `max_keys`) come from the manifest row and are
//! enforced here, not by the binding transport, so a native and a workerd
//! guest see the same refusals.

use base64::Engine as _;
use chrono::{DateTime, Utc};

use super::{now_str, LibraryStore};
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
        let rows = bookclerk_db_exec::query_canonical(
            self.db(),
            "SELECT key, value, metadata, expires_at FROM plugin_kv \
             WHERE plugin_key = ? AND namespace = ? AND key = ?",
            [plugin_key.into(), namespace.into(), key.into()],
        )
        .await
        .map_err(LibraryError::Orm)?;
        let now = Utc::now();
        rows.first()
            .map(row_to_entry)
            .transpose()?
            .filter(|entry| is_live(entry.expires_at, now))
            .map_or(Ok(None), |entry| Ok(Some(entry)))
    }

    /// Writes one key, replacing any previous value, metadata, and expiry.
    ///
    /// Enforces `spec.max_value_bytes` and `spec.max_keys` (expired rows do
    /// not count and the first expired row is reclaimed to make room).
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
                // Reclaim expired rows before refusing: the count above only
                // saw live keys, so a stale row may still hold the slot.
                let pruned = self.kv_prune_expired_in(plugin_key, namespace).await?;
                if pruned == 0 {
                    return Ok(KvPutOutcome::KeyLimitReached { max: spec.max_keys });
                }
            }
        }
        let value = base64::engine::general_purpose::STANDARD.encode(&spec.value);
        let expires_at = spec.expires_at.map(|t| t.to_rfc3339()).unwrap_or_default();
        let now = now_str();
        bookclerk_db_exec::execute_canonical(
            self.db(),
            "INSERT INTO plugin_kv \
             (plugin_key, namespace, key, value, metadata, expires_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (plugin_key, namespace, key) DO UPDATE SET \
             value = excluded.value, metadata = excluded.metadata, \
             expires_at = excluded.expires_at, updated_at = excluded.updated_at",
            [
                plugin_key.into(),
                namespace.into(),
                key.into(),
                value.into(),
                spec.metadata.into(),
                expires_at.into(),
                now.into(),
            ],
        )
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
        bookclerk_db_exec::execute_canonical(
            self.db(),
            "DELETE FROM plugin_kv WHERE plugin_key = ? AND namespace = ? AND key = ?",
            [plugin_key.into(), namespace.into(), key.into()],
        )
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
        let after = decode_cursor(cursor)?;
        let now = Utc::now();
        let mut keys = Vec::with_capacity(limit as usize);
        let mut after = after;
        // Expired rows are skipped after the query, so page until the live
        // keys fill the limit or the table runs out.
        loop {
            let rows = bookclerk_db_exec::query_canonical(
                self.db(),
                "SELECT key, metadata, expires_at FROM plugin_kv \
                 WHERE plugin_key = ? AND namespace = ? AND key > ? \
                 ORDER BY key ASC LIMIT ?",
                [
                    plugin_key.into(),
                    namespace.into(),
                    after.clone().into(),
                    i64::from(limit + 1).into(),
                ],
            )
            .await
            .map_err(LibraryError::Orm)?;
            if rows.is_empty() {
                return Ok(KvListPage {
                    keys,
                    list_complete: true,
                    cursor: String::new(),
                });
            }
            let exhausted = rows.len() <= limit as usize;
            for row in &rows {
                let key: String = row.try_get("", "key").map_err(LibraryError::Orm)?;
                after.clone_from(&key);
                if !key.starts_with(prefix) {
                    if key.as_str() > prefix && !prefix.is_empty() {
                        // Lexically past every key with the prefix.
                        return Ok(KvListPage {
                            keys,
                            list_complete: true,
                            cursor: String::new(),
                        });
                    }
                    continue;
                }
                let expires_at =
                    parse_expiry(row.try_get("", "expires_at").map_err(LibraryError::Orm)?);
                if !is_live(expires_at, now) {
                    continue;
                }
                keys.push(KvEntry {
                    key: key.clone(),
                    value: Vec::new(),
                    metadata: row.try_get("", "metadata").map_err(LibraryError::Orm)?,
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
                return Ok(KvListPage {
                    keys,
                    list_complete: true,
                    cursor: String::new(),
                });
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
        let rows = bookclerk_db_exec::query_canonical(
            self.db(),
            "SELECT COUNT(*) AS n FROM plugin_kv \
             WHERE plugin_key = ? AND namespace = ? AND (expires_at = '' OR expires_at > ?)",
            [plugin_key.into(), namespace.into(), now.into()],
        )
        .await
        .map_err(LibraryError::Orm)?;
        let n: i64 = rows
            .first()
            .map(|row| row.try_get("", "n"))
            .transpose()
            .map_err(LibraryError::Orm)?
            .unwrap_or(0);
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Removes expired rows of one namespace. Returns the rows removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn kv_prune_expired_in(&self, plugin_key: &str, namespace: &str) -> Result<u64> {
        let now = Utc::now().to_rfc3339();
        let result = bookclerk_db_exec::execute_canonical(
            self.db(),
            "DELETE FROM plugin_kv WHERE plugin_key = ? AND namespace = ? \
             AND expires_at <> '' AND expires_at <= ?",
            [plugin_key.into(), namespace.into(), now.into()],
        )
        .await
        .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected())
    }

    /// Removes every expired row across all plugins and namespaces.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn kv_prune_expired(&self) -> Result<u64> {
        let now = Utc::now().to_rfc3339();
        let result = bookclerk_db_exec::execute_canonical(
            self.db(),
            "DELETE FROM plugin_kv WHERE expires_at <> '' AND expires_at <= ?",
            [now.into()],
        )
        .await
        .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected())
    }

    /// Removes every row of one plugin (retire / uninstall).
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub async fn kv_delete_plugin(&self, plugin_key: &str) -> Result<u64> {
        let result = bookclerk_db_exec::execute_canonical(
            self.db(),
            "DELETE FROM plugin_kv WHERE plugin_key = ?",
            [plugin_key.into()],
        )
        .await
        .map_err(LibraryError::Orm)?;
        Ok(result.rows_affected())
    }
}

/// True when `expires_at` is unset or still in the future.
fn is_live(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    expires_at.is_none_or(|t| t > now)
}

/// Parses the stored expiry text; empty or malformed means never.
fn parse_expiry(text: String) -> Option<DateTime<Utc>> {
    if text.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(&text)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Maps one `plugin_kv` row onto a [`KvEntry`].
fn row_to_entry(row: &sea_orm::QueryResult) -> Result<KvEntry> {
    let value: String = row.try_get("", "value").map_err(LibraryError::Orm)?;
    let value = base64::engine::general_purpose::STANDARD
        .decode(value.as_bytes())
        .map_err(|err| {
            LibraryError::Other(anyhow::anyhow!("plugin_kv value is not base64: {err}"))
        })?;
    Ok(KvEntry {
        key: row.try_get("", "key").map_err(LibraryError::Orm)?,
        value,
        metadata: row.try_get("", "metadata").map_err(LibraryError::Orm)?,
        expires_at: parse_expiry(row.try_get("", "expires_at").map_err(LibraryError::Orm)?),
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
    async fn cursor_written_on_one_host_reads_back_on_another() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("library.db");
        let host_a = {
            let db = bookclerk_plugin_database_sqlite::open(&path)
                .await
                .expect("open a");
            crate::host_schema::apply_host_schema(&db)
                .await
                .expect("schema");
            LibraryStore::from_connection(db)
        };
        host_a
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
        drop(host_a);
        let host_b = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open(&path)
                .await
                .expect("open b"),
        );
        let entry = host_b
            .kv_get(PLUGIN, "sync.cursors", "audible:acct-1")
            .await
            .expect("read on host b")
            .expect("cursor follows the instance");
        assert_eq!(entry.value, b"page-42");
        assert_eq!(entry.metadata, r#"{"host":"a"}"#);
        assert!(
            host_b
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
