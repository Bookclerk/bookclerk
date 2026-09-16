//! Host-served `EVENTS` binding: guest publishes land in the library outbox.
//!
//! The guest never chooses its identity. Every row the [`OutboxEventPublisher`]
//! writes carries `source = plugin id` and the `Invocation.accountId` the host
//! opened the entrypoints with; the event type must be one of the consented
//! `[[events.producers]]` entries, and the payload must be JSON no larger than
//! [`MAX_EVENT_PAYLOAD_BYTES`]. Duplicate publishes (same source, type, and
//! deduplication key) coalesce onto the earlier outbox row.

use std::collections::BTreeSet;

use async_trait::async_trait;
use bookclerk_library::{LibraryStore, PublishDomainEventOutcome, PublishDomainEventSpec};
use bookclerk_plugin_abi::{
    EventPublisher, Invocation, PluginError, PluginEvent, PublishOk, MAX_EVENT_PAYLOAD_BYTES,
};

/// Plugin ABI result (the wire error the guest receives).
type Result<T> = std::result::Result<T, PluginError>;

/// Host-owned facts a session needs before it can hand a guest an `EVENTS`
/// binding: the outbox store plus the producer allowlist.
#[derive(Clone)]
pub struct EventOutbox {
    /// Library store whose outbox receives the rows.
    store: LibraryStore,
    /// Plugin id stamped as `source` on every row.
    plugin_id: String,
    /// Consented `[[events.producers]]` event types (manifest ∩ grant).
    producers: BTreeSet<String>,
}

