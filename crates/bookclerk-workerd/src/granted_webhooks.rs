//! `POST /webhooks/<op>` on the granted reverse channel: a workerd isolate's
//! `env.WEBHOOKS` calls, parsed and answered over plain byte slices.
//!
//! The isolate holds a webhooks-only grant token minted at `open`; each call
//! names its operation in the path (`url`, `pull`, `ack`) and carries that
//! operation's fields as JSON. Every request is checked with the same
//! `bookclerk_plugin_abi::webhooks` rules the host applies again when it
//! serves `Webhooks`, so a malformed call fails here with a typed error and
//! never reaches the host. Keeping parsing sans-IO lets the `webhook_route`
//! fuzz target drive the code the bridge runs.

use bookclerk_plugin_abi::webhooks::{check_webhook_ack, check_webhook_name, check_webhook_pull};
use bookclerk_plugin_abi::{
    PluginError, WebhookAckRequest, WebhookPullPage, WebhookPullRequest,
    MAX_WEBHOOK_LEASE_ID_BYTES, MAX_WEBHOOK_PULL_BATCH,
};
use serde::{Deserialize, Serialize};

/// Largest `/webhooks/*` request body: an ack naming the most leases in
/// both lists at their longest, with JSON quoting, plus room for the name.
/// The ABI checks bound each field once decoded; this only bounds the read.
pub const MAX_WEBHOOK_ROUTE_BODY_BYTES: u32 =
    2 * MAX_WEBHOOK_PULL_BATCH * (MAX_WEBHOOK_LEASE_ID_BYTES + 4) + 1024;

/// Operations the route serves, in path spelling.
pub const WEBHOOK_ROUTE_OPS: [&str; 3] = ["url", "pull", "ack"];

/// `POST /webhooks/url` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteUrl {
    /// `[[webhooks]]` name.
    pub name: String,
}

/// `POST /webhooks/pull` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutePull {
    /// `[[webhooks]]` name.
    pub name: String,
    /// Most deliveries to lease, 1 to `maxWebhookPullBatch`.
    pub batch_size: u32,
    /// Lease length in milliseconds; zero or absent means one minute.
    #[serde(default)]
    pub visibility_timeout_ms: u32,
}

/// `POST /webhooks/ack` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteAck {
    /// `[[webhooks]]` name.
    pub name: String,
    /// Leases whose deliveries are done.
    #[serde(default)]
    pub acks: Vec<String>,
    /// Leases whose deliveries the next pull offers again.
    #[serde(default)]
    pub retries: Vec<String>,
}

/// One checked `env.WEBHOOKS` call, ready for the host's `Webhooks` stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookCall {
    /// `Webhooks.url`.
    Url(String),
    /// `Webhooks.pull`.
    Pull(WebhookPullRequest),
    /// `Webhooks.ack`.
    Ack(WebhookAckRequest),
}

/// What the host answered one [`WebhookCall`] with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookReply {
    /// `url` answered this URL, or none.
    Url(Option<String>),
    /// `pull` leased these deliveries.
    Pulled(WebhookPullPage),
    /// `ack` finished.
    Done,
}

/// Decodes `body` as the JSON object of one route operation.
///
/// Serde would also read a struct from a JSON array of its fields in order;
/// the route takes only an object, so the shape is checked first.
fn decode<T: serde::de::DeserializeOwned>(op: &str, body: &[u8]) -> Result<T, PluginError> {
    let malformed = |err: serde_json::Error| {
        PluginError::invalid_params(format!("malformed webhooks {op}: {err}"))
    };
    let value: serde_json::Value = serde_json::from_slice(body).map_err(malformed)?;
    if !value.is_object() {
        return Err(PluginError::invalid_params(format!(
            "malformed webhooks {op}: the body must be a JSON object"
        )));
    }
    serde_json::from_value(value).map_err(malformed)
}

