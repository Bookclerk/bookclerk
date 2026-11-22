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
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::bounded::{MULTIPART_PART_SIZE, S3_MAX_PARTS};
use crate::error::{Result, StorageError};
use crate::intent::{
    ConditionalPutResult, HintedListPage, ListRequest, MoveOptions, MoveResult,
    StorageCapabilities, WriteCondition,
};
use crate::traits::{ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend};

/// Connection settings for one S3 / MinIO destination.
///
/// The `s3` destination plugin declares these as `[[settings]]`; the host
/// resolves them from the plugin instance document and hands them to the
/// guest as its open context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct S3Settings {
    /// Destination bucket name (required).
    pub bucket: String,
    /// AWS region id (also used by MinIO path-style clients).
    pub region: String,
    /// Optional custom endpoint (MinIO, LocalStack, …).
    pub endpoint: Option<String>,
    /// Force path-style addressing (typical for MinIO).
    pub force_path_style: bool,
}

/// Static AWS-style credentials for the S3 destination.
///
/// These come from the instance's sealed secrets; without them the SDK
/// default provider chain applies inside the guest.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct S3Credentials {
    /// AWS access key id.
    pub access_key_id: String,
    /// AWS secret access key (redacted in [`Debug`]; never logged).
    pub secret_access_key: String,
    /// Optional STS session token when using temporary credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &redact_access_key(&self.access_key_id))
            .field("secret_access_key", &"***")
            .field("session_token", &self.session_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Debug-only access-key redaction (first 4 chars + stars).
fn redact_access_key(access_key_id: &str) -> String {
    if access_key_id.len() <= 4 {
        return "****".into();
    }
    let (prefix, rest) = access_key_id.split_at(4);
    format!("{prefix}{}", "*".repeat(rest.len().min(8)))
}

/// Below this size a single `PutObject` is enough. Above it, upload in fixed-size
/// parts so memory stays bounded and objects larger than the single-PUT limit
/// (5 GiB on AWS) still work.
pub(crate) const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;

/// S3-compatible object storage.
#[derive(Clone)]
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
    /// Granted directory for multipart recovery records. Absent means crash
    /// recovery is not available; uploads are not described as recoverable.
    journal: Option<std::sync::Arc<MultipartJournal>>,
    /// Part size. Production uses [`MULTIPART_PART_SIZE`]; tests may shrink it.
    part_size: usize,
    /// Which conditional requests the store enforces, learned once by
    /// [`S3Backend::conditional_support`] and shared by clones.
    conditional: std::sync::Arc<tokio::sync::OnceCell<ConditionalSupport>>,
    /// The shared AWS configuration (region, credentials) the client was
    /// built from; the notification queue's client reuses it.
    sdk: aws_config::SdkConfig,
    /// The change feed drained from the bucket's notification queue, set by
    /// [`S3Backend::with_notification_queue`].
    queue: Option<std::sync::Arc<crate::s3_queue::QueueFeed>>,
}

/// Which conditional requests a store enforces.
///
/// Some S3-compatible stores accept `If-None-Match` and `If-Match` but ignore
/// them, which would drop a precondition without a word. The backend
/// therefore trusts a condition only after [`S3Backend::conditional_support`]
/// has seen the store refuse one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConditionalSupport {
    /// `PutObject` refuses `If-None-Match: *` on an existing key and a
    /// mismatched `If-Match`. Gates conditional writes of small objects.
    pub writes: bool,
    /// `CompleteMultipartUpload` refuses `If-None-Match: *` on an existing
    /// key and a mismatched `If-Match`. Gates conditional writes of large
    /// objects and the completion of a large no-overwrite move.
    pub completes: bool,
    /// `CopyObject` refuses `If-None-Match: *` onto an existing key. Gates
    /// no-overwrite moves.
    pub copies: bool,
    /// `CopyObject` refuses a mismatched `x-amz-copy-source-if-match`.
    /// Gates every native move, which pins its copy to the source ETag.
    pub copy_pins: bool,
    /// `UploadPartCopy` refuses a mismatched `x-amz-copy-source-if-match`.
    /// Gates every native move, because a large source is copied in parts
    /// and a rewrite between parts must not produce a mixed target.
    pub part_copy_pins: bool,
    /// `DeleteObject` refuses a mismatched `If-Match`. Gates every native
    /// move, whose last step deletes the source only while it is unchanged.
    pub deletes: bool,
}

impl ConditionalSupport {
    /// True when every request a conditional write may send is enforced,
    /// whichever of the single-request or multipart paths its size picks.
    #[must_use]
    pub fn conditional_writes(self) -> bool {
        self.writes && self.completes
    }

    /// True when a move with these options can run natively: the copy is
    /// always pinned to the source ETag and the source delete is always
    /// conditional, and a no-overwrite move also needs the copy and the
    /// multipart completion to refuse an existing target.
    #[must_use]
    pub fn native_move(self, no_overwrite: bool) -> bool {
        self.deletes
            && self.copy_pins
            && self.part_copy_pins
            && (!no_overwrite || (self.copies && self.completes))
    }
}

impl std::fmt::Debug for S3Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Backend")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("part_size", &self.part_size)
            .field("notification_queue", &self.queue.is_some())
            .field(
                "journal",
                &self
                    .journal
                    .as_ref()
                    .map(|journal| journal.dir.display().to_string()),
            )
            .finish_non_exhaustive()
    }
}

impl S3Backend {
    /// Build with explicit credentials and a granted multipart journal directory.
    ///
    /// The directory must already be a location the process is allowed to
    /// persist. For the destination guest that is its `HOME`, not the host
    /// files directory. A journal that cannot be created or read fails
    /// construction instead of claiming later recovery.
    ///
    /// # Errors
    ///
    /// Returns an error when the client cannot be built, the journal directory
    /// cannot be created, or reading existing records fails.
    pub async fn from_parts_with_journal(
        cfg: &S3Settings,
        prefix: &str,
        creds: Option<&S3Credentials>,
        journal_dir: &std::path::Path,
    ) -> Result<Self> {
        let journal = MultipartJournal::open(
            journal_dir,
            cfg.endpoint
                .as_deref()
                .map(normalize_s3_endpoint)
                .unwrap_or_default(),
            &cfg.bucket,
            crate::normalize_prefix(prefix),
        )?;
        let mut backend = Self::from_parts(cfg, prefix, creds).await?;
        backend.journal = Some(std::sync::Arc::new(journal));
        backend.retry_orphans().await?;
        Ok(backend)
    }

