//! In-process adapters that speak Cap'n Proto `api_version = 3` to external plugin processes.

mod database;
mod declared_media;
mod destination;
mod destination_local;
mod download_sink;
pub(crate) mod integration;
mod plugin_backups;
mod plugin_migration_apply;
mod scan_pass;
mod source;

pub use database::{
    backup_adapter_id, database_connect_bindings, load_external_database, migrate_database_plugin,
    migrate_library_schema, open_library_store, open_library_store_for_plugin, DatabaseRegistry,
    ExternalDatabase,
};
pub(crate) use destination::spawn_deployed_storage;
pub use destination::{
    storage_role, DeployedDestination, DeployedIntake, DestinationRegistry, DestinationSession,
    DestinationSettings, StorageRole,
};
pub use integration::ExternalIntegration;
pub use plugin_backups::{export_registered_plugin_units, restore_plugin_backup_units};
pub(crate) use source::list_scope_accounts;
pub use source::{ExternalSource, HandedPass, HandedScan, RefreshedCredentialSink};

/// Longest the host waits for one spawn-time `health` answer. A guest that
/// stays alive but never answers would otherwise hold its deployment out of
/// both `Healthy` and `Error`.
pub(crate) const HEALTH_PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Awaits one spawn-time health probe, bounded by `deadline`.
///
/// # Arguments
///
/// * `deadline` - How long the probe may take ([`HEALTH_PROBE_DEADLINE`]
///   outside tests).
/// * `probe` - The probe; its own error is returned as is.
///
/// # Errors
///
/// Returns the probe's error, or a `deadline_exceeded` error naming the
/// deadline when the guest does not answer in time.
pub(crate) async fn bounded_health(
    deadline: std::time::Duration,
    probe: impl std::future::Future<Output = crate::Result<()>>,
) -> crate::Result<()> {
    match tokio::time::timeout(deadline, probe).await {
        Ok(result) => result,
        Err(_) => Err(crate::PluginError::from_abi(
            Some("deadline_exceeded"),
            format!("health did not answer within {} s", deadline.as_secs()),
        )),
    }
}

#[cfg(test)]
mod rpc_proxy_insert;
#[cfg(test)]
mod rpc_proxy_like;
#[cfg(test)]
mod rpc_streamed_reads;

#[cfg(test)]
mod health_deadline_tests {
    use super::bounded_health;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn a_probe_that_never_answers_is_a_deadline_failure() {
        let err = bounded_health(Duration::from_secs(30), std::future::pending())
            .await
            .expect_err("a hung guest fails the probe");
        assert!(err.to_string().starts_with("deadline_exceeded:"), "{err}");
    }

    #[tokio::test]
    async fn an_answer_in_time_is_returned_as_is() {
        bounded_health(Duration::from_secs(30), async { Ok(()) })
            .await
            .expect("healthy");
        let err = bounded_health(Duration::from_secs(30), async {
            Err(crate::PluginError::message("health failed"))
        })
        .await
        .expect_err("the probe's own error");
        assert_eq!(err.to_string(), "health failed");
    }
}
