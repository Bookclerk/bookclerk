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
use bookclerk_library::{
    DomainEventRecord, EventCatalogSubscription, EventStreamRead, EventStreamStart, LibraryStore,
    PublishDomainEventOutcome, PublishDomainEventSpec,
};
use bookclerk_plugin_abi::{
    validate_event_read_request, DomainEvent, EventCursor, EventPublisher, EventReadPage,
    EventReadRequest, EventReadStart, Invocation, PluginError, PluginEvent, PublishOk,
    MAX_EVENT_PAYLOAD_BYTES,
};

use crate::consent::{canonical_event_filter, GrantedEventConsumer};
use crate::PluginManifest;

/// Plugin ABI result (the wire error the guest receives).
type Result<T> = std::result::Result<T, PluginError>;

/// Catalog rows for a manifest's `[[events.consumers]]` declarations, before
/// consent narrows them.
#[must_use]
pub fn manifest_event_subscriptions(manifest: &PluginManifest) -> Vec<EventCatalogSubscription> {
    manifest
        .events
        .consumers
        .iter()
        .map(|s| EventCatalogSubscription {
            event_type: s.event_type.clone(),
            schema_versions: s.schema_versions.clone(),
            supports_suspend: s.supports_suspend,
            resource_class: if s.resource_class.trim().is_empty() {
                "network".into()
            } else {
                s.resource_class.clone()
            },
            filter: s.filter.clone().filter(|v| !v.is_null()),
            queue: String::new(),
            max_retries: s.max_retries,
            dead_letter_queue: None,
            retry_delay_secs: 0,
            message_retention_secs: s.message_retention_period,
        })
        .collect()
}

/// Keeps the catalog rows an operator grant admits.
///
/// A row is admitted when a granted consumer has its type, covers all its
/// schema versions, and carries the same canonical filter; suspend support
/// is kept only when both sides allow it.
#[must_use]
pub fn granted_event_subscriptions(
    catalog: &[EventCatalogSubscription],
    admitted: &BTreeSet<GrantedEventConsumer>,
) -> Vec<EventCatalogSubscription> {
    catalog
        .iter()
        .filter_map(|s| {
            let catalog_filter = canonical_event_filter(&s.filter);
            admitted.iter().find_map(|c| {
                if c.event_type != s.event_type
                    || !s
                        .schema_versions
                        .iter()
                        .all(|v| c.schema_versions.contains(v))
                    || c.filter != catalog_filter
                {
                    return None;
                }
                let mut row = s.clone();
                row.supports_suspend = s.supports_suspend && c.supports_suspend;
                Some(row)
            })
        })
        .collect()
}

/// What an `EVENTS` binding may read back from the outbox.
#[derive(Clone, Debug, Default)]
pub struct EventReaders {
    /// Consented `[[events.consumers]]` subscriptions (manifest ∩ grant).
    pub subscriptions: Vec<EventCatalogSubscription>,
    /// `[events].retention_days`: reads never go further back.
    pub retention_days: u64,
}

/// Host-owned facts a session needs before it can hand a guest an `EVENTS`
/// binding: the outbox store, the producer allowlist and what it may read.
#[derive(Clone)]
pub struct EventOutbox {
    /// Library store whose outbox receives the rows.
    store: LibraryStore,
    /// Plugin id stamped as `source` on every row.
    plugin_id: String,
    /// Consented `[[events.producers]]` event types (manifest ∩ grant).
    producers: BTreeSet<String>,
    /// Consented subscriptions `read` may return.
    readers: EventReaders,
}

