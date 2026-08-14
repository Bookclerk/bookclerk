//! Destination-specific storage backends for one acquire operation.

use bookclerk_config::{normalize_storage_prefix, Config, MultiDestinationMode, OutputBackendKind};
use bookclerk_library::LibraryStore;
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{FanoutBackend, LocalFsBackend, S3Backend, StorageBackend};

use crate::error::{AcquireError, Result};

/// One configured output destination for an acquire run.
pub struct AcquireDestination {
    /// Destination backend kind (`local`, `s3`, …).
    pub kind: OutputBackendKind,
    /// Constructed storage backend for this destination.
    pub backend: Box<dyn StorageBackend>,
    /// Source download options (quality, chapter prefs, …).
    pub options: DownloadOptions,
}

/// Ordered set of acquire destinations built from config.
pub struct AcquireDestinations {
    /// Configured acquire destinations in evaluation order.
    pub items: Vec<AcquireDestination>,
    /// Index of the primary destination used for library status keys.
    pub primary: OutputBackendKind,
    /// Whether multiple destinations are enabled for this run.
    pub multi_destination: MultiDestinationMode,
}

impl AcquireDestinations {
    /// Builds destination backends from `[output]` config.
    ///
    /// `library` is used when the S3 destination loads credentials from
    /// `encrypted_secrets` (after `BOOKCLERK_AWS_*` env override, before
    /// the AWS SDK default chain).
    ///
    /// # Arguments
    ///
    /// * `config` - Host config with enabled `[output.local]` / `[output.s3]`.
    /// * `library` - Optional library store for sealed S3 credentials.
    ///
    /// # Returns
    ///
    /// Constructed destinations with a chosen primary backend.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError`] when destination config is invalid or a backend
    /// cannot be constructed.
    pub async fn from_config(config: &Config, library: Option<&LibraryStore>) -> Result<Self> {
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
                    let prefix = normalize_storage_prefix(config.output.local.prefix.trim());
                    Box::new(LocalFsBackend::with_prefix(
                        config.output.local.root.clone(),
                        &prefix,
                    )?)
                }
                OutputBackendKind::S3 => {
                    let prefix = normalize_storage_prefix(config.output.s3.prefix.trim());
                    Box::new(S3Backend::from_config(&config.output.s3, &prefix, db).await?)
                }
            };
            items.push(AcquireDestination {
                kind,
                backend,
                options: DownloadOptions::for_output_backend(config, kind),
            });
        }
        Ok(Self {
            items,
            primary,
            multi_destination: config.output.multi_destination,
        })
    }

    /// Returns the number of configured destinations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns true when no destinations are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns the primary destination used for library storage keys.
    #[must_use]
    pub fn primary_destination(&self) -> &AcquireDestination {
        self.items
            .iter()
            .find(|dest| dest.kind == self.primary)
            .unwrap_or_else(|| &self.items[0])
    }

    /// Returns the destination whose kind equals `kind`, if configured.
    #[must_use]
    pub fn destination(&self, kind: OutputBackendKind) -> Option<&AcquireDestination> {
        self.items.iter().find(|dest| dest.kind == kind)
    }

    /// Clone destination backends into a listing/match storage handle.
    ///
    /// Prefer this over a second [`bookclerk_storage::from_config`] call when
    /// acquire already built destinations (avoids decrypting S3 secrets twice).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn listing_backend(&self) -> Result<Box<dyn StorageBackend>> {
        let backends = self
            .items
            .iter()
            .map(|dest| dest.backend.clone_box())
            .collect::<Vec<_>>();
        if backends.len() == 1 {
            let mut backends = backends;
            return Ok(backends.remove(0));
        }
        Ok(Box::new(FanoutBackend::new(backends)?))
    }

    /// Consumes this set and returns a listing backend over the primary destination.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn into_listing_backend(self) -> Result<Box<dyn StorageBackend>> {
        let backends = self
            .items
            .into_iter()
            .map(|dest| dest.backend)
            .collect::<Vec<_>>();
        if backends.len() == 1 {
            let mut backends = backends;
            return Ok(backends.remove(0));
        }
        Ok(Box::new(FanoutBackend::new(backends)?))
    }
}
