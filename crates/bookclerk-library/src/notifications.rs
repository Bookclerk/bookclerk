//! Notifications: the router's rules and the shapes the library stores
//! (`docs/adr/notifications.md`, `docs/server/notifications.md`).
//!
//! The host decides what is worth a notification and who gets it; a
//! notifier plugin only sends one message to one destination. This module
//! is pure. The library database holds the destinations, the
//! administrators' routes and offers, each patron's choices, every
//! accepted notification, the delivery log and the inbox
//! (`LibraryStore::route_notification` and its neighbours), and
//! `bookclerkd` runs one `notify_send` job per delivery-log row.
//!
//! Routing, for a notification of type `T`:
//!
//! - **Operational** (`T` is for administrators): every channel destination
//!   with a route for `T` whose minimum severity the notification reaches.
//! - **Patron** (`T` is for one patron): the patron's inbox, always. Then,
//!   when the patron chose for `T`, the personal destinations they picked
//!   (when on); when they did not, every personal destination of theirs on
//!   an instance the administrators offer for `T` with `default_on`. Either
//!   way only instances the administrators offer for `T` are used.
//! - **Account** is the host's own mail. It is never routed: each message
//!   goes by itself, through the instance administrators chose for it
//!   (`store::account_mail`).
//!
//! A paused destination is skipped.

use bookclerk_plugin_abi::notifications::{check_link_path, check_message, NotificationError};
use bookclerk_plugin_abi::{
    NotificationAlertAction, NotificationAudience, NotificationField, NotificationMessage,
    NotificationSeverity, NotifyDestinationKind,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Permanent send failures in a row after which a destination is paused.
/// A delivery clears the count; an administrator or the patron resumes a
/// paused destination by hand.
pub const PAUSE_AFTER_FAILURES: i64 = 5;

/// Longest stored destination address, in bytes of its JSON text. A
/// database whose cell limit is smaller lowers it further. 1 KiB keeps the
/// insert inside the smallest statement payload any adapter advertises.
pub const MAX_ADDRESS_BYTES: usize = 1024;

/// Send attempts one delivery may make, retries included. A delivery whose
/// notifier still asks to retry after this many fails.
pub const MAX_SEND_ATTEMPTS: i64 = 10;

/// Days a notification, its delivery log and its inbox rows are kept.
pub const NOTIFICATION_RETENTION_DAYS: i64 = 30;

/// Longest destination label, in bytes.
pub const MAX_DESTINATION_LABEL_BYTES: usize = 128;

/// Most routes, offers or patron targets one notification type may have.
pub const MAX_ROUTES_PER_TYPE: usize = 32;

/// The reserved destination id of the built-in "In app" target: an
/// operational route to it puts the notification in the inbox of every
/// active Owner and Administrator, where the web app's bell, browser
/// notifications and the desktop window's system notifications show it.
/// It is not a stored destination and runs no plugin.
pub const IN_APP_DESTINATION: &str = "in-app";

/// The host's built-in `test` type: an administrator or a patron sends it to
/// one destination to check that it works.
pub const TEST_NOTIFICATION_TYPE: &str = "test";

/// Whether a destination is a library channel or one patron's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DestinationKind {
    /// A library channel; the notifier instance's own settings say where.
    Channel,
    /// One patron's personal address.
    Personal,
}

impl DestinationKind {
    /// Every kind, in the order of the `kind` column's CHECK list.
    pub const ALL: [Self; 2] = [Self::Channel, Self::Personal];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Personal => "personal",
        }
    }

    /// Parses the stored spelling; `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "channel" => Some(Self::Channel),
            "personal" => Some(Self::Personal),
            _ => None,
        }
    }

    /// The kind the notifier sees.
    #[must_use]
    pub fn wire(self) -> NotifyDestinationKind {
        match self {
            Self::Channel => NotifyDestinationKind::Channel,
            Self::Personal => NotifyDestinationKind::Personal,
        }
    }
}

/// Where one delivery stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Waiting for its send job, or between retries.
    Pending,
    /// The notifier delivered it.
    Delivered,
    /// The notifier refused it for good, or it ran out of attempts.
    Failed,
    /// Not sent: its destination was paused or removed, or its notifier
    /// instance is gone.
    Dropped,
}

impl DeliveryState {
    /// Every state, in the order of the `state` column's CHECK list.
    pub const ALL: [Self; 4] = [Self::Pending, Self::Delivered, Self::Failed, Self::Dropped];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::Dropped => "dropped",
        }
    }

    /// Parses the stored spelling; `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "failed" => Some(Self::Failed),
            "dropped" => Some(Self::Dropped),
            _ => None,
        }
    }
}

/// How long a delivered or failed account-mail record is kept.
pub const ACCOUNT_MAIL_RETENTION_DAYS: i64 = 30;

/// How long an email verification link works.
pub const EMAIL_VERIFICATION_HOURS: i64 = 24;

/// How long an emailed invite works, as for every claim ticket.
pub const INVITE_DAYS: i64 = 7;

/// How long a password-reset link works.
pub const PASSWORD_RESET_MINUTES: i64 = 30;

