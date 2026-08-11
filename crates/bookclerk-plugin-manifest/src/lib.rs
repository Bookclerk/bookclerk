//! `plugin.toml` schema — parse, validate, and canonical format.
//!
//! Shared by the host, `bookclerk-workerd`, and the Rust guest SDK tools bin.
//! Language SDKs validate against the committed JSON Schema in
//! `schema/plugin-toml.json` (kept in sync with these types).

#![allow(missing_docs)]

mod egress;
mod error;
mod fmt;
mod logo;
mod types;

pub use egress::{
    consent_domains_for, host_matches, manifest_needs_python, normalize_domain_pattern,
    normalize_hostname, with_python_runtime_hosts, EgressPolicy, DEFAULT_MAX_REDIRECTS,
    PYODIDE_EGRESS_HOSTS,
};
pub use error::{Error, Result};
pub use fmt::format_manifest;
pub use logo::{
    embedded_logo_api_path, logo_content_type, validate_logo, LogoKind, LOGO_EXTENSIONS,
    MAX_EMBEDDED_LOGO_BYTES,
};
pub use types::*;

/// Embedded JSON Schema for authoring tools (TS/Python `check`).
pub const PLUGIN_TOML_SCHEMA_JSON: &str = include_str!("../schema/plugin-toml.json");

/// Parse and validate manifest TOML.
pub fn parse(text: &str) -> Result<PluginManifest> {
    PluginManifest::parse(text)
}
