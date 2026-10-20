//! Joining an owner cluster with an enrollment link (H-4 design, section 2).

use base64::Engine as _;
use bookclerk_library::control_plane::{
    join_cycle, parse_enrollment_token, record_cluster_link, set_cluster_link_ancestry,
    ClusterLink, ClusterLinkKeys, NewClusterLink,
};
use bookclerk_library::LibraryStore;
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::endpoint::{DialError, OwnerEndpoint, ENROLL_PATH};

/// Why joining failed.
#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    /// The text is not a whole `https://<owner>/enroll#bcn1...` link.
    #[error("not an enrollment link: {0}")]
    Link(String),
    /// The owner could not be reached under the transport policy.
    #[error(transparent)]
    Dial(#[from] DialError),
    /// The owner refused the link (spent, expired, revoked, or unknown), or
    /// the sign-in's code.
    #[error("the owner refused the enrollment")]
    Refused,
    /// The library names another origin as its own; join at that one.
    #[error("this library's address is {origin}; join with that address")]
    OriginMismatch {
        /// The origin the library names.
        origin: String,
    },
    /// The owner answered with another status.
    #[error("enrollment failed with HTTP {status}: {error}")]
    Http {
        /// HTTP status.
        status: u16,
        /// The answer's `error` code.
        error: String,
    },
    /// The enroll request could not be built, or the library's `info`
    /// (its ancestry) could not be read.
    #[error("enrollment could not be recorded: {0}")]
    Record(String),
    /// The library is this one, or one of its own branches (its ancestry
    /// names this library): joining it would make a cycle.
    #[error("that library is this library or one of its branches; joining it would make a loop")]
    Cycle,
    /// The library accepted the join (an invite link or a sign-in code),
    /// but its answer did not parse or the local write failed. The one-use
    /// credential is spent and the node keys are gone, so this install
    /// cannot use the node the library minted: its owner removes that node
    /// (named when the answer said which) and this install joins again.
    #[error(
        "the library enrolled this host{} but it could not be saved here ({reason}); \
         ask the library's owner to remove that node, then join again",
        node_id.as_deref().map(|id| format!(" as node {id}")).unwrap_or_default()
    )]
    Unsaved {
        /// The node id the owner minted, when its answer named one.
        node_id: Option<String>,
        /// What failed.
        reason: String,
    },
    /// A sign-in's code was redeemed and the library enrolled this host,
    /// but the local write failed. The library answers a repeat of the
    /// same exchange (code, verifier, keys) with the same node, so a retry
    /// with the sign-in still pending can finish the join.
    #[error(
        "the library enrolled this host as node {node_id} but it could not be saved here ({reason})"
    )]
    SaveFailed {
        /// The node id the library minted.
        node_id: String,
        /// What failed.
        reason: String,
    },
}

impl JoinError {
    /// True for a sign-in redeem failure that a repeat of the same exchange
    /// (code, verifier, keys) can fix: the library unreachable (its answer
    /// may have been lost after it enrolled the node, which a repeat gets
    /// again), busy, or the local write failed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            // A bad address, an insecure transport, or a certificate that
            // does not match fails the same way on every try.
            Self::Dial(DialError::Insecure(_) | DialError::Invalid(_) | DialError::Tls(_)) => false,
            Self::Dial(DialError::Refused(status)) | Self::Http { status, .. } => {
                *status >= 500 || *status == 429
            }
            Self::Dial(_) | Self::SaveFailed { .. } => true,
            _ => false,
        }
    }
}

/// Largest `info` answer read.
const MAX_INFO_BYTES: usize = 64 * 1024;

/// Reads the library's `info` over `endpoint` and refuses a library whose
/// ancestry names this install's own cluster id (A ← B ← A). Answers its
/// ancestry, to record on the link.
///
/// # Errors
///
/// Returns [`JoinError::Cycle`] for a cycle, and [`JoinError`] when the
/// library cannot be read (fail closed: no join without the check).
pub async fn check_join_ancestry(
    store: &LibraryStore,
    endpoint: &OwnerEndpoint,
) -> Result<Vec<String>, JoinError> {
    let (status, body) = endpoint
        .get_json(bookclerk_node_proto::INFO_PATH, MAX_INFO_BYTES)
        .await?;
    if status != 200 {
        return Err(JoinError::Http {
            status,
            error: "info_unavailable".into(),
        });
    }
    let info: bookclerk_node_proto::OwnerInfo = serde_json::from_slice(&body)
        .map_err(|_| JoinError::Record("malformed library info".into()))?;
    refuse_cycle(store, &info.ancestry).await?;
    Ok(info.ancestry)
}

