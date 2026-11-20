//! Tests for [`super`]: what each destination holds, and its feed state.

use super::*;
use crate::test_support::{sqlite, Profile};

/// An object `key` of `size` bytes with signature `sig`.
fn seen(key: &str, size: u64, sig: &str) -> SeenObject {
    SeenObject {
        key: key.into(),
        size,
        signature: Some(sig.into()),
        modified_at_ms: Some(1),
    }
}

/// Keys of `changes`, each marked `-` when removed.
fn keys(changes: &[ChangedObject]) -> Vec<String> {
    changes
        .iter()
        .map(|change| {
            if change.removed {
                format!("-{}", change.key)
            } else {
                change.key.clone()
            }
        })
        .collect()
}

#[test]
fn feed_states_round_trip_and_say_when_to_list() {
    for state in FeedState::ALL {
        assert_eq!(FeedState::parse(state.as_str()), Some(state));
    }
    assert_eq!(FeedState::parse("asleep"), None);
    let now = Utc::now();
    let mut feed = DestinationFeed::new("local");
    assert!(feed.listing_due(now), "never listed");
    feed.listed_at = Some(now);
    feed.next_listing_at = Some(now + chrono::Duration::hours(1));
    assert!(!feed.listing_due(now));
    feed.feed_state = FeedState::Gap;
    assert!(feed.listing_due(now), "a gap lists at once");
    feed.feed_state = FeedState::Failing;
    assert!(feed.listing_due(now));
    feed.feed_state = FeedState::Healthy;
    assert!(!feed.listing_due(now));
    assert!(feed.listing_due(now + chrono::Duration::hours(2)), "due");
    feed.listing_seq = Some(3);
    assert!(feed.listing_due(now), "a listing in progress continues");
    assert_eq!(feed.next_seq(), 1);
    assert_eq!(feed.next_seq(), 2);
}

/// Listings and feed pages record adds, changes and removals, each at its
/// sequence; a reader pages them in order; the feed row round-trips; a
/// missing file turns its stored copy pending. Runs under every database
/// test profile.
#[tokio::test]
async fn listings_and_feeds_record_what_changed() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store().clone();
        let now = Utc::now();

        // First listing: everything is new.
        let first: Vec<SeenObject> = (0..250)
            .map(|i| seen(&format!("A/{i:03}.m4b"), 10, "s"))
            .collect();
        assert_eq!(
            store
                .record_seen_objects("d", 1, &first, now)
                .await
                .unwrap(),
            250,
            "{profile:?}"
        );
        assert_eq!(store.finish_listing("d", 1, now).await.unwrap(), 0);
        assert_eq!(store.known_object_count("d").await.unwrap(), 250);

        // Second listing: one changed, one gone, one new, the rest same.
        let mut second: Vec<SeenObject> = first[1..].to_vec();
        second[0] = seen("A/001.m4b", 11, "s");
        second.push(seen("B/new.m4b", 5, "n"));
        assert_eq!(
            store
                .record_seen_objects("d", 2, &second, now)
                .await
                .unwrap(),
            2
        );
        assert_eq!(store.unlisted_object_count("d", 2).await.unwrap(), 1);
        assert_eq!(store.finish_listing("d", 2, now).await.unwrap(), 1);
        let changed = store.changed_objects("d", (1, None), 10).await.unwrap();
        assert_eq!(keys(&changed), ["-A/000.m4b", "A/001.m4b", "B/new.m4b"]);
        assert!(changed.iter().all(|change| change.changed_seq == 2));
        // Paging continues after the last key read.
        let page = store
            .changed_objects("d", (2, Some("A/001.m4b")), 10)
            .await
            .unwrap();
        assert_eq!(keys(&page), ["B/new.m4b"]);
        // Another destination's rows are its own.
        assert!(store
            .changed_objects("other", (0, None), 10)
            .await
            .unwrap()
            .is_empty());

        // A feed page removes one and brings one back.
        assert_eq!(
            store
                .remove_seen_objects("d", 3, &["B/new.m4b".into(), "nope".into()], now)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .record_seen_objects("d", 3, &[seen("A/000.m4b", 10, "s")], now)
                .await
                .unwrap(),
            1,
            "a removed object seen again is a change"
        );
        let changed = store.changed_objects("d", (2, None), 10).await.unwrap();
        assert_eq!(keys(&changed), ["A/000.m4b", "-B/new.m4b"]);

        // A held-back listing marks the unlisted objects seen instead.
        store
            .record_seen_objects("d", 4, &[seen("A/000.m4b", 10, "s")], now)
            .await
            .unwrap();
        store.keep_unlisted_objects("d", 4).await.unwrap();
        assert_eq!(store.unlisted_object_count("d", 4).await.unwrap(), 0);
        assert_eq!(store.finish_listing("d", 4, now).await.unwrap(), 0);

        // Old tombstones are pruned.
        assert_eq!(
            store
                .prune_object_tombstones(now + chrono::Duration::seconds(5))
                .await
                .unwrap(),
            1
        );

        // The feed row round-trips.
        assert!(store.destination_feed("d").await.unwrap().is_none());
        let mut feed = DestinationFeed::new("d");
        feed.seq = 4;
        feed.feed_state = FeedState::Healthy;
        feed.feed_cursor = Some("c".into());
        feed.listed_at = Some(store_second(now));
        feed.objects = 250;
        store.save_destination_feed(&feed).await.unwrap();
        feed.checked_seq = 4;
        store.save_destination_feed(&feed).await.unwrap();
        assert_eq!(store.destination_feed("d").await.unwrap(), Some(feed));
        assert_eq!(store.destination_feeds().await.unwrap().len(), 1);
    }
}

