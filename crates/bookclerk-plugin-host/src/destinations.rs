//! Build acquire destinations from the deployed output plugins.
//!
//! Every destination is a deployed plugin instance; there is no in-process
//! fallback built from `config.toml`. A host with no present destination
//! deployment cannot acquire.

use bookclerk_acquire::{AcquireDestination, AcquireDestinations, AcquireError, Result};
use bookclerk_config::Config;
use bookclerk_source::DownloadOptions;
use bookclerk_storage::StorageBackend;

use crate::host::DestinationRegistry;

/// What a host with no deployed destination says when asked to store.
const NO_DESTINATION: &str =
    "no output destination is deployed on this host; deploy a storage plugin such as `local` or `s3`";

/// Build destination backends for acquire from the deployed output plugins.
///
/// Every deployed storage plugin instance is a destination; path
/// destinations come first, and the first destination is the primary.
///
/// # Errors
///
/// Returns an error when no destination is deployed on this host.
pub fn build_acquire_destinations(
    config: &Config,
    destinations: &DestinationRegistry,
) -> Result<AcquireDestinations> {
    let deployed = destinations.destinations();
    let primary = deployed
        .first()
        .map(|dest| dest.id.clone())
        .ok_or_else(|| AcquireError::Other(anyhow::anyhow!(NO_DESTINATION)))?;
    let items = deployed
        .into_iter()
        .map(|dest| AcquireDestination {
            options: DownloadOptions::for_destination(
                config,
                dest.key_style,
                &dest.settings.naming,
                &dest.settings.prefix,
            ),
            id: dest.id,
            backend: dest.backend.clone_box(),
        })
        .collect();
    Ok(AcquireDestinations {
        items,
        primary,
        multi_destination: config.output.multi_destination,
    })
}

/// Build listing/storage backends from the deployed output plugins.
///
/// # Errors
///
/// Returns an error when no destination is deployed.
pub fn build_storage_backend(
    destinations: &DestinationRegistry,
) -> bookclerk_storage::Result<Box<dyn StorageBackend>> {
    use bookclerk_storage::{FanoutBackend, StorageError};

    let mut backends: Vec<Box<dyn StorageBackend>> = destinations
        .destinations()
        .into_iter()
        .map(|dest| dest.backend.clone_box())
        .collect();
    if backends.is_empty() {
        return Err(StorageError::InvalidKey(NO_DESTINATION.into()));
    }
    if backends.len() == 1 {
        return Ok(backends.remove(0));
    }
    Ok(Box::new(FanoutBackend::new(backends)?))
}
