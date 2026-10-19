//! JSON frames `{v, id, type, ...}`.

use serde::{Deserialize, Serialize};

use crate::sealed::SecretRef;
use crate::{FRAME_VERSION, MAX_FRAME_BYTES};

/// A lease the host still holds when it reconnects (`hello.resume`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldFence {
    /// `job` or `delivery`.
    pub kind: String,
    /// Job id or delivery id.
    pub id: String,
    /// Lease generation the host holds.
    pub fence: i64,
}

/// What a frame says, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    /// Host → owner: first frame.
    Hello {
        /// Node id the owner minted at enrollment.
        node_id: String,
        /// Host process incarnation (new on each start).
        incarnation: String,
        /// Host's Bookclerk version.
        #[serde(default)]
        software_version: String,
        /// Leases the host still holds from an earlier connection.
        #[serde(default)]
        resume: Vec<HeldFence>,
    },
    /// Owner → host: the nonce to sign.
    Challenge {
        /// 32 random bytes, base64url without padding.
        nonce: String,
        /// Owner cluster id (also signed).
        cluster_id: String,
    },
    /// Host → owner: proof of key possession.
    Auth {
        /// Ed25519 signature over [`crate::auth_transcript`], base64url
        /// without padding.
        sig: String,
    },
    /// Owner → host: the handshake succeeded.
    Welcome {
        /// Authenticated node id.
        node_id: String,
        /// Owner cluster id.
        cluster_id: String,
        /// The owner's X25519 box key for this connection, base64url
        /// without padding. The host seals token write-backs to it
        /// (`crate::seal_secret_update`), so no secret travels as frame
        /// text either way.
        #[serde(default)]
        box_key: String,
    },
    /// Owner → host, right after [`Body::Welcome`] when the hello listed
    /// fences: which leases the owner renewed and which are lost. The host
    /// aborts lost work; every later frame under a lost fence is refused.
    Resumed {
        /// Leases renewed for this connection.
        #[serde(default)]
        renewed: Vec<HeldFence>,
        /// Leases that expired, were taken over, or are no longer trusted.
        #[serde(default)]
        lost: Vec<HeldFence>,
    },
    /// Either side: a request was not served. The connection stays open.
    Error {
        /// Stable code (`unsupported`, `invalid`, ...).
        code: String,
        /// Human-readable detail.
        #[serde(default)]
        message: String,
    },
    /// Host → owner: the host's own measurements, recorded in
    /// `node_status` like any node's heartbeat. Answered with
    /// [`Body::Accepted`].
    Heartbeat {
        /// RFC 3339 time the host daemon started.
        #[serde(default)]
        started_at: String,
        /// Promised heartbeat interval in seconds.
        interval_secs: u64,
        /// Detected CPU capacity in millicores.
        cpu_millicores: i64,
        /// Detected memory in bytes.
        #[serde(default)]
        memory_bytes: Option<i64>,
        /// Measured CPU in use, in millicores.
        #[serde(default)]
        cpu_used_millicores: Option<i64>,
        /// Measured memory available to new work.
        #[serde(default)]
        memory_available_bytes: Option<i64>,
        /// Free bytes on the host's files directory.
        #[serde(default)]
        files_dir_free_bytes: Option<i64>,
    },
    /// Host → owner: pull one work item. Answered with [`Body::Work`] or
    /// [`Body::NoWork`].
    Claim {
        /// [`ITEM_JOB`] (deliveries are not served yet).
        item: String,
        /// Job resource class; `network` when absent.
        #[serde(default)]
        class: Option<String>,
        /// The host's own narrowing: only jobs of these kinds whose every
        /// plugin its operator consented to. Absent claims everything the
        /// node is trusted with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<ClaimFilter>,
    },
    /// Owner → host: a claimed job, its lease, and its sealed secrets.
    Work {
        /// [`ITEM_JOB`].
        item: String,
        /// The job.
        job: WorkJob,
        /// Lease generation; every later frame for the job names it.
        fence: i64,
        /// Lease length; renew well within it.
        lease_secs: u64,
        /// The job's secrets sealed to the node's box key, bound to the
        /// node, job and fence (`crate::seal_secrets`). Absent when the job
        /// needs none.
        #[serde(default)]
        sealed_secrets: Option<String>,
        /// Plugin keys the job runs; the host runs it only when it still
        /// consents to every one.
        #[serde(default)]
        plugin_keys: Vec<String>,
        /// The one plugin of `plugin_keys` whose source guest runs the
        /// job (the others are destinations it writes); the owner hands a
        /// host only jobs that name one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_plugin_key: Option<String>,
    },
    /// Owner → host: nothing to claim now.
    #[serde(rename = "none")]
    NoWork {
        /// Seconds to wait before claiming again.
        retry_after_secs: u64,
    },
    /// Host → owner: extend a lease. Answered with [`Body::Renewed`] or
    /// [`Body::Lost`].
    Renew {
        /// [`ITEM_JOB`].
        item: String,
        /// Job id.
        job_id: String,
        /// Lease generation the host holds.
        fence: i64,
        /// Progress text to record.
        #[serde(default)]
        progress: Option<String>,
    },
    /// Owner → host: the lease was extended.
    Renewed {
        /// Lease length from now.
        lease_secs: u64,
        /// The operator asked to cancel; stop and report `fail`.
        #[serde(default)]
        cancel: bool,
    },
    /// Owner → host: the fence no longer owns the item; abort it and drop
    /// its secrets.
    Lost {
        /// [`ITEM_JOB`].
        item: String,
        /// Job id.
        job_id: String,
        /// The fence that was refused.
        fence: i64,
    },
    /// Host → owner: the job finished. Answered with [`Body::Accepted`] or
    /// [`Body::Lost`].
    Complete {
        /// Job id.
        job_id: String,
        /// Lease generation the host holds.
        fence: i64,
        /// Outcome text recorded as the job's progress.
        #[serde(default)]
        detail: Option<String>,
    },
    /// Host → owner: the job failed. Answered with [`Body::Accepted`] or
    /// [`Body::Lost`].
    Fail {
        /// Job id.
        job_id: String,
        /// Lease generation the host holds.
        fence: i64,
        /// Stable error kind.
        error_kind: String,
        /// Operator-facing message (never a secret).
        #[serde(default)]
        message: String,
    },
    /// Host → owner: give a job back unrun (its operator no longer
    /// consents to the job's plugin). The job returns to pending without
    /// spending the attempt. Answered with [`Body::Accepted`] or
    /// [`Body::Lost`].
    Release {
        /// Job id.
        job_id: String,
        /// Lease generation the host holds.
        fence: i64,
        /// Stable reason (`awaiting_host_consent`, ...), for the log.
        #[serde(default)]
        reason: String,
    },
    /// Host → owner: the deployments that target this host and should run.
    /// Answered with [`Body::Deployments`].
    #[serde(rename = "deployments.list")]
    DeploymentsList {},
    /// Owner → host: the answer to [`Body::DeploymentsList`].
    Deployments {
        /// One entry per deployment, in deployment-id order.
        items: Vec<RemoteDeployment>,
    },
    /// Host → owner: `CONFIG` and `SECRETS` of an instance deployed to this
    /// host. Answered with [`Body::InstanceConfig`], or `error {code:
    /// "not_found"}` for an instance not deployed here.
    #[serde(rename = "instance.config")]
    InstanceConfigGet {
        /// Plugin instance id.
        instance_id: String,
    },
    /// Owner → host: the answer to [`Body::InstanceConfigGet`].
    InstanceConfig {
        /// Plugin instance id.
        instance_id: String,
        /// Instance document revision these values apply.
        revision: i64,
        /// `CONFIG`: schema defaults overlaid with the document.
        config: serde_json::Value,
        /// `SECRETS` as a JSON object, sealed to the node's box key and bound
        /// to the node, instance and revision
        /// (`crate::seal_instance_secrets`). Absent when the instance has
        /// none or they were withheld.
        #[serde(default)]
        sealed_secrets: Option<String>,
        /// True when the instance has secrets this node is not trusted
        /// with (it receives them only when its owner may see every job's
        /// secrets).
        #[serde(default)]
        secrets_withheld: bool,
    },
    /// Host → owner: the host's observation of one of its deployments
    /// (`materialized`, or `error` with a detail such as
    /// `awaiting_host_consent`). Answered with [`Body::Accepted`].
    Observation {
        /// Deployment id.
        deployment_id: String,
        /// `materialized` or `error`.
        status: String,
        /// Bounded detail.
        #[serde(default)]
        detail: String,
        /// Instance document revision the host applied.
        #[serde(default)]
        applied_config_revision: Option<i64>,
    },
    /// Host → owner: write back a refreshed store token. Accepted only for a
    /// secret handed out with the job under this still-live fence.
    #[serde(rename = "secret.update")]
    SecretUpdate {
        /// Job the secret was handed with.
        job_id: String,
        /// Lease generation it was handed under.
        fence: i64,
        /// The secret row.
        #[serde(rename = "ref")]
        secret_ref: SecretRef,
        /// The new value sealed to the connection's owner box key
        /// ([`Body::Welcome`]), bound to the node, job, fence and ref
        /// (`crate::seal_secret_update`).
        sealed_value: String,
    },
    /// Host → owner: one batch of a scan's book rows (decision 6). The
    /// owner checks the fence, the node's trust with the job, and every
    /// row's account before it writes any row to its library, on every
    /// batch. Answered with [`Body::ScanAccepted`], [`Body::Lost`], or
    /// `error {code: "refused"}`.
    #[serde(rename = "scan.results")]
    ScanResults {
        /// Job the rows belong to.
        job_id: String,
        /// Lease generation the host holds.
        fence: i64,
        /// Plugin key whose guest produced the rows (one of the job's).
        plugin_key: String,
        /// Batch number, from 1, for the logs.
        batch: u64,
        /// Book rows as the guest's `ScanBook` (camelCase JSON objects);
        /// at most [`MAX_SCAN_BATCH_ROWS`].
        books: Vec<serde_json::Value>,
    },
    /// Owner → host: a results batch was written.
    #[serde(rename = "scan.accepted")]
    ScanAccepted {
        /// The batch number.
        batch: u64,
        /// Rows written.
        upserted: u64,
    },
    /// Host → owner: what an acquire job should fetch here. Answered with
    /// [`Body::AcquireTargets`], [`Body::Lost`], or `error`.
    #[serde(rename = "acquire.plan")]
    AcquirePlan {
        /// The acquire job.
        job_id: String,
        /// Lease generation the host holds.
        fence: i64,
    },
    /// Owner → host: the titles to acquire and how to package them.
    #[serde(rename = "acquire.targets")]
    AcquireTargets {
        /// Titles still to acquire, each one of the job's accounts.
        targets: Vec<AcquireTarget>,
        /// Every output destination deployed on the answering owner node,
        /// the primary first, each with its own packaging and naming
        /// options, so the host names each destination's files as a local
        /// acquire would.
        destinations: Vec<PlanDestination>,
    },
    /// Host → owner: ask to upload one title's files. Answered with
    /// [`Body::UploadGranted`], [`Body::Lost`], or `error`.
    #[serde(rename = "upload.grant")]
    UploadGrant {
        /// The acquire job.
        job_id: String,
        /// Lease generation the host holds.
        fence: i64,
        /// Library id of the title the files are for (one of the job's
        /// targets).
        book_uuid: String,
        /// Storage key of the title's primary audio file on the primary
        /// destination (one of `files`' copies there).
        primary_key: String,
        /// Storage key of the title's companion PDF on the primary
        /// destination (one of `files`' copies there), when one is
        /// uploaded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pdf_key: Option<String>,
        /// Every file, in upload order. Empty, with an empty `book_uuid`,
        /// for the job's closing upload, whose commit only completes the
        /// job.
        files: Vec<UploadFile>,
    },
    /// Owner → host: the upload is open.
    #[serde(rename = "upload.granted")]
    UploadGranted {
        /// Upload id; the files go to `UPLOADS_PATH<upload_id>/files/<n>`.
        upload_id: String,
        /// The bearer token for the upload endpoint, sealed to the node's
        /// box key and bound to the node, job, fence and upload
        /// (`crate::seal_upload_token`).
        sealed_token: String,
        /// Seconds the token stays valid unless extended: the owner
        /// extends it while the job's fence is live (each renewal and each
        /// accepted file).
        expires_in_secs: u64,
    },
    /// Owner → host: a message from the host's `node.<id>` queue. The host
    /// answers [`Body::Ack`] with the same frame id once it has acted.
    Wake {
        /// The queue message (`{"type": "deployment.changed", ...}`).
        message: serde_json::Value,
    },
    /// Host → owner: the wake with this frame id was handled.
    Ack {},
    /// Owner → host: the request with this frame id was applied.
    #[serde(rename = "ok")]
    Accepted {},
    /// A frame type this side does not know.
    #[serde(other)]
    Unknown,
}

