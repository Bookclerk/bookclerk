//! Uploads: a patron adds books by sending files, folders or archives.
//!
//! A client declares a batch with a manifest (`POST /api/uploads`), sends
//! each file's bytes in resumable `PATCH` requests, then commits the batch,
//! which queues an `ingest_batch` job on this node ([`job`]). The job
//! expands archives in the confined media worker, groups files into books,
//! resolves each book through an import listing the uploader holds, and
//! writes it to the library's storage. See `docs/server/uploads.md` and
//! `docs/adr/uploads-and-ingest.md`.
//!
//! Who may upload is the `upload` member control (off unless an Owner
//! turns it on); how much is the `[uploads]` settings. Bytes wait under
//! `$BOOKCLERK_FILES_DIR/uploads/` ([`staging`]) and are pruned when a
//! batch idles, finishes or loses its row.

pub(crate) mod identify;
pub(crate) mod job;
pub(crate) mod staging;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_library::control_plane::{
    batch_exists, batch_members, cancel_batch, create_upload_batch, delete_finished_before,
    final_batches_on, first_active_owner, idle_upload_batches, load_batch,
    load_or_create_node_identity, mark_committed, parse_upload_manifest, parse_upload_offset,
    record_received, review_items, user_batches, CreateRefusal, IngestBatch, IngestBatchId,
    IngestMember, IngestSource, IngestState, ManifestError, UploadQuota,
};
use bookclerk_library::{EnqueueJobSpec, JobKind, JobPayload, LibraryStore};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::api::AppState;
use crate::http_error::ErrorMessage;
use crate::jobs::{admit, queue_limits, AdmitJob};

pub(crate) use job::run_ingest_batch;

/// Largest manifest body accepted, in bytes: room for the most files a
/// batch may name with long paths and digests.
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;

/// Most batches `GET /api/uploads` lists.
const LIST_LIMIT: u64 = 50;

/// The media type of a `PATCH` body.
const OFFSET_STREAM: &str = "application/offset+octet-stream";

/// The header naming where a `PATCH` resumes, and where it ended.
const UPLOAD_OFFSET: &str = "upload-offset";

/// How long after start the prune loop first runs.
const PRUNE_STARTUP_DELAY: Duration = Duration::from_secs(120);

/// How often the prune loop runs.
const PRUNE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long finished batches' rows are kept for the uploader to read.
const FINISHED_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// An error answer.
type UploadError = (StatusCode, Json<ErrorMessage>);

/// An error answer with `status` and `message`.
fn refused(status: StatusCode, message: impl std::fmt::Display) -> UploadError {
    (status, Json(ErrorMessage::new(message)))
}

/// Logs a failed library call and returns the 500 answer.
fn failed(route: &str, err: &dyn std::fmt::Display) -> UploadError {
    tracing::error!(error = %err, route, "upload route failed");
    refused(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the upload could not be read or written",
    )
}

/// The patron an upload's books go to, and the actor its audit rows name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Uploader {
    /// The patron who holds the batch's books.
    user_id: i64,
    /// `operator` or `user:<id>`.
    actor: String,
}

/// Resolves the caller to the patron who holds what they upload: a
/// signed-in patron holds their own; the operator's uploads go to the
/// first active Owner.
///
/// # Errors
///
/// 401 when signed out, 403 for a sign-in linked to no patron or a
/// library with no active Owner, 500 when the caller cannot be read.
async fn uploader(
    state: &AppState,
    library: &LibraryStore,
    headers: &HeaderMap,
) -> Result<Uploader, UploadError> {
    match crate::auth::caller_subject(state, library, headers).await {
        crate::auth::CallerSubject::Operator => {
            let owner = first_active_owner(library)
                .await
                .map_err(|err| failed("uploader", &err))?
                .ok_or_else(|| {
                    refused(
                        StatusCode::FORBIDDEN,
                        "the library has no active Owner to hold uploaded books",
                    )
                })?;
            Ok(Uploader {
                user_id: owner,
                actor: "operator".to_string(),
            })
        }
        crate::auth::CallerSubject::Person(Some(user_id)) => Ok(Uploader {
            user_id,
            actor: format!("user:{user_id}"),
        }),
        crate::auth::CallerSubject::Person(None) => Err(refused(
            StatusCode::FORBIDDEN,
            "this sign-in is not linked to a patron who can hold books",
        )),
        crate::auth::CallerSubject::SignedOut => {
            Err(refused(StatusCode::UNAUTHORIZED, "sign in first"))
        }
        crate::auth::CallerSubject::Unavailable => Err(failed("uploader", &"caller unavailable")),
    }
}

