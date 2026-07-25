//! Outbound integrations and connect portal.
//!
//! Host binaries should depend on [`Integration`] / [`IntegrationRegistry`] only.
//! Adapter clients (e.g. Audiobookshelf HTTP) stay inside this crate’s plugin
//! modules and are not part of the host-facing API.

/// Audiobookshelf plugin (HTTP client, brand, integration adapter).
///
/// Host binaries should prefer [`Integration`] / [`IntegrationRegistry`]; use
/// this module only when deliberately talking to ABS-specific APIs.
pub mod abs;

mod brand;
mod error;
mod factory;
mod hooks;
mod portal;
mod registry;
mod tickets;
mod traits;
mod types;

pub use brand::Brand;
pub use error::{IntegrationError, Result};
pub use factory::from_config;
pub use hooks::emit_book_acquired;
pub use portal::{mint_for_external_user, portal_router, PortalState};
pub use registry::IntegrationRegistry;
pub use tickets::{
    generate_token, hash_token, identity_from_session, mint_claim_ticket, normalize_portal_base,
    redeem_ticket_to_session, session_for_identity, ticket_portal_url, MintedClaimTicket,
};
pub use traits::{Integration, IntegrationContext};
pub use types::{ExternalUser, IntegrationEvent, IntegrationHealth};
