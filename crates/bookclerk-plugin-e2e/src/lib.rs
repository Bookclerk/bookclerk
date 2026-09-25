//! Support for the staged first-party plugin end-to-end suite.
//!
//! The suite (`tests/staged_plugins.rs`) spawns every guest from a real
//! installation: platform guests under `$BOOKCLERK_FILES_DIR/plugins/`
//! (`cargo install-platform`) and optional/example guests under
//! `$BOOKCLERK_PLUGIN_ARTIFACTS` (`cargo stage-plugins`). It lives in its own
//! package so ordinary `bookclerk-plugin-host` test runs never need that
//! installation.
//!
//! `tests/native_gateway.rs` is separate: it needs no staged installation and
//! launches the test-only `native_gateway_probe` bin through the production
//! native-behind-workerd path (CI check `native_gateway`, all three OSes).
//!
//! | Variable | Effect |
//! | --- | --- |
//! | `BOOKCLERK_PLUGIN_ARTIFACTS` / `BOOKCLERK_FILES_DIR` | Staging roots; unset skips locally |
//! | `BOOKCLERK_STAGED_PLUGINS=a,b` | Check only these ids (plus platform guests); unset checks all |
//! | `BOOKCLERK_REQUIRE_STAGED_PLUGINS=1` | Missing roots or ledgers fail instead of skipping (CI) |

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Platform guests installed into `$BOOKCLERK_FILES_DIR/plugins/`; always checked.
pub const PLATFORM_PLUGINS: &[&str] = &["local", "sqlite"];

/// Optional first-party guests staged under `$BOOKCLERK_PLUGIN_ARTIFACTS`.
pub const OPTIONAL_PLUGINS: &[&str] = &[
    "audible",
    "libro",
    "chirp",
    "graphicaudio",
    "audiobookshelf",
    "s3",
    "d1",
    "postgres",
];

/// Reference Echo guests staged under `$BOOKCLERK_PLUGIN_ARTIFACTS`.
pub const EXAMPLE_PLUGINS: &[&str] = &[
    "echo_native_rust",
    "echo_native_node",
    "echo_native_python",
    "echo_workerd_ts",
    "echo_workerd_python",
    "echo_workerd_rust",
    "echo_workerd_fetch",
];

/// Environment variable selecting a subset of staged guests to check.
pub const STAGED_PLUGINS_ENV: &str = "BOOKCLERK_STAGED_PLUGINS";

/// Environment variable turning missing staging prerequisites into failures.
pub const REQUIRE_STAGED_ENV: &str = "BOOKCLERK_REQUIRE_STAGED_PLUGINS";

/// Every first-party plugin id the full installation must contain.
///
/// # Returns
///
/// Platform, optional, then example ids.
pub fn all_expected_plugins() -> Vec<&'static str> {
    PLATFORM_PLUGINS
        .iter()
        .chain(OPTIONAL_PLUGINS)
        .chain(EXAMPLE_PLUGINS)
        .copied()
        .collect()
}

/// Which staged guests a run must check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedScope {
    /// The complete installation (default).
    Full,
    /// Only these non-platform ids; platform guests are always included.
    Subset(BTreeSet<String>),
}

