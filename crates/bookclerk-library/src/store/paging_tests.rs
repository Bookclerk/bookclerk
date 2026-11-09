//! Every paged listing returns what the full listing returns.
//!
//! Each test opens a store under every [`Profile`], writes more rows than
//! one page holds, and walks the listing at several page sizes with
//! [`paging::assert_pages_match_full`]. A cursor that skips or repeats a
//! row shows up as a walk that differs from the unpaged read.

use super::*;
use crate::models::{
    DomainEventRecord, EnqueueJobSpec, EnqueueOutcome, JobClaimCursor, JobKind, JobPayload,
    JobRecord, JobResourceClass, JobTrigger, PublishDomainEventOutcome, PublishDomainEventSpec,
};
use crate::test_support::paging;
use crate::test_support::Profile;

/// Page sizes every walk is checked at: one row (the shape every listing
/// takes under the host-minimum result cap), a size that does not divide
/// the row count, and one wider than the data.
const PAGE_SIZES: [u64; 3] = [1, 3, 100];

/// An empty store that behaves like one on an external plugin advertising
/// `profile`.
async fn kit(profile: Profile) -> LibraryStore {
    crate::test_support::sqlite::memory(profile)
        .await
        .into_store()
}

/// Ids of `books`, in listing order.
fn uuids(books: &[BookRecord]) -> Vec<String> {
    books.iter().map(|b| b.uuid.clone()).collect()
}

/// Writes `count` books of `account` with alternating acquire status and
/// returns their uuids.
async fn seed_books(store: &LibraryStore, account: &str, count: usize) -> Vec<String> {
    store
        .upsert_account("audible", account, "us", Some(account), true, "audible")
        .await
        .unwrap();
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let product = format!("B{i:05}");
        let book = NewBook::minimal(&product, account, "us", format!("Title {i:05}"));
        let book = store.upsert_book(&book).await.unwrap();
        if i % 2 == 0 {
            store
                .set_acquire_status(&product, account, AcquireStatus::Acquired, None, None)
                .await
                .unwrap();
        }
        out.push(book.uuid);
    }
    out
}

/// Walks an offset-paged book listing: the cursor is the next offset.
async fn assert_offset_pages_match<F, Fut>(full: &[String], sizes: &[u64], read: F)
where
    F: Fn(u64, u64) -> Fut + Copy,
    Fut: std::future::Future<Output = BookPage>,
{
    for &size in sizes {
        paging::assert_pages_match_full(full, size, |after: Option<u64>, size| async move {
            let offset = after.unwrap_or(0);
            let page = read(size, offset).await;
            assert_eq!(page.total, full.len(), "total changes with the page");
            let next_offset = offset + u64::try_from(page.books.len()).unwrap();
            let next = (next_offset < u64::try_from(page.total).unwrap()).then_some(next_offset);
            (uuids(&page.books), next)
        })
        .await;
    }
}

