//! The `s3` destination's change feed, drained from a notification queue.
//!
//! The bucket announces writes and deletes on an Amazon SQS queue (directly,
//! through SNS, or through EventBridge; see [`crate::s3_events`]). Each
//! `changes` call receives a page of messages and turns them into changes.
//!
//! A message is deleted from the queue only once the host has the page: the
//! next call's cursor names the pages the host has read, and their messages
//! are deleted then. Until that call the messages stay invisible for
//! [`VISIBILITY_TIMEOUT_SECS`]. The receipt handles that delete them are
//! kept in a small file under the guest's `HOME`, because the guest sleeps
//! between calls and the handles are too large to carry in the cursor. A
//! lost file or an expired handle only means a message comes back: the
//! feed's changes are hints, so a repeat is harmless.
//!
//! The queue is a single-consumer feed: every message goes to one caller,
//! so only one destination instance on one node may drain a queue.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::sync::Mutex;

use crate::error::{Result, StorageError};
use crate::s3_events::{parse_event_message, EventScope, ParsedEvent};
use crate::ChangeFeedPage;

/// Prefix of every cursor this feed hands out.
const CURSOR_TAG: &str = "q1";

/// Most messages one receive asks for (the SQS maximum).
const RECEIVE_BATCH: usize = 10;

/// Seconds the first receive of a call waits for a message. Later receives
/// in the same call do not wait.
const RECEIVE_WAIT_SECS: i32 = 1;

/// Seconds a received message stays invisible before the queue offers it
/// again: long enough for the host's next `changes` call to delete it.
pub const VISIBILITY_TIMEOUT_SECS: i32 = 12 * 60 * 60;

/// Pages whose messages wait to be deleted. Older pages are forgotten and
/// their messages come back after the visibility timeout.
const MAX_PENDING_PAGES: usize = 64;

/// Default and largest page size, matching the ABI's `maxChangesPage`.
const MAX_PAGE: u32 = 256;

/// An Amazon SQS queue URL, checked at the boundary.
///
/// Accepted: `https://sqs.<region>.amazonaws.com/<account>/<name>` (also
/// `.amazonaws.com.cn`) and the legacy
/// `https://<region>.queue.amazonaws.com/<account>/<name>`, where the
/// account is 12 digits and the name 1 to 80 letters, digits, `-` or `_`,
/// counting an optional `.fifo` suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueUrl {
    /// The URL as given.
    url: String,
    /// The AWS region in its host name.
    region: String,
}

impl QueueUrl {
    /// Parses and checks a queue URL.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::S3`] naming what is wrong with the URL.
    pub fn parse(url: &str) -> Result<Self> {
        let invalid = |why: &str| StorageError::S3(format!("notification queue URL {why}"));
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| invalid("must start with https://"))?;
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| invalid("needs an account and a queue name"))?;
        let region = queue_region(host).ok_or_else(|| {
            invalid("must be an Amazon SQS queue (https://sqs.<region>.amazonaws.com/...)")
        })?;
        let (account, name) = path
            .split_once('/')
            .ok_or_else(|| invalid("needs an account and a queue name"))?;
        if account.len() != 12 || !account.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("needs a 12-digit account id"));
        }
        let base = name.strip_suffix(".fifo").unwrap_or(name);
        if base.is_empty()
            || name.len() > 80
            || !base
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("has an invalid queue name"));
        }
        Ok(Self {
            url: url.to_string(),
            region: region.to_string(),
        })
    }

    /// The URL as given.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.url
    }

    /// The AWS region the queue lives in.
    #[must_use]
    pub fn region(&self) -> &str {
        &self.region
    }
}

/// The region in an SQS host name, or `None` when the host is not SQS.
fn queue_region(host: &str) -> Option<&str> {
    let region = host
        .strip_prefix("sqs.")
        .and_then(|rest| {
            rest.strip_suffix(".amazonaws.com")
                .or_else(|| rest.strip_suffix(".amazonaws.com.cn"))
        })
        .or_else(|| host.strip_suffix(".queue.amazonaws.com"))?;
    let valid = !region.is_empty()
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    valid.then_some(region)
}

/// One message as the queue delivered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueMessage {
    /// The body.
    pub body: String,
    /// The handle that deletes or releases this delivery.
    pub receipt: String,
}

/// The queue operations the feed needs; [`SqsQueue`] in production.
#[async_trait]
pub(crate) trait NotificationQueue: Send + Sync {
    /// Receives up to `max` messages (at most [`RECEIVE_BATCH`]), waiting up
    /// to `wait_secs` for the first, and hides them for
    /// [`VISIBILITY_TIMEOUT_SECS`].
    async fn receive(&self, max: usize, wait_secs: i32) -> Result<Vec<QueueMessage>>;

