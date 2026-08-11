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

/// Spawn/delivery grant limited to the current request surface.
///
/// When a stored grant is a *superset* of what the manifest asks for today,
/// returning the stored snapshot would keep granting bindings (or domains /
/// flags) the current manifest no longer declares. Cap the returned grant to
/// `requested` while preserving identity and `approved_at` from `existing`.
#[must_use]
pub fn effective_grant(existing: &PluginGrant, requested: &PluginGrant) -> PluginGrant {
    PluginGrant {
        plugin_id: existing.plugin_id.clone(),
        kind: existing.kind.clone(),
        network_mode: requested.network_mode.clone(),
        domains: requested.domains.clone(),
        bindings: requested.bindings.clone(),
        compatibility_flags: requested.compatibility_flags.clone(),
        approved_at: existing.approved_at.clone(),
    }
}

/// Require a covering grant before enable **or** every external spawn.
pub fn require_grant(files_dir: &Path, manifest: &PluginManifest) -> Result<PluginGrant> {
    let store = PluginGrantStore::load(files_dir)?;
    let requested = consent_request(manifest);
    match store.get(&manifest.id) {
        Some(existing) if grant_covers(existing, &requested) => {
            Ok(effective_grant(existing, &requested))
        }
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

/// True when `grant.bindings` contains `name` (case-insensitive).
#[must_use]
pub fn grant_has_binding(grant: &PluginGrant, name: &str) -> bool {
    grant
        .bindings
        .iter()
        .any(|binding| binding.eq_ignore_ascii_case(name))
}

/// Fail closed when a delivery site needs a binding the covering grant lacks.
pub fn require_binding(grant: &PluginGrant, name: &str) -> Result<()> {
    if grant_has_binding(grant, name) {
        Ok(())
    } else {
        Err(PluginError::message(format!(
            "plugin `{}` grant lacks binding `{name}`; re-approve with `bookclerk plugins approve {}`",
            grant.plugin_id, grant.plugin_id
        )))
    }
}

/// Handshake `config` payload: non-empty settings only when the grant includes `config`.
#[must_use]
pub fn handshake_config_for_grant(
    grant: &PluginGrant,
    config_table: serde_json::Value,
) -> serde_json::Value {
    if grant_has_binding(grant, "config") {
        config_table
    } else {
        serde_json::json!({})
    }
}

/// Platform guests shipped with the installer (`sqlite`, `local`) skip the
/// consent UX and are enabled by default. Persist a covering grant when the
/// installed manifest stays within the safe platform envelope (deny network,
/// only `config` / `work_fs` bindings).
pub fn ensure_platform_grant(files_dir: &Path, manifest: &PluginManifest) -> Result<PluginGrant> {
    if !is_platform_plugin_id(&manifest.id) {
        return require_grant(files_dir, manifest);
    }
    let requested = consent_request(manifest);
    if !is_safe_platform_request(&requested) {
        return Err(PluginError::message(format!(
            "platform plugin `{}` declares capabilities outside the installer envelope; \
             run `bookclerk plugins approve {}`",
            manifest.id, manifest.id
        )));
    }
    let mut store = PluginGrantStore::load(files_dir)?;
    match store.get(&manifest.id) {
        Some(existing) if grant_covers(existing, &requested) => {
            Ok(effective_grant(existing, &requested))
        }
        Some(_) | None => {
            store.upsert(requested.clone());
            store.save(files_dir)?;
            Ok(requested)
        }
    }
}

/// Resolve the covering grant for spawn: platform auto-grant, else [`require_grant`].
pub fn spawn_grant(files_dir: &Path, manifest: &PluginManifest) -> Result<PluginGrant> {
    if is_platform_plugin_id(&manifest.id) {
        ensure_platform_grant(files_dir, manifest)
    } else {
        require_grant(files_dir, manifest)
    }
}

#[must_use]
pub fn is_platform_plugin_id(id: &str) -> bool {
    matches!(id.to_ascii_lowercase().as_str(), "sqlite" | "local")
}

fn is_safe_platform_request(grant: &PluginGrant) -> bool {
    grant.network_mode == "deny"
        && grant.domains.is_empty()
        && grant.compatibility_flags.is_empty()
        && grant
            .bindings
            .iter()
            .all(|b| b == "config" || b == "work_fs")
}

/// Reject handshake claims that exceed the manifest (and covering grant).
pub fn validate_handshake_capabilities(
    manifest: &PluginManifest,
    grant: &PluginGrant,
    capabilities: &[String],
    portal_auth_mode: Option<&str>,
) -> Result<()> {
    let oauth_mode = portal_auth_mode.is_some_and(|m| m.eq_ignore_ascii_case("oauth"));
    let oauth_methods = capabilities.iter().any(|cap| {
        cap.eq_ignore_ascii_case("loginStart") || cap.eq_ignore_ascii_case("loginComplete")
    });
    if oauth_mode || oauth_methods {
        if !manifest.capabilities.bindings.oauth {
            return Err(PluginError::message(format!(
                "plugin `{}` handshake advertises OAuth without bindings.oauth in plugin.toml",
                manifest.id
            )));
        }
        require_binding(grant, "oauth")?;
    }

    // Lifecycle / entrypoint methods are always implied; kind-specific surfaces
    // in `capabilities.methods` are what consent is meant to bound.
    const CORE_CAPS: &[&str] = &[
        "handshake",
        "shutdown",
        "health",
        "diagnose",
        "start",
        "onEvent",
        "pollEvents",
        "cli",
        "cliDescribe",
        "cliInvoke",
    ];
    let declared = &manifest.capabilities.methods.list;
    if declared.is_empty() {
        return Ok(());
    }
    for cap in capabilities {
        if CORE_CAPS.iter().any(|name| name.eq_ignore_ascii_case(cap)) {
            continue;
        }
        if !declared.iter().any(|name| name.eq_ignore_ascii_case(cap)) {
            return Err(PluginError::message(format!(
                "plugin `{}` handshake advertises capability `{cap}` not listed in \
                 capabilities.methods",
                manifest.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;

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

    #[test]
    fn grant_has_binding_is_case_insensitive() {
        let grant = sample_grant(&[], &["config", "Secrets"], &[]);
        assert!(grant_has_binding(&grant, "config"));
        assert!(grant_has_binding(&grant, "CONFIG"));
        assert!(grant_has_binding(&grant, "secrets"));
        assert!(!grant_has_binding(&grant, "oauth"));
    }

    #[test]
    fn require_binding_fails_closed() {
        let grant = sample_grant(&[], &["config"], &[]);
        assert!(require_binding(&grant, "config").is_ok());
        let err = require_binding(&grant, "secrets").unwrap_err().to_string();
        assert!(err.contains("lacks binding `secrets`"), "{err}");
    }

    #[test]
    fn handshake_config_omitted_without_config_binding() {
        let grant = sample_grant(&[], &["secrets"], &[]);
        let delivered = handshake_config_for_grant(
            &grant,
            serde_json::json!({ "greeting": "hi", "enabled": true }),
        );
        assert_eq!(delivered, serde_json::json!({}));
        let with_config = sample_grant(&[], &["config"], &[]);
        let kept =
            handshake_config_for_grant(&with_config, serde_json::json!({ "greeting": "hi" }));
        assert_eq!(kept["greeting"], "hi");
    }

    #[test]
    fn require_grant_fails_without_store_entry() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let err = require_grant(dir.path(), &manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no permission grant"), "{err}");
    }

    #[test]
    fn require_grant_fails_when_manifest_widens_after_grant() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.network_mode = "deny".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
secrets = true
"#,
        )
        .unwrap();
        let err = require_grant(dir.path(), &manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("capabilities widened"), "{err}");
    }

    #[test]
    fn require_grant_returns_effective_grant_when_manifest_narrows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(
            &["a.example", "b.example"],
            &["config", "secrets", "oauth"],
            &["nodejs_compat"],
        );
        existing.network_mode = "deny".into();
        existing.domains.clear();
        existing.compatibility_flags.clear();
        existing.approved_at = "2026-01-01T00:00:00Z".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let grant = require_grant(dir.path(), &manifest).unwrap();
        assert_eq!(grant.plugin_id, "demo");
        assert_eq!(grant.kind, "source");
        assert_eq!(grant.approved_at, "2026-01-01T00:00:00Z");
        assert_eq!(grant.network_mode, "deny");
        assert!(grant.domains.is_empty());
        assert!(grant.compatibility_flags.is_empty());
        assert_eq!(
            grant.bindings.iter().cloned().collect::<Vec<_>>(),
            vec!["config".to_string()]
        );
        assert!(!grant_has_binding(&grant, "secrets"));
        assert!(!grant_has_binding(&grant, "oauth"));
        // Stored snapshot remains broad; only the returned effective grant narrows.
        let stored = PluginGrantStore::load(dir.path())
            .unwrap()
            .get("demo")
            .unwrap()
            .clone();
        assert!(grant_has_binding(&stored, "secrets"));
    }

    #[test]
    fn platform_grant_auto_persists_for_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "sqlite"
kind = "database"
runtime = "native"
command = "./sqlite"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
work_fs = true
"#,
        )
        .unwrap();
        let grant = ensure_platform_grant(dir.path(), &manifest).unwrap();
        assert!(grant_has_binding(&grant, "config"));
        assert!(grant_has_binding(&grant, "work_fs"));
        // Second call returns an effective grant (same surface, preserved approved_at).
        let again = spawn_grant(dir.path(), &manifest).unwrap();
        assert_eq!(again.plugin_id, "sqlite");
        assert_eq!(again.approved_at, grant.approved_at);
        assert_eq!(again.bindings, grant.bindings);
    }

    #[test]
    fn platform_grant_returns_effective_grant_when_manifest_narrows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config", "work_fs"], &[]);
        existing.plugin_id = "sqlite".into();
        existing.kind = "database".into();
        existing.network_mode = "deny".into();
        existing.approved_at = "2026-02-02T00:00:00Z".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "sqlite"
kind = "database"
runtime = "native"
command = "./sqlite"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let grant = ensure_platform_grant(dir.path(), &manifest).unwrap();
        assert_eq!(grant.approved_at, "2026-02-02T00:00:00Z");
        assert_eq!(
            grant.bindings.iter().cloned().collect::<Vec<_>>(),
            vec!["config".to_string()]
        );
        assert!(!grant_has_binding(&grant, "work_fs"));
    }

    #[test]
    fn platform_grant_fails_outside_installer_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.plugin_id = "sqlite".into();
        existing.kind = "database".into();
        existing.network_mode = "deny".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "sqlite"
kind = "database"
runtime = "native"
command = "./sqlite"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
secrets = true
"#,
        )
        .unwrap();
        let err = ensure_platform_grant(dir.path(), &manifest)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the installer envelope"), "{err}");
    }

    #[test]
    fn effective_grant_preserves_approval_and_takes_request_surface() {
        let existing = sample_grant(
            &["a.example", "b.example"],
            &["config", "secrets"],
            &["nodejs_compat"],
        );
        let requested = sample_grant(&["a.example"], &["config"], &[]);
        let effective = effective_grant(&existing, &requested);
        assert_eq!(effective.plugin_id, existing.plugin_id);
        assert_eq!(effective.kind, existing.kind);
        assert_eq!(effective.approved_at, existing.approved_at);
        assert_eq!(effective.network_mode, requested.network_mode);
        assert_eq!(effective.domains, requested.domains);
        assert_eq!(effective.bindings, requested.bindings);
        assert_eq!(effective.compatibility_flags, requested.compatibility_flags);
    }

    #[test]
    fn require_grant_still_fails_on_widening_after_effective_grant_fix() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PluginGrantStore::default();
        let mut existing = sample_grant(&[], &["config"], &[]);
        existing.network_mode = "deny".into();
        store.upsert(existing);
        store.save(dir.path()).unwrap();

        let narrowed = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let effective = require_grant(dir.path(), &narrowed).unwrap();
        assert!(grant_has_binding(&effective, "config"));

        let widened = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