impl EventOutbox {
    /// Builds the outbox hook for `plugin_id`, or `None` when no producer is
    /// both declared and granted (the guest then sees no `EVENTS` binding).
    #[must_use]
    pub fn new(
        store: LibraryStore,
        plugin_id: &str,
        declared: impl IntoIterator<Item = String>,
        granted: &BTreeSet<String>,
    ) -> Option<Self> {
        let producers: BTreeSet<String> = declared
            .into_iter()
            .filter(|event_type| granted.contains(event_type))
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

    /// Publisher bound to one `PluginWorker.open` invocation (account scope
    /// and default correlation / causation come from `invocation`).
    #[must_use]
    pub fn publisher(&self, invocation: &Invocation) -> OutboxEventPublisher {
        OutboxEventPublisher {
            store: self.store.clone(),
            plugin_id: self.plugin_id.clone(),
            producers: self.producers.clone(),
            account_id: invocation.account_id.clone(),
            correlation_id: invocation.correlation_id.clone(),
            causation_id: invocation.causation_id.clone(),
        }
    }
}

/// [`EventPublisher`] server for one invocation: validates a guest
/// [`PluginEvent`] and appends it to the library outbox.
pub struct OutboxEventPublisher {
    /// Library store whose outbox receives the rows.
    store: LibraryStore,
    /// Plugin id stamped as `source` on every row.
    plugin_id: String,
    /// Consented `[[events.producers]]` event types.
    producers: BTreeSet<String>,
    /// `Invocation.accountId` stamped on every row.
    account_id: String,
    /// Default `correlationId` when the guest leaves it empty.
    correlation_id: String,
    /// Default `causationId` when the guest leaves it empty.
    causation_id: String,
}

impl OutboxEventPublisher {
    /// Validates `event` and builds the outbox row the host will insert.
    ///
    /// # Errors
    ///
    /// * `forbidden` when `eventType` is not a consented producer.
    /// * `payload_too_large` when `payload` exceeds [`MAX_EVENT_PAYLOAD_BYTES`].
    /// * `invalid_params` when `payload` is not UTF-8 JSON.
    pub fn spec(&self, event: PluginEvent) -> Result<PublishDomainEventSpec> {
        let event_type = event.event_type.trim();
        if event_type.is_empty() || event.event_type != event_type {
            return Err(PluginError::invalid_params(
                "eventType is required and must not have surrounding whitespace",
            ));
        }
        if !self.producers.contains(event_type) {
            return Err(PluginError::forbidden(format!(
                "plugin `{}` is not granted to publish `{event_type}`; declare it under \
                 [[events.producers]] and re-approve",
                self.plugin_id
            )));
        }
        if event.payload.len() > MAX_EVENT_PAYLOAD_BYTES as usize {
            return Err(PluginError::payload_too_large(format!(
                "event payload of {} bytes exceeds {MAX_EVENT_PAYLOAD_BYTES}",
                event.payload.len()
            )));
        }
        let payload = if event.payload.is_empty() {
            "{}".to_string()
        } else {
            let text = String::from_utf8(event.payload)
                .map_err(|_| PluginError::invalid_params("event payload must be UTF-8 JSON"))?;
            serde_json::from_str::<serde::de::IgnoredAny>(&text).map_err(|err| {
                PluginError::invalid_params(format!("event payload must be JSON: {err}"))
            })?;
            text
        };
        let dedup_key = if event.deduplication_key.trim().is_empty() {
            // The outbox requires a key; a fresh one publishes unconditionally.
            uuid::Uuid::new_v4().to_string()
        } else {
            event.deduplication_key
        };
        let inherit = |own: String, default: &str| {
            if own.trim().is_empty() {
                default.to_string()
            } else {
                own
            }
        };
        Ok(PublishDomainEventSpec {
            id: String::new(),
            event_type: event_type.to_string(),
            schema_version: i64::from(event.schema_version.max(1)),
            account_id: self.account_id.clone(),
            source: self.plugin_id.clone(),
            correlation_id: inherit(event.correlation_id, &self.correlation_id),
            causation_id: inherit(event.causation_id, &self.causation_id),
            dedup_key,
            payload,
            ordering_key: String::new(),
        })
    }
}

#[async_trait(?Send)]
impl EventPublisher for OutboxEventPublisher {
    async fn publish(&self, event: PluginEvent) -> Result<PublishOk> {
        let spec = self.spec(event)?;
        match self.store.publish_domain_event(spec).await {
            Ok(PublishDomainEventOutcome::Created { id }) => Ok(PublishOk {
                event_id: id,
                duplicate: false,
            }),
            Ok(PublishDomainEventOutcome::Duplicate { existing_id }) => Ok(PublishOk {
                event_id: existing_id,
                duplicate: true,
            }),
            Err(err) => Err(PluginError::unavailable(format!(
                "outbox publish failed: {err}"
            ))),
        }
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

    fn granted(types: &[&str]) -> BTreeSet<String> {
        types.iter().map(|t| (*t).to_string()).collect()
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

    async fn publisher(declared: &[&str], grant: &[&str]) -> Option<OutboxEventPublisher> {
        let store = memory_store().await;
        EventOutbox::new(
            store,
            "demo",
            declared.iter().map(|t| (*t).to_string()),
            &granted(grant),
        )
        .map(|outbox| outbox.publisher(&invocation("acct")))
    }

    #[tokio::test]
    async fn outbox_is_absent_without_a_declared_and_granted_producer() {
        assert!(publisher(&[], &["demo_pinged"]).await.is_none());
        assert!(publisher(&["demo_pinged"], &[]).await.is_none());
        assert!(publisher(&["demo_pinged"], &["other"]).await.is_none());
        assert!(publisher(&["demo_pinged"], &["demo_pinged"])
            .await
            .is_some());
    }

    #[tokio::test]
    async fn spec_forces_source_account_and_inherits_trace_ids() {
        let publisher = publisher(&["demo_pinged"], &["demo_pinged"])
            .await
            .expect("publisher");
        let spec = publisher
            .spec(PluginEvent {
                event_type: "demo_pinged".into(),
                deduplication_key: "k1".into(),
                payload: br#"{"n":1}"#.to_vec(),
                ..PluginEvent::default()
            })
            .expect("spec");
        assert_eq!(spec.source, "demo");
        assert_eq!(spec.account_id, "acct");
        assert_eq!(spec.schema_version, 1);
        assert_eq!(spec.correlation_id, "corr-inv");
        assert_eq!(spec.causation_id, "cause-inv");
        assert_eq!(spec.dedup_key, "k1");
        assert_eq!(spec.payload, r#"{"n":1}"#);

        let explicit = publisher
            .spec(PluginEvent {
                event_type: "demo_pinged".into(),
                schema_version: 3,
                correlation_id: "corr-own".into(),
                causation_id: "cause-own".into(),
                ..PluginEvent::default()
            })
            .expect("spec");
        assert_eq!(explicit.schema_version, 3);
        assert_eq!(explicit.correlation_id, "corr-own");
        assert_eq!(explicit.causation_id, "cause-own");
        assert_eq!(explicit.payload, "{}");
        assert!(!explicit.dedup_key.is_empty(), "empty key mints one");
    }

    #[tokio::test]
    async fn spec_rejects_ungranted_types_oversize_and_non_json_payloads() {
        let publisher = publisher(&["demo_pinged", "demo_other"], &["demo_pinged"])
            .await
            .expect("publisher");
        let code = |event: PluginEvent| publisher.spec(event).expect_err("rejected").code;
        assert_eq!(
            code(PluginEvent {
                event_type: "demo_other".into(),
                ..PluginEvent::default()
            }),
            PluginErrorCode::Forbidden
        );
        assert_eq!(
            code(PluginEvent {
                event_type: "book_acquired".into(),
                ..PluginEvent::default()
            }),
            PluginErrorCode::Forbidden
        );
        assert_eq!(
            code(PluginEvent {
                event_type: " demo_pinged".into(),
                ..PluginEvent::default()
            }),
            PluginErrorCode::InvalidParams
        );
        assert_eq!(
            code(PluginEvent {
                event_type: "demo_pinged".into(),
                payload: vec![b'{'; MAX_EVENT_PAYLOAD_BYTES as usize + 1],
                ..PluginEvent::default()
            }),
            PluginErrorCode::PayloadTooLarge
        );
        assert_eq!(
            code(PluginEvent {
                event_type: "demo_pinged".into(),
                payload: b"not json".to_vec(),
                ..PluginEvent::default()
            }),
            PluginErrorCode::InvalidParams
        );
        assert_eq!(
            code(PluginEvent {
                event_type: "demo_pinged".into(),
                payload: vec![0xff, 0xfe],
                ..PluginEvent::default()
            }),
            PluginErrorCode::InvalidParams
        );
    }

    #[tokio::test]
    async fn publish_writes_the_outbox_and_coalesces_duplicates() {
        let store = memory_store().await;
        let outbox = EventOutbox::new(
            store.clone(),
            "demo",
            vec!["demo_pinged".to_string()],
            &granted(&["demo_pinged"]),
        )
        .expect("outbox");
        let publisher = outbox.publisher(&invocation("acct"));
        let event = PluginEvent {
            event_type: "demo_pinged".into(),
            deduplication_key: "ping-1".into(),
            payload: br#"{"n":1}"#.to_vec(),
            ..PluginEvent::default()
        };
        let first = publisher.publish(event.clone()).await.expect("publish");
        assert!(!first.duplicate);
        let again = publisher.publish(event).await.expect("publish");
        assert!(again.duplicate);
        assert_eq!(again.event_id, first.event_id);

        let row = store
            .get_domain_event(&first.event_id)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(row.event_type, "demo_pinged");
        assert_eq!(row.source, "demo");
        assert_eq!(row.account_id, "acct");
        assert_eq!(row.payload, r#"{"n":1}"#);

        let fresh = publisher
            .publish(PluginEvent {
                event_type: "demo_pinged".into(),
                payload: br#"{"n":2}"#.to_vec(),
                ..PluginEvent::default()
            })
            .await
            .expect("publish");
        assert!(
            !fresh.duplicate,
            "empty dedup key publishes unconditionally"
        );
        assert_ne!(fresh.event_id, first.event_id);
        assert_eq!(store.list_domain_events(10).await.expect("list").len(), 2);
    }
}
