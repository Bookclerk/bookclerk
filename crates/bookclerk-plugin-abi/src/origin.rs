//! Sans-IO checks for the `ORIGIN` binding and the `ingress` entrypoint.
//!
//! A relay `ingress` plugin hands the host the head of every public request it
//! carries ([`OriginRequest`]) and, when it starts, the public origin it serves
//! ([`IngressStarted`]). Both come from a guest, so the host checks them here
//! before anything reaches the router or the database; the host checks the
//! response head it sends back the same way. Keeping the rules over plain
//! values lets the fuzz target and the unit tests drive the code the host runs.

use std::net::IpAddr;

use crate::generated::{IngressStarted, OriginHeader, OriginRequest};
use crate::limits::{
    MAX_INGRESS_ORIGIN_BYTES, MAX_ORIGIN_HEADERS, MAX_ORIGIN_HEADER_BYTES, MAX_ORIGIN_TARGET_BYTES,
};
use crate::sockets::is_token;
use crate::{PluginError, Result};

/// Longest request method, in bytes.
pub const MAX_ORIGIN_METHOD_BYTES: usize = 32;

/// Header fields that describe the connection rather than the request, or
/// that the host sets itself. Refused in either direction, compared
/// case-insensitively.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Request header fields the host drops instead of passing on: the
/// authority is the library's own, and the client address comes from
/// [`OriginRequest::client_address`], never from a header.
const IGNORED_REQUEST_HEADERS: &[&str] = &[
    "host",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
];

/// A checked [`OriginRequest`]: what the host dispatches into the router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedOriginRequest {
    /// Request method, an HTTP token, as sent.
    pub method: String,
    /// Path and query in origin form.
    pub target: String,
    /// Header fields in order, without the ignored ones.
    pub headers: Vec<OriginHeader>,
    /// The client's IP address, when the provider reported one.
    pub client: Option<IpAddr>,
}

/// The public origin an ingress plugin serves: `https://` and a lowercase
/// DNS host name, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IngressOrigin(String);

impl IngressOrigin {
    /// The origin as text, `https://<host>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The host name, without the scheme.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.0["https://".len()..]
    }
}

impl std::fmt::Display for IngressOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// True for a header field value: visible ASCII, spaces and tabs.
fn is_field_value(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b == b'\t' || (0x20..0x7f).contains(&b))
}

/// True when `name` is one of `list`, ignoring ASCII case.
fn named(name: &str, list: &[&str]) -> bool {
    list.iter().any(|entry| entry.eq_ignore_ascii_case(name))
}

/// Checks a header list against the count, size and syntax rules.
///
/// # Arguments
///
/// * `headers` - The fields, in order.
/// * `side` - `"request"` or `"response"`, for the error message.
///
/// # Errors
///
/// [`PluginError::invalid_params`] naming the first rule broken. Values
/// are never echoed: they may carry a credential.
fn check_headers(headers: &[OriginHeader], side: &str) -> Result<()> {
    if headers.len() > MAX_ORIGIN_HEADERS as usize {
        return Err(PluginError::invalid_params(format!(
            "{} {side} headers exceed {MAX_ORIGIN_HEADERS}",
            headers.len()
        )));
    }
    let mut total = 0usize;
    for header in headers {
        total = total
            .saturating_add(header.name.len())
            .saturating_add(header.value.len());
        if total > MAX_ORIGIN_HEADER_BYTES as usize {
            return Err(PluginError::invalid_params(format!(
                "{side} headers exceed {MAX_ORIGIN_HEADER_BYTES} bytes"
            )));
        }
        if !is_token(&header.name) {
            return Err(PluginError::invalid_params(format!(
                "a {side} header name is not an HTTP token"
            )));
        }
        if named(&header.name, HOP_BY_HOP) {
            return Err(PluginError::invalid_params(format!(
                "{side} header `{}` describes the connection",
                header.name.to_ascii_lowercase()
            )));
        }
        if !is_field_value(&header.value) {
            return Err(PluginError::invalid_params(format!(
                "{side} header `{}` has a value with control or non-ASCII bytes",
                header.name.to_ascii_lowercase()
            )));
        }
    }
    Ok(())
}