    /// Deletes up to [`RECEIVE_BATCH`] deliveries. A handle the queue no
    /// longer accepts is skipped: its message comes back and repeats.
    async fn delete(&self, receipts: &[String]) -> Result<()>;

    /// Makes up to [`RECEIVE_BATCH`] deliveries visible again now.
    async fn release(&self, receipts: &[String]) -> Result<()>;
}

/// [`NotificationQueue`] over Amazon SQS.
pub(crate) struct SqsQueue {
    /// SQS client for the queue's region.
    client: aws_sdk_sqs::Client,
    /// The queue.
    url: QueueUrl,
}

impl SqsQueue {
    /// A queue client sharing the S3 client's credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket-proxy HTTP client cannot be built.
    pub(crate) fn new(sdk: &aws_config::SdkConfig, url: QueueUrl) -> Result<Self> {
        #[allow(unused_mut)]
        let mut config = aws_sdk_sqs::config::Builder::from(sdk)
            .region(aws_sdk_sqs::config::Region::new(url.region().to_string()));
        #[cfg(any(unix, windows))]
        if crate::s3_http::socket_proxy_enabled() {
            config = config.http_client(crate::s3_http::socket_proxy_http_client()?);
        }
        Ok(Self {
            client: aws_sdk_sqs::Client::from_conf(config.build()),
            url,
        })
    }
}

/// Maps an SQS SDK error to a storage error with its message only.
fn sqs_error<E: std::fmt::Display>(what: &str, err: E) -> StorageError {
    StorageError::S3(format!("notification queue {what}: {err}"))
}

/// Logs the entries a batch request could not apply.
fn log_batch_failures(what: &str, failed: &[aws_sdk_sqs::types::BatchResultErrorEntry]) {
    for entry in failed {
        tracing::debug!(
            code = entry.code(),
            "notification queue {what} skipped a delivery; it will be received again"
        );
    }
}

#[async_trait]
impl NotificationQueue for SqsQueue {
    async fn receive(&self, max: usize, wait_secs: i32) -> Result<Vec<QueueMessage>> {
        let max = i32::try_from(max.clamp(1, RECEIVE_BATCH)).unwrap_or(1);
        let out = self
            .client
            .receive_message()
            .queue_url(self.url.as_str())
            .max_number_of_messages(max)
            .wait_time_seconds(wait_secs)
            .visibility_timeout(VISIBILITY_TIMEOUT_SECS)
            .send()
            .await
            .map_err(|err| sqs_error("receive", aws_sdk_sqs::error::DisplayErrorContext(err)))?;
        Ok(out
            .messages()
            .iter()
            .filter_map(|message| {
                Some(QueueMessage {
                    body: message.body().unwrap_or_default().to_string(),
                    receipt: message.receipt_handle()?.to_string(),
                })
            })
            .collect())
    }

    async fn delete(&self, receipts: &[String]) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }
        let entries = receipts
            .iter()
            .enumerate()
            .map(|(i, receipt)| {
                aws_sdk_sqs::types::DeleteMessageBatchRequestEntry::builder()
                    .id(i.to_string())
                    .receipt_handle(receipt)
                    .build()
                    .map_err(|err| sqs_error("delete", err))
            })
            .collect::<Result<Vec<_>>>()?;
        let out = self
            .client
            .delete_message_batch()
            .queue_url(self.url.as_str())
            .set_entries(Some(entries))
            .send()
            .await
            .map_err(|err| sqs_error("delete", aws_sdk_sqs::error::DisplayErrorContext(err)))?;
        log_batch_failures("delete", out.failed());
        Ok(())
    }

    async fn release(&self, receipts: &[String]) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }
        let entries = receipts
            .iter()
            .enumerate()
            .map(|(i, receipt)| {
                aws_sdk_sqs::types::ChangeMessageVisibilityBatchRequestEntry::builder()
                    .id(i.to_string())
                    .receipt_handle(receipt)
                    .visibility_timeout(0)
                    .build()
                    .map_err(|err| sqs_error("release", err))
            })
            .collect::<Result<Vec<_>>>()?;
        let out = self
            .client
            .change_message_visibility_batch()
            .queue_url(self.url.as_str())
            .set_entries(Some(entries))
            .send()
            .await
            .map_err(|err| sqs_error("release", aws_sdk_sqs::error::DisplayErrorContext(err)))?;
        log_batch_failures("release", out.failed());
        if out.failed().is_empty() {
            Ok(())
        } else {
            Err(StorageError::S3(format!(
                "notification queue release: {} of {} entries failed",
                out.failed().len(),
                receipts.len()
            )))
        }
    }
}