/// The batch `raw` names, when it exists and belongs to `uploader`.
///
/// # Errors
///
/// 404 for a malformed id, a missing batch or another patron's batch.
async fn owned_batch(
    library: &LibraryStore,
    uploader: &Uploader,
    raw: &str,
) -> Result<IngestBatch, UploadError> {
    let not_found = || refused(StatusCode::NOT_FOUND, "no such upload");
    let id = IngestBatchId::parse(raw).ok_or_else(not_found)?;
    let batch = load_batch(library, &id)
        .await
        .map_err(|err| failed("load batch", &err))?
        .ok_or_else(not_found)?;
    if batch.user_id != uploader.user_id {
        return Err(not_found());
    }
    Ok(batch)
}

/// One file of an upload.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UploadMemberJson {
    /// Its number in the upload; manifest files are numbered from 0 in
    /// order, an archive's entries after every earlier file.
    #[schema(example = 0)]
    member_no: u32,
    /// The archive it was expanded from.
    #[schema(example = json!(null))]
    parent_no: Option<u32>,
    /// Its path in the upload (for a refused archive entry, its cleaned
    /// name).
    #[schema(example = "Dune/01 Dune.m4b")]
    path: String,
    /// Its size in bytes.
    #[schema(example = 734_003_200)]
    size: u64,
    /// Bytes received so far; resume a `PATCH` here.
    #[schema(example = 734_003_200)]
    received: u64,
    /// What became of it: `pending`, `expanded` (an archive), `matched`,
    /// `needs_review`, `duplicate`, `refused` or `failed`.
    #[schema(example = "matched")]
    outcome: String,
    /// Why it was refused or failed, as a stable code.
    #[schema(example = json!(null))]
    reason: Option<String>,
    /// The edition it was stored under.
    #[schema(example = "6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b")]
    edition_id: Option<String>,
}

impl From<IngestMember> for UploadMemberJson {
    fn from(member: IngestMember) -> Self {
        Self {
            member_no: member.member_no,
            parent_no: member.parent_no,
            path: member.path,
            size: member.size,
            received: member.received,
            outcome: member.outcome.as_str().to_string(),
            reason: member.reason,
            edition_id: member.edition_id,
        }
    }
}

/// One upload.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UploadJson {
    /// The upload id (32 hex digits).
    #[schema(example = "4f3c2b1a0e9d8c7b6a5f4e3d2c1b0a99")]
    id: String,
    /// `open` (taking bytes), `committed` (queued), `running`, `done`,
    /// `failed` or `cancelled`.
    #[schema(example = "open")]
    state: String,
    /// `upload`, or `intake` for a batch a drop folder made.
    #[schema(example = "upload")]
    source: String,
    /// Files the manifest named.
    #[schema(example = 2)]
    file_count: u64,
    /// Bytes the manifest declared.
    #[schema(example = 734_068_736)]
    declared_bytes: u64,
    /// The ingest job, once committed.
    #[schema(example = json!(null))]
    job_id: Option<String>,
    /// Why the upload failed.
    #[schema(example = json!(null))]
    detail: Option<String>,
    /// When it was created (RFC 3339).
    #[schema(example = "2026-11-20T09:00:00Z")]
    created_at: String,
    /// Its last activity (RFC 3339).
    #[schema(example = "2026-11-20T09:05:00Z")]
    updated_at: String,
    /// When it finished (RFC 3339).
    #[schema(example = json!(null))]
    finished_at: Option<String>,
    /// Its files; left out of lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<UploadMemberJson>>,
}

impl UploadJson {
    /// The JSON of `batch`, with `members` when given.
    fn new(batch: IngestBatch, members: Option<Vec<IngestMember>>) -> Self {
        Self {
            id: batch.id.as_str().to_string(),
            state: batch.state.as_str().to_string(),
            source: batch.source.as_str().to_string(),
            file_count: batch.file_count,
            declared_bytes: batch.declared_bytes,
            job_id: batch.job_id,
            detail: batch.detail,
            created_at: batch.created_at.to_rfc3339(),
            updated_at: batch.updated_at.to_rfc3339(),
            finished_at: batch.finished_at.map(|at| at.to_rfc3339()),
            files: members.map(|members| members.into_iter().map(Into::into).collect()),
        }
    }
}

