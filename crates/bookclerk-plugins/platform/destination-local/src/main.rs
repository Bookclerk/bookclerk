//! External local filesystem output plugin for Bookclerk.

use async_trait::async_trait;
use bookclerk_plugin_destination_local::ID;
use bookclerk_plugin_sdk::{
    BookclerkPlugin, BookclerkPluginGuest, DiagnoseResult, ExistsResultDto, GetResultDto,
    HandshakeParams, HandshakeResult, HealthResult, ObjectInfoDto, ObjectProbeDto,
    OutputCopyParams, OutputGetParams, OutputKeyParams, OutputListParams, OutputPutFileParams,
    OutputPutParams, OutputTouchFileParams, PluginError, PLUGIN_API_VERSION,
};

struct LocalPlugin;

#[async_trait]
impl BookclerkPlugin for LocalPlugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
            id: ID.into(),
            kind: "output".into(),
            display_name: Some("Local filesystem".into()),
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "put".into(),
                "putFile".into(),
                "get".into(),
                "exists".into(),
                "list".into(),
                "probe".into(),
                "copy".into(),
                "delete".into(),
                "touchFile".into(),
            ],
            sort_key: Some(5),
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some(ID.into()),
            enabled: Some(true),
            detail: Some("local output plugin ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["local output plugin diagnose: ok".into()],
        })
    }

    async fn put(&self, params: OutputPutParams) -> Result<(), PluginError> {
        let OutputPutParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local put params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_put(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn put_file(&self, params: OutputPutFileParams) -> Result<(), PluginError> {
        let OutputPutFileParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local putFile params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_put_file(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn get(&self, params: OutputGetParams) -> Result<GetResultDto, PluginError> {
        let OutputGetParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local get params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_get(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn exists(&self, params: OutputKeyParams) -> Result<ExistsResultDto, PluginError> {
        let OutputKeyParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local key params",
            ));
        };
        let exists = bookclerk_plugin_destination_local::guest::guest_exists(p)
            .await
            .map_err(PluginError::internal)?;
        Ok(ExistsResultDto { exists })
    }

    async fn list(&self, params: OutputListParams) -> Result<Vec<ObjectInfoDto>, PluginError> {
        let OutputListParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local list params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_list(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn probe(&self, params: OutputKeyParams) -> Result<ObjectProbeDto, PluginError> {
        let OutputKeyParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local key params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_probe(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn copy(&self, params: OutputCopyParams) -> Result<(), PluginError> {
        let OutputCopyParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local copy params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_copy(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn delete(&self, params: OutputKeyParams) -> Result<(), PluginError> {
        let OutputKeyParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local key params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_delete(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn touch_file(&self, params: OutputTouchFileParams) -> Result<(), PluginError> {
        let OutputTouchFileParams::Local(p) = params else {
            return Err(PluginError::invalid_params(
                "local destination expected Local touchFile params",
            ));
        };
        bookclerk_plugin_destination_local::guest::guest_touch_file(p)
            .await
            .map_err(PluginError::internal)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(LocalPlugin).await?;
    Ok(())
}
