//! AWS S3 / MinIO storage backend.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use libation_config::StorageS3Config;

use crate::error::{Result, StorageError};
use crate::traits::{ObjectInfo, ObjectMeta, StorageBackend};

/// S3-compatible object storage.
#[derive(Debug, Clone)]
pub struct S3Backend {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3Backend {
    /// Build from Libation storage config (credentials via default AWS chain / env).
    pub async fn from_config(cfg: &StorageS3Config) -> Result<Self> {
        if cfg.bucket.is_empty() {
            return Err(StorageError::S3("bucket must not be empty".into()));
        }

        let mut loader =
            aws_config::defaults(BehaviorVersion::latest()).region(Region::new(cfg.region.clone()));

        // Allow explicit static credentials via standard env vars; otherwise
        // instance role / shared config is used.
        if let (Ok(access), Ok(secret)) = (
            std::env::var("AWS_ACCESS_KEY_ID"),
            std::env::var("AWS_SECRET_ACCESS_KEY"),
        ) {
            let session = std::env::var("AWS_SESSION_TOKEN").ok();
            loader = loader.credentials_provider(Credentials::new(
                access,
                secret,
                session,
                None,
                "libation-env",
            ));
        }

        let shared = loader.load().await;
        let mut s3_config = aws_sdk_s3::config::Builder::from(&shared);

        if let Some(endpoint) = &cfg.endpoint {
            s3_config = s3_config.endpoint_url(endpoint);
        }
        if cfg.force_path_style {
            s3_config = s3_config.force_path_style(true);
        }

        let client = Client::from_conf(s3_config.build());
        Ok(Self {
            client,
            bucket: cfg.bucket.clone(),
            prefix: normalize_prefix(&cfg.prefix),
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
        if let Some(created) = meta.creation_time {
            req = req.metadata("creation-time", created);
        }
        if let Some(modified) = meta.last_write_time {
            req = req.metadata("last-write-time", modified);
        }

        req.send()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?;
        Ok(())
    }
}

fn normalize_prefix(prefix: &str) -> String {
    if prefix.is_empty() {
        return String::new();
    }
    if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{prefix}/")
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    fn name(&self) -> &'static str {
        "s3"
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
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("NotFound") || msg.contains("404") || msg.contains("NoSuchKey") {
                    Ok(false)
                } else {
                    Err(StorageError::S3(msg))
                }
            }
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
        if created.is_none() && modified.is_none() {
            return Ok(());
        }
        let full = self.full_key(key);
        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&full)
            .send()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?;

        let mut meta = head.metadata().cloned().unwrap_or_default();
        if let Some(created) = created {
            meta.insert("creation-time".into(), system_time_rfc3339(created));
        }
        if let Some(modified) = modified {
            meta.insert("last-write-time".into(), system_time_rfc3339(modified));
        }

        let mut copy = self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .key(&full)
            .copy_source(format!("{}/{}", self.bucket, full))
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace);
        for (k, v) in meta {
            copy = copy.metadata(k, v);
        }
        if let Some(ct) = head.content_type() {
            copy = copy.content_type(ct);
        }
        copy.send()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?;
        Ok(())
    }
}

fn system_time_rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339()
}
