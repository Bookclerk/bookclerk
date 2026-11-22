//! Destination + job-handler surface for the Cloudflare R2 guest.

use bookclerk_plugin_sdk::{
    manifest_capabilities, Bindings, Entrypoints, ExtensibleConfig, Invocation, PluginDescribe,
    PluginError, PluginWorker, ScalarLimits, StreamCopyHandler, FEATURE_SCALAR_LIMITS,
    FEATURE_STORAGE_COPY, FEATURE_STREAMS, PRODUCT_API_VERSION,
};
use bookclerk_storage::guest;
use bookclerk_storage::s3_guest::ObjectStoreDestination;
use bookclerk_storage::{
    CloudflareAccountId, CloudflareApiToken, CloudflareQueueId, S3Backend, S3Credentials,
    S3Settings, StorageCapabilities,
};
use serde::Deserialize;

use crate::ID;

/// Result alias matching the ABI crate.
type Result<T> = std::result::Result<T, PluginError>;

/// What the guest declares: every native operation of [`S3Backend`] and
/// `storage.changes`. An instance without a notification queue answers
/// `changes` with an incomplete page and the host lists instead.
const CAPABILITIES: StorageCapabilities = StorageCapabilities {
    changes: true,
    ..S3Backend::CAPABILITIES
};

/// The jurisdiction an R2 bucket was created in, which picks its endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Jurisdiction {
    /// No jurisdiction: `<account>.r2.cloudflarestorage.com`.
    #[default]
    Default,
    /// The European Union: `<account>.eu.r2.cloudflarestorage.com`.
    Eu,
    /// FedRAMP: `<account>.fedramp.r2.cloudflarestorage.com`.
    Fedramp,
}

impl Jurisdiction {
    /// The bucket's S3 API endpoint for `account`.
    #[must_use]
    pub fn endpoint(self, account: &CloudflareAccountId) -> String {
        let account = account.as_str();
        match self {
            Self::Default => format!("https://{account}.r2.cloudflarestorage.com"),
            Self::Eu => format!("https://{account}.eu.r2.cloudflarestorage.com"),
            Self::Fedramp => format!("https://{account}.fedramp.r2.cloudflarestorage.com"),
        }
    }
}

/// The instance settings the guest reads from `CONFIG`.
#[derive(Debug, Deserialize)]
struct R2Config {
    /// Cloudflare account id.
    account_id: String,
    /// The bucket's jurisdiction.
    #[serde(default)]
    jurisdiction: Jurisdiction,
    /// Bucket name.
    bucket: String,
    /// Key prefix under the bucket.
    #[serde(default)]
    prefix: String,
    /// Id of the Cloudflare Queue that receives the bucket's events.
    #[serde(default)]
    notification_queue_id: Option<String>,
}

/// The instance secrets the guest reads from `SECRETS`.
#[derive(Default, Deserialize)]
struct R2Secrets {
    /// R2 access key id.
    #[serde(default)]
    access_key_id: Option<String>,
    /// R2 secret access key.
    #[serde(default)]
    secret_access_key: Option<String>,
    /// Cloudflare API token for the queue's pull consumer.
    #[serde(default)]
    queue_api_token: Option<String>,
}

/// A secret's text, trimmed, when it is set and not empty.
fn secret(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Builds the destination from the instance's `CONFIG` and `SECRETS`.
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
pub async fn destination_from(
    config: &ExtensibleConfig,
    secrets: &ExtensibleConfig,
) -> Result<ObjectStoreDestination<S3Backend>> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| PluginError::internal("r2 destination has no HOME"))?;
    destination_in(config, secrets, &home).await
}

