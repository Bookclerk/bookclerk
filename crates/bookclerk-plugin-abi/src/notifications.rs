//! The checks the host runs on notification requests and replies.
//!
//! Three things cross a trust boundary here, and each is checked by sans-IO
//! code that the fuzz targets and the host share:
//!
//! - **A notifier's reply** to `Notifier.send` ([`classify_send_reply`]):
//!   delivered, retry later or failed for good. The plugin's error text is
//!   cut to a short single line ([`sanitize_reason`]) before the delivery
//!   log stores it.
//! - **A destination address document** ([`parse_address_document`]): the
//!   JSON object a patron typed (an email address, a Telegram chat id, a
//!   push subscription) and that a notifier's `validateDestination`
//!   normalised. Only a flat object of short string values is accepted.
//!   The `address` object of a patron's request body is first read as
//!   [`AddressFields`], which refuses a key written twice.
//! - **A plugin's raise** on the `NOTIFY` binding ([`check_raise_params`]):
//!   a declared type id, labelled fields, an optional patron and a path
//!   inside the library.
//!
//! - **A notifier's deliverability report** ([`check_deliverability_report`]):
//!   the checks and advice shown to the operator, bounded and cut to
//!   single lines, with report links kept only when they are `https`.
//!
//! The host also checks every message it renders ([`check_message`])
//! before a notifier sees it, so a bug in a renderer cannot hand a plugin
//! an oversized body or a link to somewhere other than the library.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::{
    NotificationAlertAction, NotificationField, NotificationMessage, NotificationSeverity,
    NotifyCheckOutcome, NotifyDeliverabilityAdvice, NotifyDeliverabilityCheck,
    NotifyDeliverabilityReport, NotifyDestinationKind, NotifyRaiseParams, NotifySendParams,
    NotifySent, NotifyValidated, PluginError, PluginErrorCode, MAX_DELIVERABILITY_ADVICE,
    MAX_DELIVERABILITY_CHECKS, MAX_NOTIFICATION_FIELDS, MAX_NOTIFICATION_FIELD_BYTES,
    MAX_NOTIFICATION_HTML_BYTES, MAX_NOTIFICATION_TEXT_BYTES, MAX_NOTIFICATION_TITLE_BYTES,
    MAX_NOTIFIER_ADDRESS_BYTES,
};

/// The `address` object of a patron's
/// `POST /api/me/notifications/destinations` body, as sent: a flat object
/// of strings in which no key is written twice. JSON parsers keep the last
/// of a repeated key, so a repeat is refused instead of silently dropping
/// a value. The notifier's declared fields and [`parse_address_document`]
/// check it further.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressFields(pub BTreeMap<String, String>);

impl<'de> serde::Deserialize<'de> for AddressFields {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// Collects the entries, failing on a repeat.
        struct Fields;

        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = AddressFields;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object of strings")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    if out.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "the address repeats the key `{key}`"
                        )));
                    }
                    out.insert(key, value);
                }
                Ok(AddressFields(out))
            }
        }

        de.deserialize_map(Fields)
    }
}

/// Longest notification id, destination id or patron id, in bytes. Ids are
/// printable ASCII without spaces.
pub const MAX_NOTIFY_ID_BYTES: usize = 128;

/// Longest local notification type id (`job_failed`), in bytes; type ids
/// follow the ABI identifier rule `[a-z][a-z0-9_]{0,63}`.
pub const MAX_NOTIFICATION_TYPE_ID_BYTES: usize = 64;

/// Most keys in one destination address document.
pub const MAX_NOTIFY_ADDRESS_KEYS: usize = 16;

/// Longest reason the delivery log keeps from a notifier's error, in bytes.
pub const MAX_NOTIFY_REASON_BYTES: usize = 512;

/// Longest wait the host honours from a notifier's `retryAfterMs`. A
/// service asking for longer is retried after this long instead.
pub const MAX_NOTIFY_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// Why a notification request, message or reply was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationError {
    /// A notification type id is not `[a-z][a-z0-9_]{0,63}`, optionally
    /// prefixed by a plugin id and `:`.
    BadTypeId,
    /// An id (notification, destination, patron) is empty, too long, or
    /// holds a space or a non-printable byte.
    BadId(&'static str),
    /// A text value is empty where it may not be, over its cap, or holds
    /// a control character it may not.
    BadText(&'static str),
    /// More than [`MAX_NOTIFICATION_FIELDS`] fields.
    TooManyFields,
    /// A link is not an absolute `https` URL (or `http` to a loopback
    /// host), carries credentials, or is too long.
    BadLink,
    /// A link path is not an absolute path inside the library.
    BadLinkPath,
    /// An alert action other than `none` came without an alert key, or an
    /// alert key came without an action.
    BadAlert,
    /// An address document is not a flat JSON object of short strings, or
    /// is over [`MAX_NOTIFIER_ADDRESS_BYTES`].
    BadAddress(&'static str),
    /// An address document lacks a field the notifier requires.
    MissingAddressField(String),
    /// An address document carries a field the notifier does not declare.
    UnknownAddressField(String),
    /// A channel destination carried an address, or a personal one none.
    AddressKindMismatch,
    /// A deliverability report lists more than
    /// [`MAX_DELIVERABILITY_CHECKS`] checks or
    /// [`MAX_DELIVERABILITY_ADVICE`] pieces of advice.
    TooManyDeliverabilityEntries,
}

impl std::fmt::Display for NotificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadTypeId => f.write_str("notification type id is not an identifier"),
            Self::BadId(what) => write!(f, "notification `{what}` is not a valid id"),
            Self::BadText(what) => write!(f, "notification `{what}` is malformed"),
            Self::TooManyFields => write!(
                f,
                "notification has more than {MAX_NOTIFICATION_FIELDS} fields"
            ),
            Self::BadLink => f.write_str("notification link is not an https link"),
            Self::BadLinkPath => f.write_str("notification link path is not a library path"),
            Self::BadAlert => {
                f.write_str("notification alert action and alert key must come together")
            }
            Self::BadAddress(why) => write!(f, "destination address {why}"),
            Self::MissingAddressField(key) => {
                write!(f, "destination address lacks the required field `{key}`")
            }
            Self::UnknownAddressField(key) => {
                write!(f, "destination address field `{key}` is not declared")
            }
            Self::AddressKindMismatch => {
                f.write_str("a channel destination takes no address and a personal one needs one")
            }
            Self::TooManyDeliverabilityEntries => write!(
                f,
                "deliverability report lists more than {MAX_DELIVERABILITY_CHECKS} checks or \
                 {MAX_DELIVERABILITY_ADVICE} pieces of advice"
            ),
        }
    }
}