#[tokio::test]
async fn filtered_book_pages_match_the_full_listing() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        // `seed_books` marks every even-numbered book acquired.
        let mut seeded: Vec<(String, &str, bool)> = Vec::new();
        for (account, n) in [("acct-a", 7), ("acct-b", 2)] {
            let books = seed_books(&store, account, n).await;
            for (i, uuid) in books.into_iter().enumerate() {
                seeded.push((uuid, account, i % 2 == 0));
            }
        }
        for (account, status) in [
            (Some("acct-a"), None),
            (Some("acct-a"), Some(AcquireStatus::Acquired.as_str())),
            (None, Some(AcquireStatus::NotAcquired.as_str())),
            (None, None),
        ] {
            let full = store
                .list_books_filtered_page(account, status, BOOK_PAGE_MAX_LIMIT, 0)
                .await
                .unwrap();
            assert_eq!(full.books.len(), full.total, "{}", profile.name());
            let want_acquired = status.map(|s| s == AcquireStatus::Acquired.as_str());
            let mut expected: Vec<String> = seeded
                .iter()
                .filter(|(_, a, _)| account.is_none_or(|want| want == *a))
                .filter(|(_, _, acquired)| want_acquired.is_none_or(|want| want == *acquired))
                .map(|(uuid, _, _)| uuid.clone())
                .collect();
            expected.sort();
            let mut got = uuids(&full.books);
            got.sort();
            assert_eq!(got, expected, "{} {account:?} {status:?}", profile.name());
            let store = &store;
            assert_offset_pages_match(
                &uuids(&full.books),
                &PAGE_SIZES,
                |size, offset| async move {
                    store
                        .list_books_filtered_page(account, status, size, offset)
                        .await
                        .unwrap()
                },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn uuid_book_pages_match_the_full_listing_on_both_read_paths() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        // Fewer uuids than one read chunk page in SQL; more are ordered from
        // narrow key reads first. Both paths must agree with themselves. The
        // long path reads one row at a time under the minimum profile, so
        // it is walked at fewer page sizes.
        let all = seed_books(&store, "acct-u", 70).await;
        for (uuids_in, sizes) in [(&all[..9], &PAGE_SIZES[..]), (&all[..], &PAGE_SIZES[1..])] {
            let mut requested: Vec<String> = uuids_in.to_vec();
            requested.reverse();
            for status in [None, Some(AcquireStatus::Acquired.as_str())] {
                let full = store
                    .list_books_by_uuid_page(&requested, None, status, BOOK_PAGE_MAX_LIMIT, 0)
                    .await
                    .unwrap();
                assert_eq!(full.books.len(), full.total, "{}", profile.name());
                // `seed_books` marks every even-numbered book acquired.
                let mut expected: Vec<String> = all
                    .iter()
                    .enumerate()
                    .filter(|(i, uuid)| uuids_in.contains(uuid) && (status.is_none() || i % 2 == 0))
                    .map(|(_, uuid)| uuid.clone())
                    .collect();
                expected.sort();
                let mut got = uuids(&full.books);
                got.sort();
                assert_eq!(got, expected, "{} {status:?}", profile.name());
                let (store, requested) = (&store, &requested);
                assert_offset_pages_match(&uuids(&full.books), sizes, |size, offset| async move {
                    store
                        .list_books_by_uuid_page(requested, None, status, size, offset)
                        .await
                        .unwrap()
                })
                .await;
            }
        }
    }
}

/// A uuid list whose later entries are much longer than its first ones
/// still reads: the batch width is sized on the longest uuids, so no batch
/// of that width exceeds the adapter's payload cap.
#[tokio::test]
async fn uuid_batches_are_sized_on_the_longest_uuids() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let real = seed_books(&store, "acct-w", 3).await;
        let mut requested: Vec<String> = (0..64).map(|i| format!("s{i}")).collect();
        requested.extend((0..64).map(|i| format!("{i:03}{}", "l".repeat(200))));
        requested.extend(real.iter().cloned());
        let page = store
            .list_books_by_uuid_page(&requested, None, None, BOOK_PAGE_MAX_LIMIT, 0)
            .await
            .unwrap();
        let mut found = uuids(&page.books);
        found.sort();
        let mut expected = real.clone();
        expected.sort();
        assert_eq!(found, expected, "{}", profile.name());
    }
}

/// Long stored uuids that fold to one search hit all hydrate: the
/// hydration batch is sized on the stored uuids it binds, not on the hits,
/// so several long case variants of one hit stay within the payload cap.
#[tokio::test]
async fn long_case_variants_of_one_hit_all_hydrate() {
    let base = "v".repeat(200);
    let variants = case_variants(&base, 8);
    for profile in Profile::ALL {
        let store = kit(profile).await;
        seed_variants(&store, "acct-v", &variants, |i| format!("V {i}")).await;
        let mut requested: Vec<String> = (0..64).map(|i| format!("s{i}")).collect();
        requested.push(base.clone());
        let page = store
            .list_books_by_uuid_page(&requested, None, None, BOOK_PAGE_MAX_LIMIT, 0)
            .await
            .unwrap();
        let mut found = uuids(&page.books);
        found.sort();
        let mut expected = variants.clone();
        expected.sort();
        assert_eq!(page.total, variants.len(), "{}", profile.name());
        assert_eq!(found, expected, "{}", profile.name());
    }
}

