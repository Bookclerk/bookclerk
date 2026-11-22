//! Chapter lists, claims and their resolution, under every database
//! profile.

use super::*;
use crate::entities::domain_events;
use crate::test_support::{sqlite::memory, Profile};
use crate::{AudioStream, FileOwner};

/// A list of `n` top-level chapters a minute apart, titled with `prefix`.
fn list(prefix: &str, n: u64) -> ChapterList {
    ChapterList::new(
        (0..n)
            .map(|i| ChapterInput {
                title: format!("{prefix} {}", i + 1),
                start_ms: i * 60_000,
                depth: 0,
            })
            .collect(),
        ChapterCheck::Strict,
    )
    .unwrap()
}

/// A claim from `source` stating `list`.
fn claim(source: ChapterSource, list: &ChapterList) -> ChapterClaimWrite {
    ChapterClaimWrite {
        source,
        list: list.clone(),
        runtime_ms: Some(3_600_000),
    }
}

/// A store's literary chapters.
fn store_chapters() -> ChapterSource {
    ChapterSource::Listing {
        plugin: "plugin:audible".into(),
        kind: MarkerKind::Chapters,
    }
}

/// A library holding one title; answers its edition.
async fn edition(store: &LibraryStore, asin: &str) -> String {
    store
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .unwrap();
    let book = crate::NewBook::minimal(asin, "acct", "us", format!("Title {asin}"));
    let record = store.upsert_book(&book).await.unwrap();
    store
        .edition_of_holding(&record.holding_id)
        .await
        .unwrap()
        .expect("a held title has an edition")
}

/// How many `edition_updated` events name chapters.
async fn chapter_events(store: &LibraryStore) -> usize {
    domain_events::Entity::find()
        .filter(domain_events::Column::EventType.eq("edition_updated"))
        .all(store.db())
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.dedup_key.contains(":chapters:"))
        .count()
}

/// A bare claim for the pure resolver.
fn bare(kind: ChapterSourceKind, id: &str, quality: ClaimQuality, revision: i64) -> ChapterClaim {
    ChapterClaim {
        source_kind: kind,
        source_id: id.into(),
        quality,
        digest: list(&format!("x{id}"), 1).digest(),
        chapter_count: Some(1),
        runtime_ms: None,
        input_digest: None,
        revision,
        pinned_at: None,
        created_by_user_id: None,
        updated_at: String::new(),
    }
}

#[test]
fn a_strict_list_refuses_what_a_lenient_one_repairs() {
    let input = vec![
        ChapterInput {
            title: "  Opening\u{7}  ".into(),
            start_ms: 0,
            depth: 2,
        },
        ChapterInput {
            title: "\u{1}".into(),
            start_ms: 5,
            depth: 9,
        },
        ChapterInput {
            title: "é".repeat(400),
            start_ms: 5,
            depth: 1,
        },
    ];
    assert!(ChapterList::new(input.clone(), ChapterCheck::Strict).is_err());
    let lenient = ChapterList::new(input, ChapterCheck::Lenient).unwrap();
    let chapters = lenient.chapters();
    assert_eq!(chapters[0].title, "Opening");
    assert_eq!(chapters[0].depth, 0, "the first chapter is top level");
    assert_eq!(chapters[1].title, "Chapter 2");
    assert_eq!(chapters[1].depth, 1, "one level below the one above");
    assert_eq!(chapters[2].title.len(), 512, "cut on a character boundary");
}

#[test]
fn starts_out_of_order_and_too_many_chapters_are_refused_either_way() {
    let backwards = vec![
        ChapterInput {
            title: "B".into(),
            start_ms: 10,
            depth: 0,
        },
        ChapterInput {
            title: "A".into(),
            start_ms: 5,
            depth: 0,
        },
    ];
    for check in [ChapterCheck::Strict, ChapterCheck::Lenient] {
        assert!(ChapterList::new(backwards.clone(), check).is_err());
        let many = (0..=MAX_CHAPTERS as u64)
            .map(|i| ChapterInput {
                title: "x".into(),
                start_ms: i,
                depth: 0,
            })
            .collect();
        assert!(ChapterList::new(many, check).is_err());
        let late = vec![ChapterInput {
            title: "x".into(),
            start_ms: MAX_CHAPTER_START_MS + 1,
            depth: 0,
        }];
        assert!(ChapterList::new(late, check).is_err());
    }
}

#[test]
fn a_digest_names_the_exact_list() {
    let a = list("Chapter", 3);
    assert_eq!(a.digest(), list("Chapter", 3).digest());
    assert_ne!(a.digest(), list("Chapter", 4).digest());
    assert_ne!(a.digest(), list("Part", 3).digest());
    assert_eq!(ChapterDigest::parse(a.digest().as_str()), Some(a.digest()));
    assert_eq!(ChapterDigest::parse("ABC"), None);
    assert_eq!(ChapterDigest::parse(&"G".repeat(64)), None);
}

