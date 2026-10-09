//! In-process registry of installed [`crate::ContentSource`] implementations.
//!
//! # Audience
//!
//! Host startup / CLI code that registers first-party sources and runs
//! multi-source scans.

use std::collections::HashMap;
use std::sync::Arc;

use bookclerk_library::{LibraryStore, SourceScope};

use crate::error::{Result, SourceError};
use crate::traits::ContentSource;
use crate::types::{ScanOptions, ScanSummary, SourceAccount};

/// Maps a registry address to an installed [`ContentSource`].
///
/// A deployed source is addressed by [`ContentSource::plugin_instance_id`].
/// A source with no instance id is addressed by [`ContentSource::plugin_key`].
/// Plugin key and display alias resolve only when exactly one source matches.
#[derive(Clone, Default)]
pub struct SourceRegistry {
    /// Installed sources keyed by instance id, or by plugin key when unset.
    sources: HashMap<String, Arc<dyn ContentSource>>,
}

impl SourceRegistry {
    /// Empty registry with no sources registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a source at its instance id, or its PluginKey.
    ///
    /// A second instance of the same plugin key stays in the map. Re-registering
    /// the same instance id replaces that guest only.
    pub fn register(&mut self, source: Arc<dyn ContentSource>) {
        let key = registry_address(source.as_ref()).to_string();
        self.sources.insert(key, source);
    }

    /// Drops the source registered under `plugin_instance_id`.
    ///
    /// Other instances of the same plugin key stay. Returns whether a row was removed.
    pub fn remove_instance(&mut self, plugin_instance_id: &str) -> bool {
        if plugin_instance_id.is_empty() {
            return false;
        }
        self.sources.remove(plugin_instance_id).is_some()
    }

    /// Look up a source by plugin instance id, PluginKey, or display alias.
    ///
    /// An instance id matches that deployed source. A plugin key or alias
    /// matches only when exactly one registered source has it. Several matches
    /// return `None`; [`Self::require`] reports the ambiguity.
    #[must_use]
    pub fn get(&self, id_or_alias: &str) -> Option<Arc<dyn ContentSource>> {
        let mut matches = self.matches(id_or_alias);
        (matches.len() == 1).then(|| matches.remove(0))
    }

    /// Sources whose instance id, PluginKey, display alias, or extra aliases match.
    ///
    /// An exact registry address (instance id, or plugin key for a source with
    /// no instance) is one hit. Otherwise every key and alias match is returned
    /// so two instances of one key stay ambiguous.
    fn matches(&self, id_or_alias: &str) -> Vec<Arc<dyn ContentSource>> {
        let needle = id_or_alias.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        if let Some(s) = self.sources.get(needle) {
            return vec![s.clone()];
        }
        let lower = needle.to_ascii_lowercase();
        self.sources
            .values()
            .filter(|s| {
                s.plugin_instance_id().is_some_and(|id| id == needle)
                    || s.plugin_key() == needle
                    || s.id().eq_ignore_ascii_case(&lower)
                    || s.aliases().iter().any(|a| a.eq_ignore_ascii_case(&lower))
            })
            .cloned()
            .collect()
    }