/// `count` distinct ASCII case variants of `base`, which must have at
/// least `log2(count)` letters.
fn case_variants(base: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|i| {
            base.char_indices()
                .map(|(at, c)| {
                    if i.checked_shr(u32::try_from(at).unwrap()).unwrap_or(0) & 1 == 1 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect()
        })
        .collect()
}

/// Stores one book per uuid in `uuids` under `account`, titled by `title`.
async fn seed_variants(
    store: &LibraryStore,
    account: &str,
    uuids: &[String],
    title: impl Fn(usize) -> String,
) {
    store
        .upsert_account("audible", account, "us", None, false, "audible")
        .await
        .unwrap();
    for (i, uuid) in uuids.iter().enumerate() {
        let mut book = NewBook::minimal(format!("{account}-{i:05}"), account, "us", title(i));
        book.uuid = Some(uuid.clone());
        store.upsert_book(&book).await.unwrap();
    }
}

/// A long hit list whose one hit matches a book outside the requested page
/// with a cell larger than the Minimum profile's `maxCellBytes`: only keys
/// are read for unrequested rows, so the page still loads.
#[tokio::test]
async fn an_oversized_unrequested_case_variant_does_not_fail_the_page() {
    let base = "o".repeat(40);
    let variants = case_variants(&base, 2);
    for profile in Profile::ALL {
        let store = kit(profile).await;
        seed_variants(&store, "acct-o", &variants, |i| ["A", "B"][i].to_string()).await;
        let mut wide = NewBook::minimal("acct-o-00001", "acct-o", "us", "B");
        wide.uuid = Some(variants[1].clone());
        wide.categories = Some("c".repeat(2_000));
        store.upsert_book(&wide).await.unwrap();
        let mut requested: Vec<String> = (0..64).map(|i| format!("s{i}")).collect();
        requested.push(base.clone());
        let page = store
            .list_books_by_uuid_page(&requested, None, None, 1, 0)
            .await
            .unwrap();
        assert_eq!(page.total, 2, "{}", profile.name());
        assert_eq!(
            uuids(&page.books),
            [variants[0].clone()],
            "{}",
            profile.name()
        );
    }
}

/// One hit that matches more stored uuids than a single key read returns
/// is paged by key until exhausted, so the total counts every match. The
/// key-read limit is lowered to four so nine variants cross it.
#[tokio::test]
async fn more_case_variants_than_one_key_read_all_count() {
    let base = "m".repeat(12);
    let variants = case_variants(&base, 9);
    super::books_page::set_key_read_rows_for_test(Some(4));
    for profile in Profile::ALL {
        let store = kit(profile).await;
        seed_variants(&store, "acct-m", &variants, |i| format!("M {i:05}")).await;
        let mut requested: Vec<String> = (0..64).map(|i| format!("s{i}")).collect();
        requested.push(base.clone());
        let page = store
            .list_books_by_uuid_page(&requested, None, None, BOOK_PAGE_MAX_LIMIT, 1)
            .await
            .unwrap();
        let found = uuids(&page.books);
        let distinct: std::collections::HashSet<&String> = found.iter().collect();
        assert_eq!(page.total, variants.len(), "{}", profile.name());
        assert_eq!(found.len(), variants.len() - 1, "{}", profile.name());
        assert_eq!(distinct.len(), found.len(), "{}", profile.name());
        assert!(
            found.iter().all(|uuid| variants.contains(uuid)),
            "{}",
            profile.name()
        );
    }
    super::books_page::set_key_read_rows_for_test(None);
}

/// A scan job of `account` at `priority`; scans of different accounts never
/// coalesce.
fn scan_job(account: &str, priority: i64) -> EnqueueJobSpec {
    EnqueueJobSpec {
        kind: JobKind::Scan,
        payload: JobPayload {
            account: Some(account.to_string()),
            title: None,
            trigger: JobTrigger::Api,
            ..Default::default()
        },
        priority,
        max_attempts: 3,
        max_pending: 1_000,
        run_after: None,
    }
}

/// Enqueues `count` ready scan jobs with repeating priorities, so the
/// keyset cursor has ties to break on `created_at` and `id`.
async fn seed_jobs(store: &LibraryStore, count: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let priority = i64::try_from(i % 3).unwrap();
        match store
            .enqueue_job(scan_job(&format!("acct-{i:03}"), priority))
            .await
            .unwrap()
        {
            EnqueueOutcome::Created { id } => ids.push(id),
            other => panic!("job {i} was not created: {other:?}"),
        }
    }
    ids
}

