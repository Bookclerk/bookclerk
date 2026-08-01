//! Guest-side local filesystem output helpers.

use std::path::PathBuf;
use std::time::SystemTime;

use base64::Engine;
use bookclerk_plugin_sdk::{
    upload_file_path, LocalCopyParams, LocalGetParams, LocalKeyParams, LocalListParams,
    LocalPutFileParams, LocalPutParams, LocalTouchFileParams, ObjectInfoDto, ObjectMetaDto,
    ObjectProbeDto, OutputLocalContextDto,
};
use bookclerk_storage::{LocalFsBackend, ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend};

type Result<T> = std::result::Result<T, String>;

pub async fn guest_put(params: LocalPutParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
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

pub async fn guest_put_file(params: LocalPutFileParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
    let path = upload_file_path(params.local_path.as_deref()).map_err(|err| err.to_string())?;
    backend
        .put_file(&params.key, path.as_ref(), meta_from_dto(params.meta))
        .await
        .map_err(|err| err.to_string())
}

pub async fn guest_get(params: LocalGetParams) -> Result<bookclerk_plugin_sdk::GetResultDto> {
    let backend = backend_from_ctx(&params.ctx)?;
    let data = backend
        .get(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(bookclerk_plugin_sdk::GetResultDto {
        data_base64: base64::engine::general_purpose::STANDARD.encode(data),
    })
}

pub async fn guest_exists(params: LocalKeyParams) -> Result<serde_json::Value> {
    let backend = backend_from_ctx(&params.ctx)?;
    let exists = backend
        .exists(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(serde_json::json!({ "exists": exists }))
}

pub async fn guest_list(params: LocalListParams) -> Result<Vec<ObjectInfoDto>> {
    let backend = backend_from_ctx(&params.ctx)?;
    let items = backend
        .list(&params.prefix)
        .await
        .map_err(|err| err.to_string())?;
    Ok(items.into_iter().map(object_info_to_dto).collect())
}

pub async fn guest_probe(params: LocalKeyParams) -> Result<ObjectProbeDto> {
    let backend = backend_from_ctx(&params.ctx)?;
    let probe = backend
        .probe(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(object_probe_to_dto(probe))
}

pub async fn guest_copy(params: LocalCopyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
    backend
        .copy(&params.from, &params.to)
        .await
        .map_err(|err| err.to_string())
}

pub async fn guest_delete(params: LocalKeyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
    backend
        .delete(&params.key)
        .await
        .map_err(|err| err.to_string())
}

pub async fn guest_touch_file(params: LocalTouchFileParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
    backend
        .touch_file(
            &params.key,
            params.created.as_deref().and_then(parse_rfc3339),
            params.modified.as_deref().and_then(parse_rfc3339),
        )
        .await
        .map_err(|err| err.to_string())
}

fn backend_from_ctx(ctx: &OutputLocalContextDto) -> Result<LocalFsBackend> {
    LocalFsBackend::with_prefix(PathBuf::from(&ctx.root), &ctx.prefix)
        .map_err(|err| err.to_string())
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