/// The caller's uploads, newest first.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct UploadList {
    /// At most 50 uploads, without their files.
    uploads: Vec<UploadJson>,
}

/// Starts an upload from a manifest of the files to come.
///
/// The caller must be allowed to upload (the `upload` member control). The
/// operator's uploads are held by the library's first active Owner. Paths
/// are relative (`Folder/File.m4b`), at most 10,000 files, each within
/// `uploads.max_file_bytes`, all within the caller's staging quota.
#[utoipa::path(
    post,
    path = "/api/uploads",
    request_body(content = bookclerk_library::control_plane::UploadManifest, content_type = "application/json"),
    responses(
        (status = 201, description = "The upload is open for bytes.", body = UploadJson),
        (status = 400, description = "The manifest is malformed, names no file or too many, or a path is refused or clashes.", body = ErrorMessage, example = json!({"error": "file 1 has the same path as an earlier file"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller may not upload.", body = ErrorMessage, example = json!({"error": "uploads are not allowed for this person"})),
        (status = 413, description = "A file passes the per-file limit or the upload passes the staging quota.", body = ErrorMessage, example = json!({"error": "file 0 is larger than the per-file limit"})),
        (status = 415, description = "The body is not JSON.", body = ErrorMessage, example = json!({"error": "send the manifest as application/json"})),
        (status = 500, description = "The upload could not be created.", body = ErrorMessage, example = json!({"error": "the upload could not be read or written"})),
        (status = 507, description = "The disk would drop below its free-space floor.", body = ErrorMessage, example = json!({"error": "this server is low on disk space"})),
    ),
)]
pub(crate) async fn create_upload(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<(StatusCode, Json<UploadJson>), UploadError> {
    let (parts, body) = request.into_parts();
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next().map(str::trim) == Some("application/json"));
    if !is_json {
        return Err(refused(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send the manifest as application/json",
        ));
    }
    let library = state.library_snapshot().await;
    let uploader = uploader(&state, &library, &parts.headers).await?;
    let body = axum::body::to_bytes(body, MAX_MANIFEST_BYTES)
        .await
        .map_err(|_| refused(StatusCode::PAYLOAD_TOO_LARGE, "the manifest is too large"))?;
    let specs = parse_upload_manifest(&body).map_err(|err| {
        let status = match err {
            ManifestError::Malformed(_)
            | ManifestError::Path { .. }
            | ManifestError::Digest { .. }
            | ManifestError::TooManyFiles => StatusCode::BAD_REQUEST,
        };
        refused(status, err)
    })?;
    let (uploads, files_dir) = {
        let cfg = state.config.read().await;
        (cfg.uploads, cfg.paths().files_dir.clone())
    };
    let declared = specs
        .iter()
        .fold(0u64, |sum, spec| sum.saturating_add(spec.size));
    if staging::below_floor(&files_dir, declared, uploads.free_disk_floor()) {
        return Err(refused(
            StatusCode::INSUFFICIENT_STORAGE,
            "this server is low on disk space",
        ));
    }
    let node_id = load_or_create_node_identity(&files_dir)
        .map_err(|err| failed("node identity", &err))?
        .node_id;
    let quota = UploadQuota {
        max_file_bytes: uploads.max_file_bytes,
        max_staged_bytes: uploads.max_staged_bytes,
    };
    let batch = create_upload_batch(&library, uploader.user_id, &node_id, &specs, quota)
        .await
        .map_err(|err| failed("create batch", &err))?
        .map_err(|refusal| {
            let status = match refusal {
                CreateRefusal::NotAllowed => StatusCode::FORBIDDEN,
                CreateRefusal::FileTooLarge { .. } | CreateRefusal::OverQuota { .. } => {
                    StatusCode::PAYLOAD_TOO_LARGE
                }
                CreateRefusal::Empty
                | CreateRefusal::TooManyFiles
                | CreateRefusal::PathClash { .. }
                | CreateRefusal::BadReference { .. } => StatusCode::BAD_REQUEST,
            };
            refused(status, refusal)
        })?;
    let detail = json!({
        "batch_id": batch.id.as_str(),
        "user_id": uploader.user_id,
        "files": batch.file_count,
        "bytes": batch.declared_bytes,
    })
    .to_string();
    let _ = library
        .insert_security_audit_event(&uploader.actor, "upload_batch_created", Some(&detail))
        .await;
    let members = batch_members(&library, &batch.id)
        .await
        .map_err(|err| failed("batch members", &err))?;
    Ok((
        StatusCode::CREATED,
        Json(UploadJson::new(batch, Some(members))),
    ))
}

/// Lists the caller's uploads, newest first.
#[utoipa::path(
    get,
    path = "/api/uploads",
    responses(
        (status = 200, description = "The caller's uploads.", body = UploadList),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The sign-in is linked to no patron.", body = ErrorMessage, example = json!({"error": "this sign-in is not linked to a patron who can hold books"})),
        (status = 500, description = "The uploads could not be read.", body = ErrorMessage, example = json!({"error": "the upload could not be read or written"})),
    ),
)]
pub(crate) async fn list_uploads(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<UploadList>, UploadError> {
    let library = state.library_snapshot().await;
    let uploader = uploader(&state, &library, &headers).await?;
    let batches = user_batches(&library, uploader.user_id, LIST_LIMIT)
        .await
        .map_err(|err| failed("list batches", &err))?;
    Ok(Json(UploadList {
        uploads: batches
            .into_iter()
            .map(|batch| UploadJson::new(batch, None))
            .collect(),
    }))
}

/// Reads one of the caller's uploads with every file: how many bytes each
/// has received (where to resume) and, once ingested, what became of it.
#[utoipa::path(
    get,
    path = "/api/uploads/{id}",
    params(("id" = String, Path, description = "Upload id.")),
    responses(
        (status = 200, description = "The upload and its files.", body = UploadJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 404, description = "No such upload of the caller's.", body = ErrorMessage, example = json!({"error": "no such upload"})),
        (status = 500, description = "The upload could not be read.", body = ErrorMessage, example = json!({"error": "the upload could not be read or written"})),
    ),
)]
pub(crate) async fn get_upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<UploadJson>, UploadError> {
    let library = state.library_snapshot().await;
    let uploader = uploader(&state, &library, &headers).await?;
    let batch = owned_batch(&library, &uploader, &id).await?;
    let members = batch_members(&library, &batch.id)
        .await
        .map_err(|err| failed("batch members", &err))?;
    Ok(Json(UploadJson::new(batch, Some(members))))
}

