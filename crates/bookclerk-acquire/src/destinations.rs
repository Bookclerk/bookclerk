//! Destination-specific storage backends for one acquire operation.

use bookclerk_config::{normalize_storage_prefix, Config, MultiDestinationMode, OutputBackendKind};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{FanoutBackend, LocalFsBackend, S3Backend, StorageBackend};

use crate::error::{AcquireError, Result};

pub struct AcquireDestination {
    pub kind: OutputBackendKind,
    pub backend: Box<dyn StorageBackend>,
    pub options: DownloadOptions,
}

pub struct AcquireDestinations {
    pub items: Vec<AcquireDestination>,
    pub primary: OutputBackendKind,
    pub multi_destination: MultiDestinationMode,
}

impl AcquireDestinations {
    pub async fn from_config(config: &Config) -> Result<Self> {
        config.output.validate_destinations().map_err(|err| {
            AcquireError::Other(anyhow::anyhow!("invalid output destination config: {err}"))
        })?;

        let primary = config.output.primary_backend().ok_or_else(|| {
            AcquireError::Other(anyhow::anyhow!(
                "enable at least one of [output.local] or [output.s3]"
            ))
        })?;
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
                    let files_dir = config
                        .paths
                        .as_ref()
                        .map(|p| p.files_dir.as_path())
                        .unwrap_or_else(|| std::path::Path::new("."));
                    Box::new(S3Backend::from_config(&config.output.s3, &prefix, files_dir).await?)
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

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn primary_destination(&self) -> &AcquireDestination {
        self.items
            .iter()
            .find(|dest| dest.kind == self.primary)
            .unwrap_or_else(|| &self.items[0])
    }

    #[must_use]
    pub fn destination(&self, kind: OutputBackendKind) -> Option<&AcquireDestination> {
        self.items.iter().find(|dest| dest.kind == kind)
    }

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