impl EventOutbox {
    /// Builds the outbox hook for `plugin_id`, or `None` when the plugin
    /// neither publishes a consented producer nor consumes a consented
    /// subscription (the guest then sees no `EVENTS` binding).
    #[must_use]
    pub fn new(
        store: LibraryStore,
        plugin_id: &str,
        declared: impl IntoIterator<Item = String>,
        granted: &BTreeSet<String>,
        readers: EventReaders,
    ) -> Option<Self> {
        // Host-emitted types (`book_acquired`) never reach a plugin's
        // publisher, even from a grant recorded before the reservation.
        let producers: BTreeSet<String> = declared
            .into_iter()
            .filter(|event_type| granted.contains(event_type))
            .filter(|event_type| !bookclerk_plugin_manifest::is_host_event_type(event_type))
            .collect();
        let readers = EventReaders {
            subscriptions: readers
                .subscriptions
                .into_iter()
                .filter(|sub| !sub.is_queue())
                .collect(),
            ..readers
        };
        if producers.is_empty() && readers.subscriptions.is_empty() {
            return None;
        }
        Some(Self {
            store,
            plugin_id: plugin_id.to_string(),
            producers,
            readers,
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
            readers: self.readers.clone(),
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
    /// Consented subscriptions `read` may return.
    readers: EventReaders,
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
            ..PublishDomainEventSpec::default()
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

    async fn read(&self, request: EventReadRequest) -> Result<EventReadPage> {
        let consented: BTreeSet<String> = self
            .readers
            .subscriptions
            .iter()
            .map(|sub| sub.event_type.clone())
            .collect();
        let query = validate_event_read_request(&request, &consented)?;
        let read = EventStreamRead {
            start: match &query.start {
                EventReadStart::Oldest => EventStreamStart::Oldest,
                EventReadStart::Since(since) => EventStreamStart::Since(*since),
                EventReadStart::After(cursor) => EventStreamStart::After {
                    created_at: cursor.created_at().to_string(),
                    id: cursor.id().to_string(),
                },
            },
            subscriptions: self
                .readers
                .subscriptions
                .iter()
                .filter(|sub| query.types.contains(&sub.event_type))
                .cloned()
                .collect(),
            account_id: (!self.account_id.is_empty()).then(|| self.account_id.clone()),
            subscriber_ids: self.subscriber_ids().await?,
            retention_days: self.readers.retention_days,
            limit: usize::try_from(query.limit).unwrap_or(usize::MAX),
            now: chrono::Utc::now(),
        };
        let page = self
            .store
            .read_event_stream(&read)
            .await
            .map_err(|err| PluginError::unavailable(format!("outbox read failed: {err}")))?;
        let cursor = match &page.last_scanned {
            Some((created_at, id)) => EventCursor::new(created_at.clone(), id.clone())
                .map_err(|_| PluginError::internal("outbox row has no valid position"))?
                .encode(),
            None => request.after,
        };
        Ok(EventReadPage {
            events: page.events.iter().map(read_event).collect(),
            cursor,
            more: page.more,
            expired_event_ids: page.expired_event_ids,
            truncated: page.truncated,
        })
    }
}

impl OutboxEventPublisher {
    /// Catalog ids this plugin's deliveries are filed under: its PluginKey
    /// and every instance of it.
    ///
    /// # Errors
    ///
    /// Returns `unavailable` when the instance list cannot be read.
    async fn subscriber_ids(&self) -> Result<Vec<String>> {
        let instances = bookclerk_library::control_plane::list_plugin_instances_for_key(
            &self.store,
            &self.plugin_id,
        )
        .await
        .map_err(|err| PluginError::unavailable(format!("instance read failed: {err}")))?;
        let mut ids = vec![self.plugin_id.clone()];
        ids.extend(
            instances
                .into_iter()
                .map(|instance| instance.id.to_string()),
        );
        Ok(ids)
    }
}

/// Maps an outbox row onto the ABI event a read returns: not a delivery, so
/// `deliveryAttempt` is zero and the event id is the deduplication key.
fn read_event(event: &DomainEventRecord) -> DomainEvent {
    DomainEvent {
        event_id: event.id.clone(),
        event_type: event.event_type.clone(),
        schema_version: u32::try_from(event.schema_version).unwrap_or(1),
        occurred_at_unix_ms: u64::try_from(event.occurred_at.timestamp_millis()).unwrap_or(0),
        account_id: event.account_id.clone(),
        source: event.source.clone(),
        correlation_id: event.correlation_id.clone(),
        causation_id: event.causation_id.clone(),
        deduplication_key: event.id.clone(),
        delivery_attempt: 0,
        payload: event.payload.as_bytes().to_vec(),
        ..DomainEvent::default()
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
            EventReaders::default(),
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
    async fn host_event_types_never_reach_a_plugin_publisher() {
        // A grant recorded before the reservation still cannot publish
        // `book_acquired`: the host emits it once a source's acquire returns.
        assert!(publisher(&["book_acquired"], &["book_acquired"])
            .await
            .is_none());
        let mixed = publisher(
            &["book_acquired", "demo_pinged"],
            &["book_acquired", "demo_pinged"],
        )
        .await
        .expect("other producers stay");
        let err = mixed
            .spec(PluginEvent {
                event_type: "book_acquired".into(),
                ..PluginEvent::default()
            })
            .expect_err("host-emitted");
        assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Forbidden);
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
            EventReaders::default(),
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

    /// A reader-only outbox for `reader` consenting to `demo_pinged`.
    fn reader_outbox(store: &LibraryStore) -> EventOutbox {
        EventOutbox::new(
            store.clone(),
            "reader",
            Vec::<String>::new(),
            &BTreeSet::new(),
            EventReaders {
                subscriptions: vec![
                    EventCatalogSubscription::new("demo_pinged", vec![1]),
                    EventCatalogSubscription::queue("jobs"),
                ],
                retention_days: 7,
            },
        )
        .expect("a consented consumer alone yields a binding")
    }

    /// Publishes `count` `event_type` rows from `producer` under `account`.
    async fn seed(store: &LibraryStore, event_type: &str, account: &str, count: usize) {
        let outbox = EventOutbox::new(
            store.clone(),
            "producer",
            vec![event_type.to_string()],
            &granted(&[event_type]),
            EventReaders::default(),
        )
        .expect("producer");
        let publisher = outbox.publisher(&invocation(account));
        for n in 0..count {
            publisher
                .publish(PluginEvent {
                    event_type: event_type.into(),
                    payload: format!(r#"{{"n":{n}}}"#).into_bytes(),
                    ..PluginEvent::default()
                })
                .await
                .expect("publish");
        }
    }

    #[tokio::test]
    async fn a_reader_only_binding_reads_but_cannot_publish() {
        let store = memory_store().await;
        seed(&store, "demo_pinged", "acct", 2).await;
        seed(&store, "demo_other", "acct", 1).await;
        let reader = reader_outbox(&store).publisher(&invocation(""));

        let page = reader
            .read(EventReadRequest::default())
            .await
            .expect("read");
        assert_eq!(page.events.len(), 2, "only the consented type");
        assert!(!page.more);
        assert!(!page.truncated);
        assert!(page.expired_event_ids.is_empty());
        for event in &page.events {
            assert_eq!(event.event_type, "demo_pinged");
            assert_eq!(event.source, "producer");
            assert_eq!(event.delivery_attempt, 0);
            assert_eq!(event.deduplication_key, event.event_id);
        }
        assert!(EventCursor::decode(&page.cursor).is_ok());

        let err = reader
            .publish(PluginEvent {
                event_type: "demo_pinged".into(),
                ..PluginEvent::default()
            })
            .await
            .expect_err("a consumer is not a producer");
        assert_eq!(err.code, PluginErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn read_refuses_types_outside_the_consent() {
        let store = memory_store().await;
        let reader = reader_outbox(&store).publisher(&invocation(""));
        for event_type in ["demo_other", "jobs", ""] {
            let err = reader
                .read(EventReadRequest {
                    types: vec![event_type.into()],
                    ..EventReadRequest::default()
                })
                .await
                .expect_err("not consented");
            assert_ne!(err.code, PluginErrorCode::Unavailable, "{event_type:?}");
        }
        let err = reader
            .read(EventReadRequest {
                types: vec!["demo_other".into()],
                ..EventReadRequest::default()
            })
            .await
            .expect_err("not consented");
        assert_eq!(err.code, PluginErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn an_account_scoped_read_sees_only_its_account() {
        let store = memory_store().await;
        seed(&store, "demo_pinged", "acct-a", 2).await;
        seed(&store, "demo_pinged", "acct-b", 3).await;
        let outbox = reader_outbox(&store);
        let read = |account: &'static str| {
            let reader = outbox.publisher(&invocation(account));
            async move {
                reader
                    .read(EventReadRequest::default())
                    .await
                    .expect("read")
                    .events
            }
        };
        let a = read("acct-a").await;
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|e| e.account_id == "acct-a"));
        assert_eq!(read("acct-b").await.len(), 3);
        assert_eq!(read("").await.len(), 5, "an unscoped invocation sees all");
    }

    #[tokio::test]
    async fn the_cursor_continues_a_read_without_repeats() {
        let store = memory_store().await;
        seed(&store, "demo_pinged", "acct", 5).await;
        let reader = reader_outbox(&store).publisher(&invocation(""));
        let mut seen = Vec::new();
        let mut after = String::new();
        loop {
            let page = reader
                .read(EventReadRequest {
                    after: after.clone(),
                    limit: 2,
                    ..EventReadRequest::default()
                })
                .await
                .expect("read");
            assert!(page.events.len() <= 2);
            seen.extend(page.events.into_iter().map(|e| e.event_id));
            after = page.cursor;
            if !page.more {
                break;
            }
        }
        assert_eq!(seen.len(), 5);
        let unique: BTreeSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), 5);

        let tail = reader
            .read(EventReadRequest {
                after: after.clone(),
                ..EventReadRequest::default()
            })
            .await
            .expect("read");
        assert!(tail.events.is_empty());
        assert_eq!(tail.cursor, after, "an empty page keeps the position");

        let err = reader
            .read(EventReadRequest {
                after: "v1.zz".into(),
                ..EventReadRequest::default()
            })
            .await
            .expect_err("bad cursor");
        assert_eq!(err.code, PluginErrorCode::InvalidCursor);
    }
}
