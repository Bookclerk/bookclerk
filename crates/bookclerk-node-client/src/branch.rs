//! Joining a library with a sign-in (the OAuth code + PKCE flow of
//! `bookclerk_node_proto::BRANCH_CLIENT_ID`), and telling a library this
//! install is leaving.
//!
//! The joining side is used by the daemon (the SPA's Join wizard, which
//! keeps the verifier and keys in a pending-join row) and by `bookclerk
//! clusters join --url` (which keeps them in memory and listens on a
//! loopback callback):
//!
//! 1. [`inspect_library`] reads `GET /api/node/v1/info` under the transport
//!    policy. A self-signed library's certificate is recorded on first use
//!    and must match the fingerprint the library reports; the operator
//!    confirms it before anything else is sent.
//! 2. [`authorize_branch`] makes the verifier, `state` and node keys and
//!    the authorize URL the browser goes to.
//! 3. [`redeem_branch_code`] sends the code, the verifier and the public
//!    keys to `POST /api/node/v1/enroll/oauth`, then seals the keys and
//!    records the link exactly as [`crate::join_cluster`] does.

use std::time::Duration;

use base64::Engine as _;
use bookclerk_library::control_plane::{
    record_cluster_link, ClusterLink, ClusterLinkKeys, NewClusterLink,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{
    canonical_origin, is_branch_name, is_branch_redirect_uri, is_library_name, pkce_challenge,
    Body, OAuthEnrollRequest, OwnerInfo, AUTHORIZE_PATH, BRANCH_CLIENT_ID, BRANCH_SCOPE,
    ENROLL_OAUTH_PATH, INFO_PATH,
};
use rand::RngCore as _;
use serde::Deserialize;
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::client::{ClientTiming, HeartbeatSource, HostClient, HostClientConfig};
use crate::endpoint::{DialError, OwnerEndpoint};
use crate::enroll::JoinError;
use crate::jobs::{HostJobCtx, HostJobError, HostJobRunner};

/// A library as [`inspect_library`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedLibrary {
    /// What the library said about itself.
    pub info: OwnerInfo,
    /// Its canonical origin.
    pub origin: String,
    /// The SPKI pin to trust from now on: set for a self-signed library
    /// (the operator must confirm it), `None` when the certificate chains
    /// to the public roots or the library is on loopback.
    pub spki_pin: Option<String>,
}

/// Normalizes what an operator typed (`owner.example:8443`,
/// `https://owner.example/`) to a canonical origin. A bare `host[:port]`
/// means `https`.
///
/// # Errors
///
/// Returns [`JoinError::Link`] when it is not an http(s) origin.
pub fn library_origin(address: &str) -> Result<String, JoinError> {
    let address = address.trim();
    if address.is_empty() || address.chars().any(char::is_whitespace) {
        return Err(JoinError::Link("enter the library's address".into()));
    }
    let with_scheme = if address.contains("://") {
        address.to_string()
    } else {
        format!("https://{address}")
    };
    canonical_origin(&with_scheme)
        .ok_or_else(|| JoinError::Link("the library address must be an http(s) origin".into()))
}