/// Work item kind for jobs.
pub const ITEM_JOB: &str = "job";

/// What a host is willing to claim (`claim.filter`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimFilter {
    /// Job kinds the host runs.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Plugin keys the host's operator consented to.
    #[serde(default)]
    pub plugins: Vec<String>,
}

/// One deployment that targets a host (`deployments.items`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDeployment {
    /// Deployment id.
    pub deployment_id: String,
    /// Plugin instance the deployment runs.
    pub instance_id: String,
    /// The instance's plugin key.
    pub plugin_key: String,
    /// Desired state (`present`, `placed`).
    pub desired: String,
    /// Desired-row revision.
    pub revision: i64,
    /// True for the plugin's default instance (unpinned jobs use it).
    #[serde(default)]
    pub default_instance: bool,
    /// The plugin's active version; absent when none is installed.
    #[serde(default)]
    pub version: Option<RemoteVersion>,
}

/// The active version of a plugin a host is asked to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteVersion {
    /// Semver version.
    pub version: String,
    /// Manifest id (display alias).
    pub manifest_id: String,
    /// `plugin.toml` text.
    pub manifest_toml: String,
    /// Settings schema the version declares (JSON).
    pub settings: serde_json::Value,
    /// Per-target artifacts.
    pub artifacts: Vec<RemoteArtifact>,
    /// The grant the cluster's operator approved (JSON); empty while the
    /// version is unapproved, in which case nothing runs.
    #[serde(default)]
    pub grant_json: String,
    /// True when the publisher was verified at install.
    #[serde(default)]
    pub publisher_verified: bool,
}

