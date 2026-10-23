//! Self-service profile (display name, email, avatar) for first-party users.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::api::AppState;
use crate::auth;

/// Maximum uploaded avatar payload (bytes).
const AVATAR_MAX_BYTES: usize = 1_500_000;
/// Filename extensions (and Content-Type) accepted for stored avatars.
const AVATAR_KINDS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("webp", "image/webp"),
];
/// Maximum display-name length after trim.
const DISPLAY_NAME_MAX: usize = 80;

/// JSON body for `PATCH /api/auth/profile`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PatchProfileRequest {
    /// Replacement display name; empty or whitespace clears the field.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Replacement email; empty or whitespace clears the field.
    #[serde(default)]
    pub email: Option<String>,
    /// Picture choice (`auto`, `monogram`, `gravatar`, `upload`, or `sso:{id}`).
    #[serde(default)]
    pub avatar_source: Option<String>,
}

/// Sniffed avatar kind: filename extension.
struct AvatarKind {
    /// File extension without a leading dot (`png`, `jpg`, `webp`).
    ext: &'static str,
}

/// `{files_dir}/avatars` join used as the containment root for avatar I/O.
fn avatars_dir(files_dir: &Path) -> PathBuf {
    files_dir.join("avatars")
}

/// Absolute path of a stored avatar for `user_id` with `ext`.
///
/// `user_id` is numeric and `ext` must be an allowlisted kind, so the file name
/// cannot contain path separators. The trusted operator `files_dir` is used as
/// configured (including relative/`..` components); containment is the
/// `avatars/` join plus `starts_with` on that directory.
fn avatar_path_with_ext(files_dir: &Path, user_id: i64, ext: &str) -> Option<PathBuf> {
    if !AVATAR_KINDS.iter().any(|(kind, _)| *kind == ext) {
        return None;
    }
    let dir = avatars_dir(files_dir);
    let path = dir.join(format!("{user_id}.{ext}"));
    if path.starts_with(&dir) {
        Some(path)
    } else {
        None
    }
}

/// Stored avatar path and content type when a file exists.
fn existing_avatar(files_dir: &Path, user_id: i64) -> Option<(PathBuf, &'static str)> {
    let dir = avatars_dir(files_dir);
    let Ok(dir_canon) = dir.canonicalize() else {
        return None;
    };
    for (ext, content_type) in AVATAR_KINDS {
        let Some(path) = avatar_path_with_ext(files_dir, user_id, ext) else {
            continue;
        };
        let Some(name) = path.file_name() else {
            continue;
        };
        let candidate = dir_canon.join(name);
        if !candidate.starts_with(&dir_canon) {
            continue;
        }
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => continue,
            Ok(meta) if meta.is_file() => {}
            _ => continue,
        }
        return Some((candidate, *content_type));
    }
    None
}

/// True when a stored avatar file exists for `user_id`.
pub(crate) fn avatar_exists(files_dir: &Path, user_id: i64) -> bool {
    existing_avatar(files_dir, user_id).is_some()
}

/// Best-effort delete of a stored avatar; missing files are ignored.
///
/// Unlinks the intended directory entry under the canonical `avatars/` dir
/// without following a leaf symlink (so `7.png -> 8.png` removes the link,
/// not user 8's bytes; `7.png -> /outside` removes the link, not the target).
pub(crate) fn remove_avatar(files_dir: &Path, user_id: i64) {
    let dir = avatars_dir(files_dir);
    let Ok(dir_canon) = dir.canonicalize() else {
        return;
    };
    for (ext, _) in AVATAR_KINDS {
        let leaf = format!("{user_id}.{ext}");
        if leaf.contains("..") || leaf.contains('/') || leaf.contains('\\') {
            continue;
        }
        let path = dir_canon.join(&leaf);
        // Parent is already canonical; lexical `starts_with` confines the join
        // before we unlink the leaf entry (not its resolved target).
        if !path.starts_with(&dir_canon) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(error = %err, user_id, "failed to remove profile avatar");
            }
        }
    }
}

/// Unique same-directory staging path for an atomic avatar replace.
fn staging_avatar_path(dir: &Path, dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("avatar.bin");
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nonce: u64 = rand::random();
    dir.join(format!(
        ".{name}.tmp-{}-{nonce:016x}-{n}",
        std::process::id()
    ))
}

