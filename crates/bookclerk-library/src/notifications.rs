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
//! - **Account** is the host's own mail and is not routed here yet.
//!
//! A paused destination is skipped.

use bookclerk_plugin_abi::notifications::{check_link_path, check_message, NotificationError};
use bookclerk_plugin_abi::{
    NotificationAlertAction, NotificationAudience, NotificationField, NotificationMessage,
    NotificationSeverity, NotifyDestinationKind,
};
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
    /// The notifier instance the delivery goes through; `None` exactly
    /// when `delivery_id` is.
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