/// Where a file's bytes stand after a `PATCH`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReceivedJson {
    /// The file's number.
    #[schema(example = 0)]
    member_no: u32,
    /// Bytes received; send the next `PATCH` from here.
    #[schema(example = 67_108_864)]
    received: u64,
    /// True once every byte has arrived.
    #[schema(example = false)]
    complete: bool,
}

/// Sends bytes of one file, from the offset in the `Upload-Offset` header.
///
/// The body is `application/offset+octet-stream`. A request that breaks
/// off keeps the bytes that arrived; read the upload (or the answer's
/// `received`) and resume from there. Bytes past the file's declared size
/// are refused.
#[utoipa::path(
    patch,
    path = "/api/uploads/{id}/files/{member_no}",
    params(
        ("id" = String, Path, description = "Upload id."),
        ("member_no" = u32, Path, description = "The file's number in the manifest."),
        ("Upload-Offset" = u64, Header, description = "Bytes of the file already received."),
    ),
    request_body(content = crate::openapi::BinaryBody, content_type = "application/offset+octet-stream"),
    responses(
        (status = 200, description = "The bytes were stored.", body = ReceivedJson),
        (status = 400, description = "The Upload-Offset header is missing or malformed.", body = ErrorMessage, example = json!({"error": "send the Upload-Offset header"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 404, description = "No such upload of the caller's, or no such file in it.", body = ErrorMessage, example = json!({"error": "no such upload"})),
        (status = 409, description = "The offset is not where the file stands, another request is sending this file, or the upload no longer takes bytes.", body = ErrorMessage, example = json!({"error": "the file has 1024 bytes; resume from there"})),
        (status = 413, description = "The bytes run past the file's declared size.", body = ErrorMessage, example = json!({"error": "the bytes run past the file's declared size"})),
        (status = 415, description = "The body is not application/offset+octet-stream.", body = ErrorMessage, example = json!({"error": "send the bytes as application/offset+octet-stream"})),
        (status = 500, description = "The bytes could not be stored.", body = ErrorMessage, example = json!({"error": "the upload could not be read or written"})),
        (status = 507, description = "The disk would drop below its free-space floor.", body = ErrorMessage, example = json!({"error": "this server is low on disk space"})),
    ),
)]
pub(crate) async fn patch_upload_file(
    State(state): State<Arc<AppState>>,
    Path((id, member_no)): Path<(String, String)>,
    request: Request,
) -> Result<Response, UploadError> {
    let (parts, body) = request.into_parts();
    let is_stream = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == OFFSET_STREAM);
    if !is_stream {
        return Err(refused(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send the bytes as application/offset+octet-stream",
        ));
    }
    let offset = parts
        .headers
        .get(UPLOAD_OFFSET)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_upload_offset)
        .ok_or_else(|| refused(StatusCode::BAD_REQUEST, "send the Upload-Offset header"))?;
    let library = state.library_snapshot().await;
    let uploader = uploader(&state, &library, &parts.headers).await?;
    let batch = owned_batch(&library, &uploader, &id).await?;
    if batch.state != IngestState::Open || batch.source != IngestSource::Upload {
        return Err(refused(
            StatusCode::CONFLICT,
            "the upload no longer takes bytes",
        ));
    }
    let no_file = || refused(StatusCode::NOT_FOUND, "no such file in the upload");
    let member_no: u32 = member_no.parse().map_err(|_| no_file())?;
    let member = bookclerk_library::control_plane::load_member(&library, &batch.id, member_no)
        .await
        .map_err(|err| failed("load member", &err))?
        .filter(|member| member.parent_no.is_none())
        .ok_or_else(no_file)?;
    let Some(_slot) = staging::try_write_slot(&batch.id, member_no) else {
        return Err(refused(
            StatusCode::CONFLICT,
            "another request is sending this file",
        ));
    };
    let (floor, files_dir) = {
        let cfg = state.config.read().await;
        (cfg.uploads.free_disk_floor(), cfg.paths().files_dir.clone())
    };
    if staging::below_floor(&files_dir, member.size.saturating_sub(offset), floor) {
        return Err(refused(
            StatusCode::INSUFFICIENT_STORAGE,
            "this server is low on disk space",
        ));
    }
    let path = staging::member_file(&files_dir, &batch.id, member_no);
    let appended = match staging::append(&path, offset, member.size, body).await {
        Ok(appended) => appended,
        Err(staging::AppendError::OffsetMismatch { current }) => {
            return Err(refused(
                StatusCode::CONFLICT,
                format!("the file has {current} bytes; resume from there"),
            ))
        }
        Err(staging::AppendError::TooLong) => {
            return Err(refused(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the bytes run past the file's declared size",
            ))
        }
        Err(staging::AppendError::Io(err)) => return Err(failed("append", &err)),
    };
    let recorded = record_received(&library, &batch.id, member_no, appended.len)
        .await
        .map_err(|err| failed("record received", &err))?;
    if !recorded {
        return Err(refused(
            StatusCode::CONFLICT,
            "the upload no longer takes bytes",
        ));
    }
    let mut response = Json(ReceivedJson {
        member_no,
        received: appended.len,
        complete: appended.len == member.size,
    })
    .into_response();
    if let Ok(value) = HeaderValue::from_str(&appended.len.to_string()) {
        response.headers_mut().insert(UPLOAD_OFFSET, value);
    }
    Ok(response)
}

