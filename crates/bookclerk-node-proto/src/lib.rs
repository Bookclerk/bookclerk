//! Wire protocol between a Bookclerk host node and its owner cluster.
//!
//! A host connects to `GET /api/node/v1/connect` with the WebSocket
//! subprotocol [`SUBPROTOCOL`] and proves itself with the Ed25519 key it
//! enrolled (H-4 design, section 3):
//!
//! 1. host → owner: [`Body::Hello`] `{node_id, incarnation,
//!    software_version, resume}`
//! 2. owner → host: [`Body::Challenge`] `{nonce, cluster_id}`
//! 3. host → owner: [`Body::Auth`] `{sig}`, an Ed25519 signature over
//!    [`auth_transcript`]
//! 4. owner → host: [`Body::Welcome`], or a close with
//!    [`CLOSE_AUTH_FAILED`] / [`CLOSE_REVOKED`].
//!
//! Frames are JSON text of at most [`MAX_FRAME_BYTES`]; binary frames are a
//! protocol error because bulk data travels over HTTP.
//!
//! After the handshake, a hello that listed held fences is answered with
//! [`Body::Resumed`]. Work is pulled: the host sends [`Body::Claim`] and
//! gets [`Body::Work`] (the job, its fence, and its secrets sealed to the
//! node's box key with [`seal_secrets`]) or [`Body::NoWork`]; it then
//! sends [`Body::Renew`], [`Body::Complete`] or [`Body::Fail`] under that
//! fence, and may write a refreshed store token back with
//! [`Body::SecretUpdate`]. The owner forwards the host's `node.<id>` queue
//! as [`Body::Wake`] frames, each answered with [`Body::Ack`].
//!
//! A claim may carry a [`ClaimFilter`] (the kinds the host runs, whose work
//! it takes ([`ClaimScope`]), and the
//! plugins its operator consented to); a job handed back unrun goes with
//! [`Body::Release`]. To act on wakes the host reads
//! [`Body::DeploymentsList`] and [`Body::InstanceConfigGet`] (`SECRETS`
//! sealed with [`seal_instance_secrets`]), reports
//! [`Body::Observation`]s, and fetches stored archives over HTTP at
//! [`ARTIFACTS_PATH`] with a request signed by [`sign_artifact_request`].
//!
//! A scan run on a host returns its rows in fenced [`Body::ScanResults`]
//! batches; the owner validates and writes them. An acquire asks
//! [`Body::AcquirePlan`] for its titles, then for each title asks
//! [`Body::UploadGrant`] and streams the files over HTTP at
//! [`UPLOADS_PATH`] with the token sealed in [`Body::UploadGranted`]; the
//! owner streams them on to its output destinations without staging them,
//! and the job completes when the last title's upload commits.
//!
//! A branch shares the titles of its own library with the library it
//! joined ([`Body::CatalogBegin`], [`Body::CatalogSync`],
//! [`Body::CatalogUpload`]; see [`CatalogTitle`]): titles carry no account
//! or user field, and files reuse the upload endpoint.
//!
//! A library asks its branches whether they can supply works it knows
//! ([`LookupWork`], by strong identifiers, never titles): it stores the
//! questions and wakes the branch, which pulls them with
//! [`Body::CatalogQuestions`] and answers with [`Body::CatalogFound`].
//!
//! A member's request for a book a branch listed is a job on the library
//! pinned to that branch ([`BranchRequest`]): the branch claims it with
//! [`Body::RequestsClaim`] and reports with [`Body::RequestFinish`]; the
//! book itself travels up through the catalog sync.
//!
//! A window (a branch whose link takes no work) reads the books the
//! library holds for its person with [`Body::WindowBooks`], page by page
//! ([`Body::WindowPage`], [`WindowWork`]), and merges them by identifier
//! with the other libraries it joined.
//!
//! A host that leaves sends [`Body::NodeLeaving`]; the owner revokes the
//! node at once. Joining with a sign-in instead of a link is the OAuth
//! code + PKCE flow of the built-in client [`BRANCH_CLIENT_ID`] ([`OwnerInfo`],
//! [`OAuthEnrollRequest`]). An Owner of the library may instead let the
//! install join as a clerk (an owner node, [`ConnectionType::Clerk`]): its
//! redeem answer carries a [`ClerkBootstrap`] sealed to the node's box key.
//! A branch's redeem answer may carry the signing-in user's own store
//! accounts as an [`AccountBundle`] sealed the same way, when the user
//! chose to bring them.
//!
//! An install can also sign in to a library as a person rather than join
//! it as a node: a client ([`CLIENT_CLIENT_ID`], [`ClientTokenAnswer`]),
//! whose session token travels as `Authorization: Bookclerk-Client`
//! ([`parse_client_authorization`]).
//!
//! This crate is pure: no I/O, no clock, no randomness. The daemon (owner)
//! and the host client share it so both sides sign and check the same
//! bytes.

