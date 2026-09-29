//! AWS S3 / MinIO storage backend.

#![allow(clippy::missing_docs_in_private_items)]

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
use sha2::Digest;

use crate::error::{Result, StorageError};
use crate::s3_credentials::{load_s3_credentials, S3Credentials};
use crate::traits::{ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend};

/// Below this size a single `PutObject` is enough. Above it, upload in fixed-size
/// parts so memory stays bounded and objects larger than the single-PUT limit
/// (5 GiB on AWS) still work.
pub(crate) const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Each part is read into a buffer this large at most. S3 requires 5 MiB minimum
/// per part except the last. `8 MiB * S3_MAX_PARTS` is the application object ceiling.
pub(crate) const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

/// AWS multipart upload part-count ceiling.
pub(crate) const S3_MAX_PARTS: i32 = 10_000;

/// S3-compatible object storage.
#[derive(Debug, Clone)]
pub struct S3Backend {
    /// AWS SDK S3 client (endpoint/path-style already applied).
    client: Client,
    /// Target bucket; empty is rejected at construction.
    bucket: String,
    /// Normalized key prefix prepended to every object key.
    prefix: String,
    /// Region used in [`StorageBackend::instance_id`].
    region: String,
    /// Endpoint URL used in [`StorageBackend::instance_id`] (empty for AWS default).
    endpoint: String,
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
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn from_config(
        cfg: &OutputS3Config,
        prefix: &str,
        db: Option<&DatabaseConnection>,
    ) -> Result<Self> {
        let creds = resolve_s3_credentials(db).await?;
        Self::from_parts(cfg, prefix, creds.as_ref()).await
    }

    /// Build with explicit credentials (external output guests).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
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
        #[cfg(any(unix, windows))]
        if crate::s3_http::socket_proxy_enabled() {
            let http = crate::s3_http::socket_proxy_http_client()?;
            s3_config = s3_config.http_client(http);
        }

