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

/// Maps source id → installed [`ContentSource`] implementations.
#[derive(Clone, Default)]
pub struct SourceRegistry {
    sources: HashMap<String, Arc<dyn ContentSource>>,
}

impl SourceRegistry {
    /// Empty registry with no sources registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a source implementation.
    pub fn register(&mut self, source: Arc<dyn ContentSource>) {
        let id = source.id().to_string();
        self.sources.insert(id, source);
    }

    /// Look up a source by canonical id or alias.
    #[must_use]
    pub fn get(&self, id_or_alias: &str) -> Option<Arc<dyn ContentSource>> {
        let needle = id_or_alias.trim().to_ascii_lowercase();
        if let Some(s) = self.sources.get(&needle) {
            return Some(s.clone());
        }
        self.sources
            .values()
            .find(|s| {
                s.id().eq_ignore_ascii_case(&needle)
                    || s.aliases().iter().any(|a| a.eq_ignore_ascii_case(&needle))
            })
            .cloned()
    }

    /// Look up a source or return [`crate::SourceError::Api`] when missing.
    ///
    /// # Errors
    ///
    /// Returns an API error when `id_or_alias` is not registered.
    pub fn require(&self, id_or_alias: &str) -> Result<Arc<dyn ContentSource>> {
        self.get(id_or_alias).ok_or_else(|| {
            SourceError::api(format!("content source `{id_or_alias}` is not registered"))
        })
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
    pub async fn scan_all(&self, library: &LibraryStore, opts: ScanOptions) -> Result<ScanSummary> {
        let mut total = ScanSummary::default();
        let mut any = false;
        for source in self.all() {
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