/// `at` cut to whole seconds, as stored.
fn store_second(at: DateTime<Utc>) -> DateTime<Utc> {
    parse_time(&stored_time(at)).unwrap()
}

/// A stored copy whose file is gone turns pending and due; copies on other
/// destinations, other keys and copies not stored are left alone.
#[tokio::test]
async fn a_missing_file_sends_its_copy_back_to_pending() {
    for profile in Profile::ALL {
        let kit = sqlite::memory(profile).await;
        let store = kit.store().clone();
        store
            .upsert_account("audible", "user-1", "us", None, true, "audible")
            .await
            .unwrap();
        let mut editions = Vec::new();
        for product in ["B00ONE0001", "B00TWO0002"] {
            store
                .upsert_book(&crate::NewBook::minimal(product, "user-1", "us", "Title"))
                .await
                .unwrap();
            editions.push(
                store
                    .acquire_titles_named(None, product)
                    .await
                    .unwrap()
                    .remove(0)
                    .edition_id,
            );
        }
        let (one, two) = (&editions[0], &editions[1]);
        store.set_copy_stored(one, "d", "A/One.m4b").await.unwrap();
        store.set_copy_stored(one, "e", "A/One.m4b").await.unwrap();
        store.set_copy_stored(two, "d", "A/Two.m4b").await.unwrap();
        let now = Utc::now();
        assert_eq!(
            store
                .lose_copies("d", &["A/One.m4b".into(), "A/Gone.m4b".into()], now)
                .await
                .unwrap(),
            1,
            "{profile:?}"
        );
        let copies = store
            .copies_of_editions(&[one.clone(), two.clone()])
            .await
            .unwrap();
        let state = |edition: &str, dest: &str| {
            copies
                .iter()
                .find(|c| c.edition_id == edition && c.destination == dest)
                .unwrap()
                .clone()
        };
        let lost = state(one, "d");
        assert_eq!(lost.state, CopyState::Pending);
        assert!(lost.wants_attempt(now));
        assert_eq!(lost.storage_key.as_deref(), Some("A/One.m4b"));
        assert_eq!(state(one, "e").state, CopyState::Stored);
        assert_eq!(state(two, "d").state, CopyState::Stored);
        // A file moved to a key its edition records, which the destination
        // holds: the copy follows it instead of turning pending.
        let owner = crate::FileOwner::Edition(two.clone());
        store
            .record_edition_file("d", "B/Two.m4b", &owner, crate::FileRole::Audio)
            .await
            .unwrap();
        store
            .record_seen_objects("d", 1, &[seen("B/Two.m4b", 1, "s")], now)
            .await
            .unwrap();
        assert_eq!(
            store
                .lose_copies("d", &["A/Two.m4b".into()], now)
                .await
                .unwrap(),
            0
        );
        let moved = store
            .copies_of_editions(std::slice::from_ref(two))
            .await
            .unwrap();
        assert_eq!(moved[0].state, CopyState::Stored);
        assert_eq!(moved[0].storage_key.as_deref(), Some("B/Two.m4b"));
        // Already pending: nothing more to do.
        assert_eq!(
            store
                .lose_copies("d", &["A/One.m4b".into()], now)
                .await
                .unwrap(),
            0
        );
    }
}