    /// Look up a source or return [`crate::SourceError::Api`] when missing.
    ///
    /// # Errors
    ///
    /// Returns an API error when `id_or_alias` is not registered.
    pub fn require(&self, id_or_alias: &str) -> Result<Arc<dyn ContentSource>> {
        let mut matches = self.matches(id_or_alias);
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(SourceError::api(format!(
                "content source `{id_or_alias}` is not registered"
            ))),
            _ => {
                let keys: Vec<_> = matches
                    .iter()
                    .map(|s| match s.plugin_instance_id() {
                        Some(id) => format!("{id} ({})", s.plugin_key()),
                        None => s.plugin_key().to_string(),
                    })
                    .collect();
                Err(SourceError::api(format!(
                    "plugin alias `{id_or_alias}` is ambiguous; use a plugin instance id or a provenance-qualified PluginKey. candidates: {}",
                    keys.join(", ")
                )))
            }
        }
    }

    /// Resolve a needle to the canonical plugin id when registered.
    #[must_use]
    pub fn resolve_id(&self, id_or_alias: &str) -> Option<String> {
        self.get(id_or_alias).map(|s| s.id().to_string())
    }

    /// All registered sources in stable plugin order.
    #[must_use]
    pub fn all(&self) -> Vec<Arc<dyn ContentSource>> {
        let mut sources: Vec<_> = self.sources.values().cloned().collect();
        sources.sort_by_key(|s| {
            (
                s.sort_key(),
                s.id().to_string(),
                s.plugin_instance_id().unwrap_or("").to_string(),
            )
        });
        sources
    }

    /// Two instances of one plugin key cannot share a scan.
    ///
    /// Accounts and [`crate::SourceScope`] stay keyed by storefront id, so
    /// scanning both would read and write the same rows twice. The caller
    /// passes one plugin instance id instead.
    fn ambiguous_same_key_scan(&self) -> Option<String> {
        let mut by_key: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for source in self.sources.values() {
            let label = match source.plugin_instance_id() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => source.id().to_string(),
            };
            by_key
                .entry(source.plugin_key().to_string())
                .or_default()
                .push(label);
        }
        let problems: Vec<_> = by_key
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(key, ids)| format!("`{key}` ({})", ids.join(", ")))
            .collect();
        if problems.is_empty() {
            None
        } else {
            Some(format!(
                "scan is ambiguous for {}; pass a plugin instance id. Accounts stay keyed by storefront id",
                problems.join("; ")
            ))
        }
    }

    /// Scan every registered source (honoring per-source account filters).
    ///
    /// When `opts.accounts` is non-empty, each source only receives the subset of
    /// account needles that resolve to an account on that source. Sources with no
    /// matching accounts are skipped instead of failing the whole multi-source scan.
    /// [`ScanOptions::cancel`] is checked between sources.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn scan_all(&self, library: &LibraryStore, opts: ScanOptions) -> Result<ScanSummary> {
        if let Some(detail) = self.ambiguous_same_key_scan() {
            return Err(SourceError::api(detail));
        }
        let mut total = ScanSummary::default();
        let mut any = false;
        for source in self.all() {
            if opts.is_cancelled() {
                return Err(crate::error::SourceError::Other(anyhow::anyhow!(
                    "cancelled"
                )));
            }
            let scope = library.scope(source.id());
            let source_opts =
                match filter_scan_opts_for_source(source.as_ref(), &scope, &opts).await {
                    Ok(Some(o)) => o,
                    Ok(None) => {
                        tracing::debug!(
                            source = %source.id(),
                            "skipping source — no matching accounts in filter"
                        );
                        continue;
                    }
                    Err(err) => return Err(err),
                };
            match source.scan(&scope, source_opts).await {
                Ok(summary) => {
                    any = true;
                    total.merge(&summary);
                }
                Err(SourceError::NoAccounts(msg)) => {
                    tracing::debug!(
                        source = %source.id(),
                        %msg,
                        "skipping source with no accounts"
                    );
                }
                Err(err) => return Err(err),
            }
        }
        if !any && total.accounts == 0 {
            return Err(SourceError::no_accounts(
                "no accounts configured — connect a store in the Bookclerk Accounts UI",
            ));
        }
        Ok(total)
    }
}

/// Returns `None` when an explicit account filter matches nothing on this source.
async fn filter_scan_opts_for_source(
    source: &dyn ContentSource,
    scope: &SourceScope,
    opts: &ScanOptions,
) -> Result<Option<ScanOptions>> {
    if opts.accounts.is_empty() {
        return Ok(Some(opts.clone()));
    }
    let accounts = source.list_accounts(scope).await?;
    let filtered: Vec<String> = opts
        .accounts
        .iter()
        .filter(|needle| account_needle_matches(needle, &accounts))
        .cloned()
        .collect();
    if filtered.is_empty() {
        return Ok(None);
    }
    let mut out = opts.clone();
    out.accounts = filtered;
    Ok(Some(out))
}

/// Instance id when the reconciler deployed this source, otherwise its plugin key.
fn registry_address(source: &dyn ContentSource) -> &str {
    match source.plugin_instance_id() {
        Some(id) if !id.is_empty() => id,
        _ => source.plugin_key(),
    }
}