/// Ids of `jobs`, in listing order.
fn job_ids(jobs: &[JobRecord]) -> Vec<String> {
    jobs.iter().map(|j| j.id.clone()).collect()
}

#[tokio::test]
async fn job_claim_candidate_pages_match_the_full_listing() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let created = seed_jobs(&store, 8).await;
        let full = store
            .list_job_claim_candidates(JobResourceClass::Network, None, 1_000)
            .await
            .unwrap();
        assert!(full.next.is_none(), "{}", profile.name());
        let full = job_ids(&full.jobs);
        assert_eq!(full.len(), created.len(), "{}", profile.name());
        for size in PAGE_SIZES {
            let store = &store;
            paging::assert_pages_match_full(
                &full,
                size,
                |after: Option<JobClaimCursor>, size| async move {
                    let page = store
                        .list_job_claim_candidates(JobResourceClass::Network, after.as_ref(), size)
                        .await
                        .unwrap();
                    (job_ids(&page.jobs), page.next)
                },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn job_listing_limits_are_prefixes_of_the_full_listing() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let created = seed_jobs(&store, 5).await;
        let full = job_ids(&store.list_jobs(1_000).await.unwrap());
        assert_eq!(full.len(), created.len(), "{}", profile.name());
        for limit in 1..=full.len() + 1 {
            let page = job_ids(&store.list_jobs(limit as u64).await.unwrap());
            assert_eq!(page, full[..limit.min(full.len())], "{}", profile.name());
        }
    }
}

