//! The `b2` destination's change feed: B2 event notifications posted to the
//! host's webhook (`b2` feature).
//!
//! B2 posts a bucket's object events to a webhook URL, signed with a shared
//! secret. The host serves the destination's `[[webhooks]]` row, checks the
//! signature and keeps each request until the guest acknowledges it, so the
//! guest pulls requests and later acknowledges or retries them by lease id:
//! the receive / delete / release cycle the shared
//! [`QueueFeed`](crate::s3_queue::QueueFeed) runs. Request bodies are read
//! by [`crate::b2_events`].
//!
//! The feed is on only when the operator sets a signing secret and the host
//! has a webhook URL (the library is reachable from the Internet). The feed
//! then keeps three rules on the bucket for the destination's prefix (one
//! per event category, see [`b2_events::rule_names`]) and checks them again
//! every [`RULE_RECHECK_SECS`], or at once when the URL or secret changes. A
//! page after the rules were (re)written, or after they could not be
//! checked, is incomplete, so the host lists. When the feed is turned off,
//! or the URL goes away, the rules it wrote are removed and the host lists.
//!
//! What the feed last did to the rules is kept in a small file beside the
//! queue state, under the guest's `HOME`, because the guest sleeps between
//! calls.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    WebhookAckRequest, WebhookPullRequest, Webhooks, MAX_WEBHOOK_PULL_BATCH,
    MAX_WEBHOOK_VISIBILITY_MS,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::b2::B2Backend;
use crate::b2_events::{self, B2EventScope, B2SigningSecret};
use crate::error::{Result, StorageError};
use crate::s3_guest::ChangeSource;
use crate::s3_queue::{
    MessageParser, QueueFeed, QueueMessage, QueueSource, Received, VISIBILITY_TIMEOUT_SECS,
};
use crate::ChangeFeedPage;

/// Seconds between checks that the bucket still holds the destination's
/// rules as written. B2 suspends a rule whose deliveries keep failing, and
/// a suspended rule sends nothing, so the feed looks again this often.
pub const RULE_RECHECK_SECS: u64 = 60 * 60;

/// How long a pulled request stays leased: the shared feed's visibility
/// timeout, within the binding's limit.
fn lease_millis() -> u32 {
    let wanted = u32::try_from(VISIBILITY_TIMEOUT_SECS)
        .unwrap_or(u32::MAX)
        .saturating_mul(1000);
    wanted.min(MAX_WEBHOOK_VISIBILITY_MS)
}

/// The host's store of one webhook's requests, as a queue.
struct WebhookQueue {
    /// The guest's webhook binding.
    hooks: Rc<dyn Webhooks>,
    /// The `[[webhooks]]` row's name.
    name: String,
}

impl QueueSource for WebhookQueue {
    async fn receive(&self, max: usize, _wait_secs: i32) -> Result<Received> {
        let batch = u32::try_from(max)
            .unwrap_or(u32::MAX)
            .clamp(1, MAX_WEBHOOK_PULL_BATCH);
        let page = self
            .hooks
            .pull(WebhookPullRequest {
                name: self.name.clone(),
                batch_size: batch,
                visibility_timeout_ms: lease_millis(),
            })
            .await
            .map_err(|err| StorageError::Remote(format!("webhook pull: {err}")))?;
        Ok(Received {
            messages: page
                .deliveries
                .into_iter()
                .map(|delivery| QueueMessage {
                    body: delivery.body,
                    receipt: delivery.lease_id,
                })
                .collect(),
            missed: page.missed,
        })
    }

    async fn delete(&self, receipts: &[String]) -> Result<()> {
        self.settle(receipts.to_vec(), Vec::new()).await
    }

    async fn release(&self, receipts: &[String]) -> Result<()> {
        self.settle(Vec::new(), receipts.to_vec()).await
    }
}

impl WebhookQueue {
    /// Acknowledges `acks` and offers `retries` again.
    async fn settle(&self, acks: Vec<String>, retries: Vec<String>) -> Result<()> {
        if acks.is_empty() && retries.is_empty() {
            return Ok(());
        }
        self.hooks
            .ack(WebhookAckRequest {
                name: self.name.clone(),
                acks,
                retries,
            })
            .await
            .map_err(|err| StorageError::Remote(format!("webhook ack: {err}")))
    }
}

/// What the feed last did to the bucket's rules.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RuleState {
    /// SHA-256 (hex) of the bucket, prefix, URL and secret the rules were
    /// written for.
    digest: String,
    /// When they were last found or written, in Unix seconds.
    checked_at: u64,
    /// Whether the destination's rules are on the bucket.
    installed: bool,
}

/// Whether the feed runs, and what it needs when it does.
enum Mode {
    /// The operator set a signing secret.
    On {
        /// The guest's webhook binding.
        hooks: Rc<dyn Webhooks>,
        /// The `[[webhooks]]` row's name.
        name: String,
        /// The secret B2 signs requests with.
        secret: B2SigningSecret,
        /// The queue feed over the row's requests (boxed: it is much
        /// larger than the other variant).
        feed: Box<QueueFeed<WebhookQueue>>,
    },
    /// No secret (or no webhook binding): the host lists, and rules the
    /// feed wrote earlier are removed.
    Off,
}

/// The `b2` destination's change feed.
pub struct B2ChangeFeed {
    /// The backend whose bucket sends the events.
    backend: B2Backend,
    /// Where [`RuleState`] is kept.
    rules_path: PathBuf,
    /// On or off.
    mode: Mode,
}

