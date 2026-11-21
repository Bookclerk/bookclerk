//! Notification routing, the delivery log and the inbox, under every
//! database profile.

use bookclerk_plugin_abi::{
    NotificationAlertAction, NotificationAudience, NotificationField, NotificationSeverity,
};

use super::notifications::{NotifyCursor, NotifyQueueLimits};
use super::LibraryStore;
use crate::error::LibraryError;
use crate::models::{JobFence, JobKind, UserRole};
use crate::notifications::{
    DeliveryState, DestinationKind, FailedAttempt, FailureEffect, NewDestination, NewNotification,
    PatronChoice, PatronOffer, Route, PAUSE_AFTER_FAILURES,
};
use crate::test_support::{sqlite::memory, Profile};

/// Room for every job these tests enqueue.
const LIMITS: NotifyQueueLimits = NotifyQueueLimits {
    max_pending: 1_000,
    max_attempts: 3,
};

/// Every delivery-log row (for one destination when given), newest first,
/// walked page by page so no read passes the profile's result-row cap.
async fn all_deliveries(
    store: &LibraryStore,
    destination: Option<&str>,
) -> Vec<crate::notifications::DeliveryRecord> {
    let mut out = Vec::new();
    let mut cursor: Option<NotifyCursor> = None;
    loop {
        let page = store
            .notify_deliveries(destination, cursor.as_ref(), 50)
            .await
            .unwrap();
        if let Some(last) = page.items.last() {
            cursor = Some(NotifyCursor {
                created_at: last.created_at.clone(),
                id: last.id.clone(),
            });
        }
        out.extend(page.items);
        if !page.more {
            return out;
        }
    }
}

/// An operational notification of `notification_type` at `severity`.
fn operational(notification_type: &str, severity: NotificationSeverity) -> NewNotification {
    NewNotification {
        notification_type: notification_type.into(),
        audience: NotificationAudience::Operational,
        severity,
        title: "Scan failed".into(),
        text: "The Libro.fm scan stopped.".into(),
        link_path: Some("/jobs".into()),
        fields: vec![NotificationField {
            name: "Account".into(),
            value: "libro-1".into(),
        }],
        group_key: None,
        alert_action: NotificationAlertAction::None,
        alert_key: None,
        source: "host".into(),
        patron: None,
    }
}

/// A patron notification of `notification_type` for `patron`.
fn for_patron(notification_type: &str, patron: i64) -> NewNotification {
    NewNotification {
        audience: NotificationAudience::Patron,
        title: "A new book".into(),
        text: "Your wishlist book is out.".into(),
        patron: Some(patron),
        ..operational(notification_type, NotificationSeverity::Info)
    }
}

/// A channel on notifier instance `instance`.
async fn channel(store: &LibraryStore, instance: &str, label: &str) -> String {
    store
        .create_notify_destination(&NewDestination {
            plugin_instance_id: instance.into(),
            kind: DestinationKind::Channel,
            user_id: None,
            label: label.into(),
            address_json: String::new(),
        })
        .await
        .unwrap()
        .id
}

/// A personal destination of `user` on notifier instance `instance`.
async fn personal(store: &LibraryStore, user: i64, instance: &str) -> String {
    store
        .create_notify_destination(&NewDestination {
            plugin_instance_id: instance.into(),
            kind: DestinationKind::Personal,
            user_id: Some(user),
            label: "My phone".into(),
            address_json: r#"{"chatId":"42"}"#.into(),
        })
        .await
        .unwrap()
        .id
}