plugin_kv = true
"#,
        )
        .unwrap();
        let err = require_grant(dir.path(), &widened).unwrap_err().to_string();
        assert!(err.contains("capabilities widened"), "{err}");
    }

    #[test]
    fn validate_handshake_rejects_oauth_without_binding() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "source"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.bindings]
config = true
"#,
        )
        .unwrap();
        let grant = consent_request(&manifest);
        let err = validate_handshake_capabilities(
            &manifest,
            &grant,
            &["loginStart".into()],
            Some("oauth"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("OAuth"), "{err}");
    }

    #[test]
    fn validate_handshake_rejects_undeclared_methods() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "integration"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.methods]
list = ["handshake", "health"]
"#,
        )
        .unwrap();
        let grant = consent_request(&manifest);
        let err = validate_handshake_capabilities(
            &manifest,
            &grant,
            &["handshake".into(), "scanLibrary".into()],
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("scanLibrary"), "{err}");
    }

    #[test]
    fn validate_handshake_allows_core_entrypoint_methods() {
        let manifest = PluginManifest::parse(
            r#"
api_version = 1
id = "demo"
kind = "integration"
runtime = "native"
command = "./demo"

[capabilities.network]
mode = "deny"

[capabilities.methods]
list = ["handshake", "health", "diagnose", "onEvent", "cli"]
"#,
        )
        .unwrap();
        let grant = consent_request(&manifest);
        validate_handshake_capabilities(
            &manifest,
            &grant,
            &[
                "handshake".into(),
                "health".into(),
                "start".into(),
                "cli".into(),
            ],
            None,
        )
        .unwrap();
    }
}
