//! Progress, play sessions and bookmarks, under every database profile.

use super::*;
use crate::control_plane::{changed_topics, live_revisions};
use crate::models::UserRole;
use crate::test_support::{sqlite::memory, Profile};

/// Records `asins` on the store account `account`; answers the work id of
/// each.
async fn hold(store: &LibraryStore, _user: i64, account: &str, asins: &[String]) -> Vec<String> {
    store
        .upsert_account("audible", account, "us", None, true, "audible")
        .await
        .unwrap();
    let mut works = Vec::new();
    for asin in asins {
        let book = crate::NewBook::minimal(asin, account, "us", format!("Title {asin}"));
        let record = store.upsert_book(&book).await.unwrap();
        works.push(store.work_id_for_book(&record.uuid).await.unwrap().unwrap());
    }
    works
}

/// A timeline length of `ms` milliseconds.
fn ms(ms: u64) -> PlaybackMs {
    PlaybackMs::new(ms).expect("in range")
}

/// A library where a new member holds one title; answers the member, the
/// title's edition and its work.
async fn listener(store: &LibraryStore, asin: &str) -> (i64, String, String) {
    let user = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    let work = hold(store, user, "riley-acct", &[asin.to_string()])
        .await
        .remove(0);
    let record = store.get_book(asin, "riley-acct").await.unwrap().unwrap();
    let edition = store
        .edition_of_book(&record.uuid)
        .await
        .unwrap()
        .expect("a held title has an edition");
    (user, edition, work)
}

/// A session of `user` on `edition`, opened from the web player.
async fn open(store: &LibraryStore, user: i64, edition: &str) -> PlaybackSession {
    store
        .open_playback_session(NewPlaybackSession {
            user_id: user,
            edition_id: edition,
            serving_node_id: "node-1",
            destination: "local",
            files_digest: "digest",
            client_kind: PlayerKind::Web,
            client_name: "  Firefox on Linux  ",
        })
        .await
        .unwrap()
}

#[test]
fn a_timeline_length_is_bounded() {
    assert_eq!(PlaybackMs::new(0), Some(PlaybackMs::ZERO));
    assert_eq!(ms(MAX_PLAYBACK_MS).get(), MAX_PLAYBACK_MS);
    assert_eq!(PlaybackMs::new(MAX_PLAYBACK_MS + 1), None);
    assert_eq!(PlaybackMs::from_stored(-5), PlaybackMs::ZERO);
    assert_eq!(PlaybackMs::from_stored(i64::MAX).get(), MAX_PLAYBACK_MS);
    assert_eq!(ms(1500).stored(), 1500);
}

#[test]
fn the_end_is_the_last_percent_or_the_last_half_minute() {
    let ten_hours = ms(36_000_000);
    assert!(!reaches_end(ms(35_000_000), ten_hours));
    assert!(reaches_end(ms(35_640_000), ten_hours), "99 percent");
    let half_hour = ms(1_800_000);
    assert!(!reaches_end(ms(1_769_999), half_hour));
    assert!(reaches_end(ms(1_770_000), half_hour), "30 s from the end");
    let clip = ms(40_000);
    assert!(
        !reaches_end(ms(20_000), clip),
        "no 30 s rule under a minute"
    );
    assert!(reaches_end(ms(39_600), clip));
    assert!(!reaches_end(ms(0), PlaybackMs::ZERO), "unknown runtime");
}

#[test]
fn player_kinds_round_trip_and_unknown_reads_as_app() {
    for kind in PlayerKind::ALL {
        assert_eq!(PlayerKind::parse(kind.as_str()), kind);
    }
    assert_eq!(PlayerKind::parse("watch"), PlayerKind::App);
}

#[test]
fn text_is_cut_on_a_character_boundary() {
    assert_eq!(truncated("abc", 8), "abc");
    assert_eq!(truncated("héllo", 2), "h");
    assert_eq!(truncated("héllo", 3), "hé");
}

