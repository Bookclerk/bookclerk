//! Sans-IO checks for host-served inbound webhooks (`Bindings.webhooks`).
//!
//! The guest SDK applies them before it calls, the host when it serves
//! `Webhooks`, and both sides before a delivery crosses the wire. Keeping
//! them over plain values lets the fuzz target and the unit tests drive the
//! code the host runs.

use crate::generated::{
    WebhookAckRequest, WebhookDelivered, WebhookDelivery, WebhookHeader, WebhookPullPage,
    WebhookPullRequest,
};
use crate::limits::{
    MAX_WEBHOOK_BODY_BYTES, MAX_WEBHOOK_HEADERS, MAX_WEBHOOK_HEADER_VALUE_BYTES,
    MAX_WEBHOOK_LEASE_ID_BYTES, MAX_WEBHOOK_NAME_BYTES, MAX_WEBHOOK_PULL_BATCH,
    MAX_WEBHOOK_VISIBILITY_MS,
};
use crate::{PluginError, Result};

/// Longest header name a delivery may carry, in bytes.
pub const MAX_WEBHOOK_HEADER_NAME_BYTES: usize = 64;

/// Visibility timeout a pull uses when the guest passes zero, in
/// milliseconds (one minute).
pub const DEFAULT_WEBHOOK_VISIBILITY_MS: u32 = 60_000;

/// True when `name` is a valid webhook name: `[a-z0-9][a-z0-9-]*`, at most
/// `maxWebhookNameBytes` bytes.
#[must_use]
pub fn is_valid_webhook_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_WEBHOOK_NAME_BYTES as usize
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// True when `name` is a lowercase HTTP header name: RFC 9110 token
/// characters, lowercase, at most [`MAX_WEBHOOK_HEADER_NAME_BYTES`] bytes.
#[must_use]
pub fn is_valid_webhook_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_WEBHOOK_HEADER_NAME_BYTES
        && name.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Fails closed on a malformed webhook name.
///
/// # Errors
///
/// Returns `invalid_params` when `name` is not a valid webhook name.
pub fn check_webhook_name(name: &str) -> Result<()> {
    if is_valid_webhook_name(name) {
        Ok(())
    } else {
        Err(PluginError::invalid_params("malformed webhook name"))
    }
}

/// Fails closed on a malformed lease id: 1 to `maxWebhookLeaseIdBytes`
/// bytes of ASCII letters, digits, `-` and `_`.
///
/// # Errors
///
/// Returns `invalid_params` for a malformed lease id.
pub fn check_webhook_lease_id(lease: &str) -> Result<()> {
    let ok = !lease.is_empty()
        && lease.len() <= MAX_WEBHOOK_LEASE_ID_BYTES as usize
        && lease
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(PluginError::invalid_params("malformed webhook lease id"))
    }
}

/// A pull request after its checks, with the visibility timeout resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebhookPull {
    /// Most deliveries to lease, 1 to `maxWebhookPullBatch`.
    pub batch_size: u32,
    /// How long leased deliveries stay hidden, in milliseconds, 1 to
    /// `maxWebhookVisibilityMs`.
    pub visibility_ms: u32,
}

/// Checks a `Webhooks.pull` request and resolves its visibility timeout.
///
/// # Arguments
///
/// * `request` - The guest's request.
///
/// # Returns
///
/// The batch size and the visibility timeout to use.
///
/// # Errors
///
/// Returns `invalid_params` for a malformed name, a batch size outside 1 to
/// `maxWebhookPullBatch`, or a visibility timeout above
/// `maxWebhookVisibilityMs`.
pub fn check_webhook_pull(request: &WebhookPullRequest) -> Result<WebhookPull> {
    check_webhook_name(&request.name)?;
    if request.batch_size == 0 || request.batch_size > MAX_WEBHOOK_PULL_BATCH {
        return Err(PluginError::invalid_params(format!(
            "webhook batch size must be 1..={MAX_WEBHOOK_PULL_BATCH}"
        )));
    }
    if request.visibility_timeout_ms > MAX_WEBHOOK_VISIBILITY_MS {
        return Err(PluginError::invalid_params(format!(
            "webhook visibility timeout must be at most {MAX_WEBHOOK_VISIBILITY_MS} ms"
        )));
    }
    let visibility_ms = if request.visibility_timeout_ms == 0 {
        DEFAULT_WEBHOOK_VISIBILITY_MS
    } else {
        request.visibility_timeout_ms
    };
    Ok(WebhookPull {
        batch_size: request.batch_size,
        visibility_ms,
    })
}

