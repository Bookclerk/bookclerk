//! Wire protocol between a Bookclerk branch node and its library.
//!
//! A host connects to `GET /api/node/v1/connect` with the WebSocket
//! subprotocol [`SUBPROTOCOL`] and proves itself with the Ed25519 key it
//! enrolled (H-4 design, section 3):
//!
//! 1. host → owner: [`Body::Hello`] `{node_id, incarnation,
//!    software_version, resume}`
//! 2. owner → host: [`Body::Challenge`] `{nonce, library_id}`
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
//! batches; the owner validates and writes them. Each account's pass then
//! reports the ids a delta pass found gone ([`Body::ScanRemoved`]) and how
//! it ended ([`Body::ScanPassEnd`]), so the owner keeps the account's sync
//! token ([`WorkJob::scan_tokens`] hands it back) and prunes after a
//! complete full pass. An acquire asks
//! [`Body::AcquirePlan`] for its titles, then for each title asks
//! [`Body::UploadGrant`] and streams the files over HTTP at
//! [`UPLOADS_PATH`] with the token sealed in [`Body::UploadGranted`] (a file
//! larger than [`UPLOAD_PART_BYTES`] as consecutive [`UploadPart`]s); the
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
//! A window (a branch whose link takes no work) reads the library's books
//! with [`Body::WindowBooks`], page by page ([`Body::WindowPage`],
//! [`WindowWork`], each edition a [`WindowEdition`] marked when it is the
//! branch owner's), and merges them by identifier with the other libraries
//! it joined.
//!
//! A host that leaves sends [`Body::NodeLeaving`]; the owner revokes the
//! node at once. Joining with a sign-in instead of a link is the OAuth
//! code + PKCE flow of the built-in client [`BRANCH_CLIENT_ID`] ([`OwnerInfo`],
//! [`OAuthEnrollRequest`]). An Owner of the library may instead let the
//! install join as a shared branch (a shared node, [`ConnectionType::SharedBranch`]): its
//! redeem answer carries a [`SharedBranchBootstrap`] sealed to the node's box key.
//! A branch's redeem answer may carry the signing-in user's own store
//! accounts as an [`AccountBundle`] sealed the same way, when the user
//! chose to bring them.
//!
//! An install can also sign in to a library as a person rather than join
//! it as a node: a clerk ([`CLERK_CLIENT_ID`], [`ClerkTokenAnswer`]),
//! whose session token travels as `Authorization: Bookclerk-Clerk`
//! ([`parse_clerk_authorization`]). With it the clerk holds the clerk
//! socket at [`CLERK_CONNECT_PATH`] ([`ClerkFrame`], [`LibraryFrame`]):
//! changed topics and pages of the library's books, never work.
//!
//! This crate is pure: no I/O, no clock, no randomness. The daemon (owner)
//! and the host client share it so both sides sign and check the same
//! bytes.

mod accounts;
mod catalog;
mod clerk;
mod clerk_socket;
mod copies;
mod frames;
mod join;
mod keys;
mod lookup;
mod origin;
mod requests;
mod sealed;
mod shared_branch;
mod signature;
mod tunnel;
mod upload_part;
mod window;

