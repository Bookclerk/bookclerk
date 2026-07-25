//! `libation plugins` — list discovered external plugins.

use clap::Subcommand;
use libation_config::Config;

#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// List plugins found under plugin search directories.
    List,
}

pub async fn run(command: PluginsCommand, config: &Config) -> anyhow::Result<()> {
    match command {
        PluginsCommand::List => {
            let dirs = libation_plugin::plugin_search_dirs(config);
            println!("search dirs:");
            for d in &dirs {
                println!("  {}", d.display());
            }
            let plugins = libation_plugin::discover_plugins(config)?;
            if plugins.is_empty() {
                println!("no plugins discovered");
                return Ok(());
            }
            for p in plugins {
                let enabled = match p.manifest.kind {
                    libation_plugin::PluginKind::Source => {
                        config.sources.is_enabled(&p.manifest.id)
                    }
                    libation_plugin::PluginKind::Integration => {
                        config.integrations.is_enabled(&p.manifest.id)
                    }
                    libation_plugin::PluginKind::Output => false,
                };
                println!(
                    "{} kind={} enabled={} command={}",
                    p.manifest.id,
                    p.manifest.kind.as_str(),
                    enabled,
                    p.command.display()
                );
            }
            Ok(())
        }
    }
}