/// Deliveries handed to the host on one page, deleted once it has read it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PendingPage {
    /// The page's sequence number (its cursor).
    seq: u64,
    /// Receipt handles of the page's messages.
    receipts: Vec<String>,
}

/// What survives the guest sleeping: the feed's identity and the pages
/// whose messages are still to be deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FeedState {
    /// Random id of this feed; cursors carry it, so a cursor from a lost
    /// state file (or another queue) is recognised as foreign.
    feed: String,
    /// The sequence number the next page gets.
    next_seq: u64,
    /// Pages not yet known to be read, oldest first.
    pending: VecDeque<PendingPage>,
    /// Set before receiving and cleared when the page is handed out: still
    /// set means a receive was interrupted or failed, and messages it took
    /// stay hidden until the visibility timeout, so the next page is
    /// incomplete.
    #[serde(default)]
    receiving: bool,
}

impl FeedState {
    /// A new feed with nothing pending.
    fn fresh() -> Self {
        Self {
            feed: uuid::Uuid::new_v4().simple().to_string(),
            next_seq: 1,
            pending: VecDeque::new(),
            receiving: false,
        }
    }
}

/// The change feed of one destination, drained from its notification queue.
pub(crate) struct QueueFeed {
    /// The queue.
    queue: Box<dyn NotificationQueue>,
    /// The destination's bucket; events for other buckets are skipped.
    bucket: String,
    /// The destination's normalized key prefix.
    prefix: String,
    /// Where [`FeedState`] is kept.
    state_path: PathBuf,
    /// The state, loaded at construction; the lock also serializes calls.
    state: Mutex<FeedState>,
}

