//! Tests for the `file_probe` walk.

use bookclerk_library::{FileOwner, FileRole, NewBook};

use super::*;

/// The storefront's provenance-qualified key, as listings record it.
const PLUGIN: &str = "platform:bookclerk/audible";

/// A file stored before probes were recorded is probed by the walk, with
/// the plain id of the edition's one store as its source; a file that
/// cannot be read stays unprobed for the next walk.
#[tokio::test]
async fn the_walk_probes_files_stored_without_a_probe() {
    bookclerk_media::replace_pool(bookclerk_media::MediaPool::new(
        bookclerk_media::MediaPoolConfig {
            confinement: bookclerk_media::Confinement::Off,
            ..bookclerk_media::MediaPoolConfig::default()
        },
    ));
    let files = tempfile::tempdir().expect("tempdir");
    let storage = tempfile::tempdir().expect("tempdir");
    let (state, library) = crate::node_api::tests::test_state(&files).await;
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());
    library
        .upsert_account(PLUGIN, "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let book = library
        .upsert_book(&NewBook {
            plugin_key: PLUGIN.to_string(),
            ..NewBook::minimal("B0PROBE1", "acct", "us", "Old Copy")
        })
        .await
        .expect("book");
    let edition = library
        .edition_of_holding(&book.holding_id)
        .await
        .expect("read")
        .expect("edition");
    let owner = FileOwner::Edition(edition.clone());
    std::fs::create_dir_all(storage.path().join("Old Copy")).expect("dir");
    std::fs::write(
        storage.path().join("Old Copy/Old Copy.m4b"),
        bookclerk_tags::fixtures::tagged_m4b(),
    )
    .expect("write");
    std::fs::write(storage.path().join("Old Copy/part 2.mp3"), b"not audio").expect("write");
    for (key, role) in [
        ("Old Copy/Old Copy.m4b", FileRole::Audio),
        ("Old Copy/part 2.mp3", FileRole::Part),
    ] {
        assert!(library
            .record_edition_file("local", key, &owner, role)
            .await
            .expect("record"));
    }

    // Rows written just now are still settling: no walk yet.
    queue_file_probe_if_due(&state, "node-a").await;
    let key = JobKind::FileProbe.dedup_key(&probe_payload("node-a"));
    assert!(!library
        .job_admitted_since(&key, Utc::now() - chrono::Duration::minutes(1))
        .await
        .expect("read"));
    assert!(run_file_probe(&state, None)
        .await
        .expect("walk")
        .starts_with("0 file(s) probed, 0 could not"));

    // Once they have stood, the heartbeat queues one walk, and no other
    // inside the interval.
    queue_file_probe_settled(&state, "node-a", std::time::Duration::ZERO).await;
    assert!(library
        .job_admitted_since(&key, Utc::now() - chrono::Duration::minutes(1))
        .await
        .expect("read"));
    let AdmitJob::Duplicate(_) = enqueue_file_probe(&state, "node-a").await.expect("admit") else {
        panic!("the queued walk absorbs another");
    };

    let summary = walk_file_probes(&state, None, std::time::Duration::ZERO)
        .await
        .expect("walk");
    assert!(
        summary.starts_with("1 file(s) probed, 1 could not"),
        "{summary}"
    );
    let stored = library.edition_audio_files(&edition).await.expect("files");
    let audio = stored
        .iter()
        .find(|f| f.storage_key == "Old Copy/Old Copy.m4b")
        .expect("the audio row");
    let probe = audio.probe.as_ref().expect("probed");
    assert_eq!(probe.content_type, "audio/mp4");
    assert_eq!(probe.source_store.as_deref(), Some("audible"));
    assert!(audio.stream.duration_ms.is_some());
    let part = stored
        .iter()
        .find(|f| f.storage_key == "Old Copy/part 2.mp3")
        .expect("the part row");
    assert!(
        part.probe.is_none(),
        "an unreadable file stays for the next walk"
    );
    let left = library
        .unprobed_audio_files(&["local".to_string()], "9999-01-01T00:00:00Z", None, 10)
        .await
        .expect("read");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].storage_key, "Old Copy/part 2.mp3");

    // A probe of a file written again since the walk read it stores
    // nothing.
    let replaced = UnprobedFile {
        written_at: "2000-01-01T00:00:00Z".to_string(),
        ..left[0].clone()
    };
    let probe = FileProbe {
        content_type: "audio/mpeg".to_string(),
        bitrate_kbps: Some(64),
        lossless: false,
        source_store: None,
    };
    assert!(!library
        .set_backfilled_file_probe(&replaced, &probe, &AudioStream::default())
        .await
        .expect("write"));
    assert!(library
        .set_backfilled_file_probe(&left[0], &probe, &AudioStream::default())
        .await
        .expect("write"));
}