#[test]
fn spans_end_at_the_next_start_and_the_last_at_the_timeline_end() {
    let tree = ChapterList::new(
        vec![
            ChapterInput {
                title: "Part One".into(),
                start_ms: 0,
                depth: 0,
            },
            ChapterInput {
                title: "One".into(),
                start_ms: 0,
                depth: 1,
            },
            ChapterInput {
                title: "Two".into(),
                start_ms: 60_000,
                depth: 1,
            },
            ChapterInput {
                title: "Part Two".into(),
                start_ms: 120_000,
                depth: 0,
            },
        ],
        ChapterCheck::Strict,
    )
    .unwrap();
    let ends: Vec<(u64, u64, u32)> = tree
        .spans(Some(200_000))
        .iter()
        .map(|s| (s.start_ms, s.end_ms, s.depth))
        .collect();
    assert_eq!(
        ends,
        vec![
            (0, 0, 0),
            (0, 60_000, 1),
            (60_000, 120_000, 1),
            (120_000, 200_000, 0)
        ]
    );
    let last = tree.spans(None);
    assert_eq!(
        last[3].end_ms, 120_000,
        "no runtime: the last ends at its start"
    );
    let short = tree.spans(Some(10));
    assert_eq!(
        short[3].end_ms, 120_000,
        "a runtime before the start is ignored"
    );
    assert!(ChapterList::default().spans(Some(5)).is_empty());
}

#[test]
fn stored_spellings_round_trip() {
    for kind in ChapterSourceKind::ALL {
        assert_eq!(ChapterSourceKind::parse(kind.as_str()), Some(kind));
    }
    for quality in ClaimQuality::ALL {
        assert_eq!(ClaimQuality::parse(quality.as_str()), Some(quality));
    }
}

#[test]
fn the_resolver_follows_pins_edits_then_the_machine_ranking() {
    use ChapterSourceKind as K;
    use ClaimQuality as Q;
    let processed = bare(K::Processed, "", Q::None, 1);
    let provider = bare(K::Provider, "plugin:audnexus", Q::StrongAccurate, 2);
    let store_chapters = bare(K::Listing, "plugin:audible", Q::Chapters, 3);
    let tracks = bare(K::Listing, "plugin:libro", Q::Tracks, 4);
    let parts = bare(K::PartNames, "plugin:libro", Q::None, 5);
    let outside = bare(K::OutsideEdit, "abs", Q::None, 6);
    let person = bare(K::Person, "", Q::None, 7);

    let machine = [
        parts.clone(),
        tracks.clone(),
        store_chapters.clone(),
        provider.clone(),
        processed.clone(),
    ];
    assert_eq!(resolve_chapters(&machine), Some(&processed));
    assert_eq!(resolve_chapters(&machine[..4]), Some(&provider));
    assert_eq!(resolve_chapters(&machine[..3]), Some(&store_chapters));
    assert_eq!(resolve_chapters(&machine[..2]), Some(&tracks));
    assert_eq!(resolve_chapters(&machine[..1]), Some(&parts));

    let mut all = machine.to_vec();
    all.push(outside.clone());
    assert_eq!(resolve_chapters(&all), Some(&outside), "a human override");
    all.push(person.clone());
    assert_eq!(resolve_chapters(&all), Some(&person));

    let mut pinned = provider.clone();
    pinned.pinned_at = Some("2026-11-20T00:00:00Z".into());
    all.push(pinned.clone());
    assert_eq!(resolve_chapters(&all), Some(&pinned));

    // Order never matters.
    let mut reversed = all.clone();
    reversed.reverse();
    assert_eq!(resolve_chapters(&reversed), resolve_chapters(&all));
}

#[test]
fn unfinished_and_empty_machine_lists_never_win() {
    use ChapterSourceKind as K;
    use ClaimQuality as Q;
    let mut unfinished = bare(K::Processed, "", Q::None, 9);
    unfinished.chapter_count = None;
    let mut empty = bare(K::Provider, "plugin:a", Q::StrongAccurate, 8);
    empty.chapter_count = Some(0);
    let tracks = bare(K::Listing, "plugin:b", Q::Tracks, 1);
    let claims = [unfinished, empty, tracks.clone()];
    assert_eq!(resolve_chapters(&claims), Some(&tracks));

    let mut cleared = bare(K::Person, "", Q::None, 10);
    cleared.chapter_count = Some(0);
    let with_clear = [claims[2].clone(), cleared.clone()];
    assert_eq!(
        resolve_chapters(&with_clear),
        Some(&cleared),
        "a person may clear the chapters"
    );
}

