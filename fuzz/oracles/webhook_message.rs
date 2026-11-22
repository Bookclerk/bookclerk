//! Shared oracle for the `webhook_message` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one Cap'n Proto `Webhooks` message a native guest sends
//! the host (a pull or an ack request), or a delivery as the host hands it
//! out: the first byte picks the codec (`0` pull request, `1` ack request,
//! anything else a delivery) and the rest is the serialized message.
//! [`roundtrip_webhook_message`] must decode only what re-encodes to an
//! equal value, and the host's checks must agree with the ABI rules
//! restated here: a pull or ack request passes
//! [`check_webhook_pull`] or [`check_webhook_ack`] exactly when it is
//! inside them.

use bookclerk_plugin_abi::webhooks::{
    check_webhook_ack, check_webhook_delivery, check_webhook_pull,
};
use bookclerk_plugin_abi::{
    roundtrip_webhook_message, RoundTripFailure, WebhookCodec, WebhookWireMessage,
    MAX_WEBHOOK_LEASE_ID_BYTES, MAX_WEBHOOK_NAME_BYTES, MAX_WEBHOOK_PULL_BATCH,
    MAX_WEBHOOK_VISIBILITY_MS,
};

/// The name rule, restated: 1 to `maxWebhookNameBytes` bytes of
/// `[a-z0-9][a-z0-9-]*`.
fn name_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    let cap = usize::try_from(MAX_WEBHOOK_NAME_BYTES).unwrap_or(usize::MAX);
    bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.len() <= cap
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// The lease id rule, restated: 1 to `maxWebhookLeaseIdBytes` bytes of
/// `[A-Za-z0-9_-]`.
fn lease_ok(lease: &str) -> bool {
    let cap = usize::try_from(MAX_WEBHOOK_LEASE_ID_BYTES).unwrap_or(usize::MAX);
    (1..=cap).contains(&lease.len())
        && lease
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Checks one input.
///
/// # Panics
///
/// Panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, bytes)) = data.split_first() else {
        return;
    };
    let codec = match selector {
        0 => WebhookCodec::PullRequest,
        1 => WebhookCodec::AckRequest,
        _ => WebhookCodec::Delivery,
    };
    let message = match roundtrip_webhook_message(codec, bytes) {
        Ok(message) => message,
        Err(RoundTripFailure::Rejected(_)) => return,
        Err(RoundTripFailure::Codec(why)) => panic!("{why}"),
    };
    let cap = usize::try_from(MAX_WEBHOOK_PULL_BATCH).unwrap_or(usize::MAX);
    match message {
        WebhookWireMessage::PullRequest(request) => {
            let inside = name_ok(&request.name)
                && (1..=MAX_WEBHOOK_PULL_BATCH).contains(&request.batch_size)
                && request.visibility_timeout_ms <= MAX_WEBHOOK_VISIBILITY_MS;
            assert_eq!(check_webhook_pull(&request).is_ok(), inside, "{request:?}");
        }
        WebhookWireMessage::AckRequest(request) => {
            let inside = name_ok(&request.name)
                && request.acks.len() <= cap
                && request.retries.len() <= cap
                && request
                    .acks
                    .iter()
                    .chain(&request.retries)
                    .all(|lease| lease_ok(lease))
                && !request
                    .acks
                    .iter()
                    .any(|lease| request.retries.contains(lease));
            assert_eq!(check_webhook_ack(&request).is_ok(), inside, "{request:?}");
        }
        WebhookWireMessage::Delivery(delivery) => {
            let _ = check_webhook_delivery(&delivery);
        }
    }
}