impl std::error::Error for NotificationError {}

impl From<NotificationError> for PluginError {
    fn from(err: NotificationError) -> Self {
        PluginError::invalid_params(err.to_string())
    }
}

/// Converts a `u32` ABI cap to `usize`, saturating on 16-bit targets.
fn cap(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Whether `id` is a local notification type id: `[a-z][a-z0-9_]{0,63}`.
///
/// # Arguments
///
/// * `id` - The type id as a plugin declares it, without its plugin prefix.
///
/// # Returns
///
/// True when the id is well formed.
#[must_use]
pub fn is_notification_type_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            first.is_ascii_lowercase()
                && bytes.len() <= MAX_NOTIFICATION_TYPE_ID_BYTES
                && rest
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        }
        None => false,
    }
}

/// Whether `id` is a routed notification type id: a host type
/// (`job_failed`) or a plugin type qualified by its plugin id
/// (`ntfy_relay:sync_failed`, plugin ids being `[a-z0-9_]{2,32}`).
///
/// # Arguments
///
/// * `id` - The type id as the router and the routing rules name it.
///
/// # Returns
///
/// True when the id is well formed.
#[must_use]
pub fn is_qualified_notification_type(id: &str) -> bool {
    match id.split_once(':') {
        None => is_notification_type_id(id),
        Some((plugin, local)) => {
            (2..=32).contains(&plugin.len())
                && plugin
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                && is_notification_type_id(local)
        }
    }
}

/// Whether `id` is a usable id: 1 to [`MAX_NOTIFY_ID_BYTES`] bytes of
/// printable ASCII with no space.
///
/// # Arguments
///
/// * `id` - The id to check.
///
/// # Returns
///
/// True when the id is well formed.
#[must_use]
pub fn is_notify_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_NOTIFY_ID_BYTES && id.bytes().all(|b| b.is_ascii_graphic())
}

/// Whether `c` breaks a line or reorders text without being a control
/// character: the Unicode line and paragraph separators and the
/// bidirectional marks, embeddings, overrides and isolates. A one-line
/// value refuses them so it shows as written.
///
/// # Arguments
///
/// * `c` - The character to test.
///
/// # Returns
///
/// True for U+2028, U+2029, U+061C, U+200E, U+200F, U+202A to U+202E and
/// U+2066 to U+2069.
#[must_use]
pub fn is_layout_char(c: char) -> bool {
    matches!(
        c,
        '\u{2028}'
            | '\u{2029}'
            | '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Checks one text value: within `max` bytes and free of control and
/// [layout](is_layout_char) characters, except tab, line breaks and the
/// line and paragraph separators when `multiline`; non-blank when
/// `required`.
///
/// # Errors
///
/// [`NotificationError::BadText`] naming `what`.
fn check_text(
    value: &str,
    max: usize,
    multiline: bool,
    required: bool,
    what: &'static str,
) -> Result<(), NotificationError> {
    let allowed = |c: char| multiline && matches!(c, '\n' | '\r' | '\t' | '\u{2028}' | '\u{2029}');
    if value.len() > max
        || (required && value.trim().is_empty())
        || value
            .chars()
            .any(|c| (c.is_control() || is_layout_char(c)) && !allowed(c))
    {
        return Err(NotificationError::BadText(what));
    }
    Ok(())
}

/// Checks labelled fields: at most [`MAX_NOTIFICATION_FIELDS`], each name a
/// non-blank single line within [`MAX_NOTIFICATION_TITLE_BYTES`] and each
/// value within [`MAX_NOTIFICATION_FIELD_BYTES`].
///
/// # Errors
///
/// [`NotificationError::TooManyFields`] or [`NotificationError::BadText`].
fn check_fields(fields: &[NotificationField]) -> Result<(), NotificationError> {
    if fields.len() > cap(MAX_NOTIFICATION_FIELDS) {
        return Err(NotificationError::TooManyFields);
    }
    for field in fields {
        check_text(
            &field.name,
            cap(MAX_NOTIFICATION_TITLE_BYTES),
            false,
            true,
            "fields.name",
        )?;
        check_text(
            &field.value,
            cap(MAX_NOTIFICATION_FIELD_BYTES),
            true,
            false,
            "fields.value",
        )?;
    }
    Ok(())
}

/// Checks that an alert action and an alert key come together.
///
/// # Errors
///
/// [`NotificationError::BadAlert`] when one comes without the other, or
/// [`NotificationError::BadText`] for a malformed key.
fn check_alert(
    action: NotificationAlertAction,
    key: Option<&str>,
) -> Result<(), NotificationError> {
    let key = key.filter(|k| !k.is_empty());
    match (action, key) {
        (NotificationAlertAction::None, None) => Ok(()),
        (NotificationAlertAction::None, Some(_)) => Err(NotificationError::BadAlert),
        (NotificationAlertAction::Trigger | NotificationAlertAction::Resolve, None) => {
            Err(NotificationError::BadAlert)
        }
        (NotificationAlertAction::Trigger | NotificationAlertAction::Resolve, Some(key)) => {
            check_text(
                key,
                cap(MAX_NOTIFICATION_FIELD_BYTES),
                false,
                true,
                "alertKey",
            )
        }
    }
}

/// Checks an optional group key: a non-blank single line within
/// [`MAX_NOTIFICATION_FIELD_BYTES`].
///
/// # Errors
///
/// [`NotificationError::BadText`].
fn check_group_key(key: Option<&str>) -> Result<(), NotificationError> {
    match key.filter(|k| !k.is_empty()) {
        Some(key) => check_text(
            key,
            cap(MAX_NOTIFICATION_FIELD_BYTES),
            false,
            true,
            "groupKey",
        ),
        None => Ok(()),
    }
}

/// Checks an absolute link into the library: `https://host[:port]/...`, or
/// `http://` only to `localhost`, `127.0.0.1` or `[::1]`; no credentials,
/// no whitespace or control characters, at most
/// [`MAX_NOTIFICATION_FIELD_BYTES`].
///
/// # Arguments
///
/// * `link` - The link to check.
///
/// # Errors
///
/// [`NotificationError::BadLink`].
pub fn check_link(link: &str) -> Result<(), NotificationError> {
    if link.len() > cap(MAX_NOTIFICATION_FIELD_BYTES)
        || link
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(NotificationError::BadLink);
    }
    let (secure, rest) = if let Some(rest) = link.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = link.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(NotificationError::BadLink);
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(NotificationError::BadLink);
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((inner, after)) = bracketed.split_once(']') else {
            return Err(NotificationError::BadLink);
        };
        if !(after.is_empty() || is_port_suffix(after)) || inner.is_empty() {
            return Err(NotificationError::BadLink);
        }
        inner
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if let Some(port) = port {
            if !is_port_suffix(&format!(":{port}")) {
                return Err(NotificationError::BadLink);
            }
        }
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(NotificationError::BadLink);
        }
        host
    };
    if !secure && !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return Err(NotificationError::BadLink);
    }
    Ok(())
}

