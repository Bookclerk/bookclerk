//! Work frames on the host-node socket (H-4 design, sections 3 and 5).
//!
//! Each request frame maps onto the library's remote-work calls
//! (`bookclerk_library::control_plane::claim_remote_job` and friends), which
//! run the existing routed claim, fenced heartbeat, and fenced
//! finalization as a `Claimant::Remote`:
//!
//! | Frame | Answer |
//! |---|---|
//! | `heartbeat` | `ok`, after `record_node_heartbeat` |
//! | `claim {item: job}` | `work {job, fence, sealed_secrets}` or `none` |
//! | `renew` | `renewed {cancel}` or `lost` |
//! | `complete` / `fail` | `ok` or `lost` |
//! | `secret.update` | `ok`, or `error {code: "refused"}` |
//!
//! A job's secrets are unsealed only after the handoff trust re-check and
//! are sealed at once to the node's enrolled X25519 key, bound to the node,
//! the job, and its fence ([`bookclerk_node_proto::seal_secrets`]). A token
//! the host writes back arrives sealed to this connection's owner box key
//! (sent in `welcome`), so no secret is ever frame text in either
//! direction, even where the WebSocket library traces frames. The
//! plaintext lives in zeroizing buffers and is never logged: log lines name
//! jobs and fences only. A connection keeps no work state; every frame is
//! checked against the database, so a host can resume on any owner node.

use std::time::Duration;

use bookclerk_library::control_plane::{
    claim_remote_job, finish_remote_job, record_node_heartbeat, release_remote_job,
    renew_remote_job, write_back_job_secret, JobSecretRef, NodeHeartbeat, RemoteClaimSettings,
    RemoteFinish, RemoteJobClaim, RemoteOutcome, RemoteRenew, WriteBack, SECRETS_UNAVAILABLE,
};
use bookclerk_library::{JobResourceClass, LibraryStore};
use bookclerk_node_proto::{
    open_secret_update, seal_secrets, Body, Frame, HeldFence, SecretEntry, SecretRef, SecretValue,
    WorkJob, ITEM_JOB, MAX_FRAME_BYTES,
};
use rand::RngCore as _;
use zeroize::{Zeroize as _, Zeroizing};

/// Seconds a host waits before claiming again after `none`.
pub(crate) const RETRY_AFTER_SECS: u64 = 5;

/// Bounds on the heartbeat interval a host may promise.
const HEARTBEAT_INTERVAL_SECS: std::ops::RangeInclusive<u64> = 5..=300;

/// Fences one hello may resume; any beyond are answered lost.
const MAX_RESUME_FENCES: usize = 256;

/// Longest progress, detail, or error text recorded from a host.
const MAX_TEXT_BYTES: usize = 4 * 1024;

/// One authenticated host connection's fixed facts.
pub(crate) struct HostContext {
    /// Authenticated node id.
    pub node_id: String,
    /// Incarnation from the hello.
    pub incarnation: String,
    /// The node's enrolled X25519 box key.
    pub box_public_key: Vec<u8>,
    /// This connection's owner box secret; its public key went out in
    /// `welcome`, and token write-backs are sealed to it.
    pub owner_box_secret: Zeroizing<[u8; 32]>,
    /// Lease and semaphore settings of this owner node.
    pub settings: RemoteClaimSettings,
}

/// An `error` answer.
fn error(id: String, code: &str, message: &str) -> Frame {
    Frame::new(
        id,
        Body::Error {
            code: code.into(),
            message: message.into(),
        },
    )
}

/// A `lost` answer for a job fence.
fn lost(id: String, job_id: String, fence: i64) -> Frame {
    Frame::new(
        id,
        Body::Lost {
            item: ITEM_JOB.into(),
            job_id,
            fence,
        },
    )
}

