//! Database factory context payloads (host connect params + public adapter config).
//!
//! Guests such as `sqlite` / `d1` / `postgres` implement the SeaORM proxy
//! boundary. The host never links SQL engines; it opens the library through
//! `DatabaseContext` + typed adapter sessions after Cap'n Proto spawn.
//!
//! A [`crate::DatabaseContext`] carries one of two bootstraps:
//!
//! - `DbConnectParams` (feature `host`) — host-private serde type for
//!   first-party connect-param building; not public JSON RPC. Wire fields use
//!   camelCase; its `backend` tag is lowercase (`sqlite`, `d1`, `postgres`).
//! - [`crate::DatabaseAdapterConfig`] (public) — generic bootstrap for
//!   third-party adapters: the operator's granted `[database.<id>]` table plus
//!   the scoped data dir. Travels typed in [`crate::DatabaseContext::adapter`];
//!   read it with [`database_adapter_config_from_context`].
//!
//! Semantic capability limits live in [`crate::DbCapabilities`]
//! (`crate::db_execute`); bootstrap metadata lives in [`crate::DbBootstrap`].

#[cfg(feature = "host")]
use serde::{Deserialize, Serialize};

/// Host-private tagged connect params for first-party database guests.
///
/// Travels only inside [`crate::DatabaseContext::config`] built by the
/// host ([`database_context_from_params`]); not part of the public plugin
/// author ABI. Discriminant is wire field `backend` with lowercase tags.
/// SQLite guests open `library.db` at [`Self::Sqlite::sqlite_path`] (also
/// injected as `BOOKCLERK_SQLITE_PATH`); D1 / Postgres receive host-injected
/// credentials in the params. Third-party adapters receive the public
/// [`crate::DatabaseAdapterConfig`] payload instead.
#[cfg(feature = "host")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum DbConnectParams {
    /// Local SQLite file backend (`backend: "sqlite"`).
    #[serde(rename_all = "camelCase")]
    Sqlite {
        /// Scoped writable directory for this plugin
        /// (`…/plugins/<id>/data`, wire `pluginDataDir`).
        plugin_data_dir: String,
        /// Absolute path to the DB file (wire `sqlitePath`). The sqlite jail
        /// grants this file and its journal sidecars at spawn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite_path: Option<String>,
        /// Named plugin database binding this open serves, if any. Binding
        /// opens use a dedicated connection at `sqlite_path` (the spawn env
        /// override never applies) so each binding is its own database file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        /// When false, open an existing file and do not create a missing one.
        #[serde(default = "default_true", skip_serializing_if = "skip_if_true")]
        provision: bool,
    },
    /// Cloudflare D1 HTTP API backend (`backend: "d1"`).
    #[serde(rename_all = "camelCase")]
    D1 {
        /// Scoped writable directory for this plugin (wire `pluginDataDir`).
        plugin_data_dir: String,
        /// Cloudflare account id for the D1 API (wire `accountId`).
        account_id: String,
        /// D1 database UUID (wire `databaseId`). Empty for binding opens: the
        /// adapter resolves (and creates) the database by `databaseName`.
        database_id: String,
        /// API base URL (for example `https://api.cloudflare.com/client/v4`).
        api_base: String,
        /// Bearer / API token the host injects; guests must not read env for this.
        api_token: String,
        /// Named plugin database binding this open serves, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        /// D1 database name to resolve/provision for a binding open.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database_name: Option<String>,
        /// When false, look up an existing D1 database and do not create it.
        #[serde(default = "default_true", skip_serializing_if = "skip_if_true")]
        provision: bool,
    },
    /// PostgreSQL connection-string backend (`backend: "postgres"`).
    #[serde(rename_all = "camelCase")]
    Postgres {
        /// Scoped writable directory for this plugin (wire `pluginDataDir`).
        plugin_data_dir: String,
        /// Full Postgres connection URL (host-injected; may contain secrets).
        url: String,
        /// Named plugin database binding this open serves, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        /// Isolated PostgreSQL database for a binding open (created if missing).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
        /// When false, connect to an existing database and do not `CREATE DATABASE`.
        #[serde(default = "default_true", skip_serializing_if = "skip_if_true")]
        provision: bool,
    },
}