/// A new member.
async fn member(store: &LibraryStore, name: &str) -> i64 {
    store
        .create_user(UserRole::Member, Some(name), None)
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn an_operational_notification_goes_to_the_routes_its_severity_reaches_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let ops = channel(&store, "slack-1", "Ops").await;
        let pager = channel(&store, "pagerduty-1", "Pager").await;
        let quiet = channel(&store, "slack-1", "Paused").await;
        store
            .set_notify_routes(
                "scan_failed",
                &[
                    Route {
                        destination_id: ops.clone(),
                        min_severity: NotificationSeverity::Info,
                    },
                    Route {
                        destination_id: pager.clone(),
                        min_severity: NotificationSeverity::Critical,
                    },
                    Route {
                        destination_id: quiet.clone(),
                        min_severity: NotificationSeverity::Info,
                    },
                ],
            )
            .await
            .unwrap();
        // Five refusals pause a channel; a paused channel gets nothing.
        for _ in 0..PAUSE_AFTER_FAILURES {
            let routed = store
                .route_notification(
                    &operational("scan_failed", NotificationSeverity::Warning),
                    LIMITS,
                )
                .await
                .unwrap();
            for delivery in &routed.deliveries {
                let work = store.notify_send_work(&delivery.id).await.unwrap().unwrap();
                let dest = work.destination.unwrap().id;
                if dest == quiet {
                    store
                        .record_notify_failed(
                            &delivery.id,
                            "401 from Slack",
                            FailedAttempt::New,
                            None,
                        )
                        .await
                        .unwrap();
                } else {
                    store
                        .record_notify_delivered(&delivery.id, Some("msg-1"), None)
                        .await
                        .unwrap();
                }
            }
        }
        let paused = store.notify_destination(&quiet).await.unwrap().unwrap();
        assert!(paused.paused_at.is_some(), "{name}");
        assert_eq!(
            paused.paused_reason.as_deref(),
            Some("401 from Slack"),
            "{name}"
        );

        let warning = store
            .route_notification(
                &operational("scan_failed", NotificationSeverity::Warning),
                LIMITS,
            )
            .await
            .unwrap();
        assert!(!warning.inboxed, "{name}");
        assert_eq!(warning.deliveries.len(), 1, "{name}");
        assert_eq!(
            warning.deliveries[0].plugin_instance_id, "slack-1",
            "{name}"
        );

        let critical = store
            .route_notification(
                &operational("scan_failed", NotificationSeverity::Critical),
                LIMITS,
            )
            .await
            .unwrap();
        let mut instances: Vec<_> = critical
            .deliveries
            .iter()
            .map(|d| d.plugin_instance_id.clone())
            .collect();
        instances.sort();
        assert_eq!(instances, ["pagerduty-1", "slack-1"], "{name}");

        // Each delivery has its send job, pinned to its notifier instance.
        for delivery in &critical.deliveries {
            let work = store.notify_send_work(&delivery.id).await.unwrap().unwrap();
            assert_eq!(work.state, DeliveryState::Pending, "{name}");
            assert_eq!(work.notification.id, critical.notification_id, "{name}");
            assert_eq!(work.notification.notification.fields.len(), 1, "{name}");
        }
        assert!(store
            .unscheduled_notify_deliveries(10)
            .await
            .unwrap()
            .is_empty());
        let jobs = store.list_jobs(100).await.unwrap();
        let sends: Vec<_> = jobs
            .iter()
            .filter(|j| j.kind == JobKind::NotifySend)
            .filter(|j| {
                critical
                    .deliveries
                    .iter()
                    .any(|d| j.payload.notify_delivery.as_deref() == Some(d.id.as_str()))
            })
            .collect();
        assert_eq!(sends.len(), 2, "{name}");
        for job in sends {
            assert_eq!(
                job.payload.requires,
                [job.payload.plugin_instance_id.clone().unwrap()],
                "{name}"
            );
        }

        // An unrouted type is stored and goes nowhere.
        let nowhere = store
            .route_notification(
                &operational("other", NotificationSeverity::Critical),
                LIMITS,
            )
            .await
            .unwrap();
        assert!(nowhere.deliveries.is_empty(), "{name}");
    }
}

