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

/// Maps PluginKey → installed [`ContentSource`] implementations.
#[derive(Clone, Default)]
pub struct SourceRegistry {
    /// Installed sources keyed by [`ContentSource::plugin_key`].
    sources: HashMap<String, Arc<dyn ContentSource>>,
}

impl SourceRegistry {
    /// Empty registry with no sources registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a source implementation by PluginKey.
    pub fn register(&mut self, source: Arc<dyn ContentSource>) {
        let key = source.plugin_key().to_string();
        self.sources.insert(key, source);
    }

    /// Look up a source by PluginKey, or by its installation-unique display alias.
    ///
    /// Multiple alias hits fail closed (invalid installation state).
    #[must_use]
    pub fn get(&self, id_or_alias: &str) -> Option<Arc<dyn ContentSource>> {
        let mut matches = self.matches(id_or_alias);
        (matches.len() == 1).then(|| matches.remove(0))
    }

    /// Sources whose PluginKey, display alias, or extra aliases match `id_or_alias`.
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
                s.id().eq_ignore_ascii_case(&lower)
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
                let keys: Vec<_> = matches.iter().map(|s| s.plugin_key().to_string()).collect();
                Err(SourceError::api(format!(
                    "plugin alias `{id_or_alias}` is ambiguous; use a provenance-qualified PluginKey. candidates: {}",
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
        sources.sort_by_key(|s| (s.sort_key(), s.id().to_string()));
        sources
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
    use super::account_needle_matches;
    use crate::types::SourceAccount;

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
