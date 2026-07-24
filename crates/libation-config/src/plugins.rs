//! Plugin-style `[sources.*]` and `[integrations.*]` configuration.
//!
//! Layout mirrors a small plugin registry for content sources and optional
//! third-party integrations. **Diagnostics are not an integration** — they
//! stay under top-level `[diagnostics]`.
//!
//! ```toml
//! [sources.audible]
//! enabled = true
//!
//! [sources.graphicaudio]
//! enabled = true
//! access = "web"          # source-specific knob
//! ingest = "highest"      # optional override of [download.ingest]
//!
//! # [integrations.some_future_hook]
//! # enabled = true
//!
//! [diagnostics]
//! share_reports = false
//! ```
//!
//! CLI/daemon registries only register sources with `enabled = true`. Future
//! source crates should add a table under `[sources.<id>]` and a matching
//! `is_enabled` arm rather than top-level TOML keys.

use serde::{Deserialize, Serialize};

use crate::pipeline_opts::{GraphicAudioAccess, IngestQuality};

fn default_true() -> bool {
    true
}

/// Per-content-source plugins under `[sources]`.
///
/// Each source is independently enableable. Source-specific knobs live on that
/// source's table (e.g. `[sources.graphicaudio] access`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct SourcesConfig {
    pub audible: SourcePluginConfig,
    pub libro: SourcePluginConfig,
    pub chirp: SourcePluginConfig,
    pub graphicaudio: GraphicAudioSourceConfig,
}

impl SourcesConfig {
    /// Whether a content source id should be registered / scanned.
    #[must_use]
    pub fn is_enabled(&self, source: &str) -> bool {
        match source.trim().to_ascii_lowercase().as_str() {
            "audible" => self.audible.enabled,
            "libro" | "libro.fm" | "librofm" => self.libro.enabled,
            "chirp" => self.chirp.enabled,
            "graphicaudio" | "graphic_audio" | "ga" => self.graphicaudio.enabled,
            _ => true,
        }
    }

    /// Optional per-source ingest quality override.
    #[must_use]
    pub fn ingest_override(&self, source: &str) -> Option<IngestQuality> {
        match source.trim().to_ascii_lowercase().as_str() {
            "audible" => self.audible.ingest,
            "libro" | "libro.fm" | "librofm" => self.libro.ingest,
            "chirp" => self.chirp.ingest,
            "graphicaudio" | "graphic_audio" | "ga" => self.graphicaudio.ingest,
            _ => None,
        }
    }
}

/// Common knobs shared by every content-source plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SourcePluginConfig {
    /// When false, the source is not registered in CLI/daemon registries.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional ingest quality override for this source only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest: Option<IngestQuality>,
}

impl Default for SourcePluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ingest: None,
        }
    }
}

/// GraphicAudio plugin (`[sources.graphicaudio]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct GraphicAudioSourceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Fetch path: `web` (default) | `zip` | `device`.
    #[serde(default)]
    pub access: GraphicAudioAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingest: Option<IngestQuality>,
}

impl Default for GraphicAudioSourceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            access: GraphicAudioAccess::Web,
            ingest: None,
        }
    }
}

/// Optional third-party integrations under `[integrations]`.
///
/// Reserved for future hooks (webhooks, external library sync, …). Crash /
/// error-burst reporting is **not** an integration — use top-level
/// [`crate::DiagnosticsConfig`] / `[diagnostics]`.
///
/// Unknown keys are rejected so a mistaken `[integrations.diagnostics]` fails
/// loudly instead of being ignored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct IntegrationsConfig {}
