//! Discovery: recommendations, embeddings, listening sync, wishlist.

use anyhow::Result;
use bookclerk_config::Config;
use bookclerk_library::{LibraryStore, NewTitleRequest, RequestStatus};
use clap::Subcommand;

use crate::format_out::{self, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk discover` verbs: recommend, enrich, embed, listening sync, wishlist.
pub enum DiscoverCommand {
    /// Optionally enrich + embed works, then print recommendations.
    Recommend {
        /// Max recommendations to print.
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Skip purchase-hint HTTP lookups.
        #[arg(long)]
        no_purchase_hints: bool,
        /// Filter listening signals to one external user id (any listening integration).
        #[arg(long)]
        user: Option<String>,
        /// Ignore listening_progress entirely (owned-library taste only).
        #[arg(long)]
        no_listening: bool,
        /// Only use listening rows from these integration ids (repeatable).
        #[arg(long = "listening-provider", value_name = "ID")]
        listening_providers: Vec<String>,
    },
    /// Fill metadata gaps (description, subjects, cover, language) from the
    /// deployed metadata-provider plugins, such as Open Library.
    #[command(alias = "enrich-openlibrary")]
    EnrichMetadata,
    /// Embed dirty works.
    Embed {
        /// Force the local-hash embedder (no ONNX download).
        #[arg(long)]
        hash: bool,
    },
    /// Sync listening progress from all capable integrations into the library DB.
    SyncListening,
    /// Personal wishlist helpers (also feed the shared global queue).
    Wishlist {
        #[command(subcommand)]
        /// Nested wishlist verb (add, list, remove).
        command: WishlistCommand,
    },
}

#[derive(Debug, Subcommand)]
/// Personal wishlist helpers that also feed the shared global queue.
pub enum WishlistCommand {
    /// Add an open wishlist item.
    Add {
        /// Display title for the wishlist row (required positional).
        title: String,
        #[arg(long)]
        /// Optional author list stored on the request.
        authors: Option<String>,
        #[arg(long)]
        /// Optional Audible ASIN used for identity matching.
        asin: Option<String>,
        #[arg(long)]
        /// Optional ISBN-13 used for identity matching.
        isbn: Option<String>,
        #[arg(long)]
        /// Optional operator notes stored on the request.
        notes: Option<String>,
    },
    /// List open wishlist items (operator-owned when no portal identity).
    List,
    /// Un-wishlist by uuid (sets status to cancelled).
    Remove {
        /// Wishlist entry UUID to cancel.
        uuid: String,
    },
}

/// Dispatches a discover verb against the library store and source registry.
pub async fn run(cfg: &Config, format: OutputFormat, command: DiscoverCommand) -> Result<()> {
    let host = crate::registry::open_host(cfg).await?;
    let library = host.store.clone();
    let registry = crate::registry::sources(&host).await?;

    match command {
        DiscoverCommand::EnrichMetadata => {
            let providers = crate::registry::integrations(&host).await?;
            if providers.metadata_providers().is_empty() {
                eprintln!("no metadata-provider plugin is deployed; nothing to ask");
            }
            let n = bookclerk_discover::enrich_books_from_metadata(
                &library,
                &providers,
                &bookclerk_discover::MetadataEnrichOptions::default(),
            )
            .await?;
            format_out::line(format!("metadata providers filled {n} book(s)"));
        }
        DiscoverCommand::Embed { hash } => {
            let n = embed_works(cfg, &library, !hash && cfg.discovery.embeddings_enabled).await?;
            format_out::line(format!("embedded {n} work(s)"));
        }
        DiscoverCommand::SyncListening => {
            let summary = sync_listening(cfg, &library).await?;
            format_out::emit(format, &summary, || {
                if summary.by_provider.is_empty() {
                    println!("(no listening-capable integrations enabled)");
                } else {
                    println!("upserted {} listening row(s)", summary.upserted);
                    for p in &summary.by_provider {
                        if let Some(err) = &p.error {
                            println!("  {}: error — {err}", p.id);
                        } else {
                            println!("  {}: {}", p.id, p.upserted);
                        }
                    }
                }
            })?;
        }
        DiscoverCommand::Recommend {
            limit,
            no_purchase_hints,
            user,
            no_listening,
            listening_providers,
        } => {
            let prefer_onnx = cfg.discovery.embeddings_enabled;
            let mut embedder = bookclerk_discover::open_embedder(
                &cfg.paths().models_dir,
                cfg.discovery.embed_intra_threads,
                prefer_onnx,
            )?;
            let model_id = embedder.model_id().to_string();

            // Enrich first, so the works it changes are embedded this run.
            let providers = crate::registry::integrations(&host).await?;
            if !providers.metadata_providers().is_empty() {
                if let Err(err) = bookclerk_discover::enrich_books_from_metadata(
                    &library,
                    &providers,
                    &bookclerk_discover::MetadataEnrichOptions::default(),
                )
                .await
                {
                    tracing::warn!(error = %err, "metadata enrichment failed");
                }
            }
            let _ = bookclerk_discover::embed_dirty_works(&library, embedder.as_mut()).await?;

            let operator_prefs = library
                .get_user_preferences_or_default(bookclerk_library::OPERATOR_PREFS_KEY, None)
                .await
                .unwrap_or_else(|_| {
                    bookclerk_library::UserPreferences::defaults_for(
                        bookclerk_library::OPERATOR_PREFS_KEY,
                        None,
                    )
                });

            let opts = bookclerk_discover::RecommendOptions {
                limit: limit.unwrap_or(cfg.discovery.recommend_limit),
                embedding_model: model_id,
                region: String::from("us"),
                include_purchase_hints: !no_purchase_hints,
                external_user_id: user,
                include_listening: !no_listening,
                listening_providers,
                fetch_storefront_candidates: cfg.discovery.storefront_candidates,
                storefront_seed_limit: cfg.discovery.storefront_seed_limit,
                storefront_max_remote_calls: cfg.discovery.storefront_max_remote_calls,
                exclude_graphicaudio_series_sets: cfg.discovery.exclude_graphicaudio_series_sets,
                disabled_shelves: operator_prefs.disabled_shelves,
                // Similarity always on; the models dir opts candidates into ONNX.
                models_dir: prefer_onnx.then(|| cfg.paths().models_dir.clone()),
                embed_intra_threads: cfg.discovery.embed_intra_threads,
                embeddings_enabled: true,
            };
            let feed = bookclerk_discover::recommend_feed(&library, &registry, &opts).await?;
            format_out::emit(format, &feed, || {
                if feed.shelves.is_empty() {
                    println!("(no recommendations)");
                }
                for shelf in &feed.shelves {
                    println!("\n## {} ({})", shelf.title, shelf.id);
                    if let Some(sub) = &shelf.subtitle {
                        println!("   {sub}");
                    }
                    for (i, r) in shelf.items.iter().enumerate() {
                        println!(
                            "  {}. {} [{}] score={:.2}",
                            i + 1,
                            r.title,
                            r.authors.as_deref().unwrap_or("?"),
                            r.score
                        );
                        if !r.reasons.is_empty() {
                            println!("     reasons: {}", r.reasons.join("; "));
                        }
                        for h in &r.purchase_hints {
                            let url = h
                                .url
                                .as_ref()
                                .map(|u| format!(" ({u})"))
                                .unwrap_or_default();
                            println!("     buy via {}: {}{url}", h.source, h.product_id);
                        }
                    }
                }
            })?;
        }
        DiscoverCommand::Wishlist { command } => match command {
            WishlistCommand::Add {
                title,
                authors,
                asin,
                isbn,
                notes,
            } => {
                let request = wish_request(title, authors, asin, isbn, notes);
                let row = library.create_title_request(&request).await?;
                format_out::emit(format, &row, || {
                    format_out::line(format!("wishlisted {}", row.uuid));
                })?;
            }
            WishlistCommand::List => {
                let rows = library.list_wishlist(None).await?;
                format_out::emit(format, &rows, || {
                    for r in &rows {
                        format_out::line(format!(
                            "{} [{}] {} — {}",
                            r.uuid,
                            r.status.as_str(),
                            r.title,
                            r.authors.as_deref().unwrap_or("?")
                        ));
                    }
                })?;
            }
            WishlistCommand::Remove { uuid } => {
                library
                    .update_title_request_status(&uuid, RequestStatus::Cancelled, None)
                    .await?;
                format_out::line(format!("removed {uuid} from wishlist"));
            }
        },
    }
    Ok(())
}

/// Embeds dirty works with ONNX when enabled, otherwise the local-hash embedder.
async fn embed_works(cfg: &Config, library: &LibraryStore, prefer_onnx: bool) -> Result<usize> {
    let mut embedder = bookclerk_discover::open_embedder(
        &cfg.paths().models_dir,
        cfg.discovery.embed_intra_threads,
        prefer_onnx,
    )?;
    Ok(bookclerk_discover::embed_dirty_works(library, embedder.as_mut()).await?)
}

/// Pulls listening progress from every capable integration into the library DB.
async fn sync_listening(
    cfg: &Config,
    library: &LibraryStore,
) -> Result<bookclerk_integrations::SyncListeningSummary> {
    let host = crate::registry::open_host(cfg).await?;
    let registry = crate::registry::integrations(&host).await?;
    Ok(registry.sync_listening_progress_all(library).await)
}

/// Builds the open wishlist request `discover wishlist add` stores.
///
/// The library resolves the wish into its work when it stores it (making a
/// catalog work for a book it does not know yet), as the web API's wishes
/// are.
///
/// # Arguments
///
/// * `title`, `authors`, `asin`, `isbn`, `notes` - The wish as typed.
///
/// # Returns
///
/// The request to hand to [`LibraryStore::create_title_request`].
fn wish_request(
    title: String,
    authors: Option<String>,
    asin: Option<String>,
    isbn: Option<String>,
    notes: Option<String>,
) -> NewTitleRequest {
    NewTitleRequest {
        uuid: None,
        identity_id: None,
        title,
        authors,
        asin,
        isbn,
        notes,
        status: RequestStatus::Open,
        resolved_book_uuid: None,
        cover_url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wish for a work the library holds names that work; a sequel with
    /// a similar title names a new catalog work of its own.
    #[tokio::test]
    async fn a_wish_records_the_work_it_resolves_to() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("libro", "reader@example.com", "us", None, true, "libro")
            .await
            .unwrap();
        let mut book =
            bookclerk_library::NewBook::minimal("p1", "reader@example.com", "us", "Dune");
        book.source = "libro".into();
        book.plugin_key = "libro".into();
        book.authors = Some("Frank Herbert".into());
        let book = store.upsert_book(&book).await.unwrap();
        let dune = store.work_id_for_book(&book.uuid).await.unwrap().unwrap();

        let author = || Some("Frank Herbert".to_string());
        let owned = store
            .create_title_request(&wish_request("Dune".into(), author(), None, None, None))
            .await
            .unwrap();
        assert_eq!(owned.work_id, dune);
        assert_eq!(owned.status, RequestStatus::Open);
        assert!(!owned.work_key.is_empty());
        let sequel = store
            .create_title_request(&wish_request(
                "Dune Messiah".into(),
                author(),
                None,
                None,
                None,
            ))
            .await
            .unwrap();
        assert_ne!(sequel.work_id, dune);
        assert_eq!(sequel.title, "Dune Messiah");
    }
}
