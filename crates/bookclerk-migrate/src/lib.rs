//! Import/export classic Libation Files and native Bookclerk backups.
//!
//! # Audience
//!
//! CLI migration commands and host tooling. Converts classic Libation directories
//! into Bookclerk `files` layouts, and round-trips native `.tar.zst` backups.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

mod accounts;
mod classic_storefront;
mod discover;
/// Migration error types (`MigrateError`) returned to CLI import/export commands.
mod error;
mod export_libation;
mod files;
mod library;
mod native;
mod settings;
mod store;

pub use accounts::import_accounts;
pub use classic_storefront::write_classic_storefront_file;
pub use discover::{discover_source, ClassicSource};
pub use error::{MigrateError, Result};
pub use export_libation::{export_libation, LibationExportOptions, LibationExportSummary};
pub use files::load_audio_paths;
pub use library::import_library_db;
pub use native::{
    export_native, import_native, NativeExportOptions, NativeExportSummary, NativeImportOptions,
    NativeImportSummary, NATIVE_BACKUP_FORMAT_VERSION,
};
pub use settings::{
    apply_settings_json, config_to_settings_json, load_settings_json, ClassicPluginSettings,
};

use std::collections::HashMap;
use std::path::Path;

use bookclerk_config::Config;
use serde::{Deserialize, Serialize};

/// Options for a full classic Libation Files directory import.
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// Absolute path to the classic Libation Files directory to import.
    pub source: std::path::PathBuf,
    /// Destination files dir (auth, library.db, config.toml).
    pub dest_files_dir: std::path::PathBuf,
    /// When true, overwrite an existing destination `config.toml`.
    pub force: bool,
    /// Retained for CLI compatibility; credentials are never imported by migrate
    /// (use the Audible plugin: `auth login` / `auth import`).
    pub skip_auth: bool,
    /// Report only; do not write.
    pub dry_run: bool,
    /// Canonical PluginKey of the Audible storefront plugin. Imported
    /// accounts and books are owned by that plugin; the caller resolves it
    /// from the installed plugin (`bookclerk plugins list`).
    pub audible_plugin_key: String,
    /// Canonical PluginKey of the `local` destination plugin, when installed.
    /// The classic `Books` directory is written to its default instance as
    /// `root`, so acquired files keep landing where Libation put them.
    pub local_plugin_key: Option<String>,
    /// Host configuration used to enrol the installed plugins into the
    /// destination database before instance settings are seeded. A fresh
    /// files directory has no pinned schema and no default instance until a
    /// host enrols, so without this the `root` and `bitrate` seeds have
    /// nowhere to go. `None` skips enrollment (tests, dry runs).
    pub host_config: Option<bookclerk_config::Config>,
}

/// Summary of a migration run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MigrateSummary {
    /// When true, `Settings.json` was found and applied (or would be, in dry-run).
    pub settings_imported: bool,
    /// Count of accounts imported or exported.
    pub accounts: usize,
    /// Count of credential blobs imported.
    pub credentials: usize,
    /// Count of book rows imported or exported.
    pub books: usize,
    /// Count of books marked acquired after import.
    pub acquired: usize,
    /// Count of storage-key mappings written.
    pub storage_keys: usize,
    /// Count of plugin instance settings written (`local` root, Audible bitrate).
    #[serde(default)]
    pub plugin_settings: usize,
    /// Non-fatal warnings collected during the run (operator-facing).
    pub warnings: Vec<String>,
}