/// Serde default for omitted `provision` (create the binding unit).
#[cfg(feature = "host")]
fn default_true() -> bool {
    true
}

/// Omit `provision` from JSON when it is the default (`true`).
#[cfg(feature = "host")]
fn skip_if_true(value: &bool) -> bool {
    *value
}

/// Media type for [`crate::DatabaseContext::config`] connect payloads.
#[cfg(feature = "host")]
pub const DATABASE_CONTEXT_MEDIA_TYPE: &str = "application/vnd.bookclerk.db-connect+json";

/// Builds a [`crate::DatabaseContext`] carrying the public author-facing
/// [`crate::DatabaseAdapterConfig`] (granted settings + data dir) in the typed
/// `adapter` field; `config` stays empty so host-private connect params never
/// reach third-party adapters.
#[must_use]
pub fn database_context_from_adapter_config(
    config: &crate::DatabaseAdapterConfig,
) -> crate::DatabaseContext {
    crate::DatabaseContext {
        config: crate::ExtensibleConfig::default(),
        adapter: config.clone(),
    }
}

/// Decodes the public [`crate::DatabaseAdapterConfig`] from a database
/// factory context (third-party adapter bootstrap).
///
/// # Errors
///
/// Returns when the context carries host-private connect params instead of
/// an adapter bootstrap (empty `adapter.pluginDataDir`).
pub fn database_adapter_config_from_context(
    ctx: &crate::DatabaseContext,
) -> crate::Result<crate::DatabaseAdapterConfig> {
    if ctx.adapter.plugin_data_dir.is_empty() {
        return Err(crate::PluginError::invalid_params(
            "database context does not carry an adapter bootstrap (pluginDataDir is empty)",
        ));
    }
    Ok(ctx.adapter.clone())
}

/// Builds a [`crate::DatabaseContext`] from host-internal connect params.
///
/// # Errors
///
/// Returns when JSON serialization fails.
#[cfg(feature = "host")]
pub fn database_context_from_params(
    params: &DbConnectParams,
) -> crate::Result<crate::DatabaseContext> {
    let payload = serde_json::to_vec(params).map_err(|err| {
        crate::PluginError::internal(format!("database context encode failed: {err}"))
    })?;
    Ok(crate::DatabaseContext {
        config: crate::ExtensibleConfig {
            schema_version: 0,
            media_type: DATABASE_CONTEXT_MEDIA_TYPE.into(),
            payload,
        },
        adapter: crate::DatabaseAdapterConfig::default(),
    })
}

/// Decodes host-internal connect params from a database factory context.
///
/// # Errors
///
/// Returns when the context omits connect params or JSON is invalid.
#[cfg(feature = "host")]
pub fn connect_params_from_context(ctx: &crate::DatabaseContext) -> crate::Result<DbConnectParams> {
    if ctx.config.payload.is_empty() {
        return Err(crate::PluginError::invalid_params(
            "database context is missing connect params",
        ));
    }
    if ctx.config.media_type != DATABASE_CONTEXT_MEDIA_TYPE {
        return Err(crate::PluginError::invalid_params(format!(
            "database context media type `{}` is not `{DATABASE_CONTEXT_MEDIA_TYPE}`",
            ctx.config.media_type
        )));
    }
    serde_json::from_slice(&ctx.config.payload).map_err(|err| {
        crate::PluginError::invalid_params(format!("database context decode failed: {err}"))
    })
}

#[cfg(all(test, feature = "host"))]
#[allow(clippy::missing_panics_doc)]
mod host_tests {
    use super::*;

