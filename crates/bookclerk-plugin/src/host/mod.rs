//! In-process adapters that speak JSON-RPC to external plugin processes.

mod database;
mod destination;
mod integration;
mod source;

pub use database::{
    load_external_database, open_library_store, DatabaseRegistry, ExternalDatabase,
};
pub use destination::{load_external_destinations, DestinationRegistry, ExternalDestination};
pub use integration::{load_external_integrations, ExternalIntegration};
pub use source::{load_external_sources, ExternalSource};