/// Text from a host, cut to [`MAX_TEXT_BYTES`] on a character boundary.
fn bounded(text: &str) -> &str {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The answer to one request frame; `error {code: "unsupported"}` for a
/// frame this owner does not serve.
pub(crate) async fn answer(library: &LibraryStore, host: &HostContext, frame: Frame) -> Frame {
    let id = frame.id;
    let result = match frame.body {
        Body::Heartbeat {
            started_at,
            interval_secs,
            cpu_millicores,
            memory_bytes,
            cpu_used_millicores,
            memory_available_bytes,
            files_dir_free_bytes,
        } => {
            let heartbeat = NodeHeartbeat {
                incarnation: host.incarnation.clone(),
                started_at: bounded(&started_at).to_string(),
                interval: Duration::from_secs(interval_secs.clamp(
                    *HEARTBEAT_INTERVAL_SECS.start(),
                    *HEARTBEAT_INTERVAL_SECS.end(),
                )),
                cpu_millicores: cpu_millicores.max(0),
                memory_bytes: memory_bytes.map(|v| v.max(0)),
                cpu_used_millicores: cpu_used_millicores.map(|v| v.max(0)),
                memory_available_bytes: memory_available_bytes.map(|v| v.max(0)),
                files_dir_free_bytes: files_dir_free_bytes.map(|v| v.max(0)),
            };
            record_node_heartbeat(library, &host.node_id, &heartbeat)
                .await
                .map(|_| Frame::new(id.clone(), Body::Accepted {}))
        }
        Body::Claim { item, class } => {
            if item != ITEM_JOB {
                return error(id, "unsupported", "only job claims are served");
            }
            let class = match class.as_deref() {
                None => JobResourceClass::Network,
                Some(raw) => match JobResourceClass::parse(raw) {
                    Some(class) => class,
                    None => return error(id, "invalid", "unknown resource class"),
                },
            };
            claim(library, host, id.clone(), class).await
        }
        Body::Renew {
            item,
            job_id,
            fence,
            progress,
        } => {
            if item != ITEM_JOB {
                return error(id, "unsupported", "only job leases are served");
            }
            let progress = progress.as_deref().map(bounded);
            match renew_remote_job(
                library,
                &host.node_id,
                &job_id,
                fence,
                host.settings,
                progress,
            )
            .await
            {
                Ok(RemoteRenew::Renewed { cancel }) => Ok(Frame::new(
                    id.clone(),
                    Body::Renewed {
                        lease_secs: host.settings.lease_secs,
                        cancel,
                    },
                )),
                Ok(RemoteRenew::Lost) => Ok(lost(id.clone(), job_id, fence)),
                Err(err) => Err(err),
            }
        }
        Body::Complete {
            job_id,
            fence,
            detail,
        } => {
            let outcome = RemoteOutcome::Complete(detail.as_deref().map(bounded));
            finish(library, host, id.clone(), job_id, fence, outcome).await
        }
        Body::Fail {
            job_id,
            fence,
            error_kind,
            message,
        } => {
            let outcome = RemoteOutcome::Fail {
                kind: bounded(&error_kind),
                message: bounded(&message),
            };
            finish(library, host, id.clone(), job_id, fence, outcome).await
        }
        Body::SecretUpdate {
            job_id,
            fence,
            secret_ref,
            sealed_value,
        } => {
            let Ok(value) = open_secret_update(
                &host.owner_box_secret,
                &host.node_id,
                &job_id,
                fence,
                &secret_ref,
                &sealed_value,
            ) else {
                tracing::warn!(
                    node_id = %host.node_id,
                    job_id = %job_id,
                    fence,
                    "host token write-back does not open for this connection"
                );
                return error(id, "refused", "sealed_value_invalid");
            };
            let secret_ref = JobSecretRef {
                kind: secret_ref.kind,
                provider: secret_ref.provider,
                account_id: secret_ref.account_id,
                name: secret_ref.name,
            };
            match write_back_job_secret(
                library,
                &host.node_id,
                &job_id,
                fence,
                &secret_ref,
                value.expose(),
            )
            .await
            {
                Ok(WriteBack::Written) => {
                    tracing::info!(
                        node_id = %host.node_id,
                        job_id = %job_id,
                        fence,
                        kind = %secret_ref.kind,
                        account_id = %secret_ref.account_id,
                        "host wrote back a refreshed store token"
                    );
                    Ok(Frame::new(id.clone(), Body::Accepted {}))
                }
                Ok(WriteBack::Refused(reason)) => {
                    tracing::warn!(
                        node_id = %host.node_id,
                        job_id = %job_id,
                        fence,
                        reason,
                        "host token write-back refused"
                    );
                    Ok(error(id.clone(), "refused", reason))
                }
                Err(err) => Err(err),
            }
        }
        _ => {
            return error(
                id,
                "unsupported",
                "this owner does not serve that frame type",
            )
        }
    };
    result.unwrap_or_else(|err| {
        tracing::warn!(node_id = %host.node_id, error = %err, "host work frame failed");
        error(
            id,
            "unavailable",
            "the owner could not serve this request now",
        )
    })
}

/// `claim`: the next trusted job, its fence, and its sealed secrets.
async fn claim(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    class: JobResourceClass,
) -> bookclerk_library::Result<Frame> {
    let none = || {
        Frame::new(
            id.clone(),
            Body::NoWork {
                retry_after_secs: RETRY_AFTER_SECS,
            },
        )
    };
    let Some(RemoteJobClaim {
        job,
        secrets,
        resumed,
    }) = claim_remote_job(library, &host.node_id, class, host.settings).await?
    else {
        return Ok(none());
    };
    let fence = job.lease_generation;
    let sealed = if secrets.is_empty() {
        Ok(None)
    } else {
        let entries: Vec<SecretEntry> = secrets
            .iter()
            .map(|secret| SecretEntry {
                secret_ref: SecretRef {
                    kind: secret.secret_ref.kind.clone(),
                    provider: secret.secret_ref.provider.clone(),
                    account_id: secret.secret_ref.account_id.clone(),
                    name: secret.secret_ref.name.clone(),
                },
                value: SecretValue::new(secret.value.to_vec()),
            })
            .collect();
        let mut ephemeral = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut ephemeral);
        let sealed = seal_secrets(
            &host.box_public_key,
            &ephemeral,
            &host.node_id,
            &job.id,
            fence,
            &entries,
        );
        ephemeral.zeroize();
        sealed.map(Some)
    };
    drop(secrets);
    let work = sealed.ok().map(|sealed_secrets| {
        Frame::new(
            id.clone(),
            Body::Work {
                item: ITEM_JOB.into(),
                job: WorkJob {
                    id: job.id.clone(),
                    kind: job.kind.as_str().into(),
                    payload: serde_json::to_value(&job.payload).unwrap_or_default(),
                    attempt: job.attempt_count,
                },
                fence,
                lease_secs: host.settings.lease_secs,
                sealed_secrets,
                resumed,
            },
        )
    });
    match work {
        Some(work) if work.to_text().len() <= MAX_FRAME_BYTES => {
            tracing::info!(
                node_id = %host.node_id,
                job_id = %job.id,
                fence,
                kind = job.kind.as_str(),
                "job handed to host node"
            );
            Ok(work)
        }
        _ => {
            // The bundle could not be sealed to this node's key, or does
            // not fit a frame: give the attempt back as a failure so the
            // job retries where it can run.
            tracing::warn!(
                node_id = %host.node_id,
                job_id = %job.id,
                fence,
                "job secrets could not be sealed for host node"
            );
            finish_remote_job(
                library,
                &host.node_id,
                &job.id,
                fence,
                host.settings.limits,
                RemoteOutcome::Fail {
                    kind: SECRETS_UNAVAILABLE,
                    message: "the job's secrets could not be sealed for a host node",
                },
            )
            .await?;
            Ok(none())
        }
    }
}