    #[test]
    fn connect_params_are_tagged_by_backend_with_camel_case_fields() {
        let sqlite = DbConnectParams::Sqlite {
            plugin_data_dir: "/tmp/p".into(),
            sqlite_path: Some("/tmp/library.db".into()),
            binding: None,
            provision: true,
        };
        let v = serde_json::to_value(&sqlite).unwrap();
        assert_eq!(v["backend"], "sqlite");
        assert!(v.get("pluginDataDir").is_some());
        assert!(v.get("sqlitePath").is_some());
        assert!(v.get("plugin_data_dir").is_none());
        let back: DbConnectParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, sqlite);
    }

    #[test]
    fn connect_params_roundtrip_through_database_context() {
        let params = DbConnectParams::Postgres {
            plugin_data_dir: "/tmp/p".into(),
            url: "postgres://localhost/db".into(),
            binding: None,
            database: None,
            provision: true,
        };
        let ctx = database_context_from_params(&params).unwrap();
        assert_eq!(ctx.config.media_type, DATABASE_CONTEXT_MEDIA_TYPE);
        assert_eq!(ctx.config.schema_version, 0);
        assert_eq!(connect_params_from_context(&ctx).unwrap(), params);
    }

    #[test]
    fn postgres_binding_uses_database_wire_name() {
        let v = serde_json::json!({
            "backend": "postgres",
            "pluginDataDir": "/tmp/p",
            "url": "postgres://localhost/library",
            "binding": "DB",
            "database": "pb_echo_db"
        });
        let params: DbConnectParams = serde_json::from_value(v).unwrap();
        match params {
            DbConnectParams::Postgres {
                database, binding, ..
            } => {
                assert_eq!(binding.as_deref(), Some("DB"));
                assert_eq!(database.as_deref(), Some("pb_echo_db"));
            }
            other => panic!("expected postgres params, got {other:?}"),
        }
        let rejected = serde_json::json!({
            "backend": "postgres",
            "pluginDataDir": "/tmp/p",
            "url": "postgres://localhost/library",
            "binding": "DB",
            "schema": "pb_echo_db"
        });
        let params: DbConnectParams = serde_json::from_value(rejected).unwrap();
        match params {
            DbConnectParams::Postgres { database, .. } => {
                assert_eq!(
                    database, None,
                    "unreleased schema wire name must not decode"
                );
            }
            other => panic!("expected postgres params, got {other:?}"),
        }
    }

    #[test]
    fn adapter_config_context_is_not_decodable_as_connect_params() {
        let cfg = crate::DatabaseAdapterConfig {
            plugin_data_dir: "/tmp/p".into(),
            settings: crate::ExtensibleConfig::json_from(
                &serde_json::json!({ "url": "custom://x" }),
            )
            .unwrap(),
            binding: None,
            instance_id: None,
            open_existing: false,
        };
        let ctx = database_context_from_adapter_config(&cfg);
        connect_params_from_context(&ctx)
            .expect_err("public adapter config must not parse as host connect params");
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn adapter_config_roundtrips_through_database_context() {
        let settings = serde_json::json!({ "url": "custom://host/db", "poolSize": 4 });
        let cfg = crate::DatabaseAdapterConfig {
            plugin_data_dir: "/tmp/plugins/custom/data".into(),
            settings: crate::ExtensibleConfig::json_from(&settings).unwrap(),
            binding: None,
            instance_id: None,
            open_existing: false,
        };
        let ctx = database_context_from_adapter_config(&cfg);
        assert!(ctx.config.is_empty(), "no host-private connect params");
        let back = database_adapter_config_from_context(&ctx).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(
            back.settings.json_value().unwrap()["url"],
            "custom://host/db"
        );
        let bare = crate::DatabaseContext::default();
        database_adapter_config_from_context(&bare)
            .expect_err("empty adapter bootstrap must not decode");
    }
}