impl QueueFeed {
    /// A feed over `queue` whose state lives in `state_dir`, in a file named
    /// after `queue_id`, `bucket` and `prefix`, so each queue and scope
    /// keeps its own and a cursor from another scope is foreign.
    ///
    /// A missing or unreadable state file starts a new feed: its first page
    /// reports that changes may have been missed.
    ///
    /// # Errors
    ///
    /// Returns an error when `state_dir` cannot be created.
    pub(crate) fn open(
        queue: Box<dyn NotificationQueue>,
        queue_id: &str,
        bucket: &str,
        prefix: &str,
        state_dir: &Path,
    ) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let digest = sha2::Sha256::digest(format!("{queue_id}\n{bucket}\n{prefix}").as_bytes());
        let name: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let state_path = state_dir.join(format!("queue-{name}.json"));
        let state = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<FeedState>(&bytes).ok())
            .unwrap_or_else(FeedState::fresh);
        Ok(Self {
            queue,
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
            state_path,
            state: Mutex::new(state),
        })
    }

    /// One page of changes since `cursor`.
    ///
    /// The cursor's pages (and older ones) are deleted from the queue first;
    /// pages handed out after it are released so they are received again.
    /// The first call, and a cursor from another feed (a lost state file, a
    /// changed queue), get only the current position, marked incomplete so
    /// the caller lists. Otherwise the page is `complete` unless a message
    /// could not be read in full, a release failed (the message stays
    /// hidden until the visibility timeout), or the previous call's receive
    /// was interrupted.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::InvalidCursor`] for a cursor that is not a
    /// queue cursor or is ahead of the feed, and queue or state-file errors.
    pub(crate) async fn changes(&self, cursor: Option<&str>, limit: u32) -> Result<ChangeFeedPage> {
        let limit = usize::try_from(if limit == 0 {
            MAX_PAGE
        } else {
            limit.min(MAX_PAGE)
        })
        .unwrap_or(1);
        let mut state = self.state.lock().await;
        let read_up_to = match cursor {
            None => None,
            Some(cursor) => {
                let (feed, seq) = parse_cursor(cursor)?;
                if feed == state.feed && seq >= state.next_seq {
                    return Err(StorageError::InvalidCursor(
                        "queue cursor is ahead of the feed".into(),
                    ));
                }
                (feed == state.feed).then_some(seq)
            }
        };
        // Without a cursor of this feed the caller starts over, so nothing
        // handed out before is known to be read: it is all released, and
        // the page is only the current position (the caller lists).
        let Some(read_up_to) = read_up_to else {
            // The page is incomplete anyway, so a failed release changes
            // nothing the caller is told.
            let _released = self.settle(&mut state, 0).await?;
            return self
                .hand_out(&mut state, Vec::new(), Vec::new(), false, false)
                .await;
        };
        let mut complete = self.settle(&mut state, read_up_to).await? && !state.receiving;
        state.receiving = true;
        self.persist(&state)?;

        let mut events: Vec<ParsedEvent> = Vec::new();
        let mut receipts = Vec::new();
        let mut first = true;
        let mut full = false;
        // Messages count against the page too, so a backlog of skipped or
        // unreadable messages cannot make one call drain the whole queue.
        let mut taken = 0;
        while events.len() < limit {
            if taken >= limit {
                full = true;
                break;
            }
            let want = (limit - events.len().max(taken)).min(RECEIVE_BATCH);
            let wait = if first { RECEIVE_WAIT_SECS } else { 0 };
            first = false;
            let messages = match self.queue.receive(want, wait).await {
                Ok(messages) => messages,
                Err(err) => {
                    let _released = self.release_all(&receipts).await;
                    return Err(err);
                }
            };
            let received = messages.len();
            taken += received;
            let mut overflow = Vec::new();
            for message in messages {
                let parsed = parse_event_message(
                    message.body.as_bytes(),
                    EventScope {
                        bucket: &self.bucket,
                        prefix: &self.prefix,
                    },
                );
                match parsed {
                    Ok(parsed) if events.len() + parsed.len() <= limit => {
                        events.extend(parsed);
                        receipts.push(message.receipt);
                    }
                    Ok(mut parsed) if events.is_empty() => {
                        // One message with more changes than a page holds:
                        // hand out what fits and say the rest was missed.
                        parsed.truncate(limit);
                        events.extend(parsed);
                        receipts.push(message.receipt);
                        complete = false;
                    }
                    Ok(_) => overflow.push(message.receipt),
                    Err(err) => {
                        tracing::warn!(error = %err, "unreadable notification message dropped");
                        receipts.push(message.receipt);
                        complete = false;
                    }
                }
            }
            if !overflow.is_empty() {
                complete &= self.release_all(&overflow).await;
                full = true;
                break;
            }
            if received < want {
                break;
            }
            if events.len() >= limit {
                full = true;
            }
        }

        order_events(&mut events);
        self.hand_out(&mut state, events, receipts, full, complete)
            .await
    }

    /// Records a page as handed out and builds it: the page's deliveries
    /// wait under its sequence number until a later cursor says it was read.
    /// When the state cannot be saved, the deliveries are released.
    async fn hand_out(
        &self,
        state: &mut FeedState,
        events: Vec<ParsedEvent>,
        receipts: Vec<String>,
        more: bool,
        complete: bool,
    ) -> Result<ChangeFeedPage> {
        let seq = state.next_seq;
        let mut next = state.clone();
        next.next_seq = seq.saturating_add(1);
        next.receiving = false;
        if !receipts.is_empty() {
            next.pending.push_back(PendingPage {
                seq,
                receipts: receipts.clone(),
            });
            while next.pending.len() > MAX_PENDING_PAGES {
                next.pending.pop_front();
            }
        }
        if let Err(err) = self.persist(&next) {
            let _released = self.release_all(&receipts).await;
            return Err(err);
        }
        let cursor = format!("{CURSOR_TAG}.{}.{seq}", next.feed);
        *state = next;
        Ok(ChangeFeedPage {
            changes: events.into_iter().map(|event| event.change).collect(),
            cursor,
            more,
            complete,
            live_watch: false,
        })
    }

    /// Deletes the messages of pages up to `seq` and releases those after
    /// it, then saves the state. Sequence numbers start at 1, so `seq` 0
    /// releases every pending page.
    ///
    /// # Returns
    ///
    /// `false` when a release failed.
    ///
    /// # Errors
    ///
    /// Returns delete and state-file errors.
    async fn settle(&self, state: &mut FeedState, seq: u64) -> Result<bool> {
        let (read, later): (Vec<_>, Vec<_>) = state
            .pending
            .iter()
            .cloned()
            .partition(|page| page.seq <= seq);
        if read.is_empty() && later.is_empty() {
            return Ok(true);
        }
        let read: Vec<String> = read.into_iter().flat_map(|page| page.receipts).collect();
        for chunk in read.chunks(RECEIVE_BATCH) {
            self.queue.delete(chunk).await?;
        }
        let later: Vec<String> = later.into_iter().flat_map(|page| page.receipts).collect();
        let released = self.release_all(&later).await;
        state.pending.clear();
        self.persist(state)?;
        Ok(released)
    }

    /// Makes deliveries visible again, best effort: a failure delays them
    /// until the visibility timeout.
    ///
    /// # Returns
    ///
    /// `false` when any release failed.
    async fn release_all(&self, receipts: &[String]) -> bool {
        let mut released = true;
        for chunk in receipts.chunks(RECEIVE_BATCH) {
            if let Err(err) = self.queue.release(chunk).await {
                tracing::warn!(error = %err, "notification queue release failed");
                released = false;
            }
        }
        released
    }

    /// Writes the state file through a temporary file and a rename.
    fn persist(&self, state: &FeedState) -> Result<()> {
        let bytes = serde_json::to_vec(state)
            .map_err(|err| StorageError::S3(format!("notification queue state: {err}")))?;
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.state_path)?;
        Ok(())
    }
}

