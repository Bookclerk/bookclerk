//! Destination + job-handler surface for the Backblaze B2 guest.

use bookclerk_plugin_sdk::{
    manifest_capabilities, Bindings, Entrypoints, ExtensibleConfig, Invocation, PluginDescribe,
    PluginError, PluginWorker, ScalarLimits, StreamCopyHandler, FEATURE_SCALAR_LIMITS,
    FEATURE_STORAGE_COPY, FEATURE_STREAMS, PRODUCT_API_VERSION,
};
use bookclerk_storage::guest;
use bookclerk_storage::s3_guest::ObjectStoreDestination;
use bookclerk_storage::{B2ApplicationKey, B2Backend, B2Settings, StorageError};
use serde::Deserialize;

use crate::ID;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// The instance settings the guest reads from `CONFIG`.
#[derive(Debug, Deserialize)]
struct B2Config {
    /// Bucket name.
    bucket: String,
    /// Key prefix under the bucket.
    #[serde(default)]
    prefix: String,
}

/// The instance secrets the guest reads from `SECRETS`.
#[derive(Default, Deserialize)]
struct B2Secrets {
    /// The application key's id.
    #[serde(default)]
    application_key_id: Option<String>,
    /// The application key.
    #[serde(default)]
    application_key: Option<String>,
}

/// A secret's text, trimmed, when it is set and not empty.
fn secret(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Builds the destination from the instance's `CONFIG` and `SECRETS`.
/// Nothing is sent to B2 until the first storage call.
///
/// # Arguments
///
/// * `config` - the instance settings.
/// * `secrets` - the instance's sealed secrets.
///
/// # Errors
///
/// Returns invalid_params for a missing or malformed setting or secret, and
/// internal when the client cannot be constructed.
pub fn destination_from(
    config: &ExtensibleConfig,
    secrets: &ExtensibleConfig,
) -> Result<ObjectStoreDestination<B2Backend>> {
    let invalid = |what: String| PluginError::invalid_params(format!("b2 destination: {what}"));
    let config: B2Config = config.json_into().map_err(|err| invalid(err.to_string()))?;
    let secrets: B2Secrets = match secrets.json_value() {
        Ok(serde_json::Value::Null) | Err(_) => B2Secrets::default(),
        Ok(value) => serde_json::from_value(value).map_err(|err| invalid(err.to_string()))?,
    };
    let bucket = config.bucket.trim().to_string();
    if bucket.is_empty() {
        return Err(invalid("the bucket is not set".into()));
    }
    let (Some(key_id), Some(key)) = (
        secret(secrets.application_key_id),
        secret(secrets.application_key),
    ) else {
        return Err(invalid(
            "the application key ID and application key are required".into(),
        ));
    };
    let settings = B2Settings::new(key_id, B2ApplicationKey::new(key), bucket, config.prefix);
    let backend = B2Backend::new(settings).map_err(|err| match err {
        StorageError::InvalidKey(msg) => invalid(msg),
        other => PluginError::internal(other.to_string()),
    })?;
    Ok(ObjectStoreDestination::new(backend))
}

/// Root capability for the B2 destination guest.
pub struct B2Root;

#[async_trait::async_trait(?Send)]
impl PluginWorker for B2Root {
    async fn describe(&self) -> Result<PluginDescribe> {
        // Only what every B2 backend does: no instance is open here, and
        // the capabilities do not depend on settings.
        let capabilities = bookclerk_storage::StorageCapabilities {
            translate_keys: true,
            ..bookclerk_storage::StorageCapabilities::default()
        };
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            display_name: Some("Backblaze B2".into()),
            rpc_features: [
                FEATURE_SCALAR_LIMITS.into(),
                FEATURE_STREAMS.into(),
                FEATURE_STORAGE_COPY.into(),
            ]
            .into_iter()
            .chain(guest::storage_features(capabilities))
            .collect(),
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            ..PluginDescribe::default()
        })
    }

    async fn open(&self, _invocation: Invocation, bindings: Bindings) -> Result<Entrypoints> {
        Ok(Entrypoints {
            storage: Some(Box::new(destination_from(
                &bindings.config,
                &bindings.secrets,
            )?)),
            job_runner: Some(Box::new(StreamCopyHandler)),
            ..Entrypoints::default()
        })
    }
}

/// Keeps the backend's own declaration and the guest's in step.
#[cfg(test)]
fn backend_capabilities() -> bookclerk_storage::StorageCapabilities {
    use bookclerk_storage::StorageBackend as _;
    let settings = B2Settings::new("id", B2ApplicationKey::new("key"), "bucket", "");
    B2Backend::new(settings).expect("backend").capabilities()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: serde_json::Value) -> ExtensibleConfig {
        let mut base = serde_json::json!({ "bucket": "books", "prefix": "library/" });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        ExtensibleConfig::json(&base)
    }

    fn keys() -> ExtensibleConfig {
        ExtensibleConfig::json(&serde_json::json!({
            "application_key_id": "id",
            "application_key": "secret",
        }))
    }

    fn refusal(config: ExtensibleConfig, secrets: ExtensibleConfig) -> String {
        destination_from(&config, &secrets)
            .err()
            .expect("refused")
            .to_string()
    }

    #[tokio::test]
    async fn describe_declares_what_b2_does_natively() {
        let described = B2Root.describe().await.expect("describe");
        assert_eq!(described.id, "b2");
        assert!(described
            .rpc_features
            .iter()
            .any(|f| f == bookclerk_plugin_sdk::FEATURE_STORAGE_TRANSLATE_KEYS));
        assert!(
            !described
                .rpc_features
                .iter()
                .any(|f| f == bookclerk_plugin_sdk::FEATURE_STORAGE_CONDITIONAL_WRITE),
            "B2 has no conditional writes"
        );
        let declared = guest::storage_features(backend_capabilities());
        for feature in declared {
            assert!(described.rpc_features.contains(&feature), "{feature}");
        }
    }

    #[test]
    fn bad_settings_are_refused_at_open() {
        let err = refusal(config(serde_json::json!({"bucket": " "})), keys());
        assert!(err.contains("bucket"), "{err}");
        let err = refusal(config(serde_json::json!({})), ExtensibleConfig::default());
        assert!(err.contains("application key"), "{err}");
        let err = refusal(config(serde_json::json!({"prefix": "a//b"})), keys());
        assert!(err.contains("prefix"), "{err}");
        let err = refusal(ExtensibleConfig::json(&serde_json::json!({})), keys());
        assert!(err.contains("b2 destination"), "{err}");
    }

    #[test]
    fn good_settings_open_without_signing_in() {
        destination_from(&config(serde_json::json!({})), &keys()).expect("open");
        let spaced = ExtensibleConfig::json(&serde_json::json!({
            "application_key_id": " id ",
            "application_key": " secret ",
        }));
        destination_from(&config(serde_json::json!({})), &spaced).expect("trimmed");
    }
}
