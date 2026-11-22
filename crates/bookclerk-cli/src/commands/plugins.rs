//! `bookclerk plugins` — discover, manage, and invoke dynamic plugin CLI.

use std::collections::BTreeSet;
use std::path::PathBuf;

use bookclerk_config::{Config, PluginRegistryEntry};
use bookclerk_library::control_plane::{
    create_node_local_instance, create_plugin_instance, default_instance_for_key,
    delete_plugin_instance, ensure_plugin_deployment, import_instance_config_if_absent,
    list_all_plugin_instances, list_deployments_for_instance, list_node_ids, list_plugin_schemas,
    load_deployment_for_node, load_instance_config, load_observation, load_plugin_instance,
    parse_setting_text, replace_instance_config, replace_plugin_deployment, require_plugin_schema,
    seal_instance_secret, set_secret_ref, set_setting, ConfigActor, DeploymentReplace,
    InstanceConfigReplace, PluginInstance, PluginInstanceConfigV1, PluginInstanceId, SettingCell,
    SettingValue,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_catalog::{
    federated_search, host_bookclerk_target, CargoAdapter, InstallOptions, InstallOutcome,
    InstallReceipt, Installer, NpmAdapter, PackageCoordinate, PluginMutationLock, PypiAdapter,
    RegistryAdapter, SearchQuery, StaticAdapter, TrustPolicy,
};
use bookclerk_plugin_host::{
    consent_request, consent_summary, host_target_triple,
    install_from_manifest_with_configured_aliases, is_bootstrap_only, plugin_matches_occupancy,
    require_grant, search_crates_io, CliInvokeParams, CliInvokeResult, CliSchema, DiscoveredPlugin,
    Entrypoint, PluginFamily, PluginGrant, PluginGrantStore, PluginSession, CRATE_NAME_PREFIX,
    HOST_SHARED_ACCOUNT, OPERATOR_ACCOUNT,
};
use clap::{Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;

use crate::cli_plugin::{
    find_command, matches_to_invoke_args, schema_to_command, RESERVED_PLUGIN_SUBCOMMANDS,
};
use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// `bookclerk plugins` subcommands: install plugins into the library, deploy
/// them onto nodes, configure instances, and manage registries.
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
    /// Install a plugin version into the library from a source-qualified
    /// coordinate or a local archive. The first installed version (or
    /// `--activate`) becomes the active version every node runs; deploy it
    /// with `plugins deploy`.
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
        /// Make this the active version even when another version is active.
        #[arg(long, alias = "replace")]
        activate: bool,
        /// Record the requested consent as approved (no separate `approve`).
        #[arg(long)]
        approve_capabilities: bool,
        /// Allow community packages without independent publisher authenticity (digest still required).
        #[arg(long)]
        allow_unverified_publisher: bool,
        /// Do not download; only resolve and print the plan.
        #[arg(long)]
        dry_run: bool,
        /// Refuse remote downloads.
        #[arg(long)]
        offline: bool,
    },
    /// Install and activate a newer version of an installed plugin (or
    /// update a staged package by its receipt).
    Update {
        /// Plugin runtime id (default: all with receipts).
        id: Option<String>,
        /// Exact version to install.
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        /// Allow community packages without independent publisher authenticity on this update (digest still required).
        allow_unverified_publisher: bool,
        #[arg(long)]
        /// Approve sandbox/network capability changes without a separate prompt.
        approve_capabilities: bool,
        #[arg(long)]
        /// Resolve and print the plan without downloading.
        dry_run: bool,
    },
    /// Activate an earlier installed version of a plugin.
    Rollback {
        /// Plugin alias or PluginKey.
        id: String,
        /// Installed version to activate.
        version: String,
    },
    /// List installed versions (the active one is starred).
    Versions {
        /// Only this plugin (alias or PluginKey).
        id: Option<String>,
    },
    /// Uninstall a plugin: clear its active version and undeploy it from
    /// every node (or remove a staged package directory).
    Uninstall {
        /// Plugin runtime id.
        id: String,
        /// Also delete `$FILES_DIR/plugin-state/<PluginKey>/`.
        #[arg(long)]
        purge_state: bool,
        /// Delete rejected ACL journals with plugin state. Requires `--purge-state`.
        /// Does not revoke the package-SID ACEs those records name; read the
        /// rejected file and remove those ACEs by hand first.
        #[arg(long)]
        discard_acl_journals: bool,
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
    /// Deploy a plugin instance. With no flags it runs on this node (or on
    /// `--node`) by hand: the instance's deployment there is `present`.
    /// With `--replicas` the library scheduler (one daemon, under a library
    /// lease) places it on that many nodes carrying every `--on` label and
    /// writes the deployments itself; `--manual` hands those deployments
    /// back to be managed by hand.
    ///
    /// Database adapters are the bootstrap exception and still select
    /// `[database].plugin` in `config.toml`.
    Deploy {
        /// Plugin alias, PluginKey, or plugin instance id (an alias or key
        /// names the plugin's default instance).
        id: String,
        /// Node to deploy on by hand (default: this node).
        #[arg(long, conflicts_with_all = ["replicas", "on", "manual"])]
        node: Option<String>,
        /// Nodes that should run the instance: a number (0 stops every
        /// replica) or `all` (every live, schedulable node with room).
        #[arg(long)]
        replicas: Option<bookclerk_library::control_plane::Replicas>,
        /// Label constraint every chosen node must satisfy (`nas`,
        /// `zone=eu`, `node=<id>`, `target=linux-x64-gnu`); prefix with `!`
        /// to exclude. Repeatable.
        #[arg(long = "on", value_name = "LABEL", requires = "replicas")]
        on: Vec<String>,
        /// Drop the scheduler placement: the instance's deployments stay as
        /// they are and are managed by hand again.
        #[arg(long, conflicts_with_all = ["replicas", "on"])]
        manual: bool,
    },
    /// Undeploy a plugin instance from this node (or `--node`): its
    /// deployment there becomes `absent` and the guest is retired.
    Undeploy {
        /// Plugin alias, PluginKey, or plugin instance id.
        id: String,
        /// Node to undeploy from (default: this node).
        #[arg(long)]
        node: Option<String>,
    },
    /// Plugin instances: one per plugin by default, more when an operator
    /// wants several configurations of one plugin.
    Instances {
        #[command(subcommand)]
        /// Instance action.
        command: InstancesCommand,
    },
    /// Instance settings, validated against the plugin's declared schema.
    Config {
        #[command(subcommand)]
        /// Settings action.
        command: PluginConfigCommand,
    },
    /// Approve network domains and host bindings for a plugin.
    ///
    /// Grants cover the plugin's declared outbound domains and bindings.
    /// Operators may add extra network destinations. Structural capabilities
    /// cannot be invented beyond the package. Redirect policy is separate.
    Approve {
        /// Plugin id.
        id: String,
        /// Approve without interactive confirmation (required when stdin is not a TTY).
        #[arg(long, short = 'y')]
        yes: bool,
        /// Approve the package's request as is, dropping the network
        /// destinations you added or denied earlier and your redirect
        /// answer. Without it a re-approval keeps them. (An installed
        /// version always records its package's request.)
        #[arg(long)]
        reset: bool,
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
/// `plugins instances` subcommands.
pub enum InstancesCommand {
    /// List instances with their deployments on every node.
    List {
        /// Only instances of this plugin (alias or PluginKey).
        #[arg(long)]
        plugin: Option<String>,
    },
    /// Create another instance of a plugin that has a pinned schema.
    Create {
        /// Plugin alias or PluginKey.
        plugin: String,
        /// Operator-facing name.
        #[arg(long)]
        name: Option<String>,
        /// Make the instance local to this node (a node's own `local`
        /// output, for example): it deploys only there and is never
        /// placed by the scheduler.
        #[arg(long, value_name = "NODE_ID")]
        node: Option<String>,
    },
    /// Delete an instance that has no deployment on any node.
    Delete {
        /// Plugin instance id.
        instance: String,
    },
    /// Show or change which books a storage destination takes.
    Filter {
        #[command(subcommand)]
        /// Nested get/set/clear action.
        command: FilterCommand,
    },
}

#[derive(Debug, Subcommand)]
/// `plugins instances filter` subcommands.
pub enum FilterCommand {
    /// Show a destination's filter.
    Get {
        /// Plugin alias, PluginKey, or plugin instance id of the destination.
        instance: String,
    },
    /// Replace a destination's filter. Each rule is `FIELD=VALUE`, where
    /// FIELD is `patron` (a user id), `storefront` (a plugin alias such as
    /// `audible`), `account` (a store account id), `media_kind` (`book`
    /// or `podcast`), `format` (a copy format: `m4b`, `mp3`,
    /// `mp3_chapters` or `original`) or `quality` (a copy quality such as
    /// `original`, `64k-mono` or `v2`). With any `--include`, the
    /// destination takes only books that match one; an `--exclude` always
    /// wins. `format` and `quality` rules choose which of the library's
    /// copies (`output.copies`) the destination receives, the same way
    /// among themselves. Acquires that start afterwards use the new filter.
    Set {
        /// Plugin alias, PluginKey, or plugin instance id of the destination.
        instance: String,
        /// Take books that match this rule (repeatable).
        #[arg(long, value_name = "FIELD=VALUE")]
        include: Vec<String>,
        /// Refuse books that match this rule (repeatable).
        #[arg(long, value_name = "FIELD=VALUE")]
        exclude: Vec<String>,
    },
    /// Remove a destination's filter so it takes every book.
    Clear {
        /// Plugin alias, PluginKey, or plugin instance id of the destination.
        instance: String,
    },
}

#[derive(Debug, Subcommand)]
/// `plugins config` subcommands.
pub enum PluginConfigCommand {
    /// Show an instance's effective settings: schema defaults under the
    /// operator's document. Secrets show as set or unset, never their value.
    Get {
        /// Plugin alias, PluginKey, or plugin instance id.
        instance: String,
    },
    /// Set one setting. A `secret` setting takes `--secret` and reads the
    /// value from stdin so it never appears on a command line.
    Set {
        /// Plugin alias, PluginKey, or plugin instance id.
        instance: String,
        /// Setting key from the plugin's `[[settings]]`.
        key: String,
        /// Value text: `true`/`false`, a number, or a string. A `table`
        /// setting takes a JSON array of row objects, or one row with its
        /// cells in column order separated by `|` (an empty cell takes the
        /// column default; `\|` is a literal bar).
        value: Option<String>,
        /// Seal a secret read from stdin.
        #[arg(long)]
        secret: bool,
        /// For a `table` setting, add the given rows after the current ones
        /// instead of replacing them.
        #[arg(long, conflicts_with = "secret")]
        append: bool,
    },
    /// Unset one setting so the schema default applies, or drop a secret.
    Unset {
        /// Plugin alias, PluginKey, or plugin instance id.
        instance: String,
        /// Setting key.
        key: String,
        /// For a `table` setting, remove only this row (counted from 1, as
        /// `config get` lists them) and keep the others.
        #[arg(long, value_name = "N")]
        row: Option<usize>,
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
    /// Runtime plugin id from `plugin.toml` (unique within this node plugin namespace).
    id: String,
    /// Provenance-qualified PluginKey (canonical text).
    plugin_key: String,
    /// Host-evaluated provenance (`platform_bundled`, `verified_staged`, `installed`, …).
    provenance: String,
    /// Primary handler family (`source`, `integration`, `output`, `database`).
    family: String,
    /// Exported entrypoints declared in `plugin.toml` (`storefront`, `cli`, …).
    entrypoints: Vec<String>,
    /// True when a deployment of the plugin's default instance is desired
    /// present on this node (database adapters: selected in `[database]`).
    deployed: bool,
    /// Guest executable path used for describe and CLI invoke.
    command: String,
    /// Optional human-readable name from the manifest.
    name: Option<String>,
    /// True when the manifest advertises at least one CLI command.
    has_cli: bool,
    /// Instances of this plugin with their deployments.
    instances: Vec<InstanceListItem>,
}

#[derive(Debug, Clone, Serialize)]
/// One plugin instance row for `plugins list` / `plugins instances list`.
struct InstanceListItem {
    /// Instance id.
    plugin_instance_id: String,
    /// Canonical PluginKey.
    plugin_key: String,
    /// Manifest id recorded with the pinned schema.
    alias: String,
    /// Operator-facing name; empty for the default instance.
    display_name: String,
    /// True when a bare key or alias routes to this instance.
    is_default: bool,
    /// Deployment rows across nodes.
    deployments: Vec<DeploymentListItem>,
    /// Scheduler placement; absent when the instance is hand-managed.
    #[serde(skip_serializing_if = "Option::is_none")]
    placement: Option<PlacementListItem>,
}

#[derive(Debug, Clone, Serialize)]
/// Scheduler placement of one instance.
struct PlacementListItem {
    /// Requested replicas.
    replicas: bookclerk_library::control_plane::Replicas,
    /// Label constraints.
    constraints: Vec<String>,
    /// Intent revision.
    revision: i64,
    /// Replicas the last scheduler pass placed.
    placed: u32,
    /// Why fewer are placed; empty when satisfied.
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
/// One deployment row with that node's latest observation.
struct DeploymentListItem {
    /// Node the row targets.
    node_id: String,
    /// `present` or `absent`.
    desired: String,
    /// Compare-and-swap revision.
    revision: i64,
    /// Latest observation status, when the node reported one.
    status: Option<String>,
    /// Observation detail (redacted error text).
    detail: Option<String>,
    /// True when `node_id` is this node.
    this_node: bool,
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
            let host = crate::registry::open_host(config).await?;
            let plugins = bookclerk_plugin_host::discover_plugins(config)?;
            let mut items = Vec::with_capacity(plugins.len());
            for p in &plugins {
                let instances = if is_bootstrap_only(p) {
                    Vec::new()
                } else {
                    instance_items(
                        &host.store,
                        host.node_id(),
                        Some(p.plugin_key().canonical()),
                    )
                    .await?
                };
                items.push(PluginListItem {
                    id: p.manifest.id.clone(),
                    plugin_key: p.plugin_key().canonical().to_string(),
                    provenance: p.identity.provenance.to_string(),
                    family: p.manifest.primary_family().as_str().to_string(),
                    entrypoints: p
                        .manifest
                        .entrypoints
                        .iter()
                        .map(|e| e.wire_name().to_string())
                        .collect(),
                    deployed: deployed_here(config, p, &instances),
                    command: p.command.display().to_string(),
                    name: p.manifest.name.clone(),
                    has_cli: p
                        .manifest
                        .cli
                        .as_ref()
                        .is_some_and(|c| !c.commands.is_empty()),
                    instances,
                });
            }
            emit(
                format,
                &json!({ "node_id": host.node_id(), "search_dirs": dirs, "plugins": items }),
                || {
                    println!("node: {}", host.node_id());
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
                            "{} key={} provenance={} family={} entrypoints={} deployed={} cli={} command={}",
                            p.id,
                            p.plugin_key,
                            p.provenance,
                            p.family,
                            p.entrypoints.join(","),
                            p.deployed,
                            p.has_cli,
                            p.command
                        );
                        for instance in &p.instances {
                            print_instance_line(instance);
                        }
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
            activate,
            approve_capabilities,
            allow_unverified_publisher,
            dry_run,
            offline,
        } => {
            crate::commands::plugin_installs::run_install(
                config,
                crate::commands::plugin_installs::InstallArgs {
                    coordinate: &coordinate,
                    archive: archive.as_deref(),
                    manifest: manifest.as_deref(),
                    target,
                    activate,
                    approve_capabilities,
                    allow_unverified_publisher,
                    dry_run,
                    offline,
                },
                format,
            )
            .await
        }
        PluginsCommand::Rollback { id, version } => {
            crate::commands::plugin_installs::run_rollback(config, &id, &version, format).await
        }
        PluginsCommand::Versions { id } => {
            crate::commands::plugin_installs::run_list(config, id.as_deref(), format).await
        }
        PluginsCommand::Update {
            id,
            to,
            allow_unverified_publisher,
            approve_capabilities,
            dry_run,
        } => {
            let installed = crate::commands::plugin_installs::run_update(
                config,
                id.as_deref(),
                to.as_deref(),
                allow_unverified_publisher,
                approve_capabilities,
                dry_run,
                format,
            )
            .await?;
            if installed {
                return Ok(());
            }
            run_update(
                config,
                id,
                to,
                allow_unverified_publisher,
                approve_capabilities,
                dry_run,
                format,
            )
            .await
        }
        PluginsCommand::Uninstall {
            id,
            purge_state,
            discard_acl_journals,
        } => {
            if crate::commands::plugin_installs::run_uninstall(config, &id, format).await? {
                return Ok(());
            }
            run_remove(config, &id, purge_state, discard_acl_journals, format)
        }
        PluginsCommand::Info { id } => {
            let plugin = find_plugin(config, &id)?;
            let schema = plugin.manifest.cli.clone().unwrap_or_default();
            let host = crate::registry::open_host(config).await?;
            let instances = if is_bootstrap_only(&plugin) {
                Vec::new()
            } else {
                instance_items(
                    &host.store,
                    host.node_id(),
                    Some(plugin.plugin_key().canonical()),
                )
                .await?
            };
            let enabled = deployed_here(config, &plugin, &instances);
            let receipt = InstallReceipt::load(&plugin.root).ok();
            let requested = consent_request(&plugin.manifest, plugin.plugin_key());
            let stored = PluginGrantStore::load(&config.paths().files_dir)
                .ok()
                .and_then(|store| {
                    store
                        .get_by_plugin_key(plugin.plugin_key().canonical())
                        .cloned()
                });
            let pending = stored
                .as_ref()
                .map(|g| bookclerk_plugin_host::pending_structural(g, &requested));
            let payload = json!({
                "id": plugin.manifest.id,
                "family": plugin.manifest.primary_family().as_str(),
                "families": plugin.manifest.families(),
                "entrypoints": plugin.manifest.entrypoints,
                "consumes": plugin.manifest.events.consumers.iter().map(|c| c.event_type.as_str()).collect::<Vec<_>>(),
                "produces": plugin.manifest.producer_types(),
                "name": plugin.manifest.name,
                "deployed": enabled,
                "command": plugin.command.display().to_string(),
                "root": plugin.root.display().to_string(),
                "cli": schema,
                "receipt": receipt,
                "node_target": host_bookclerk_target(),
                "runtime": plugin.manifest.runtime,
                "plugin_key": plugin.plugin_key().canonical(),
                "pending_structural": pending.as_ref().map(|p| p.summary_lines()),
                "settings": plugin.manifest.settings,
                "instances": instances,
            });
            emit(format, &payload, || {
                println!("id={}", plugin.manifest.id);
                println!("plugin_key={}", plugin.plugin_key().canonical());
                println!("family={}", plugin.manifest.primary_family().as_str());
                println!(
                    "entrypoints={}",
                    plugin
                        .manifest
                        .entrypoints
                        .iter()
                        .map(|e| e.wire_name())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                println!("name={}", plugin.manifest.name.as_deref().unwrap_or("-"));
                println!("deployed={enabled}");
                println!("command={}", plugin.command.display());
                println!("root={}", plugin.root.display());
                if let Ok(r) = InstallReceipt::load(&plugin.root) {
                    println!("coordinate={}", r.coordinate);
                    println!("version={}", r.version);
                    println!("artifact_sha256={}", r.archive_sha256);
                }
                if let Some(pending) = pending.as_ref() {
                    for line in pending.summary_lines() {
                        println!("{line}");
                    }
                }
                for instance in &instances {
                    print_instance_line(instance);
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
                vec![bookclerk_plugin_host::resolve_plugin_ref(&plugins, &id)
                    .map_err(|err| anyhow::anyhow!("{err}"))?
                    .clone()]
            } else {
                let host = crate::registry::open_host(config).await?;
                let mut enabled = Vec::new();
                for p in plugins {
                    let instances = if is_bootstrap_only(&p) {
                        Vec::new()
                    } else {
                        instance_items(
                            &host.store,
                            host.node_id(),
                            Some(p.plugin_key().canonical()),
                        )
                        .await?
                    };
                    if deployed_here(config, &p, &instances) {
                        enabled.push(p);
                    }
                }
                enabled
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
        PluginsCommand::Deploy {
            id,
            node,
            replicas,
            on,
            manual,
        } => {
            if replicas.is_some() || manual {
                run_deploy(config, &id, replicas, on, manual, format).await
            } else {
                set_plugin_deployed(config, &id, node, true, format).await
            }
        }
        PluginsCommand::Undeploy { id, node } => {
            set_plugin_deployed(config, &id, node, false, format).await
        }
        PluginsCommand::Instances { command } => run_instances(config, command, format).await,
        PluginsCommand::Config { command } => run_plugin_config(config, command, format).await,
        PluginsCommand::Approve { id, yes, reset } => {
            if find_plugin(config, &id).is_err()
                && crate::commands::plugin_installs::run_approve(config, &id, yes, format).await?
            {
                return Ok(());
            }
            run_approve(config, &id, Approval { yes, reset }, format).await
        }
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
                    "node_target": host_bookclerk_target(),
                    "rust_triple": host_target_triple(),
                    "prefix": CRATE_NAME_PREFIX,
                    "plugins": legacy,
                }),
                || {
                    if legacy.is_empty() {
                        println!(
                            "no crates.io plugins matching `{CRATE_NAME_PREFIX}*` yet \
                             (see docs/plugins/registry.md)"
                        );
                        return;
                    }
                    for h in &legacy {
                        let family = h.parsed.as_ref().map(|p| p.family.as_str()).unwrap_or("?");
                        let id = h.parsed.as_ref().map(|p| p.id.as_str()).unwrap_or("?");
                        println!(
                            "cargo:{}@{}  family={family} id={id} downloads={}",
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
            "node_target": host_bookclerk_target(),
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

/// Updates installed receipts to a newer (or pinned) version, restoring the previous tree on health failure.
async fn run_update(
    config: &Config,
    id: Option<String>,
    to: Option<String>,
    allow_unverified_publisher: bool,
    approve_capabilities: bool,
    dry_run: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plugins_root = config.paths().files_dir.join("plugins");
    let mutation_lock = PluginMutationLock::acquire(&config.paths().files_dir)?;
    let plugins = bookclerk_plugin_host::discover_plugins(config)?;
    let targets: Vec<_> = match id.as_deref() {
        Some(want) => vec![bookclerk_plugin_host::resolve_plugin_ref(&plugins, want)
            .map_err(|err| anyhow::anyhow!("{err}"))?
            .clone()],
        None => plugins,
    };
    let targets: Vec<_> = targets
        .into_iter()
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
            // Same PluginKey at the same alias; replace updates files but cannot
            // rename aliases or seize a foreign PluginKey's alias.
            replace: true,
            offline: false,
            trust: TrustPolicy {
                allow_unverified_publisher: allow_unverified_publisher
                    || config.plugins.allow_unverified_publisher,
                ..TrustPolicy::default()
            },
            skip_health: false,
            approve_capabilities,
        };
        let outcome = install_from_manifest_with_configured_aliases(
            config,
            &mutation_lock,
            &manifest,
            &coord,
            &opts,
        )?;
        if !outcome.dry_run {
            if let Err(err) = health_check_installed(config, plugin.plugin_key().canonical()).await
            {
                return Err(health_failure_after_rollback(
                    err,
                    Installer::rollback(&outcome),
                    "previous version restored",
                ));
            }
            commit_install_cleanup(&outcome)?;
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

/// Deletes an installed plugin directory, optionally purging `plugin-state/`.
fn run_remove(
    config: &Config,
    id: &str,
    purge_state: bool,
    discard_acl_journals: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if discard_acl_journals && !purge_state {
        anyhow::bail!("--discard-acl-journals requires --purge-state");
    }
    let plugins_root = config.paths().files_dir.join("plugins");
    let mutation_lock = PluginMutationLock::acquire(&config.paths().files_dir)?;
    if purge_state {
        Installer::remove_with_lock_preparing_purge(
            &mutation_lock,
            &plugins_root,
            id,
            true,
            |state| {
                bookclerk_plugin_host::revoke_plugin_state_before_purge(state, discard_acl_journals)
            },
        )?;
    } else {
        Installer::remove_with_lock(&mutation_lock, &plugins_root, id, false)?;
    }
    let payload = json!({ "id": id, "purge_state": purge_state });
    emit(format, &payload, || {
        println!(
            "uninstalled staged plugin `{id}`{}",
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
    let targets: Vec<_> = match id.as_deref() {
        Some(want) => vec![bookclerk_plugin_host::resolve_plugin_ref(&plugins, want)
            .map_err(|err| anyhow::anyhow!("{err}"))?
            .clone()],
        None => plugins,
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
        match health_check_installed(config, plugin.plugin_key().canonical()).await {
            Ok(msg) => lines.push(msg),
            Err(err) => lines.push(format!("health=FAIL {err:#}")),
        }
        reports.push(json!({
            "id": plugin.manifest.id,
            "plugin_key": plugin.plugin_key().canonical(),
            "lines": lines
        }));
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
    /// Owning plugin PluginKey.
    plugin_id: String,
    /// Binding name from `plugin.toml` `capabilities.bindings.databases`.
    binding: String,
    /// Adapter PluginKey that provisioned the unit.
    adapter_plugin_key: String,
    /// Diagnostic adapter family (`sqlite`, `postgres`, `d1`).
    backend_kind: String,
    /// Backend-native unit: file path, Postgres database name, or D1 database name.
    unit_ref: String,
    /// RFC 3339 provisioning time.
    created_at: String,
}

/// Registry rows for `spec`: canonical PluginKey, or an alias unique within
/// this node plugin namespace (`$FILES_DIR`).
///
/// A parseable PluginKey is an exact registry lookup. A bare alias resolves
/// through discovery to exactly one PluginKey. Leftover alias-era rows keyed
/// by the bare alias are listed only when nothing is installed under that
/// alias.
async fn plugin_db_rows_for_spec(
    store: &bookclerk_library::LibraryStore,
    config: &Config,
    spec: Option<&str>,
) -> anyhow::Result<Vec<bookclerk_library::PluginDatabaseRecord>> {
    match spec.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(store.list_plugin_databases(None).await?),
        Some(spec) if bookclerk_plugin_catalog::PluginKey::parse(spec).is_ok() => {
            Ok(store.list_plugin_databases(Some(spec)).await?)
        }
        Some(alias) => {
            let plugins = bookclerk_plugin_host::discover_plugins(config)?;
            match bookclerk_plugin_host::resolve_plugin_ref(&plugins, alias) {
                Ok(plugin) => Ok(store
                    .list_plugin_databases(Some(plugin.plugin_key().canonical()))
                    .await?),
                Err(_) => {
                    let all = store.list_plugin_databases(None).await?;
                    Ok(all
                        .into_iter()
                        .filter(|row| row.plugin_id.eq_ignore_ascii_case(alias))
                        .collect())
                }
            }
        }
    }
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
            let rows = plugin_db_rows_for_spec(&store, config, plugin.as_deref()).await?;
            let items: Vec<PluginDbListItem> = rows
                .into_iter()
                .map(|r| PluginDbListItem {
                    plugin_id: r.plugin_id,
                    binding: r.binding,
                    adapter_plugin_key: r.adapter_plugin_key,
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
                        "{}/{}: adapter={} backend={} unit={} created={}",
                        item.plugin_id,
                        item.binding,
                        item.adapter_plugin_key,
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
            let rows = plugin_db_rows_for_spec(&store, config, Some(&plugin)).await?;
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
                    &row.adapter_plugin_key,
                    &row.unit_ref,
                )
                .await
                .map_err(|err| anyhow::anyhow!(err.to_string()))?;
                store
                    .remove_plugin_databases(&row.plugin_id, Some(&row.binding))
                    .await?;
                println!(
                    "deleted {}/{} (adapter {} backend {} {})",
                    row.plugin_id,
                    row.binding,
                    row.adapter_plugin_key,
                    row.backend_kind,
                    row.unit_ref
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
pub(crate) fn resolve_coordinate(raw: &str) -> anyhow::Result<PackageCoordinate> {
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
    let families = plugin.manifest.families();
    if families.contains(&PluginFamily::Source) || families.contains(&PluginFamily::Integration) {
        HOST_SHARED_ACCOUNT
    } else {
        OPERATOR_ACCOUNT
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
    let entrypoints = session
        .describe_snapshot()
        .capabilities
        .entrypoints
        .iter()
        .map(|e| e.wire_name())
        .collect::<Vec<_>>()
        .join(",");
    // `health` is a lifecycle method on every family that exposes one; a
    // storage-only plugin has no health RPC, so report describe() only.
    let mut probed = false;
    if session.has_entrypoint(Entrypoint::Storefront) {
        let _ = session
            .storefront(|src| async move { src.health().await })
            .await?;
        probed = true;
    }
    if session.has_entrypoint(Entrypoint::RemoteLibrary) {
        let _ = session
            .remote_library(|lib| async move { lib.health().await })
            .await?;
        probed = true;
    }
    if session.has_entrypoint(Entrypoint::Metadata) {
        let health = session
            .metadata(|provider| async move { provider.health().await })
            .await?;
        if !health.ok {
            anyhow::bail!("metadata health failed: {}", health.detail);
        }
        probed = true;
    }
    if session.has_entrypoint(Entrypoint::Covers) {
        let health = session
            .covers(|fetcher| async move { fetcher.health().await })
            .await?;
        if !health.ok {
            anyhow::bail!("covers health failed: {}", health.detail);
        }
        probed = true;
    }
    if session.has_entrypoint(Entrypoint::MetadataFiles) {
        let health = session
            .metadata_files(|files| async move { files.health().await })
            .await?;
        if !health.ok {
            anyhow::bail!("metadataFiles health failed: {}", health.detail);
        }
        probed = true;
    }
    if session.has_entrypoint(Entrypoint::Notifier) {
        let health = session
            .notifier(|notifier| async move { notifier.health().await })
            .await?;
        if !health.ok {
            anyhow::bail!("notifier health failed: {}", health.detail);
        }
        probed = true;
    }
    if session.has_entrypoint(Entrypoint::Ai) {
        let health = session
            .ai(|models| async move { models.health().await })
            .await?;
        if !health.ok {
            anyhow::bail!("ai health failed: {}", health.detail);
        }
        probed = true;
    }
    if session.has_entrypoint(Entrypoint::DatabaseAdapter) {
        probe_database(&session, config, &plugin).await?;
        probed = true;
    }
    if probed {
        Ok(format!("health=ok api={api} entrypoints={entrypoints}"))
    } else {
        Ok(format!("api={api} entrypoints={entrypoints}"))
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
        let host = crate::registry::open_host(config).await?;
        let instances = if is_bootstrap_only(&plugin) {
            Vec::new()
        } else {
            instance_items(
                &host.store,
                host.node_id(),
                Some(plugin.plugin_key().canonical()),
            )
            .await?
        };
        if !deployed_here(config, &plugin, &instances) {
            anyhow::bail!(
                "plugin `{}` is not deployed on this node; run `bookclerk plugins deploy {}`",
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
    let result: CliInvokeResult = session.cli_invoke(params).await?;

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
        if result.stdout.is_empty() && !result.payload.is_empty() {
            match result.payload.json_value() {
                Ok(json) => println!("{}", serde_json::to_string_pretty(&json)?),
                Err(_) => println!(
                    "[{} payload, {} bytes]",
                    result.payload.media_type,
                    result.payload.payload.len()
                ),
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

/// Prefers live `cli.describe`, then the describe-embedded CLI schema, then the on-disk manifest schema.
async fn resolve_schema(
    session: &PluginSession,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<CliSchema> {
    if session.has_entrypoint(Entrypoint::Cli) {
        return Ok(session.cli_describe().await?);
    }
    let described = session.describe_snapshot().cli.clone();
    if !described.commands.is_empty() {
        return Ok(described);
    }
    Ok(plugin.manifest.cli.clone().unwrap_or_default())
}

/// Opens the database guest and issues `SELECT 1` as the health/diagnose probe.
async fn probe_database(
    session: &PluginSession,
    config: &Config,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<()> {
    let ctx = bookclerk_plugin_host::database_connect_bindings(config, plugin, session)?;
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
    let lines: anyhow::Result<Vec<String>> = if session.has_entrypoint(Entrypoint::Storefront) {
        session
            .storefront(|src| async move { src.diagnose().await })
            .await
            .map_err(anyhow::Error::from)
    } else if session.has_entrypoint(Entrypoint::RemoteLibrary) {
        session
            .remote_library(|lib| async move { lib.diagnose().await })
            .await
            .map_err(anyhow::Error::from)
    } else if session.has_entrypoint(Entrypoint::DatabaseAdapter) {
        probe_database(&session, config, plugin)
            .await
            .map(|()| vec!["ping=ok".to_string()])
    } else {
        return Ok(vec![format!(
            "plugin `{}` exports no entrypoint with a diagnose RPC",
            plugin.manifest.id
        )]);
    };
    match lines {
        Ok(lines) => Ok(lines),
        Err(err) => Ok(vec![format!("diagnose failed: {err:#}")]),
    }
}

/// How `plugins approve` asks and what it keeps.
#[derive(Debug, Clone, Copy)]
struct Approval {
    /// Approve without the interactive prompt.
    yes: bool,
    /// Store the package's request as is instead of keeping the operator's
    /// earlier network answers.
    reset: bool,
}

/// The grant `plugins approve` stores for `request`.
///
/// With a stored grant and no `--reset`, the request is re-approved on top
/// of it so the operator's network additions and denials, redirect answer
/// and refused own `User-Agent` hosts survive
/// ([`bookclerk_plugin_host::reapprove_grant`]). Otherwise the request
/// itself, as a first approval stores it.
///
/// # Errors
///
/// Returns an error when the re-approved draft fails validation.
fn approval_grant(
    stored: Option<&PluginGrant>,
    request: &PluginGrant,
    reset: bool,
) -> anyhow::Result<PluginGrant> {
    match stored {
        Some(stored) if !reset => Ok(bookclerk_plugin_host::reapprove_grant(stored, request)?),
        _ => Ok(request.clone()),
    }
}

/// Lines naming the operator's earlier network changes a re-approval
/// keeps, empty when there are none.
fn kept_operator_lines(grant: &PluginGrant) -> Vec<String> {
    let tcp = |grants: &BTreeSet<bookclerk_plugin_host::TcpGrant>| -> Vec<String> {
        grants
            .iter()
            .map(|t| {
                let ports: Vec<String> = t.ports.iter().map(ToString::to_string).collect();
                format!("{}:{}", t.host, ports.join(","))
            })
            .collect()
    };
    let mut added: Vec<String> = grant.operator_added_domains.iter().cloned().collect();
    added.extend(tcp(&grant.operator_added_tcp));
    added.extend(grant.operator_added_cidrs.iter().cloned());
    let mut denied: Vec<String> = grant.operator_denied_domains.iter().cloned().collect();
    denied.extend(tcp(&grant.operator_denied_tcp));
    denied.extend(grant.operator_denied_cidrs.iter().cloned());
    let mut lines = Vec::new();
    if !added.is_empty() {
        lines.push(format!(
            "Keeping destinations you added: {}",
            added.join(", ")
        ));
    }
    if !denied.is_empty() {
        lines.push(format!(
            "Keeping destinations you denied: {}",
            denied.join(", ")
        ));
    }
    if !lines.is_empty() {
        lines.push("Pass --reset to approve the package's request as is.".into());
    }
    lines
}

/// `plugins approve` for a plugin found on disk: stores its consent grant.
///
/// A re-approval keeps the operator's earlier network answers unless
/// `approval.reset` is set (see [`approval_grant`]). The grant also approves
/// every destination the plugin's network settings name now (listed under
/// "Destinations from settings"), and withdraws any approved destination no
/// setting names any more.
///
/// # Errors
///
/// Returns an error when the plugin is unknown, approval is declined, the
/// re-approved grant is invalid, the library cannot be read, or the grants
/// file cannot be written.
async fn run_approve(
    config: &Config,
    id: &str,
    approval: Approval,
    format: OutputFormat,
) -> anyhow::Result<()> {
    use std::io::{self, IsTerminal, Write};

    let plugin = find_plugin(config, id)?;
    let request = consent_request(&plugin.manifest, plugin.plugin_key());
    let files_dir = &config.paths().files_dir;
    let store = PluginGrantStore::load(files_dir)?;
    let stored = store.get_by_plugin_key(plugin.plugin_key().canonical());
    // What this version asks for beyond the stored grant: the same diff
    // `plugins info` and the web consent dialog show.
    let pending = stored
        .map(|stored| bookclerk_plugin_host::pending_structural(stored, &request).summary_lines())
        .unwrap_or_default();
    let preview = approval_grant(stored, &request, approval.reset)?;
    let summary = consent_summary(&preview);
    let kept = kept_operator_lines(&preview);
    let setting_rows = if bookclerk_plugin_host::network_settings(&plugin.manifest)
        .next()
        .is_some()
    {
        let library = crate::registry::open_library(config).await?;
        bookclerk_plugin_host::library_setting_statuses(
            &library,
            &plugin.manifest,
            plugin.plugin_key().canonical(),
            stored,
        )
        .await?
    } else {
        Vec::new()
    };
    let setting_lines = setting_destination_lines(&setting_rows);

    // With `--format json` stdout carries only the JSON document (which has
    // both lists), so the review copy goes to stderr beside the prompt.
    for line in pending
        .iter()
        .chain(&summary)
        .chain(&kept)
        .chain(&setting_lines)
    {
        match format {
            OutputFormat::Text => println!("{line}"),
            OutputFormat::Json => eprintln!("{line}"),
        }
    }

    if !approval.yes {
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

    // Reload after the prompt: another writer may have changed the store
    // while the operator was reading, and saving the early copy would
    // drop that change. The grant is rebuilt on what is stored now.
    let mut store = PluginGrantStore::load(files_dir)?;
    let mut grant = approval_grant(
        store.get_by_plugin_key(plugin.plugin_key().canonical()),
        &request,
        approval.reset,
    )?;
    let in_use = bookclerk_plugin_host::destinations_in_use(&setting_rows);
    let approved = store
        .get_by_plugin_key(plugin.plugin_key().canonical())
        .map(|stored| stored.setting_destinations.clone())
        .unwrap_or_default();
    grant.setting_destinations = bookclerk_plugin_host::approve_setting_destinations(
        &approved,
        &plugin.manifest,
        &in_use,
        &in_use,
    );
    store.upsert(grant.clone());
    store.save(files_dir)?;
    // Describe what was stored, which may differ from the preview when the
    // grant changed while the operator read it.
    let summary = consent_summary(&grant);
    let kept = kept_operator_lines(&grant);

    let payload = json!({
        "id": plugin.manifest.id,
        "approved": true,
        "grant": grant,
        "summary": summary,
        "pending": pending,
        "kept": kept,
        "setting_destinations": setting_lines,
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

/// The review lines for the destinations a plugin's network settings name:
/// a heading, then one line per instance and setting with its source label
/// and whether it may reach the local network.
///
/// # Returns
///
/// No lines when the plugin declares no network setting.
fn setting_destination_lines(
    rows: &[bookclerk_plugin_host::InstanceSettingStatuses],
) -> Vec<String> {
    use bookclerk_plugin_host::DestinationScope;
    use bookclerk_plugin_host::SettingGrantState;

    let mut lines = Vec::new();
    for row in rows {
        for status in &row.statuses {
            let instance = row
                .instance_id
                .as_deref()
                .map(|id| format!(" (instance {id})"))
                .unwrap_or_default();
            let Some(destination) = &status.destination else {
                let why = match (&status.state, &status.error) {
                    (SettingGrantState::Invalid, Some(err)) => err.to_string(),
                    _ => "not set".to_string(),
                };
                lines.push(format!("  {}{instance}: {why}", status.label));
                continue;
            };
            let scope = match destination.scope {
                DestinationScope::Name => "may resolve to your local network, for this name only",
                DestinationScope::PublicAddress => "public address",
                DestinationScope::LocalAddress => "local network, this address only",
                DestinationScope::Restricted => {
                    "loopback or special address, also needs an address CIDR"
                }
            };
            let encryption = if destination.tls {
                ""
            } else {
                ", not encrypted"
            };
            let state = match status.state {
                SettingGrantState::Approved => "approved",
                SettingGrantState::Pending => "new",
                SettingGrantState::Unset | SettingGrantState::Invalid => "",
            };
            lines.push(format!(
                "  {}: {} from {}{instance} ({scope}{encryption}) [{state}]",
                status.network.as_str(),
                destination.display(),
                status.label,
            ));
        }
    }
    if !lines.is_empty() {
        lines.insert(0, "Destinations from settings:".to_string());
    }
    lines
}

/// Operator actor recorded on control-plane writes from the CLI.
fn cli_actor() -> ConfigActor {
    ConfigActor::Operator { id: "cli".into() }
}

/// Unique operation id for a CLI control-plane write.
fn operation_id(kind: &str) -> String {
    format!(
        "cli-{kind}-{}",
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| chrono::Utc::now().timestamp_micros())
    )
}

/// Enables or disables a plugin.
///
/// Non-database plugins: the resolved instance's deployment on `node` is set
/// `present` (after a grant check when the plugin is installed here) or
/// `absent`; the daemon's reconciler spawns or retires the guest. Database
/// adapters keep selecting `[database].plugin` in `config.toml`.
async fn set_plugin_deployed(
    config: &Config,
    needle: &str,
    node: Option<String>,
    enabled: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let local = find_plugin(config, needle).ok();
    if let Some(plugin) = local.as_ref().filter(|p| is_bootstrap_only(p)) {
        return set_database_plugin_deployed(config, plugin, enabled, format);
    }
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let instance = resolve_instance(store, needle).await?;
    bookclerk_library::control_plane::ensure_hand_managed(store, &instance.id).await?;
    let installed = plane
        .enrolled
        .iter()
        .find(|e| e.plugin.plugin_key().canonical() == instance.plugin_key)
        .map(|e| e.plugin.clone());
    let node_id = node.unwrap_or_else(|| plane.node_id().to_string());
    let active =
        bookclerk_library::control_plane::load_active_version(store, &instance.plugin_key).await?;
    if enabled {
        if let Some(version) = &active {
            // An installed plugin runs on any node from its digest; consent
            // is the grant recorded on the version.
            bookclerk_plugin_host::version_grant(version)
                .map_err(|err| anyhow::anyhow!("{err}"))?;
            if node_id != plane.node_id() && !list_node_ids(store).await?.contains(&node_id) {
                anyhow::bail!("node `{node_id}` is not registered with this library");
            }
        } else if node_id == plane.node_id() {
            let plugin = installed.ok_or_else(|| {
                anyhow::anyhow!(
                    "plugin `{}` is not installed on this node; install it or pass --node",
                    instance.plugin_key
                )
            })?;
            require_grant(&config.paths().files_dir, &plugin).map_err(|err| {
                anyhow::anyhow!(
                    "{err}\nRun `bookclerk plugins approve {}` first.",
                    plugin.manifest.id
                )
            })?;
        } else if !list_node_ids(store).await?.contains(&node_id) {
            anyhow::bail!("node `{node_id}` is not registered with this library");
        }
    }
    let desired = bookclerk_library::control_plane::DesiredState::operator(enabled);
    let row = match load_deployment_for_node(store, &instance.id, &node_id).await? {
        None => {
            ensure_plugin_deployment(store, &cli_actor(), &instance.id, &node_id, desired).await?
        }
        Some(current) if current.desired == desired => current,
        Some(current) => match replace_plugin_deployment(
            store,
            &cli_actor(),
            &current.deployment_id,
            current.revision,
            desired,
            &operation_id("deployment"),
        )
        .await?
        {
            DeploymentReplace::Applied(row) => row,
            DeploymentReplace::Replayed { .. } => current,
            DeploymentReplace::Conflict { current_revision } => anyhow::bail!(
                "deployment changed concurrently (revision {current_revision}); retry"
            ),
        },
    };
    let payload = json!({
        "plugin_key": instance.plugin_key,
        "plugin_instance_id": instance.id.to_string(),
        "node_id": row.node_id,
        "desired": row.desired.as_str(),
        "revision": row.revision,
        "deployed": enabled,
    });
    emit(format, &payload, || {
        println!(
            "{} {} instance {} on node {} (deployment {})",
            if enabled { "deployed" } else { "undeployed" },
            instance.plugin_key,
            instance.id,
            row.node_id,
            row.desired
        );
        if node_id == plane.node_id() {
            println!("a running bookclerkd applies the change on its next reconcile pass");
        }
    })
}

/// Selects or refuses to drop the bootstrap database adapter in `config.toml`.
fn set_database_plugin_deployed(
    config: &Config,
    plugin: &DiscoveredPlugin,
    enabled: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if !enabled {
        if occupies_database_slot(plugin, &config.database.plugin) {
            anyhow::bail!(
                "cannot undeploy the active database plugin `{}`; \
                 deploy another backend first with `bookclerk plugins deploy <id>`",
                plugin.manifest.id
            );
        }
        anyhow::bail!(
            "database plugin `{}` is not active ([database].plugin = `{}`)",
            plugin.manifest.id,
            config.database.plugin
        );
    }
    require_grant(&config.paths().files_dir, plugin).map_err(|err| {
        anyhow::anyhow!(
            "{err}\nRun `bookclerk plugins approve {}` first.",
            plugin.manifest.id
        )
    })?;
    let mut cfg = config.clone();
    bookclerk_plugin_host::stamp_occupancy_plugin_key(&mut cfg, plugin)
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let path = cfg.paths().config_file.clone();
    cfg.write_toml_file(&path)?;
    let payload = json!({
        "id": plugin.manifest.id,
        "deployed": true,
        "config": path.display().to_string(),
    });
    emit(format, &payload, || {
        println!(
            "deployed database plugin {} (wrote {})",
            plugin.manifest.id,
            path.display()
        );
    })
}

/// Resolves an instance id, PluginKey, or alias to one instance.
///
/// An id matches exactly. A key or alias (matched against the pinned
/// schemas, so a plugin need not be installed here) names the plugin's
/// default instance.
async fn resolve_instance(store: &LibraryStore, needle: &str) -> anyhow::Result<PluginInstance> {
    if let Ok(id) = PluginInstanceId::parse(needle) {
        if let Some(instance) = load_plugin_instance(store, &id).await? {
            return Ok(instance);
        }
        anyhow::bail!("plugin instance `{needle}` does not exist");
    }
    let key = resolve_plugin_key(store, needle).await?;
    default_instance_for_key(store, &key).await?.ok_or_else(|| {
        anyhow::anyhow!(
            "plugin `{key}` has no instance yet; run a node that has it installed \
                 (enrollment creates the default instance)"
        )
    })
}

/// Resolves a PluginKey or alias against the pinned schemas.
async fn resolve_plugin_key(store: &LibraryStore, needle: &str) -> anyhow::Result<String> {
    let schemas = list_plugin_schemas(store).await?;
    if let Some(exact) = schemas.iter().find(|s| s.plugin_key == needle) {
        return Ok(exact.plugin_key.clone());
    }
    let by_alias: Vec<_> = schemas
        .iter()
        .filter(|s| s.manifest_id.eq_ignore_ascii_case(needle))
        .collect();
    match by_alias.as_slice() {
        [one] => Ok(one.plugin_key.clone()),
        [] => {
            anyhow::bail!("no plugin named `{needle}` is enrolled (see `bookclerk plugins list`)")
        }
        many => anyhow::bail!(
            "alias `{needle}` names {} plugins; pass a PluginKey: {}",
            many.len(),
            many.iter()
                .map(|s| s.plugin_key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Instances (optionally of one plugin key) with their deployments.
async fn instance_items(
    store: &LibraryStore,
    this_node: &str,
    plugin_key: Option<&str>,
) -> anyhow::Result<Vec<InstanceListItem>> {
    let aliases: std::collections::HashMap<String, String> = list_plugin_schemas(store)
        .await?
        .into_iter()
        .map(|s| (s.plugin_key, s.manifest_id))
        .collect();
    let mut defaults: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut items = Vec::new();
    for instance in list_all_plugin_instances(store).await? {
        if plugin_key.is_some_and(|key| key != instance.plugin_key) {
            continue;
        }
        if !defaults.contains_key(&instance.plugin_key) {
            if let Some(default) = default_instance_for_key(store, &instance.plugin_key).await? {
                defaults.insert(instance.plugin_key.clone(), default.id.to_string());
            }
        }
        let mut deployments = Vec::new();
        for deployment in list_deployments_for_instance(store, &instance.id).await? {
            let observation =
                load_observation(store, &deployment.deployment_id, &deployment.node_id).await?;
            deployments.push(DeploymentListItem {
                this_node: deployment.node_id == this_node,
                node_id: deployment.node_id,
                desired: deployment.desired.as_str().to_string(),
                revision: deployment.revision,
                status: observation.as_ref().map(|o| o.status.as_str().to_string()),
                detail: observation
                    .map(|o| o.detail)
                    .filter(|detail| !detail.is_empty()),
            });
        }
        items.push(InstanceListItem {
            plugin_instance_id: instance.id.to_string(),
            alias: aliases
                .get(&instance.plugin_key)
                .cloned()
                .unwrap_or_default(),
            is_default: defaults
                .get(&instance.plugin_key)
                .is_some_and(|id| *id == instance.id.as_str()),
            placement: bookclerk_library::control_plane::load_plugin_placement(store, &instance.id)
                .await?
                .map(|p| PlacementListItem {
                    replicas: p.spec.replicas,
                    constraints: p.spec.constraints,
                    revision: p.revision,
                    placed: p.placed,
                    detail: p.detail,
                }),
            plugin_key: instance.plugin_key,
            display_name: instance.display_name,
            deployments,
        });
    }
    Ok(items)
}

/// True when the plugin is deployed on this node: a bootstrap database
/// adapter occupies `[database].plugin`; any other plugin has its default
/// instance's deployment desired present here.
fn deployed_here(
    config: &Config,
    plugin: &DiscoveredPlugin,
    instances: &[InstanceListItem],
) -> bool {
    if is_bootstrap_only(plugin) {
        return occupies_database_slot(plugin, &config.database.plugin);
    }
    instances.iter().any(|instance| {
        instance.is_default
            && instance
                .deployments
                .iter()
                .any(|d| d.this_node && (d.desired == "present" || d.desired == "placed"))
    })
}

/// Prints one instance and its deployments as indented text lines.
fn print_instance_line(instance: &InstanceListItem) {
    println!(
        "  instance {}{}{}",
        instance.plugin_instance_id,
        if instance.is_default {
            " (default)"
        } else {
            ""
        },
        if instance.display_name.is_empty() {
            String::new()
        } else {
            format!(" name={}", instance.display_name)
        }
    );
    if let Some(p) = &instance.placement {
        println!(
            "    placed by the scheduler: {}/{} replica(s){}{}",
            p.placed,
            p.replicas,
            if p.constraints.is_empty() {
                String::new()
            } else {
                format!(" on {}", p.constraints.join(", "))
            },
            if p.detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", p.detail)
            }
        );
    }
    if instance.deployments.is_empty() {
        println!("    no deployments");
    }
    for d in &instance.deployments {
        println!(
            "    node {}{} desired={} status={}{}",
            d.node_id,
            if d.this_node { " (this node)" } else { "" },
            d.desired,
            d.status.as_deref().unwrap_or("-"),
            d.detail
                .as_deref()
                .map(|detail| format!(" detail={detail}"))
                .unwrap_or_default()
        );
    }
}

/// `plugins deploy`: write or drop an instance's scheduler placement.
async fn run_deploy(
    config: &Config,
    needle: &str,
    replicas: Option<bookclerk_library::control_plane::Replicas>,
    on: Vec<String>,
    manual: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    use bookclerk_library::control_plane::{
        delete_plugin_placement, effective_node_labels, labels_satisfy, list_nodes, live_node_ids,
        load_plugin_placement, put_plugin_placement, PlacementSpec, PlacementWrite,
    };
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    let instance = resolve_instance(store, needle).await?;
    if manual {
        let deleted = delete_plugin_placement(store, &cli_actor(), &instance.id).await?;
        let payload = json!({
            "plugin_key": instance.plugin_key,
            "plugin_instance_id": instance.id.to_string(),
            "placement": null,
            "deleted": deleted,
        });
        return emit(format, &payload, || {
            if deleted {
                println!(
                    "instance {} is hand-managed again; its deployments are unchanged \
                     (use `plugins deploy|undeploy`)",
                    instance.id
                );
            } else {
                println!("instance {} had no placement", instance.id);
            }
        });
    }
    let replicas = replicas.ok_or_else(|| anyhow::anyhow!("--replicas is required"))?;
    let current = load_plugin_placement(store, &instance.id).await?;
    let spec = PlacementSpec {
        replicas,
        constraints: on,
    };
    let placement = match put_plugin_placement(
        store,
        &cli_actor(),
        &instance.id,
        &spec,
        current.as_ref().map(|p| p.revision),
    )
    .await?
    {
        PlacementWrite::Applied(placement) => placement,
        PlacementWrite::Conflict { current_revision } => {
            anyhow::bail!("placement changed concurrently (revision {current_revision}); retry")
        }
    };
    let live = live_node_ids(store).await?;
    let eligible: Vec<String> = list_nodes(store)
        .await?
        .iter()
        .filter(|host| {
            live.contains(&host.node_id)
                && labels_satisfy(&effective_node_labels(host), &placement.spec.constraints)
        })
        .map(|host| host.node_id.clone())
        .collect();
    let payload = json!({
        "plugin_key": instance.plugin_key,
        "plugin_instance_id": instance.id.to_string(),
        "placement": {
            "replicas": placement.spec.replicas,
            "constraints": placement.spec.constraints,
            "revision": placement.revision,
        },
        "eligible_nodes": eligible,
    });
    emit(format, &payload, || {
        println!(
            "placed {} instance {}: {} replica(s){} (revision {}); the scheduler applies it on its next pass",
            instance.plugin_key,
            instance.id,
            placement.spec.replicas,
            if placement.spec.constraints.is_empty() {
                String::new()
            } else {
                format!(" on {}", placement.spec.constraints.join(", "))
            },
            placement.revision
        );
        let wanted = match placement.spec.replicas {
            bookclerk_library::control_plane::Replicas::Count(n) => {
                usize::try_from(n).unwrap_or(usize::MAX)
            }
            bookclerk_library::control_plane::Replicas::All => 1,
        };
        if eligible.len() < wanted {
            println!(
                "warning: {} live node(s) satisfy the constraints now; label more with `bookclerk nodes label`",
                eligible.len()
            );
        }
    })
}

/// `plugins instances` actions.
async fn run_instances(
    config: &Config,
    command: InstancesCommand,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    match command {
        InstancesCommand::List { plugin } => {
            let key = match plugin.as_deref() {
                Some(needle) => Some(resolve_plugin_key(store, needle).await?),
                None => None,
            };
            let items = instance_items(store, plane.node_id(), key.as_deref()).await?;
            emit(
                format,
                &json!({ "node_id": plane.node_id(), "instances": items }),
                || {
                    if items.is_empty() {
                        println!("no plugin instances");
                    }
                    for item in &items {
                        println!("{} ({})", item.plugin_key, item.alias);
                        print_instance_line(item);
                    }
                },
            )
        }
        InstancesCommand::Create { plugin, name, node } => {
            let key = resolve_plugin_key(store, &plugin).await?;
            let pinned = require_plugin_schema(store, &key).await?;
            let name = name.as_deref().unwrap_or("").trim();
            let instance = match node.as_deref().map(str::trim) {
                Some(node) => {
                    create_node_local_instance(store, &cli_actor(), &key, node, name).await?
                }
                None => create_plugin_instance(store, &cli_actor(), &key, name).await?,
            };
            import_instance_config_if_absent(
                store,
                &cli_actor(),
                &instance.id,
                &pinned.schema,
                &PluginInstanceConfigV1::empty(),
                &format!("seed-instance-config-{}", instance.id),
            )
            .await?;
            let payload = json!({
                "plugin_instance_id": instance.id.to_string(),
                "plugin_key": instance.plugin_key,
                "display_name": instance.display_name,
                "owner_node_id": instance.owner_node_id,
            });
            emit(format, &payload, || {
                println!(
                    "created instance {} of {} (deploy it with `bookclerk plugins deploy {}`)",
                    instance.id, instance.plugin_key, instance.id
                );
            })
        }
        InstancesCommand::Filter { command } => run_filter(store, command, format).await,
        InstancesCommand::Delete { instance } => {
            let id = PluginInstanceId::parse(&instance)?;
            delete_plugin_instance(store, &cli_actor(), &id).await?;
            emit(
                format,
                &json!({ "plugin_instance_id": id.to_string(), "deleted": true }),
                || {
                    println!("deleted instance {id}");
                },
            )
        }
    }
}

/// Reads one `FIELD=VALUE` filter rule from the command line.
///
/// # Errors
///
/// Returns an error when the argument has no `=` or names an unknown field.
fn filter_rule(
    arg: &str,
    effect: bookclerk_config::FilterEffect,
) -> anyhow::Result<bookclerk_config::FilterRule> {
    use bookclerk_config::FilterField;
    let (field, value) = arg
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("a filter rule is FIELD=VALUE, not `{arg}`"))?;
    let field = FilterField::parse(field.trim()).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown filter field `{}`; use patron, storefront, account, media_kind, \
             format or quality",
            field.trim()
        )
    })?;
    Ok(bookclerk_config::FilterRule {
        field,
        value: value.to_string(),
        effect,
    })
}

/// Prints a destination's filter, one rule per line.
fn print_filter(instance: &PluginInstanceId, filter: &bookclerk_config::DestinationFilter) {
    if filter.takes_everything() {
        println!("destination {instance} takes every book");
        return;
    }
    println!("destination {instance} filter:");
    for rule in &filter.rules {
        let effect = match rule.effect {
            bookclerk_config::FilterEffect::Include => "include",
            bookclerk_config::FilterEffect::Exclude => "exclude",
        };
        println!("  {effect} {}={}", rule.field.as_str(), rule.value);
    }
}

/// `plugins instances filter` actions.
async fn run_filter(
    store: &LibraryStore,
    command: FilterCommand,
    format: OutputFormat,
) -> anyhow::Result<()> {
    use bookclerk_config::{DestinationFilter, FilterEffect};
    use bookclerk_library::control_plane::{destination_filter, set_destination_filter};
    let (instance, filter) = match command {
        FilterCommand::Get { instance } => {
            let instance = resolve_instance(store, &instance).await?;
            let filter = destination_filter(store, instance.id.as_str()).await?;
            (instance, filter)
        }
        FilterCommand::Set {
            instance,
            include,
            exclude,
        } => {
            let instance = resolve_instance(store, &instance).await?;
            let rules = include
                .iter()
                .map(|arg| filter_rule(arg, FilterEffect::Include))
                .chain(
                    exclude
                        .iter()
                        .map(|arg| filter_rule(arg, FilterEffect::Exclude)),
                )
                .collect::<anyhow::Result<Vec<_>>>()?;
            let stored = set_destination_filter(
                store,
                &cli_actor(),
                instance.id.as_str(),
                DestinationFilter { rules },
            )
            .await?;
            warn_about_copy_rules(store, &stored.body.filter).await;
            (instance, stored.body.filter)
        }
        FilterCommand::Clear { instance } => {
            let instance = resolve_instance(store, &instance).await?;
            let stored = set_destination_filter(
                store,
                &cli_actor(),
                instance.id.as_str(),
                DestinationFilter::default(),
            )
            .await?;
            (instance, stored.body.filter)
        }
    };
    emit(
        format,
        &json!({ "plugin_instance_id": instance.id.to_string(), "filter": filter }),
        || print_filter(&instance.id, &filter),
    )
}

/// Warns on stderr about `format` and `quality` rules that name no copy
/// the library makes (`output.copies` and the main output), and when the
/// destination would receive none of them. The filter is stored either
/// way: the copy list may change later.
async fn warn_about_copy_rules(store: &LibraryStore, filter: &bookclerk_config::DestinationFilter) {
    if !filter.has_copy_rules() {
        return;
    }
    let Ok(output) = bookclerk_library::control_plane::load_output(store).await else {
        return;
    };
    let mut config = bookclerk_config::OutputConfig::default();
    output.body.apply_to(&mut config, None);
    let copies = config.copies_to_make();
    for warning in copy_rule_warnings(filter, &copies) {
        eprintln!("warning: {warning}");
    }
}

/// The warnings [`warn_about_copy_rules`] prints for `filter` against the
/// library's `copies`.
fn copy_rule_warnings(
    filter: &bookclerk_config::DestinationFilter,
    copies: &[bookclerk_config::OutputCopy],
) -> Vec<String> {
    let names = copies
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let mut warnings: Vec<String> = filter
        .rules
        .iter()
        .filter(|rule| rule.field.is_copy_field())
        .filter(|rule| !copies.iter().any(|copy| rule.matches_copy(copy)))
        .map(|rule| {
            format!(
                "the {} rule `{}` names no copy the library makes ({names})",
                rule.field.as_str(),
                rule.value
            )
        })
        .collect();
    if !copies.iter().any(|copy| filter.accepts_copy(copy)) {
        warnings.push(format!(
            "this destination receives none of the library's copies ({names})"
        ));
    }
    warnings
}

/// `plugins config` actions.
async fn run_plugin_config(
    config: &Config,
    command: PluginConfigCommand,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plane = crate::registry::open_host(config).await?;
    let store = &plane.store;
    match command {
        PluginConfigCommand::Get { instance } => {
            let instance = resolve_instance(store, &instance).await?;
            let pinned = require_plugin_schema(store, &instance.plugin_key).await?;
            let document = load_instance_config(store, &instance.id).await?;
            let stored: std::collections::BTreeMap<String, serde_json::Value> = document
                .body
                .settings
                .iter()
                .map(|(key, value)| (key.clone(), SettingValue::to_json(value)))
                .collect();
            let effective = pinned.schema.effective_config(&stored);
            // Which declared `secret` keys have a sealed value. Holds key
            // names and a presence flag only; no ciphertext or plaintext.
            let sealed_presence: std::collections::BTreeMap<String, bool> = pinned
                .schema
                .secret_entries()
                .map(|spec| {
                    let set = document.body.secret_refs.iter().any(|r| r.key == spec.key);
                    (spec.key.clone(), set)
                })
                .collect();
            let payload = json!({
                "plugin_instance_id": instance.id.to_string(),
                "plugin_key": instance.plugin_key,
                "revision": document.revision,
                "settings": effective.config,
                "secrets": sealed_presence,
                "missing_required": effective.missing,
                "schema": pinned.schema.settings,
            });
            emit(format, &payload, || {
                println!("instance {} ({})", instance.id, instance.plugin_key);
                println!("revision {}", document.revision);
                // `effective.config` holds non-secret entries only (secrets
                // never enter the document); redaction is belt and braces.
                for (key, value) in &effective.config {
                    let shown = match value {
                        serde_json::Value::String(text) => bookclerk_config::redact_str(text),
                        other => bookclerk_config::redact_str(&other.to_string()),
                    };
                    let origin = if stored.contains_key(key) {
                        ""
                    } else {
                        "  (default)"
                    };
                    println!("{key} = {shown}{origin}");
                }
                for (key, set) in &sealed_presence {
                    println!("{key} = <secret {}>", if *set { "set" } else { "unset" });
                }
                for key in &effective.missing {
                    println!("{key} = <missing, required>");
                }
            })
        }
        PluginConfigCommand::Set {
            instance,
            key,
            value,
            secret,
            append,
        } => {
            let instance = resolve_instance(store, &instance).await?;
            let pinned = require_plugin_schema(store, &instance.plugin_key).await?;
            let document = load_instance_config(store, &instance.id).await?;
            let mut body = document.body.clone();
            let is_secret = pinned.schema.get(&key).is_some_and(|spec| spec.is_secret());
            let mut pending_secret = None;
            if secret || is_secret {
                if value.is_some() {
                    anyhow::bail!(
                        "secret values are not accepted on the command line; \
                         pass --secret and provide the value on stdin"
                    );
                }
                if !is_secret {
                    anyhow::bail!("setting `{key}` is not declared as a secret by the plugin");
                }
                let plaintext = read_secret_from_stdin()?;
                if plaintext.is_empty() {
                    anyhow::bail!("secret value on stdin is empty");
                }
                pending_secret = Some(plaintext);
                set_secret_ref(&mut body, &key);
            } else {
                let text = value.ok_or_else(|| anyhow::anyhow!("a value is required"))?;
                let mut parsed = parse_setting_text(&pinned.schema, &key, &text)?;
                if append {
                    let SettingValue::Rows(added) = parsed else {
                        anyhow::bail!("--append takes a `table` setting; `{key}` is not one");
                    };
                    let mut rows = current_rows(&pinned.schema, &body, &key)?;
                    rows.extend(added);
                    parsed = SettingValue::Rows(rows);
                }
                set_setting(&mut body, &key, Some(parsed));
            }
            // Commit the document first so a conflict or a validation error
            // leaves the live secret untouched; seal only after it lands.
            let revision =
                replace_document(store, &instance, &pinned.schema, document.revision, &body)
                    .await?;
            if let Some(plaintext) = pending_secret {
                seal_instance_secret(store, &instance.id, &instance.plugin_key, &key, &plaintext)
                    .await
                    .map_err(|err| {
                        anyhow::anyhow!(
                            "document revision {revision} was saved but secret `{key}` was not \
                             stored ({err}); run the command again"
                        )
                    })?;
            }
            // A network setting's new value names a destination that waits
            // for consent; say so rather than let the plugin fail later.
            let destination = pinned
                .schema
                .get(&key)
                .and_then(|spec| spec.network.map(|network| (network, spec.default_port)))
                .and_then(|(network, default_port)| match body.settings.get(&key) {
                    Some(SettingValue::String(text)) => {
                        Some(bookclerk_plugin_host::derive_setting_destination(
                            network,
                            default_port,
                            text,
                        ))
                    }
                    _ => None,
                });
            let note = destination.as_ref().map(|derived| match derived {
                Ok(destination) => format!(
                    "`{key}` names the network destination {}; the plugin reaches it only once it \
                     is approved (`bookclerk plugins approve {}`)",
                    destination.display(),
                    instance.plugin_key
                ),
                Err(err) => format!("`{key}` names no network destination: {err}"),
            });
            emit(
                format,
                &json!({
                    "plugin_instance_id": instance.id.to_string(),
                    "key": key,
                    "revision": revision,
                    "network_note": note,
                }),
                || {
                    println!(
                        "set {key} on instance {} (revision {revision})",
                        instance.id
                    );
                    if let Some(note) = &note {
                        println!("{note}");
                    }
                },
            )
        }
        PluginConfigCommand::Unset { instance, key, row } => {
            let instance = resolve_instance(store, &instance).await?;
            let pinned = require_plugin_schema(store, &instance.plugin_key).await?;
            let document = load_instance_config(store, &instance.id).await?;
            let mut body = document.body.clone();
            if let Some(row) = row {
                let mut rows = current_rows(&pinned.schema, &body, &key)?;
                if row == 0 || row > rows.len() {
                    anyhow::bail!(
                        "setting `{key}` has {} rows; there is no row {row}",
                        rows.len()
                    );
                }
                rows.remove(row - 1);
                set_setting(&mut body, &key, Some(SettingValue::Rows(rows)));
            } else {
                set_setting(&mut body, &key, None);
                body.secret_refs.retain(|r| r.key != key);
            }
            let revision =
                replace_document(store, &instance, &pinned.schema, document.revision, &body)
                    .await?;
            emit(
                format,
                &json!({
                    "plugin_instance_id": instance.id.to_string(),
                    "key": key,
                    "revision": revision,
                }),
                || {
                    println!(
                        "unset {key} on instance {} (revision {revision})",
                        instance.id
                    )
                },
            )
        }
    }
}

/// Rows a `table` setting holds now: the stored rows, else the declared
/// default, else none.
///
/// # Errors
///
/// Returns an error when `key` is not a declared table setting.
fn current_rows(
    schema: &bookclerk_plugin_abi::SettingsSchema,
    body: &PluginInstanceConfigV1,
    key: &str,
) -> anyhow::Result<Vec<std::collections::BTreeMap<String, SettingCell>>> {
    let spec = schema
        .get(key)
        .filter(|spec| spec.kind == bookclerk_plugin_abi::SettingKind::Table)
        .ok_or_else(|| anyhow::anyhow!("setting `{key}` is not a `table` setting"))?;
    if let Some(SettingValue::Rows(rows)) = body.settings.get(key) {
        return Ok(rows.clone());
    }
    match spec
        .default
        .clone()
        .map(serde_json::from_value::<SettingValue>)
    {
        Some(Ok(SettingValue::Rows(rows))) => Ok(rows),
        _ => Ok(Vec::new()),
    }
}

/// Compare-and-swap replace of an instance document; returns the new revision.
async fn replace_document(
    store: &LibraryStore,
    instance: &PluginInstance,
    schema: &bookclerk_plugin_abi::SettingsSchema,
    expected_revision: i64,
    body: &PluginInstanceConfigV1,
) -> anyhow::Result<i64> {
    match replace_instance_config(
        store,
        &cli_actor(),
        &instance.id,
        schema,
        expected_revision,
        body,
        &operation_id("instance-config"),
    )
    .await?
    {
        InstanceConfigReplace::Applied(doc) => Ok(doc.revision),
        InstanceConfigReplace::Replayed { revision } => Ok(revision),
        InstanceConfigReplace::Conflict { current_revision } => anyhow::bail!(
            "instance document changed concurrently (revision {current_revision}); retry"
        ),
    }
}

/// Reads one secret from stdin (first line, trailing newline removed); on
/// a terminal, prompts with echo off.
fn read_secret_from_stdin() -> anyhow::Result<String> {
    let text = crate::secret_input::read_secret_stdin("secret value: ")?;
    let value = text
        .split_once('\n')
        .map_or(text.as_str(), |(first, _)| first)
        .trim_end_matches('\r')
        .to_string();
    if value.is_empty() {
        anyhow::bail!("no secret value on stdin");
    }
    Ok(value)
}

/// True when `plugin` occupies `[database].plugin`.
///
/// An empty active slot means no database backend is selected — do not fall
/// back to the candidate's own alias (that would mark every adapter enabled).
fn occupies_database_slot(plugin: &DiscoveredPlugin, active: &str) -> bool {
    let active = active.trim();
    if active.is_empty() {
        return false;
    }
    plugin_matches_occupancy(plugin, active)
}

/// Discovers plugins and returns the one whose manifest id matches, or errors if missing.
fn find_plugin(config: &Config, id: &str) -> anyhow::Result<DiscoveredPlugin> {
    let plugins = bookclerk_plugin_host::discover_plugins(config)?;
    bookclerk_plugin_host::resolve_plugin_ref(&plugins, id)
        .cloned()
        .map_err(|err| anyhow::anyhow!("{err}"))
}

/// Converts a plugin settings TOML table to JSON, substituting `{}` if serialization fails.
fn toml_table_to_json(table: &toml::Table) -> serde_json::Value {
    serde_json::to_value(table).unwrap_or_else(|_| json!({}))
}

/// Combines a failed health check with rollback outcome. Never claims restore
/// succeeded unless rollback returned `Ok`.
fn health_failure_after_rollback(
    health: anyhow::Error,
    rollback: bookclerk_plugin_catalog::Result<()>,
    restored_message: &str,
) -> anyhow::Error {
    match rollback {
        Ok(()) => anyhow::anyhow!("health check failed: {health:#}; {restored_message}"),
        Err(rollback_err) => {
            anyhow::anyhow!("health check failed: {health:#}; rollback also failed: {rollback_err}")
        }
    }
}

/// Surfaces commit cleanup failure without implying the install itself rolled back.
fn commit_install_cleanup(outcome: &InstallOutcome) -> anyhow::Result<()> {
    Installer::commit(outcome).map_err(|err| {
        anyhow::anyhow!(
            "plugin installation succeeded but commit cleanup failed: {err:#}; \
             the new plugin is active, but a previous-version backup may remain under \
             plugins/.staging"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::health_failure_after_rollback;
    use bookclerk_plugin_catalog::CatalogError;

    #[test]
    fn health_rollback_failure_does_not_claim_restore() {
        let err = health_failure_after_rollback(
            anyhow::anyhow!("spawn exploded"),
            Err(CatalogError::message("ledger restore failed")),
            "previous version restored",
        )
        .to_string();
        assert!(err.contains("health check failed"), "{err}");
        assert!(err.contains("spawn exploded"), "{err}");
        assert!(err.contains("rollback also failed"), "{err}");
        assert!(err.contains("ledger restore failed"), "{err}");
        assert!(
            !err.contains("previous version restored"),
            "must not claim restore after a failed rollback: {err}"
        );
    }

    #[test]
    fn health_rollback_success_reports_restored() {
        let err = health_failure_after_rollback(
            anyhow::anyhow!("health boom"),
            Ok(()),
            "previous version restored",
        )
        .to_string();
        assert!(err.contains("health check failed: health boom"), "{err}");
        assert!(err.contains("previous version restored"), "{err}");
        assert!(!err.contains("rollback also failed"), "{err}");
    }
}

#[cfg(test)]
mod filter_tests {
    use bookclerk_config::{FilterEffect, FilterField};
    use bookclerk_library::control_plane::{create_plugin_instance, destination_filter};
    use bookclerk_library::LibraryStore;

    use super::{cli_actor, filter_rule, run_filter, FilterCommand, OutputFormat};

    #[test]
    fn copy_rules_that_name_no_library_copy_are_warned_about() {
        use bookclerk_config::{DestinationFilter, OutputCopy};
        let copies = [
            OutputCopy::parse("m4b:original").unwrap(),
            OutputCopy::parse("mp3:64k-mono").unwrap(),
        ];
        let filter = |rules: &[(&str, FilterEffect)]| DestinationFilter {
            rules: rules
                .iter()
                .map(|(arg, effect)| filter_rule(arg, *effect).unwrap())
                .collect(),
        };
        assert!(super::copy_rule_warnings(
            &filter(&[("format=mp3", FilterEffect::Include)]),
            &copies
        )
        .is_empty());
        let warnings =
            super::copy_rule_warnings(&filter(&[("quality=v2", FilterEffect::Include)]), &copies);
        assert_eq!(
            warnings,
            [
                "the quality rule `v2` names no copy the library makes \
                 (m4b:original, mp3:64k-mono)",
                "this destination receives none of the library's copies \
                 (m4b:original, mp3:64k-mono)",
            ]
        );
    }

    #[test]
    fn rules_are_read_as_field_equals_value() {
        let rule = filter_rule("media_kind=podcast", FilterEffect::Exclude).unwrap();
        assert_eq!(rule.field, FilterField::MediaKind);
        assert_eq!(rule.value, "podcast");
        assert_eq!(rule.effect, FilterEffect::Exclude);
        for (arg, field) in [
            ("patron=3", FilterField::Patron),
            ("storefront=audible", FilterField::Storefront),
            ("account=a=b", FilterField::Account),
            ("format=mp3", FilterField::Format),
            ("quality=64k-mono", FilterField::Quality),
        ] {
            assert_eq!(
                filter_rule(arg, FilterEffect::Include).unwrap().field,
                field
            );
        }
        assert_eq!(
            filter_rule("account=a=b", FilterEffect::Include)
                .unwrap()
                .value,
            "a=b"
        );
        assert!(filter_rule("storefront", FilterEffect::Include).is_err());
        assert!(filter_rule("genre=Kids", FilterEffect::Include).is_err());
    }

    #[tokio::test]
    async fn a_destination_filter_is_set_shown_and_cleared() {
        let store = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let instance = create_plugin_instance(&store, &cli_actor(), "s3", "Backup")
            .await
            .unwrap();
        let id = instance.id.to_string();
        run_filter(
            &store,
            FilterCommand::Set {
                instance: id.clone(),
                include: vec!["storefront=libro".into()],
                exclude: vec!["media_kind=podcast".into()],
            },
            OutputFormat::Text,
        )
        .await
        .unwrap();
        let stored = destination_filter(&store, &id).await.unwrap();
        assert_eq!(stored.rules.len(), 2);
        run_filter(
            &store,
            FilterCommand::Get {
                instance: id.clone(),
            },
            OutputFormat::Json,
        )
        .await
        .unwrap();
        run_filter(
            &store,
            FilterCommand::Get {
                instance: id.clone(),
            },
            OutputFormat::Text,
        )
        .await
        .unwrap();
        // A rule the filter refuses changes nothing.
        assert!(run_filter(
            &store,
            FilterCommand::Set {
                instance: id.clone(),
                include: vec!["patron=me".into()],
                exclude: Vec::new(),
            },
            OutputFormat::Text,
        )
        .await
        .is_err());
        assert_eq!(destination_filter(&store, &id).await.unwrap(), stored);
        run_filter(
            &store,
            FilterCommand::Clear {
                instance: id.clone(),
            },
            OutputFormat::Text,
        )
        .await
        .unwrap();
        assert!(destination_filter(&store, &id)
            .await
            .unwrap()
            .takes_everything());
    }
}

#[cfg(test)]
mod reserved_verbs_tests {
    use super::PluginsCommand;
    use crate::cli_plugin::RESERVED_PLUGIN_SUBCOMMANDS;
    use clap::Subcommand;

    #[test]
    fn every_builtin_plugins_verb_is_reserved_from_plugin_clis() {
        let cmd = PluginsCommand::augment_subcommands(clap::Command::new("plugins"));
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            assert!(
                RESERVED_PLUGIN_SUBCOMMANDS.contains(&name),
                "`plugins {name}` would be routed to a plugin CLI; add it to RESERVED_PLUGIN_SUBCOMMANDS"
            );
        }
    }
}