/// Reads what the library at `address` says about itself, under the
/// transport policy (plain HTTP only on loopback).
///
/// - A `self_signed` library must present the certificate whose pin it
///   reports; the pin is answered for the operator to confirm.
/// - Any other TLS library must present a certificate that chains to the
///   public roots.
/// - The library must name the origin it was reached at as its own (the
///   origin its node handshake signs), so the link is recorded with an
///   origin that works.
///
/// # Errors
///
/// Returns [`JoinError`] when the address is malformed, the library is
/// unreachable or untrusted, or its answer is malformed or names another
/// origin.
pub async fn inspect_library(address: &str) -> Result<InspectedLibrary, JoinError> {
    let origin = library_origin(address)?;
    let endpoint = OwnerEndpoint::new(&origin, None)?;
    let (status, body, probe) = endpoint.probe_json(INFO_PATH).await?;
    if status != 200 {
        return Err(http_error(status, &body));
    }
    let info: OwnerInfo = serde_json::from_slice(&body)
        .map_err(|_| JoinError::Record("malformed library info".into()))?;
    let spki_pin = match (&probe, info.transport.as_str()) {
        (None, _) => None,
        (Some(probe), "self_signed") => {
            if info.spki_fingerprint.as_deref() != Some(probe.spki_pin.as_str()) {
                return Err(JoinError::Dial(DialError::Tls(
                    "the library's certificate is not the one it reports".into(),
                )));
            }
            Some(probe.spki_pin.clone())
        }
        (Some(probe), _) if probe.webpki_trusted => None,
        (Some(_), _) => {
            return Err(JoinError::Dial(DialError::Tls(
                "the library's certificate is not trusted".into(),
            )))
        }
    };
    let named = canonical_origin(&info.origin).unwrap_or_default();
    if named != origin {
        return Err(JoinError::OriginMismatch { origin: named });
    }
    // The library is named by its public host: a DNS name, longer than a
    // branch name may be.
    if !is_library_name(&info.library_name) || info.cluster_id.trim().is_empty() {
        return Err(JoinError::Record("malformed library info".into()));
    }
    Ok(InspectedLibrary {
        info,
        origin,
        spki_pin,
    })
}

/// One sign-in about to start. `Debug` prints no secret.
pub struct BranchAuthorization {
    /// Where to send the browser.
    pub authorize_url: String,
    /// The OAuth `state` (also in the URL).
    pub state: Zeroizing<String>,
    /// The PKCE verifier; never leaves this install.
    pub verifier: Zeroizing<String>,
    /// The node keys the branch will enroll.
    pub keys: ClusterLinkKeys,
    /// The redirect URI in the URL.
    pub redirect_uri: String,
}

impl std::fmt::Debug for BranchAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchAuthorization")
            .field("redirect_uri", &self.redirect_uri)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

/// 32 random bytes as base64url without padding (43 characters).
fn random_token() -> Zeroizing<String> {
    let mut raw = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(raw.as_mut_slice());
    Zeroizing::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_slice()))
}

/// Starts a sign-in at `library` for this install as `branch_name`, coming
/// back to `redirect_uri` (a loopback or https URL).
///
/// # Errors
///
/// Returns [`JoinError::Link`] for a redirect URI the library would refuse
/// or a malformed branch name.
pub fn authorize_branch(
    library: &InspectedLibrary,
    redirect_uri: &str,
    branch_name: &str,
) -> Result<BranchAuthorization, JoinError> {
    if !is_branch_redirect_uri(redirect_uri) {
        return Err(JoinError::Link(
            "this install's address must be loopback or https to sign in".into(),
        ));
    }
    let branch_name = branch_name.trim();
    if !is_branch_name(branch_name) {
        return Err(JoinError::Link(
            "the branch name must be 1 to 64 characters".into(),
        ));
    }
    let verifier = random_token();
    let state = random_token();
    let mut url = url::Url::parse(&format!("{}{AUTHORIZE_PATH}", library.origin))
        .map_err(|_| JoinError::Link("the library address does not parse".into()))?;
    url.query_pairs_mut()
        .append_pair("client_id", BRANCH_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", BRANCH_SCOPE)
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("branch_name", branch_name);
    Ok(BranchAuthorization {
        authorize_url: url.into(),
        state,
        verifier,
        keys: ClusterLinkKeys::generate(),
        redirect_uri: redirect_uri.to_string(),
    })
}

/// What [`redeem_branch_code`] needs besides the keys.
pub struct BranchRedeem<'a> {
    /// The library's canonical origin.
    pub origin: &'a str,
    /// The pin confirmed for a self-signed library.
    pub spki_pin: Option<&'a str>,
    /// The cluster id the library reported before sign-in.
    pub cluster_id: &'a str,
    /// The code from the callback.
    pub code: &'a str,
    /// The PKCE verifier.
    pub verifier: &'a str,
    /// The redirect URI of the authorization.
    pub redirect_uri: &'a str,
    /// The branch name.
    pub branch_name: &'a str,
    /// This install's version.
    pub software_version: &'a str,
}

/// `POST /api/node/v1/enroll/oauth` answer.
#[derive(Deserialize)]
struct Enrolled {
    /// Owner cluster id.
    cluster_id: String,
    /// The node id minted for this host.
    node_id: String,
}