/// Checks a `Webhooks.ack` request.
///
/// # Errors
///
/// Returns `invalid_params` for a malformed name or lease id, more than
/// `maxWebhookPullBatch` leases in either list, or a lease named in both.
pub fn check_webhook_ack(request: &WebhookAckRequest) -> Result<()> {
    check_webhook_name(&request.name)?;
    let cap = MAX_WEBHOOK_PULL_BATCH as usize;
    if request.acks.len() > cap || request.retries.len() > cap {
        return Err(PluginError::invalid_params(format!(
            "a webhook ack names at most {cap} leases in each list"
        )));
    }
    for lease in request.acks.iter().chain(&request.retries) {
        check_webhook_lease_id(lease)?;
    }
    if request
        .retries
        .iter()
        .any(|lease| request.acks.contains(lease))
    {
        return Err(PluginError::invalid_params(
            "a webhook lease is both acknowledged and retried",
        ));
    }
    Ok(())
}

/// Checks one kept header.
///
/// # Errors
///
/// Returns `invalid_params` for a header name that is not lowercase token
/// characters or a value over `maxWebhookHeaderValueBytes` or holding a
/// control character other than tab.
fn check_webhook_header(header: &WebhookHeader) -> Result<()> {
    if !is_valid_webhook_header_name(&header.name) {
        return Err(PluginError::invalid_params("malformed webhook header name"));
    }
    if header.value.len() > MAX_WEBHOOK_HEADER_VALUE_BYTES as usize {
        return Err(PluginError::invalid_params(format!(
            "webhook header value exceeds {MAX_WEBHOOK_HEADER_VALUE_BYTES} bytes"
        )));
    }
    if header
        .value
        .bytes()
        .any(|b| (b.is_ascii_control() && b != b'\t') || b == 0x7f)
    {
        return Err(PluginError::invalid_params(
            "webhook header value holds a control character",
        ));
    }
    Ok(())
}

/// Checks one delivery the host is about to hand a guest (or a guest
/// received).
///
/// # Errors
///
/// Returns `invalid_params` for a malformed lease id or header, or more
/// than `maxWebhookHeaders + 1` headers (the row's own plus
/// `content-type`); `payload_too_large` for a body over
/// `maxWebhookBodyBytes`.
pub fn check_webhook_delivery(delivery: &WebhookDelivery) -> Result<()> {
    check_webhook_lease_id(&delivery.lease_id)?;
    let header_cap = MAX_WEBHOOK_HEADERS as usize + 1;
    if delivery.headers.len() > header_cap {
        return Err(PluginError::invalid_params(format!(
            "a webhook delivery carries at most {header_cap} headers"
        )));
    }
    for header in &delivery.headers {
        check_webhook_header(header)?;
    }
    if delivery.body.len() > MAX_WEBHOOK_BODY_BYTES as usize {
        return Err(PluginError::payload_too_large(format!(
            "webhook body of {} bytes exceeds {MAX_WEBHOOK_BODY_BYTES}",
            delivery.body.len()
        )));
    }
    Ok(())
}