/// A session starts at the stored position, syncs move the progress,
/// reaching the end finishes the edition and its work once, a closed
/// session takes no more reports, and playing a finished edition again
/// starts it over.
#[tokio::test]
async fn a_session_moves_progress_and_finishes_the_work_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let (user, edition, work) = listener(&store, "B0PLAY0001").await;
        assert!(store
            .playback_progress(user, &edition)
            .await
            .unwrap()
            .is_none());

        let before = live_revisions(&store).await.unwrap();
        let session = open(&store, user, &edition).await;
        let opened = live_revisions(&store).await.unwrap();
        assert_eq!(
            changed_topics(&before, &opened),
            vec![LiveTopic::Playback],
            "{name}"
        );
        assert_eq!(session.start_position, PlaybackMs::ZERO, "{name}");
        assert_eq!(session.client_name, "Firefox on Linux", "{name}");
        assert_eq!(session.client_kind, PlayerKind::Web, "{name}");
        assert!(session.closed_at.is_none(), "{name}");

        let hour = ms(3_600_000);
        let change = store
            .sync_playback_session(
                &session,
                Some(&work),
                &PlaybackSync {
                    position: ms(60_000),
                    listened: ms(15_000),
                    duration: Some(hour),
                    close: false,
                },
            )
            .await
            .unwrap()
            .expect("an open session syncs");
        assert!(!change.became_finished, "{name}");
        assert_eq!(change.progress.position, ms(60_000), "{name}");
        assert_eq!(change.progress.duration, Some(hour), "{name}");
        assert_eq!(
            changed_topics(&opened, &live_revisions(&store).await.unwrap()),
            vec![],
            "{name}: a position sync does not move the topic"
        );
        let stored = store.playback_session(&session.id).await.unwrap().unwrap();
        assert_eq!(stored.listened, ms(15_000), "{name}");
        assert_eq!(stored.position, ms(60_000), "{name}");

        // The end: the runtime is the one stored by the sync before.
        let change = store
            .sync_playback_session(
                &stored,
                Some(&work),
                &PlaybackSync {
                    position: ms(3_590_000),
                    listened: ms(10_000),
                    duration: None,
                    close: true,
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(change.became_finished, "{name}");
        assert!(change.progress.is_finished, "{name}");
        assert!(change.progress.finished_at.is_some(), "{name}");
        let state = store.user_title_state(user, &work).await.unwrap().unwrap();
        assert!(state.is_finished, "{name}: the work is finished too");
        let closed = store.playback_session(&session.id).await.unwrap().unwrap();
        assert!(closed.closed_at.is_some(), "{name}");
        assert_eq!(closed.listened, ms(25_000), "{name}");

        // Closed: neither the read-before copy nor a stale open copy syncs.
        for stale in [&closed, &stored] {
            assert!(
                store
                    .sync_playback_session(
                        stale,
                        Some(&work),
                        &PlaybackSync {
                            position: ms(5),
                            listened: PlaybackMs::ZERO,
                            duration: None,
                            close: false,
                        },
                    )
                    .await
                    .unwrap()
                    .is_none(),
                "{name}"
            );
        }
        assert_eq!(
            store
                .playback_progress(user, &edition)
                .await
                .unwrap()
                .unwrap()
                .position,
            ms(3_590_000),
            "{name}: a refused sync moved nothing"
        );

        let again = open(&store, user, &edition).await;
        assert_eq!(again.start_position, PlaybackMs::ZERO, "{name}");
        let restarted = store
            .playback_progress(user, &edition)
            .await
            .unwrap()
            .unwrap();
        assert!(!restarted.is_finished, "{name}");
        assert!(
            store
                .user_title_state(user, &work)
                .await
                .unwrap()
                .unwrap()
                .is_finished,
            "{name}: the work stays finished"
        );
    }
}