pub use accounts::{
    account_bundle_aad, account_offer_aad, account_reconcile_aad, check_account_bundle,
    open_account_bundle, open_account_offer, open_account_reconcile, parse_account_bundle,
    seal_account_bundle, seal_account_offer, seal_account_reconcile, AccountBundle, CarriedAccount,
    CarriedSecret, ACCOUNT_BUNDLE_AAD_DOMAIN, ACCOUNT_BUNDLE_VERSION, ACCOUNT_OFFER_AAD_DOMAIN,
    ACCOUNT_RECONCILE_AAD_DOMAIN, MAX_ACCOUNT_BUNDLE_BYTES, MAX_ACCOUNT_SECRETS,
    MAX_BUNDLE_ACCOUNTS, MAX_CARRIED_ACCOUNT_ID_BYTES, MAX_CARRIED_FIELD_BYTES,
    MAX_CARRIED_PLUGIN_KEY_BYTES, MAX_CARRIED_SECRET_BYTES,
};
pub use catalog::{
    is_ancestry, is_library_id, is_marketplace, is_product_id, is_store_id, CatalogFile,
    CatalogMetadata, CatalogRef, CatalogTitle, CATALOG_ITEM, MAX_ANCESTRY,
    MAX_CATALOG_BATCH_TITLES, MAX_CATALOG_CURSOR_BYTES, MAX_CATALOG_FILE_VERSION_BYTES,
    MAX_CATALOG_TITLE_BYTES, MAX_CATALOG_WANTED, MAX_MARKETPLACE_BYTES,
};
pub use clerk::{
    clerk_exchange_matches, clerk_request_problem, is_clerk_name, is_clerk_token,
    is_s256_challenge, parse_clerk_authorization, parse_clerk_query, parse_clerk_token_answer,
    parse_clerk_token_request, ClerkAuthorizeParams, ClerkTokenAnswer, ClerkTokenRequest,
    ClerkUser, StoredClerkCode, CLERK_AUTH_SCHEME, CLERK_CALLBACK_PATH, CLERK_CLIENT_ID,
    CLERK_CODE_TTL_SECS, CLERK_IDLE_DAYS, CLERK_SCOPE, CLERK_TOKEN_LEN, CLERK_TOKEN_PATH,
    MAX_CLERK_ANSWER_BYTES, MAX_CLERK_DISPLAY_NAME_BYTES, MAX_CLERK_NAME_BYTES,
    MAX_CLERK_STATE_BYTES,
};
pub use clerk_socket::{
    is_topic_name, parse_clerk_frame, parse_library_frame, ClerkCopy, ClerkFrame, ClerkSocketError,
    ClerkTopic, CopyToken, LibraryFrame, CLERK_CONNECT_PATH, CLERK_SOCKET_VERSION,
    CLERK_SUBPROTOCOL, MAX_CLERK_FRAME_BYTES, MAX_CLERK_TOPICS, MAX_CLERK_TOPIC_BYTES,
};
pub use copies::{
    copy_answer_head_end, copy_grant_problem, copy_granted_problem, is_copy_id, parse_copy_answer,
    parse_copy_range, CopyAnswer, CopyFile, CopyGrantedFrame, CopyRange, CopyRole, COPIES_PATH,
    COPY_AUTH_SCHEME, MAX_COPY_ANSWER_HEAD_BYTES, MAX_COPY_FILES, MAX_COPY_FILE_BYTES,
    MAX_COPY_NAME_BYTES, MAX_COPY_TOKEN_SECS,
};
pub use ed25519_dalek::SigningKey;
pub use frames::{
    deployments_page_problem, is_destination_id, is_storage_key, scan_pass_end_problem,
    scan_removed_problem, target_destinations_problem, AcquireTarget, Body, CallNumber,
    ClaimFilter, ClaimScope, FileCopy, Frame, FrameError, HeldFence, JobStatusEntry, KeeperOutcome,
    PlanDestination, RemoteArtifact, RemoteDeployment, RemoteVersion, ScanImport, ScanToken,
    TargetTitle, UploadAnswer, UploadCommitRequest, UploadFile, UploadProbe, WorkJob, ITEM_JOB,
    MAX_CALL_NUMBER_BYTES, MAX_DESTINATION_ID_BYTES, MAX_PROBE_BITRATE_KBPS, MAX_SCAN_BATCH_ROWS,
    MAX_SCAN_PRODUCT_ID_BYTES, MAX_SCAN_REMOVED_IDS, MAX_SCAN_ROW_BYTES, MAX_SCAN_SYNC_TOKEN_BYTES,
    MAX_SCAN_TOKENS, MAX_STATUS_JOBS, MAX_UPLOAD_COPIES, MAX_UPLOAD_FILES, MAX_UPLOAD_FILE_BYTES,
    MAX_UPLOAD_KEY_BYTES,
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
pub use origin::{BranchOrigin, MAX_BRANCH_ORIGIN_BYTES};
pub use requests::{
    claimed_problem, finish_problem, is_request_job, BranchRequest, RequestOutcome, ITEM_REQUEST,
    MAX_CLAIMED_REQUESTS, MAX_REQUEST_JOB_BYTES,
};
pub use sealed::{
    copy_token_aad, instance_secrets_aad, open, open_copy_token, open_instance_secrets,
    open_secret_update, open_secrets, open_upload_token, seal, seal_copy_token,
    seal_instance_secrets, seal_secret_update, seal_secrets, seal_upload_token, secret_update_aad,
    secrets_aad, upload_token_aad, SealError, SecretEntry, SecretRef, SecretValue,
    COPY_TOKEN_AAD_DOMAIN, INSTANCE_SECRETS_AAD_DOMAIN, SEALED_BOX_INFO, SEALED_BOX_OVERHEAD,
    SECRETS_AAD_DOMAIN, SECRET_UPDATE_AAD_DOMAIN, UPLOAD_TOKEN_AAD_DOMAIN,
};
pub use shared_branch::{
    open_shared_branch_bootstrap, seal_shared_branch_bootstrap, shared_branch_bootstrap_aad,
    ConnectionType, SharedBranchBootstrap, SharedBranchDatabase, DEK_LEN,
    SHARED_BRANCH_BOOTSTRAP_AAD_DOMAIN, SHARED_BRANCH_BOOTSTRAP_VERSION,
    SHARED_BRANCH_GRANTED_SCOPE, SHARED_BRANCH_OFFER_PARAM,
};
pub use signature::{
    artifact_transcript, auth_transcript, canonical_origin, decode_nonce, encode_nonce,
    parse_artifact_authorization, sign_artifact_request, sign_auth, verify_artifact_request,
    verify_auth, ArtifactRequest, AuthContext, AuthError, ARTIFACT_AUTH_SCHEME, ARTIFACT_DOMAIN,
    ARTIFACT_REQUEST_MAX_SKEW_SECS, AUTH_DOMAIN, NONCE_LEN,
};
pub use tunnel::{
    normalize_tunnel_path, tunnel_refusal, TunnelExposure, TunnelRefusal, BRANCH_API_PATHS,
    LEGACY_CONTROL_PATHS, OPERATOR_ONLY_PATHS,
};
pub use upload_part::{
    parse_upload_range, upload_parts, UploadPart, UploadRange, UPLOAD_PART_BYTES,
};
pub use window::{
    window_page_problem, WindowCopy, WindowCursor, WindowEdition, WindowWork,
    MAX_COPY_CONTENT_TYPE_BYTES, MAX_COPY_VERSION_BYTES, MAX_WINDOW_WORKS,
};

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
