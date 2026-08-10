//! Authoring helpers: `check`, `fmt`, `package`, and (with feature `tools`) `smoke`.
//!
//! The `bookclerk-plugin` binary requires `--features tools` so guest plugins
//! that depend on this crate with default features do not pull `bookclerk-workerd`.

mod check;
#[cfg(feature = "tools")]
mod cli;
mod package;
#[cfg(feature = "tools")]
mod smoke;

pub use check::{check_plugin, sync_embed};
#[cfg(feature = "tools")]
pub use cli::run as run_tools_cli;
pub use package::package_plugin;
#[cfg(feature = "tools")]
pub use smoke::smoke_plugin;

use bookclerk_plugin_manifest::{format_manifest, parse, PluginManifest};

/// Format `plugin.toml` text to the canonical form.
pub fn fmt_plugin_toml(text: &str) -> Result<String, bookclerk_plugin_manifest::Error> {
    let m = parse(text)?;
    format_manifest(&m)
}

/// Parse + validate only.
pub fn load_manifest(text: &str) -> Result<PluginManifest, bookclerk_plugin_manifest::Error> {
    parse(text)
}
