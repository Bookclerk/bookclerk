//! `bookclerk library` — scan, acquire, accounts, set-status, get-license, search, export.

use bookclerk_acquire::{
    convert_book, match_storage_to_library, AcquireRequest, ConvertRequest, MatchStorageOptions,
};
use bookclerk_config::{AudioQuality, Config};
use bookclerk_library::{AcquireStatus, LibraryStore};
use bookclerk_plugin_host::HostControlPlane;
use bookclerk_search::SearchEngine;
use bookclerk_source::{source_scope, DownloadOptions, FetchOptions};
use bookclerk_storage::StorageBackend;
use clap::Subcommand;

use crate::commands::export::{export_csv, export_json, export_xlsx, filter_books, load_books};
use crate::commands::queued;
use crate::format_out;
use crate::progress::BatchProgress;
use crate::registry::{open_host, resolve_source, sources};

#[derive(Debug, Subcommand)]
/// `bookclerk library` subcommands for scan, acquire, accounts, and export.
pub enum LibraryCommand {
    /// List configured bookstore accounts.
    Accounts {
        /// Content source filter. Omit for all.
        #[arg(long)]
        source: Option<String>,
        /// Tab-separated values for scripts (source, account, name, locale, scan, auth).
        #[arg(short, long)]
        bare: bool,
    },
    /// Enable or disable an account for library scans (GUI: Include in library scan).
    SetScan {
        /// Account id, auth-file stem, or nickname.
        account: String,
        /// Content source, when the same account id exists on several plugins.
        #[arg(long)]
        source: Option<String>,
        /// Include this account when scanning (default: true).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        scan: bool,
    },
    /// Show token validity / refresh health across sources.
    Status {
        /// Content source filter. Omit for all.
        #[arg(long)]
        source: Option<String>,
    },
    /// Remove store credentials but keep acquired books and account rows.
    RevokeAccount {
        /// Account id, auth-file stem, or nickname.
        account: String,
        /// Content source, when the same account id exists on several plugins.
        #[arg(long)]
        source: Option<String>,
    },
    /// Sync the library from the content sources: queues one scan job per
    /// account (every scan-enabled account when none is named) for a
    /// bookclerkd node to run, and waits for them.
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
        /// Pin the scan to one deployed plugin instance (default: each
        /// plugin's default instance).
        #[arg(long)]
        instance: Option<String>,
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
        /// Queue the scan jobs and print their ids without waiting for a
        /// bookclerkd node to run them.
        #[arg(long, conflicts_with = "match_storage")]
        no_wait: bool,
    },
    /// Download + decrypt + store titles: queues one acquire job per title
    /// id (or one for every pending title) for a bookclerkd node to run, and
    /// waits for them. `--dry-run` only prints planned keys, here.
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
        /// Fetch through this deployed plugin instance (default: the book's
        /// plugin default instance).
        #[arg(long)]
        instance: Option<String>,
        /// Dry-run: print planned storage keys only.
        #[arg(long)]
        dry_run: bool,
        /// Re-download even when matching media already exists in storage.
        #[arg(short, long)]
        force: bool,
        /// Download companion PDF only.
        #[arg(short, long)]
        pdf: bool,
        /// Queue the acquire jobs and print their ids without waiting for
        /// a bookclerkd node to run them.
        #[arg(long)]
        no_wait: bool,
    },
    /// Match storage files to library rows and update acquire status.
    ///
    /// Lists audio in storage and probes custom object metadata (S3
    /// `HeadObject` / local `.bookclerk-meta.json`) without downloading bodies.
    /// Falls back to ASIN/ISBN tokens embedded in the object key.
    SetStatus {
        #[arg(long)]
        /// Limit matching to this account id, stem, or nickname.
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
        /// Title ids to update (UUID, ASIN, ISBN, or product id); empty means all matching the account filter.
        asins: Vec<String>,
    },
    /// Fetch a content license for an ASIN.
    GetLicense {
        /// Title id to resolve to an Audible ASIN (UUID, ASIN, ISBN, or product id).
        asin: String,
        #[arg(long)]
        /// Audible account when the title is missing or owned on multiple accounts.
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
        /// Write CSV (default when no format flag is set and the path ends in `.csv`).
        csv: bool,
        #[arg(long)]
        /// Write JSON.
        json: bool,
        #[arg(long)]
        /// Write XLSX.
        xlsx: bool,
        /// Limit to specific ASINs.
        asins: Vec<String>,
        #[arg(long)]
        /// Limit the export to this account id, stem, or nickname.
        account: Option<String>,
    },
    /// List books in the local library DB.
    List {
        #[arg(long)]
        /// Limit listed rows to this account id, stem, or nickname.
        account: Option<String>,
        #[arg(long)]
        /// Filter by acquire status (`acquired`, `not_downloaded`, …).
        status: Option<String>,
    },
    /// Convert acquired m4b/m4a to mp3.
    Convert {
        #[arg(long)]
        /// Limit conversion to this account id, stem, or nickname.
        account: Option<String>,
        #[arg(short, long)]
        /// Re-encode even when an MP3 already exists in storage.
        force: bool,
        #[arg(value_name = "ASIN")]
        /// Title ids to convert; empty means all acquired titles matching the account filter.
        asins: Vec<String>,
    },
    /// Manage saved quick filters.
    Filters {
        #[command(subcommand)]
        /// Nested saved-filter list/save/delete action.
        command: FilterCommand,
    },
}