/// The store writes, reads and resolves claims; a person's edit wins,
/// removing it falls back, a pin wins over the edit, and each change of
/// the played list publishes one event.
#[tokio::test]
async fn claims_resolve_and_announce_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let edition = edition(&store, "B0CHAP0001").await;
        assert_eq!(
            store.edition_chapters(&edition).await.unwrap(),
            None,
            "{name}"
        );

        let marks = list("Track", 3);
        let outcome = store
            .record_chapter_claim(&edition, &claim(store_chapters(), &marks))
            .await
            .unwrap();
        assert_eq!(outcome, ChaptersOutcome::Changed, "{name}");
        let resolved = store.edition_chapters(&edition).await.unwrap().unwrap();
        assert_eq!(resolved.list, marks, "{name}");
        assert_eq!(resolved.source_kind, ChapterSourceKind::Listing, "{name}");
        assert_eq!(resolved.runtime_ms, Some(3_600_000), "{name}");
        assert_eq!(chapter_events(&store).await, 1, "{name}");

        // The same claim again changes nothing.
        let again = store
            .record_chapter_claim(&edition, &claim(store_chapters(), &marks))
            .await
            .unwrap();
        assert_eq!(again, ChaptersOutcome::Unchanged, "{name}");
        assert_eq!(chapter_events(&store).await, 1, "{name}");

        let aligned = list("Aligned", 3);
        store
            .record_chapter_claim(
                &edition,
                &claim(
                    ChapterSource::Processed {
                        input: Some(marks.digest()),
                    },
                    &aligned,
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .edition_chapters(&edition)
                .await
                .unwrap()
                .unwrap()
                .list,
            aligned,
            "{name}: Bookclerk's aligned chapters outrank the store's"
        );

        let edit = list("Edited", 2);
        store
            .record_chapter_claim(
                &edition,
                &claim(ChapterSource::Person { user_id: None }, &edit),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .edition_chapters(&edition)
                .await
                .unwrap()
                .unwrap()
                .list,
            edit,
            "{name}"
        );

        store
            .pin_chapter_claim(
                &edition,
                Some((ChapterSourceKind::Listing, "plugin:audible")),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .edition_chapters(&edition)
                .await
                .unwrap()
                .unwrap()
                .list,
            marks,
            "{name}: a pin wins over the edit"
        );
        assert!(store
            .pin_chapter_claim(&edition, Some((ChapterSourceKind::File, "x")), None)
            .await
            .is_err());

        store.pin_chapter_claim(&edition, None, None).await.unwrap();
        store.remove_chapter_edit(&edition).await.unwrap();
        assert_eq!(
            store
                .edition_chapters(&edition)
                .await
                .unwrap()
                .unwrap()
                .list,
            aligned,
            "{name}: without the edit and the pin, processing wins again"
        );

        let claims = store.chapter_claims(&edition).await.unwrap();
        assert_eq!(claims.len(), 2, "{name}");
        assert!(
            claims[0].revision > claims[1].revision,
            "{name}: newest first"
        );
        assert_eq!(chapter_events(&store).await, 6, "{name}");

        assert!(matches!(
            store
                .record_chapter_claim("missing", &claim(store_chapters(), &marks))
                .await,
            Err(LibraryError::NotFound(_))
        ));
    }
}

/// A list longer than one batch, with titles at the cell limit, is stored
/// over several batches and read back whole under every profile.
#[tokio::test]
async fn a_long_list_is_stored_and_read_in_pages_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let edition = edition(&store, "B0CHAP0002").await;
        let long = ChapterList::new(
            (0..MAX_CHAPTERS as u64)
                .map(|i| ChapterInput {
                    title: format!("{i:04}{}", "x".repeat(MAX_CHAPTER_TITLE_BYTES - 4)),
                    start_ms: i * 1_000,
                    depth: u32::from(i % 2 == 1),
                })
                .collect(),
            ChapterCheck::Strict,
        )
        .unwrap();
        store
            .record_chapter_claim(&edition, &claim(store_chapters(), &long))
            .await
            .unwrap();
        let read = store.edition_chapters(&edition).await.unwrap().unwrap();
        assert_eq!(read.list, long, "{name}");

        let cleared = ChapterList::default();
        store
            .record_chapter_claim(
                &edition,
                &claim(ChapterSource::Person { user_id: None }, &cleared),
            )
            .await
            .unwrap();
        let read = store.edition_chapters(&edition).await.unwrap().unwrap();
        assert!(
            read.list.is_empty(),
            "{name}: an Administrator cleared them"
        );
    }
}

