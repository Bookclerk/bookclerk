//! Who the desktop window is: the patron this OS user's profile names, and
//! the session cookies the window holds for them where the page cannot see
//! them.
//!
//! The window acts as the operator only until the library has an active
//! Owner (first run, or a computer that is only a clerk of another
//! library). After that it acts as a patron: the one named by the session in
//! this OS user's `profile.toml` (see [`bookclerk_config::DesktopProfile`]),
//! or nobody until they sign in. [`WindowJar`] keeps that patron's portal
//! session cookie, and the operator session cookie an elevation sets, so
//! the routes the web app uses for elevation, impersonation and sign-out
//! work unchanged; `Set-Cookie` never reaches the page.
//!
//! See `docs/adr/desktop-window-identity.md`.

use axum::http::HeaderValue;
use bookclerk_config::ProfileSession;
use bookclerk_library::{hash_token, LibraryStore, SessionClientInfo, UserStatus};
use chrono::{Duration as ChronoDuration, Utc};

use crate::auth::{PORTAL_SESSION_COOKIE, SESSION_COOKIE};

/// How long a window session lasts before the window must be signed in
/// again. The window renews it on every launch, so only a computer left
/// unopened this long signs out.
pub const DESKTOP_SESSION_TTL: ChronoDuration = ChronoDuration::days(90);

/// The session cookies the window holds: never sent to the page, added to
/// each request instead of anything the page sent.
#[derive(Default, Clone, PartialEq, Eq)]
pub struct WindowJar {
    /// The patron's portal session token (`bookclerk_portal_session`).
    pub portal: Option<String>,
    /// An operator session token (`bookclerk_operator_session`), present
    /// while the patron is elevated.
    pub operator: Option<String>,
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
            let expired = attributes.any(|attribute| {
                attribute
                    .split_once('=')
                    .is_some_and(|(key, age)| key.eq_ignore_ascii_case("max-age") && age == "0")
            });
            let kept = (!expired && !value.is_empty()).then(|| value.to_owned());
            match name {
                PORTAL_SESSION_COOKIE => self.portal = kept,
                SESSION_COOKIE => self.operator = kept,
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
/// **Desktop app on <computer>** in the patron's session list, lasting
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
pub async fn mint(library: &LibraryStore, user_id: i64) -> Option<ProfileSession> {
    let user = library.get_user(user_id).await.ok()??;
    if matches!(user.status, UserStatus::Disabled) {
        return None;
    }
    let identity = library
        .ensure_local_portal_identity(user.id, user.display_name.as_deref())
        .await
        .ok()?;
    let raw = uuid::Uuid::new_v4().to_string();
    let session = ProfileSession::parse(&raw)?;
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

/// The patron a profile's session signs in as, when it still can: the
/// session exists and has not expired, and its patron exists and is not
/// disabled.
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `session` - The profile's session.
///
/// # Returns
///
/// The patron's user id, or `None`.
pub async fn patron(library: &LibraryStore, session: &ProfileSession) -> Option<i64> {
    let identity = library
        .get_portal_session_identity(&hash_token(session.expose()))
        .await
        .ok()??;
    let user = library.get_user(identity.user_id?).await.ok()??;
    (!matches!(user.status, UserStatus::Disabled)).then_some(user.id)
}

/// Ends a session the window no longer uses (best effort).
///
/// # Arguments
///
/// * `library` - The library on this computer.
/// * `session` - The session to end.
pub async fn end(library: &LibraryStore, session: &ProfileSession) {
    let _ = library
        .delete_portal_session(&hash_token(session.expose()))
        .await;
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
        jar.absorb(&[value("bookclerk_portal_session=p2; Max-Age=0")]);
        assert_eq!(jar, WindowJar::default());
        let shown = format!(
            "{:?}",
            WindowJar {
                portal: Some("secret".into()),
                operator: None
            }
        );
        assert!(!shown.contains("secret"), "{shown}");
    }
}
