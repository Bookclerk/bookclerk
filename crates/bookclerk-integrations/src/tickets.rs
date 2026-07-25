//! Claim ticket minting and hashing.

use bookclerk_config::IntegrationsConfig;
use bookclerk_library::{ClaimTicketRecord, LibraryStore, PortalIdentity};
use chrono::{Duration, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::error::{IntegrationError, Result};
use crate::types::ExternalUser;

/// Hash a raw ticket/session token for storage.
#[must_use]
pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

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
    pub record: ClaimTicketRecord,
    pub token: String,
    pub identity: PortalIdentity,
    pub portal_url: Option<String>,
}

/// Ensure identity exists and mint a claim ticket.
pub fn mint_claim_ticket(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    user: &ExternalUser,
    created_by: &str,
) -> Result<MintedClaimTicket> {
    let identity = library.upsert_portal_identity(
        &user.provider,
        &user.external_user_id,
        user.display_name.as_deref(),
    )?;
    let token = generate_token();
    let token_hash = hash_token(&token);
    let expires = Utc::now() + Duration::hours(integrations.claim_ticket_ttl_hours as i64);
    let record =
        library.insert_claim_ticket(&token_hash, Some(identity.id), expires, created_by)?;
    let portal_url = ticket_portal_url(integrations, &token);
    Ok(MintedClaimTicket {
        record,
        token,
        identity,
        portal_url,
    })
}

/// Build a shareable portal URL when `public_origin` is configured.
#[must_use]
pub fn ticket_portal_url(integrations: &IntegrationsConfig, token: &str) -> Option<String> {
    let origin = integrations.public_origin.as_deref()?.trim_end_matches('/');
    let base = normalize_portal_base(&integrations.portal_base_path);
    // Prefer `/connect?ticket=` (axum nest matches `/connect`, not `/connect/`).
    Some(format!("{origin}{base}?ticket={token}"))
}

#[must_use]
pub fn normalize_portal_base(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return String::new();
    }
    let with_slash = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    with_slash.trim_end_matches('/').to_string()
}

/// Redeem a claim ticket into a portal session cookie value (plaintext).
pub fn redeem_ticket_to_session(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    raw_ticket: &str,
) -> Result<(String, PortalIdentity)> {
    let hash = hash_token(raw_ticket);
    let ticket = library.redeem_claim_ticket(&hash)?;
    let identity_id = ticket
        .identity_id
        .ok_or_else(|| IntegrationError::message("claim ticket is not bound to an identity"))?;
    let identity = library
        .get_portal_identity_by_id(identity_id)?
        .ok_or_else(|| IntegrationError::message("portal identity missing for claim ticket"))?;
    let session = generate_token();
    let session_hash = hash_token(&session);
    let expires = Utc::now() + Duration::hours(integrations.portal_session_ttl_hours as i64);
    library.insert_portal_session(&session_hash, identity.id, expires)?;
    Ok((session, identity))
}

/// Create a portal session for an already-resolved identity (credential login).
pub fn session_for_identity(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    identity: &PortalIdentity,
) -> Result<String> {
    let session = generate_token();
    let session_hash = hash_token(&session);
    let expires = Utc::now() + Duration::hours(integrations.portal_session_ttl_hours as i64);
    library.insert_portal_session(&session_hash, identity.id, expires)?;
    Ok(session)
}

/// Resolve identity from a portal session cookie value.
pub fn identity_from_session(
    library: &LibraryStore,
    raw_session: &str,
) -> Result<Option<PortalIdentity>> {
    let hash = hash_token(raw_session);
    Ok(library.get_portal_session_identity(&hash)?)
}
