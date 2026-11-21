//! The `r2` destination's change feed queue: a Cloudflare Queue read
//! through its HTTP pull consumer.
//!
//! An R2 event notification rule sends the bucket's object-create and
//! object-delete events to a Cloudflare Queue. With HTTP pull turned on for
//! that queue, the destination pulls batches of messages and later
//! acknowledges them (or puts them back) by lease id, which is exactly the
//! receive / delete / release cycle the shared [`QueueFeed`] runs. Pull
//! replies and messages are read by [`crate::r2_events`].
//!
//! [`QueueFeed`]: crate::s3_queue::QueueFeed

use async_trait::async_trait;
use bookclerk_plugin_sdk::http::Client;
use serde::Serialize;

use crate::error::{Result, StorageError};
use crate::r2_events::parse_pull_reply;
use crate::s3_queue::{NotificationQueue, QueueMessage, VISIBILITY_TIMEOUT_SECS};

/// The Cloudflare API this client calls.
const API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Most messages one pull asks for. The shared feed works in batches of
/// this size; Queues allows up to 100.
const PULL_BATCH: usize = 10;

/// A Cloudflare account id: 32 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudflareAccountId(String);

impl CloudflareAccountId {
    /// Checks an account id.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::S3`] when `id` is not 32 hex digits.
    pub fn parse(id: &str) -> Result<Self> {
        let id = id.trim().to_ascii_lowercase();
        if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(id))
        } else {
            Err(StorageError::S3(
                "Cloudflare account id must be 32 hex digits".into(),
            ))
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Cloudflare Queue id: 32 lowercase hex digits (the queue's id, not its
/// name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudflareQueueId(String);

impl CloudflareQueueId {
    /// Checks a queue id.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::S3`] when `id` is not 32 hex digits.
    pub fn parse(id: &str) -> Result<Self> {
        let id = id.trim().to_ascii_lowercase();
        if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(id))
        } else {
            Err(StorageError::S3(
                "notification queue id must be the queue's 32-hex-digit id".into(),
            ))
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Cloudflare API token with Queues read and write on the account.
#[derive(Clone)]
pub struct CloudflareApiToken(String);

impl CloudflareApiToken {
    /// Wraps a token.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::S3`] for an empty token or one holding
    /// characters a header cannot carry.
    pub fn new(token: String) -> Result<Self> {
        let token = token.trim().to_string();
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(StorageError::S3("Cloudflare API token is not valid".into()));
        }
        Ok(Self(token))
    }
}

impl std::fmt::Debug for CloudflareApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CloudflareApiToken(<redacted>)")
    }
}

/// [`NotificationQueue`] over a Cloudflare Queue's HTTP pull consumer.
pub(crate) struct CloudflareQueue {
    /// HTTP client (through the workerd socket proxy in a guest).
    http: Client,
    /// `.../accounts/<account>/queues/<queue>/messages`.
    messages_url: String,
    /// Bearer token.
    token: CloudflareApiToken,
}

/// The pull request body.
#[derive(Serialize)]
struct PullRequest {
    /// Most messages to return.
    batch_size: usize,
    /// How long pulled messages stay invisible, in milliseconds.
    visibility_timeout_ms: u64,
}

/// One lease in an ack request.
#[derive(Serialize)]
struct Lease<'a> {
    /// The lease id.
    lease_id: &'a str,
}

/// The ack request body.
#[derive(Serialize)]
struct AckRequest<'a> {
    /// Deliveries done with.
    acks: Vec<Lease<'a>>,
    /// Deliveries to offer again now.
    retries: Vec<Lease<'a>>,
}