#[tokio::test]
async fn a_patron_notification_follows_offers_defaults_and_choices_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let riley = member(&store, "Riley").await;
        let telegram = personal(&store, riley, "telegram-1").await;
        let mail = personal(&store, riley, "email-1").await;
        let unoffered = personal(&store, riley, "discord-1").await;

        // Nothing offered: the inbox only.
        let routed = store
            .route_notification(&for_patron("echo:book_new", riley), LIMITS)
            .await
            .unwrap();
        assert!(routed.inboxed, "{name}");
        assert!(routed.deliveries.is_empty(), "{name}");

        store
            .set_patron_offers(
                "echo:book_new",
                &[
                    PatronOffer {
                        plugin_instance_id: "telegram-1".into(),
                        default_on: true,
                    },
                    PatronOffer {
                        plugin_instance_id: "email-1".into(),
                        default_on: false,
                    },
                ],
            )
            .await
            .unwrap();
        // No choice yet: the default-on offers.
        let routed = store
            .route_notification(&for_patron("echo:book_new", riley), LIMITS)
            .await
            .unwrap();
        assert_eq!(routed.deliveries.len(), 1, "{name}");
        assert_eq!(
            routed.deliveries[0].plugin_instance_id, "telegram-1",
            "{name}"
        );

        // A choice replaces the defaults.
        store
            .set_patron_choice(
                riley,
                &PatronChoice {
                    notification_type: "echo:book_new".into(),
                    enabled: true,
                    destination_ids: vec![mail.clone()],
                },
            )
            .await
            .unwrap();
        let routed = store
            .route_notification(&for_patron("echo:book_new", riley), LIMITS)
            .await
            .unwrap();
        assert_eq!(routed.deliveries.len(), 1, "{name}");
        assert_eq!(routed.deliveries[0].plugin_instance_id, "email-1", "{name}");
        let choices = store.patron_choices(riley).await.unwrap();
        assert_eq!(choices.len(), 1, "{name}");
        assert_eq!(
            choices[0].destination_ids,
            std::slice::from_ref(&mail),
            "{name}"
        );

        // Turned off: still in the inbox, sent nowhere.
        store
            .set_patron_choice(
                riley,
                &PatronChoice {
                    notification_type: "echo:book_new".into(),
                    enabled: false,
                    destination_ids: vec![telegram.clone()],
                },
            )
            .await
            .unwrap();
        let routed = store
            .route_notification(&for_patron("echo:book_new", riley), LIMITS)
            .await
            .unwrap();
        assert!(routed.inboxed, "{name}");
        assert!(routed.deliveries.is_empty(), "{name}");

        store
            .clear_patron_choice(riley, "echo:book_new")
            .await
            .unwrap();
        assert!(store.patron_choices(riley).await.unwrap().is_empty());

        // Another patron's destination answers like a missing one.
        let sam = member(&store, "Sam").await;
        let err = store
            .set_patron_choice(
                sam,
                &PatronChoice {
                    notification_type: "echo:book_new".into(),
                    enabled: true,
                    destination_ids: vec![telegram.clone()],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::NotFound(_)), "{name}: {err}");
        let err = store
            .set_patron_choice(
                riley,
                &PatronChoice {
                    notification_type: "echo:book_new".into(),
                    enabled: true,
                    destination_ids: vec![mail.clone(), mail.clone()],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");
        // A notifier not offered for the type cannot be picked for it,
        // though a choice that is off may still name it.
        let unoffered_pick = |enabled| PatronChoice {
            notification_type: "echo:book_new".into(),
            enabled,
            destination_ids: vec![unoffered.clone()],
        };
        let err = store
            .set_patron_choice(riley, &unoffered_pick(true))
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");
        store
            .set_patron_choice(riley, &unoffered_pick(false))
            .await
            .unwrap();

        // A patron keeps a bounded number of addresses, so the default
        // fanout fits one routing batch.
        let many = member(&store, "Many").await;
        for n in 0..crate::notifications::MAX_ROUTES_PER_TYPE {
            personal(&store, many, &format!("telegram-{n}")).await;
        }
        let err = store
            .create_notify_destination(&NewDestination {
                plugin_instance_id: "telegram-x".into(),
                kind: DestinationKind::Personal,
                user_id: Some(many),
                label: "One too many".into(),
                address_json: "{\"chat_id\":\"1\"}".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");

        // A patron notification needs a patron who exists.
        let err = store
            .route_notification(&for_patron("echo:book_new", 9_999), LIMITS)
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::NotFound(_)), "{name}: {err}");
        let mut orphan = for_patron("echo:book_new", riley);
        orphan.patron = None;
        let err = store.route_notification(&orphan, LIMITS).await.unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");
    }
}

