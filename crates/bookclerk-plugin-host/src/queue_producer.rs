//! Host-served `[[queues.producers]]` bindings: guest sends land in the
//! library outbox under the queue the manifest row names.
//!
//! The guest never chooses the queue or its identity. A binding is granted
//! only when the manifest declares it and the operator grant carries the
//! matching `queue:<BINDING>` token; every row the [`OutboxQueueProducer`]
//! writes carries `source = plugin id`, the `Invocation.accountId` the host
//! opened the entrypoints with, the queue of the binding, and a fresh
//! deduplication key (queue sends never coalesce). Bodies are bounded at
//! [`MAX_EVENT_PAYLOAD_BYTES`]; `json` bodies must parse, `text` bodies must
//! be UTF-8, and `bytes` bodies are stored as base64 text.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use bookclerk_library::{
    PublishDomainEventOutcome, PublishDomainEventSpec, QUEUE_CONTENT_TYPE_BYTES,
    QUEUE_CONTENT_TYPE_JSON, QUEUE_CONTENT_TYPE_TEXT,
};
use bookclerk_plugin_abi::{
    Invocation, PluginError, QueueProducer, QueueSendMessage, QueueSendOk, MAX_EVENT_PAYLOAD_BYTES,
    MAX_LIST_PAGE,
};
use bookclerk_plugin_manifest::PluginManifest;

use crate::consent::PluginGrant;
use bookclerk_library::LibraryStore;

/// Plugin ABI result (the wire error the guest receives).
type Result<T> = std::result::Result<T, PluginError>;

/// Host-owned facts a session needs before it can hand a guest its queue
/// producer bindings: the outbox store plus the granted `(binding, queue)`
/// pairs.
#[derive(Clone)]
pub struct QueueOutbox {
    /// Library store whose outbox receives the rows.
    store: LibraryStore,
    /// Plugin id stamped as `source` on every row.
    plugin_id: String,
    /// Granted `[[queues.producers]]` rows as `(binding, queue)` in manifest order.
    producers: Vec<(String, String)>,
}

impl QueueOutbox {
    /// Builds the outbox hook for `plugin_id`, or `None` when no producer row
    /// is both declared and granted (the guest then sees no queue bindings).
    #[must_use]
    pub fn new(
        store: LibraryStore,
        plugin_id: &str,
        manifest: &PluginManifest,
        grant: &PluginGrant,
    ) -> Option<Self> {
        let producers: Vec<(String, String)> = manifest
            .queues
            .producers
            .iter()
            .filter(|row| grant.allows_queue_producer(&row.binding))
            .map(|row| (row.binding.clone(), row.queue.clone()))
            .collect();
        if producers.is_empty() {
            return None;
        }
        Some(Self {
            store,
            plugin_id: plugin_id.to_string(),
            producers,
        })
    }

    /// Granted binding names in manifest order.
    #[must_use]
    pub fn binding_names(&self) -> Vec<String> {
        self.producers.iter().map(|(b, _)| b.clone()).collect()
    }

    /// Producers bound to one `PluginWorker.open` invocation, shaped for
    /// `HostBindings::queues` as `(binding, queue, producer)`.
    #[must_use]
    pub fn producers(
        &self,
        invocation: &Invocation,
    ) -> Vec<(String, String, Arc<dyn QueueProducer>)> {
        self.producers
            .iter()
            .map(|(binding, queue)| {
                let producer: Arc<dyn QueueProducer> = Arc::new(OutboxQueueProducer {
                    store: self.store.clone(),
                    plugin_id: self.plugin_id.clone(),
                    queue: queue.clone(),
                    account_id: invocation.account_id.clone(),
                    correlation_id: invocation.correlation_id.clone(),
                    causation_id: invocation.causation_id.clone(),
                });
                (binding.clone(), queue.clone(), producer)
            })
            .collect()
    }
}