/// What an account-mail message says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountMailKind {
    /// A test message from the account-mail settings; delivering it marks
    /// the instance tested.
    Test,
    /// An invite: a link that sets up the account (a claim ticket).
    Invite,
    /// A link that proves the user's email address.
    VerifyEmail,
    /// A link that sets a new password, sent only to a verified address.
    PasswordReset,
    /// A notice, with no link, that the password was changed through a
    /// reset link.
    PasswordChanged,
}

impl AccountMailKind {
    /// Every kind, in the order of the `kind` column's CHECK list.
    pub const ALL: [Self; 5] = [
        Self::Test,
        Self::Invite,
        Self::VerifyEmail,
        Self::PasswordReset,
        Self::PasswordChanged,
    ];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Invite => "invite",
            Self::VerifyEmail => "verify_email",
            Self::PasswordReset => "password_reset",
            Self::PasswordChanged => "password_changed",
        }
    }

    /// Parses the stored spelling; `None` when unknown.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "test" => Some(Self::Test),
            "invite" => Some(Self::Invite),
            "verify_email" => Some(Self::VerifyEmail),
            "password_reset" => Some(Self::PasswordReset),
            "password_changed" => Some(Self::PasswordChanged),
            _ => None,
        }
    }
}

/// The notifier instance administrators chose for account mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMailSettings {
    /// The instance that sends account mail.
    pub plugin_instance_id: String,
    /// When a test message through it was last delivered; `None` until one
    /// is, and again after the instance changes.
    pub tested_at: Option<DateTime<Utc>>,
}

/// A message to queue: what it says, who it is for and who asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountMailRequest<'a> {
    /// What it says.
    pub kind: AccountMailKind,
    /// The user it is for.
    pub user_id: i64,
    /// The address it goes to.
    pub address: &'a str,
    /// The Administrator who asked for it, recorded so an invite can name
    /// them; `None` when the library sends it on its own.
    pub requested_by: Option<i64>,
}

/// One account-mail message and where its delivery stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMail {
    /// Message id.
    pub id: String,
    /// What it says.
    pub kind: AccountMailKind,
    /// The user it is for.
    pub user_id: i64,
    /// The address it goes to.
    pub address: String,
    /// The notifier instance that sends it.
    pub plugin_instance_id: String,
    /// Where it stands.
    pub state: DeliveryState,
    /// Send attempts made.
    pub attempts: i64,
    /// The Administrator who asked for an invite, named in its subject;
    /// `None` when the library sent it on its own or that user is gone.
    pub requested_by: Option<i64>,
}

/// The link token an account-mail send mints: the raw value goes into the
/// message once and only its hash is stored.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedToken(String);

impl MintedToken {
    /// The raw token, for the link in the message. Never logged.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Wraps a raw token the store just minted.
    pub(crate) fn new(raw: String) -> Self {
        Self(raw)
    }
}

impl std::fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MintedToken(<redacted>)")
    }
}

/// Length of a verification link's token: 32 random bytes as lowercase hex.
pub const VERIFY_TOKEN_CHARS: usize = 64;

/// Longest plugin instance id the account-mail settings body takes.
pub const MAX_INSTANCE_ID_BYTES: usize = 128;

/// The token of a verification link, as a patron's browser sends it back:
/// exactly [`VERIFY_TOKEN_CHARS`] lowercase hex characters.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifyEmailToken(String);

impl VerifyEmailToken {
    /// The raw token, to hash and look up. Never logged.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for VerifyEmailToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VerifyEmailToken(<redacted>)")
    }
}

/// The body of `PUT /api/notifications/account-mail`, read from a JSON
/// object only, before [`AccountMailSettingsRequest::validate`] checks it.
///
/// The route reads it with the web framework's JSON extractor and
/// [`parse_account_mail_settings`] reads it from bytes; both go through
/// this type's reader and the same validation.
#[derive(Debug)]
pub struct AccountMailSettingsRequest(SettingsBody);

/// The fields of [`AccountMailSettingsRequest`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    /// The notifier instance; `Some(None)` for `null`. A missing key fails
    /// the read.
    #[serde(deserialize_with = "present")]
    plugin_instance_id: Option<Option<String>>,
}

impl<'de> Deserialize<'de> for AccountMailSettingsRequest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::control_plane::from_object(d, "a JSON object").map(Self)
    }
}

impl AccountMailSettingsRequest {
    /// Checks the instance id.
    ///
    /// # Returns
    ///
    /// The instance id: 1 to [`MAX_INSTANCE_ID_BYTES`] printable ASCII
    /// bytes with no space, or `None` for `null`.
    ///
    /// # Errors
    ///
    /// [`crate::LibraryError::Invalid`] when the id breaks those rules.
    pub fn validate(self) -> crate::Result<Option<String>> {
        let Some(id) = self.0.plugin_instance_id.flatten() else {
            return Ok(None);
        };
        if id.is_empty()
            || id.len() > MAX_INSTANCE_ID_BYTES
            || !id.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(crate::LibraryError::Invalid(format!(
                "account-mail settings: plugin_instance_id must be 1 to \
                 {MAX_INSTANCE_ID_BYTES} printable characters with no space"
            )));
        }
        Ok(Some(id))
    }
}

