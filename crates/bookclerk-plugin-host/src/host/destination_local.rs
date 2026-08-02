//! [`StorageBackend`] adapter over the local filesystem output plugin process.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use base64::Engine;
use bookclerk_config::{normalize_storage_prefix, Config};
use bookclerk_storage::{ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend, StorageError};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::protocol::{
    methods, GetResultDto, LocalCopyParams, LocalGetParams, LocalKeyParams, LocalListParams,
    LocalPutFileParams, LocalPutParams, LocalTouchFileParams, ObjectInfoDto, ObjectMetaDto,
    ObjectProbeDto, OutputLocalContextDto,
};
use crate::rpc::PluginClient;
use crate::Result as PluginResult;

const LOCAL_PLUGIN_ID: &str = "local";

fn resolved_local_output_root(config: &Config) -> PathBuf {
    let root = &config.output.local.root;
    if root.is_absolute() {
        root.clone()
    } else {
        config.paths().files_dir.join(root)
    }
}

/// External local filesystem destination backed by a discovered output plugin.
#[derive(Clone)]
pub struct ExternalLocalDestination {
    client: Arc<PluginClient>,
    plugin_data_dir: PathBuf,
    root: PathBuf,
    prefix: String,
}

impl ExternalLocalDestination {
    /// Spawn and handshake a local output plugin.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> PluginResult<Self> {
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let client = Arc::new(PluginClient::spawn(plugin, config, config_json).await?);
        let plugin_data_dir = plugin_data_dir(config, &plugin.manifest.id);
        let root = resolved_local_output_root(config);
        let prefix = normalize_storage_prefix(config.output.local.prefix.trim());
        Ok(Self {
            client,
            plugin_data_dir,
            root,
            prefix,
        })
    }

    fn ctx(&self) -> OutputLocalContextDto {
        OutputLocalContextDto {
            plugin_data_dir: self.plugin_data_dir.display().to_string(),
            root: self.root.display().to_string(),
            prefix: self.prefix.clone(),
        }
    }

    fn map_err(err: crate::PluginError) -> StorageError {
        match err {
            crate::PluginError::Io(io) => StorageError::Io(io),
            other => StorageError::Other(anyhow::anyhow!(other)),
        }
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
impl StorageBackend for ExternalLocalDestination {
    fn name(&self) -> &'static str {
        "local"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> bookclerk_storage::Result<()> {
        let params = LocalPutParams {
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
        let params = LocalPutFileParams {
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
        if self.client.has_side_channel() || self.client.has_acl_grants() {
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
        let params = LocalGetParams {
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
            .map_err(|err| StorageError::InvalidKey(format!("invalid get base64: {err}")))
    }

    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        let params = LocalKeyParams {
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
        let params = LocalListParams {
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
        let params = LocalKeyParams {
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
        let params = LocalCopyParams {
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
        let params = LocalKeyParams {
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
        let params = LocalTouchFileParams {
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

pub(crate) async fn try_load_local(
    plugin: &DiscoveredPlugin,
    config: &Config,
    registry: &mut super::destination::DestinationRegistry,
) {
    if plugin.manifest.id != LOCAL_PLUGIN_ID {
        return;
    }
    if !config.output.local.enabled {
        tracing::debug!(id = %plugin.manifest.id, "local output disabled in config; skipping external plugin");
        return;
    }
    match ExternalLocalDestination::spawn(plugin, config).await {
        Ok(dest) => {
            tracing::info!(
                id = %plugin.manifest.id,
                path = %plugin.command.display(),
                "loaded external local output plugin"
            );
            registry.set_local(Arc::new(dest));
        }
        Err(err) => {
            tracing::warn!(
                id = %plugin.manifest.id,
                %err,
                "failed to start external local output plugin; falling back to in-process backend"
            );
        }
    }
}

fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn map_json_err(err: serde_json::Error) -> StorageError {
    StorageError::Other(anyhow::anyhow!("serialize output RPC params: {err}"))
}

fn parse_exists_response(value: &Value) -> bookclerk_storage::Result<bool> {
    value
        .get("exists")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| {
            StorageError::Other(anyhow::anyhow!(
                "plugin exists response missing boolean \"exists\" field: {value}"
            ))
        })
}
