//! `bookclerk config database setup d1` — the hosted database's guided
//! setup.
//!
//! The D1 plugin's `provisioning` entrypoint runs with the operator's
//! pasted Cloudflare token: it finds or creates the database and mints a
//! token limited to D1. This command then seals that token with the
//! library's data-encryption key and keeps it in the master key's store
//! (`bookclerk_library::database_credential`), and writes the account and
//! database ids to `[database.d1]` in config.toml. The pasted token is used
//! for this run only.
//!
//! A re-run keeps the token an earlier run minted, since shared branches
//! of the library hold a copy of it; `--rotate-token`, or a different
//! account, mints a new one and deletes the old.

use std::collections::BTreeMap;

use bookclerk_config::{Config, KeyStoreKind};
use bookclerk_library::database_credential::{
    load_database_credential, store_database_credential, DatabaseCredential,
};
use bookclerk_library::MasterKey;
use bookclerk_plugin_abi::provision::CheckedProvision;
use bookclerk_plugin_host::{provision_bootstrap, ProvisionToken};
use serde_json::Value;

use crate::format_out::{emit, OutputFormat};

/// The plugin id of the D1 database guest.
const D1_PLUGIN: &str = "d1";

/// A hosted database the setup knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SetupBackend {
    /// Cloudflare D1.
    D1,
}

/// The operator's choices, each optional.
#[derive(Debug, Clone, Default)]
pub struct SetupChoices {
    /// Cloudflare account id.
    pub account_id: Option<String>,
    /// D1 database name.
    pub database_name: Option<String>,
    /// `default`, `eu` or `fedramp`.
    pub jurisdiction: Option<String>,
    /// Mint a new token even when an earlier run minted one.
    pub rotate_token: bool,
}

impl SetupChoices {
    /// The choices as the plugin's `[provisioning] inputs`, for a library
    /// whose `[database.d1]` names `configured_account` (empty when none).
    ///
    /// A different account always rotates: the earlier token is limited to
    /// the account it was minted in.
    fn inputs(&self, configured_account: &str) -> BTreeMap<String, Value> {
        let mut inputs: BTreeMap<String, Value> = [
            ("account_id", &self.account_id),
            ("database_name", &self.database_name),
            ("jurisdiction", &self.jurisdiction),
        ]
        .into_iter()
        .filter_map(|(key, value)| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| (key.to_string(), Value::from(value)))
        })
        .collect();
        let configured = configured_account.trim();
        let other_account = inputs
            .get("account_id")
            .and_then(Value::as_str)
            .is_some_and(|chosen| {
                !configured.is_empty() && !chosen.eq_ignore_ascii_case(configured)
            });
        if self.rotate_token || other_account {
            inputs.insert("rotate_token".into(), Value::Bool(true));
        }
        inputs
    }
}

/// What the plugin's setup produced, as this command uses it.
#[derive(Debug)]
struct D1Outputs {
    /// Cloudflare account id.
    account_id: String,
    /// The database's UUID.
    database_id: String,
    /// The database's name, for the summary.
    database_name: String,
    /// The newly minted token and its id, or `None` when the setup kept
    /// the earlier one.
    credential: Option<DatabaseCredential>,
    /// Lines for the operator.
    summary: Vec<String>,
}

/// Reads the setup's checked outputs.
///
/// # Errors
///
/// Returns an error when an output this command needs is missing.
fn d1_outputs(checked: &CheckedProvision) -> anyhow::Result<D1Outputs> {
    let text = |key: &str| -> anyhow::Result<String> {
        checked
            .settings
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("the D1 setup returned no `{key}`"))
    };
    let token_id = text("api_token_id").ok();
    let credential = checked
        .secrets
        .get("api_token")
        .map(|token| DatabaseCredential::new(token.expose(), token_id.as_deref()))
        .transpose()?;
    Ok(D1Outputs {
        account_id: text("account_id")?,
        database_id: text("database_id")?,
        database_name: text("database_name").unwrap_or_default(),
        credential,
        summary: checked.summary.clone(),
    })
}

/// What [`apply_d1_setup`] wrote.
#[derive(Debug)]
struct Applied {
    /// The store a newly minted token went to, or `None` when the earlier
    /// token was kept.
    store: Option<KeyStoreKind>,
    /// The config.toml written.
    config_file: std::path::PathBuf,
}

