//! Typed plugin-instance configuration and CONFIG / SECRETS resolution.
//!
//! The document address is [`DocumentKey::plugin_instance`]. Validation does
//! not take a plugin family, entrypoint, alias, or host id. GraphicAudio's
//! extra allowlist is selected by the installed manifest id `graphicaudio`.

use std::collections::BTreeMap;

use bookclerk_plugin_abi::ExtensibleConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::documents::{
    import_if_absent, load_document, replace_document, ConfigActor, DocumentKey, ReplaceOutcome,
    StoredDocument,
};
use super::instances::{load_plugin_instance, PluginInstanceId};
use crate::error::{LibraryError, Result};
use crate::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret,
    upsert_secret, FORMAT_SEALED_V1,
};
use crate::store::LibraryStore;

/// Domain schema of [`PluginInstanceConfigV1`].
pub const PLUGIN_INSTANCE_CONFIG_SCHEMA_VERSION: i64 = 1;

/// Maximum settings in one schema-1 document.
pub const MAX_INSTANCE_SETTINGS: usize = 32;

/// Maximum secret refs in one schema-1 document.
pub const MAX_INSTANCE_SECRET_REFS: usize = 16;

/// Maximum UTF-8 size of one string setting.
pub const MAX_INSTANCE_SETTING_BYTES: usize = 1024;

/// Manifest id that selects the GraphicAudio setting allowlist.
pub const GRAPHICAUDIO_MANIFEST_ID: &str = "graphicaudio";

/// How strictly schema 1 is checked for one package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstancePackagePolicy {
    /// Scalar settings and secret-ref names only.
    Generic,
    /// GraphicAudio allowlist, selected by manifest id `graphicaudio`.
    GraphicAudio,
}

impl InstancePackagePolicy {
    /// GraphicAudio when `manifest_id` is the product package id.
    #[must_use]
    pub fn from_manifest_id(manifest_id: &str) -> Self {
        if manifest_id.eq_ignore_ascii_case(GRAPHICAUDIO_MANIFEST_ID) {
            Self::GraphicAudio
        } else {
            Self::Generic
        }
    }
}

/// One scalar setting. Nested objects and arrays are not representable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    /// JSON boolean.
    Bool(bool),
    /// Finite JSON number.
    Number(serde_json::Number),
    /// JSON string, at most 1024 bytes.
    String(String),
}

/// A name in `encrypted_secrets`. The ciphertext is not part of the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSecretRefV1 {
    /// Key in the `SECRETS` object delivered to the guest.
    pub key: String,
    /// `encrypted_secrets.name` for this instance.
    pub name: String,
}

/// Schema-1 plugin instance configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginInstanceConfigV1 {
    /// Operator settings. Keys match `^[a-z][a-z0-9_]{0,63}$`.
    pub settings: BTreeMap<String, SettingValue>,
    /// Secret names. Values stay in `encrypted_secrets`.
    #[serde(default)]
    pub secret_refs: Vec<InstanceSecretRefV1>,
}

impl PluginInstanceConfigV1 {
    /// Empty settings and no secret refs.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            settings: BTreeMap::new(),
            secret_refs: Vec::new(),
        }
    }

    /// Rejects schema-1 documents this binary will not store.
    ///
    /// # Errors
    ///
    /// Returns an error when a key, value, count, or GraphicAudio allowlist fails.
    pub fn validate(&self, policy: InstancePackagePolicy) -> Result<()> {
        if self.settings.len() > MAX_INSTANCE_SETTINGS {
            return Err(invalid("at most 32 settings are allowed"));
        }
        if self.secret_refs.len() > MAX_INSTANCE_SECRET_REFS {
            return Err(invalid("at most 16 secret refs are allowed"));
        }
        for (key, value) in &self.settings {
            validate_key(key, "setting")?;
            validate_setting_value(value)?;
        }
        let mut seen_keys = BTreeMap::<&str, ()>::new();
        for secret_ref in &self.secret_refs {
            validate_key(&secret_ref.key, "secret ref")?;
            validate_key(&secret_ref.name, "secret name")?;
            if seen_keys.insert(secret_ref.key.as_str(), ()).is_some() {
                return Err(invalid("duplicate secret ref key"));
            }
        }
        if policy == InstancePackagePolicy::GraphicAudio {
            validate_graphicaudio(self)?;
        }
        let json = self.to_json()?;
        if json.len() > super::documents::MAX_CONFIGURATION_DOCUMENT_BYTES {
            return Err(invalid("document exceeds 16384 bytes"));
        }
        Ok(())
    }

    /// Canonical JSON body stored in `document_json`.
    ///
    /// # Errors
    ///
    /// Returns an error when the body cannot be serialized.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|err| invalid(&format!("encode failed: {err}")))
    }

    /// Settings object delivered as the `CONFIG` payload.
    #[must_use]
    pub fn settings_json(&self) -> Value {
        serde_json::to_value(&self.settings).unwrap_or_else(|_| Value::Object(Default::default()))
    }
}

