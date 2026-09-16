//! `bookclerk plugins` — discover, manage, and invoke dynamic plugin CLI.

use std::path::PathBuf;

use bookclerk_config::{Config, PluginRegistryEntry};
use bookclerk_plugin_catalog::{
    federated_search, host_bookclerk_target, CargoAdapter, InstallOptions, InstallReceipt,
    Installer, NpmAdapter, PackageCoordinate, PypiAdapter, RegistryAdapter, SearchQuery,
    StaticAdapter, TrustPolicy,
};
use bookclerk_plugin_host::{
    consent_request, consent_summary, host_target_triple, require_grant, search_crates_io,
    CliInvokeParams, CliInvokeResult, CliSchema, DiscoveredPlugin, PluginGrantStore, PluginKind,
    PluginSession, CRATE_NAME_PREFIX, HOST_SHARED_ACCOUNT, OPERATOR_ACCOUNT,
};
use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;

use crate::cli_plugin::{
    find_command, matches_to_invoke_args, schema_to_command, RESERVED_PLUGIN_SUBCOMMANDS,
};
use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk plugins` subcommands for discover, install, enable, and registry ops.
pub enum PluginsCommand {
    /// List plugins found under plugin search directories.
    List,
    /// Search configured registries (and crates.io) for Bookclerk plugins.
    Search {
        /// Free-text query.
        query: Option<String>,
        /// Max hits to fetch (1–100).
        #[arg(long, default_value_t = 25)]
        limit: u32,
    },
    /// Install a plugin from a source-qualified coordinate or local archive.
    Install {
        /// Coordinate (`cargo:…@ver`, `registry:…#name@ver`, `local:…`) or legacy crate name.
        coordinate: String,
        /// Local archive path (sets artifact to this file; use with a static manifest coord).
        #[arg(long)]
        archive: Option<PathBuf>,
        /// Path to a Bookclerk package manifest JSON (required with `--archive` alone).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Override Bookclerk target (e.g. `linux-x64-gnu`).
        #[arg(long)]
        target: Option<String>,
        /// Replace an existing install with a different coordinate.
        #[arg(long)]
        replace: bool,
        /// Approve sandbox/network capability changes on update/replace.
        #[arg(long)]
        approve_capabilities: bool,
        /// Allow unsigned community plugins (digest still required).
        #[arg(long)]
        allow_unsigned: bool,
        /// Do not download; only resolve and print the plan.
        #[arg(long)]
        dry_run: bool,
        /// Refuse remote downloads.
        #[arg(long)]
        offline: bool,
    },
    /// Update an installed plugin to a newer matching version.
    Update {
        /// Plugin runtime id (default: all with receipts).
        id: Option<String>,
        /// Exact version to install.
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        /// Allow unsigned community plugins on this update (digest still required).
        allow_unsigned: bool,
        #[arg(long)]
        /// Approve sandbox/network capability changes without a separate prompt.
        approve_capabilities: bool,
        #[arg(long)]
        /// Resolve and print the plan without downloading.
        dry_run: bool,
    },
    /// Remove an installed plugin directory.
    Remove {
        /// Plugin runtime id.
        id: String,
        /// Also delete data/ and tmp/ state.
        #[arg(long)]
        purge_state: bool,
    },
    /// Show details for one discovered plugin (local + receipt).
    Info {
        /// Plugin id.
        id: String,
    },
    /// Run diagnose probes for one (or all) plugins that support it.
    Diagnose {
        /// Plugin id (default: all discovered that are enabled).
        id: Option<String>,
    },
    /// Check install integrity, target, jail, and describe/health status.
    Doctor {
        /// Plugin id (default: all discovered).
        id: Option<String>,
    },
    /// Enable a plugin in `config.toml`.
    Enable {
        /// Plugin id.
        id: String,
    },
    /// Disable a plugin in `config.toml`.
    Disable {
        /// Plugin id.
        id: String,
    },
    /// Approve network domains and host bindings for a plugin.
    ///
    /// Grants cover the plugin's declared outbound domains and bindings.
    /// Redirect hops after an allowed initial host do not require re-approval.
    Approve {
        /// Plugin id.
        id: String,
        /// Approve without interactive confirmation (required when stdin is not a TTY).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Manage configured plugin registries.
    Registry {
        #[command(subcommand)]
        /// Nested registry list/add/remove action.
        command: RegistryCommand,
    },
    /// Manage isolated plugin database bindings (`capabilities.bindings.databases`).
    Db {
        #[command(subcommand)]
        /// Nested db list/drop action.
        command: PluginDbCommand,
    },
}

