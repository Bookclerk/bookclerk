//! Account import helpers (DB-backed `encrypted_secrets`).

use std::path::Path;

use bookclerk_library::SourceScope;
use serde::{Deserialize, Serialize};

use crate::auth::load_authenticator;
use crate::db::save_authenticator_to_db;
use crate::error::{AudibleError, Result};

/// Import an audible-rs auth file into the `encrypted_secrets` table.
///
/// Reads (decrypting via `BOOKCLERK_AUTH_PASSWORD` when needed) a user-supplied
/// auth file, then persists the authenticator into the DB. No `Accounts/` file
/// is written.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn import_auth_file(
    scope: &SourceScope,
    source: &Path,
    label: Option<&str>,
    force: bool,
) -> Result<AccountInfo> {
    if !source.is_file() {
        return Err(AudibleError::Import(format!(
            "auth file not found: {}",
            source.display()
        )));
    }

    let auth = load_authenticator(source).await.map_err(|err| {
        AudibleError::Import(format!("could not load {}: {err}", source.display()))
    })?;

    let marketplace = auth.locale().country_code.to_string();
    let customer_id = auth.customer_id().map(str::to_string);
    let account_name = label
        .map(str::to_string)
        .or_else(|| {
            source
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .or_else(|| customer_id.clone())
        .unwrap_or_else(|| marketplace.clone());

    persist_imported_auth(scope, &auth, &account_name, marketplace, customer_id, force).await
}

/// Import mkb79/audible-cli legacy auth JSON (LibationCli: `import-account`).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn import_mkb79_auth_json(
    scope: &SourceScope,
    source: &Path,
    label: Option<&str>,
    force: bool,
) -> Result<AccountInfo> {
    if !source.is_file() {
        return Err(AudibleError::Import(format!(
            "account JSON not found: {}",
            source.display()
        )));
    }

    let auth = audible_rs::auth::Authenticator::import_file(source, None)
        .await
        .map_err(|err| AudibleError::Import(format!("invalid mkb79/audible-cli JSON: {err}")))?;

    let marketplace = auth.locale().country_code.to_string();
    let customer_id = auth.customer_id().map(str::to_string);
    let account_name = label
        .map(str::to_string)
        .or_else(|| {
            source
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .or_else(|| customer_id.clone())
        .unwrap_or_else(|| marketplace.clone());

    let mut info =
        persist_imported_auth(scope, &auth, &account_name, marketplace, customer_id, force).await?;
    info.status = AccountStatus::Valid;
    Ok(info)
}

/// Seals an imported authenticator into `encrypted_secrets`, refusing overwrite unless `force`.
async fn persist_imported_auth(
    scope: &SourceScope,
    auth: &audible_rs::auth::Authenticator,
    account_name: &str,
    marketplace: String,
    customer_id: Option<String>,
    force: bool,
) -> Result<AccountInfo> {
    // Prefer Audible customer id as the secret/account key; `account_name`
    // (label / file stem) is display-only.
    let account_id = customer_id
        .clone()
        .unwrap_or_else(|| account_name.to_string());
    let label = if account_name != account_id {
        Some(account_name.to_string())
    } else {
        None
    };

    if !force {
        let existing = crate::db::load_authenticator_from_db(scope, &account_id).await?;
        if existing.is_some() {
            return Err(AudibleError::Import(format!(
                "audible account `{account_id}` already exists in encrypted_secrets \
                 (pass --force to overwrite)"
            )));
        }
    }

    save_authenticator_to_db(auth, scope, &account_id)
        .await
        .map_err(|err| AudibleError::Import(format!("failed to save auth to DB: {err}")))?;

    scope
        .upsert_account(&account_id, &marketplace, label.as_deref(), true)
        .await
        .map_err(|err| AudibleError::Import(format!("failed to upsert account row: {err}")))?;

    Ok(AccountInfo {
        account_id: account_id.clone(),
        marketplace,
        label,
        status: AccountStatus::Unknown,
        auth_file: Some(format!("encrypted_secrets:{account_id}")),
    })
}

/// Summary of a configured account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    /// Account Identifier.
    pub account_id: String,
    /// Marketplace.
    pub marketplace: String,
    /// Operator-visible label for this account or item.
    pub label: Option<String>,
    /// Status.
    pub status: AccountStatus,
    /// Reference to where credentials live (`encrypted_secrets:<id>`), for display only.
    pub auth_file: Option<String>,
}

/// Token health.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// Valid variant.
    Valid,
    /// Expiring soon variant.
    ExpiringSoon,
    /// Expired variant.
    Expired,
    /// Missing refresh variant.
    MissingRefresh,
    /// Unknown variant.
    Unknown,
}

impl AccountStatus {
    /// As str.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::ExpiringSoon => "expiring_soon",
            Self::Expired => "expired",
            Self::MissingRefresh => "missing_refresh",
            Self::Unknown => "unknown",
        }
    }
}

/// Import classic Libation `AccountsSettings.json` account metadata only.
///
/// IdentityTokens are not converted (discarded file-based auth path). Use
/// [`import_auth_file`] / [`import_mkb79_auth_json`] or interactive login for
/// credentials.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn import_libation_accounts_json(path: &Path) -> Result<Vec<AccountInfo>> {
    let text = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| AudibleError::Import(format!("invalid JSON: {err}")))?;

    let accounts = if let Some(arr) = value.as_array() {
        arr.clone()
    } else if let Some(arr) = value.get("Accounts").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = value.get("accounts").and_then(|v| v.as_array()) {
        arr.clone()
    } else {
        return Err(AudibleError::Import(
            "expected AccountsSettings.json array or object with Accounts".into(),
        ));
    };

    let mut out = Vec::new();
    for (idx, acct) in accounts.iter().enumerate() {
        let account_id = acct
            .get("AccountId")
            .or_else(|| acct.get("account_id"))
            .or_else(|| acct.get("AccountName"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("imported-{idx}"));
        let marketplace = acct
            .get("Locale")
            .or_else(|| acct.get("marketplace"))
            .or_else(|| acct.get("CountryCode"))
            .and_then(|v| v.as_str())
            .unwrap_or("us")
            .to_string();
        let label = acct
            .get("AccountName")
            .or_else(|| acct.get("label"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        out.push(AccountInfo {
            account_id,
            marketplace,
            label,
            status: AccountStatus::Unknown,
            auth_file: None,
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn import_accounts_array() {
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"[{{"AccountId":"abc","Locale":"us","AccountName":"Main"}}]"#
        )
        .unwrap();
        let accounts = import_libation_accounts_json(f.path()).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, "abc");
        assert_eq!(accounts[0].marketplace, "us");
    }
}