mod accounts;
mod catalog;
mod clerk;
mod client;
mod frames;
mod join;
mod keys;
mod lookup;
mod requests;
mod sealed;
mod signature;
mod window;

pub use accounts::{
    account_bundle_aad, account_offer_aad, check_account_bundle, open_account_bundle,
    open_account_offer, parse_account_bundle, seal_account_bundle, seal_account_offer,
    AccountBundle, CarriedAccount, CarriedSecret, ACCOUNT_BUNDLE_AAD_DOMAIN,
    ACCOUNT_BUNDLE_VERSION, ACCOUNT_OFFER_AAD_DOMAIN, MAX_ACCOUNT_BUNDLE_BYTES,
    MAX_ACCOUNT_SECRETS, MAX_BUNDLE_ACCOUNTS, MAX_CARRIED_ACCOUNT_ID_BYTES,
    MAX_CARRIED_FIELD_BYTES, MAX_CARRIED_PLUGIN_KEY_BYTES, MAX_CARRIED_SECRET_BYTES,
};
pub use catalog::{
    is_ancestry, is_cluster_id, is_marketplace, is_product_id, is_store_id, CatalogFile,
    CatalogMetadata, CatalogRef, CatalogTitle, CATALOG_ITEM, MAX_ANCESTRY,
    MAX_CATALOG_BATCH_TITLES, MAX_CATALOG_CURSOR_BYTES, MAX_CATALOG_TITLE_BYTES,
    MAX_CATALOG_WANTED, MAX_MARKETPLACE_BYTES,
};
pub use clerk::{
    clerk_bootstrap_aad, open_clerk_bootstrap, seal_clerk_bootstrap, ClerkBootstrap, ClerkDatabase,
    ConnectionType, CLERK_BOOTSTRAP_AAD_DOMAIN, CLERK_BOOTSTRAP_VERSION, CLERK_GRANTED_SCOPE,
    CLERK_OFFER_PARAM, DEK_LEN,
};
pub use client::{
    client_request_problem, is_client_name, is_client_token, is_s256_challenge,
    parse_client_authorization, parse_client_query, ClientAuthorizeParams, ClientTokenAnswer,
    ClientTokenRequest, ClientUser, CLIENT_AUTH_SCHEME, CLIENT_CLIENT_ID, CLIENT_CODE_TTL_SECS,
    CLIENT_IDLE_DAYS, CLIENT_SCOPE, CLIENT_TOKEN_LEN, CLIENT_TOKEN_PATH, MAX_CLIENT_NAME_BYTES,
    MAX_CLIENT_STATE_BYTES,
};
pub use ed25519_dalek::SigningKey;
pub use frames::{
    is_destination_id, is_storage_key, AcquireTarget, Body, ClaimFilter, ClaimScope, FileCopy,
    Frame, FrameError, HeldFence, JobStatusEntry, LibraryId, PlanDestination, RemoteArtifact,
    RemoteDeployment, RemoteVersion, ScanImport, UploadAnswer, UploadCommitRequest, UploadFile,
    WorkJob, ITEM_JOB, MAX_DESTINATION_ID_BYTES, MAX_LIBRARY_ID_BYTES, MAX_SCAN_BATCH_ROWS,
    MAX_SCAN_ROW_BYTES, MAX_STATUS_JOBS, MAX_UPLOAD_COPIES, MAX_UPLOAD_FILES,
    MAX_UPLOAD_FILE_BYTES, MAX_UPLOAD_KEY_BYTES,
};
pub use join::{
    is_branch_name, is_branch_redirect_uri, is_library_name, pkce_challenge, OAuthEnrollAnswer,
    OAuthEnrollRequest, OwnerInfo, AUTHORIZE_PATH, BRANCH_CLIENT_ID, BRANCH_CODE_TTL_SECS,
    BRANCH_SCOPE, ENROLL_OAUTH_PATH, INFO_PATH, JOIN_CALLBACK_PATH, MAX_BRANCH_NAME_BYTES,
    MAX_LIBRARY_NAME_BYTES,
};
pub use keys::{
    box_public_key, sign_public_key, validate_box_public_key, validate_sign_public_key, KeyError,
    PUBLIC_KEY_LEN,
};
pub use lookup::{
    found_problem, is_work_ref, questions_problem, Availability, LookupAnswer, LookupEdition,
    LookupFormat, LookupIdentifier, LookupScheme, LookupWork, MAX_LOOKUP_EDITIONS,
    MAX_LOOKUP_IDENTIFIERS, MAX_LOOKUP_WORKS, MAX_WORK_REF_BYTES,
};
pub use requests::{
    claimed_problem, finish_problem, is_request_job, BranchRequest, RequestOutcome, ITEM_REQUEST,
    MAX_CLAIMED_REQUESTS, MAX_REQUEST_JOB_BYTES,
};
pub use sealed::{
    instance_secrets_aad, open, open_instance_secrets, open_secret_update, open_secrets,
    open_upload_token, seal, seal_instance_secrets, seal_secret_update, seal_secrets,
    seal_upload_token, secret_update_aad, secrets_aad, upload_token_aad, SealError, SecretEntry,
    SecretRef, SecretValue, INSTANCE_SECRETS_AAD_DOMAIN, SEALED_BOX_INFO, SEALED_BOX_OVERHEAD,
    SECRETS_AAD_DOMAIN, SECRET_UPDATE_AAD_DOMAIN, UPLOAD_TOKEN_AAD_DOMAIN,
};
pub use signature::{
    artifact_transcript, auth_transcript, canonical_origin, decode_nonce, encode_nonce,
    parse_artifact_authorization, sign_artifact_request, sign_auth, verify_artifact_request,
    verify_auth, ArtifactRequest, AuthContext, AuthError, ARTIFACT_AUTH_SCHEME, ARTIFACT_DOMAIN,
    ARTIFACT_REQUEST_MAX_SKEW_SECS, AUTH_DOMAIN, NONCE_LEN,
};
pub use window::{window_page_problem, WindowWork, MAX_WINDOW_WORKS};