#[derive(Debug, Subcommand)]
/// Nested `plugins db` actions over the `plugin_databases` registry.
pub enum PluginDbCommand {
    /// List provisioned plugin database bindings.
    List {
        /// Limit to one plugin id.
        plugin: Option<String>,
    },
    /// Drop provisioned binding units and their registry rows.
    ///
    /// Physically deletes the SQLite file (and journal sidecars), drops the
    /// PostgreSQL database, or deletes the Cloudflare D1 database, then
    /// removes the registry row. Physical delete must succeed first; unknown
    /// adapters fail closed and keep the row.
    Drop {
        /// Plugin id.
        plugin: String,
        /// Binding name (default: every binding of the plugin).
        binding: Option<String>,
        /// Drop without interactive confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
/// Nested `plugins registry` actions that edit `[[plugins.registries]]`.
pub enum RegistryCommand {
    /// List configured registries.
    List,
    /// Add a registry source to `config.toml`.
    Add {
        /// Adapter kind.
        #[arg(value_enum)]
        kind: RegistryKindArg,
        /// Index / registry URL (required for `static`).
        #[arg(long)]
        url: Option<String>,
        /// Optional display name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a registry by index (from `registry list`) or URL.
    Remove {
        /// Zero-based index from `plugins registry list`.
        #[arg(long)]
        index: Option<usize>,
        /// Match by URL.
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
/// Registry adapter kind accepted by `plugins registry add`.
pub enum RegistryKindArg {
    /// HTTPS index of Bookclerk package manifests.
    Static,
    /// crates.io (or a Cargo-compatible registry URL).
    Cargo,
    /// npm registry (default `registry.npmjs.org`).
    Npm,
    /// PyPI (or a compatible simple-index URL).
    Pypi,
}

#[derive(Debug, Serialize)]
/// One discovered plugin row for `plugins list` JSON/text output.
struct PluginListItem {
    /// Runtime plugin id from `plugin.toml`.
    id: String,
    /// Manifest kind (`source`, `integration`, `output`, `database`).
    kind: String,
    /// Whether this plugin is enabled in `config.toml`.
    enabled: bool,
    /// Guest executable path used for describe and CLI invoke.
    command: String,
    /// Optional human-readable name from the manifest.
    name: Option<String>,
    /// True when the manifest advertises at least one CLI command.
    has_cli: bool,
}

/// Dispatches a `plugins` subcommand against the loaded config.
pub async fn run(
    command: PluginsCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        PluginsCommand::List => {
            let dirs = bookclerk_plugin_host::plugin_search_dirs(config);
            let plugins = bookclerk_plugin_host::discover_plugins(config)?;
            let items: Vec<PluginListItem> = plugins
                .iter()
                .map(|p| PluginListItem {
                    id: p.manifest.id.clone(),
                    kind: p.manifest.kind.as_str().to_string(),
                    enabled: is_enabled(config, p),
                    command: p.command.display().to_string(),
                    name: p.manifest.name.clone(),
                    has_cli: p
                        .manifest
                        .cli
                        .as_ref()
                        .is_some_and(|c| !c.commands.is_empty()),
                })
                .collect();
            emit(
                format,
                &json!({ "search_dirs": dirs, "plugins": items }),
                || {
                    println!("search dirs:");
                    for d in &dirs {
                        println!("  {}", d.display());
                    }
                    if items.is_empty() {
                        println!("no plugins discovered");
                        return;
                    }
                    for p in &items {
                        println!(
                            "{} kind={} enabled={} cli={} command={}",
                            p.id, p.kind, p.enabled, p.has_cli, p.command
                        );
                    }
                },
            )
        }
        PluginsCommand::Search { query, limit } => run_search(config, query, limit, format),
        PluginsCommand::Install {
            coordinate,
            archive,
            manifest,
            target,
            replace,
            approve_capabilities,
            allow_unsigned,
            dry_run,
            offline,
        } => {
            run_install(
                config,
                &coordinate,
                archive.as_deref(),
                manifest.as_deref(),
                target,
                replace,
                approve_capabilities,
                allow_unsigned,
                dry_run,
                offline,
                format,
            )
            .await
        }
        PluginsCommand::Update {
            id,
            to,
            allow_unsigned,
            approve_capabilities,
            dry_run,
        } => {
            run_update(
                config,
                id,
                to,
                allow_unsigned,
                approve_capabilities,
                dry_run,
                format,
            )
            .await
        }
        PluginsCommand::Remove { id, purge_state } => run_remove(config, &id, purge_state, format),
        PluginsCommand::Info { id } => {
            let plugin = find_plugin(config, &id)?;
            let schema = plugin.manifest.cli.clone().unwrap_or_default();
            let enabled = is_enabled(config, &plugin);
            let receipt = InstallReceipt::load(&plugin.root).ok();
            let payload = json!({
                "id": plugin.manifest.id,
                "kind": plugin.manifest.kind.as_str(),
                "name": plugin.manifest.name,
                "enabled": enabled,
                "command": plugin.command.display().to_string(),
                "root": plugin.root.display().to_string(),
                "cli": schema,
                "receipt": receipt,
                "host_target": host_bookclerk_target(),
                "runtime": plugin.manifest.runtime,
            });
            emit(format, &payload, || {
                println!("id={}", plugin.manifest.id);
                println!("kind={}", plugin.manifest.kind.as_str());
                println!("name={}", plugin.manifest.name.as_deref().unwrap_or("-"));
                println!("enabled={enabled}");
                println!("command={}", plugin.command.display());
                println!("root={}", plugin.root.display());
                if let Ok(r) = InstallReceipt::load(&plugin.root) {
                    println!("coordinate={}", r.coordinate);
                    println!("version={}", r.version);
                    println!("artifact_sha256={}", r.archive_sha256);
                }
                if schema.commands.is_empty() {
                    println!(
                        "cli commands: (none in plugin.toml; may still advertise via describe)"
                    );
                } else {
                    println!("cli commands:");
                    for c in &schema.commands {
                        println!("  {} — {}", c.name, c.about.as_deref().unwrap_or(""));
                    }
                }
            })
        }
        PluginsCommand::Diagnose { id } => {
            let plugins = bookclerk_plugin_host::discover_plugins(config)?;
            let targets: Vec<_> = if let Some(id) = id {
                let p = plugins
                    .into_iter()
                    .find(|p| p.manifest.id == id)
                    .ok_or_else(|| anyhow::anyhow!("plugin `{id}` not discovered"))?;
                vec![p]
            } else {
                plugins
                    .into_iter()
                    .filter(|p| is_enabled(config, p))
                    .collect()
            };
            if targets.is_empty() {
                println!("no plugins to diagnose");
                return Ok(());
            }
            let mut reports = Vec::new();
            for plugin in targets {
                let lines = diagnose_plugin(config, &plugin).await?;
                reports.push(json!({
                    "id": plugin.manifest.id,
                    "lines": lines,
                }));
            }
            emit(format, &reports, || {
                for report in &reports {
                    let id = report["id"].as_str().unwrap_or("?");
                    println!("== {id}");
                    if let Some(lines) = report["lines"].as_array() {
                        for line in lines {
                            if let Some(s) = line.as_str() {
                                println!("{s}");
                            }
                        }
                    }
                }
            })
        }
        PluginsCommand::Doctor { id } => run_doctor(config, id, format).await,
        PluginsCommand::Enable { id } => set_plugin_enabled(config, &id, true, format),
        PluginsCommand::Disable { id } => set_plugin_enabled(config, &id, false, format),
        PluginsCommand::Approve { id, yes } => run_approve(config, &id, yes, format),
        PluginsCommand::Registry { command } => run_registry(config, command, format),
        PluginsCommand::Db { command } => run_plugin_db(config, command, format).await,
    }
}

/// Federated registry search, falling back to crates.io when no registries are configured.
fn run_search(
    config: &Config,
    query: Option<String>,
    limit: u32,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let q = SearchQuery {
        text: query.clone(),
        limit,
    };
    let mut owned: Vec<Box<dyn RegistryAdapter>> = Vec::new();
    if config.plugins.registries.is_empty() {
        owned.push(Box::new(CargoAdapter::default()));
    } else {
        for entry in &config.plugins.registries {
            match entry.kind.to_ascii_lowercase().as_str() {
                "static" => {
                    let url = entry
                        .url
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("static registry requires url"))?;
                    owned.push(Box::new(StaticAdapter::open(url)?));
                }
                "cargo" => owned.push(Box::new(CargoAdapter {
                    registry_url: entry
                        .url
                        .clone()
                        .unwrap_or_else(|| "https://crates.io".into()),
                })),
                "npm" => owned.push(Box::new(NpmAdapter {
                    registry_url: entry
                        .url
                        .clone()
                        .unwrap_or_else(|| "https://registry.npmjs.org".into()),
                })),
                "pypi" => owned.push(Box::new(PypiAdapter {
                    base_url: entry
                        .url
                        .clone()
                        .unwrap_or_else(|| "https://pypi.org".into()),
                })),
                other => anyhow::bail!("unknown registry kind `{other}`"),
            }
        }
    }
    let refs: Vec<&dyn RegistryAdapter> = owned.iter().map(|a| a.as_ref()).collect();
    let hits = match federated_search(&refs, &q) {
        Ok(h) => h,
        Err(_) if config.plugins.registries.is_empty() => {
            // Fall back to host crates.io DTO for empty-config search.
            let legacy = search_crates_io(query.as_deref(), limit)?;
            return emit(
                format,
                &json!({
                    "schema_version": 1,
                    "host_target": host_bookclerk_target(),
                    "rust_triple": host_target_triple(),
                    "prefix": CRATE_NAME_PREFIX,
                    "plugins": legacy,
                }),
                || {
                    if legacy.is_empty() {
                        println!(
                            "no crates.io plugins matching `{CRATE_NAME_PREFIX}*` yet \
                             (see docs/plugin-registry.md)"
                        );
                        return;
                    }
                    for h in &legacy {
                        let kind = h.parsed.as_ref().map(|p| p.kind.as_str()).unwrap_or("?");
                        let id = h.parsed.as_ref().map(|p| p.id.as_str()).unwrap_or("?");
                        println!(
                            "cargo:{}@{}  kind={kind} id={id} downloads={}",
                            h.crate_name, h.version, h.downloads
                        );
                        if let Some(desc) = &h.description {
                            println!("  {desc}");
                        }
                    }
                },
            );
        }
        Err(err) => return Err(err.into()),
    };
    emit(
        format,
        &json!({
            "schema_version": 1,
            "host_target": host_bookclerk_target(),
            "plugins": hits,
        }),
        || {
            if hits.is_empty() {
                println!(
                    "no plugins matched (configure [[plugins.registries]] or try cargo: search)"
                );
                return;
            }
            for h in &hits {
                let coord = h
                    .coordinate
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| {
                        format!("{}:{}@{}", h.source_kind, h.package_name, h.version)
                    });
                let runtime = h
                    .runtime
                    .as_ref()
                    .map(|r| format!("{}:{}", r.kind, r.id))
                    .unwrap_or_else(|| "?".into());
                println!("{coord}  runtime={runtime}");
                if let Some(desc) = &h.description {
                    println!("  {desc}");
                }
            }
        },
    )
}

#[allow(clippy::too_many_arguments)]
/// Installs from a coordinate or local archive, then health-checks and rolls back on failure.
async fn run_install(
    config: &Config,
    coordinate: &str,
    archive: Option<&std::path::Path>,
    manifest_path: Option<&std::path::Path>,
    target: Option<String>,
    replace: bool,
    approve_capabilities: bool,
    allow_unsigned: bool,
    dry_run: bool,
    offline: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plugins_root = config.paths().files_dir.join("plugins");
    let trust = TrustPolicy {
        allow_unsigned: allow_unsigned || config.plugins.allow_unsigned,
        ..TrustPolicy::default()
    };
    let opts = InstallOptions {
        plugins_root: plugins_root.clone(),
        target,
        dry_run,
        replace,
        offline,
        trust,
        skip_health: true,
        approve_capabilities,
    };

    let outcome = if let Some(archive) = archive {
        let manifest_path = manifest_path.ok_or_else(|| {
            anyhow::anyhow!("--archive requires --manifest <bookclerk-package.json>")
        })?;
        let text = std::fs::read_to_string(manifest_path)?;
        let manifest = bookclerk_plugin_catalog::BookclerkPackageManifest::from_json(&text)?;
        Installer::install_local_archive(archive, &manifest, &opts)?
    } else {
        let coord = resolve_coordinate(coordinate)?;
        let manifest = bookclerk_plugin_catalog::fetch_manifest_for_coordinate(&coord, &[])?;
        Installer::install_from_manifest(&manifest, &coord, &opts)?
    };

    // Post-install health when not dry-run.
    // Historical note: `skip_health: true` (install default) means "run health
    // here"; `false` leaves health + commit/rollback to the caller (update).
    if !outcome.dry_run && opts.skip_health {
        if let Err(err) = health_check_installed(config, &outcome.receipt.runtime.id).await {
            let _ = Installer::rollback(&outcome);
            anyhow::bail!("post-install health check failed: {err:#}; install rolled back");
        }
        let _ = Installer::commit(&outcome);
    }

    let payload = json!({
        "plugin_root": outcome.plugin_root,
        "dry_run": outcome.dry_run,
        "receipt": outcome.receipt,
    });
    emit(format, &payload, || {
        if outcome.dry_run {
            println!(
                "dry-run: would install {} -> {}",
                outcome.receipt.coordinate,
                outcome.plugin_root.display()
            );
        } else {
            println!(
                "installed {} ({}) -> {}",
                outcome.receipt.runtime.id,
                outcome.receipt.coordinate,
                outcome.plugin_root.display()
            );
        }
    })
}

/// Updates installed receipts to a newer (or pinned) version, restoring the previous tree on health failure.
async fn run_update(
    config: &Config,
    id: Option<String>,
    to: Option<String>,
    allow_unsigned: bool,
    approve_capabilities: bool,
    dry_run: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plugins_root = config.paths().files_dir.join("plugins");
    let plugins = bookclerk_plugin_host::discover_plugins(config)?;
    let targets: Vec<_> = plugins
        .into_iter()
        .filter(|p| id.as_ref().is_none_or(|want| want == &p.manifest.id))
        .filter(|p| InstallReceipt::path_in(&p.root).is_file())
        .collect();
    if targets.is_empty() {
        anyhow::bail!("no installed plugins with receipts to update");
    }
    let mut results = Vec::new();
    for plugin in targets {
        let receipt = InstallReceipt::load(&plugin.root)?;
        let mut coord = receipt.coordinate.clone();
        if let Some(ver) = &to {
            coord.version = ver.clone();
        } else {
            match bookclerk_plugin_catalog::resolve_newer_version(&coord, &[])? {
                Some(ver) => coord.version = ver,
                None => {
                    results.push(json!({
                        "id": plugin.manifest.id,
                        "coordinate": receipt.coordinate.to_string(),
                        "version": receipt.version,
                        "dry_run": dry_run,
                        "up_to_date": true,
                    }));
                    continue;
                }
            }
        }
        let manifest = bookclerk_plugin_catalog::fetch_manifest_for_coordinate(&coord, &[])?;
        let opts = InstallOptions {
            plugins_root: plugins_root.clone(),
            target: Some(receipt.target.clone()),
            dry_run,
            // Same runtime id from the same coordinate family; replace allows
            // collision but must not bypass capability approval (see installer).
            replace: true,
            offline: false,
            trust: TrustPolicy {
                allow_unsigned: allow_unsigned || config.plugins.allow_unsigned,
                ..TrustPolicy::default()
            },
            skip_health: false,
            approve_capabilities,
        };
        let outcome = Installer::install_from_manifest(&manifest, &coord, &opts)?;
        if !outcome.dry_run {
            if let Err(err) = health_check_installed(config, &plugin.manifest.id).await {
                let _ = Installer::rollback(&outcome);
                anyhow::bail!(
                    "update health check failed for {}: {err:#}; previous version restored",
                    plugin.manifest.id
                );
            }
            let _ = Installer::commit(&outcome);
        }
        results.push(json!({
            "id": plugin.manifest.id,
            "coordinate": outcome.receipt.coordinate.to_string(),
            "version": outcome.receipt.version,
            "dry_run": outcome.dry_run,
            "up_to_date": false,
        }));
    }
    emit(format, &results, || {
        for r in &results {
            println!(
                "{} -> {}{}",
                r["id"],
                r["coordinate"],
                if r["dry_run"].as_bool().unwrap_or(false) {
                    " (dry-run)"
                } else {
                    ""
                }
            );
        }
    })
}

/// Deletes an installed plugin directory, optionally purging `data/` and `tmp/`.
fn run_remove(
    config: &Config,
    id: &str,
    purge_state: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plugins_root = config.paths().files_dir.join("plugins");
    Installer::remove(&plugins_root, id, purge_state)?;
    let payload = json!({ "id": id, "purge_state": purge_state });
    emit(format, &payload, || {
        println!(
            "removed plugin `{id}`{}",
            if purge_state { " (purged state)" } else { "" }
        );
    })
}

/// Reports receipt integrity, executable digest, and describe/health status for one or all plugins.
async fn run_doctor(
    config: &Config,
    id: Option<String>,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plugins = bookclerk_plugin_host::discover_plugins(config)?;
    let targets: Vec<_> = if let Some(id) = id {
        let p = plugins
            .into_iter()
            .find(|p| p.manifest.id == id)
            .ok_or_else(|| anyhow::anyhow!("plugin `{id}` not discovered"))?;
        vec![p]
    } else {
        plugins
    };
    let mut reports = Vec::new();
    for plugin in targets {
        let mut lines = Vec::new();
        lines.push(format!("target_host={}", host_bookclerk_target()));
        lines.push(format!("runtime={:?}", plugin.manifest.runtime));
        match InstallReceipt::load(&plugin.root) {
            Ok(r) => {
                lines.push(format!("coordinate={}", r.coordinate));
                lines.push(format!("archive_sha256={}", r.archive_sha256));
                if let Ok(exe_digest) = bookclerk_plugin_catalog::sha256_file(&plugin.command) {
                    if let Some(expected) = &r.executable_sha256 {
                        if exe_digest.eq_ignore_ascii_case(expected) {
                            lines.push("executable_digest=ok".into());
                        } else {
                            lines.push(format!(
                                "executable_digest=MISMATCH got={exe_digest} expected={expected}"
                            ));
                        }
                    } else {
                        lines.push(format!("executable_digest={exe_digest} (not pinned)"));
                    }
                }
            }
            Err(_) => lines.push("receipt=missing (manual drop-in)".into()),
        }
        match health_check_installed(config, &plugin.manifest.id).await {
            Ok(msg) => lines.push(msg),
            Err(err) => lines.push(format!("health=FAIL {err:#}")),
        }
        reports.push(json!({ "id": plugin.manifest.id, "lines": lines }));
    }
    emit(format, &reports, || {
        for report in &reports {
            println!("== {}", report["id"]);
            if let Some(lines) = report["lines"].as_array() {
                for line in lines {
                    if let Some(s) = line.as_str() {
                        println!("  {s}");
                    }
                }
            }
        }
    })
}

#[derive(Debug, Serialize)]
/// One `plugins db list` row from the `plugin_databases` registry.
struct PluginDbListItem {
    /// Owning plugin id.
    plugin_id: String,
    /// Binding name from `plugin.toml` `capabilities.bindings.databases`.
    binding: String,
    /// Adapter family that provisioned the unit (`sqlite`, `postgres`, `d1`).
    backend_kind: String,
    /// Backend-native unit: file path, Postgres database name, or D1 database name.
    unit_ref: String,
    /// RFC 3339 provisioning time.
    created_at: String,
}

/// Lists or drops isolated plugin database bindings via the registry.
async fn run_plugin_db(
    config: &Config,
    command: PluginDbCommand,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let store = crate::registry::open_library(config).await?;
    match command {
        PluginDbCommand::List { plugin } => {
            let rows = store.list_plugin_databases(plugin.as_deref()).await?;
            let items: Vec<PluginDbListItem> = rows
                .into_iter()
                .map(|r| PluginDbListItem {
                    plugin_id: r.plugin_id,
                    binding: r.binding,
                    backend_kind: r.backend_kind,
                    unit_ref: r.unit_ref,
                    created_at: r.created_at,
                })
                .collect();
            emit(format, &items, || {
                if items.is_empty() {
                    println!("no plugin database bindings provisioned");
                    return;
                }
                for item in &items {
                    println!(
                        "{}/{}: backend={} unit={} created={}",
                        item.plugin_id,
                        item.binding,
                        item.backend_kind,
                        item.unit_ref,
                        item.created_at
                    );
                }
            })
        }
        PluginDbCommand::Drop {
            plugin,
            binding,
            yes,
        } => {
            let rows = store.list_plugin_databases(Some(&plugin)).await?;
            let rows: Vec<_> = rows
                .into_iter()
                .filter(|r| binding.as_deref().is_none_or(|b| r.binding == b))
                .collect();
            if rows.is_empty() {
                anyhow::bail!(
                    "no provisioned plugin database bindings match `{plugin}`{}",
                    binding.map(|b| format!("/{b}")).unwrap_or_default()
                );
            }
            if !yes {
                anyhow::bail!(
                    "refusing to drop {} binding unit(s) without --yes",
                    rows.len()
                );
            }
            for row in &rows {
                bookclerk_plugin_host::ExternalDatabase::drop_provisioned_unit(
                    config,
                    &row.backend_kind,
                    &row.unit_ref,
                )
                .await
                .map_err(|err| anyhow::anyhow!(err.to_string()))?;
                store
                    .remove_plugin_databases(&row.plugin_id, Some(&row.binding))
                    .await?;
                println!(
                    "deleted {}/{} ({} {})",
                    row.plugin_id, row.binding, row.backend_kind, row.unit_ref
                );
            }
            Ok(())
        }
    }
}

/// Lists, appends, or removes `[[plugins.registries]]` entries and writes `config.toml`.
fn run_registry(
    config: &Config,
    command: RegistryCommand,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        RegistryCommand::List => {
            let entries = &config.plugins.registries;
            emit(format, entries, || {
                if entries.is_empty() {
                    println!("no [[plugins.registries]] configured (default: crates.io search)");
                    return;
                }
                for (i, e) in entries.iter().enumerate() {
                    println!(
                        "{i}: kind={} url={} name={}",
                        e.kind,
                        e.url.as_deref().unwrap_or("-"),
                        e.name.as_deref().unwrap_or("-")
                    );
                }
            })
        }
        RegistryCommand::Add { kind, url, name } => {
            let kind = match kind {
                RegistryKindArg::Static => "static",
                RegistryKindArg::Cargo => "cargo",
                RegistryKindArg::Npm => "npm",
                RegistryKindArg::Pypi => "pypi",
            };
            if kind == "static" && url.as_ref().is_none_or(|u| u.trim().is_empty()) {
                anyhow::bail!("static registry requires --url");
            }
            let mut cfg = config.clone();
            cfg.plugins.registries.push(PluginRegistryEntry {
                kind: kind.into(),
                url,
                name,
            });
            let path = cfg.paths().config_file.clone();
            cfg.write_toml_file(&path)?;
            emit(
                format,
                &json!({ "wrote": path, "registries": cfg.plugins.registries }),
                || println!("added registry (wrote {})", path.display()),
            )
        }
        RegistryCommand::Remove { index, url } => {
            let mut cfg = config.clone();
            let before = cfg.plugins.registries.len();
            if let Some(i) = index {
                if i >= cfg.plugins.registries.len() {
                    anyhow::bail!("registry index {i} out of range");
                }
                cfg.plugins.registries.remove(i);
            } else if let Some(url) = url {
                cfg.plugins
                    .registries
                    .retain(|e| e.url.as_deref() != Some(url.as_str()));
            } else {
                anyhow::bail!("pass --index or --url");
            }
            if cfg.plugins.registries.len() == before {
                anyhow::bail!("no registry removed");
            }
            let path = cfg.paths().config_file.clone();
            cfg.write_toml_file(&path)?;
            emit(
                format,
                &json!({ "wrote": path, "registries": cfg.plugins.registries }),
                || println!("removed registry (wrote {})", path.display()),
            )
        }
    }
}

/// Parses a source-qualified coordinate, or a legacy `name@version` as `cargo:`.
fn resolve_coordinate(raw: &str) -> anyhow::Result<PackageCoordinate> {
    if raw.contains(':') {
        return Ok(PackageCoordinate::parse(raw)?);
    }
    // Legacy: bare crate name or id — require version via @.
    if let Some((name, ver)) = raw.rsplit_once('@') {
        return Ok(PackageCoordinate::parse(&format!("cargo:{name}@{ver}"))?);
    }
    anyhow::bail!(
        "install requires a source-qualified coordinate \
         (cargo:name@ver | npm:name@ver | pypi:name==ver | registry:url#name@ver | local:path) \
         or --archive/--manifest"
    );
}

/// Account id for a CLI spawn: sources/integrations cannot use the operator isolate.
fn cli_account(plugin: &DiscoveredPlugin) -> &'static str {
    match plugin.manifest.kind {
        PluginKind::Source | PluginKind::Integration => HOST_SHARED_ACCOUNT,
        _ => OPERATOR_ACCOUNT,
    }
}

