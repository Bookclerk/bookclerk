//! AWS S3 / MinIO storage backend.

use std::path::Path;
use std::time::SystemTime;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder;
use aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use bookclerk_config::OutputS3Config;
use bytes::Bytes;
use sea_orm::DatabaseConnection;

use crate::error::{Result, StorageError};
use crate::s3_credentials::{load_s3_credentials, S3Credentials};
use crate::traits::{ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend};

/// Below this size a single `PutObject` is enough. Above it, upload in fixed-size
/// parts so memory stays bounded and objects larger than the single-PUT limit
/// (5 GiB on AWS) still work.
pub(crate) const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Each part is read into a buffer this large at most. S3 requires 5 MiB minimum
/// per part except the last.
pub(crate) const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

/// S3-compatible object storage.
#[derive(Debug, Clone)]
pub struct S3Backend {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3Backend {
    /// Build from Bookclerk S3 output config.
    ///
    /// Credential resolution order:
    /// 1. `BOOKCLERK_AWS_ACCESS_KEY_ID` + `BOOKCLERK_AWS_SECRET_ACCESS_KEY` env override
    ///    (wins when both are set; empty string counts as set — intentional override).
    ///    Do NOT confuse with bare `AWS_*` which the SDK chain may use independently.
    /// 2. `encrypted_secrets` (`kind=s3`, `name=default`) when `db` is provided (sealed-v1)
    /// 3. AWS SDK default provider chain (`~/.aws/credentials`, SSO, EC2/ECS/EKS roles, etc.)
    ///
    /// `prefix` should already be the normalized destination prefix for this S3 plugin.
    pub async fn from_config(
        cfg: &OutputS3Config,
        prefix: &str,
        db: Option<&DatabaseConnection>,
    ) -> Result<Self> {
        let creds = resolve_s3_credentials(db).await?;
        Self::from_parts(cfg, prefix, creds.as_ref()).await
    }