/// Reads a present value, `null` included, as `Some`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// The body of `POST /api/auth/verify-email`, read from a JSON object
/// only, before [`VerifyEmailRequest::validate`] checks it.
///
/// The route reads it with the web framework's JSON extractor and
/// [`parse_verify_email_request`] reads it from bytes; both go through
/// this type's reader and the same validation.
pub struct VerifyEmailRequest(VerifyBody);

impl<'de> Deserialize<'de> for VerifyEmailRequest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::control_plane::from_object(d, "a JSON object").map(Self)
    }
}

impl std::fmt::Debug for VerifyEmailRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VerifyEmailRequest(<redacted>)")
    }
}

/// The fields of [`VerifyEmailRequest`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyBody {
    /// The token from the link.
    token: String,
}

impl VerifyEmailRequest {
    /// Checks the token.
    ///
    /// # Returns
    ///
    /// The token: exactly [`VERIFY_TOKEN_CHARS`] lowercase hex characters.
    ///
    /// # Errors
    ///
    /// [`crate::LibraryError::Invalid`] when the token breaks that rule.
    pub fn validate(self) -> crate::Result<VerifyEmailToken> {
        let token = self.0.token;
        if token.len() != VERIFY_TOKEN_CHARS
            || !token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(crate::LibraryError::Invalid(format!(
                "verify-email body: token must be {VERIFY_TOKEN_CHARS} lowercase hex characters"
            )));
        }
        Ok(VerifyEmailToken(token))
    }
}

/// Longest password a reset link sets, in bytes.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Whether `token` is a link token: exactly [`VERIFY_TOKEN_CHARS`]
/// lowercase hex characters.
fn is_link_token(token: &str) -> bool {
    token.len() == VERIFY_TOKEN_CHARS
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A reset link that was redeemed: the password is set and every session
/// of the account is signed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedeemedReset {
    /// How many portal sessions were signed out, or `None` when the
    /// database's reply was lost and the retry found this reset already
    /// applied: the count left with the lost reply.
    pub revoked_sessions: Option<u64>,
}

/// The second factor an account required when its reset was checked.
///
/// Redeeming a link repeats the check inside the same atomic write, so a
/// factor enrolled or removed after the check makes the redemption fail
/// rather than set a password without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordResetFactor {
    /// No authenticator and no passkey.
    None,
    /// An authenticator app (it takes precedence over passkeys).
    Totp,
    /// At least one passkey and no authenticator.
    Passkey,
}

/// The token of a password-reset link, as the patron's browser sends it
/// back: exactly [`VERIFY_TOKEN_CHARS`] lowercase hex characters.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordResetToken(String);

impl PasswordResetToken {
    /// The raw token, to hash and look up. Never logged.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Checks a token read from a body.
    ///
    /// # Errors
    ///
    /// [`crate::LibraryError::Invalid`] when it is not a link token.
    fn checked(token: String) -> crate::Result<Self> {
        if is_link_token(&token) {
            Ok(Self(token))
        } else {
            Err(crate::LibraryError::Invalid(format!(
                "password reset: token must be {VERIFY_TOKEN_CHARS} lowercase hex characters"
            )))
        }
    }
}

impl std::fmt::Debug for PasswordResetToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordResetToken(<redacted>)")
    }
}

/// The body of `POST /api/auth/password-reset`: the address to send a
/// reset link to, read from a JSON object only.
#[derive(Debug)]
pub struct PasswordResetAsk(AskBody);

/// The fields of [`PasswordResetAsk`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskBody {
    /// The address, as typed.
    email: String,
}

impl<'de> Deserialize<'de> for PasswordResetAsk {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::control_plane::from_object(d, "a JSON object").map(Self)
    }
}

impl PasswordResetAsk {
    /// Checks the address.
    ///
    /// # Returns
    ///
    /// The address trimmed and lowercased, as addresses are stored.
    ///
    /// # Errors
    ///
    /// [`crate::LibraryError::Invalid`] when it is empty or not a valid
    /// address.
    pub fn validate(self) -> crate::Result<String> {
        match crate::email::normalize_user_email(Some(&self.0.email)) {
            Ok(Some(address)) => Ok(address),
            Ok(None) | Err(_) => Err(crate::LibraryError::Invalid(
                "password reset: email must be a valid address".into(),
            )),
        }
    }
}

/// The body of `POST /api/auth/password-reset/check` and
/// `POST /api/auth/password-reset/passkey`: a reset link's token, read
/// from a JSON object only.
pub struct PasswordResetLink(LinkBody);

/// The fields of [`PasswordResetLink`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkBody {
    /// The token from the link.
    token: String,
}

impl std::fmt::Debug for PasswordResetLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordResetLink(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for PasswordResetLink {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::control_plane::from_object(d, "a JSON object").map(Self)
    }
}

impl PasswordResetLink {
    /// Checks the token.
    ///
    /// # Errors
    ///
    /// [`crate::LibraryError::Invalid`] when it is not a link token.
    pub fn validate(self) -> crate::Result<PasswordResetToken> {
        PasswordResetToken::checked(self.0.token)
    }
}

