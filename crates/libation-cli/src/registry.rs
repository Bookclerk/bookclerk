//! Default content-source registry (Audible + Libro.fm).

use std::sync::Arc;

use libation_audible::AudibleSource;
use libation_libro::LibroSource;
use libation_source::{SourceKind, SourceRegistry};

/// Build a registry with every first-party content source installed.
#[must_use]
pub fn default_registry() -> SourceRegistry {
    let mut r = SourceRegistry::new();
    r.register(Arc::new(AudibleSource));
    r.register(Arc::new(LibroSource::new()));
    r
}

/// Clap / env parser for `--source audible|libro`.
pub fn parse_source_kind(s: &str) -> Result<SourceKind, String> {
    SourceKind::parse(s)
        .ok_or_else(|| format!("unknown source `{s}` (expected `audible` or `libro`)"))
}