/// A walk that was cancelled still counts toward the daily interval: the
/// heartbeat queues no second walk until the interval has passed.
#[tokio::test]
async fn a_cancelled_walk_still_holds_off_the_next_one() {
    let files = tempfile::tempdir().expect("tempdir");
    let storage = tempfile::tempdir().expect("tempdir");
    let (state, library) = crate::node_api::tests::test_state(&files).await;
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());
    library
        .upsert_account(PLUGIN, "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let book = library
        .upsert_book(&NewBook {
            plugin_key: PLUGIN.to_string(),
            ..NewBook::minimal("B0PROBE3", "acct", "us", "Held Off")
        })
        .await
        .expect("book");
    let edition = library
        .edition_of_holding(&book.holding_id)
        .await
        .expect("read")
        .expect("edition");
    assert!(library
        .record_edition_file(
            "local",
            "Held Off/Held Off.m4b",
            &FileOwner::Edition(edition),
            FileRole::Audio
        )
        .await
        .expect("record"));

    let walks = |jobs: Vec<bookclerk_library::JobRecord>| {
        jobs.into_iter()
            .filter(|j| j.kind == JobKind::FileProbe)
            .count()
    };
    queue_file_probe_settled(&state, "node-a", std::time::Duration::ZERO).await;
    let queued = library.list_jobs(50).await.expect("jobs");
    assert_eq!(walks(queued.clone()), 1);
    let id = queued
        .iter()
        .find(|j| j.kind == JobKind::FileProbe)
        .expect("the walk")
        .id
        .clone();
    library
        .request_job_cancel(&id)
        .await
        .expect("cancel")
        .expect("the job");

    queue_file_probe_settled(&state, "node-a", std::time::Duration::ZERO).await;
    assert_eq!(
        walks(library.list_jobs(50).await.expect("jobs")),
        1,
        "the cancelled walk still counts"
    );
}

/// The walk waits for an acquire that holds the work lock, and a file the
/// acquire wrote again meanwhile keeps no probe of the bytes the walk
/// would have read before.
#[tokio::test]
async fn the_walk_waits_for_an_acquire_writing_its_file() {
    bookclerk_media::replace_pool(bookclerk_media::MediaPool::new(
        bookclerk_media::MediaPoolConfig {
            confinement: bookclerk_media::Confinement::Off,
            ..bookclerk_media::MediaPoolConfig::default()
        },
    ));
    let files = tempfile::tempdir().expect("tempdir");
    let storage = tempfile::tempdir().expect("tempdir");
    let (state, library) = crate::node_api::tests::test_state(&files).await;
    crate::node_api::uploads::set_test_destination(files.path(), storage.path());
    library
        .upsert_account(PLUGIN, "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let book = library
        .upsert_book(&NewBook {
            plugin_key: PLUGIN.to_string(),
            ..NewBook::minimal("B0PROBE2", "acct", "us", "Busy Copy")
        })
        .await
        .expect("book");
    let edition = library
        .edition_of_holding(&book.holding_id)
        .await
        .expect("read")
        .expect("edition");
    let owner = FileOwner::Edition(edition.clone());
    let key = "Busy Copy/Busy Copy.m4b";
    std::fs::create_dir_all(storage.path().join("Busy Copy")).expect("dir");
    std::fs::write(
        storage.path().join(key),
        bookclerk_tags::fixtures::tagged_m4b(),
    )
    .expect("write");
    assert!(library
        .record_edition_file("local", key, &owner, FileRole::Audio)
        .await
        .expect("record"));

    // An acquire holds the lock: the walk does not finish while it does.
    let acquire = state.work_lock.lock().await;
    let walk = walk_file_probes(&state, None, std::time::Duration::ZERO);
    tokio::pin!(walk);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut walk)
            .await
            .is_err(),
        "the walk waits for the work lock"
    );
    // The acquire records the key again and writes new bytes, then lets go.
    assert!(library
        .record_edition_file("local", key, &owner, FileRole::Audio)
        .await
        .expect("record"));
    std::fs::write(storage.path().join(key), b"not audio yet").expect("write");
    drop(acquire);

    let summary = walk.await.expect("walk");
    assert!(summary.starts_with("0 file(s) probed"), "{summary}");
    let stored = library.edition_audio_files(&edition).await.expect("files");
    assert!(
        stored
            .iter()
            .find(|f| f.storage_key == key)
            .expect("the audio row")
            .probe
            .is_none(),
        "the replaced file keeps no probe of the old bytes"
    );
}
