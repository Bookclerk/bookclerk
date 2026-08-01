//! `bookclerk library` — scan, acquire, set-status, get-license, search, export.

use bookclerk_acquire::{
    acquire_book_indexed, acquire_pdf_only, convert_book, match_storage_to_library, AcquireRequest,
    ConvertRequest, MatchStorageOptions, StorageIndex,
};
use bookclerk_config::{apply_setting_overrides, AudioQuality, BadBookAction, Config};
use bookclerk_library::{AcquireStatus, LibraryStore};
use bookclerk_search::SearchEngine;
use bookclerk_source::{DownloadOptions, FetchOptions, ScanOptions};
use bookclerk_storage::StorageBackend;
use clap::Subcommand;

use crate::commands::export::{export_csv, export_json, export_xlsx, filter_books, load_books};
use crate::progress::BatchProgress;
use crate::registry::{default_registry_with_plugins, resolve_source_id};

#[derive(Debug, Subcommand)]
pub enum LibraryCommand {
    /// Sync library into the local DB from configured content sources.
    Scan {
        /// Limit sync to one account (alias for positional account list).
        #[arg(long)]
        account: Option<String>,
        /// Account nickname(s) or id(s) to scan.
        #[arg(value_name = "ACCOUNT")]
        accounts: Vec<String>,
        /// Limit to one content source (`audible`, `libro`, `graphicaudio`, or `chirp`). Default: all.
        #[arg(long)]
        source: Option<String>,
        /// After scan, match existing files in storage to library rows.
        ///
        /// Lists `.m4b` / `.mp3` / `.m4a` / `.flac` / `.aac` / `.ogg` / `.oga`, probes object metadata (no body
        /// download), and falls back to ASIN/ISBN tokens in the path.
        #[arg(long)]
        match_storage: bool,
        /// With `--match-storage`, relocate matched audio + sidecars onto the
        /// configured naming-profile layout (also `library.fix_storage_layout`).
        #[arg(long)]
        fix_layout: bool,
    },
    /// Download + decrypt + store titles.
    Acquire {
        /// Acquire a single ASIN / product id / UUID.
        #[arg(long)]
        asin: Option<String>,
        /// Alias for `--asin` (Libro ISBN or any title id).
        #[arg(long)]
        isbn: Option<String>,
        /// Positional title id(s): UUID, ASIN, ISBN, or product id.
        #[arg(value_name = "ID")]
        asins: Vec<String>,
        /// Account id (required with `--asin` when multiple accounts exist).
        #[arg(long)]
        account: Option<String>,
        /// Dry-run: print planned storage keys only.
        #[arg(long)]
        dry_run: bool,
        /// Re-download even when matching media already exists in storage.
        #[arg(short, long)]
        force: bool,
        /// Download companion PDF only.
        #[arg(short, long)]
        pdf: bool,
        /// Read license JSON from file (or `-` for stdin) instead of requesting.
        #[arg(long, value_name = "FILE")]
        license: Option<std::path::PathBuf>,
        /// Runtime setting override (`-o key=value`). Repeatable.
        #[arg(short = 'o', long = "override", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
    },
    /// Match storage files to library rows and update acquire status.
    ///
    /// Lists audio in storage and probes custom object metadata (S3
    /// `HeadObject` / local `.bookclerk-meta.json`) without downloading bodies.
    /// Falls back to ASIN/ISBN tokens embedded in the object key.
    SetStatus {
        #[arg(long)]
        account: Option<String>,
        /// Do not clear Acquired status when the file is missing.
        #[arg(long)]
        keep_missing: bool,
        /// Only mark titles with matching files as Acquired.
        #[arg(long, conflicts_with = "not_downloaded")]
        downloaded: bool,
        /// Only clear Acquired status when the file is missing.
        #[arg(long, conflicts_with = "downloaded")]
        not_downloaded: bool,
        /// Force status without checking storage.
        #[arg(short, long)]
        force: bool,
        /// Relocate matched audio + accompanying sidecars onto the configured
        /// naming-profile layout (`library.fix_storage_layout`).
        #[arg(long)]
        fix_layout: bool,
        #[arg(value_name = "ASIN")]
        asins: Vec<String>,
    },
    /// Fetch a content license for an ASIN.
    GetLicense {
        asin: String,
        #[arg(long)]
        account: Option<String>,
        /// Emit full license summary as JSON.
        #[arg(long)]
        json: bool,
        /// Emit full license API response JSON (not summary).
        #[arg(long, conflicts_with = "json")]
        full: bool,
    },
    /// Search the library index.
    Search {
        /// Lucene-style query string.
        query: String,
        /// Max results (0 = all).
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
        /// Print ASINs only.
        #[arg(short, long)]
        bare: bool,
        /// Rebuild the search index before querying.
        #[arg(long)]
        rebuild_index: bool,
        /// Run a saved quick filter by name.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Export library rows as CSV / JSON / XLSX (alias of `export library`).
    Export {
        /// Output file path.
        #[arg(short, long)]
        path: std::path::PathBuf,
        #[arg(long)]
        csv: bool,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        xlsx: bool,
        /// Limit to specific ASINs.
        asins: Vec<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// List books in the local library DB.
    List {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    /// Convert acquired m4b/m4a to mp3.
    Convert {
        #[arg(long)]
        account: Option<String>,
        #[arg(short, long)]
        force: bool,
        #[arg(value_name = "ASIN")]
        asins: Vec<String>,
    },
    /// Manage saved quick filters.
    Filters {
        #[command(subcommand)]
        command: FilterCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum FilterCommand {
    /// List saved filters.
    List,
    /// Save or update a named filter.
    Save {
        name: String,
        /// Lucene-style query string.
        query: String,
    },
    /// Delete a saved filter.
    Delete { name: String },
}

pub async fn run(command: LibraryCommand, config: &Config) -> anyhow::Result<()> {
    let paths = config.paths();
    let store = crate::registry::open_library(config).await?;

    match command {
        LibraryCommand::Scan {
            account,
            accounts,
            source,
            match_storage,
            fix_layout,
        } => {
            let mut scan_accounts = accounts;
            if let Some(one) = account {
                scan_accounts.push(one);
            }
            let registry = default_registry_with_plugins(config).await?;
            let opts = ScanOptions {
                accounts: scan_accounts.clone(),
                page_size: 50,
                import_episodes: config.library.import_episodes,
                import_plus_titles: config.library.import_plus_titles,
            };
            let summary = if let Some(needle) = source {
                let id = resolve_source_id(&registry, &needle)?;
                registry.require(&id)?.scan(&store.scope(id), opts).await?
            } else {
                registry.scan_all(&store, opts).await?
            };
            println!(
                "scan complete: {} account(s), {} book upsert(s), {} page(s), {} skipped (scan disabled)",
                summary.accounts, summary.books_upserted, summary.pages, summary.skipped_disabled
            );
            if config.library.enrich_from_audible {
                match bookclerk_enrich::enrich_books_from_audible(
                    &store,
                    config.library.enrich_min_confidence,
                )
                .await
                {
                    Ok(n) if n > 0 => {
                        println!(
                            "Audible enrichment: updated {n} book(s) (min confidence {}%)",
                            config.library.enrich_min_confidence
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!(error = %err, "Audible enrichment failed"),
                }
            }
            if match_storage {
                let storage = storage_for_config(config, &store).await?;
                let recon = match_storage_to_library(
                    &store,
                    storage.as_ref(),
                    MatchStorageOptions {
                        account: scan_accounts.first().cloned(),
                        clear_missing: true,
                        fix_layout: fix_layout || config.library.fix_storage_layout,
                        download: DownloadOptions::from(config),
                        ..Default::default()
                    },
                )
                .await?;
                println!(
                    "storage match: matched={} relocated={} cleared={} unchanged={} unmatched_files={}",
                    recon.matched,
                    recon.relocated,
                    recon.cleared,
                    recon.unchanged,
                    recon.unmatched_files
                );
            }
            let engine = SearchEngine::open(&paths.search_index_dir)?;
            let indexed = engine.rebuild(&store).await?;
            println!("search index rebuilt: {indexed} book(s)");
            Ok(())
        }
        LibraryCommand::Acquire {
            asin,
            isbn,
            asins,
            account,
            dry_run,
            force,
            pdf,
            license,
            overrides,
        } => {
            let mut cfg = config.clone();
            let pairs: Vec<(&str, &str)> = overrides
                .iter()
                .filter_map(|s| s.split_once('='))
                .map(|(k, v)| (k.trim(), v.trim()))
                .collect();
            apply_setting_overrides(&mut cfg, &pairs);
            let destinations =
                bookclerk_plugin_host::load_external_destinations(&cfg, Some(store.db())).await?;
            let destinations = bookclerk_plugin_host::build_acquire_destinations(
                &cfg,
                Some(&store),
                &destinations,
            )
            .await?;
            let storage = destinations.listing_backend()?;
            let registry = default_registry_with_plugins(&cfg).await?;

            // Match existing media first (same as bookclerkd) so we do not
            // re-download titles already on disk.
            if !dry_run {
                let _ = match_storage_to_library(
                    &store,
                    storage.as_ref(),
                    MatchStorageOptions {
                        account: account.clone(),
                        clear_missing: true,
                        fix_layout: cfg.library.fix_storage_layout,
                        download: DownloadOptions::from(&cfg),
                        ..Default::default()
                    },
                )
                .await?;
            }

            let books = store.list_books(account.as_deref()).await?;
            let filter_ids: Vec<String> = asin.into_iter().chain(isbn).chain(asins).collect();
            let targets: Vec<_> = books
                .into_iter()
                .filter(|b| {
                    filter_ids.is_empty() || filter_ids.iter().any(|a| title_id_matches(b, a))
                })
                .filter(|b| {
                    if pdf {
                        force || b.pdf_status != AcquireStatus::Acquired
                    } else {
                        (force || b.acquire_status != AcquireStatus::Acquired)
                            && bookclerk_library::is_downloadable(&b.content_kind)
                            && (cfg.library.download_episodes || b.content_kind != "episode")
                    }
                })
                .collect();

            if targets.is_empty() {
                eprintln!("nothing to acquire");
                return Ok(());
            }

            if license.is_some() {
                anyhow::bail!(
                    "preloaded license vouchers (--license) are not supported on the host path yet"
                );
            }

            paths.ensure_dirs()?;
            let options = DownloadOptions::from(&cfg);
            let mut index = if dry_run {
                None
            } else {
                Some(StorageIndex::from_storage(storage.as_ref()).await?)
            };

            let integrations = if dry_run {
                let mut registry = bookclerk_integrations::IntegrationRegistry::new();
                bookclerk_plugin_host::register_builtin_integrations(&cfg, &mut registry)?;
                registry
            } else {
                bookclerk_plugin_host::load_integrations(&cfg).await?
            };

            let mut ok = 0u32;
            let mut matched = 0u32;
            let mut failed = 0u32;
            let bad_book = cfg.output.bad_book_action;

            let total = targets.len();
            let mut batch = BatchProgress::new(total, if pdf { "pdf" } else { "acquire" });

            for (idx, book) in targets.into_iter().enumerate() {
                batch.set(idx + 1, book.asin_or_isbn());
                let content_source = registry.get(&book.source).ok_or_else(|| {
                    anyhow::anyhow!(
                        "no content source registered for `{}` (title {})",
                        book.source,
                        book.asin_or_isbn()
                    )
                })?;
                let req = AcquireRequest {
                    asin: book.download_product_id().to_string(),
                    book_uuid: Some(book.uuid.clone()),
                    source: book.source.clone(),
                    account_id: book.account_id.clone(),
                    title: book.title.clone(),
                    authors: book.authors.clone(),
                    narrators: book.narrators.clone(),
                    series: book.series.clone(),
                    series_index: book.series_index.clone(),
                    options: options.clone(),
                    files_dir: paths.files_dir.clone(),
                    cache_dir: cfg.download_cache_dir(),
                    force,
                    write_destinations: None,
                };
                if dry_run {
                    let key = if pdf {
                        bookclerk_acquire::sidecar_key(
                            &bookclerk_acquire::planned_storage_key(&store, &req).await,
                            "pdf",
                        )
                    } else {
                        bookclerk_acquire::planned_storage_key(&store, &req).await
                    };
                    println!("{}\t{}", book.asin_or_isbn(), key);
                    continue;
                }

                let mut attempts = 0u32;
                loop {
                    attempts += 1;
                    let result = if pdf {
                        acquire_pdf_only(&store, &destinations, &req, content_source.as_ref()).await
                    } else {
                        acquire_book_indexed(
                            &store,
                            &destinations,
                            req.clone(),
                            index.as_mut(),
                            content_source.as_ref(),
                        )
                        .await
                    };
                    match result {
                        Ok(result) if result.matched_existing => {
                            println!("matched {} -> {}", result.asin, result.storage_key);
                            bookclerk_integrations::emit_book_acquired(
                                &integrations,
                                &store,
                                &result.asin,
                                &result.storage_key,
                            )
                            .await;
                            matched += 1;
                            break;
                        }
                        Ok(result) => {
                            println!("acquired {} -> {}", result.asin, result.storage_key);
                            bookclerk_integrations::emit_book_acquired(
                                &integrations,
                                &store,
                                &result.asin,
                                &result.storage_key,
                            )
                            .await;
                            ok += 1;
                            break;
                        }
                        Err(err) => {
                            if bad_book == BadBookAction::Retry && attempts < 2 {
                                eprintln!("acquire {}: {err} (retrying)", book.asin_or_isbn());
                                continue;
                            }
                            eprintln!("acquire {}: {err}", book.asin_or_isbn());
                            failed += 1;
                            if matches!(bad_book, BadBookAction::Ask | BadBookAction::Abort) {
                                anyhow::bail!(
                                    "acquire aborted after failure on {} ({err})",
                                    book.asin_or_isbn()
                                );
                            }
                            break;
                        }
                    }
                }
            }

            batch.finish();

            if dry_run {
                return Ok(());
            }
            if failed > 0 && bad_book == BadBookAction::Retry {
                anyhow::bail!(
                    "acquire finished with {failed} failure(s) (acquired={ok} matched={matched})"
                );
            }
            Ok(())
        }
        LibraryCommand::SetStatus {
            account,
            keep_missing,
            downloaded,
            not_downloaded,
            force,
            fix_layout,
            asins,
        } => {
            let asins: Vec<String> = asins.into_iter().map(|a| a.to_ascii_uppercase()).collect();
            if force {
                let status = if not_downloaded {
                    AcquireStatus::NotAcquired
                } else {
                    AcquireStatus::Acquired
                };
                let n = store
                    .bulk_set_acquire_status(account.as_deref(), &asins, status)
                    .await?;
                println!("force-updated {n} book(s) to {}", status.as_str());
                return Ok(());
            }
            let storage = storage_for_config(config, &store).await?;
            let summary = match_storage_to_library(
                &store,
                storage.as_ref(),
                MatchStorageOptions {
                    account,
                    clear_missing: !keep_missing && !downloaded,
                    asins,
                    only_mark_found: downloaded,
                    only_clear_missing: not_downloaded,
                    fix_layout: fix_layout || config.library.fix_storage_layout,
                    download: DownloadOptions::from(config),
                },
            )
            .await?;
            println!(
                "matched {}\trelocated {}\tcleared {}\tunchanged {}",
                summary.matched, summary.relocated, summary.cleared, summary.unchanged
            );
            Ok(())
        }
        LibraryCommand::GetLicense {
            asin,
            account,
            json,
            full,
        } => {
            let registry = default_registry_with_plugins(config).await?;
            let source = registry.require("audible")?;
            let (account_key, license_asin) =
                resolve_license_target(&store, &asin, account.as_deref()).await?;
            let quality = match config
                .sources
                .get_string("audible", "bitrate")
                .unwrap_or("high")
                .to_ascii_lowercase()
                .as_str()
            {
                "normal" => AudioQuality::Normal,
                _ => AudioQuality::High,
            };
            let mut download = DownloadOptions::from(config);
            download.quality = quality;
            let inspected = source
                .inspect_title(
                    &store.scope(source.id()),
                    &account_key,
                    &license_asin,
                    &FetchOptions {
                        download,
                        cache_dir: paths.cache_dir.clone(),
                        files_dir: paths.files_dir.clone(),
                    },
                )
                .await?;
            if full {
                let full_json = inspected
                    .get("full")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                println!("{}", serde_json::to_string_pretty(&full_json)?);
                return Ok(());
            }
            let summary = inspected
                .get("summary")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
                return Ok(());
            }
            println!(
                "asin\t{}",
                summary
                    .get("asin")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&license_asin)
            );
            println!(
                "status\t{}",
                summary
                    .get("status_code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-")
            );
            println!(
                "drm_type\t{}",
                summary
                    .get("drm_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-")
            );
            println!(
                "content_format\t{}",
                summary
                    .get("content_format")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-")
            );
            println!(
                "content_size\t{}",
                summary
                    .get("content_size")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into())
            );
            println!(
                "granted\t{}",
                summary
                    .get("granted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            );
            println!(
                "has_voucher\t{}",
                summary
                    .get("has_voucher")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            );
            println!(
                "offline_url\t{}",
                summary
                    .get("offline_url_present")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            );
            if let Some(msg) = summary.get("denial_message").and_then(|v| v.as_str()) {
                println!("denial\t{msg}");
            }
            Ok(())
        }
        LibraryCommand::Search {
            query,
            limit,
            bare,
            rebuild_index,
            filter,
        } => {
            let engine = SearchEngine::open(&paths.search_index_dir)?;
            if rebuild_index {
                let n = engine.rebuild(&store).await?;
                println!("search index rebuilt: {n} book(s)");
            }
            let query_text = if let Some(name) = filter {
                store
                    .get_saved_filter(&name)
                    .await?
                    .map(|f| f.query)
                    .ok_or_else(|| anyhow::anyhow!("unknown saved filter: {name}"))?
            } else {
                query
            };
            let hits = engine.search(&query_text, limit)?;
            if limit != 0 {
                println!("Found {} matching result(s).", hits.len());
            }
            for hit in hits {
                if bare {
                    println!("{}", hit.asin);
                } else {
                    println!("{} — {}", hit.asin, hit.title);
                }
            }
            Ok(())
        }
        LibraryCommand::Export {
            path,
            csv,
            json,
            xlsx,
            asins,
            account,
        } => {
            let books = filter_books(
                load_books(&store, account.as_deref()).await?,
                if asins.is_empty() { None } else { Some(&asins) },
            );
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if csv || (!json && !xlsx && ext == "csv") {
                export_csv(&path, &books)?;
            } else if json || ext == "json" {
                export_json(&path, &books)?;
            } else if xlsx || ext == "xlsx" {
                export_xlsx(&path, &books)?;
            } else {
                export_csv(&path, &books)?;
            }
            println!("exported {} book(s) to {}", books.len(), path.display());
            Ok(())
        }
        LibraryCommand::Convert {
            account,
            force,
            asins,
        } => {
            let storage = storage_for_config(config, &store).await?;
            let filter: Vec<String> = asins.into_iter().map(|a| a.to_ascii_uppercase()).collect();
            let targets: Vec<_> = store
                .list_books(account.as_deref())
                .await?
                .into_iter()
                .filter(|b| b.acquire_status == AcquireStatus::Acquired)
                .filter(|b| filter.is_empty() || filter.iter().any(|a| title_id_matches(b, a)))
                .collect();
            if targets.is_empty() {
                eprintln!("nothing to convert");
                return Ok(());
            }
            paths.ensure_dirs()?;
            let req = ConvertRequest {
                cache_dir: config.download_cache_dir(),
                force,
                lame: config.output.lame.clone(),
                max_sample_rate: config.output.max_sample_rate,
            };
            let total = targets.len();
            let mut batch = BatchProgress::new(total, "convert");
            let mut converted = 0u32;
            let mut failed = 0u32;
            for (idx, book) in targets.into_iter().enumerate() {
                batch.set(idx + 1, book.asin_or_isbn());
                match convert_book(&store, storage.as_ref(), &book, &req).await {
                    Ok(key) => {
                        println!("converted {} -> {}", book.asin_or_isbn(), key);
                        converted += 1;
                    }
                    Err(err) => {
                        eprintln!("convert {}: {err}", book.asin_or_isbn());
                        failed += 1;
                    }
                }
            }
            batch.finish();
            if failed > 0 {
                anyhow::bail!("convert finished with {failed} failure(s) (converted={converted})");
            }
            Ok(())
        }
        LibraryCommand::Filters { command } => match command {
            FilterCommand::List => {
                for f in store.list_saved_filters().await? {
                    println!("{}\t{}", f.name, f.query);
                }
                Ok(())
            }
            FilterCommand::Save { name, query } => {
                store.upsert_saved_filter(&name, &query).await?;
                println!("saved filter {name}");
                Ok(())
            }
            FilterCommand::Delete { name } => {
                store.delete_saved_filter(&name).await?;
                println!("deleted filter {name}");
                Ok(())
            }
        },
        LibraryCommand::List { account, status } => {
            let books = store.list_books(account.as_deref()).await?;
            for book in books {
                if let Some(filter) = &status {
                    if book.acquire_status.as_str() != filter.as_str() {
                        continue;
                    }
                }
                println!(
                    "{}\t{}\t{}\t{}",
                    book.asin_or_isbn(),
                    book.acquire_status.as_str(),
                    book.title,
                    book.storage_key.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
    }
}

/// Resolve an account + Audible ASIN for `library get-license`.
///
/// Accepts uuid / product_id / isbn / asin. Never sends a Libro ISBN or library
/// UUID as the license title id. Enriched non-Audible rows may supply an ASIN,
/// but the account must still be an Audible auth identity.
async fn resolve_license_target(
    store: &LibraryStore,
    id: &str,
    account: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let books = store.list_books(None).await?;
    let matches: Vec<_> = books
        .into_iter()
        .filter(|b| title_id_matches(b, id))
        .collect();

    if matches.is_empty() {
        let Some(account) = account else {
            anyhow::bail!("ASIN {id} not in library — pass --account or run library scan");
        };
        // Raw ASIN + explicit account (classic path when the title is not scanned).
        return Ok((account.to_string(), id.to_string()));
    }

    // Prefer an Audible ownership row when several match (e.g. shared ISBN).
    let book = matches
        .iter()
        .find(|b| b.source.eq_ignore_ascii_case("audible"))
        .or_else(|| matches.first())
        .expect("matches non-empty");

    let license_asin = book.audible_asin().ok_or_else(|| {
        anyhow::anyhow!(
            "title {id} has no Audible ASIN (source={}); enrich via ISBN or use an Audible library id",
            book.source
        )
    })?;

    if let Some(account) = account {
        return Ok((account.to_string(), license_asin.to_string()));
    }

    if book.source.eq_ignore_ascii_case("audible") {
        return Ok((book.account_id.clone(), license_asin.to_string()));
    }

    // Libro (or other) row with an enriched ASIN — find an Audible account that
    // owns the same ASIN, otherwise require --account.
    let audible_owners: Vec<_> = store
        .list_books(None)
        .await?
        .into_iter()
        .filter(|b| {
            b.source.eq_ignore_ascii_case("audible")
                && b.audible_asin()
                    .is_some_and(|a| a.eq_ignore_ascii_case(license_asin))
        })
        .collect();
    match audible_owners.as_slice() {
        [one] => Ok((one.account_id.clone(), license_asin.to_string())),
        [] => anyhow::bail!(
            "title {id} is not on an Audible account; pass --account with an Audible login"
        ),
        many => anyhow::bail!(
            "ASIN {license_asin} exists on {} Audible accounts; pass --account ({})",
            many.len(),
            many.iter()
                .map(|b| b.account_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn title_id_matches(book: &bookclerk_library::BookRecord, id: &str) -> bool {
    id.eq_ignore_ascii_case(&book.uuid)
        || id.eq_ignore_ascii_case(&book.product_id)
        || book
            .isbn
            .as_ref()
            .is_some_and(|isbn| id.eq_ignore_ascii_case(isbn))
        || book
            .asin
            .as_ref()
            .is_some_and(|a| id.eq_ignore_ascii_case(a))
}

async fn storage_for_config(
    config: &Config,
    store: &LibraryStore,
) -> anyhow::Result<Box<dyn StorageBackend>> {
    let destinations =
        bookclerk_plugin_host::load_external_destinations(config, Some(store.db())).await?;
    Ok(bookclerk_plugin_host::build_storage_backend(config, Some(store), &destinations).await?)
}
