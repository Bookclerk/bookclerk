//! `libation config` — get settings (LibationCli: `get-setting`).

use clap::Subcommand;
use libation_config::{
    classic_key_aliases, resolve_replacement_characters, Config, StorageBackendKind,
};
use libation_liberate::{storage_key_with_rules, NamingContext};
use libation_library::LibraryStore;

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print a configuration value by dotted key or classic Settings.json name.
    Get {
        /// Dotted key (`download.quality`) or classic name (`FileDownloadQuality`).
        key: Option<String>,
        /// Bare list of all classic setting keys and values.
        #[arg(short, long)]
        bare: bool,
    },
    /// Print the effective configuration as TOML-ish summary.
    Show,
    /// Print resolved filesystem paths.
    Paths,
    /// Naming template helpers (Chardonnay tag engine).
    Template {
        #[command(subcommand)]
        command: TemplateCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum TemplateCommand {
    /// List supported naming template property tags.
    Tags,
    /// Preview folder/file templates for a library title.
    Preview {
        /// Title ASIN from the local library DB.
        asin: String,
        /// Account id when the ASIN exists on multiple accounts.
        #[arg(long)]
        account: Option<String>,
        /// Override `download.folder_template` for this preview.
        #[arg(long)]
        folder: Option<String>,
        /// Override `download.file_template` for this preview.
        #[arg(long)]
        file: Option<String>,
        /// File extension (default: m4b).
        #[arg(long, default_value = "m4b")]
        ext: String,
    },
}

pub fn run(command: ConfigCommand, config: &Config) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Get { key, bare } => {
            if bare {
                for (classic, dotted) in classic_key_aliases() {
                    if let Some(value) = lookup(config, dotted) {
                        println!("{classic}\t{value}");
                    }
                }
                return Ok(());
            }
            let key = key.ok_or_else(|| anyhow::anyhow!("pass a key or use --bare"))?;
            let dotted = classic_key_aliases()
                .get(key.as_str())
                .copied()
                .unwrap_or(key.as_str());
            let value = lookup(config, dotted)
                .ok_or_else(|| anyhow::anyhow!("unknown config key: {key}"))?;
            println!("{value}");
            Ok(())
        }
        ConfigCommand::Show => {
            println!("storage.backend = {:?}", config.storage.backend);
            println!(
                "storage.local.root = {}",
                config.storage.local.root.display()
            );
            println!("storage.s3.bucket = {}", config.storage.s3.bucket);
            println!("storage.s3.prefix = {}", config.storage.s3.prefix);
            println!("storage.s3.region = {}", config.storage.s3.region);
            println!(
                "storage.s3.endpoint = {}",
                config.storage.s3.endpoint.as_deref().unwrap_or("-")
            );
            println!(
                "storage.s3.force_path_style = {}",
                config.storage.s3.force_path_style
            );
            println!("download.quality = {:?}", config.download.quality);
            println!("download.format = {:?}", config.download.format);
            println!("download.widevine = {}", config.download.widevine);
            println!("download.xhe_aac = {}", config.download.xhe_aac);
            println!(
                "download.widevine_cdm = {}",
                config
                    .download
                    .widevine_cdm
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".into())
            );
            println!(
                "download.folder_template = {}",
                config.download.folder_template.as_deref().unwrap_or("-")
            );
            println!(
                "download.file_template = {}",
                config.download.file_template.as_deref().unwrap_or("-")
            );
            println!(
                "download.path_sanitization = {:?}",
                config.download.path_sanitization
            );
            println!(
                "download.download_cover = {}",
                config.download.download_cover
            );
            println!("download.download_pdf = {}", config.download.download_pdf);
            println!("download.create_cue = {}", config.download.create_cue);
            println!("download.output = {:?}", config.download.effective_output());
            println!(
                "download.ingest.quality = {:?}",
                config.download.ingest.quality
            );
            println!(
                "download.fixup_metadata = {}",
                config.download.fixup_metadata
            );
            println!(
                "download.chapter_json = {:?}",
                config.download.effective_chapter_json()
            );
            println!("download.cover_size = {}", config.download.cover_size);
            println!(
                "download.chapter_layout = {}",
                config.download.chapter_layout
            );
            println!("library.auto_liberate = {}", config.library.auto_liberate);
            println!(
                "library.scan_interval_minutes = {}",
                config.library.scan_interval_minutes
            );
            println!("daemon.listen = {}", config.daemon.listen);
            println!("daemon.json_logs = {}", config.daemon.json_logs);
            println!(
                "diagnostics.share_reports = {}",
                config.diagnostics.share_reports
            );
            println!(
                "diagnostics.collector_url = {}",
                config.diagnostics.effective_collector_url()
            );
            println!(
                "diagnostics.upload_on_crash = {}",
                config.diagnostics.upload_on_crash
            );
            println!(
                "diagnostics.upload_on_error_burst = {}",
                config.diagnostics.upload_on_error_burst
            );
            println!(
                "diagnostics.error_burst_threshold = {}",
                config.diagnostics.error_burst_threshold
            );
            println!(
                "diagnostics.error_burst_window_secs = {}",
                config.diagnostics.error_burst_window_secs
            );
            println!(
                "diagnostics.upload_on_warn_burst = {}",
                config.diagnostics.upload_on_warn_burst
            );
            println!(
                "diagnostics.warn_burst_threshold = {}",
                config.diagnostics.warn_burst_threshold
            );
            println!(
                "diagnostics.warn_burst_window_secs = {}",
                config.diagnostics.warn_burst_window_secs
            );
            println!(
                "diagnostics.ring_buffer_capacity = {}",
                config.diagnostics.ring_buffer_capacity
            );
            Ok(())
        }
        ConfigCommand::Paths => {
            let paths = config.paths();
            println!("files_dir\t{}", paths.files_dir.display());
            println!("config_file\t{}", paths.config_file.display());
            println!("library_db\t{}", paths.library_db.display());
            println!("cache_dir\t{}", paths.cache_dir.display());
            println!("log_dir\t{}", paths.log_dir.display());
            Ok(())
        }
        ConfigCommand::Template { command } => run_template(command, config),
    }
}