/// The answer's `error` code, as a [`JoinError`].
fn http_error(status: u16, body: &[u8]) -> JoinError {
    let error = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_string))
        .unwrap_or_default();
    if status == 403 && error == "enrollment_refused" {
        return JoinError::Refused;
    }
    JoinError::Http { status, error }
}

/// Redeems an approved sign-in: the library runs the same redeem as for a
/// link (one node, a receipt, idempotent retries), then this install seals
/// `keys` and records the link. The code and verifier go once in the
/// request body and are never logged.
///
/// # Errors
///
/// Returns [`JoinError`] when the library is unreachable or refuses; once
/// the library has enrolled a node, a malformed answer or one for another
/// cluster gives [`JoinError::Unsaved`], which names that node so it can be
/// revoked there, as does a link this install refuses to record. A failed
/// database or disk write gives [`JoinError::SaveFailed`] (redeeming the
/// same code again with the same verifier and keys gets the same node, so
/// that one can be retried).
pub async fn redeem_branch_code(
    store: &LibraryStore,
    redeem: &BranchRedeem<'_>,
    keys: &ClusterLinkKeys,
) -> Result<ClusterLink, JoinError> {
    let endpoint = OwnerEndpoint::new(redeem.origin, redeem.spki_pin)?;
    // An answer (or an error string from the library) that echoes either
    // value is redacted wherever it is logged.
    bookclerk_config::register_secret(redeem.code);
    bookclerk_config::register_secret(redeem.verifier);
    // Checked again before the code is spent: the library's ancestry may
    // have changed since the operator started.
    let ancestry = crate::enroll::check_join_ancestry(store, &endpoint).await?;
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let body = Zeroizing::new(
        serde_json::to_vec(&OAuthEnrollRequest {
            code: redeem.code.to_string(),
            code_verifier: redeem.verifier.to_string(),
            redirect_uri: redeem.redirect_uri.to_string(),
            sign_pk: b64(&keys.sign_public_key()),
            box_pk: b64(&keys.box_public_key()),
            branch_name: redeem.branch_name.to_string(),
            software_version: redeem.software_version.to_string(),
        })
        .map_err(|err| JoinError::Record(err.to_string()))?,
    );
    let (status, answer) = endpoint.post_json(ENROLL_OAUTH_PATH, &body).await?;
    drop(body);
    if status != 200 {
        return Err(http_error(status, &answer));
    }
    // From here the library has enrolled a node: a failure names it, so the
    // operator knows which node to have revoked there.
    let enrolled: Enrolled = serde_json::from_slice(&answer).map_err(|_| JoinError::Unsaved {
        node_id: serde_json::from_slice::<serde_json::Value>(&answer)
            .ok()
            .and_then(|value| value.get("node_id")?.as_str().map(str::to_string)),
        reason: "malformed enroll answer".into(),
    })?;
    if enrolled.cluster_id != redeem.cluster_id {
        return Err(JoinError::Unsaved {
            node_id: Some(enrolled.node_id),
            reason: "the library answered for another cluster".into(),
        });
    }
    let node_id = enrolled.node_id.clone();
    let mut recorded = record_cluster_link(
        store,
        &NewClusterLink {
            owner_cluster_id: enrolled.cluster_id,
            owner_origin: endpoint.origin().to_string(),
            remote_node_id: enrolled.node_id,
            spki_pin: redeem.spki_pin.map(str::to_string),
        },
        keys,
    )
    .await
    .map_err(|err| match err {
        // A database or disk hiccup: the library answers a repeat of this
        // exchange with the same node, so a retry can finish the join.
        bookclerk_library::LibraryError::Orm(_) | bookclerk_library::LibraryError::Io(_) => {
            JoinError::SaveFailed {
                node_id,
                reason: err.to_string(),
            }
        }
        // Anything else (an answer this install refuses to record, no
        // master key) fails the same way every time.
        _ => JoinError::Unsaved {
            node_id: Some(node_id),
            reason: err.to_string(),
        },
    })?;
    crate::enroll::record_ancestry(store, &mut recorded, &ancestry).await;
    tracing::info!(
        cluster_id = %recorded.owner_cluster_id,
        node_id = %recorded.remote_node_id,
        origin = %recorded.owner_origin,
        pinned = recorded.spki_pin.is_some(),
        "joined the library as a branch"
    );
    Ok(recorded)
}