/// GraphicAudio keys copied once from `[sources.graphicaudio]`.
pub const GRAPHICAUDIO_IMPORT_KEYS: &[&str] =
    &["access", "base_url", "store_url", "bitrate", "container"];

/// Builds a GraphicAudio document from string pairs.
///
/// Unknown keys and an `access` value other than `web`, `zip`, or `device`
/// fail before any write.
///
/// # Errors
///
/// Returns an error when the pairs fail the GraphicAudio allowlist.
pub fn graphicaudio_config_from_pairs<I, K, V>(pairs: I) -> Result<PluginInstanceConfigV1>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut settings = BTreeMap::new();
    for (key, value) in pairs {
        let key = key.as_ref().to_string();
        if !GRAPHICAUDIO_IMPORT_KEYS.contains(&key.as_str()) {
            return Err(invalid(&format!(
                "GraphicAudio setting `{key}` is not importable"
            )));
        }
        settings.insert(key, SettingValue::String(value.as_ref().to_string()));
    }
    let body = PluginInstanceConfigV1 {
        settings,
        secret_refs: Vec::new(),
    };
    body.validate(InstancePackagePolicy::GraphicAudio)?;
    Ok(body)
}

/// A parsed instance document plus its revision.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceConfigDocument {
    /// Parsed body.
    pub body: PluginInstanceConfigV1,
    /// Compare-and-swap revision.
    pub revision: i64,
    /// Domain schema version. Schema 1 is the only version this binary applies.
    pub schema_version: i64,
    /// Instance id (`scope_id`).
    pub instance_id: String,
}

/// Loads and parses the instance config document.
///
/// An unknown `schema_version` fails the read. The stored JSON is not rewritten.
///
/// # Errors
///
/// Returns an error when the row is missing, the schema is unknown, or the body is invalid.
pub async fn load_instance_config(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<InstanceConfigDocument> {
    let stored = load_stored(store, instance_id).await?.ok_or_else(|| {
        LibraryError::NotFound(format!(
            "configuration plugin instance {instance_id} is not initialized"
        ))
    })?;
    parse_instance_config(stored)
}

/// Inserts the document when it is absent.
///
/// An existing row is returned and the seed is not validated again.
///
/// # Errors
///
/// Returns an error when the actor may not import, the body is invalid, or the write fails.
pub async fn import_instance_config_if_absent(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    policy: InstancePackagePolicy,
    body: &PluginInstanceConfigV1,
    operation_id: &str,
) -> Result<InstanceConfigDocument> {
    let key = DocumentKey::plugin_instance(instance_id.as_str());
    if let Some(existing) = load_document(store, &key).await? {
        return parse_instance_config(existing);
    }
    body.validate(policy)?;
    let json = body.to_json()?;
    let stored = import_if_absent(
        store,
        actor,
        &key,
        PLUGIN_INSTANCE_CONFIG_SCHEMA_VERSION,
        &json,
        operation_id,
    )
    .await?;
    parse_instance_config(stored)
}

/// Outcome of [`replace_instance_config`].
#[derive(Debug, Clone, PartialEq)]
pub enum InstanceConfigReplace {
    /// The new document is committed.
    Applied(InstanceConfigDocument),
    /// `expected_revision` was not current. No audit row was written.
    Conflict {
        /// Revision currently stored.
        current_revision: i64,
    },
    /// This operation id already committed at `revision`.
    Replayed {
        /// Revision of the original commit.
        revision: i64,
    },
}

/// Replaces the instance document when `expected_revision` is current.
///
/// # Errors
///
/// Returns an error when validation, authorization, or the compare-and-swap fails.
pub async fn replace_instance_config(
    store: &LibraryStore,
    actor: &ConfigActor,
    instance_id: &PluginInstanceId,
    policy: InstancePackagePolicy,
    expected_revision: i64,
    body: &PluginInstanceConfigV1,
    operation_id: &str,
) -> Result<InstanceConfigReplace> {
    body.validate(policy)?;
    let json = body.to_json()?;
    match replace_document(
        store,
        actor,
        &DocumentKey::plugin_instance(instance_id.as_str()),
        PLUGIN_INSTANCE_CONFIG_SCHEMA_VERSION,
        expected_revision,
        &json,
        operation_id,
    )
    .await?
    {
        ReplaceOutcome::Applied(stored) => Ok(InstanceConfigReplace::Applied(
            parse_instance_config(stored)?,
        )),
        ReplaceOutcome::Conflict { current_revision } => {
            Ok(InstanceConfigReplace::Conflict { current_revision })
        }
        ReplaceOutcome::Replayed { revision } => Ok(InstanceConfigReplace::Replayed { revision }),
    }
}

/// Whether the plugin-instance config grant includes `config` or `secrets`.
///
/// Resolved from the plugin key's grant before this function runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceBindingGrant {
    /// Grant includes the `config` binding.
    pub config: bool,
    /// Grant includes the `secrets` binding.
    pub secrets: bool,
}

