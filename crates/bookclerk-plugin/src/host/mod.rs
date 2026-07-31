//! In-process adapters that speak JSON-RPC to external plugin processes.

mod destination;
mod integration;
mod source;

pub use destination::{load_external_destinations, DestinationRegistry, ExternalDestination};
pub use integration::{load_external_integrations, ExternalIntegration};
pub use source::{load_external_sources, ExternalSource};
