//! Host-served `[[kv_namespaces]]` bindings: Workers `KVNamespace`-shaped
//! durable storage on the library database, scoped per plugin and namespace.
//!
//! The guest never chooses the plugin key or namespace: both come from the
//! manifest row and the spawned identity, and the row is granted only when
//! the operator grant carries the `plugin_kv` binding token. The namespace's
//! `max_value_bytes` and `max_keys` are enforced by the library store, so a
//! native and a workerd guest see the same refusals. Values live in the
//! cluster: an instance moved to another host reads its own keys.

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_library::{KvPutOutcome, KvWriteSpec, LibraryStore};
use bookclerk_plugin_abi::{
    KvDeleteOk, KvGetOk, KvListKey, KvListOk, KvListOptions, KvNamespace, KvPutOk, KvPutOptions,
    PluginError, MAX_LIST_PAGE,
};
use bookclerk_plugin_manifest::{KvNamespaceManifest, PluginManifest};
use chrono::{DateTime, TimeZone, Utc};

use crate::consent::{grant_has_binding, PluginGrant};

/// Plugin ABI result (the wire error the guest receives).
type Result<T> = std::result::Result<T, PluginError>;

/// Grant token that authorizes every `[[kv_namespaces]]` row of a plugin.
pub const KV_GRANT_BINDING: &str = "plugin_kv";

/// Host-owned facts a session needs before it can hand a guest its KV
/// bindings: the library store plus the granted rows.
#[derive(Clone)]
pub struct KvNamespaces {
    /// Library store that holds `plugin_kv`.
    store: LibraryStore,
    /// Plugin key every row is scoped to.
    plugin_key: String,
    /// Granted rows in manifest order.
    rows: Vec<KvNamespaceManifest>,
}

impl KvNamespaces {
    /// Builds the KV hook for `plugin_key`, or `None` when the manifest
    /// declares no row or the grant lacks [`KV_GRANT_BINDING`].
    #[must_use]
    pub fn new(
        store: LibraryStore,
        plugin_key: &str,
        manifest: &PluginManifest,
        grant: &PluginGrant,
    ) -> Option<Self> {
        if manifest.kv_namespaces.is_empty() || !grant_has_binding(grant, KV_GRANT_BINDING) {
            return None;
        }
        Some(Self {
            store,
            plugin_key: plugin_key.to_string(),
            rows: manifest.kv_namespaces.clone(),
        })
    }

    /// Granted binding names in manifest order.
    #[must_use]
    pub fn binding_names(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| row.binding_name().to_string())
            .collect()
    }

    /// Namespaces shaped for `HostBindings::kv` as `(binding, namespace, kv)`.
    #[must_use]
    pub fn namespaces(&self) -> Vec<(String, String, Arc<dyn KvNamespace>)> {
        self.rows
            .iter()
            .map(|row| {
                let namespace = row.namespace_name();
                let kv: Arc<dyn KvNamespace> = Arc::new(LibraryKvNamespace {
                    store: self.store.clone(),
                    plugin_key: self.plugin_key.clone(),
                    namespace: namespace.clone(),
                    max_value_bytes: row.max_value_bytes,
                    max_keys: row.max_keys,
                });
                (row.binding_name().to_string(), namespace, kv)
            })
            .collect()
    }
}

/// [`KvNamespace`] server for one `(plugin_key, namespace)` on the library.
pub struct LibraryKvNamespace {
    /// Library store that holds `plugin_kv`.
    store: LibraryStore,
    /// Plugin key every row is scoped to.
    plugin_key: String,
    /// Namespace every row is scoped to.
    namespace: String,
    /// Largest value this namespace accepts.
    max_value_bytes: u32,
    /// Most live keys this namespace holds.
    max_keys: u32,
}

impl LibraryKvNamespace {
    /// Builds one namespace directly (tests and in-process hosts).
    #[must_use]
    pub fn new(store: LibraryStore, plugin_key: &str, row: &KvNamespaceManifest) -> Self {
        Self {
            store,
            plugin_key: plugin_key.to_string(),
            namespace: row.namespace_name(),
            max_value_bytes: row.max_value_bytes,
            max_keys: row.max_keys,
        }
    }

