//! Integration tests: recommend flow (bookclerk-discover).

#[cfg(test)]
mod integration {
    use bookclerk_discover::{
        embed_dirty_works, open_embedder, recommend, serving_space, Embedder, RecommendOptions,
        VectorSource,
    };
    use bookclerk_library::{NewBook, NewTitleRequest, RequestStatus, VectorSpace};

    #[tokio::test]
    async fn works_embed_and_recommend_series_gap() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("audible", "acct", "us", Some("Main"), true, "audible")
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
                &b1.holding_id,
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
        let request = store
            .create_title_request(&NewTitleRequest {
                uuid: None,
                identity_id: None,
                title: "Series Book 3".into(),
                authors: Some("Ada Author".into()),
                asin: Some("B000003".into()),
                isbn: None,
                notes: None,
                status: RequestStatus::Open,
                resolved_edition_id: None,
                cover_url: Some("https://img.example/book-3.jpg".into()),
            })
            .await
            .unwrap();

        // Every upserted title is resolved into a work as it is stored.
        assert!(store.list_works().await.unwrap().len() >= 2);

        let mut embedder = open_embedder();
        let pass = embed_dirty_works(&store, embedder.as_mut(), None)
            .await
            .unwrap();
        assert!(pass.finished && pass.written >= 1);