/// Commits an upload whose files have all arrived: queues its ingest job.
#[utoipa::path(
    post,
    path = "/api/uploads/{id}/commit",
    params(("id" = String, Path, description = "Upload id.")),
    responses(
        (status = 202, description = "The ingest job is queued.", body = UploadJson),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 404, description = "No such upload of the caller's.", body = ErrorMessage, example = json!({"error": "no such upload"})),
        (status = 409, description = "Files are still missing bytes, or the upload is not open.", body = ErrorMessage, example = json!({"error": "2 files are still missing bytes"})),
        (status = 500, description = "The upload could not be committed.", body = ErrorMessage, example = json!({"error": "the upload could not be read or written"})),
        (status = 503, description = "The job queue is full; try again later.", body = ErrorMessage, example = json!({"error": "the job queue is full"})),
    ),
)]
pub(crate) async fn commit_upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<UploadJson>), UploadError> {
    let library = state.library_snapshot().await;
    let uploader = uploader(&state, &library, &headers).await?;
    let batch = owned_batch(&library, &uploader, &id).await?;
    if batch.state != IngestState::Open {
        return Err(refused(StatusCode::CONFLICT, "the upload is not open"));
    }
    let members = batch_members(&library, &batch.id)
        .await
        .map_err(|err| failed("batch members", &err))?;
    let missing = members.iter().filter(|member| !member.complete()).count();
    if missing > 0 {
        return Err(refused(
            StatusCode::CONFLICT,
            format!("{missing} files are still missing bytes"),
        ));
    }
    let job_id = match queue_batch(&state, &library, &batch).await {
        Ok(job_id) => job_id,
        Err(QueueRefusal::QueueFull) => {
            return Err(refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "the job queue is full",
            ))
        }
        // The queued job finds the batch uncommitted and fails at once.
        Err(QueueRefusal::NotOpen) => {
            return Err(refused(StatusCode::CONFLICT, "the upload is not open"))
        }
        Err(QueueRefusal::Failed(err)) => return Err(failed("queue batch", &err)),
    };
    let detail = json!({ "batch_id": batch.id.as_str(), "job_id": job_id }).to_string();
    let _ = library
        .insert_security_audit_event(&uploader.actor, "upload_batch_committed", Some(&detail))
        .await;
    let batch = load_batch(&library, &batch.id)
        .await
        .map_err(|err| failed("load batch", &err))?
        .ok_or_else(|| refused(StatusCode::NOT_FOUND, "no such upload"))?;
    Ok((StatusCode::ACCEPTED, Json(UploadJson::new(batch, None))))
}

