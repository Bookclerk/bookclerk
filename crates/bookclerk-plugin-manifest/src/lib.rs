//! `plugin.toml` schema — parse, validate, and canonical format.
//!
//! This crate is the Rust projection of the install-time plugin descriptor.
//! It is shared by the host (`bookclerk-plugin-host`), the workerd launcher
//! (`bookclerk-workerd`), and the Rust guest SDK author tools (`check` /
//! `fmt`). Language SDKs (TypeScript / Python) validate against the committed
//! JSON Schema in [`PLUGIN_TOML_SCHEMA_JSON`] (`schema/plugin-toml.json`),
//! which must stay in sync with the types here.
//!
//! # Audience
//!
//! - **Host / launcher authors** — parse manifests at discovery, build
//!   [`EgressPolicy`] for isolate injection, map network + OAuth bindings to
//!   jail policy via [`PluginManifest::jail_network_need`].
//! - **Plugin authors / SDK tools** — validate and pretty-print `plugin.toml`
//!   without pulling host dependencies.
//!
//! # Modules
//!
//! | Module | Responsibility |
//! | --- | --- |
//! | [`egress`] | Hostname allowlist policy, IDNA normalization, Pyodide hosts |
//! | [`error`] | Parse / validate error type |
//! | [`fmt`] | Canonical TOML serialization for `fmt --check` |
//! | [`id`] | Strict plugin id grammar |
//! | [`logo`] | Remote URL vs embedded path logo validation |
//! | [`types`] | `PluginManifest` and nested capability / workerd tables |
//!
//! Product narrative: `docs/plugins.md` (network consent, egress matching,
//! packaging). ABI contract: `bookclerk-plugin-abi`.
//!
//! # Examples
//!
//! ```
//! use bookclerk_plugin_manifest::parse;
//!
//! let manifest = parse(r#"
//! api_version = 2
//! id = "echo"
//! kind = "integration"
//! runtime = "native"
//! command = "./echo"
//!
//! [capabilities.network]
//! mode = "deny"
//! "#).expect("valid plugin.toml");
//! assert_eq!(manifest.id, "echo");
//! ```

pub mod egress;
pub mod error;
pub mod fmt;
pub mod id;
pub mod logo;
pub mod types;

pub use egress::{
    consent_domains_for, host_matches, manifest_needs_python, normalize_domain_pattern,
    normalize_hostname, with_python_runtime_hosts, EgressPolicy, DEFAULT_MAX_REDIRECTS,
    PYODIDE_EGRESS_HOSTS,
};
pub use error::{Error, Result};
pub use fmt::format_manifest;
pub use id::validate_plugin_id;
pub use logo::{
    embedded_logo_api_path, logo_content_type, validate_logo, LogoKind, LOGO_EXTENSIONS,
    MAX_EMBEDDED_LOGO_BYTES,
};
pub use types::*;

/// Embedded JSON Schema text for authoring tools (`check` in TS/Python SDKs).
///
/// Identical on disk to `schema/plugin-toml.json`. Prefer this constant over
/// reading the file when packaging tools as a single crate artifact.
pub const PLUGIN_TOML_SCHEMA_JSON: &str = include_str!("../schema/plugin-toml.json");

/// Parses and validates a `plugin.toml` document.
///
/// Equivalent to [`PluginManifest::parse`]: deserializes TOML then runs
/// [`PluginManifest::validate`].
///
/// # Arguments
///
/// * `text` - Full contents of a `plugin.toml` file (UTF-8).
///
/// # Returns
///
/// A validated [`PluginManifest`].
///
/// # Errors
///
/// Returns [`Error::TomlDe`] on malformed TOML, or [`Error::Message`] when
/// semantic validation fails (id grammar, runtime requirements, network
/// domains, logo, and so on).
///
/// # Examples
///
/// ```
/// use bookclerk_plugin_manifest::parse;
///
/// let m = parse(r#"
/// api_version = 2
/// id = "sqlite"
/// kind = "database"
/// runtime = "native"
/// command = "./bookclerk-plugin-database-sqlite"
///
/// [capabilities.network]
/// mode = "deny"
/// "#).unwrap();
/// assert_eq!(m.kind.as_str(), "database");
/// ```
pub fn parse(text: &str) -> Result<PluginManifest> {
    PluginManifest::parse(text)
}