/// Replace `to` with staging file `from` without following a leaf symlink at `to`.
///
/// Unix `rename` replaces the directory entry. Windows `std::fs::rename` cannot
/// replace an existing file, so this uses `MoveFileExW(REPLACE_EXISTING |
/// WRITE_THROUGH)` (same pattern as `bookclerk-config`).
fn replace_avatar_entry(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        avatar_replace_windows(from, to)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // MoveFileExW FFI — same boundary as bookclerk-config.
fn avatar_replace_windows(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let src = wide(from);
    let dst = wide(to);
    // SAFETY: `src` and `dst` are NUL-terminated wide paths that outlive the call.
    let ok = unsafe {
        MoveFileExW(
            src.as_ptr(),
            dst.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Write `bytes` to `dest` via same-dir temp + atomic replace.
///
/// `dir` must already be the canonical `avatars/` directory. A pre-existing
/// leaf symlink at `dest` is replaced as a directory entry (not followed).
/// Staging is created with `create_new` so an unexpected existing entry is not
/// truncated or followed; cleanup only runs after this open succeeds.
fn write_avatar_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if !dest.starts_with(dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "avatar destination escapes avatars directory",
        ));
    }
    let staging = staging_avatar_path(dir, dest);
    if !staging.starts_with(dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "avatar staging path escapes avatars directory",
        ));
    }
    use std::fs::OpenOptions;
    use std::io::Write;
    // Exclusive create: do not truncate/follow an unexpected existing entry.
    // `?` returns before any cleanup, so open failure never unlinks a foreign path.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    if let Err(err) = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })() {
        let _ = std::fs::remove_file(&staging);
        return Err(err);
    }
    match replace_avatar_entry(&staging, dest) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&staging);
            Err(err)
        }
    }
}

/// Files directory from live config, when [`bookclerk_config::Config::load`] populated paths.
pub(crate) async fn files_dir(state: &AppState) -> Option<PathBuf> {
    let cfg = state.config.read().await;
    cfg.paths.as_ref().map(|p| p.files_dir.clone())
}

/// SHA-256 Gravatar digest when `email` is non-empty.
pub(crate) fn gravatar_hash_for(email: Option<&str>) -> Option<String> {
    email
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(bookclerk_library::gravatar_hash)
}

/// Wire `avatar_source` (`auto` when the column is unset).
pub(crate) fn avatar_source_wire(source: Option<&str>) -> String {
    source
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| String::from("auto"))
}

/// IdP pictures as JSON objects for `/me` and user lists.
pub(crate) fn sso_pictures_json(
    pictures: &[bookclerk_library::UserSsoPicture],
) -> Vec<serde_json::Value> {
    pictures
        .iter()
        .map(|p| {
            serde_json::json!({
                "identity_id": p.identity_id,
                "provider": p.provider,
                "picture_url": p.picture_url,
                "last_used_at": p.last_used_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect()
}

/// Profile snapshot including resolved avatar choices.
pub(crate) async fn profile_user_json(
    library: &bookclerk_library::LibraryStore,
    user: &bookclerk_library::UserRecord,
    files: Option<&Path>,
) -> serde_json::Value {
    let pictures = library
        .list_user_sso_pictures(user.id)
        .await
        .unwrap_or_default();
    serde_json::json!({
        "id": user.id,
        "role": user.role.as_str(),
        "display_name": user.display_name,
        "email": user.email,
        "has_password": user.has_password,
        "has_avatar": files.is_some_and(|d| avatar_exists(d, user.id)),
        "avatar_source": avatar_source_wire(user.avatar_source.as_deref()),
        "gravatar_hash": gravatar_hash_for(user.email.as_deref()),
        "sso_pictures": sso_pictures_json(&pictures),
    })
}

/// Resolves the signed-in first-party user; impersonation and operator-only sessions are refused.
pub(crate) async fn require_self_user_id(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<i64, StatusCode> {
    let auth = state.auth_snapshot().await;
    if let Some(op) = auth::resolve_operator_session(state, &auth, headers).await {
        if op.impersonating_user_id.is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        return op.elevated_from_user_id.ok_or(StatusCode::FORBIDDEN);
    }
    let library = state.library_snapshot().await;
    let identity = auth::timed_portal_identity_from_headers(&library, headers)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    identity.user_id.ok_or(StatusCode::FORBIDDEN)
}

/// Updates the caller's display name and/or email.
#[utoipa::path(
    patch,
    path = "/api/auth/profile",
    request_body = PatchProfileRequest,
    responses(
        (status = 200, description = "The profile after the change.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "A value is invalid."),
    ),
)]
pub async fn patch_profile(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PatchProfileRequest>,
) -> Result<Response, StatusCode> {
    if body.display_name.is_none() && body.email.is_none() && body.avatar_source.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let user_id = require_self_user_id(&state, &headers).await?;
    let library = state.library_snapshot().await;
    let mut user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if let Some(raw) = body.display_name.as_deref() {
        let trimmed = raw.trim();
        if trimmed.len() > DISPLAY_NAME_MAX {
            return Ok(json_error(
                StatusCode::BAD_REQUEST,
                "display_name_too_long",
                "Display name must be 80 characters or fewer.",
            ));
        }
        user = library
            .set_user_display_name(user_id, Some(trimmed))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    if let Some(raw) = body.email.as_deref() {
        user = match library.set_user_email(user_id, Some(raw)).await {
            Ok(u) => u,
            Err(bookclerk_library::LibraryError::InvalidEmail) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_email",
                    "Enter a valid email address.",
                ));
            }
            Err(err) if is_unique_violation(&err) => {
                return Ok(json_error(
                    StatusCode::CONFLICT,
                    "email_in_use",
                    "That email is already used by another account.",
                ));
            }
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        };
    }

    if let Some(raw) = body.avatar_source.as_deref() {
        let parsed = match parse_avatar_source(&library, user_id, raw).await {
            Ok(value) => value,
            Err(StatusCode::BAD_REQUEST) => {
                return Ok(json_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_avatar_source",
                    "Choose a valid picture source.",
                ));
            }
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        };
        user = library
            .set_user_avatar_source(user_id, parsed.as_deref())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let _ = library
        .insert_security_audit_event(
            &format!("user:{user_id}"),
            "profile_patch",
            Some(&format!(r#"{{"user_id":{user_id}}}"#)),
        )
        .await;

    let files = files_dir(&state).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "user": profile_user_json(&library, &user, files.as_deref()).await,
    }))
    .into_response())
}

