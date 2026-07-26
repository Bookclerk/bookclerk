//! `bookclerk config` — get/set settings and naming helpers.

use bookclerk_acquire::{storage_key_with_contexts, NamingContext};
use bookclerk_config::{
    apply_setting_overrides, classic_key_aliases, resolve_replacement_characters, Config,
    NamingProfile,
};
use bookclerk_library::LibraryStore;
use bookclerk_source::DownloadOptions;
use chrono::Datelike;
use clap::Subcommand;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print a configuration value by dotted key or classic Settings.json name.
    Get {
        /// Dotted key (`sources.audible.bitrate`) or classic name (`FileDownloadQuality`).
        key: Option<String>,
        /// Bare list of all classic setting keys and values.
        #[arg(short, long)]
        bare: bool,
    },
    /// Set a configuration value and write `config.toml`.
    Set {
        /// Dotted key (`library.auto_acquire`) or classic name (`AutoDownloadEpisodes`).
        key: String,
        /// Value to assign.
        value: String,
    },
    /// Print the effective configuration as TOML-ish summary.
    Show,
    /// Print resolved filesystem paths.
    Paths,
    /// Naming template helpers.
    Template {
        #[command(subcommand)]
        command: TemplateCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum TemplateCommand {
    /// List supported naming template property tags.
    Tags,
    /// List built-in naming profiles and their templates.
    Profiles,
    /// Preview folder/file templates for a library title.
    Preview {
        /// Title ASIN from the local library DB.
        asin: String,
        /// Account id when the ASIN exists on multiple accounts.
        #[arg(long)]
        account: Option<String>,
        /// Override `output.naming_profile` for this preview.
        #[arg(long)]
        profile: Option<String>,
        /// Override `output.folder_template` for this preview.
        #[arg(long)]
        folder: Option<String>,
        /// Override `output.file_template` for this preview.
        #[arg(long)]
        file: Option<String>,
        /// File extension (default: m4b).
        #[arg(long, default_value = "m4b")]
        ext: String,
    },
}

pub fn run(command: ConfigCommand, config: &Config, format: OutputFormat) -> anyhow::Result<()> {
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
            let payload = serde_json::json!({ "key": dotted, "value": value });
            emit(format, &payload, || println!("{value}"))
        }
        ConfigCommand::Set { key, value } => {
            let mut cfg = config.clone();
            let dotted = classic_key_aliases()
                .get(key.as_str())
                .copied()
                .unwrap_or(key.as_str())
                .to_string();
            apply_setting_overrides(&mut cfg, &[(&dotted, value.as_str())]);
            let path = cfg.paths().config_file.clone();
            cfg.write_toml_file(&path)?;
            let new_value = lookup(&cfg, &dotted).unwrap_or_else(|| value.clone());
            let payload = serde_json::json!({
                "key": dotted,
                "value": new_value,
                "config": path.display().to_string(),
            });
            emit(format, &payload, || {
                println!("set {dotted}={new_value}");
                println!("wrote {}", path.display());
            })
        }
        ConfigCommand::Show => {
            println!(
                "output.backends = {}",
                config.output.enabled_backend_names().join(",")
            );
            println!(
                "output.path_limit_prefix = {}",
                config.output.path_limit_prefix()
            );
            println!("output.local.enabled = {}", config.output.local.enabled);
            println!("output.local.root = {}", config.output.local.root.display());
            println!("output.local.prefix = {}", config.output.local.prefix);
            println!("output.s3.enabled = {}", config.output.s3.enabled);
            println!("output.s3.bucket = {}", config.output.s3.bucket);
            println!("output.s3.prefix = {}", config.output.s3.prefix);
            println!("output.s3.region = {}", config.output.s3.region);
            println!(
                "output.s3.endpoint = {}",
                config.output.s3.endpoint.as_deref().unwrap_or("-")
            );
            println!(
                "output.s3.force_path_style = {}",
                config.output.s3.force_path_style
            );
            println!(
                "output.s3.credentials_file = {}",
                config
                    .output
                    .s3
                    .credentials_file
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .or_else(|| {
                        config
                            .resolved_s3_credentials_path()
                            .map(|p| p.display().to_string())
                    })
                    .unwrap_or_else(|| "-".into())
            );
            for (id, value) in &config.sources.plugins {
                let Some(table) = value.as_table() else {
                    println!("sources.{id} = {value}");
                    continue;
                };
                for (key, v) in table {
                    println!("sources.{id}.{key} = {v}");
                }
            }
            println!("output.format = {:?}", config.output.format);
            println!("output.widevine = {}", config.output.widevine);
            println!("output.xhe_aac = {}", config.output.xhe_aac);
            println!(
                "output.widevine_cdm = {}",
                config
                    .output
                    .widevine_cdm
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".into())
            );
            println!(
                "output.naming_profile = {}",
                config.output.naming_profile.as_str()
            );
            let resolved = config.output.resolve_naming_templates();
            println!(
                "output.folder_template = {}{}",
                config.output.folder_template.as_deref().unwrap_or("-"),
                if config.output.folder_template.is_none() {
                    format!(" (profile: {})", resolved.folder)
                } else {
                    String::new()
                }
            );
            println!(
                "output.file_template = {}{}",
                config.output.file_template.as_deref().unwrap_or("-"),
                if config.output.file_template.is_none() {
                    format!(" (profile: {})", resolved.file)
                } else {
                    String::new()
                }
            );
            println!(
                "output.path_sanitization = {:?}",
                config.output.path_sanitization
            );
            println!(
                "output.max_filename_length = {}",
                config.output.max_filename_length
            );
            println!("output.download_cover = {}", config.output.download_cover);
            println!("output.download_pdf = {}", config.output.download_pdf);
            println!("output.create_cue = {}", config.output.create_cue);
            println!("output.fixup_metadata = {}", config.output.fixup_metadata);
            println!(
                "output.chapter_json = {:?}",
                config.output.effective_chapter_json()
            );
            println!("output.cover_size = {}", config.output.cover_size);
            println!("output.chapter_layout = {}", config.output.chapter_layout);
            println!("library.auto_acquire = {}", config.library.auto_acquire);
            println!(
                "library.fix_storage_layout = {}",
                config.library.fix_storage_layout
            );
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
            for (name, fmt) in bookclerk_naming::property_tag_names() {
                println!("{name}\t{}", if *fmt { "formatted" } else { "plain" });
            }
            Ok(())
        }
        TemplateCommand::Profiles => {
            for profile in NamingProfile::all() {
                let t = profile.templates();
                println!("{}\t{}", profile.as_str(), profile.description());
                println!("  folder_template\t{}", t.folder);
                println!("  file_template\t{}", t.file);
                println!("  chapter_file_template\t{}", t.chapter_file);
            }
            Ok(())
        }
        TemplateCommand::Preview {
            asin,
            account,
            profile,
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
                year_published: book.published_at.map(|dt| dt.year()),
                account_id: Some(book.account_id.clone()),
                locale: Some(book.marketplace.clone()),
                publisher: book.publisher.clone(),
                categories: book.categories.clone(),
                length_minutes: book.length_minutes,
                is_abridged: book.is_abridged,
                content_kind: Some(book.content_kind.clone()),
                ..Default::default()
            };
            let naming_profile = profile
                .as_deref()
                .and_then(NamingProfile::parse)
                .unwrap_or(config.output.naming_profile);
            let resolved = bookclerk_config::ResolvedNamingTemplates::resolve(
                naming_profile,
                folder
                    .as_deref()
                    .or(config.output.folder_template.as_deref()),
                file.as_deref().or(config.output.file_template.as_deref()),
                config.output.chapter_file_template.as_deref(),
            );
            let rules = resolve_replacement_characters(
                &config.output.replacement_characters,
                config.output.path_sanitization,
                config.output.is_s3(),
            );
            let path_limits = DownloadOptions::from(config).path_limits;
            let key = storage_key_with_contexts(
                &ctx,
                &ctx,
                Some(resolved.folder.as_str()),
                Some(resolved.file.as_str()),
                &ext,
                &rules,
                path_limits,
            );
            println!("asin\t{}", book.asin_or_isbn());
            println!("naming_profile\t{}", naming_profile.as_str());
            println!("folder_template\t{}", resolved.folder);
            println!("file_template\t{}", resolved.file);
            println!("path_sanitization\t{:?}", config.output.path_sanitization);
            println!(
                "max_filename_length\t{}",
                if path_limits.max_filename_length == 0 {
                    "disabled".to_string()
                } else {
                    path_limits.max_filename_length.to_string()
                }
            );
            if path_limits.max_storage_key_bytes > 0 {
                println!(
                    "max_storage_key_bytes\t{}",
                    path_limits.max_storage_key_bytes
                );
            }
            println!("storage_key\t{key}");
            Ok(())
        }
    }
}