/// The second factor a reset is finished with.
#[derive(Clone, PartialEq)]
pub enum ResetSecondFactor {
    /// None was sent; enough only for an account without one.
    None,
    /// A six-digit authenticator code.
    Totp(String),
    /// A passkey assertion answering the challenge the reset began.
    Passkey {
        /// The challenge from `POST /api/auth/password-reset/passkey`.
        challenge_id: String,
        /// The browser's credential, a JSON object WebAuthn reads.
        credential: serde_json::Value,
    },
}

impl std::fmt::Debug for ResetSecondFactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::Totp(_) => "Totp(<redacted>)",
            Self::Passkey { .. } => "Passkey(<redacted>)",
        })
    }
}

/// A checked [`PasswordResetFinish`].
pub struct ResetFinish {
    /// The link's token.
    pub token: PasswordResetToken,
    /// The new password, trimmed; 1 to [`MAX_PASSWORD_BYTES`] bytes.
    pub password: zeroize::Zeroizing<String>,
    /// The second factor sent with it.
    pub second_factor: ResetSecondFactor,
}

impl std::fmt::Debug for ResetFinish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResetFinish")
            .field("token", &self.token)
            .field("password", &"<redacted>")
            .field("second_factor", &self.second_factor)
            .finish()
    }
}

/// The body of `POST /api/auth/password-reset/finish`, read from a JSON
/// object only.
pub struct PasswordResetFinish(FinishBody);

/// The fields of [`PasswordResetFinish`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishBody {
    /// The token from the link.
    token: String,
    /// The new password.
    password: String,
    /// An authenticator code, for an account with one.
    #[serde(default)]
    totp_code: Option<String>,
    /// A passkey assertion, for an account with passkeys and no
    /// authenticator app.
    #[serde(default)]
    passkey: Option<PasskeyAnswer>,
}

/// A passkey assertion in a reset's finish body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasskeyAnswer {
    /// The challenge the reset began.
    challenge_id: String,
    /// The browser's credential.
    credential: serde_json::Value,
}

/// Longest passkey challenge id a reset body takes.
const MAX_CHALLENGE_ID_BYTES: usize = 64;

impl std::fmt::Debug for PasswordResetFinish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordResetFinish(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for PasswordResetFinish {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::control_plane::from_object(d, "a JSON object").map(Self)
    }
}

impl PasswordResetFinish {
    /// Checks the body.
    ///
    /// # Returns
    ///
    /// The token, the trimmed password and the second factor.
    ///
    /// # Errors
    ///
    /// [`crate::LibraryError::Invalid`] naming the first rule the body
    /// breaks: the token is not a link token; the password is empty or
    /// longer than [`MAX_PASSWORD_BYTES`]; `totp_code` is not six digits;
    /// the passkey's challenge id is not 1 to 64 printable characters or
    /// its credential is not an object; or both factors are sent.
    pub fn validate(self) -> crate::Result<ResetFinish> {
        let body = self.0;
        let invalid = |rule: &str| crate::LibraryError::Invalid(format!("password reset: {rule}"));
        let token = PasswordResetToken::checked(body.token)?;
        let raw = zeroize::Zeroizing::new(body.password);
        let password = raw.trim();
        if password.is_empty() || password.len() > MAX_PASSWORD_BYTES {
            return Err(invalid(&format!(
                "password must be 1 to {MAX_PASSWORD_BYTES} bytes"
            )));
        }
        let second_factor = match (body.totp_code, body.passkey) {
            (Some(_), Some(_)) => return Err(invalid("send totp_code or passkey, not both")),
            (Some(code), None) => {
                if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid("totp_code must be six digits"));
                }
                ResetSecondFactor::Totp(code)
            }
            (None, Some(answer)) => {
                if answer.challenge_id.is_empty()
                    || answer.challenge_id.len() > MAX_CHALLENGE_ID_BYTES
                    || !answer.challenge_id.bytes().all(|b| b.is_ascii_graphic())
                {
                    return Err(invalid(
                        "passkey.challenge_id must be 1 to 64 printable characters",
                    ));
                }
                if !answer.credential.is_object() {
                    return Err(invalid("passkey.credential must be an object"));
                }
                ResetSecondFactor::Passkey {
                    challenge_id: answer.challenge_id,
                    credential: answer.credential,
                }
            }
            (None, None) => ResetSecondFactor::None,
        };
        Ok(ResetFinish {
            token,
            password: zeroize::Zeroizing::new(password.to_string()),
            second_factor,
        })
    }
}

/// Parses the body of `POST /api/auth/password-reset` from bytes
/// (sans-IO), for the fuzz target and tests; the route reads the same
/// [`PasswordResetAsk`] with the JSON extractor.
///
/// # Errors
///
/// [`crate::LibraryError::Invalid`] naming the first rule the body breaks.
pub fn parse_password_reset_ask(body: &[u8]) -> crate::Result<String> {
    read_body::<PasswordResetAsk>(body, "password reset")?.validate()
}