/// [`QueueProducer`] server for one binding of one invocation: validates a
/// guest [`QueueSendMessage`] and appends it to the library outbox.
pub struct OutboxQueueProducer {
    /// Library store whose outbox receives the rows.
    store: LibraryStore,
    /// Plugin id stamped as `source` on every row.
    plugin_id: String,
    /// Queue every row of this binding lands in.
    queue: String,
    /// `Invocation.accountId` stamped on every row.
    account_id: String,
    /// `correlationId` copied onto every row.
    correlation_id: String,
    /// `causationId` copied onto every row.
    causation_id: String,
}

impl OutboxQueueProducer {
    /// Queue this producer sends to.
    #[must_use]
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// Validates `message` and builds the outbox row the host will insert.
    /// `default_delay` applies when the message carries no delay of its own.
    ///
    /// # Errors
    ///
    /// * `payload_too_large` when `body` exceeds [`MAX_EVENT_PAYLOAD_BYTES`].
    /// * `invalid_params` when `contentType` is unknown, a `json` body is not
    ///   UTF-8 JSON, or a `text` body is not UTF-8.
    pub fn spec(
        &self,
        message: QueueSendMessage,
        default_delay: u32,
    ) -> Result<PublishDomainEventSpec> {
        if message.body.len() > MAX_EVENT_PAYLOAD_BYTES as usize {
            return Err(PluginError::payload_too_large(format!(
                "queue message body of {} bytes exceeds {MAX_EVENT_PAYLOAD_BYTES}",
                message.body.len()
            )));
        }
        let content_type = if message.content_type.trim().is_empty() {
            QUEUE_CONTENT_TYPE_JSON
        } else {
            message.content_type.trim()
        };
        let payload = match content_type {
            QUEUE_CONTENT_TYPE_JSON => {
                if message.body.is_empty() {
                    "null".to_string()
                } else {
                    let text = String::from_utf8(message.body).map_err(|_| {
                        PluginError::invalid_params("json queue message body must be UTF-8 JSON")
                    })?;
                    serde_json::from_str::<serde::de::IgnoredAny>(&text).map_err(|err| {
                        PluginError::invalid_params(format!(
                            "json queue message body must be JSON: {err}"
                        ))
                    })?;
                    text
                }
            }
            QUEUE_CONTENT_TYPE_TEXT => String::from_utf8(message.body).map_err(|_| {
                PluginError::invalid_params("text queue message body must be UTF-8")
            })?,
            QUEUE_CONTENT_TYPE_BYTES => {
                base64::engine::general_purpose::STANDARD.encode(message.body)
            }
            other => {
                return Err(PluginError::invalid_params(format!(
                    "unknown queue message content type `{other}`; expected json, text, or bytes"
                )))
            }
        };
        let delay_seconds = if message.delay_seconds > 0 {
            message.delay_seconds
        } else {
            default_delay
        };
        Ok(PublishDomainEventSpec {
            id: String::new(),
            event_type: String::new(),
            schema_version: 1,
            account_id: self.account_id.clone(),
            source: self.plugin_id.clone(),
            correlation_id: self.correlation_id.clone(),
            causation_id: self.causation_id.clone(),
            // Queue sends never coalesce: every send is its own message.
            dedup_key: uuid::Uuid::new_v4().to_string(),
            payload,
            ordering_key: String::new(),
            queue_name: self.queue.clone(),
            content_type: content_type.to_string(),
            delay_seconds,
        })
    }

    /// Inserts one validated row and returns the outbox id the host assigned.
    async fn publish_one(&self, spec: PublishDomainEventSpec) -> Result<String> {
        match self.store.publish_domain_event(spec).await {
            Ok(PublishDomainEventOutcome::Created { id }) => Ok(id),
            Ok(PublishDomainEventOutcome::Duplicate { existing_id }) => Ok(existing_id),
            Err(err) => Err(PluginError::unavailable(format!(
                "queue send failed: {err}"
            ))),
        }
    }
}

#[async_trait(?Send)]
impl QueueProducer for OutboxQueueProducer {
    async fn send(&self, message: QueueSendMessage) -> Result<QueueSendOk> {
        let spec = self.spec(message, 0)?;
        let id = self.publish_one(spec).await?;
        Ok(QueueSendOk {
            message_ids: vec![id],
        })
    }