/// Whether `s` is `:` followed by a port number from 1 to 65535.
fn is_port_suffix(s: &str) -> bool {
    s.strip_prefix(':').is_some_and(|p| {
        (1..=5).contains(&p.len())
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u16>().is_ok_and(|port| port != 0)
    })
}

/// Checks a path inside the library that the host will prefix with its
/// public origin: starts with one `/`, no `//` at the start, no
/// backslash, whitespace or control characters, at most
/// [`MAX_NOTIFICATION_FIELD_BYTES`].
///
/// # Arguments
///
/// * `path` - The path a raise names (`/books/123`).
///
/// # Errors
///
/// [`NotificationError::BadLinkPath`].
pub fn check_link_path(path: &str) -> Result<(), NotificationError> {
    if path.len() > cap(MAX_NOTIFICATION_FIELD_BYTES)
        || !path.starts_with('/')
        || path.starts_with("//")
        || path
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(NotificationError::BadLinkPath);
    }
    Ok(())
}

/// Checks a message the host rendered before any notifier sees it.
///
/// # Arguments
///
/// * `message` - The rendered message.
///
/// # Errors
///
/// The first [`NotificationError`] the message breaks: a bad id or type,
/// a blank or oversized title, an oversized body, a link or library origin
/// outside the rules, too many fields, or a mismatched alert.
pub fn check_message(message: &NotificationMessage) -> Result<(), NotificationError> {
    if !is_notify_id(&message.notification_id) {
        return Err(NotificationError::BadId("notificationId"));
    }
    if !is_qualified_notification_type(&message.notification_type) {
        return Err(NotificationError::BadTypeId);
    }
    check_text(
        &message.title,
        cap(MAX_NOTIFICATION_TITLE_BYTES),
        false,
        true,
        "title",
    )?;
    check_text(
        &message.text,
        cap(MAX_NOTIFICATION_TEXT_BYTES),
        true,
        false,
        "text",
    )?;
    if let Some(markdown) = &message.markdown {
        check_text(
            markdown,
            cap(MAX_NOTIFICATION_TEXT_BYTES),
            true,
            false,
            "markdown",
        )?;
    }
    if let Some(html) = &message.html {
        check_text(html, cap(MAX_NOTIFICATION_HTML_BYTES), true, false, "html")?;
    }
    if let Some(link) = message.link.as_deref().filter(|l| !l.is_empty()) {
        check_link(link)?;
    }
    if let Some(origin) = message.library_origin.as_deref().filter(|l| !l.is_empty()) {
        check_link(origin)?;
    }
    check_fields(&message.fields)?;
    check_group_key(message.group_key.as_deref())?;
    check_alert(message.alert_action, message.alert_key.as_deref())
}

/// A destination address document: a flat JSON object of string values,
/// keyed by the `[[notifier.address_fields]]` keys. Values may be secrets
/// (a push subscription's auth key); the type never prints them.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct NotifyAddress {
    /// Field key to value, sorted by key.
    fields: BTreeMap<String, String>,
}

