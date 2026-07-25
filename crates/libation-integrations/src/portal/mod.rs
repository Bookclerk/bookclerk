//! Connect portal HTTP nest (claim ticket + integration credential login).

mod brands;
mod html;
mod routes;

#[cfg(test)]
mod enabled_tests;

pub use routes::{mint_for_external_user, portal_router, PortalState};
