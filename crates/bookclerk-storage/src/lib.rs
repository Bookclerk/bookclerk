//! Storage backends for acquired audiobooks, covers, and PDFs.
//!
//! # Audience
//!
//! Host acquire / library code that writes through [`StorageBackend`]. Guest
//! plugins do not depend on this crate.
//!
//! Product narrative: destination sections in `docs/configuration.md`. Style:
//! `docs/code-documentation.md`.

mod error;
mod fanout;
mod local;
mod s3;
mod s3_credentials;
mod traits;

pub use error::{Result, StorageError};
pub use fanout::FanoutBackend;
pub use local::LocalFsBackend;
pub use s3::S3Backend;
pub use s3_credentials::{
    delete_s3_credentials, load_s3_credentials, save_s3_credentials, S3Credentials,
    ENV_AWS_ACCESS_KEY_ID, ENV_AWS_SECRET_ACCESS_KEY, ENV_AWS_SESSION_TOKEN, S3_SECRET_NAME,
};
pub use traits::{
    bookclerk_meta_sidecar_key, is_audio_key, ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend,
    AUDIO_EXTENSIONS,
};

use bookclerk_config::{normalize_storage_prefix, Config, OutputBackendKind};
use sea_orm::DatabaseConnection;

/// Build the configured storage backend(s) from `[output.*]`.
///
/// When multiple destination plugins are enabled, returns a [`FanoutBackend`]
/// that writes to all of them.
///
/// # Arguments
///
/// * `config` - Loaded Bookclerk configuration (validated destinations).
/// * `db` - Optional SeaORM connection so S3 can load `encrypted_secrets`
///   after env override (`BOOKCLERK_AWS_*`) and before the AWS SDK chain.
///
/// # Returns
///
/// A boxed [`StorageBackend`] (single backend or fan-out).
///
/// # Errors
///
/// Returns [`StorageError::InvalidKey`] when destination validation fails, and
/// propagates backend construction failures otherwise.
pub async fn from_config(
    config: &Config,
    db: Option<&DatabaseConnection>,
) -> Result<Box<dyn StorageBackend>> {
    config
        .output
        .validate_destinations()
        .map_err(|err| StorageError::InvalidKey(err.to_string()))?;

    let mut backends: Vec<Box<dyn StorageBackend>> = Vec::new();
    for kind in config.output.enabled_backends() {
        match kind {
            OutputBackendKind::Local => {
                let prefix = normalize_storage_prefix(config.output.local.prefix.trim());
                backends.push(Box::new(LocalFsBackend::with_prefix(
                    config.output.local.root.clone(),
                    &prefix,
                )?));
            }
            OutputBackendKind::S3 => {
                let prefix = normalize_storage_prefix(config.output.s3.prefix.trim());
                backends.push(Box::new(
                    S3Backend::from_config(&config.output.s3, &prefix, db).await?,
                ));
            }
        }
    }

    if backends.len() == 1 {
        return Ok(backends.remove(0));
    }
    Ok(Box::new(FanoutBackend::new(backends)?))
}

pub(crate) fn normalize_prefix(prefix: &str) -> String {
    normalize_storage_prefix(prefix)
}