/// Orders a page oldest first: by event time (events without one first),
/// then the events of each key that carry S3's sequencer among themselves,
/// kept in the places those events already hold. Events without a
/// sequencer stay where the time sort put them.
fn order_events(events: &mut [ParsedEvent]) {
    events.sort_by(|a, b| a.time.cmp(&b.time));
    let mut by_key: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (at, event) in events.iter().enumerate() {
        if event.sequencer.is_some() {
            by_key.entry(event.change.key.clone()).or_default().push(at);
        }
    }
    for slots in by_key.into_values().filter(|slots| slots.len() > 1) {
        let mut group: Vec<ParsedEvent> = slots.iter().map(|&at| events[at].clone()).collect();
        group.sort_by(|a, b| {
            sequencer_cmp(
                a.sequencer.as_deref().unwrap_or_default(),
                b.sequencer.as_deref().unwrap_or_default(),
            )
        });
        for (at, event) in slots.into_iter().zip(group) {
            events[at] = event;
        }
    }
}

/// Compares two sequencers: the shorter one is padded with zeros on the
/// left, as S3 documents, so they compare as hex numbers.
fn sequencer_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let width = a.len().max(b.len());
    format!("{a:0>width$}")
        .to_ascii_uppercase()
        .cmp(&format!("{b:0>width$}").to_ascii_uppercase())
}

