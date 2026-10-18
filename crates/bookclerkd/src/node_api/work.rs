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
//! | `claim {item: job, filter}` | `work {job, fence, sealed_secrets, plugin_keys}` or `none` |
//! | `renew` | `renewed {cancel}` or `lost` |
//! | `complete` / `fail` | `ok` or `lost` |
//! | `release` | `ok` or `lost` (the job returns to pending unspent) |
//! | `secret.update` | `ok`, or `error {code: "refused"}` |
//! | `deployments.list` | `deployments {items}` |
//! | `instance.config` | `instance_config {config, sealed_secrets}`, or `error {code: "not_found"}` |
//! | `observation` | `ok`, or `error {code: "refused"}` |
//! | `scan.results` / `acquire.plan` / `upload.grant` | see [`super::host_results`] |
//!
//! A claim's `filter` is the host's own narrowing (the kinds it runs and
//! the plugins its operator consented to); the owner applies it before
//! permits and the CAS, so a job the host would refuse is never claimed.
//! The reads a host makes after a wake are scoped to the deployments that
//! target it; an instance's secrets go only to a node trusted with every
//! job (instance secrets are the cluster operator's), sealed to its box
//! key and bound to the instance and document revision.
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
    claim_remote_job, finish_remote_job, record_node_heartbeat, record_remote_observation,
    release_remote_job, remote_deployments_for_node, remote_instance_for_node, renew_remote_job,
    resolve_instance_bindings, write_back_job_secret, DeploymentStatus, JobClaimFilter,
    JobSecretRef, NodeHeartbeat, NodeTrust, PluginVersion, RemoteClaimSettings, RemoteFinish,
    RemoteJobClaim, RemoteObservation, RemoteOutcome, RemoteRenew, WriteBack, SECRETS_UNAVAILABLE,
};
use bookclerk_library::{JobResourceClass, LibraryStore};
use bookclerk_node_proto::{
    open_secret_update, seal_instance_secrets, seal_secrets, Body, ClaimFilter, Frame, HeldFence,
    RemoteArtifact, RemoteDeployment, RemoteVersion, SecretEntry, SecretRef, SecretValue, WorkJob,
    ITEM_JOB, MAX_FRAME_BYTES,
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

/// Most job kinds and plugin keys one claim filter may name.
const MAX_FILTER_ENTRIES: usize = 256;

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
    /// This owner node's id: uploads granted on this connection are
    /// received here.
    pub owner_node_id: String,
    /// The daemon, for this node's output destinations and settings.
    pub state: std::sync::Arc<crate::api::AppState>,
}

/// An `error` answer.
pub(super) fn error(id: String, code: &str, message: &str) -> Frame {
    Frame::new(
        id,
        Body::Error {
            code: code.into(),
            message: message.into(),
        },
    )
}

