//! Signing in to a library as a clerk: the OAuth code + PKCE flow of
//! `bookclerk_node_proto::CLERK_CLIENT_ID`, which gives this install a
//! library card for its person instead of making it a node.
//!
//! 1. [`crate::inspect_library`] reads `GET /api/node/v1/info` under the
//!    transport policy, as a branch join does; the person confirms a
//!    self-signed library's fingerprint.
//! 2. [`authorize_clerk`] makes the verifier, `state` and the authorize URL
//!    the browser goes to.
//! 3. [`redeem_clerk_code`] sends the code and the verifier to `POST
//!    /api/clerk/v1/token`, checks the answer
//!    ([`bookclerk_node_proto::parse_clerk_token_answer`]) is for the
//!    library that was inspected, and seals the card into a clerk link
//!    (`bookclerk_library::control_plane::record_clerk_link`). A sign-in
//!    that replaces a branch splits it: [`exchange_clerk_code`] gets the
//!    card, the daemon leaves the branch, then [`ExchangedCard::record`]
//!    seals it.
//!
//! The daemon then dials the library's clerk socket with the card
//! ([`crate::OwnerEndpoint::connect_clerk_socket`]).

use bookclerk_library::control_plane::{record_clerk_link, ClerkLink, LibraryCard, NewClerkLink};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{
    is_branch_redirect_uri, is_clerk_name, parse_clerk_token_answer, pkce_challenge,
    ClerkTokenRequest, AUTHORIZE_PATH, CLERK_CLIENT_ID, CLERK_SCOPE, CLERK_TOKEN_PATH,
};
use zeroize::Zeroizing;

use crate::branch::{http_error, random_token, InspectedLibrary};
use crate::endpoint::OwnerEndpoint;
use crate::enroll::JoinError;

/// One clerk sign-in about to start. `Debug` prints no secret.
pub struct ClerkAuthorization {
    /// Where to send the browser.
    pub authorize_url: String,
    /// The OAuth `state` (also in the URL).
    pub state: Zeroizing<String>,
    /// The PKCE verifier; never leaves this install.
    pub verifier: Zeroizing<String>,
    /// The redirect URI in the URL.
    pub redirect_uri: String,
    /// The name this install signs in as, trimmed.
    pub clerk_name: String,
}

impl std::fmt::Debug for ClerkAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClerkAuthorization")
            .field("redirect_uri", &self.redirect_uri)
            .field("clerk_name", &self.clerk_name)
            .finish_non_exhaustive()
    }
}

/// Starts a clerk sign-in at `library` for this install as `clerk_name`,
/// coming back to `redirect_uri` (a loopback or https URL).
///
/// # Errors
///
/// Returns [`JoinError::Link`] for a redirect URI the library would refuse
/// or a malformed clerk name.
pub fn authorize_clerk(
    library: &InspectedLibrary,
    redirect_uri: &str,
    clerk_name: &str,
) -> Result<ClerkAuthorization, JoinError> {
    if !is_branch_redirect_uri(redirect_uri) {
        return Err(JoinError::Link(
            "this install's address must be loopback or https to sign in".into(),
        ));
    }
    let clerk_name = clerk_name.trim();
    if !is_clerk_name(clerk_name) {
        return Err(JoinError::Link(
            "this computer's name must be 1 to 64 characters".into(),
        ));
    }
    let verifier = random_token();
    let state = random_token();
    let mut url = url::Url::parse(&format!("{}{AUTHORIZE_PATH}", library.origin))
        .map_err(|_| JoinError::Link("the library address does not parse".into()))?;
    url.query_pairs_mut()
        .append_pair("client_id", CLERK_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", CLERK_SCOPE)
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("clerk_name", clerk_name);
    Ok(ClerkAuthorization {
        authorize_url: url.into(),
        state,
        verifier,
        redirect_uri: redirect_uri.to_string(),
        clerk_name: clerk_name.to_string(),
    })
}

/// What [`redeem_clerk_code`] needs: the inspected library and the pending
/// sign-in.
pub struct ClerkRedeem<'a> {
    /// The library's canonical origin.
    pub origin: &'a str,
    /// The pin confirmed for a self-signed library.
    pub spki_pin: Option<&'a str>,
    /// The library id the library's info reported; the answer must match.
    pub library_id: &'a str,
    /// The library's name from its info.
    pub library_name: &'a str,
    /// The code from the redirect.
    pub code: &'a str,
    /// The sign-in's PKCE verifier.
    pub verifier: &'a str,
    /// The redirect URI the authorize request named.
    pub redirect_uri: &'a str,
    /// The name this install signed in as.
    pub clerk_name: &'a str,
}