/// Import classic Libation Files into a bookclerk data directory.
///
/// # Arguments
///
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `MigrateSummary` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn migrate(opts: MigrateOptions) -> Result<MigrateSummary> {
    let source = discover_source(&opts.source)?;
    let mut summary = MigrateSummary::default();

    if !opts.dry_run {
        std::fs::create_dir_all(&opts.dest_files_dir)?;
    }

    // --- Settings.json → config.toml ---
    let mut config = Config::default();
    let mut plugin_settings = ClassicPluginSettings::default();
    // True once the imported settings replace this destination's config;
    // a refused overwrite (no `--force`) must not seed `core.output` either.
    let mut config_accepted = false;
    if let Some(settings_path) = &source.settings_json {
        let patch = load_settings_json(settings_path)?;
        plugin_settings = apply_settings_json(&mut config, &patch);
        summary.settings_imported = true;
        if !opts.dry_run {
            let dest = opts.dest_files_dir.join("config.toml");
            if dest.exists() && !opts.force {
                summary.warnings.push(format!(
                    "config.toml already exists at {} (pass --force to overwrite)",
                    dest.display()
                ));
            } else {
                config.write_toml_file(&dest)?;
                config_accepted = true;
            }
        }
    } else {
        summary
            .warnings
            .push("Settings.json not found — using defaults".into());
    }

    let books_root = plugin_settings
        .books_root
        .clone()
        .unwrap_or_else(|| opts.dest_files_dir.join("Audiobooks"));

    // A settings-only import still opens the destination so the imported
    // packaging settings can seed `core.output` and the plugin instances.
    let dest = if source.accounts_settings.is_some()
        || source.library_db.is_some()
        || (summary.settings_imported && !opts.dry_run)
    {
        Some(store::DestStore::open(&opts.dest_files_dir, opts.dry_run).await?)
    } else {
        None
    };

    // --- plugin instance settings (Books → local root, quality → bitrate) ---
    if let Some(dest_store) = dest.as_ref() {
        if !opts.dry_run {
            // Enrol first: pins the `local` / Audible schemas and creates
            // their default instances in this database, the way the first
            // host start would, so the seeds below validate and land.
            if let Some(host) = opts.host_config.as_ref() {
                let mut cfg = host.clone();
                cfg.paths = Some(bookclerk_config::Paths::from_files_dir(
                    opts.dest_files_dir.clone(),
                ));
                // The imported Settings.json packaging seeds `core.output`
                // when this database has no output document yet, unless the
                // config overwrite was refused. The scratch directory is
                // host-local and stays this host's.
                if config_accepted {
                    let in_progress = cfg.output.in_progress.clone();
                    cfg.output = config.output.clone();
                    cfg.output.in_progress = in_progress;
                }
                match bookclerk_plugin_host::enroll_node(&cfg, &dest_store.store).await {
                    Ok(enrolled) => {
                        // Bootstrap seeds the portable form (CDM path relative
                        // to the files directory); compare against the same.
                        // Bootstrap imports a seed leniently (an out-of-range
                        // field becomes its default), so compare the same form.
                        let (seeded, _) =
                            bookclerk_library::control_plane::OutputSettingsV1::from_seed_lenient(
                                &bookclerk_library::control_plane::portable_output_seed(
                                    &cfg.output,
                                    &opts.dest_files_dir,
                                ),
                            );
                        if config_accepted && seeded != enrolled.session.output.body {
                            summary.warnings.push(format!(
                                "core.output already exists (revision {}); imported packaging \
                                 settings were not applied — use `bookclerk config set output.*`",
                                enrolled.session.output.revision
                            ));
                        }
                    }
                    Err(err) => summary.warnings.push(format!(
                        "plugin enrollment failed; instance settings may not be written: {err}"
                    )),
                }
            }
            if let Some(local_key) = opts.local_plugin_key.as_deref() {
                match seed_instance_setting(
                    &dest_store.store,
                    local_key,
                    "root",
                    &books_root.display().to_string(),
                )
                .await
                {
                    Ok(()) => summary.plugin_settings += 1,
                    Err(err) => summary
                        .warnings
                        .push(format!("local destination root not written: {err}")),
                }
            }
            if let Some(bitrate) = plugin_settings.audible_bitrate.as_deref() {
                match seed_instance_setting(
                    &dest_store.store,
                    &opts.audible_plugin_key,
                    "bitrate",
                    bitrate,
                )
                .await
                {
                    Ok(()) => summary.plugin_settings += 1,
                    Err(err) => summary
                        .warnings
                        .push(format!("audible bitrate not written: {err}")),
                }
            }
        }
    }

    // --- AccountsSettings.json ---
    let mut account_id_map = HashMap::new();
    if let Some(accounts_path) = &source.accounts_settings {
        let dest_store = dest.as_ref().ok_or_else(|| {
            MigrateError::Accounts("destination store required to import accounts".into())
        })?;
        let acct = import_accounts(
            accounts_path,
            &dest_store.store,
            &opts.audible_plugin_key,
            opts.force,
            opts.skip_auth,
            opts.dry_run,
        )
        .await?;
        summary.accounts = acct.accounts;
        summary.credentials = acct.credentials;
        summary.warnings.extend(acct.warnings);
        account_id_map = acct.account_id_map;
    } else {
        summary
            .warnings
            .push("AccountsSettings.json not found — accounts not imported".into());
    }

    // --- FileLocationsV2.json (optional) ---
    let audio_paths = if let Some(path) = &source.file_locations {
        load_audio_paths(path)?
    } else {
        Default::default()
    };

    // --- LibationContext.db ---
    if let Some(db_path) = &source.library_db {
        let dest_store = dest.as_ref().ok_or_else(|| {
            MigrateError::Library("destination store required to import library".into())
        })?;
        let lib = import_library_db(
            db_path,
            &dest_store.store,
            &opts.audible_plugin_key,
            &audio_paths,
            books_root.as_path(),
            &account_id_map,
            opts.dry_run,
        )
        .await?;
        summary.books = lib.books;
        summary.acquired = lib.acquired;
        summary.storage_keys = lib.storage_keys;
        summary.warnings.extend(lib.warnings);
    } else {
        summary
            .warnings
            .push("LibationContext.db not found — library not imported".into());
    }

    Ok(summary)
}

