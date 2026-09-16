//! Database factory context payloads (host connect params + public adapter config).
//!
//! Guests such as `sqlite` / `d1` / `postgres` implement the SeaORM proxy
//! boundary. The host never links SQL engines; it opens the library through
//! `DatabaseContext` + typed adapter sessions after Cap'n Proto spawn.
//!
//! Two payload kinds travel in [`crate::DatabaseContext::config`], selected by
//! media type:
//!
//! - `DbConnectParams` (feature `host`) — host-private serde type for
//!   first-party connect-param building; not public JSON RPC. Wire fields use
//!   camelCase; its `backend` tag is lowercase (`sqlite`, `d1`, `postgres`).
//! - [`crate::DatabaseAdapterConfig`] (public) — generic bootstrap for
//!   third-party adapters: the operator's granted `[database.<id>]` table plus
//!   the scoped data dir. Decode with [`database_adapter_config_from_context`].
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
    },
}

/// Media type for [`crate::DatabaseContext::config`] connect payloads.
#[cfg(feature = "host")]
pub const DATABASE_CONTEXT_MEDIA_TYPE: &str = "application/vnd.bookclerk.db-connect+json";

/// Media type for the public [`crate::DatabaseAdapterConfig`] payload carried
/// in [`crate::DatabaseContext::config`] for third-party adapters.
pub const DATABASE_ADAPTER_CONFIG_MEDIA_TYPE: &str =
    "application/vnd.bookclerk.db-adapter-config+json";

/// Builds a [`crate::DatabaseContext`] carrying the public author-facing
/// [`crate::DatabaseAdapterConfig`] (granted settings + data dir).
///
/// # Errors
///
/// Returns when JSON serialization fails.
pub fn database_context_from_adapter_config(
    config: &crate::DatabaseAdapterConfig,
) -> crate::Result<crate::DatabaseContext> {
    let payload = serde_json::to_vec(config).map_err(|err| {
        crate::PluginError::internal(format!("database adapter config encode failed: {err}"))
    })?;
    Ok(crate::DatabaseContext {
        json: String::new(),
        config: crate::ExtensibleConfig {
            schema_version: 0,
            media_type: DATABASE_ADAPTER_CONFIG_MEDIA_TYPE.into(),
            payload,
        },
    })
}

/// Decodes the public [`crate::DatabaseAdapterConfig`] from a database
/// factory context (third-party adapter bootstrap).
///
/// # Errors
///
/// Returns when the context does not carry an adapter-config payload or the
/// JSON is invalid.
pub fn database_adapter_config_from_context(
    ctx: &crate::DatabaseContext,
) -> crate::Result<crate::DatabaseAdapterConfig> {
    if ctx.config.media_type != DATABASE_ADAPTER_CONFIG_MEDIA_TYPE {
        return Err(crate::PluginError::invalid_params(format!(
            "database context media type `{}` is not `{DATABASE_ADAPTER_CONFIG_MEDIA_TYPE}`",
            ctx.config.media_type
        )));
    }
    serde_json::from_slice(&ctx.config.payload).map_err(|err| {
        crate::PluginError::invalid_params(format!("database adapter config decode failed: {err}"))
    })
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
        json: String::new(),
        config: crate::ExtensibleConfig {
            schema_version: 0,
            media_type: DATABASE_CONTEXT_MEDIA_TYPE.into(),
            payload,
        },
    })
}

/// Decodes host-internal connect params from a database factory context.
///
/// # Errors
///
/// Returns when the context omits connect params or JSON is invalid.
#[cfg(feature = "host")]
pub fn connect_params_from_context(ctx: &crate::DatabaseContext) -> crate::Result<DbConnectParams> {
    if !ctx.config.payload.is_empty() {
        if ctx.config.media_type != DATABASE_CONTEXT_MEDIA_TYPE {
            return Err(crate::PluginError::invalid_params(format!(
                "database context media type `{}` is not `{DATABASE_CONTEXT_MEDIA_TYPE}`",
                ctx.config.media_type
            )));
        }
        return serde_json::from_slice(&ctx.config.payload).map_err(|err| {
            crate::PluginError::invalid_params(format!("database context decode failed: {err}"))
        });
    }
    if ctx.json.trim().is_empty() {
        return Err(crate::PluginError::invalid_params(
            "database context is missing connect params",
        ));
    }
    serde_json::from_str(&ctx.json).map_err(|err| {
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
            config: serde_json::json!({ "url": "custom://x" }),
            binding: None,
            instance_id: None,
        };
        let ctx = database_context_from_adapter_config(&cfg).unwrap();
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
        let cfg = crate::DatabaseAdapterConfig {
            plugin_data_dir: "/tmp/plugins/custom/data".into(),
            config: serde_json::json!({ "url": "custom://host/db", "poolSize": 4 }),
            binding: None,
            instance_id: None,
        };
        let ctx = database_context_from_adapter_config(&cfg).unwrap();
        assert_eq!(ctx.config.media_type, DATABASE_ADAPTER_CONFIG_MEDIA_TYPE);
        assert_eq!(ctx.config.schema_version, 0);
        let back = database_adapter_config_from_context(&ctx).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.config["url"], "custom://host/db");
    }
}