    /// Namespace this binding is scoped to.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

/// Resolves `expirationUnixMs` / `expirationTtlSeconds` onto an absolute
/// expiry; `None` never expires. An expiry already in the past is honoured
/// (the key is immediately absent), as Workers KV does.
fn expiry_for(options: &KvPutOptions, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    if options.expiration_unix_ms > 0 {
        let ms = i64::try_from(options.expiration_unix_ms)
            .map_err(|_| PluginError::invalid_params("expirationUnixMs is out of range"))?;
        return Utc
            .timestamp_millis_opt(ms)
            .single()
            .map(Some)
            .ok_or_else(|| PluginError::invalid_params("expirationUnixMs is out of range"));
    }
    if options.expiration_ttl_seconds > 0 {
        let secs = i64::try_from(options.expiration_ttl_seconds)
            .map_err(|_| PluginError::invalid_params("expirationTtlSeconds is out of range"))?;
        return now
            .checked_add_signed(chrono::Duration::seconds(secs))
            .map(Some)
            .ok_or_else(|| PluginError::invalid_params("expirationTtlSeconds is out of range"));
    }
    Ok(None)
}

/// UTC Unix milliseconds for the wire; zero when unset.
fn unix_ms(expires_at: Option<DateTime<Utc>>) -> u64 {
    expires_at
        .map(|t| u64::try_from(t.timestamp_millis()).unwrap_or(0))
        .unwrap_or(0)
}

/// Maps a library failure onto the retryable wire code.
fn unavailable(err: impl std::fmt::Display) -> PluginError {
    PluginError::unavailable(format!("kv store failed: {err}"))
}

#[async_trait(?Send)]
impl KvNamespace for LibraryKvNamespace {
    async fn get(&self, key: String) -> Result<KvGetOk> {
        let entry = self
            .store
            .kv_get(&self.plugin_key, &self.namespace, &key)
            .await
            .map_err(unavailable)?;
        Ok(match entry {
            Some(entry) => KvGetOk {
                found: true,
                value: entry.value,
                metadata: entry.metadata,
                expiration_unix_ms: unix_ms(entry.expires_at),
            },
            None => KvGetOk::default(),
        })
    }

    async fn put(&self, key: String, value: Vec<u8>, options: KvPutOptions) -> Result<KvPutOk> {
        if !options.metadata.is_empty() {
            serde_json::from_str::<serde::de::IgnoredAny>(&options.metadata).map_err(|err| {
                PluginError::invalid_params(format!("kv metadata must be JSON: {err}"))
            })?;
        }
        let expires_at = expiry_for(&options, Utc::now())?;
        let outcome = self
            .store
            .kv_put(
                &self.plugin_key,
                &self.namespace,
                &key,
                KvWriteSpec {
                    value,
                    metadata: options.metadata,
                    expires_at,
                    max_value_bytes: self.max_value_bytes,
                    max_keys: self.max_keys,
                },
            )
            .await
            .map_err(unavailable)?;
        match outcome {
            KvPutOutcome::Written => Ok(KvPutOk {
                expiration_unix_ms: unix_ms(expires_at),
            }),
            KvPutOutcome::ValueTooLarge { len, max } => Err(PluginError::payload_too_large(
                format!("kv value of {len} bytes exceeds the namespace cap of {max}"),
            )),
            KvPutOutcome::KeyLimitReached { max } => Err(PluginError::forbidden(format!(
                "kv namespace `{}` already holds its {max} keys; delete one first",
                self.namespace
            ))),
        }
    }

    async fn delete(&self, key: String) -> Result<KvDeleteOk> {
        let existed = self
            .store
            .kv_delete(&self.plugin_key, &self.namespace, &key)
            .await
            .map_err(unavailable)?;
        Ok(KvDeleteOk { existed })
    }

    async fn list(&self, options: KvListOptions) -> Result<KvListOk> {
        let limit = options.limit.min(MAX_LIST_PAGE);
        let page = self
            .store
            .kv_list(
                &self.plugin_key,
                &self.namespace,
                &options.prefix,
                &options.cursor,
                limit,
            )
            .await
            .map_err(|err| {
                if err.to_string().contains("cursor") {
                    PluginError::new(
                        bookclerk_plugin_abi::PluginErrorCode::InvalidCursor,
                        "malformed kv list cursor",
                    )
                } else {
                    unavailable(err)
                }
            })?;
        Ok(KvListOk {
            keys: page
                .keys
                .into_iter()
                .map(|entry| KvListKey {
                    key: entry.key,
                    metadata: entry.metadata,
                    expiration_unix_ms: unix_ms(entry.expires_at),
                })
                .collect(),
            list_complete: page.list_complete,
            cursor: page.cursor,
        })
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::PluginErrorCode;

    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        )
    }