/// [`destination_from`] with its state (the multipart journal and the
/// queue's lease ids) under `home`.
///
/// # Errors
///
/// As [`destination_from`].
async fn destination_in(
    config: &ExtensibleConfig,
    secrets: &ExtensibleConfig,
    home: &std::path::Path,
) -> Result<ObjectStoreDestination<S3Backend>> {
    let invalid = |what: String| PluginError::invalid_params(format!("r2 destination: {what}"));
    let config: R2Config = config.json_into().map_err(|err| invalid(err.to_string()))?;
    let secrets: R2Secrets = match secrets.json_value() {
        Ok(serde_json::Value::Null) | Err(_) => R2Secrets::default(),
        Ok(value) => serde_json::from_value(value).map_err(|err| invalid(err.to_string()))?,
    };
    let account =
        CloudflareAccountId::parse(&config.account_id).map_err(|err| invalid(err.to_string()))?;
    let bucket = config.bucket.trim().to_string();
    if bucket.is_empty() {
        return Err(invalid("the bucket is not set".into()));
    }
    let queue = config
        .notification_queue_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(CloudflareQueueId::parse)
        .transpose()
        .map_err(|err| invalid(err.to_string()))?;
    let (Some(access_key_id), Some(secret_access_key)) = (
        secret(secrets.access_key_id),
        secret(secrets.secret_access_key),
    ) else {
        return Err(invalid(
            "the access key ID and secret access key are required".into(),
        ));
    };
    let token = match (&queue, secret(secrets.queue_api_token)) {
        (Some(_), Some(token)) => {
            Some(CloudflareApiToken::new(token).map_err(|err| invalid(err.to_string()))?)
        }
        (Some(_), None) => {
            return Err(invalid(
                "a notification queue needs the queue API token".into(),
            ))
        }
        (None, _) => None,
    };
    let settings = S3Settings {
        bucket,
        // R2 takes any region and documents `auto`.
        region: "auto".into(),
        endpoint: Some(config.jurisdiction.endpoint(&account)),
        // Path-style keeps every request on the account's endpoint host.
        force_path_style: true,
    };
    let credentials = S3Credentials {
        access_key_id,
        secret_access_key,
        session_token: None,
    };
    let backend = S3Backend::from_parts_with_journal(
        &settings,
        &config.prefix,
        Some(&credentials),
        &home.join("multipart-journal"),
    )
    .await
    .map_err(|err| PluginError::internal(err.to_string()))?;
    let backend = match (queue, token) {
        (Some(queue), Some(token)) => backend
            .with_cloudflare_queue(&account, &queue, token, &home.join("notification-queue"))
            .map_err(|err| PluginError::internal(err.to_string()))?,
        _ => backend,
    };
    Ok(ObjectStoreDestination::new(backend))
}

/// Root capability for the R2 destination guest.
pub struct R2Root;