/// Spawns a plugin guest for CLI health / invoke / diagnose.
async fn spawn_cli_session(
    config: &Config,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<PluginSession> {
    let settings = bookclerk_plugin_host::settings_table(config, plugin);
    Ok(PluginSession::spawn_for_account(
        plugin,
        config,
        toml_table_to_json(&settings),
        cli_account(plugin),
    )
    .await?)
}

/// Spawns the guest, calls `describe()`, and calls `health` when advertised.
async fn health_check_installed(config: &Config, id: &str) -> anyhow::Result<String> {
    let plugin = find_plugin(config, id)?;
    let session = spawn_cli_session(config, &plugin).await?;
    let api = session.describe_snapshot().api_version;
    let caps = session.describe_snapshot().supported_roles.len();
    if session.has_capability("health") {
        match plugin.manifest.kind {
            PluginKind::Source => {
                let _ = session.content_source_json("{}", "health", "{}").await?;
            }
            PluginKind::Integration => {
                let _ = session.integration_json("{}", "health", "{}").await?;
            }
            PluginKind::Database => {
                probe_database(&session, config, &plugin).await?;
            }
            PluginKind::Output => {
                anyhow::bail!(
                    "plugin `{}` advertises health but output guests have no health RPC",
                    plugin.manifest.id
                );
            }
        }
        Ok(format!("health=ok api={api} roles={caps}"))
    } else {
        Ok(format!("api={api} roles={caps}"))
    }
}

/// Invoke `bookclerk plugins <plugin-id> <command> …` via JSON-RPC `cli.invoke`.
pub async fn run_plugin_cli(
    config: &Config,
    plugin_id: &str,
    argv: &[String],
    format: OutputFormat,
) -> anyhow::Result<()> {
    if RESERVED_PLUGIN_SUBCOMMANDS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(plugin_id))
    {
        anyhow::bail!("`{plugin_id}` is a reserved plugins subcommand");
    }
    let plugin = find_plugin(config, plugin_id)?;
    let help_only = argv.iter().any(|a| a == "--help" || a == "-h");

    // Help uses manifest schema (no spawn / enable required).
    let (schema, session) = if help_only {
        (plugin.manifest.cli.clone().unwrap_or_default(), None)
    } else {
        if !is_enabled(config, &plugin) {
            anyhow::bail!(
                "plugin `{}` is disabled; run `bookclerk plugins enable {}`",
                plugin.manifest.id,
                plugin.manifest.id
            );
        }
        let session = spawn_cli_session(config, &plugin).await?;
        let schema = resolve_schema(&session, &plugin).await?;
        (schema, Some(session))
    };

    if schema.commands.is_empty() && !help_only {
        anyhow::bail!(
            "plugin `{}` has no CLI commands (advertise capability `cli` + cli.describe)",
            plugin.manifest.id
        );
    }

    let clap_cmd = schema_to_command(
        &plugin.manifest.id,
        plugin.manifest.name.as_deref(),
        &schema,
    );
    let mut argv_full = vec![plugin.manifest.id.clone()];
    argv_full.extend(argv.iter().cloned());
    let matches = match clap_cmd.try_get_matches_from(&argv_full) {
        Ok(m) => m,
        Err(err) => {
            err.print().map_err(|e| anyhow::anyhow!("{e}"))?;
            return if err.use_stderr() {
                Err(anyhow::anyhow!("invalid plugin CLI arguments"))
            } else {
                Ok(())
            };
        }
    };

    let Some(session) = session else {
        return Ok(());
    };

    let (cmd_name, cmd_matches) = matches
        .subcommand()
        .ok_or_else(|| anyhow::anyhow!("missing plugin subcommand"))?;
    let spec = find_command(&schema, cmd_name).ok_or_else(|| {
        anyhow::anyhow!(
            "plugin `{}` has no command `{cmd_name}`",
            plugin.manifest.id
        )
    })?;
    let args = matches_to_invoke_args(spec, cmd_matches)?;
    let params = CliInvokeParams {
        command: cmd_name.to_string(),
        args,
    };
    let raw = session
        .cli_invoke_json(serde_json::to_string(&params)?)
        .await?;
    let result: CliInvokeResult = serde_json::from_str(&raw)?;

    if format.is_json() {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        if !result.stdout.is_empty() {
            print!("{}", result.stdout);
            if !result.stdout.ends_with('\n') {
                println!();
            }
        }
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
            if !result.stderr.ends_with('\n') {
                eprintln!();
            }
        }
        if let Some(json) = &result.json {
            if result.stdout.is_empty() {
                println!("{}", serde_json::to_string_pretty(json)?);
            }
        }
    }
    if result.exit_code != 0 {
        anyhow::bail!(
            "plugin `{}` command `{cmd_name}` exited {}",
            plugin.manifest.id,
            result.exit_code
        );
    }
    Ok(())
}

