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
    Some(format!("{origin}/invite?ticket={token}"))
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
    redeem_ticket_to_session_with_client(library, integrations, raw_ticket, None, None).await
}

/// Resolve a claim ticket's identity without consuming the ticket.
///
/// Used so invite/reset password validation can fail without burning the
/// one-time token. Credential mutation happens only inside
/// [`redeem_ticket_to_session_with_client`].
pub async fn inspect_claim_ticket(
    library: &LibraryStore,
    raw_ticket: &str,
) -> Result<PortalIdentity> {
    let hash = hash_token(raw_ticket);
    let ticket = library
        .get_claim_ticket_by_hash(&hash)
        .await?
        .ok_or_else(|| {
            IntegrationError::message("claim ticket invalid, expired, or already redeemed")
        })?;
    if ticket.redeemed_at.is_some() || ticket.expires_at <= Utc::now() {
        return Err(IntegrationError::message(
            "claim ticket invalid, expired, or already redeemed",
        ));
    }
    let identity_id = ticket
        .identity_id
        .ok_or_else(|| IntegrationError::message("claim ticket is not bound to an identity"))?;
    library
        .get_portal_identity_by_id(identity_id)
        .await?
        .ok_or_else(|| IntegrationError::message("portal identity missing for claim ticket"))
}

/// Redeem a claim ticket and mint a portal session with optional client metadata.
///
/// `password_hash` is applied in the same transaction as ticket consume and
/// session insert, and only while the ticket-bound local user has no password.
pub async fn redeem_ticket_to_session_with_client(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    raw_ticket: &str,
    client: Option<&bookclerk_library::SessionClientInfo>,
    password_hash: Option<&str>,
) -> Result<(String, PortalIdentity)> {
    let hash = hash_token(raw_ticket);
    let session = generate_token();
    let session_hash = hash_token(&session);
    let expires = Utc::now() + Duration::hours(integrations.portal_session_ttl_hours as i64);
    let identity = library
        .redeem_claim_ticket_to_session(&hash, &session_hash, expires, client, password_hash)
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
    session_for_identity_with_client(library, integrations, identity, None).await
}

/// Create a portal session for an identity with optional client metadata.
pub async fn session_for_identity_with_client(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    identity: &PortalIdentity,
    client: Option<&bookclerk_library::SessionClientInfo>,
) -> Result<String> {
    let session = generate_token();
    let session_hash = hash_token(&session);
    let expires = Utc::now() + Duration::hours(integrations.portal_session_ttl_hours as i64);
    library
        .insert_portal_session_with_client(&session_hash, identity.id, expires, client)
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