#[test]
fn a_copy_is_named_by_its_destination_and_keys_in_any_order() {
    let keys = |k: &[&str]| k.iter().map(|k| (*k).to_string()).collect::<Vec<_>>();
    let id = copy_source_id("local", &keys(&["B/Part 2.mp3", "B/Part 1.mp3"]));
    assert_eq!(
        id,
        copy_source_id("local", &keys(&["B/Part 1.mp3", "B/Part 2.mp3"]))
    );
    assert_ne!(
        id,
        copy_source_id("s3", &keys(&["B/Part 1.mp3", "B/Part 2.mp3"]))
    );
    // The separator keeps a key's text from moving into its neighbour.
    assert_ne!(
        copy_source_id("local", &keys(&["ab", "c"])),
        copy_source_id("local", &keys(&["a", "bc"]))
    );
    assert_eq!(id.len(), "copy-".len() + 32);
    assert!(id.starts_with("copy-"));
}

#[tokio::test]
async fn a_copy_far_from_the_lists_timeline_plays_its_own_chapters_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let edition = edition(&store, "B0CHAP0003").await;
        let owner = FileOwner::Edition(edition.clone());
        let key = "B/Book.m4b".to_string();
        assert!(store
            .record_edition_file("local", &key, &owner, crate::FileRole::Audio)
            .await
            .unwrap());
        let keys = vec![key.clone()];
        let copy = copy_source_id("local", &keys);

        // No chapters at all: nothing to play.
        assert_eq!(
            store
                .chapters_for_copy(&edition, "local", &keys)
                .await
                .unwrap(),
            None,
            "{name}"
        );

        // The store's list, measured on an hour; the file's own list on 50
        // minutes, which ranks below it.
        let marks = list("Store", 3);
        store
            .record_chapter_claim(&edition, &claim(store_chapters(), &marks))
            .await
            .unwrap();
        let own = list("Own", 2);
        store
            .record_chapter_claim(
                &edition,
                &ChapterClaimWrite {
                    source: ChapterSource::File { copy: copy.clone() },
                    list: own.clone(),
                    runtime_ms: Some(3_000_000),
                },
            )
            .await
            .unwrap();
        let resolved = store.edition_chapters(&edition).await.unwrap().unwrap();
        assert_eq!(
            resolved.list, marks,
            "{name}: a store's chapters outrank a file's"
        );

        // Not probed yet: the resolved list stands.
        let played = store
            .chapters_for_copy(&edition, "local", &keys)
            .await
            .unwrap();
        assert_eq!(played.map(|p| p.list), Some(marks.clone()), "{name}");

        // Probed within two seconds of the hour: still the resolved list.
        let stream = |ms| AudioStream {
            duration_ms: Some(ms),
            ..AudioStream::default()
        };
        store
            .set_edition_file_stream("local", &key, &owner, &stream(3_601_500))
            .await
            .unwrap();
        let played = store
            .chapters_for_copy(&edition, "local", &keys)
            .await
            .unwrap();
        assert_eq!(played.map(|p| p.list), Some(marks.clone()), "{name}");

        // Fifty minutes long: the copy plays the chapters in its own file.
        store
            .set_edition_file_stream("local", &key, &owner, &stream(3_000_000))
            .await
            .unwrap();
        let played = store
            .chapters_for_copy(&edition, "local", &keys)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(played.list, own, "{name}");
        assert_eq!(played.source_kind, ChapterSourceKind::File, "{name}");
        assert_eq!(played.source_id, copy, "{name}");
        assert_eq!(played.runtime_ms, Some(3_000_000), "{name}");

        // Another copy with no chapters of its own keeps the resolved list.
        let other = vec!["B/Other.m4b".to_string()];
        assert!(store
            .record_edition_file("local", &other[0], &owner, crate::FileRole::Audio)
            .await
            .unwrap());
        store
            .set_edition_file_stream("local", &other[0], &owner, &stream(3_000_000))
            .await
            .unwrap();
        let played = store
            .chapters_for_copy(&edition, "local", &other)
            .await
            .unwrap();
        assert_eq!(played.map(|p| p.list), Some(marks.clone()), "{name}");
    }
}

#[tokio::test]
async fn an_editions_asins_are_listed_under_every_profile() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = memory(profile).await.into_store();
        let edition = edition(&store, "B0CHAP0004").await;
        let asins = store.edition_asins(&edition).await.unwrap();
        assert_eq!(
            asins.first().map(|(asin, _)| asin.as_str()),
            Some("B0CHAP0004"),
            "{name}"
        );
        assert!(
            store.edition_asins("gone").await.unwrap().is_empty(),
            "{name}"
        );
    }
}