    /// Overrides the multipart part size. Values below 1 are ignored.
    #[must_use]
    pub fn with_part_size(mut self, part_size: usize) -> Self {
        if part_size > 0 {
            self.part_size = part_size;
        }
        self
    }

    /// Build with explicit settings and credentials.
    ///
    /// Without [`Self::from_parts_with_journal`], multipart uploads are not
    /// recoverable after process death.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn from_parts(
        cfg: &S3Settings,
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
        Ok(Self {
            client,
            bucket: cfg.bucket.clone(),
            prefix: crate::normalize_prefix(prefix),
            region: cfg.region.clone(),
            endpoint: cfg
                .endpoint
                .as_deref()
                .map(normalize_s3_endpoint)
                .unwrap_or_default(),
            journal: None,
            part_size: MULTIPART_PART_SIZE,
            conditional: std::sync::Arc::default(),
            sdk: shared,
            queue: None,
        })
    }

    /// Serves `changes` from the bucket's notification queue.
    ///
    /// The bucket must send its object-created and object-removed events to
    /// `queue` (directly, through SNS, or through an EventBridge rule), and
    /// nothing else may drain the queue. The feed keeps the receipts of the
    /// messages it has handed out in `state_dir` (the guest's `HOME`) until
    /// the host's next call says it has read them.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue client cannot be built or
    /// `state_dir` cannot be created.
    pub fn with_notification_queue(
        mut self,
        queue: crate::s3_queue::QueueUrl,
        state_dir: &Path,
    ) -> Result<Self> {
        let id = queue.as_str().to_string();
        let client = crate::s3_queue::SqsQueue::new(&self.sdk, queue)?;
        let feed = crate::s3_queue::QueueFeed::open(
            Box::new(client),
            crate::s3_queue::s3_message_parser(&self.bucket, &self.prefix),
            &id,
            &self.bucket,
            &self.prefix,
            state_dir,
        )?;
        self.queue = Some(std::sync::Arc::new(feed));
        Ok(self)
    }

    /// Serves `changes` from an R2 bucket's event notifications, pulled
    /// from a Cloudflare Queue.
    ///
    /// The bucket's event notification rule must send object-create and
    /// object-delete events to `queue`, HTTP pull must be on for it, and
    /// nothing else may consume it. Like the SQS feed, the feed keeps the
    /// lease ids it has handed out in `state_dir` (the guest's `HOME`) until
    /// the host's next call says it has read them.
    ///
    /// # Arguments
    ///
    /// * `account` - the Cloudflare account the bucket and queue belong to.
    /// * `queue` - the queue's id.
    /// * `token` - an API token with Queues read and write on the account.
    /// * `state_dir` - where the feed keeps its state.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue client cannot be built or
    /// `state_dir` cannot be created.
    pub fn with_cloudflare_queue(
        self,
        account: &crate::r2_queue::CloudflareAccountId,
        queue: &crate::r2_queue::CloudflareQueueId,
        token: crate::r2_queue::CloudflareApiToken,
        state_dir: &Path,
    ) -> Result<Self> {
        self.with_cloudflare_queue_at(crate::r2_queue::API_BASE, account, queue, token, state_dir)
    }

    /// [`Self::with_cloudflare_queue`] against the Cloudflare API at
    /// `api_base`, so tests can point it at a local server.
    ///
    /// # Errors
    ///
    /// As [`Self::with_cloudflare_queue`].
    pub(crate) fn with_cloudflare_queue_at(
        mut self,
        api_base: &str,
        account: &crate::r2_queue::CloudflareAccountId,
        queue: &crate::r2_queue::CloudflareQueueId,
        token: crate::r2_queue::CloudflareApiToken,
        state_dir: &Path,
    ) -> Result<Self> {
        let client = crate::r2_queue::CloudflareQueue::new(api_base, account, queue, token)?;
        let scope_account = account.as_str().to_string();
        let bucket = self.bucket.clone();
        let prefix = self.prefix.clone();
        let parse: crate::s3_queue::MessageParser = Box::new(move |body| {
            crate::r2_events::parse_r2_event(
                body,
                crate::r2_events::R2EventScope {
                    account: &scope_account,
                    bucket: &bucket,
                    prefix: &prefix,
                },
            )
        });
        let feed = crate::s3_queue::QueueFeed::open(
            Box::new(client),
            parse,
            &format!("cloudflare:{}:{}", account.as_str(), queue.as_str()),
            &self.bucket,
            &self.prefix,
            state_dir,
        )?;
        self.queue = Some(std::sync::Arc::new(feed));
        Ok(self)
    }

    /// True when `changes` is served from a notification queue.
    #[must_use]
    pub fn has_notification_queue(&self) -> bool {
        self.queue.is_some()
    }

    /// Prepends the destination prefix to `key` (no-op when the prefix is empty).
    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}{key}", self.prefix)
        }
    }

    /// Single `PutObject` with Bookclerk metadata headers, sent with
    /// `condition` as `If-None-Match` / `If-Match`. Returns the new ETag.
    async fn put_body(
        &self,
        key: &str,
        body: ByteStream,
        meta: ObjectMeta,
        condition: Option<&WriteCondition>,
    ) -> Result<Option<String>> {
        let mut req = apply_meta_put(
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(self.full_key(key))
                .body(body),
            &meta,
        );
        match condition {
            Some(WriteCondition::IfNoneMatch) => req = req.if_none_match("*"),
            Some(WriteCondition::IfMatch(etag)) => req = req.if_match(etag),
            None => {}
        }
        let out = req
            .send()
            .await
            .map_err(|err| map_conditional(key, condition, &err))?;
        Ok(out.e_tag().map(str::to_string))
    }

    /// What every S3 backend can do natively once the store has shown it
    /// enforces conditions; each operation falls back to the host's
    /// algorithm on a store that does not (see [`Self::conditional_support`]).
    pub const CAPABILITIES: StorageCapabilities = StorageCapabilities {
        move_object: true,
        move_many: true,
        delete_many: true,
        head_many: true,
        conditional_write: true,
        changes: false,
        translate_keys: true,
    };

    /// Which conditional requests this store enforces, probed on first use
    /// and remembered for the backend's lifetime.
    ///
    /// Each request kind is probed on its own, because a store can enforce a
    /// condition on one and ignore it on another. Under `.bookclerk-probe/`
    /// the probe writes a scratch object with `If-None-Match: *` twice
    /// (the second must be refused) and overwrites it with a wrong
    /// `If-Match` (refused); completes a one-part multipart upload onto it
    /// with each condition (both refused) and aborts the upload; copies it
    /// onto a second existing object with a wrong
    /// `x-amz-copy-source-if-match` (refused) and with `If-None-Match: *`
    /// (refused); copies it as an upload part with a wrong
    /// `x-amz-copy-source-if-match` (refused) and aborts that upload; and
    /// deletes it with a wrong `If-Match` (refused). It then removes both
    /// objects. Any other answer, including an error, counts as "not
    /// enforced", so a store is never trusted by default.
    pub async fn conditional_support(&self) -> ConditionalSupport {
        *self
            .conditional
            .get_or_init(|| self.probe_conditional_support())
            .await
    }

    /// Runs the probe behind [`Self::conditional_support`].
    async fn probe_conditional_support(&self) -> ConditionalSupport {
        let id = uuid::Uuid::new_v4();
        let key = self.full_key(&format!(".bookclerk-probe/conditional-{id}"));
        let other = self.full_key(&format!(".bookclerk-probe/conditional-{id}-target"));
        let mut support = ConditionalSupport::default();
        if let Some(writes) = self.probe_writes(&key).await {
            support.writes = writes;
            support.completes = self.probe_completes(&key).await;
            (support.copies, support.copy_pins) = self.probe_copies(&key, &other).await;
            support.part_copy_pins = self.probe_part_copy_pins(&key).await;
            support.deletes = refused(
                self.client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .if_match(PROBE_MISMATCH)
                    .send()
                    .await,
            );
        }
        for scratch in [&key, &other] {
            let _ = self
                .client
                .delete_object()
                .bucket(&self.bucket)
                .key(scratch)
                .send()
                .await;
        }
        tracing::debug!(
            bucket = %self.bucket,
            writes = support.writes,
            completes = support.completes,
            copies = support.copies,
            copy_pins = support.copy_pins,
            part_copy_pins = support.part_copy_pins,
            deletes = support.deletes,
            "probed S3 conditional request support"
        );
        support
    }

    /// Creates the probe object at `key` and checks that `PutObject` then
    /// refuses both conditions.
    ///
    /// # Returns
    ///
    /// Whether both were refused, or `None` when the probe object could not
    /// be created (the rest of the probe needs it, so nothing is trusted).
    async fn probe_writes(&self, key: &str) -> Option<bool> {
        let put = || {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(ByteStream::from_static(b"probe"))
        };
        put().if_none_match("*").send().await.ok()?;
        Some(
            refused(put().if_none_match("*").send().await)
                && refused(put().if_match(PROBE_MISMATCH).send().await),
        )
    }

    /// True when `CompleteMultipartUpload` onto the existing `key` refuses
    /// both `If-None-Match: *` and a mismatched `If-Match`. The upload is
    /// aborted afterwards whatever happened.
    async fn probe_completes(&self, key: &str) -> bool {
        let Ok(created) = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        else {
            return false;
        };
        let Some(upload_id) = created.upload_id().map(str::to_string) else {
            return false;
        };
        let enforced = async {
            let part = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(1)
                .body(ByteStream::from_static(b"probe"))
                .send()
                .await
                .ok()?;
            let parts = CompletedMultipartUpload::builder()
                .parts(
                    CompletedPart::builder()
                        .part_number(1)
                        .set_e_tag(part.e_tag().map(str::to_string))
                        .build(),
                )
                .build();
            let complete = || {
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .multipart_upload(parts.clone())
            };
            Some(
                refused(complete().if_none_match("*").send().await)
                    && refused(complete().if_match(PROBE_MISMATCH).send().await),
            )
        }
        .await
        .unwrap_or(false);
        let _ = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .send()
            .await;
        enforced
    }

    /// Checks how `CopyObject` from `key` onto the existing `other` treats
    /// its conditions, sent the way a move sends them.
    ///
    /// The source's ETag is read afresh, because an earlier probe step that
    /// a store did not refuse may have rewritten it, and a stale
    /// `x-amz-copy-source-if-match` would be refused for the wrong reason.
    ///
    /// # Returns
    ///
    /// `(copies, copy_pins)`: whether `If-None-Match: *` onto `other` was
    /// refused, and whether a wrong `x-amz-copy-source-if-match` was.
    async fn probe_copies(&self, key: &str, other: &str) -> (bool, bool) {
        let created = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(other)
            .body(ByteStream::from_static(b"target"))
            .send()
            .await;
        if created.is_err() {
            return (false, false);
        }
        let copy = || {
            self.client
                .copy_object()
                .bucket(&self.bucket)
                .key(other)
                .copy_source(encode_copy_source(&self.bucket, key))
        };
        // Leaves `other` in place whether or not the store refuses it.
        let copy_pins = refused(copy().copy_source_if_match(PROBE_MISMATCH).send().await);
        let Some(etag) = self.probe_etag(key).await else {
            return (false, copy_pins);
        };
        let copies = refused(
            copy()
                .copy_source_if_match(&etag)
                .if_none_match("*")
                .send()
                .await,
        );
        (copies, copy_pins)
    }

    /// True when `UploadPartCopy` from `key` refuses a wrong
    /// `x-amz-copy-source-if-match`. The upload is aborted afterwards
    /// whatever happened, so `key` is never rewritten.
    async fn probe_part_copy_pins(&self, key: &str) -> bool {
        let Ok(created) = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        else {
            return false;
        };
        let Some(upload_id) = created.upload_id().map(str::to_string) else {
            return false;
        };
        let enforced = refused(
            self.client
                .upload_part_copy()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(1)
                .copy_source(encode_copy_source(&self.bucket, key))
                .copy_source_if_match(PROBE_MISMATCH)
                .send()
                .await,
        );
        let _ = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(&upload_id)
            .send()
            .await;
        enforced
    }

    /// The current ETag of the probe object at `key`, or `None` when it
    /// cannot be read.
    async fn probe_etag(&self, key: &str) -> Option<String> {
        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .ok()?;
        head.e_tag()
            .filter(|etag| !etag.is_empty())
            .map(str::to_string)
    }

    /// Streams a local file as fixed-size parts (used above [`MULTIPART_THRESHOLD`]).
    ///
    /// Uses the same journal, digest, and abort path as [`Self::put_stream`].
    async fn put_file_multipart(&self, key: &str, path: &Path, meta: ObjectMeta) -> Result<()> {
        let file = tokio::fs::File::open(path).await?;
        self.put_stream_multipart(key, Box::pin(file), meta, None)
            .await
            .map(|_| ())
    }

    /// Streams `body` as multipart parts (bounded window; no full-object buffer).
    ///
    /// Returns `(bytes, sha256 hex, ETag)`. `condition` is sent on
    /// `CompleteMultipartUpload`, so nothing is published unless it holds. A
    /// dropped future aborts the upload via
    /// [`MultipartGuard`]. When a journal is configured, the upload id is
    /// recorded before any part is sent. A failed journal write aborts the
    /// upload. The whole-object SHA-256 is stored in a sidecar object before
    /// `CompleteMultipartUpload`; a multipart ETag is not that digest.
    async fn put_stream_multipart(
        &self,
        key: &str,
        mut body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        mut meta: ObjectMeta,
        condition: Option<&WriteCondition>,
    ) -> Result<(u64, String, Option<String>)> {
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
        if let Err(err) = self.record_orphan(&full_key, &upload_id) {
            self.abort_upload(&full_key, &upload_id).await;
            return Err(err);
        }
        let mut guard = MultipartGuard::arm(
            self.client.clone(),
            self.bucket.clone(),
            full_key.clone(),
            upload_id.clone(),
            self.journal.clone(),
        );

        let upload = async {
            let mut part_number: i32 = 1;
            let mut completed = Vec::new();
            let mut buffer = vec![0u8; self.part_size];
            let mut total = 0u64;
            let mut hasher = Sha256::new();

            loop {
                let mut filled = 0usize;
                while filled < self.part_size {
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
                let etag = self
                    .put_body(key, ByteStream::from(Bytes::new()), meta, condition)
                    .await?;
                return Ok((0, hex::encode(Sha256::digest([])), etag));
            }

            let mut complete = self
                .client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&full_key)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(completed))
                        .build(),
                );
            match condition {
                Some(WriteCondition::IfNoneMatch) => complete = complete.if_none_match("*"),
                Some(WriteCondition::IfMatch(etag)) => complete = complete.if_match(etag),
                None => {}
            }
            let completed_out = complete
                .send()
                .await
                .map_err(|err| map_conditional(key, condition, &err))?;
            let etag = completed_out.e_tag().unwrap_or("").to_string();
            if etag.is_empty() {
                return Err(StorageError::Integrity(
                    "multipart complete returned no ETag; integrity was not published".into(),
                ));
            }
            // Bind the digest to this ETag only after the body is the current
            // object. A failed complete never writes it, so the previous
            // version's record stays attached to the previous ETag.
            self.put_bound_integrity(key, &etag, &meta).await?;
            guard.disarm();
            self.clear_orphan(&upload_id);
            Ok((total, digest, Some(etag)))
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
            Ok(_) => self.clear_orphan(upload_id),
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("NoSuchUpload") || msg.contains("404") {
                    self.clear_orphan(upload_id);
                } else {
                    self.note_orphan_failure(upload_id);
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

    /// Server-side copies the object `probe` describes from `from` to `to`,
    /// only while the source still has the probed ETag, and with `target`
    /// sent as the destination's precondition.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::PayloadTooLarge`] above the supported object
    /// size, [`StorageError::Integrity`] when the source has no ETag or
    /// changed during an unconditioned copy, [`StorageError::Conflict`] when
    /// a conditioned copy is refused, and S3 errors otherwise.
    /// Copies `from` to `to` and returns the bytes copied, read from the
    /// same probe the copy pins its source ETag to, so the size reported is
    /// the size of the object actually copied.
    ///
    /// # Errors
    ///
    /// Returns an error when the source cannot be probed or the copy fails.
    pub(crate) async fn copy_sized(&self, from: &str, to: &str) -> Result<u64> {
        let probe = self.probe(from).await?;
        if from != to {
            self.copy_probed(from, to, &probe, None).await?;
        }
        Ok(probe.size)
    }

    async fn copy_probed(
        &self,
        from: &str,
        to: &str,
        probe: &ObjectProbe,
        target: Option<&WriteCondition>,
    ) -> Result<()> {
        if probe.size > crate::bounded::MAX_SUPPORTED_OBJECT_BYTES {
            return Err(StorageError::PayloadTooLarge(format!(
                "copy of {} bytes exceeds {}",
                probe.size,
                crate::bounded::MAX_SUPPORTED_OBJECT_BYTES
            )));
        }
        let etag = probe
            .etag
            .clone()
            .filter(|etag| !etag.is_empty())
            .ok_or_else(|| {
                StorageError::Integrity("refusing server copy without a source ETag".into())
            })?;
        let multipart = if self.part_size == MULTIPART_PART_SIZE {
            !crate::bounded::copy_uses_single_request(probe.size)
        } else {
            // Tests shrink the part size so a multipart copy can be exercised
            // without a multi-gigabyte object. Production keeps the 5 GiB ceiling.
            probe.size > self.part_size as u64
        };
        if multipart {
            self.copy_multipart(from, to, probe, target).await
        } else {
            self.copy_object_single(from, to, &etag, &probe.meta, target)
                .await
        }
    }

    async fn copy_object_single(
        &self,
        from: &str,
        to: &str,
        source_etag: &str,
        source_meta: &ObjectMeta,
        target: Option<&WriteCondition>,
    ) -> Result<()> {
        let mut req = self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .key(self.full_key(to))
            .copy_source(encode_copy_source(&self.bucket, &self.full_key(from)))
            .copy_source_if_match(source_etag)
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Copy);
        match target {
            Some(WriteCondition::IfNoneMatch) => req = req.if_none_match("*"),
            Some(WriteCondition::IfMatch(etag)) => req = req.if_match(etag),
            None => {}
        }
        let copied = req.send().await.map_err(|err| {
            let msg = format!("{err:?}");
            // S3 answers 412 for both the source and the target condition
            // without saying which one failed; either way the copy did not
            // happen and the caller's precondition no longer holds.
            if target.is_some() && refused_precondition(&msg) {
                StorageError::Conflict(format!(
                    "{to} did not meet its precondition or {from} changed during the copy"
                ))
            } else {
                map_copy_failure(from, msg)
            }
        })?;
        if source_meta.sha256_hex.is_some() || source_meta.commit_token.is_some() {
            // The CopyObject response is the publication identity. A later HEAD
            // can observe a replacement and must not label that body with this
            // copy's digest.
            let etag = copied
                .copy_object_result()
                .and_then(|result| result.e_tag())
                .filter(|etag| !etag.is_empty())
                .ok_or_else(|| {
                    StorageError::Integrity(
                        "CopyObject returned no ETag; integrity was not published".into(),
                    )
                })?;
            self.put_bound_integrity(to, etag, source_meta).await?;
        }
        Ok(())
    }

    async fn copy_multipart(
        &self,
        from: &str,
        to: &str,
        probe: &ObjectProbe,
        target: Option<&WriteCondition>,
    ) -> Result<()> {
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
        if let Err(err) = self.record_orphan(&full_to, &upload_id) {
            self.abort_upload(&full_to, &upload_id).await;
            return Err(err);
        }
        let mut guard = MultipartGuard::arm(
            self.client.clone(),
            self.bucket.clone(),
            full_to.clone(),
            upload_id.clone(),
            self.journal.clone(),
        );
        let part_size = self.part_size as u64;
        let source_etag = probe
            .etag
            .clone()
            .filter(|etag| !etag.is_empty())
            .ok_or_else(|| {
                StorageError::Integrity("refusing multipart copy without a source ETag".into())
            })?;
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
                    .copy_source_if_match(&source_etag)
                    .copy_source_range(format!("bytes={start}-{last}"))
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .send()
                    .await
                    .map_err(|err| map_copy_failure(from, format!("{err:?}")))?;
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
            let mut complete = self
                .client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&full_to)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(completed))
                        .build(),
                );
            match target {
                Some(WriteCondition::IfNoneMatch) => complete = complete.if_none_match("*"),
                Some(WriteCondition::IfMatch(etag)) => complete = complete.if_match(etag),
                None => {}
            }
            let completed_out = complete
                .send()
                .await
                .map_err(|err| map_conditional(to, target, &err))?;
            let etag = completed_out.e_tag().unwrap_or("").to_string();
            if etag.is_empty() {
                return Err(StorageError::Integrity(
                    "multipart copy complete returned no ETag; integrity was not published".into(),
                ));
            }
            self.put_bound_integrity(to, &etag, &probe.meta).await?;
            Ok(())
        };
        match copy.await {
            Ok(()) => {
                guard.disarm();
                self.clear_orphan(&upload_id);
                Ok(())
            }
            Err(err) => {
                self.abort_upload(&full_to, &upload_id).await;
                guard.disarm();
                Err(err)
            }
        }
    }

    /// Aborts multipart uploads recorded for this endpoint, bucket, and prefix
    /// whose owner lock is not held.
    ///
    /// Live uploads are left alone. Failed aborts stay on disk and stop being
    /// retried after 8 failed attempts.
    ///
    /// # Errors
    ///
    /// Returns I/O errors reading the journal directory.
    pub async fn retry_orphans(&self) -> Result<usize> {
        let Some(journal) = &self.journal else {
            return Ok(0);
        };
        let mut retried = 0usize;
        for (path, record) in journal.list_records()? {
            if record.endpoint != self.endpoint
                || record.bucket != self.bucket
                || record.prefix != self.prefix
            {
                continue;
            }
            if journal.owner_is_live(&record.owner_id) {
                continue;
            }
            if record.attempts >= MAX_ORPHAN_ATTEMPTS {
                tracing::error!(
                    upload_id = %record.upload_id,
                    attempts = record.attempts,
                    "multipart cleanup exhausted its retry budget; record retained"
                );
                continue;
            }
            retried += 1;
            self.abort_upload(&record.key, &record.upload_id).await;
            let _ = path;
        }
        Ok(retried)
    }

    fn record_orphan(&self, key: &str, upload_id: &str) -> Result<()> {
        let Some(journal) = &self.journal else {
            tracing::warn!(
                key,
                upload_id,
                "multipart upload has no journal; process death cannot abort it"
            );
            return Ok(());
        };
        journal.record(key, upload_id)
    }

    fn clear_orphan(&self, upload_id: &str) {
        if let Some(journal) = &self.journal {
            journal.clear(upload_id);
        }
    }

    fn note_orphan_failure(&self, upload_id: &str) {
        if let Some(journal) = &self.journal {
            journal.note_failure(upload_id);
        }
    }

    /// Writes digest and commit token for one object version.
    ///
    /// The record key includes the ETag. `probe` reads that key only when HEAD
    /// returns the same ETag, so a failed or interleaved upload cannot attach
    /// its digest to a different body.
    async fn put_bound_integrity(&self, key: &str, etag: &str, meta: &ObjectMeta) -> Result<()> {
        if etag.is_empty() {
            return Err(StorageError::Integrity(
                "refusing to store integrity without an object ETag".into(),
            ));
        }
        if meta.sha256_hex.is_none() && meta.commit_token.is_none() {
            return Ok(());
        }
        let record = BoundIntegrity {
            etag: etag.to_string(),
            sha256_hex: meta.sha256_hex.clone(),
            commit_token: meta.commit_token.clone(),
        };
        let payload = serde_json::to_vec(&record)
            .map_err(|err| StorageError::Io(std::io::Error::other(err)))?;
        self.put_body(
            &bound_integrity_key(key, etag),
            ByteStream::from(Bytes::from(payload)),
            ObjectMeta {
                content_type: Some("application/json".into()),
                ..ObjectMeta::default()
            },
            None,
        )
        .await
        .map(|_| ())
    }

    async fn read_bound_integrity(&self, key: &str, etag: &str) -> Result<Option<BoundIntegrity>> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.full_key(&bound_integrity_key(key, etag)))
            .send()
            .await;
        let out = match out {
            Ok(out) => out,
            Err(err) => {
                let msg = format!("{err:?}");
                if msg.contains("NoSuchKey") || msg.contains("404") || msg.contains("NotFound") {
                    return Ok(None);
                }
                return Err(StorageError::S3(err.to_string()));
            }
        };
        let hint = out.content_length().map(|len| len as u64);
        let bytes = read_capped_record(hint, Box::pin(out.body.into_async_read())).await?;
        let parsed: BoundIntegrity = serde_json::from_slice(&bytes)
            .map_err(|err| StorageError::Integrity(format!("integrity record: {err}")))?;
        if parsed.etag != etag {
            return Ok(None);
        }
        Ok(Some(parsed))
    }

    async fn delete_bound_integrity(&self, key: &str) -> Result<()> {
        if key.contains(".bookclerk-integrity/") {
            return Ok(());
        }
        let prefix = format!("{}.bookclerk-integrity/", self.full_key(key));
        let mut token = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);
            if let Some(token) = &token {
                req = req.continuation_token(token);
            }
            let page = req
                .send()
                .await
                .map_err(|err| StorageError::S3(err.to_string()))?;
            for obj in page.contents() {
                if let Some(name) = obj.key() {
                    self.client
                        .delete_object()
                        .bucket(&self.bucket)
                        .key(name)
                        .send()
                        .await
                        .map_err(|err| StorageError::S3(err.to_string()))?;
                }
            }
            if page.is_truncated().unwrap_or(false) {
                token = page.next_continuation_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        let legacy = crate::bookclerk_meta_sidecar_key(key);
        let _ = self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.full_key(&legacy))
            .send()
            .await;
        Ok(())
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
        self.put_body(key, data.into(), meta, None)
            .await
            .map(|_| ())
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
        self.put_body(key, body, meta, None).await.map(|_| ())
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
                // A HEAD answer has no body, so the SDK's message for a
                // missing key is only "service error": read the status.
                let missing = err.as_service_error().is_some_and(|e| e.is_not_found())
                    || err
                        .raw_response()
                        .is_some_and(|raw| raw.status().as_u16() == 404);
                let msg = err.to_string();
                if missing
                    || msg.contains("NotFound")
                    || msg.contains("404")
                    || msg.contains("NoSuchKey")
                {
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
        let mut meta = ObjectMeta {
            sha256_hex,
            commit_token,
            ..meta
        };
        if !key.contains(".bookclerk-integrity/") {
            if let Some(etag) = out.e_tag() {
                if let Some(bound) = self.read_bound_integrity(key, etag).await? {
                    if meta.sha256_hex.is_none() {
                        meta.sha256_hex = bound.sha256_hex;
                    }
                    if meta.commit_token.is_none() {
                        meta.commit_token = bound.commit_token;
                    }
                }
            }
        }
        Ok(ObjectProbe {
            key: key.to_string(),
            size: meta.content_length.unwrap_or(0),
            content_type: meta.content_type.clone(),
            meta,
            etag: out.e_tag().map(str::to_string),
            modified_at_ms: out.last_modified().and_then(unix_ms),
            version_id: out.version_id().map(str::to_string),
            signature: out.e_tag().map(str::to_string),
        })
    }

    async fn copy_sized(&self, from: &str, to: &str) -> Result<u64> {
        S3Backend::copy_sized(self, from, to).await
    }

    async fn copy(&self, from: &str, to: &str) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let probe = self.probe(from).await?;
        self.copy_probed(from, to, &probe, None).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
            .map_err(|err| StorageError::S3(err.to_string()))?;
        self.delete_bound_integrity(key).await?;
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
                    etag: obj.e_tag().map(str::to_string),
                    modified_at_ms: obj.last_modified().and_then(unix_ms),
                    version_id: None,
                    signature: obj.e_tag().map(str::to_string),
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
        // The bytes must come from the object the probe describes: a
        // replacement between the two requests would otherwise be read
        // under the old object's identity.
        if let Some(version_id) = probe.version_id.as_deref() {
            req = req.version_id(version_id);
        }
        if let Some(etag) = probe.etag.as_deref() {
            req = req.if_match(etag);
        }
        let out = req.send().await.map_err(|err| {
            if refused_precondition(&format!("{err:?}")) {
                return StorageError::Conflict(format!("{key}: changed while it was opened"));
            }
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
        let (n, sha, etag) = self.put_stream_multipart(key, body, meta, None).await?;
        Ok(crate::PutStreamResult {
            bytes_written: n,
            etag,
            sha256_hex: Some(sha),
        })
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            changes: self.queue.is_some(),
            ..Self::CAPABILITIES
        }
    }

    /// Object keys that fit under this backend's prefix
    /// ([`crate::key_syntax::translate_for_object_store`]).
    async fn translate_keys(&self, keys: &[crate::key_syntax::HostKey]) -> Result<Vec<String>> {
        Ok(keys
            .iter()
            .map(|key| crate::key_syntax::translate_for_object_store(key, self.prefix.len()))
            .collect())
    }

    /// Drains the bucket's notification queue when one is configured
    /// ([`S3Backend::with_notification_queue`]); otherwise there is no feed.
    async fn changes(
        &self,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Option<crate::ChangeFeedPage>> {
        match &self.queue {
            Some(queue) => queue.changes(cursor, limit).await.map(Some),
            None => Ok(None),
        }
    }

    /// Server-side copies the source, only while it still has the ETag the
    /// move started from (and, with `no_overwrite`, only while nothing is at
    /// the target), then deletes the source only while it still has that
    /// ETag. S3 has no rename, so the move is never atomic. A store that has
    /// not shown it enforces these conditions gets
    /// [`crate::intent::fallback_move`] instead.
    async fn move_object(&self, from: &str, to: &str, options: &MoveOptions) -> Result<MoveResult> {
        let support = self.conditional_support().await;
        if !support.native_move(options.no_overwrite) {
            return crate::intent::fallback_move(self, from, to, options).await;
        }
        let Some(source) = self.head(from).await? else {
            if from != to && self.head(to).await?.is_some() {
                return Ok(MoveResult {
                    atomic: false,
                    already_moved: true,
                });
            }
            return Err(StorageError::NotFound(from.to_string()));
        };
        if let Some(expected) = &options.if_source_match {
            if source.etag.as_deref() != Some(expected.as_str()) {
                return Err(StorageError::Conflict(format!(
                    "{from} changed since it was read"
                )));
            }
        }
        if from == to {
            return Ok(MoveResult {
                atomic: true,
                already_moved: false,
            });
        }
        let Some(source_etag) = source.etag.clone().filter(|etag| !etag.is_empty()) else {
            return crate::intent::fallback_move(self, from, to, options).await;
        };
        let target = options.no_overwrite.then_some(WriteCondition::IfNoneMatch);
        self.copy_probed(from, to, &source, target.as_ref())
            .await
            .map_err(|err| match err {
                StorageError::Integrity(msg) if msg.contains("changed during copy") => {
                    StorageError::Conflict(format!("{from} changed while it was copied to {to}"))
                }
                other => other,
            })?;
        let deleted = self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.full_key(from))
            .if_match(&source_etag)
            .send()
            .await;
        match deleted {
            Ok(_) => self.delete_bound_integrity(from).await?,
            Err(err) => {
                let msg = format!("{err:?}");
                if refused_precondition(&msg) {
                    return Err(StorageError::Conflict(format!(
                        "{from} changed while it was copied to {to}; the source was kept"
                    )));
                }
                // Someone else already removed the source; the copy stands.
                if !matches!(map_missing(from, msg), StorageError::NotFound(_)) {
                    return Err(StorageError::S3(format!("{err}")));
                }
            }
        }
        Ok(MoveResult {
            atomic: false,
            already_moved: false,
        })
    }

    /// Sends the condition with the upload (`If-None-Match` / `If-Match` on
    /// `PutObject` or `CompleteMultipartUpload`), so the store decides it
    /// atomically, after a cheap `HeadObject` that refuses a write bound to
    /// fail before its body is uploaded. A store that has not shown it
    /// enforces conditions gets [`crate::intent::fallback_put_conditional`].
    async fn put_stream_conditional(
        &self,
        key: &str,
        body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: ObjectMeta,
        condition: &WriteCondition,
    ) -> Result<ConditionalPutResult> {
        if !self.conditional_support().await.conditional_writes() {
            return crate::intent::fallback_put_conditional(self, key, body, meta, condition).await;
        }
        let current = self.head(key).await?;
        let holds = match (condition, &current) {
            (WriteCondition::IfNoneMatch, current) => current.is_none(),
            (WriteCondition::IfMatch(expected), current) => {
                current.as_ref().and_then(|p| p.etag.as_deref()) == Some(expected.as_str())
            }
        };
        if !holds {
            return Err(StorageError::Conflict(format!(
                "{key}: precondition failed"
            )));
        }
        let (bytes_written, sha, etag) = self
            .put_stream_multipart(key, body, meta, Some(condition))
            .await?;
        Ok(ConditionalPutResult {
            put: crate::PutStreamResult {
                bytes_written,
                etag,
                sha256_hex: Some(sha),
            },
            atomic: true,
        })
    }

    /// `ListObjectsV2` already carries each object's ETag and modification
    /// time, so every page has signatures. The time filter runs on the page
    /// the store returned, so a filtered page can be short or empty while
    /// `next_cursor` says more remain.
    async fn list_hinted(&self, request: &ListRequest) -> Result<HintedListPage> {
        let mut page = self
            .list_page(&request.prefix, request.cursor.as_deref(), request.limit)
            .await?;
        if let Some(since) = request.modified_since_ms {
            page.objects
                .retain(|object| object.modified_at_ms.is_none_or(|at| at >= since));
        }
        Ok(HintedListPage {
            page,
            signatures_included: true,
            modified_since_applied: request.modified_since_ms.is_some(),
        })
    }
}

