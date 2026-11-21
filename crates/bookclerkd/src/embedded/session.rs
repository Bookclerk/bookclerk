//! Who the desktop window is: the patron whose session this branch keeps
//! sealed for its window, and the session cookies the window holds for them
//! where the page cannot see them.
//!
//! The window acts as the operator only until the library has an active
//! Owner (first run, or a computer that is only a clerk of another
//! library). After that it acts as a patron: the one whose portal session
//! is sealed in this branch's `encrypted_secrets` ([`remembered`]), or
//! nobody until they sign in. The files directory, and so the database, is
//! already this OS account's own, so the sealed row is per OS account
//! without a file of its own. [`WindowJar`] keeps that patron's portal
//! session cookie, and the operator session cookie an elevation sets, so
//! the routes the web app uses for elevation, impersonation and sign-out
//! work unchanged; `Set-Cookie` never reaches the page.
//!
//! See `docs/adr/desktop-window-identity.md`.

use axum::http::HeaderValue;
use bookclerk_library::{
    build_sealed_record, delete_secret, get_secret, hash_token, secret_account_type, secret_kind,
    unseal_secret, upsert_secret, LibraryError, LibraryStore, SessionClientInfo, UserStatus,
};
use chrono::{Duration as ChronoDuration, Utc};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

use crate::auth::{PORTAL_SESSION_COOKIE, SESSION_COOKIE};

/// How long a window session lasts before the window must be signed in
/// again. The window renews it on every launch, so only a computer left
/// unopened this long signs out.
pub const DESKTOP_SESSION_TTL: ChronoDuration = ChronoDuration::days(90);

/// `encrypted_secrets.name` of the window's sealed patron session.
const WINDOW_SESSION_NAME: &str = "patron_session";

/// A patron's raw portal session token, as the window keeps it: never
/// printed by `Debug`, and wiped from memory when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct WindowSession(Zeroizing<String>);

impl WindowSession {
    /// Wraps a raw session token, and registers it with log redaction.
    ///
    /// # Arguments
    ///
    /// * `raw` - The token, as the portal session cookie carries it.
    ///
    /// # Returns
    ///
    /// The session, or `None` for an empty token.
    #[must_use]
    pub fn new(raw: &str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }
        bookclerk_config::register_secret(raw);
        Some(Self(Zeroizing::new(raw.to_owned())))
    }

    /// The raw token, for the cookie and the session lookup only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for WindowSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WindowSession(<redacted>)")
    }
}

/// The session cookies the window holds: never sent to the page, added to
/// each request instead of anything the page sent.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct WindowJar {
    /// The patron's portal session token (`bookclerk_portal_session`).
    pub portal: Option<String>,
    /// An operator session token (`bookclerk_operator_session`), present
    /// while the patron is elevated.
    pub operator: Option<String>,
    /// When the operator cookie's `Max-Age` runs out; `None` for a cookie
    /// with no `Max-Age`, which lasts until the window closes.
    pub operator_until: Option<Instant>,
}

impl std::fmt::Debug for WindowJar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowJar")
            .field("portal", &self.portal.is_some())
            .field("operator", &self.operator.is_some())
            .finish()
    }
}