/// Parses the body of `POST /api/auth/password-reset/check` from bytes
/// (sans-IO), for the fuzz target and tests; the route reads the same
/// [`PasswordResetLink`] with the JSON extractor.
///
/// # Errors
///
/// [`crate::LibraryError::Invalid`] naming the first rule the body breaks.
pub fn parse_password_reset_link(body: &[u8]) -> crate::Result<PasswordResetToken> {
    read_body::<PasswordResetLink>(body, "password reset")?.validate()
}

/// Parses the body of `POST /api/auth/password-reset/finish` from bytes
/// (sans-IO), for the fuzz target and tests; the route reads the same
/// [`PasswordResetFinish`] with the JSON extractor.
///
/// # Errors
///
/// [`crate::LibraryError::Invalid`] naming the first rule the body breaks.
pub fn parse_password_reset_finish(body: &[u8]) -> crate::Result<ResetFinish> {
    read_body::<PasswordResetFinish>(body, "password reset")?.validate()
}

/// Reads a body as `T` from bytes, for the fuzz target and tests (the
/// routes use the web framework's JSON extractor on the same types).
fn read_body<'de, T: Deserialize<'de>>(body: &'de [u8], what: &str) -> crate::Result<T> {
    serde_json::from_slice::<T>(body)
        .map_err(|err| crate::LibraryError::Invalid(format!("{what}: {err}")))
}

/// Parses the body of `PUT /api/notifications/account-mail`
/// from bytes (sans-IO), for the fuzz target and tests. The route reads
/// the same [`AccountMailSettingsRequest`] with the JSON extractor and
/// calls the same validation.
///
/// The body is one JSON object whose only key is `plugin_instance_id`: a
/// string of 1 to [`MAX_INSTANCE_ID_BYTES`] printable ASCII bytes with no
/// space, or `null` to turn account mail off. The key is required and may
/// not repeat.
///
/// # Returns
///
/// The instance id, or `None` for `null`.
///
/// # Errors
///
/// [`crate::LibraryError::Invalid`] naming the first rule the body breaks.
pub fn parse_account_mail_settings(body: &[u8]) -> crate::Result<Option<String>> {
    read_body::<AccountMailSettingsRequest>(body, "account-mail settings")?.validate()
}

/// Parses the body of `POST /api/auth/verify-email` from bytes (sans-IO),
/// for the fuzz target and tests. The route reads the same
/// [`VerifyEmailRequest`] with the JSON extractor and calls the same
/// validation.
///
/// The body is one JSON object whose only key is `token`: exactly
/// [`VERIFY_TOKEN_CHARS`] lowercase hex characters. The key is required
/// and may not repeat.
///
/// # Errors
///
/// [`crate::LibraryError::Invalid`] naming the first rule the body breaks.
pub fn parse_verify_email_request(body: &[u8]) -> crate::Result<VerifyEmailToken> {
    read_body::<VerifyEmailRequest>(body, "verify-email body")?.validate()
}

/// Every audience, in the order of the `audience` column's CHECK list.
pub const AUDIENCES: [NotificationAudience; 3] = [
    NotificationAudience::Operational,
    NotificationAudience::Patron,
    NotificationAudience::Account,
];

/// Every severity, lowest first, in the order of the CHECK lists.
pub const SEVERITIES: [NotificationSeverity; 3] = [
    NotificationSeverity::Info,
    NotificationSeverity::Warning,
    NotificationSeverity::Critical,
];

/// Every alert action, in the order of the `alert_action` CHECK list.
pub const ALERT_ACTIONS: [NotificationAlertAction; 3] = [
    NotificationAlertAction::None,
    NotificationAlertAction::Trigger,
    NotificationAlertAction::Resolve,
];

/// A notification type the host itself raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostNotificationType {
    /// Type id; host types carry no plugin prefix.
    pub id: &'static str,
    /// Who it is for.
    pub audience: NotificationAudience,
    /// Default severity.
    pub severity: NotificationSeverity,
    /// Title shown in the routing matrix and on each notification.
    pub title: &'static str,
}

/// The host's own notification types. Producers of operational and patron
/// events add theirs here as they land.
pub const HOST_NOTIFICATION_TYPES: &[HostNotificationType] = &[HostNotificationType {
    id: TEST_NOTIFICATION_TYPE,
    audience: NotificationAudience::Operational,
    severity: NotificationSeverity::Info,
    title: "Test notification",
}];

/// The host type named `id`, if it is one.
#[must_use]
pub fn host_notification_type(id: &str) -> Option<&'static HostNotificationType> {
    HOST_NOTIFICATION_TYPES.iter().find(|t| t.id == id)
}

/// The routed id of a plugin's type: `<plugin id>:<type id>`.
#[must_use]
pub fn qualified_plugin_type(plugin_id: &str, type_id: &str) -> String {
    format!("{plugin_id}:{type_id}")
}

