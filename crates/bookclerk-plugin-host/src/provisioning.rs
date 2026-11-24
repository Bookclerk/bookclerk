//! Guided setup runs: one short-lived guest per `provision` or
//! `deprovision`.
//!
//! The operator hands the host a short-lived grant (a pasted API token).
//! The host spawns a fresh guest for the instance under the plugin's normal
//! grant, network included, opens it with
//! [`InvocationPurpose::Provision`] so required settings may still be
//! missing, calls the `provisioning` entrypoint once and lets the guest go.
//! What the guest returns is checked against `[provisioning] outputs`
//! ([`check_provisioned`]) before the caller stores anything. The grant is
//! never stored or logged; it is registered with log redaction for the
//! life of the process.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    load_plugin_instance, resolve_instance_bindings_with, PluginInstanceId, RequiredSettings,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_abi::provision::{
    check_provisioned, CheckedProvision, ProvisionOutputs, ProvisionRefusal,
};
use bookclerk_plugin_manifest::ProvisionGrantType;
use bookclerk_plugin_sdk::{
    BindingValues, DeprovisionRequest, ExtensibleConfig, InvocationPurpose, PluginError,
    ProvisionGrant, ProvisionGrantKind, ProvisionRequest, MAX_PROVISION_SETTINGS_BYTES,
    MAX_PROVISION_TOKEN_BYTES,
};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::rpc_session::{PluginSession, SessionServices, HOST_SHARED_ACCOUNT};

/// Longest a `provision` or `deprovision` call may take: creating cloud
/// resources and minting a token are a handful of API calls.
pub const PROVISION_TIMEOUT: Duration = Duration::from_secs(120);

/// A token the operator pasted for one guided setup run. `Debug` never
/// prints it, and constructing one registers it with log redaction.
#[derive(Clone, PartialEq, Eq)]
pub struct ProvisionToken(String);

impl ProvisionToken {
    /// Checks and keeps a pasted token.
    ///
    /// # Arguments
    ///
    /// * `token` - The text the operator pasted; surrounding space is
    ///   trimmed.
    ///
    /// # Errors
    ///
    /// Returns a message when the token is empty, longer than
    /// `maxProvisionTokenBytes`, or holds a space or control character.
    pub fn parse(token: &str) -> Result<Self, String> {
        let token = token.trim();
        if token.is_empty() {
            return Err("the token is empty".into());
        }
        if token.len() > MAX_PROVISION_TOKEN_BYTES as usize {
            return Err(format!(
                "the token is longer than {MAX_PROVISION_TOKEN_BYTES} bytes"
            ));
        }
        if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("the token holds a space or a control character".into());
        }
        bookclerk_config::register_secret(token);
        Ok(Self(token.to_string()))
    }

    /// The token's text, for the plugin.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The grant this token is on the wire.
    fn grant(&self) -> ProvisionGrant {
        ProvisionGrant {
            kind: ProvisionGrantKind::ApiToken,
            token: Some(self.0.clone()),
            expires_at_unix_ms: None,
        }
    }
}

impl std::fmt::Debug for ProvisionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProvisionToken(<redacted>)")
    }
}

/// Why a guided setup run did not produce something to store.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionRunError {
    /// The plugin offers no guided setup, or not with this grant, or does
    /// not remove what it creates.
    #[error("{0}")]
    NotOffered(String),
    /// An input is not one `[provisioning] inputs` lists, or the inputs are
    /// too long.
    #[error("{0}")]
    BadInput(String),
    /// The guest refused the run (its typed error, for example a choice it
    /// cannot honour or a token without the needed permissions).
    #[error("the plugin refused: {}", .0.message)]
    Refused(PluginError),
    /// The guest's answer broke the provisioning contract.
    #[error(transparent)]
    Answer(#[from] ProvisionRefusal),
    /// The guest could not be prepared, spawned, opened or reached in time.
    #[error("{0}")]
    Host(String),
}

/// Overlays the operator's `inputs` onto `config`.
///
/// # Errors
///
/// Returns [`ProvisionRunError::Host`] when `config` is not a JSON object.
fn overlay(
    config: &ExtensibleConfig,
    inputs: &BTreeMap<String, Value>,
) -> Result<Value, ProvisionRunError> {
    let mut object = match config.json_value() {
        Ok(Value::Object(object)) => object,
        Ok(Value::Null) | Err(_) => serde_json::Map::new(),
        Ok(_) => {
            return Err(ProvisionRunError::Host(
                "the instance settings are not a JSON object".into(),
            ))
        }
    };
    for (key, value) in inputs {
        object.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(object))
}

