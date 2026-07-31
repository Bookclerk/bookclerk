//! `bookclerk plugins` — discover, manage, and invoke dynamic plugin CLI.

use bookclerk_config::Config;
use bookclerk_plugin::{
    host_target_triple, methods, search_crates_io, CliInvokeParams, CliSchema, DiscoveredPlugin,
    PluginClient, PluginKind, CRATE_NAME_PREFIX,
};
use clap::Subcommand;
use serde::Serialize;
use serde_json::json;

use crate::cli_plugin::{
    find_command, matches_to_invoke_args, schema_to_command, RESERVED_PLUGIN_SUBCOMMANDS,
};
use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// List plugins found under plugin search directories.
    List,
    /// Search crates.io for publishable Bookclerk plugins.
    Search {
        /// Free-text query (combined with the `bookclerk-plugin` keyword).
        query: Option<String>,
        /// Max crates.io hits to fetch (1–100).
        #[arg(long, default_value_t = 25)]
        limit: u32,
    },
    /// Install a plugin from the registry (prebuilt archive — no Rust required).
    Install {
        /// Crate name (`bookclerk-plugin-{kind}-{id}`) or plugin id.
        crate_or_id: String,
    },
    /// Show details for one discovered plugin.
    Info {
        /// Plugin id.
        id: String,
    },
    /// Run diagnose probes for one (or all) plugins that support it.
    Diagnose {
        /// Plugin id (default: all discovered that are enabled).
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
}

#[derive(Debug, Serialize)]
struct PluginListItem {
    id: String,
    kind: String,
    enabled: bool,
    command: String,
    name: Option<String>,
    has_cli: bool,
}