/// `CONFIG` and `SECRETS` payloads for one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInstanceBindings {
    /// Granted settings as `application/json`.
    pub config: ExtensibleConfig,
    /// Granted secret values as `application/json`, or an empty payload.
    pub secrets: ExtensibleConfig,
    /// Document revision applied by this resolution.
    pub config_revision: i64,
}

/// Resolves `CONFIG` and `SECRETS` for `instance_id`.
///
/// The only inputs are the instance id and the grant already resolved for its
/// plugin key. A missing document, a dangling secret ref, a secret that fails
/// to unseal, a missing `config` grant, or secret refs without a `secrets`
/// grant fails resolution. An empty ref list with no `secrets` grant yields
/// the empty secrets payload.
///
/// # Errors
///
/// Returns an error when the document or a secret ref cannot be resolved.
pub async fn resolve_instance_bindings(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    grant: &InstanceBindingGrant,
) -> Result<ResolvedInstanceBindings> {
    let instance = load_plugin_instance(store, instance_id)
        .await?
        .ok_or_else(|| LibraryError::NotFound(format!("plugin instance {instance_id}")))?;
    let document = load_instance_config(store, instance_id).await?;
    if !grant.config {
        return Err(invalid(
            "plugin grant lacks config; instance deployment cannot spawn",
        ));
    }
    let secrets = resolve_secrets(store, &instance, &document.body, grant.secrets).await?;
    let config = ExtensibleConfig::json(&document.body.settings_json());
    Ok(ResolvedInstanceBindings {
        config,
        secrets,
        config_revision: document.revision,
    })
}

/// Seals one instance secret with the cluster DEK.
///
/// The plaintext is not written to `configuration_documents` or `configuration_audit`.
///
/// # Errors
///
/// Returns an error when the value is empty or the seal fails.
pub async fn seal_instance_secret(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
    plugin_key: &str,
    name: &str,
    plaintext: &str,
) -> Result<()> {
    validate_key(name, "secret name")?;
    if plaintext.is_empty() || plaintext.len() > MAX_INSTANCE_SETTING_BYTES {
        return Err(invalid("secret value must be 1..=1024 bytes"));
    }
    let record = build_sealed_record(
        plaintext.as_bytes(),
        secret_kind::PLUGIN_INSTANCE,
        plugin_key,
        secret_account_type::PLUGIN_INSTANCE,
        instance_id.as_str(),
        name,
    )?;
    debug_assert_eq!(record.format, FORMAT_SEALED_V1);
    upsert_secret(store.db(), &record).await
}