impl std::fmt::Debug for NotifyAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyAddress")
            .field("keys", &self.fields.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl NotifyAddress {
    /// The value of field `key`, if present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// Field keys, sorted.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }

    /// Number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the document has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The canonical JSON text of the document (keys sorted, no spaces),
    /// the form the host stores and sends.
    #[must_use]
    pub fn to_json(&self) -> String {
        // A map of strings always serialises.
        serde_json::to_string(&self.fields).unwrap_or_else(|_| String::from("{}"))
    }
}

/// Parses a destination address document.
///
/// The document must be a JSON object of at most
/// [`MAX_NOTIFY_ADDRESS_KEYS`] keys, each key an identifier
/// (`[a-z][a-z0-9_]{0,63}`) appearing once, each value a string of at most
/// [`MAX_NOTIFICATION_FIELD_BYTES`] bytes with no control characters; the
/// whole text at most [`MAX_NOTIFIER_ADDRESS_BYTES`].
///
/// # Arguments
///
/// * `text` - The document as a patron or a notifier gave it.
///
/// # Returns
///
/// The parsed document.
///
/// # Errors
///
/// [`NotificationError::BadAddress`] saying what is wrong.
pub fn parse_address_document(text: &str) -> Result<NotifyAddress, NotificationError> {
    if text.len() > cap(MAX_NOTIFIER_ADDRESS_BYTES) {
        return Err(NotificationError::BadAddress("is too long"));
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| NotificationError::BadAddress("is not JSON"))?;
    let serde_json::Value::Object(object) = value else {
        return Err(NotificationError::BadAddress("is not a JSON object"));
    };
    // serde_json keeps the last of duplicate keys; refuse duplicates so
    // the stored document says exactly what was sent.
    if count_top_level_keys(text) != Some(object.len()) {
        return Err(NotificationError::BadAddress("repeats a key"));
    }
    if object.len() > MAX_NOTIFY_ADDRESS_KEYS {
        return Err(NotificationError::BadAddress("has too many fields"));
    }
    let mut fields = BTreeMap::new();
    for (key, value) in object {
        if !is_notification_type_id(&key) {
            return Err(NotificationError::BadAddress(
                "has a key that is not an identifier",
            ));
        }
        let serde_json::Value::String(value) = value else {
            return Err(NotificationError::BadAddress(
                "has a value that is not a string",
            ));
        };
        if value.len() > cap(MAX_NOTIFICATION_FIELD_BYTES) || value.chars().any(char::is_control) {
            return Err(NotificationError::BadAddress("has a malformed value"));
        }
        fields.insert(key, value);
    }
    Ok(NotifyAddress { fields })
}

/// Counts the keys of the top-level JSON object in `text` without
/// deduplicating them, or `None` when `text` is not an object.
fn count_top_level_keys(text: &str) -> Option<usize> {
    /// Visitor that counts map entries; the field is the running count.
    struct Counter(usize);
    impl<'de> serde::de::Visitor<'de> for Counter {
        type Value = usize;
        /// What the visitor accepts, for serde's error messages.
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        /// Counts every entry, duplicates included.
        fn visit_map<A: serde::de::MapAccess<'de>>(
            mut self,
            mut map: A,
        ) -> std::result::Result<usize, A::Error> {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {
                self.0 = self.0.saturating_add(1);
            }
            Ok(self.0)
        }
    }
    let mut de = serde_json::Deserializer::from_str(text);
    serde::Deserializer::deserialize_map(&mut de, Counter(0)).ok()
}

/// One address field a notifier declares in `[[notifier.address_fields]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFieldRule {
    /// Field key (`email`, `chat_id`).
    pub key: String,
    /// Whether a personal destination must carry it.
    pub required: bool,
}

/// Checks an address document against the fields the notifier declares:
/// every required field present and non-blank, no undeclared field.
///
/// # Arguments
///
/// * `address` - The parsed document.
/// * `rules` - The notifier's declared address fields.
///
/// # Errors
///
/// [`NotificationError::MissingAddressField`] or
/// [`NotificationError::UnknownAddressField`].
pub fn check_address_against_fields(
    address: &NotifyAddress,
    rules: &[AddressFieldRule],
) -> Result<(), NotificationError> {
    for key in address.keys() {
        if !rules.iter().any(|rule| rule.key == key) {
            return Err(NotificationError::UnknownAddressField(key.to_string()));
        }
    }
    for rule in rules.iter().filter(|rule| rule.required) {
        if address.get(&rule.key).is_none_or(|v| v.trim().is_empty()) {
            return Err(NotificationError::MissingAddressField(rule.key.clone()));
        }
    }
    Ok(())
}

/// Checks a `Notifier.send` request before it leaves the host.
///
/// # Arguments
///
/// * `params` - The message and destination.
///
/// # Errors
///
/// The message's [`check_message`] error, a bad destination id, or an
/// address that does not fit the destination kind.
pub fn check_send_params(params: &NotifySendParams) -> Result<(), NotificationError> {
    check_message(&params.message)?;
    let destination = &params.destination;
    if !is_notify_id(&destination.destination_id) {
        return Err(NotificationError::BadId("destinationId"));
    }
    match destination.kind {
        NotifyDestinationKind::Channel => {
            if !destination.address_json.is_empty() {
                return Err(NotificationError::AddressKindMismatch);
            }
        }
        NotifyDestinationKind::Personal => {
            if destination.address_json.is_empty() {
                return Err(NotificationError::AddressKindMismatch);
            }
            parse_address_document(&destination.address_json)?;
        }
    }
    Ok(())
}

/// Checks a `Notifier.validateDestination` answer: a well-formed address
/// document and an optional short single-line label.
///
/// # Arguments
///
/// * `validated` - The notifier's answer.
///
/// # Returns
///
/// The parsed, normalised document.
///
/// # Errors
///
/// [`NotificationError::BadAddress`] or [`NotificationError::BadText`].
pub fn check_validated(validated: &NotifyValidated) -> Result<NotifyAddress, NotificationError> {
    let address = parse_address_document(&validated.address_json)?;
    if address.is_empty() {
        return Err(NotificationError::BadAddress("is empty"));
    }
    if let Some(label) = &validated.label {
        check_text(
            label,
            cap(MAX_NOTIFICATION_TITLE_BYTES),
            false,
            false,
            "label",
        )?;
    }
    Ok(address)
}

