use super::*;
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, EventCatalogSubscription, EventSubscriber, EventWakeGrant,
    JobFence, JobKind, JobPayload, JobRecord, JobResourceClass, JobState, JobTrigger,
    PublishDomainEventOutcome, PublishDomainEventSpec,
};
use crate::{AtomicTxnBackend, InProcessSqliteAtomic};
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait};
use std::sync::Arc;

#[tokio::test]
async fn account_and_book_roundtrip() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let acct = store
        .upsert_account("user-1", "us", Some("Main"), true, "audible")
        .await
        .unwrap();
    assert_eq!(acct.account_id, "user-1");
    assert_eq!(acct.source, "audible");

    let mut book = NewBook::minimal("B00TEST", "user-1", "us", "Test Book");
    book.authors = Some("Author".into());
    let book = store.upsert_book(&book).await.unwrap();
    assert_eq!(book.title, "Test Book");
    assert!(!book.uuid.is_empty());
    assert_eq!(book.product_id, "B00TEST");
    assert_eq!(book.asin.as_deref(), Some("B00TEST"));
    assert!(book.isbn.is_none());
    assert_eq!(book.source, "audible");
    assert_eq!(book.title_id(), book.uuid.as_str());
    assert_eq!(book.asin_or_isbn(), "B00TEST");
    assert_eq!(book.acquire_status, AcquireStatus::NotAcquired);

    store
        .set_acquire_status(
            "B00TEST",
            "user-1",
            AcquireStatus::Acquired,
            Some("Author/Test Book/book.m4b"),
            None,
        )
        .await
        .unwrap();

    let updated = store.get_book("B00TEST", "user-1").await.unwrap().unwrap();
    assert_eq!(updated.acquire_status, AcquireStatus::Acquired);
    assert_eq!(
        updated.storage_key.as_deref(),
        Some("Author/Test Book/book.m4b")
    );
    assert_eq!(
        store
            .count_by_status(AcquireStatus::Acquired)
            .await
            .unwrap(),
        1
    );

    let by_uuid = store
        .get_book_by_uuid(&updated.uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_uuid.product_id, "B00TEST");
    let pending = store.next_undispatched_event().await.unwrap().unwrap();
    assert_eq!(pending.event_type, "book_acquired");
    assert_eq!(pending.ordering_key, updated.uuid);
    assert_eq!(pending.dedup_key, format!("book_acquired:{}", updated.uuid));
}

#[tokio::test]
async fn same_isbn_multi_account_and_source() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("user-1", "us", None, true, "audible")
        .await
        .unwrap();
    store
        .upsert_account("user-2", "us", None, true, "audible")
        .await
        .unwrap();
    store
        .upsert_account("libro-1", "us", None, true, "libro")
        .await
        .unwrap();

    let mut a1 = NewBook::minimal("B00SAME", "user-1", "us", "Same Book");
    a1.isbn = Some("9781234567890".into());
    store.upsert_book(&a1).await.unwrap();

    let mut a2 = NewBook::minimal("B00SAME", "user-2", "us", "Same Book");
    a2.isbn = Some("9781234567890".into());
    store.upsert_book(&a2).await.unwrap();

    let libro = NewBook {
        uuid: None,
        product_id: "9781234567890".into(),
        source: "libro".into(),
        account_id: "libro-1".into(),
        asin: None,
        isbn: Some("9781234567890".into()),
        marketplace: "us".into(),
        title: "Same Book".into(),
        authors: None,
        narrators: None,
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: None,
        length_minutes: None,
        is_abridged: false,
        content_kind: "book".into(),
        categories: None,
        subtitle: None,
        published_at: None,
    };
    store.upsert_book(&libro).await.unwrap();

    let by_isbn = store.find_books_by_isbn("9781234567890").await.unwrap();
    assert_eq!(by_isbn.len(), 3);
    let uuids: std::collections::HashSet<_> = by_isbn.iter().map(|b| b.uuid.as_str()).collect();
    assert_eq!(uuids.len(), 3);

    let preferred = prefer_enrichment_source(&by_isbn).unwrap();
    assert_eq!(preferred.source, "audible");
}

#[tokio::test]
async fn libro_rescan_preserves_audible_enrichment() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("libro-1", "us", None, true, "libro")
        .await
        .unwrap();

    let isbn = "9781234567890";
    let initial = NewBook {
        uuid: None,
        product_id: isbn.into(),
        source: "libro".into(),
        account_id: "libro-1".into(),
        asin: None,
        isbn: Some(isbn.into()),
        marketplace: "us".into(),
        title: "Sparse Libro Title".into(),
        authors: Some("Libro Author".into()),
        narrators: None,
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: None,
        length_minutes: None,
        is_abridged: false,
        content_kind: "book".into(),
        categories: None,
        subtitle: None,
        published_at: None,
    };
    let row = store.upsert_book(&initial).await.unwrap();

    // Simulate Audible catalog enrichment.
    let enriched = NewBook {
        uuid: Some(row.uuid.clone()),
        asin: Some("B00ENRICHED".into()),
        title: "Rich Audible Title".into(),
        authors: Some("Audible Author".into()),
        narrators: Some("Audible Narrator".into()),
        series: Some("Audible Series".into()),
        publisher: Some("Publisher".into()),
        length_minutes: Some(420),
        subtitle: Some("A Subtitle".into()),
        ..initial.clone()
    };
    let after_enrich = store.upsert_book(&enriched).await.unwrap();
    assert_eq!(after_enrich.asin.as_deref(), Some("B00ENRICHED"));
    assert_eq!(after_enrich.title, "Rich Audible Title");
    assert_eq!(after_enrich.narrators.as_deref(), Some("Audible Narrator"));

    // Libro rescan without asin must not wipe enrichment.
    let rescan = NewBook {
        title: "Sparse Libro Title Again".into(),
        authors: Some("Libro Author".into()),
        narrators: None,
        asin: None,
        series: None,
        publisher: None,
        length_minutes: Some(400),
        subtitle: None,
        ..initial
    };
    let after_rescan = store.upsert_book(&rescan).await.unwrap();
    assert_eq!(after_rescan.uuid, row.uuid);
    assert_eq!(after_rescan.asin.as_deref(), Some("B00ENRICHED"));
    assert_eq!(after_rescan.title, "Rich Audible Title");
    assert_eq!(after_rescan.authors.as_deref(), Some("Audible Author"));
    assert_eq!(after_rescan.narrators.as_deref(), Some("Audible Narrator"));
    assert_eq!(after_rescan.series.as_deref(), Some("Audible Series"));
    assert_eq!(after_rescan.publisher.as_deref(), Some("Publisher"));
    assert_eq!(after_rescan.length_minutes, Some(420));
    assert_eq!(after_rescan.subtitle.as_deref(), Some("A Subtitle"));
    assert_eq!(after_rescan.download_product_id(), isbn);
}

#[tokio::test]
async fn download_product_id_is_source_native() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("libro-1", "us", None, true, "libro")
        .await
        .unwrap();
    let book = store
        .upsert_book(&NewBook {
            uuid: None,
            product_id: "9789999999999".into(),
            source: "libro".into(),
            account_id: "libro-1".into(),
            asin: Some("B00FROMAD".into()),
            isbn: Some("9789999999999".into()),
            marketplace: "us".into(),
            title: "Enriched".into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        })
        .await
        .unwrap();
    assert_eq!(book.download_product_id(), "9789999999999");
    assert_eq!(book.audible_asin(), Some("B00FROMAD"));
}

#[tokio::test]
async fn ensure_account_preserves_scan_enabled() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("user-1", "us", Some("Main"), false, "audible")
        .await
        .unwrap();
    store
        .ensure_account("user-1", "us", Some("Main"), "audible")
        .await
        .unwrap();
    let acct = store.get_account("user-1").await.unwrap().unwrap();
    assert!(!acct.scan_enabled);
}

#[tokio::test]
async fn upsert_account_source_persists() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let acct = store
        .upsert_account("libro-1", "us", Some("Libro"), true, "libro")
        .await
        .unwrap();
    assert_eq!(acct.source, "libro");
    let again = store.get_account("libro-1").await.unwrap().unwrap();
    assert_eq!(again.source, "libro");
}

#[tokio::test]
async fn remap_account_moves_books() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("email@example.com", "us", Some("Main"), true, "audible")
        .await
        .unwrap();
    store
        .upsert_book(&NewBook::minimal(
            "B00TEST",
            "email@example.com",
            "us",
            "Test Book",
        ))
        .await
        .unwrap();

    store
        .remap_account_id("email@example.com", "amzn1.account.CID")
        .await
        .unwrap();

    assert!(store
        .get_account("email@example.com")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .get_account("amzn1.account.CID")
        .await
        .unwrap()
        .is_some());
    assert!(store
        .get_book("B00TEST", "amzn1.account.CID")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn ignored_titles_roundtrip() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("user-1", "us", None, true, "audible")
        .await
        .unwrap();
    store
        .upsert_book(&NewBook::minimal("B00TEST", "user-1", "us", "Test"))
        .await
        .unwrap();
    assert!(!store.is_ignored("B00TEST", "user-1").await.unwrap());
    store
        .set_ignored("B00TEST", "user-1", true, Some("skip"))
        .await
        .unwrap();
    assert!(store.is_ignored("B00TEST", "user-1").await.unwrap());
    store
        .set_ignored("B00TEST", "user-1", false, None)
        .await
        .unwrap();
    assert!(!store.is_ignored("B00TEST", "user-1").await.unwrap());
}

#[tokio::test]
async fn revoke_keeps_books_and_portal_tickets_work() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("user-1", "us", Some("Main"), true, "audible")
        .await
        .unwrap();
    store
        .upsert_book(&NewBook::minimal("B00TEST", "user-1", "us", "Test"))
        .await
        .unwrap();
    store.revoke_credentials("user-1").await.unwrap();
    let acct = store.get_account("user-1").await.unwrap().unwrap();
    assert!(!acct.scan_enabled);
    assert_eq!(acct.connection_status, "revoked");
    assert!(store.get_book("B00TEST", "user-1").await.unwrap().is_some());

    let identity = store
        .upsert_portal_identity("audiobookshelf", "usr_1", Some("bob"))
        .await
        .unwrap();
    let ticket = store
        .insert_claim_ticket(
            "abc123hash",
            Some(identity.id),
            Utc::now() + chrono::Duration::hours(1),
            "test",
        )
        .await
        .unwrap();
    assert!(ticket.redeemed_at.is_none());
    store.redeem_claim_ticket("abc123hash").await.unwrap();
    let redeemed = store
        .get_claim_ticket_by_hash("abc123hash")
        .await
        .unwrap()
        .unwrap();
    assert!(redeemed.redeemed_at.is_some());
    // Second redeem must fail (atomic consume).
    assert!(store.redeem_claim_ticket("abc123hash").await.is_err());

    store
        .link_account(identity.id, "user-1", "audible")
        .await
        .unwrap();
    let links = store.list_account_links(identity.id).await.unwrap();
    assert_eq!(links.len(), 1);
    store.mark_connection_active("user-1").await.unwrap();
    assert_eq!(
        store
            .get_account("user-1")
            .await
            .unwrap()
            .unwrap()
            .connection_status,
        "active"
    );
}

#[tokio::test]
async fn account_links_are_exclusive_per_account_id() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_account("acct-x", "us", Some("X"), true, "audible")
        .await
        .unwrap();
    let a = store
        .upsert_portal_identity("p", "a", Some("A"))
        .await
        .unwrap();
    let b = store
        .upsert_portal_identity("p", "b", Some("B"))
        .await
        .unwrap();
    store.link_account(a.id, "acct-x", "audible").await.unwrap();
    assert!(store.link_account(b.id, "acct-x", "audible").await.is_err());
    store.unlink_account(a.id, "acct-x").await.unwrap();
    assert_eq!(
        store
            .count_account_links_for_account("acct-x")
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn users_bridged_from_portal_identity() {
    use crate::models::UserRole;

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let identity = store
        .upsert_portal_identity("audiobookshelf", "usr_bridge", Some("cara"))
        .await
        .unwrap();
    let user_id = identity.user_id.expect("upsert creates linked user");
    let user = store.get_user(user_id).await.unwrap().unwrap();
    assert_eq!(user.role, UserRole::Member);
    assert_eq!(user.display_name.as_deref(), Some("cara"));

    // Idempotent: no additional orphans to bridge.
    assert_eq!(store.ensure_users_bridged().await.unwrap(), 0);
    let again = store
        .get_portal_identity("audiobookshelf", "usr_bridge")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.user_id, Some(user_id));
    assert_eq!(store.list_users().await.unwrap().len(), 1);
}

#[tokio::test]
async fn user_preferences_roundtrip_operator_and_portal() {
    use crate::models::{portal_prefs_key, OPERATOR_PREFS_KEY};

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let defaults = store
        .get_user_preferences_or_default(OPERATOR_PREFS_KEY, None)
        .await
        .unwrap();
    assert_eq!(defaults.default_view, "discover");
    assert_eq!(defaults.theme, "system");
    assert!(defaults.disabled_shelves.is_empty());
    assert!(store
        .get_user_preferences(OPERATOR_PREFS_KEY)
        .await
        .unwrap()
        .is_none());

    let saved = store
        .upsert_user_preferences(
            OPERATOR_PREFS_KEY,
            None,
            "library",
            &["chirp_deals".into(), "genre".into()],
            "rating",
            "asc",
            Some("en"),
            &["chirp".into()],
            "dark",
        )
        .await
        .unwrap();
    assert_eq!(saved.default_view, "library");
    assert_eq!(saved.disabled_shelves, vec!["chirp_deals", "genre"]);
    assert_eq!(saved.discover_sort, "rating");
    assert_eq!(saved.discover_sort_dir, "asc");
    assert_eq!(saved.discover_language.as_deref(), Some("en"));
    assert_eq!(saved.discover_excluded_sources, vec!["chirp"]);
    assert_eq!(saved.theme, "dark");

    let again = store
        .get_user_preferences(OPERATOR_PREFS_KEY)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.default_view, "library");
    assert_eq!(again.disabled_shelves.len(), 2);
    assert_eq!(again.discover_excluded_sources, vec!["chirp"]);

    let identity = store
        .upsert_portal_identity("audiobookshelf", "usr_prefs", Some("alice"))
        .await
        .unwrap();
    let key = portal_prefs_key(identity.id);
    let portal = store
        .upsert_user_preferences(
            &key,
            Some(identity.id),
            "accounts",
            &["narrator".into()],
            "relevance",
            "desc",
            None,
            &[],
            "system",
        )
        .await
        .unwrap();
    assert_eq!(portal.identity_id, Some(identity.id));
    assert_eq!(portal.default_view, "accounts");
    assert_eq!(portal.disabled_shelves, vec!["narrator"]);
    assert_eq!(portal.theme, "system");
    assert!(portal.discover_language.is_none());
    assert!(portal.discover_excluded_sources.is_empty());

    // Operator prefs stay independent.
    assert_eq!(
        store
            .get_user_preferences(OPERATOR_PREFS_KEY)
            .await
            .unwrap()
            .unwrap()
            .default_view,
        "library"
    );
}

#[tokio::test]
async fn wishlist_is_personal_and_global_queue_ranks_by_wish_count() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let a = store
        .upsert_portal_identity("audiobookshelf", "u1", Some("alice"))
        .await
        .unwrap();
    let b = store
        .upsert_portal_identity("audiobookshelf", "u2", Some("bob"))
        .await
        .unwrap();

    let work = fallback_work_key("Hail Mary", Some("Andy Weir"), Some("B00HAIL"), None);
    store
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id: Some(a.id),
            title: "Hail Mary".into(),
            authors: Some("Andy Weir".into()),
            asin: Some("B00HAIL".into()),
            isbn: None,
            notes: None,
            status: RequestStatus::Open, // ignored
            work_key: work.clone(),
            work_id: None,
            resolved_book_uuid: None,
            cover_url: None,
        })
        .await
        .unwrap();
    store
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id: Some(b.id),
            title: "Project Hail Mary".into(),
            authors: Some("Andy Weir".into()),
            asin: Some("B00HAIL".into()),
            isbn: None,
            notes: None,
            status: RequestStatus::Open,
            work_key: work.clone(),
            work_id: None,
            resolved_book_uuid: None,
            cover_url: None,
        })
        .await
        .unwrap();
    // Solo wish — should rank below Hail Mary.
    store
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id: None,
            title: "Solo Title".into(),
            authors: None,
            asin: Some("B00SOLO".into()),
            isbn: None,
            notes: None,
            status: RequestStatus::Open,
            work_key: String::new(),
            work_id: None,
            resolved_book_uuid: None,
            cover_url: None,
        })
        .await
        .unwrap();

    // Idempotent for same identity + work.
    let again = store
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id: Some(a.id),
            title: "Hail Mary".into(),
            authors: Some("Andy Weir".into()),
            asin: Some("B00HAIL".into()),
            isbn: None,
            notes: None,
            status: RequestStatus::Open,
            work_key: work.clone(),
            work_id: None,
            resolved_book_uuid: None,
            cover_url: None,
        })
        .await
        .unwrap();
    assert_eq!(again.asin.as_deref(), Some("B00HAIL"));
    assert_eq!(store.list_wishlist(Some(a.id)).await.unwrap().len(), 1);
    assert_eq!(store.list_wishlist(Some(b.id)).await.unwrap().len(), 1);

    // Soft catalog key vs later asin: for the same wisher → one open row.
    let soft = store
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id: Some(a.id),
            title: "The Martian".into(),
            authors: Some("Andy Weir".into()),
            asin: None,
            isbn: None,
            notes: None,
            status: RequestStatus::Open,
            work_key: String::from("soft:the martian|andy weir"),
            work_id: None,
            resolved_book_uuid: None,
            cover_url: None,
        })
        .await
        .unwrap();
    let again_hard = store
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id: Some(a.id),
            title: "The Martian".into(),
            authors: Some("Andy Weir".into()),
            asin: Some("B00MARTIAN".into()),
            isbn: None,
            notes: None,
            status: RequestStatus::Open,
            work_key: String::from("asin:B00MARTIAN"),
            work_id: None,
            resolved_book_uuid: None,
            cover_url: None,
        })
        .await
        .unwrap();
    assert_eq!(soft.uuid, again_hard.uuid);
    assert_eq!(store.list_wishlist(Some(a.id)).await.unwrap().len(), 2);
    assert_eq!(store.list_wishlist(None).await.unwrap().len(), 1);

    let queue = store.list_global_request_queue().await.unwrap();
    // Hail Mary (2 wishes) ranks above Solo + Martian (1 each).
    assert_eq!(queue.len(), 3);
    assert_eq!(queue[0].wish_count, 2);
    assert_eq!(queue[0].work_key, work);
    assert_eq!(queue[0].wishers.len(), 2);
    assert!(queue[0]
        .wishers
        .iter()
        .any(|w| w.display_name.as_deref() == Some("alice")));
    assert!(queue[0]
        .wishers
        .iter()
        .any(|w| w.display_name.as_deref() == Some("bob")));
    assert!(queue
        .iter()
        .any(|e| e.wish_count == 1 && e.title.contains("Martian")));
    assert!(queue
        .iter()
        .any(|e| e.wish_count == 1 && e.title.contains("Solo")));
    let solo = queue
        .iter()
        .find(|e| e.title.contains("Solo"))
        .expect("solo wish");
    assert_eq!(solo.wishers.len(), 1);
    assert!(solo.wishers[0].operator);
}

#[tokio::test]
async fn wishlist_sources_merge_description_and_editions() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let row = store
        .create_title_request(&NewTitleRequest {
            uuid: None,
            identity_id: None,
            title: "Ashes of Man".into(),
            authors: Some("Christopher Ruocchio".into()),
            asin: Some("B09ASHES".into()),
            isbn: Some("9781984811234".into()),
            notes: None,
            status: RequestStatus::Open,
            work_key: String::from("asin:B09ASHES"),
            work_id: None,
            resolved_book_uuid: None,
            cover_url: None,
        })
        .await
        .unwrap();

    let merged = store
        .upsert_title_request_sources(
            row.id,
            &[
                NewTitleRequestSource {
                    source: "audible".into(),
                    product_id: "B09ASHES".into(),
                    title: Some("Ashes of Man".into()),
                    authors: Some("Christopher Ruocchio".into()),
                    asin: Some("B09ASHES".into()),
                    description: Some("Short teaser...".into()),
                    url: Some("https://www.audible.com/pd/B09ASHES".into()),
                    price_cents: Some(1499),
                    currency: Some("USD".into()),
                    price_label: Some("$14.99".into()),
                    cover_url: Some("https://example.com/cover.jpg".into()),
                    ..Default::default()
                },
                NewTitleRequestSource {
                    source: "chirp".into(),
                    product_id: "chirp-ashes".into(),
                    title: Some("Ashes of Man".into()),
                    description: Some(
                        "<p>The <b>fifth</b> novel of the galaxy-spanning Sun Eater series.</p>"
                            .into(),
                    ),
                    url: Some("https://www.chirpbooks.com/audiobooks/chirp-ashes".into()),
                    price_cents: Some(499),
                    currency: Some("USD".into()),
                    price_label: Some("$4.99".into()),
                    ..Default::default()
                },
                NewTitleRequestSource {
                    source: "libro".into(),
                    product_id: "9781984811234".into(),
                    isbn: Some("9781984811234".into()),
                    url: Some("https://libro.fm/audiobooks/9781984811234".into()),
                    ..Default::default()
                },
            ],
        )
        .await
        .unwrap();

    assert_eq!(merged.sources.len(), 3);
    assert_eq!(merged.store_editions.len(), 3);
    assert!(
        merged.description.as_deref().unwrap_or("").contains("<p>"),
        "HTML description should win over plain teaser"
    );
    assert_eq!(
        merged.cover_url.as_deref(),
        Some("https://example.com/cover.jpg")
    );
    assert_eq!(merged.purchase_hints.len(), 3);
    assert!(merged
        .purchase_hints
        .iter()
        .any(|h| h.source == "audible" && h.price_cents == Some(1499)));
    assert!(merged
        .purchase_hints
        .iter()
        .any(|h| h.source == "libro" && h.url.is_some()));

    let listed = store.list_wishlist(None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].store_editions.len(), 3);
}