#[tokio::test]
async fn the_inbox_pages_newest_first_and_marks_read_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let riley = member(&store, "Riley").await;
        let sam = member(&store, "Sam").await;
        let mut ids = Vec::new();
        for n in 0..5 {
            let mut new = for_patron("echo:book_new", riley);
            new.title = format!("Book {n}");
            ids.push(
                store
                    .route_notification(&new, LIMITS)
                    .await
                    .unwrap()
                    .notification_id,
            );
        }
        store
            .route_notification(&for_patron("echo:book_new", sam), LIMITS)
            .await
            .unwrap();
        assert_eq!(store.inbox_unread(riley).await.unwrap(), 5, "{name}");

        // Pages never pass the profile's result-row cap; walking them
        // gives every entry newest first: (created_at, id) falls.
        let cap = match profile.capabilities().max_result_rows {
            0 => usize::MAX,
            rows => usize::try_from(rows).unwrap(),
        };
        let walk = |size: u64| {
            let store = &store;
            async move {
                let mut seen = Vec::new();
                let mut cursor: Option<NotifyCursor> = None;
                let mut pages = 0;
                loop {
                    let page = store.inbox(riley, cursor.as_ref(), size).await.unwrap();
                    pages += 1;
                    assert!(page.items.len() <= cap, "{size}: page past the cap");
                    let last = page.items.last().unwrap();
                    cursor = Some(NotifyCursor {
                        created_at: last.notification.created_at.clone(),
                        id: last.notification.id.clone(),
                    });
                    seen.extend(
                        page.items
                            .into_iter()
                            .map(|e| (e.notification.created_at, e.notification.id)),
                    );
                    if !page.more {
                        break;
                    }
                }
                (seen, pages)
            }
        };
        let (keys, pages) = walk(50).await;
        assert_eq!(pages, 5usize.div_ceil(cap.min(50)), "{name}");
        assert!(keys.windows(2).all(|w| w[0] > w[1]), "{name}: {keys:?}");
        let mut expected = ids.clone();
        expected.sort();
        let mut listed: Vec<String> = keys.iter().map(|(_, id)| id.clone()).collect();
        listed.sort();
        assert_eq!(listed, expected, "{name}");

        // Walking two at a time gives the same order, and the last page
        // hands out no cursor.
        let (two_at_a_time, pages) = walk(2).await;
        assert_eq!(pages, 5usize.div_ceil(cap.min(2)), "{name}");
        assert_eq!(two_at_a_time, keys, "{name}");
        // A full final page says no more follows.
        let two = store.inbox(sam, None, 1).await.unwrap();
        assert_eq!(two.items.len(), 1, "{name}");
        assert!(!two.more, "{name}");

        assert_eq!(
            store.mark_inbox_read(riley, Some(&ids[0])).await.unwrap(),
            1,
            "{name}"
        );
        assert_eq!(store.inbox_unread(riley).await.unwrap(), 4, "{name}");
        // Someone else's notification is not found in this inbox.
        let err = store.mark_inbox_read(sam, Some(&ids[1])).await.unwrap_err();
        assert!(matches!(err, LibraryError::NotFound(_)), "{name}: {err}");
        assert_eq!(
            store.mark_inbox_read(riley, None).await.unwrap(),
            4,
            "{name}"
        );
        assert_eq!(store.inbox_unread(riley).await.unwrap(), 0, "{name}");
        assert_eq!(store.inbox_unread(sam).await.unwrap(), 1, "{name}");
    }
}