/// Why a complete batch could not be queued.
#[derive(Debug)]
pub(crate) enum QueueRefusal {
    /// The job queue is full; try again later.
    QueueFull,
    /// The batch is no longer open (cancelled or already committed).
    NotOpen,
    /// The library could not be read or written.
    Failed(anyhow::Error),
}

/// Queues the `ingest_batch` job for a batch whose members are all
/// complete, on the node that holds its bytes, and marks it committed.
///
/// # Returns
///
/// The job id.
///
/// # Errors
///
/// [`QueueRefusal`] when the queue is full, the batch is not open, or the
/// library fails.
async fn queue_batch(
    state: &AppState,
    library: &LibraryStore,
    batch: &IngestBatch,
) -> Result<String, QueueRefusal> {
    let (max_pending, max_attempts) = queue_limits(state).await;
    let admitted = admit(
        state,
        EnqueueJobSpec {
            kind: JobKind::IngestBatch,
            payload: JobPayload {
                node_id: Some(batch.node_id.clone()),
                ingest_batch: Some(batch.id.as_str().to_string()),
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts,
            max_pending,
            run_after: None,
        },
    )
    .await
    .map_err(QueueRefusal::Failed)?;
    let job_id = match admitted {
        AdmitJob::Created(id) | AdmitJob::Duplicate(id) => id,
        AdmitJob::QueueFull => return Err(QueueRefusal::QueueFull),
    };
    let committed = mark_committed(library, &batch.id, &job_id)
        .await
        .map_err(|err| QueueRefusal::Failed(err.into()))?;
    if committed {
        Ok(job_id)
    } else {
        Err(QueueRefusal::NotOpen)
    }
}

/// Why an intake batch was not queued.
#[derive(Debug)]
pub(crate) enum IntakeError {
    /// The members were refused (empty, too many, clashing paths, bad
    /// references) or the holder may not upload.
    Refused(CreateRefusal),
    /// The batch was created but not queued; it stays open until the idle
    /// prune cancels it.
    Queue(QueueRefusal),
}

/// Queues one ingest batch of storage objects for `user_id` to hold: the
/// drop folder's entry point.
///
/// The members name objects in the storage instance `instance` deployed on
/// this node; the job copies each one into staging, then ingests it like
/// an upload. The batch's end publishes `ingest_batch_finished`.
///
/// # Arguments
///
/// * `state` - the daemon state.
/// * `user_id` - the patron who holds the books.
/// * `instance` - the storage plugin instance id the objects live in.
/// * `specs` - one member per object: relative path, size, optional
///   SHA-256 and its [`bookclerk_library::control_plane::IntakeRef`].
///
/// # Returns
///
/// The queued batch's id.
///
/// # Errors
///
/// [`IntakeError`] when the members are refused or the job cannot be
/// queued.
pub(crate) async fn queue_intake_batch(
    state: &AppState,
    user_id: i64,
    instance: &str,
    specs: &[bookclerk_library::control_plane::MemberSpec],
) -> Result<IngestBatchId, IntakeError> {
    let library = state.library_snapshot().await;
    let files_dir = state.config.read().await.paths().files_dir.clone();
    let node_id = load_or_create_node_identity(&files_dir)
        .map_err(|err| IntakeError::Queue(QueueRefusal::Failed(err.into())))?
        .node_id;
    let batch = bookclerk_library::control_plane::create_intake_batch(
        &library, user_id, &node_id, instance, specs,
    )
    .await
    .map_err(|err| IntakeError::Queue(QueueRefusal::Failed(err.into())))?
    .map_err(IntakeError::Refused)?;
    queue_batch(state, &library, &batch)
        .await
        .map_err(IntakeError::Queue)?;
    Ok(batch.id)
}

/// Cancels an upload that has not started ingesting and drops its bytes.
#[utoipa::path(
    delete,
    path = "/api/uploads/{id}",
    params(("id" = String, Path, description = "Upload id.")),
    responses(
        (status = 204, description = "The upload is cancelled."),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 404, description = "No such upload of the caller's.", body = ErrorMessage, example = json!({"error": "no such upload"})),
        (status = 409, description = "The upload is already ingesting or finished.", body = ErrorMessage, example = json!({"error": "the upload is already ingesting or finished"})),
        (status = 500, description = "The upload could not be cancelled.", body = ErrorMessage, example = json!({"error": "the upload could not be read or written"})),
    ),
)]
pub(crate) async fn delete_upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, UploadError> {
    let library = state.library_snapshot().await;
    let uploader = uploader(&state, &library, &headers).await?;
    let batch = owned_batch(&library, &uploader, &id).await?;
    let cancelled = cancel_batch(&library, &batch.id)
        .await
        .map_err(|err| failed("cancel batch", &err))?;
    if !cancelled {
        return Err(refused(
            StatusCode::CONFLICT,
            "the upload is already ingesting or finished",
        ));
    }
    let files_dir = state.config.read().await.paths().files_dir.clone();
    if let Err(err) = staging::remove_batch(&files_dir, &batch.id).await {
        warn!(batch = %batch.id, error = %err, "could not remove a cancelled upload's bytes");
    }
    let detail = json!({ "batch_id": batch.id.as_str() }).to_string();
    let _ = library
        .insert_security_audit_event(&uploader.actor, "upload_batch_cancelled", Some(&detail))
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Query of the review list.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub(crate) struct ReviewQuery {
    /// The `next_cursor` of the previous page.
    cursor: Option<String>,
}

/// One stored book to review.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ReviewItemJson {
    /// The upload.
    #[schema(example = "4f3c2b1a0e9d8c7b6a5f4e3d2c1b0a99")]
    upload_id: String,
    /// The file's number in it.
    #[schema(example = 0)]
    member_no: u32,
    /// The file's path in the upload.
    #[schema(example = "Unknown/track01.mp3")]
    path: String,
    /// The local edition it was stored under.
    #[schema(example = "6c1f2e8a-4b3d-4f5e-8a9b-0c1d2e3f4a5b")]
    edition_id: Option<String>,
    /// The patron who uploaded it.
    #[schema(example = 3)]
    user_id: i64,
}