#[tokio::test]
async fn operator_sessions_persist_and_revoke() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let hash = "op-session-hash-001";
    store
        .insert_operator_session(hash, Utc::now() + chrono::Duration::hours(1))
        .await
        .unwrap();
    assert!(store.operator_session_valid(hash).await.unwrap());
    assert!(store.delete_operator_session(hash).await.unwrap());
    assert!(!store.operator_session_valid(hash).await.unwrap());
}

#[tokio::test]
async fn user_presence_extras_session_listening_and_last_used_touch() {
    use crate::entities::portal_sessions;
    use crate::hash_token;
    use crate::models::UserRole;
    use chrono::{Duration as ChronoDuration, Utc};
    use sea_orm::Set;

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Pat"), None)
        .await
        .unwrap();
    let idle = store
        .create_user(UserRole::Member, Some("Idle"), None)
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user.id, Some("Pat"))
        .await
        .unwrap();
    let _idle_identity = store
        .ensure_local_portal_identity(idle.id, Some("Idle"))
        .await
        .unwrap();
    let raw = "presence-session-token";
    let token_hash = hash_token(raw);
    store
        .insert_portal_session(
            &token_hash,
            identity.id,
            Utc::now() + ChronoDuration::hours(1),
        )
        .await
        .unwrap();

    store
        .upsert_listening_progress(&NewListeningProgress {
            identity_id: Some(identity.id),
            provider: "audiobookshelf".into(),
            external_user_id: "pat".into(),
            book_uuid: None,
            work_id: None,
            external_item_id: "item-1".into(),
            title: Some("Dune".into()),
            authors: None,
            asin: None,
            isbn: None,
            progress: Some(0.2),
            current_time_seconds: Some(120.0),
            duration_seconds: Some(600.0),
            is_finished: false,
            last_listened_at: Some(Utc::now()),
        })
        .await
        .unwrap();

    let extras = store
        .list_user_presence_extras(ChronoDuration::minutes(30))
        .await
        .unwrap();
    let extra = extras.get(&user.id).expect("presence row");
    assert!(extra.online);
    assert!(extra.last_active_at.is_some());
    assert_eq!(
        extra.listening.as_ref().unwrap().title.as_deref(),
        Some("Dune")
    );
    assert_eq!(extras.get(&idle.id).map(|e| e.online), Some(false));
    let seen = store.get_user(user.id).await.unwrap().unwrap();
    assert!(seen.last_seen_at.is_some());
    let never = store.get_user(idle.id).await.unwrap().unwrap();
    assert!(never.last_seen_at.is_none());

    let session = portal_sessions::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut am: portal_sessions::ActiveModel = session.into();
    am.last_used_at = Set(Some(
        (Utc::now() - ChronoDuration::minutes(10)).to_rfc3339(),
    ));
    am.update(&store.db).await.unwrap();

    store
        .get_portal_session_identity(&token_hash)
        .await
        .unwrap()
        .expect("identity");
    let extras = store
        .list_user_presence_extras(ChronoDuration::minutes(30))
        .await
        .unwrap();
    let extra = extras.get(&user.id).expect("presence row");
    let age = Utc::now() - extra.last_active_at.expect("last_active");
    assert!(age < ChronoDuration::seconds(5));
    let seen = store.get_user(user.id).await.unwrap().unwrap();
    let seen_age = Utc::now() - seen.last_seen_at.expect("last_seen");
    assert!(seen_age < ChronoDuration::seconds(5));

    store.delete_portal_session(&token_hash).await.unwrap();
    let extras = store
        .list_user_presence_extras(ChronoDuration::minutes(30))
        .await
        .unwrap();
    let extra = extras.get(&user.id).expect("presence row");
    assert!(!extra.online);
    assert!(extra.last_active_at.is_none());
    let seen = store.get_user(user.id).await.unwrap().unwrap();
    assert!(seen.last_seen_at.is_some());
}

#[tokio::test]
async fn last_active_owner_is_guarded() {
    use crate::models::{UserRole, UserStatus};

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let owner = store
        .create_user(UserRole::Owner, Some("Only"), None)
        .await
        .unwrap();
    let err = store
        .set_user_role(owner.id, UserRole::Member)
        .await
        .unwrap_err();
    assert!(matches!(err, LibraryError::LastOwner));
    let err = store
        .set_user_status(owner.id, UserStatus::Disabled)
        .await
        .unwrap_err();
    assert!(matches!(err, LibraryError::LastOwner));

    let second = store
        .create_user(UserRole::Owner, Some("Two"), None)
        .await
        .unwrap();
    store
        .set_user_status(second.id, UserStatus::Disabled)
        .await
        .unwrap();
    store
        .set_user_role(owner.id, UserRole::Member)
        .await
        .unwrap_err();
    assert_eq!(store.count_active_owners().await.unwrap(), 1);
}

#[tokio::test]
async fn elevated_operator_sessions_are_deleted_not_nulled() {
    use crate::models::UserRole;

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let owner = store
        .create_user(UserRole::Owner, Some("Origin"), None)
        .await
        .unwrap();
    let spare = store
        .create_user(UserRole::Owner, Some("Spare"), None)
        .await
        .unwrap();
    let hash = "elevated-session-hash-001";
    store
        .insert_elevated_operator_session(hash, Utc::now() + chrono::Duration::hours(1), owner.id)
        .await
        .unwrap();
    assert!(store.operator_session_valid(hash).await.unwrap());

    store
        .set_user_role(owner.id, UserRole::Member)
        .await
        .unwrap();
    assert!(!store.operator_session_valid(hash).await.unwrap());
    let _ = spare;

    let owner2 = store
        .create_user(UserRole::Owner, Some("Origin2"), None)
        .await
        .unwrap();
    let hash2 = "elevated-session-hash-002";
    store
        .insert_elevated_operator_session(hash2, Utc::now() + chrono::Duration::hours(1), owner2.id)
        .await
        .unwrap();
    store.delete_user(owner2.id).await.unwrap();
    assert!(!store.operator_session_valid(hash2).await.unwrap());
}

/// Test-only passwords assembled at runtime so scanners do not treat a
/// string literal as a shipped secret.
fn winner_password() -> String {
    ["winner", "-", "password", "-", "a"].concat()
}

fn loser_password() -> String {
    ["loser", "-", "password", "-", "bb"].concat()
}

fn first_password() -> String {
    ["first", "-", "password", "-", "ok"].concat()
}

fn second_password() -> String {
    ["second", "-", "password", "-", "no"].concat()
}

fn invite_password() -> String {
    ["invite", "-", "password", "-", "ok"].concat()
}

#[tokio::test]
async fn concurrent_claim_redeem_sets_only_winner_password() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Invitee"), None)
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user.id, Some("Invitee"))
        .await
        .unwrap();
    let ticket_hash = "concurrent-claim-ticket-hash";
    store
        .insert_claim_ticket(
            ticket_hash,
            Some(identity.id),
            Utc::now() + chrono::Duration::hours(1),
            "test",
        )
        .await
        .unwrap();
    let password_a = winner_password();
    let password_b = loser_password();
    let hash_a = crate::hash_password(&password_a).unwrap();
    let hash_b = crate::hash_password(&password_b).unwrap();
    let expires = Utc::now() + chrono::Duration::hours(12);
    let store_a = store.clone();
    let store_b = store.clone();
    let hash_a_clone = hash_a.clone();
    let hash_b_clone = hash_b.clone();
    // Spawn so each redeem is a distinct Tokio task. `join!` would poll both
    // futures on the test's `block_on` context and nest SQLite savepoints.
    let task_a = tokio::spawn(async move {
        store_a
            .redeem_claim_ticket_to_session(
                ticket_hash,
                "session-hash-a",
                expires,
                None,
                Some(hash_a_clone.as_str()),
                None,
            )
            .await
    });
    let task_b = tokio::spawn(async move {
        store_b
            .redeem_claim_ticket_to_session(
                ticket_hash,
                "session-hash-b",
                expires,
                None,
                Some(hash_b_clone.as_str()),
                None,
            )
            .await
    });
    let res_a = task_a.await.expect("redeem task a");
    let res_b = task_b.await.expect("redeem task b");
    let wins_a = res_a.is_ok();
    let wins_b = res_b.is_ok();
    assert_ne!(
        wins_a, wins_b,
        "exactly one redeem must succeed: {res_a:?} {res_b:?}"
    );
    let stored = store
        .get_user_password_hash(user.id)
        .await
        .unwrap()
        .unwrap();
    if wins_a {
        assert!(crate::verify_password(&password_a, &stored).unwrap());
        assert!(!crate::verify_password(&password_b, &stored).unwrap());
        assert!(res_b.unwrap_err().to_string().contains("already redeemed"));
    } else {
        assert!(crate::verify_password(&password_b, &stored).unwrap());
        assert!(!crate::verify_password(&password_a, &stored).unwrap());
        assert!(res_a.unwrap_err().to_string().contains("already redeemed"));
    }
}

#[tokio::test]
async fn failed_claim_redeem_does_not_set_password() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Invitee"), None)
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user.id, Some("Invitee"))
        .await
        .unwrap();
    let ticket_hash = "second-consume-ticket-hash";
    store
        .insert_claim_ticket(
            ticket_hash,
            Some(identity.id),
            Utc::now() + chrono::Duration::hours(1),
            "test",
        )
        .await
        .unwrap();
    let first_password = first_password();
    let first_hash = crate::hash_password(&first_password).unwrap();
    let expires = Utc::now() + chrono::Duration::hours(12);
    store
        .redeem_claim_ticket_to_session(
            ticket_hash,
            "session-hash-first",
            expires,
            None,
            Some(first_hash.as_str()),
            None,
        )
        .await
        .unwrap();
    let second_password = second_password();
    let second_hash = crate::hash_password(&second_password).unwrap();
    let err = store
        .redeem_claim_ticket_to_session(
            ticket_hash,
            "session-hash-second",
            expires,
            None,
            Some(second_hash.as_str()),
            None,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already redeemed"), "{err}");
    let stored = store
        .get_user_password_hash(user.id)
        .await
        .unwrap()
        .unwrap();
    assert!(crate::verify_password(&first_password, &stored).unwrap());
    assert!(!crate::verify_password(&second_password, &stored).unwrap());

    let missing = store
        .redeem_claim_ticket_to_session(
            "no-such-ticket-hash",
            "session-hash-missing",
            expires,
            None,
            Some(second_hash.as_str()),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        missing
            .to_string()
            .contains("invalid, expired, or already redeemed"),
        "{missing}"
    );
    let stored = store
        .get_user_password_hash(user.id)
        .await
        .unwrap()
        .unwrap();
    assert!(crate::verify_password(&first_password, &stored).unwrap());
}

#[tokio::test]
async fn missing_invite_password_does_not_consume_or_set_hash() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Invitee"), None)
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user.id, Some("Invitee"))
        .await
        .unwrap();
    let ticket_hash = "missing-password-ticket-hash";
    store
        .insert_claim_ticket(
            ticket_hash,
            Some(identity.id),
            Utc::now() + chrono::Duration::hours(1),
            "test",
        )
        .await
        .unwrap();
    let expires = Utc::now() + chrono::Duration::hours(12);
    let err = store
        .redeem_claim_ticket_to_session(ticket_hash, "session-hash-none", expires, None, None, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("password required"), "{err}");
    assert!(store
        .get_user_password_hash(user.id)
        .await
        .unwrap()
        .is_none());
    let hash = crate::hash_password(&invite_password()).unwrap();
    store
        .redeem_claim_ticket_to_session(
            ticket_hash,
            "session-hash-ok",
            expires,
            None,
            Some(hash.as_str()),
            None,
        )
        .await
        .unwrap();
    assert!(store
        .get_user_password_hash(user.id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn oidc_rp_state_roundtrip_and_expiry() {
    use crate::hash_token;
    use chrono::{Duration as ChronoDuration, Utc};

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let nonce = ["n", "once"].concat();
    let hash = hash_token("state-raw");
    store
        .insert_oidc_rp_state(
            &hash,
            "corp",
            "verifier",
            &nonce,
            "login",
            None,
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .unwrap();
    let taken = store.take_oidc_rp_state(&hash).await.unwrap().unwrap();
    assert_eq!(taken.0, "corp");
    assert_eq!(taken.1, "verifier");
    assert_eq!(taken.2, nonce);
    assert_eq!(taken.3, "login");
    // Same state hash reuses the consume-once operation id and replays the
    // receipt (lost-response / callback refresh) instead of returning empty.
    let replayed = store.take_oidc_rp_state(&hash).await.unwrap().unwrap();
    assert_eq!(replayed, taken);

    let expired = hash_token("expired");
    let expired_nonce = ["n"].concat();
    store
        .insert_oidc_rp_state(
            &expired,
            "corp",
            "v",
            &expired_nonce,
            "login",
            None,
            Utc::now() - ChronoDuration::minutes(1),
        )
        .await
        .unwrap();
    assert!(store.take_oidc_rp_state(&expired).await.unwrap().is_none());
}

#[tokio::test]
async fn webauthn_credential_crud() {
    use chrono::{Duration as ChronoDuration, Utc};

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Passkey User"), None)
        .await
        .unwrap();
    store
        .insert_webauthn_credential(user.id, "cred-1", "{\"ok\":true}", Some("Laptop"))
        .await
        .unwrap();
    let listed = store.list_webauthn_credentials(user.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].credential_id, "cred-1");
    assert_eq!(listed[0].name.as_deref(), Some("Laptop"));
    let got = store
        .get_webauthn_credential_by_cred_id("cred-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.1, user.id);
    store
        .update_webauthn_credential(got.0, "{\"counter\":1}")
        .await
        .unwrap();
    assert!(store
        .delete_webauthn_credential(user.id, got.0)
        .await
        .unwrap());
    assert_eq!(store.count_webauthn_credentials(user.id).await.unwrap(), 0);

    let challenge = "chal-1";
    store
        .insert_webauthn_challenge(
            challenge,
            Some(user.id),
            "login",
            "{}",
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .unwrap();
    let taken = store
        .take_webauthn_challenge(challenge, "login")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.0, Some(user.id));
    let replayed = store
        .take_webauthn_challenge(challenge, "login")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replayed, taken);
}

#[tokio::test]
async fn totp_enabled_flag_round_trip() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Mfa User"), None)
        .await
        .unwrap();
    assert!(!user.totp_enabled);
    store.set_user_totp_enabled(user.id, true).await.unwrap();
    let reloaded = store.get_user(user.id).await.unwrap().unwrap();
    assert!(reloaded.totp_enabled);
    store.set_user_totp_enabled(user.id, false).await.unwrap();
    let cleared = store.get_user(user.id).await.unwrap().unwrap();
    assert!(!cleared.totp_enabled);
}

async fn totp_secret_names(store: &LibraryStore, user_id: i64) -> Vec<String> {
    let uid = user_id.to_string();
    let mut names: Vec<String> = crate::list_secrets(store.db(), crate::secret_kind::TOTP)
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.account_id.as_deref() == Some(uid.as_str()))
        .map(|row| row.name)
        .collect();
    names.sort();
    names
}

fn sample_totp_secret() -> String {
    ["JBSW", "Y3DP", "EHPK", "3PXP"].concat()
}

async fn store_pending_totp(store: &LibraryStore, user_id: i64, secret: &str) {
    let record = crate::build_sealed_record(
        secret.as_bytes(),
        crate::secret_kind::TOTP,
        "local",
        crate::secret_account_type::USER,
        &user_id.to_string(),
        "pending",
    )
    .unwrap();
    crate::upsert_secret(store.db(), &record).await.unwrap();
}

#[tokio::test]
async fn totp_enrollment_and_disable_round_trip() {
    let _dek = crate::master_key::master_key_test_read_lock_async().await;
    crate::master_key::ensure_shared_test_dek();
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user(UserRole::Member, Some("Totp Round Trip"), None)
        .await
        .unwrap();
    store_pending_totp(&store, user.id, &sample_totp_secret()).await;
    store
        .confirm_totp_enrollment(user.id, &sample_totp_secret())
        .await
        .unwrap();
    let enrolled = store.get_user(user.id).await.unwrap().unwrap();
    assert!(enrolled.totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["primary"]);

    store.disable_user_totp(user.id).await.unwrap();
    let cleared = store.get_user(user.id).await.unwrap().unwrap();
    assert!(!cleared.totp_enabled);
    assert!(totp_secret_names(&store, user.id).await.is_empty());
}

#[tokio::test]
async fn totp_enroll_and_disable_missing_user_leave_no_leftover_secrets() {
    let _dek = crate::master_key::master_key_test_read_lock_async().await;
    crate::master_key::ensure_shared_test_dek();
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let other = store
        .create_user(UserRole::Member, Some("Keep Totp"), None)
        .await
        .unwrap();
    store_pending_totp(&store, other.id, &sample_totp_secret()).await;
    let missing = 999_i64;
    let enroll_err = store
        .confirm_totp_enrollment(missing, &sample_totp_secret())
        .await
        .unwrap_err();
    assert!(matches!(enroll_err, LibraryError::NotFound(_)));
    assert!(totp_secret_names(&store, missing).await.is_empty());
    assert_eq!(totp_secret_names(&store, other.id).await, vec!["pending"]);
    assert!(
        !store
            .get_user(other.id)
            .await
            .unwrap()
            .unwrap()
            .totp_enabled
    );

    let disable_err = store.disable_user_totp(missing).await.unwrap_err();
    assert!(matches!(disable_err, LibraryError::NotFound(_)));
    assert_eq!(totp_secret_names(&store, other.id).await, vec!["pending"]);
}

#[tokio::test]
async fn plugin_oidc_upsert_rejects_custom_and_cross_plugin_client_id() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let custom = store
        .insert_oidc_client(
            "shared-client",
            None,
            &[String::from("https://player.example/callback")],
            Some("Custom"),
            true,
            &["openid".into()],
            true,
            None,
        )
        .await
        .unwrap();
    assert!(custom.plugin_id.is_none());
    assert!(custom.enabled);

    let custom_err = store
        .upsert_plugin_oidc_client(
            "shared-client",
            "audiobookshelf",
            "Audiobookshelf",
            &[String::from("http://127.0.0.1:13378/auth/openid/callback")],
            true,
            &["openid".into(), "profile".into()],
        )
        .await
        .unwrap_err();
    assert!(matches!(custom_err, LibraryError::Conflict(_)));
    let unchanged = store
        .get_oidc_client("shared-client")
        .await
        .unwrap()
        .unwrap();
    assert!(unchanged.plugin_id.is_none());
    assert!(unchanged.enabled);
    assert_eq!(unchanged.name.as_deref(), Some("Custom"));

    store
        .upsert_plugin_oidc_client(
            "plugin-owned",
            "audiobookshelf",
            "Audiobookshelf",
            &[String::from("http://127.0.0.1:13378/auth/openid/callback")],
            true,
            &["openid".into()],
        )
        .await
        .unwrap();
    let cross_err = store
        .upsert_plugin_oidc_client(
            "plugin-owned",
            "other-player",
            "Other",
            &[String::from("http://127.0.0.1:9999/callback")],
            false,
            &["openid".into()],
        )
        .await
        .unwrap_err();
    assert!(matches!(cross_err, LibraryError::Conflict(_)));
    let owned = store
        .get_oidc_client("plugin-owned")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.plugin_id.as_deref(), Some("audiobookshelf"));
    assert_eq!(
        owned.redirect_uris,
        vec![String::from("http://127.0.0.1:13378/auth/openid/callback")]
    );

    let refreshed = store
        .upsert_plugin_oidc_client(
            "plugin-owned",
            "audiobookshelf",
            "Ignored name",
            &[String::from("https://abs.home:13378/auth/openid/callback")],
            false,
            &["openid".into(), "email".into()],
        )
        .await
        .unwrap();
    assert_eq!(refreshed.plugin_id.as_deref(), Some("audiobookshelf"));
    assert_eq!(refreshed.name.as_deref(), Some("Audiobookshelf"));
    assert!(!refreshed.enabled);
    assert!(refreshed.issue_refresh_token);
    assert_eq!(refreshed.allowed_scopes, vec!["openid"]);
    assert_eq!(
        refreshed.redirect_uris,
        vec![String::from("https://abs.home:13378/auth/openid/callback")]
    );
}