/// Path prefix of the owner's artifact endpoint: `GET
/// /api/node/v1/artifacts/<archive_sha256>` with a signed `Authorization`
/// ([`sign_artifact_request`]) streams one stored archive to a host that
/// runs it.
pub const ARTIFACTS_PATH: &str = "/api/node/v1/artifacts/";

/// Path prefix of the owner's upload endpoint (H-4 design, section 6):
/// `PUT|HEAD <prefix><upload_id>/files/<n>` streams one declared file and
/// `POST <prefix><upload_id>/commit` commits the title, each with
/// `Authorization: Bookclerk-Upload <token>` ([`UPLOAD_AUTH_SCHEME`]).
pub const UPLOADS_PATH: &str = "/api/node/v1/uploads/";

/// `Authorization` scheme of the upload endpoint.
pub const UPLOAD_AUTH_SCHEME: &str = "Bookclerk-Upload";

/// WebSocket subprotocol a host must offer.
pub const SUBPROTOCOL: &str = "bookclerk-node.v1";

/// Frame envelope version (`v`).
pub const FRAME_VERSION: u32 = 1;

/// Largest frame either side accepts (1 MiB).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Seconds between pings from each side.
pub const PING_INTERVAL_SECS: u64 = 20;

/// Seconds without any inbound frame before a side closes the connection.
pub const IDLE_TIMEOUT_SECS: u64 = 60;

/// Frames a side queues for sending before it stops forwarding wakes and
/// waits for the peer to read.
pub const SEND_QUEUE_FRAMES: usize = 64;

/// Seconds the owner waits for each handshake frame.
pub const HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// Close code: the handshake failed (unknown node, bad signature, or a
/// malformed or late handshake frame).
pub const CLOSE_AUTH_FAILED: u16 = 4401;

/// Close code: the node's credential is revoked.
pub const CLOSE_REVOKED: u16 = 4403;

/// Close code: a newer authenticated connection for the same node replaced
/// this one.
pub const CLOSE_SUPERSEDED: u16 = 4409;

/// Close code: a frame broke the protocol after the handshake (binary,
/// oversized, malformed, or the wrong version).
pub const CLOSE_PROTOCOL: u16 = 4400;
