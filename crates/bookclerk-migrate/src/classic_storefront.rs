//! Classic Libation `AccountsSettings.json` writer.
//!
//! Kept off the operator-summary path so CLI stdout/JSON reports a cardinality
//! rather than `list_accounts()` payloads.

use std::path::Path;

use serde_json::{json, Value};

use crate::error::{MigrateError, Result};

/// Writes `AccountsSettings.json` from library identity rows.
///
/// # Arguments
///
/// * `files_dir` - Bookclerk files directory containing `library.db`.
/// * `dest_dir` - Classic Libation Files directory to write into.
///
/// # Returns
///
/// `Ok(())` when the file is written, or when `library.db` is absent.
///
/// # Errors
///
/// Returns an error when listing identities or writing the JSON file fails.
pub async fn write_classic_storefront_file(files_dir: &Path, dest_dir: &Path) -> Result<()> {
    let library_db = files_dir.join("library.db");
    if !library_db.exists() {
        return Ok(());
    }
    let dest = crate::store::DestStore::open(files_dir, false).await?;
    let rows = dest.store.list_accounts().await?;
    let payload = classic_storefront_json(&rows);
    let path = dest_dir.join("AccountsSettings.json");
    let bytes =
        serde_json::to_vec_pretty(&payload).map_err(|e| MigrateError::Accounts(e.to_string()))?;
    std::fs::write(&path, bytes)?;
    Ok(())
}

/// Projects Bookclerk identity rows into classic `AccountsSettings.json` shape.
fn classic_storefront_json(rows: &[bookclerk_library::AccountRecord]) -> Value {
    let list: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "AccountId": row.account_id,
                "AccountName": row.label.clone().unwrap_or_else(|| row.account_id.clone()),
                "IdentityTokens": {
                    "Locale": row.marketplace,
                },
                "LibraryScan": row.scan_enabled,
            })
        })
        .collect();
    json!({ "Accounts": list })
}