/// Refuses `ancestry` when it names this install's own cluster id, or is
/// malformed.
///
/// # Errors
///
/// Returns [`JoinError::Cycle`] or [`JoinError::Record`].
pub async fn refuse_cycle(store: &LibraryStore, ancestry: &[String]) -> Result<(), JoinError> {
    if !bookclerk_node_proto::is_ancestry(ancestry) {
        return Err(JoinError::Record("malformed library ancestry".into()));
    }
    match join_cycle(store, ancestry).await {
        Ok(None) => Ok(()),
        Ok(Some(own)) => {
            tracing::warn!(cluster_id = %own, "refused to join a library whose ancestry names this one");
            Err(JoinError::Cycle)
        }
        Err(err) => Err(JoinError::Record(err.to_string())),
    }
}

/// Records the owner's ancestry on `link`, just joined, and on the value
/// returned. A failure is logged; the next catalog session records it
/// again.
pub(crate) async fn record_ancestry(
    store: &LibraryStore,
    link: &mut ClusterLink,
    ancestry: &[String],
) {
    match set_cluster_link_ancestry(store, &link.owner_cluster_id, ancestry).await {
        Ok(true) => link.ancestry = ancestry.to_vec(),
        Ok(false) => {}
        Err(err) => tracing::warn!(
            cluster_id = %link.owner_cluster_id,
            error = %err,
            "could not record the library's ancestry"
        ),
    }
}

/// `POST /api/node/v1/enroll` answer.
#[derive(Deserialize)]
struct Enrolled {
    /// Owner cluster id.
    cluster_id: String,
    /// The node id minted for this host.
    node_id: String,
}

/// Enrolls this install into the owner cluster named by `link`.
///
/// Generates fresh node keys, redeems the link at the owner (under the
/// transport policy and the link's SPKI pin), then seals the keys into this
/// install's `encrypted_secrets` and records the `cluster_links` row. The
/// link secret is sent once in the request body and never logged.
///
/// `link` is the whole link as minted (`https://owner/enroll#bcn1...`):
/// the origin it names is the one this host dials and signs from then on.
///
/// # Errors
///
/// Returns [`JoinError`] when the link is malformed, the owner is
/// unreachable or refuses, or the local write fails.
pub async fn join_cluster(
    store: &LibraryStore,
    link: &str,
    software_version: &str,
) -> Result<ClusterLink, JoinError> {
    let link = link.trim();
    let token = parse_enrollment_token(link).map_err(|err| JoinError::Link(err.to_string()))?;
    let origin = link.rsplit_once('#').map(|(url, _)| url).ok_or_else(|| {
        JoinError::Link("the whole link, with the owner origin, is needed".into())
    })?;
    let endpoint = OwnerEndpoint::new(origin, token.spki_pin.as_deref())?;
    // Before the link is spent: never join this library or one below it.
    let ancestry = check_join_ancestry(store, &endpoint).await?;
    let keys = ClusterLinkKeys::generate();
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let body = Zeroizing::new(
        serde_json::to_vec(&serde_json::json!({
            "link_id": token.link_id,
            "secret": token.secret.expose(),
            "sign_pk": b64(&keys.sign_public_key()),
            "box_pk": b64(&keys.box_public_key()),
            "software_version": software_version,
        }))
        .map_err(|err| JoinError::Record(err.to_string()))?,
    );
    let (status, answer) = endpoint.post_json(ENROLL_PATH, &body).await?;
    drop(body);
    if status != 200 {
        let error = serde_json::from_slice::<serde_json::Value>(&answer)
            .ok()
            .and_then(|value| value.get("error")?.as_str().map(str::to_string))
            .unwrap_or_default();
        if status == 403 && error == "enrollment_refused" {
            return Err(JoinError::Refused);
        }
        return Err(JoinError::Http { status, error });
    }
    let enrolled: Enrolled = serde_json::from_slice(&answer).map_err(|_| JoinError::Unsaved {
        node_id: serde_json::from_slice::<serde_json::Value>(&answer)
            .ok()
            .and_then(|value| value.get("node_id")?.as_str().map(str::to_string)),
        reason: "malformed enroll answer".into(),
    })?;
    let node_id = enrolled.node_id.clone();
    let mut recorded = record_cluster_link(
        store,
        &NewClusterLink {
            owner_cluster_id: enrolled.cluster_id,
            owner_origin: endpoint.origin().to_string(),
            remote_node_id: enrolled.node_id,
            spki_pin: token.spki_pin.clone(),
        },
        &keys,
    )
    .await
    .map_err(|err| JoinError::Unsaved {
        node_id: Some(node_id),
        reason: err.to_string(),
    })?;
    record_ancestry(store, &mut recorded, &ancestry).await;
    tracing::info!(
        cluster_id = %recorded.owner_cluster_id,
        node_id = %recorded.remote_node_id,
        origin = %recorded.owner_origin,
        pinned = recorded.spki_pin.is_some(),
        "enrolled into the owner cluster"
    );
    Ok(recorded)
}
