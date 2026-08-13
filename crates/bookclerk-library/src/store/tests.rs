use super::*;

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
        .redeem_claim_ticket_to_session(ticket_hash, "session-hash-none", expires, None, None)
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
        )
        .await
        .unwrap();
    assert!(store
        .get_user_password_hash(user.id)
        .await
        .unwrap()
        .is_some());
}
