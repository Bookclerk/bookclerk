//! The host side of the node API (H-4 design, sections 2 to 4; step 7).
//!
//! A host node is a separate Bookclerk install that enrolled into an owner
//! cluster with a single-use link ([`join_cluster`]). Its node keys are
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
//! Host consent for owner plugins (step 8) and streamed uploads (step 9)
//! are not here: until they exist, nothing on a host may run an owner
//! plugin, so a daemon starts its clients with
//! [`HostClientConfig::max_jobs`] at zero.

mod client;
mod endpoint;
mod enroll;
mod jobs;

pub use client::{
    ClientExit, ClientTiming, HeartbeatSource, HostClient, HostClientConfig, HostClientHandle,
    MAX_LEASE, SECRETS_UNOPENABLE,
};
pub use endpoint::{DialError, OwnerEndpoint, OwnerIo, OwnerSocket, CONNECT_PATH, ENROLL_PATH};
pub use enroll::{join_cluster, JoinError};
pub use jobs::{HandedSecrets, HostJob, HostJobCtx, HostJobError, HostJobRunner, WriteBackError};