/// Redeems a clerk sign-in's code for a library card and records it:
/// [`exchange_clerk_code`], then [`ExchangedCard::record`].
///
/// The code, the verifier and the card are registered for redaction before
/// they could reach a log; the code and verifier go once in the request
/// body.
///
/// # Errors
///
/// Returns [`JoinError::Refused`] when the library refuses the code (spent,
/// expired, or not this sign-in's), [`JoinError`] when it is unreachable
/// or answers another status, [`JoinError::Record`] when its answer is
/// malformed or names another library, and [`JoinError::Unsaved`] when the
/// card could not be recorded here (the library then holds a session this
/// install cannot use; the person ends it from the library's session list).
pub async fn redeem_clerk_code(
    store: &LibraryStore,
    redeem: &ClerkRedeem<'_>,
) -> Result<ClerkLink, JoinError> {
    exchange_clerk_code(redeem).await?.record(store).await
}

/// A library card the library handed over for a clerk sign-in, not yet
/// recorded here. `Debug` prints no secret.
pub struct ExchangedCard {
    /// The card; registered for redaction.
    card: LibraryCard,
    /// The clerk link to record it under.
    link: NewClerkLink,
}

impl std::fmt::Debug for ExchangedCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangedCard")
            .field("library_id", &self.link.library_id)
            .field("origin", &self.link.origin)
            .finish_non_exhaustive()
    }
}

impl ExchangedCard {
    /// The library the card is for.
    #[must_use]
    pub fn library_id(&self) -> &str {
        &self.link.library_id
    }

    /// Seals the card into a clerk link
    /// (`bookclerk_library::control_plane::record_clerk_link`).
    ///
    /// # Errors
    ///
    /// Returns [`JoinError::Unsaved`] when the card could not be recorded
    /// here, among others while this install is still an active branch of
    /// the library (the library then holds a session this install cannot
    /// use; the person ends it from the library's session list).
    pub async fn record(self, store: &LibraryStore) -> Result<ClerkLink, JoinError> {
        let link = record_clerk_link(store, &self.link, &self.card)
            .await
            .map_err(|err| JoinError::Unsaved {
                node_id: None,
                reason: err.to_string(),
            })?;
        tracing::info!(
            library_id = %link.library_id,
            origin = %link.origin,
            pinned = link.spki_pin.is_some(),
            "signed in to the library with a library card"
        );
        Ok(link)
    }
}

