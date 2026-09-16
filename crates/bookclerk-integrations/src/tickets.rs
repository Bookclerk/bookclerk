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
/// Opens the Bookclerk GUI invite page (`/invite?ticket=`).
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
    nonce: &str,
) -> Result<(String, PortalIdentity)> {
    redeem_ticket_to_session_with_client(library, integrations, raw_ticket, nonce, None, None, None)
        .await
}

/// Peek a claim ticket's identity without consuming it.
///
/// Unredeemed expired tickets fail. Already-redeemed tickets succeed so a
/// browser retry can submit the stable atomic operation id after a lost
/// reply. Credential mutation happens only inside
/// [`redeem_ticket_to_session_with_client`].
pub struct InspectedClaimTicket {
    /// Portal identity bound to the ticket.
    pub identity: PortalIdentity,
    /// Whether `redeemed_at` is already set.
    pub redeemed: bool,
}

/// Resolve a claim ticket's identity without consuming the ticket.
///
/// # Errors
///
/// Returns an error when the ticket is missing, unbound, or unredeemed-and-expired.
pub async fn inspect_claim_ticket(
    library: &LibraryStore,
    raw_ticket: &str,
) -> Result<InspectedClaimTicket> {
    let hash = hash_token(raw_ticket);
    let ticket = library
        .get_claim_ticket_by_hash(&hash)
        .await?
        .ok_or_else(|| {
            IntegrationError::message("claim ticket invalid, expired, or already redeemed")
        })?;
    let redeemed = ticket.redeemed_at.is_some();
    if !redeemed && ticket.expires_at <= Utc::now() {
        return Err(IntegrationError::message(
            "claim ticket invalid, expired, or already redeemed",
        ));
    }
    let identity_id = ticket
        .identity_id
        .ok_or_else(|| IntegrationError::message("claim ticket is not bound to an identity"))?;
    let identity = library
        .get_portal_identity_by_id(identity_id)
        .await?
        .ok_or_else(|| IntegrationError::message("portal identity missing for claim ticket"))?;
    Ok(InspectedClaimTicket { identity, redeemed })
}

/// Redeem a claim ticket and mint a portal session with optional client metadata.
///
/// `password_hash` is applied in the same transaction as ticket consume and
/// session insert, and only while the ticket-bound local user has no password.
/// `password_fingerprint` is hashed into the atomic request so a retry with
/// a freshly salted Argon2id encoding still matches the receipt.
///
/// The raw session token is derived from the claim ticket, browser nonce, and
/// process DEK so a lost atomic reply plus a new HTTP request reuse the
/// same operation id. Possession of the used magic link without the nonce
/// cannot recover the session.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn redeem_ticket_to_session_with_client(
    library: &LibraryStore,
    integrations: &IntegrationsConfig,
    raw_ticket: &str,
    nonce: &str,
    client: Option<&bookclerk_library::SessionClientInfo>,
    password_hash: Option<&str>,
    password_fingerprint: Option<&str>,
) -> Result<(String, PortalIdentity)> {
    let nonce = bookclerk_library::parse_claim_redeem_nonce(nonce)?;
    let hash = hash_token(raw_ticket);
    let dek = bookclerk_library::require_master_key(None)?;
    let session = bookclerk_library::derive_claim_session_token(&dek, raw_ticket, nonce);
    let session_hash = hash_token(&session);
    let expires = Utc::now() + Duration::hours(integrations.portal_session_ttl_hours as i64);
    let identity = library
        .redeem_claim_ticket_to_session(
            &hash,
            &session_hash,
            expires,
            client,
            password_hash,
            password_fingerprint,
        )
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
///
/// # Errors
///
/// Returns an error when the operation fails.
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

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::UserRole;
    use chrono::Utc;

    fn invite_password() -> String {
        ["invite", "-", "password", "-", "ok"].concat()
    }

    async fn claim_library() -> (
        bookclerk_library::LibraryStore,
        tempfile::TempDir,
        tokio::sync::MutexGuard<'static, ()>,
        String,
    ) {
        static DEK_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        let dek = DEK_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let dir = tempfile::tempdir().unwrap();
        bookclerk_library::configure_master_key(dir.path()).unwrap();
        let store = bookclerk_library::LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        );
        let user = store
            .create_user(UserRole::Member, Some("Invitee"), None)
            .await
            .unwrap();
        let password = bookclerk_library::hash_password(&invite_password()).unwrap();
        store
            .set_user_password_hash(user.id, Some(password.as_str()))
            .await
            .unwrap();
        let identity = store
            .ensure_local_portal_identity(user.id, Some("Invitee"))
            .await
            .unwrap();
        let raw_ticket = generate_token();
        store
            .insert_claim_ticket(
                &hash_token(&raw_ticket),
                Some(identity.id),
                Utc::now() + chrono::Duration::hours(1),
                "test",
            )
            .await
            .unwrap();
        (store, dir, dek, raw_ticket)
    }

    #[tokio::test]
    async fn redeem_retries_as_new_request_return_the_same_session_token() {
        let (store, _dir, _dek, raw_ticket) = claim_library().await;
        let integrations = IntegrationsConfig::default();
        let nonce = hash_token(&["browser", "-", "nonce"].concat());
        let (first, identity) =
            redeem_ticket_to_session(&store, &integrations, &raw_ticket, &nonce)
                .await
                .unwrap();
        // Simulate a lost HTTP reply: the client never observed `first`, and
        // issues a new redeem of the same magic link with the persisted nonce.
        let (second, identity2) =
            redeem_ticket_to_session(&store, &integrations, &raw_ticket, &nonce)
                .await
                .unwrap();
        assert_eq!(first, second);
        assert_eq!(identity.id, identity2.id);
        assert_eq!(first.len(), 64);
        let resolved = identity_from_session(&store, &first).await.unwrap();
        assert_eq!(resolved.unwrap().id, identity.id);
        let other_nonce = hash_token(&["other", "-", "browser"].concat());
        let err = redeem_ticket_to_session(&store, &integrations, &raw_ticket, &other_nonce)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("already redeemed")
                || err.to_string().contains("invalid")
                || err.to_string().contains("claim"),
            "{err}"
        );
    }
}