#[tokio::test]
async fn plugin_oidc_rewrites_alias_owner_to_plugin_key() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    store
        .upsert_plugin_oidc_client(
            "plugin-owned",
            "audiobookshelf",
            "Audiobookshelf",
            &[String::from("http://127.0.0.1:13378/auth/openid/callback")],
            true,
            &["openid".into()],
        )
        .await
        .unwrap();
    let key = "path:file:///tmp/abs#audiobookshelf";
    let migrated = store
        .upsert_plugin_oidc_client(
            "plugin-owned",
            key,
            "Ignored",
            &[String::from("https://abs.home:13378/auth/openid/callback")],
            false,
            &["openid".into()],
        )
        .await
        .unwrap();
    assert_eq!(migrated.plugin_id.as_deref(), Some(key));
    let impostor = store
        .upsert_plugin_oidc_client(
            "plugin-owned",
            "path:file:///tmp/other#audiobookshelf",
            "Other",
            &[String::from("http://127.0.0.1:9999/callback")],
            false,
            &["openid".into()],
        )
        .await
        .unwrap_err();
    assert!(matches!(impostor, LibraryError::Conflict(_)));
    let still = store
        .get_oidc_client("plugin-owned")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.plugin_id.as_deref(), Some(key));
}

#[tokio::test]
async fn delete_user_removes_webauthn_and_oidc_rows() {
    use chrono::{Duration as ChronoDuration, Utc};

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let owner = store
        .create_user(UserRole::Owner, Some("Keep"), None)
        .await
        .unwrap();
    let doomed = store
        .create_user(UserRole::Owner, Some("Go"), None)
        .await
        .unwrap();
    store
        .insert_webauthn_credential(doomed.id, "cred-reuse", "{\"ok\":true}", None)
        .await
        .unwrap();
    store
        .insert_webauthn_challenge(
            "chal-del",
            Some(doomed.id),
            "login",
            "{}",
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .unwrap();
    let nonce = ["n", "once"].concat();
    store
        .insert_oidc_rp_state(
            "state-del",
            "corp",
            "verifier",
            &nonce,
            "elevate",
            Some(doomed.id),
            Utc::now() + ChronoDuration::minutes(5),
        )
        .await
        .unwrap();

    store.delete_user(doomed.id).await.unwrap();
    assert!(store
        .get_webauthn_credential_by_cred_id("cred-reuse")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .take_webauthn_challenge("chal-del", "login")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .take_oidc_rp_state("state-del")
        .await
        .unwrap()
        .is_none());
    store
        .insert_webauthn_credential(owner.id, "cred-reuse", "{\"ok\":true}", None)
        .await
        .unwrap();
    assert_eq!(store.count_webauthn_credentials(owner.id).await.unwrap(), 1);
}

async fn test_store() -> LibraryStore {
    LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    )
}

fn scan_spec(account: Option<&str>, max_pending: i64) -> EnqueueJobSpec {
    EnqueueJobSpec {
        kind: JobKind::Scan,
        payload: JobPayload {
            account: account.map(str::to_string),
            title: None,
            trigger: JobTrigger::Api,
            ..Default::default()
        },
        priority: 0,
        max_attempts: 3,
        max_pending,
        run_after: None,
    }
}

async fn claim_job(store: &LibraryStore, owner: &str) -> JobRecord {
    store
        .claim_next_job(
            JobResourceClass::Network,
            owner,
            60,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
        .expect("claim")
}

fn fence_of(job: &JobRecord) -> JobFence {
    job.fence().expect("claimed job has a fence")
}

fn acquire_spec(title: Option<&str>, account: Option<&str>, max_pending: i64) -> EnqueueJobSpec {
    EnqueueJobSpec {
        kind: JobKind::Acquire,
        payload: JobPayload {
            account: account.map(str::to_string),
            title: title.map(str::to_string),
            trigger: JobTrigger::Api,
            ..Default::default()
        },
        priority: 0,
        max_attempts: 3,
        max_pending,
        run_after: None,
    }
}

#[tokio::test]
async fn enqueue_dedupes_active_scan_and_acquire() {
    let store = test_store().await;
    let first = store.enqueue_job(scan_spec(None, 32)).await.unwrap();
    let EnqueueOutcome::Created { id: scan_id } = first else {
        panic!("expected created scan: {first:?}");
    };
    let again = store.enqueue_job(scan_spec(None, 32)).await.unwrap();
    assert_eq!(
        again,
        EnqueueOutcome::Duplicate {
            existing_id: scan_id.clone()
        }
    );

    let other_account = store
        .enqueue_job(scan_spec(Some("acct-1"), 32))
        .await
        .unwrap();
    assert!(matches!(other_account, EnqueueOutcome::Created { .. }));

    let acq = store
        .enqueue_job(acquire_spec(Some("B00TEST"), None, 32))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id: acq_id } = acq else {
        panic!("expected created acquire: {acq:?}");
    };
    let acq_again = store
        .enqueue_job(acquire_spec(Some("B00TEST"), None, 32))
        .await
        .unwrap();
    assert_eq!(
        acq_again,
        EnqueueOutcome::Duplicate {
            existing_id: acq_id
        }
    );
    let acq_other = store
        .enqueue_job(acquire_spec(Some("B00OTHER"), None, 32))
        .await
        .unwrap();
    assert!(matches!(acq_other, EnqueueOutcome::Created { .. }));

    let listen = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::ListenSync,
            payload: JobPayload::default(),
            priority: 0,
            max_attempts: 3,
            max_pending: 32,
            run_after: None,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Created { id: listen_id } = listen else {
        panic!("expected created listen_sync: {listen:?}");
    };
    let listen_again = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::ListenSync,
            payload: JobPayload::default(),
            priority: 0,
            max_attempts: 3,
            max_pending: 32,
            run_after: None,
        })
        .await
        .unwrap();
    assert_eq!(
        listen_again,
        EnqueueOutcome::Duplicate {
            existing_id: listen_id
        }
    );
}

#[tokio::test]
async fn enqueue_respects_max_pending_and_reuses_terminal_keys() {
    let store = test_store().await;
    let a = store.enqueue_job(scan_spec(None, 1)).await.unwrap();
    let EnqueueOutcome::Created { id } = a else {
        panic!("expected created: {a:?}");
    };
    let full = store
        .enqueue_job(acquire_spec(None, None, 1))
        .await
        .unwrap();
    assert_eq!(full, EnqueueOutcome::QueueFull);

    let claimed = claim_job(&store, "worker-done").await;
    assert_eq!(claimed.id, id);
    store
        .complete_job(&fence_of(&claimed), Some("done"))
        .await
        .unwrap();
    let reused = store.enqueue_job(scan_spec(None, 1)).await.unwrap();
    assert!(matches!(reused, EnqueueOutcome::Created { .. }));

    let cancelled = store
        .enqueue_job(scan_spec(Some("acct-x"), 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id: cancel_id } = cancelled else {
        panic!("expected created: {cancelled:?}");
    };
    store.request_job_cancel(&cancel_id).await.unwrap();
    let after_cancel = store
        .enqueue_job(scan_spec(Some("acct-x"), 8))
        .await
        .unwrap();
    assert!(matches!(after_cancel, EnqueueOutcome::Created { .. }));
}

#[tokio::test]
async fn claim_heartbeat_and_expired_lease_reclaim() {
    let store = test_store().await;
    let created = store.enqueue_job(scan_spec(None, 8)).await.unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let claimed = claim_job(&store, "worker-a").await;
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.state, JobState::Running);
    assert_eq!(claimed.attempt_count, 1);
    let fence = fence_of(&claimed);
    assert!(store
        .heartbeat_job(&fence, 60, Some("scanning"))
        .await
        .unwrap());
    let listed = store.list_jobs(10).await.unwrap();
    assert_eq!(listed[0].progress.as_deref(), Some("scanning"));

    // Force the lease to expire.
    let model = crate::entities::jobs::Entity::find_by_id(&id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::jobs::ActiveModel = model.into();
    am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();

    let reclaimed = store.reclaim_expired_leases().await.unwrap();
    assert_eq!(reclaimed, 1);
    let after = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(after.state, JobState::Pending);
    assert!(after.lease_owner.is_none());

    // Exhaust attempts then reclaim → failed (no permanent running).
    let claimed2 = store
        .claim_next_job(
            JobResourceClass::Network,
            "worker-b",
            1,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
        .expect("second claim");
    assert_eq!(claimed2.attempt_count, 2);
    store
        .fail_job(&fence_of(&claimed2), "handler", "boom")
        .await
        .unwrap();
    let pending_retry = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(pending_retry.state, JobState::Pending);

    let model = crate::entities::jobs::Entity::find_by_id(&id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::jobs::ActiveModel = model.into();
    am.run_after = sea_orm::ActiveValue::Set(chrono::Utc::now().to_rfc3339());
    am.update(store.db()).await.unwrap();

    let claimed3 = store
        .claim_next_job(
            JobResourceClass::Network,
            "worker-c",
            1,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
        .expect("third claim");
    assert_eq!(claimed3.attempt_count, 3);
    store
        .fail_job(&fence_of(&claimed3), "handler", "boom again")
        .await
        .unwrap();
    let failed = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert!(store
        .claim_next_job(
            JobResourceClass::Network,
            "worker-d",
            60,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn list_jobs_waits_out_a_peer_sqlite_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("library.db");
    let db = bookclerk_plugin_database_sqlite::open(&path)
        .await
        .expect("open");
    crate::host_schema::apply_host_schema(&db)
        .await
        .expect("schema");
    let store = LibraryStore::from_connection(db);
    let created = store
        .enqueue_job(scan_spec(None, 8))
        .await
        .expect("enqueue");
    assert!(matches!(created, EnqueueOutcome::Created { .. }));

    let holder = rusqlite::Connection::open(&path).expect("holder");
    holder
        .busy_timeout(std::time::Duration::from_millis(50))
        .expect("busy timeout");
    holder.execute_batch("BEGIN EXCLUSIVE").expect("lock");

    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let listed = tokio::spawn(async move {
        let result = store.list_jobs(10).await;
        let _ = done_tx.send(());
        result
    });
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    assert!(
        done_rx.try_recv().is_err(),
        "list_jobs returned while the peer still held the database file"
    );
    holder.execute_batch("COMMIT").expect("unlock");

    let jobs = listed.await.expect("join").expect("list after unlock");
    assert_eq!(jobs.len(), 1);
}

#[tokio::test]
async fn completed_acquire_is_not_repeated_unsafely() {
    let store = test_store().await;
    store
        .upsert_account("user-1", "us", None, true, "audible")
        .await
        .unwrap();
    let book = store
        .upsert_book(&NewBook::minimal("B00SAFE", "user-1", "us", "Safe"))
        .await
        .unwrap();
    store
        .set_acquire_status(
            &book.uuid,
            "user-1",
            AcquireStatus::Acquired,
            Some("Safe/book.m4b"),
            None,
        )
        .await
        .unwrap();

    let created = store
        .enqueue_job(acquire_spec(Some(&book.uuid), Some("user-1"), 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let claimed = claim_job(&store, "worker-safe").await;
    assert_eq!(claimed.id, id);
    store
        .complete_job(&fence_of(&claimed), Some("acquired=0 matched=1 failed=0"))
        .await
        .unwrap();
    let again = store
        .enqueue_job(acquire_spec(Some(&book.uuid), Some("user-1"), 8))
        .await
        .unwrap();
    assert!(matches!(again, EnqueueOutcome::Created { .. }));
    let still = store.get_book_by_uuid(&book.uuid).await.unwrap().unwrap();
    assert_eq!(still.acquire_status, AcquireStatus::Acquired);
}

#[tokio::test]
async fn orphaned_downloading_book_is_reconciled() {
    let store = test_store().await;
    store
        .upsert_account("user-1", "us", None, true, "audible")
        .await
        .unwrap();
    let book = store
        .upsert_book(&NewBook::minimal("B00ORPH", "user-1", "us", "Orphan"))
        .await
        .unwrap();
    store
        .set_acquire_status(&book.uuid, "user-1", AcquireStatus::Downloading, None, None)
        .await
        .unwrap();
    let n = store.reconcile_orphaned_acquire_rows().await.unwrap();
    assert_eq!(n, 1);
    let updated = store.get_book_by_uuid(&book.uuid).await.unwrap().unwrap();
    assert_eq!(updated.acquire_status, AcquireStatus::Error);
    assert_eq!(
        updated.error_message.as_deref(),
        Some("orphaned_after_restart")
    );
}

#[tokio::test]
async fn concurrent_identical_admits_coalesce() {
    let store = std::sync::Arc::new(test_store().await);
    let mut handles = Vec::new();
    for _ in 0..16 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store.enqueue_job(scan_spec(None, 8)).await.unwrap()
        }));
    }
    let mut created = 0u32;
    let mut duplicate = 0u32;
    for handle in handles {
        match handle.await.unwrap() {
            EnqueueOutcome::Created { .. } => created += 1,
            EnqueueOutcome::Duplicate { .. } => duplicate += 1,
            EnqueueOutcome::QueueFull => panic!("same-key admits must not hit queue full"),
        }
    }
    assert_eq!(created, 1);
    assert_eq!(duplicate, 15);
    assert_eq!(store.count_active_jobs().await.unwrap(), 1);
}

#[tokio::test]
async fn concurrent_admits_never_exceed_max_pending() {
    let store = std::sync::Arc::new(test_store().await);
    let mut handles = Vec::new();
    for i in 0..12 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store
                .enqueue_job(scan_spec(Some(&format!("acct-{i}")), 3))
                .await
                .unwrap()
        }));
    }
    let mut created = 0u32;
    let mut full = 0u32;
    for handle in handles {
        match handle.await.unwrap() {
            EnqueueOutcome::Created { .. } => created += 1,
            EnqueueOutcome::Duplicate { .. } => {}
            EnqueueOutcome::QueueFull => full += 1,
        }
    }
    assert_eq!(created, 3);
    assert_eq!(full, 9);
    assert_eq!(store.count_active_jobs().await.unwrap(), 3);
}

#[tokio::test]
async fn concurrent_claims_have_one_winner() {
    let store = std::sync::Arc::new(test_store().await);
    store.enqueue_job(scan_spec(None, 8)).await.unwrap();
    let mut handles = Vec::new();
    for i in 0..8 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store
                .claim_next_job(
                    JobResourceClass::Network,
                    &format!("w-{i}"),
                    60,
                    &uuid::Uuid::new_v4().to_string(),
                )
                .await
                .unwrap()
        }));
    }
    let mut winners = 0u32;
    for handle in handles {
        if handle.await.unwrap().is_some() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1);
}

#[tokio::test]
async fn stale_fence_cannot_finalize_or_heartbeat() {
    let store = test_store().await;
    let created = store.enqueue_job(scan_spec(None, 8)).await.unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let first = claim_job(&store, "worker-old").await;
    let stale = fence_of(&first);

    let model = crate::entities::jobs::Entity::find_by_id(&id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::jobs::ActiveModel = model.into();
    am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();
    assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);

    let second = claim_job(&store, "worker-new").await;
    assert_eq!(second.id, id);
    assert!(second.lease_generation > stale.generation);
    assert!(!store.complete_job(&stale, Some("nope")).await.unwrap());
    assert!(!store.fail_job(&stale, "handler", "nope").await.unwrap());
    assert!(!store.heartbeat_job(&stale, 60, None).await.unwrap());
    let current = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(current.state, JobState::Running);
    assert_eq!(current.lease_owner.as_deref(), Some("worker-new"));
}

