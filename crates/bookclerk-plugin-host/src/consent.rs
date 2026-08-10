//! Operator permission grants for plugin capabilities.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manifest::PluginManifest;
use crate::{PluginError, Result};

/// Filename under `$BOOKCLERK_FILES_DIR` for persisted grants.
pub const GRANTS_FILE: &str = "plugin-grants.json";

/// One approved grant snapshot for a plugin id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginGrant {
    pub plugin_id: String,
    pub kind: String,
    pub network_mode: String,
    pub domains: BTreeSet<String>,
    pub bindings: BTreeSet<String>,
    pub compatibility_flags: BTreeSet<String>,
    pub approved_at: String,
}

/// On-disk grant store.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginGrantStore {
    #[serde(default)]
    pub grants: Vec<PluginGrant>,
}

impl PluginGrantStore {
    pub fn path(files_dir: &Path) -> PathBuf {
        files_dir.join(GRANTS_FILE)
    }

    pub fn load(files_dir: &Path) -> Result<Self> {
        let path = Self::path(files_dir);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, files_dir: &Path) -> Result<()> {
        let path = Self::path(files_dir);
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    pub fn get(&self, plugin_id: &str) -> Option<&PluginGrant> {
        self.grants.iter().find(|g| g.plugin_id == plugin_id)
    }

    pub fn upsert(&mut self, grant: PluginGrant) {
        if let Some(existing) = self
            .grants
            .iter_mut()
            .find(|g| g.plugin_id == grant.plugin_id)
        {
            *existing = grant;
        } else {
            self.grants.push(grant);
        }
    }
}

/// Build the consent request surface from a manifest.
#[must_use]
pub fn consent_request(manifest: &PluginManifest) -> PluginGrant {
    let mut bindings = BTreeSet::new();
    let b = &manifest.capabilities.bindings;
    if b.config {
        bindings.insert("config".into());
    }
    if b.secrets {
        bindings.insert("secrets".into());
    }
    if b.plugin_kv {
        bindings.insert("plugin_kv".into());
    }
    if b.work_fs {
        bindings.insert("work_fs".into());
    }
    if b.oauth {
        bindings.insert("oauth".into());
    }
    let flags = manifest
        .workerd
        .as_ref()
        .map(|w| w.compatibility_flags.iter().cloned().collect())
        .unwrap_or_default();
    PluginGrant {
        plugin_id: manifest.id.clone(),
        kind: manifest.kind.as_str().to_string(),
        network_mode: match manifest.capabilities.network.mode {
            crate::manifest::NetworkMode::Deny => "deny".into(),
            crate::manifest::NetworkMode::Outbound => "outbound".into(),
        },
        domains: manifest
            .capabilities
            .network
            .domains
            .iter()
            .cloned()
            .collect(),
        bindings,
        compatibility_flags: flags,
        approved_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Human-readable consent lines for CLI/UI.
#[must_use]
pub fn consent_summary(grant: &PluginGrant) -> Vec<String> {
    let mut lines = vec![
        format!("Plugin: {} ({})", grant.plugin_id, grant.kind),
        format!("Network: {}", grant.network_mode),
    ];
    if grant.network_mode == "outbound" && grant.domains.is_empty() {
        lines.push(
            "WARNING: Native outbound has NO hostname allowlist — the jail permits \
             general internet access (TCP/UDP). Prefer workerd when you need domain \
             filtering."
                .into(),
        );
    }
    if !grant.domains.is_empty() {
        lines.push(format!(
            "Workerd initial outbound domains: {}",
            grant.domains.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
        lines
            .push("Redirect hops after an allowed initial host do not require re-approval.".into());
    }
    if !grant.bindings.is_empty() {
        lines.push(format!(
            "Host bindings: {}",
            grant
                .bindings
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !grant.compatibility_flags.is_empty() {
        lines.push(format!(
            "Compatibility flags: {}",
            grant
                .compatibility_flags
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines
}

/// True when an existing grant covers the manifest's current requests.
#[must_use]
pub fn grant_covers(existing: &PluginGrant, requested: &PluginGrant) -> bool {
    existing.network_mode == requested.network_mode
        && existing.domains.is_superset(&requested.domains)
        && existing.bindings.is_superset(&requested.bindings)
        && existing
            .compatibility_flags
            .is_superset(&requested.compatibility_flags)
}

/// Require a covering grant before enable.
pub fn require_grant(files_dir: &Path, manifest: &PluginManifest) -> Result<PluginGrant> {
    let store = PluginGrantStore::load(files_dir)?;
    let requested = consent_request(manifest);
    match store.get(&manifest.id) {
        Some(existing) if grant_covers(existing, &requested) => Ok(existing.clone()),
        Some(_) => Err(PluginError::message(format!(
            "plugin `{}` capabilities widened; re-approve with `bookclerk plugins approve {}`",
            manifest.id, manifest.id
        ))),
        None => Err(PluginError::message(format!(
            "plugin `{}` has no permission grant; run `bookclerk plugins approve {}` first",
            manifest.id, manifest.id
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_grant(domains: &[&str], bindings: &[&str], flags: &[&str]) -> PluginGrant {
        PluginGrant {
            plugin_id: "demo".into(),
            kind: "source".into(),
            network_mode: "outbound".into(),
            domains: domains.iter().map(|s| (*s).to_string()).collect(),
            bindings: bindings.iter().map(|s| (*s).to_string()).collect(),
            compatibility_flags: flags.iter().map(|s| (*s).to_string()).collect(),
            approved_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn grant_covers_rejects_domain_widening() {
        let existing = sample_grant(&["a.example"], &["config"], &[]);
        let requested = sample_grant(&["a.example", "b.example"], &["config"], &[]);
        assert!(!grant_covers(&existing, &requested));
    }

    #[test]
    fn grant_covers_rejects_binding_widening() {
        let existing = sample_grant(&["a.example"], &["config"], &[]);
        let requested = sample_grant(&["a.example"], &["config", "secrets"], &[]);
        assert!(!grant_covers(&existing, &requested));
    }

    #[test]
    fn grant_covers_rejects_flag_widening() {
        let existing = sample_grant(&[], &[], &["nodejs_compat"]);
        let requested = sample_grant(&[], &[], &["nodejs_compat", "streams_enable_constructors"]);
        assert!(!grant_covers(&existing, &requested));
    }

    #[test]
    fn grant_covers_allows_subset_of_approved_capabilities() {
        let existing = sample_grant(
            &["a.example", "b.example"],
            &["config", "secrets"],
            &["nodejs_compat"],
        );
        let requested = sample_grant(&["a.example"], &["config"], &[]);
        assert!(grant_covers(&existing, &requested));
    }

    #[test]
    fn grant_covers_rejects_network_mode_change() {
        let mut existing = sample_grant(&[], &[], &[]);
        existing.network_mode = "deny".into();
        let requested = sample_grant(&[], &[], &[]);
        assert!(!grant_covers(&existing, &requested));
    }
}
