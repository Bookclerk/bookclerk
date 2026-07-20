//! Account listing / import helpers.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::auth::{load_authenticator, save_authenticator, SaveAuthOptions};
use crate::error::{AudibleError, Result};
use crate::paths::{auth_file_for, ensure_accounts_dir, list_auth_files, sanitize_name};
use crate::AuthSession;

/// Import an audible-rs `.auth` file into `{files_dir}/Accounts/`.
///
/// Loads (decrypting via env / password file when needed), then re-saves
/// with the configured protection into the canonical Accounts directory.
pub async fn import_auth_file(
    files_dir: &Path,
    source: &Path,
    label: Option<&str>,
    force: bool,
) -> Result<AccountInfo> {
    import_auth_file_with_options(files_dir, source, label, force, SaveAuthOptions::default()).await
}

/// Import with explicit save options (password file / plaintext policy).
pub async fn import_auth_file_with_options(
    files_dir: &Path,
    source: &Path,
    label: Option<&str>,
    force: bool,
    save_opts: SaveAuthOptions<'_>,
) -> Result<AccountInfo> {
    if !source.is_file() {
        return Err(AudibleError::Import(format!(
            "auth file not found: {}",
            source.display()
        )));
    }

    let auth = load_authenticator(source, save_opts.password_file)
        .await
        .map_err(|err| {
            AudibleError::Import(format!("could not load {}: {err}", source.display()))
        })?;

    let marketplace = auth.locale().country_code.to_string();
    let customer_id = auth.customer_id().map(str::to_string);
    let stem = label
        .map(str::to_string)
        .or_else(|| {
            source
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .or_else(|| customer_id.clone())
        .unwrap_or_else(|| marketplace.clone());

    ensure_accounts_dir(files_dir)?;
    let dest = auth_file_for(files_dir, &sanitize_name(&stem));
    if dest.exists() && !force {
        return Err(AudibleError::Import(format!(
            "{} already exists (pass --force to overwrite)",
            dest.display()
        )));
    }

    save_authenticator(&auth, &dest, save_opts).await?;

    let account_id = customer_id.unwrap_or_else(|| stem.clone());
    Ok(AccountInfo {
        account_id,
        marketplace,
        label: Some(stem),
        status: AccountStatus::Unknown,
        auth_file: Some(dest.display().to_string()),
    })
}

/// Summary of a configured account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInfo {
    pub account_id: String,
    pub marketplace: String,
    pub label: Option<String>,
    pub status: AccountStatus,
    pub auth_file: Option<String>,
}

/// Token health.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Valid,
    ExpiringSoon,
    Expired,
    MissingRefresh,
    Unknown,
}

impl AccountStatus {
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

/// List accounts from auth files under `files_dir`, probing token health.
pub async fn list_accounts(files_dir: &Path) -> Result<Vec<AccountInfo>> {
    let mut out = Vec::new();
    for path in list_auth_files(files_dir)? {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("account")
            .to_string();

        match crate::auth::load_authenticator(&path, None).await {
            Ok(auth) => {
                let marketplace = auth.locale().country_code.to_string();
                let account_id = auth
                    .customer_id()
                    .map(str::to_string)
                    .unwrap_or_else(|| stem.clone());
                let client =
                    audible_rs::api::client::Client::new(auth).map_err(AudibleError::from)?;
                let token = client.token_status().await;
                out.push(AccountInfo {
                    account_id,
                    marketplace,
                    label: Some(stem),
                    status: classify_token(&token),
                    auth_file: Some(path.display().to_string()),
                });
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "skipping auth file");
                out.push(AccountInfo {
                    account_id: stem.clone(),
                    marketplace: "unknown".into(),
                    label: Some(stem),
                    status: AccountStatus::Unknown,
                    auth_file: Some(path.display().to_string()),
                });
            }
        }
    }
    Ok(out)
}

fn classify_token(token: &audible_rs::api::client::TokenStatus) -> AccountStatus {
    if !token.has_refresh_token {
        return AccountStatus::MissingRefresh;
    }
    match token.remaining_secs {
        Some(secs) if secs <= 0 => AccountStatus::Expired,
        Some(secs) if secs < 3600 => AccountStatus::ExpiringSoon,
        Some(_) => AccountStatus::Valid,
        None if token.has_access_token => AccountStatus::Unknown,
        None => AccountStatus::Expired,
    }
}

/// Resolve auth file for an account id / label (filename stem match only).
pub fn resolve_auth_file(files_dir: &Path, account: &str) -> Result<std::path::PathBuf> {
    let direct = auth_file_for(files_dir, account);
    if direct.exists() {
        return Ok(direct);
    }
    for path in list_auth_files(files_dir)? {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if stem.eq_ignore_ascii_case(account) {
            return Ok(path);
        }
    }
    Err(AudibleError::AccountNotFound(account.into()))
}

/// Resolve auth file by stem **or** by matching `customer_id` inside the file.
///
/// Library rows store Audible `customer_id` as `account_id`, while auth files may
/// be named with a user label (`main.auth`). Prefer stem match, then probe files.
pub async fn resolve_auth_file_async(
    files_dir: &Path,
    account: &str,
) -> Result<std::path::PathBuf> {
    if let Ok(path) = resolve_auth_file(files_dir, account) {
        return Ok(path);
    }

    for path in list_auth_files(files_dir)? {
        match crate::auth::load_authenticator(&path, None).await {
            Ok(auth) => {
                if auth
                    .customer_id()
                    .is_some_and(|id| id.eq_ignore_ascii_case(account))
                {
                    return Ok(path);
                }
            }
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %err,
                    "skipping unreadable auth file during account resolve"
                );
            }
        }
    }
    Err(AudibleError::AccountNotFound(account.into()))
}

/// Import mkb79/audible-cli legacy auth JSON (LibationCli: `import-account`).
pub async fn import_mkb79_auth_json(
    files_dir: &Path,
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
    let stem = label
        .map(str::to_string)
        .or_else(|| {
            source
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .or_else(|| customer_id.clone())
        .unwrap_or_else(|| marketplace.clone());

    ensure_accounts_dir(files_dir)?;
    let dest = auth_file_for(files_dir, &sanitize_name(&stem));
    if dest.exists() && !force {
        return Err(AudibleError::Import(format!(
            "{} already exists (pass --force to overwrite)",
            dest.display()
        )));
    }

    save_authenticator(&auth, &dest, SaveAuthOptions::default())
        .await
        .map_err(|err| AudibleError::Import(format!("failed to save auth: {err}")))?;

    let account_id = customer_id.unwrap_or_else(|| stem.clone());
    Ok(AccountInfo {
        account_id,
        marketplace,
        label: Some(stem),
        status: AccountStatus::Valid,
        auth_file: Some(dest.display().to_string()),
    })
}

/// Import Libation `AccountsSettings.json` metadata (auth material still via audible import).
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

/// Register a just-logged-in session into the account list shape.
#[must_use]
pub fn session_to_info(session: &AuthSession) -> AccountInfo {
    AccountInfo {
        account_id: session.account_id.clone(),
        marketplace: session.marketplace.clone(),
        label: session.label.clone(),
        status: AccountStatus::Valid,
        auth_file: Some(session.auth_file.display().to_string()),
    }
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