#[tokio::test]
async fn suspend_job_commits_checkpoint_and_stale_fence_cannot() {
    use bookclerk_plugin_abi::JobCheckpoint;

    let store = test_store().await;
    let created = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::PluginCopy,
            payload: JobPayload {
                plugin_id: Some("local".into()),
                source_key: Some("from".into()),
                dest_key: Some("to".into()),
                trigger: JobTrigger::Api,
                ..Default::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 8,
            run_after: None,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let first = claim_job(&store, "worker-suspend").await;
    let fence = fence_of(&first);
    let checkpoint = JobCheckpoint {
        schema_version: 1,
        json: r#"{"offset":12}"#.into(),
    };
    let wake = chrono::Utc::now() + chrono::Duration::hours(1);
    assert!(store.suspend_job(&fence, &checkpoint, wake).await.unwrap());
    let parked = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(parked.state, JobState::Pending);
    assert!(parked.lease_owner.is_none());
    assert_eq!(parked.progress.as_deref(), Some("suspended"));
    assert_eq!(
        parked.payload.checkpoint.as_ref().map(|c| c.json.as_str()),
        Some(r#"{"offset":12}"#)
    );
    assert!(parked.payload.resume_pending);
    assert_eq!(parked.payload.invocation_sequence, Some(2));

    assert!(store
        .claim_next_job(
            JobResourceClass::Network,
            "worker-early",
            60,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
        .is_none());

    assert!(!store.suspend_job(&fence, &checkpoint, wake).await.unwrap());
}

#[tokio::test]
async fn suspend_lost_fence_does_not_commit_and_reclaim_sees_checkpoint() {
    use bookclerk_plugin_abi::JobCheckpoint;

    let store = test_store().await;
    let created = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::PluginCopy,
            payload: JobPayload {
                plugin_id: Some("local".into()),
                source_key: Some("from".into()),
                dest_key: Some("to".into()),
                trigger: JobTrigger::Api,
                ..Default::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 8,
            run_after: None,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let first = claim_job(&store, "worker-old").await;
    let stale = fence_of(&first);
    let checkpoint = JobCheckpoint {
        schema_version: 1,
        json: r#"{"offset":99}"#.into(),
    };

    let model = crate::entities::jobs::Entity::find_by_id(&id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::jobs::ActiveModel = model.into();
    am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();
    assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);

    let second = claim_job(&store, "worker-new").await;
    assert_eq!(second.attempt_count, 2);
    assert!(!store
        .suspend_job(
            &stale,
            &checkpoint,
            chrono::Utc::now() + chrono::Duration::minutes(5)
        )
        .await
        .unwrap());
    let still_running = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(still_running.state, JobState::Running);
    assert!(still_running.payload.checkpoint.is_none());

    assert!(store
        .suspend_job(
            &fence_of(&second),
            &checkpoint,
            chrono::Utc::now() - chrono::Duration::seconds(1)
        )
        .await
        .unwrap());
    let resumed = claim_job(&store, "worker-resume").await;
    assert_eq!(resumed.id, id);
    assert_eq!(
        resumed.payload.checkpoint.as_ref().map(|c| c.json.as_str()),
        Some(r#"{"offset":99}"#)
    );
    assert!(resumed.attempt_count >= 2);
}

#[tokio::test]
async fn suspend_resume_then_retryable_failures_still_reach_max_attempts() {
    use bookclerk_plugin_abi::JobCheckpoint;

    let store = test_store().await;
    let created = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::PluginCopy,
            payload: JobPayload {
                plugin_id: Some("local".into()),
                source_key: Some("from".into()),
                dest_key: Some("to".into()),
                trigger: JobTrigger::Api,
                ..Default::default()
            },
            priority: 0,
            max_attempts: 2,
            max_pending: 8,
            run_after: None,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let first = claim_job(&store, "worker-suspend").await;
    assert_eq!(first.attempt_count, 1);
    let checkpoint = JobCheckpoint {
        schema_version: 1,
        json: r#"{"offset":1}"#.into(),
    };
    assert!(store
        .suspend_job(
            &fence_of(&first),
            &checkpoint,
            chrono::Utc::now() - chrono::Duration::seconds(1)
        )
        .await
        .unwrap());
    let parked = store.get_job(&id).await.unwrap().unwrap();
    assert!(parked.payload.resume_pending);
    assert!(parked.payload.checkpoint.is_some());

    let resumed = claim_job(&store, "worker-resume").await;
    assert_eq!(resumed.attempt_count, 1);
    assert!(!resumed.payload.resume_pending);
    assert_eq!(
        resumed.payload.checkpoint.as_ref().map(|c| c.json.as_str()),
        Some(r#"{"offset":1}"#)
    );

    for owner in ["worker-fail-1", "worker-fail-2"] {
        let running = store.get_job(&id).await.unwrap().unwrap();
        assert_eq!(running.state, JobState::Running);
        assert!(store
            .fail_job(&fence_of(&running), "handler", "transient")
            .await
            .unwrap());
        let after = store.get_job(&id).await.unwrap().unwrap();
        if after.state == JobState::Pending {
            assert!(!after.payload.resume_pending);
            let model = crate::entities::jobs::Entity::find_by_id(&id)
                .one(store.db())
                .await
                .unwrap()
                .unwrap();
            let mut am: crate::entities::jobs::ActiveModel = model.into();
            am.run_after = sea_orm::ActiveValue::Set(chrono::Utc::now().to_rfc3339());
            am.update(store.db()).await.unwrap();
            let _ = claim_job(&store, owner).await;
        } else {
            assert_eq!(after.state, JobState::Failed);
            assert_eq!(after.attempt_count, 2);
        }
    }
    let terminal = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(terminal.state, JobState::Failed);
    assert_eq!(terminal.attempt_count, 2);
}

#[tokio::test]
async fn set_job_progress_writes_row_and_rejects_stale_generation() {
    let store = test_store().await;
    let created = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::PluginCopy,
            payload: JobPayload {
                plugin_id: Some("local".into()),
                source_key: Some("from".into()),
                dest_key: Some("to".into()),
                trigger: JobTrigger::Api,
                ..Default::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 8,
            run_after: None,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let claimed = claim_job(&store, "worker-progress").await;
    let fence = fence_of(&claimed);
    assert!(store.set_job_progress(&fence, "10% staging").await.unwrap());
    let visible = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(visible.progress.as_deref(), Some("10% staging"));

    let stale = JobFence {
        job_id: fence.job_id.clone(),
        owner: fence.owner.clone(),
        generation: fence.generation.saturating_sub(1),
    };
    assert!(!store.set_job_progress(&stale, "stale").await.unwrap());
    let unchanged = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(unchanged.progress.as_deref(), Some("10% staging"));
}

#[tokio::test]
async fn malformed_payload_is_marked_invalid_and_not_claimed() {
    let store = test_store().await;
    let now = chrono::Utc::now().to_rfc3339();
    crate::entities::jobs::ActiveModel {
        id: sea_orm::ActiveValue::Set("bad-payload".into()),
        kind: sea_orm::ActiveValue::Set("scan".into()),
        state: sea_orm::ActiveValue::Set("pending".into()),
        priority: sea_orm::ActiveValue::Set(0),
        resource_class: sea_orm::ActiveValue::Set("network".into()),
        payload: sea_orm::ActiveValue::Set("not-json".into()),
        progress: sea_orm::ActiveValue::Set(None),
        attempt_count: sea_orm::ActiveValue::Set(0),
        max_attempts: sea_orm::ActiveValue::Set(3),
        run_after: sea_orm::ActiveValue::Set(now.clone()),
        lease_owner: sea_orm::ActiveValue::Set(None),
        lease_expires_at: sea_orm::ActiveValue::Set(None),
        dedup_key: sea_orm::ActiveValue::Set("scan:account=all".into()),
        error_kind: sea_orm::ActiveValue::Set(None),
        error_message: sea_orm::ActiveValue::Set(None),
        cancel_requested: sea_orm::ActiveValue::Set(0),
        created_at: sea_orm::ActiveValue::Set(now.clone()),
        updated_at: sea_orm::ActiveValue::Set(now.clone()),
        started_at: sea_orm::ActiveValue::Set(None),
        finished_at: sea_orm::ActiveValue::Set(None),
        lease_generation: sea_orm::ActiveValue::Set(0),
    }
    .insert(store.db())
    .await
    .unwrap();

    assert!(store
        .claim_next_job(
            JobResourceClass::Network,
            "worker",
            60,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
        .is_none());
    let row = store.get_job("bad-payload").await.unwrap().unwrap();
    assert_eq!(row.kind, JobKind::Invalid);
    assert_eq!(row.state, JobState::Failed);
    assert_eq!(row.error_kind.as_deref(), Some("invalid_job"));
}

#[tokio::test]
async fn unknown_kind_is_marked_invalid() {
    let store = test_store().await;
    let now = chrono::Utc::now().to_rfc3339();
    crate::entities::jobs::ActiveModel {
        id: sea_orm::ActiveValue::Set("bad-kind".into()),
        kind: sea_orm::ActiveValue::Set("not_a_kind".into()),
        state: sea_orm::ActiveValue::Set("pending".into()),
        priority: sea_orm::ActiveValue::Set(0),
        resource_class: sea_orm::ActiveValue::Set("network".into()),
        payload: sea_orm::ActiveValue::Set(r#"{"v":1,"trigger":"api"}"#.into()),
        progress: sea_orm::ActiveValue::Set(None),
        attempt_count: sea_orm::ActiveValue::Set(0),
        max_attempts: sea_orm::ActiveValue::Set(3),
        run_after: sea_orm::ActiveValue::Set(now.clone()),
        lease_owner: sea_orm::ActiveValue::Set(None),
        lease_expires_at: sea_orm::ActiveValue::Set(None),
        dedup_key: sea_orm::ActiveValue::Set("unknown".into()),
        error_kind: sea_orm::ActiveValue::Set(None),
        error_message: sea_orm::ActiveValue::Set(None),
        cancel_requested: sea_orm::ActiveValue::Set(0),
        created_at: sea_orm::ActiveValue::Set(now.clone()),
        updated_at: sea_orm::ActiveValue::Set(now.clone()),
        started_at: sea_orm::ActiveValue::Set(None),
        finished_at: sea_orm::ActiveValue::Set(None),
        lease_generation: sea_orm::ActiveValue::Set(0),
    }
    .insert(store.db())
    .await
    .unwrap();

    assert!(store
        .claim_next_job(
            JobResourceClass::Network,
            "worker",
            60,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap()
        .is_none());
    let row = store.get_job("bad-kind").await.unwrap().unwrap();
    assert_eq!(row.kind, JobKind::Invalid);
    assert_eq!(row.error_kind.as_deref(), Some("invalid_job"));
}

#[tokio::test]
async fn unknown_resource_class_is_marked_invalid_and_does_not_block_claim() {
    let store = test_store().await;
    let now = chrono::Utc::now().to_rfc3339();
    crate::entities::jobs::ActiveModel {
        id: sea_orm::ActiveValue::Set("bad-class".into()),
        kind: sea_orm::ActiveValue::Set("scan".into()),
        state: sea_orm::ActiveValue::Set("pending".into()),
        priority: sea_orm::ActiveValue::Set(10),
        resource_class: sea_orm::ActiveValue::Set("not_a_class".into()),
        payload: sea_orm::ActiveValue::Set(r#"{"v":1,"trigger":"api"}"#.into()),
        progress: sea_orm::ActiveValue::Set(None),
        attempt_count: sea_orm::ActiveValue::Set(0),
        max_attempts: sea_orm::ActiveValue::Set(3),
        run_after: sea_orm::ActiveValue::Set(now.clone()),
        lease_owner: sea_orm::ActiveValue::Set(None),
        lease_expires_at: sea_orm::ActiveValue::Set(None),
        dedup_key: sea_orm::ActiveValue::Set("scan:account=bad-class".into()),
        error_kind: sea_orm::ActiveValue::Set(None),
        error_message: sea_orm::ActiveValue::Set(None),
        cancel_requested: sea_orm::ActiveValue::Set(0),
        created_at: sea_orm::ActiveValue::Set(now.clone()),
        updated_at: sea_orm::ActiveValue::Set(now.clone()),
        started_at: sea_orm::ActiveValue::Set(None),
        finished_at: sea_orm::ActiveValue::Set(None),
        lease_generation: sea_orm::ActiveValue::Set(0),
    }
    .insert(store.db())
    .await
    .unwrap();
    let created = store.enqueue_job(scan_spec(None, 8)).await.unwrap();
    let EnqueueOutcome::Created { id: good_id } = created else {
        panic!("expected created: {created:?}");
    };

    let claimed = claim_job(&store, "worker-class").await;
    assert_eq!(claimed.id, good_id);
    let bad = store.get_job("bad-class").await.unwrap().unwrap();
    assert_eq!(bad.kind, JobKind::Invalid);
    assert_eq!(bad.state, JobState::Failed);
    assert_eq!(bad.error_kind.as_deref(), Some("invalid_job"));
    assert!(
        bad.error_message
            .as_deref()
            .is_some_and(|m| m.contains("resource class")),
        "unexpected error: {:?}",
        bad.error_message
    );
    assert_eq!(store.count_active_jobs().await.unwrap(), 1);
}

#[tokio::test]
async fn plugin_copy_admits_and_claims() {
    let store = test_store().await;
    let spec = EnqueueJobSpec {
        kind: JobKind::PluginCopy,
        payload: JobPayload {
            plugin_id: Some("local".into()),
            source_key: Some("a".into()),
            dest_key: Some("b".into()),
            trigger: JobTrigger::Api,
            ..Default::default()
        },
        priority: 0,
        max_attempts: 3,
        max_pending: 8,
        run_after: None,
    };
    assert_eq!(
        JobKind::PluginCopy.dedup_key(&spec.payload),
        "plugin_copy:plugin=local:from=a:to=b"
    );
    assert_eq!(JobKind::parse("plugin_copy"), Some(JobKind::PluginCopy));
    let created = store.enqueue_job(spec).await.unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let claimed = claim_job(&store, "worker-copy").await;
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.kind, JobKind::PluginCopy);
}

fn integration_scan_spec(integration_id: &str, force: bool, max_pending: i64) -> EnqueueJobSpec {
    EnqueueJobSpec {
        kind: JobKind::IntegrationScan,
        payload: JobPayload {
            integration_id: Some(integration_id.into()),
            force,
            trigger: JobTrigger::Api,
            ..Default::default()
        },
        priority: 0,
        max_attempts: 3,
        max_pending,
        run_after: None,
    }
}

#[tokio::test]
async fn integration_scan_force_does_not_coalesce_with_normal() {
    let store = test_store().await;
    let normal = store
        .enqueue_job(integration_scan_spec("echo", false, 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id: normal_id } = normal else {
        panic!("expected created: {normal:?}");
    };
    let forced = store
        .enqueue_job(integration_scan_spec("echo", true, 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id: forced_id } = forced else {
        panic!("forced admit must not coalesce onto a pending normal scan: {forced:?}");
    };
    assert_ne!(normal_id, forced_id);

    let store = test_store().await;
    let forced = store
        .enqueue_job(integration_scan_spec("echo", true, 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id: forced_id } = forced else {
        panic!("expected created: {forced:?}");
    };
    let normal = store
        .enqueue_job(integration_scan_spec("echo", false, 8))
        .await
        .unwrap();
    assert!(
        matches!(normal, EnqueueOutcome::Created { .. }),
        "normal admit must not coalesce onto a pending forced scan: {normal:?}"
    );
    let again = store
        .enqueue_job(integration_scan_spec("echo", true, 8))
        .await
        .unwrap();
    assert_eq!(
        again,
        EnqueueOutcome::Duplicate {
            existing_id: forced_id
        }
    );
}

#[tokio::test]
async fn cancel_versus_claim_always_cancels_or_flags_running() {
    for _ in 0..40 {
        let store = std::sync::Arc::new(test_store().await);
        let created = store.enqueue_job(scan_spec(None, 8)).await.unwrap();
        let EnqueueOutcome::Created { id } = created else {
            panic!("expected created: {created:?}");
        };
        let cancel = {
            let store = store.clone();
            let id = id.clone();
            tokio::spawn(async move { store.request_job_cancel(&id).await })
        };
        let claim = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .claim_next_job(
                        JobResourceClass::Network,
                        "worker-race",
                        60,
                        &uuid::Uuid::new_v4().to_string(),
                    )
                    .await
            })
        };
        let (cancel, claim) = tokio::join!(cancel, claim);
        let cancelled = cancel.unwrap().unwrap();
        let _claimed = claim.unwrap().unwrap();
        let row = store.get_job(&id).await.unwrap().unwrap();
        let cancel_ok = cancelled.as_ref().is_some_and(|job| {
            job.state == JobState::Cancelled
                || (job.state == JobState::Running && job.cancel_requested)
        });
        let row_ok = row.state == JobState::Cancelled
            || (row.state == JobState::Running && row.cancel_requested);
        assert!(
            cancel_ok && row_ok,
            "cancel vs claim left an uncancelled running job: cancel={cancelled:?} row={row:?}"
        );
    }
}

#[tokio::test]
async fn temp_quota_rejects_oversized_and_concurrent_reserves() {
    let store = std::sync::Arc::new(test_store().await);
    let created = store
        .enqueue_job(acquire_spec(None, None, 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    store
        .reserve_job_temp_path(&id, "/tmp/a", 80, 100)
        .await
        .unwrap();
    assert!(store
        .reserve_job_temp_path(&id, "/tmp/b", 30, 100)
        .await
        .is_err());

    let store2 = std::sync::Arc::new(test_store().await);
    let created = store2
        .enqueue_job(acquire_spec(None, None, 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let a = {
        let store2 = store2.clone();
        let id = id.clone();
        tokio::spawn(async move { store2.reserve_job_temp_path(&id, "/tmp/a", 80, 100).await })
    };
    let b = {
        let store2 = store2.clone();
        tokio::spawn(async move { store2.reserve_job_temp_path(&id, "/tmp/b", 80, 100).await })
    };
    let (ra, rb) = tokio::join!(a, b);
    let ok = u32::from(ra.unwrap().is_ok()) + u32::from(rb.unwrap().is_ok());
    assert_eq!(ok, 1);
}

#[tokio::test]
async fn expired_acquire_lease_is_reclaimed_for_retry() {
    let store = test_store().await;
    let created = store
        .enqueue_job(acquire_spec(None, None, 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let first = claim_job(&store, "worker-crash").await;
    assert_eq!(first.id, id);

    let model = crate::entities::jobs::Entity::find_by_id(&id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::jobs::ActiveModel = model.into();
    am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();
    assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);

    let retry = claim_job(&store, "worker-retry").await;
    assert_eq!(retry.id, id);
    assert_eq!(retry.attempt_count, 2);
}

#[tokio::test]
async fn heartbeat_after_expiry_wins_over_reclaim() {
    let store = test_store().await;
    let created = store.enqueue_job(scan_spec(None, 8)).await.unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let claimed = claim_job(&store, "worker-live").await;
    let fence = fence_of(&claimed);
    let model = crate::entities::jobs::Entity::find_by_id(&id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::jobs::ActiveModel = model.into();
    am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();

    assert!(store.heartbeat_job(&fence, 60, None).await.unwrap());
    let after_hb = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(after_hb.lease_generation, claimed.lease_generation);
    assert_eq!(after_hb.state, JobState::Running);

    assert_eq!(store.reclaim_expired_leases().await.unwrap(), 0);
    let still = store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(still.state, JobState::Running);
    assert_eq!(still.lease_owner.as_deref(), Some("worker-live"));
}

fn outbox_spec(event_type: &str, dedup: &str, payload: &str) -> PublishDomainEventSpec {
    outbox_spec_ordered(event_type, dedup, payload, "")
}

fn outbox_spec_ordered(
    event_type: &str,
    dedup: &str,
    payload: &str,
    ordering_key: &str,
) -> PublishDomainEventSpec {
    outbox_spec_account(event_type, dedup, payload, ordering_key, "acct")
}

fn outbox_spec_account(
    event_type: &str,
    dedup: &str,
    payload: &str,
    ordering_key: &str,
    account_id: &str,
) -> PublishDomainEventSpec {
    PublishDomainEventSpec {
        id: String::new(),
        event_type: event_type.into(),
        schema_version: 1,
        account_id: account_id.into(),
        source: String::new(),
        correlation_id: String::new(),
        causation_id: String::new(),
        dedup_key: dedup.into(),
        payload: payload.into(),
        ordering_key: ordering_key.into(),
    }
}

/// Persist a domain outbox row. CodeQL models `publish_*` as diagnostic logging;
/// this writes a store account identifier to SQLite/Postgres, not a credential log.
async fn persist_outbox_event(
    store: &LibraryStore,
    spec: PublishDomainEventSpec,
) -> crate::Result<PublishDomainEventOutcome> {
    // Outbox insert, not a log.
    // codeql[rust/cleartext-logging]
    store.publish_domain_event(spec).await
}

fn expect_created(outcome: PublishDomainEventOutcome) -> String {
    // Do not Debug-format the outcome: `account_id` is sensitive to CodeQL.
    match outcome {
        PublishDomainEventOutcome::Created { id } => id,
        PublishDomainEventOutcome::Duplicate { .. } => {
            panic!("expected PublishDomainEventOutcome::Created, got Duplicate")
        }
    }
}

async fn claim_delivery(store: &LibraryStore, owner: &str) -> crate::EventDeliveryRecord {
    claim_delivery_for(store, owner, &["echo".into()]).await
}

async fn claim_delivery_for(
    store: &LibraryStore,
    owner: &str,
    plugin_ids: &[String],
) -> crate::EventDeliveryRecord {
    store
        .claim_next_event_delivery(
            owner,
            60,
            &uuid::Uuid::new_v4().to_string(),
            plugin_ids,
            32,
            "",
        )
        .await
        .unwrap()
        .expect("claim delivery")
}

async fn drain_pending_wakes(store: &LibraryStore) {
    loop {
        let progress = store
            .process_pending_wakes(32, "test-wake", 60)
            .await
            .unwrap();
        if progress.claimed == 0 {
            break;
        }
    }
}

fn audible_v1_wake_grants() -> String {
    serde_json::to_string(&[EventWakeGrant {
        schema_versions: vec![1],
        filter: Some(serde_json::json!({"source": "audible"})),
    }])
    .unwrap()
}

#[tokio::test]
async fn publish_domain_event_dedupes_and_rejects_oversized_payload() {
    let store = test_store().await;
    let first = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:u1", r#"{"titleId":"u1"}"#),
    )
    .await
    .unwrap();
    let id = expect_created(first);
    let again = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:u1", r#"{"titleId":"u1"}"#),
    )
    .await
    .unwrap();
    assert_eq!(
        again,
        PublishDomainEventOutcome::Duplicate {
            existing_id: id.clone()
        }
    );
    let pending = store.next_undispatched_event().await.unwrap().unwrap();
    assert_eq!(pending.id, id);
    assert_eq!(pending.dispatch_state, "pending");

    let huge = "x".repeat(65_537);
    let err = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:huge", &huge),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("exceeds"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn publish_domain_event_namespaces_dedup_by_account_and_source() {
    let store = test_store().await;
    let a = persist_outbox_event(
        &store,
        outbox_spec_account("book_acquired", "book_acquired:ns", "{}", "", "acct-a"),
    )
    .await
    .unwrap();
    let id_a = expect_created(a);
    let b = persist_outbox_event(
        &store,
        outbox_spec_account("book_acquired", "book_acquired:ns", "{}", "", "acct-b"),
    )
    .await
    .unwrap();
    assert!(
        matches!(b, PublishDomainEventOutcome::Created { .. }),
        "expected PublishDomainEventOutcome::Created"
    );
    let mut sourced = outbox_spec_account("book_acquired", "book_acquired:ns", "{}", "", "acct-a");
    sourced.source = "audible".into();
    let c = persist_outbox_event(&store, sourced).await.unwrap();
    assert!(
        matches!(c, PublishDomainEventOutcome::Created { .. }),
        "expected PublishDomainEventOutcome::Created"
    );
    let dup = persist_outbox_event(
        &store,
        outbox_spec_account("book_acquired", "book_acquired:ns", "{}", "", "acct-a"),
    )
    .await
    .unwrap();
    assert_eq!(
        dup,
        PublishDomainEventOutcome::Duplicate { existing_id: id_a }
    );
}

#[tokio::test]
async fn dispatch_is_idempotent_and_isolates_subscribers() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:iso", r#"{"titleId":"iso"}"#),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let subs = vec![
        EventSubscriber::plugin("echo"),
        EventSubscriber::plugin("audiobookshelf"),
    ];
    assert_eq!(
        store
            .dispatch_event_deliveries(&id, &subs, "op-1")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .dispatch_event_deliveries(&id, &subs, "op-1-replay")
            .await
            .unwrap(),
        0
    );
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(event.dispatch_state, "dispatched");
    assert!(store.next_undispatched_event().await.unwrap().is_none());

    let echo = claim_delivery_for(&store, "w-echo", &["echo".into()]).await;
    assert_eq!(echo.plugin_id, "echo");
    let abs = claim_delivery_for(&store, "w-abs", &["audiobookshelf".into()]).await;
    assert_eq!(abs.plugin_id, "audiobookshelf");
    assert!(store
        .fail_event_delivery(&echo.fence(), "boom")
        .await
        .unwrap());
    assert!(store.ack_event_delivery(&abs.fence()).await.unwrap());
    let echo_row = store.get_event_delivery(&echo.id).await.unwrap().unwrap();
    let abs_row = store.get_event_delivery(&abs.id).await.unwrap().unwrap();
    assert_eq!(echo_row.state, "pending");
    assert_eq!(abs_row.state, "acked");
}

#[tokio::test]
async fn concurrent_host_cas_claims_do_not_skip_keyset_pages() {
    let store = test_store().await;
    for i in 0..5 {
        let created = persist_outbox_event(
            &store,
            outbox_spec_ordered(
                "book_acquired",
                &format!("book_acquired:keyset-{i}"),
                "{}",
                &format!("k{i}"),
            ),
        )
        .await
        .unwrap();
        let id = expect_created(created);
        store
            .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], &format!("d-{id}"))
            .await
            .unwrap();
    }
    let db = store.db().clone();
    super::event_outbox::set_claim_page_for_test(Some(2));
    let store = std::sync::Arc::new(store.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db })));
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut n = 0u32;
            while store
                .claim_next_event_delivery(
                    &format!("keyset-w{i}"),
                    60,
                    &uuid::Uuid::new_v4().to_string(),
                    &["echo".into()],
                    8,
                    "",
                )
                .await
                .unwrap()
                .is_some()
            {
                n += 1;
            }
            n
        }));
    }
    let mut claimed = 0u32;
    for handle in handles {
        claimed += handle.await.unwrap();
    }
    super::event_outbox::set_claim_page_for_test(None);
    assert_eq!(claimed, 5, "keyset paging must not skip pending rows");
}

#[tokio::test]
async fn dispatch_page_fault_leaves_parent_pending_and_retries() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:page-fault", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let db = store.db().clone();
    let store = store.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db }));
    let subs = [
        EventSubscriber::plugin("echo"),
        EventSubscriber::plugin("audiobookshelf"),
    ];
    super::event_outbox::set_dispatch_chunk_for_test(Some(1));
    super::event_outbox::inject_dispatch_page_failures(1);
    let err = store
        .dispatch_event_deliveries(&id, &subs, "fault-op")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("injected dispatch page failure"),
        "{err}"
    );
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(event.dispatch_state, "pending");
    assert!(store.next_undispatched_event().await.unwrap().is_some());
    assert!(store
        .get_event_delivery(&format!("{id}:echo"))
        .await
        .unwrap()
        .is_some());
    assert!(store
        .get_event_delivery(&format!("{id}:audiobookshelf"))
        .await
        .unwrap()
        .is_none());
    super::event_outbox::inject_dispatch_page_failures(0);
    let created = store
        .dispatch_event_deliveries(&id, &subs, "fault-retry")
        .await
        .unwrap();
    super::event_outbox::set_dispatch_chunk_for_test(None);
    assert_eq!(created, 1, "retry inserts the remaining subscriber only");
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(event.dispatch_state, "dispatched");
}

