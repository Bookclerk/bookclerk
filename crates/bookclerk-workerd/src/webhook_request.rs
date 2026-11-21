//! Sans-IO checks for one request on the public webhook route
//! (`POST /hooks/<token>/<name>`), in the order the route applies them.
//!
//! The route is outside operator auth, so everything here reads bytes from
//! anyone on the Internet. It runs over plain values: the method, the
//! declared length, the raw header pairs and the body. The `webhook_request`
//! fuzz target and the unit tests drive the same functions the daemon does.
//! It lives beside the isolate routes so the fuzz build needs no host crate.
//!
//! 1. [`check_webhook_head`]: the method must be `POST` and a declared
//!    length must fit the row's `max_body_bytes`, before the body is read.
//! 2. [`WebhookRateLimiter`]: at most the row's `rate_per_minute` requests
//!    per instance and webhook, as a token bucket.
//! 3. [`verify_webhook_request`]: the body fits, the signature rule holds
//!    (HMAC-SHA256 compared in constant time), and the kept headers are
//!    picked out and checked.

use std::collections::HashMap;

use base64::Engine as _;
use bookclerk_plugin_abi::webhooks::check_webhook_delivery;
use bookclerk_plugin_abi::{WebhookDelivery, WebhookHeader, MAX_WEBHOOK_HEADER_VALUE_BYTES};
use bookclerk_plugin_manifest::{
    HmacSignature, SignatureEncoding, WebhookManifest, WebhookSignature,
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Why the route refused a request. Each maps onto one HTTP status, and
/// the reply body never says more than the status does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookRefusal {
    /// The method is not `POST` (405).
    MethodNotAllowed,
    /// The token or name matches no webhook (404).
    NotFound,
    /// The instance's webhook is over its request rate (429).
    RateLimited,
    /// The body is larger than the row allows (413).
    PayloadTooLarge,
    /// A header is malformed or repeated where it must appear once (400).
    BadRequest,
    /// The signature is missing or does not match (401).
    BadSignature,
    /// The webhook cannot take requests now: its signing secret is not set,
    /// webhooks are turned off, or the instance's waiting cap is full (503).
    Unavailable,
}

impl WebhookRefusal {
    /// The HTTP status the route answers with.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::MethodNotAllowed => 405,
            Self::NotFound => 404,
            Self::RateLimited => 429,
            Self::PayloadTooLarge => 413,
            Self::BadRequest => 400,
            Self::BadSignature => 401,
            Self::Unavailable => 503,
        }
    }
}

/// Checks the request line and declared length before the body is read.
///
/// # Arguments
///
/// * `row` - The webhook's manifest row.
/// * `method` - Request method as received.
/// * `content_length` - The declared `Content-Length`, when one was sent.
///
/// # Errors
///
/// [`WebhookRefusal::MethodNotAllowed`] for anything but `POST`;
/// [`WebhookRefusal::PayloadTooLarge`] when the declared length is over the
/// row's `max_body_bytes`.
pub fn check_webhook_head(
    row: &WebhookManifest,
    method: &str,
    content_length: Option<u64>,
) -> Result<(), WebhookRefusal> {
    if method != "POST" {
        return Err(WebhookRefusal::MethodNotAllowed);
    }
    if content_length.is_some_and(|len| len > u64::from(row.max_body_bytes)) {
        return Err(WebhookRefusal::PayloadTooLarge);
    }
    Ok(())
}

/// The one value of header `name`, matched case-insensitively.
///
/// # Returns
///
/// `Ok(None)` when absent.
///
/// # Errors
///
/// [`WebhookRefusal::BadRequest`] when the header appears more than once.
fn single_header<'a>(
    headers: &'a [(String, Vec<u8>)],
    name: &str,
) -> Result<Option<&'a [u8]>, WebhookRefusal> {
    let mut found = None;
    for (key, value) in headers {
        if key.eq_ignore_ascii_case(name) {
            if found.is_some() {
                return Err(WebhookRefusal::BadRequest);
            }
            found = Some(value.as_slice());
        }
    }
    Ok(found)
}

/// Decodes the digest a signature header carries.
///
/// # Returns
///
/// `None` when the prefix is missing or the digest does not decode.
fn signature_digest(rule: &HmacSignature, value: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(value).ok()?.trim();
    let digest = text.strip_prefix(rule.prefix.as_str())?;
    match rule.encoding {
        SignatureEncoding::Hex => hex::decode(digest).ok(),
        SignatureEncoding::Base64 => base64::engine::general_purpose::STANDARD
            .decode(digest)
            .ok(),
    }
}

/// Checks one HMAC-SHA256 signature in constant time.
///
/// # Errors
///
/// [`WebhookRefusal::Unavailable`] when no secret is set;
/// [`WebhookRefusal::BadRequest`] when the header is repeated;
/// [`WebhookRefusal::BadSignature`] when it is missing, malformed or wrong.
fn check_hmac(
    rule: &HmacSignature,
    secret: Option<&[u8]>,
    headers: &[(String, Vec<u8>)],
    body: &[u8],
) -> Result<(), WebhookRefusal> {
    let secret = secret
        .filter(|secret| !secret.is_empty())
        .ok_or(WebhookRefusal::Unavailable)?;
    let value = single_header(headers, &rule.header)?.ok_or(WebhookRefusal::BadSignature)?;
    let digest = signature_digest(rule, value).ok_or(WebhookRefusal::BadSignature)?;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret)
        .map_err(|_| WebhookRefusal::Unavailable)?;
    mac.update(body);
    mac.verify_slice(&digest)
        .map_err(|_| WebhookRefusal::BadSignature)
}

