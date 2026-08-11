//! Plugin kind shared by catalog manifests (mirrors host `PluginKind`).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Which Bookclerk surface a plugin implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Content-source storefront guest (Audible, Libro.fm, …).
    Source,
    /// Portal / library integration guest (not a content storefront).
    Integration,
    /// Destination / output guest (local disk, S3, …).
    Output,
    /// Library database backend guest (`sqlite`, `d1`, `postgres`).
    Database,
}

impl PluginKind {
    /// Returns the canonical lowercase kind string for manifests and crate names.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Integration => "integration",
            Self::Output => "output",
            Self::Database => "database",
        }
    }

    /// Parse a kind segment from a coordinate or crate name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "source" => Some(Self::Source),
            "integration" => Some(Self::Integration),
            "output" => Some(Self::Output),
            "database" => Some(Self::Database),
            _ => None,
        }
    }
}

impl fmt::Display for PluginKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime identity used for discovery and install collision checks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    /// Destination backend kind (`local`, `s3`, …).
    pub kind: PluginKind,
    /// Plugin id (`[a-z0-9_]{2,32}`), globally unique across kinds.
    pub id: String,
}

impl RuntimeIdentity {
    /// Builds a runtime identity from plugin kind and id.
    #[must_use]
    pub fn new(kind: PluginKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }
}

impl fmt::Display for RuntimeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.id)
    }
}