    async fn send_batch(
        &self,
        messages: Vec<QueueSendMessage>,
        delay_seconds: u32,
    ) -> Result<QueueSendOk> {
        if messages.is_empty() {
            return Err(PluginError::invalid_params(
                "sendBatch requires at least one message",
            ));
        }
        if messages.len() > MAX_LIST_PAGE as usize {
            return Err(PluginError::payload_too_large(format!(
                "queue batch of {} exceeds {MAX_LIST_PAGE}",
                messages.len()
            )));
        }
        // Validate every message before the first write so a malformed tail
        // never leaves a partial batch behind.
        let specs = messages
            .into_iter()
            .map(|message| self.spec(message, delay_seconds))
            .collect::<Result<Vec<_>>>()?;
        let mut message_ids = Vec::with_capacity(specs.len());
        for spec in specs {
            message_ids.push(self.publish_one(spec).await?);
        }
        Ok(QueueSendOk { message_ids })
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::PluginErrorCode;

    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        )
    }

    fn manifest() -> PluginManifest {
        PluginManifest::parse(
            r#"
api_version = 3
id = "demo"
runtime = "native"
command = "./demo"
entrypoints = ["cli"]

[capabilities.network]
mode = "deny"

[[queues.producers]]
queue = "notify"
binding = "NOTIFY"

[[queues.producers]]
queue = "audit"
binding = "AUDIT"
"#,
        )
        .expect("manifest")
    }

    fn grant(bindings: &[&str]) -> PluginGrant {
        PluginGrant {
            bindings: bindings.iter().map(|s| (*s).to_string()).collect(),
            ..PluginGrant::empty()
        }
    }

    fn invocation(account: &str) -> Invocation {
        Invocation {
            id: "inv-1".into(),
            account_id: account.into(),
            correlation_id: "corr-inv".into(),
            causation_id: "cause-inv".into(),
            ..Invocation::default()
        }
    }

    #[tokio::test]
    async fn outbox_follows_manifest_and_grant() {
        let store = memory_store().await;
        assert!(QueueOutbox::new(store.clone(), "demo", &manifest(), &grant(&[])).is_none());
        assert!(
            QueueOutbox::new(store.clone(), "demo", &manifest(), &grant(&["queue:OTHER"]))
                .is_none()
        );
        let outbox = QueueOutbox::new(
            store,
            "demo",
            &manifest(),
            &grant(&["queue:AUDIT", "queue_consumer:notify"]),
        )
        .expect("granted producer");
        assert_eq!(outbox.binding_names(), vec!["AUDIT".to_string()]);
        let producers = outbox.producers(&invocation("acct"));
        assert_eq!(producers.len(), 1);
        assert_eq!(producers[0].0, "AUDIT");
        assert_eq!(producers[0].1, "audit");
    }

    #[tokio::test]
    async fn spec_encodes_each_content_type_and_rejects_bad_bodies() {
        let store = memory_store().await;
        let outbox = QueueOutbox::new(store, "demo", &manifest(), &grant(&["queue:NOTIFY"]))
            .expect("outbox");
        let producer = OutboxQueueProducer {
            store: outbox.store.clone(),
            plugin_id: "demo".into(),
            queue: "notify".into(),
            account_id: "acct".into(),
            correlation_id: "corr".into(),
            causation_id: "cause".into(),
        };
        let json = producer
            .spec(
                QueueSendMessage {
                    body: br#"{"n":1}"#.to_vec(),
                    content_type: String::new(),
                    delay_seconds: 0,
                },
                4,
            )
            .expect("json");
        assert_eq!(json.queue_name, "notify");
        assert_eq!(json.content_type, "json");
        assert_eq!(json.payload, r#"{"n":1}"#);
        assert_eq!(json.delay_seconds, 4, "batch delay applies to zero delays");
        assert_eq!(json.source, "demo");
        assert_eq!(json.account_id, "acct");
        assert!(json.event_type.is_empty());
        assert!(!json.dedup_key.is_empty());

        let empty = producer
            .spec(QueueSendMessage::default(), 0)
            .expect("empty json");
        assert_eq!(empty.payload, "null");

        let text = producer
            .spec(
                QueueSendMessage {
                    body: b"hello".to_vec(),
                    content_type: "text".into(),
                    delay_seconds: 9,
                },
                4,
            )
            .expect("text");
        assert_eq!(text.payload, "hello");
        assert_eq!(text.delay_seconds, 9, "own delay wins");

        let bytes = producer
            .spec(
                QueueSendMessage {
                    body: vec![0xff, 0x00],
                    content_type: "bytes".into(),
                    delay_seconds: 0,
                },
                0,
            )
            .expect("bytes");
        assert_eq!(bytes.content_type, "bytes");
        assert_eq!(bytes.payload, "/wA=");

        let code =
            |message: QueueSendMessage| producer.spec(message, 0).expect_err("rejected").code;
        assert_eq!(
            code(QueueSendMessage {
                body: b"not json".to_vec(),
                ..QueueSendMessage::default()
            }),
            PluginErrorCode::InvalidParams
        );
        assert_eq!(
            code(QueueSendMessage {
                body: vec![0xff],
                content_type: "text".into(),
                ..QueueSendMessage::default()
            }),
            PluginErrorCode::InvalidParams
        );
        assert_eq!(
            code(QueueSendMessage {
                body: Vec::new(),
                content_type: "xml".into(),
                ..QueueSendMessage::default()
            }),
            PluginErrorCode::InvalidParams
        );
        assert_eq!(
            code(QueueSendMessage {
                body: vec![b'1'; MAX_EVENT_PAYLOAD_BYTES as usize + 1],
                ..QueueSendMessage::default()
            }),
            PluginErrorCode::PayloadTooLarge
        );
    }

    #[tokio::test]
    async fn send_and_send_batch_write_queue_rows() {
        let store = memory_store().await;
        let outbox = QueueOutbox::new(
            store.clone(),
            "demo",
            &manifest(),
            &grant(&["queue:NOTIFY"]),
        )
        .expect("outbox");
        let producers = outbox.producers(&invocation("acct"));
        let producer = Arc::clone(&producers[0].2);
        let one = producer
            .send(QueueSendMessage {
                body: br#"{"n":1}"#.to_vec(),
                ..QueueSendMessage::default()
            })
            .await
            .expect("send");
        assert_eq!(one.message_ids.len(), 1);
        let same = producer
            .send(QueueSendMessage {
                body: br#"{"n":1}"#.to_vec(),
                ..QueueSendMessage::default()
            })
            .await
            .expect("send");
        assert_ne!(same.message_ids, one.message_ids, "sends never coalesce");

        let batch = producer
            .send_batch(
                vec![
                    QueueSendMessage {
                        body: b"a".to_vec(),
                        content_type: "text".into(),
                        delay_seconds: 0,
                    },
                    QueueSendMessage {
                        body: b"b".to_vec(),
                        content_type: "text".into(),
                        delay_seconds: 0,
                    },
                ],
                2,
            )
            .await
            .expect("batch");
        assert_eq!(batch.message_ids.len(), 2);
        let rows = store
            .list_domain_events_for_queue("notify", 10)
            .await
            .expect("rows");
        assert_eq!(rows.len(), 4);
        assert!(rows
            .iter()
            .all(|r| r.queue_name == "notify" && r.source == "demo"));

        let err = producer
            .send_batch(Vec::new(), 0)
            .await
            .expect_err("empty batch");
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        let too_many = (0..=MAX_LIST_PAGE)
            .map(|_| QueueSendMessage::default())
            .collect();
        let err = producer
            .send_batch(too_many, 0)
            .await
            .expect_err("oversize batch");
        assert_eq!(err.code, PluginErrorCode::PayloadTooLarge);
        let err = producer
            .send_batch(
                vec![
                    QueueSendMessage::default(),
                    QueueSendMessage {
                        body: b"nope".to_vec(),
                        ..QueueSendMessage::default()
                    },
                ],
                0,
            )
            .await
            .expect_err("malformed tail");
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
        assert_eq!(
            store
                .list_domain_events_for_queue("notify", 10)
                .await
                .expect("rows")
                .len(),
            4,
            "a malformed batch writes nothing"
        );
    }
}