/// Checks one kept header value: at most `maxWebhookHeaderValueBytes` of
/// UTF-8 with no control character but tab.
fn kept_value(value: &[u8]) -> Result<String, WebhookRefusal> {
    if value.len() > MAX_WEBHOOK_HEADER_VALUE_BYTES as usize {
        return Err(WebhookRefusal::BadRequest);
    }
    let text = std::str::from_utf8(value).map_err(|_| WebhookRefusal::BadRequest)?;
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err(WebhookRefusal::BadRequest);
    }
    Ok(text.to_string())
}

/// Verifies a request whose body has been read, and picks out the headers
/// the delivery keeps.
///
/// # Arguments
///
/// * `row` - The webhook's manifest row.
/// * `secret` - The signing secret the row's rule names, when it has one
///   and the operator set it.
/// * `headers` - Raw request headers as `(name, value)` pairs.
/// * `body` - The request body.
///
/// # Returns
///
/// The kept headers, lowercase: `content-type` first when sent, then the
/// row's `headers` in the order it lists them. Absent headers are skipped.
///
/// # Errors
///
/// [`WebhookRefusal::PayloadTooLarge`] for a body over `max_body_bytes`;
/// the `check_hmac` refusals for a signed row; and
/// [`WebhookRefusal::BadRequest`] for a kept header that repeats, is over
/// the value cap or holds a control character.
pub fn verify_webhook_request(
    row: &WebhookManifest,
    secret: Option<&[u8]>,
    headers: &[(String, Vec<u8>)],
    body: &[u8],
) -> Result<Vec<WebhookHeader>, WebhookRefusal> {
    if body.len() > row.max_body_bytes as usize {
        return Err(WebhookRefusal::PayloadTooLarge);
    }
    match &row.signature {
        WebhookSignature::None => {}
        WebhookSignature::HmacSha256(rule) => check_hmac(rule, secret, headers, body)?,
    }
    let mut kept = Vec::with_capacity(row.headers.len() + 1);
    let names = std::iter::once("content-type").chain(
        row.headers
            .iter()
            .map(String::as_str)
            .filter(|name| *name != "content-type"),
    );
    for name in names {
        if let Some(value) = single_header(headers, name)? {
            kept.push(WebhookHeader {
                name: name.to_string(),
                value: kept_value(value)?,
            });
        }
    }
    // The delivery must pass the same check the binding applies before it
    // reaches a guest, so a stored request can always be handed out.
    check_webhook_delivery(&WebhookDelivery {
        lease_id: "check".into(),
        received_at_unix_ms: 0,
        attempt: 0,
        headers: kept.clone(),
        body: Vec::new(),
    })
    .map_err(|_| WebhookRefusal::BadRequest)?;
    Ok(kept)
}

/// Token-bucket rate limit per instance and webhook, kept in memory on the
/// node that serves the route.
///
/// Each bucket holds up to the row's `rate_per_minute` tokens and refills
/// continuously at that rate; a request takes one. Buckets idle long enough
/// to be full again are dropped when the map grows past
/// [`WebhookRateLimiter::MAX_BUCKETS`].
#[derive(Debug, Default)]
pub struct WebhookRateLimiter {
    /// Bucket per `(instance id, webhook name)`: tokens left (in units of
    /// 1/60 000 of a request) and when it was last refilled, in Unix
    /// milliseconds.
    buckets: HashMap<(String, String), (u64, i64)>,
}

impl WebhookRateLimiter {
    /// Buckets kept before full, idle ones are swept.
    pub const MAX_BUCKETS: usize = 4096;

    /// Takes one request from the bucket of `(instance, name)`.
    ///
    /// # Arguments
    ///
    /// * `instance` - Plugin instance id.
    /// * `name` - Webhook name.
    /// * `rate_per_minute` - The row's rate, at least 1.
    /// * `now_ms` - Current time in Unix milliseconds.
    ///
    /// # Errors
    ///
    /// [`WebhookRefusal::RateLimited`] when the bucket is empty.
    pub fn take(
        &mut self,
        instance: &str,
        name: &str,
        rate_per_minute: u32,
        now_ms: i64,
    ) -> Result<(), WebhookRefusal> {
        // Tokens are kept in units of 1/60 000 of a request, so a bucket
        // refills by exactly `rate` units per millisecond with no rounding
        // that frequent calls could lose.
        const PER_REQUEST: u64 = 60_000;
        let rate = u64::from(rate_per_minute.max(1));
        let capacity = rate.saturating_mul(PER_REQUEST);
        if self.buckets.len() >= Self::MAX_BUCKETS {
            self.buckets
                .retain(|_, (_, last)| now_ms.saturating_sub(*last) < 60_000);
        }
        let bucket = self
            .buckets
            .entry((instance.to_string(), name.to_string()))
            .or_insert((capacity, now_ms));
        let elapsed = u64::try_from(now_ms.saturating_sub(bucket.1)).unwrap_or(0);
        bucket.0 = bucket
            .0
            .saturating_add(rate.saturating_mul(elapsed))
            .min(capacity);
        bucket.1 = bucket.1.max(now_ms);
        if bucket.0 < PER_REQUEST {
            return Err(WebhookRefusal::RateLimited);
        }
        bucket.0 -= PER_REQUEST;
        Ok(())
    }
}

#[cfg(test)]
#[path = "webhook_request_tests.rs"]
mod tests;
