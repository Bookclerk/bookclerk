//! AWS S3 / MinIO storage backend.

use std::path::Path;
use std::time::SystemTime;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bookclerk_config::OutputS3Config;
use bytes::Bytes;
use sea_orm::DatabaseConnection;

use crate::error::{Result, StorageError};
use crate::s3_credentials::load_s3_credentials;
use crate::traits::{ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend};

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
        if cfg.bucket.is_empty() {
            return Err(StorageError::S3("bucket must not be empty".into()));
        }

        let mut loader =
            aws_config::defaults(BehaviorVersion::latest()).region(Region::new(cfg.region.clone()));

        if let (Ok(access), Ok(secret)) = (
            std::env::var(crate::s3_credentials::ENV_AWS_ACCESS_KEY_ID),
            std::env::var(crate::s3_credentials::ENV_AWS_SECRET_ACCESS_KEY),
        ) {
            bookclerk_config::register_secret(&access);
            bookclerk_config::register_secret(&secret);
            let session = std::env::var(crate::s3_credentials::ENV_AWS_SESSION_TOKEN).ok();
            if let Some(ref token) = session {
                bookclerk_config::register_secret(token);
            }
            loader = loader.credentials_provider(Credentials::new(
                access,
                secret,
                session,
                None,
                "bookclerk-env",
            ));
        } else if let Some(db) = db {
            if let Some(creds) = load_s3_credentials(db).await? {
                loader = loader.credentials_provider(Credentials::new(
                    creds.access_key_id,
                    creds.secret_access_key,
                    creds.session_token,
                    None,
                    "bookclerk-encrypted-secrets",
                ));
            }
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
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .body(body);

        if let Some(ct) = meta.content_type {
            req = req.content_type(ct);
        }
        if let Some(len) = meta.content_length {
            req = req.content_length(len as i64);
        }
        if let Some(asin) = meta.asin {
            req = req.metadata("asin", asin);
        }
        if let Some(title) = meta.title {
            req = req.metadata("title", title);
        }
        // Logical timestamps as user-defined metadata (x-amz-meta-*). AWS S3 and
        // most compatible providers refuse to set system Last-Modified; this is
        // the only cost-free way to record purchased/published times at upload.
        // Also set `mtime` (unix secs) for s3fs/rclone-style mounts.
        if let Some(created) = meta.creation_time.clone() {
            req = req.metadata("creation-time", created);
        }
        if let Some(modified) = meta.last_write_time.clone() {
            req = req.metadata("last-write-time", modified.clone());
            if let Some(secs) = rfc3339_unix_secs(&modified) {
                req = req.metadata("mtime", secs.to_string());
            }
        }

        req.send()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?;
        Ok(())
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
        if meta.content_length.is_none() {
            if let Ok(stat) = tokio::fs::metadata(path).await {
                meta.content_length = Some(stat.len());
            }
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
}
