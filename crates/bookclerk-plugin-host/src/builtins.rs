//! External-only source and integration loading.
//!
//! Hosts (`bookclerk` / `bookclerkd`) discover staged guests under `plugins/`
//! and talk to them through the workerd front door. Production hosts never
//! link storefront or integration implementation crates in-process.

use bookclerk_config::Config;
use bookclerk_integrations::IntegrationRegistry;
use bookclerk_source::SourceRegistry;

use crate::rpc_session::SessionServices;

/// Discovered external source plugins.
///
/// `services` carries the host facilities external guests may receive as
/// bindings (the `EVENTS` outbox); pass [`SessionServices::default`] when the
/// caller has no library store.
///
/// # Errors
///
/// Returns an error when discovery or guest spawn fails.
pub async fn load_sources(
    config: &Config,
    services: &SessionServices,
) -> crate::Result<SourceRegistry> {
    load_sources_skipping(config, services, &std::collections::BTreeSet::new()).await
}

/// [`load_sources`] that leaves `skip` plugin keys to the deployment reconciler.
///
/// # Errors
///
/// Returns an error when discovery or guest spawn fails.
pub async fn load_sources_skipping(
    config: &Config,
    services: &SessionServices,
    skip: &std::collections::BTreeSet<String>,
) -> crate::Result<SourceRegistry> {
    let mut registry = SourceRegistry::new();
    crate::load_external_sources_skipping(config, &mut registry, services, skip).await?;
    Ok(registry)
}

/// Discovered external integration plugins.
///
/// `services` carries the host facilities external guests may receive as
/// bindings (the `EVENTS` outbox); pass [`SessionServices::default`] when the
/// caller has no library store.
///
/// # Errors
///
/// Returns an error when discovery or guest spawn fails.
pub async fn load_integrations(
    config: &Config,
    services: &SessionServices,
) -> crate::Result<IntegrationRegistry> {
    load_integrations_skipping(config, services, &std::collections::BTreeSet::new()).await
}

/// [`load_integrations`] that leaves `skip` plugin keys to the deployment reconciler.
///
/// # Errors
///
/// Returns an error when discovery or guest spawn fails.
pub async fn load_integrations_skipping(
    config: &Config,
    services: &SessionServices,
    skip: &std::collections::BTreeSet<String>,
) -> crate::Result<IntegrationRegistry> {
    let mut registry = IntegrationRegistry::new();
    crate::load_external_integrations_skipping(config, &mut registry, services, skip).await?;
    Ok(registry)
}
