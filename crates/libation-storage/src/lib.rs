//! Storage backends for liberated audiobooks, covers, and PDFs.

mod error;
mod local;
mod s3;
mod traits;

pub use error::{Result, StorageError};
pub use local::LocalFsBackend;
pub use s3::S3Backend;
pub use traits::{
    is_audio_key, libation_meta_sidecar_key, ObjectInfo, ObjectMeta, ObjectProbe, StorageBackend,
    AUDIO_EXTENSIONS,
};

use libation_config::{normalize_storage_prefix, Config, StorageBackendKind};

/// Build the configured storage backend.
pub async fn from_config(config: &Config) -> Result<Box<dyn StorageBackend>> {
    let prefix = config.storage.effective_prefix();
    match config.storage.backend {
        StorageBackendKind::Local => {
            let root = &config.storage.local.root;
            Ok(Box::new(LocalFsBackend::with_prefix(
                root.clone(),
                &prefix,
            )?))
        }
        StorageBackendKind::S3 => {
            let backend = S3Backend::from_config(&config.storage.s3, &prefix).await?;
            Ok(Box::new(backend))
        }
    }
}

pub(crate) fn normalize_prefix(prefix: &str) -> String {
    normalize_storage_prefix(prefix)
}