impl StagedScope {
    /// Parses a `BOOKCLERK_STAGED_PLUGINS` value.
    ///
    /// # Arguments
    ///
    /// * `raw` - Comma-separated plugin ids; `None`, empty, or `all` means full.
    ///
    /// # Returns
    ///
    /// The scope to check.
    ///
    /// # Errors
    ///
    /// Returns a message naming the first id that is not a known first-party
    /// plugin, so a typo cannot silently shrink coverage.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Self::Full);
        };
        if raw == "all" {
            return Ok(Self::Full);
        }
        let known = all_expected_plugins();
        let mut ids = BTreeSet::new();
        for id in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !known.contains(&id) {
                return Err(format!(
                    "{STAGED_PLUGINS_ENV}: unknown plugin id `{id}` (known: {known:?})"
                ));
            }
            if !PLATFORM_PLUGINS.contains(&id) {
                ids.insert(id.to_string());
            }
        }
        Ok(Self::Subset(ids))
    }

    /// Reads the scope from `BOOKCLERK_STAGED_PLUGINS`.
    ///
    /// # Returns
    ///
    /// The parsed scope.
    ///
    /// # Errors
    ///
    /// Same as [`StagedScope::parse`].
    pub fn from_env() -> Result<Self, String> {
        let raw = std::env::var(STAGED_PLUGINS_ENV).ok();
        Self::parse(raw.as_deref())
    }

    /// Whether `id` is checked in this scope.
    ///
    /// # Arguments
    ///
    /// * `id` - Plugin manifest id.
    ///
    /// # Returns
    ///
    /// `true` for platform guests and for ids inside the scope.
    pub fn includes(&self, id: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Subset(ids) => PLATFORM_PLUGINS.contains(&id) || ids.contains(id),
        }
    }

    /// Ids that must be discovered for this scope.
    ///
    /// # Returns
    ///
    /// Platform ids followed by the scoped ids (all expected ids when full).
    pub fn required_ids(&self) -> Vec<String> {
        match self {
            Self::Full => all_expected_plugins()
                .into_iter()
                .map(str::to_string)
                .collect(),
            Self::Subset(ids) => PLATFORM_PLUGINS
                .iter()
                .map(|s| s.to_string())
                .chain(ids.iter().cloned())
                .collect(),
        }
    }
}

/// Whether missing staging prerequisites must fail the run.
///
/// # Returns
///
/// `true` when `BOOKCLERK_REQUIRE_STAGED_PLUGINS=1`.
pub fn require_staged() -> bool {
    std::env::var(REQUIRE_STAGED_ENV).is_ok_and(|v| v == "1")
}

/// Skips (local) or panics (required mode) for a missing prerequisite.
///
/// # Arguments
///
/// * `reason` - Human-readable prerequisite that is missing.
///
/// # Panics
///
/// Panics when [`require_staged`] is true.
pub fn skip_or_fail(reason: &str) {
    if require_staged() {
        panic!("{REQUIRE_STAGED_ENV}=1 but {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// Finds the staged directory whose `plugin.toml` declares `id`.
///
/// Staging writes `pk-*` directories keyed by plugin key, so an id cannot be
/// joined onto the artifacts root directly.
///
/// # Arguments
///
/// * `root` - Staging root (`$BOOKCLERK_PLUGIN_ARTIFACTS`).
/// * `id` - Plugin manifest id.
///
/// # Returns
///
/// The guest directory, or `None` when no staged manifest declares `id`.
pub fn staged_plugin_dir(root: &Path, id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        let Ok(text) = std::fs::read_to_string(dir.join("plugin.toml")) else {
            continue;
        };
        if bookclerk_plugin_manifest::parse(&text).is_ok_and(|m| m.id == id) {
            return Some(dir);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_all_is_full() {
        assert_eq!(StagedScope::parse(None), Ok(StagedScope::Full));
        assert_eq!(StagedScope::parse(Some("  ")), Ok(StagedScope::Full));
        assert_eq!(StagedScope::parse(Some("all")), Ok(StagedScope::Full));
    }

    #[test]
    fn subset_always_includes_platform() {
        let scope = StagedScope::parse(Some("libro, echo_workerd_ts")).unwrap();
        assert!(scope.includes("libro"));
        assert!(scope.includes("echo_workerd_ts"));
        assert!(scope.includes("sqlite"));
        assert!(scope.includes("local"));
        assert!(!scope.includes("audible"));
        assert_eq!(
            scope.required_ids(),
            vec!["local", "sqlite", "echo_workerd_ts", "libro"]
        );
    }

    #[test]
    fn unknown_id_is_rejected() {
        let err = StagedScope::parse(Some("libro,libr0")).unwrap_err();
        assert!(err.contains("libr0"), "{err}");
    }

    #[test]
    fn full_requires_every_expected_id() {
        let ids = StagedScope::Full.required_ids();
        assert_eq!(ids.len(), all_expected_plugins().len());
        assert!(ids.iter().any(|i| i == "postgres"));
        assert!(ids.iter().any(|i| i == "echo_workerd_fetch"));
    }
}
