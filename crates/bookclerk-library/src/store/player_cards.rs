//! Player library cards and player passwords (`docs/adr/player-api.md`).
//!
//! An audiobook player app signs in to the host's `/player` API and holds
//! a **player card**: a `player` row of `portal_sessions`, so the patron
//! sees it in their session list and can revoke it there, plus its tokens
//! in `player_card_tokens`. A card has short access tokens (one hour) and
//! a rotating refresh token (thirty days), the pair the apps expect. Only
//! SHA-256 digests of tokens are stored; the session row's own token hash
//! is a digest of random bytes nobody is given, so the card never works as
//! a browser cookie or a clerk's library card.
//!
//! A **player password** is what a patron makes in Account settings for an
//! app that has only a username and password box. The host generates it
//! with enough entropy that a SHA-256 is a safe store, and it is looked up
//! by that digest; the account password is never accepted by the API.
//!
//! Every write is one atomic batch through the database plugin.

use bookclerk_plugin_abi::TypedDbStatement;
use chrono::{DateTime, Duration, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

use super::{now_str, parse_dt, parse_dt_opt, LibraryStore};
use crate::control_plane::{batch, read_paged};
use crate::entities::{player_card_tokens, player_passwords, portal_sessions};
use crate::error::{LibraryError, Result};
use crate::models::{PortalIdentity, PortalSessionKind};

/// Lifetime of a player card's access token.
pub const PLAYER_ACCESS_TTL_SECS: i64 = 60 * 60;

/// Lifetime of a player card's refresh token; each refresh starts it again.
pub const PLAYER_REFRESH_TTL_DAYS: i64 = 30;

/// How long the other refresh tokens of a card keep working after one of
/// them is used: long enough for two refreshes that raced with the same
/// token to both succeed, and for a phone whose refresh answer was lost
/// while it slept to try again (the apps expect ten minutes).
pub const PLAYER_REFRESH_GRACE_SECS: i64 = 10 * 60;

/// Most player passwords one patron keeps.
pub const MAX_PLAYER_PASSWORDS: u64 = 20;

/// Longest player password label, in bytes.
pub const MAX_PLAYER_PASSWORD_LABEL_BYTES: usize = 64;

/// The kind value of an access token row.
const ACCESS: &str = "access";

/// The kind value of a refresh token row.
const REFRESH: &str = "refresh";

/// A player password as its owner sees it in Account settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerPassword {
    /// Password id.
    pub id: String,
    /// The patron it signs in as.
    pub user_id: i64,
    /// The name the patron gave it.
    pub label: String,
    /// When it was made.
    pub created_at: DateTime<Utc>,
    /// When it last signed an app in.
    pub last_used_at: Option<DateTime<Utc>>,
}

/// What signing a card in or refreshing it hands out, as digests: the
/// caller holds the tokens themselves.
#[derive(Debug, Clone, Copy)]
pub struct PlayerCardTokens<'a> {
    /// SHA-256 hex of the new access token.
    pub access_hash: &'a str,
    /// SHA-256 hex of the new refresh token.
    pub refresh_hash: &'a str,
}

/// A new player card ([`LibraryStore::mint_player_card`]).
#[derive(Debug, Clone, Copy)]
pub struct NewPlayerCard<'a> {
    /// The portal identity the card signs in as.
    pub identity_id: i64,
    /// SHA-256 hex of random bytes nobody is given: the session row's own
    /// token hash, unique per card.
    pub session_hash: &'a str,
    /// The player password it was made with, when it was.
    pub password_id: Option<&'a str>,
    /// The app's name, shown in the session list.
    pub label: &'a str,
    /// The app's `User-Agent`, when it sent one.
    pub user_agent: Option<&'a str>,
    /// Best-effort device class (`mobile`, `tablet`, `desktop`, `api`).
    pub device_type: &'a str,
    /// The card's first tokens.
    pub tokens: PlayerCardTokens<'a>,
}

/// A live player card, as an access token resolves to it.
#[derive(Debug, Clone)]
pub struct PlayerCard {
    /// The card's `portal_sessions.id`.
    pub session_id: i64,
    /// The identity it signs in as.
    pub identity: PortalIdentity,
}