/// The sans-IO half of the host's patron-scope check on a raise: the
/// library user a raise's `patron_id` names, which the host then looks up
/// as a holder of the invocation's store account.
///
/// A raise outside every account scope is refused here: an invocation with
/// no account (an operator invocation) can name no patron, and a patron id
/// must be a user id written canonically (a positive decimal with no sign,
/// no leading zero and no other character) that fits an `i64`, so one user
/// is never named two ways.
///
/// # Arguments
///
/// * `invocation_account` - `Invocation.accountId`; empty for an operator
///   invocation.
/// * `patron_id` - The raise's patron id.
///
/// # Returns
///
/// The user id to look up, or `None` to refuse the raise.
#[must_use]
pub fn scoped_patron_id(invocation_account: &str, patron_id: &str) -> Option<i64> {
    if invocation_account.is_empty()
        || patron_id.is_empty()
        || patron_id.starts_with('0')
        || !patron_id.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    patron_id.parse().ok()
}

/// Checks a raise a plugin sent on its `NOTIFY` binding. Whether the type
/// is one the plugin declared, and whether the patron is inside the
/// invocation's account scope, are the host's checks, since only it knows
/// the manifest and the scope.
///
/// # Arguments
///
/// * `params` - The raise.
///
/// # Errors
///
/// The first [`NotificationError`] the raise breaks.
pub fn check_raise_params(params: &NotifyRaiseParams) -> Result<(), NotificationError> {
    if !is_notification_type_id(&params.notification_type) {
        return Err(NotificationError::BadTypeId);
    }
    check_fields(&params.fields)?;
    if let Some(patron) = params.patron_id.as_deref().filter(|p| !p.is_empty()) {
        if !is_notify_id(patron) {
            return Err(NotificationError::BadId("patronId"));
        }
    }
    if let Some(path) = params.link_path.as_deref().filter(|p| !p.is_empty()) {
        check_link_path(path)?;
    }
    check_group_key(params.group_key.as_deref())?;
    check_alert(params.alert_action, params.alert_key.as_deref())
}

/// What the host does with one destination after a `Notifier.send` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// Delivered; the service's id for the message, when it gave a usable
    /// one.
    Delivered {
        /// Service message id, at most [`MAX_NOTIFICATION_FIELD_BYTES`] of
        /// printable text; dropped when malformed.
        provider_id: Option<String>,
    },
    /// Try again later.
    Retry {
        /// How long the service asked for, capped at
        /// [`MAX_NOTIFY_RETRY_AFTER`]; `None` uses the job's backoff.
        after: Option<Duration>,
        /// Short single-line reason for the delivery log.
        reason: String,
    },
    /// Give up on this destination.
    Failed {
        /// Short single-line reason for the delivery log.
        reason: String,
    },
}

/// Turns a `Notifier.send` reply into what the host does next.
///
/// `rate_limited`, `unavailable`, `deadline_exceeded` and `cancelled` are
/// retried, `rate_limited` after the
/// service's `retryAfterMs` when it gave one; every other error is a
/// permanent failure for this destination. A delivered reply with a
/// malformed provider id is still delivered, without the id.
///
/// # Arguments
///
/// * `reply` - The decoded reply.
///
/// # Returns
///
/// The outcome, with any reason cut by [`sanitize_reason`].
#[must_use]
pub fn classify_send_reply(reply: &Result<NotifySent, PluginError>) -> SendOutcome {
    match reply {
        Ok(sent) => SendOutcome::Delivered {
            provider_id: sent
                .provider_id
                .as_deref()
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= cap(MAX_NOTIFICATION_FIELD_BYTES)
                        && !id.chars().any(|c| c.is_control() || is_layout_char(c))
                })
                .map(str::to_string),
        },
        Err(err) => {
            let reason = sanitize_reason(&format!("{}: {}", err.code, err.message));
            match err.code {
                PluginErrorCode::RateLimited => SendOutcome::Retry {
                    after: err
                        .retry_after()
                        .map(|wait| wait.min(MAX_NOTIFY_RETRY_AFTER)),
                    reason,
                },
                // The service was down, the call ran out of time, or the
                // host cancelled it (shutdown): none is the message's fault.
                PluginErrorCode::Unavailable
                | PluginErrorCode::DeadlineExceeded
                | PluginErrorCode::Cancelled => SendOutcome::Retry {
                    after: None,
                    reason,
                },
                PluginErrorCode::InvalidParams
                | PluginErrorCode::Unauthorized
                | PluginErrorCode::Forbidden
                | PluginErrorCode::NotFound
                | PluginErrorCode::Unsupported
                | PluginErrorCode::Internal
                | PluginErrorCode::PayloadTooLarge
                | PluginErrorCode::InvalidCursor
                | PluginErrorCode::Conflict
                | PluginErrorCode::LimitExceeded
                // The destination waits on operator approval; retrying
                // before then cannot succeed.
                | PluginErrorCode::NetworkNotGranted
                | PluginErrorCode::Unknown => SendOutcome::Failed { reason },
            }
        }
    }
}

/// Cuts a notifier's error text to one line of at most
/// [`MAX_NOTIFY_REASON_BYTES`] bytes: control characters become spaces and
/// the cut falls on a character boundary.
///
/// # Arguments
///
/// * `reason` - The text to cut.
///
/// # Returns
///
/// The cut text.
#[must_use]
pub fn sanitize_reason(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len().min(MAX_NOTIFY_REASON_BYTES));
    for c in reason.chars() {
        let c = if c.is_control() || is_layout_char(c) {
            ' '
        } else {
            c
        };
        if out.len() + c.len_utf8() > MAX_NOTIFY_REASON_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// Cuts text to one line of at most `cap` bytes: control, line-separator
/// and bidi characters become spaces, and the text ends before the first
/// character that would pass the cap. The same rule as [`sanitize_reason`],
/// at another cap.
///
/// # Arguments
///
/// * `text` - Text from a plugin.
/// * `cap` - Most bytes kept.
///
/// # Returns
///
/// The cut text.
#[must_use]
pub fn cut_line(text: &str, cap: usize) -> String {
    let mut out = String::with_capacity(text.len().min(cap));
    for c in text.chars() {
        let c = if c.is_control() || is_layout_char(c) {
            ' '
        } else {
            c
        };
        if out.len() + c.len_utf8() > cap {
            break;
        }
        out.push(c);
    }
    out
}

/// Whether a report link may be shown to the operator as a link: an
/// absolute `https` URL with a host, no credentials, no space or control
/// character, at most [`MAX_NOTIFICATION_FIELD_BYTES`].
///
/// # Arguments
///
/// * `url` - The link a plugin gave.
#[must_use]
pub fn is_report_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    url.len() <= cap(MAX_NOTIFICATION_FIELD_BYTES)
        && !authority.is_empty()
        && !authority.contains('@')
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || is_layout_char(c) || c == '\\')
}