/// Spawns a guest for `instance_id` and opens it for guided setup with the
/// stored settings, `inputs` laid over them, and the stored secrets.
///
/// # Errors
///
/// Returns [`ProvisionRunError::Host`] when the instance is not this
/// plugin's or its bindings, the spawn or the open fail.
async fn open_for_provision(
    store: &LibraryStore,
    config: &Config,
    plugin: &DiscoveredPlugin,
    instance_id: &PluginInstanceId,
    inputs: &BTreeMap<String, Value>,
) -> Result<PluginSession, ProvisionRunError> {
    let host = |err: &dyn std::fmt::Display| ProvisionRunError::Host(err.to_string());
    let instance = load_plugin_instance(store, instance_id)
        .await
        .map_err(|err| host(&err))?
        .ok_or_else(|| {
            ProvisionRunError::Host(format!("plugin instance `{instance_id}` was not found"))
        })?;
    if instance.plugin_key != plugin.plugin_key().canonical() {
        return Err(ProvisionRunError::Host(format!(
            "plugin instance `{instance_id}` belongs to `{}`",
            instance.plugin_key
        )));
    }
    let files_dir: &Path = &config.paths().files_dir;
    let grant = crate::spawn_grant(files_dir, plugin).map_err(|err| host(&err))?;
    let flags = crate::binding_grant_flags(&grant);
    let resolved = resolve_instance_bindings_with(
        store,
        instance_id,
        &flags,
        &plugin.manifest.settings_schema(),
        RequiredSettings::Relax,
    )
    .await
    .map_err(|err| host(&err))?;
    let table = overlay(&resolved.config, inputs)?;
    let session = PluginSession::spawn_with(
        plugin,
        config,
        table.clone(),
        HOST_SHARED_ACCOUNT,
        &[],
        SessionServices::default(),
    )
    .await
    .map_err(|err| host(&err))?;
    session
        .open(BindingValues {
            config: ExtensibleConfig::json(&table),
            secrets: resolved.secrets,
            purpose: InvocationPurpose::Provision,
            ..BindingValues::default()
        })
        .await
        .map_err(|err| host(&err))?;
    Ok(session)
}

/// Flattens a call whose guest answer travels inside a successful session
/// call, so the guest's typed error keeps its code.
///
/// # Errors
///
/// [`ProvisionRunError::Host`] when the session failed or timed out,
/// [`ProvisionRunError::Refused`] when the guest refused.
fn settle<T>(
    outcome: Result<crate::Result<Result<T, PluginError>>, tokio::time::error::Elapsed>,
    what: &str,
) -> Result<T, ProvisionRunError> {
    match outcome {
        Err(_) => Err(ProvisionRunError::Host(format!(
            "{what} took longer than {} s",
            PROVISION_TIMEOUT.as_secs()
        ))),
        Ok(Err(err)) => Err(ProvisionRunError::Host(err.to_string())),
        Ok(Ok(Err(refused))) => Err(ProvisionRunError::Refused(refused)),
        Ok(Ok(Ok(value))) => Ok(value),
    }
}

/// Runs `plugin`'s guided setup for `instance_id` with `token`.
///
/// # Arguments
///
/// * `store` - The library, for the instance's stored settings and secrets.
/// * `config` - Host configuration, for the spawn.
/// * `plugin` - The installed plugin the instance belongs to.
/// * `instance_id` - The instance being set up.
/// * `token` - The operator's pasted token.
/// * `inputs` - The operator's choices, keyed by `[provisioning] inputs`.
///
/// # Returns
///
/// The settings to store, credentials to seal and summary lines, checked
/// against `[provisioning] outputs`. The caller still checks each value
/// against the pinned settings schema before writing.
///
/// # Errors
///
/// Returns a [`ProvisionRunError`] for each way the run can fail.
pub async fn provision_instance(
    store: &LibraryStore,
    config: &Config,
    plugin: &DiscoveredPlugin,
    instance_id: &PluginInstanceId,
    token: &ProvisionToken,
    inputs: &BTreeMap<String, Value>,
) -> Result<CheckedProvision, ProvisionRunError> {
    let (outputs, inputs_json) = check_offer(plugin, inputs)?;
    let session = open_for_provision(store, config, plugin, instance_id, inputs).await?;
    run_provision(session, token, inputs_json, &outputs).await
}