/// A notification for the router to accept: rendered once, copied to every
/// destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNotification {
    /// Qualified type (`test`, `<plugin>:<type>`).
    pub notification_type: String,
    /// Who it is for; decides how it is routed.
    pub audience: NotificationAudience,
    /// How urgent it is.
    pub severity: NotificationSeverity,
    /// Short title.
    pub title: String,
    /// Plain-text body.
    pub text: String,
    /// Path inside the library to link to; the send job prefixes the
    /// public origin.
    pub link_path: Option<String>,
    /// Labelled values.
    pub fields: Vec<NotificationField>,
    /// Grouping key.
    pub group_key: Option<String>,
    /// Alert lifecycle step.
    pub alert_action: NotificationAlertAction,
    /// Alert key; required unless `alert_action` is `none`.
    pub alert_key: Option<String>,
    /// Empty for a host type, else the raising plugin's id.
    pub source: String,
    /// Patron a patron-audience notification is for; required for that
    /// audience and refused for the others.
    pub patron: Option<i64>,
}

impl NewNotification {
    /// Checks the notification the way every send will be checked, so a bad
    /// one is refused before anything is stored.
    ///
    /// # Arguments
    ///
    /// * `notification_id` - The id it will be stored under.
    ///
    /// # Errors
    ///
    /// The first [`NotificationError`] the rendered message or its link path
    /// breaks; [`NotificationError::BadId`] for a patron that does not match
    /// the audience.
    pub fn check(&self, notification_id: &str) -> Result<(), NotificationError> {
        match (self.audience, self.patron) {
            (NotificationAudience::Patron, Some(_)) => {}
            (NotificationAudience::Patron, None) => {
                return Err(NotificationError::BadId("patronId"));
            }
            (NotificationAudience::Operational | NotificationAudience::Account, Some(_)) => {
                return Err(NotificationError::BadId("patronId"));
            }
            (NotificationAudience::Operational | NotificationAudience::Account, None) => {}
        }
        if let Some(path) = self.link_path.as_deref().filter(|p| !p.is_empty()) {
            check_link_path(path)?;
        }
        check_message(&self.message(notification_id, None, None))
    }

    /// The message a notifier is sent, with `link` and `library_origin`
    /// already built from the public origin ([`link_for`]).
    #[must_use]
    pub fn message(
        &self,
        notification_id: &str,
        link: Option<String>,
        library_origin: Option<String>,
    ) -> NotificationMessage {
        NotificationMessage {
            notification_id: notification_id.to_string(),
            notification_type: self.notification_type.clone(),
            severity: self.severity,
            title: self.title.clone(),
            text: self.text.clone(),
            markdown: None,
            html: None,
            link,
            fields: self.fields.clone(),
            group_key: self.group_key.clone(),
            alert_action: self.alert_action,
            alert_key: self.alert_key.clone(),
            sensitive: false,
            library_origin,
        }
    }
}

/// A plain-text body from labelled fields: one `name: value` line each,
/// cut at a character boundary (ending in `…`) when it would pass
/// `maxNotificationTextBytes`, so a raise within the field limits is never
/// refused for its rendered body; the fields themselves are kept whole.
#[must_use]
pub fn fields_text(fields: &[NotificationField]) -> String {
    let text = fields
        .iter()
        .map(|f| format!("{}: {}", f.name, f.value))
        .collect::<Vec<_>>()
        .join("\n");
    let max =
        usize::try_from(bookclerk_plugin_abi::MAX_NOTIFICATION_TEXT_BYTES).unwrap_or(usize::MAX);
    if text.len() <= max {
        return text;
    }
    let ellipsis = '\u{2026}';
    let mut end = max.saturating_sub(ellipsis.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = text[..end].to_owned();
    cut.push(ellipsis);
    cut
}

/// The link a notifier is sent: the library's public origin and the path.
/// `None` when either is missing, or when the result is not a link a
/// notifier may be sent (an `http://` origin that is not loopback, say), so
/// the message goes without a link rather than not at all.
#[must_use]
pub fn link_for(public_origin: Option<&str>, path: Option<&str>) -> Option<String> {
    let origin = public_origin?.trim().trim_end_matches('/');
    let path = path.filter(|p| !p.is_empty())?;
    if origin.is_empty() {
        return None;
    }
    let link = format!("{origin}{path}");
    bookclerk_plugin_abi::notifications::check_link(&link)
        .is_ok()
        .then_some(link)
}

/// Whether a route with minimum severity `min` takes a notification of
/// `severity`.
#[must_use]
pub fn severity_reaches(severity: NotificationSeverity, min: NotificationSeverity) -> bool {
    bookclerk_plugin_abi::notifications::severity_rank(severity)
        >= bookclerk_plugin_abi::notifications::severity_rank(min)
}

/// One accepted notification's delivery waiting for its send job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelivery {
    /// Delivery-log row id.
    pub id: String,
    /// Notifier instance that sends it.
    pub plugin_instance_id: String,
}

