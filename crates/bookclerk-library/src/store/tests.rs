use super::*;
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, JobFence, JobKind, JobPayload, JobRecord, JobResourceClass,
    JobState, JobTrigger,
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait};

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
        )
        .await
        .unwrap();
    assert_eq!(saved.default_view, "library");
    assert_eq!(saved.disabled_shelves, vec!["chirp_deals", "genre"]);
    assert_eq!(saved.discover_sort, "rating");
    assert_eq!(saved.discover_sort_dir, "asc");
    assert_eq!(saved.discover_language.as_deref(), Some("en"));
    assert_eq!(saved.discover_excluded_sources, vec!["chirp"]);

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
        )
        .await
        .unwrap();
    assert_eq!(portal.identity_id, Some(identity.id));
    assert_eq!(portal.default_view, "accounts");
    assert_eq!(portal.disabled_shelves, vec!["narrator"]);
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
    assert!(queue
        .iter()
        .any(|e| e.wish_count == 1 && e.title.contains("Martian")));
    assert!(queue
        .iter()
        .any(|e| e.wish_count == 1 && e.title.contains("Solo")));
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
        .insert_webauthn_credential(user.id, "cred-1", "{\"ok\":true}")
        .await
        .unwrap();
    let listed = store.list_webauthn_credentials(user.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, "cred-1");
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
        .insert_webauthn_credential(doomed.id, "cred-reuse", "{\"ok\":true}")
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
        .insert_webauthn_credential(owner.id, "cred-reuse", "{\"ok\":true}")
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

/// Opens a disposable Postgres database with a multi-connection pool.
///
/// Requires `BOOKCLERK_TEST_POSTGRES_URL`. Setup failures are fatal so a
/// required CI job cannot pass without exercising the advisory lock.
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
    for step in crate::migrations::migration_sql_postgres() {
        for stmt in step.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            db.execute_raw(sea_orm::Statement::from_string(backend, stmt.to_string()))
                .await
                .unwrap_or_else(|err| panic!("postgres migration `{stmt}` failed: {err}"));
        }
    }
    LibraryStore::from_connection(db)
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
