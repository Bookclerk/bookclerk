//! The host side of the node API (H-4 design, sections 2 to 4; step 7).
//!
//! A host node is a separate Bookclerk install that enrolled into an owner
//! cluster with a single-use link ([`join_cluster`]) or a sign-in at the
//! owner ([`inspect_library`], [`authorize_branch`],
//! [`redeem_branch_code`]); it tells the owner when it leaves
//! ([`announce_leaving`]). Its node keys are
//! sealed in its own database
//! (`bookclerk_library::control_plane::record_cluster_link`). For every
//! active link, [`HostClient`] keeps one socket to the owner's
//! `/api/node/v1/connect`:
//!
//! - TLS everywhere except loopback, verified against the link's SPKI pin
//!   or the webpki roots ([`OwnerEndpoint`]);
//! - the Ed25519 challenge signed with the enrolled key;
//! - heartbeats into the owner's `node_status`;
//! - claimed jobs run through a [`HostJobRunner`], their secrets opened
//!   with the enrolled X25519 key, renewed and reported under the fence;
//! - refreshed store tokens written back sealed to the connection's owner
//!   box key ([`HostJobCtx::write_back`]);
//! - wakes acked after local subscribers have them;
//! - reconnect with backoff, resuming the fences still held.
//!
//! - claims narrowed to the job kinds this host runs and the plugins its
//!   operator consented to ([`HostClientHandle::set_consented_plugins`]);
//!   a job whose consent is revoked is cancelled and given back unspent;
//! - the owner reads a host acts on after a wake
//!   ([`HostClientHandle::request`] for `deployments.list`,
//!   `instance.config` and `observation`; [`HostClientHandle::fetch_artifact`]
//!   for a stored archive, signed with the node key).
//!
//! - a scan's rows returned in fenced batches
//!   ([`HostJobCtx::send_scan_results`]); an acquire's titles asked for
//!   ([`HostJobCtx::acquire_plan`]) and its files streamed to the owner
//!   under a sealed upload token ([`HostJobCtx::grant_upload`],
//!   [`HostJobCtx::upload_file`], [`HostJobCtx::commit_upload`]); the job
//!   completes with its closing upload's commit.
//!
//! The consent decisions themselves (which plugins, which grant) and how a
//! job runs its guest are the daemon's.

mod branch;
mod client;
mod endpoint;
mod enroll;
mod jobs;

pub use branch::{
    announce_leaving, authorize_branch, inspect_library, library_origin, redeem_branch_code,
    BranchAuthorization, BranchRedeem, InspectedLibrary,
};
pub use client::{
    CatalogBatchAccepted, CatalogSession, ClientExit, ClientTiming, HeartbeatSource, HostClient,
    HostClientConfig, HostClientHandle, AWAITING_HOST_CONSENT, KIND_NOT_RUN_HERE,
    MAX_ARTIFACT_BYTES, SECRETS_UNOPENABLE,
};
pub use endpoint::{
    DialError, OwnerEndpoint, OwnerIo, OwnerSocket, TlsProbe, CONNECT_PATH, ENROLL_OAUTH_PATH,
    ENROLL_PATH, INFO_PATH,
};
pub use enroll::{check_join_ancestry, join_cluster, refuse_cycle, JoinError};
pub use jobs::{
    HandedSecrets, HostJob, HostJobCtx, HostJobError, HostJobRunner, HostUpload, OwnerCallError,
    WriteBackError,
};
