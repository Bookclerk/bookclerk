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
    /// fences: which leases the owner renewed, which are lost, and which it
    /// could not check now. The host aborts lost work; every later frame
    /// under a lost fence is refused.
    Resumed {
        /// Leases renewed for this connection.
        #[serde(default)]
        renewed: Vec<HeldFence>,
        /// Leases that expired, were taken over, or are no longer trusted.
        #[serde(default)]
        lost: Vec<HeldFence>,
        /// Leases the owner could not check (a transient read or write
        /// failure). They are neither renewed nor lost: the host keeps the
        /// work and renews them as usual, and that `renew` decides.
        #[serde(default)]
        unchecked: Vec<HeldFence>,
        /// The lease, in seconds, the `renewed` fences now run for, counted
        /// from the host's `hello`. The owner's lease length may have
        /// changed while the host was away, so the host counts this rather
        /// than its original grant; absent when nothing was renewed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease_secs: Option<u64>,
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
        /// The claim resumed a suspended job: no attempt was spent, and the
        /// job's checkpoint applies.
        #[serde(default)]
        resumed: bool,
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
        /// The `resumed` flag of the job's `work` frame, so a resumed
        /// claim is given back resumable.
        #[serde(default)]
        resumed: bool,
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
                    resumed: false,
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
                    resumed: false,
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
                    unchecked: Vec::new(),
                    lease_secs: None,
                },
                "resumed",
            ),
        ] {
            let text = Frame::new("3", body.clone()).to_text();
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
