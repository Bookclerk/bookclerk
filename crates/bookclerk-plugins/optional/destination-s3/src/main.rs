//! External S3 / MinIO output plugin for Bookclerk.

use async_trait::async_trait;
use bookclerk_plugin_destination_s3::ID;
use bookclerk_plugin_sdk::{
    BookclerkPlugin, BookclerkPluginGuest, DiagnoseResult, ExistsResultDto, GetResultDto,
    HandshakeParams, HandshakeResult, HealthResult, ObjectInfoDto, ObjectProbeDto,
    OutputCopyParams, OutputGetParams, OutputKeyParams, OutputListParams, OutputPutFileParams,
    OutputPutParams, OutputTouchFileParams, PluginError, PLUGIN_API_VERSION,
};

/// External S3 / MinIO destination guest (`kind = output`).
struct S3Plugin;

#[async_trait]
impl BookclerkPlugin for S3Plugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
            id: ID.into(),
            kind: "output".into(),
            display_name: Some("S3 / MinIO".into()),
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
            sort_key: Some(10),
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some(ID.into()),
            enabled: Some(true),
            detail: Some("S3 output plugin ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["s3 output plugin diagnose: ok".into()],
        })
    }

    async fn put(&self, params: OutputPutParams) -> Result<(), PluginError> {
        let OutputPutParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 put params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_put(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn put_file(&self, params: OutputPutFileParams) -> Result<(), PluginError> {
        let OutputPutFileParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 putFile params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_put_file(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn get(&self, params: OutputGetParams) -> Result<GetResultDto, PluginError> {
        let OutputGetParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 get params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_get(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn exists(&self, params: OutputKeyParams) -> Result<ExistsResultDto, PluginError> {
        let OutputKeyParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 key params",
            ));
        };
        let exists = bookclerk_plugin_destination_s3::guest::guest_exists(p)
            .await
            .map_err(PluginError::internal)?;
        Ok(ExistsResultDto { exists })
    }

    async fn list(&self, params: OutputListParams) -> Result<Vec<ObjectInfoDto>, PluginError> {
        let OutputListParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 list params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_list(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn probe(&self, params: OutputKeyParams) -> Result<ObjectProbeDto, PluginError> {
        let OutputKeyParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 key params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_probe(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn copy(&self, params: OutputCopyParams) -> Result<(), PluginError> {
        let OutputCopyParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 copy params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_copy(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn delete(&self, params: OutputKeyParams) -> Result<(), PluginError> {
        let OutputKeyParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 key params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_delete(p)
            .await
            .map_err(PluginError::internal)
    }

    async fn touch_file(&self, params: OutputTouchFileParams) -> Result<(), PluginError> {
        let OutputTouchFileParams::S3(p) = params else {
            return Err(PluginError::invalid_params(
                "s3 destination expected S3 touchFile params",
            ));
        };
        bookclerk_plugin_destination_s3::guest::guest_touch_file(p)
            .await
            .map_err(PluginError::internal)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(S3Plugin).await?;
    Ok(())
}