/// Accepts a JPEG/PNG/WebP upload and stores it under `files_dir/avatars`.
#[utoipa::path(
    put,
    path = "/api/auth/profile/avatar",
    request_body(content = crate::openapi::BinaryBody, content_type = "image/*"),
    responses(
        (status = 200, description = "The avatar was stored.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
        (status = 400, description = "The image is not JPEG, PNG or WebP."),
        (status = 413, description = "The image is too large."),
    ),
)]
pub async fn put_avatar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, StatusCode> {
    let user_id = require_self_user_id(&state, &headers).await?;
    if body.len() > AVATAR_MAX_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let files = files_dir(&state)
        .await
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let kind = sniff_avatar(&body).ok_or(StatusCode::BAD_REQUEST)?;
    let dir = avatars_dir(&files);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Prefer the canonical avatars directory once it exists so writes cannot
    // follow a replaced leaf symlink under a non-canonical join.
    let dir = dir
        .canonicalize()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    remove_avatar(&files, user_id);
    let leaf = format!("{user_id}.{}", kind.ext);
    // Single-component leaf: reject separators / `..` before join (CodeQL
    // DotDotCheck + lexical deny). `user_id` is i64 so this is belt-and-braces.
    if leaf.contains("..") || leaf.contains('/') || leaf.contains('\\') {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let dest = dir.join(&leaf);
    if !dest.starts_with(&dir) {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    // Same-dir temp + atomic replace so a leftover leaf symlink is overwritten
    // as a directory entry instead of followed (see `write_avatar_atomic`).
    // Run on a blocking pool so sync_all cannot stall a Tokio worker.
    let dir_c = dir.clone();
    let dest_c = dest.clone();
    let bytes = body.to_vec();
    tokio::task::spawn_blocking(move || write_avatar_atomic(&dir_c, &dest_c, &bytes))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let library = state.library_snapshot().await;
    let _ = library
        .set_user_avatar_source(user_id, Some("upload"))
        .await;
    let _ = library
        .insert_security_audit_event(
            &format!("user:{user_id}"),
            "profile_avatar_put",
            Some(&format!(r#"{{"user_id":{user_id}}}"#)),
        )
        .await;

    Ok(
        Json(serde_json::json!({ "ok": true, "has_avatar": true, "avatar_source": "upload" }))
            .into_response(),
    )
}

/// Removes the caller's stored avatar, if any.
#[utoipa::path(
    delete,
    path = "/api/auth/profile/avatar",
    responses(
        (status = 200, description = "The avatar was removed.", body = serde_json::Value),
        (status = 401, description = "The caller is not signed in."),
        (status = 403, description = "The caller may not do this."),
    ),
)]
pub async fn delete_avatar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let user_id = require_self_user_id(&state, &headers).await?;
    let files = files_dir(&state)
        .await
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    remove_avatar(&files, user_id);
    let library = state.library_snapshot().await;
    if let Ok(Some(user)) = library.get_user(user_id).await {
        if user.avatar_source.as_deref() == Some("upload") {
            let _ = library.set_user_avatar_source(user_id, None).await;
        }
    }
    let _ = library
        .insert_security_audit_event(
            &format!("user:{user_id}"),
            "profile_avatar_delete",
            Some(&format!(r#"{{"user_id":{user_id}}}"#)),
        )
        .await;
    Ok(Json(serde_json::json!({ "ok": true, "has_avatar": false })).into_response())
}

/// Serves a stored avatar JPEG for any authenticated household member.
#[utoipa::path(
    get,
    path = "/api/users/{id}/avatar",
    params(("id" = i64, Path, description = "User id.")),
    responses(
        (status = 200, description = "The avatar.", content_type = "image/jpeg"),
        (status = 404, description = "The user has no avatar."),
    ),
)]
pub async fn get_avatar(
    State(state): State<Arc<AppState>>,
    AxumPath(user_id): AxumPath<i64>,
) -> Result<Response, StatusCode> {
    let library = state.library_snapshot().await;
    let _user = library
        .get_user(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let files = files_dir(&state).await.ok_or(StatusCode::NOT_FOUND)?;
    let dir = avatars_dir(&files);
    let Ok(dir_canon) = dir.canonicalize() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let mut found: Option<(PathBuf, &'static str)> = None;
    for (ext, content_type) in AVATAR_KINDS {
        let leaf = format!("{user_id}.{ext}");
        if leaf.contains("..") || leaf.contains('/') || leaf.contains('\\') {
            continue;
        }
        let candidate = dir_canon.join(&leaf);
        if !candidate.starts_with(&dir_canon) {
            continue;
        }
        // Serve the owned directory entry only — refuse peer/outside leaf links.
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => continue,
            Ok(meta) if meta.is_file() => {}
            _ => continue,
        }
        found = Some((candidate, *content_type));
        break;
    }
    let (path, content_type) = found.ok_or(StatusCode::NOT_FOUND)?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "private, max-age=60"),
        ],
        bytes,
    )
        .into_response())
}

