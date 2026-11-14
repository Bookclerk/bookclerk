//! Where a client sign-in's session token is accepted.
//!
//! A clerk signs in to this library as a person
//! ([`crate::node_api::client`]) and sends its session token as
//! `Authorization: Bookclerk-Client <token>`. The token is accepted only on
//! the route group a library session may use for the person's own books
//! (`Access::OperatorOrPortal`), never on administration, never on routes
//! that check a session themselves, and never as a cookie:
//!
//! 1. [`hold_client_token`] runs before routing, on every request. It takes
//!    a `Bookclerk-Client` value out of the headers, so no route sees it,
//!    and keeps a well-formed token as a [`HeldClientToken`] extension.
//! 2. [`admit_client_token`] runs only in front of that one route group. It
//!    checks the node API transport policy (TLS except on loopback) and
//!    puts the header back, so the group's auth layer and its handlers
//!    resolve the session through [`client_token_from_headers`].
//!
//! Every other route sees a request with no credential and refuses it as
//! signed out.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use bookclerk_node_proto::{parse_client_authorization, CLIENT_AUTH_SCHEME};
use zeroize::Zeroizing;

use crate::api::AppState;

/// A client session token taken out of a request's headers by
/// [`hold_client_token`], for [`admit_client_token`] to put back.
#[derive(Clone)]
pub(crate) struct HeldClientToken(Arc<Zeroizing<String>>);

impl std::fmt::Debug for HeldClientToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HeldClientToken(..)")
    }
}

/// True when an `Authorization` value names the client scheme, whether or
/// not the token after it is well formed.
fn names_client_scheme(value: &[u8]) -> bool {
    let scheme = value.split(|b| *b == b' ').next().unwrap_or_default();
    scheme.eq_ignore_ascii_case(CLIENT_AUTH_SCHEME.as_bytes())
}

/// Takes every `Authorization: Bookclerk-Client` value out of the request
/// and keeps the token, when there is exactly one and it is well formed,
/// as a [`HeldClientToken`] extension. Other `Authorization` values (the
/// operator's `Bearer`) are left alone.
///
/// # Arguments
///
/// * `req` - The request, before routing.
/// * `next` - The rest of the stack.
///
/// # Returns
///
/// The response of the rest of the stack.
pub(crate) async fn hold_client_token(mut req: Request, next: Next) -> Response {
    let headers = req.headers_mut();
    let values: Vec<HeaderValue> = headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .cloned()
        .collect();
    if values.iter().any(|v| names_client_scheme(v.as_bytes())) {
        headers.remove(header::AUTHORIZATION);
        let mut held = None;
        let mut client_values = 0usize;
        for value in values {
            if names_client_scheme(value.as_bytes()) {
                client_values += 1;
                held = parse_client_authorization(value.as_bytes()).map(str::to_owned);
            } else {
                headers.append(header::AUTHORIZATION, value);
            }
        }
        if let (1, Some(token)) = (client_values, held) {
            req.extensions_mut()
                .insert(HeldClientToken(Arc::new(Zeroizing::new(token))));
        }
    }
    next.run(req).await
}

/// Puts a held client token back as `Authorization: Bookclerk-Client` after
/// the node API transport policy passes. Layered only in front of the
/// route group a client may use.
///
/// # Arguments
///
/// * `state` - Shared daemon state, for the transport policy.
/// * `req` - The request, routed to that group.
/// * `next` - The group's auth layer and handler.
///
/// # Returns
///
/// The handler's response, or the transport refusal (404
/// `node_api_unmounted`, 403 `transport_insecure`).
pub(crate) async fn admit_client_token(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(held) = req.extensions().get::<HeldClientToken>().cloned() else {
        return next.run(req).await;
    };
    let (mut parts, body) = req.into_parts();
    if let Err(refused) = crate::node_api::check_transport(&state, &mut parts, "client_token").await
    {
        return transport_refusal(refused.status());
    }
    let value = format!("{CLIENT_AUTH_SCHEME} {}", held.0.as_str());
    match HeaderValue::from_str(&value) {
        Ok(mut value) => {
            value.set_sensitive(true);
            parts.headers.insert(header::AUTHORIZATION, value);
        }
        // A held token is base64url, so this does not happen; the request
        // then goes on with no credential and is refused as signed out.
        Err(_) => tracing::warn!("a held client token could not be put back"),
    }
    next.run(Request::from_parts(parts, body)).await
}

/// The refusal of a client token that failed the transport policy, in the
/// standard error body the routes it reaches already declare (`ErrorJson`),
/// rather than the node API's own shape.
///
/// # Arguments
///
/// * `status` - The status the node API check answered: 404 when the node
///   API is not mounted here, otherwise 403.
///
/// # Returns
///
/// A 403 `transport_insecure` when the connection is not TLS (or loopback),
/// or a 403 `client_sign_in_unavailable` when this daemon does not serve
/// remote installs.
fn transport_refusal(status: axum::http::StatusCode) -> Response {
    use axum::http::StatusCode;
    if status == StatusCode::NOT_FOUND {
        return crate::auth::json_auth_error(
            StatusCode::FORBIDDEN,
            CLIENT_SIGN_IN_UNAVAILABLE,
            CLIENT_SIGN_IN_UNAVAILABLE_MESSAGE,
        );
    }
    crate::auth::json_auth_error(
        StatusCode::FORBIDDEN,
        TRANSPORT_INSECURE,
        TRANSPORT_INSECURE_MESSAGE,
    )
}

/// The `error` of a library card refused by the transport policy.
pub(crate) const TRANSPORT_INSECURE: &str = "transport_insecure";

/// The `message` of that refusal.
pub(crate) const TRANSPORT_INSECURE_MESSAGE: &str =
    "A library card is only accepted over HTTPS, or on this machine.";

/// The `error` of a library card sent where the node API is not mounted.
pub(crate) const CLIENT_SIGN_IN_UNAVAILABLE: &str = "client_sign_in_unavailable";

/// The `message` of that refusal.
pub(crate) const CLIENT_SIGN_IN_UNAVAILABLE_MESSAGE: &str =
    "This library does not accept clerks on this address.";

/// The client session token in `headers`, when an admitted request carries
/// one.
///
/// # Arguments
///
/// * `headers` - Request headers.
///
/// # Returns
///
/// The token, or `None`.
pub(crate) fn client_token_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .find_map(|value| parse_client_authorization(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scheme_is_recognised_without_its_token() {
        assert!(names_client_scheme(b"Bookclerk-Client abc"));
        assert!(names_client_scheme(b"bookclerk-client"));
        assert!(!names_client_scheme(b"Bearer abc"));
        assert!(!names_client_scheme(b"Bookclerk-Clients abc"));
    }
}