/// `complete` / `fail`.
async fn finish(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job_id: String,
    fence: i64,
    outcome: RemoteOutcome<'_>,
) -> bookclerk_library::Result<Frame> {
    Ok(
        match finish_remote_job(
            library,
            &host.node_id,
            &job_id,
            fence,
            host.settings.limits,
            outcome,
        )
        .await?
        {
            RemoteFinish::Done => Frame::new(id, Body::Accepted {}),
            RemoteFinish::Lost => lost(id, job_id, fence),
            RemoteFinish::Refused(reason) => error(id, "refused", reason),
        },
    )
}

/// Gives back a job claimed for a `work` answer that will not reach the
/// host (its connection was superseded or is closing), so the job is
/// claimable again at once instead of after its lease expires. Does
/// nothing for any other answer.
pub(crate) async fn release_unsent(library: &LibraryStore, host: &HostContext, answer: &Frame) {
    let Body::Work {
        job,
        fence,
        resumed,
        ..
    } = &answer.body
    else {
        return;
    };
    match release_remote_job(
        library,
        &host.node_id,
        &job.id,
        *fence,
        *resumed,
        host.settings.limits,
    )
    .await
    {
        Ok(released) => tracing::info!(
            node_id = %host.node_id,
            job_id = %job.id,
            fence,
            released,
            "job claimed on a closing host connection was given back"
        ),
        Err(err) => tracing::warn!(
            node_id = %host.node_id,
            job_id = %job.id,
            fence,
            error = %err,
            "job claimed on a closing host connection could not be given back; its lease will expire"
        ),
    }
}

/// Renews each fence a reconnecting host still holds.
///
/// Only job fences are served; any other kind is lost, as is every fence
/// beyond [`MAX_RESUME_FENCES`]. A fence that expired and was reclaimed
/// while the host was away, or that the node is no longer trusted with, is
/// lost, and every later frame under it is refused the same way. A fence
/// whose renewal failed with an error (a transient database fault) is
/// listed as unchecked, not lost: the host keeps the work and its next
/// `renew` decides, while the lease is still the one it held.
pub(crate) async fn resume(
    library: &LibraryStore,
    host: &HostContext,
    held: Vec<HeldFence>,
) -> Body {
    let mut renewed = Vec::new();
    let mut lost = Vec::new();
    let mut unchecked = Vec::new();
    for (index, fence) in held.into_iter().enumerate() {
        if index >= MAX_RESUME_FENCES || fence.kind != ITEM_JOB {
            lost.push(fence);
            continue;
        }
        match renew_remote_job(
            library,
            &host.node_id,
            &fence.id,
            fence.fence,
            host.settings,
            None,
        )
        .await
        {
            Ok(RemoteRenew::Renewed { .. }) => renewed.push(fence),
            Ok(RemoteRenew::Lost) => lost.push(fence),
            Err(err) => {
                tracing::warn!(
                    node_id = %host.node_id,
                    job_id = %fence.id,
                    fence = fence.fence,
                    error = %err,
                    "host lease could not be checked on resume"
                );
                unchecked.push(fence);
            }
        }
    }
    tracing::info!(
        node_id = %host.node_id,
        renewed = renewed.len(),
        lost = lost.len(),
        unchecked = unchecked.len(),
        "host node resumed its leases"
    );
    Body::Resumed {
        renewed,
        lost,
        unchecked,
    }
}
