//! Plugin authoring tools behind the `bookclerk-plugin` CLI: `check`, `fmt`,
//! `sync-embed`, `package`, and `smoke`.
//!
//! Audience: plugin maintainers validating `plugin.toml` layout and packaging
//! archives for distribution. Kept out of `bookclerk-plugin-sdk` so guest
//! plugins never link `bookclerk-workerd` or the packaging stack.
//!
//! | Helper | Purpose |
//! | --- | --- |
//! | [`check_plugin`] / [`fmt_plugin_toml`] / [`package_plugin`] | Validate, format, archive |
//! | [`sync_embed`] | Optional workerd JS vendor |
//! | [`run_tools_cli`] / [`smoke_plugin`] | CLI entry + live workerd smoke |
//!
//! See `docs/plugins.md` and the crate README.

mod check;
mod cli;
mod package;
mod smoke;

pub use check::{
    check_main_module_source, check_plugin, entrypoint_export_class, sync_embed, MainModuleLanguage,
};
pub use cli::run as run_tools_cli;
pub use package::package_plugin;
pub use smoke::smoke_plugin;

use bookclerk_plugin_manifest::{format_manifest, parse, PluginManifest};

/// Formats `plugin.toml` text to the canonical serialized form.
///
/// Parses with the manifest crate, then re-emits a stable key order / spacing
/// suitable for `bookclerk-plugin fmt` and CI `--check` diffs.
///
/// # Arguments
///
/// * `text` - Raw contents of a `plugin.toml` file.
///
/// # Returns
///
/// Canonical TOML string (typically ending with a trailing newline).
///
/// # Errors
///
/// Returns a manifest parse/format error when the text is invalid TOML or
/// fails schema validation.
pub fn fmt_plugin_toml(text: &str) -> Result<String, bookclerk_plugin_manifest::Error> {
    let m = parse(text)?;
    format_manifest(&m)
}

/// Parses and validates `plugin.toml` text without rewriting it.
///
/// # Arguments
///
/// * `text` - Raw contents of a `plugin.toml` file.
///
/// # Returns
///
/// Structured [`PluginManifest`] ready for inspection.
///
/// # Errors
///
/// Returns a manifest error when parsing or validation fails.
pub fn load_manifest(text: &str) -> Result<PluginManifest, bookclerk_plugin_manifest::Error> {
    parse(text)
}
