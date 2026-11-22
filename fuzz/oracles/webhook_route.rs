//! Shared oracle for the `webhook_route` fuzz target and the
//! `bookclerk-workerd` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one `POST /webhooks/<op>` call a workerd isolate sends on
//! the granted reverse channel: the first byte picks the operation (`0` to
//! `2` name `url`, `pull` and `ack`; anything else is an operation the route
//! does not serve) and the rest is the JSON body. [`parse_webhook_call`]
//! must:
//!
//! - never panic;
//! - refuse an unknown operation with `not_found`, and nothing else with it;
//! - refuse a body that is not a JSON object, or that carries a field the
//!   operation does not define, with `invalid_params`, and fail with
//!   nothing else;
//! - accept only calls inside the ABI rules, restated here: names of 1 to
//!   `maxWebhookNameBytes` bytes matching `[a-z0-9][a-z0-9-]*`, batch sizes
//!   of 1 to `maxWebhookPullBatch`, visibility timeouts up to
//!   `maxWebhookVisibilityMs`, at most `maxWebhookPullBatch` lease ids per
//!   ack list, each 1 to `maxWebhookLeaseIdBytes` bytes of
//!   `[A-Za-z0-9_-]`, and no lease in both lists;
//! - parse what it accepts the same way again after the call is written
//!   back out in the route's JSON form.

use bookclerk_plugin_abi::{
    PluginErrorCode, MAX_WEBHOOK_LEASE_ID_BYTES, MAX_WEBHOOK_NAME_BYTES, MAX_WEBHOOK_PULL_BATCH,
    MAX_WEBHOOK_VISIBILITY_MS,
};
use bookclerk_workerd::granted_webhooks::{
    parse_webhook_call, RouteAck, RoutePull, RouteUrl, WebhookCall, WEBHOOK_ROUTE_OPS,
};

/// Fields each operation's body may carry.
fn allowed_fields(op: &str) -> &'static [&'static str] {
    match op {
        "url" => &["name"],
        "pull" => &["name", "batchSize", "visibilityTimeoutMs"],
        "ack" => &["name", "acks", "retries"],
        _ => &[],
    }
}

/// The name rule, restated.
fn name_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    let cap = usize::try_from(MAX_WEBHOOK_NAME_BYTES).unwrap_or(usize::MAX);
    !bytes.is_empty()
        && bytes.len() <= cap
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// The lease id rule, restated.
fn lease_ok(lease: &str) -> bool {
    let cap = usize::try_from(MAX_WEBHOOK_LEASE_ID_BYTES).unwrap_or(usize::MAX);
    (1..=cap).contains(&lease.len())
        && lease
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Writes an accepted call back out as its route JSON body.
fn route_body(call: &WebhookCall) -> Vec<u8> {
    let encoded = match call {
        WebhookCall::Url(name) => serde_json::to_vec(&RouteUrl { name: name.clone() }),
        WebhookCall::Pull(request) => serde_json::to_vec(&RoutePull {
            name: request.name.clone(),
            batch_size: request.batch_size,
            visibility_timeout_ms: request.visibility_timeout_ms,
        }),
        WebhookCall::Ack(request) => serde_json::to_vec(&RouteAck {
            name: request.name.clone(),
            acks: request.acks.clone(),
            retries: request.retries.clone(),
        }),
    };
    match encoded {
        Ok(bytes) => bytes,
        Err(err) => panic!("an accepted call does not serialize: {err}"),
    }
}

/// Checks the route parser's invariants for one input.
///
/// # Panics
///
/// Panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let op = WEBHOOK_ROUTE_OPS
        .get(usize::from(selector))
        .copied()
        .unwrap_or("unknown");
    let parsed = parse_webhook_call(op, body);
    if !WEBHOOK_ROUTE_OPS.contains(&op) {
        let code = parsed.as_ref().err().map(|err| err.code);
        assert_eq!(code, Some(PluginErrorCode::NotFound), "unknown op");
        return;
    }
    let object = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.as_object().cloned());
    let call = match parsed {
        Err(err) => {
            assert_eq!(
                err.code,
                PluginErrorCode::InvalidParams,
                "{op}: unexpected {:?}",
                err.code
            );
            return;
        }
        Ok(call) => call,
    };
    let Some(fields) = object else {
        panic!("{op}: accepted a body that is not an object");
    };
    assert!(
        fields
            .keys()
            .all(|key| allowed_fields(op).contains(&key.as_str())),
        "{op}: accepted an unknown field"
    );
    let cap = usize::try_from(MAX_WEBHOOK_PULL_BATCH).unwrap_or(usize::MAX);
    match &call {
        WebhookCall::Url(name) => {
            assert_eq!(op, "url");
            assert!(name_ok(name), "url: bad name {name:?}");
        }
        WebhookCall::Pull(request) => {
            assert_eq!(op, "pull");
            assert!(name_ok(&request.name), "pull: bad name");
            assert!(
                (1..=MAX_WEBHOOK_PULL_BATCH).contains(&request.batch_size),
                "pull: batch size {}",
                request.batch_size
            );
            assert!(
                request.visibility_timeout_ms <= MAX_WEBHOOK_VISIBILITY_MS,
                "pull: visibility {}",
                request.visibility_timeout_ms
            );
        }
        WebhookCall::Ack(request) => {
            assert_eq!(op, "ack");
            assert!(name_ok(&request.name), "ack: bad name");
            assert!(request.acks.len() <= cap && request.retries.len() <= cap);
            assert!(
                request
                    .acks
                    .iter()
                    .chain(&request.retries)
                    .all(|l| lease_ok(l)),
                "ack: bad lease id"
            );
            assert!(
                !request
                    .acks
                    .iter()
                    .any(|lease| request.retries.contains(lease)),
                "ack: a lease in both lists"
            );
        }
    }
    let again = parse_webhook_call(op, &route_body(&call));
    assert_eq!(
        again.ok(),
        Some(call),
        "{op}: written-back call parses differently"
    );
}