/// Maps a stored player password.
fn map_password(m: player_passwords::Model) -> PlayerPassword {
    PlayerPassword {
        id: m.id,
        user_id: m.user_id,
        label: m.label,
        created_at: parse_dt(&m.created_at),
        last_used_at: parse_dt_opt(m.last_used_at.as_deref()),
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
fn truncated(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The statements that end every card in `cards`, a subquery of
/// `portal_sessions.id` values with `binds` for its placeholders: their
/// tokens, their card rows and their session rows.
fn end_cards(cards: &str, binds: &[bookclerk_plugin_abi::DbValue]) -> [TypedDbStatement; 3] {
    [
        batch::exec(
            &format!("DELETE FROM player_card_tokens WHERE session_id IN ({cards})"),
            binds.to_vec(),
        ),
        batch::exec(
            &format!("DELETE FROM portal_sessions WHERE kind = 'player' AND id IN ({cards})"),
            binds.to_vec(),
        ),
        batch::exec(
            &format!("DELETE FROM player_cards WHERE session_id IN ({cards})"),
            binds.to_vec(),
        ),
    ]
}

impl LibraryStore {
    /// Stores a new player password for `user_id`.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The patron.
    /// * `label` - Its name; trimmed, and at most
    ///   [`MAX_PLAYER_PASSWORD_LABEL_BYTES`] bytes are kept.
    /// * `password_hash` - SHA-256 hex of the generated password.
    ///
    /// # Returns
    ///
    /// The password, or `None` when the patron already keeps
    /// [`MAX_PLAYER_PASSWORDS`].
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn insert_player_password(
        &self,
        user_id: i64,
        label: &str,
        password_hash: &str,
    ) -> Result<Option<PlayerPassword>> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_str();
        let cap = i64::try_from(MAX_PLAYER_PASSWORDS).unwrap_or(i64::MAX);
        let statements = vec![batch::exec(
            "INSERT INTO player_passwords (id, user_id, label, password_hash, created_at) \
             SELECT ?, ?, ?, ?, ? WHERE \
             (SELECT COUNT(*) FROM player_passwords WHERE user_id = ?) < ?",
            vec![
                batch::text(&id),
                batch::int(user_id),
                batch::text(truncated(label.trim(), MAX_PLAYER_PASSWORD_LABEL_BYTES)),
                batch::text(password_hash),
                batch::text(&now),
                batch::int(user_id),
                batch::int(cap),
            ],
        )];
        let reply = self
            .execute_host_batch(batch::request(
                &format!("player-password-add-{id}"),
                statements,
            ))
            .await?;
        if batch::rows_affected(&reply, 0) == 0 {
            return Ok(None);
        }
        Ok(player_passwords::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(map_password))
    }

    /// A patron's player passwords, newest first.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    pub async fn list_player_passwords(&self, user_id: i64) -> Result<Vec<PlayerPassword>> {
        let select = player_passwords::Entity::find()
            .filter(player_passwords::Column::UserId.eq(user_id))
            .order_by_desc(player_passwords::Column::CreatedAt);
        // The primary key, id, is the tie-break `read_paged` appends.
        Ok(read_paged(self, select, Some(MAX_PLAYER_PASSWORDS))
            .await?
            .into_iter()
            .map(map_password)
            .collect())
    }

    /// Removes one of a patron's player passwords and ends every card it
    /// signed in.
    ///
    /// # Returns
    ///
    /// True when a password of theirs was removed.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn delete_player_password(&self, user_id: i64, id: &str) -> Result<bool> {
        let cards = "SELECT session_id FROM player_cards WHERE password_id = ? AND password_id IN \
                     (SELECT id FROM player_passwords WHERE user_id = ?)";
        let binds = [batch::text(id), batch::int(user_id)];
        let mut statements = vec![batch::exec(
            "DELETE FROM player_passwords WHERE id = ? AND user_id = ?",
            vec![batch::text(id), batch::int(user_id)],
        )];
        // The cards go first: their subquery reads the password row.
        let ended = end_cards(cards, &binds);
        let mut ordered: Vec<TypedDbStatement> = ended.into_iter().collect();
        ordered.append(&mut statements);
        let reply = self
            .execute_host_batch(batch::request(
                &format!("player-password-delete-{}", uuid::Uuid::new_v4()),
                ordered,
            ))
            .await?;
        // Statement 3 is the password's own delete.
        Ok(batch::rows_affected(&reply, 3) > 0)
    }

    /// The player password a sign-in presents, by digest, and its
    /// patron; marks it used.
    ///
    /// # Returns
    ///
    /// The password, or `None` for no such password.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn use_player_password(&self, password_hash: &str) -> Result<Option<PlayerPassword>> {
        let Some(found) = player_passwords::Entity::find()
            .filter(player_passwords::Column::PasswordHash.eq(password_hash))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let now = now_str();
        self.execute_host_batch(batch::request(
            &format!("player-password-use-{}", uuid::Uuid::new_v4()),
            vec![batch::exec(
                "UPDATE player_passwords SET last_used_at = ? WHERE id = ?",
                vec![batch::text(&now), batch::text(&found.id)],
            )],
        ))
        .await?;
        let mut password = map_password(found);
        password.last_used_at = Some(parse_dt(&now));
        Ok(Some(password))
    }

    /// Makes a player card: its session row, its card row and its first
    /// access and refresh tokens, in one batch.
    ///
    /// # Returns
    ///
    /// The card's session id, or `None` when the player password it names
    /// was removed meanwhile.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn mint_player_card(&self, card: NewPlayerCard<'_>) -> Result<Option<i64>> {
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let refresh_until = (now + Duration::days(PLAYER_REFRESH_TTL_DAYS)).to_rfc3339();
        let access_until = (now + Duration::seconds(PLAYER_ACCESS_TTL_SECS)).to_rfc3339();
        // A card made with a password exists only while the password does.
        let (guard, guard_binds) = match card.password_id {
            Some(id) => (
                " WHERE EXISTS (SELECT 1 FROM player_passwords WHERE id = ?)",
                vec![batch::text(id)],
            ),
            None => ("", Vec::new()),
        };
        let mut insert_session = vec![
            batch::text(card.session_hash),
            batch::int(card.identity_id),
            batch::text(&refresh_until),
            batch::text(&now_text),
            batch::text(&now_text),
            batch::text_opt(card.user_agent),
            batch::text(card.device_type),
            batch::text(truncated(
                card.label.trim(),
                MAX_PLAYER_PASSWORD_LABEL_BYTES,
            )),
        ];
        insert_session.extend(guard_binds);
        let token = |hash: &str, kind: &str, until: &str| {
            batch::exec(
                "INSERT INTO player_card_tokens (token_hash, session_id, kind, expires_at) \
                 SELECT ?, id, ?, ? FROM portal_sessions WHERE token_hash = ?",
                vec![
                    batch::text(hash),
                    batch::text(kind),
                    batch::text(until),
                    batch::text(card.session_hash),
                ],
            )
        };
        let statements = vec![
            batch::exec(
                &format!(
                    "INSERT INTO portal_sessions \
                     (token_hash, identity_id, expires_at, created_at, last_used_at, \
                      user_agent, device_type, client_label, kind) \
                     SELECT ?, ?, ?, ?, ?, ?, ?, ?, 'player'{guard}"
                ),
                insert_session,
            ),
            batch::exec(
                "INSERT INTO player_cards (session_id, password_id, created_at) \
                 SELECT id, ?, ? FROM portal_sessions WHERE token_hash = ?",
                vec![
                    batch::text_opt(card.password_id),
                    batch::text(&now_text),
                    batch::text(card.session_hash),
                ],
            ),
            token(card.tokens.access_hash, ACCESS, &access_until),
            token(card.tokens.refresh_hash, REFRESH, &refresh_until),
        ];
        let reply = self
            .execute_host_batch(batch::request(
                &format!("player-card-mint-{}", uuid::Uuid::new_v4()),
                statements,
            ))
            .await?;
        if batch::rows_affected(&reply, 0) == 0 {
            return Ok(None);
        }
        Ok(portal_sessions::Entity::find()
            .filter(portal_sessions::Column::TokenHash.eq(card.session_hash))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|row| row.id))
    }

    /// The live card a token of `kind` belongs to: the token has not
    /// expired, and its session row is a `player` row that has not.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    async fn player_card_session(
        &self,
        token_hash: &str,
        kind: &str,
    ) -> Result<Option<portal_sessions::Model>> {
        let now = now_str();
        let Some(token) = player_card_tokens::Entity::find_by_id(token_hash.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .filter(|token| token.kind == kind && token.expires_at > now)
        else {
            return Ok(None);
        };
        Ok(portal_sessions::Entity::find_by_id(token.session_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .filter(|session| {
                session.kind == PortalSessionKind::Player.as_str() && session.expires_at > now
            }))
    }

    /// Resolves a player card's access token to its card and identity,
    /// touching the session at most once a minute as a browser session is.
    ///
    /// # Arguments
    ///
    /// * `access_hash` - SHA-256 hex of the access token.
    ///
    /// # Returns
    ///
    /// `None` for an unknown or expired token or an ended card.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn player_card_by_access(&self, access_hash: &str) -> Result<Option<PlayerCard>> {
        let Some(session) = self.player_card_session(access_hash, ACCESS).await? else {
            return Ok(None);
        };
        let session_id = session.id;
        let identity = self.touch_session_identity(session, None).await?;
        Ok(identity.map(|identity| PlayerCard {
            session_id,
            identity,
        }))
    }

    /// Trades a refresh token for a new access and refresh token, in one
    /// batch. The presented token and every other refresh token of the
    /// card keep working for [`PLAYER_REFRESH_GRACE_SECS`] more, so two
    /// refreshes racing with one token both succeed; the card's thirty
    /// days start again.
    ///
    /// # Arguments
    ///
    /// * `refresh_hash` - SHA-256 hex of the presented refresh token.
    /// * `tokens` - Digests of the new pair.
    ///
    /// # Returns
    ///
    /// The card, or `None` when the token does not refresh: unknown,
    /// expired, or its card ended.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn refresh_player_card(
        &self,
        refresh_hash: &str,
        tokens: PlayerCardTokens<'_>,
    ) -> Result<Option<PlayerCard>> {
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        let refresh_until = (now + Duration::days(PLAYER_REFRESH_TTL_DAYS)).to_rfc3339();
        let access_until = (now + Duration::seconds(PLAYER_ACCESS_TTL_SECS)).to_rfc3339();
        let grace_until = (now + Duration::seconds(PLAYER_REFRESH_GRACE_SECS)).to_rfc3339();
        let new_card = "(SELECT session_id FROM player_card_tokens WHERE token_hash = ?)";
        let insert = |hash: &str, kind: &str, until: &str| {
            batch::exec(
                "INSERT INTO player_card_tokens (token_hash, session_id, kind, expires_at) \
                 SELECT ?, t.session_id, ?, ? FROM player_card_tokens t \
                 INNER JOIN portal_sessions s ON s.id = t.session_id \
                 WHERE t.token_hash = ? AND t.kind = 'refresh' AND t.expires_at > ? \
                 AND s.kind = 'player' AND s.expires_at > ?",
                vec![
                    batch::text(hash),
                    batch::text(kind),
                    batch::text(until),
                    batch::text(refresh_hash),
                    batch::text(&now_text),
                    batch::text(&now_text),
                ],
            )
        };
        let statements = vec![
            insert(tokens.refresh_hash, REFRESH, &refresh_until),
            insert(tokens.access_hash, ACCESS, &access_until),
            // Every other refresh token of the card has the grace period left.
            batch::exec(
                &format!(
                    "UPDATE player_card_tokens SET expires_at = ? \
                     WHERE kind = 'refresh' AND token_hash <> ? AND expires_at > ? \
                     AND session_id = {new_card}"
                ),
                vec![
                    batch::text(&grace_until),
                    batch::text(tokens.refresh_hash),
                    batch::text(&grace_until),
                    batch::text(tokens.refresh_hash),
                ],
            ),
            batch::exec(
                &format!(
                    "DELETE FROM player_card_tokens WHERE expires_at <= ? \
                     AND session_id = {new_card}"
                ),
                vec![batch::text(&now_text), batch::text(tokens.refresh_hash)],
            ),
            batch::exec(
                &format!(
                    "UPDATE portal_sessions SET expires_at = ?, last_used_at = ? \
                     WHERE kind = 'player' AND id = {new_card}"
                ),
                vec![
                    batch::text(&refresh_until),
                    batch::text(&now_text),
                    batch::text(tokens.refresh_hash),
                ],
            ),
        ];
        let reply = self
            .execute_host_batch(batch::request(
                &format!("player-card-refresh-{}", uuid::Uuid::new_v4()),
                statements,
            ))
            .await?;
        if batch::rows_affected(&reply, 0) == 0 {
            return Ok(None);
        }
        self.player_card_by_access(tokens.access_hash).await
    }

    /// Ends the card a token (access or refresh) belongs to: the app
    /// signed out.
    ///
    /// # Returns
    ///
    /// True when a live card was ended.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn end_player_card(&self, token_hash: &str) -> Result<bool> {
        let Some(token) = player_card_tokens::Entity::find_by_id(token_hash.to_string())
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(false);
        };
        let reply = self
            .execute_host_batch(batch::request(
                &format!("player-card-end-{}", uuid::Uuid::new_v4()),
                end_cards("?", &[batch::int(token.session_id)])
                    .into_iter()
                    .collect(),
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 1) > 0)
    }
}

#[cfg(test)]
#[path = "player_cards_tests.rs"]
mod tests;