fn resolve_book_for_preview(
    store: &LibraryStore,
    asin: &str,
    account: Option<&str>,
) -> anyhow::Result<bookclerk_library::BookRecord> {
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
        [] => anyhow::bail!("ASIN {asin} not in library — run `bookclerk library scan`"),
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
        "output.backend" | "output.backends" => config.output.enabled_backend_names().join(","),
        "output.local.enabled" => config.output.local.enabled.to_string(),
        "output.local.root" => config.output.local.root.display().to_string(),
        "output.local.prefix" => config.output.local.prefix.clone(),
        "output.effective_prefix" | "output.path_limit_prefix" => config.output.path_limit_prefix(),
        "output.s3.enabled" => config.output.s3.enabled.to_string(),
        "output.s3.bucket" => config.output.s3.bucket.clone(),
        "output.s3.prefix" => config.output.s3.prefix.clone(),
        "output.s3.region" => config.output.s3.region.clone(),
        "output.s3.endpoint" => config.output.s3.endpoint.clone().unwrap_or_default(),
        "output.s3.force_path_style" => config.output.s3.force_path_style.to_string(),
        "output.s3.credentials_file" => config
            .output
            .s3
            .credentials_file
            .as_ref()
            .map(|p| p.display().to_string())
            .or_else(|| {
                config
                    .resolved_s3_credentials_path()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_default(),
        "output.format" => format!("{:?}", config.output.format).to_ascii_lowercase(),
        "output.widevine" => config.output.widevine.to_string(),
        "output.xhe_aac" => config.output.xhe_aac.to_string(),
        "output.widevine_cdm" => config
            .output
            .widevine_cdm
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "output.naming_profile" => config.output.naming_profile.as_str().to_string(),
        "output.folder_template" => config.output.folder_template.clone().unwrap_or_default(),
        "output.file_template" => config.output.file_template.clone().unwrap_or_default(),
        "output.path_sanitization" => {
            format!("{:?}", config.output.path_sanitization).to_ascii_lowercase()
        }
        "output.max_filename_length" => config.output.max_filename_length.to_string(),
        "output.download_cover" => config.output.download_cover.to_string(),
        "output.download_pdf" => config.output.download_pdf.to_string(),
        "output.create_cue" => config.output.create_cue.to_string(),
        "output.fixup_metadata" => config.output.fixup_metadata.to_string(),
        "output.chapter_json" => {
            format!("{:?}", config.output.effective_chapter_json()).to_ascii_lowercase()
        }
        "output.save_chapter_json" => config
            .output
            .effective_chapter_json()
            .wants_any()
            .to_string(),
        "output.save_metadata_json" => config.output.save_metadata_json.to_string(),
        "output.cover_size" => config.output.cover_size.clone(),
        "output.chapter_layout" => config.output.chapter_layout.clone(),
        "output.overwrite_existing" => config.output.overwrite_existing.to_string(),
        "output.in_progress" => config
            .output
            .in_progress
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "output.bad_book_action" => format!("{:?}", config.output.bad_book_action),
        "output.split_mp3_max_mb" => config.output.split_mp3_max_mb.to_string(),
        "output.chapter_file_template" => config
            .output
            .chapter_file_template
            .clone()
            .unwrap_or_default(),
        "output.chapter_title_template" => config
            .output
            .chapter_title_template
            .clone()
            .unwrap_or_default(),
        "output.minimum_file_duration_minutes" => {
            config.output.minimum_file_duration_minutes.to_string()
        }
        "output.combine_nested_chapter_titles" => {
            config.output.combine_nested_chapter_titles.to_string()
        }
        "output.merge_opening_and_end_credits" => {
            config.output.merge_opening_and_end_credits.to_string()
        }
        "output.strip_unabridged" => config.output.strip_unabridged.to_string(),
        "output.strip_audible_brand_audio" => config.output.strip_audible_brand_audio.to_string(),
        "output.download_clips_bookmarks" => config.output.download_clips_bookmarks.to_string(),
        "output.retain_aax_file" => config.output.retain_aax_file.to_string(),
        "output.download_speed_limit_kbps" => config.output.download_speed_limit_kbps.to_string(),
        "output.lame.target" => config.output.lame.target.clone(),
        "output.lame.vbr_quality" => config.output.lame.vbr_quality.to_string(),
        "output.lame.bitrate_kbps" => config.output.lame.bitrate_kbps.to_string(),
        "output.lame.mode" => config.output.lame.mode.clone(),
        "output.lame.downsample_mono" => config.output.lame.downsample_mono.to_string(),
        "output.lame.constant_bitrate" => config.output.lame.constant_bitrate.to_string(),
        "output.max_sample_rate" => config
            .output
            .max_sample_rate
            .map(|n| n.to_string())
            .unwrap_or_default(),
        "output.creation_time" => format!("{:?}", config.output.creation_time).to_ascii_lowercase(),
        "output.last_write_time" => {
            format!("{:?}", config.output.last_write_time).to_ascii_lowercase()
        }
        "library.auto_acquire" => config.library.auto_acquire.to_string(),
        "library.scan_interval_minutes" => config.library.scan_interval_minutes.to_string(),
        "library.import_episodes" => config.library.import_episodes.to_string(),
        "library.import_plus_titles" => config.library.import_plus_titles.to_string(),
        "library.download_episodes" => config.library.download_episodes.to_string(),
        "library.save_podcasts_to_parent_folder" => {
            config.library.save_podcasts_to_parent_folder.to_string()
        }
        "library.enrich_from_audible" => config.library.enrich_from_audible.to_string(),
        "library.enrich_min_confidence" => config.library.enrich_min_confidence.to_string(),
        "library.fix_storage_layout" => config.library.fix_storage_layout.to_string(),
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
        other if let Some(rest) = other.strip_prefix("sources.") => {
            let (id, key) = rest.split_once('.')?;
            if key == "enabled" {
                return Some(config.sources.is_enabled(id).to_string());
            }
            if let Some(s) = config.sources.get_string(id, key) {
                return Some(s.to_string());
            }
            config
                .sources
                .table(id)?
                .get(key)
                .map(|v| v.to_string().trim_matches('"').to_string())?
        }
        _ => return None,
    })
}