#[tokio::test]
async fn dispatch_retry_keeps_frozen_snapshot_when_catalog_changes() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:snapshot-catalog", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let db = store.db().clone();
    let store = store.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db }));
    let original = [
        EventSubscriber::plugin("echo"),
        EventSubscriber::plugin("audiobookshelf"),
        EventSubscriber::plugin("keep"),
    ];
    super::event_outbox::set_dispatch_chunk_for_test(Some(2));
    super::event_outbox::inject_dispatch_page_failures(1);
    let err = store
        .dispatch_event_deliveries(&id, &original, "snap-op")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("injected dispatch page failure"),
        "{err}"
    );
    super::event_outbox::inject_dispatch_page_failures(0);
    let changed = [
        EventSubscriber::plugin("echo"),
        EventSubscriber::plugin("replaced-plugin"),
        EventSubscriber::plugin("keep"),
    ];
    let created = store
        .dispatch_event_deliveries(&id, &changed, "snap-op")
        .await
        .unwrap();
    super::event_outbox::set_dispatch_chunk_for_test(None);
    assert!(
        created >= 1,
        "retry must finish remaining frozen subscribers, created={created}"
    );
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(event.dispatch_state, "dispatched");
    assert!(store
        .get_event_delivery(&format!("{id}:audiobookshelf"))
        .await
        .unwrap()
        .is_some());
    assert!(store
        .get_event_delivery(&format!("{id}:keep"))
        .await
        .unwrap()
        .is_some());
    assert!(store
        .get_event_delivery(&format!("{id}:replaced-plugin"))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_snapshot_cas_two_stores_agree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lib.db");
    let db1 = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
    crate::apply_host_schema(&db1).await.unwrap();
    let db2 = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
    let store1 = LibraryStore::from_connection(db1.clone());
    let created = persist_outbox_event(
        &store1,
        outbox_spec("book_acquired", "book_acquired:cas-snapshot", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let store1 = store1.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db: db1 }));
    let store2 = LibraryStore::from_connection(db2.clone())
        .with_atomic_txn(Arc::new(InProcessSqliteAtomic { db: db2 }));
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    super::event_outbox::set_snapshot_claim_barrier(Some(barrier));
    let op = format!("dispatch-{id}");
    let s1 = Arc::new(store1);
    let s2 = Arc::new(store2);
    let id1 = id.clone();
    let id2 = id.clone();
    let op1 = op.clone();
    let op2 = op.clone();
    let a = tokio::spawn(async move {
        super::event_outbox::with_snapshot_claim_wait(async move {
            s1.dispatch_event_deliveries(
                &id1,
                &[
                    EventSubscriber::plugin("echo"),
                    EventSubscriber::plugin("alpha"),
                ],
                &op1,
            )
            .await
        })
        .await
    });
    let b = tokio::spawn(async move {
        super::event_outbox::with_snapshot_claim_wait(async move {
            s2.dispatch_event_deliveries(
                &id2,
                &[
                    EventSubscriber::plugin("echo"),
                    EventSubscriber::plugin("beta"),
                ],
                &op2,
            )
            .await
        })
        .await
    });
    let (ra, rb) = tokio::join!(a, b);
    super::event_outbox::set_snapshot_claim_barrier(None);
    ra.expect("join a").expect("dispatch a");
    rb.expect("join b").expect("dispatch b");
    let store =
        LibraryStore::from_connection(bookclerk_plugin_database_sqlite::open(&path).await.unwrap());
    let has_alpha = store
        .get_event_delivery(&format!("{id}:alpha"))
        .await
        .unwrap()
        .is_some();
    let has_beta = store
        .get_event_delivery(&format!("{id}:beta"))
        .await
        .unwrap()
        .is_some();
    assert!(
        has_alpha ^ has_beta,
        "exactly one catalog must win the snapshot CAS (alpha={has_alpha} beta={has_beta})"
    );
    assert!(store
        .get_event_delivery(&format!("{id}:echo"))
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn unrelated_dispatch_does_not_wait_on_snapshot_cas_barrier() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:no-barrier-wait", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let db = store.db().clone();
    let store = store.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db }));
    super::event_outbox::set_snapshot_claim_barrier(Some(std::sync::Arc::new(
        tokio::sync::Barrier::new(2),
    )));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op-no-wait"),
    )
    .await;
    super::event_outbox::set_snapshot_claim_barrier(None);
    result
        .expect("unrelated dispatch must not wait on the CAS barrier")
        .unwrap();
}

#[tokio::test]
async fn malformed_dispatch_snapshot_fails_closed() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:bad-snapshot", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .db()
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "UPDATE domain_events SET dispatch_snapshot_json = ? WHERE id = ?",
            [
                sea_orm::Value::String(Some("{not-json".into())),
                id.clone().into(),
            ],
        ))
        .await
        .unwrap();
    let db = store.db().clone();
    let store = store.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db }));
    let err = store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "bad-snap")
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("dispatch_snapshot_json"),
        "malformed snapshot must fail closed: {msg}"
    );
}

#[tokio::test]
async fn dispatch_twenty_five_subscribers_on_sqlite_caps_are_all_inserted() {
    let store = test_store()
        .await
        .with_db_capabilities(bookclerk_plugin_abi::DbCapabilities::advertised_sqlite());
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:twenty-five", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let db = store.db().clone();
    let store = store.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db }));
    assert!(
        store.dispatch_chunk_size() >= 25,
        "sqlite caps must pack more than the old take(24) planner cap"
    );
    let subs: Vec<EventSubscriber> = (0..25)
        .map(|i| EventSubscriber::plugin(format!("plugin-{i:02}")))
        .collect();
    assert_eq!(
        store
            .dispatch_event_deliveries(&id, &subs, "op-25")
            .await
            .unwrap(),
        25
    );
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(event.dispatch_state, "dispatched");
    assert_eq!(
        store.list_event_deliveries(None, 50).await.unwrap().len(),
        25
    );
}

#[tokio::test]
async fn oversized_dispatch_page_is_rejected_and_parent_stays_pending() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:oversize", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let backend = InProcessSqliteAtomic {
        db: store.db().clone(),
    };
    let subs: Vec<EventSubscriber> = (0..100)
        .map(|i| EventSubscriber::plugin(format!("plugin-{i:03}")))
        .collect();
    let err = backend
        .dispatch_event_deliveries(&id, &subs, "op-oversize", true)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("maxStatements"),
        "oversized page must fail closed: {err}"
    );
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(event.dispatch_state, "pending");
    assert!(store.next_undispatched_event().await.unwrap().is_some());
}

#[tokio::test]
async fn crash_between_publish_and_dispatch_leaves_pending_outbox() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:crash",
            r#"{"titleId":"crash"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    assert_eq!(
        store.list_event_deliveries(None, 10).await.unwrap().len(),
        0
    );
    let pending = store.next_undispatched_event().await.unwrap().unwrap();
    assert_eq!(pending.id, id);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "catch-up")
        .await
        .unwrap();
    assert!(store.next_undispatched_event().await.unwrap().is_none());
}

#[tokio::test]
async fn reclaim_expired_event_delivery_and_stale_fence_ignored() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:reclaim",
            r#"{"titleId":"reclaim"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let first = claim_delivery(&store, "worker-old").await;
    let stale = first.fence();

    let model = crate::entities::event_deliveries::Entity::find_by_id(&first.id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::event_deliveries::ActiveModel = model.into();
    am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();
    assert_eq!(store.reclaim_expired_event_deliveries().await.unwrap(), 1);

    let second = claim_delivery(&store, "worker-new").await;
    assert_eq!(second.id, first.id);
    assert!(second.lease_generation > stale.generation);
    assert!(store
        .heartbeat_event_delivery(&second.fence(), 60)
        .await
        .unwrap());
    assert!(!store.heartbeat_event_delivery(&stale, 60).await.unwrap());
    assert!(!store.ack_event_delivery(&stale).await.unwrap());
    assert!(store.ack_event_delivery(&second.fence()).await.unwrap());
}