/// Identifies JPEG, PNG, or WebP from magic bytes.
fn sniff_avatar(bytes: &[u8]) -> Option<AvatarKind> {
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some(AvatarKind { ext: "jpg" });
    }
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(AvatarKind { ext: "png" });
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some(AvatarKind { ext: "webp" });
    }
    None
}

/// Parses `auto` / `monogram` / `gravatar` / `upload` / `sso:{id}` for the caller.
async fn parse_avatar_source(
    library: &bookclerk_library::LibraryStore,
    user_id: i64,
    raw: &str,
) -> Result<Option<String>, StatusCode> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "monogram" | "gravatar" | "upload" => Ok(Some(lower)),
        s if s.starts_with("sso:") => {
            let id: i64 = s[4..].parse().map_err(|_| StatusCode::BAD_REQUEST)?;
            let ident = library
                .get_portal_identity_by_id(id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::BAD_REQUEST)?;
            if ident.user_id != Some(user_id) {
                return Err(StatusCode::BAD_REQUEST);
            }
            if ident
                .picture_url
                .as_deref()
                .map(str::trim)
                .is_none_or(|u| u.is_empty())
            {
                return Err(StatusCode::BAD_REQUEST);
            }
            Ok(Some(format!("sso:{id}")))
        }
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

/// True when a SeaORM/SQLite unique index rejected the write (typically email).
fn is_unique_violation(err: &bookclerk_library::LibraryError) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("unique") || text.contains("constraint")
}