/// A send job that has been ready to run for a long time and that nothing
/// claimed. It is stalled only when its notifier instance runs on no live
/// node; otherwise it is waiting for a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledSend {
    /// The pending send job nothing claimed.
    pub job_id: String,
    /// When the job became ready to run, as stored: the write that fails
    /// it matches this, so a job retried in the meantime is left alone.
    pub run_after: String,
    /// The job's lease generation when it was read, matched the same way.
    pub lease_generation: i64,
    /// Its pending delivery, named by the job's dedup key; `None` when the
    /// delivery is gone, already finished or linked to another job, and
    /// the job only needs cancelling.
    pub delivery_id: Option<String>,
    /// Its pending account mail, when the job sends one instead of a
    /// delivery; `None` when the mail is gone, finished or linked to
    /// another job.
    pub account_mail_id: Option<String>,
    /// The notifier instance the delivery or account mail goes through,
    /// or that a password-reset request names; `None` when the job has
    /// none of those.
    pub notifier: Option<String>,
}

/// What failing one [`StalledSend`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallOutcome {
    /// The job was claimed, retried or ended meanwhile; nothing changed.
    NotStalled,
    /// The job was cancelled; it had no pending delivery to fail.
    Cancelled,
    /// The job was cancelled and its delivery dropped, because the
    /// destination was already paused; nothing was counted.
    Dropped,
    /// The job was cancelled and its delivery failed, in one write.
    Failed(FailureEffect),
}

/// What the router did with a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    /// The id it was stored under.
    pub notification_id: String,
    /// The deliveries it made, each with a send job enqueued (or left for
    /// the sweep when the enqueue failed).
    pub deliveries: Vec<PendingDelivery>,
    /// Whether it reached an inbox: its patron's, or the staff's through an
    /// "In app" route.
    pub inboxed: bool,
}

/// A destination as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Destination id.
    pub id: String,
    /// Notifier instance that sends to it.
    pub plugin_instance_id: String,
    /// Channel or personal.
    pub kind: DestinationKind,
    /// Owner of a personal destination.
    pub user_id: Option<i64>,
    /// Name shown to people.
    pub label: String,
    /// Canonical address document (empty for a channel). May hold secrets.
    pub address_json: String,
    /// Permanent failures since the last delivery.
    pub failure_count: i64,
    /// When it was paused; `None` while active.
    pub paused_at: Option<String>,
    /// Why it was paused.
    pub paused_reason: Option<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// A destination to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDestination {
    /// Notifier instance that will send to it.
    pub plugin_instance_id: String,
    /// Channel or personal.
    pub kind: DestinationKind,
    /// Owner; required for a personal destination, refused for a channel.
    pub user_id: Option<i64>,
    /// Name shown to people; one line, at most
    /// [`MAX_DESTINATION_LABEL_BYTES`].
    pub label: String,
    /// Canonical address document the notifier validated (empty for a
    /// channel).
    pub address_json: String,
}

/// One operational route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Channel destination, or [`IN_APP_DESTINATION`] for the staff inbox.
    pub destination_id: String,
    /// Lowest severity sent.
    pub min_severity: NotificationSeverity,
}

/// One instance offered to patrons for a patron type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatronOffer {
    /// Notifier instance.
    pub plugin_instance_id: String,
    /// Whether a patron who has not chosen gets the type there.
    pub default_on: bool,
}

/// One patron's choice for one patron type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatronChoice {
    /// Qualified patron type.
    pub notification_type: String,
    /// Whether the patron wants it outside the inbox.
    pub enabled: bool,
    /// The patron's personal destinations it goes to.
    pub destination_ids: Vec<String>,
}

/// A stored notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNotification {
    /// Notification id.
    pub id: String,
    /// The notification as accepted.
    pub notification: NewNotification,
    /// RFC 3339 creation time.
    pub created_at: String,
}

/// Everything one `notify_send` job needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendWork {
    /// Delivery-log row id.
    pub delivery_id: String,
    /// Where the delivery stands.
    pub state: DeliveryState,
    /// Attempts made so far.
    pub attempts: i64,
    /// Notifier instance that sends it.
    pub plugin_instance_id: String,
    /// The notification.
    pub notification: StoredNotification,
    /// The destination; `None` when it was removed.
    pub destination: Option<Destination>,
}

/// One delivery-log row, joined with what a reader needs to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRecord {
    /// Delivery-log row id.
    pub id: String,
    /// Notification id.
    pub notification_id: String,
    /// Qualified type.
    pub notification_type: String,
    /// Notification title.
    pub title: String,
    /// Destination id.
    pub destination_id: String,
    /// Notifier instance.
    pub plugin_instance_id: String,
    /// Where it stands.
    pub state: DeliveryState,
    /// Attempts made.
    pub attempts: i64,
    /// The service's id for the delivered message.
    pub provider_id: Option<String>,
    /// Last failure or retry reason.
    pub reason: Option<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time of the last change.
    pub updated_at: String,
}

/// One inbox entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxEntry {
    /// The notification.
    pub notification: StoredNotification,
    /// When the patron read it; `None` while unread.
    pub read_at: Option<String>,
}

/// What a finished send attempt did to its destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureEffect {
    /// The destination stays active.
    Counted,
    /// This failure paused the destination.
    Paused,
}

