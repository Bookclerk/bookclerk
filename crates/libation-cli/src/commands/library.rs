//! `libation library` — scan, liberate, set-status, get-license, search, export.

use clap::Subcommand;
use libation_audible::{
    license_full_json, open_account_client, parse_license_json, request_content_license,
    summarize_license, DownloadOptions,
};
use libation_config::{apply_setting_overrides, BadBookAction, Config};
use libation_liberate::{
    convert_book, liberate_book_indexed, liberate_pdf_only, match_storage_to_library,
    ConvertRequest, LiberateRequest, MatchStorageOptions, StorageIndex,
};
use libation_library::{LiberateStatus, LibraryStore};
use libation_search::SearchEngine;
use libation_source::{ScanOptions, SourceKind};
use libation_storage::from_config;

use crate::commands::export::{export_csv, export_json, export_xlsx, filter_books, load_books};
use crate::progress::BatchProgress;
use crate::registry::{default_registry, parse_source_kind};

#[derive(Debug, Subcommand)]
pub enum LibraryCommand {
    /// Sync library into the local DB from configured content sources.
    Scan {
        /// Limit sync to one account (alias for positional account list).
        #[arg(long)]
        account: Option<String>,
        /// Account nickname(s) or id(s) to scan (LibationCli: `scan nick1 nick2`).
        #[arg(value_name = "ACCOUNT")]
        accounts: Vec<String>,
        /// Limit to one content source (`audible`, `libro`, `graphicaudio`, or `chirp`). Default: all.
        #[arg(long, value_parser = parse_source_kind)]
        source: Option<SourceKind>,
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
    /// Download + decrypt + store titles (LibationCli: `liberate`).
    Liberate {
        /// Liberate a single ASIN / product id / UUID.
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
        /// Download companion PDF only (classic `liberate --pdf`).
        #[arg(short, long)]
        pdf: bool,
        /// Read license JSON from file (or `-` for stdin) instead of requesting.
        #[arg(long, value_name = "FILE")]
        license: Option<std::path::PathBuf>,
        /// Runtime setting override (classic `-o Setting=value`). Repeatable.
        #[arg(short = 'o', long = "override", value_name = "KEY=VALUE")]
        overrides: Vec<String>,
    },
    /// Match storage files to library rows and update liberate status.
    ///
    /// Lists audio in storage and probes custom object metadata (S3
    /// `HeadObject` / local `.libation-meta.json`) without downloading bodies.
    /// Falls back to ASIN/ISBN tokens embedded in the object key.
    SetStatus {
        #[arg(long)]
        account: Option<String>,
        /// Do not clear Liberated status when the file is missing.
        #[arg(long)]
        keep_missing: bool,
        /// Only mark titles with matching files as Liberated.
        #[arg(long, conflicts_with = "not_downloaded")]
        downloaded: bool,
        /// Only clear Liberated status when the file is missing.
        #[arg(long, conflicts_with = "downloaded")]
        not_downloaded: bool,
        /// Force status without checking storage (classic `--force`).
        #[arg(short, long)]
        force: bool,
        /// Relocate matched audio + accompanying sidecars onto the configured
        /// naming-profile layout (`library.fix_storage_layout`).
        #[arg(long)]
        fix_layout: bool,
        #[arg(value_name = "ASIN")]
        asins: Vec<String>,
    },
    /// Fetch a content license for an ASIN (LibationCli: `get-license`).
    GetLicense {
        asin: String,
        #[arg(long)]
        account: Option<String>,
        /// Emit full license summary as JSON (classic prints license JSON).
        #[arg(long)]
        json: bool,
        /// Emit full license API response JSON (not summary).
        #[arg(long, conflicts_with = "json")]
        full: bool,
    },
    /// Search the library index (LibationCli: `search`).
    Search {
        /// Lucene-style query string.
        query: String,
        /// Max results (0 = all, classic `-n 0`).
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
    /// Export library rows (LibationCli: `export`).
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
    /// Convert liberated m4b/m4a to mp3 (LibationCli: `convert`).
    Convert {
        #[arg(long)]
        account: Option<String>,
        #[arg(short, long)]
        force: bool,
        #[arg(value_name = "ASIN")]
        asins: Vec<String>,
    },
    /// Manage saved quick filters (classic Lucene shortcuts).
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
    let store = LibraryStore::open(&paths.library_db)?;

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
            let registry = default_registry(config);
            let opts = ScanOptions {
                accounts: scan_accounts.clone(),
                page_size: 50,
                import_episodes: config.library.import_episodes,
                import_plus_titles: config.library.import_plus_titles,
            };
            let summary = if let Some(kind) = source {
                registry
                    .require(kind)?
                    .scan(&paths.files_dir, &store, opts)
                    .await?
            } else {
                registry.scan_all(&paths.files_dir, &store, opts).await?
            };
            println!(
                "scan complete: {} account(s), {} book upsert(s), {} page(s), {} skipped (scan disabled)",
                summary.accounts, summary.books_upserted, summary.pages, summary.skipped_disabled
            );
            if config.library.enrich_from_audible {
                match libation_enrich::enrich_books_from_audible(
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
                let storage = from_config(config).await?;
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
            let indexed = engine.rebuild(&store)?;
            println!("search index rebuilt: {indexed} book(s)");
            Ok(())
        }
        LibraryCommand::Liberate {
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
            let storage = from_config(&cfg).await?;
            let registry = default_registry(&cfg);

            // Match existing media first (same as libationd) so we do not
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

            let books = store.list_books(account.as_deref())?;
            let filter_ids: Vec<String> = asin.into_iter().chain(isbn).chain(asins).collect();
            let targets: Vec<_> = books
                .into_iter()
                .filter(|b| {
                    filter_ids.is_empty() || filter_ids.iter().any(|a| title_id_matches(b, a))
                })
                .filter(|b| {
                    if pdf {
                        force || b.pdf_status != LiberateStatus::Liberated
                    } else {
                        (force || b.liberate_status != LiberateStatus::Liberated)
                            && libation_library::is_downloadable(&b.content_kind)
                            && (cfg.library.download_episodes || b.content_kind != "episode")
                    }
                })
                .collect();

            if targets.is_empty() {
                eprintln!("nothing to liberate");
                return Ok(());
            }

            let preloaded_license = if let Some(path) = license {
                let text = read_license_input(&path).await?;
                Some(parse_license_json(&text)?)
            } else {
                None
            };

            paths.ensure_dirs()?;
            let options = DownloadOptions::from(&cfg);
            let mut index = if dry_run {
                None
            } else {
                Some(StorageIndex::from_storage(storage.as_ref()).await?)
            };

            let mut ok = 0u32;
            let mut matched = 0u32;
            let mut failed = 0u32;
            let bad_book = cfg.download.bad_book_action;

            let total = targets.len();
            let mut batch = BatchProgress::new(total, if pdf { "pdf" } else { "liberate" });

            for (idx, book) in targets.into_iter().enumerate() {
                batch.set(idx + 1, book.asin_or_isbn());
                let source_kind = SourceKind::parse(&book.source).unwrap_or(SourceKind::Audible);
                let content_source = registry.require(source_kind).ok();
                let req = LiberateRequest {
                    asin: book.download_product_id().to_string(),
                    book_uuid: Some(book.uuid.clone()),
                    source: source_kind,
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
                    preloaded_license: preloaded_license.clone(),
                };
                if dry_run {
                    let key = if pdf {
                        libation_liberate::sidecar_key(
                            &libation_liberate::planned_storage_key(&store, &req),
                            "pdf",
                        )
                    } else {
                        libation_liberate::planned_storage_key(&store, &req)
                    };
                    println!("{}\t{}", book.asin_or_isbn(), key);
                    continue;
                }

                let mut attempts = 0u32;
                loop {
                    attempts += 1;
                    let result = if pdf {
                        liberate_pdf_only(&store, storage.as_ref(), &req).await
                    } else {
                        liberate_book_indexed(
                            &store,
                            storage.as_ref(),
                            req.clone(),
                            index.as_mut(),
                            content_source.as_deref(),
                        )
                        .await
                    };
                    match result {
                        Ok(result) if result.matched_existing => {
                            println!("matched {} -> {}", result.asin, result.storage_key);
                            matched += 1;
                            break;
                        }
                        Ok(result) => {
                            println!("liberated {} -> {}", result.asin, result.storage_key);
                            ok += 1;
                            break;
                        }
                        Err(err) => {
                            if bad_book == BadBookAction::Retry && attempts < 2 {
                                eprintln!("liberate {}: {err} (retrying)", book.asin_or_isbn());
                                continue;
                            }
                            eprintln!("liberate {}: {err}", book.asin_or_isbn());
                            failed += 1;
                            if matches!(bad_book, BadBookAction::Ask | BadBookAction::Abort) {
                                anyhow::bail!(
                                    "liberate aborted after failure on {} ({err})",
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
                    "liberate finished with {failed} failure(s) (liberated={ok} matched={matched})"
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
                    LiberateStatus::NotLiberated
                } else {
                    LiberateStatus::Liberated
                };
                let n = store.bulk_set_liberate_status(account.as_deref(), &asins, status)?;
                println!("force-updated {n} book(s) to {}", status.as_str());
                return Ok(());
            }
            let storage = from_config(config).await?;
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
            let (account_key, license_asin) =
                resolve_audible_license_target(&store, &asin, account.as_deref())?;
            let client = open_account_client(&paths.files_dir, &account_key).await?;
            let license = request_content_license(
                &client.client,
                &client.marketplace,
                &license_asin,
                config.download.quality,
            )
            .await?;
            if full {
                println!("{}", license_full_json(&license));
                return Ok(());
            }
            let summary = summarize_license(&license);
            if json {
                println!("{}", serde_json::to_string_pretty(&summary)?);
                return Ok(());
            }
            println!("asin\t{}", summary.asin);
            println!("status\t{}", summary.status_code);
            println!("drm_type\t{}", summary.drm_type.as_deref().unwrap_or("-"));
            println!(
                "content_format\t{}",
                summary.content_format.as_deref().unwrap_or("-")
            );
            println!(
                "content_size\t{}",
                summary
                    .content_size
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".into())
            );
            println!("granted\t{}", summary.granted);
            println!("has_voucher\t{}", summary.has_voucher);
            println!("offline_url\t{}", summary.offline_url_present);
            if let Some(msg) = &summary.denial_message {
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
                let n = engine.rebuild(&store)?;
                println!("search index rebuilt: {n} book(s)");
            }
            let query_text = if let Some(name) = filter {
                store
                    .get_saved_filter(&name)?
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
                load_books(&store, account.as_deref())?,
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
            let storage = from_config(config).await?;
            let filter: Vec<String> = asins.into_iter().map(|a| a.to_ascii_uppercase()).collect();
            let targets: Vec<_> = store
                .list_books(account.as_deref())?
                .into_iter()
                .filter(|b| b.liberate_status == LiberateStatus::Liberated)
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
                lame: config.download.lame.clone(),
                max_sample_rate: config.download.max_sample_rate,
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
                for f in store.list_saved_filters()? {
                    println!("{}\t{}", f.name, f.query);
                }
                Ok(())
            }
            FilterCommand::Save { name, query } => {
                store.upsert_saved_filter(&name, &query)?;
                println!("saved filter {name}");
                Ok(())
            }
            FilterCommand::Delete { name } => {
                store.delete_saved_filter(&name)?;
                println!("deleted filter {name}");
                Ok(())
            }
        },
        LibraryCommand::List { account, status } => {
            let books = store.list_books(account.as_deref())?;
            for book in books {
                if let Some(filter) = &status {
                    if book.liberate_status.as_str() != filter.as_str() {
                        continue;
                    }
                }
                println!(
                    "{}\t{}\t{}\t{}",
                    book.asin_or_isbn(),
                    book.liberate_status.as_str(),
                    book.title,
                    book.storage_key.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
    }
}

async fn read_license_input(path: &std::path::Path) -> anyhow::Result<String> {
    if path.as_os_str() == "-" {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        tokio::io::stdin().read_to_string(&mut buf).await?;
        Ok(buf)
    } else {
        Ok(tokio::fs::read_to_string(path).await?)
    }
}

/// Resolve an Audible account + real Audible ASIN for `get-license`.
///
/// Accepts uuid / product_id / isbn / asin. Never sends a Libro ISBN or library
/// UUID to Audible's license API. Enriched Libro rows may supply an ASIN, but
/// the account must still be an Audible auth identity.
fn resolve_audible_license_target(
    store: &LibraryStore,
    id: &str,
    account: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let books = store.list_books(None)?;
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
        .list_books(None)?
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

fn title_id_matches(book: &libation_library::BookRecord, id: &str) -> bool {
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
