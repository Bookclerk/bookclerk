//! Audiobookshelf integration plugin: Workers RPC guest plus the
//! [`bookclerk_integrations::Integration`] trait used by the guest binary.
//!
//! Hosts load this package as a staged external guest. Do not link this crate
//! into `bookclerk` / `bookclerkd` for in-process execution.

mod brand;
mod client;
pub mod guest;
mod integration;
mod listening;

pub use brand::{brand_for_id, matches_id, BRAND};
pub use client::{
    AbsApiClient, AbsLibrary, AbsLibraryItem, AbsMediaProgress, AbsUser, AbsUserDetail,
};
pub use guest::AbsGuestState;
pub use integration::{AbsIntegration, AudiobookshelfConfig};
pub use listening::{collect_listening_snapshots, sync_listening_progress};

/// Integration id used in describe() and `[integrations.audiobookshelf]`.
pub const ID: &str = "audiobookshelf";

/// Bookclerk-as-IdP client templates declared by this plugin (`oidcClients`).
#[must_use]
pub fn oidc_client_templates() -> Vec<bookclerk_plugin_sdk::OidcClientTemplate> {
    vec![bookclerk_plugin_sdk::OidcClientTemplate {
        client_id: ID.into(),
        display_name: "Audiobookshelf".into(),
        callback_path: "/auth/openid/callback".into(),
        public_client: true,
        default_scopes: vec!["openid".into(), "profile".into()],
        issue_refresh_token: true,
        origin_config_key: "integrations.audiobookshelf.base_url".into(),
    }]
}
