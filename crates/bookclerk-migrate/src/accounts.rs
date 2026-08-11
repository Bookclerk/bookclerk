//! Import classic `AccountsSettings.json` account metadata into the library DB.
//!
//! Credentials are not written here — IdentityTokens conversion was tied to the
//! discarded file-based auth path. Use the Audible plugin (`auth login` /
//! `auth import` of an audible-rs auth file) after migrate.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::error::{MigrateError, Result};

#[derive(Debug, Default)]
pub struct AccountsImportSummary {
    pub accounts: usize,
    /// Always `0` — credentials are not written by migrate.
    pub credentials: usize,
    pub warnings: Vec<String>,
    /// Classic AccountId (email) + locale → canonical account_id used in DB.
    pub account_id_map: HashMap<(String, String), String>,
}

/// Import account metadata from `AccountsSettings.json`.
///
/// `skip_auth` is retained for CLI compatibility; credentials are never sealed
/// here (Audible IdentityTokens are not converted in migrate).
///
/// # Arguments
///
/// * `path` - Filesystem path involved in this operation.
/// * `dest_files_dir` - Filesystem path (`dest_files_dir`).
/// * `force` - When true, overwrite or force a full remote rescan.
/// * `skip_auth` - Boolean flag `skip_auth`.
/// * `dry_run` - Boolean flag `dry_run`.
///
/// # Returns
///
/// On success, the inner `AccountsImportSummary` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn import_accounts(
    path: &Path,
    dest_files_dir: &Path,
    _force: bool,
    _skip_auth: bool,
    dry_run: bool,
) -> Result<AccountsImportSummary> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        MigrateError::Accounts(format!("failed to read {}: {err}", path.display()))
    })?;
    let root: Value = serde_json::from_str(&text)
        .map_err(|err| MigrateError::Accounts(format!("invalid AccountsSettings.json: {err}")))?;

    let accounts = root
        .get("Accounts")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| root.as_array().cloned())
        .ok_or_else(|| {
            MigrateError::Accounts("expected Accounts array in AccountsSettings.json".into())
        })?;

    let store = if dry_run {
        bookclerk_plugin_database_sqlite::open_store_memory().await?
    } else {
        bookclerk_plugin_database_sqlite::open_store(&dest_files_dir.join("library.db")).await?
    };

    let mut summary = AccountsImportSummary::default();
    let mut tokens_seen = 0usize;

    for (idx, acct) in accounts.iter().enumerate() {
        let account_id_classic = acct
            .get("AccountId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if account_id_classic.is_empty() {
            summary
                .warnings
                .push(format!("skipping account[{idx}]: missing AccountId"));
            continue;
        }

        let label = acct
            .get("AccountName")
            .and_then(Value::as_str)
            .map(str::to_string);
        let tokens = acct.get("IdentityTokens");
        let marketplace = tokens
            .and_then(|t| t.get("LocaleName"))
            .and_then(Value::as_str)
            .or_else(|| acct.get("Locale").and_then(Value::as_str))
            .unwrap_or("us")
            .to_string();
        let scan_enabled = acct
            .get("LibraryScan")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        // Prefer Amazon account id from tokens when present (stable key); otherwise
        // keep classic AccountId (usually email) until the operator re-logins.
        let canonical_id = tokens
            .and_then(|t| t.get("AmazonAccountId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(account_id_classic.as_str())
            .to_string();

        if tokens.is_some_and(|t| t.is_object()) {
            tokens_seen += 1;
        }

        if !dry_run {
            store
                .upsert_account(
                    &canonical_id,
                    &marketplace,
                    label.as_deref(),
                    scan_enabled,
                    "audible",
                )
                .await?;
        }

        summary.account_id_map.insert(
            (account_id_classic.clone(), marketplace.clone()),
            canonical_id,
        );
        summary.accounts += 1;
    }

    if tokens_seen > 0 {
        summary.warnings.push(format!(
            "{tokens_seen} account(s) had IdentityTokens — credentials were not imported; \
             re-authenticate with `bookclerk auth login --source audible`, or import an \
             audible-rs auth file via `bookclerk auth import`"
        ));
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn imports_metadata_without_credentials() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("AccountsSettings.json");
        std::fs::write(
            &path,
            r#"{
              "Accounts": [{
                "AccountId": "reader@example.com",
                "AccountName": "Reader",
                "LibraryScan": true,
                "IdentityTokens": {
                  "LocaleName": "us",
                  "AmazonAccountId": "amzn1.account.EXAMPLE",
                  "RefreshToken": {"Value": "rt"}
                }
              }]
            }"#,
        )
        .unwrap();

        let summary = import_accounts(&path, dir.path(), false, false, false)
            .await
            .unwrap();
        assert_eq!(summary.accounts, 1);
        assert_eq!(summary.credentials, 0);
        assert_eq!(
            summary
                .account_id_map
                .get(&("reader@example.com".into(), "us".into()))
                .map(String::as_str),
            Some("amzn1.account.EXAMPLE")
        );
        assert!(summary
            .warnings
            .iter()
            .any(|w| w.contains("IdentityTokens")));
    }
}