/// One downloadable artifact of a [`RemoteVersion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteArtifact {
    /// Bookclerk target (`linux-x64-gnu`, `any`, ...).
    pub target: String,
    /// `https:` URL, or `blob:sha256:<hex>` for an archive the owner stores
    /// (fetched from the owner's artifact endpoint, [`crate::ARTIFACTS_PATH`]).
    pub url: String,
    /// Archive format.
    pub format: String,
    /// Path inside the archive to the plugin root.
    pub archive_root: String,
    /// SHA-256 of the archive.
    pub archive_sha256: String,
    /// Payload-root digest of the extracted tree.
    pub payload_root_sha256: String,
    /// Executable inside the plugin root.
    pub executable: String,
}

/// A job handed to a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkJob {
    /// Job id.
    pub id: String,
    /// Job kind (`scan`, `acquire`, ...).
    pub kind: String,
    /// Kind-specific payload, as the owner stores it.
    pub payload: serde_json::Value,
    /// Claims of this job so far, this one included.
    pub attempt: i64,
}

/// Most rows one [`Body::ScanResults`] batch may carry.
pub const MAX_SCAN_BATCH_ROWS: usize = 200;

/// Largest serialized row in a [`Body::ScanResults`] batch.
pub const MAX_SCAN_ROW_BYTES: usize = 32 * 1024;

