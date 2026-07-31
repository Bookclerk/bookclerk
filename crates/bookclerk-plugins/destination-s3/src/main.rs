//! External S3 / MinIO output plugin for Bookclerk.

use bookclerk_plugin_destination_s3::ID;
use bookclerk_plugin_sdk::{methods, HandshakeResult, HealthDto, PluginGuest, PLUGIN_API_VERSION};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    PluginGuest::serve(|method, params| async move {
        match method.as_str() {
            methods::HANDSHAKE => Ok(serde_json::to_value(HandshakeResult {
                api_version: PLUGIN_API_VERSION,
                id: ID.into(),
                kind: "output".into(),
                display_name: Some("S3 / MinIO".into()),
                capabilities: vec![
                    "health".into(),
                    "diagnose".into(),
                    "put".into(),
                    "put_file".into(),
                    "get".into(),
                    "exists".into(),
                    "list".into(),
                    "probe".into(),
                    "copy".into(),
                    "delete".into(),
                    "touch_file".into(),
                ],
                portal_auth_mode: None,
                password_env_var: None,
                aliases: vec![],
                sort_key: Some(10),
                brand: None,
                config_options: vec![],
                cli: None,
            })
            .unwrap()),
            methods::HEALTH => Ok(serde_json::to_value(HealthDto {
                id: ID.into(),
                enabled: true,
                ok: true,
                detail: Some("S3 output plugin ready".into()),
            })
            .unwrap()),
            methods::DIAGNOSE => Ok(json!(["s3 output plugin diagnose: ok"])),
            methods::PUT => {
                let p: bookclerk_plugin_sdk::PutParams =
                    serde_json::from_value(params).map_err(|e| format!("put params: {e}"))?;
                bookclerk_plugin_destination_s3::guest::guest_put(p)
                    .await
                    .map(|_| json!(null))
            }
            methods::PUT_FILE => {
                let p: bookclerk_plugin_sdk::PutFileParams =
                    serde_json::from_value(params).map_err(|e| format!("put_file params: {e}"))?;
                bookclerk_plugin_destination_s3::guest::guest_put_file(p)
                    .await
                    .map(|_| json!(null))
            }
            methods::GET => {
                let p: bookclerk_plugin_sdk::GetParams =
                    serde_json::from_value(params).map_err(|e| format!("get params: {e}"))?;
                let dto = bookclerk_plugin_destination_s3::guest::guest_get(p).await?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::EXISTS => {
                let p: bookclerk_plugin_sdk::KeyParams =
                    serde_json::from_value(params).map_err(|e| format!("exists params: {e}"))?;
                bookclerk_plugin_destination_s3::guest::guest_exists(p).await
            }
            methods::LIST => {
                let p: bookclerk_plugin_sdk::ListParams =
                    serde_json::from_value(params).map_err(|e| format!("list params: {e}"))?;
                let dtos = bookclerk_plugin_destination_s3::guest::guest_list(p).await?;
                Ok(serde_json::to_value(dtos).unwrap())
            }
            methods::PROBE => {
                let p: bookclerk_plugin_sdk::KeyParams =
                    serde_json::from_value(params).map_err(|e| format!("probe params: {e}"))?;
                let dto = bookclerk_plugin_destination_s3::guest::guest_probe(p).await?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::COPY => {
                let p: bookclerk_plugin_sdk::CopyParams =
                    serde_json::from_value(params).map_err(|e| format!("copy params: {e}"))?;
                bookclerk_plugin_destination_s3::guest::guest_copy(p)
                    .await
                    .map(|_| json!(null))
            }
            methods::DELETE => {
                let p: bookclerk_plugin_sdk::KeyParams =
                    serde_json::from_value(params).map_err(|e| format!("delete params: {e}"))?;
                bookclerk_plugin_destination_s3::guest::guest_delete(p)
                    .await
                    .map(|_| json!(null))
            }
            methods::TOUCH_FILE => {
                let p: bookclerk_plugin_sdk::TouchFileParams = serde_json::from_value(params)
                    .map_err(|e| format!("touch_file params: {e}"))?;
                bookclerk_plugin_destination_s3::guest::guest_touch_file(p)
                    .await
                    .map(|_| json!(null))
            }
            other => Err(format!("unsupported method `{other}`")),
        }
    })
    .await?;
    Ok(())
}
