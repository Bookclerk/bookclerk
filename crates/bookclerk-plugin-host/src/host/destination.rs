//! [`StorageBackend`] adapter over an external output plugin process.
//!
//! The host never grants the guest filesystem access to acquire scratch or the
//! output library. Large uploads arrive as an open file descriptor on the side
//! channel; credentials and bucket config are injected per RPC.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use base64::Engine;
use bookclerk_config::{normalize_storage_prefix, Config, OutputS3Config};
use bookclerk_storage::{
    load_s3_credentials, ObjectInfo, ObjectMeta, ObjectProbe, S3Credentials, StorageBackend,
    StorageError,
};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::protocol::{
    methods, CopyParams, GetParams, GetResultDto, KeyParams, ListParams, ObjectInfoDto,
    ObjectMetaDto, ObjectProbeDto, OutputS3ContextDto, PutFileParams, PutParams, S3CredentialsDto,
    TouchFileParams,
};
use crate::rpc::PluginClient;
use crate::Result as PluginResult;

const S3_PLUGIN_ID: &str = "s3";

/// External S3 destination backed by a discovered output plugin.
#[derive(Clone)]
pub struct ExternalDestination {
    client: Arc<PluginClient>,
    plugin_data_dir: PathBuf,
    s3_config: OutputS3Config,
    prefix: String,
    credentials: Option<S3Credentials>,
}

impl ExternalDestination {
    /// Spawn and handshake an S3 output plugin.
    pub async fn spawn(
        plugin: &DiscoveredPlugin,
        config: &Config,
        db: Option<&DatabaseConnection>,
    ) -> PluginResult<Self> {
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let client = Arc::new(PluginClient::spawn(plugin, config, config_json).await?);
        let plugin_data_dir = plugin_data_dir(config, &plugin.manifest.id);
        let s3_config = config.output.s3.clone();
        let prefix = normalize_storage_prefix(s3_config.prefix.trim());
        let credentials = resolve_host_credentials(db)
            .await
            .map_err(|err| crate::PluginError::message(err.to_string()))?;
        Ok(Self {
            client,
            plugin_data_dir,
            s3_config,
            prefix,
            credentials,
        })
    }

    fn ctx(&self) -> OutputS3ContextDto {
        OutputS3ContextDto {
            plugin_data_dir: self.plugin_data_dir.display().to_string(),
            bucket: self.s3_config.bucket.clone(),
            prefix: self.prefix.clone(),
            region: self.s3_config.region.clone(),
            endpoint: self.s3_config.endpoint.clone(),
            force_path_style: self.s3_config.force_path_style,
            credentials: self.credentials.as_ref().map(credentials_to_dto),
        }
    }

    fn map_err(err: crate::PluginError) -> StorageError {
        StorageError::S3(err.to_string())
    }

    fn meta_to_dto(meta: &ObjectMeta) -> ObjectMetaDto {
        ObjectMetaDto {
            content_type: meta.content_type.clone(),
            content_length: meta.content_length,
            asin: meta.asin.clone(),
            title: meta.title.clone(),
            creation_time: meta.creation_time.clone(),
            last_write_time: meta.last_write_time.clone(),
        }
    }

    fn meta_from_dto(dto: ObjectMetaDto) -> ObjectMeta {
        ObjectMeta {
            content_type: dto.content_type,
            content_length: dto.content_length,
            asin: dto.asin,
            title: dto.title,
            creation_time: dto.creation_time,
            last_write_time: dto.last_write_time,
        }
    }

    fn rfc3339(time: SystemTime) -> Option<String> {
        Some(DateTime::<Utc>::from(time).to_rfc3339())
    }
}