/// Most files one upload may declare.
pub const MAX_UPLOAD_FILES: usize = 512;

/// Largest file one upload may declare (8 GiB).
pub const MAX_UPLOAD_FILE_BYTES: u64 = 8 << 30;

/// Longest storage key an upload may declare.
pub const MAX_UPLOAD_KEY_BYTES: usize = 1024;

/// Most destinations one uploaded file may be written to.
pub const MAX_UPLOAD_COPIES: usize = 16;

/// Longest destination id.
pub const MAX_DESTINATION_ID_BYTES: usize = 64;

/// One title an acquire job should fetch on a host
/// ([`Body::AcquireTargets`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquireTarget {
    /// Library id of the title.
    pub book_uuid: String,
    /// Plugin key of the store that sells it.
    pub plugin_key: String,
    /// Store account that owns it.
    pub account_id: String,
    /// The owner's library row (JSON), for naming and metadata.
    pub book: serde_json::Value,
}

/// One output destination of the owner node that answered an acquire plan
/// ([`Body::AcquireTargets`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDestination {
    /// Destination id on that owner node (its output kind, such as `local`
    /// or `s3`; one destination per kind).
    pub id: String,
    /// That destination's `DownloadOptions` (JSON): its naming templates,
    /// replacement characters and path limits.
    pub options: serde_json::Value,
}

