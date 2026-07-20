//! Storage backends for liberated audiobooks, covers, and PDFs.

mod error;
mod local;
mod s3;
mod traits;

pub use error::{Result, StorageError};
pub use local::LocalFsBackend;
pub use s3::S3Backend;
pub use traits::{ObjectInfo, ObjectMeta, StorageBackend};

use libation_config::{Config, StorageBackendKind};

/// Build the configured storage backend.
pub async fn from_config(config: &Config) -> Result<Box<dyn StorageBackend>> {
    match config.storage.backend {
        StorageBackendKind::Local => {
            let root = &config.storage.local.root;
            Ok(Box::new(LocalFsBackend::new(root.clone())?))
        }
        StorageBackendKind::S3 => {
            let backend = S3Backend::from_config(&config.storage.s3).await?;
            Ok(Box::new(backend))
        }
    }
}