/// Resolve a classic Libation path from CLI / env.
///
/// # Arguments
///
/// * `explicit` - Filesystem path (`explicit`).
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
pub fn resolve_classic_files_dir(explicit: Option<&Path>) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("BOOKCLERK_CLASSIC_FILES") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(std::path::PathBuf::from(trimmed));
        }
    }
    None
}

/// Writes one setting on the plugin's default instance document.
///
/// The plugin must be enrolled (its schema pinned and default instance
/// created by a host that has it installed); the value is validated against
/// that schema.
async fn seed_instance_setting(
    store: &bookclerk_library::LibraryStore,
    plugin_key: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    use bookclerk_library::control_plane::{
        default_instance_for_key, load_instance_config, parse_setting_text,
        replace_instance_config, require_plugin_schema, set_setting, ConfigActor,
        InstanceConfigReplace,
    };
    let failed = |detail: String| MigrateError::Settings(detail);
    let pinned = require_plugin_schema(store, plugin_key)
        .await
        .map_err(|err| failed(err.to_string()))?;
    let instance = default_instance_for_key(store, plugin_key)
        .await
        .map_err(|err| failed(err.to_string()))?
        .ok_or_else(|| failed(format!("plugin `{plugin_key}` has no instance yet")))?;
    let document = load_instance_config(store, &instance.id)
        .await
        .map_err(|err| failed(err.to_string()))?;
    let mut body = document.body.clone();
    let parsed =
        parse_setting_text(&pinned.schema, key, value).map_err(|err| failed(err.to_string()))?;
    set_setting(&mut body, key, Some(parsed));
    match replace_instance_config(
        store,
        &ConfigActor::Operator {
            id: "migrate".into(),
        },
        &instance.id,
        &pinned.schema,
        document.revision,
        &body,
        &format!("migrate-{}-{key}", instance.id),
    )
    .await
    .map_err(|err| failed(err.to_string()))?
    {
        InstanceConfigReplace::Applied(_) | InstanceConfigReplace::Replayed { .. } => Ok(()),
        InstanceConfigReplace::Conflict { current_revision } => Err(failed(format!(
            "instance document changed concurrently (revision {current_revision})"
        ))),
    }
}