/// Splits a queue cursor into its feed id and page number.
fn parse_cursor(cursor: &str) -> Result<(&str, u64)> {
    let invalid = || StorageError::InvalidCursor("not a notification-queue cursor".into());
    let mut parts = cursor.splitn(3, '.');
    if parts.next() != Some(CURSOR_TAG) {
        return Err(invalid());
    }
    let feed = parts
        .next()
        .filter(|id| !id.is_empty())
        .ok_or_else(invalid)?;
    let seq = parts
        .next()
        .and_then(|seq| seq.parse::<u64>().ok())
        .ok_or_else(invalid)?;
    Ok((feed, seq))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use super::*;
    use crate::{ObjectChange, ObjectChangeKind};

    /// An in-memory queue: messages wait in `ready`, received ones sit in
    /// `hidden` until deleted or released.
    #[derive(Default)]
    struct FakeQueue {
        state: StdMutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        ready: VecDeque<(u64, String)>,
        hidden: Vec<(u64, String, String)>,
        deliveries: u64,
        deleted: Vec<u64>,
        fail_receive: bool,
        fail_release: bool,
    }

    impl FakeQueue {
        fn send(&self, body: String) {
            let mut state = self.state.lock().unwrap();
            let id =
                state.ready.len() as u64 + state.hidden.len() as u64 + state.deleted.len() as u64;
            state.ready.push_back((id, body));
        }
        fn waiting(&self) -> usize {
            self.state.lock().unwrap().ready.len()
        }
        fn hidden(&self) -> usize {
            self.state.lock().unwrap().hidden.len()
        }
        fn deleted(&self) -> usize {
            self.state.lock().unwrap().deleted.len()
        }
    }

    #[async_trait]
    impl NotificationQueue for Arc<FakeQueue> {
        async fn receive(&self, max: usize, _wait_secs: i32) -> Result<Vec<QueueMessage>> {
            let mut state = self.state.lock().unwrap();
            if state.fail_receive {
                return Err(StorageError::S3("receive failed".into()));
            }
            let mut out = Vec::new();
            while out.len() < max.min(RECEIVE_BATCH) {
                let Some((id, body)) = state.ready.pop_front() else {
                    break;
                };
                state.deliveries += 1;
                let receipt = format!("r{}", state.deliveries);
                state.hidden.push((id, body.clone(), receipt.clone()));
                out.push(QueueMessage { body, receipt });
            }
            Ok(out)
        }
        async fn delete(&self, receipts: &[String]) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            for receipt in receipts {
                if let Some(at) = state.hidden.iter().position(|(_, _, r)| r == receipt) {
                    let (id, _, _) = state.hidden.remove(at);
                    state.deleted.push(id);
                }
            }
            Ok(())
        }
        async fn release(&self, receipts: &[String]) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if state.fail_release {
                return Err(StorageError::S3("release failed".into()));
            }
            for receipt in receipts {
                if let Some(at) = state.hidden.iter().position(|(_, _, r)| r == receipt) {
                    let (id, body, _) = state.hidden.remove(at);
                    state.ready.push_front((id, body));
                }
            }
            Ok(())
        }
    }

    fn put(key: &str, time: &str) -> String {
        format!(
            r#"{{"Records":[{{"eventName":"ObjectCreated:Put","eventTime":"{time}","s3":{{"bucket":{{"name":"books"}},"object":{{"key":"library/{key}","size":1,"eTag":"e"}}}}}}]}}"#
        )
    }

    fn feed(queue: &Arc<FakeQueue>, dir: &Path) -> QueueFeed {
        QueueFeed::open(
            Box::new(Arc::clone(queue)),
            "queue-a",
            "books",
            "library/",
            dir,
        )
        .expect("open")
    }

    fn keys(page: &ChangeFeedPage) -> Vec<&str> {
        page.changes.iter().map(|c| c.key.as_str()).collect()
    }

    #[test]
    fn queue_urls_are_checked() {
        let url = QueueUrl::parse("https://sqs.eu-west-1.amazonaws.com/123456789012/books-events")
            .expect("valid");
        assert_eq!(url.region(), "eu-west-1");
        assert_eq!(
            url.as_str(),
            "https://sqs.eu-west-1.amazonaws.com/123456789012/books-events"
        );
        for ok in [
            "https://sqs.cn-north-1.amazonaws.com.cn/123456789012/q.fifo",
            "https://us-east-1.queue.amazonaws.com/123456789012/q_1",
        ] {
            assert!(QueueUrl::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://sqs.us-east-1.amazonaws.com/123456789012/q",
            "https://sqs.us-east-1.amazonaws.com",
            "https://sqs.us-east-1.amazonaws.com/123456789012",
            "https://sqs.us-east-1.amazonaws.com/12345/q",
            "https://sqs.us-east-1.amazonaws.com/123456789012/",
            "https://sqs.us-east-1.amazonaws.com/123456789012/a/b",
            "https://sqs..amazonaws.com/123456789012/q",
            "https://sqs.US-EAST-1.amazonaws.com/123456789012/q",
            "https://evil.example/123456789012/q",
            "https://sqs.us-east-1.amazonaws.com.evil.example/123456789012/q",
        ] {
            assert!(QueueUrl::parse(bad).is_err(), "{bad}");
        }
        let long = format!(
            "https://sqs.us-east-1.amazonaws.com/123456789012/{}",
            "a".repeat(81)
        );
        assert!(QueueUrl::parse(&long).is_err());
        // The 80-character limit counts the `.fifo` suffix.
        let base = "https://sqs.us-east-1.amazonaws.com/123456789012/";
        let fits = format!("{base}{}.fifo", "a".repeat(75));
        assert!(QueueUrl::parse(&fits).is_ok());
        let over = format!("{base}{}.fifo", "a".repeat(76));
        assert!(QueueUrl::parse(&over).is_err());
    }

    #[tokio::test]
    async fn pages_are_deleted_only_after_the_next_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        queue.send(put("b", "2026-11-18T08:00:02.000Z"));
        queue.send(put("a", "2026-11-18T08:00:01.000Z"));
        let feed = feed(&queue, dir.path());

        let start = feed.changes(None, 0).await.expect("start");
        assert!(
            start.changes.is_empty(),
            "the first call is only a position"
        );
        assert!(!start.complete, "the caller lists after the first call");
        assert!(!start.live_watch);
        assert_eq!(queue.waiting(), 2, "nothing is received yet");

        let first = feed.changes(Some(&start.cursor), 0).await.expect("first");
        assert_eq!(keys(&first), ["a", "b"], "oldest first");
        assert!(first.complete);
        assert!(!first.more);
        assert!(!first.live_watch);
        assert_eq!(first.changes[0].kind, ObjectChangeKind::Upserted);
        assert_eq!((queue.hidden(), queue.deleted()), (2, 0));

        let second = feed.changes(Some(&first.cursor), 0).await.expect("second");
        assert!(second.changes.is_empty());
        assert!(second.complete);
        assert_eq!((queue.hidden(), queue.deleted()), (0, 2));
    }

    #[tokio::test]
    async fn the_state_survives_the_guest_sleeping() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        queue.send(put("a", "t"));
        let asleep = feed(&queue, dir.path());
        let start = asleep.changes(None, 0).await.expect("start").cursor;
        let cursor = asleep.changes(Some(&start), 0).await.expect("first").cursor;
        drop(asleep);
        let woken = feed(&queue, dir.path());
        let page = woken.changes(Some(&cursor), 0).await.expect("after sleep");
        assert!(page.complete);
        assert_eq!(queue.deleted(), 1);
    }

    #[tokio::test]
    async fn a_lost_state_file_is_a_foreign_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        queue.send(put("a", "t"));
        let lost = feed(&queue, dir.path());
        let start = lost.changes(None, 0).await.expect("start").cursor;
        let cursor = lost.changes(Some(&start), 0).await.expect("first").cursor;
        drop(lost);
        for entry in std::fs::read_dir(dir.path()).expect("dir") {
            std::fs::remove_file(entry.expect("entry").path()).expect("remove");
        }
        let page = feed(&queue, dir.path())
            .changes(Some(&cursor), 0)
            .await
            .expect("after loss");
        assert!(!page.complete);
        assert!(page.changes.is_empty());
        assert_eq!(queue.deleted(), 0, "nothing is deleted without the handles");
    }

    #[tokio::test]
    async fn a_retried_cursor_releases_the_pages_after_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        let feed = feed(&queue, dir.path());
        let start = feed.changes(None, 0).await.expect("start");
        queue.send(put("a", "t1"));
        let first = feed.changes(Some(&start.cursor), 0).await.expect("first");
        assert_eq!(keys(&first), ["a"]);
        queue.send(put("b", "t2"));
        let second = feed.changes(Some(&first.cursor), 0).await.expect("second");
        assert_eq!(keys(&second), ["b"]);
        // The host lost the second page and asks again from the first.
        let retry = feed.changes(Some(&first.cursor), 0).await.expect("retry");
        assert_eq!(keys(&retry), ["b"]);
        assert!(retry.complete);
        let _ = feed.changes(Some(&retry.cursor), 0).await.expect("settle");
        assert_eq!(
            (queue.waiting(), queue.hidden(), queue.deleted()),
            (0, 0, 2)
        );
    }

    #[tokio::test]
    async fn pages_respect_the_limit_and_say_more() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        for i in 0..25 {
            queue.send(put(&format!("k{i:02}"), &format!("t{i:02}")));
        }
        let feed = feed(&queue, dir.path());
        let start = feed.changes(None, 12).await.expect("start");
        let page = feed.changes(Some(&start.cursor), 12).await.expect("page");
        assert_eq!(page.changes.len(), 12);
        assert!(page.more);
        let next = feed.changes(Some(&page.cursor), 12).await.expect("next");
        assert_eq!(next.changes.len(), 12);
        let last = feed.changes(Some(&next.cursor), 12).await.expect("last");
        assert_eq!(last.changes.len(), 1);
        assert!(!last.more);
    }

    #[tokio::test]
    async fn a_message_that_does_not_fit_is_released_for_the_next_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        queue.send(put("a", "t1"));
        let two = r#"{"Records":[{"eventName":"ObjectCreated:Put","s3":{"bucket":{"name":"books"},"object":{"key":"library/b"}}},{"eventName":"ObjectCreated:Put","s3":{"bucket":{"name":"books"},"object":{"key":"library/c"}}}]}"#;
        queue.send(two.to_string());
        let feed = feed(&queue, dir.path());
        let start = feed.changes(None, 2).await.expect("start");
        let page = feed.changes(Some(&start.cursor), 2).await.expect("page");
        assert_eq!(keys(&page), ["a"]);
        assert!(page.more);
        assert_eq!(queue.waiting(), 1, "the two-change message went back");
        let next = feed.changes(Some(&page.cursor), 2).await.expect("next");
        assert_eq!(keys(&next), ["b", "c"]);
        assert!(next.complete);

        // A message larger than a whole page is cut and marked incomplete.
        queue.send(two.to_string());
        let cut = feed.changes(Some(&next.cursor), 1).await.expect("cut");
        assert_eq!(keys(&cut), ["b"]);
        assert!(!cut.complete);
    }

    #[tokio::test]
    async fn skipped_messages_count_against_the_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        let feed = feed(&queue, dir.path());
        let start = feed.changes(None, 5).await.expect("start");
        for i in 0..30 {
            queue.send(put(&format!("k{i}"), "t").replace("\"books\"", "\"other\""));
        }
        let page = feed.changes(Some(&start.cursor), 5).await.expect("page");
        assert!(page.changes.is_empty());
        assert!(page.more, "the backlog is drained over several calls");
        assert_eq!(queue.hidden(), 5);
        assert_eq!(queue.waiting(), 25);
    }

    #[tokio::test]
    async fn unreadable_messages_are_dropped_and_mark_the_page_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        let feed = feed(&queue, dir.path());
        let first = feed.changes(None, 0).await.expect("first");
        queue.send("garbage".into());
        queue.send(put("a", "t"));
        let page = feed.changes(Some(&first.cursor), 0).await.expect("page");
        assert_eq!(keys(&page), ["a"]);
        assert!(!page.complete);
        let _ = feed.changes(Some(&page.cursor), 0).await.expect("settle");
        assert_eq!(queue.deleted(), 2, "the unreadable message is deleted too");
    }

    #[tokio::test]
    async fn bad_cursors_and_receive_errors_are_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        let feed = feed(&queue, dir.path());
        for bad in ["w1.x.1", "q1..1", "q1.x", "q1.x.y"] {
            assert!(
                matches!(
                    feed.changes(Some(bad), 0).await,
                    Err(StorageError::InvalidCursor(_))
                ),
                "{bad}"
            );
        }
        let page = feed.changes(None, 0).await.expect("first");
        let (id, seq) = parse_cursor(&page.cursor).expect("cursor");
        let ahead = format!("q1.{id}.{}", seq + 1);
        assert!(matches!(
            feed.changes(Some(&ahead), 0).await,
            Err(StorageError::InvalidCursor(_))
        ));
        queue.send(put("a", "t"));
        queue.state.lock().unwrap().fail_receive = true;
        assert!(feed.changes(Some(&page.cursor), 0).await.is_err());
        queue.state.lock().unwrap().fail_receive = false;
        let again = feed.changes(Some(&page.cursor), 0).await.expect("again");
        assert_eq!(keys(&again), ["a"]);
        assert!(
            !again.complete,
            "the failed receive may have hidden messages"
        );
    }

    #[test]
    fn sequencers_compare_padded() {
        use std::cmp::Ordering;
        assert_eq!(sequencer_cmp("0A", "000A"), Ordering::Equal);
        assert_eq!(sequencer_cmp("FF", "100"), Ordering::Less);
        assert_eq!(sequencer_cmp("0B", "0A"), Ordering::Greater);
        assert_eq!(sequencer_cmp("0a", "0B"), Ordering::Less);
        let event = |key: &str, seq: Option<&str>| ParsedEvent {
            change: ObjectChange {
                key: key.into(),
                kind: ObjectChangeKind::Changed,
                info: None,
            },
            time: Some("t".into()),
            sequencer: seq.map(str::to_string),
        };
        let mut events = vec![
            event("a", Some("02")),
            event("b", Some("01")),
            event("a", Some("01")),
        ];
        order_events(&mut events);
        let order: Vec<_> = events
            .iter()
            .map(|e| (e.change.key.as_str(), e.sequencer.as_deref()))
            .collect();
        assert_eq!(
            order,
            [("a", Some("01")), ("b", Some("01")), ("a", Some("02"))]
        );
        // An event without a sequencer keeps its place; the others around
        // it still sort among themselves.
        let mut events = vec![
            event("a", Some("03")),
            event("a", None),
            event("a", Some("01")),
        ];
        order_events(&mut events);
        let order: Vec<_> = events.iter().map(|e| e.sequencer.as_deref()).collect();
        assert_eq!(order, [Some("01"), None, Some("03")]);
    }

    #[tokio::test]
    async fn an_interrupted_receive_makes_the_next_page_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        let start = feed(&queue, dir.path())
            .changes(None, 0)
            .await
            .expect("start")
            .cursor;
        // A guest stopped in the middle of a receive leaves the flag set.
        let path = std::fs::read_dir(dir.path())
            .expect("dir")
            .next()
            .expect("state file")
            .expect("entry")
            .path();
        let mut state: FeedState =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("state");
        state.receiving = true;
        std::fs::write(&path, serde_json::to_vec(&state).expect("json")).expect("write");
        let resumed = feed(&queue, dir.path());
        queue.send(put("a", "t"));
        let page = resumed.changes(Some(&start), 0).await.expect("page");
        assert_eq!(keys(&page), ["a"]);
        assert!(!page.complete);
        let next = resumed.changes(Some(&page.cursor), 0).await.expect("next");
        assert!(next.complete, "the flag clears once a page is handed out");
    }

    #[tokio::test]
    async fn a_failed_release_makes_the_page_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        let feed = feed(&queue, dir.path());
        let start = feed.changes(None, 0).await.expect("start");
        queue.send(put("a", "t"));
        let first = feed.changes(Some(&start.cursor), 0).await.expect("first");
        assert_eq!(keys(&first), ["a"]);
        queue.state.lock().unwrap().fail_release = true;
        // Retrying the start cursor releases the page after it, which fails.
        let retried = feed.changes(Some(&start.cursor), 0).await.expect("retry");
        assert!(retried.changes.is_empty());
        assert!(!retried.complete);
    }

    #[tokio::test]
    async fn another_prefix_on_the_same_queue_is_a_foreign_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Arc::new(FakeQueue::default());
        let start = feed(&queue, dir.path())
            .changes(None, 0)
            .await
            .expect("start")
            .cursor;
        let other = QueueFeed::open(
            Box::new(Arc::clone(&queue)),
            "queue-a",
            "books",
            "other/",
            dir.path(),
        )
        .expect("open");
        let page = other.changes(Some(&start), 0).await.expect("page");
        assert!(!page.complete);
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 2);
    }
}