/// Unseals secret refs into the `SECRETS` object.
///
/// # Errors
///
/// Returns an error when a ref is missing, cannot be unsealed, or the grant
/// lacks `secrets` while refs are present.
async fn resolve_secrets(
    store: &LibraryStore,
    instance: &super::instances::PluginInstance,
    body: &PluginInstanceConfigV1,
    grant_secrets: bool,
) -> Result<ExtensibleConfig> {
    if body.secret_refs.is_empty() {
        return Ok(ExtensibleConfig::default());
    }
    if !grant_secrets {
        return Err(invalid(
            "plugin grant lacks secrets; instance deployment cannot spawn",
        ));
    }
    let mut object = serde_json::Map::new();
    for secret_ref in &body.secret_refs {
        let row = get_secret(
            store.db(),
            secret_kind::PLUGIN_INSTANCE,
            Some(instance.plugin_key.as_str()),
            secret_account_type::PLUGIN_INSTANCE,
            Some(instance.id.as_str()),
            &secret_ref.name,
        )
        .await?
        .ok_or_else(|| {
            invalid(&format!(
                "secret ref `{}` has no encrypted_secrets row",
                secret_ref.name
            ))
        })?;
        let plaintext = unseal_secret(&row).map_err(|_| {
            invalid(&format!(
                "secret ref `{}` could not be unsealed",
                secret_ref.name
            ))
        })?;
        let text = String::from_utf8(plaintext)
            .map_err(|_| invalid(&format!("secret ref `{}` is not UTF-8", secret_ref.name)))?;
        object.insert(secret_ref.key.clone(), Value::String(text));
    }
    Ok(ExtensibleConfig::json(&Value::Object(object)))
}

/// Reads the raw row.
///
/// # Errors
///
/// Returns an error when the read fails.
async fn load_stored(
    store: &LibraryStore,
    instance_id: &PluginInstanceId,
) -> Result<Option<StoredDocument>> {
    load_document(store, &DocumentKey::plugin_instance(instance_id.as_str())).await
}

/// Parses schema 1. An unknown schema version fails closed.
fn parse_instance_config(stored: StoredDocument) -> Result<InstanceConfigDocument> {
    if stored.schema_version != PLUGIN_INSTANCE_CONFIG_SCHEMA_VERSION {
        return Err(super::documents::unsupported(
            super::documents::PLUGIN_INSTANCE_CONFIG_NAMESPACE,
            stored.schema_version,
        ));
    }
    let body: PluginInstanceConfigV1 = serde_json::from_str(&stored.document_json)
        .map_err(|err| invalid(&format!("decode failed: {err}")))?;
    body.validate(InstancePackagePolicy::Generic)?;
    Ok(InstanceConfigDocument {
        body,
        revision: stored.revision,
        schema_version: stored.schema_version,
        instance_id: stored.key.scope_id,
    })
}

/// GraphicAudio allowlist. `access` is `web`, `zip`, or `device` when present.
fn validate_graphicaudio(body: &PluginInstanceConfigV1) -> Result<()> {
    for key in body.settings.keys() {
        if !GRAPHICAUDIO_IMPORT_KEYS.contains(&key.as_str()) {
            return Err(invalid(&format!(
                "GraphicAudio setting `{key}` is not allowed"
            )));
        }
    }
    for (key, value) in &body.settings {
        let SettingValue::String(text) = value else {
            return Err(invalid(&format!(
                "GraphicAudio setting `{key}` must be a string"
            )));
        };
        if key == "access" && !matches!(text.as_str(), "web" | "zip" | "device") {
            return Err(invalid("GraphicAudio access must be web, zip, or device"));
        }
    }
    Ok(())
}

/// Setting and secret-ref keys use the schema-1 grammar.
fn validate_key(key: &str, label: &str) -> Result<()> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(invalid(&format!(
            "{label} key must match ^[a-z][a-z0-9_]{{0,63}}$"
        )));
    };
    let ok = first.is_ascii_lowercase()
        && key.len() <= 64
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
    if !ok {
        return Err(invalid(&format!(
            "{label} key must match ^[a-z][a-z0-9_]{{0,63}}$"
        )));
    }
    Ok(())
}

/// Scalars only. Non-finite numbers are rejected.
fn validate_setting_value(value: &SettingValue) -> Result<()> {
    match value {
        SettingValue::Bool(_) => Ok(()),
        SettingValue::String(text) => {
            if text.len() > MAX_INSTANCE_SETTING_BYTES || text.contains('\u{0000}') {
                return Err(invalid("string settings must be at most 1024 bytes"));
            }
            Ok(())
        }
        SettingValue::Number(number) => match number.as_f64() {
            Some(value) if value.is_finite() => Ok(()),
            _ => Err(invalid("numeric settings must be finite")),
        },
    }
}

/// Error for a plugin-instance document outside schema 1.
fn invalid(detail: &str) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("invalid configuration: {detail}"))
}
