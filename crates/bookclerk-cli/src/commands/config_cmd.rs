//! `bookclerk config` — get/set settings and naming helpers.

use bookclerk_acquire::{storage_key_with_contexts, NamingContext};
use bookclerk_config::{
    apply_setting_overrides, classic_key_aliases, resolve_replacement_characters, Config,
    NamingProfile,
};
use bookclerk_library::control_plane::{
    bootstrap_control_plane, is_output_setting_key, load_events, load_host_enrollment, load_output,
    portable_output_seed, replace_events, replace_output, set_host_enrollment_enabled, ConfigActor,
    EventsReplace, EventsSettingsV1, OutputReplace, OutputSettingsV1, OUTPUT_SETTING_KEYS,
};
use bookclerk_library::{
    inspect_master_key, wrap_master_key, LibraryStore, MasterKeyFormat,
    MASTER_KEY_AUTH_PASSWORD_ENV,
};
use bookclerk_source::DownloadOptions;
use chrono::Datelike;
use clap::Subcommand;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk config` subcommands for settings, secrets, templates, and database migrate.
pub enum ConfigCommand {
    /// Print a configuration value by dotted key or classic Settings.json name.
    Get {
        /// Dotted key (`output.widevine`) or classic name (`UseWidevine`).
        /// Plugin settings are read with `bookclerk plugins config get`.
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
    /// Inspect or wrap `{files_dir}/master.key` (BCK1 ↔ BCK2).
    MasterKey {
        #[command(subcommand)]
        /// Nested master-key status/wrap action (BCK1 raw vs BCK2 sealed).
        command: MasterKeyCommand,
    },
    /// Naming template helpers.
    Template {
        #[command(subcommand)]
        /// Nested naming-template tags/profiles/preview action.
        command: TemplateCommand,
    },
    /// Database backend helpers (plugin switch / migration).
    Database {
        #[command(subcommand)]
        /// Nested database-plugin migrate action.
        command: DatabaseCommand,
    },
}

#[derive(Debug, Subcommand)]
/// Nested `config database` actions for copying rows between backend plugins.
pub enum DatabaseCommand {
    /// Copy library data from one `[database].plugin` backend to another.
    ///
    /// Does not change `config.toml` unless `--apply` is passed after a successful copy.
    Migrate {
        /// Source plugin id (`sqlite`, `d1`, `postgres`). Default: current `[database].plugin`.
        #[arg(long)]
        from: Option<String>,
        /// Destination plugin id.
        #[arg(long)]
        to: String,
        /// Report row counts only; do not write to the destination.
        #[arg(long)]
        dry_run: bool,
        /// Allow copying into a destination that already has library rows.
        #[arg(long)]
        force: bool,
        /// After a successful migrate, set `[database].plugin` to `--to` and write config.toml.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
/// Nested `config master-key` actions for inspecting or wrapping `master.key`.
pub enum MasterKeyCommand {
    /// Show whether `master.key` exists and if it is raw (BCK1) or wrapped (BCK2).
    Status,
    /// Wrap a BCK1 `master.key` with a passphrase (no-op unlock if already BCK2).
    ///
    /// Password from `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` — never argv.
    Wrap,
}

#[derive(Debug, Subcommand)]
/// Nested `config template` actions for naming tags, profiles, and path preview.
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

/// Dispatches a `config` subcommand against the loaded configuration.
pub async fn run(
    command: ConfigCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Get { key, bare } => {
            if bare {
                // `output.*` reads the database document, like `config get`.
                let output = match bare_output_body(
                    load_output_for_show(config).await,
                    &portable_output_seed(&config.output, &config.paths().files_dir),
                ) {
                    Ok(body) => Some(body),
                    Err(err) => {
                        eprintln!("output.* omitted: {}", safe_events_load_error(&err));
                        None
                    }
                };
                for (classic, dotted) in classic_key_aliases() {
                    let value = if is_output_setting_key(dotted) {
                        output.as_ref().and_then(|body| body.get(dotted))
                    } else {
                        lookup(config, dotted)
                    };
                    if let Some(value) = value {
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
            if dotted.starts_with("events.") {
                return get_events_setting(config, dotted, format).await;
            }
            if is_output_setting_key(dotted) {
                return get_output_setting(config, dotted, format).await;
            }
            if dotted == HOST_ENROLLMENT_ENABLED_KEY {
                return get_host_enrollment(config, format).await;
            }
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
            if dotted.starts_with("events.") {
                return set_events_setting(config, &dotted, &value, format).await;
            }
            if is_output_setting_key(&dotted) {
                return set_output_setting(config, &dotted, &value, format).await;
            }
            if dotted == HOST_ENROLLMENT_ENABLED_KEY {
                return set_host_enrollment(config, &value, format).await;
            }
            apply_setting_overrides(&mut cfg, &[(&dotted, value.as_str())]);
            let path = cfg.paths().config_file.clone();
            cfg.write_toml_file(&path)?;
            // Setting auth.password should wrap BCK1 immediately (same as wrap CLI).
            if dotted == "auth.password" {
                if let Some(pw) = cfg.auth_password() {
                    wrap_master_key(&cfg.paths().files_dir, &pw)?;
                }
            }
            let new_value = if dotted == "auth.password" {
                String::from("***")
            } else {
                lookup(&cfg, &dotted).unwrap_or_else(|| value.clone())
            };
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
            print_output_authority(config).await;
            println!(
                "output.in_progress = {} (host-local, config.toml)",
                config.download_cache_dir().display()
            );
            println!("library.auto_acquire = {}", config.library.auto_acquire);
            println!(
                "library.fix_storage_layout = {}",
                config.library.fix_storage_layout
            );
            println!(
                "library.scan_interval_minutes = {}",
                config.library.scan_interval_minutes
            );
            // Both confinement tiers, so an operator can confirm what is jailed
            // without reading the startup log.
            println!("media.workers = {}", config.media.workers);
            println!("media.isolation = {}", config.media.isolation.as_str());
            println!(
                "media.worker_bin = {}",
                helper_bin(&config.media.worker_bin)
            );
            println!("plugins.isolation = {}", config.plugins.isolation.as_str());
            println!(
                "plugins.jail_bin = {}",
                helper_bin(&config.plugins.jail_bin)
            );
            println!("daemon.listen = {}", config.daemon.listen.join_comma());
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
            print_events_authority(config).await;
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
        ConfigCommand::MasterKey { command } => run_master_key(command, config, format),
        ConfigCommand::Template { command } => run_template(command, config).await,
        ConfigCommand::Database { command } => run_database(command, config, format).await,
    }
}

/// Reports `master.key` format or wraps BCK1 with the configured passphrase (never argv).
fn run_master_key(
    command: MasterKeyCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let files_dir = &config.paths().files_dir;
    match command {
        MasterKeyCommand::Status => {
            let format_kind = inspect_master_key(files_dir)?;
            let (exists, kind) = match format_kind {
                None => (false, "missing"),
                Some(MasterKeyFormat::Raw) => (true, "BCK1"),
                Some(MasterKeyFormat::Wrapped) => (true, "BCK2"),
            };
            let payload = serde_json::json!({
                "path": bookclerk_library::master_key_path(files_dir).display().to_string(),
                "exists": exists,
                "format": kind,
                "password_configured": config.auth_password().is_some(),
            });
            emit(format, &payload, || {
                println!(
                    "master.key\t{}\tformat={kind}\tpassword={}",
                    if exists { "present" } else { "missing" },
                    if config.auth_password().is_some() {
                        "set"
                    } else {
                        "unset"
                    }
                );
            })
        }
        MasterKeyCommand::Wrap => {
            let password = config.auth_password().ok_or_else(|| {
                anyhow::anyhow!(
                    "set {MASTER_KEY_AUTH_PASSWORD_ENV} or [auth].password before wrapping \
                     (secrets are not accepted on argv)"
                )
            })?;
            wrap_master_key(files_dir, &password)?;
            let kind = inspect_master_key(files_dir)?;
            let payload = serde_json::json!({
                "path": bookclerk_library::master_key_path(files_dir).display().to_string(),
                "format": match kind {
                    Some(MasterKeyFormat::Wrapped) => "BCK2",
                    Some(MasterKeyFormat::Raw) => "BCK1",
                    None => "missing",
                },
                "wrapped": matches!(kind, Some(MasterKeyFormat::Wrapped)),
            });
            emit(format, &payload, || {
                println!(
                    "wrapped {}",
                    bookclerk_library::master_key_path(files_dir).display()
                );
            })
        }
    }
}

/// Lists naming tags/profiles or previews a storage key for a library title.
async fn run_template(command: TemplateCommand, config: &Config) -> anyhow::Result<()> {
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
            // Naming comes from `core.output` when a host has enrolled it.
            // Preview is read-only: it never imports the TOML seed, and a
            // missing document falls back to the local config.
            let store = crate::registry::open_library(config).await?;
            let mut overlaid = config.clone();
            match load_output(&store).await {
                Ok(doc) => {
                    let files_dir = overlaid.paths.as_ref().map(|p| p.files_dir.clone());
                    doc.body
                        .apply_to(&mut overlaid.output, files_dir.as_deref());
                }
                Err(bookclerk_library::LibraryError::NotFound(_)) => {}
                Err(err) => return Err(err.into()),
            }
            let config = &overlaid;
            let book = resolve_book_for_preview(&store, &asin, account.as_deref()).await?;
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
                false,
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

/// Copies library rows between database plugins and optionally writes `[database].plugin`.
async fn run_database(
    command: DatabaseCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        DatabaseCommand::Migrate {
            from,
            to,
            dry_run,
            force,
            apply,
        } => {
            let from_plugin = from
                .unwrap_or_else(|| config.database.plugin.clone())
                .trim()
                .to_string();
            let to_plugin = to.trim().to_string();

            let summary = bookclerk_plugin_host::migrate_database_plugin(
                config,
                &from_plugin,
                &to_plugin,
                &bookclerk_library::BackendMigrateOptions { dry_run, force },
            )
            .await?;

            let mut message = if dry_run {
                format!(
                    "dry-run: would copy {} row(s) from `{from_plugin}` to `{to_plugin}`",
                    summary.total_rows()
                )
            } else {
                format!(
                    "copied {} row(s) from `{from_plugin}` to `{to_plugin}`",
                    summary.total_rows()
                )
            };

            if apply && !dry_run {
                let plugins = bookclerk_plugin_host::discover_plugins(config)?;
                let plugin = bookclerk_plugin_host::resolve_plugin_ref(&plugins, &to_plugin)
                    .map_err(|err| anyhow::anyhow!("{err}"))?;
                let mut cfg = config.clone();
                bookclerk_plugin_host::stamp_occupancy_plugin_key(&mut cfg, plugin)
                    .map_err(|err| anyhow::anyhow!("{err}"))?;
                let path = cfg.paths().config_file.clone();
                cfg.write_toml_file(&path)?;
                message.push_str(&format!(
                    "; updated [database].plugin and wrote {}",
                    path.display()
                ));
                message.push_str(
                    " — reload bookclerkd config (SIGHUP or POST /api/config/reload) to switch backends live",
                );
            } else if !dry_run {
                message.push_str(
                    "; [database].plugin unchanged — pass --apply to update config.toml, \
                     then reload bookclerkd config or POST /api/database/migrate with apply",
                );
            }

            let payload = serde_json::json!({
                "from": from_plugin,
                "to": to_plugin,
                "dry_run": dry_run,
                "apply": apply,
                "tables": summary.tables,
                "total_rows": summary.total_rows(),
                "message": message,
            });
            emit(format, &payload, || println!("{message}"))
        }
    }
}

/// Resolves a library row by ASIN/ISBN/UUID, requiring `--account` when several match.
async fn resolve_book_for_preview(
    store: &LibraryStore,
    asin: &str,
    account: Option<&str>,
) -> anyhow::Result<bookclerk_library::BookRecord> {
    if let Some(account) = account {
        return store
            .get_book(asin, account)
            .await
            .map_err(|e| e.into())
            .and_then(|opt| {
                opt.ok_or_else(|| anyhow::anyhow!("ASIN {asin} not found for account {account}"))
            });
    }
    let matches: Vec<_> = store
        .list_books(None)
        .await?
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

/// Formats one dotted config key as a string for `config get`.
/// Unique id for one CLI configuration write.
fn cli_operation_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("cli-{}-{nanos}", std::process::id())
}

/// Opens the library and enrolls this files directory.
async fn open_control_plane(
    config: &Config,
) -> anyhow::Result<(
    LibraryStore,
    bookclerk_library::control_plane::ControlPlaneSession,
)> {
    let store = crate::registry::open_library(config).await?;
    let scratch_dir = config.download_cache_dir();
    let session = bootstrap_control_plane(
        &store,
        &config.paths().files_dir,
        &scratch_dir,
        config.auth_password().as_deref(),
        config,
    )
    .await?;
    Ok((store, session))
}

/// Prints one `events.*` value from the database document.
async fn get_events_setting(
    config: &Config,
    dotted: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let (_store, session) = open_control_plane(config).await?;
    let value = events_field(&session.events.body, dotted)?;
    let payload = serde_json::json!({
        "key": dotted,
        "value": value,
        "authority": "database",
        "revision": session.events.revision,
        "namespace": session.events.namespace,
    });
    emit(format, &payload, || {
        println!("{value}");
        println!("authority=database revision={}", session.events.revision);
    })
}

/// Compare-and-swaps one `events.*` field in the database.
async fn set_events_setting(
    config: &Config,
    dotted: &str,
    value: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let (store, session) = open_control_plane(config).await?;
    let mut body = session.events.body.clone();
    match dotted {
        "events.retention_days" => {
            body.retention_days = value
                .parse()
                .map_err(|_| anyhow::anyhow!("events.retention_days must be an integer"))?;
        }
        "events.dead_letter_retention_days" => {
            body.dead_letter_retention_days = value.parse().map_err(|_| {
                anyhow::anyhow!("events.dead_letter_retention_days must be an integer")
            })?;
        }
        "events.concurrency" => {
            body.concurrency = value
                .parse()
                .map_err(|_| anyhow::anyhow!("events.concurrency must be an integer"))?;
        }
        other => anyhow::bail!("unknown config key: {other}"),
    }
    body.validate()?;
    if body == session.events.body {
        let payload = serde_json::json!({
            "key": dotted,
            "value": value,
            "authority": "database",
            "revision": session.events.revision,
        });
        return emit(format, &payload, || {
            println!("events unchanged");
            println!("authority=database revision={}", session.events.revision);
        });
    }
    let outcome = replace_events(
        &store,
        &ConfigActor::Operator { id: "cli".into() },
        session.events.revision,
        &body,
        &cli_operation_id(),
    )
    .await?;
    let revision = match outcome {
        EventsReplace::Applied(doc) => doc.revision,
        EventsReplace::Replayed { revision } => revision,
        EventsReplace::Conflict { current_revision } => anyhow::bail!(
            "revision conflict: events were not changed; current revision is {current_revision}"
        ),
    };
    let shown = events_field(&body, dotted)?;
    let payload = serde_json::json!({
        "key": dotted,
        "value": shown,
        "authority": "database",
        "revision": revision,
    });
    emit(format, &payload, || {
        println!("set {dotted}={shown}");
        println!("authority=database revision={revision}");
    })
}

/// Config key of the cluster's host-enrollment switch (`core.host_enrollment`).
const HOST_ENROLLMENT_ENABLED_KEY: &str = "host_enrollment.enabled";

/// Prints whether host nodes may enroll into this cluster.
async fn get_host_enrollment(config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    let (store, _session) = open_control_plane(config).await?;
    let settings = load_host_enrollment(&store).await?;
    let payload = serde_json::json!({
        "key": HOST_ENROLLMENT_ENABLED_KEY,
        "value": settings.body.enabled,
        "authority": "database",
        "revision": settings.revision,
    });
    emit(format, &payload, || {
        println!("{}", settings.body.enabled);
        match settings.revision {
            Some(revision) => println!("authority=database revision={revision}"),
            None => println!("authority=database (default)"),
        }
    })
}

/// Turns host enrollment on or off for the cluster.
async fn set_host_enrollment(
    config: &Config,
    value: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let enabled = match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        _ => anyhow::bail!("{HOST_ENROLLMENT_ENABLED_KEY} must be true or false"),
    };
    let (store, _session) = open_control_plane(config).await?;
    let doc =
        set_host_enrollment_enabled(&store, &ConfigActor::Operator { id: "cli".into() }, enabled)
            .await?;
    let payload = serde_json::json!({
        "key": HOST_ENROLLMENT_ENABLED_KEY,
        "value": doc.body.enabled,
        "authority": "database",
        "revision": doc.revision,
    });
    emit(format, &payload, || {
        println!("set {HOST_ENROLLMENT_ENABLED_KEY}={}", doc.body.enabled);
        println!("authority=database revision={}", doc.revision);
    })
}

/// Formats one events field for display.
fn events_field(body: &EventsSettingsV1, dotted: &str) -> anyhow::Result<String> {
    match dotted {
        "events.retention_days" => Ok(body.retention_days.to_string()),
        "events.dead_letter_retention_days" => Ok(body.dead_letter_retention_days.to_string()),
        "events.concurrency" => Ok(body.concurrency.to_string()),
        other => anyhow::bail!("unknown config key: {other}"),
    }
}

/// Prints whether `[events]` is database-authoritative.
async fn print_events_authority(config: &Config) {
    match load_events_for_show(config).await {
        Ok(doc) => {
            println!("events.authority = database");
            println!("events.revision = {}", doc.revision);
            println!("events.retention_days = {}", doc.body.retention_days);
            println!(
                "events.dead_letter_retention_days = {}",
                doc.body.dead_letter_retention_days
            );
            println!("events.concurrency = {}", doc.body.concurrency);
        }
        Err(err) => {
            println!("events.authority = transitional");
            let detail = safe_events_load_error(&err);
            println!(
                "events.retention_days = {} (config.toml / environment; {detail})",
                config.events.retention_days
            );
        }
    }
}

/// Stable events-load failure for stdout.
///
/// The raw anyhow chain can carry parser text (serde echoes a bad stored
/// value) and backend text (paths and connection URLs). Registered secrets
/// and `user:password@` URL userinfo are removed. The `database unavailable`
/// prefix stays so the failure is still recognizable. A document the
/// database does not hold yet (`NotFound`) is not a failure, and says so.
fn safe_events_load_error(err: &anyhow::Error) -> String {
    if document_not_imported(err) {
        // The database answered; the document just has not been imported.
        return "not in the database yet; the first enrollment imports it".into();
    }
    let mut chain = err.to_string();
    for cause in err.chain().skip(1) {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
    }
    let scrubbed = redact_credential_urls(&bookclerk_config::redact_str(&chain));
    format!("database unavailable: {scrubbed}")
}

/// Whether a core-document load failed only because the row does not exist yet.
///
/// Only a missing document is imported from the TOML/environment seed on the
/// next enrollment; any other load error leaves the stored row in place.
///
/// # Arguments
///
/// * `err` - The error returned by a core-document load.
fn document_not_imported(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<bookclerk_library::LibraryError>(),
        Some(bookclerk_library::LibraryError::NotFound(_))
    )
}

/// Replaces `scheme://user:password@host` userinfo.
///
/// Runs after [`bookclerk_config::redact_str`], so a URL that was not registered
/// as an exact secret still cannot print its password.
fn redact_credential_urls(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(marker) = rest.find("://") {
        let after = marker + 3;
        out.push_str(&rest[..after]);
        let tail = &rest[after..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ','))
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        if let Some(at) = authority.find('@') {
            let userinfo = &authority[..at];
            if userinfo.contains(':') {
                out.push_str(bookclerk_config::REDACTED);
                out.push('@');
                rest = &tail[at + 1..];
                continue;
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Loads `core.events` without importing a missing document.
async fn load_events_for_show(
    config: &Config,
) -> anyhow::Result<bookclerk_library::control_plane::ConfigurationDocument<EventsSettingsV1>> {
    let store = crate::registry::open_library(config).await?;
    Ok(load_events(&store).await?)
}

/// Loads `core.output` without importing a missing document.
async fn load_output_for_show(
    config: &Config,
) -> anyhow::Result<bookclerk_library::control_plane::ConfigurationDocument<OutputSettingsV1>> {
    let store = crate::registry::open_library(config).await?;
    Ok(load_output(&store).await?)
}

/// Resolves the `core.output` body that `config get --bare` lists.
///
/// A missing document falls back to the seed the first bootstrap would import,
/// which is what `config get <output key>` reports on the same install.
///
/// # Errors
///
/// Returns an error when the document could not be loaded for any other
/// reason, or when it is missing and the seed is invalid.
fn bare_output_body(
    loaded: anyhow::Result<
        bookclerk_library::control_plane::ConfigurationDocument<OutputSettingsV1>,
    >,
    seed: &bookclerk_config::OutputConfig,
) -> anyhow::Result<OutputSettingsV1> {
    match loaded {
        Ok(doc) => Ok(doc.body),
        Err(err) if document_not_imported(&err) => {
            // What the first enrollment will store.
            Ok(OutputSettingsV1::from_seed_lenient(seed).0)
        }
        Err(err) => Err(err),
    }
}

/// Prints the shared `[output]` settings from `core.output`.
async fn print_output_authority(config: &Config) {
    match load_output_for_show(config).await {
        Ok(doc) => {
            println!("output.authority = database");
            println!("output.revision = {}", doc.revision);
            let resolved = bookclerk_config::ResolvedNamingTemplates::resolve(
                doc.body.naming_profile,
                doc.body.folder_template.as_deref(),
                doc.body.file_template.as_deref(),
                doc.body.chapter_file_template.as_deref(),
            );
            for key in OUTPUT_SETTING_KEYS {
                let value = doc.body.get(key).unwrap_or_default();
                let profile_default = match *key {
                    "output.folder_template" => Some(&resolved.folder),
                    "output.file_template" => Some(&resolved.file),
                    "output.chapter_file_template" => Some(&resolved.chapter_file),
                    _ => None,
                };
                match profile_default {
                    Some(default) if value.is_empty() => {
                        println!("{key} = - (profile: {default})");
                    }
                    _ => println!("{key} = {value}"),
                }
            }
        }
        Err(err) if !document_not_imported(&err) => {
            // A stored document that cannot be read (unsupported version,
            // corrupt body, unreachable database) is never re-imported, so
            // the TOML/environment seed would not apply and is not shown.
            println!("output.authority = unavailable");
            println!("output.error = {}", safe_events_load_error(&err));
        }
        Err(err) => {
            println!("output.authority = transitional");
            let detail = safe_events_load_error(&err);
            println!("output.source = config.toml / environment ({detail})");
            let seed = portable_output_seed(&config.output, &config.paths().files_dir);
            // What the first enrollment will store: an out-of-range value
            // shows its default, marked.
            let (seed, replaced) = OutputSettingsV1::from_seed_lenient(&seed);
            for key in OUTPUT_SETTING_KEYS {
                let value = seed.get(key).unwrap_or_default();
                if replaced
                    .iter()
                    .any(|field| key.strip_prefix("output.") == Some(*field))
                {
                    println!("{key} = {value} (default; the configured value is out of range)");
                } else {
                    println!("{key} = {value}");
                }
            }
        }
    }
}

/// Prints one `output.*` value from the database document.
async fn get_output_setting(
    config: &Config,
    dotted: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let (_store, session) = open_control_plane(config).await?;
    let value = session
        .output
        .body
        .get(dotted)
        .ok_or_else(|| anyhow::anyhow!("{dotted} is write-only; read its canonical key"))?;
    let payload = serde_json::json!({
        "key": dotted,
        "value": value,
        "authority": "database",
        "revision": session.output.revision,
        "namespace": session.output.namespace,
    });
    emit(format, &payload, || {
        println!("{value}");
        println!("authority=database revision={}", session.output.revision);
    })
}

/// Compare-and-swaps one `output.*` field in the database.
async fn set_output_setting(
    config: &Config,
    dotted: &str,
    value: &str,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let (store, session) = open_control_plane(config).await?;
    let mut body = session.output.body.clone();
    body.set(dotted, value)?;
    let shown = body.get(dotted).unwrap_or_else(|| value.trim().to_string());
    if body == session.output.body {
        let payload = serde_json::json!({
            "key": dotted,
            "value": shown,
            "authority": "database",
            "revision": session.output.revision,
        });
        return emit(format, &payload, || {
            println!("output unchanged");
            println!("authority=database revision={}", session.output.revision);
        });
    }
    let outcome = replace_output(
        &store,
        &ConfigActor::Operator { id: "cli".into() },
        session.output.revision,
        &body,
        &cli_operation_id(),
    )
    .await?;
    let revision = match outcome {
        OutputReplace::Applied(doc) => doc.revision,
        OutputReplace::Replayed { revision } => revision,
        OutputReplace::Conflict { current_revision } => anyhow::bail!(
            "revision conflict: output was not changed; current revision is {current_revision}"
        ),
    };
    let payload = serde_json::json!({
        "key": dotted,
        "value": shown,
        "authority": "database",
        "revision": revision,
    });
    emit(format, &payload, || {
        println!("set {dotted}={shown}");
        println!("authority=database revision={revision}");
    })
}

/// Formats one dotted config key as a string for `config get`.
fn lookup(config: &Config, key: &str) -> Option<String> {
    let paths = config.paths.as_ref();
    Some(match key {
        "output.in_progress" => config
            .output
            .in_progress
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
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
        "auth.password" => {
            if config.auth_password().is_some() {
                "***".into()
            } else {
                String::new()
            }
        }
        // A confinement helper is normally unset, in which case it is found
        // beside the running executable — a path only the host resolving it can
        // report, so this shows what was configured rather than guessing.
        "media.workers" => config.media.workers.to_string(),
        "media.isolation" => config.media.isolation.as_str().to_string(),
        "media.worker_bin" => helper_bin(&config.media.worker_bin),
        "plugins.isolation" => config.plugins.isolation.as_str().to_string(),
        "plugins.jail_bin" => helper_bin(&config.plugins.jail_bin),
        "daemon.listen" => config.daemon.listen.join_comma(),
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

/// Render a configured helper-binary path, or `-` when it is found by search.
fn helper_bin(path: &Option<std::path::PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_output_body_falls_back_to_seed_when_document_missing() {
        let seed = bookclerk_config::OutputConfig {
            widevine: true,
            ..bookclerk_config::OutputConfig::default()
        };
        let missing = Err(anyhow::Error::new(
            bookclerk_library::LibraryError::NotFound(
                "configuration core.output is not initialized".into(),
            ),
        ));
        let body = bare_output_body(missing, &seed).expect("seed fallback");
        assert_eq!(body.get("output.widevine").as_deref(), Some("true"));
        assert_eq!(body, OutputSettingsV1::from_config(&seed).unwrap());
    }

    #[test]
    fn a_document_not_imported_yet_is_not_reported_as_unavailable() {
        let missing = anyhow::Error::new(bookclerk_library::LibraryError::NotFound(
            "configuration core.output is not initialized".into(),
        ));
        let detail = safe_events_load_error(&missing);
        assert!(!detail.contains("unavailable"), "{detail}");
        assert!(detail.contains("first enrollment"), "{detail}");
        let failed = anyhow::anyhow!("database unreachable");
        assert!(safe_events_load_error(&failed).starts_with("database unavailable:"));
    }

    #[test]
    fn only_a_missing_document_is_treated_as_not_imported() {
        let missing = anyhow::Error::new(bookclerk_library::LibraryError::NotFound(
            "configuration core.output is not initialized".into(),
        ));
        assert!(document_not_imported(&missing));
        // An unsupported or corrupt stored row is kept by bootstrap, so the
        // seed must not be presented as what applies.
        let corrupt = anyhow::Error::new(bookclerk_library::LibraryError::Invalid(
            "invalid configuration: unsupported schema".into(),
        ));
        assert!(!document_not_imported(&corrupt));
        assert!(!document_not_imported(&anyhow::anyhow!(
            "database unreachable"
        )));
    }

    #[test]
    fn bare_output_body_propagates_other_load_errors() {
        let failed = Err(anyhow::anyhow!("database unreachable"));
        assert!(bare_output_body(failed, &bookclerk_config::OutputConfig::default()).is_err());
    }
}