impl WindowJar {
    /// The `Cookie` header for the jar's sessions.
    ///
    /// # Returns
    ///
    /// `name=value` pairs joined by `; `, or `None` when the jar is empty.
    #[must_use]
    pub fn cookie_header(&self) -> Option<String> {
        let pairs: Vec<String> = [
            (PORTAL_SESSION_COOKIE, &self.portal),
            (SESSION_COOKIE, &self.operator),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.as_ref().map(|value| format!("{name}={value}")))
        .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    /// Drops the operator cookie once its `Max-Age` has run out, as a
    /// browser would, so an elevation that ended on its own stops counting.
    ///
    /// # Arguments
    ///
    /// * `now` - The current time.
    pub fn drop_expired(&mut self, now: Instant) {
        if self.operator_until.is_some_and(|until| until <= now) {
            self.operator = None;
            self.operator_until = None;
        }
    }

    /// Applies the router's `Set-Cookie` headers to the jar.
    ///
    /// Only the two session cookies are kept; an empty value or
    /// `Max-Age=0` removes one. Other cookies (the sign-in provider's
    /// transaction cookie) are ignored: the window cannot follow a provider
    /// sign-in anyway.
    ///
    /// # Arguments
    ///
    /// * `set_cookies` - The response's `Set-Cookie` values, in order.
    pub fn absorb(&mut self, set_cookies: &[HeaderValue]) {
        for raw in set_cookies {
            let Ok(text) = raw.to_str() else {
                continue;
            };
            let mut attributes = text.split(';').map(str::trim);
            let Some((name, value)) = attributes.next().and_then(|pair| pair.split_once('='))
            else {
                continue;
            };
            let max_age = attributes.find_map(|attribute| {
                attribute
                    .split_once('=')
                    .filter(|(key, _)| key.eq_ignore_ascii_case("max-age"))
                    .map(|(_, age)| age.parse::<u64>().unwrap_or(0))
            });
            let kept = (max_age != Some(0) && !value.is_empty()).then(|| value.to_owned());
            match name {
                PORTAL_SESSION_COOKIE => self.portal = kept,
                SESSION_COOKIE => {
                    self.operator_until = kept
                        .as_ref()
                        .and(max_age)
                        .and_then(|age| Instant::now().checked_add(Duration::from_secs(age)));
                    self.operator = kept;
                }
                _ => {}
            }
        }
    }
}

/// This computer's name, for the session list.
fn computer_name() -> String {
    crate::library_link::join::default_branch_name()
}

/// Mints a window session for `user_id`: a portal session labelled
/// **Desktop app on `<computer>`** in the patron's session list, lasting
/// [`DESKTOP_SESSION_TTL`].
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `user_id` - The patron.
///
/// # Returns
///
/// The new session's raw token, or `None` when the patron does not exist,
/// is disabled, or the session cannot be stored.
pub async fn mint(library: &LibraryStore, user_id: i64) -> Option<WindowSession> {
    let user = library.get_user(user_id).await.ok()??;
    if matches!(user.status, UserStatus::Disabled) {
        return None;
    }
    let identity = library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
        .ok()?;
    let raw = uuid::Uuid::new_v4().to_string();
    let session = WindowSession::new(&raw)?;
    let client = SessionClientInfo {
        user_agent: None,
        device_type: String::from("desktop"),
        client_label: format!("Desktop app on {}", computer_name()),
    };
    library
        .insert_portal_session_with_client(
            &hash_token(&raw),
            identity.id,
            Utc::now() + DESKTOP_SESSION_TTL,
            Some(&client),
        )
        .await
        .ok()?;
    let _ = library
        .insert_security_audit_event(&format!("user:{}", user.id), "desktop_session", None)
        .await;
    Some(session)
}

/// The patron a window session signs in as, when it still can: the
/// session exists and has not expired, and its patron exists and is not
/// disabled.
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `session` - The window's session.
///
/// # Returns
///
/// The patron's user id, or `None` when the session has ended.
///
/// # Errors
///
/// Returns the read error, so a database that is briefly unreachable is
/// not taken for an ended session.
pub async fn patron(
    library: &LibraryStore,
    session: &WindowSession,
) -> Result<Option<i64>, LibraryError> {
    let Some(identity) = library
        .get_portal_session_identity(&hash_token(session.expose()))
        .await?
    else {
        return Ok(None);
    };
    let Some(user_id) = identity.user_id else {
        return Ok(None);
    };
    let Some(user) = library.get_user(user_id).await? else {
        return Ok(None);
    };
    Ok((!matches!(user.status, UserStatus::Disabled)).then_some(user.id))
}

/// Ends a session the window no longer uses (best effort).
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `session` - The session to end.
pub async fn end(library: &LibraryStore, session: &WindowSession) {
    let _ = library
        .delete_portal_session(&hash_token(session.expose()))
        .await;
}

/// The patron session this branch's window last signed in with.
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `node_id` - This branch's node id, which keys the sealed row.
///
/// # Returns
///
/// The session, or `None` when none is kept.
///
/// # Errors
///
/// Returns the read error, or an error when the row does not unseal.
pub async fn remembered(
    library: &LibraryStore,
    node_id: &str,
) -> Result<Option<WindowSession>, LibraryError> {
    let Some(record) = get_secret(
        library.db(),
        secret_kind::WINDOW_SESSION,
        Some(node_id),
        secret_account_type::OPERATOR,
        Some(node_id),
        WINDOW_SESSION_NAME,
    )
    .await?
    else {
        return Ok(None);
    };
    let plaintext = Zeroizing::new(unseal_secret(&record)?);
    let raw = std::str::from_utf8(&plaintext)
        .map_err(|_| LibraryError::Invalid("stored window session is not text".into()))?;
    Ok(WindowSession::new(raw))
}

/// Seals `session` as the one this branch's window signs in with,
/// replacing any other.
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `node_id` - This branch's node id, which keys the sealed row.
/// * `session` - The patron's session.
///
/// # Errors
///
/// Returns an error when the session cannot be sealed or stored.
pub async fn remember(
    library: &LibraryStore,
    node_id: &str,
    session: &WindowSession,
) -> Result<(), LibraryError> {
    let record = build_sealed_record(
        session.expose().as_bytes(),
        secret_kind::WINDOW_SESSION,
        node_id,
        secret_account_type::OPERATOR,
        node_id,
        WINDOW_SESSION_NAME,
    )?;
    upsert_secret(library.db(), &record).await
}

/// Forgets the session this branch's window signs in with.
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `node_id` - This branch's node id, which keys the sealed row.
///
/// # Errors
///
/// Returns the delete error.
pub async fn forget(library: &LibraryStore, node_id: &str) -> Result<(), LibraryError> {
    delete_secret(
        library.db(),
        secret_kind::WINDOW_SESSION,
        Some(node_id),
        secret_account_type::OPERATOR,
        Some(node_id),
        WINDOW_SESSION_NAME,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> HeaderValue {
        HeaderValue::from_str(text).unwrap()
    }

    #[test]
    fn the_jar_keeps_the_two_session_cookies_and_drops_cleared_ones() {
        let mut jar = WindowJar::default();
        assert_eq!(jar.cookie_header(), None);
        jar.absorb(&[
            value("bookclerk_portal_session=p1; Path=/; HttpOnly; Max-Age=3600"),
            value("bookclerk_oidc_tx=t; Path=/"),
            value("garbage"),
        ]);
        assert_eq!(
            jar.cookie_header().as_deref(),
            Some("bookclerk_portal_session=p1")
        );
        jar.absorb(&[value("bookclerk_operator_session=o1; Path=/; max-age=900")]);
        assert_eq!(
            jar.cookie_header().as_deref(),
            Some("bookclerk_portal_session=p1; bookclerk_operator_session=o1")
        );
        jar.absorb(&[value("bookclerk_operator_session=; Path=/; Max-Age=0")]);
        assert_eq!(jar.operator, None);
        jar.absorb(&[value("bookclerk_operator_session=o2; Path=/; Max-Age=900")]);
        jar.drop_expired(Instant::now());
        assert_eq!(jar.operator.as_deref(), Some("o2"), "still elevated");
        jar.drop_expired(Instant::now() + Duration::from_secs(901));
        assert_eq!((jar.operator.clone(), jar.operator_until), (None, None));
        jar.absorb(&[value("bookclerk_portal_session=p2; Max-Age=0")]);
        assert_eq!(jar, WindowJar::default());
        let shown = format!(
            "{:?}",
            WindowJar {
                portal: Some("secret".into()),
                operator: None,
                operator_until: None,
            }
        );
        assert!(!shown.contains("secret"), "{shown}");
        let session = WindowSession::new("secret").unwrap();
        assert_eq!(session.expose(), "secret");
        assert!(!format!("{session:?}").contains("secret"));
        assert_eq!(WindowSession::new(""), None);
    }
}