fn run_template(command: TemplateCommand, config: &Config) -> anyhow::Result<()> {
    match command {
        TemplateCommand::Tags => {
            for (name, fmt) in libation_naming::property_tag_names() {
                println!("{name}\t{}", if *fmt { "formatted" } else { "plain" });
            }
            Ok(())
        }
        TemplateCommand::Preview {
            asin,
            account,
            folder,
            file,
            ext,
        } => {
            let paths = config.paths();
            let store = LibraryStore::open(&paths.library_db)?;
            let book = resolve_book_for_preview(&store, &asin, account.as_deref())?;
            let ctx = NamingContext {
                asin: book.asin_or_isbn().to_string(),
                title: book.title.clone(),
                subtitle: book.subtitle.clone(),
                authors: book.authors.clone(),
                narrators: book.narrators.clone(),
                series: book.series.clone(),
                series_index: book.series_index.clone(),
                account_id: Some(book.account_id.clone()),
                locale: Some(book.marketplace.clone()),
                publisher: book.publisher.clone(),
                categories: book.categories.clone(),
                length_minutes: book.length_minutes,
                is_abridged: book.is_abridged,
                content_kind: Some(book.content_kind.clone()),
                ..Default::default()
            };
            let folder_tpl = folder
                .as_deref()
                .or(config.download.folder_template.as_deref());
            let file_tpl = file.as_deref().or(config.download.file_template.as_deref());
            let rules = resolve_replacement_characters(
                &config.download.replacement_characters,
                config.download.path_sanitization,
                config.storage.backend == StorageBackendKind::S3,
            );
            let key = storage_key_with_rules(&ctx, folder_tpl, file_tpl, &ext, &rules);
            println!("asin\t{}", book.asin_or_isbn());
            println!(
                "folder_template\t{}",
                folder_tpl.unwrap_or("<author>/<title>")
            );
            println!("file_template\t{}", file_tpl.unwrap_or("<asin>"));
            println!("path_sanitization\t{:?}", config.download.path_sanitization);
            println!("storage_key\t{key}");
            Ok(())
        }
    }
}