#[async_trait::async_trait(?Send)]
impl PluginWorker for R2Root {
    async fn describe(&self) -> Result<PluginDescribe> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: ID.into(),
            display_name: Some("Cloudflare R2".into()),
            rpc_features: [
                FEATURE_SCALAR_LIMITS.into(),
                FEATURE_STREAMS.into(),
                FEATURE_STORAGE_COPY.into(),
            ]
            .into_iter()
            .chain(guest::storage_features(CAPABILITIES))
            .collect(),
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            ..PluginDescribe::default()
        })
    }

    async fn open(&self, _invocation: Invocation, bindings: Bindings) -> Result<Entrypoints> {
        Ok(Entrypoints {
            storage: Some(Box::new(
                destination_from(&bindings.config, &bindings.secrets).await?,
            )),
            job_runner: Some(Box::new(StreamCopyHandler)),
            ..Entrypoints::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "3f4b7e3dcab231cbfdaa90a6a28bd548";

    fn config(extra: serde_json::Value) -> ExtensibleConfig {
        let mut base = serde_json::json!({
            "account_id": ACCOUNT,
            "bucket": "books",
            "prefix": "library/",
        });
        if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        ExtensibleConfig::json(&base)
    }

    fn keys() -> ExtensibleConfig {
        ExtensibleConfig::json(&serde_json::json!({
            "access_key_id": "id",
            "secret_access_key": "secret",
        }))
    }

    async fn refusal(config: ExtensibleConfig, secrets: ExtensibleConfig) -> String {
        let home = tempfile::tempdir().expect("tempdir");
        destination_in(&config, &secrets, home.path())
            .await
            .err()
            .expect("refused")
            .to_string()
    }

    #[test]
    fn jurisdictions_pick_the_endpoint() {
        let account = CloudflareAccountId::parse(ACCOUNT).unwrap();
        assert_eq!(
            Jurisdiction::Default.endpoint(&account),
            format!("https://{ACCOUNT}.r2.cloudflarestorage.com")
        );
        assert_eq!(
            Jurisdiction::Eu.endpoint(&account),
            format!("https://{ACCOUNT}.eu.r2.cloudflarestorage.com")
        );
        assert_eq!(
            Jurisdiction::Fedramp.endpoint(&account),
            format!("https://{ACCOUNT}.fedramp.r2.cloudflarestorage.com")
        );
        let parsed: Jurisdiction = serde_json::from_str("\"eu\"").unwrap();
        assert_eq!(parsed, Jurisdiction::Eu);
        assert!(serde_json::from_str::<Jurisdiction>("\"apac\"").is_err());
    }

    #[tokio::test]
    async fn describe_declares_the_native_storage_operations() {
        let described = R2Root.describe().await.expect("describe");
        assert_eq!(described.id, "r2");
        for feature in [
            bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE,
            bookclerk_plugin_sdk::FEATURE_STORAGE_MOVE_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_DELETE_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_HEAD_MANY,
            bookclerk_plugin_sdk::FEATURE_STORAGE_CONDITIONAL_WRITE,
            bookclerk_plugin_sdk::FEATURE_STORAGE_CHANGES,
        ] {
            assert!(
                described.rpc_features.iter().any(|f| f == feature),
                "{feature} missing from {:?}",
                described.rpc_features
            );
        }
    }

    #[tokio::test]
    async fn bad_settings_are_refused_at_open() {
        let err = refusal(config(serde_json::json!({"account_id": "nope"})), keys()).await;
        assert!(err.contains("account id"), "{err}");
        let err = refusal(config(serde_json::json!({"bucket": " "})), keys()).await;
        assert!(err.contains("bucket"), "{err}");
        let err = refusal(config(serde_json::json!({"jurisdiction": "apac"})), keys()).await;
        assert!(err.contains("r2 destination"), "{err}");
        let err = refusal(config(serde_json::json!({})), ExtensibleConfig::default()).await;
        assert!(err.contains("access key"), "{err}");
        let queue = serde_json::json!({"notification_queue_id": "a".repeat(32)});
        let err = refusal(config(queue), keys()).await;
        assert!(err.contains("queue API token"), "{err}");
        let err = refusal(
            config(serde_json::json!({"notification_queue_id": "my-queue"})),
            keys(),
        )
        .await;
        assert!(err.contains("32-hex-digit"), "{err}");
    }

    #[tokio::test]
    async fn good_settings_open_with_and_without_a_queue() {
        let home = tempfile::tempdir().expect("tempdir");
        destination_in(&config(serde_json::json!({})), &keys(), home.path())
            .await
            .expect("no queue");
        let with_queue = config(serde_json::json!({
            "notification_queue_id": "a".repeat(32),
            "jurisdiction": "eu",
        }));
        let secrets = ExtensibleConfig::json(&serde_json::json!({
            "access_key_id": " id ",
            "secret_access_key": "secret",
            "queue_api_token": "token",
        }));
        destination_in(&with_queue, &secrets, home.path())
            .await
            .expect("queue");
        assert!(home.path().join("notification-queue").is_dir());
        let err = refusal(
            config(serde_json::json!({"notification_queue_id": "a".repeat(32)})),
            ExtensibleConfig::json(&serde_json::json!({
                "access_key_id": "id",
                "secret_access_key": "secret",
                "queue_api_token": "bad token",
            })),
        )
        .await;
        assert!(err.contains("API token"), "{err}");
    }
}