/// Checks a notifier's deliverability report before the operator sees it:
/// at most [`MAX_DELIVERABILITY_CHECKS`] checks and
/// [`MAX_DELIVERABILITY_ADVICE`] pieces of advice; every text cut to one
/// line at its cap ([`cut_line`]); a report link kept only when
/// [`is_report_url`] accepts it; advice marked `skipped` shown as a
/// `warning`; and at most [`MAX_DELIVERABILITY_CHECKS`] sources per piece
/// of advice.
///
/// # Arguments
///
/// * `report` - The plugin's report.
///
/// # Returns
///
/// The report as the operator sees it.
///
/// # Errors
///
/// [`NotificationError::TooManyDeliverabilityEntries`] when a list is over
/// its cap.
pub fn check_deliverability_report(
    report: NotifyDeliverabilityReport,
) -> Result<NotifyDeliverabilityReport, NotificationError> {
    if report.checks.len() > cap(MAX_DELIVERABILITY_CHECKS)
        || report.advice.len() > cap(MAX_DELIVERABILITY_ADVICE)
    {
        return Err(NotificationError::TooManyDeliverabilityEntries);
    }
    let title = cap(MAX_NOTIFICATION_TITLE_BYTES);
    let field = cap(MAX_NOTIFICATION_FIELD_BYTES);
    Ok(NotifyDeliverabilityReport {
        checks: report
            .checks
            .into_iter()
            .map(|check| NotifyDeliverabilityCheck {
                service: cut_line(&check.service, title),
                outcome: check.outcome,
                summary: cut_line(&check.summary, field),
                report_url: check.report_url.filter(|url| is_report_url(url)),
            })
            .collect(),
        advice: report
            .advice
            .into_iter()
            .map(|advice| NotifyDeliverabilityAdvice {
                outcome: match advice.outcome {
                    NotifyCheckOutcome::Skipped => NotifyCheckOutcome::Warning,
                    other @ (NotifyCheckOutcome::Passed
                    | NotifyCheckOutcome::Warning
                    | NotifyCheckOutcome::Failed) => other,
                },
                title: cut_line(&advice.title, title),
                detail: cut_line(&advice.detail, field),
                sources: advice
                    .sources
                    .iter()
                    .take(cap(MAX_DELIVERABILITY_CHECKS))
                    .map(|source| cut_line(source, title))
                    .collect(),
            })
            .collect(),
    })
}

/// Rank of a severity for comparing against a rule's minimum: info 0,
/// warning 1, critical 2.
///
/// # Arguments
///
/// * `severity` - The severity to rank.
///
/// # Returns
///
/// Its rank.
#[must_use]
pub fn severity_rank(severity: NotificationSeverity) -> u8 {
    match severity {
        NotificationSeverity::Info => 0,
        NotificationSeverity::Warning => 1,
        NotificationSeverity::Critical => 2,
    }
}

/// Reads a message from `bytes` with a traversal limit proportional to its
/// size, so a crafted message cannot make the reader walk more than it
/// sent.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
fn read_message(
    bytes: &[u8],
) -> capnp::Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)
}

/// Decodes a `NotifySendReply` from an unpacked Cap'n Proto message, the
/// form a notifier's reply crosses the plugin boundary in.
///
/// # Arguments
///
/// * `bytes` - The message bytes.
///
/// # Returns
///
/// The reply: delivered, or the notifier's typed error.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
pub fn decode_notify_send_reply_bytes(
    bytes: &[u8],
) -> capnp::Result<Result<NotifySent, PluginError>> {
    let reader = read_message(bytes)?;
    crate::read_notify_send_reply(
        reader.get_root::<crate::plugin_capnp::notify_send_reply::Reader<'_>>()?,
    )
}

