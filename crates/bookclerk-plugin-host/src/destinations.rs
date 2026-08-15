//! Build acquire destinations (in-process + external output plugins).

use bookclerk_acquire::{AcquireDestination, AcquireDestinations, AcquireError, Result};
use bookclerk_config::{normalize_storage_prefix, Config, OutputBackendKind};
use bookclerk_library::LibraryStore;
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{LocalFsBackend, S3Backend, StorageBackend};

use crate::host::DestinationRegistry;

/// Build destination backends for acquire, preferring external output plugins when loaded.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn build_acquire_destinations(
    config: &Config,
    library: Option<&LibraryStore>,
    destinations: &DestinationRegistry,
) -> Result<AcquireDestinations> {
    config.output.validate_destinations().map_err(|err| {
        AcquireError::Other(anyhow::anyhow!("invalid output destination config: {err}"))
    })?;

    let primary = config.output.primary_backend().ok_or_else(|| {
        AcquireError::Other(anyhow::anyhow!(
            "enable at least one of [output.local] or [output.s3]"
        ))
    })?;
    let db = library.map(|store| store.db());
    let mut items = Vec::new();
    for kind in config.output.enabled_backends() {
        let backend: Box<dyn StorageBackend> = match kind {
            OutputBackendKind::Local => {
                if let Some(ext) = destinations.local() {
                    ext.clone_box()
                } else {
                    let prefix = normalize_storage_prefix(config.output.local.prefix.trim());
                    Box::new(LocalFsBackend::with_prefix(
                        config.output.local.root.clone(),
                        &prefix,
                    )?)
                }
            }
            OutputBackendKind::S3 => {
                if let Some(ext) = destinations.s3() {
                    ext.clone_box()
                } else {
                    let prefix = normalize_storage_prefix(config.output.s3.prefix.trim());
                    Box::new(S3Backend::from_config(&config.output.s3, &prefix, db).await?)
                }
            }
        };
        items.push(AcquireDestination {
            kind,
            backend,
            options: DownloadOptions::for_output_backend(config, kind),
        });
    }
    Ok(AcquireDestinations {
        items,
        primary,
        multi_destination: config.output.multi_destination,
    })
}

/// Build listing/storage backends (mirrors [`bookclerk_storage::from_config`]).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn build_storage_backend(
    config: &Config,
    library: Option<&LibraryStore>,
    destinations: &DestinationRegistry,
) -> bookclerk_storage::Result<Box<dyn StorageBackend>> {
    use bookclerk_storage::{FanoutBackend, StorageError};

    config
        .output
        .validate_destinations()
        .map_err(|err| StorageError::InvalidKey(err.to_string()))?;

    let db = library.map(|store| store.db());
    let mut backends: Vec<Box<dyn StorageBackend>> = Vec::new();
    for kind in config.output.enabled_backends() {
        match kind {
            OutputBackendKind::Local => {
                if let Some(ext) = destinations.local() {
                    backends.push(ext.clone_box());
                } else {
                    let prefix = normalize_storage_prefix(config.output.local.prefix.trim());
                    backends.push(Box::new(LocalFsBackend::with_prefix(
                        config.output.local.root.clone(),
                        &prefix,
                    )?));
                }
            }
            OutputBackendKind::S3 => {
                if let Some(ext) = destinations.s3() {
                    backends.push(ext.clone_box());
                } else {
                    let prefix = normalize_storage_prefix(config.output.s3.prefix.trim());
                    backends.push(Box::new(
                        S3Backend::from_config(&config.output.s3, &prefix, db).await?,
                    ));
                }
            }
        }
    }

    if backends.len() == 1 {
        return Ok(backends.remove(0));
    }
    Ok(Box::new(FanoutBackend::new(backends)?))
}