/// Keeps a newly minted token and writes `[database.d1]`.
///
/// The credential is stored first, so config.toml never names a database
/// this install holds no token for. A setup that kept the earlier token
/// needs this install to hold one already.
///
/// # Errors
///
/// Returns an error when the credential can't be stored, the earlier token
/// is kept but this install has none, or the config can't be written.
fn apply_d1_setup(
    config: &Config,
    dek: &MasterKey,
    outputs: &D1Outputs,
) -> anyhow::Result<Applied> {
    let files_dir = &config.paths().files_dir;
    let store = match &outputs.credential {
        Some(credential) => Some(store_database_credential(files_dir, dek, credential)?),
        None => {
            if load_database_credential(files_dir, dek)?.is_none()
                && bookclerk_config::resolve_d1_api_token().is_err()
            {
                anyhow::bail!(
                    "the D1 setup kept the earlier token, but this server holds none; run it again with --rotate-token"
                );
            }
            None
        }
    };
    let mut cfg = config.clone();
    cfg.database.d1.account_id.clone_from(&outputs.account_id);
    cfg.database.d1.database_id.clone_from(&outputs.database_id);
    let config_file = cfg.paths().config_file.clone();
    cfg.write_toml_file(&config_file)?;
    Ok(Applied { store, config_file })
}

/// The settings the plugin starts from: what config.toml and the stored
/// credential already say, so a re-run reuses the account and keeps (or,
/// when asked, replaces) the earlier token.
fn current_settings(config: &Config, dek: &MasterKey) -> serde_json::Map<String, Value> {
    let mut settings = serde_json::Map::new();
    let d1 = &config.database.d1;
    if !d1.account_id.trim().is_empty() {
        settings.insert("account_id".into(), Value::from(d1.account_id.trim()));
    }
    if !d1.database_id.trim().is_empty() {
        settings.insert("database_id".into(), Value::from(d1.database_id.trim()));
    }
    // An unreadable earlier credential only means a new token is minted and
    // the old one is left for the operator to delete.
    if let Ok(Some(previous)) = load_database_credential(&config.paths().files_dir, dek) {
        if let Some(id) = previous.token_id() {
            settings.insert("api_token_id".into(), Value::from(id));
        }
    }
    settings
}

/// How a store is named to the operator.
fn store_label(store: KeyStoreKind) -> &'static str {
    match store {
        KeyStoreKind::File => "database.credential beside master.key",
        KeyStoreKind::Keychain => "OS keychain",
        KeyStoreKind::SystemdCreds => "systemd-creds credential (TPM)",
    }
}