/// Runs the guided setup of a bootstrap plugin, one configured in
/// `config.toml` rather than through a library instance (a hosted database
/// such as Cloudflare D1, which must work before the library opens).
///
/// The guest is opened with `settings` (what the caller's configuration
/// already holds) with `inputs` laid over them, and no secrets.
///
/// # Arguments
///
/// * `config` - Host configuration, for the spawn.
/// * `plugin` - The installed plugin.
/// * `settings` - The plugin's current settings, as a JSON object.
/// * `token` - The operator's pasted token.
/// * `inputs` - The operator's choices, keyed by `[provisioning] inputs`.
///
/// # Returns
///
/// The checked settings, credentials and summary lines, for the caller to
/// write into its configuration and credential store.
///
/// # Errors
///
/// Returns a [`ProvisionRunError`] for each way the run can fail.
pub async fn provision_bootstrap(
    config: &Config,
    plugin: &DiscoveredPlugin,
    settings: &serde_json::Map<String, Value>,
    token: &ProvisionToken,
    inputs: &BTreeMap<String, Value>,
) -> Result<CheckedProvision, ProvisionRunError> {
    let (outputs, inputs_json) = check_offer(plugin, inputs)?;
    let mut table = settings.clone();
    for (key, value) in inputs {
        table.insert(key.clone(), value.clone());
    }
    let table = Value::Object(table);
    let host = |err: &dyn std::fmt::Display| ProvisionRunError::Host(err.to_string());
    let session = PluginSession::spawn_with(
        plugin,
        config,
        table.clone(),
        HOST_SHARED_ACCOUNT,
        &[],
        SessionServices::default(),
    )
    .await
    .map_err(|err| host(&err))?;
    session
        .open(BindingValues {
            config: ExtensibleConfig::json(&table),
            purpose: InvocationPurpose::Provision,
            ..BindingValues::default()
        })
        .await
        .map_err(|err| host(&err))?;
    run_provision(session, token, inputs_json, &outputs).await
}

/// Checks that `plugin` offers a pasted-token guided setup taking
/// `inputs`.
///
/// # Returns
///
/// The outputs the setup may write, and the inputs as JSON.
///
/// # Errors
///
/// [`ProvisionRunError::NotOffered`] or [`ProvisionRunError::BadInput`].
fn check_offer(
    plugin: &DiscoveredPlugin,
    inputs: &BTreeMap<String, Value>,
) -> Result<(ProvisionOutputs, String), ProvisionRunError> {
    let manifest = &plugin.manifest;
    let (Some(table), Some(outputs)) =
        (manifest.provisioning.as_ref(), manifest.provision_outputs())
    else {
        return Err(ProvisionRunError::NotOffered(format!(
            "plugin `{}` offers no guided setup",
            manifest.id
        )));
    };
    if !table.grants.contains(&ProvisionGrantType::ApiToken) {
        return Err(ProvisionRunError::NotOffered(format!(
            "plugin `{}` does not take a pasted token",
            manifest.id
        )));
    }
    if let Some(key) = inputs.keys().find(|key| !table.inputs.contains(key)) {
        return Err(ProvisionRunError::BadInput(format!(
            "`{key}` is not one of the guided setup's inputs"
        )));
    }
    let inputs_json = serde_json::to_string(inputs)
        .map_err(|err| ProvisionRunError::BadInput(err.to_string()))?;
    if inputs_json.len() > MAX_PROVISION_SETTINGS_BYTES as usize {
        return Err(ProvisionRunError::BadInput(format!(
            "the inputs are longer than {MAX_PROVISION_SETTINGS_BYTES} bytes"
        )));
    }
    Ok((outputs, inputs_json))
}

