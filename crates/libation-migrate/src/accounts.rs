//! Import classic `AccountsSettings.json`, converting IdentityTokens → `.auth`.

use std::collections::HashMap;
use std::path::Path;

use audible_rs::auth::authfile::KdfParams;
use audible_rs::auth::Authenticator;
use chrono::{DateTime, Utc};
use libation_audible::auth_file_for;
use libation_library::LibraryStore;
use serde_json::Value;

use crate::error::{MigrateError, Result};

#[derive(Debug, Default)]
pub struct AccountsImportSummary {
    pub accounts: usize,
    pub auth_files: usize,
    pub warnings: Vec<String>,
    /// Classic AccountId (email) + locale → canonical account_id used in DB.
    pub account_id_map: HashMap<(String, String), String>,
}

/// Import accounts; when tokens are present, write audible-rs `.auth` files.
pub async fn import_accounts(
    path: &Path,
    dest_files_dir: &Path,
    force: bool,
    skip_auth: bool,
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
        LibraryStore::open_in_memory()?
    } else {
        LibraryStore::open(&dest_files_dir.join("library.db"))?
    };

    let mut summary = AccountsImportSummary::default();

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

        let decrypt_key = acct
            .get("DecryptKey")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let mut canonical_id = account_id_classic.clone();
        let mut wrote_auth = false;

        if !skip_auth {
            match tokens {
                Some(tokens) if tokens.is_object() => {
                    match convert_identity_tokens(tokens, &marketplace, decrypt_key.as_deref()) {
                        Ok(auth) => {
                            if let Some(cid) = auth.customer_id() {
                                canonical_id = cid.to_string();
                            }
                            let stem = label.clone().unwrap_or_else(|| account_id_classic.clone());
                            let dest = auth_file_for(dest_files_dir, &stem);
                            if dest.exists() && !force {
                                summary.warnings.push(format!(
                                    "auth file {} exists (pass --force to overwrite)",
                                    dest.display()
                                ));
                            } else if !dry_run {
                                auth.save_to(&dest, None, KdfParams::default())
                                    .await
                                    .map_err(|err| {
                                        MigrateError::Auth(format!(
                                            "failed to write {}: {err}",
                                            dest.display()
                                        ))
                                    })?;
                                wrote_auth = true;
                            } else {
                                wrote_auth = true;
                            }
                        }
                        Err(err) => {
                            summary.warnings.push(format!(
                                "account {account_id_classic}: could not convert tokens ({err}); \
                                 metadata imported — re-login with `libation auth login`"
                            ));
                        }
                    }
                }
                _ => {
                    summary.warnings.push(format!(
                        "account {account_id_classic}: no IdentityTokens — metadata only"
                    ));
                }
            }
        }

        store.upsert_account(&canonical_id, &marketplace, label.as_deref(), scan_enabled)?;
        summary.account_id_map.insert(
            (account_id_classic.clone(), marketplace.clone()),
            canonical_id,
        );
        summary.accounts += 1;
        if wrote_auth {
            summary.auth_files += 1;
        }
    }

    Ok(summary)
}

/// Convert Libation IdentityTokens JSON into an audible-rs [`Authenticator`]
/// via the legacy Python/audible-cli auth shape (same mapping as Mkb79Auth).
fn convert_identity_tokens(
    tokens: &Value,
    marketplace: &str,
    decrypt_key: Option<&str>,
) -> Result<Authenticator> {
    let access_token = tokens
        .get("ExistingAccessToken")
        .and_then(|t| t.get("TokenValue"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let expires = tokens
        .get("ExistingAccessToken")
        .and_then(|t| t.get("Expires"))
        .and_then(expires_to_unix);
    let refresh_token = extract_secret(tokens.get("RefreshToken"));
    let adp_token = extract_secret(tokens.get("AdpToken"));
    let private_key = extract_secret(tokens.get("PrivateKey"))
        .or_else(|| extract_secret(tokens.get("DevicePrivateKey")));

    let device_serial = tokens
        .get("DeviceSerialNumber")
        .and_then(Value::as_str)
        .map(str::to_string);
    let device_type = tokens
        .get("DeviceType")
        .and_then(Value::as_str)
        .map(str::to_string);
    let device_name = tokens
        .get("DeviceName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let amazon_account_id = tokens
        .get("AmazonAccountId")
        .and_then(Value::as_str)
        .map(str::to_string);

    let website_cookies = cookies_to_map(tokens.get("Cookies"));

    let locale_code = tokens
        .get("LocaleName")
        .and_then(Value::as_str)
        .unwrap_or(marketplace)
        .to_string();

    if refresh_token.is_none() && access_token.is_none() {
        return Err(MigrateError::Auth(
            "IdentityTokens missing refresh/access token".into(),
        ));
    }

    let legacy = serde_json::json!({
        "adp_token": adp_token,
        "access_token": access_token,
        "refresh_token": refresh_token,
        "device_private_key": private_key,
        "expires": expires,
        "locale_code": locale_code,
        "with_username": false,
        "activation_bytes": decrypt_key,
        "website_cookies": website_cookies,
        "device_info": {
            "device_serial_number": device_serial,
            "device_type": device_type,
            "device_name": device_name,
        },
        "customer_info": {
            "user_id": amazon_account_id,
            "account_pool": "Amazon",
        },
    });

    Authenticator::from_legacy_value(legacy).map_err(|err| MigrateError::Auth(err.to_string()))
}

fn extract_secret(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(s) = value.as_str() {
        let trimmed = s.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    value
        .get("Value")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn cookies_to_map(cookies: Option<&Value>) -> Option<HashMap<String, Option<String>>> {
    let arr = cookies?.as_array()?;
    let mut map = HashMap::new();
    for c in arr {
        let Some(key) = c
            .get("Key")
            .or_else(|| c.get("key"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let val = c
            .get("Value")
            .or_else(|| c.get("value"))
            .and_then(Value::as_str)
            .map(str::to_string);
        map.insert(key.to_string(), val);
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

fn expires_to_unix(value: &Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        return Some(n);
    }
    let s = value.as_str()?;
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).timestamp() as f64);
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc().timestamp() as f64);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_secret_from_object_or_string() {
        assert_eq!(
            extract_secret(Some(&serde_json::json!("plain"))),
            Some("plain".into())
        );
        assert_eq!(
            extract_secret(Some(&serde_json::json!({"Value": "wrapped"}))),
            Some("wrapped".into())
        );
    }
}