impl CloudflareQueue {
    /// A client for `queue` in `account`.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub(crate) fn new(
        account: &CloudflareAccountId,
        queue: &CloudflareQueueId,
        token: CloudflareApiToken,
    ) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|err| StorageError::S3(format!("notification queue client: {err}")))?;
        Ok(Self {
            http,
            messages_url: format!(
                "{API_BASE}/accounts/{}/queues/{}/messages",
                account.as_str(),
                queue.as_str()
            ),
            token,
        })
    }

    /// Posts `body` to `<messages>/<action>` and returns the reply body.
    async fn post<T: Serialize + Sync>(&self, action: &str, body: &T) -> Result<Vec<u8>> {
        let reply = self
            .http
            .post(format!("{}/{action}", self.messages_url))
            .bearer_auth(&self.token.0)
            .json(body)
            .send()
            .await
            .map_err(|err| StorageError::S3(format!("notification queue {action}: {err}")))?;
        let status = reply.status();
        let bytes = reply
            .bytes()
            .await
            .map_err(|err| StorageError::S3(format!("notification queue {action}: {err}")))?;
        if !status.is_success() {
            // The reply's errors say why (a bad token, HTTP pull turned off);
            // the parser keeps only codes and messages.
            let detail = match parse_pull_reply(&bytes) {
                Err(err) => err.to_string(),
                Ok(_) => String::new(),
            };
            return Err(StorageError::S3(format!(
                "notification queue {action}: HTTP {status} {detail}"
            )));
        }
        Ok(bytes.to_vec())
    }

    /// Acknowledges and retries leases in one call.
    async fn ack(&self, acks: &[String], retries: &[String]) -> Result<()> {
        let body = AckRequest {
            acks: acks.iter().map(|id| Lease { lease_id: id }).collect(),
            retries: retries.iter().map(|id| Lease { lease_id: id }).collect(),
        };
        let reply = self.post("ack", &body).await?;
        // The ack reply is an envelope without messages.
        parse_pull_reply(&reply)
            .map(|_| ())
            .map_err(|err| StorageError::S3(format!("notification queue ack: {err}")))
    }
}

#[async_trait]
impl NotificationQueue for CloudflareQueue {
    async fn receive(&self, max: usize, _wait_secs: i32) -> Result<Vec<QueueMessage>> {
        // Queues has no long poll: an empty reply comes back at once.
        let body = PullRequest {
            batch_size: max.clamp(1, PULL_BATCH),
            visibility_timeout_ms: u64::try_from(VISIBILITY_TIMEOUT_SECS)
                .unwrap_or(0)
                .saturating_mul(1000),
        };
        let reply = self.post("pull", &body).await?;
        let messages = parse_pull_reply(&reply)
            .map_err(|err| StorageError::S3(format!("notification queue pull: {err}")))?;
        Ok(messages
            .into_iter()
            .map(|message| QueueMessage {
                body: message.body,
                receipt: message.lease_id,
            })
            .collect())
    }

    async fn delete(&self, receipts: &[String]) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }
        self.ack(receipts, &[]).await
    }

    async fn release(&self, receipts: &[String]) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }
        self.ack(&[], receipts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked_and_lowercased() {
        let account = CloudflareAccountId::parse(" 3F4B7E3DCAB231CBFDAA90A6A28BD548 ").unwrap();
        assert_eq!(account.as_str(), "3f4b7e3dcab231cbfdaa90a6a28bd548");
        assert!(CloudflareAccountId::parse("abc").is_err());
        assert!(CloudflareAccountId::parse(&"g".repeat(32)).is_err());
        assert!(CloudflareQueueId::parse(&"a".repeat(32)).is_ok());
        assert!(CloudflareQueueId::parse("my-queue").is_err());
    }

    #[test]
    fn tokens_are_redacted_and_checked() {
        let token = CloudflareApiToken::new(" secret-token ".into()).unwrap();
        assert_eq!(format!("{token:?}"), "CloudflareApiToken(<redacted>)");
        assert!(CloudflareApiToken::new(String::new()).is_err());
        assert!(CloudflareApiToken::new("a\nb".into()).is_err());
    }

    #[test]
    fn requests_have_the_documented_shape() {
        let pull = serde_json::to_value(PullRequest {
            batch_size: 10,
            visibility_timeout_ms: 43_200_000,
        })
        .unwrap();
        assert_eq!(
            pull,
            serde_json::json!({"batch_size": 10, "visibility_timeout_ms": 43_200_000})
        );
        let acks = ["a".to_string()];
        let retries = ["b".to_string()];
        let ack = serde_json::to_value(AckRequest {
            acks: acks.iter().map(|id| Lease { lease_id: id }).collect(),
            retries: retries.iter().map(|id| Lease { lease_id: id }).collect(),
        })
        .unwrap();
        assert_eq!(
            ack,
            serde_json::json!({"acks": [{"lease_id": "a"}], "retries": [{"lease_id": "b"}]})
        );
    }
}
