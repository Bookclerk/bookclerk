#[cfg(test)]
mod integration {
    use bookclerk_discover::{
        embed_dirty_works, rebuild_works_from_library, recommend, HashEmbedder, RecommendOptions,
    };
    use bookclerk_library::{NewBook, NewTitleRequest, RequestStatus};

    #[tokio::test]
    async fn works_embed_and_recommend_series_gap() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("acct", "us", Some("Main"), true, "audible")
            .await
            .unwrap();

        let mut b1 = NewBook::minimal("B000001", "acct", "us", "Series Book 1");
        b1.authors = Some("Ada Author".into());
        b1.series = Some("Test Series".into());
        b1.series_index = Some("1".into());
        b1.categories = Some("Fantasy".into());
        let b1 = store.upsert_book(&b1).await.unwrap();
        store
            .update_user_fields(
                &b1.uuid,
                "acct",
                &bookclerk_library::UserBookFields {
                    is_finished: Some(true),
                    rating_overall: Some(5.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // A second owned book in another series so we have embedding seeds,
        // plus an unowned-looking work created only via request / second account.
        let mut b2 = NewBook::minimal("B000002", "acct", "us", "Series Book 2");
        b2.authors = Some("Ada Author".into());
        b2.series = Some("Test Series".into());
        b2.series_index = Some("2".into());
        b2.categories = Some("Fantasy".into());
        store.upsert_book(&b2).await.unwrap();

        // Simulate a catalog-only candidate by creating a work without ownership:
        // use a second account "wishlist" book then we still treat it as owned.
        // Instead, open a title request for book 3.
        store
            .create_title_request(&NewTitleRequest {
                uuid: None,
                identity_id: None,
                title: "Series Book 3".into(),
                authors: Some("Ada Author".into()),
                asin: Some("B000003".into()),
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

        let linked = rebuild_works_from_library(&store).await.unwrap();
        assert!(linked >= 2);

        let mut embedder = HashEmbedder::new(32);
        let n = embed_dirty_works(&store, &mut embedder).await.unwrap();
        assert!(n >= 1);

        let opts = RecommendOptions {
            limit: 10,
            embedding_model: String::from("local-hash-v1"),
            region: String::from("us"),
            include_purchase_hints: false,
            external_user_id: None,
            include_listening: true,
            listening_providers: Vec::new(),
            fetch_storefront_candidates: false,
            storefront_seed_limit: 0,
            storefront_max_remote_calls: 0,
            exclude_graphicaudio_series_sets: false,
            disabled_shelves: Vec::new(),
            models_dir: None,
            embed_intra_threads: 1,
            embeddings_enabled: true,
        };
        let registry = bookclerk_source::SourceRegistry::new();
        let recs = recommend(&store, &registry, &opts).await.unwrap();
        assert!(
            recs.iter()
                .any(|r| r.from_request && r.title.contains("Book 3")),
            "expected open request in recommendations: {recs:?}"
        );
    }
}
