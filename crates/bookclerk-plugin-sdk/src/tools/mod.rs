//! Authoring helpers: `check`, `fmt`, `package`, and (with feature `tools`) `smoke`.
//!
//! Audience: plugin maintainers validating `plugin.toml` layout and packaging
//! archives for distribution. The `bookclerk-plugin` binary requires
//! `--features tools` so guest plugins that depend on this crate with default
//! features do not pull `bookclerk-workerd`.
//!
//! | Helper | Feature | Purpose |
//! | --- | --- | --- |
//! | [`check_plugin`] / [`fmt_plugin_toml`] / [`package_plugin`] | always | Validate, format, archive |
//! | [`sync_embed`] | always | Optional workerd JS vendor |
//! | `run_tools_cli` / `smoke_plugin` | `tools` | CLI entry + live workerd smoke |
//!
//! See `docs/plugins.md` and the crate README.

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