    /// Build with explicit credentials (external output guests).
    pub async fn from_parts(
        cfg: &OutputS3Config,
        prefix: &str,
        creds: Option<&S3Credentials>,
    ) -> Result<Self> {
        if cfg.bucket.is_empty() {
            return Err(StorageError::S3("bucket must not be empty".into()));
        }

        let mut loader =
            aws_config::defaults(BehaviorVersion::latest()).region(Region::new(cfg.region.clone()));

        if let Some(creds) = creds {
            bookclerk_config::register_secret(&creds.access_key_id);
            bookclerk_config::register_secret(&creds.secret_access_key);
            if let Some(token) = &creds.session_token {
                bookclerk_config::register_secret(token);
            }
            loader = loader.credentials_provider(Credentials::new(
                creds.access_key_id.clone(),
                creds.secret_access_key.clone(),
                creds.session_token.clone(),
                None,
                "bookclerk-injected",
            ));
        }

        let shared = loader.load().await;
        let mut s3_config = aws_sdk_s3::config::Builder::from(&shared);

        if let Some(endpoint) = &cfg.endpoint {
            let endpoint = normalize_s3_endpoint(endpoint);
            if !endpoint.is_empty() {
                s3_config = s3_config.endpoint_url(endpoint);
            }
        }
        if cfg.force_path_style {
            s3_config = s3_config.force_path_style(true);
        }

        let client = Client::from_conf(s3_config.build());
        Ok(Self {
            client,
            bucket: cfg.bucket.clone(),
            prefix: crate::normalize_prefix(prefix),
        })
    }

    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{key}", self.prefix)
        }
    }

    async fn put_body(&self, key: &str, body: ByteStream, meta: ObjectMeta) -> Result<()> {
        let req = apply_meta_put(
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(self.full_key(key))
                .body(body),
            &meta,
        );

        req.send()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?;
        Ok(())
    }

    async fn put_file_multipart(&self, key: &str, path: &Path, meta: ObjectMeta) -> Result<()> {
        use tokio::io::AsyncReadExt;

        let full_key = self.full_key(key);
        let created = apply_meta_multipart(
            self.client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(&full_key),
            &meta,
        )
        .send()
        .await
        .map_err(|err| StorageError::S3(err.to_string()))?;

        let upload_id = created.upload_id().ok_or_else(|| {
            StorageError::S3("CreateMultipartUpload returned no upload id".into())
        })?;

        let upload = async {
            let mut file = tokio::fs::File::open(path).await?;
            let mut part_number: i32 = 1;
            let mut completed = Vec::new();
            let mut buffer = vec![0u8; MULTIPART_PART_SIZE];

            loop {
                let mut filled = 0usize;
                while filled < MULTIPART_PART_SIZE {
                    let n = file.read(&mut buffer[filled..]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    break;
                }

                let uploaded = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(&full_key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .body(ByteStream::from(Bytes::copy_from_slice(&buffer[..filled])))
                    .send()
                    .await
                    .map_err(|err| StorageError::S3(err.to_string()))?;

                let etag = uploaded.e_tag().ok_or_else(|| {
                    StorageError::S3(format!("UploadPart {part_number} returned no ETag"))
                })?;
                completed.push(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .e_tag(etag)
                        .build(),
                );
                part_number += 1;
            }

            if completed.is_empty() {
                return Err(StorageError::S3(format!(
                    "refusing empty multipart upload for {}",
                    path.display()
                )));
            }

            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&full_key)
                .upload_id(upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(completed))
                        .build(),
                )
                .send()
                .await
                .map_err(|err| StorageError::S3(err.to_string()))?;
            Ok(())
        };

        match upload.await {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Err(abort_err) = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(&full_key)
                    .upload_id(upload_id)
                    .send()
                    .await
                {
                    tracing::warn!(
                        key = %full_key,
                        upload_id,
                        error = %abort_err,
                        "failed to abort multipart upload after error"
                    );
                }
                Err(err)
            }
        }
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
        let mut meta = meta;
        if meta.content_length.is_none() {
            meta.content_length = Some(data.len() as u64);
        }
        self.put_body(key, data.into(), meta).await
    }

    async fn put_file(&self, key: &str, path: &Path, meta: ObjectMeta) -> Result<()> {
        let mut meta = meta;
        let len = match meta.content_length {
            Some(len) => len,
            None => {
                let stat = tokio::fs::metadata(path).await?;
                meta.content_length = Some(stat.len());
                stat.len()
            }
        };

        if use_multipart(len) {
            tracing::debug!(
                key,
                bytes = len,
                part_size = MULTIPART_PART_SIZE,
                "uploading large object via S3 multipart"
            );
            return self.put_file_multipart(key, path, meta).await;
        }

        let body = ByteStream::from_path(path)
            .await
            .map_err(|err| StorageError::S3(format!("failed to open {}: {err}", path.display())))?;
        self.put_body(key, body, meta).await
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
            .map_err(|err| {
                let msg = err.to_string();
                if msg.contains("NoSuchKey") || msg.contains("404") {
                    StorageError::NotFound(key.into())
                } else {
                    StorageError::S3(msg)
                }
            })?;
        let data = out
            .body
            .collect()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?
            .into_bytes();
        Ok(data)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        match self.probe(key).await {
            Ok(_) => Ok(true),
            Err(StorageError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let full_prefix = self.full_key(prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&full_prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req
                .send()
                .await
                .map_err(|err| StorageError::S3(err.to_string()))?;

            for obj in resp.contents() {
                let Some(raw_key) = obj.key() else { continue };
                let key = raw_key
                    .strip_prefix(&self.prefix)
                    .unwrap_or(raw_key)
                    .to_string();
                out.push(ObjectInfo {
                    key,
                    size: obj.size().unwrap_or(0) as u64,
                });
            }

            if resp.is_truncated().unwrap_or(false) {
                token = resp.next_continuation_token().map(str::to_string);
            } else {
                break;
            }
        }

        Ok(out)
    }

    async fn probe(&self, key: &str) -> Result<ObjectProbe> {
        let out = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
            .map_err(|err| {
                let msg = err.to_string();
                if msg.contains("NotFound") || msg.contains("404") || msg.contains("NoSuchKey") {
                    StorageError::NotFound(key.into())
                } else {
                    StorageError::S3(msg)
                }
            })?;

        let user_meta = out.metadata();
        let meta = ObjectMeta {
            content_type: out.content_type().map(str::to_string),
            content_length: out.content_length().map(|n| n as u64),
            asin: meta_get(user_meta, "asin"),
            title: meta_get(user_meta, "title"),
            creation_time: meta_get(user_meta, "creation-time"),
            last_write_time: meta_get(user_meta, "last-write-time"),
        };
        Ok(ObjectProbe {
            key: key.to_string(),
            size: meta.content_length.unwrap_or(0),
            content_type: meta.content_type.clone(),
            meta,
        })
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        // Server-side copy — no object body download. MetadataDirective::COPY
        // preserves x-amz-meta-* written at acquire time.
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(self.full_key(to))
            .copy_source(format!("{}/{}", self.bucket, self.full_key(from)))
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Copy)
            .send()
            .await
            .map_err(|err| {
                let msg = err.to_string();
                if msg.contains("NoSuchKey") || msg.contains("404") || msg.contains("NotFound") {
                    StorageError::NotFound(from.into())
                } else {
                    StorageError::S3(msg)
                }
            })?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?;
        Ok(())
    }

    async fn touch_file(
        &self,
        key: &str,
        created: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<()> {
        // Logical times are already written on PutObject as x-amz-meta-*.
        // Do not CopyObject (second full-size version on versioned buckets) and
        // do not PutObjectTagging: Backblaze B2's S3 API accepts tagging calls
        // but stores the Tagging XML as a new object body, destroying media.
        let _ = (key, created, modified);
        Ok(())
    }
}

/// True when a file should be uploaded in parts rather than one `PutObject`.
#[must_use]
pub(crate) fn use_multipart(content_length: u64) -> bool {
    content_length >= MULTIPART_THRESHOLD
}

fn apply_meta_put(mut req: PutObjectFluentBuilder, meta: &ObjectMeta) -> PutObjectFluentBuilder {
    if let Some(ct) = &meta.content_type {
        req = req.content_type(ct.clone());
    }
    if let Some(len) = meta.content_length {
        req = req.content_length(len as i64);
    }
    if let Some(asin) = &meta.asin {
        req = req.metadata("asin", asin.clone());
    }
    if let Some(title) = &meta.title {
        req = req.metadata("title", title.clone());
    }
    if let Some(created) = &meta.creation_time {
        req = req.metadata("creation-time", created.clone());
    }
    if let Some(modified) = &meta.last_write_time {
        req = req.metadata("last-write-time", modified.clone());
        if let Some(secs) = rfc3339_unix_secs(modified) {
            req = req.metadata("mtime", secs.to_string());
        }
    }
    req
}

fn apply_meta_multipart(
    mut req: CreateMultipartUploadFluentBuilder,
    meta: &ObjectMeta,
) -> CreateMultipartUploadFluentBuilder {
    if let Some(ct) = &meta.content_type {
        req = req.content_type(ct.clone());
    }
    if let Some(asin) = &meta.asin {
        req = req.metadata("asin", asin.clone());
    }
    if let Some(title) = &meta.title {
        req = req.metadata("title", title.clone());
    }
    if let Some(created) = &meta.creation_time {
        req = req.metadata("creation-time", created.clone());
    }
    if let Some(modified) = &meta.last_write_time {
        req = req.metadata("last-write-time", modified.clone());
        if let Some(secs) = rfc3339_unix_secs(modified) {
            req = req.metadata("mtime", secs.to_string());
        }
    }
    req
}

/// Prepend `https://` when `endpoint` looks like a bare hostname (no scheme).
#[must_use]
pub(crate) fn normalize_s3_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

fn rfc3339_unix_secs(raw: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp().max(0) as u64)
}

