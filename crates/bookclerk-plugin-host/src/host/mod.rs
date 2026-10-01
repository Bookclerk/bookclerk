//! In-process adapters that speak Cap'n Proto `api_version = 3` to external plugin processes.

mod database;
mod destination;
mod destination_local;
mod integration;
mod plugin_backups;
mod plugin_migration_apply;
mod source;

pub use database::{
    backup_adapter_id, database_connect_bindings, load_external_database, migrate_database_plugin,
    migrate_library_schema, open_library_store, open_library_store_for_plugin, DatabaseRegistry,
    ExternalDatabase,
};
pub(crate) use destination::spawn_deployed_storage;
pub use destination::{
    load_external_destinations, load_external_destinations_with_store, DestinationRegistry,
};
pub use integration::{
    load_external_integrations, load_external_integrations_skipping, ExternalIntegration,
};
pub use plugin_backups::{export_registered_plugin_units, restore_plugin_backup_units};
pub use source::{load_external_sources, load_external_sources_skipping, ExternalSource};

#[cfg(test)]
mod rpc_proxy_like;
