//! In-process adapters that speak Cap'n Proto `api_version = 2` to external plugin processes.

mod database;
mod destination;
mod destination_local;
mod integration;
mod plugin_backups;
mod plugin_migration_apply;
mod source;

pub use database::{
    backup_adapter_id, database_connect_context, load_external_database, migrate_database_plugin,
    migrate_library_schema, open_library_store, open_library_store_for_plugin, DatabaseRegistry,
    ExternalDatabase,
};
pub use destination::{load_external_destinations, DestinationRegistry};
pub use integration::{load_external_integrations, ExternalIntegration};
pub use plugin_backups::{export_registered_plugin_units, restore_plugin_backup_units};
pub use source::{load_external_sources, ExternalSource};