#[tokio::test]
async fn send_outcomes_move_deliveries_once_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let ops = channel(&store, "slack-1", "Ops").await;
        store
            .set_notify_routes(
                "test",
                &[Route {
                    destination_id: ops.clone(),
                    min_severity: NotificationSeverity::Info,
                }],
            )
            .await
            .unwrap();
        let route = || async {
            store
                .route_notification(&operational("test", NotificationSeverity::Info), LIMITS)
                .await
                .unwrap()
                .deliveries
                .remove(0)
                .id
        };

        let retried = route().await;
        assert_eq!(
            store
                .record_notify_retry(&retried, "429 rate limited", None)
                .await
                .unwrap(),
            Some(1),
            "{name}"
        );
        assert_eq!(
            store
                .record_notify_retry(&retried, "again", None)
                .await
                .unwrap(),
            Some(2),
            "{name}"
        );
        assert!(store
            .record_notify_delivered(&retried, None, None)
            .await
            .unwrap());
        // A finished delivery does not move again.
        assert!(!store
            .record_notify_delivered(&retried, None, None)
            .await
            .unwrap());
        assert_eq!(
            store
                .record_notify_failed(&retried, "late", FailedAttempt::New, None)
                .await
                .unwrap(),
            None,
            "{name}"
        );
        assert_eq!(
            store
                .record_notify_retry(&retried, "late", None)
                .await
                .unwrap(),
            None,
            "{name}"
        );

        let dropped = route().await;
        assert!(store
            .record_notify_dropped(&dropped, "destination paused", None)
            .await
            .unwrap());
        let failed = route().await;
        assert_eq!(
            store
                .record_notify_failed(&failed, "bad\naddress", FailedAttempt::New, None)
                .await
                .unwrap(),
            Some(FailureEffect::Counted),
            "{name}"
        );
        // A delivery that ran out of retries is not charged its last
        // answer twice.
        let exhausted = route().await;
        assert_eq!(
            store
                .record_notify_retry(&exhausted, "429", None)
                .await
                .unwrap(),
            Some(1),
            "{name}"
        );
        assert_eq!(
            store
                .record_notify_failed(&exhausted, "gave up", FailedAttempt::AlreadyCounted, None)
                .await
                .unwrap(),
            Some(FailureEffect::Counted),
            "{name}"
        );
        // A send whose claim no longer holds its job records nothing.
        let stale = route().await;
        let fence = JobFence {
            job_id: "notify_send-gone".into(),
            owner: "worker".into(),
            generation: 1,
        };
        assert!(!store
            .record_notify_delivered(&stale, None, Some(&fence))
            .await
            .unwrap());
        assert_eq!(
            store
                .record_notify_retry(&stale, "429", Some(&fence))
                .await
                .unwrap(),
            None,
            "{name}"
        );
        assert_eq!(
            store
                .record_notify_failed(&stale, "refused", FailedAttempt::New, Some(&fence))
                .await
                .unwrap(),
            None,
            "{name}"
        );
        assert!(!store
            .record_notify_dropped(&stale, "paused", Some(&fence))
            .await
            .unwrap());

        let log = all_deliveries(&store, Some(&ops)).await;
        let state_of = |id: &str| log.iter().find(|d| d.id == id).unwrap().clone();
        let r = state_of(&retried);
        assert_eq!(r.state, DeliveryState::Delivered, "{name}");
        assert_eq!(r.attempts, 3, "{name}");
        assert_eq!(r.reason, None, "{name}");
        assert_eq!(state_of(&dropped).state, DeliveryState::Dropped, "{name}");
        let f = state_of(&failed);
        assert_eq!(f.state, DeliveryState::Failed, "{name}");
        // Reasons are one line.
        assert!(!f.reason.unwrap().contains('\n'), "{name}");
        assert_eq!(state_of(&exhausted).attempts, 1, "{name}");
        let st = state_of(&stale);
        assert_eq!(st.state, DeliveryState::Pending, "{name}");
        assert_eq!(st.attempts, 0, "{name}");
        assert_eq!(
            store
                .notify_destination(&ops)
                .await
                .unwrap()
                .unwrap()
                .failure_count,
            2,
            "{name}"
        );

        // Deleting a destination takes its routes and log rows with it.
        store.delete_notify_destination(&ops).await.unwrap();
        assert!(store.notify_destination(&ops).await.unwrap().is_none());
        assert!(store.notify_routes().await.unwrap().is_empty(), "{name}");
        assert!(store
            .notify_deliveries(None, None, 50)
            .await
            .unwrap()
            .items
            .is_empty());
    }
}

