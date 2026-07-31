//! Guest-side S3 output helpers.

use std::time::SystemTime;

use base64::Engine;
use bookclerk_config::OutputS3Config;
use bookclerk_plugin_sdk::{
    upload_file_path, CopyParams, GetParams, GetResultDto, KeyParams, ListParams, ObjectInfoDto,
    ObjectMetaDto, ObjectProbeDto, OutputS3ContextDto, PutFileParams, PutParams, S3CredentialsDto,
    TouchFileParams,
};
use bookclerk_storage::{
    ObjectInfo, ObjectMeta, ObjectProbe, S3Backend, S3Credentials, StorageBackend,
};

type Result<T> = std::result::Result<T, String>;

pub async fn guest_put(params: PutParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(params.data_base64)
        .map_err(|err| format!("invalid put base64: {err}"))?;
    backend
        .put(
            &params.key,
            bytes::Bytes::from(data),
            meta_from_dto(params.meta),
        )
        .await
        .map_err(|err| err.to_string())
}

pub async fn guest_put_file(params: PutFileParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let path = upload_file_path(params.local_path.as_deref()).map_err(|err| err.to_string())?;
    backend
        .put_file(&params.key, path.as_ref(), meta_from_dto(params.meta))
        .await
        .map_err(|err| err.to_string())
}

pub async fn guest_get(params: GetParams) -> Result<GetResultDto> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let data = backend
        .get(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(GetResultDto {
        data_base64: base64::engine::general_purpose::STANDARD.encode(data),
    })
}

pub async fn guest_exists(params: KeyParams) -> Result<serde_json::Value> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let exists = backend
        .exists(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(serde_json::json!({ "exists": exists }))
}

pub async fn guest_list(params: ListParams) -> Result<Vec<ObjectInfoDto>> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let items = backend
        .list(&params.prefix)
        .await
        .map_err(|err| err.to_string())?;
    Ok(items.into_iter().map(object_info_to_dto).collect())
}

pub async fn guest_probe(params: KeyParams) -> Result<ObjectProbeDto> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let probe = backend
        .probe(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(object_probe_to_dto(probe))
}

pub async fn guest_copy(params: CopyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
    backend
        .copy(&params.from, &params.to)
        .await
        .map_err(|err| err.to_string())
}

pub async fn guest_delete(params: KeyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
    backend
        .delete(&params.key)
        .await
        .map_err(|err| err.to_string())
}

pub async fn guest_touch_file(params: TouchFileParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
    backend
        .touch_file(
            &params.key,
            params.created.as_deref().and_then(parse_rfc3339),
            params.modified.as_deref().and_then(parse_rfc3339),
        )
        .await
        .map_err(|err| err.to_string())
}

async fn backend_from_ctx(ctx: &OutputS3ContextDto) -> Result<S3Backend> {
    let cfg = OutputS3Config {
        enabled: true,
        bucket: ctx.bucket.clone(),
        prefix: ctx.prefix.clone(),
        region: ctx.region.clone(),
        endpoint: ctx.endpoint.clone(),
        force_path_style: ctx.force_path_style,
        naming: Default::default(),
    };
    let creds = ctx.credentials.as_ref().map(credentials_from_dto);
    S3Backend::from_parts(&cfg, &ctx.prefix, creds.as_ref())
        .await
        .map_err(|err| err.to_string())
}

fn credentials_from_dto(dto: &S3CredentialsDto) -> S3Credentials {
    S3Credentials {
        access_key_id: dto.access_key_id.clone(),
        secret_access_key: dto.secret_access_key.clone(),
        session_token: dto.session_token.clone(),
        label: None,
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

fn object_info_to_dto(info: ObjectInfo) -> ObjectInfoDto {
    ObjectInfoDto {
        key: info.key,
        size: info.size,
    }
}

fn object_probe_to_dto(probe: ObjectProbe) -> ObjectProbeDto {
    ObjectProbeDto {
        key: probe.key,
        size: probe.size,
        content_type: probe.content_type,
        meta: ObjectMetaDto {
            content_type: probe.meta.content_type,
            content_length: probe.meta.content_length,
            asin: probe.meta.asin,
            title: probe.meta.title,
            creation_time: probe.meta.creation_time,
            last_write_time: probe.meta.last_write_time,
        },
    }
}

fn parse_rfc3339(raw: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(SystemTime::from)
}
