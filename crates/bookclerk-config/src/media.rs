//! Media worker pool settings (`[media]` section).

use serde::{Deserialize, Serialize};

/// How strictly media worker processes must be confined.
///
/// Codec work runs in a child process that restricts itself to the paths its
/// job declared. This picks what happens when the host cannot enforce that.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MediaIsolation {
    /// Refuse to process media when the jail does not engage. The default: a
    /// host that cannot sandbox should not decode untrusted audio in the same
    /// process as the master key.
    #[default]
    Required,
    /// Confine where the platform allows and log what did not engage. Use on
    /// kernels without Landlock, accepting that codecs run unconfined there.
    BestEffort,
    /// Run codec work in-process with no jail. Development only.
    Off,
}

impl MediaIsolation {
    /// Parse a config or environment value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" | "on" | "true" => Some(Self::Required),
            "best-effort" | "best_effort" | "besteffort" => Some(Self::BestEffort),
            "off" | "false" | "none" => Some(Self::Off),
            _ => None,
        }
    }
}

/// `[media]` — decode, encode, and packaging worker pool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MediaConfig {
    /// Maximum codec jobs running at once. `0` derives a value from the
    /// machine's available parallelism, capped so a large host does not run
    /// dozens of memory-hungry encoders at the same time.
    pub workers: usize,
    /// How strictly workers must be confined.
    pub isolation: MediaIsolation,
    /// Explicit path to `bookclerk-media-worker`. Normally left unset, in which
    /// case the worker is found beside the running executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_bin: Option<std::path::PathBuf>,
}

impl MediaConfig {
    /// Apply `BOOKCLERK_MEDIA_*` environment overrides.
    ///
    /// Environment wins over TOML, matching every other section.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(value) = std::env::var("BOOKCLERK_MEDIA_WORKERS") {
            if let Ok(workers) = value.trim().parse::<usize>() {
                self.workers = workers;
            }
        }
        if let Ok(value) = std::env::var("BOOKCLERK_MEDIA_ISOLATION") {
            if let Some(isolation) = MediaIsolation::parse(&value) {
                self.isolation = isolation;
            }
        }
        if let Ok(value) = std::env::var("BOOKCLERK_MEDIA_WORKER") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                self.worker_bin = Some(std::path::PathBuf::from(trimmed));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_defaults_to_required() {
        assert_eq!(MediaConfig::default().isolation, MediaIsolation::Required);
    }

    #[test]
    fn isolation_parses_the_documented_spellings() {
        assert_eq!(
            MediaIsolation::parse("required"),
            Some(MediaIsolation::Required)
        );
        assert_eq!(
            MediaIsolation::parse("best-effort"),
            Some(MediaIsolation::BestEffort)
        );
        assert_eq!(
            MediaIsolation::parse(" BEST_EFFORT "),
            Some(MediaIsolation::BestEffort)
        );
        assert_eq!(MediaIsolation::parse("off"), Some(MediaIsolation::Off));
        assert_eq!(MediaIsolation::parse("maybe"), None);
    }

    #[test]
    fn workers_defaults_to_automatic() {
        assert_eq!(MediaConfig::default().workers, 0);
    }

    #[test]
    fn section_round_trips_through_toml() {
        let config = MediaConfig {
            workers: 4,
            isolation: MediaIsolation::BestEffort,
            worker_bin: None,
        };
        let encoded = toml::to_string(&config).expect("serialize");
        let decoded: MediaConfig = toml::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, config);
        assert!(
            encoded.contains("best-effort"),
            "isolation should serialize kebab-case: {encoded}"
        );
    }
}