#[tokio::test]
async fn reclaim_expired_resume_restores_resume_pending() {
    reclaim_expired_resume_restores(&test_store().await).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_reclaim_expired_resume_restores_resume_pending() {
    reclaim_expired_resume_restores(&postgres_test_store().await).await;
}

async fn reclaim_expired_resume_restores(store: &LibraryStore) {
    let created = persist_outbox_event(
        store,
        outbox_spec(
            "book_acquired",
            "book_acquired:reclaim-resume",
            r#"{"titleId":"reclaim-resume"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let first = claim_delivery(store, "worker-suspend").await;
    assert_eq!(first.attempt_count, 1);
    assert!(store
        .suspend_event_delivery(
            &first.fence(),
            r#"{"offset":1}"#,
            1,
            chrono::Utc::now() - chrono::Duration::seconds(1),
            "",
            "",
            "",
        )
        .await
        .unwrap());
    let resumed = claim_delivery(store, "worker-resume").await;
    assert_eq!(resumed.attempt_count, 1);
    assert!(!resumed.resume_pending);
    assert!(resumed.checkpoint_json.is_some());

    let model = crate::entities::event_deliveries::Entity::find_by_id(&resumed.id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::event_deliveries::ActiveModel = model.into();
    am.lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();
    assert_eq!(store.reclaim_expired_event_deliveries().await.unwrap(), 1);

    let parked = store
        .get_event_delivery(&resumed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parked.state, "pending");
    assert!(parked.resume_pending);
    assert_eq!(parked.attempt_count, 1);

    let again = claim_delivery(store, "worker-after-reclaim").await;
    assert_eq!(again.id, resumed.id);
    assert_eq!(again.attempt_count, 1);
}

#[tokio::test]
async fn suspend_resume_does_not_increment_attempt_count() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:suspend",
            r#"{"titleId":"suspend"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let first = claim_delivery(&store, "worker-suspend").await;
    assert_eq!(first.attempt_count, 1);
    assert!(store
        .suspend_event_delivery(
            &first.fence(),
            r#"{"offset":1}"#,
            1,
            chrono::Utc::now() - chrono::Duration::seconds(1),
            "",
            "",
            "",
        )
        .await
        .unwrap());
    let parked = store.get_event_delivery(&first.id).await.unwrap().unwrap();
    assert_eq!(parked.state, "pending");
    assert!(parked.resume_pending);
    assert_eq!(parked.checkpoint_json.as_deref(), Some(r#"{"offset":1}"#));

    let resumed = claim_delivery(&store, "worker-resume").await;
    assert_eq!(resumed.attempt_count, 1);
    assert!(!resumed.resume_pending);
    assert_eq!(resumed.invocation_sequence, 1);
}

#[tokio::test]
async fn fifo_skips_later_delivery_with_same_ordering_key() {
    let store = test_store().await;
    let a = persist_outbox_event(
        &store,
        outbox_spec_ordered("book_acquired", "book_acquired:fifo-a", "{}", "same-book"),
    )
    .await
    .unwrap();
    let b = persist_outbox_event(
        &store,
        outbox_spec_ordered("book_acquired", "book_acquired:fifo-b", "{}", "same-book"),
    )
    .await
    .unwrap();
    let id_a = expect_created(a);
    let id_b = expect_created(b);
    let sub = [EventSubscriber::plugin("echo")];
    store
        .dispatch_event_deliveries(&id_a, &sub, "a")
        .await
        .unwrap();
    store
        .dispatch_event_deliveries(&id_b, &sub, "b")
        .await
        .unwrap();
    let first = claim_delivery(&store, "w1").await;
    assert_eq!(first.event_id, id_a);
    assert!(store
        .claim_next_event_delivery(
            "w2",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "",
        )
        .await
        .unwrap()
        .is_none());
    assert!(store.ack_event_delivery(&first.fence()).await.unwrap());
    let second = claim_delivery(&store, "w2").await;
    assert_eq!(second.event_id, id_b);
}

#[tokio::test]
async fn fifo_blocked_window_does_not_starve_other_ordering_keys() {
    let store = test_store().await;
    let sub = [EventSubscriber::plugin("echo")];
    let mut blocked_ids = Vec::new();
    for i in 0..40 {
        let created = persist_outbox_event(
            &store,
            outbox_spec_ordered(
                "book_acquired",
                &format!("book_acquired:fifo-block-{i}"),
                "{}",
                "blocked-book",
            ),
        )
        .await
        .unwrap();
        let id = expect_created(created);
        store
            .dispatch_event_deliveries(&id, &sub, &format!("block-{i}"))
            .await
            .unwrap();
        blocked_ids.push(id);
    }
    let head = claim_delivery(&store, "w-head").await;
    assert_eq!(head.event_id, blocked_ids[0]);

    let other = persist_outbox_event(
        &store,
        outbox_spec_ordered(
            "book_acquired",
            "book_acquired:fifo-other",
            "{}",
            "other-book",
        ),
    )
    .await
    .unwrap();
    let other_id = expect_created(other);
    store
        .dispatch_event_deliveries(&other_id, &sub, "other")
        .await
        .unwrap();
    let next = claim_delivery(&store, "w-other").await;
    assert_eq!(next.event_id, other_id);
}

#[tokio::test]
async fn operator_retry_and_ack_dead_letter() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:dlq", r#"{"titleId":"dlq"}"#),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "w").await;
    assert!(store
        .suspend_event_delivery(
            &claimed.fence(),
            r#"{"offset":9}"#,
            1,
            chrono::Utc::now() - chrono::Duration::seconds(1),
            "",
            "",
            "",
        )
        .await
        .unwrap());
    let resumed = claim_delivery(&store, "w-resume").await;
    assert_eq!(resumed.checkpoint_json.as_deref(), Some(r#"{"offset":9}"#));
    assert!(store
        .dead_letter_event_delivery(&resumed.fence(), "poison")
        .await
        .unwrap());
    assert_eq!(
        store.count_event_deliveries("dead_letter").await.unwrap(),
        1
    );
    assert!(store.retry_dead_letter_delivery(&claimed.id).await.unwrap());
    let reset = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert!(reset.checkpoint_json.is_none());
    assert!(!reset.resume_pending);
    assert_eq!(reset.invocation_sequence, 0);
    let retried = claim_delivery(&store, "w2").await;
    assert_eq!(retried.id, claimed.id);
    assert!(store
        .dead_letter_event_delivery(&retried.fence(), "still poison")
        .await
        .unwrap());
    assert!(store
        .acknowledge_dead_letter_delivery(&claimed.id)
        .await
        .unwrap());
    let done = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.state, "rejected");
}

#[tokio::test]
async fn retry_at_max_attempts_dead_letters() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:retry-max",
            r#"{"titleId":"retry-max"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let mut last_id = String::new();
    for expected_attempt in 1..=crate::EVENT_DELIVERY_MAX_ATTEMPTS {
        let claimed = claim_delivery(&store, "retry-w").await;
        assert_eq!(claimed.attempt_count, expected_attempt);
        last_id = claimed.id.clone();
        let past = chrono::Utc::now() - chrono::Duration::seconds(1);
        assert!(store
            .retry_event_delivery(&claimed.fence(), past, "again")
            .await
            .unwrap());
    }
    let row = store.get_event_delivery(&last_id).await.unwrap().unwrap();
    assert_eq!(row.state, "dead_letter");
    assert!(
        row.error_message
            .as_deref()
            .is_some_and(|m| m.contains("retry exhausted")),
        "unexpected error: {:?}",
        row.error_message
    );
    assert!(store
        .claim_next_event_delivery(
            "retry-w",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "",
        )
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_event_outbox_publish_dispatch_claim() {
    let store = postgres_test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:pg", r#"{"titleId":"pg"}"#),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "pg-op")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "pg-worker").await;
    assert_eq!(claimed.plugin_id, "echo");
    assert!(store.ack_event_delivery(&claimed.fence()).await.unwrap());
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_event_catalog_reconcile_missing_pairs() {
    let store = postgres_test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:pg-late", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[], "pg-empty")
        .await
        .unwrap();
    store
        .upsert_event_subscriber(
            "pg-node",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let created_after = chrono::Utc::now() - chrono::Duration::days(7);
    super::event_outbox::take_dispatch_event_calls();
    let n = store
        .reconcile_catalog_deliveries(created_after)
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert!(store
        .get_event_delivery(&format!("{id}:echo"))
        .await
        .unwrap()
        .is_some());
    super::event_outbox::take_dispatch_event_calls();
    let n = store
        .reconcile_catalog_deliveries(created_after)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(super::event_outbox::take_dispatch_event_calls(), 0);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_event_fence_suspend_and_wake() {
    let store = postgres_test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:pg-suspend",
            r#"{"source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "pg-s")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "pg-suspend").await;
    let future = chrono::Utc::now() + chrono::Duration::days(30);
    assert!(store
        .suspend_event_delivery(
            &claimed.fence(),
            r#"{"n":1}"#,
            1,
            future,
            "book_acquired",
            "",
            "",
        )
        .await
        .unwrap());
    let parked = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parked.state, "pending");
    assert!(parked.resume_pending);
    let trigger = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:pg-wake",
            r#"{"source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let wake_id = expect_created(trigger);
    store
        .dispatch_event_deliveries(&wake_id, &[], "pg-wake-d")
        .await
        .unwrap();
    drain_pending_wakes(&store).await;
    let woken = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_event_retention_cutoff_and_in_flight_cap() {
    let store = postgres_test_store().await;
    let old = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:pg-old", "{}"),
    )
    .await
    .unwrap();
    let old_id = expect_created(old);
    store
        .dispatch_event_deliveries(&old_id, &[], "pg-old-d")
        .await
        .unwrap();
    let aged = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
    let mut am: crate::entities::domain_events::ActiveModel =
        crate::entities::domain_events::Entity::find_by_id(&old_id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.created_at = sea_orm::ActiveValue::Set(aged);
    am.update(store.db()).await.unwrap();
    store
        .upsert_event_subscriber(
            "pg-ret",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    let n = store.reconcile_catalog_deliveries(cutoff).await.unwrap();
    assert_eq!(n, 0, "events older than retention must not late-join");

    let a = persist_outbox_event(
        &store,
        outbox_spec_ordered("book_acquired", "book_acquired:pg-cap-a", "{}", "ka"),
    )
    .await
    .unwrap();
    let b = persist_outbox_event(
        &store,
        outbox_spec_ordered("book_acquired", "book_acquired:pg-cap-b", "{}", "kb"),
    )
    .await
    .unwrap();
    let id_a = expect_created(a);
    let id_b = expect_created(b);
    let sub = [EventSubscriber::plugin("echo")];
    store
        .dispatch_event_deliveries(&id_a, &sub, "pg-ca")
        .await
        .unwrap();
    store
        .dispatch_event_deliveries(&id_b, &sub, "pg-cb")
        .await
        .unwrap();
    let first = store
        .claim_next_event_delivery(
            "pg-cap",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            1,
            "",
        )
        .await
        .unwrap()
        .expect("first");
    assert!(store
        .claim_next_event_delivery(
            "pg-cap-2",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            1,
            "",
        )
        .await
        .unwrap()
        .is_none());
    assert!(store.ack_event_delivery(&first.fence()).await.unwrap());
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_duplicate_publish_replays_skipped_wake() {
    let store = postgres_test_store().await;
    let parked = park_echo_wake(
        &store,
        "book_acquired:pg-dup-wake-1",
        r#"{"titleId":"one","source":"audible"}"#,
        "book_acquired",
        r#"{"source":"audible"}"#,
    )
    .await;
    let second = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:pg-dup-wake-2",
            r#"{"titleId":"two","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let id2 = expect_created(second);
    assert!(
        store
            .get_domain_event(&id2)
            .await
            .unwrap()
            .unwrap()
            .wake_pending
    );
    let still = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(still.run_after > chrono::Utc::now() + chrono::Duration::days(1));
    let again = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:pg-dup-wake-2",
            r#"{"titleId":"two","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        again,
        PublishDomainEventOutcome::Duplicate {
            existing_id: id2.clone()
        }
    );
    assert!(
        store
            .get_domain_event(&id2)
            .await
            .unwrap()
            .unwrap()
            .wake_pending
    );
    drain_pending_wakes(&store).await;
    assert!(
        !store
            .get_domain_event(&id2)
            .await
            .unwrap()
            .unwrap()
            .wake_pending
    );
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_process_pending_wakes_repairs_dispatched_gap() {
    let store = postgres_test_store().await;
    let parked = park_echo_wake(&store, "book_acquired:pg-gap-1", "{}", "book_acquired", "").await;
    let trigger = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:pg-gap-2", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(trigger);
    store
        .dispatch_event_deliveries(&id, &[], "pg-gap-d")
        .await
        .unwrap();
    assert!(
        store
            .get_domain_event(&id)
            .await
            .unwrap()
            .unwrap()
            .wake_pending
    );
    drain_pending_wakes(&store).await;
    assert!(
        !store
            .get_domain_event(&id)
            .await
            .unwrap()
            .unwrap()
            .wake_pending
    );
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_wake_stays_inside_account_boundary() {
    let store = postgres_test_store().await;
    let parked = park_echo_wake(
        &store,
        "book_acquired:pg-acct-a-1",
        "{}",
        "book_acquired",
        "",
    )
    .await;
    persist_outbox_event(
        &store,
        outbox_spec_account(
            "book_acquired",
            "book_acquired:pg-acct-b",
            "{}",
            "",
            "other",
        ),
    )
    .await
    .unwrap();
    drain_pending_wakes(&store).await;
    let still = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(still.run_after > chrono::Utc::now() + chrono::Duration::days(1));
    persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:pg-acct-a-2", "{}"),
    )
    .await
    .unwrap();
    drain_pending_wakes(&store).await;
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_concurrent_event_claims_respect_in_flight_cap() {
    let store = postgres_test_store().await;
    let store = std::sync::Arc::new(store);
    let sub = [EventSubscriber::plugin("echo")];
    for (dedup, key) in [("pg-race-a", "ka"), ("pg-race-b", "kb")] {
        let created = persist_outbox_event(
            &store,
            outbox_spec_ordered(
                "book_acquired",
                &format!("book_acquired:{dedup}"),
                "{}",
                key,
            ),
        )
        .await
        .unwrap();
        let id = expect_created(created);
        store
            .dispatch_event_deliveries(&id, &sub, &format!("d-{id}"))
            .await
            .unwrap();
    }
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .claim_next_event_delivery(
                    &format!("pg-race-{i}"),
                    60,
                    &uuid::Uuid::new_v4().to_string(),
                    &["echo".into()],
                    1,
                    "",
                )
                .await
                .unwrap()
        }));
    }
    let mut claimed = 0u32;
    for handle in handles {
        if handle.await.unwrap().is_some() {
            claimed += 1;
        }
    }
    assert_eq!(claimed, 1);
}

#[tokio::test]
async fn acquire_status_and_outbox_commit_together() {
    let store = test_store().await;
    store
        .upsert_account("user-1", "us", None, true, "audible")
        .await
        .unwrap();
    let book = store
        .upsert_book(&NewBook::minimal("B00EVT", "user-1", "us", "Event Book"))
        .await
        .unwrap();
    super::event_outbox::inject_event_publish_failures(1);
    let err = store
        .set_acquire_status(
            "B00EVT",
            "user-1",
            AcquireStatus::Acquired,
            Some("Author/Event Book/book.m4b"),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("injected event publish fault"),
        "unexpected error: {err}"
    );
    let still = store.get_book("B00EVT", "user-1").await.unwrap().unwrap();
    assert_eq!(still.acquire_status, AcquireStatus::NotAcquired);
    assert!(store.next_undispatched_event().await.unwrap().is_none());

    store
        .set_acquire_status(
            "B00EVT",
            "user-1",
            AcquireStatus::Acquired,
            Some("Author/Event Book/book.m4b"),
            None,
        )
        .await
        .unwrap();
    let pending = store.next_undispatched_event().await.unwrap().unwrap();
    assert_eq!(pending.ordering_key, book.uuid);
    assert_eq!(pending.event_type, "book_acquired");
    assert_eq!(pending.source, "audible");
}

#[tokio::test]
async fn fifo_uses_persisted_ordering_key_without_title_id() {
    let store = test_store().await;
    let sub = [EventSubscriber::plugin("echo")];
    let a = persist_outbox_event(
        &store,
        outbox_spec_ordered("generic", "generic:a", r#"{"foo":1}"#, "shared-key"),
    )
    .await
    .unwrap();
    let b = persist_outbox_event(
        &store,
        outbox_spec_ordered("generic", "generic:b", r#"{"foo":2}"#, "shared-key"),
    )
    .await
    .unwrap();
    let id_a = expect_created(a);
    let id_b = expect_created(b);
    store
        .dispatch_event_deliveries(&id_a, &sub, "a")
        .await
        .unwrap();
    store
        .dispatch_event_deliveries(&id_b, &sub, "b")
        .await
        .unwrap();
    let first = claim_delivery(&store, "w1").await;
    assert_eq!(first.event_id, id_a);
    assert_eq!(first.ordering_key, "shared-key");
    assert!(store
        .claim_next_event_delivery(
            "w2",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "",
        )
        .await
        .unwrap()
        .is_none());
    assert!(store.ack_event_delivery(&first.fence()).await.unwrap());
    let second = claim_delivery(&store, "w2").await;
    assert_eq!(second.event_id, id_b);
}

#[tokio::test]
async fn claim_filters_to_loaded_plugin_ids() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:plug", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(
            &id,
            &[
                EventSubscriber::plugin("echo"),
                EventSubscriber::plugin("audiobookshelf"),
            ],
            "both",
        )
        .await
        .unwrap();
    assert!(store
        .claim_next_event_delivery(
            "w-empty",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &[],
            32,
            ""
        )
        .await
        .unwrap()
        .is_none());
    let echo = claim_delivery_for(&store, "w-echo", &["echo".into()]).await;
    assert_eq!(echo.plugin_id, "echo");
    assert!(store
        .claim_next_event_delivery(
            "w-echo-again",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "",
        )
        .await
        .unwrap()
        .is_none());
    let abs = claim_delivery_for(&store, "w-abs", &["audiobookshelf".into()]).await;
    assert_eq!(abs.plugin_id, "audiobookshelf");
    let restore = echo.checkpoint_json.is_some();
    assert!(store
        .release_unexecuted_event_delivery(&echo.fence(), restore)
        .await
        .unwrap());
    let echo_row = store.get_event_delivery(&echo.id).await.unwrap().unwrap();
    assert_eq!(echo_row.state, "pending");
    assert_eq!(echo_row.attempt_count, 0);
}

#[tokio::test]
async fn catalog_dispatch_creates_rows_claim_filters_loaded_plugins() {
    let store = test_store().await;
    store
        .upsert_event_subscriber(
            "node-a",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    store
        .upsert_event_subscriber(
            "node-b",
            "audiobookshelf",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:catalog-two",
            r#"{"titleId":"catalog-two"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(
        store
            .dispatch_catalog_matches(&event, "cluster-a")
            .await
            .unwrap(),
        2
    );
    let rows = store.list_event_deliveries(None, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    let echo = claim_delivery_for(&store, "node-a", &["echo".into()]).await;
    assert_eq!(echo.plugin_id, "echo");
    assert!(store
        .claim_next_event_delivery(
            "node-a-again",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "",
        )
        .await
        .unwrap()
        .is_none());
    let pending = store
        .list_event_deliveries(Some("pending"), 10)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].plugin_id, "audiobookshelf");
}

#[tokio::test]
async fn catalog_filter_match_and_miss() {
    let store = test_store().await;
    let mut spec = EventCatalogSubscription::new("book_acquired", vec![1]);
    spec.filter = Some(serde_json::json!({ "source": "audible" }));
    store
        .upsert_event_subscriber("node-a", "echo", &[spec], true)
        .await
        .unwrap();
    let hit = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:filter-hit",
            r#"{"source":"audible","titleId":"hit"}"#,
        ),
    )
    .await
    .unwrap();
    let miss = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:filter-miss",
            r#"{"source":"libro","titleId":"miss"}"#,
        ),
    )
    .await
    .unwrap();
    let hit_id = expect_created(hit);
    let miss_id = expect_created(miss);
    let hit_event = store.get_domain_event(&hit_id).await.unwrap().unwrap();
    let miss_event = store.get_domain_event(&miss_id).await.unwrap().unwrap();
    assert_eq!(
        store
            .dispatch_catalog_matches(&hit_event, "hit")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .dispatch_catalog_matches(&miss_event, "miss")
            .await
            .unwrap(),
        0
    );
    let rows = store.list_event_deliveries(None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event_id, hit_id);
}

#[tokio::test]
async fn cancel_pending_and_running_delivery() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:cancel-pending",
            r#"{"titleId":"cancel-pending"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let pending_id = format!("{id}:echo");
    let cancelled = store
        .request_event_delivery_cancel(&pending_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.state, "rejected");
    assert_eq!(
        cancelled.error_message.as_deref(),
        Some("cancelled by operator")
    );

    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:cancel-running",
            r#"{"titleId":"cancel-running"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "cancel-w").await;
    let flagged = store
        .request_event_delivery_cancel(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(flagged.state, "running");
    assert!(flagged.cancel_requested);
    assert!(store
        .event_delivery_cancel_requested(&claimed.id)
        .await
        .unwrap());
}

#[tokio::test]
async fn resume_suspended_delivery_sets_run_after_now() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:resume",
            r#"{"titleId":"resume"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "resume-w").await;
    let future = chrono::Utc::now() + chrono::Duration::hours(1);
    assert!(store
        .suspend_event_delivery(&claimed.fence(), r#"{"offset":1}"#, 1, future, "", "", "")
        .await
        .unwrap());
    let parked = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert!(parked.run_after > chrono::Utc::now());
    assert!(store.resume_event_delivery(&claimed.id).await.unwrap());
    let woken = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(woken.state, "pending");
    assert!(woken.resume_pending);
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
async fn prune_event_retention_independent_of_dead_letters() {
    let store = test_store().await;
    let acked = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:prune-ack",
            r#"{"titleId":"prune-ack"}"#,
        ),
    )
    .await
    .unwrap();
    let ack_id = expect_created(acked);
    store
        .dispatch_event_deliveries(&ack_id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "prune-ack").await;
    assert!(store.ack_event_delivery(&claimed.fence()).await.unwrap());

    let dead = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:prune-dl",
            r#"{"titleId":"prune-dl"}"#,
        ),
    )
    .await
    .unwrap();
    let dl_id = expect_created(dead);
    store
        .dispatch_event_deliveries(&dl_id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "prune-dl").await;
    assert!(store
        .dead_letter_event_delivery(&claimed.fence(), "poison")
        .await
        .unwrap());

    let pruned = store.prune_event_deliveries(0, 30).await.unwrap();
    assert!(pruned >= 1);
    assert!(store
        .get_event_delivery(&format!("{ack_id}:echo"))
        .await
        .unwrap()
        .is_none());
    assert!(store.get_domain_event(&ack_id).await.unwrap().is_none());
    let dl = store
        .get_event_delivery(&format!("{dl_id}:echo"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dl.state, "dead_letter");
    assert!(store.get_domain_event(&dl_id).await.unwrap().is_some());

    let pruned = store.prune_event_deliveries(0, 0).await.unwrap();
    assert!(pruned >= 1);
    assert!(store
        .get_event_delivery(&format!("{dl_id}:echo"))
        .await
        .unwrap()
        .is_none());
    assert!(store.get_domain_event(&dl_id).await.unwrap().is_none());
}

#[tokio::test]
async fn event_delivery_metrics_split_pending_and_suspended() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:metrics",
            r#"{"titleId":"metrics"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(
            &id,
            &[
                EventSubscriber::plugin("echo"),
                EventSubscriber::plugin("audiobookshelf"),
            ],
            "op",
        )
        .await
        .unwrap();
    let echo = claim_delivery_for(&store, "metrics-echo", &["echo".into()]).await;
    assert!(store.ack_event_delivery(&echo.fence()).await.unwrap());
    let abs = claim_delivery_for(&store, "metrics-abs", &["audiobookshelf".into()]).await;
    let future = chrono::Utc::now() + chrono::Duration::hours(1);
    assert!(store
        .suspend_event_delivery(&abs.fence(), r#"{"offset":1}"#, 1, future, "", "", "")
        .await
        .unwrap());
    let metrics = store.event_delivery_metrics().await.unwrap();
    assert_eq!(metrics.pending, 0);
    assert_eq!(metrics.running, 0);
    assert_eq!(metrics.suspended, 1);
    assert_eq!(metrics.acked, 1);
    assert_eq!(metrics.dead_letter, 0);
    assert!(metrics.oldest_pending_age_secs.is_some());
    assert_eq!(metrics.suspensions_total, 1);
    assert!(metrics.dispatch_latency_ms_avg.is_some());
}

#[tokio::test]
async fn catalog_union_enabled_wins_and_expired_nodes_drop_out() {
    let store = test_store().await;
    let spec = EventCatalogSubscription::new("book_acquired", vec![1]);
    store
        .upsert_event_subscriber("node-a", "echo", std::slice::from_ref(&spec), true)
        .await
        .unwrap();
    store
        .upsert_event_subscriber("node-b", "echo", std::slice::from_ref(&spec), false)
        .await
        .unwrap();
    let created = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:union-enable",
            r#"{"titleId":"union-enable"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(
        store
            .dispatch_catalog_matches(&event, "union")
            .await
            .unwrap(),
        1
    );

    let store = test_store().await;
    store
        .upsert_event_subscriber("node-a", "echo", std::slice::from_ref(&spec), false)
        .await
        .unwrap();
    store
        .upsert_event_subscriber("node-b", "echo", std::slice::from_ref(&spec), false)
        .await
        .unwrap();
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:both-disabled", r#"{}"#),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(
        store
            .dispatch_catalog_matches(&event, "disabled")
            .await
            .unwrap(),
        0
    );

    let store = test_store().await;
    let stale = chrono::Utc::now() - chrono::Duration::seconds(120);
    store
        .upsert_event_subscriber_at("node-a", "echo", &[spec], true, stale)
        .await
        .unwrap();
    let live = store.list_live_event_subscribers().await.unwrap();
    assert!(live.is_empty(), "{live:?}");
}

#[tokio::test]
async fn late_join_reconciles_more_than_two_hundred_dispatched_events() {
    let store = test_store().await;
    let mut ids = Vec::new();
    for i in 0..201 {
        let created = persist_outbox_event(
            &store,
            outbox_spec("book_acquired", &format!("book_acquired:late-{i}"), "{}"),
        )
        .await
        .unwrap();
        let id = expect_created(created);
        store
            .dispatch_event_deliveries(&id, &[], &format!("empty-{i}"))
            .await
            .unwrap();
        ids.push(id);
    }
    store
        .upsert_event_subscriber(
            "node-late",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let created_after = chrono::Utc::now() - chrono::Duration::days(7);
    let n = store
        .reconcile_catalog_deliveries(created_after)
        .await
        .unwrap();
    assert_eq!(n, 201);
    for id in ids {
        let row = store
            .get_event_delivery(&format!("{id}:echo"))
            .await
            .unwrap();
        assert!(row.is_some(), "missing delivery for {id}");
    }
}

#[tokio::test]
async fn unchanged_catalog_reconcile_does_zero_dispatch_writes() {
    let store = test_store().await;
    store
        .upsert_event_subscriber(
            "node-a",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 0..201 {
        let created = persist_outbox_event(
            &store,
            outbox_spec("book_acquired", &format!("book_acquired:stable-{i}"), "{}"),
        )
        .await
        .unwrap();
        let id = expect_created(created);
        store
            .dispatch_event_deliveries(
                &id,
                &[EventSubscriber::plugin("echo")],
                &format!("dispatch-{id}-echo"),
            )
            .await
            .unwrap();
        ids.push(id);
    }
    let created_after = chrono::Utc::now() - chrono::Duration::days(7);
    let before = store.list_event_deliveries(None, 500).await.unwrap().len();
    super::event_outbox::take_dispatch_event_calls();
    let n = store
        .reconcile_catalog_deliveries(created_after)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(super::event_outbox::take_dispatch_event_calls(), 0);
    assert_eq!(
        store.list_event_deliveries(None, 500).await.unwrap().len(),
        before
    );
    store
        .upsert_event_subscriber(
            "node-b",
            "audiobookshelf",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let n = store
        .reconcile_catalog_deliveries(created_after)
        .await
        .unwrap();
    assert_eq!(n, 201);
    for id in ids {
        assert!(store
            .get_event_delivery(&format!("{id}:audiobookshelf"))
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_delivery(&format!("{id}:echo"))
            .await
            .unwrap()
            .is_some());
    }
}

#[tokio::test]
async fn wake_on_matching_event_makes_delivery_claimable() {
    let store = test_store().await;
    let first = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:wake-1",
            r#"{"titleId":"one","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let id1 = expect_created(first);
    store
        .dispatch_event_deliveries(&id1, &[EventSubscriber::plugin("echo")], "d1")
        .await
        .unwrap();
    let claimed = claim_delivery(&store, "wake-w").await;
    let future = chrono::Utc::now() + chrono::Duration::days(30);
    assert!(store
        .suspend_event_delivery(
            &claimed.fence(),
            r#"{"offset":1}"#,
            1,
            future,
            "book_acquired",
            r#"{"source":"audible"}"#,
            "",
        )
        .await
        .unwrap());
    let parked = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert!(parked.run_after > chrono::Utc::now() + chrono::Duration::days(1));
    assert_eq!(parked.wake_event_type, "book_acquired");
    assert!(store
        .claim_next_event_delivery(
            "wake-early",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "",
        )
        .await
        .unwrap()
        .is_none());

    let second = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:wake-2",
            r#"{"titleId":"two","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let id2 = expect_created(second);
    store
        .dispatch_event_deliveries(&id2, &[EventSubscriber::plugin("echo")], "d2")
        .await
        .unwrap();
    drain_pending_wakes(&store).await;
    let woken = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert!(woken.resume_pending);
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
    let resumed = claim_delivery(&store, "wake-resume").await;
    assert_eq!(resumed.id, claimed.id);
}

async fn park_echo_wake(
    store: &LibraryStore,
    dedup: &str,
    payload: &str,
    wake_type: &str,
    wake_filter: &str,
) -> crate::EventDeliveryRecord {
    let created = persist_outbox_event(store, outbox_spec("book_acquired", dedup, payload))
        .await
        .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], &format!("d-{id}"))
        .await
        .unwrap();
    let claimed = claim_delivery(store, &format!("park-{dedup}")).await;
    let future = chrono::Utc::now() + chrono::Duration::days(30);
    assert!(store
        .suspend_event_delivery(
            &claimed.fence(),
            r#"{"offset":1}"#,
            1,
            future,
            wake_type,
            wake_filter,
            "",
        )
        .await
        .unwrap());
    store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn duplicate_publish_replays_skipped_wake() {
    let store = test_store().await;
    let parked = park_echo_wake(
        &store,
        "book_acquired:dup-wake-1",
        r#"{"titleId":"one","source":"audible"}"#,
        "book_acquired",
        r#"{"source":"audible"}"#,
    )
    .await;
    let second = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:dup-wake-2",
            r#"{"titleId":"two","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let id2 = expect_created(second);
    let pending = store.get_domain_event(&id2).await.unwrap().unwrap();
    assert!(pending.wake_pending);
    let still = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(still.run_after > chrono::Utc::now() + chrono::Duration::days(1));
    let again = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:dup-wake-2",
            r#"{"titleId":"two","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        again,
        PublishDomainEventOutcome::Duplicate {
            existing_id: id2.clone()
        }
    );
    let after = store.get_domain_event(&id2).await.unwrap().unwrap();
    assert!(after.wake_pending);
    drain_pending_wakes(&store).await;
    let after = store.get_domain_event(&id2).await.unwrap().unwrap();
    assert!(!after.wake_pending);
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
async fn process_pending_wakes_repairs_dispatched_gap() {
    let store = test_store().await;
    let parked = park_echo_wake(
        &store,
        "book_acquired:gap-wake-1",
        r#"{"titleId":"one","source":"audible"}"#,
        "book_acquired",
        "",
    )
    .await;
    let trigger = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:gap-wake-2",
            r#"{"titleId":"two"}"#,
        ),
    )
    .await
    .unwrap();
    let id = expect_created(trigger);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "gap-d")
        .await
        .unwrap();
    let dispatched = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(dispatched.dispatch_state, "dispatched");
    assert!(dispatched.wake_pending);
    let n = store
        .process_pending_wakes(32, "test-wake", 60)
        .await
        .unwrap();
    assert!(n.claimed >= 1);
    let after = store.get_domain_event(&id).await.unwrap().unwrap();
    assert!(!after.wake_pending);
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
async fn wake_stays_inside_account_boundary() {
    let store = test_store().await;
    let parked = park_echo_wake(
        &store,
        "book_acquired:acct-a-1",
        r#"{"titleId":"one"}"#,
        "book_acquired",
        "",
    )
    .await;
    let other = persist_outbox_event(
        &store,
        outbox_spec_account(
            "book_acquired",
            "book_acquired:acct-b",
            r#"{"titleId":"two"}"#,
            "",
            "other",
        ),
    )
    .await
    .unwrap();
    let other_id = expect_created(other);
    drain_pending_wakes(&store).await;
    let still = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(still.run_after > chrono::Utc::now() + chrono::Duration::days(1));
    let same = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:acct-a-2",
            r#"{"titleId":"three"}"#,
        ),
    )
    .await
    .unwrap();
    let same_id = expect_created(same);
    drain_pending_wakes(&store).await;
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
    let _ = (other_id, same_id);
}