/// One page of books to review.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ReviewPage {
    /// The books, oldest upload first.
    items: Vec<ReviewItemJson>,
    /// Pass as `cursor` for the next page; absent when this page is empty.
    #[schema(example = "4f3c2b1a0e9d8c7b6a5f4e3d2c1b0a99:0")]
    next_cursor: Option<String>,
}

/// Lists uploaded books stored under a new local edition because nothing
/// strong identified them, for an Administrator to check and, when they
/// are a known edition, pin.
#[utoipa::path(
    get,
    path = "/api/uploads/review",
    params(ReviewQuery),
    responses(
        (status = 200, description = "One page of books to review.", body = ReviewPage),
        (status = 400, description = "The cursor is not one this route made.", body = ErrorMessage, example = json!({"error": "invalid page cursor"})),
        (status = 401, description = "No one is signed in.", body = ErrorMessage, example = json!({"error": "sign in first"})),
        (status = 403, description = "The caller is not an Administrator.", body = ErrorMessage, example = json!({"error": "only an Administrator can review uploads"})),
        (status = 500, description = "The list could not be read.", body = ErrorMessage, example = json!({"error": "the upload could not be read or written"})),
    ),
)]
pub(crate) async fn list_upload_review(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<ReviewPage>, Response> {
    if let Err(refusal) = crate::auth::library_administrator(&state, &headers).await {
        return Err(refusal.answer(|status| {
            if status == StatusCode::UNAUTHORIZED {
                refused(status, "sign in first")
            } else {
                refused(status, "only an Administrator can review uploads")
            }
        }));
    }
    let after = match query.cursor.as_deref() {
        None => None,
        Some(raw) => Some(parse_review_cursor(raw).ok_or_else(|| {
            refused(StatusCode::BAD_REQUEST, "invalid page cursor").into_response()
        })?),
    };
    let library = state.library_snapshot().await;
    let items = review_items(&library, after.as_ref().map(|(id, no)| (id, *no)))
        .await
        .map_err(|err| failed("review items", &err).into_response())?;
    let next_cursor = items
        .last()
        .map(|item| format!("{}:{}", item.batch_id, item.member_no));
    Ok(Json(ReviewPage {
        items: items
            .into_iter()
            .map(|item| ReviewItemJson {
                upload_id: item.batch_id.as_str().to_string(),
                member_no: item.member_no,
                path: item.path,
                edition_id: item.edition_id,
                user_id: item.user_id,
            })
            .collect(),
        next_cursor,
    }))
}

/// Parses a review cursor (`<upload id>:<member number>`).
fn parse_review_cursor(raw: &str) -> Option<(IngestBatchId, u32)> {
    let (id, member) = raw.split_once(':')?;
    if !member.bytes().all(|b| b.is_ascii_digit()) || member.is_empty() {
        return None;
    }
    Some((IngestBatchId::parse(id)?, member.parse().ok()?))
}

/// Starts this node's upload prune loop: idle uploads are cancelled after
/// `uploads.idle_ttl_hours`, finished and orphaned staging folders are
/// removed, and finished uploads' rows go after a week.
pub(crate) fn spawn_prune(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + PRUNE_STARTUP_DELAY,
            PRUNE_INTERVAL,
        );
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(err) = prune_once(&state).await {
                warn!(error = %err, "upload prune failed");
            }
        }
    });
}