#[tokio::test]
async fn a_paused_destination_resumes_and_routes_refuse_bad_targets_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let riley = member(&store, "Riley").await;
        let ops = channel(&store, "slack-1", "Ops").await;
        let phone = personal(&store, riley, "telegram-1").await;
        store
            .set_notify_routes(
                "test",
                &[Route {
                    destination_id: ops.clone(),
                    min_severity: NotificationSeverity::Info,
                }],
            )
            .await
            .unwrap();
        let mut effect = None;
        for _ in 0..PAUSE_AFTER_FAILURES {
            let id = store
                .route_notification(&operational("test", NotificationSeverity::Info), LIMITS)
                .await
                .unwrap()
                .deliveries
                .remove(0)
                .id;
            effect = store
                .record_notify_failed(&id, "410 gone", FailedAttempt::New, None)
                .await
                .unwrap();
        }
        assert_eq!(effect, Some(FailureEffect::Paused), "{name}");
        store.resume_notify_destination(&ops).await.unwrap();
        let resumed = store.notify_destination(&ops).await.unwrap().unwrap();
        assert!(resumed.paused_at.is_none(), "{name}");
        assert_eq!(resumed.failure_count, 0, "{name}");

        store
            .rename_notify_destination(&ops, "  Ops room ")
            .await
            .unwrap();
        assert_eq!(
            store.notify_destination(&ops).await.unwrap().unwrap().label,
            "Ops room",
            "{name}"
        );
        let err = store
            .rename_notify_destination(&ops, "two\nlines")
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");

        // Routes go to channels only, each once.
        let err = store
            .set_notify_routes(
                "test",
                &[Route {
                    destination_id: phone.clone(),
                    min_severity: NotificationSeverity::Info,
                }],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, LibraryError::Invalid(_) | LibraryError::NotFound(_)),
            "{name}: {err}"
        );
        let twice = Route {
            destination_id: ops.clone(),
            min_severity: NotificationSeverity::Info,
        };
        let err = store
            .set_notify_routes("test", &[twice.clone(), twice])
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");

        // A channel has no owner; a personal destination has one.
        let err = store
            .create_notify_destination(&NewDestination {
                plugin_instance_id: "slack-1".into(),
                kind: DestinationKind::Channel,
                user_id: Some(riley),
                label: "Mine".into(),
                address_json: String::new(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");
        let err = store
            .create_notify_destination(&NewDestination {
                plugin_instance_id: "telegram-1".into(),
                kind: DestinationKind::Personal,
                user_id: None,
                label: "Nobody's".into(),
                address_json: "{}".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{name}: {err}");
        assert_eq!(store.notify_channels().await.unwrap().len(), 1, "{name}");
        assert_eq!(
            store.personal_destinations(riley).await.unwrap().len(),
            1,
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_full_queue_leaves_deliveries_for_the_sweep_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let ops = channel(&store, "slack-1", "Ops").await;
        store
            .set_notify_routes(
                "test",
                &[Route {
                    destination_id: ops,
                    min_severity: NotificationSeverity::Info,
                }],
            )
            .await
            .unwrap();
        let full = NotifyQueueLimits {
            max_pending: 0,
            max_attempts: 3,
        };
        let routed = store
            .route_notification(&operational("test", NotificationSeverity::Info), full)
            .await
            .unwrap();
        let waiting = store.unscheduled_notify_deliveries(10).await.unwrap();
        assert_eq!(waiting.len(), 1, "{name}");
        assert_eq!(waiting[0].id, routed.deliveries[0].id, "{name}");
        let job = store
            .enqueue_notify_send(&waiting[0], LIMITS)
            .await
            .unwrap()
            .expect("room in the queue");
        assert!(store
            .unscheduled_notify_deliveries(10)
            .await
            .unwrap()
            .is_empty());
        // A live job keeps its delivery; a cancelled one drops it.
        assert_eq!(
            store.drop_orphaned_notify_deliveries().await.unwrap(),
            0,
            "{name}"
        );
        store.request_job_cancel(&job).await.unwrap();
        assert_eq!(
            store.drop_orphaned_notify_deliveries().await.unwrap(),
            1,
            "{name}"
        );
        let work = store
            .notify_send_work(&waiting[0].id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.state, DeliveryState::Dropped, "{name}");

        // A send job whose link write was lost: once it ends, the sweep
        // drops the delivery instead of queueing a second send.
        let routed = store
            .route_notification(&operational("test", NotificationSeverity::Info), full)
            .await
            .unwrap();
        let unlinked = routed.deliveries[0].id.clone();
        let outcome = store
            .enqueue_job(crate::models::EnqueueJobSpec {
                kind: JobKind::NotifySend,
                payload: crate::models::JobPayload {
                    notify_delivery: Some(unlinked.clone()),
                    ..crate::models::JobPayload::default()
                },
                priority: 0,
                max_attempts: 1,
                max_pending: 1_000,
                run_after: None,
            })
            .await
            .unwrap();
        let crate::models::EnqueueOutcome::Created { id: lost } = outcome else {
            panic!("{name}: {outcome:?}");
        };
        assert!(!store
            .drop_notify_delivery_if_job_ended(&unlinked)
            .await
            .unwrap());
        store.request_job_cancel(&lost).await.unwrap();
        assert!(store
            .drop_notify_delivery_if_job_ended(&unlinked)
            .await
            .unwrap());
        let work = store.notify_send_work(&unlinked).await.unwrap().unwrap();
        assert_eq!(work.state, DeliveryState::Dropped, "{name}");
    }
}

#[tokio::test]
async fn pruning_deletes_old_notifications_with_their_rows_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let riley = member(&store, "Riley").await;
        store
            .route_notification(&for_patron("echo:book_new", riley), LIMITS)
            .await
            .unwrap();
        assert_eq!(
            store.prune_notifications(chrono::Utc::now()).await.unwrap(),
            0,
            "{name}"
        );
        let later = chrono::Utc::now() + chrono::Duration::days(31);
        assert_eq!(store.prune_notifications(later).await.unwrap(), 1, "{name}");
        assert!(store.inbox(riley, None, 10).await.unwrap().items.is_empty());
    }
}

#[tokio::test]
async fn a_patron_holds_an_account_through_a_linked_identity_under_every_profile() {
    use sea_orm::ConnectionTrait;
    for profile in Profile::ALL {
        let name = profile.name();
        let kit = memory(profile).await;
        let store = kit.store();
        let riley = member(store, "Riley").await;
        let sam = member(store, "Sam").await;
        store
            .upsert_account("audible", "riley-acct", "us", None, true, "audible")
            .await
            .unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        for sql in [
            format!(
                "INSERT INTO portal_identities (id, provider, external_user_id, user_id, \
                 created_at) VALUES (501, 'local', 'riley', {riley}, '{now}')"
            ),
            format!(
                "INSERT INTO account_links (identity_id, plugin_key, account_id, source, \
                 created_at) VALUES (501, 'audible', 'riley-acct', 'audible', '{now}')"
            ),
        ] {
            kit.connection()
                .execute_unprepared(&sql)
                .await
                .unwrap_or_else(|err| panic!("{sql}: {err}"));
        }
        assert!(store
            .user_holds_account(riley, "audible", "riley-acct")
            .await
            .unwrap());
        assert!(!store
            .user_holds_account(sam, "audible", "riley-acct")
            .await
            .unwrap());
        assert!(!store
            .user_holds_account(riley, "libro", "riley-acct")
            .await
            .unwrap());
        assert!(
            !store
                .user_holds_account(riley, "audible", "other")
                .await
                .unwrap(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_test_goes_to_one_destination_whatever_the_routes_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let ops = channel(&store, "slack-1", "Ops").await;
        let routed = store
            .route_notification_to(
                &operational("test", NotificationSeverity::Info),
                &ops,
                LIMITS,
            )
            .await
            .unwrap();
        assert_eq!(routed.deliveries.len(), 1, "{name}");
        let err = store
            .route_notification_to(
                &operational("test", NotificationSeverity::Info),
                "missing",
                LIMITS,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::NotFound(_)), "{name}: {err}");
    }
}

#[test]
fn a_cursor_round_trips_and_a_foreign_token_is_refused() {
    let cursor = NotifyCursor {
        created_at: "2026-11-21T10:00:00+00:00".into(),
        id: "6c1f2e8a".into(),
    };
    assert_eq!(NotifyCursor::decode(&cursor.encode()).unwrap(), cursor);
    for bad in ["", "!!", "e30", &"A".repeat(600)] {
        assert!(NotifyCursor::decode(bad).is_err(), "{bad}");
    }
}