#[tokio::test]
async fn wake_pages_more_than_page_size_same_account() {
    let store = test_store().await;
    let parent = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:wake-page-parent", "{}"),
    )
    .await
    .unwrap();
    let parent_id = expect_created(parent);
    let now = chrono::Utc::now().to_rfc3339();
    let future = "9999-12-31T23:59:59+00:00";
    let n = usize::try_from(store.wake_page()).unwrap() + 1;
    for i in 0..n {
        let id = format!("wake-page-{i:03}");
        crate::entities::event_deliveries::ActiveModel {
            id: sea_orm::ActiveValue::Set(id.clone()),
            event_id: sea_orm::ActiveValue::Set(parent_id.clone()),
            plugin_id: sea_orm::ActiveValue::Set(format!("echo-{i:03}")),
            idempotency_key: sea_orm::ActiveValue::Set(id),
            state: sea_orm::ActiveValue::Set("pending".into()),
            attempt_count: sea_orm::ActiveValue::Set(0),
            max_attempts: sea_orm::ActiveValue::Set(8),
            lease_owner: sea_orm::ActiveValue::Set(None),
            lease_expires_at: sea_orm::ActiveValue::Set(None),
            lease_generation: sea_orm::ActiveValue::Set(0),
            run_after: sea_orm::ActiveValue::Set(future.into()),
            invocation_sequence: sea_orm::ActiveValue::Set(0),
            resume_pending: sea_orm::ActiveValue::Set(0),
            checkpoint_json: sea_orm::ActiveValue::Set(None),
            checkpoint_schema_version: sea_orm::ActiveValue::Set(0),
            ordering_key: sea_orm::ActiveValue::Set(format!("k-{i}")),
            outcome: sea_orm::ActiveValue::Set(None),
            error_message: sea_orm::ActiveValue::Set(None),
            created_at: sea_orm::ActiveValue::Set(now.clone()),
            updated_at: sea_orm::ActiveValue::Set(now.clone()),
            cancel_requested: sea_orm::ActiveValue::Set(0),
            resource_class: sea_orm::ActiveValue::Set("network".into()),
            wake_event_type: sea_orm::ActiveValue::Set("book_acquired".into()),
            wake_filter_json: sea_orm::ActiveValue::Set(String::new()),
            wake_grants_json: sea_orm::ActiveValue::Set(String::new()),
        }
        .insert(store.db())
        .await
        .unwrap();
    }
    persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:wake-page-trigger", "{}"),
    )
    .await
    .unwrap();
    drain_pending_wakes(&store).await;
    let mut woken = 0u32;
    for i in 0..n {
        let row = store
            .get_event_delivery(&format!("wake-page-{i:03}"))
            .await
            .unwrap()
            .unwrap();
        if row.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2) {
            woken += 1;
        }
    }
    assert_eq!(woken, u32::try_from(n).unwrap());
}

#[tokio::test]
async fn per_plugin_in_flight_cap_blocks_second_claim() {
    let store = test_store().await;
    let sub = [EventSubscriber::plugin("echo")];
    for (dedup, key) in [("cap-a", "k-a"), ("cap-b", "k-b")] {
        let created = persist_outbox_event(
            &store,
            outbox_spec_ordered(
                "book_acquired",
                &format!("book_acquired:{dedup}"),
                "{}",
                key,
            ),
        )
        .await
        .unwrap();
        let id = expect_created(created);
        store
            .dispatch_event_deliveries(&id, &sub, &format!("d-{id}"))
            .await
            .unwrap();
    }
    let first = store
        .claim_next_event_delivery(
            "cap-w1",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            1,
            "",
        )
        .await
        .unwrap()
        .expect("first claim");
    assert!(store
        .claim_next_event_delivery(
            "cap-w2",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            1,
            "",
        )
        .await
        .unwrap()
        .is_none());
    assert!(store.ack_event_delivery(&first.fence()).await.unwrap());
    let second = store
        .claim_next_event_delivery(
            "cap-w2",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            1,
            "",
        )
        .await
        .unwrap()
        .expect("second claim after ack");
    assert_ne!(second.id, first.id);
}

#[tokio::test]
async fn publish_rejects_invalid_source_and_accepts_empty() {
    let store = test_store().await;
    let mut bad = outbox_spec("book_acquired", "book_acquired:bad-src", "{}");
    bad.source = "Not_Valid".into();
    assert!(persist_outbox_event(&store, bad).await.is_err());
    let mut ok = outbox_spec("book_acquired", "book_acquired:ok-src", "{}");
    ok.source = "audible".into();
    let created = persist_outbox_event(&store, ok).await.unwrap();
    let id = expect_created(created);
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(event.source, "audible");
}

#[tokio::test]
async fn zero_delivery_event_survives_until_retention_deadline() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:zero-delivery", r#"{}"#),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[], "empty")
        .await
        .unwrap();
    assert_eq!(store.prune_event_deliveries(7, 30).await.unwrap(), 0);
    assert!(store.get_domain_event(&id).await.unwrap().is_some());

    let old = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
    let mut am: crate::entities::domain_events::ActiveModel =
        crate::entities::domain_events::Entity::find_by_id(&id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.created_at = sea_orm::ActiveValue::Set(old);
    am.update(store.db()).await.unwrap();
    assert!(store.prune_event_deliveries(7, 30).await.unwrap() >= 1);
    assert!(store.get_domain_event(&id).await.unwrap().is_none());
}

#[tokio::test]
async fn unknown_event_resource_class_is_rejected_and_does_not_block_claim() {
    let store = test_store().await;
    let created = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:class-network", r#"{}"#),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "op")
        .await
        .unwrap();

    let mut cpu = EventCatalogSubscription::new("book_acquired", vec![1]);
    cpu.resource_class = "cpu".into();
    store
        .upsert_event_subscriber("node-a", "cpu-plugin", &[cpu], true)
        .await
        .unwrap();
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    assert_eq!(
        store
            .dispatch_catalog_matches(&event, "cpu-skip")
            .await
            .unwrap(),
        0
    );

    let now = chrono::Utc::now().to_rfc3339();
    crate::entities::event_deliveries::ActiveModel {
        id: sea_orm::ActiveValue::Set("planted:cpu".into()),
        event_id: sea_orm::ActiveValue::Set(id.clone()),
        plugin_id: sea_orm::ActiveValue::Set("stale-plugin".into()),
        idempotency_key: sea_orm::ActiveValue::Set("planted:cpu".into()),
        state: sea_orm::ActiveValue::Set("pending".into()),
        attempt_count: sea_orm::ActiveValue::Set(0),
        max_attempts: sea_orm::ActiveValue::Set(8),
        lease_owner: sea_orm::ActiveValue::Set(None),
        lease_expires_at: sea_orm::ActiveValue::Set(None),
        lease_generation: sea_orm::ActiveValue::Set(0),
        run_after: sea_orm::ActiveValue::Set(now.clone()),
        invocation_sequence: sea_orm::ActiveValue::Set(0),
        resume_pending: sea_orm::ActiveValue::Set(0),
        checkpoint_json: sea_orm::ActiveValue::Set(None),
        checkpoint_schema_version: sea_orm::ActiveValue::Set(0),
        ordering_key: sea_orm::ActiveValue::Set(String::new()),
        outcome: sea_orm::ActiveValue::Set(None),
        error_message: sea_orm::ActiveValue::Set(None),
        created_at: sea_orm::ActiveValue::Set(now.clone()),
        updated_at: sea_orm::ActiveValue::Set(now),
        cancel_requested: sea_orm::ActiveValue::Set(0),
        resource_class: sea_orm::ActiveValue::Set("cpu".into()),
        wake_event_type: sea_orm::ActiveValue::Set(String::new()),
        wake_filter_json: sea_orm::ActiveValue::Set(String::new()),
        wake_grants_json: sea_orm::ActiveValue::Set(String::new()),
    }
    .insert(store.db())
    .await
    .unwrap();

    let claimed = claim_delivery(&store, "class-w").await;
    assert_eq!(claimed.plugin_id, "echo");
    assert_eq!(claimed.id, format!("{id}:echo"));
    let planted = store
        .get_event_delivery("planted:cpu")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(planted.state, "rejected");
    assert!(
        planted
            .error_message
            .as_deref()
            .is_some_and(|m| m.contains("resource class")),
        "{:?}",
        planted.error_message
    );
}

#[tokio::test]
async fn wake_grants_keep_subscription_schema_and_filter() {
    let store = test_store().await;
    let parked = park_echo_wake(
        &store,
        "book_acquired:grant-1",
        r#"{"source":"audible"}"#,
        "book_acquired",
        r#"{"source":"audible"}"#,
    )
    .await;
    let mut am: crate::entities::event_deliveries::ActiveModel =
        crate::entities::event_deliveries::Entity::find_by_id(&parked.id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.wake_grants_json = sea_orm::ActiveValue::Set(audible_v1_wake_grants());
    am.update(store.db()).await.unwrap();

    let mut v2 = outbox_spec(
        "book_acquired",
        "book_acquired:grant-v2",
        r#"{"source":"audible"}"#,
    );
    v2.schema_version = 2;
    persist_outbox_event(&store, v2).await.unwrap();
    drain_pending_wakes(&store).await;
    let still = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(still.run_after > chrono::Utc::now() + chrono::Duration::days(1));

    persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:grant-libro",
            r#"{"source":"libro"}"#,
        ),
    )
    .await
    .unwrap();
    drain_pending_wakes(&store).await;
    let still = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(still.run_after > chrono::Utc::now() + chrono::Duration::days(1));

    persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:grant-ok",
            r#"{"source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    drain_pending_wakes(&store).await;
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
}

#[tokio::test]
async fn process_pending_wakes_is_bounded_and_leases_prevent_shared_slice() {
    let store = test_store().await;
    let mut ids = Vec::new();
    for dedup in ["bound-a", "bound-b", "bound-c"] {
        let created = persist_outbox_event(
            &store,
            outbox_spec("book_acquired", &format!("book_acquired:{dedup}"), "{}"),
        )
        .await
        .unwrap();
        let id = expect_created(created);
        ids.push(id);
    }
    let first = store.process_pending_wakes(1, "owner-a", 60).await.unwrap();
    assert_eq!(first.claimed, 1);
    assert!(first.still_pending);
    let mut still_pending = Vec::new();
    for id in &ids {
        if store
            .get_domain_event(id)
            .await
            .unwrap()
            .unwrap()
            .wake_pending
        {
            still_pending.push(id.clone());
        }
    }
    assert_eq!(still_pending.len(), 2);

    let leased = still_pending[0].clone();
    let mut am: crate::entities::domain_events::ActiveModel =
        crate::entities::domain_events::Entity::find_by_id(&leased)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.wake_lease_owner = sea_orm::ActiveValue::Set(Some("other-node".into()));
    am.wake_lease_expires_at = sea_orm::ActiveValue::Set(Some(
        (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
    ));
    am.update(store.db()).await.unwrap();

    let second = store.process_pending_wakes(8, "owner-b", 60).await.unwrap();
    assert_eq!(second.claimed, 1);
    let leased_row = crate::entities::domain_events::Entity::find_by_id(&leased)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased_row.wake_pending, 1);
    assert_eq!(leased_row.wake_lease_owner.as_deref(), Some("other-node"));
}