/// Parses and checks one `POST /webhooks/<op>` call.
///
/// # Arguments
///
/// * `op` - Path segment after `/webhooks/`.
/// * `body` - Request body bytes, already bounded by
///   [`MAX_WEBHOOK_ROUTE_BODY_BYTES`].
///
/// # Returns
///
/// The call, with every field within the ABI limits.
///
/// # Errors
///
/// Returns `not_found` for an operation the route does not serve, and
/// `invalid_params` for malformed JSON, an unknown field, a malformed name
/// or lease id, or a batch size or lease length out of range.
pub fn parse_webhook_call(op: &str, body: &[u8]) -> Result<WebhookCall, PluginError> {
    match op {
        "url" => {
            let route: RouteUrl = decode(op, body)?;
            check_webhook_name(&route.name)?;
            Ok(WebhookCall::Url(route.name))
        }
        "pull" => {
            let route: RoutePull = decode(op, body)?;
            let request = WebhookPullRequest {
                name: route.name,
                batch_size: route.batch_size,
                visibility_timeout_ms: route.visibility_timeout_ms,
            };
            check_webhook_pull(&request)?;
            Ok(WebhookCall::Pull(request))
        }
        "ack" => {
            let route: RouteAck = decode(op, body)?;
            let request = WebhookAckRequest {
                name: route.name,
                acks: route.acks,
                retries: route.retries,
            };
            check_webhook_ack(&request)?;
            Ok(WebhookCall::Ack(request))
        }
        other => Err(PluginError::not_found(format!(
            "unknown webhooks operation {other:?}"
        ))),
    }
}

/// `url` reply body.
#[derive(Serialize)]
struct UrlBody<'a> {
    /// The URL, or `null` when the library cannot be reached.
    url: Option<&'a str>,
}

/// Encodes a host answer as the route's JSON reply body.
///
/// `url` answers `{"url": "..."}` (or `null`), `pull` answers the
/// [`WebhookPullPage`] (bodies as base64), and `ack` answers `{}`.
///
/// # Errors
///
/// Returns the serializer's error, which a well-formed page never produces.
pub fn webhook_reply_json(reply: &WebhookReply) -> serde_json::Result<Vec<u8>> {
    match reply {
        WebhookReply::Url(url) => serde_json::to_vec(&UrlBody {
            url: url.as_deref(),
        }),
        WebhookReply::Pulled(page) => serde_json::to_vec(page),
        WebhookReply::Done => Ok(b"{}".to_vec()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_parses() {
        assert_eq!(
            parse_webhook_call("url", br#"{"name":"events"}"#).expect("url"),
            WebhookCall::Url("events".into())
        );
        assert_eq!(
            parse_webhook_call("pull", br#"{"name":"events","batchSize":5}"#).expect("pull"),
            WebhookCall::Pull(WebhookPullRequest {
                name: "events".into(),
                batch_size: 5,
                visibility_timeout_ms: 0,
            })
        );
        assert_eq!(
            parse_webhook_call("ack", br#"{"name":"events","acks":["a"],"retries":["b"]}"#)
                .expect("ack"),
            WebhookCall::Ack(WebhookAckRequest {
                name: "events".into(),
                acks: vec!["a".into()],
                retries: vec!["b".into()],
            })
        );
    }

    #[test]
    fn malformed_calls_are_refused() {
        for (op, body) in [
            ("url", &br#"["events"]"#[..]),
            ("url", br#"{"name":"Events"}"#),
            ("url", br#"{"name":"events","extra":1}"#),
            ("pull", br#"{"name":"events","batchSize":0}"#),
            ("pull", br#"{"name":"events","batchSize":101}"#),
            ("ack", br#"{"name":"events","acks":["a"],"retries":["a"]}"#),
            ("ack", br#"{"name":"events","acks":["a b"]}"#),
            ("delete", br"{}"),
        ] {
            assert!(parse_webhook_call(op, body).is_err(), "{op} {body:?}");
        }
    }

    #[test]
    fn a_full_ack_fits_the_body_cap() {
        let lease = "l".repeat(MAX_WEBHOOK_LEASE_ID_BYTES as usize);
        let leases: Vec<String> = (0..MAX_WEBHOOK_PULL_BATCH)
            .map(|i| format!("{}{i:03}", &lease[3..]))
            .collect();
        let body = serde_json::to_vec(&RouteAck {
            name: "a".repeat(32),
            acks: leases.clone(),
            retries: leases.iter().map(|l| l.replace('l', "r")).collect(),
        })
        .expect("json");
        assert!(body.len() <= MAX_WEBHOOK_ROUTE_BODY_BYTES as usize);
        assert!(parse_webhook_call("ack", &body).is_ok());
    }

    #[test]
    fn replies_encode() {
        assert_eq!(
            webhook_reply_json(&WebhookReply::Url(None)).expect("json"),
            br#"{"url":null}"#
        );
        assert_eq!(
            webhook_reply_json(&WebhookReply::Done).expect("json"),
            b"{}"
        );
    }
}
