//! Storage backends for acquired audiobooks, covers, and PDFs.

mod error;
mod fanout;
mod local;
mod s3;
mod s3_auth;
mod traits;

pub use error::{Result, StorageError};
pub use fanout::FanoutBackend;
pub use local::LocalFsBackend;
pub use s3::S3Backend;
pub use s3_auth::{
    credentials_file_for, default_credentials_file, load_auth as load_s3_auth,
    resolve_credentials_path, save_auth as save_s3_auth, S3AuthFile, AUTH_SUFFIX as S3_AUTH_SUFFIX,
    DEFAULT_STEM as S3_DEFAULT_STEM,
};
pub use traits::{
    bookclerk_meta_sidecar_key, is_audio_key, ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend,
    AUDIO_EXTENSIONS,
};

use std::path::Path;

use bookclerk_config::{normalize_storage_prefix, Config, OutputBackendKind};

/// Build the configured storage backend(s).
///
/// When multiple `[output.*]` destination plugins are enabled, returns a
/// [`FanoutBackend`] that writes to all of them.
pub async fn from_config(config: &Config) -> Result<Box<dyn StorageBackend>> {
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
                let files_dir = config
                    .paths
                    .as_ref()
                    .map(|p| p.files_dir.as_path())
                    .unwrap_or_else(|| Path::new("."));
                backends.push(Box::new(
                    S3Backend::from_config(&config.output.s3, &prefix, files_dir).await?,
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
