use super::*;

#[tokio::test]
async fn account_and_book_roundtrip() {
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
async fn user_preferences_roundtrip_operator_and_portal() {
    use crate::models::{portal_prefs_key, OPERATOR_PREFS_KEY};

    let store = LibraryStore::open_in_memory().await.unwrap();
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
        )
        .await
        .unwrap();
    assert_eq!(saved.default_view, "library");
    assert_eq!(saved.disabled_shelves, vec!["chirp_deals", "genre"]);

    let again = store
        .get_user_preferences(OPERATOR_PREFS_KEY)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.default_view, "library");
    assert_eq!(again.disabled_shelves.len(), 2);

    let identity = store
        .upsert_portal_identity("audiobookshelf", "usr_prefs", Some("alice"))
        .await
        .unwrap();
    let key = portal_prefs_key(identity.id);
    let portal = store
        .upsert_user_preferences(&key, Some(identity.id), "accounts", &["narrator".into()])
        .await
        .unwrap();
    assert_eq!(portal.identity_id, Some(identity.id));
    assert_eq!(portal.default_view, "accounts");
    assert_eq!(portal.disabled_shelves, vec!["narrator"]);

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
    let store = LibraryStore::open_in_memory().await.unwrap();
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
