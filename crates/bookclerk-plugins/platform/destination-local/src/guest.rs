//! Guest-side local filesystem destination helpers.
//!
//! Each function is the Workers RPC body for one storage method. The host
//! passes an [`OutputLocalContextDto`] (root + key prefix); the guest opens a
//! [`LocalFsBackend`] and maps results to ABI DTOs. Prefer these over calling
//! `bookclerk_storage` directly from host code so Landlock / AppContainer
//! confinement stays around the guest process.

use std::path::PathBuf;
use std::time::SystemTime;

use base64::Engine;
use bookclerk_plugin_sdk::{
    upload_file_path, LocalCopyParams, LocalGetParams, LocalKeyParams, LocalListParams,
    LocalPutFileParams, LocalPutParams, LocalTouchFileParams, ObjectInfoDto, ObjectMetaDto,
    ObjectProbeDto, OutputLocalContextDto,
};
use bookclerk_storage::{LocalFsBackend, ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend};

/// Guest RPC result that surfaces storage failures as operator-facing strings.
type Result<T> = std::result::Result<T, String>;

/// Writes object bytes under `params.key` relative to the configured local root.
///
/// Call after acquire packaging when the payload is already in memory (small
/// metadata or test fixtures). Prefer [`guest_put_file`] for large audio.
///
/// # Arguments
///
/// * `params` - Root/prefix context, object key, base64 body, and optional meta.
///
/// # Errors
///
/// Returns an error string when base64 decoding fails, the root is invalid, or
/// the filesystem write fails.
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

/// Streams a local cache file into the destination under `params.key`.
///
/// Use this for acquire output: the host stages audio beside the guest and
/// passes `local_path` (or the upload FD path) instead of base64.
///
/// # Arguments
///
/// * `params` - Context, key, path to the staged file, and optional meta.
///
/// # Errors
///
/// Returns an error string when the upload path cannot be resolved or the
/// filesystem write fails.
pub async fn guest_put_file(params: LocalPutFileParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
    let path = upload_file_path(params.local_path.as_deref()).map_err(|err| err.to_string())?;
    backend
        .put_file(&params.key, path.as_ref(), meta_from_dto(params.meta))
        .await
        .map_err(|err| err.to_string())
}

/// Reads an object and returns its bytes as standard base64.
///
/// # Arguments
///
/// * `params` - Context and object key.
///
/// # Returns
///
/// [`GetResultDto`](bookclerk_plugin_sdk::GetResultDto) with `data_base64`.
///
/// # Errors
///
/// Returns an error string when the key is missing or the read fails.
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

/// Returns whether an object exists at `params.key`.
///
/// # Arguments
///
/// * `params` - Context and object key.
///
/// # Errors
///
/// Returns an error string when the backend cannot check the path.
pub async fn guest_exists(params: LocalKeyParams) -> Result<bool> {
    let backend = backend_from_ctx(&params.ctx)?;
    backend
        .exists(&params.key)
        .await
        .map_err(|err| err.to_string())
}

/// Lists objects whose keys start with `params.prefix`.
///
/// # Arguments
///
/// * `params` - Context and key prefix (empty = entire configured tree).
///
/// # Returns
///
/// Key/size pairs for matching objects.
///
/// # Errors
///
/// Returns an error string when directory listing fails.
pub async fn guest_list(params: LocalListParams) -> Result<Vec<ObjectInfoDto>> {
    let backend = backend_from_ctx(&params.ctx)?;
    let items = backend
        .list(&params.prefix)
        .await
        .map_err(|err| err.to_string())?;
    Ok(items.into_iter().map(object_info_to_dto).collect())
}

/// Returns size and metadata for one object without reading its body.
///
/// # Arguments
///
/// * `params` - Context and object key.
///
/// # Errors
///
/// Returns an error string when the key is missing or metadata cannot be read.
pub async fn guest_probe(params: LocalKeyParams) -> Result<ObjectProbeDto> {
    let backend = backend_from_ctx(&params.ctx)?;
    let probe = backend
        .probe(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(object_probe_to_dto(probe))
}

/// Copies an object from `params.from` to `params.to` within the same root.
///
/// # Arguments
///
/// * `params` - Context plus source and destination keys.
///
/// # Errors
///
/// Returns an error string when either key is invalid or the copy fails.
pub async fn guest_copy(params: LocalCopyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
    backend
        .copy(&params.from, &params.to)
        .await
        .map_err(|err| err.to_string())
}

/// Deletes the object at `params.key` if present.
///
/// # Arguments
///
/// * `params` - Context and object key.
///
/// # Errors
///
/// Returns an error string when the delete fails (missing keys are backend-dependent).
pub async fn guest_delete(params: LocalKeyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx)?;
    backend
        .delete(&params.key)
        .await
        .map_err(|err| err.to_string())
}

/// Updates filesystem created/modified timestamps for an existing object.
///
/// Used after packaging so players and sync tools see bookstore purchase times
/// instead of download wall-clock. Timestamps are RFC3339 strings when set.
///
/// # Arguments
///
/// * `params` - Context, key, and optional `created` / `modified` RFC3339 values.
///
/// # Errors
///
/// Returns an error string when the key is missing or `utimens`-style update fails.
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

/// Opens a `LocalFsBackend` under the host-supplied root and key prefix.
fn backend_from_ctx(ctx: &OutputLocalContextDto) -> Result<LocalFsBackend> {
    LocalFsBackend::with_prefix(PathBuf::from(&ctx.root), &ctx.prefix)
        .map_err(|err| err.to_string())
}

/// Copies ABI object metadata into the storage-backend struct.
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

/// Projects a listed object’s key and size onto the ABI DTO.
fn object_info_to_dto(info: ObjectInfo) -> ObjectInfoDto {
    ObjectInfoDto {
        key: info.key,
        size: info.size,
    }
}

/// Projects a probed object (size, type, meta) onto the ABI DTO.
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

/// Parses an RFC 3339 timestamp for `touch`; `None` when the string is not a valid datetime.
fn parse_rfc3339(raw: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(SystemTime::from)
}