/// Augment the `plugins` clap command with discovered plugin ids (manifest CLI).
pub fn augment_plugins_command(mut plugins_cmd: clap::Command, config: &Config) -> clap::Command {
    let Ok(discovered) = bookclerk_plugin_host::discover_plugins(config) else {
        return plugins_cmd;
    };
    for plugin in discovered {
        if RESERVED_PLUGIN_SUBCOMMANDS
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&plugin.manifest.id))
        {
            tracing::warn!(
                id = %plugin.manifest.id,
                "plugin id conflicts with a reserved plugins subcommand; skipping CLI registration"
            );
            continue;
        }
        let schema = plugin.manifest.cli.clone().unwrap_or_default();
        let about = plugin
            .manifest
            .name
            .clone()
            .unwrap_or_else(|| format!("Plugin `{}`", plugin.manifest.id));
        let sub = if schema.commands.is_empty() {
            clap::Command::new(plugin.manifest.id.clone())
                .about(about)
                .subcommand_required(false)
                .arg_required_else_help(false)
                .after_help("This plugin has no CLI schema in plugin.toml. If it advertises capability `cli`, commands are resolved at invoke time via cli.describe.")
        } else {
            schema_to_command(&plugin.manifest.id, Some(about.as_str()), &schema)
        };
        plugins_cmd = plugins_cmd.subcommand(sub);
    }
    plugins_cmd
}