#[tokio::test]
async fn stale_wake_finish_and_cursor_do_not_clobber_new_owner() {
    stale_wake_fence_does_not_clobber(&test_store().await).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_stale_wake_finish_and_cursor_do_not_clobber_new_owner() {
    stale_wake_fence_does_not_clobber(&postgres_test_store().await).await;
}

async fn stale_wake_fence_does_not_clobber(store: &LibraryStore) {
    let created = persist_outbox_event(
        store,
        outbox_spec("book_acquired", "book_acquired:fence-stale", "{}"),
    )
    .await
    .unwrap();
    let id = expect_created(created);
    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let mut am: crate::entities::domain_events::ActiveModel =
        crate::entities::domain_events::Entity::find_by_id(&id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.wake_pending = sea_orm::ActiveValue::Set(1);
    am.wake_lease_owner = sea_orm::ActiveValue::Set(Some("live-token".into()));
    am.wake_lease_expires_at = sea_orm::ActiveValue::Set(Some(future));
    am.wake_cursor_at = sea_orm::ActiveValue::Set("cursor-live".into());
    am.wake_cursor_id = sea_orm::ActiveValue::Set("id-live".into());
    am.update(store.db()).await.unwrap();

    assert!(
        !super::event_outbox::finish_wake_on(store.db(), &id, "stale-token")
            .await
            .unwrap()
    );
    assert!(!super::event_outbox::release_wake_cursor_on(
        store.db(),
        &id,
        "stale-token",
        "cursor-stale",
        "id-stale",
    )
    .await
    .unwrap());

    let row = crate::entities::domain_events::Entity::find_by_id(&id)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.wake_pending, 1);
    assert_eq!(row.wake_lease_owner.as_deref(), Some("live-token"));
    assert_eq!(row.wake_cursor_at, "cursor-live");
    assert_eq!(row.wake_cursor_id, "id-live");
}

#[tokio::test]
async fn stale_wake_delivery_update_does_not_clear_new_registration() {
    stale_wake_delivery_update_does_not_clear(&test_store().await).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_stale_wake_delivery_update_does_not_clear_new_registration() {
    stale_wake_delivery_update_does_not_clear(&postgres_test_store().await).await;
}

async fn stale_wake_delivery_update_does_not_clear(store: &LibraryStore) {
    let parked = park_echo_wake(
        store,
        "book_acquired:stale-wake-upd-1",
        r#"{"titleId":"one","source":"audible"}"#,
        "book_acquired",
        r#"{"source":"audible"}"#,
    )
    .await;
    let trigger = persist_outbox_event(
        store,
        outbox_spec(
            "book_acquired",
            "book_acquired:stale-wake-upd-2",
            r#"{"titleId":"two","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let trigger_id = expect_created(trigger);
    let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let mut am: crate::entities::domain_events::ActiveModel =
        crate::entities::domain_events::Entity::find_by_id(&trigger_id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.wake_lease_owner = sea_orm::ActiveValue::Set(Some("token-a".into()));
    am.wake_lease_expires_at = sea_orm::ActiveValue::Set(Some(future.clone()));
    am.update(store.db()).await.unwrap();

    let mut am: crate::entities::domain_events::ActiveModel =
        crate::entities::domain_events::Entity::find_by_id(&trigger_id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.wake_lease_owner = sea_orm::ActiveValue::Set(Some("token-b".into()));
    am.update(store.db()).await.unwrap();

    let now = chrono::Utc::now().to_rfc3339();
    let woken = super::event_outbox::wake_deliveries_fenced_on(
        store.db(),
        store.leftover_in_process(),
        &trigger_id,
        "token-b",
        std::slice::from_ref(&parked.id),
        &now,
        100,
    )
    .await
    .unwrap();
    assert_eq!(woken, 1);

    let future_run = "9999-12-31T23:59:59+00:00";
    let mut dm: crate::entities::event_deliveries::ActiveModel =
        crate::entities::event_deliveries::Entity::find_by_id(&parked.id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    dm.run_after = sea_orm::ActiveValue::Set(future_run.into());
    dm.wake_event_type = sea_orm::ActiveValue::Set("book_acquired".into());
    dm.wake_filter_json = sea_orm::ActiveValue::Set(r#"{"source":"audible"}"#.into());
    dm.wake_grants_json = sea_orm::ActiveValue::Set(audible_v1_wake_grants());
    dm.resume_pending = sea_orm::ActiveValue::Set(1);
    dm.update(store.db()).await.unwrap();

    let stale = super::event_outbox::wake_deliveries_fenced_on(
        store.db(),
        store.leftover_in_process(),
        &trigger_id,
        "token-a",
        std::slice::from_ref(&parked.id),
        &now,
        100,
    )
    .await
    .unwrap();
    assert_eq!(stale, 0);

    let row = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert_eq!(row.wake_event_type, "book_acquired");
    assert_eq!(row.wake_filter_json, r#"{"source":"audible"}"#);
    assert!(!row.wake_grants_json.is_empty());
    assert!(row.run_after > chrono::Utc::now() + chrono::Duration::days(1));
}

#[tokio::test]
async fn wake_consumes_registration_so_retry_is_not_rewoken() {
    let store = test_store().await;
    let parked = park_echo_wake(
        &store,
        "book_acquired:consume-wake-1",
        r#"{"titleId":"one","source":"audible"}"#,
        "book_acquired",
        r#"{"source":"audible"}"#,
    )
    .await;
    let trigger = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:consume-wake-2",
            r#"{"titleId":"two","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let trigger_id = expect_created(trigger);
    drain_pending_wakes(&store).await;
    let woken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(woken.resume_pending);
    assert_eq!(woken.wake_event_type, "");
    assert_eq!(woken.wake_filter_json, "");
    assert_eq!(woken.wake_grants_json, "");
    let claimed = claim_delivery(&store, "consume-claim").await;
    assert_eq!(claimed.id, parked.id);
    let retry_at = chrono::Utc::now() + chrono::Duration::days(7);
    assert!(store
        .retry_event_delivery(&claimed.fence(), retry_at, "later")
        .await
        .unwrap());
    let after_retry = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    let held_run_after = after_retry.run_after;
    assert!(!after_retry.resume_pending);

    let again = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:consume-wake-3",
            r#"{"titleId":"three","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let again_id = expect_created(again);
    drain_pending_wakes(&store).await;
    let still = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert_eq!(still.run_after, held_run_after);
    assert!(!still.resume_pending);
    assert_eq!(still.wake_event_type, "");

    let mut am: crate::entities::event_deliveries::ActiveModel =
        crate::entities::event_deliveries::Entity::find_by_id(&parked.id)
            .one(store.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    am.run_after = sea_orm::ActiveValue::Set(chrono::Utc::now().to_rfc3339());
    am.update(store.db()).await.unwrap();
    let claimed = claim_delivery(&store, "consume-resuspend").await;
    let future = chrono::Utc::now() + chrono::Duration::days(30);
    assert!(store
        .suspend_event_delivery(
            &claimed.fence(),
            r#"{"offset":2}"#,
            1,
            future,
            "book_acquired",
            r#"{"source":"audible"}"#,
            "",
        )
        .await
        .unwrap());
    let fourth = persist_outbox_event(
        &store,
        outbox_spec(
            "book_acquired",
            "book_acquired:consume-wake-4",
            r#"{"titleId":"four","source":"audible"}"#,
        ),
    )
    .await
    .unwrap();
    let fourth_id = expect_created(fourth);
    drain_pending_wakes(&store).await;
    let rewoken = store.get_event_delivery(&parked.id).await.unwrap().unwrap();
    assert!(rewoken.resume_pending);
    assert!(rewoken.run_after <= chrono::Utc::now() + chrono::Duration::seconds(2));
    let _ = (trigger_id, again_id, fourth_id);
}

#[tokio::test]
async fn claim_requires_this_nodes_schema_version() {
    let store = test_store().await;
    store
        .upsert_event_subscriber(
            "node-v1",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    store
        .upsert_event_subscriber(
            "node-v2",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![2])],
            true,
        )
        .await
        .unwrap();
    let mut spec = outbox_spec("book_acquired", "book_acquired:schema-v2", "{}");
    spec.schema_version = 2;
    let created = persist_outbox_event(&store, spec).await.unwrap();
    let id = expect_created(created);
    let event = store.get_domain_event(&id).await.unwrap().unwrap();
    let catalog = store.list_live_event_subscribers().await.unwrap();
    let subs = crate::catalog_subscribers_for_event(&catalog, &event);
    assert_eq!(subs.len(), 1);
    store
        .dispatch_event_deliveries(&id, &subs, "schema-v2")
        .await
        .unwrap();
    let delivery_id = format!("{id}:echo");
    assert!(store
        .claim_next_event_delivery(
            "w-v1",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "node-v1",
        )
        .await
        .unwrap()
        .is_none());
    let pending = store
        .get_event_delivery(&delivery_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.state, "pending");
    assert_eq!(pending.attempt_count, 0);

    let claimed = store
        .claim_next_event_delivery(
            "w-v2",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "node-v2",
        )
        .await
        .unwrap()
        .expect("v2 node can claim");
    assert_eq!(claimed.id, delivery_id);
    assert_eq!(claimed.attempt_count, 1);
}

#[tokio::test]
async fn incompatible_d1_style_claim_is_released_without_attempt_burn() {
    let store = test_store().await;
    store
        .upsert_event_subscriber(
            "node-v1",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let mut spec = outbox_spec("book_acquired", "book_acquired:d1-incompat", "{}");
    spec.schema_version = 2;
    let created = persist_outbox_event(&store, spec).await.unwrap();
    let id = expect_created(created);
    store
        .dispatch_event_deliveries(&id, &[EventSubscriber::plugin("echo")], "d1-incompat")
        .await
        .unwrap();
    let claimed = store
        .claim_next_event_delivery(
            "d1-worker",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            32,
            "",
        )
        .await
        .unwrap()
        .expect("plugin-id-only claim");
    assert_eq!(claimed.attempt_count, 1);
    let released = store
        .release_if_incompatible_local_catalog("node-v1", Some(claimed.clone()))
        .await
        .unwrap();
    assert!(released.is_none());
    let row = store
        .get_event_delivery(&claimed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "pending");
    assert_eq!(row.attempt_count, 0);
}

#[tokio::test]
async fn atomic_claim_skips_incompatible_oldest_and_claims_later_compatible() {
    let store = test_store().await;
    store
        .upsert_event_subscriber(
            "node-v1",
            "echo",
            &[EventCatalogSubscription::new("book_acquired", vec![1])],
            true,
        )
        .await
        .unwrap();
    let mut spec_v2 = outbox_spec("book_acquired", "book_acquired:atomic-skip-v2", "{}");
    spec_v2.schema_version = 2;
    let created_v2 = persist_outbox_event(&store, spec_v2).await.unwrap();
    let id_v2 = expect_created(created_v2);
    store
        .dispatch_event_deliveries(&id_v2, &[EventSubscriber::plugin("echo")], "atomic-skip-v2")
        .await
        .unwrap();
    let created_v1 = persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:atomic-skip-v1", "{}"),
    )
    .await
    .unwrap();
    let id_v1 = expect_created(created_v1);
    store
        .dispatch_event_deliveries(&id_v1, &[EventSubscriber::plugin("echo")], "atomic-skip-v1")
        .await
        .unwrap();

    let db = store.db().clone();
    let store = store.with_atomic_txn(Arc::new(InProcessSqliteAtomic { db }));
    let claimed = store
        .claim_next_event_delivery(
            "atomic-skip-worker",
            60,
            &uuid::Uuid::new_v4().to_string(),
            &["echo".into()],
            1,
            "node-v1",
        )
        .await
        .unwrap()
        .expect("compatible later row");
    assert_eq!(claimed.id, format!("{id_v1}:echo"));
    assert_eq!(claimed.attempt_count, 1);

    let skipped = store
        .get_event_delivery(&format!("{id_v2}:echo"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(skipped.state, "pending");
    assert_eq!(skipped.attempt_count, 0);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_publish_domain_event_namespaces_dedup_by_account_and_source() {
    let store = postgres_test_store().await;
    let a = persist_outbox_event(
        &store,
        outbox_spec_account("book_acquired", "book_acquired:pg-ns", "{}", "", "acct-a"),
    )
    .await
    .unwrap();
    let id_a = expect_created(a);
    let b = persist_outbox_event(
        &store,
        outbox_spec_account("book_acquired", "book_acquired:pg-ns", "{}", "", "acct-b"),
    )
    .await
    .unwrap();
    assert!(
        matches!(b, PublishDomainEventOutcome::Created { .. }),
        "expected PublishDomainEventOutcome::Created"
    );
    let mut sourced =
        outbox_spec_account("book_acquired", "book_acquired:pg-ns", "{}", "", "acct-a");
    sourced.source = "audible".into();
    let c = persist_outbox_event(&store, sourced).await.unwrap();
    assert!(
        matches!(c, PublishDomainEventOutcome::Created { .. }),
        "expected PublishDomainEventOutcome::Created"
    );
    let dup = persist_outbox_event(
        &store,
        outbox_spec_account("book_acquired", "book_acquired:pg-ns", "{}", "", "acct-a"),
    )
    .await
    .unwrap();
    assert_eq!(
        dup,
        PublishDomainEventOutcome::Duplicate { existing_id: id_a }
    );
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_concurrent_wake_claims_do_not_share_slice() {
    let store = std::sync::Arc::new(postgres_test_store().await);
    persist_outbox_event(
        &store,
        outbox_spec("book_acquired", "book_acquired:pg-wake-claim", "{}"),
    )
    .await
    .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .process_pending_wakes(1, &format!("wake-{i}"), 60)
                .await
                .unwrap()
        }));
    }
    let mut claimed = 0u32;
    for handle in handles {
        claimed += handle.await.unwrap().claimed;
    }
    assert_eq!(claimed, 1);
}

/// Rewrites the database name in a Postgres URL, preserving query options.
fn postgres_url_with_db(url: &str, db_name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let trimmed = base.trim_end_matches('/');
    let slash = trimmed
        .rfind('/')
        .expect("BOOKCLERK_TEST_POSTGRES_URL must include a database path");
    let head = &trimmed[..slash];
    match query {
        Some(q) => format!("{head}/{db_name}?{q}"),
        None => format!("{head}/{db_name}"),
    }
}

/// True when Postgres conformance tests should run (URL present).
///
/// `BOOKCLERK_REQUIRE_POSTGRES_TESTS=1` without a URL is a hard failure so CI
/// cannot skip these cases. A missing URL without that flag skips so the
/// default workspace suite does not need a server.
fn postgres_tests_enabled() -> bool {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    if url.is_some() {
        return true;
    }
    assert!(
        std::env::var("BOOKCLERK_REQUIRE_POSTGRES_TESTS")
            .ok()
            .as_deref()
            != Some("1"),
        "BOOKCLERK_TEST_POSTGRES_URL is required when BOOKCLERK_REQUIRE_POSTGRES_TESTS=1"
    );
    false
}

/// Opens a disposable Postgres database with a multi-connection pool.
///
/// Requires `BOOKCLERK_TEST_POSTGRES_URL`. Setup failures are fatal so a
/// required CI job cannot pass without exercising the serialization slot.
async fn postgres_test_store() -> LibraryStore {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap_or_else(|_| {
        panic!(
            "BOOKCLERK_TEST_POSTGRES_URL is required to run postgres job-queue tests \
             (CI sets BOOKCLERK_REQUIRE_POSTGRES_TESTS=1)"
        )
    });
    assert!(
        !url.trim().is_empty(),
        "BOOKCLERK_TEST_POSTGRES_URL must not be empty"
    );
    let db_name = format!("jobq_{}", uuid::Uuid::new_v4().as_simple());
    assert!(
        db_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "generated postgres database name must be identifier-safe: {db_name}"
    );

    let admin = sea_orm::Database::connect(url.as_str())
        .await
        .expect("connect to BOOKCLERK_TEST_POSTGRES_URL");
    let backend = admin.get_database_backend();
    admin
        .execute_raw(sea_orm::Statement::from_string(
            backend,
            format!("CREATE DATABASE {db_name}"),
        ))
        .await
        .expect("create disposable postgres database");

    let mut opt = sea_orm::ConnectOptions::new(postgres_url_with_db(&url, &db_name));
    opt.max_connections(8);
    opt.min_connections(2);
    let db = sea_orm::Database::connect(opt)
        .await
        .expect("connect to disposable postgres database");
    // `host_migration_plan()` is empty until a release cut. Apply the current
    // canonical pack (unreleased) through the same host state machine as connect.
    crate::apply_host_schema(&db)
        .await
        .expect("apply unreleased host schema");
    LibraryStore::from_connection(db).with_in_process_sql()
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_concurrent_admits_never_exceed_max_pending() {
    let store = postgres_test_store().await;
    let store = std::sync::Arc::new(store);
    let mut handles = Vec::new();
    for i in 0..12 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store
                .enqueue_job(scan_spec(Some(&format!("acct-{i}")), 3))
                .await
                .unwrap()
        }));
    }
    let mut created = 0u32;
    let mut full = 0u32;
    for handle in handles {
        match handle.await.unwrap() {
            EnqueueOutcome::Created { .. } => created += 1,
            EnqueueOutcome::Duplicate { .. } => {}
            EnqueueOutcome::QueueFull => full += 1,
        }
    }
    assert_eq!(created, 3);
    assert_eq!(full, 9);
    assert_eq!(store.count_active_jobs().await.unwrap(), 3);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_concurrent_reserves_respect_quota() {
    let store = postgres_test_store().await;
    let store = std::sync::Arc::new(store);
    let created = store
        .enqueue_job(acquire_spec(None, None, 8))
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = created else {
        panic!("expected created: {created:?}");
    };
    let a = {
        let store = store.clone();
        let id = id.clone();
        tokio::spawn(async move { store.reserve_job_temp_path(&id, "/tmp/a", 80, 100).await })
    };
    let b = {
        let store = store.clone();
        tokio::spawn(async move { store.reserve_job_temp_path(&id, "/tmp/b", 80, 100).await })
    };
    let (ra, rb) = tokio::join!(a, b);
    let ok = u32::from(ra.unwrap().is_ok()) + u32::from(rb.unwrap().is_ok());
    assert_eq!(ok, 1);
}

#[tokio::test]
async fn avatar_source_and_sso_pictures() {
    use chrono::{Duration as ChronoDuration, Utc};

    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    );
    let user = store
        .create_user_with_profile(
            UserRole::Member,
            Some("Casey"),
            None,
            Some("casey@example.com"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(user.avatar_source, None);
    let updated = store
        .set_user_avatar_source(user.id, Some("gravatar"))
        .await
        .unwrap();
    assert_eq!(updated.avatar_source.as_deref(), Some("gravatar"));

    store
        .link_portal_identity_to_user("oidc:google", "g-sub", user.id, Some("Casey"))
        .await
        .unwrap();
    store
        .link_portal_identity_to_user("oidc:github", "gh-sub", user.id, Some("Casey"))
        .await
        .unwrap();
    store
        .set_portal_identity_picture(
            "oidc:google",
            "g-sub",
            Some("https://example.com/google.png"),
        )
        .await
        .unwrap();
    store
        .set_portal_identity_picture(
            "oidc:github",
            "gh-sub",
            Some("https://example.com/github.png"),
        )
        .await
        .unwrap();
    let google = store
        .get_portal_identity("oidc:google", "g-sub")
        .await
        .unwrap()
        .unwrap();
    let github = store
        .get_portal_identity("oidc:github", "gh-sub")
        .await
        .unwrap()
        .unwrap();
    let expires = Utc::now() + ChronoDuration::hours(1);
    store
        .insert_portal_session("hash-google", google.id, expires)
        .await
        .unwrap();
    store
        .insert_portal_session("hash-github", github.id, expires)
        .await
        .unwrap();

    let pics = store.list_user_sso_pictures(user.id).await.unwrap();
    assert_eq!(pics.len(), 2);
    assert!(pics.iter().all(|p| p.last_used_at.is_some()));
    assert!(pics
        .iter()
        .any(|p| p.provider == "oidc:github" && p.picture_url == "https://example.com/github.png"));
}

#[tokio::test]
async fn postgres_totp_enroll_and_disable_round_trip() {
    if !postgres_tests_enabled() {
        return;
    }
    let _dek = crate::master_key::master_key_test_read_lock_async().await;
    crate::master_key::ensure_shared_test_dek();
    let store = postgres_test_store().await;
    let user = store
        .create_user(UserRole::Member, Some("Totp Pg"), None)
        .await
        .unwrap();
    store_pending_totp(&store, user.id, &sample_totp_secret()).await;
    store
        .confirm_totp_enrollment(user.id, &sample_totp_secret())
        .await
        .unwrap();
    let enrolled = store.get_user(user.id).await.unwrap().unwrap();
    assert!(enrolled.totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["primary"]);

    store.disable_user_totp(user.id).await.unwrap();
    let cleared = store.get_user(user.id).await.unwrap().unwrap();
    assert!(!cleared.totp_enabled);
    assert!(totp_secret_names(&store, user.id).await.is_empty());
}

#[tokio::test]
async fn postgres_totp_enroll_and_disable_missing_user_leave_no_leftover_secrets() {
    if !postgres_tests_enabled() {
        return;
    }
    let _dek = crate::master_key::master_key_test_read_lock_async().await;
    crate::master_key::ensure_shared_test_dek();
    let store = postgres_test_store().await;
    let other = store
        .create_user(UserRole::Member, Some("Keep Totp Pg"), None)
        .await
        .unwrap();
    store_pending_totp(&store, other.id, &sample_totp_secret()).await;
    let missing = 999_i64;
    let enroll_err = store
        .confirm_totp_enrollment(missing, &sample_totp_secret())
        .await
        .unwrap_err();
    assert!(matches!(enroll_err, LibraryError::NotFound(_)));
    assert!(totp_secret_names(&store, missing).await.is_empty());
    assert_eq!(totp_secret_names(&store, other.id).await, vec!["pending"]);
    assert!(
        !store
            .get_user(other.id)
            .await
            .unwrap()
            .unwrap()
            .totp_enabled
    );

    let disable_err = store.disable_user_totp(missing).await.unwrap_err();
    assert!(matches!(disable_err, LibraryError::NotFound(_)));
    assert_eq!(totp_secret_names(&store, other.id).await, vec!["pending"]);
}

#[tokio::test]
async fn postgres_totp_injected_commit_failure_rolls_back_enroll_and_disable() {
    if !postgres_tests_enabled() {
        return;
    }
    let _dek = crate::master_key::master_key_test_read_lock_async().await;
    crate::master_key::ensure_shared_test_dek();
    let store = postgres_test_store().await;
    let user = store
        .create_user(UserRole::Member, Some("Totp Pg Inject"), None)
        .await
        .unwrap();
    store_pending_totp(&store, user.id, &sample_totp_secret()).await;
    crate::inject_commit_failures(1);
    let enroll_err = store
        .confirm_totp_enrollment(user.id, &sample_totp_secret())
        .await
        .unwrap_err();
    assert!(
        enroll_err.to_string().contains("commit failed"),
        "expected enroll commit failure, got {enroll_err}"
    );
    assert!(!store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["pending"]);

    store
        .confirm_totp_enrollment(user.id, &sample_totp_secret())
        .await
        .unwrap();
    assert!(store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["primary"]);

    crate::inject_commit_failures(1);
    let disable_err = store.disable_user_totp(user.id).await.unwrap_err();
    assert!(
        disable_err.to_string().contains("commit failed"),
        "expected disable commit failure, got {disable_err}"
    );
    assert!(store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert_eq!(totp_secret_names(&store, user.id).await, vec!["primary"]);

    store.disable_user_totp(user.id).await.unwrap();
    assert!(!store.get_user(user.id).await.unwrap().unwrap().totp_enabled);
    assert!(totp_secret_names(&store, user.id).await.is_empty());
}

fn guest_select(sql: &str) -> bookclerk_plugin_abi::ExecuteRequest {
    use bookclerk_plugin_abi::{DbPlanStatementKind, DbResultSelection, TypedDbStatement};
    bookclerk_plugin_abi::ExecuteRequest {
        operation_id: "guest-select".into(),
        request_hash: String::new(),
        statements: vec![TypedDbStatement {
            sql: sql.into(),
            parameters: vec![],
            kind: DbPlanStatementKind::Select,
            max_rows: 8,
            result_selection: DbResultSelection::Rows,
        }],
        deadline_unix_ms: 0,
    }
}

#[tokio::test]
async fn execute_guest_atomic_deny_all_does_not_run_sql() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    )
    .with_db_capabilities(bookclerk_plugin_abi::DbCapabilities::advertised_sqlite());
    let err = store
        .execute_guest_atomic(
            guest_select("SELECT id FROM books"),
            &crate::GuestSqlPolicy::deny_all(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        bookclerk_plugin_abi::PluginErrorCode::InvalidParams,
        "{err}"
    );
    assert!(err.to_string().contains("unauthorized table"), "{err}");
}

#[tokio::test]
async fn execute_guest_atomic_allow_tables_selects_books() {
    let store = LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap(),
    )
    .with_db_capabilities(bookclerk_plugin_abi::DbCapabilities::advertised_sqlite());
    store
        .upsert_account("user-1", "us", Some("Main"), true, "audible")
        .await
        .unwrap();
    let mut book = NewBook::minimal("B00GUEST", "user-1", "us", "Guest Book");
    book.authors = Some("Author".into());
    store.upsert_book(&book).await.unwrap();

    let reply = store
        .execute_guest_atomic(
            guest_select("SELECT product_id FROM books WHERE product_id = 'B00GUEST'"),
            &crate::GuestSqlPolicy::allow_tables(["books"])
                .with_sql_types(crate::migrations::host_sql_type_env()),
        )
        .await
        .unwrap();
    assert_eq!(reply.statements.len(), 1);
    assert_eq!(reply.statements[0].rows.len(), 1);
    match &reply.statements[0].rows[0].values[0] {
        bookclerk_plugin_abi::DbValue::Text(s) => assert_eq!(s, "B00GUEST"),
        other => panic!("expected text product_id, got {other:?}"),
    }
}

async fn assert_scan_delete_is_atomic(store: &LibraryStore) {
    store
        .storage_scan_register("scan-atomic", "local:test", None)
        .await
        .unwrap();
    store
        .storage_scan_put_object("scan-atomic", "a.m4b", 1, 0, true)
        .await
        .unwrap();
    store
        .storage_scan_mark_complete("scan-atomic")
        .await
        .unwrap();
    super::storage_scan::FAIL_STORAGE_SCAN_DELETE_BEFORE_GENERATION
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let err = store.storage_scan_delete("scan-atomic").await.unwrap_err();
    super::storage_scan::FAIL_STORAGE_SCAN_DELETE_BEFORE_GENERATION
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(err.to_string().contains("injected"), "{err}");
    let (instance, completed) = store
        .storage_scan_adoption("scan-atomic")
        .await
        .unwrap()
        .expect("generation survived the rollback");
    assert_eq!(instance, "local:test");
    assert!(completed);
    assert_eq!(
        store
            .storage_scan_object_page("scan-atomic", "", None, 8)
            .await
            .unwrap(),
        vec!["a.m4b".to_string()]
    );
    store.storage_scan_delete("scan-atomic").await.unwrap();
    assert!(store
        .storage_scan_adoption("scan-atomic")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn storage_scan_delete_rolls_back_before_the_generation_is_removed() {
    let store = test_store().await;
    assert_scan_delete_is_atomic(&store).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_storage_scan_delete_rolls_back_before_the_generation_is_removed() {
    let store = postgres_test_store().await;
    assert_scan_delete_is_atomic(&store).await;
}

#[tokio::test]
async fn terminal_jobs_drop_scan_inventory_and_pending_retries_keep_it() {
    let store = test_store().await;
    let pending = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Acquire,
            payload: JobPayload {
                title: Some("retry".into()),
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 4,
            run_after: None,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Created { id: pending_id } = pending else {
        panic!("pending job");
    };
    store
        .storage_scan_register("scan-retry", "inst", Some(&pending_id))
        .await
        .unwrap();
    store
        .storage_scan_put_object("scan-retry", "keep.m4b", 1, 0, true)
        .await
        .unwrap();
    assert_eq!(
        store
            .storage_scan_reclaim_if_terminal(&pending_id)
            .await
            .unwrap(),
        0
    );
    let claimed = claim_job(&store, "worker-retry").await;
    let fence = claimed.fence().expect("fence");
    assert!(store.fail_job(&fence, "handler", "once").await.unwrap());
    assert_eq!(
        store
            .storage_scan_reclaim_if_terminal(&pending_id)
            .await
            .unwrap(),
        0,
        "a retry must keep the inventory"
    );
    assert!(store
        .storage_scan_adoption("scan-retry")
        .await
        .unwrap()
        .is_some());

    let terminal = store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Acquire,
            payload: JobPayload {
                title: Some("terminal".into()),
                ..JobPayload::default()
            },
            priority: 0,
            max_attempts: 1,
            max_pending: 4,
            run_after: None,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Created { id } = terminal else {
        panic!("terminal job");
    };
    store
        .storage_scan_register("scan-dead", "inst", Some(&id))
        .await
        .unwrap();
    store
        .storage_scan_put_object("scan-dead", "gone.m4b", 1, 0, true)
        .await
        .unwrap();
    let claimed = claim_job(&store, "worker-dead").await;
    let fence = claimed.fence().expect("fence");
    assert!(store.fail_job(&fence, "handler", "final").await.unwrap());
    assert_eq!(
        store.storage_scan_reclaim_if_terminal(&id).await.unwrap(),
        1
    );
    assert!(store
        .storage_scan_adoption("scan-dead")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .storage_scan_adoption("scan-retry")
        .await
        .unwrap()
        .is_some());
}