/// One pass of the prune loop.
///
/// # Returns
///
/// How many staging folders and rows it removed.
///
/// # Errors
///
/// Returns a library or I/O error; the pass stops at the first.
pub(crate) async fn prune_once(state: &AppState) -> anyhow::Result<u64> {
    let library = state.library_snapshot().await;
    let (files_dir, idle_ttl) = {
        let cfg = state.config.read().await;
        (cfg.paths().files_dir.clone(), cfg.uploads.idle_ttl())
    };
    let node_id = load_or_create_node_identity(&files_dir)?.node_id;
    let now = chrono::Utc::now();
    let mut removed = 0u64;
    let idle_before = now - chrono::Duration::from_std(idle_ttl)?;
    for id in idle_upload_batches(&library, &node_id, idle_before).await? {
        if cancel_batch(&library, &id).await? {
            staging::remove_batch(&files_dir, &id).await?;
            removed += 1;
        }
    }
    for id in final_batches_on(&library, &node_id).await? {
        if staging::batch_dir(&files_dir, &id).exists() {
            staging::remove_batch(&files_dir, &id).await?;
            removed += 1;
        }
    }
    for id in staging::staged_batches(&files_dir).await? {
        if !batch_exists(&library, &id).await? {
            staging::remove_batch(&files_dir, &id).await?;
            removed += 1;
        }
    }
    let finished_before = now - chrono::Duration::from_std(FINISHED_RETENTION)?;
    removed += u64::try_from(delete_finished_before(&library, finished_before).await?)?;
    Ok(removed)
}

#[cfg(test)]
mod tests;