/// Prefers live `cli.describe`, then the describe-metadata CLI schema, then the on-disk manifest schema.
async fn resolve_schema(
    session: &PluginSession,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<CliSchema> {
    if session.has_capability("cli") {
        let raw = session.cli_describe().await?;
        return Ok(serde_json::from_str(&raw)?);
    }
    if let Some(cli) = session.plugin_metadata().cli {
        return Ok(cli);
    }
    Ok(plugin.manifest.cli.clone().unwrap_or_default())
}

/// Opens the database guest and issues `SELECT 1` as the health/diagnose probe.
async fn probe_database(
    session: &PluginSession,
    config: &Config,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<()> {
    let ctx = bookclerk_plugin_host::database_connect_context(config, plugin, session)?;
    session.db_open(ctx).await?;
    let _ = session.db_capabilities().await?;
    Ok(())
}

/// Spawns the guest and collects `diagnose` probe lines, or notes a missing capability.
async fn diagnose_plugin(
    config: &Config,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<Vec<String>> {
    let session = spawn_cli_session(config, plugin).await?;
    if !session.has_capability("diagnose") {
        return Ok(vec![format!(
            "plugin `{}` has no diagnose capability",
            plugin.manifest.id
        )]);
    }
    let raw: anyhow::Result<String> = match plugin.manifest.kind {
        PluginKind::Source => session
            .content_source_json("{}", "diagnose", "{}")
            .await
            .map_err(anyhow::Error::from),
        PluginKind::Integration => session
            .integration_json("{}", "diagnose", "{}")
            .await
            .map_err(anyhow::Error::from),
        PluginKind::Database => probe_database(&session, config, plugin)
            .await
            .map(|()| r#"["ping=ok"]"#.to_string()),
        PluginKind::Output => {
            return Ok(vec![format!(
                "plugin `{}` advertises diagnose but output guests have no diagnose RPC",
                plugin.manifest.id
            )]);
        }
    };
    match raw {
        Ok(text) => Ok(parse_diagnose_lines(&text)),
        Err(err) => Ok(vec![format!("diagnose failed: {err:#}")]),
    }
}

/// Parses a diagnose JSON array, `{ "lines": [...] }`, or a raw fallback string.
fn parse_diagnose_lines(raw: &str) -> Vec<String> {
    if let Ok(lines) = serde_json::from_str::<Vec<String>>(raw) {
        return lines;
    }
    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(arr) = obj.get("lines").and_then(|v| v.as_array()) {
            return arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
    }
    vec![raw.to_string()]
}

/// Interactively (or `--yes`) records a network/binding grant for the plugin.
fn run_approve(config: &Config, id: &str, yes: bool, format: OutputFormat) -> anyhow::Result<()> {
    use std::io::{self, IsTerminal, Write};

    let plugin = find_plugin(config, id)?;
    let grant = consent_request(&plugin.manifest);
    let summary = consent_summary(&grant);

    for line in &summary {
        println!("{line}");
    }

    if !yes {
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "stdin is not a TTY; pass --yes to approve plugin `{}` non-interactively",
                plugin.manifest.id
            );
        }
        eprint!(
            "Approve these permissions for `{}`? [y/N] ",
            plugin.manifest.id
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let ok = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
        if !ok {
            anyhow::bail!("approval cancelled");
        }
    }

    let files_dir = &config.paths().files_dir;
    let mut store = PluginGrantStore::load(files_dir)?;
    store.upsert(grant.clone());
    store.save(files_dir)?;

    let payload = json!({
        "id": plugin.manifest.id,
        "approved": true,
        "grant": grant,
        "summary": summary,
        "grants_file": PluginGrantStore::path(files_dir),
    });
    emit(format, &payload, || {
        println!(
            "approved permissions for plugin `{}` (wrote {})",
            plugin.manifest.id,
            PluginGrantStore::path(files_dir).display()
        );
    })
}

/// Enables or disables a plugin in `config.toml` after grant checks; refuses disabling the active database backend.
fn set_plugin_enabled(
    config: &Config,
    id: &str,
    enabled: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plugin = find_plugin(config, id)?;
    if enabled {
        require_grant(&config.paths().files_dir, &plugin.manifest).map_err(|err| {
            anyhow::anyhow!(
                "{err}\nRun `bookclerk plugins approve {}` first.",
                plugin.manifest.id
            )
        })?;
    }
    let mut cfg = config.clone();
    match plugin.manifest.kind {
        PluginKind::Source => cfg.sources.set_enabled(&plugin.manifest.id, enabled),
        PluginKind::Integration => cfg.integrations.set_enabled(&plugin.manifest.id, enabled),
        PluginKind::Output if plugin.manifest.id == "s3" => {
            cfg.output.s3.enabled = enabled;
        }
        PluginKind::Output if plugin.manifest.id == "local" => {
            cfg.output.local.enabled = enabled;
        }
        PluginKind::Output => {
            anyhow::bail!(
                "output plugin `{}` enable/disable is not mapped to config.toml yet",
                plugin.manifest.id
            );
        }
        PluginKind::Database => {
            if enabled {
                cfg.database.plugin = plugin.manifest.id.clone();
            } else if matches_plugin_id(&plugin.manifest.id, &config.database.plugin) {
                anyhow::bail!(
                    "cannot disable the active database plugin `{}`; \
                     enable another backend first with `bookclerk plugins enable <id>`",
                    plugin.manifest.id
                );
            } else {
                anyhow::bail!(
                    "database plugin `{}` is not active ([database].plugin = `{}`)",
                    plugin.manifest.id,
                    config.database.plugin
                );
            }
        }
    }
    let path = cfg.paths().config_file.clone();
    cfg.write_toml_file(&path)?;
    let payload = json!({
        "id": plugin.manifest.id,
        "enabled": enabled,
        "config": path.display().to_string(),
    });
    emit(format, &payload, || {
        println!(
            "{} plugin {} (wrote {})",
            if enabled { "enabled" } else { "disabled" },
            plugin.manifest.id,
            path.display()
        );
    })
}

/// Case-insensitive equality between a manifest id and the active `[database].plugin`.
fn matches_plugin_id(manifest_id: &str, active: &str) -> bool {
    manifest_id.eq_ignore_ascii_case(active)
}

/// Discovers plugins and returns the one whose manifest id matches, or errors if missing.
fn find_plugin(config: &Config, id: &str) -> anyhow::Result<DiscoveredPlugin> {
    let plugins = bookclerk_plugin_host::discover_plugins(config)?;
    plugins
        .into_iter()
        .find(|p| p.manifest.id == id)
        .ok_or_else(|| anyhow::anyhow!("plugin `{id}` not discovered"))
}

/// Whether `config.toml` currently enables this discovered plugin.
fn is_enabled(config: &Config, plugin: &DiscoveredPlugin) -> bool {
    match plugin.manifest.kind {
        PluginKind::Source => config.sources.is_enabled(&plugin.manifest.id),
        PluginKind::Integration => config.integrations.is_enabled(&plugin.manifest.id),
        PluginKind::Output if plugin.manifest.id == "s3" => config.output.s3.enabled,
        PluginKind::Output if plugin.manifest.id == "local" => config.output.local.enabled,
        PluginKind::Output => false,
        PluginKind::Database => config
            .database
            .plugin
            .eq_ignore_ascii_case(&plugin.manifest.id),
    }
}

/// Converts a plugin settings TOML table to JSON, substituting `{}` if serialization fails.
fn toml_table_to_json(table: &toml::Table) -> serde_json::Value {
    serde_json::to_value(table).unwrap_or_else(|_| json!({}))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn parse_diagnose_lines_reads_json_array() {
        assert_eq!(
            parse_diagnose_lines(r#"["ping=ok","wal=ok"]"#),
            vec!["ping=ok".to_string(), "wal=ok".to_string()]
        );
    }

    #[test]
    fn parse_diagnose_lines_does_not_treat_cli_schema_as_probes() {
        let schema = r#"{"name":"d1","commands":[{"name":"query"}]}"#;
        let lines = parse_diagnose_lines(schema);
        assert_eq!(lines, vec![schema.to_string()]);
    }
}