    fn manifest() -> PluginManifest {
        PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[kv_namespaces]]

[[kv_namespaces]]
binding = "CURSORS"
namespace = "sync.cursors"
max_value_bytes = 16
max_keys = 2
"#,
        )
        .expect("manifest")
    }

    fn grant(bindings: &[&str]) -> PluginGrant {
        PluginGrant {
            bindings: bindings.iter().map(|s| (*s).to_string()).collect(),
            ..PluginGrant::empty()
        }
    }

    #[tokio::test]
    async fn namespaces_follow_manifest_and_grant() {
        let store = memory_store().await;
        assert!(KvNamespaces::new(store.clone(), "demo", &manifest(), &grant(&[])).is_none());
        let hook =
            KvNamespaces::new(store, "demo", &manifest(), &grant(&["plugin_kv"])).expect("granted");
        assert_eq!(hook.binding_names(), ["KV", "CURSORS"]);
        let namespaces = hook.namespaces();
        assert_eq!(namespaces[0].0, "KV");
        assert_eq!(namespaces[0].1, "kv");
        assert_eq!(namespaces[1].1, "sync.cursors");
    }

    #[tokio::test]
    async fn binding_applies_namespace_limits_and_expiry() {
        let store = memory_store().await;
        let row = manifest().kv_namespaces[1].clone();
        let kv = LibraryKvNamespace::new(store.clone(), "demo", &row);
        assert_eq!(kv.namespace(), "sync.cursors");
        let miss = kv.get("a".into()).await.expect("get");
        assert!(!miss.found);
        let put = kv
            .put(
                "a".into(),
                b"page-1".to_vec(),
                KvPutOptions {
                    expiration_ttl_seconds: 3_600,
                    metadata: r#"{"n":1}"#.into(),
                    ..KvPutOptions::default()
                },
            )
            .await
            .expect("put");
        assert!(put.expiration_unix_ms > 0);
        let hit = kv.get("a".into()).await.expect("get");
        assert!(hit.found);
        assert_eq!(hit.value, b"page-1");
        assert_eq!(hit.metadata, r#"{"n":1}"#);
        assert_eq!(hit.expiration_unix_ms, put.expiration_unix_ms);

        let too_big = kv
            .put("b".into(), vec![0u8; 17], KvPutOptions::default())
            .await
            .expect_err("namespace value cap");
        assert_eq!(too_big.code, PluginErrorCode::PayloadTooLarge);
        let bad_meta = kv
            .put(
                "b".into(),
                b"x".to_vec(),
                KvPutOptions {
                    metadata: "not json".into(),
                    ..KvPutOptions::default()
                },
            )
            .await
            .expect_err("metadata must be JSON");
        assert_eq!(bad_meta.code, PluginErrorCode::InvalidParams);
        kv.put("b".into(), b"x".to_vec(), KvPutOptions::default())
            .await
            .expect("second key");
        let full = kv
            .put("c".into(), b"x".to_vec(), KvPutOptions::default())
            .await
            .expect_err("max_keys");
        assert_eq!(full.code, PluginErrorCode::Forbidden);

        let page = kv
            .list(KvListOptions {
                prefix: String::new(),
                cursor: String::new(),
                limit: 1,
            })
            .await
            .expect("list");
        assert_eq!(page.keys.len(), 1);
        assert_eq!(page.keys[0].key, "a");
        assert!(!page.list_complete);
        let rest = kv
            .list(KvListOptions {
                prefix: String::new(),
                cursor: page.cursor,
                limit: 10,
            })
            .await
            .expect("list");
        assert_eq!(rest.keys[0].key, "b");
        assert!(rest.list_complete);
        let bad_cursor = kv
            .list(KvListOptions {
                prefix: String::new(),
                cursor: "!!".into(),
                limit: 10,
            })
            .await
            .expect_err("cursor");
        assert_eq!(bad_cursor.code, PluginErrorCode::InvalidCursor);

        assert!(kv.delete("a".into()).await.expect("delete").existed);
        assert!(!kv.delete("a".into()).await.expect("delete").existed);
        // The default namespace of the first row is a separate store.
        let other = LibraryKvNamespace::new(store, "demo", &manifest().kv_namespaces[0]);
        assert!(!other.get("b".into()).await.expect("get").found);
    }
}