/// True when `needle` matches an account id or display label, ignoring ASCII case.
fn account_needle_matches(needle: &str, accounts: &[SourceAccount]) -> bool {
    accounts.iter().any(|a| {
        a.account_id.eq_ignore_ascii_case(needle)
            || a.label
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case(needle))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::{account_needle_matches, SourceRegistry};
    use crate::brand::SourceBrand;
    use crate::traits::{ContentSource, PortalAuthMode};
    use crate::types::SourceAccount;
    use crate::Result;

    const BRAND: SourceBrand = SourceBrand {
        id: "stub",
        name: "Stub",
        bg: "#000000",
        fg: "#ffffff",
        accent: "#111111",
        icon_url: "https://example.invalid/icon",
    };

    struct Stub {
        id: &'static str,
        key: &'static str,
        instance: Option<&'static str>,
        aliases: &'static [&'static str],
    }

    #[async_trait]
    impl ContentSource for Stub {
        fn id(&self) -> &str {
            self.id
        }

        fn plugin_key(&self) -> &str {
            self.key
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            self.instance
        }

        fn aliases(&self) -> &'static [&'static str] {
            self.aliases
        }

        fn portal_auth_mode(&self) -> PortalAuthMode {
            PortalAuthMode::Password
        }

        fn portal_brand(&self) -> SourceBrand {
            BRAND
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: crate::types::LoginOptions,
        ) -> Result<crate::types::SourceAccount> {
            unimplemented!("lookup stub")
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> Result<Vec<crate::types::SourceAccount>> {
            unimplemented!("lookup stub")
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: crate::types::ScanOptions,
        ) -> Result<crate::types::ScanSummary> {
            unimplemented!("lookup stub")
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            _opts: &crate::types::FetchOptions,
        ) -> Result<crate::types::SourceFetch> {
            unimplemented!("lookup stub")
        }
    }

    fn stub(
        id: &'static str,
        key: &'static str,
        instance: Option<&'static str>,
        aliases: &'static [&'static str],
    ) -> Arc<dyn ContentSource> {
        Arc::new(Stub {
            id,
            key,
            instance,
            aliases,
        })
    }

    #[test]
    fn legacy_key_and_alias_resolve_a_single_source() {
        let mut registry = SourceRegistry::new();
        registry.register(stub("graphicaudio", "local/graphicaudio", None, &["ga"]));
        assert_eq!(
            registry.get("local/graphicaudio").unwrap().id(),
            "graphicaudio"
        );
        assert_eq!(
            registry.get("GA").unwrap().plugin_key(),
            "local/graphicaudio"
        );
    }

    #[test]
    fn two_instances_of_one_key_resolve_by_instance_id_only() {
        let mut registry = SourceRegistry::new();
        registry.register(stub(
            "graphicaudio",
            "local/graphicaudio",
            Some("instance-a"),
            &["ga"],
        ));
        registry.register(stub(
            "graphicaudio",
            "local/graphicaudio",
            Some("instance-b"),
            &["ga"],
        ));
        assert_eq!(
            registry.get("instance-a").unwrap().plugin_instance_id(),
            Some("instance-a")
        );
        assert_eq!(
            registry.get("instance-b").unwrap().plugin_instance_id(),
            Some("instance-b")
        );
        assert!(registry.get("local/graphicaudio").is_none());
        assert!(registry.get("graphicaudio").is_none());
        assert!(registry.get("ga").is_none());
        let err = match registry.require("graphicaudio") {
            Err(err) => err.to_string(),
            Ok(_) => panic!("plugin key must be ambiguous"),
        };
        assert!(err.contains("plugin instance id"), "{err}");
        assert!(err.contains("instance-a"), "{err}");
        assert!(err.contains("instance-b"), "{err}");

        registry.register(stub(
            "replaced",
            "local/graphicaudio",
            Some("instance-a"),
            &[],
        ));
        assert_eq!(registry.get("instance-a").unwrap().id(), "replaced");
        assert_eq!(
            registry.get("instance-b").unwrap().plugin_instance_id(),
            Some("instance-b")
        );
    }

    #[test]
    fn account_needle_matches_id_and_label() {
        let accounts = vec![SourceAccount {
            account_id: "libro-user@example.com".into(),
            source: "libro".into(),
            marketplace: "us".into(),
            label: Some("Libro Main".into()),
            scan_enabled: true,
        }];
        assert!(account_needle_matches("libro-user@example.com", &accounts));
        assert!(account_needle_matches("LIBRO MAIN", &accounts));
        assert!(!account_needle_matches("audible-only", &accounts));
    }
}