/// True for an origin-form request target: `/`, then visible ASCII with no
/// fragment.
fn is_origin_form(target: &str) -> bool {
    target.starts_with('/')
        && !target.starts_with("//")
        && target
            .bytes()
            .all(|b| (0x21..0x7f).contains(&b) && b != b'#')
}

/// Checks the head of one public request a relay plugin carries and keeps
/// what the host passes on.
///
/// # Arguments
///
/// * `request` - The head as the guest sent it.
///
/// # Returns
///
/// The method, target and client address as checked, and the headers
/// without `host` and forwarding fields.
///
/// # Errors
///
/// [`PluginError::invalid_params`] when the method is not a token, the
/// target is not origin form or too long, a header breaks a rule, or the
/// client address is not an IP address.
pub fn check_origin_request(request: &OriginRequest) -> Result<CheckedOriginRequest> {
    if request.method.len() > MAX_ORIGIN_METHOD_BYTES || !is_token(&request.method) {
        return Err(PluginError::invalid_params(
            "the request method is not an HTTP token",
        ));
    }
    if request.target.len() > MAX_ORIGIN_TARGET_BYTES as usize {
        return Err(PluginError::invalid_params(format!(
            "the request target exceeds {MAX_ORIGIN_TARGET_BYTES} bytes"
        )));
    }
    if !is_origin_form(&request.target) {
        return Err(PluginError::invalid_params(
            "the request target is not a path and query",
        ));
    }
    check_headers(&request.headers, "request")?;
    let client =
        match request.client_address.as_deref() {
            None | Some("") => None,
            Some(text) => Some(text.parse::<IpAddr>().map_err(|_| {
                PluginError::invalid_params("the client address is not an IP address")
            })?),
        };
    let headers = request
        .headers
        .iter()
        .filter(|header| !named(&header.name, IGNORED_REQUEST_HEADERS))
        .cloned()
        .collect();
    Ok(CheckedOriginRequest {
        method: request.method.clone(),
        target: request.target.clone(),
        headers,
        client,
    })
}

/// Checks the head of a response the host sends back to a relay plugin.
///
/// # Errors
///
/// [`PluginError::invalid_params`] when the status is outside 100 to 599
/// or a header breaks a rule.
pub fn check_origin_response(status: u16, headers: &[OriginHeader]) -> Result<()> {
    if !(100..=599).contains(&status) {
        return Err(PluginError::invalid_params(format!(
            "response status {status} is not an HTTP status"
        )));
    }
    check_headers(headers, "response")
}

/// True for one DNS label: 1 to 63 lowercase letters, digits and hyphens,
/// neither starting nor ending with a hyphen.
fn is_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Parses the public origin an ingress plugin reports from `start`.
///
/// # Arguments
///
/// * `started` - The plugin's answer.
///
/// # Returns
///
/// The origin, `https://` and a lowercase host name of at least two labels,
/// at most `maxIngressOriginBytes`; the top label is not all digits, so an
/// IP address never passes.
///
/// # Errors
///
/// [`PluginError::invalid_params`] for any other scheme, a port, path,
/// query, user info, trailing dot or slash, uppercase, or a host that is not
/// a DNS name.
pub fn parse_ingress_origin(started: &IngressStarted) -> Result<IngressOrigin> {
    let refuse = || {
        PluginError::invalid_params(
            "the ingress origin must be https:// and a lowercase host name, nothing else",
        )
    };
    let host = started.origin.strip_prefix("https://").ok_or_else(refuse)?;
    if host.len() > MAX_INGRESS_ORIGIN_BYTES as usize {
        return Err(refuse());
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 || !labels.iter().all(|label| is_label(label)) {
        return Err(refuse());
    }
    if labels
        .last()
        .is_some_and(|top| top.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(refuse());
    }
    Ok(IngressOrigin(started.origin.clone()))
}

#[cfg(test)]
#[path = "origin_tests.rs"]
mod tests;