/// Publishes `count` events and dispatches each to no subscriber, so every
/// row is `dispatched` and misses a delivery for every plugin.
async fn seed_dispatched_events(store: &LibraryStore, count: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let spec = PublishDomainEventSpec {
            id: String::new(),
            event_type: "book_acquired".into(),
            schema_version: 1,
            account_id: "acct".into(),
            dedup_key: format!("book_acquired:page-{i:03}"),
            payload: "{}".into(),
            ..PublishDomainEventSpec::default()
        };
        // Outbox insert, not a log.
        // codeql[rust/cleartext-logging]
        let id = match store.publish_domain_event(spec).await.unwrap() {
            PublishDomainEventOutcome::Created { id } => id,
            PublishDomainEventOutcome::Duplicate { .. } => panic!("event {i} was a duplicate"),
        };
        store
            .dispatch_event_deliveries(&id, &[], &format!("dispatch-{id}"))
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

/// Ids of `events`, in listing order.
fn event_ids(events: &[DomainEventRecord]) -> Vec<String> {
    events.iter().map(|e| e.id.clone()).collect()
}

/// The `(created_at, id)` cursor the dispatch reconciler derives from the
/// last event of a page.
fn event_cursor(events: &[DomainEventRecord]) -> Option<(String, String)> {
    events
        .last()
        .map(|e| (e.created_at.to_rfc3339(), e.id.clone()))
}

#[tokio::test]
async fn dispatched_event_pages_match_the_full_listing() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let created = seed_dispatched_events(&store, 7).await;
        let full = store
            .list_dispatched_events_page("", None, 1_000)
            .await
            .unwrap();
        let full = event_ids(&full);
        assert_eq!(full.len(), created.len(), "{}", profile.name());
        for size in PAGE_SIZES {
            let store = &store;
            paging::assert_pages_match_full(
                &full,
                size,
                |after: Option<(String, String)>, size| async move {
                    let page = store
                        .list_dispatched_events_page(
                            "",
                            after.as_ref().map(|(c, i)| (c.as_str(), i.as_str())),
                            size,
                        )
                        .await
                        .unwrap();
                    let next = (u64::try_from(page.len()).unwrap() >= size)
                        .then(|| event_cursor(&page))
                        .flatten();
                    (event_ids(&page), next)
                },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn missing_delivery_event_pages_match_the_full_listing() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let created = seed_dispatched_events(&store, 7).await;
        let types = vec!["book_acquired".to_string()];
        let full = store
            .list_missing_dispatched_events_for_plugin("echo", &types, &[], "", None, 1_000)
            .await
            .unwrap();
        let full = event_ids(&full);
        assert_eq!(full.len(), created.len(), "{}", profile.name());
        for size in PAGE_SIZES {
            let (store, types) = (&store, &types);
            paging::assert_pages_match_full(
                &full,
                size,
                |after: Option<(String, String)>, size| async move {
                    let page = store
                        .list_missing_dispatched_events_for_plugin(
                            "echo",
                            types,
                            &[],
                            "",
                            after.as_ref().map(|(c, i)| (c.as_str(), i.as_str())),
                            size,
                        )
                        .await
                        .unwrap();
                    let next = (u64::try_from(page.len()).unwrap() >= size)
                        .then(|| event_cursor(&page))
                        .flatten();
                    (event_ids(&page), next)
                },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn domain_event_listing_limits_are_prefixes_of_the_full_listing() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let created = seed_dispatched_events(&store, 5).await;
        let full = event_ids(&store.list_domain_events(1_000).await.unwrap());
        assert_eq!(full.len(), created.len(), "{}", profile.name());
        for limit in 1..=full.len() + 1 {
            let page = event_ids(&store.list_domain_events(limit as u64).await.unwrap());
            assert_eq!(page, full[..limit.min(full.len())], "{}", profile.name());
        }
    }
}

#[tokio::test]
async fn storage_scan_identity_pages_match_the_full_listing() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        store
            .storage_scan_register("scan-page", "local:test", None)
            .await
            .unwrap();
        // Ranks repeat, so the cursor breaks ties on the object key; a
        // second identity must stay out of the page.
        for i in 0..7u8 {
            store
                .storage_scan_put_identity("scan-page", "isbn:1", &format!("a/{i}.m4b"), 1, i % 3)
                .await
                .unwrap();
        }
        store
            .storage_scan_put_identity("scan-page", "isbn:2", "b/0.m4b", 1, 0)
            .await
            .unwrap();
        let full = store
            .storage_scan_identity_page("scan-page", "isbn:1", None, 64)
            .await
            .unwrap();
        assert_eq!(full.len(), 7, "{}", profile.name());
        for size in PAGE_SIZES {
            let store = &store;
            paging::assert_pages_match_full(
                &full,
                size,
                |after: Option<(i64, String)>, size| async move {
                    let page = store
                        .storage_scan_identity_page(
                            "scan-page",
                            "isbn:1",
                            after.as_ref().map(|(rank, key)| (*rank, key.as_str())),
                            size,
                        )
                        .await
                        .unwrap();
                    // The listing clamps its page to 64 rows.
                    let next = (u64::try_from(page.len()).unwrap() >= size.min(64))
                        .then(|| page.last().map(|(key, rank)| (*rank, key.clone())))
                        .flatten();
                    (page, next)
                },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn discover_candidates_are_read_whole_under_every_profile() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let subject = DiscoverSubject::User(7);
        // Rows as wide as one statement of the profile carries, so forty of
        // them exceed the smallest result cap many times over and the
        // internal id walk has to keep going.
        let caps = store.db_capabilities();
        let width = usize::try_from(caps.max_payload_bytes / 3)
            .unwrap()
            .min(usize::try_from(caps.max_cell_bytes).unwrap() / 2);
        let rows: Vec<(String, String)> = (0..40)
            .map(|i| {
                let key = format!("cand-{i:03}");
                let json = format!("{{\"key\":\"{key}\",\"pad\":\"{}\"}}", "x".repeat(width));
                (key, json)
            })
            .collect();
        store
            .replace_discover_candidates(&subject, "audible", SeedScope::Own, &rows)
            .await
            .unwrap();
        let listed = store.list_discover_candidates(&subject).await.unwrap();
        let mut keys: Vec<String> = listed.iter().map(|c| c.candidate_key.clone()).collect();
        keys.sort();
        let expected: Vec<String> = rows.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, expected, "{}", profile.name());
        for candidate in &listed {
            let (_, json) = rows
                .iter()
                .find(|(k, _)| *k == candidate.candidate_key)
                .unwrap();
            assert_eq!(&candidate.candidate_json, json, "{}", profile.name());
        }
    }
}