/// Calls `provision` on an opened guest and checks its answer.
///
/// # Errors
///
/// As [`provision_instance`].
async fn run_provision(
    session: PluginSession,
    token: &ProvisionToken,
    inputs_json: String,
    outputs: &ProvisionOutputs,
) -> Result<CheckedProvision, ProvisionRunError> {
    let request = ProvisionRequest {
        grant: token.grant(),
        inputs_json,
    };
    let answer = settle(
        tokio::time::timeout(
            PROVISION_TIMEOUT,
            session.provisioning(move |setup| async move { Ok(setup.provision(request).await) }),
        )
        .await,
        "the guided setup",
    )?;
    drop(session);
    Ok(check_provisioned(&answer, outputs, Some(token.expose()))?)
}

/// Asks `plugin` to remove what its guided setup created for
/// `instance_id`.
///
/// # Arguments
///
/// * `store` - The library, for the instance's stored settings and secrets.
/// * `config` - Host configuration, for the spawn.
/// * `plugin` - The installed plugin the instance belongs to.
/// * `instance_id` - The instance being torn down.
/// * `token` - A fresh token from the operator, or `None` for the plugin to
///   use its stored secrets.
///
/// # Errors
///
/// Returns [`ProvisionRunError::NotOffered`] unless the manifest sets
/// `[provisioning] deprovision = true`, and the other variants as
/// [`provision_instance`] does.
pub async fn deprovision_instance(
    store: &LibraryStore,
    config: &Config,
    plugin: &DiscoveredPlugin,
    instance_id: &PluginInstanceId,
    token: Option<&ProvisionToken>,
) -> Result<(), ProvisionRunError> {
    let offered = plugin
        .manifest
        .provisioning
        .as_ref()
        .is_some_and(|table| table.deprovision);
    if !offered {
        return Err(ProvisionRunError::NotOffered(format!(
            "plugin `{}` does not remove what its guided setup creates",
            plugin.manifest.id
        )));
    }
    let session = open_for_provision(store, config, plugin, instance_id, &BTreeMap::new()).await?;
    let request = DeprovisionRequest {
        grant: token.map(ProvisionToken::grant).unwrap_or_default(),
    };
    settle(
        tokio::time::timeout(
            PROVISION_TIMEOUT,
            session.provisioning(move |setup| async move { Ok(setup.deprovision(request).await) }),
        )
        .await,
        "removing what the guided setup created",
    )
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_checked_and_never_printed() {
        let token = ProvisionToken::parse("  abcDEF123_-.  ").expect("token");
        assert_eq!(token.expose(), "abcDEF123_-.");
        assert_eq!(format!("{token:?}"), "ProvisionToken(<redacted>)");
        assert_eq!(token.grant().kind, ProvisionGrantKind::ApiToken);
        for bad in ["", "   ", "two words", "tab\there", "nul\u{0}"] {
            assert!(ProvisionToken::parse(bad).is_err(), "{bad:?}");
        }
        assert!(ProvisionToken::parse(&"a".repeat(5000)).is_err());
    }

    #[test]
    fn inputs_overlay_the_stored_settings() {
        let stored = ExtensibleConfig::json(&serde_json::json!({"bucket": "old", "prefix": "p/"}));
        let inputs = BTreeMap::from([("bucket".to_string(), Value::from("new"))]);
        assert_eq!(
            overlay(&stored, &inputs).expect("overlay"),
            serde_json::json!({"bucket": "new", "prefix": "p/"})
        );
        assert_eq!(
            overlay(&ExtensibleConfig::default(), &inputs).expect("overlay"),
            serde_json::json!({"bucket": "new"})
        );
        assert!(overlay(&ExtensibleConfig::json(&serde_json::json!([1])), &inputs).is_err());
    }

    #[tokio::test]
    async fn settle_keeps_each_failure_apart() {
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .expect_err("elapsed");
        let late: Result<crate::Result<Result<(), PluginError>>, _> = Err(elapsed);
        assert!(
            matches!(settle(late, "the run"), Err(ProvisionRunError::Host(m)) if m.contains("120 s"))
        );
        let broken = Ok(Err(crate::PluginError::Message("gone".into())));
        assert!(
            matches!(settle::<()>(broken, "x"), Err(ProvisionRunError::Host(m)) if m == "gone")
        );
        let refused = Ok(Ok(Err(PluginError::new(
            bookclerk_plugin_sdk::PluginErrorCode::Unauthorized,
            "no",
        ))));
        let err = settle::<()>(refused, "x").expect_err("refused");
        assert_eq!(err.to_string(), "the plugin refused: no");
        assert_eq!(settle(Ok(Ok(Ok(7))), "x").expect("value"), 7);
    }

    /// A store with the host schema.
    async fn memory_store() -> LibraryStore {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("memory store");
        bookclerk_library::apply_host_schema(&db)
            .await
            .expect("host schema");
        LibraryStore::from_connection(db)
    }

    /// A native plugin in `root` whose `[provisioning]` table removes what
    /// it creates when `deprovision` is set.
    fn plugin(root: &Path, provisioning: bool, deprovision: bool) -> DiscoveredPlugin {
        let (entrypoints, table) = if provisioning {
            (
                r#"["storage", "provisioning"]"#,
                format!(
                    r#"
[provisioning]
provider = "cloudflare"
grants = ["api_token"]
inputs = ["bucket"]
outputs = ["bucket", "access_key_id"]
deprovision = {deprovision}
"#
                ),
            )
        } else {
            (r#"["storage"]"#, String::new())
        };
        let toml = format!(
            r#"
api_version = 3
id = "guided"
version = "1.0.0"
runtime = "native"
command = "./guided"
entrypoints = {entrypoints}

[capabilities.network]
mode = "deny"
{table}
[[settings]]
key = "bucket"
label = "Bucket"
type = "string"

[[settings]]
key = "access_key_id"
label = "Access key"
type = "secret"

[vars]

[secrets]
"#
        );
        std::fs::write(root.join("plugin.toml"), &toml).expect("manifest");
        std::fs::write(root.join("guided"), b"#!/bin/sh\n").expect("command");
        let manifest = crate::PluginManifest::parse(&toml).expect("manifest");
        DiscoveredPlugin::try_new(manifest, root.to_path_buf(), root.join("guided"), None)
            .expect("plugin")
    }

    #[tokio::test]
    async fn runs_refuse_before_spawning_a_guest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = memory_store().await;
        let config = Config::default();
        let token = ProvisionToken::parse("tok").expect("token");
        let missing = PluginInstanceId::mint();
        let none = BTreeMap::new();

        let bare = plugin(dir.path(), false, false);
        let run = provision_instance(&store, &config, &bare, &missing, &token, &none).await;
        assert!(
            matches!(run, Err(ProvisionRunError::NotOffered(_))),
            "{run:?}"
        );
        let undo = deprovision_instance(&store, &config, &bare, &missing, None).await;
        assert!(
            matches!(undo, Err(ProvisionRunError::NotOffered(_))),
            "{undo:?}"
        );

        let kept = plugin(dir.path(), true, false);
        let undo = deprovision_instance(&store, &config, &kept, &missing, Some(&token)).await;
        assert!(
            matches!(undo, Err(ProvisionRunError::NotOffered(_))),
            "{undo:?}"
        );
        let stray = BTreeMap::from([("prefix".to_string(), Value::from("p/"))]);
        let run = provision_instance(&store, &config, &kept, &missing, &token, &stray).await;
        assert!(
            matches!(run, Err(ProvisionRunError::BadInput(_))),
            "{run:?}"
        );
        let long = BTreeMap::from([(
            "bucket".to_string(),
            Value::from("b".repeat(MAX_PROVISION_SETTINGS_BYTES as usize)),
        )]);
        let run = provision_instance(&store, &config, &kept, &missing, &token, &long).await;
        assert!(
            matches!(run, Err(ProvisionRunError::BadInput(_))),
            "{run:?}"
        );
        let run = provision_instance(&store, &config, &kept, &missing, &token, &none).await;
        assert!(
            matches!(&run, Err(ProvisionRunError::Host(m)) if m.contains("was not found")),
            "{run:?}"
        );

        let removes = plugin(dir.path(), true, true);
        let other = bookclerk_library::control_plane::create_plugin_instance(
            &store,
            &bookclerk_library::control_plane::ConfigActor::Bootstrap,
            "other",
            "",
        )
        .await
        .expect("instance");
        let undo = deprovision_instance(&store, &config, &removes, &other.id, None).await;
        assert!(
            matches!(&undo, Err(ProvisionRunError::Host(m)) if m.contains("belongs to `other`")),
            "{undo:?}"
        );
    }
}
