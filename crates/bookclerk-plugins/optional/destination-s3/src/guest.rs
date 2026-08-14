//! Guest-side S3 / MinIO destination helpers.
//!
//! Each function is the Workers RPC body for one storage method. The host
//! injects bucket/region/endpoint plus optional explicit credentials; the guest
//! builds an [`S3Backend`] and maps results to ABI DTOs so network and secrets
//! stay inside the jailed process.

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

/// Guest RPC result; errors are operator-facing strings returned to the host.
type Result<T> = std::result::Result<T, String>;

/// Uploads object bytes to `params.key` under the configured bucket/prefix.
///
/// Prefer [`guest_put_file`] for large audio; this path base64-decodes the body
/// in the guest and is intended for small payloads or tests.
///
/// # Arguments
///
/// * `params` - Bucket context, object key, base64 body, and optional meta.
///
/// # Errors
///
/// Returns an error string when credentials/config are invalid, base64 decode
/// fails, or the PutObject call fails.
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

/// Uploads a staged local file to `params.key` via the S3 multipart/put path.
///
/// Call during acquire after the media worker writes the packaged file into the
/// guest-visible upload path.
///
/// # Arguments
///
/// * `params` - Context, key, staged file path, and optional meta.
///
/// # Errors
///
/// Returns an error string when the upload path cannot be resolved or S3
/// upload fails.
pub async fn guest_put_file(params: PutFileParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let path = upload_file_path(params.local_path.as_deref()).map_err(|err| err.to_string())?;
    backend
        .put_file(&params.key, path.as_ref(), meta_from_dto(params.meta))
        .await
        .map_err(|err| err.to_string())
}

/// Downloads an object and returns its bytes as standard base64.
///
/// # Arguments
///
/// * `params` - Context and object key.
///
/// # Returns
///
/// [`GetResultDto`] with `data_base64`.
///
/// # Errors
///
/// Returns an error string when the object is missing or GetObject fails.
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

/// Reports whether an object exists at `params.key` (HeadObject / existence check).
///
/// # Arguments
///
/// * `params` - Context and object key.
///
/// # Errors
///
/// Returns an error string when HeadObject / existence check fails unexpectedly.
pub async fn guest_exists(params: KeyParams) -> Result<bool> {
    let backend = backend_from_ctx(&params.ctx).await?;
    backend
        .exists(&params.key)
        .await
        .map_err(|err| err.to_string())
}

/// Lists objects whose keys start with `params.prefix`.
///
/// # Arguments
///
/// * `params` - Context and key prefix (empty = entire configured prefix).
///
/// # Returns
///
/// Key/size pairs for matching objects.
///
/// # Errors
///
/// Returns an error string when ListObjects fails.
pub async fn guest_list(params: ListParams) -> Result<Vec<ObjectInfoDto>> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let items = backend
        .list(&params.prefix)
        .await
        .map_err(|err| err.to_string())?;
    Ok(items.into_iter().map(object_info_to_dto).collect())
}

/// Returns size and metadata for one object without downloading its body.
///
/// # Arguments
///
/// * `params` - Context and object key.
///
/// # Errors
///
/// Returns an error string when the object is missing or HeadObject fails.
pub async fn guest_probe(params: KeyParams) -> Result<ObjectProbeDto> {
    let backend = backend_from_ctx(&params.ctx).await?;
    let probe = backend
        .probe(&params.key)
        .await
        .map_err(|err| err.to_string())?;
    Ok(object_probe_to_dto(probe))
}

/// Server-side copies an object from `params.from` to `params.to` in the bucket.
///
/// # Arguments
///
/// * `params` - Context plus source and destination keys.
///
/// # Errors
///
/// Returns an error string when CopyObject fails.
pub async fn guest_copy(params: CopyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
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
/// Returns an error string when DeleteObject fails.
pub async fn guest_delete(params: KeyParams) -> Result<()> {
    let backend = backend_from_ctx(&params.ctx).await?;
    backend
        .delete(&params.key)
        .await
        .map_err(|err| err.to_string())
}

/// Best-effort metadata timestamp update for an existing object.
///
/// S3 does not expose POSIX mtimes the way local FS does; the backend maps
/// RFC3339 `created` / `modified` onto supported metadata fields when possible.
///
/// # Arguments
///
/// * `params` - Context, key, and optional RFC3339 timestamps.
///
/// # Errors
///
/// Returns an error string when the object is missing or the metadata update fails.
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

/// Builds an [`S3Backend`] from the host-injected bucket/region/endpoint context.
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

/// Copies ABI credential fields into the storage crate's credential struct.
fn credentials_from_dto(dto: &S3CredentialsDto) -> S3Credentials {
    S3Credentials {
        access_key_id: dto.access_key_id.clone(),
        secret_access_key: dto.secret_access_key.clone(),
        session_token: dto.session_token.clone(),
        label: None,
    }
}

/// Maps an ABI [`ObjectMetaDto`] onto [`ObjectMeta`] for Put/PutFile.
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

/// Maps a listed object's key and size onto the ABI DTO.
fn object_info_to_dto(info: ObjectInfo) -> ObjectInfoDto {
    ObjectInfoDto {
        key: info.key,
        size: info.size,
    }
}

/// Maps a HeadObject-style probe (size + metadata) onto the ABI DTO.
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

/// Parses an RFC3339 timestamp into [`SystemTime`]; invalid strings become `None`.
fn parse_rfc3339(raw: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(SystemTime::from)
}