/// Converts an S3 timestamp to UTC unix milliseconds; `None` before 1970.
fn unix_ms(at: &aws_sdk_s3::primitives::DateTime) -> Option<u64> {
    u64::try_from(at.to_millis().ok()?).ok()
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

/// Stop automatic abort retries after this many failed attempts. The record stays.
const MAX_ORPHAN_ATTEMPTS: u32 = 8;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OrphanRecord {
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    bucket: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    upload_id: String,
    #[serde(default)]
    owner_id: String,
    #[serde(default)]
    attempts: u32,
}

struct MultipartJournal {
    dir: std::path::PathBuf,
    endpoint: String,
    bucket: String,
    prefix: String,
    owner_id: String,
    _lock: std::fs::File,
}

impl std::fmt::Debug for MultipartJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultipartJournal")
            .field("dir", &self.dir)
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("owner_id", &self.owner_id)
            .finish_non_exhaustive()
    }
}

impl MultipartJournal {
    fn open(dir: &std::path::Path, endpoint: String, bucket: &str, prefix: String) -> Result<Self> {
        std::fs::create_dir_all(dir.join("owners")).map_err(StorageError::Io)?;
        std::fs::create_dir_all(dir.join("uploads")).map_err(StorageError::Io)?;
        let owner_id = uuid::Uuid::new_v4().to_string();
        let lock_path = dir.join("owners").join(format!("{owner_id}.lock"));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(StorageError::Io)?;
        fs4::FileExt::lock(&lock).map_err(StorageError::Io)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            endpoint,
            bucket: bucket.to_string(),
            prefix,
            owner_id,
            _lock: lock,
        })
    }

    fn record_path(&self, upload_id: &str) -> std::path::PathBuf {
        let name = hex::encode(sha2::Sha256::digest(upload_id.as_bytes()));
        self.dir.join("uploads").join(format!("{name}.json"))
    }

    fn record(&self, key: &str, upload_id: &str) -> Result<()> {
        let path = self.record_path(upload_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StorageError::Io)?;
        }
        let record = OrphanRecord {
            endpoint: self.endpoint.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            key: key.to_string(),
            upload_id: upload_id.to_string(),
            owner_id: self.owner_id.clone(),
            attempts: 0,
        };
        let text = serde_json::to_string(&record)
            .map_err(|err| StorageError::Io(std::io::Error::other(err)))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(StorageError::Io)?;
        std::fs::rename(&tmp, &path).map_err(StorageError::Io)?;
        Ok(())
    }

    fn clear(&self, upload_id: &str) {
        let _ = std::fs::remove_file(self.record_path(upload_id));
    }

    fn note_failure(&self, upload_id: &str) {
        let path = self.record_path(upload_id);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(mut record) = serde_json::from_str::<OrphanRecord>(&text) else {
            return;
        };
        record.attempts = record.attempts.saturating_add(1);
        if let Ok(text) = serde_json::to_string(&record) {
            let _ = std::fs::write(path, text);
        }
    }

    fn list_records(&self) -> Result<Vec<(std::path::PathBuf, OrphanRecord)>> {
        let dir = self.dir.join("uploads");
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(StorageError::Io)? {
            let entry = entry.map_err(StorageError::Io)?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match serde_json::from_str::<OrphanRecord>(&text) {
                Ok(record) => out.push((path, record)),
                Err(_) => {
                    let _ = std::fs::rename(&path, path.with_extension("corrupt"));
                }
            }
        }
        Ok(out)
    }

    fn owner_is_live(&self, owner_id: &str) -> bool {
        if owner_id.is_empty() {
            return false;
        }
        let path = self.dir.join("owners").join(format!("{owner_id}.lock"));
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            return false;
        };
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => {
                let _ = fs4::FileExt::unlock(&file);
                false
            }
            Err(_) => true,
        }
    }
}