/// The current time in Unix seconds.
fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// SHA-256 (hex) of the newline-joined `parts`.
fn digest_of(parts: &[&str]) -> String {
    hex::encode(sha2::Sha256::digest(parts.join("\n").as_bytes()))
}

impl B2ChangeFeed {
    /// The feed of `backend`'s bucket and prefix, with its state in
    /// `state_dir`.
    ///
    /// # Arguments
    ///
    /// * `backend` - the destination's backend.
    /// * `hooks` - the guest's webhook binding (`None` when the host granted
    ///   none, which turns the feed off).
    /// * `webhook` - the `[[webhooks]]` row B2 posts to.
    /// * `secret` - the signing secret the operator set (`None` turns the
    ///   feed off).
    /// * `state_dir` - a directory that survives the guest sleeping.
    ///
    /// # Errors
    ///
    /// Returns an error when `state_dir` cannot be created.
    pub fn open(
        backend: B2Backend,
        hooks: Option<Box<dyn Webhooks>>,
        webhook: &str,
        secret: Option<B2SigningSecret>,
        state_dir: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let bucket = backend.bucket().to_string();
        let prefix = backend.prefix().to_string();
        let tag = &digest_of(&[&bucket, &prefix])[..16];
        let rules_path = state_dir.join(format!("b2-rules-{tag}.json"));
        let mode = match (hooks, secret) {
            (Some(hooks), Some(secret)) => {
                let hooks: Rc<dyn Webhooks> = Rc::from(hooks);
                let parse: MessageParser = {
                    let bucket = bucket.clone();
                    let prefix = prefix.clone();
                    Box::new(move |body| {
                        b2_events::parse_b2_events(
                            body,
                            B2EventScope {
                                bucket: &bucket,
                                prefix: &prefix,
                            },
                        )
                    })
                };
                let queue = WebhookQueue {
                    hooks: Rc::clone(&hooks),
                    name: webhook.to_string(),
                };
                let feed = QueueFeed::open(
                    queue,
                    parse,
                    &format!("b2-webhook:{webhook}"),
                    &bucket,
                    &prefix,
                    state_dir,
                )?;
                Mode::On {
                    hooks,
                    name: webhook.to_string(),
                    secret,
                    feed: Box::new(feed),
                }
            }
            _ => Mode::Off,
        };
        Ok(Self {
            backend,
            rules_path,
            mode,
        })
    }

    /// Whether the feed is on (a secret and a webhook binding).
    #[must_use]
    pub fn is_on(&self) -> bool {
        match self.mode {
            Mode::On { .. } => true,
            Mode::Off => false,
        }
    }

    /// The saved rule state; a missing or unreadable file is an empty one.
    fn load_rules(&self) -> RuleState {
        std::fs::read(&self.rules_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Saves the rule state; a failure only costs a check next time.
    fn save_rules(&self, state: &RuleState) {
        let saved = serde_json::to_vec(state)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&self.rules_path, bytes));
        if let Err(err) = saved {
            tracing::warn!(error = %err, "B2 notification rule state not saved");
        }
    }

    /// Makes sure the rules post to `url` signed with `secret`.
    ///
    /// # Returns
    ///
    /// `true` when they were already in place, `false` when they were just
    /// written (events before that were never sent) or could not be
    /// checked.
    async fn keep_rules(&self, url: &str, secret: &B2SigningSecret) -> bool {
        let digest = digest_of(&[
            self.backend.bucket(),
            self.backend.prefix(),
            url,
            secret.expose(),
        ]);
        let now = unix_secs();
        let state = self.load_rules();
        if state.installed
            && state.digest == digest
            && now.saturating_sub(state.checked_at) < RULE_RECHECK_SECS
        {
            return true;
        }
        match self.backend.keep_notification_rules(url, secret).await {
            Ok(rewritten) => {
                self.save_rules(&RuleState {
                    digest,
                    checked_at: now,
                    installed: true,
                });
                !rewritten
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "B2 event notification rules could not be set; listing instead"
                );
                false
            }
        }
    }

    /// Removes the rules the feed wrote, when its state says it wrote
    /// some.
    async fn withdraw_rules(&self) {
        let state = self.load_rules();
        if !state.installed {
            return;
        }
        match self.backend.drop_notification_rules().await {
            Ok(_) => self.save_rules(&RuleState::default()),
            Err(err) => {
                tracing::warn!(error = %err, "B2 event notification rules not removed");
            }
        }
    }
}

#[async_trait(?Send)]
impl ChangeSource for B2ChangeFeed {
    async fn changes(&self, cursor: Option<&str>, limit: u32) -> Result<Option<ChangeFeedPage>> {
        let Mode::On {
            hooks,
            name,
            secret,
            feed,
        } = &self.mode
        else {
            self.withdraw_rules().await;
            return Ok(None);
        };
        let url = hooks
            .url(name.clone())
            .await
            .map_err(|err| StorageError::Remote(format!("webhook URL: {err}")))?;
        let Some(url) = url else {
            self.withdraw_rules().await;
            return Ok(None);
        };
        let steady = self.keep_rules(&url, secret).await;
        // A page from the listing fallback carries the "no feed" cursor;
        // to the queue feed it is a new start.
        let cursor = cursor.filter(|cursor| *cursor != crate::guest::NO_FEED_CURSOR);
        let mut page = feed.changes(cursor, limit).await?;
        page.complete &= steady;
        Ok(Some(page))
    }
}