/// Reports that overlap (each read the session before the other wrote)
/// both count their listened time, and one that lost the race with the
/// session's close finishes nothing, the work included.
#[tokio::test]
async fn overlapping_reports_add_up_and_a_late_one_finishes_nothing_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let (user, edition, work) = listener(&store, "B0PLAY0005").await;
        let read_once = open(&store, user, &edition).await;
        let hour = ms(3_600_000);
        let report = |position: u64, listened: u64, close: bool| PlaybackSync {
            position: ms(position),
            listened: ms(listened),
            duration: Some(hour),
            close,
        };

        for position in [10_000, 20_000] {
            store
                .sync_playback_session(&read_once, Some(&work), &report(position, 1000, false))
                .await
                .unwrap()
                .expect("an open session syncs");
        }
        let stored = store
            .playback_session(&read_once.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.listened, ms(2000), "{name}: both reports count");

        // The total never passes the longest timeline.
        store
            .sync_playback_session(
                &read_once,
                Some(&work),
                &report(30_000, MAX_PLAYBACK_MS, false),
            )
            .await
            .unwrap()
            .unwrap();
        let stored = store
            .playback_session(&read_once.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.listened, ms(MAX_PLAYBACK_MS), "{name}");

        // A close lands; then a report that read the session while it was
        // open says the listener reached the end.
        store
            .sync_playback_session(&read_once, Some(&work), &report(40_000, 0, true))
            .await
            .unwrap()
            .unwrap();
        store
            .delete_playback_progress(user, &edition)
            .await
            .unwrap();
        assert!(
            store
                .sync_playback_session(&read_once, Some(&work), &report(3_599_000, 0, false))
                .await
                .unwrap()
                .is_none(),
            "{name}: the session had closed"
        );
        assert!(
            store
                .playback_progress(user, &edition)
                .await
                .unwrap()
                .is_none(),
            "{name}: a late report does not start progress again"
        );
        assert!(
            !store
                .user_title_state(user, &work)
                .await
                .unwrap()
                .is_some_and(|state| state.is_finished),
            "{name}: a late report does not finish the work"
        );
    }
}

/// Progress set outright creates the row, decides finished from the
/// position, takes an explicit flag either way, and is forgotten on
/// delete.
#[tokio::test]
async fn progress_is_set_and_forgotten_outright_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let (user, edition, work) = listener(&store, "B0PLAY0002").await;
        let hour = ms(3_600_000);
        let set = store
            .set_playback_progress(
                user,
                &edition,
                Some(&work),
                &PlaybackProgressEdit {
                    position: Some(ms(120_000)),
                    duration: Some(hour),
                    finished: None,
                },
            )
            .await
            .unwrap();
        assert!(!set.became_finished, "{name}");
        assert_eq!(set.progress.position, ms(120_000), "{name}");

        let marked = store
            .set_playback_progress(
                user,
                &edition,
                Some(&work),
                &PlaybackProgressEdit {
                    position: None,
                    duration: None,
                    finished: Some(true),
                },
            )
            .await
            .unwrap();
        assert!(marked.became_finished, "{name}");
        assert_eq!(marked.progress.position, ms(120_000), "{name}: kept");
        assert_eq!(marked.progress.duration, Some(hour), "{name}: kept");
        assert!(
            store
                .user_title_state(user, &work)
                .await
                .unwrap()
                .unwrap()
                .is_finished
        );

        let cleared = store
            .set_playback_progress(
                user,
                &edition,
                Some(&work),
                &PlaybackProgressEdit {
                    position: Some(PlaybackMs::ZERO),
                    duration: None,
                    finished: Some(false),
                },
            )
            .await
            .unwrap();
        assert!(!cleared.progress.is_finished, "{name}");
        assert!(!cleared.became_finished, "{name}");

        let by_position = store
            .set_playback_progress(
                user,
                &edition,
                None,
                &PlaybackProgressEdit {
                    position: Some(ms(3_599_000)),
                    duration: None,
                    finished: None,
                },
            )
            .await
            .unwrap();
        assert!(by_position.became_finished, "{name}");

        assert!(store
            .delete_playback_progress(user, &edition)
            .await
            .unwrap());
        assert!(!store
            .delete_playback_progress(user, &edition)
            .await
            .unwrap());
        assert!(store
            .playback_progress(user, &edition)
            .await
            .unwrap()
            .is_none());
    }
}