struct MultipartGuard {
    client: Client,
    bucket: String,
    key: String,
    upload_id: String,
    journal: Option<std::sync::Arc<MultipartJournal>>,
    armed: bool,
}

impl MultipartGuard {
    fn arm(
        client: Client,
        bucket: String,
        key: String,
        upload_id: String,
        journal: Option<std::sync::Arc<MultipartJournal>>,
    ) -> Self {
        Self {
            client,
            bucket,
            key,
            upload_id,
            journal,
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
        #[cfg(test)]
        if SKIP_DROP_ABORT.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let key = self.key.clone();
        let upload_id = self.upload_id.clone();
        let journal = self.journal.clone();
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
                    Ok(_) => {
                        if let Some(journal) = journal {
                            journal.clear(&upload_id);
                        }
                    }
                    Err(err) => {
                        if let Some(journal) = journal {
                            journal.note_failure(&upload_id);
                        }
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

#[cfg(test)]
static SKIP_DROP_ABORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Integrity JSON is metadata, not a media object. Reads stop at this cap.
const INTEGRITY_RECORD_MAX_BYTES: u64 = 64 * 1024;

/// Reads one integrity record, counting bytes rather than trusting `Content-Length`.
///
/// A hint above the cap is rejected before the body is read. A missing or
/// understated length still stops at [`INTEGRITY_RECORD_MAX_BYTES`] + 1.
///
/// # Errors
///
/// Returns [`StorageError::PayloadTooLarge`] when the record exceeds the cap,
/// and [`StorageError::Io`] when the read fails.
async fn read_capped_record(
    hint: Option<u64>,
    reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
) -> Result<bytes::Bytes> {
    if let Some(hint) = hint {
        if hint > INTEGRITY_RECORD_MAX_BYTES {
            return Err(StorageError::PayloadTooLarge(format!(
                "integrity record hint of {hint} bytes exceeds {INTEGRITY_RECORD_MAX_BYTES}"
            )));
        }
    }
    crate::bounded::read_scalar_body(reader, INTEGRITY_RECORD_MAX_BYTES).await
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BoundIntegrity {
    etag: String,
    #[serde(default)]
    sha256_hex: Option<String>,
    #[serde(default)]
    commit_token: Option<String>,
}

fn bound_integrity_key(key: &str, etag: &str) -> String {
    format!(
        "{key}.bookclerk-integrity/{}.json",
        hex::encode(etag.as_bytes())
    )
}

/// The `If-Match` value the conditional probe sends, which no real object's
/// ETag equals.
const PROBE_MISMATCH: &str = "\"bookclerk-probe-mismatch\"";

/// True when a probe request failed because the store refused its
/// precondition; success or any other failure means "not enforced".
fn refused<T, E: std::fmt::Debug>(answer: std::result::Result<T, E>) -> bool {
    matches!(answer, Err(err) if refused_precondition(&format!("{err:?}")))
}

/// True when an S3 error says a request's precondition did not hold.
fn refused_precondition(msg: &str) -> bool {
    msg.contains("PreconditionFailed")
        || msg.contains("412")
        || msg.contains("ConditionalRequestConflict")
}

/// Maps a failed conditional write of `key`.
///
/// A refused precondition is [`StorageError::Conflict`] when the request
/// carried one; the error's debug text is read only to recognise that
/// refusal. Every other failure keeps the plain [`StorageError::S3`] message
/// an unconditional write reports.
fn map_conditional<E>(key: &str, condition: Option<&WriteCondition>, err: &E) -> StorageError
where
    E: std::fmt::Debug + std::fmt::Display,
{
    if condition.is_some() && refused_precondition(&format!("{err:?}")) {
        StorageError::Conflict(format!("{key}: precondition failed"))
    } else {
        StorageError::S3(err.to_string())
    }
}

fn map_copy_failure(key: &str, msg: String) -> StorageError {
    if msg.contains("PreconditionFailed")
        || msg.contains("412")
        || msg.contains("ConditionalRequestConflict")
    {
        StorageError::Integrity(format!("source `{key}` changed during copy: {msg}"))
    } else {
        map_missing(key, msg)
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

#[cfg(test)]
#[path = "s3_protocol.rs"]
pub(crate) mod s3_protocol;

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