fn meta_get(map: Option<&std::collections::HashMap<String, String>>, key: &str) -> Option<String> {
    map.and_then(|m| {
        m.get(key)
            .cloned()
            .or_else(|| m.get(&key.to_ascii_lowercase()).cloned())
    })
}

/// Resolve S3 credentials for the in-process backend (env → DB → SDK chain).
pub(crate) async fn resolve_s3_credentials(
    db: Option<&DatabaseConnection>,
) -> Result<Option<S3Credentials>> {
    if let (Ok(access), Ok(secret)) = (
        std::env::var(crate::s3_credentials::ENV_AWS_ACCESS_KEY_ID),
        std::env::var(crate::s3_credentials::ENV_AWS_SECRET_ACCESS_KEY),
    ) {
        let session = std::env::var(crate::s3_credentials::ENV_AWS_SESSION_TOKEN).ok();
        return Ok(Some(S3Credentials {
            access_key_id: access,
            secret_access_key: secret,
            session_token: session,
            label: None,
        }));
    }
    if let Some(db) = db {
        return load_s3_credentials(db).await;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_bare_hostname_endpoint() {
        assert_eq!(
            normalize_s3_endpoint("minio.example.com:9000"),
            "https://minio.example.com:9000"
        );
        assert_eq!(
            normalize_s3_endpoint("http://minio:9000"),
            "http://minio:9000"
        );
        assert_eq!(normalize_s3_endpoint("   "), "");
    }

    #[test]
    fn multipart_is_used_from_one_hundred_mebibytes_up() {
        assert!(!use_multipart(MULTIPART_THRESHOLD - 1));
        assert!(use_multipart(MULTIPART_THRESHOLD));
        assert!(use_multipart(5 * 1024 * 1024 * 1024));
    }
}
