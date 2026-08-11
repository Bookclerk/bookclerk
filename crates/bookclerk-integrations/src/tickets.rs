//! Claim ticket minting and hashing.

use bookclerk_config::IntegrationsConfig;
use bookclerk_library::{ClaimTicketRecord, LibraryStore, PortalIdentity};
use chrono::{Duration, Utc};
use rand::Rng;

use crate::error::{IntegrationError, Result};
use crate::types::ExternalUser;

pub use bookclerk_library::hash_token;

/// Generate a URL-safe random token.
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Freshly minted claim ticket (includes plaintext once).
#[derive(Debug, Clone)]
pub struct MintedClaimTicket {
    /// Persisted claim-ticket row (hash only; no plaintext token).
    pub record: ClaimTicketRecord,
    /// Plaintext secret shown once at mint time; only a hash is persisted.
    pub token: String,
    /// Portal identity bound to this ticket.
    pub identity: PortalIdentity,
    /// Shareable SPA claim URL when `public_origin` is configured.
    pub portal_url: Option<String>,
}

/// Ensure identity exists and mint a claim ticket.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `integrations` - Integrations config (TTL, public origin, …).
/// * `user` - External user identity from an integration login/watcher.
/// * `created_by` - Actor string recorded on the claim ticket (`daemon`, CLI user, …).
///
/// # Returns
///
/// On success, the inner `MintedClaimTicket` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn mint_claim_ticket(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    user: &ExternalUser,
    created_by: &str,
) -> Result<MintedClaimTicket> {
    let identity = library
        .upsert_portal_identity(
            &user.provider,
            &user.external_user_id,
            user.display_name.as_deref(),
        )
        .await?;
    let token = generate_token();
    let token_hash = hash_token(&token);
    let expires = Utc::now() + Duration::hours(integrations.claim_ticket_ttl_hours as i64);
    let record = library
        .insert_claim_ticket(&token_hash, Some(identity.id), expires, created_by)
        .await?;
    let portal_url = ticket_portal_url(integrations, &token);
    Ok(MintedClaimTicket {
        record,
        token,
        identity,
        portal_url,
    })
}

/// Build a shareable SPA claim URL when `public_origin` is configured.
///
/// Opens the Bookclerk GUI login page with `?ticket=` (see `LoginPage`).
///
/// # Arguments
///
/// * `integrations` - Integrations config (TTL, public origin, …).
/// * `token` - Plaintext claim or session token.
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
#[must_use]
pub fn ticket_portal_url(integrations: &IntegrationsConfig, token: &str) -> Option<String> {
    let origin = integrations.public_origin.as_deref()?.trim_end_matches('/');
    Some(format!("{origin}/?ticket={token}"))
}

/// Redeem a claim ticket into a portal session cookie value (plaintext).
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `integrations` - Integrations config (TTL, public origin, …).
/// * `raw_ticket` - Plaintext claim ticket from the SPA / share URL.
///
/// # Returns
///
/// The successful result value for this operation.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn redeem_ticket_to_session(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    raw_ticket: &str,
) -> Result<(String, PortalIdentity)> {
    let hash = hash_token(raw_ticket);
    let ticket = library.redeem_claim_ticket(&hash).await?;
    let identity_id = ticket
        .identity_id
        .ok_or_else(|| IntegrationError::message("claim ticket is not bound to an identity"))?;
    let identity = library
        .get_portal_identity_by_id(identity_id)
        .await?
        .ok_or_else(|| IntegrationError::message("portal identity missing for claim ticket"))?;
    let session = generate_token();
    let session_hash = hash_token(&session);
    let expires = Utc::now() + Duration::hours(integrations.portal_session_ttl_hours as i64);
    library
        .insert_portal_session(&session_hash, identity.id, expires)
        .await?;
    Ok((session, identity))
}

/// Create a portal session for an already-resolved identity (credential login).
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `integrations` - Integrations config (TTL, public origin, …).
/// * `identity` - Already-resolved portal identity.
///
/// # Returns
///
/// On success, the inner `String` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn session_for_identity(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    identity: &PortalIdentity,
) -> Result<String> {
    let session = generate_token();
    let session_hash = hash_token(&session);
    let expires = Utc::now() + Duration::hours(integrations.portal_session_ttl_hours as i64);
    library
        .insert_portal_session(&session_hash, identity.id, expires)
        .await?;
    Ok(session)
}

/// Resolve identity from a portal session cookie value.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `raw_session` - Plaintext portal session cookie value.
///
/// # Returns
///
/// On success, the inner `Option<PortalIdentity>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn identity_from_session(
    library: &LibraryStore,
    raw_session: &str,
) -> Result<Option<PortalIdentity>> {
    let hash = hash_token(raw_session);
    Ok(library.get_portal_session_identity(&hash).await?)
}