/// True when `id` is a destination id: a short lowercase token.
#[must_use]
pub fn is_destination_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_DESTINATION_ID_BYTES
        && id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// True when `key` is a relative storage key: no empty, `.` or `..`
/// segment, no backslash or control character, not absolute, at most
/// [`MAX_UPLOAD_KEY_BYTES`].
#[must_use]
pub fn is_storage_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_UPLOAD_KEY_BYTES
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key.chars().any(char::is_control)
        && key
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Where one uploaded file is written: a destination and its key there.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCopy {
    /// A [`PlanDestination::id`].
    pub destination: String,
    /// Storage key on that destination.
    pub key: String,
}

/// One file of an upload ([`Body::UploadGrant`]). Its content crosses the
/// network once and is written to every copy, each destination under the
/// key that destination's own naming options give it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadFile {
    /// Where the file is written; at most one copy per destination.
    pub copies: Vec<FileCopy>,
    /// Exact size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    /// MIME type to store with it.
    #[serde(default)]
    pub content_type: Option<String>,
}

impl UploadFile {
    /// True when the file has between one and [`MAX_UPLOAD_COPIES`]
    /// copies, each naming a different destination id
    /// ([`is_destination_id`]) and a storage key ([`is_storage_key`]); the
    /// size is within [`MAX_UPLOAD_FILE_BYTES`]; and the digest is
    /// lowercase hex SHA-256.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let mut destinations = std::collections::BTreeSet::new();
        let copies_ok = !self.copies.is_empty()
            && self.copies.len() <= MAX_UPLOAD_COPIES
            && self.copies.iter().all(|copy| {
                is_destination_id(&copy.destination)
                    && is_storage_key(&copy.key)
                    && destinations.insert(copy.destination.as_str())
            });
        let digest_ok = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let type_ok = self
            .content_type
            .as_deref()
            .is_none_or(|t| t.len() <= 128 && !t.chars().any(char::is_control));
        copies_ok && digest_ok && type_ok && self.size <= MAX_UPLOAD_FILE_BYTES
    }

    /// This file's key on `destination`, if it is written there.
    #[must_use]
    pub fn key_on(&self, destination: &str) -> Option<&str> {
        self.copies
            .iter()
            .find(|copy| copy.destination == destination)
            .map(|copy| copy.key.as_str())
    }
}

