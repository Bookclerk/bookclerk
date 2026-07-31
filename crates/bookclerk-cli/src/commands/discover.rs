//! Discovery: recommendations, embeddings, listening sync, wishlist.

use anyhow::Result;
use bookclerk_config::Config;
use bookclerk_library::{LibraryStore, NewTitleRequest, RequestStatus};
use clap::Subcommand;

use crate::format_out::{self, OutputFormat};

#[derive(Debug, Subcommand)]
pub enum DiscoverCommand {
    /// Rebuild works graph, optionally enrich + embed, then print recommendations.
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
    /// Rebuild the works graph from ownership rows.
    RebuildWorks,
    /// Run Open Library enrichment for metadata gaps.
    EnrichOpenlibrary,
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
        command: WishlistCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum WishlistCommand {
    /// Add an open wishlist item.
    Add {
        title: String,
        #[arg(long)]
        authors: Option<String>,
        #[arg(long)]
        asin: Option<String>,
        #[arg(long)]
        isbn: Option<String>,
        #[arg(long)]
        notes: Option<String>,
    },
    /// List open wishlist items (operator-owned when no portal identity).
    List,
    /// Un-wishlist by uuid (sets status to cancelled).
    Remove { uuid: String },
}

pub async fn run(cfg: &Config, format: OutputFormat, command: DiscoverCommand) -> Result<()> {
    let library = LibraryStore::open_from_config(cfg).await?;
    let registry = crate::registry::default_registry_with_plugins(cfg).await?;

    match command {
        DiscoverCommand::RebuildWorks => {
            let n = bookclerk_discover::rebuild_works_from_library(&library).await?;
            println!("linked {n} book(s) into works");
        }
        DiscoverCommand::EnrichOpenlibrary => {
            let opts = bookclerk_discover::OpenLibraryOptions {
                contact_email: cfg.discovery.openlibrary_contact_email.clone(),
                max_requests: cfg.discovery.openlibrary_max_requests_per_run.max(1),
                ..Default::default()
            };
            let n = bookclerk_discover::enrich_books_from_openlibrary_with(&library, &opts).await?;
            println!("open library enriched {n} book(s)");
        }
        DiscoverCommand::Embed { hash } => {
            let n = embed_works(cfg, &library, !hash && cfg.discovery.embeddings_enabled).await?;
            println!("embedded {n} work(s)");
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
            let _ = bookclerk_discover::rebuild_works_from_library(&library).await?;
            let prefer_onnx = cfg.discovery.embeddings_enabled;
            let mut embedder = bookclerk_discover::open_embedder(
                &cfg.paths().models_dir,
                cfg.discovery.embed_intra_threads,
                prefer_onnx,
            )?;
            let model_id = embedder.model_id().to_string();
            let _ = bookclerk_discover::embed_dirty_works(&library, embedder.as_mut()).await?;

            if cfg.discovery.openlibrary_enabled {
                let ol = bookclerk_discover::OpenLibraryOptions {
                    contact_email: cfg.discovery.openlibrary_contact_email.clone(),
                    max_requests: cfg.discovery.openlibrary_max_requests_per_run.max(1),
                    ..Default::default()
                };
                let _ = bookclerk_discover::enrich_books_from_openlibrary_with(&library, &ol).await;
            }

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
                models_dir: Some(cfg.paths().models_dir.clone()),
                embed_intra_threads: cfg.discovery.embed_intra_threads,
                embeddings_enabled: cfg.discovery.embeddings_enabled,
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
                let work_key = bookclerk_discover::work_map_key(
                    asin.as_deref(),
                    isbn.as_deref(),
                    &title,
                    authors.as_deref(),
                    None,
                    asin.as_deref().or(isbn.as_deref()),
                );
                let row = library
                    .create_title_request(&NewTitleRequest {
                        uuid: None,
                        identity_id: None,
                        title,
                        authors,
                        asin,
                        isbn,
                        notes,
                        status: RequestStatus::Open,
                        work_key,
                        work_id: None,
                        resolved_book_uuid: None,
                    })
                    .await?;
                format_out::emit(format, &row, || {
                    println!("wishlisted {}", row.uuid);
                })?;
            }
            WishlistCommand::List => {
                let rows = library.list_wishlist(None).await?;
                format_out::emit(format, &rows, || {
                    for r in &rows {
                        println!(
                            "{} [{}] {} — {}",
                            r.uuid,
                            r.status.as_str(),
                            r.title,
                            r.authors.as_deref().unwrap_or("?")
                        );
                    }
                })?;
            }
            WishlistCommand::Remove { uuid } => {
                library
                    .update_title_request_status(&uuid, RequestStatus::Cancelled, None)
                    .await?;
                println!("removed {uuid} from wishlist");
            }
        },
    }
    Ok(())
}

async fn embed_works(cfg: &Config, library: &LibraryStore, prefer_onnx: bool) -> Result<usize> {
    let mut embedder = bookclerk_discover::open_embedder(
        &cfg.paths().models_dir,
        cfg.discovery.embed_intra_threads,
        prefer_onnx,
    )?;
    Ok(bookclerk_discover::embed_dirty_works(library, embedder.as_mut()).await?)
}

async fn sync_listening(
    cfg: &Config,
    library: &LibraryStore,
) -> Result<bookclerk_integrations::SyncListeningSummary> {
    let registry = bookclerk_plugin::load_integrations(cfg).await?;
    Ok(registry.sync_listening_progress_all(library).await)
}