        let opts = RecommendOptions {
            limit: 10,
            vectors: VectorSource::stand_in(),
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
            embeddings_enabled: true,
        };
        let registry = bookclerk_source::SourceRegistry::new();
        let recs = recommend(&store, &registry, &opts).await.unwrap();
        let requested = recs
            .iter()
            .find(|r| r.from_request && r.title.contains("Book 3"))
            .unwrap_or_else(|| panic!("expected open request in recommendations: {recs:?}"));
        // The card links back to the request it came from.
        assert_eq!(
            requested.request_uuid.as_deref(),
            Some(request.uuid.as_str())
        );
        // And carries what the request recorded about the work.
        assert_eq!(requested.authors.as_deref(), Some("Ada Author"));
        assert_eq!(requested.asin.as_deref(), Some("B000003"));
        assert_eq!(
            requested.cover_url.as_deref(),
            Some("https://img.example/book-3.jpg")
        );
        assert_eq!(requested.wish_count, 1);
        assert!(requested.score > 0.0, "{requested:?}");
        assert!(!requested.reasons.is_empty(), "{requested:?}");
        assert!(!requested.work_key.is_empty(), "{requested:?}");
    }

    /// Stands for a model: every vector shares one large component, as a
    /// real model's vectors do, and differs by what the text is about.
    struct TopicModel {
        /// The space it claims.
        space: VectorSpace,
        /// Texts embedded so far.
        embedded: usize,
    }

    #[async_trait::async_trait]
    impl Embedder for TopicModel {
        fn space(&self) -> &VectorSpace {
            &self.space
        }

        fn dimensions(&self) -> usize {
            4
        }

        async fn embed(
            &mut self,
            texts: &[String],
        ) -> Result<Vec<Vec<f32>>, bookclerk_discover::DiscoverError> {
            self.embedded += texts.len();
            Ok(texts
                .iter()
                .map(|text| {
                    let dragons = text.to_lowercase().contains("dragon");
                    // A little of the text itself, so no two are equal.
                    let own = (text.len() % 7) as f32 * 0.01;
                    if dragons {
                        vec![0.8, 0.5, 0.1, own]
                    } else {
                        vec![0.8, 0.1, 0.5, own]
                    }
                })
                .collect())
        }
    }

    /// Opens a wishlist entry titled `title`.
    async fn wish(store: &bookclerk_library::LibraryStore, title: &str, asin: &str) {
        store
            .create_title_request(&NewTitleRequest {
                uuid: None,
                identity_id: None,
                title: title.into(),
                authors: Some("Someone Else".into()),
                asin: Some(asin.into()),
                isbn: None,
                notes: None,
                status: RequestStatus::Open,
                resolved_edition_id: None,
                cover_url: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_models_vectors_rank_by_standing_not_by_a_fixed_threshold() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("audible", "acct", "us", Some("Main"), true, "audible")
            .await
            .unwrap();
        let mut held = NewBook::minimal("B000001", "acct", "us", "A Dragon's Hoard");
        held.authors = Some("Ada Author".into());
        store.upsert_book(&held).await.unwrap();
        let reader = store
            .create_user(bookclerk_library::UserRole::Member, Some("Reader"), None)
            .await
            .unwrap();
        let held_work = store.list_works().await.unwrap().remove(0).id;
        store
            .set_user_title_state(
                reader.id,
                &held_work,
                &bookclerk_library::UserBookFields {
                    is_finished: Some(true),
                    rating_overall: Some(5.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for i in 0..5 {
            wish(
                &store,
                &format!("Dragon Rider {i}"),
                &format!("B0DRAGON{i}"),
            )
            .await;
        }
        for i in 0..9 {
            wish(
                &store,
                &format!("Ledgers and Audits {i}"),
                &format!("B0LEDGER{i}"),
            )
            .await;
        }

        let mut model = TopicModel {
            space: VectorSpace {
                model: "@embedder/topic".into(),
                revision: "r1".into(),
                packs: String::new(),
            },
            embedded: 0,
        };
        let space = model.space.clone();
        // Before the model has embedded the library, the stand-in serves.
        assert_ne!(serving_space(&store, &space).await.unwrap(), space);
        embed_dirty_works(&store, &mut model, None).await.unwrap();
        assert_eq!(serving_space(&store, &space).await.unwrap(), space);
        let works = model.embedded;

        let opts = RecommendOptions {
            limit: 50,
            vectors: VectorSource::new(space.clone(), Some(Box::new(model))),
            include_purchase_hints: false,
            fetch_storefront_candidates: false,
            ..RecommendOptions::default()
        };
        let registry = bookclerk_source::SourceRegistry::new();
        let recs = recommend(&store, &registry, &opts).await.unwrap();
        let similar = |title: &str| {
            recs.iter()
                .filter(|rec| rec.title.starts_with(title))
                .map(|rec| {
                    rec.reasons
                        .iter()
                        .any(|reason| reason.starts_with("similar to your books and wishes"))
                })
                .collect::<Vec<bool>>()
        };
        // Every text is closer than 0.9 to the taste, as with a real
        // model; only the ones that stand out count as similar.
        assert_eq!(similar("Dragon Rider"), vec![true; 5]);
        assert_eq!(similar("Ledgers and Audits"), vec![false; 9]);
        let best = &recs[0];
        assert!(best.title.starts_with("Dragon Rider"), "{best:?}");

        // The texts were embedded once and stored: a second ranking with
        // no embedder at all gives the same similar titles.
        let stored_only = RecommendOptions {
            vectors: VectorSource::new(space.clone(), None),
            ..opts.clone()
        };
        let again = recommend(&store, &registry, &stored_only).await.unwrap();
        let similar_again: Vec<&str> = again
            .iter()
            .filter(|rec| {
                rec.reasons
                    .iter()
                    .any(|reason| reason.starts_with("similar to your books and wishes"))
            })
            .map(|rec| rec.title.as_str())
            .collect();
        assert_eq!(similar_again.len(), 5);
        // Every work was embedded once, before the ranking.
        assert_eq!(works, store.list_works().await.unwrap().len());

        // Similarity turned off reads no vector.
        let off = RecommendOptions {
            embeddings_enabled: false,
            ..opts.clone()
        };
        let plain = recommend(&store, &registry, &off).await.unwrap();
        assert!(plain.iter().all(|rec| {
            !rec.reasons
                .iter()
                .any(|reason| reason.starts_with("similar to your books and wishes"))
        }));
    }
}