/// A `lost` answer for a job fence.
pub(super) fn lost(id: String, job_id: String, fence: i64) -> Frame {
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
        Body::Claim {
            item,
            class,
            filter,
        } => {
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
            let filter = match filter.map(claim_filter).transpose() {
                Ok(filter) => filter,
                Err(()) => return error(id, "invalid", "claim filter is too large"),
            };
            claim(library, host, id.clone(), class, filter.as_ref()).await
        }
        Body::Release {
            job_id,
            fence,
            reason,
        } => {
            tracing::info!(
                node_id = %host.node_id,
                job_id = %job_id,
                fence,
                reason = bounded(&reason),
                "host node gives a job back"
            );
            finish_release(library, host, id.clone(), job_id, fence).await
        }
        Body::DeploymentsList {} => deployments(library, host, id.clone()).await,
        Body::InstanceConfigGet { instance_id } => {
            instance_config(library, host, id.clone(), &instance_id).await
        }
        Body::Observation {
            deployment_id,
            status,
            detail,
            applied_config_revision,
        } => {
            let status = match DeploymentStatus::parse(&status) {
                Ok(status) => status,
                Err(_) => return error(id, "invalid", "unknown observation status"),
            };
            match record_remote_observation(
                library,
                &host.node_id,
                &host.incarnation,
                &RemoteObservation {
                    deployment_id: &deployment_id,
                    status,
                    detail: bounded(&detail),
                    applied_config_revision,
                },
            )
            .await
            {
                Ok(true) => Ok(Frame::new(id.clone(), Body::Accepted {})),
                Ok(false) => Ok(error(id.clone(), "refused", "not_your_deployment")),
                Err(err) => Err(err),
            }
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
        Body::ScanResults {
            job_id,
            fence,
            plugin_key,
            batch,
            books,
        } => {
            super::host_results::scan_results(
                library,
                host,
                id.clone(),
                job_id,
                fence,
                super::host_results::ScanBatch {
                    plugin_key: &plugin_key,
                    batch,
                    rows: books,
                },
            )
            .await
        }
        Body::AcquirePlan { job_id, fence } => {
            super::host_results::acquire_plan(library, host, id.clone(), job_id, fence).await
        }
        Body::UploadGrant {
            job_id,
            fence,
            book_uuid,
            primary_key,
            pdf_key,
            files,
        } => {
            super::host_results::upload_grant(
                library,
                host,
                id.clone(),
                job_id,
                fence,
                bookclerk_library::control_plane::UploadGrantRequest {
                    book_uuid: &book_uuid,
                    primary_key: &primary_key,
                    pdf_key: pdf_key.as_deref(),
                    files: &files,
                    destinations: &[],
                },
            )
            .await
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

/// The library filter for a host's `claim.filter`, or `Err` when it names
/// too many entries.
fn claim_filter(filter: ClaimFilter) -> Result<JobClaimFilter, ()> {
    if filter.kinds.len() > MAX_FILTER_ENTRIES || filter.plugins.len() > MAX_FILTER_ENTRIES {
        return Err(());
    }
    Ok(JobClaimFilter {
        kinds: filter.kinds.into_iter().collect(),
        plugin_keys: filter.plugins.into_iter().collect(),
    })
}

/// `claim`: the next trusted job, its fence, and its sealed secrets.
async fn claim(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    class: JobResourceClass,
    filter: Option<&JobClaimFilter>,
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
        plugin_keys,
    }) = claim_remote_job(library, &host.node_id, class, host.settings, filter).await?
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
                plugin_keys: plugin_keys.clone(),
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

/// `release`: the job goes back to pending unspent.
async fn finish_release(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job_id: String,
    fence: i64,
) -> bookclerk_library::Result<Frame> {
    Ok(
        match release_remote_job(library, &host.node_id, &job_id, fence, host.settings.limits)
            .await?
        {
            RemoteFinish::Done => Frame::new(id, Body::Accepted {}),
            RemoteFinish::Lost => lost(id, job_id, fence),
            RemoteFinish::Refused(reason) => error(id, "refused", reason),
        },
    )
}

/// A version as a host receives it.
fn remote_version(version: &PluginVersion) -> RemoteVersion {
    RemoteVersion {
        version: version.version.clone(),
        manifest_id: version.manifest_id.clone(),
        manifest_toml: version.manifest_toml.clone(),
        settings: serde_json::to_value(&version.settings).unwrap_or_default(),
        artifacts: version
            .artifacts
            .iter()
            .map(|artifact| RemoteArtifact {
                target: artifact.target.clone(),
                url: artifact.url.clone(),
                format: artifact.format.clone(),
                archive_root: artifact.archive_root.clone(),
                archive_sha256: artifact.archive_sha256.clone(),
                payload_root_sha256: artifact.payload_root_sha256.clone(),
                executable: artifact.executable.clone(),
            })
            .collect(),
        grant_json: version.grant_json.clone(),
        publisher_verified: version.publisher_verified,
    }
}

/// `deployments.list`: the deployments that target this host.
async fn deployments(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
) -> bookclerk_library::Result<Frame> {
    let items = remote_deployments_for_node(library, &host.node_id)
        .await?
        .into_iter()
        .map(|entry| RemoteDeployment {
            deployment_id: entry.deployment.deployment_id,
            instance_id: entry.deployment.plugin_instance_id.as_str().to_string(),
            plugin_key: entry.plugin_key,
            desired: entry.deployment.desired.as_str().to_string(),
            revision: entry.deployment.revision,
            default_instance: entry.default_instance,
            version: entry.version.as_ref().map(remote_version),
        })
        .collect();
    let frame = Frame::new(id.clone(), Body::Deployments { items });
    if frame.to_text().len() > MAX_FRAME_BYTES {
        tracing::warn!(node_id = %host.node_id, "host deployment list does not fit a frame");
        return Ok(error(
            id,
            "too_large",
            "the deployment list does not fit a frame",
        ));
    }
    Ok(frame)
}

/// `instance.config`: `CONFIG` and, for a node trusted with every job,
/// `SECRETS` sealed to its box key.
async fn instance_config(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    instance_id: &str,
) -> bookclerk_library::Result<Frame> {
    let not_found = || error(id.clone(), "not_found", "no such instance is deployed here");
    let Some(instance) = remote_instance_for_node(library, &host.node_id, instance_id).await?
    else {
        return Ok(not_found());
    };
    let Some(version) = instance.version.as_ref() else {
        return Ok(error(
            id,
            "not_installed",
            "the instance's plugin has no active version",
        ));
    };
    let Ok(grant) = bookclerk_plugin_host::version_grant(version) else {
        return Ok(error(
            id,
            "not_approved",
            "the instance's version is not approved",
        ));
    };
    let flags = bookclerk_plugin_host::binding_grant_flags(&grant);
    let resolved = match resolve_instance_bindings(
        library,
        &instance.deployment.plugin_instance_id,
        &flags,
        &version.settings,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(
                node_id = %host.node_id,
                instance_id,
                error = %err,
                "instance bindings could not be resolved for a host"
            );
            return Ok(error(
                id,
                "unresolved",
                "the instance's settings cannot be resolved",
            ));
        }
    };
    let revision = resolved.config_revision;
    let config = resolved.config.json_value().unwrap_or_default();
    let mut secrets = resolved.secrets;
    let has_secrets = !secrets.payload.is_empty()
        && secrets
            .json_value()
            .ok()
            .and_then(|value| value.as_object().map(|object| !object.is_empty()))
            .unwrap_or(true);
    let trusted = instance.trust == NodeTrust::All;
    let sealed = if has_secrets && trusted {
        let mut ephemeral = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut ephemeral);
        let sealed = seal_instance_secrets(
            &host.box_public_key,
            &ephemeral,
            &host.node_id,
            instance_id,
            revision,
            &secrets.payload,
        );
        ephemeral.zeroize();
        match sealed {
            Ok(sealed) => Some(sealed),
            Err(_) => {
                secrets.payload.zeroize();
                return Ok(error(
                    id,
                    "unavailable",
                    "secrets could not be sealed for this node",
                ));
            }
        }
    } else {
        None
    };
    secrets.payload.zeroize();
    let frame = Frame::new(
        id.clone(),
        Body::InstanceConfig {
            instance_id: instance_id.to_string(),
            revision,
            config,
            sealed_secrets: sealed,
            secrets_withheld: has_secrets && !trusted,
        },
    );
    if frame.to_text().len() > MAX_FRAME_BYTES {
        return Ok(error(
            id,
            "too_large",
            "the instance config does not fit a frame",
        ));
    }
    tracing::info!(
        node_id = %host.node_id,
        instance_id,
        revision,
        secrets = has_secrets && trusted,
        "instance config handed to host node"
    );
    Ok(frame)
}

/// Renews each fence a reconnecting host still holds; the rest are lost.
///
/// Only job fences are served; any other kind is lost. A fence that
/// expired and was reclaimed while the host was away, or that the node is
/// no longer trusted with, is lost, and every later frame under it is
/// refused the same way.
pub(crate) async fn resume(
    library: &LibraryStore,
    host: &HostContext,
    held: Vec<HeldFence>,
) -> Body {
    let mut renewed = Vec::new();
    let mut lost = Vec::new();
    for (index, fence) in held.into_iter().enumerate() {
        let kept = index < MAX_RESUME_FENCES
            && fence.kind == ITEM_JOB
            && matches!(
                renew_remote_job(
                    library,
                    &host.node_id,
                    &fence.id,
                    fence.fence,
                    host.settings,
                    None
                )
                .await,
                Ok(RemoteRenew::Renewed { .. })
            );
        if kept {
            renewed.push(fence);
        } else {
            lost.push(fence);
        }
    }
    tracing::info!(
        node_id = %host.node_id,
        renewed = renewed.len(),
        lost = lost.len(),
        "host node resumed its leases"
    );
    Body::Resumed { renewed, lost }
}