/// Continue listening pages one person's unfinished editions, most
/// recently played first, and never shows another person's.
#[tokio::test]
async fn progress_pages_most_recent_first_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let (user, first, _) = listener(&store, "B0PLAY0003").await;
        let asins: Vec<String> = (4..7).map(|n| format!("B0PLAY000{n}")).collect();
        hold(&store, user, "riley-acct", &asins).await;
        let mut editions = vec![first];
        for asin in &asins {
            let record = store.get_book(asin, "riley-acct").await.unwrap().unwrap();
            editions.push(store.edition_of_book(&record.uuid).await.unwrap().unwrap());
        }
        let other = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap()
            .id;
        for (index, edition) in editions.iter().enumerate() {
            let position = u64::try_from(index).unwrap() * 1000 + 1000;
            for person in [user, other] {
                store
                    .set_playback_progress(
                        person,
                        edition,
                        None,
                        &PlaybackProgressEdit {
                            position: Some(ms(position)),
                            duration: None,
                            // The second edition is finished for Riley.
                            finished: Some(index == 1 && person == user),
                        },
                    )
                    .await
                    .unwrap();
            }
            // Distinct stamps, so the order is the write order.
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        let (page, next) = store
            .list_playback_progress(user, false, None, 2)
            .await
            .unwrap();
        let ids: Vec<&str> = page.iter().map(|p| p.edition_id.as_str()).collect();
        assert_eq!(ids, [editions[3].as_str(), editions[2].as_str()], "{name}");
        let next = next.expect("a third unfinished edition remains");
        let (page, next) = store
            .list_playback_progress(user, false, Some(&next), 2)
            .await
            .unwrap();
        let ids: Vec<&str> = page.iter().map(|p| p.edition_id.as_str()).collect();
        assert_eq!(ids, [editions[0].as_str()], "{name}");
        assert!(next.is_none(), "{name}");
        let (all, next) = store
            .list_playback_progress(user, true, None, 10)
            .await
            .unwrap();
        assert_eq!(all.len(), 4, "{name}");
        assert!(next.is_none(), "{name}");
        assert!(all.iter().all(|p| p.user_id == user), "{name}");
    }
}

/// Bookmarks are a person's own: listed in timeline order, edited and
/// removed only by them, and capped per edition.
#[tokio::test]
async fn bookmarks_belong_to_their_person_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let (user, edition, _) = listener(&store, "B0PLAY0007").await;
        let other = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap()
            .id;
        let late = store
            .add_bookmark(user, &edition, ms(90_000), "  the storm  ")
            .await
            .unwrap()
            .unwrap();
        let early = store
            .add_bookmark(user, &edition, ms(5_000), "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(late.title, "the storm", "{name}");
        let listed = store.list_bookmarks(user, &edition).await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, [early.id.as_str(), late.id.as_str()], "{name}");
        assert!(store
            .list_bookmarks(other, &edition)
            .await
            .unwrap()
            .is_empty());

        assert!(store
            .edit_bookmark(other, &late.id, None, Some("mine now"))
            .await
            .unwrap()
            .is_none());
        let edited = store
            .edit_bookmark(user, &late.id, Some(ms(91_000)), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edited.position, ms(91_000), "{name}");
        assert_eq!(edited.title, "the storm", "{name}: kept");
        let renamed = store
            .edit_bookmark(user, &late.id, None, Some(""))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.title, "", "{name}: an empty title clears it");
        assert_eq!(renamed.position, ms(91_000), "{name}");

        assert!(!store.delete_bookmark(other, &early.id).await.unwrap());
        assert!(store.delete_bookmark(user, &early.id).await.unwrap());
        assert!(store.bookmark(&early.id).await.unwrap().is_none());
    }
}