/// Whether recording a failed delivery spends one more attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedAttempt {
    /// A send the notifier refused for good: this answer is a new attempt.
    New,
    /// A delivery that ran out of attempts: its last answer (a retry) was
    /// already counted when it was recorded.
    AlreadyCounted,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operational() -> NewNotification {
        NewNotification {
            notification_type: "demo:sync_failed".into(),
            audience: NotificationAudience::Operational,
            severity: NotificationSeverity::Warning,
            title: "Sync failed".into(),
            text: "account: main".into(),
            link_path: Some("/settings/plugins".into()),
            fields: vec![NotificationField {
                name: "account".into(),
                value: "main".into(),
            }],
            group_key: None,
            alert_action: NotificationAlertAction::None,
            alert_key: None,
            source: "demo".into(),
            patron: None,
        }
    }

    #[test]
    fn kinds_and_states_round_trip_their_spelling() {
        for kind in DestinationKind::ALL {
            assert_eq!(DestinationKind::parse(kind.as_str()), Some(kind));
        }
        for state in DeliveryState::ALL {
            assert_eq!(DeliveryState::parse(state.as_str()), Some(state));
        }
        assert_eq!(DestinationKind::parse("x"), None);
        assert_eq!(DeliveryState::parse("x"), None);
        assert_eq!(
            DestinationKind::Personal.wire(),
            NotifyDestinationKind::Personal
        );
        assert_eq!(
            DestinationKind::Channel.wire(),
            NotifyDestinationKind::Channel
        );
    }

    #[test]
    fn a_patron_is_required_exactly_for_the_patron_audience() {
        let id = "8f0e2c9a-0000-4000-8000-000000000001";
        assert!(operational().check(id).is_ok());
        let mut with_patron = operational();
        with_patron.patron = Some(3);
        assert_eq!(
            with_patron.check(id),
            Err(NotificationError::BadId("patronId"))
        );
        let mut patron = operational();
        patron.audience = NotificationAudience::Patron;
        assert_eq!(patron.check(id), Err(NotificationError::BadId("patronId")));
        patron.patron = Some(3);
        assert!(patron.check(id).is_ok());
    }

    #[test]
    fn a_bad_link_path_or_title_is_refused() {
        let id = "8f0e2c9a-0000-4000-8000-000000000001";
        let mut bad = operational();
        bad.link_path = Some("//evil.example/x".into());
        assert_eq!(bad.check(id), Err(NotificationError::BadLinkPath));
        let mut blank = operational();
        blank.title = "  ".into();
        assert!(blank.check(id).is_err());
        let mut no_link = operational();
        no_link.link_path = Some(String::new());
        assert!(no_link.check(id).is_ok());
    }

    #[test]
    fn links_join_the_origin_and_the_path() {
        assert_eq!(
            link_for(Some("https://books.example/"), Some("/works/1")),
            Some("https://books.example/works/1".into())
        );
        assert_eq!(link_for(None, Some("/works/1")), None);
        assert_eq!(link_for(Some("https://books.example"), None), None);
        assert_eq!(link_for(Some("https://books.example"), Some("")), None);
        assert_eq!(link_for(Some("  "), Some("/x")), None);
        // A plain-http LAN origin is not a link notifiers may be sent.
        assert_eq!(link_for(Some("http://192.168.1.10:8787"), Some("/x")), None);
        assert_eq!(
            link_for(Some("http://127.0.0.1:8787"), Some("/x")).as_deref(),
            Some("http://127.0.0.1:8787/x")
        );
    }

    #[test]
    fn severity_reaches_compares_by_rank() {
        use NotificationSeverity::{Critical, Info, Warning};
        assert!(severity_reaches(Critical, Warning));
        assert!(severity_reaches(Warning, Warning));
        assert!(!severity_reaches(Info, Warning));
    }

    #[test]
    fn fields_render_as_lines() {
        let fields = vec![
            NotificationField {
                name: "a".into(),
                value: "1".into(),
            },
            NotificationField {
                name: "b".into(),
                value: "2".into(),
            },
        ];
        assert_eq!(fields_text(&fields), "a: 1\nb: 2");
        assert_eq!(fields_text(&[]), "");
    }

    #[test]
    fn a_long_rendering_is_cut_within_the_text_cap() {
        // Sixteen full fields render past the cap; the body is cut at a
        // character boundary and ends in an ellipsis.
        let fields: Vec<NotificationField> = (b'A'..=b'P')
            .map(|c| NotificationField {
                name: char::from(c).to_string(),
                value: "\u{e9}".repeat(512),
            })
            .collect();
        let max = usize::try_from(bookclerk_plugin_abi::MAX_NOTIFICATION_TEXT_BYTES).unwrap();
        let text = fields_text(&fields);
        assert!(text.len() <= max, "{}", text.len());
        assert!(text.ends_with('\u{2026}'));
        assert!(text.starts_with("A: \u{e9}"));
    }

    #[test]
    fn host_types_are_found_by_id_and_plugin_types_are_qualified() {
        assert_eq!(
            host_notification_type(TEST_NOTIFICATION_TYPE).map(|t| t.title),
            Some("Test notification")
        );
        assert!(host_notification_type("nope").is_none());
        assert_eq!(qualified_plugin_type("demo", "x"), "demo:x");
    }
}