pub async fn run(
    command: PluginsCommand,
    config: &Config,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match command {
        PluginsCommand::List => {
            let dirs = bookclerk_plugin::plugin_search_dirs(config);
            let plugins = bookclerk_plugin::discover_plugins(config)?;
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
        PluginsCommand::Search { query, limit } => {
            let hits = search_crates_io(query.as_deref(), limit)?;
            emit(
                format,
                &json!({
                    "prefix": CRATE_NAME_PREFIX,
                    "host_target": host_target_triple(),
                    "plugins": hits,
                }),
                || {
                    if hits.is_empty() {
                        println!(
                            "no crates.io plugins matching `{CRATE_NAME_PREFIX}*` yet \
                             (see docs/plugin-registry.md)"
                        );
                        return;
                    }
                    for h in &hits {
                        let kind = h.parsed.as_ref().map(|p| p.kind.as_str()).unwrap_or("?");
                        let id = h.parsed.as_ref().map(|p| p.id.as_str()).unwrap_or("?");
                        println!(
                            "{}  kind={kind} id={id} v{} downloads={}",
                            h.crate_name, h.version, h.downloads
                        );
                        if let Some(desc) = &h.description {
                            println!("  {desc}");
                        }
                    }
                },
            )
        }
        PluginsCommand::Install { crate_or_id } => {
            // Phase C: download+verify+unpack prebuilt archives (no rustc).
            anyhow::bail!(
                "`bookclerk plugins install` is not implemented yet \
                 (crate_or_id={crate_or_id}, host_target={}). \
                 Unpack a release archive under $BOOKCLERK_FILES_DIR/plugins/<id>/ \
                 for now — see docs/plugin-registry.md",
                host_target_triple()
            );
        }
        PluginsCommand::Info { id } => {
            let plugin = find_plugin(config, &id)?;
            let schema = plugin.manifest.cli.clone().unwrap_or_default();
            let enabled = is_enabled(config, &plugin);
            let payload = json!({
                "id": plugin.manifest.id,
                "kind": plugin.manifest.kind.as_str(),
                "name": plugin.manifest.name,
                "enabled": enabled,
                "command": plugin.command.display().to_string(),
                "root": plugin.root.display().to_string(),
                "cli": schema,
            });
            emit(format, &payload, || {
                println!("id={}", plugin.manifest.id);
                println!("kind={}", plugin.manifest.kind.as_str());
                println!("name={}", plugin.manifest.name.as_deref().unwrap_or("-"));
                println!("enabled={enabled}");
                println!("command={}", plugin.command.display());
                println!("root={}", plugin.root.display());
                if schema.commands.is_empty() {
                    println!(
                        "cli commands: (none in plugin.toml; may still advertise via handshake)"
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
            let plugins = bookclerk_plugin::discover_plugins(config)?;
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
        PluginsCommand::Enable { id } => set_plugin_enabled(config, &id, true, format),
        PluginsCommand::Disable { id } => set_plugin_enabled(config, &id, false, format),
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
    let (schema, client) = if help_only {
        (plugin.manifest.cli.clone().unwrap_or_default(), None)
    } else {
        if !is_enabled(config, &plugin) {
            anyhow::bail!(
                "plugin `{}` is disabled; run `bookclerk plugins enable {}`",
                plugin.manifest.id,
                plugin.manifest.id
            );
        }
        let settings = bookclerk_plugin::settings_table(config, &plugin);
        let client = PluginClient::spawn(&plugin, config, toml_table_to_json(&settings)).await?;
        let schema = resolve_schema(&client, &plugin).await?;
        (schema, Some(client))
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

    let Some(client) = client else {
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
    let result = client
        .cli_invoke(CliInvokeParams {
            command: cmd_name.to_string(),
            args,
        })
        .await?;

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
    let Ok(discovered) = bookclerk_plugin::discover_plugins(config) else {
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

async fn resolve_schema(
    client: &PluginClient,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<CliSchema> {
    if client.has_capability("cli") {
        return Ok(client.cli_describe().await?);
    }
    if let Some(cli) = &client.handshake().cli {
        return Ok(cli.clone());
    }
    Ok(plugin.manifest.cli.clone().unwrap_or_default())
}

async fn diagnose_plugin(
    config: &Config,
    plugin: &DiscoveredPlugin,
) -> anyhow::Result<Vec<String>> {
    let settings = bookclerk_plugin::settings_table(config, plugin);
    let client = PluginClient::spawn(plugin, config, toml_table_to_json(&settings)).await?;
    if !client.has_capability("diagnose") {
        return Ok(vec![format!(
            "plugin `{}` has no diagnose capability",
            plugin.manifest.id
        )]);
    }
    let lines: Vec<String> = client
        .call(methods::DIAGNOSE, json!({}))
        .await
        .unwrap_or_else(|err| vec![format!("diagnose failed: {err:#}")]);
    Ok(lines)
}

fn set_plugin_enabled(
    config: &Config,
    id: &str,
    enabled: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let plugin = find_plugin(config, id)?;
    let mut cfg = config.clone();
    match plugin.manifest.kind {
        PluginKind::Source => cfg.sources.set_enabled(&plugin.manifest.id, enabled),
        PluginKind::Integration => cfg.integrations.set_enabled(&plugin.manifest.id, enabled),
        PluginKind::Output if plugin.manifest.id == "s3" => {
            cfg.output.s3.enabled = enabled;
        }
        PluginKind::Output => {
            anyhow::bail!(
                "output plugin `{}` is not configurable yet",
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

fn matches_plugin_id(manifest_id: &str, active: &str) -> bool {
    manifest_id.eq_ignore_ascii_case(active)
}

fn find_plugin(config: &Config, id: &str) -> anyhow::Result<DiscoveredPlugin> {
    let plugins = bookclerk_plugin::discover_plugins(config)?;
    plugins
        .into_iter()
        .find(|p| p.manifest.id == id)
        .ok_or_else(|| anyhow::anyhow!("plugin `{id}` not discovered"))
}

fn is_enabled(config: &Config, plugin: &DiscoveredPlugin) -> bool {
    match plugin.manifest.kind {
        PluginKind::Source => config.sources.is_enabled(&plugin.manifest.id),
        PluginKind::Integration => config.integrations.is_enabled(&plugin.manifest.id),
        PluginKind::Output if plugin.manifest.id == "s3" => config.output.s3.enabled,
        PluginKind::Output => false,
        PluginKind::Database => config
            .database
            .plugin
            .eq_ignore_ascii_case(&plugin.manifest.id),
    }
}

fn toml_table_to_json(table: &toml::Table) -> serde_json::Value {
    serde_json::to_value(table).unwrap_or_else(|_| json!({}))
}