/// Body of `POST <UPLOADS_PATH><upload_id>/commit`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadCommitRequest {
    /// Complete the job with this commit (the job's closing upload).
    #[serde(default)]
    pub finish: bool,
    /// Outcome text recorded as the job's progress when `finish` is set.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Answer of the upload endpoint (every status carries one).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadAnswer {
    /// Stable outcome: `received`, `committed`, `lost`, `refused`,
    /// `misdirected`, `unauthorized`, `unavailable`.
    pub status: String,
    /// Why, for `refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For `committed`: the job is finished.
    #[serde(default)]
    pub job_completed: bool,
}

/// One frame: the envelope and its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    /// Envelope version; always [`FRAME_VERSION`].
    pub v: u32,
    /// Request id, echoed by the answer.
    pub id: String,
    /// The body, tagged by `type`.
    #[serde(flatten)]
    pub body: Body,
}

/// Why a frame was not accepted.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    /// More than [`MAX_FRAME_BYTES`].
    #[error("frame exceeds {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    /// Not a JSON frame of the expected shape.
    #[error("malformed frame")]
    Malformed,
    /// `v` is not [`FRAME_VERSION`].
    #[error("unsupported frame version {0}")]
    Version(u32),
}

impl Frame {
    /// A version-1 frame.
    #[must_use]
    pub fn new(id: impl Into<String>, body: Body) -> Self {
        Self {
            v: FRAME_VERSION,
            id: id.into(),
            body,
        }
    }

    /// Parses a text frame, checking size and version.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError`] for an oversized, malformed, or wrong-version
    /// frame.
    pub fn parse(text: &str) -> Result<Self, FrameError> {
        if text.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let frame: Self = serde_json::from_str(text).map_err(|_| FrameError::Malformed)?;
        if frame.v != FRAME_VERSION {
            return Err(FrameError::Version(frame.v));
        }
        Ok(frame)
    }

    /// Serializes the frame as JSON text.
    #[must_use]
    pub fn to_text(&self) -> String {
        // A frame is plain strings and numbers; serializing cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_files_must_be_relative_keys_with_hex_digests() {
        let copy = |destination: &str, key: &str| FileCopy {
            destination: destination.into(),
            key: key.into(),
        };
        let file = |key: &str| UploadFile {
            copies: vec![copy("local", key)],
            size: 1,
            sha256: "0f".repeat(32),
            content_type: Some("audio/mp4".into()),
        };
        assert!(file("Author/Title/Title.m4b").is_well_formed());
        for bad in ["", "/abs.m4b", "a/../b.m4b", "a//b", "./a", "a\\b", "a\nb"] {
            assert!(!file(bad).is_well_formed(), "{bad:?}");
        }
        let mut upper = file("a.m4b");
        upper.sha256 = "0F".repeat(32);
        assert!(!upper.is_well_formed());
        let mut huge = file("a.m4b");
        huge.size = MAX_UPLOAD_FILE_BYTES + 1;
        assert!(!huge.is_well_formed());

        let mut two = file("Author/Title/Title.m4b");
        two.copies.push(copy("s3", "Title/Title.m4b"));
        assert!(two.is_well_formed(), "each destination has its own key");
        assert_eq!(two.key_on("s3"), Some("Title/Title.m4b"));
        assert_eq!(two.key_on("local"), Some("Author/Title/Title.m4b"));
        assert_eq!(two.key_on("other"), None);
        let mut twice = file("a.m4b");
        twice.copies.push(copy("local", "b.m4b"));
        assert!(!twice.is_well_formed(), "one copy per destination");
        let mut none = file("a.m4b");
        none.copies.clear();
        assert!(!none.is_well_formed(), "a file goes somewhere");
        for bad in ["", "Local", "s3/x", "a b"] {
            assert!(!UploadFile {
                copies: vec![copy(bad, "a.m4b")],
                ..file("a.m4b")
            }
            .is_well_formed());
        }
    }