        let client = Client::from_conf(s3_config.build());
        let backend = Self {
            client,
            bucket: cfg.bucket.clone(),
            prefix: crate::normalize_prefix(prefix),
            region: cfg.region.clone(),
            endpoint: cfg
                .endpoint
                .as_deref()
                .map(normalize_s3_endpoint)
                .unwrap_or_default(),
        };
        if let Some(dir) = orphan_dir() {
            if let Err(err) = backend.retry_orphans(&dir).await {
                tracing::warn!(error = %err, "multipart orphan retry failed");
            }
        }
        Ok(backend)
    }

    /// Prepends the destination prefix to `key` (no-op when the prefix is empty).
    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{key}", self.prefix)
        }
    }

    /// Single `PutObject` with Bookclerk metadata headers.
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

    /// Streams a local file as fixed-size parts (used above [`MULTIPART_THRESHOLD`]).
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

    /// Streams `body` as multipart parts (bounded window; no full-object buffer).
    ///
    /// Returns `(bytes, sha256 hex)`. A dropped future aborts the upload via
    /// [`MultipartGuard`]. Failed aborts leave the upload id in the orphan
    /// directory when `BOOKCLERK_FILES_DIR` is set.
    async fn put_stream_multipart(
        &self,
        key: &str,
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        mut meta: ObjectMeta,
    ) -> Result<(u64, String)> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncReadExt;

        if let Some(expected) = meta.sha256_hex.as_deref() {
            let _ = crate::bounded::parse_sha256_hex(expected)?;
        }
        if let Some(len) = meta.content_length {
            if len > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES {
                return Err(StorageError::PayloadTooLarge(format!(
                    "object length {len} exceeds {}",
                    crate::bounded::MAX_SUPPORTED_OBJECT_BYTES
                )));
            }
        }

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

        let upload_id = created
            .upload_id()
            .ok_or_else(|| StorageError::S3("CreateMultipartUpload returned no upload id".into()))?
            .to_string();
        record_orphan(&self.bucket, &full_key, &upload_id);
        let mut guard = MultipartGuard::arm(
            self.client.clone(),
            self.bucket.clone(),
            full_key.clone(),
            upload_id.clone(),
        );

        let upload = async {
            let mut part_number: i32 = 1;
            let mut completed = Vec::new();
            let mut buffer = vec![0u8; MULTIPART_PART_SIZE];
            let mut total = 0u64;
            let mut hasher = Sha256::new();

            loop {
                let mut filled = 0usize;
                while filled < MULTIPART_PART_SIZE {
                    let n = body.read(&mut buffer[filled..]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    break;
                }
                let next = total + filled as u64;
                if let Some(expected) = meta.content_length {
                    if next > expected {
                        return Err(StorageError::Integrity(format!(
                            "S3 upload exceeded declared length {expected}"
                        )));
                    }
                }
                if next > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES || part_number > S3_MAX_PARTS {
                    return Err(StorageError::PayloadTooLarge(format!(
                        "S3 upload exceeded {} bytes or {S3_MAX_PARTS} parts",
                        crate::bounded::MAX_SUPPORTED_OBJECT_BYTES
                    )));
                }
                hasher.update(&buffer[..filled]);
                total = next;

                let uploaded = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(&full_key)
                    .upload_id(&upload_id)
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

            let digest = hex::encode(hasher.finalize());
            if let Some(expected) = meta.sha256_hex.as_deref() {
                if !expected.eq_ignore_ascii_case(&digest) {
                    return Err(StorageError::Integrity(
                        "S3 upload sha256 does not match the body".into(),
                    ));
                }
            }
            if let Some(expected) = meta.content_length {
                if total != expected {
                    return Err(StorageError::Integrity(format!(
                        "S3 upload wrote {total} bytes, expected {expected}"
                    )));
                }
            }
            meta.sha256_hex = Some(digest.clone());

            if completed.is_empty() {
                self.abort_upload(&full_key, &upload_id).await;
                guard.disarm();
                self.put(key, Bytes::new(), meta).await?;
                return Ok((0, hex::encode(Sha256::digest([]))));
            }

            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&full_key)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(completed))
                        .build(),
                )
                .send()
                .await
                .map_err(|err| StorageError::S3(err.to_string()))?;
            guard.disarm();
            clear_orphan(&upload_id);
            Ok((total, digest))
        };

        match upload.await {
            Ok(done) => Ok(done),
            Err(err) => {
                self.abort_upload(&full_key, &upload_id).await;
                guard.disarm();
                Err(err)
            }
        }
    }

    async fn abort_upload(&self, key: &str, upload_id: &str) {
        match self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
        {
            Ok(_) => clear_orphan(upload_id),
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("NoSuchUpload") || msg.contains("404") {
                    clear_orphan(upload_id);
                } else {
                    tracing::error!(
                        key,
                        upload_id,
                        error = %err,
                        "multipart abort failed; upload id retained for retry"
                    );
                }
            }
        }
    }

    async fn copy_object_single(&self, from: &str, to: &str) -> Result<()> {
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(self.full_key(to))
            .copy_source(encode_copy_source(&self.bucket, &self.full_key(from)))
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Copy)
            .send()
            .await
            .map_err(|err| map_missing(from, err.to_string()))?;
        Ok(())
    }

    async fn copy_multipart(&self, from: &str, to: &str, probe: &ObjectProbe) -> Result<()> {
        let full_from = self.full_key(from);
        let full_to = self.full_key(to);
        let source = encode_copy_source(&self.bucket, &full_from);
        let created = apply_meta_multipart(
            self.client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(&full_to),
            &probe.meta,
        )
        .send()
        .await
        .map_err(|err| StorageError::S3(err.to_string()))?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| StorageError::S3("CreateMultipartUpload returned no upload id".into()))?
            .to_string();
        record_orphan(&self.bucket, &full_to, &upload_id);
        let mut guard = MultipartGuard::arm(
            self.client.clone(),
            self.bucket.clone(),
            full_to.clone(),
            upload_id.clone(),
        );
        let part_size = MULTIPART_PART_SIZE as u64;
        let copy = async {
            let mut completed = Vec::new();
            let mut start = 0u64;
            let mut part_number: i32 = 1;
            while start < probe.size {
                if part_number > S3_MAX_PARTS {
                    return Err(StorageError::PayloadTooLarge(format!(
                        "multipart copy needs more than {S3_MAX_PARTS} parts"
                    )));
                }
                let end = (start + part_size).min(probe.size);
                let last = end.saturating_sub(1);
                let copied = self
                    .client
                    .upload_part_copy()
                    .bucket(&self.bucket)
                    .key(&full_to)
                    .copy_source(&source)
                    .copy_source_range(format!("bytes={start}-{last}"))
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .send()
                    .await
                    .map_err(|err| StorageError::S3(err.to_string()))?;
                let etag = copied
                    .copy_part_result()
                    .and_then(|part| part.e_tag())
                    .ok_or_else(|| {
                        StorageError::S3(format!("UploadPartCopy {part_number} returned no ETag"))
                    })?;
                completed.push(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .e_tag(etag)
                        .build(),
                );
                start = end;
                part_number += 1;
            }
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&full_to)
                .upload_id(&upload_id)
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
        match copy.await {
            Ok(()) => {
                guard.disarm();
                clear_orphan(&upload_id);
                Ok(())
            }
            Err(err) => {
                self.abort_upload(&full_to, &upload_id).await;
                guard.disarm();
                Err(err)
            }
        }
    }

    /// Aborts multipart uploads previously recorded for this bucket.
    ///
    /// # Errors
    ///
    /// Returns I/O errors reading the orphan directory. Individual abort
    /// failures are retained and logged; they do not fail the sweep.
    pub async fn retry_orphans(&self, dir: &std::path::Path) -> Result<usize> {
        if !dir.is_dir() {
            return Ok(0);
        }
        let mut retried = 0usize;
        for entry in std::fs::read_dir(dir).map_err(StorageError::Io)? {
            let entry = entry.map_err(StorageError::Io)?;
            let path = entry.path();
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<OrphanRecord>(&text) else {
                continue;
            };
            if record.bucket != self.bucket {
                continue;
            }
            retried += 1;
            self.abort_upload(&record.key, &record.upload_id).await;
        }
        Ok(retried)
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn instance_id(&self) -> String {
        format!(
            "s3:{}:{}:{}:{}",
            self.endpoint, self.region, self.bucket, self.prefix
        )
    }

    fn supports_server_copy(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(&self, key: &str, data: Bytes, meta: ObjectMeta) -> Result<()> {
        crate::bounded::ensure_scalar_len(data.len(), crate::bounded::MAX_SCALAR_OBJECT_BYTES)?;
        let mut meta = meta;
        if meta.content_length.is_none() {
            meta.content_length = Some(data.len() as u64);
        }
        if let Some(expected) = meta.sha256_hex.as_deref() {
            let want = crate::bounded::parse_sha256_hex(expected)?;
            if want.as_slice() != sha2::Sha256::digest(&data).as_slice() {
                return Err(StorageError::Integrity(
                    "scalar put sha256 does not match the buffer".into(),
                ));
            }
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
        let probe = self.probe(key).await?;
        crate::bounded::reject_scalar_hint(probe.size, crate::bounded::MAX_SCALAR_OBJECT_BYTES)?;
        let (_opened, body) = self.get_stream(key, None).await?;
        let data =
            crate::bounded::read_scalar_body(body, crate::bounded::MAX_SCALAR_OBJECT_BYTES).await?;
        if data.len() as u64 != probe.size {
            return Err(StorageError::Integrity(format!(
                "scalar get read {} bytes after HEAD reported {}",
                data.len(),
                probe.size
            )));
        }
        Ok(data)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        match self.probe(key).await {
            Ok(_) => Ok(true),
            Err(StorageError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
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
            ..Default::default()
        };
        let sha256_hex = meta_get(user_meta, "sha256");
        let commit_token = meta_get(user_meta, "commit-token");
        let meta = ObjectMeta {
            sha256_hex,
            commit_token,
            ..meta
        };
        Ok(ObjectProbe {
            key: key.to_string(),
            size: meta.content_length.unwrap_or(0),
            content_type: meta.content_type.clone(),
            meta,
            etag: out.e_tag().map(str::to_string),
        })
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let probe = self.probe(from).await?;
        if probe.size > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES {
            return Err(StorageError::PayloadTooLarge(format!(
                "copy of {} bytes exceeds {}",
                probe.size,
                crate::bounded::MAX_SUPPORTED_OBJECT_BYTES
            )));
        }
        if crate::bounded::copy_uses_single_request(probe.size) {
            return self.copy_object_single(from, to).await;
        }
        self.copy_multipart(from, to, &probe).await
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

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<crate::ListPage> {
        let full_prefix = self.full_key(prefix);
        let limit = crate::bounded::clamp_page_limit(limit);
        let mut token = cursor.map(str::to_string);
        let mut empty_hops = 0u32;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&full_prefix)
                .max_keys(i32::try_from(limit).unwrap_or(256));
            if let Some(token) = &token {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|err| StorageError::S3(err.to_string()))?;
            if resp.contents().len() > limit {
                return Err(StorageError::PayloadTooLarge(format!(
                    "S3 list page of {} objects exceeds {limit}",
                    resp.contents().len()
                )));
            }
            let mut objects = Vec::with_capacity(resp.contents().len());
            for obj in resp.contents() {
                let Some(raw_key) = obj.key() else { continue };
                let key = raw_key
                    .strip_prefix(&self.prefix)
                    .unwrap_or(raw_key)
                    .to_string();
                objects.push(ObjectInfo {
                    key,
                    size: obj.size().unwrap_or(0) as u64,
                });
            }
            let truncated = resp.is_truncated().unwrap_or(false);
            let next = if truncated {
                Some(
                    resp.next_continuation_token()
                        .map(str::to_string)
                        .ok_or_else(|| {
                            StorageError::InvalidCursor(
                                "S3 list page was truncated without a continuation token".into(),
                            )
                        })?,
                )
            } else {
                None
            };
            if let (Some(prev), Some(next)) = (token.as_deref(), next.as_deref()) {
                if prev == next {
                    return Err(StorageError::InvalidCursor(
                        "S3 continuation token did not advance".into(),
                    ));
                }
            }
            if objects.is_empty() {
                if let Some(next) = next {
                    empty_hops += 1;
                    if empty_hops > 8 {
                        return Err(StorageError::InvalidCursor(
                            "S3 returned empty pages without finishing".into(),
                        ));
                    }
                    token = Some(next);
                    continue;
                }
            }
            return Ok(crate::ListPage {
                objects,
                next_cursor: next,
            });
        }
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<crate::ByteRange>,
    ) -> Result<(
        ObjectProbe,
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    )> {
        let probe = self.probe(key).await?;
        let span = crate::bounded::normalize_range(range, Some(probe.size))?;
        let mut req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.full_key(key));
        if let Some(span) = span {
            req = req.range(s3_range_header(span));
        }
        let out = req.send().await.map_err(|err| {
            let msg = err.to_string();
            if msg.contains("NoSuchKey") || msg.contains("404") || msg.contains("NotFound") {
                StorageError::NotFound(key.into())
            } else {
                StorageError::S3(msg)
            }
        })?;
        let reader = out.body.into_async_read();
        Ok((probe, Box::pin(reader)))
    }

    async fn put_stream(
        &self,
        key: &str,
        body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: ObjectMeta,
    ) -> Result<crate::PutStreamResult> {
        let (n, sha) = self.put_stream_multipart(key, body, meta).await?;
        Ok(crate::PutStreamResult {
            bytes_written: n,
            etag: None,
            sha256_hex: Some(sha),
        })
    }
}

/// True when a file should be uploaded in parts rather than one `PutObject`.
#[must_use]
pub(crate) fn use_multipart(content_length: u64) -> bool {
    content_length >= MULTIPART_THRESHOLD
}

/// Copies content-type/length and `x-amz-meta-*` identity fields onto a PutObject.
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
    if let Some(sha) = &meta.sha256_hex {
        req = req.metadata("sha256", sha.clone());
    }
    if let Some(token) = &meta.commit_token {
        req = req.metadata("commit-token", token.clone());
    }
    req
}