/// Runs the guided setup for `backend` with the pasted `token`.
///
/// # Errors
///
/// Returns an error when the token is unusable, the D1 plugin isn't
/// installed, the setup fails, or its outputs can't be kept.
pub async fn run(
    config: &Config,
    format: OutputFormat,
    backend: SetupBackend,
    token: &str,
    choices: SetupChoices,
) -> anyhow::Result<()> {
    match backend {
        SetupBackend::D1 => {}
    }
    let token = ProvisionToken::parse(token).map_err(|err| anyhow::anyhow!("{err}"))?;
    let plugins = bookclerk_plugin_host::discover_plugins(config)?;
    let plugin = bookclerk_plugin_host::resolve_plugin_ref(&plugins, D1_PLUGIN).map_err(|err| {
        anyhow::anyhow!(
            "{err}; install the D1 plugin first (`cargo stage-plugins --plugin d1` in a checkout)"
        )
    })?;
    let files_dir = &config.paths().files_dir;
    let dek =
        bookclerk_library::configure_master_key_with(files_dir, config.auth_password().as_deref())?;
    let settings = current_settings(config, &dek);
    let checked = provision_bootstrap(
        config,
        plugin,
        &settings,
        &token,
        &choices.inputs(&config.database.d1.account_id),
    )
    .await
    .map_err(|err| anyhow::anyhow!("the D1 setup failed: {err}"))?;
    let outputs = d1_outputs(&checked)?;
    let applied = apply_d1_setup(config, &dek, &outputs)?;
    let active = config.database.plugin.trim() == D1_PLUGIN;
    let payload = serde_json::json!({
        "account_id": outputs.account_id,
        "database_id": outputs.database_id,
        "database_name": outputs.database_name,
        "credential_store": applied.store.map(KeyStoreKind::as_str),
        "rotated": applied.store.is_some(),
        "config": applied.config_file.display().to_string(),
        "active": active,
        "summary": outputs.summary,
    });
    emit(format, &payload, || {
        for line in &outputs.summary {
            println!("{line}");
        }
        match applied.store {
            Some(store) => println!(
                "kept the D1 token in the {}, sealed with the library's key",
                store_label(store)
            ),
            None => println!("kept the D1 token this server already holds"),
        }
        println!(
            "set database.d1.account_id and database.d1.database_id={} in {}",
            outputs.database_id,
            applied.config_file.display()
        );
        if !active {
            println!(
                "the library still uses `{}`; move it with `bookclerk config database migrate --to d1 --apply`",
                config.database.plugin
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config for a fresh files directory.
    fn config(dir: &std::path::Path) -> Config {
        Config::load(Some(dir.to_path_buf()), None).expect("config")
    }

    #[test]
    fn choices_become_inputs_without_blanks() {
        let choices = SetupChoices {
            account_id: Some(" 3f4b ".into()),
            database_name: Some("   ".into()),
            jurisdiction: None,
            rotate_token: false,
        };
        let inputs = choices.inputs("3F4B");
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs["account_id"], "3f4b");
        assert!(SetupChoices::default().inputs("").is_empty());
        assert!(!choices.inputs("").contains_key("rotate_token"));
        // Another account, or asking, rotates the token.
        assert_eq!(choices.inputs("aaaa")["rotate_token"], true);
        let asked = SetupChoices {
            rotate_token: true,
            ..SetupChoices::default()
        };
        assert_eq!(asked.inputs("aaaa")["rotate_token"], true);
    }

    #[test]
    fn applying_stores_the_token_then_writes_the_ids() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let dek = bookclerk_library::master_key::resolve_master_key_with(dir.path(), None).unwrap();
        let outputs = D1Outputs {
            account_id: "3f4b7e3dcab231cbfdaa90a6a28bd548".into(),
            database_id: "0c3a1d2e-4b5f-4a6b-8c7d-9e0f1a2b3c4d".into(),
            database_name: "bookclerk".into(),
            credential: Some(DatabaseCredential::new("minted", Some("tok_1")).unwrap()),
            summary: vec![],
        };
        let applied = apply_d1_setup(&config, &dek, &outputs).unwrap();
        assert_eq!(applied.store, Some(KeyStoreKind::File));
        let written = Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        assert_eq!(written.database.d1.account_id, outputs.account_id);
        assert_eq!(written.database.d1.database_id, outputs.database_id);
        let text = std::fs::read_to_string(&applied.config_file).unwrap();
        assert!(!text.contains("minted"), "the token is not in config.toml");

        // A re-run starts from what was written, with the earlier token's id.
        let settings = current_settings(&written, &dek);
        assert_eq!(settings["account_id"], outputs.account_id.as_str());
        assert_eq!(settings["database_id"], outputs.database_id.as_str());
        assert_eq!(settings["api_token_id"], "tok_1");
        assert!(current_settings(&config, &dek).get("account_id").is_none());

        // A re-run that kept the earlier token writes the ids and leaves
        // the stored token as it was.
        let kept = D1Outputs {
            credential: None,
            database_id: "1d4b2e3f-5c6a-4b7c-9d8e-0f1a2b3c4d5e".into(),
            ..outputs
        };
        let applied = apply_d1_setup(&written, &dek, &kept).unwrap();
        assert_eq!(applied.store, None);
        let rewritten = Config::load(Some(dir.path().to_path_buf()), None).unwrap();
        assert_eq!(rewritten.database.d1.database_id, kept.database_id);
        let still = load_database_credential(dir.path(), &dek).unwrap().unwrap();
        assert_eq!(still.token(), "minted");

        // Kept, but this server holds no token: refused.
        let fresh = tempfile::tempdir().unwrap();
        let fresh_config = Config::load(Some(fresh.path().to_path_buf()), None).unwrap();
        let fresh_dek =
            bookclerk_library::master_key::resolve_master_key_with(fresh.path(), None).unwrap();
        if bookclerk_config::resolve_d1_api_token().is_err() {
            let err = apply_d1_setup(&fresh_config, &fresh_dek, &kept).unwrap_err();
            assert!(err.to_string().contains("--rotate-token"), "{err}");
        }
        for store in [
            KeyStoreKind::File,
            KeyStoreKind::Keychain,
            KeyStoreKind::SystemdCreds,
        ] {
            assert!(!store_label(store).is_empty());
        }
    }

    #[tokio::test]
    async fn a_bad_token_or_a_missing_plugin_is_refused_before_any_run() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let err = run(
            &config,
            OutputFormat::Text,
            SetupBackend::D1,
            "has space",
            SetupChoices::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("space"), "{err}");
        // Only meaningful where no D1 guest is staged for this process.
        let staged = bookclerk_plugin_host::discover_plugins(&config)
            .unwrap()
            .iter()
            .any(|plugin| plugin.manifest.id == D1_PLUGIN);
        if !staged {
            let err = run(
                &config,
                OutputFormat::Text,
                SetupBackend::D1,
                "token",
                SetupChoices::default(),
            )
            .await
            .unwrap_err();
            assert!(err.to_string().contains("D1 plugin"), "{err}");
        }
        assert!(
            !bookclerk_library::master_key_path(dir.path()).exists(),
            "no key is minted before the plugin is found"
        );
    }
}