/// A runner that claims nothing (the leaving connection runs no job).
struct NoJobs;

impl HostJobRunner for NoJobs {
    async fn run(&self, _ctx: HostJobCtx) -> Result<String, HostJobError> {
        Err(HostJobError::new("leaving", "this connection runs no job"))
    }
}

/// Tells the library of `link` that this install is leaving: connects with
/// the link's keys (claiming nothing; `sampler` answers the connection's
/// first heartbeat), sends `node.leaving`, and waits for the answer for up
/// to `timeout`. Answers true when the library accepted it. Call it before
/// the keys are deleted.
///
/// A running client for the same link is superseded by this connection;
/// the library then revokes the node, so that client stops too.
pub async fn announce_leaving(
    link: ClusterLink,
    keys: ClusterLinkKeys,
    software_version: &str,
    sampler: Box<dyn HeartbeatSource>,
    timeout: Duration,
) -> bool {
    let cluster_id = link.owner_cluster_id.clone();
    let config = HostClientConfig {
        link,
        keys,
        software_version: software_version.to_string(),
        incarnation: uuid_like(),
        max_jobs: 0,
        claim_kinds: Vec::new(),
        // It claims nothing, so nothing waits for a deployments re-read.
        claims_after_refresh: false,
        timing: ClientTiming {
            request_timeout: timeout,
            ..ClientTiming::default()
        },
    };
    let Ok((client, handle)) = HostClient::new(config, NoJobs, sampler) else {
        return false;
    };
    let (stop, shutdown) = watch::channel(false);
    let running = tokio::spawn(client.run(shutdown));
    let accepted = matches!(
        tokio::time::timeout(timeout, handle.request(Body::NodeLeaving {})).await,
        Ok(Some(frame)) if matches!(frame.body, Body::Accepted {})
    );
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
    if accepted {
        tracing::info!(%cluster_id, "told the library this install is leaving");
    } else {
        tracing::warn!(%cluster_id, "the library did not answer node.leaving");
    }
    accepted
}

/// A fresh plain id for the leaving connection's incarnation.
fn uuid_like() -> String {
    let mut raw = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_default_to_https() {
        assert_eq!(
            library_origin("Owner.Example:8443").unwrap(),
            "https://owner.example:8443"
        );
        assert_eq!(
            library_origin(" https://owner.example/ ").unwrap(),
            "https://owner.example"
        );
        assert_eq!(
            library_origin("http://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
        for bad in ["", "ftp://x", "a b", "https://"] {
            assert!(library_origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_authorize_url_carries_the_challenge_not_the_verifier() {
        let library = InspectedLibrary {
            info: OwnerInfo {
                cluster_id: "c".into(),
                library_name: "owner.example".into(),
                origin: "https://owner.example".into(),
                transport: "acme".into(),
                enrollment: true,
                spki_fingerprint: None,
                software_version: String::new(),
                ancestry: vec!["c".into()],
            },
            origin: "https://owner.example".into(),
            spki_pin: None,
        };
        let auth =
            authorize_branch(&library, "http://localhost:8787/join/callback", "den").unwrap();
        let url = url::Url::parse(&auth.authorize_url).unwrap();
        assert_eq!(url.path(), AUTHORIZE_PATH);
        let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], BRANCH_CLIENT_ID);
        assert_eq!(pairs["scope"], BRANCH_SCOPE);
        assert_eq!(pairs["state"], auth.state.as_str());
        assert_eq!(pairs["code_challenge"], pkce_challenge(&auth.verifier));
        assert!(!auth.authorize_url.contains(auth.verifier.as_str()));
        assert!(!format!("{auth:?}").contains(auth.verifier.as_str()));
        assert!(authorize_branch(&library, "http://192.168.1.2/join/callback", "den").is_err());
        assert!(authorize_branch(&library, "https://h.example/join/callback", "").is_err());
    }
}