#[derive(Debug, Subcommand)]
/// Nested `library filters` actions for named Lucene quick filters.
pub(crate) enum FilterCommand {
    /// List saved filters.
    List,
    /// Save or update a named filter.
    Save {
        /// Saved filter name to create or overwrite.
        name: String,
        /// Lucene-style query string.
        query: String,
    },
    /// Delete a saved filter.
    Delete {
        /// Saved filter name to delete.
        name: String,
    },
}

/// Dispatches a `library` subcommand against the opened library store.
pub async fn run(command: LibraryCommand, config: &Config) -> anyhow::Result<()> {
    let host = open_host(config).await?;
    // Database-authoritative domains (`core.events`, `core.output`) are
    // overlaid on the host config; read that, not the TOML seed.
    let overlaid = host.config.clone();
    let config = &overlaid;
    let paths = config.paths();
    let store = host.store.clone();

    match command {
        LibraryCommand::Scan {
            account,
            accounts,
            source,
            instance,
            match_storage,
            fix_layout,
            no_wait,
        } => {
            let mut scan_accounts = accounts;
            if let Some(one) = account {
                scan_accounts.push(one);
            }
            let finished = queued::scan(
                &store,
                config,
                queued::ScanArgs {
                    accounts: &scan_accounts,
                    source: source.as_deref(),
                    instance: instance.as_deref(),
                    wait: !no_wait,
                },
            )
            .await?;
            if !finished {
                return Ok(());
            }
            if match_storage {
                let (storage, download) = storage_and_options_for_host(&host, config).await?;
                let recon = match_storage_to_library(
                    &store,
                    storage.as_ref(),
                    MatchStorageOptions {
                        account: scan_accounts.first().cloned(),
                        clear_missing: true,
                        fix_layout: fix_layout || config.library.fix_storage_layout,
                        download,
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
            instance,
            dry_run,
            force,
            pdf,
            no_wait,
        } => {
            if !dry_run {
                // Real acquires go through the job queue; only a dry run
                // plans keys here.
                let titles: Vec<String> = asin.into_iter().chain(isbn).chain(asins).collect();
                queued::acquire(
                    &store,
                    config,
                    queued::AcquireArgs {
                        titles: &titles,
                        account: account.as_deref(),
                        instance: instance.as_deref(),
                        force,
                        pdf_only: pdf,
                        wait: !no_wait,
                    },
                )
                .await?;
                return Ok(());
            }
            let cfg = config;
            let destinations = crate::registry::destinations(&host).await?;
            let destinations =
                bookclerk_plugin_host::build_acquire_destinations(cfg, &destinations)?;
            // The primary destination's options carry its naming overrides
            // and prefix, so the planned keys match what an acquire writes.
            let options = destinations.primary_destination().options.clone();
            let registry = sources(&host).await?;

            let books = store.list_books(account.as_deref()).await?;
            let filter_ids: Vec<String> = asin.into_iter().chain(isbn).chain(asins).collect();
            let (targets, foreign): (Vec<_>, Vec<_>) = books
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
                .partition(|b| {
                    // A pinned instance serves its own plugin's books only;
                    // the rest are skipped, not failed.
                    instance
                        .as_deref()
                        .is_none_or(|pin| registry.route(&b.plugin_key, Some(pin)).is_some())
                });
            if !foreign.is_empty() {
                eprintln!(
                    "skipping {} title(s) owned by other plugins (--instance pins one plugin)",
                    foreign.len()
                );
            }

            if targets.is_empty() {
                eprintln!("nothing to acquire");
                return Ok(());
            }

            for book in targets {
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
                    job_id: None,
                    temp_quota_bytes: None,
                };
                let key = bookclerk_acquire::planned_storage_key(&store, &req).await;
                let key = if pdf {
                    bookclerk_acquire::sidecar_key(&key, "pdf")
                } else {
                    key
                };
                println!("{}\t{}", book.asin_or_isbn(), key);
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
            let (storage, download) = storage_and_options_for_host(&host, config).await?;
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
                    download,
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
            let registry = sources(&host).await?;
            let source = resolve_source(&registry, "audible", None)?;
            let (account_key, license_asin) =
                resolve_license_target(&store, &asin, account.as_deref()).await?;
            let mut download = DownloadOptions::from(config);
            download.quality = instance_audio_quality(&store, source.as_ref()).await;
            let inspected = source
                .inspect_title(
                    &source_scope(&store, source.as_ref()),
                    &account_key,
                    &license_asin,
                    &FetchOptions {
                        download,
                        cache_dir: paths.cache_dir.clone(),
                        files_dir: paths.files_dir.clone(),
                        max_cache_bytes: None,
                        cancel: None,
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
            let storage = storage_for_host(&host).await?;
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
                job_id: None,
                temp_quota_bytes: Some(config.jobs.temp_quota_bytes),
                cancel: None,
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
        LibraryCommand::Accounts { source, bare } => {
            list_all_accounts(&host, source.as_deref(), bare).await
        }
        LibraryCommand::SetScan {
            account,
            source,
            scan,
        } => {
            let registry = sources(&host).await?;
            let plugin_key = match source.as_deref() {
                Some(needle) => Some(account_plugin_key(&store, &registry, needle).await?),
                None => None,
            };
            let (plugin_key, account_id) =
                match store.find_account(&account, plugin_key.as_deref()).await? {
                    Some(acct) => (acct.plugin_key, acct.account_id),
                    None => {
                        // Not in the library yet (connected, never scanned):
                        // ask the deployed storefronts and record the row.
                        let mut found = None;
                        for src in registry.routed() {
                            if plugin_key
                                .as_deref()
                                .is_some_and(|key| key != src.plugin_key())
                            {
                                continue;
                            }
                            let Ok(accounts) =
                                src.list_accounts(&source_scope(&store, src.as_ref())).await
                            else {
                                continue;
                            };
                            if let Some(a) = accounts.into_iter().find(|a| {
                                a.account_id.eq_ignore_ascii_case(&account)
                                    || a.label
                                        .as_deref()
                                        .is_some_and(|l| l.eq_ignore_ascii_case(&account))
                            }) {
                                store
                                    .upsert_account(
                                        src.plugin_key(),
                                        &a.account_id,
                                        &a.marketplace,
                                        a.label.as_deref(),
                                        scan,
                                        src.id(),
                                    )
                                    .await?;
                                found = Some((src.plugin_key().to_string(), a.account_id));
                                break;
                            }
                        }
                        found.ok_or_else(|| {
                            anyhow::anyhow!(
                                "account `{account}` not found — run `bookclerk library accounts`"
                            )
                        })?
                    }
                };
            store
                .set_scan_enabled(&plugin_key, &account_id, scan)
                .await?;
            format_out::line(format!(
                "account {} scan_enabled={}",
                account_id,
                if scan { "yes" } else { "no" }
            ));
            Ok(())
        }
        LibraryCommand::Status { source } => {
            let registry = sources(&host).await?;
            let sources: Vec<_> = match source.as_deref() {
                Some(needle) => vec![resolve_source(&registry, needle, None)?],
                None => registry.routed(),
            };
            let mut any = false;
            for src in sources {
                let accounts = src
                    .list_accounts(&source_scope(&store, src.as_ref()))
                    .await?;
                for acct in accounts {
                    any = true;
                    format_out::line(format!(
                        "{}\t{}\t{}\tstatus=present",
                        acct.source, acct.account_id, acct.marketplace
                    ));
                }
            }
            if !any {
                eprintln!("no accounts configured");
            }
            Ok(())
        }
        LibraryCommand::RevokeAccount { account, source } => {
            let registry = sources(&host).await?;
            let plugin_key = match source.as_deref() {
                Some(needle) => Some(account_plugin_key(&store, &registry, needle).await?),
                None => None,
            };
            let acct = store
                .find_account(&account, plugin_key.as_deref())
                .await?
                .ok_or_else(|| anyhow::anyhow!("account `{account}` not found in library DB"))?;
            let scope = store.scope(acct.plugin_key.as_str(), acct.source.as_str());
            if let Ok(content) = registry.require_routed(&acct.plugin_key, None) {
                if let Err(e) = content.revoke_credentials(&scope, &acct.account_id).await {
                    tracing::warn!(
                        error = %e,
                        source = %acct.source,
                        account = %acct.account_id,
                        "failed to revoke source credentials"
                    );
                }
            } else {
                // Plugin not deployed here — still clear common secret patterns.
                bookclerk_source::revoke_credentials_default(&scope, &acct.account_id).await?;
            }
            store
                .revoke_credentials(&acct.plugin_key, &acct.account_id)
                .await?;
            format_out::line(format!(
                "revoked credentials for {} (books retained, scan_enabled=false)",
                acct.account_id
            ));
            Ok(())
        }
    }
}

/// Prints storefront plus DB-only accounts, optionally filtered by source.
///
/// Accounts belong to a plugin, so the storefront list and the database rows
/// are matched on `(plugin_key, account_id)`: a third-party plugin that
/// reuses an alias never claims another plugin's accounts.
async fn list_all_accounts(
    host: &HostControlPlane,
    source_filter: Option<&str>,
    bare: bool,
) -> anyhow::Result<()> {
    let store = &host.store;
    let registry = sources(host).await?;
    let db_accounts = store.list_accounts().await?;

    let sources: Vec<_> = match source_filter {
        Some(needle) => vec![resolve_source(&registry, needle, None)?],
        None => registry.routed(),
    };
    let filter_key = source_filter
        .is_some()
        .then(|| sources[0].plugin_key().to_string());

    let mut listed = std::collections::HashSet::new();
    let mut any = false;

    let scan_by_key: std::collections::HashMap<(String, String), bool> = db_accounts
        .iter()
        .map(|a| ((a.plugin_key.clone(), a.account_id.clone()), a.scan_enabled))
        .collect();

    for src in sources {
        let key = src.plugin_key().to_string();
        let accounts = src
            .list_accounts(&source_scope(store, src.as_ref()))
            .await?;
        for acct in accounts {
            any = true;
            listed.insert((key.clone(), acct.account_id.clone()));
            let name = acct.label.as_deref().unwrap_or(&acct.account_id);
            let scan = yes_no(
                scan_by_key
                    .get(&(key.clone(), acct.account_id.clone()))
                    .copied()
                    .unwrap_or(true),
            );
            let auth_ok = true;
            let status = String::from("ok");
            if bare {
                format_out::line(format!(
                    "{}\t{}\t{name}\t{}\t{scan}\t{}",
                    acct.source,
                    acct.account_id,
                    acct.marketplace,
                    yes_no(auth_ok)
                ));
            } else {
                format_out::line(format!(
                    "{}\t{}\t{}\t{}\t{status}",
                    acct.source, acct.account_id, acct.marketplace, name
                ));
            }
        }
    }

    // DB-only rows (e.g. after migrate, or a plugin not deployed on this host).
    for db in db_accounts {
        if listed.contains(&(db.plugin_key.clone(), db.account_id.clone())) {
            continue;
        }
        if let Some(key) = &filter_key {
            if db.plugin_key != *key {
                continue;
            }
        }
        any = true;
        let name = db.label.as_deref().unwrap_or(&db.account_id);
        if bare {
            format_out::line(format!(
                "{}\t{}\t{name}\t{}\t{}\tno",
                db.source,
                db.account_id,
                db.marketplace,
                yes_no(db.scan_enabled)
            ));
        } else {
            format_out::line(format!(
                "{}\t{}\t{}\t{}\tdb_only",
                db.source, db.account_id, db.marketplace, name
            ));
        }
    }

    if !any {
        eprintln!("no accounts configured — connect a store in the Bookclerk Accounts UI");
    }
    Ok(())
}

/// Resolves `--source` to the plugin key that owns an account.
///
/// A storefront deployed on this host wins (alias, PluginKey, or instance
/// id). Otherwise the needle may name a PluginKey or display alias already
/// recorded on an account row: the plugin can be deployed on another cluster
/// host, and account rows belong to the plugin key, not to a host.
async fn account_plugin_key(
    store: &LibraryStore,
    registry: &bookclerk_source::SourceRegistry,
    needle: &str,
) -> anyhow::Result<String> {
    if let Ok(source) = registry.require_routed(needle, None) {
        return Ok(source.plugin_key().to_string());
    }
    let mut keys: Vec<String> = store
        .list_accounts()
        .await?
        .into_iter()
        .filter(|a| a.plugin_key == needle || a.source.eq_ignore_ascii_case(needle))
        .map(|a| a.plugin_key)
        .collect();
    keys.sort();
    keys.dedup();
    match keys.len() {
        0 => resolve_source(registry, needle, None).map(|src| src.plugin_key().to_string()),
        1 => Ok(keys.pop().unwrap_or_default()),
        _ => Err(anyhow::anyhow!(
            "`{needle}` names more than one plugin on this library ({}); pass the PluginKey",
            keys.join(", ")
        )),
    }
}

/// Formats a boolean as `yes` / `no` for tabular CLI output.
fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
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

/// Whether `id` matches the book's UUID, product id, ISBN, or ASIN (case-insensitive).
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

/// Storage backend over the destination deployments present on this host.
async fn storage_for_host(host: &HostControlPlane) -> anyhow::Result<Box<dyn StorageBackend>> {
    let destinations = crate::registry::destinations(host).await?;
    Ok(bookclerk_plugin_host::build_storage_backend(&destinations)?)
}

/// Listing backend plus the primary destination's download options, so a
/// match or layout pass computes the same keys acquire wrote (destination
/// naming overrides, S3 prefix, and S3 path limits included).
async fn storage_and_options_for_host(
    host: &HostControlPlane,
    config: &Config,
) -> anyhow::Result<(Box<dyn StorageBackend>, DownloadOptions)> {
    let destinations = crate::registry::destinations(host).await?;
    let acquire = bookclerk_plugin_host::build_acquire_destinations(config, &destinations)?;
    let options = acquire.primary_destination().options.clone();
    let storage = acquire.into_listing_backend()?;
    Ok((storage, options))
}

/// The routed source's `bitrate` setting as an [`AudioQuality`]: `normal`
/// lowers the license request, anything else (or no instance document)
/// keeps the default high quality.
async fn instance_audio_quality(
    store: &LibraryStore,
    source: &dyn bookclerk_source::ContentSource,
) -> AudioQuality {
    use bookclerk_library::control_plane::{load_instance_config, PluginInstanceId, SettingValue};
    let Some(instance_id) = source
        .plugin_instance_id()
        .and_then(|id| PluginInstanceId::parse(id).ok())
    else {
        return AudioQuality::High;
    };
    match load_instance_config(store, &instance_id).await {
        Ok(document) => match document.body.settings.get("bitrate") {
            Some(SettingValue::String(text)) if text.eq_ignore_ascii_case("normal") => {
                AudioQuality::Normal
            }
            _ => AudioQuality::High,
        },
        Err(err) => {
            tracing::warn!(error = %err, "instance document unreadable; using high quality");
            AudioQuality::High
        }
    }
}