/// Sends a clerk sign-in's code and verifier to the library and checks
/// the card it answers is for the library that was inspected, without
/// recording it here ([`ExchangedCard::record`] does).
///
/// The code, the verifier and the card are registered for redaction before
/// they could reach a log; the code and verifier go once in the request
/// body.
///
/// # Errors
///
/// Returns [`JoinError::Refused`] when the library refuses the code (spent,
/// expired, or not this sign-in's), [`JoinError`] when it is unreachable
/// or answers another status, and [`JoinError::Record`] when its answer is
/// malformed or names another library.
pub async fn exchange_clerk_code(redeem: &ClerkRedeem<'_>) -> Result<ExchangedCard, JoinError> {
    let endpoint = OwnerEndpoint::new(redeem.origin, redeem.spki_pin)?;
    bookclerk_config::register_secret(redeem.code);
    bookclerk_config::register_secret(redeem.verifier);
    let body = Zeroizing::new(
        serde_json::to_vec(&ClerkTokenRequest {
            code: redeem.code.to_string(),
            code_verifier: redeem.verifier.to_string(),
            redirect_uri: redeem.redirect_uri.to_string(),
        })
        .map_err(|err| JoinError::Record(err.to_string()))?,
    );
    let (status, answer) = endpoint.post_json(CLERK_TOKEN_PATH, &body).await?;
    drop(body);
    let answer = Zeroizing::new(answer);
    if status == 403
        && serde_json::from_slice::<serde_json::Value>(&answer)
            .ok()
            .and_then(|value| value.get("error")?.as_str().map(str::to_string))
            .as_deref()
            == Some("sign_in_refused")
    {
        // A code the library will not redeem: unknown, spent, expired, or
        // sent with another verifier or redirect.
        return Err(JoinError::Refused);
    }
    if status != 200 {
        return Err(http_error(status, &answer));
    }
    let parsed = parse_clerk_token_answer(&answer)
        .ok_or_else(|| JoinError::Record("malformed library card answer".into()))?;
    bookclerk_config::register_secret(&parsed.token);
    let card = LibraryCard::new(parsed.token).map_err(|err| JoinError::Record(err.to_string()))?;
    if parsed.library_id != redeem.library_id {
        return Err(JoinError::Record(
            "the library answered for another library".into(),
        ));
    }
    Ok(ExchangedCard {
        card,
        link: NewClerkLink {
            library_id: parsed.library_id,
            origin: endpoint.origin().to_string(),
            spki_pin: redeem.spki_pin.map(str::to_string),
            library_name: redeem.library_name.to_string(),
            remote_user_id: parsed.user.id,
            display_name: parsed.user.display_name,
            clerk_name: redeem.clerk_name.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_node_proto::{parse_clerk_query, OwnerInfo};

    fn library() -> InspectedLibrary {
        InspectedLibrary {
            info: serde_json::from_value::<OwnerInfo>(serde_json::json!({
                "library_id": "lib-1",
                "library_name": "library.example",
                "origin": "https://library.example",
                "transport": "tls",
                "enrollment": false,
                "software_version": "1.0.0",
            }))
            .expect("info"),
            origin: "https://library.example".into(),
            spki_pin: None,
        }
    }

    #[test]
    fn the_authorize_url_is_a_request_the_library_accepts() {
        let auth = authorize_clerk(
            &library(),
            "http://127.0.0.1:8787/clerk/callback",
            " Laptop ",
        )
        .unwrap();
        assert_eq!(auth.clerk_name, "Laptop");
        let url = url::Url::parse(&auth.authorize_url).unwrap();
        assert_eq!(url.path(), AUTHORIZE_PATH);
        let params = parse_clerk_query(url.query().unwrap()).expect("parses");
        assert_eq!(bookclerk_node_proto::clerk_request_problem(&params), None);
        assert_eq!(params.state.as_deref(), Some(auth.state.as_str()));
        assert_eq!(params.code_challenge, pkce_challenge(&auth.verifier));
        assert_eq!(params.clerk_name.as_deref(), Some("Laptop"));
        let debug = format!("{auth:?}");
        assert!(!debug.contains(auth.verifier.as_str()));
        assert!(!debug.contains(auth.state.as_str()));
    }

    #[test]
    fn a_bad_redirect_or_name_is_refused() {
        assert!(matches!(
            authorize_clerk(&library(), "http://example.com/cb", "Laptop"),
            Err(JoinError::Link(_))
        ));
        assert!(matches!(
            authorize_clerk(&library(), "https://me.example/cb", "  "),
            Err(JoinError::Link(_))
        ));
        assert!(matches!(
            authorize_clerk(&library(), "https://me.example/cb", &"x".repeat(65)),
            Err(JoinError::Link(_))
        ));
    }
}