/// The cap holds: the bookmark past it is not stored.
#[tokio::test]
async fn an_edition_takes_a_bounded_number_of_bookmarks() {
    let store = memory(Profile::ALL[0]).await.into_store();
    let (user, edition, _) = listener(&store, "B0PLAY0008").await;
    for index in 0..MAX_BOOKMARKS_PER_EDITION {
        assert!(store
            .add_bookmark(user, &edition, ms(index), "")
            .await
            .unwrap()
            .is_some());
    }
    assert!(store
        .add_bookmark(user, &edition, ms(1), "one too many")
        .await
        .unwrap()
        .is_none());
}

/// Deleting a person removes their listening state.
#[tokio::test]
async fn a_deleted_person_leaves_no_listening_state() {
    let store = memory(Profile::ALL[0]).await.into_store();
    let (user, edition, _) = listener(&store, "B0PLAY0009").await;
    store
        .create_user(UserRole::Owner, Some("Owner"), None)
        .await
        .unwrap();
    let session = open(&store, user, &edition).await;
    let bookmark = store
        .add_bookmark(user, &edition, ms(1), "")
        .await
        .unwrap()
        .unwrap();
    store.delete_user(user).await.unwrap();
    assert!(store
        .playback_progress(user, &edition)
        .await
        .unwrap()
        .is_none());
    assert!(store.playback_session(&session.id).await.unwrap().is_none());
    assert!(store.bookmark(&bookmark.id).await.unwrap().is_none());
}

/// Under a database plugin whose reply holds one row of these tables and
/// not two, the progress list and an edition's bookmarks are still read
/// whole and in order: each is read in pages that shrink to what the
/// plugin allows.
#[tokio::test]
async fn lists_read_whole_when_a_reply_holds_one_row() {
    let _lock = crate::master_key::master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, caps) = crate::control_plane::capped_store(dir.path()).await;
    let (user, first, _) = listener(&store, "B0PLAY0020").await;
    let asins: Vec<String> = (1..4).map(|n| format!("B0PLAY002{n}")).collect();
    hold(&store, user, "riley-acct", &asins).await;
    let mut editions = vec![first];
    for asin in &asins {
        let record = store.get_book(asin, "riley-acct").await.unwrap().unwrap();
        editions.push(store.edition_of_book(&record.uuid).await.unwrap().unwrap());
    }
    for edition in &editions {
        store
            .set_playback_progress(
                user,
                edition,
                None,
                &PlaybackProgressEdit {
                    position: Some(ms(1000)),
                    duration: None,
                    finished: Some(false),
                },
            )
            .await
            .unwrap();
        // Distinct stamps, so the order is the write order.
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    for position in [30_000, 10_000, 20_000] {
        store
            .add_bookmark(user, &editions[0], ms(position), "")
            .await
            .unwrap()
            .unwrap();
    }
    // One row of either table carries about 100 bytes of text (ids and
    // stamps); 150 admits one row and refuses two.
    caps.reply.store(150, std::sync::atomic::Ordering::SeqCst);

    let newest_first: Vec<&str> = editions.iter().rev().map(String::as_str).collect();
    let (page, next) = store
        .list_playback_progress(user, false, None, 3)
        .await
        .unwrap();
    let ids: Vec<&str> = page.iter().map(|p| p.edition_id.as_str()).collect();
    assert_eq!(ids, newest_first[..3]);
    let next = next.expect("a fourth edition remains");
    let (page, next) = store
        .list_playback_progress(user, false, Some(&next), 3)
        .await
        .unwrap();
    let ids: Vec<&str> = page.iter().map(|p| p.edition_id.as_str()).collect();
    assert_eq!(ids, newest_first[3..]);
    assert!(next.is_none());

    let places: Vec<u64> = store
        .list_bookmarks(user, &editions[0])
        .await
        .unwrap()
        .iter()
        .map(|b| b.position.get())
        .collect();
    assert_eq!(places, [10_000, 20_000, 30_000]);
}