/// Encodes a `NotifySendReply`, the inverse of
/// [`decode_notify_send_reply_bytes`].
///
/// # Arguments
///
/// * `reply` - The reply to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_notify_send_reply_bytes(
    reply: &Result<NotifySent, PluginError>,
) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_notify_send_reply(
        message.init_root::<crate::plugin_capnp::notify_send_reply::Builder<'_>>(),
        reply,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

/// Decodes a `NotifyDeliverabilityReply` from an unpacked Cap'n Proto
/// message, the form a notifier's report crosses the plugin boundary in.
///
/// # Arguments
///
/// * `bytes` - The message bytes.
///
/// # Returns
///
/// The report, or the notifier's typed error.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
pub fn decode_notify_deliverability_reply_bytes(
    bytes: &[u8],
) -> capnp::Result<Result<NotifyDeliverabilityReport, PluginError>> {
    let reader = read_message(bytes)?;
    crate::read_notify_deliverability_reply(
        reader.get_root::<crate::plugin_capnp::notify_deliverability_reply::Reader<'_>>()?,
    )
}

/// Encodes a `NotifyDeliverabilityReply`, the inverse of
/// [`decode_notify_deliverability_reply_bytes`].
///
/// # Arguments
///
/// * `reply` - The reply to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_notify_deliverability_reply_bytes(
    reply: &Result<NotifyDeliverabilityReport, PluginError>,
) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_notify_deliverability_reply(
        message.init_root::<crate::plugin_capnp::notify_deliverability_reply::Builder<'_>>(),
        reply,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

/// Decodes `NotifyRaiseParams` from an unpacked Cap'n Proto message, the
/// form a plugin's raise crosses the plugin boundary in.
///
/// # Arguments
///
/// * `bytes` - The message bytes.
///
/// # Returns
///
/// The raise.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
pub fn decode_notify_raise_params_bytes(bytes: &[u8]) -> capnp::Result<NotifyRaiseParams> {
    let reader = read_message(bytes)?;
    crate::read_notify_raise_params(
        reader.get_root::<crate::plugin_capnp::notify_raise_params::Reader<'_>>()?,
    )
}

/// Encodes `NotifyRaiseParams`, the inverse of
/// [`decode_notify_raise_params_bytes`].
///
/// # Arguments
///
/// * `params` - The raise to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_notify_raise_params_bytes(params: &NotifyRaiseParams) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_notify_raise_params(
        message.init_root::<crate::plugin_capnp::notify_raise_params::Builder<'_>>(),
        params,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::NotifyDestination;

    fn message() -> NotificationMessage {
        NotificationMessage {
            notification_id: "n_01".into(),
            notification_type: "job_failed".into(),
            severity: NotificationSeverity::Warning,
            title: "A job failed".into(),
            text: "Scan of Audible failed.\nIt will retry.".into(),
            link: Some("https://books.example.com/jobs/12".into()),
            fields: vec![NotificationField {
                name: "Job".into(),
                value: "scan".into(),
            }],
            ..NotificationMessage::default()
        }
    }

    #[test]
    fn a_good_message_passes() {
        check_message(&message()).expect("good");
    }

    #[test]
    fn messages_outside_the_rules_are_refused() {
        let mut m = message();
        m.title = "two\nlines".into();
        assert_eq!(check_message(&m), Err(NotificationError::BadText("title")));
        let mut m = message();
        m.title = " ".into();
        assert_eq!(check_message(&m), Err(NotificationError::BadText("title")));
        let mut m = message();
        m.notification_type = "Bad".into();
        assert_eq!(check_message(&m), Err(NotificationError::BadTypeId));
        let mut m = message();
        m.notification_id = "has space".into();
        assert!(matches!(
            check_message(&m),
            Err(NotificationError::BadId(_))
        ));
        let mut m = message();
        m.text = "x".repeat(cap(MAX_NOTIFICATION_TEXT_BYTES) + 1);
        assert_eq!(check_message(&m), Err(NotificationError::BadText("text")));
        let mut m = message();
        m.fields = vec![NotificationField::default(); cap(MAX_NOTIFICATION_FIELDS) + 1];
        assert_eq!(check_message(&m), Err(NotificationError::TooManyFields));
        let mut m = message();
        m.alert_action = NotificationAlertAction::Trigger;
        assert_eq!(check_message(&m), Err(NotificationError::BadAlert));
        m.alert_key = Some("disk-low".into());
        check_message(&m).expect("trigger with key");
        m.alert_action = NotificationAlertAction::None;
        assert_eq!(check_message(&m), Err(NotificationError::BadAlert));
    }

    #[test]
    fn plugin_types_are_qualified() {
        assert!(is_qualified_notification_type("job_failed"));
        assert!(is_qualified_notification_type("ntfy_relay:sync_failed"));
        assert!(!is_qualified_notification_type("x:sync"));
        assert!(!is_qualified_notification_type("a:b:c"));
        assert!(!is_qualified_notification_type("Plugin:sync"));
        assert!(!is_qualified_notification_type(""));
    }

    #[test]
    fn links_must_point_at_an_https_origin() {
        for good in [
            "https://books.example.com",
            "https://books.example.com:8443/a?b#c",
            "http://localhost:8787/",
            "http://127.0.0.1/x",
            "http://[::1]:8787/x",
            "https://[2001:db8::1]/x",
        ] {
            check_link(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
        for bad in [
            "http://books.example.com/",
            "https://user:pw@books.example.com/",
            "javascript:alert(1)",
            "https:///path",
            "https://books.example.com/a b",
            "https://bo\\oks/",
            "https://books.example.com:99999999/",
            "https://books.example.com:65536/",
            "https://books.example.com:0/",
            "//books.example.com",
            "https://[::1/x",
        ] {
            assert_eq!(check_link(bad), Err(NotificationError::BadLink), "{bad}");
        }
    }

    #[test]
    fn link_paths_stay_inside_the_library() {
        check_link_path("/books/1").expect("good");
        for bad in ["books", "//evil.example", "/a b", "/a\\b", ""] {
            assert_eq!(check_link_path(bad), Err(NotificationError::BadLinkPath));
        }
    }

    #[test]
    fn address_documents_are_flat_string_objects() {
        let address = parse_address_document(r#"{"email":"a@example.com"}"#).expect("good");
        assert_eq!(address.get("email"), Some("a@example.com"));
        assert_eq!(address.to_json(), r#"{"email":"a@example.com"}"#);
        assert!(!format!("{address:?}").contains("example"));
        for bad in [
            "[]",
            "\"x\"",
            r#"{"email":1}"#,
            r#"{"Email":"x"}"#,
            r#"{"a":"x","a":"y"}"#,
            r#"{"a":{"b":"c"}}"#,
            r#"{"a":"line\nbreak"}"#,
            "not json",
        ] {
            assert!(
                matches!(
                    parse_address_document(bad),
                    Err(NotificationError::BadAddress(_))
                ),
                "{bad}"
            );
        }
        let many: serde_json::Map<String, serde_json::Value> = (0..=MAX_NOTIFY_ADDRESS_KEYS)
            .map(|i| (format!("k{i}"), serde_json::Value::from("v")))
            .collect();
        let text = serde_json::Value::Object(many).to_string();
        assert!(parse_address_document(&text).is_err());
    }

    #[test]
    fn addresses_match_declared_fields() {
        let rules = vec![
            AddressFieldRule {
                key: "chat_id".into(),
                required: true,
            },
            AddressFieldRule {
                key: "thread".into(),
                required: false,
            },
        ];
        let ok = parse_address_document(r#"{"chat_id":"42"}"#).expect("parse");
        check_address_against_fields(&ok, &rules).expect("good");
        let missing = parse_address_document(r#"{"thread":"1"}"#).expect("parse");
        assert_eq!(
            check_address_against_fields(&missing, &rules),
            Err(NotificationError::MissingAddressField("chat_id".into()))
        );
        let unknown = parse_address_document(r#"{"chat_id":"1","x":"y"}"#).expect("parse");
        assert_eq!(
            check_address_against_fields(&unknown, &rules),
            Err(NotificationError::UnknownAddressField("x".into()))
        );
    }

    #[test]
    fn send_params_match_the_destination_kind() {
        let mut params = NotifySendParams {
            message: message(),
            destination: NotifyDestination {
                kind: NotifyDestinationKind::Channel,
                destination_id: "d_1".into(),
                address_json: String::new(),
            },
        };
        check_send_params(&params).expect("channel");
        params.destination.address_json = r#"{"email":"a@b"}"#.into();
        assert_eq!(
            check_send_params(&params),
            Err(NotificationError::AddressKindMismatch)
        );
        params.destination.kind = NotifyDestinationKind::Personal;
        check_send_params(&params).expect("personal");
        params.destination.address_json = String::new();
        assert_eq!(
            check_send_params(&params),
            Err(NotificationError::AddressKindMismatch)
        );
    }

    #[test]
    fn replies_classify_into_outcomes() {
        assert_eq!(
            classify_send_reply(&Ok(NotifySent {
                provider_id: Some("abc".into())
            })),
            SendOutcome::Delivered {
                provider_id: Some("abc".into())
            }
        );
        assert_eq!(
            classify_send_reply(&Ok(NotifySent {
                provider_id: Some("a\nb".into())
            })),
            SendOutcome::Delivered { provider_id: None }
        );
        let limited = PluginError::rate_limited("slow down", Some(Duration::from_secs(7200)));
        assert!(matches!(
            classify_send_reply(&Err(limited)),
            SendOutcome::Retry { after: Some(after), .. } if after == MAX_NOTIFY_RETRY_AFTER
        ));
        assert!(matches!(
            classify_send_reply(&Err(PluginError::unavailable("down"))),
            SendOutcome::Retry { after: None, .. }
        ));
        for code in [
            PluginErrorCode::DeadlineExceeded,
            PluginErrorCode::Cancelled,
        ] {
            assert!(
                matches!(
                    classify_send_reply(&Err(PluginError::new(code, "stopped"))),
                    SendOutcome::Retry { after: None, .. }
                ),
                "{code} is retried"
            );
        }
        assert!(matches!(
            classify_send_reply(&Err(PluginError::new(
                PluginErrorCode::Unauthorized,
                "bad token"
            ))),
            SendOutcome::Failed { .. }
        ));
    }

    #[test]
    fn reasons_are_short_single_lines() {
        let cut = sanitize_reason(&format!("a\nb{}", "é".repeat(400)));
        assert!(cut.starts_with("a b"));
        assert!(cut.len() <= MAX_NOTIFY_REASON_BYTES);
        assert!(!cut.contains('\n'));
        assert_eq!(sanitize_reason("ok\u{2028}fake\u{202E}x"), "ok fake x");
    }

    #[test]
    fn one_line_texts_refuse_separators_and_bidi() {
        let mut m = message();
        m.title = "Done\u{2028}Not done".into();
        assert_eq!(check_message(&m), Err(NotificationError::BadText("title")));
        m.title = "\u{202E}enod".into();
        assert_eq!(check_message(&m), Err(NotificationError::BadText("title")));
        m.title = "Done".into();
        m.text = "line one\u{2028}line two".into();
        check_message(&m).expect("a body may use line separators");
        m.text = "\u{2066}x\u{2069}".into();
        assert_eq!(check_message(&m), Err(NotificationError::BadText("text")));
    }

    #[test]
    fn raises_are_checked() {
        let mut raise = NotifyRaiseParams {
            notification_type: "sync_failed".into(),
            link_path: Some("/accounts".into()),
            ..NotifyRaiseParams::default()
        };
        check_raise_params(&raise).expect("good");
        raise.notification_type = "other:sync_failed".into();
        assert_eq!(
            check_raise_params(&raise),
            Err(NotificationError::BadTypeId)
        );
        raise.notification_type = "sync_failed".into();
        raise.link_path = Some("https://evil.example".into());
        assert_eq!(
            check_raise_params(&raise),
            Err(NotificationError::BadLinkPath)
        );
    }

    #[test]
    fn wire_roundtrips() {
        let reply: Result<NotifySent, PluginError> = Err(PluginError::rate_limited(
            "slow",
            Some(Duration::from_millis(1500)),
        ));
        let bytes = encode_notify_send_reply_bytes(&reply).expect("encode");
        let back = decode_notify_send_reply_bytes(&bytes).expect("decode");
        assert_eq!(
            back.expect_err("err").retry_after(),
            Some(Duration::from_millis(1500))
        );
        let raise = NotifyRaiseParams {
            notification_type: "sync_failed".into(),
            patron_id: Some("u_1".into()),
            ..NotifyRaiseParams::default()
        };
        let bytes = encode_notify_raise_params_bytes(&raise).expect("encode");
        assert_eq!(
            decode_notify_raise_params_bytes(&bytes).expect("decode"),
            raise
        );
    }
}