fn resolve_book_for_preview(
    store: &LibraryStore,
    asin: &str,
    account: Option<&str>,
) -> anyhow::Result<libation_library::BookRecord> {
    if let Some(account) = account {
        return store
            .get_book(asin, account)
            .map_err(|e| e.into())
            .and_then(|opt| {
                opt.ok_or_else(|| anyhow::anyhow!("ASIN {asin} not found for account {account}"))
            });
    }
    let matches: Vec<_> = store
        .list_books(None)?
        .into_iter()
        .filter(|b| {
            b.uuid.eq_ignore_ascii_case(asin)
                || b.product_id.eq_ignore_ascii_case(asin)
                || b.isbn
                    .as_ref()
                    .is_some_and(|i| i.eq_ignore_ascii_case(asin))
                || b.asin
                    .as_ref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(asin))
        })
        .collect();
    match matches.as_slice() {
        [] => anyhow::bail!("ASIN {asin} not in library — run `libation library scan`"),
        [one] => Ok(one.clone()),
        many => anyhow::bail!(
            "ASIN {asin} exists on {} accounts; pass --account ({})",
            many.len(),
            many.iter()
                .map(|b| b.account_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn lookup(config: &Config, key: &str) -> Option<String> {
    let paths = config.paths.as_ref();
    Some(match key {
        "storage.backend" => format!("{:?}", config.storage.backend).to_ascii_lowercase(),
        "storage.local.root" => config.storage.local.root.display().to_string(),
        "storage.s3.bucket" => config.storage.s3.bucket.clone(),
        "storage.s3.prefix" => config.storage.s3.prefix.clone(),
        "storage.s3.region" => config.storage.s3.region.clone(),
        "storage.s3.endpoint" => config.storage.s3.endpoint.clone().unwrap_or_default(),
        "storage.s3.force_path_style" => config.storage.s3.force_path_style.to_string(),
        "download.quality" => format!("{:?}", config.download.quality).to_ascii_lowercase(),
        "download.format" => format!("{:?}", config.download.format).to_ascii_lowercase(),
        "download.widevine" => config.download.widevine.to_string(),
        "download.xhe_aac" => config.download.xhe_aac.to_string(),
        "download.widevine_cdm" => config
            .download
            .widevine_cdm
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "download.folder_template" => config.download.folder_template.clone().unwrap_or_default(),
        "download.file_template" => config.download.file_template.clone().unwrap_or_default(),
        "download.path_sanitization" => {
            format!("{:?}", config.download.path_sanitization).to_ascii_lowercase()
        }
        "download.download_cover" => config.download.download_cover.to_string(),
        "download.download_pdf" => config.download.download_pdf.to_string(),
        "download.create_cue" => config.download.create_cue.to_string(),
        "download.fixup_metadata" => config.download.fixup_metadata.to_string(),
        "download.chapter_json" => {
            format!("{:?}", config.download.effective_chapter_json()).to_ascii_lowercase()
        }
        "download.save_chapter_json" => config
            .download
            .effective_chapter_json()
            .wants_any()
            .to_string(),
        "download.output" => {
            format!("{:?}", config.download.effective_output()).to_ascii_lowercase()
        }
        "download.save_metadata_json" => config.download.save_metadata_json.to_string(),
        "download.cover_size" => config.download.cover_size.clone(),
        "download.chapter_layout" => config.download.chapter_layout.clone(),
        "download.overwrite_existing" => config.download.overwrite_existing.to_string(),
        "download.in_progress" => config
            .download
            .in_progress
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "download.bad_book_action" => format!("{:?}", config.download.bad_book_action),
        "download.split_files_by_chapter" => config.download.split_files_by_chapter.to_string(),
        "download.split_mp3_max_mb" => config.download.split_mp3_max_mb.to_string(),
        "download.ingest.quality" => {
            format!("{:?}", config.download.ingest.quality).to_ascii_lowercase()
        }
        "download.chapter_file_template" => config
            .download
            .chapter_file_template
            .clone()
            .unwrap_or_default(),
        "download.chapter_title_template" => config
            .download
            .chapter_title_template
            .clone()
            .unwrap_or_default(),
        "download.minimum_file_duration_minutes" => {
            config.download.minimum_file_duration_minutes.to_string()
        }
        "download.combine_nested_chapter_titles" => {
            config.download.combine_nested_chapter_titles.to_string()
        }
        "download.merge_opening_and_end_credits" => {
            config.download.merge_opening_and_end_credits.to_string()
        }
        "download.strip_unabridged" => config.download.strip_unabridged.to_string(),
        "download.strip_audible_brand_audio" => {
            config.download.strip_audible_brand_audio.to_string()
        }
        "download.download_clips_bookmarks" => config.download.download_clips_bookmarks.to_string(),
        "download.retain_aax_file" => config.download.retain_aax_file.to_string(),
        "download.download_speed_limit_kbps" => {
            config.download.download_speed_limit_kbps.to_string()
        }
        "download.lame.target" => config.download.lame.target.clone(),
        "download.lame.vbr_quality" => config.download.lame.vbr_quality.to_string(),
        "download.lame.bitrate_kbps" => config.download.lame.bitrate_kbps.to_string(),
        "download.lame.mode" => config.download.lame.mode.clone(),
        "download.lame.downsample_mono" => config.download.lame.downsample_mono.to_string(),
        "download.lame.constant_bitrate" => config.download.lame.constant_bitrate.to_string(),
        "download.max_sample_rate" => config
            .download
            .max_sample_rate
            .map(|n| n.to_string())
            .unwrap_or_default(),
        "download.creation_time" => {
            format!("{:?}", config.download.creation_time).to_ascii_lowercase()
        }
        "download.last_write_time" => {
            format!("{:?}", config.download.last_write_time).to_ascii_lowercase()
        }
        "library.auto_liberate" => config.library.auto_liberate.to_string(),
        "library.scan_interval_minutes" => config.library.scan_interval_minutes.to_string(),
        "library.import_episodes" => config.library.import_episodes.to_string(),
        "library.import_plus_titles" => config.library.import_plus_titles.to_string(),
        "library.download_episodes" => config.library.download_episodes.to_string(),
        "library.save_podcasts_to_parent_folder" => {
            config.library.save_podcasts_to_parent_folder.to_string()
        }
        "library.enrich_from_audible" => config.library.enrich_from_audible.to_string(),
        "library.enrich_min_confidence" => config.library.enrich_min_confidence.to_string(),
        "daemon.listen" => config.daemon.listen.clone(),
        "daemon.json_logs" => config.daemon.json_logs.to_string(),
        "diagnostics.share_reports" => config.diagnostics.share_reports.to_string(),
        "diagnostics.collector_url" => config.diagnostics.effective_collector_url(),
        "diagnostics.upload_on_crash" => config.diagnostics.upload_on_crash.to_string(),
        "diagnostics.upload_on_error_burst" => config.diagnostics.upload_on_error_burst.to_string(),
        "diagnostics.error_burst_threshold" => config.diagnostics.error_burst_threshold.to_string(),
        "diagnostics.error_burst_window_secs" => {
            config.diagnostics.error_burst_window_secs.to_string()
        }
        "diagnostics.upload_on_warn_burst" => config.diagnostics.upload_on_warn_burst.to_string(),
        "diagnostics.warn_burst_threshold" => config.diagnostics.warn_burst_threshold.to_string(),
        "diagnostics.warn_burst_window_secs" => {
            config.diagnostics.warn_burst_window_secs.to_string()
        }
        "diagnostics.ring_buffer_capacity" => config.diagnostics.ring_buffer_capacity.to_string(),
        "paths.files_dir" => paths?.files_dir.display().to_string(),
        "paths.config_file" => paths?.config_file.display().to_string(),
        "paths.library_db" => paths?.library_db.display().to_string(),
        "paths.cache_dir" => paths?.cache_dir.display().to_string(),
        "paths.log_dir" => paths?.log_dir.display().to_string(),
        _ => return None,
    })
}
