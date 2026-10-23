//! Portal HTTP APIs for SPA Accounts / claim tickets (no legacy HTML shell).

mod brands;
mod routes;

pub use routes::{
    mint_for_external_user, portal_identity_from_headers, portal_spa_router, portal_spa_routes,
    redeem_lose_next_responses, PortalState,
};