#[async_trait]
impl StorageBackend for ExternalDestination {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        let params = PutParams {
            ctx: self.ctx(),
            key: key.to_string(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            meta: Self::meta_to_dto(&meta),
        };
        self.client
            .call::<Value>(
                methods::PUT,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn put_file(
        &self,
        key: &str,
        path: &Path,
        meta: ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        let params = PutFileParams {
            ctx: self.ctx(),
            key: key.to_string(),
            meta: Self::meta_to_dto(&meta),
            local_path: if self.client.has_side_channel() {
                None
            } else {
                Some(path.display().to_string())
            },
        };
        let params_json = serde_json::to_value(params).map_err(map_json_err)?;
        if self.client.has_side_channel() {
            self.client
                .call_raw_with_upload_file(methods::PUT_FILE, params_json, path)
                .await
                .map_err(Self::map_err)?;
        } else {
            self.client
                .call_raw(methods::PUT_FILE, params_json)
                .await
                .map_err(Self::map_err)?;
        }
        Ok(())
    }

    async fn get(&self, key: &str) -> bookclerk_storage::Result<Bytes> {
        let params = GetParams {
            ctx: self.ctx(),
            key: key.to_string(),
        };
        let result: GetResultDto = self
            .client
            .call(
                methods::GET,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        base64::engine::general_purpose::STANDARD
            .decode(result.data_base64)
            .map(Bytes::from)
            .map_err(|err| StorageError::S3(format!("invalid get base64: {err}")))
    }

    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        let params = KeyParams {
            ctx: self.ctx(),
            key: key.to_string(),
        };
        let value: Value = self
            .client
            .call(
                methods::EXISTS,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        parse_exists_response(&value)
    }

    async fn list(&self, prefix: &str) -> bookclerk_storage::Result<Vec<ObjectInfo>> {
        let params = ListParams {
            ctx: self.ctx(),
            prefix: prefix.to_string(),
        };
        let items: Vec<ObjectInfoDto> = self
            .client
            .call(
                methods::LIST,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        Ok(items
            .into_iter()
            .map(|item| ObjectInfo {
                key: item.key,
                size: item.size,
            })
            .collect())
    }

    async fn probe(&self, key: &str) -> bookclerk_storage::Result<ObjectProbe> {
        let params = KeyParams {
            ctx: self.ctx(),
            key: key.to_string(),
        };
        let dto: ObjectProbeDto = self
            .client
            .call(
                methods::PROBE,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        Ok(ObjectProbe {
            key: dto.key,
            size: dto.size,
            content_type: dto.content_type,
            meta: Self::meta_from_dto(dto.meta),
        })
    }

    async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        let params = CopyParams {
            ctx: self.ctx(),
            from: from.to_string(),
            to: to.to_string(),
        };
        self.client
            .call::<Value>(
                methods::COPY,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
        let params = KeyParams {
            ctx: self.ctx(),
            key: key.to_string(),
        };
        self.client
            .call::<Value>(
                methods::DELETE,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn touch_file(
        &self,
        key: &str,
        created: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> bookclerk_storage::Result<()> {
        let params = TouchFileParams {
            ctx: self.ctx(),
            key: key.to_string(),
            created: created.and_then(Self::rfc3339),
            modified: modified.and_then(Self::rfc3339),
        };
        self.client
            .call::<Value>(
                methods::TOUCH_FILE,
                serde_json::to_value(params).map_err(map_json_err)?,
            )
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }
}

/// Long-lived external output plugins loaded at host startup.
#[derive(Default, Clone)]
pub struct DestinationRegistry {
    s3: Option<Arc<ExternalDestination>>,
    local: Option<Arc<super::destination_local::ExternalLocalDestination>>,
}

impl DestinationRegistry {
    #[must_use]
    pub fn s3(&self) -> Option<Arc<ExternalDestination>> {
        self.s3.clone()
    }

    #[must_use]
    pub fn local(&self) -> Option<Arc<super::destination_local::ExternalLocalDestination>> {
        self.local.clone()
    }

    pub(crate) fn set_local(
        &mut self,
        dest: Arc<super::destination_local::ExternalLocalDestination>,
    ) {
        self.local = Some(dest);
    }
}

/// Discover and spawn external output plugins.
pub async fn load_external_destinations(
    config: &Config,
    db: Option<&DatabaseConnection>,
) -> PluginResult<DestinationRegistry> {
    let mut registry = DestinationRegistry::default();
    for plugin in crate::discover_plugins(config)? {
        if plugin.manifest.kind != crate::PluginKind::Output {
            continue;
        }
        if plugin.manifest.id == S3_PLUGIN_ID {
            if !config.output.s3.enabled {
                tracing::debug!(id = %plugin.manifest.id, "S3 output disabled in config; skipping external plugin");
                continue;
            }
            match ExternalDestination::spawn(&plugin, config, db).await {
                Ok(dest) => {
                    tracing::info!(
                        id = %plugin.manifest.id,
                        path = %plugin.command.display(),
                        "loaded external S3 output plugin"
                    );
                    registry.s3 = Some(Arc::new(dest));
                }
                Err(err) => {
                    tracing::warn!(
                        id = %plugin.manifest.id,
                        %err,
                        "failed to start external S3 output plugin; falling back to in-process backend"
                    );
                }
            }
            continue;
        }
        super::destination_local::try_load_local(&plugin, config, &mut registry).await;
    }
    Ok(registry)
}

async fn resolve_host_credentials(
    db: Option<&DatabaseConnection>,
) -> std::result::Result<Option<S3Credentials>, StorageError> {
    if let (Ok(access), Ok(secret)) = (
        std::env::var(bookclerk_storage::ENV_AWS_ACCESS_KEY_ID),
        std::env::var(bookclerk_storage::ENV_AWS_SECRET_ACCESS_KEY),
    ) {
        let session = std::env::var(bookclerk_storage::ENV_AWS_SESSION_TOKEN).ok();
        return Ok(Some(S3Credentials {
            access_key_id: access,
            secret_access_key: secret,
            session_token: session,
            label: None,
        }));
    }
    if let Some(db) = db {
        return load_s3_credentials(db).await;
    }
    Ok(None)
}

fn credentials_to_dto(creds: &S3Credentials) -> S3CredentialsDto {
    S3CredentialsDto {
        access_key_id: creds.access_key_id.clone(),
        secret_access_key: creds.secret_access_key.clone(),
        session_token: creds.session_token.clone(),
    }
}

fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn map_json_err(err: serde_json::Error) -> StorageError {
    StorageError::S3(format!("serialize output RPC params: {err}"))
}

fn parse_exists_response(value: &Value) -> bookclerk_storage::Result<bool> {
    value
        .get("exists")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| {
            StorageError::S3(format!(
                "plugin exists response missing boolean \"exists\" field: {value}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::parse_exists_response;
    use serde_json::json;

    #[test]
    fn exists_response_requires_boolean_field() {
        assert!(parse_exists_response(&json!({ "exists": true })).unwrap());
        assert!(!parse_exists_response(&json!({ "exists": false })).unwrap());
        assert!(parse_exists_response(&json!({})).is_err());
        assert!(parse_exists_response(&json!({ "exists": "yes" })).is_err());
    }
}
