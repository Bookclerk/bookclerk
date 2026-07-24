//! Registry of content sources.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use libation_library::LibraryStore;

use crate::error::{Result, SourceError};
use crate::traits::ContentSource;
use crate::types::{ScanOptions, ScanSummary, SourceAccount, SourceKind};

/// Maps [`SourceKind`] → installed [`ContentSource`] implementations.
#[derive(Clone, Default)]
pub struct SourceRegistry {
    sources: HashMap<SourceKind, Arc<dyn ContentSource>>,
}

impl SourceRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a source implementation.
    pub fn register(&mut self, source: Arc<dyn ContentSource>) {
        self.sources.insert(source.kind(), source);
    }

    /// Look up a source by kind.
    #[must_use]
    pub fn get(&self, kind: SourceKind) -> Option<Arc<dyn ContentSource>> {
        self.sources.get(&kind).cloned()
    }

    /// Require a source or return an error.
    pub fn require(&self, kind: SourceKind) -> Result<Arc<dyn ContentSource>> {
        self.get(kind)
            .ok_or_else(|| SourceError::api(format!("content source `{kind}` is not registered")))
    }

    /// All registered sources in stable order (Audible first, then Libro).
    #[must_use]
    pub fn all(&self) -> Vec<Arc<dyn ContentSource>> {
        let mut kinds: Vec<SourceKind> = self.sources.keys().copied().collect();
        kinds.sort_by_key(|k| match k {
            SourceKind::Audible => 0,
            SourceKind::LibroFm => 1,
            SourceKind::GraphicAudio => 2,
            SourceKind::Chirp => 3,
        });
        kinds
            .into_iter()
            .filter_map(|k| self.sources.get(&k).cloned())
            .collect()
    }

    /// Scan every registered source (honoring per-source account filters).
    ///
    /// When `opts.accounts` is non-empty, each source only receives the subset of
    /// account needles that resolve to an account on that source. Sources with no
    /// matching accounts are skipped instead of failing the whole multi-source scan.
    pub async fn scan_all(
        &self,
        files_dir: &Path,
        library: &LibraryStore,
        opts: ScanOptions,
    ) -> Result<ScanSummary> {
        let mut total = ScanSummary::default();
        let mut any = false;
        for source in self.all() {
            let source_opts =
                match filter_scan_opts_for_source(source.as_ref(), files_dir, &opts).await {
                    Ok(Some(o)) => o,
                    Ok(None) => {
                        tracing::debug!(
                            source = %source.kind(),
                            "skipping source — no matching accounts in filter"
                        );
                        continue;
                    }
                    Err(err) => return Err(err),
                };
            match source.scan(files_dir, library, source_opts).await {
                Ok(summary) => {
                    any = true;
                    total.merge(&summary);
                }
                Err(SourceError::NoAccounts(msg)) => {
                    tracing::debug!(
                        source = %source.kind(),
                        %msg,
                        "skipping source with no accounts"
                    );
                }
                Err(err) => return Err(err),
            }
        }
        if !any && total.accounts == 0 {
            return Err(SourceError::no_accounts(
                "no accounts configured — run `libation auth login` first",
            ));
        }
        Ok(total)
    }
}

/// Returns `None` when an explicit account filter matches nothing on this source.
async fn filter_scan_opts_for_source(
    source: &dyn ContentSource,
    files_dir: &Path,
    opts: &ScanOptions,
) -> Result<Option<ScanOptions>> {
    if opts.accounts.is_empty() {
        return Ok(Some(opts.clone()));
    }
    let accounts = source.list_accounts(files_dir).await?;
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
    use crate::types::{SourceAccount, SourceKind};

    #[test]
    fn account_needle_matches_id_and_label() {
        let accounts = vec![SourceAccount {
            account_id: "libro-user@example.com".into(),
            source: SourceKind::LibroFm,
            marketplace: "us".into(),
            label: Some("Libro Main".into()),
            scan_enabled: true,
        }];
        assert!(account_needle_matches("libro-user@example.com", &accounts));
        assert!(account_needle_matches("LIBRO MAIN", &accounts));
        assert!(!account_needle_matches("audible-only", &accounts));
    }
}