/// Copies the same metadata onto `CreateMultipartUpload` (no content-length).
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
    if let Some(sha) = &meta.sha256_hex {
        req = req.metadata("sha256", sha.clone());
    }
    if let Some(token) = &meta.commit_token {
        req = req.metadata("commit-token", token.clone());
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

/// Parses an RFC3339 timestamp into non-negative Unix seconds for `mtime` metadata.
fn rfc3339_unix_secs(raw: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp().max(0) as u64)
}

/// `bytes=start-end` (inclusive) or `bytes=start-` through EOF.
pub(crate) fn s3_range_header(span: crate::bounded::ReadSpan) -> String {
    match span {
        crate::bounded::ReadSpan::ToEnd { offset } => format!("bytes={offset}-"),
        crate::bounded::ReadSpan::Exact { offset, length } => {
            let end = offset.saturating_add(length.saturating_sub(1));
            format!("bytes={offset}-{end}")
        }
    }
}

fn encode_copy_source(bucket: &str, key: &str) -> String {
    format!("{}/{}", encode_copy_token(bucket), encode_copy_token(key))
}

fn encode_copy_token(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn map_missing(key: &str, msg: String) -> StorageError {
    if msg.contains("NoSuchKey") || msg.contains("404") || msg.contains("NotFound") {
        StorageError::NotFound(key.into())
    } else {
        StorageError::S3(msg)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct OrphanRecord {
    bucket: String,
    key: String,
    upload_id: String,
    attempts: u32,
}

fn orphan_dir() -> Option<std::path::PathBuf> {
    let root = std::env::var_os("BOOKCLERK_FILES_DIR")?;
    Some(std::path::PathBuf::from(root).join("storage-orphans"))
}

fn orphan_path(upload_id: &str) -> Option<std::path::PathBuf> {
    let dir = orphan_dir()?;
    let name = hex::encode(sha2::Sha256::digest(upload_id.as_bytes()));
    Some(dir.join(format!("{name}.json")))
}

fn record_orphan(bucket: &str, key: &str, upload_id: &str) {
    let Some(path) = orphan_path(upload_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let record = OrphanRecord {
        bucket: bucket.to_string(),
        key: key.to_string(),
        upload_id: upload_id.to_string(),
        attempts: 0,
    };
    if let Ok(text) = serde_json::to_string(&record) {
        let _ = std::fs::write(path, text);
    }
}

fn clear_orphan(upload_id: &str) {
    if let Some(path) = orphan_path(upload_id) {
        let _ = std::fs::remove_file(path);
    }
}

struct MultipartGuard {
    client: Client,
    bucket: String,
    key: String,
    upload_id: String,
    armed: bool,
}

impl MultipartGuard {
    fn arm(client: Client, bucket: String, key: String, upload_id: String) -> Self {
        Self {
            client,
            bucket,
            key,
            upload_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for MultipartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let key = self.key.clone();
        let upload_id = self.upload_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let result = client
                    .abort_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                match result {
                    Ok(_) => clear_orphan(&upload_id),
                    Err(err) => {
                        tracing::error!(
                            upload_id,
                            error = %err,
                            "dropped multipart upload abort failed; id retained"
                        );
                    }
                }
            });
        } else {
            tracing::error!(
                upload_id,
                "dropped multipart upload could not schedule abort; id retained"
            );
        }
    }
}

/// Reads a metadata key, falling back to the lowercase form S3 may return.
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

    #[test]
    fn copy_source_encodes_spaces_and_slashes() {
        assert_eq!(
            encode_copy_source("bucket", "a/b c.m4b"),
            "bucket/a%2Fb%20c.m4b"
        );
    }

    #[test]
    fn range_header_matches_normalized_span() {
        assert_eq!(
            s3_range_header(crate::bounded::ReadSpan::ToEnd { offset: 4 }),
            "bytes=4-"
        );
        assert_eq!(
            s3_range_header(crate::bounded::ReadSpan::Exact {
                offset: 4,
                length: 2
            }),
            "bytes=4-5"
        );
    }
}
