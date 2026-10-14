//! Source registry access for jobs.
//!
//! The deployment reconciler owns every guest this process runs and keeps
//! [`crate::api::AppState::sources`] current: each registered source is a
//! present deployment on this host, keyed by its plugin instance id, with the
//! plugin's default instance recorded so a bare plugin key or alias routes.
//! A job takes a clone of that registry; nothing is spawned per job.

use bookclerk_source::SourceRegistry;

/// Source registry for one job: a snapshot of the live registry.
pub async fn registry_for_job(state: &crate::api::AppState) -> SourceRegistry {
    state.sources.read().await.clone()
}