/// Checks a pull page against the request it answers.
///
/// # Arguments
///
/// * `page` - The page the host returned.
/// * `batch_size` - The batch size the guest asked for.
///
/// # Errors
///
/// Returns `invalid_params` when the page holds more deliveries than asked
/// for, or any delivery fails [`check_webhook_delivery`].
pub fn check_webhook_page(page: &WebhookPullPage, batch_size: u32) -> Result<()> {
    let cap = batch_size.min(MAX_WEBHOOK_PULL_BATCH) as usize;
    if page.deliveries.len() > cap {
        return Err(PluginError::invalid_params(format!(
            "a webhook pull returned more than {cap} deliveries"
        )));
    }
    page.deliveries.iter().try_for_each(check_webhook_delivery)
}

/// Checks a `WebhookHandler.delivered` event.
///
/// # Errors
///
/// Returns `invalid_params` for a malformed name.
pub fn check_webhook_delivered(event: &WebhookDelivered) -> Result<()> {
    check_webhook_name(&event.name)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::missing_panics_doc)]
mod tests {
    use super::*;

    fn delivery() -> WebhookDelivery {
        WebhookDelivery {
            lease_id: "lease_1".into(),
            received_at_unix_ms: 1,
            attempt: 1,
            headers: vec![WebhookHeader {
                name: "content-type".into(),
                value: "application/json".into(),
            }],
            body: b"{}".to_vec(),
        }
    }

    #[test]
    fn names_follow_the_manifest_rule() {
        assert!(is_valid_webhook_name("events"));
        assert!(is_valid_webhook_name("b2-events"));
        assert!(!is_valid_webhook_name(""));
        assert!(!is_valid_webhook_name("-x"));
        assert!(!is_valid_webhook_name("Events"));
        assert!(!is_valid_webhook_name(&"a".repeat(33)));
    }

    #[test]
    fn pull_resolves_the_default_visibility() {
        let pull = check_webhook_pull(&WebhookPullRequest {
            name: "events".into(),
            batch_size: 10,
            visibility_timeout_ms: 0,
        })
        .expect("valid");
        assert_eq!(pull.visibility_ms, DEFAULT_WEBHOOK_VISIBILITY_MS);
        for (batch, vis) in [
            (0, 0),
            (MAX_WEBHOOK_PULL_BATCH + 1, 0),
            (1, MAX_WEBHOOK_VISIBILITY_MS + 1),
        ] {
            assert!(check_webhook_pull(&WebhookPullRequest {
                name: "events".into(),
                batch_size: batch,
                visibility_timeout_ms: vis,
            })
            .is_err());
        }
    }

    #[test]
    fn ack_refuses_overlap_and_bad_leases() {
        let ok = WebhookAckRequest {
            name: "events".into(),
            acks: vec!["a".into()],
            retries: vec!["b".into()],
        };
        assert!(check_webhook_ack(&ok).is_ok());
        let overlap = WebhookAckRequest {
            retries: vec!["a".into()],
            ..ok.clone()
        };
        assert!(check_webhook_ack(&overlap).is_err());
        let bad = WebhookAckRequest {
            acks: vec!["a b".into()],
            ..ok
        };
        assert!(check_webhook_ack(&bad).is_err());
    }

    #[test]
    fn deliveries_are_bounded() {
        assert!(check_webhook_delivery(&delivery()).is_ok());
        let mut big = delivery();
        big.body = vec![0; MAX_WEBHOOK_BODY_BYTES as usize + 1];
        assert!(check_webhook_delivery(&big).is_err());
        let mut ctl = delivery();
        ctl.headers[0].value = "a\r\nb".into();
        assert!(check_webhook_delivery(&ctl).is_err());
        let mut upper = delivery();
        upper.headers[0].name = "Content-Type".into();
        assert!(check_webhook_delivery(&upper).is_err());
        let page = WebhookPullPage {
            deliveries: vec![delivery(), delivery()],
            missed: false,
        };
        assert!(check_webhook_page(&page, 1).is_err());
        assert!(check_webhook_page(&page, 2).is_ok());
    }
}
