//! The host side of the node API (H-4 design, sections 2 to 4; step 7).
//!
//! A branch node is a separate Bookclerk install that enrolled into an owner
//! library with a single-use link ([`join_library`]) or a sign-in at the
//! owner ([`inspect_library`], [`authorize_branch`],
//! [`redeem_branch_code`]); it tells the owner when it leaves
//! ([`announce_leaving`]). An empty install may instead join as a shared branch, an
//! shared node that shares the library's database and DEK
//! ([`redeem_shared_branch_code`], [`apply_shared_branch_bootstrap`]). Its node keys are
//! sealed in its own database
//! (`bookclerk_library::control_plane::record_library_link`). For every
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
//! - after a wake, the host requests its deployment list and instance
//!   configuration from the owner and sends its observations back
//!   ([`HostClientHandle::request`] for `deployments.list`,
//!   `instance.config` and `observation`), and fetches a stored archive
//!   with [`HostClientHandle::fetch_artifact`], signed with the node key.
//!
//! - a scan's rows returned in fenced batches
//!   ([`HostJobCtx::send_scan_results`]); an acquire's titles asked for
//!   ([`HostJobCtx::acquire_plan`]) and its files streamed to the owner
//!   under a sealed upload token ([`HostJobCtx::grant_upload`],
//!   [`HostJobCtx::upload_file`], [`HostJobCtx::commit_upload`]); the job
//!   completes with its closing upload's commit.
//!
//! A branch may also take the copy the library already holds of an
//! edition: [`HostClientHandle::copy_grant`] asks for it and
//! [`HostClientHandle::fetch_copy_file`] downloads each file, resuming a
//! download that was cut short ([`fetch_copy_file`] is the same download
//! for a clerk, which holds a plain token).
//!
//! An install may instead sign in to a library as a clerk, holding a
//! library card for its person rather than node keys ([`authorize_clerk`],
//! [`exchange_clerk_code`]); the daemon reads the person's books over the
//! library's clerk socket ([`OwnerEndpoint::connect_clerk_socket`]).
//!
//! The consent decisions themselves (which plugins, which grant) and how a
//! job runs its guest are the daemon's.

mod branch;
mod clerk;
mod client;
mod copies;
mod endpoint;
mod enroll;
mod jobs;
mod shared_branch;

pub use branch::{
    announce_leaving, authorize_branch, authorize_join, bind_join_person, inspect_library,
    keep_brought_connections, library_origin, redeem_branch_code, BranchAuthorization,
    BranchRedeem, BroughtConnections, InspectedLibrary, RedeemedBranch,
};
pub use clerk::{
    authorize_clerk, exchange_clerk_code, ClerkAuthorization, ClerkRedeem, ExchangedCard,
};
pub use client::{
    CatalogBatchAccepted, CatalogSession, ClientExit, ClientTiming, HeartbeatSource, HostClient,
    HostClientConfig, HostClientHandle, OfferedAccounts, SignInPushed, AWAITING_HOST_CONSENT,
    KIND_NOT_RUN_HERE, MAX_ARTIFACT_BYTES, MAX_LEASE, SECRETS_UNOPENABLE,
};
pub use copies::{fetch_copy_file, CopyFetchError};
pub use endpoint::{
    CopyStreamError, DialError, LocalFileSource, OwnerEndpoint, OwnerIo, OwnerSocket,
    SocketMessage, TlsProbe, UploadSource, CONNECT_PATH, ENROLL_OAUTH_PATH, ENROLL_PATH, INFO_PATH,
    MAX_UPLOAD_RESUMES,
};
pub use enroll::{check_join_ancestry, join_library, refuse_cycle, refuse_loop, JoinError};
pub use jobs::{
    GrantedCopy, HandedSecrets, HostJob, HostJobCtx, HostJobError, HostJobRunner, HostUpload,
    OwnerCallError, WriteBackError,
};
pub use shared_branch::{
    apply_shared_branch_bootstrap, bootstrap_database_kind, checked_d1_api_base,
    database_guest_installed, install_problem, interrupted_shared_branch_join,
    pending_shared_branch_join, redeem_shared_branch_code, AppliedSharedBranch,
    PendingSharedBranchJoin, RevertFailed, SharedBranchJoined, DATABASE_ENV_OVERRIDES,
    SHARED_BRANCH_POSTGRES_URL_FILE,
};