    #[test]
    fn frames_round_trip_and_unknown_types_parse() {
        let hello = Frame::new(
            "1",
            Body::Hello {
                node_id: "n".into(),
                incarnation: "i".into(),
                software_version: "1.0.0".into(),
                resume: vec![HeldFence {
                    kind: "job".into(),
                    id: "7".into(),
                    fence: 3,
                }],
            },
        );
        let text = hello.to_text();
        assert!(text.contains(r#""type":"hello""#), "{text}");
        assert_eq!(Frame::parse(&text).unwrap(), hello);

        let unknown = Frame::parse(r#"{"v":1,"id":"2","type":"kv.get","key":"k"}"#).unwrap();
        assert_eq!(unknown.body, Body::Unknown);
        let claim = Frame::parse(r#"{"v":1,"id":"2","type":"claim","item":"job"}"#).unwrap();
        assert_eq!(
            claim.body,
            Body::Claim {
                item: ITEM_JOB.into(),
                class: None,
                filter: None,
            }
        );
        let job_id = || "scan-1".to_string();
        for (body, tag) in [
            (
                Body::NoWork {
                    retry_after_secs: 5,
                },
                "none",
            ),
            (Body::Accepted {}, "ok"),
            (Body::Ack {}, "ack"),
            (
                Body::Renew {
                    item: ITEM_JOB.into(),
                    job_id: job_id(),
                    fence: 2,
                    progress: None,
                },
                "renew",
            ),
            (
                Body::Lost {
                    item: ITEM_JOB.into(),
                    job_id: job_id(),
                    fence: 2,
                },
                "lost",
            ),
            (
                Body::Complete {
                    job_id: job_id(),
                    fence: 2,
                    detail: Some("d".into()),
                },
                "complete",
            ),
            (
                Body::Fail {
                    job_id: job_id(),
                    fence: 2,
                    error_kind: "k".into(),
                    message: "m".into(),
                },
                "fail",
            ),
            (
                Body::Work {
                    item: ITEM_JOB.into(),
                    job: WorkJob {
                        id: job_id(),
                        kind: "scan".into(),
                        payload: serde_json::json!({"account": "a"}),
                        attempt: 1,
                    },
                    fence: 2,
                    lease_secs: 60,
                    sealed_secrets: Some("x".into()),
                    plugin_keys: vec!["store".into()],
                    source_plugin_key: Some("store".into()),
                },
                "work",
            ),
            (
                Body::Claim {
                    item: ITEM_JOB.into(),
                    class: None,
                    filter: Some(ClaimFilter {
                        kinds: vec!["scan".into()],
                        plugins: vec!["store".into()],
                    }),
                },
                "claim",
            ),
            (
                Body::Release {
                    job_id: job_id(),
                    fence: 2,
                    reason: "awaiting_host_consent".into(),
                },
                "release",
            ),
            (Body::DeploymentsList {}, "deployments.list"),
            (
                Body::Deployments {
                    items: vec![RemoteDeployment {
                        deployment_id: "d".into(),
                        instance_id: "i".into(),
                        plugin_key: "store".into(),
                        desired: "present".into(),
                        revision: 1,
                        default_instance: true,
                        version: None,
                    }],
                },
                "deployments",
            ),
            (
                Body::InstanceConfigGet {
                    instance_id: "i".into(),
                },
                "instance.config",
            ),
            (
                Body::InstanceConfig {
                    instance_id: "i".into(),
                    revision: 3,
                    config: serde_json::json!({"region": "us"}),
                    sealed_secrets: None,
                    secrets_withheld: true,
                },
                "instance_config",
            ),
            (
                Body::Observation {
                    deployment_id: "d".into(),
                    status: "error".into(),
                    detail: "awaiting_host_consent".into(),
                    applied_config_revision: None,
                },
                "observation",
            ),
            (
                Body::Resumed {
                    renewed: Vec::new(),
                    lost: vec![HeldFence {
                        kind: ITEM_JOB.into(),
                        id: job_id(),
                        fence: 1,
                    }],
                },
                "resumed",
            ),
        ] {
            let text = Frame::new("3", body.clone()).to_text();
            assert!(text.contains(&format!(r#""type":"{tag}""#)), "{text}");
            assert_eq!(Frame::parse(&text).unwrap().body, body);
        }
        for (body, tag) in [
            (
                Body::ScanResults {
                    job_id: job_id(),
                    fence: 2,
                    plugin_key: "store".into(),
                    batch: 1,
                    books: vec![serde_json::json!({"productId": "p"})],
                },
                "scan.results",
            ),
            (
                Body::ScanAccepted {
                    batch: 1,
                    upserted: 1,
                },
                "scan.accepted",
            ),
            (
                Body::AcquirePlan {
                    job_id: job_id(),
                    fence: 2,
                },
                "acquire.plan",
            ),
            (
                Body::AcquireTargets {
                    targets: vec![AcquireTarget {
                        book_uuid: "b".into(),
                        plugin_key: "store".into(),
                        account_id: "a".into(),
                        book: serde_json::json!({}),
                    }],
                    destinations: vec![PlanDestination {
                        id: "local".into(),
                        options: serde_json::json!({}),
                    }],
                },
                "acquire.targets",
            ),
            (
                Body::UploadGrant {
                    job_id: job_id(),
                    fence: 2,
                    book_uuid: "b".into(),
                    primary_key: "A/B.m4b".into(),
                    pdf_key: None,
                    files: vec![UploadFile {
                        copies: vec![FileCopy {
                            destination: "local".into(),
                            key: "A/B.m4b".into(),
                        }],
                        size: 3,
                        sha256: "ab".repeat(32),
                        content_type: None,
                    }],
                },
                "upload.grant",
            ),
            (
                Body::UploadGranted {
                    upload_id: "u".into(),
                    sealed_token: "x".into(),
                    expires_in_secs: 3600,
                },
                "upload.granted",
            ),
        ] {
            let text = Frame::new("4", body.clone()).to_text();
            assert!(text.contains(&format!(r#""type":"{tag}""#)), "{text}");
            assert_eq!(Frame::parse(&text).unwrap().body, body);
        }
        assert_eq!(
            Frame::parse(r#"{"v":2,"id":"2","type":"auth","sig":"x"}"#),
            Err(FrameError::Version(2))
        );
        assert_eq!(Frame::parse("[]"), Err(FrameError::Malformed));
        let huge = format!(
            r#"{{"v":1,"id":"{}","type":"auth","sig":""}}"#,
            "x".repeat(MAX_FRAME_BYTES)
        );
        assert_eq!(Frame::parse(&huge), Err(FrameError::TooLarge));
    }

    #[test]
    fn a_secret_update_carries_only_a_sealed_value() {
        let owner_secret = [3u8; 32];
        let secret_ref = SecretRef {
            kind: "source_auth".into(),
            provider: "store".into(),
            account_id: "acct".into(),
            name: "acct.auth".into(),
        };
        let sealed_value = crate::seal_secret_update(
            &crate::box_public_key(&owner_secret),
            &[4; 32],
            "node",
            "scan-1",
            2,
            &secret_ref,
            b"refreshed-token",
        )
        .unwrap();
        let update = Frame::new(
            "9",
            Body::SecretUpdate {
                job_id: "scan-1".into(),
                fence: 2,
                secret_ref: secret_ref.clone(),
                sealed_value,
            },
        );
        let text = update.to_text();
        assert!(text.contains(r#""type":"secret.update""#), "{text}");
        assert!(text.contains(r#""ref":{"#), "{text}");
        assert!(!text.contains("refreshed-token"), "{text}");
        let parsed = Frame::parse(&text).unwrap();
        assert_eq!(parsed, update);
        let Body::SecretUpdate { sealed_value, .. } = parsed.body else {
            unreachable!()
        };
        let opened = crate::open_secret_update(
            &owner_secret,
            "node",
            "scan-1",
            2,
            &secret_ref,
            &sealed_value,
        )
        .unwrap();
        assert_eq!(opened.expose(), b"refreshed-token");
        // Bound to the job, fence and ref it was sealed for.
        assert!(crate::open_secret_update(
            &owner_secret,
            "node",
            "scan-1",
            3,
            &secret_ref,
            &sealed_value
        )
        .is_err());
    }
}
