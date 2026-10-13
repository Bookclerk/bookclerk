//! Build acquire destinations from the deployed output plugins.
//!
//! Every destination is a deployed plugin instance; there is no in-process
//! fallback built from `config.toml`. A host with no present destination
//! deployment cannot acquire.

use bookclerk_acquire::{AcquireDestination, AcquireDestinations, AcquireError, Result};
use bookclerk_config::{Config, OutputBackendKind};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::StorageBackend;

use crate::host::DestinationRegistry;

/// One deployed destination: its kind and a handle on its storage backend.
type DeployedBackend = (OutputBackendKind, Box<dyn StorageBackend>);

/// Deployed `(kind, backend)` pairs, local first so it is the primary when present.
fn deployed_backends(
    destinations: &DestinationRegistry,
) -> std::result::Result<Vec<DeployedBackend>, String> {
    let mut out = Vec::new();
    if let Some(local) = destinations.require_local()? {
        out.push((OutputBackendKind::Local, local.clone_box()));
    }
    if let Some(s3) = destinations.require_s3()? {
        out.push((OutputBackendKind::S3, s3.clone_box()));
    }
    Ok(out)
}

/// Build destination backends for acquire from the deployed output plugins.
///
/// # Errors
///
/// Returns an error when no destination is deployed on this host or two
/// instances of one kind are deployed.
pub fn build_acquire_destinations(
    config: &Config,
    destinations: &DestinationRegistry,
) -> Result<AcquireDestinations> {
    let backends =
        deployed_backends(destinations).map_err(|err| AcquireError::Other(anyhow::anyhow!(err)))?;
    let primary = backends.first().map(|(kind, _)| *kind).ok_or_else(|| {
        AcquireError::Other(anyhow::anyhow!(
            "no output destination is deployed on this host; enable the `local` or `s3` plugin"
        ))
    })?;
    let items = backends
        .into_iter()
        .map(|(kind, backend)| {
            let settings = destinations.settings_for(kind).unwrap_or_default();
            AcquireDestination {
                kind,
                options: DownloadOptions::for_destination(
                    config,
                    kind,
                    &settings.naming,
                    &settings.prefix,
                ),
                backend,
            }
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
/// Returns an error when no destination is deployed or two instances of one
/// kind are deployed.
pub fn build_storage_backend(
    destinations: &DestinationRegistry,
) -> bookclerk_storage::Result<Box<dyn StorageBackend>> {
    use bookclerk_storage::{FanoutBackend, StorageError};

    let mut backends: Vec<Box<dyn StorageBackend>> = deployed_backends(destinations)
        .map_err(StorageError::InvalidKey)?
        .into_iter()
        .map(|(_, backend)| backend)
        .collect();
    if backends.is_empty() {
        return Err(StorageError::InvalidKey(
            "no output destination is deployed on this host; enable the `local` or `s3` plugin"
                .into(),
        ));
    }
    if backends.len() == 1 {
        return Ok(backends.remove(0));
    }
    Ok(Box::new(FanoutBackend::new(backends)?))
}