/// JSON error body with a machine `error` slug and human `message`.
fn json_error(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": error, "message": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use bookclerk_config::Paths;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::auth::tests::{phase2_harness, portal_cookie_for};

    /// 1×1 PNG so decode tests do not depend on an image crate.
    fn tiny_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xfe,
            0xd4, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn sniff_avatar_recognizes_png_magic() {
        let png = tiny_png();
        assert_eq!(sniff_avatar(&png).map(|k| k.ext), Some("png"));
        assert!(sniff_avatar(b"not-an-image").is_none());
    }

    #[test]
    fn avatar_paths_stay_under_avatars_dir() {
        let dir = PathBuf::from("/tmp/bookclerk-files");
        let path = avatar_path_with_ext(&dir, 7, "png").expect("png");
        assert!(path.starts_with(dir.join("avatars")));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("7.png"));
        assert!(avatar_path_with_ext(&dir, 7, "exe").is_none());
        // Trusted files_dir is used verbatim (including `..`); the leaf name is
        // still numeric + allowlisted and stays under that dir's `avatars/`.
        let nested = avatar_path_with_ext(Path::new("/tmp/bookclerk-files/../data"), 7, "png")
            .expect("verbatim files_dir");
        assert_eq!(
            nested,
            PathBuf::from("/tmp/bookclerk-files/../data/avatars/7.png")
        );
    }

    #[test]
    fn existing_avatar_refuses_leaf_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let files = tmp.path();
        let avatars = files.join("avatars");
        std::fs::create_dir_all(&avatars).expect("avatars");
        let outside = tmp.path().join("outside.png");
        std::fs::write(&outside, b"nope").expect("outside");
        let link = avatars.join("7.png");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).expect("symlink");
            assert!(existing_avatar(files, 7).is_none());
        }
        #[cfg(not(unix))]
        {
            let _ = (outside, link);
        }
    }

    /// Outside target bytes must survive remove + atomic replace of a leaf symlink.
    #[cfg(unix)]
    #[test]
    fn remove_and_replace_leave_outside_symlink_target_intact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let files = tmp.path();
        let avatars = files.join("avatars");
        std::fs::create_dir_all(&avatars).expect("avatars");
        let outside = tmp.path().join("outside.png");
        std::fs::write(&outside, b"victim-bytes").expect("outside");
        let link = avatars.join("7.png");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");

        remove_avatar(files, 7);
        assert!(
            !link
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink()),
            "remove_avatar must unlink the leaf entry"
        );
        assert_eq!(
            std::fs::read(&outside).expect("outside after remove"),
            b"victim-bytes"
        );

        // Re-plant the escape and prove atomic replace does not truncate outside.
        std::os::unix::fs::symlink(&outside, &link).expect("re-symlink");
        let dir = avatars.canonicalize().expect("avatars canon");
        let dest = dir.join("7.png");
        let png = tiny_png();
        write_avatar_atomic(&dir, &dest, &png).expect("atomic write");
        assert_eq!(
            std::fs::read(&outside).expect("outside after write"),
            b"victim-bytes",
            "write must not follow the leaf symlink"
        );
        assert!(dest.is_file(), "dest must be a regular file after replace");
        assert!(!dest
            .symlink_metadata()
            .expect("dest meta")
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&dest).expect("new avatar"), png);
    }

    /// Dangling outside link: remove/replace must not create the missing target.
    #[cfg(unix)]
    #[test]
    fn remove_and_replace_refuse_dangling_outside_link() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let files = tmp.path();
        let avatars = files.join("avatars");
        std::fs::create_dir_all(&avatars).expect("avatars");
        let missing = tmp.path().join("missing-outside.png");
        let link = avatars.join("7.png");
        std::os::unix::fs::symlink(&missing, &link).expect("dangling symlink");

        remove_avatar(files, 7);
        assert!(!link.exists() && link.symlink_metadata().is_err());
        assert!(!missing.exists(), "remove must not create dangling target");

        std::os::unix::fs::symlink(&missing, &link).expect("re-symlink");
        let dir = avatars.canonicalize().expect("avatars canon");
        let dest = dir.join("7.png");
        let png = tiny_png();
        write_avatar_atomic(&dir, &dest, &png).expect("atomic write");
        assert!(!missing.exists(), "write must not create dangling target");
        assert_eq!(std::fs::read(&dest).expect("new avatar"), png);
    }

    /// Cross-user link: deleting/replacing user 7 must not delete user 8's bytes.
    #[cfg(unix)]
    #[test]
    fn remove_and_replace_leave_peer_avatar_intact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let files = tmp.path();
        let avatars = files.join("avatars");
        std::fs::create_dir_all(&avatars).expect("avatars");
        let peer = avatars.join("8.png");
        std::fs::write(&peer, b"user-8-bytes").expect("peer");
        let link = avatars.join("7.png");
        std::os::unix::fs::symlink(&peer, &link).expect("cross-user symlink");

        remove_avatar(files, 7);
        assert!(link.symlink_metadata().is_err(), "user 7 link removed");
        assert_eq!(
            std::fs::read(&peer).expect("peer after remove"),
            b"user-8-bytes"
        );

        std::os::unix::fs::symlink(&peer, &link).expect("re-symlink");
        let dir = avatars.canonicalize().expect("avatars canon");
        let dest = dir.join("7.png");
        let png = tiny_png();
        write_avatar_atomic(&dir, &dest, &png).expect("atomic write");
        assert_eq!(
            std::fs::read(&peer).expect("peer after write"),
            b"user-8-bytes"
        );
        assert_eq!(std::fs::read(&dest).expect("user 7 avatar"), png);
    }

    #[tokio::test]
    async fn member_can_patch_own_profile() {
        let (_state, app, library) = phase2_harness("op-token-profile").await;
        let cookie = portal_cookie_for(&library, "test", "member-ext").await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/auth/profile")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(
                        r#"{"display_name":"Alex","email":"alex@example.com"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
            .unwrap();
        assert!(body.contains("Alex"));
        assert!(body.contains("alex@example.com"));
        assert!(body.contains("6db61e6dcbcf2390e4a46af426f26a133a3bee45021422fc7ae86e9136f14110"));
        assert!(!body.contains("User #"));
    }

    #[tokio::test]
    async fn member_can_set_avatar_source() {
        let (_state, app, library) = phase2_harness("op-token-profile-avatar-src").await;
        let cookie = portal_cookie_for(&library, "test", "member-ext").await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/auth/profile")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"avatar_source":"monogram"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec())
            .unwrap();
        assert!(body.contains("\"avatar_source\":\"monogram\""));
    }

    #[tokio::test]
    async fn incomplete_email_is_rejected() {
        let (_state, app, library) = phase2_harness("op-token-profile-bad-email").await;
        let cookie = portal_cookie_for(&library, "test", "member-ext").await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/auth/profile")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"email":"alex.doe@gmail"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn duplicate_email_is_conflict() {
        let (_state, app, library) = phase2_harness("op-token-profile-email").await;
        let owner = library
            .get_portal_identity("test", "admin-ext")
            .await
            .unwrap()
            .unwrap();
        library
            .set_user_email(owner.user_id.unwrap(), Some("taken@example.com"))
            .await
            .unwrap();
        let cookie = portal_cookie_for(&library, "test", "member-ext").await;
        let res = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/auth/profile")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(r#"{"email":"taken@example.com"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn impersonation_cannot_patch_profile() {
        let (state, app, library) = phase2_harness("op-token-profile-imp").await;
        let member = library
            .get_portal_identity("test", "member-ext")
            .await
            .unwrap()
            .unwrap();
        let user_id = member.user_id.expect("bridged");
        let op_cookie = crate::auth::operator_session_cookie(&state).await;
        let imp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/impersonate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::from(format!(r#"{{"user_id":{user_id}}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(imp.status(), StatusCode::OK);
        let denied = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/auth/profile")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::from(r#"{"display_name":"Nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn operator_only_cannot_patch_profile() {
        let (state, app, _library) = phase2_harness("op-token-profile-op").await;
        let op_cookie = crate::auth::operator_session_cookie(&state).await;
        let denied = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/auth/profile")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::COOKIE, &op_cookie)
                    .body(Body::from(r#"{"display_name":"Nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn avatar_put_get_delete() {
        let (state, app, library) = phase2_harness("op-token-profile-av").await;
        let tmp = tempfile::tempdir().expect("tmp");
        {
            let mut cfg = state.config.write().await;
            cfg.paths = Some(Paths::from_files_dir(tmp.path().to_path_buf()));
        }
        let cookie = portal_cookie_for(&library, "test", "member-ext").await;
        let member = library
            .get_portal_identity("test", "member-ext")
            .await
            .unwrap()
            .unwrap();
        let user_id = member.user_id.expect("bridged");

        let put = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/auth/profile/avatar")
                    .header(header::CONTENT_TYPE, "image/png")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(tiny_png()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::OK,);

        let get = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/users/{user_id}/avatar"))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(
            get.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        let png = get.into_body().collect().await.unwrap().to_bytes();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));

        let del = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/auth/profile/avatar")
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del.status(), StatusCode::OK);

        let missing = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/users/{user_id}/avatar"))
                    .header(header::COOKIE, &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
