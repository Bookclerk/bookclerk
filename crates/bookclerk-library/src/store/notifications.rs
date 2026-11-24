//! Notification routing, destinations, the delivery log and the inbox
//! ([`crate::notifications`] holds the rules).
//!
//! Every write is one atomic host batch through the canonical SQL-v1
//! contract. The router reads the routes, offers and choices first and then
//! writes the notification, its delivery-log rows and its inbox row in one
//! batch, so a notification is either stored with all its deliveries or not
//! at all. Each delivery's `notify_send` job is enqueued after that batch;
//! a delivery whose enqueue failed keeps `job_id` NULL and the daemon's
//! sweep enqueues it later ([`LibraryStore::unscheduled_notify_deliveries`]).

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_plugin_abi::notifications::{is_layout_char, sanitize_reason};
use bookclerk_plugin_abi::{
    DbValue, NotificationAlertAction, NotificationAudience, NotificationField,
    NotificationSeverity, TypedDbStatement,
};
use chrono::{DateTime, Duration, Utc};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::LibraryStore;
use crate::control_plane::{batch, bump_if_any, LiveTopic};
use crate::entities::{
    account_links, jobs, notifications, notify_deliveries, notify_destinations, notify_inbox,
    notify_inbox_routes, notify_patron_offers, notify_patron_prefs, notify_patron_targets,
    notify_route_revisions, notify_routes, portal_identities, users,
};
use crate::error::{EntityKind, LibraryError, Result};
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, JobFence, JobKind, JobPayload, JobState, JobTrigger,
};
use crate::notifications::{
    severity_reaches, DeliveryRecord, DeliveryState, Destination, DestinationKind, FailedAttempt,
    FailureEffect, InboxEntry, NewDestination, NewNotification, PatronChoice, PatronOffer,
    PendingDelivery, Route, Routed, SendWork, StallOutcome, StalledSend, StoredNotification,
    IN_APP_DESTINATION, MAX_DESTINATION_LABEL_BYTES, MAX_ROUTES_PER_TYPE,
    NOTIFICATION_RETENTION_DAYS, PAUSE_AFTER_FAILURES,
};

/// Queue limits a `notify_send` admission uses (the daemon's `[jobs]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifyQueueLimits {
    /// Global cap on pending and running jobs.
    pub max_pending: i64,
    /// Claims before a failing send job stops.
    pub max_attempts: i64,
}

/// Most rows one page of the delivery log or the inbox returns.
pub const MAX_NOTIFY_PAGE: u64 = 200;

/// Where a page of the delivery log or an inbox starts: the last row of the
/// previous page (newest first).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyCursor {
    /// RFC 3339 creation time of the last row read.
    pub created_at: String,
    /// Id of the last row read (the notification id for an inbox).
    pub id: String,
}

impl NotifyCursor {
    /// The opaque token a page answer carries.
    #[must_use]
    pub fn encode(&self) -> String {
        use base64::Engine as _;
        // Serializing a struct of strings cannot fail.
        let json = serde_json::to_vec(self).unwrap_or_default();
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    /// Reads a token made by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] when the token is not one: not base64url,
    /// not the JSON shape, or a field over 128 bytes.
    pub fn decode(token: &str) -> Result<Self> {
        use base64::Engine as _;
        let invalid = || LibraryError::Invalid("invalid page cursor".into());
        if token.len() > 512 {
            return Err(invalid());
        }
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.as_bytes())
            .map_err(|_| invalid())?;
        // A struct also reads from a JSON array; a cursor is only ever the
        // object `encode` writes.
        if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
            return Err(invalid());
        }
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        // SQL-v1 refuses U+0000 in a bound text value.
        if cursor.created_at.len() > 128
            || cursor.id.len() > 128
            || cursor.created_at.contains('\0')
            || cursor.id.contains('\0')
        {
            return Err(invalid());
        }
        Ok(cursor)
    }
}

/// The stored spelling of an audience.
fn audience_str(audience: NotificationAudience) -> &'static str {
    audience.wire_name()
}

/// Parses a stored audience, failing on a value the CHECK list forbids.
fn parse_audience(s: &str) -> Result<NotificationAudience> {
    NotificationAudience::from_wire_name(s)
        .ok_or_else(|| LibraryError::Invalid(format!("stored audience `{s}` is unknown")))
}

/// Parses a stored severity.
fn parse_severity(s: &str) -> Result<NotificationSeverity> {
    NotificationSeverity::from_wire_name(s)
        .ok_or_else(|| LibraryError::Invalid(format!("stored severity `{s}` is unknown")))
}

/// Parses a stored alert action.
fn parse_alert(s: &str) -> Result<NotificationAlertAction> {
    NotificationAlertAction::from_wire_name(s)
        .ok_or_else(|| LibraryError::Invalid(format!("stored alert action `{s}` is unknown")))
}

/// Parses a stored delivery state.
fn parse_state(s: &str) -> Result<DeliveryState> {
    DeliveryState::parse(s)
        .ok_or_else(|| LibraryError::Invalid(format!("stored delivery state `{s}` is unknown")))
}

/// The send job's lease as a predicate on one outcome write: with a fence,
/// the write applies only while that claim still holds the job, so a send
/// that lost its lease (whose delivery a replacement job may have
/// reclaimed) cannot record an outcome. `None`, for a caller outside a job,
/// adds nothing. `sql` must end with its `WHERE` clause.
fn fenced(sql: &str, mut parameters: Vec<DbValue>, fence: Option<&JobFence>) -> TypedDbStatement {
    match fence {
        Some(fence) => {
            parameters.push(batch::text(&fence.job_id));
            parameters.push(batch::text(&fence.owner));
            parameters.push(batch::int(fence.generation));
            batch::exec(
                &format!(
                    "{sql} AND EXISTS (SELECT 1 FROM jobs WHERE id = ? AND state = 'running' \
                     AND lease_owner = ? AND lease_generation = ?)"
                ),
                parameters,
            )
        }
        None => batch::exec(sql, parameters),
    }
}

/// A destination from its row.
fn destination(row: notify_destinations::Model) -> Result<Destination> {
    let kind = DestinationKind::parse(&row.kind).ok_or_else(|| {
        LibraryError::Invalid(format!("stored destination kind `{}` is unknown", row.kind))
    })?;
    Ok(Destination {
        id: row.id,
        plugin_instance_id: row.plugin_instance_id,
        kind,
        user_id: row.user_id,
        label: row.label,
        address_json: row.address_json,
        failure_count: row.failure_count,
        paused_at: row.paused_at,
        paused_reason: row.paused_reason,
        created_at: row.created_at,
    })
}

/// A stored notification from its row.
fn stored(row: notifications::Model) -> Result<StoredNotification> {
    let fields: Vec<NotificationField> = serde_json::from_str(&row.fields_json)
        .map_err(|err| LibraryError::Invalid(format!("stored notification fields: {err}")))?;
    Ok(StoredNotification {
        id: row.id,
        notification: NewNotification {
            notification_type: row.notification_type,
            audience: parse_audience(&row.audience)?,
            severity: parse_severity(&row.severity)?,
            title: row.title,
            text: row.body_text,
            link_path: row.link_path,
            fields,
            group_key: row.group_key,
            alert_action: parse_alert(&row.alert_action)?,
            alert_key: row.alert_key,
            source: row.source,
            patron: row.user_id,
        },
        created_at: row.created_at,
    })
}

/// Checks a destination label: one non-blank line within the cap.
fn check_label(label: &str) -> Result<()> {
    if label.trim().is_empty()
        || label.len() > MAX_DESTINATION_LABEL_BYTES
        || label.chars().any(|c| c.is_control() || is_layout_char(c))
    {
        return Err(LibraryError::Invalid(format!(
            "a destination label is one line of 1 to {MAX_DESTINATION_LABEL_BYTES} bytes"
        )));
    }
    Ok(())
}

/// `created_at < ? OR (created_at = ? AND id < ?)` for a newest-first page.
fn before_cursor<C: ColumnTrait>(created: C, id: C, cursor: &NotifyCursor) -> Condition {
    Condition::any()
        .add(created.lt(cursor.created_at.as_str()))
        .add(
            Condition::all()
                .add(created.eq(cursor.created_at.as_str()))
                .add(id.lt(cursor.id.as_str())),
        )
}

/// A page size clamped to `1..=MAX_NOTIFY_PAGE`.
fn page(limit: u64) -> u64 {
    limit.clamp(1, MAX_NOTIFY_PAGE)
}

/// What the routing rules picked for one notification.
#[derive(Debug, Default)]
struct RouteTargets {
    /// Active destinations, each once.
    destinations: Vec<Destination>,
    /// Whether an "In app" route puts it in every active Owner's and
    /// Administrator's inbox.
    staff_inbox: bool,
}

/// A patron's choice for one notification type, as routing reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PatronPick {
    /// No choice: the type's default-on notifiers apply.
    Default,
    /// The patron turned the type off: inbox only.
    Off,
    /// The patron picked these destination ids (none means inbox only).
    Chosen(BTreeSet<String>),
}

/// How one listing read is sized under the database's result-row cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageWindow {
    /// Rows the page holds: the asked size, clamped, and never more than
    /// the cap.
    size: u64,
    /// Rows the first read asks for: one past `size` (the look-ahead that
    /// says another page follows) when the cap leaves room, else `size`.
    read: u64,
}

impl PageWindow {
    /// The window for a page of `limit` rows under a cap of `cap` rows per
    /// read (at least 1).
    fn new(limit: u64, cap: u64) -> Self {
        let size = page(limit).min(cap.max(1));
        let read = size.saturating_add(1).min(cap.max(1));
        Self { size, read }
    }

    /// Whether a full page read without its look-ahead row needs a
    /// one-row probe past its last row to learn whether more follow.
    fn needs_probe(self, got: usize, more: bool) -> bool {
        !more && self.read == self.size && u64::try_from(got).is_ok_and(|n| n == self.size)
    }
}

/// One page of a newest-first listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyPage<T> {
    /// The page's rows, at most the page size asked for.
    pub items: Vec<T>,
    /// Whether at least one row follows the last one: only then does the
    /// listing hand out a cursor, so a full final page ends the walk.
    pub more: bool,
}

/// Splits `rows`, read with one row beyond `size`, into a page.
fn split_page<T>(mut rows: Vec<T>, size: u64) -> (Vec<T>, bool) {
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    let more = rows.len() > size;
    rows.truncate(size);
    (rows, more)
}

impl LibraryStore {
    /// The read window for a listing page of `limit` rows under this
    /// database's result-row cap.
    fn page_window(&self, limit: u64) -> PageWindow {
        PageWindow::new(limit, self.result_row_cap())
    }

    /// Accepts a notification: stores it, writes one delivery-log row per
    /// destination the routing rules pick and, for a patron, the inbox row,
    /// in one batch; then enqueues one `notify_send` job per delivery.
    ///
    /// # Arguments
    ///
    /// * `new` - The notification, already rendered.
    /// * `limits` - The queue limits the send jobs are admitted under.
    ///
    /// # Returns
    ///
    /// The stored id and the deliveries made.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] when the notification breaks a message
    /// rule; [`LibraryError::NotFound`] when its patron does not exist;
    /// [`LibraryError::Conflict`] when route replacements kept landing while
    /// it was routed; an error when a read or the batch fails. A failed
    /// enqueue is not an error: the delivery waits for the sweep.
    pub async fn route_notification(
        &self,
        new: &NewNotification,
        limits: NotifyQueueLimits,
    ) -> Result<Routed> {
        let id = uuid::Uuid::new_v4().to_string();
        new.check(&id)
            .map_err(|err| LibraryError::Invalid(format!("notification: {err}")))?;
        if let Some(patron) = new.patron {
            self.require_user(patron).await?;
        }
        let targets = self.route_targets(new).await?;
        self.store_notification(id, new, &targets, limits).await
    }

    /// Stores `new` with one delivery to destination `destination_id`
    /// alone, whatever the routes say: an administrator's test of a
    /// destination.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] when the notification breaks a message
    /// rule; [`LibraryError::NotFound`] when the destination does not
    /// exist; an error when a read or the batch fails.
    pub async fn route_notification_to(
        &self,
        new: &NewNotification,
        destination_id: &str,
        limits: NotifyQueueLimits,
    ) -> Result<Routed> {
        let id = uuid::Uuid::new_v4().to_string();
        new.check(&id)
            .map_err(|err| LibraryError::Invalid(format!("notification: {err}")))?;
        let target = self
            .notify_destination(destination_id)
            .await?
            .ok_or_else(|| {
                LibraryError::not_found(EntityKind::NotifyDestination, destination_id)
            })?;
        let targets = RouteTargets {
            destinations: vec![target],
            staff_inbox: false,
        };
        self.store_notification(id, new, &targets, limits).await
    }

    /// Writes notification `id`, its inbox rows and one delivery per target
    /// in one batch, then enqueues the send jobs.
    async fn store_notification(
        &self,
        id: String,
        new: &NewNotification,
        targets: &RouteTargets,
        limits: NotifyQueueLimits,
    ) -> Result<Routed> {
        let now = super::now_str();
        let fields_json = serde_json::to_string(&new.fields)
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("notification fields: {err}")))?;
        let mut statements = vec![batch::exec(
            "INSERT INTO notifications (id, notification_type, audience, severity, title, \
             body_text, link_path, fields_json, group_key, alert_action, alert_key, source, \
             user_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                batch::text(&id),
                batch::text(&new.notification_type),
                batch::text(audience_str(new.audience)),
                batch::text(new.severity.wire_name()),
                batch::text(&new.title),
                batch::text(&new.text),
                batch::text_opt(new.link_path.as_deref().filter(|p| !p.is_empty())),
                batch::text(&fields_json),
                batch::text_opt(new.group_key.as_deref().filter(|g| !g.is_empty())),
                batch::text(new.alert_action.wire_name()),
                batch::text_opt(new.alert_key.as_deref().filter(|k| !k.is_empty())),
                batch::text(&new.source),
                batch::int_opt(new.patron),
                batch::text(&now),
            ],
        )];
        let inboxed = match (new.audience, new.patron) {
            (NotificationAudience::Patron, Some(patron)) => {
                statements.push(batch::exec(
                    "INSERT INTO notify_inbox (user_id, notification_id, read_at, created_at) \
                     VALUES (?, ?, NULL, ?)",
                    vec![batch::int(patron), batch::text(&id), batch::text(&now)],
                ));
                true
            }
            _ => false,
        };
        let inboxed = if targets.staff_inbox {
            // Every active Owner and Administrator, read in the same batch
            // so a role change between the route read and this write cannot
            // inbox someone who no longer holds the role.
            statements.push(batch::exec(
                "INSERT INTO notify_inbox (user_id, notification_id, read_at, created_at) \
                 SELECT id, ?, NULL, ? FROM users \
                 WHERE role IN ('owner', 'administrator') AND status = 'active'",
                vec![batch::text(&id), batch::text(&now)],
            ));
            true
        } else {
            inboxed
        };
        if inboxed {
            statements.push(bump_if_any(
                LiveTopic::Notifications,
                batch::exec(
                    "SELECT 1 FROM notify_inbox WHERE notification_id = ?",
                    vec![batch::text(&id)],
                ),
            ));
        }
        let mut deliveries = Vec::with_capacity(targets.destinations.len());
        for target in &targets.destinations {
            let delivery = PendingDelivery {
                id: uuid::Uuid::new_v4().to_string(),
                plugin_instance_id: target.plugin_instance_id.clone(),
            };
            statements.push(batch::exec(
                "INSERT INTO notify_deliveries (id, notification_id, destination_id, \
                 plugin_instance_id, state, attempts, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, 'pending', 0, ?, ?)",
                vec![
                    batch::text(&delivery.id),
                    batch::text(&id),
                    batch::text(&target.id),
                    batch::text(&delivery.plugin_instance_id),
                    batch::text(&now),
                    batch::text(&now),
                ],
            ));
            deliveries.push(delivery);
        }
        self.execute_host_batch(batch::request(&format!("notify-route-{id}"), statements))
            .await?;
        for delivery in &deliveries {
            if let Err(err) = self.enqueue_notify_send(delivery, limits).await {
                tracing::warn!(
                    delivery = %delivery.id,
                    error = %err,
                    "notify_send enqueue failed; the sweep retries it"
                );
            }
        }
        Ok(Routed {
            notification_id: id,
            deliveries,
            inboxed,
        })
    }

    /// What a patron chose for `notification_type`: the preference and its
    /// targets are two reads, so the preference is read again after the
    /// targets and the pair is read afresh (up to three times) when a
    /// concurrent change or clear moved it, so routing follows one
    /// complete configuration rather than an enabled preference with
    /// another revision's targets.
    ///
    /// # Errors
    ///
    /// An error when a read fails.
    async fn patron_pick(&self, patron: i64, notification_type: &str) -> Result<PatronPick> {
        let read_pref = || {
            notify_patron_prefs::Entity::find()
                .filter(notify_patron_prefs::Column::UserId.eq(patron))
                .filter(notify_patron_prefs::Column::NotificationType.eq(notification_type))
                .one(&self.db)
        };
        let mut pref = read_pref().await.map_err(LibraryError::Orm)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let pick = match &pref {
                None => PatronPick::Default,
                Some(p) if p.enabled == 0 => PatronPick::Off,
                Some(_) => PatronPick::Chosen(
                    crate::store::keyset::read_keyset(
                        self,
                        notify_patron_targets::Entity::find()
                            .filter(notify_patron_targets::Column::UserId.eq(patron))
                            .filter(
                                notify_patron_targets::Column::NotificationType
                                    .eq(notification_type),
                            ),
                        &[notify_patron_targets::Column::DestinationId],
                    )
                    .await?
                    .into_iter()
                    .map(|t| t.destination_id)
                    .collect(),
                ),
            };
            if !matches!(pick, PatronPick::Chosen(_)) || attempt >= 3 {
                return Ok(pick);
            }
            let again = read_pref().await.map_err(LibraryError::Orm)?;
            if again == pref {
                return Ok(pick);
            }
            pref = again;
        }
    }

    /// The active destinations the routing rules pick for `new`, each once,
    /// and whether an "In app" route puts it in the staff inbox.
    ///
    /// # Errors
    ///
    /// An error when a read fails, and [`LibraryError::Conflict`] when route
    /// replacements kept landing between the reads.
    async fn route_targets(&self, new: &NewNotification) -> Result<RouteTargets> {
        let mut out: BTreeMap<String, Destination> = BTreeMap::new();
        let mut staff_inbox = false;
        match (new.audience, new.patron) {
            (NotificationAudience::Operational, _) => {
                // The channel routes and the In app route are two reads. A
                // replacement writes both, and a fresh revision, in one
                // batch; the revision is read before and after them, and
                // equal revisions (never reused) mean no replacement landed
                // between the reads. Routing follows one complete
                // configuration: after three tries that each saw a
                // replacement, the notification is refused rather than
                // routed by a mixed pair.
                let read_revision = || {
                    notify_route_revisions::Entity::find_by_id(new.notification_type.clone())
                        .one(&self.db)
                };
                let mut attempt = 0;
                let (routes, in_app) = loop {
                    attempt += 1;
                    let before = read_revision().await.map_err(LibraryError::Orm)?;
                    let routes = crate::store::keyset::read_keyset(
                        self,
                        notify_routes::Entity::find().filter(
                            notify_routes::Column::NotificationType.eq(&new.notification_type),
                        ),
                        &[notify_routes::Column::DestinationId],
                    )
                    .await?;
                    let in_app =
                        notify_inbox_routes::Entity::find_by_id(new.notification_type.clone())
                            .one(&self.db)
                            .await
                            .map_err(LibraryError::Orm)?;
                    let after = read_revision().await.map_err(LibraryError::Orm)?;
                    if before == after {
                        break (routes, in_app);
                    }
                    if attempt >= 3 {
                        return Err(LibraryError::Conflict(
                            "the notification routes changed while this notification was routed"
                                .into(),
                        ));
                    }
                };
                let reaches = |min: &str| {
                    NotificationSeverity::from_wire_name(min)
                        .is_some_and(|min| severity_reaches(new.severity, min))
                };
                let wanted: Vec<String> = routes
                    .into_iter()
                    .filter(|r| reaches(&r.min_severity))
                    .map(|r| r.destination_id)
                    .collect();
                staff_inbox = in_app.is_some_and(|r| reaches(&r.min_severity));
                if !wanted.is_empty() {
                    for row in super::library_model::find_chunked::<notify_destinations::Entity, _>(
                        self,
                        &wanted,
                        |chunk| {
                            notify_destinations::Entity::find()
                                .filter(notify_destinations::Column::Id.is_in(chunk.to_vec()))
                                .filter(notify_destinations::Column::Kind.eq("channel"))
                                .filter(notify_destinations::Column::PausedAt.is_null())
                        },
                    )
                    .await?
                    {
                        let dest = destination(row)?;
                        out.insert(dest.id.clone(), dest);
                    }
                }
            }
            (NotificationAudience::Patron, Some(patron)) => {
                let offers = crate::store::keyset::read_keyset(
                    self,
                    notify_patron_offers::Entity::find().filter(
                        notify_patron_offers::Column::NotificationType.eq(&new.notification_type),
                    ),
                    &[notify_patron_offers::Column::PluginInstanceId],
                )
                .await?;
                if offers.is_empty() {
                    return Ok(RouteTargets::default());
                }
                let offered: BTreeSet<String> = offers
                    .iter()
                    .map(|o| o.plugin_instance_id.clone())
                    .collect();
                let defaults: BTreeSet<String> = offers
                    .iter()
                    .filter(|o| o.default_on != 0)
                    .map(|o| o.plugin_instance_id.clone())
                    .collect();
                let personal = crate::store::keyset::read_keyset(
                    self,
                    notify_destinations::Entity::find()
                        .filter(notify_destinations::Column::UserId.eq(patron))
                        .filter(notify_destinations::Column::Kind.eq("personal"))
                        .filter(notify_destinations::Column::PausedAt.is_null()),
                    &[notify_destinations::Column::Id],
                )
                .await?;
                let picked = match self.patron_pick(patron, &new.notification_type).await? {
                    PatronPick::Off => return Ok(RouteTargets::default()),
                    PatronPick::Default => None,
                    PatronPick::Chosen(picked) => Some(picked),
                };
                for row in personal {
                    let dest = destination(row)?;
                    if !offered.contains(&dest.plugin_instance_id) {
                        continue;
                    }
                    let take = match &picked {
                        Some(picked) => picked.contains(&dest.id),
                        None => defaults.contains(&dest.plugin_instance_id),
                    };
                    if take {
                        out.insert(dest.id.clone(), dest);
                    }
                }
            }
            // `check` refuses a patron audience without a patron; account
            // mail is enqueued by its own flow, not routed.
            (NotificationAudience::Patron, None) | (NotificationAudience::Account, _) => {}
        }
        Ok(RouteTargets {
            destinations: out.into_values().collect(),
            staff_inbox,
        })
    }

    /// Fails with [`LibraryError::NotFound`] unless user `id` exists.
    async fn require_user(&self, id: i64) -> Result<()> {
        let found = users::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        match found {
            Some(_) => Ok(()),
            None => Err(LibraryError::not_found(EntityKind::User, id)),
        }
    }

    /// Whether user `user_id` holds store account (`plugin_key`,
    /// `account_id`) through one of their linked identities: the check a
    /// plugin's patron raise must pass inside its invocation's account
    /// scope.
    ///
    /// # Errors
    ///
    /// An error when a read fails.
    pub async fn user_holds_account(
        &self,
        user_id: i64,
        plugin_key: &str,
        account_id: &str,
    ) -> Result<bool> {
        let identities: Vec<i64> = crate::store::keyset::read_keyset(
            self,
            portal_identities::Entity::find().filter(portal_identities::Column::UserId.eq(user_id)),
            &[portal_identities::Column::Id],
        )
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();
        if identities.is_empty() {
            return Ok(false);
        }
        let link = account_links::Entity::find()
            .filter(account_links::Column::IdentityId.is_in(identities))
            .filter(account_links::Column::PluginKey.eq(plugin_key))
            .filter(account_links::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(link.is_some())
    }

    /// The notifications with the given ids, read in chunks that fit the
    /// backend's bind and result limits.
    ///
    /// # Errors
    ///
    /// An error when a read fails.
    async fn notifications_by_id(&self, ids: &[String]) -> Result<Vec<notifications::Model>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        super::library_model::find_chunked::<notifications::Entity, _>(self, ids, |chunk| {
            notifications::Entity::find().filter(notifications::Column::Id.is_in(chunk.to_vec()))
        })
        .await
    }

    /// Enqueues the `notify_send` job of one delivery and records its id.
    ///
    /// # Returns
    ///
    /// The job id, or `None` when the queue is full (the sweep tries again).
    ///
    /// # Errors
    ///
    /// An error when the enqueue or the write fails.
    pub async fn enqueue_notify_send(
        &self,
        delivery: &PendingDelivery,
        limits: NotifyQueueLimits,
    ) -> Result<Option<String>> {
        let outcome = self
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::NotifySend,
                payload: JobPayload {
                    plugin_instance_id: Some(delivery.plugin_instance_id.clone()),
                    requires: vec![delivery.plugin_instance_id.clone()],
                    trigger: JobTrigger::Scheduler,
                    notify_delivery: Some(delivery.id.clone()),
                    ..JobPayload::default()
                },
                priority: 0,
                max_attempts: limits.max_attempts.max(1),
                max_pending: limits.max_pending,
                run_after: None,
            })
            .await?;
        let job_id = match outcome {
            EnqueueOutcome::Created { id } | EnqueueOutcome::Duplicate { existing_id: id } => id,
            EnqueueOutcome::QueueFull => return Ok(None),
        };
        // A fresh operation id: the first enqueue and a sweep can both link
        // the same job, and the second write must not be refused as a
        // reused operation with another request.
        self.execute_host_batch(batch::request(
            &format!("notify-job-{}-{}", delivery.id, uuid::Uuid::new_v4()),
            vec![batch::exec(
                "UPDATE notify_deliveries SET job_id = ?, updated_at = ? \
                 WHERE id = ? AND state = 'pending'",
                vec![
                    batch::text(&job_id),
                    batch::text(&super::now_str()),
                    batch::text(&delivery.id),
                ],
            )],
        ))
        .await?;
        Ok(Some(job_id))
    }

    /// Pending deliveries no send job was recorded for, oldest first: the
    /// ones whose enqueue failed or found the queue full.
    ///
    /// # Errors
    ///
    /// An error when the read fails.
    pub async fn unscheduled_notify_deliveries(&self, limit: u64) -> Result<Vec<PendingDelivery>> {
        Ok(notify_deliveries::Entity::find()
            .filter(notify_deliveries::Column::State.eq(DeliveryState::Pending.as_str()))
            .filter(notify_deliveries::Column::JobId.is_null())
            .order_by_asc(notify_deliveries::Column::CreatedAt)
            .order_by_asc(notify_deliveries::Column::Id)
            .limit(page(limit).min(self.result_row_cap()))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(|row| PendingDelivery {
                id: row.id,
                plugin_instance_id: row.plugin_instance_id,
            })
            .collect())
    }

    /// Drops pending deliveries whose send job ended without recording a
    /// result (an operator cancelled it, or it failed on a host error), so
    /// the sweep does not send them again behind the operator's back and
    /// the delivery log says what happened.
    ///
    /// # Returns
    ///
    /// How many deliveries were dropped.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn drop_orphaned_notify_deliveries(&self) -> Result<u64> {
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-orphans-{}", uuid::Uuid::new_v4()),
                vec![batch::exec(
                    "UPDATE notify_deliveries SET state = 'dropped', \
                     reason = 'the send job ended without a result', updated_at = ? \
                     WHERE state = 'pending' AND job_id IS NOT NULL AND job_id NOT IN \
                     (SELECT id FROM jobs WHERE state IN ('pending', 'running'))",
                    vec![batch::text(&super::now_str())],
                )],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0))
    }

    /// One page of send jobs still pending although they have been ready
    /// to run since before `ready_before`, with the pending delivery and
    /// notifier instance of each when it still has one. Whether a job is
    /// stalled or only waiting behind other work depends on whether its
    /// notifier runs anywhere, which the caller decides.
    ///
    /// Pending send jobs are walked by id, `after` the last id of the
    /// previous page (`None` for the first), at most `limit` rows a page;
    /// the second value is the cursor of the next page, `None` after the
    /// last. Only the id, readiness, lease generation and dedup key columns
    /// are read, so a page stays narrow under the database's result-size
    /// cap, and a page the database still refuses for its size is read
    /// again at half the rows. Readiness is compared here rather than in
    /// SQL because stored timestamps carry a varying number of fractional
    /// digits, so they do not order as text.
    ///
    /// The delivery comes from the job's dedup key (it names the delivery
    /// the job was enqueued for), not from the delivery's `job_id`, so a
    /// job whose link write failed is still matched with its delivery.
    ///
    /// # Errors
    ///
    /// An error when a read fails.
    pub async fn stalled_notify_sends(
        &self,
        ready_before: DateTime<Utc>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<(Vec<StalledSend>, Option<String>)> {
        /// The columns of a pending send job the stall check needs.
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct PendingSendRow {
            /// Job id.
            id: String,
            /// When the job became ready to run, as stored.
            run_after: String,
            /// Lease generation when read.
            lease_generation: i64,
            /// Dedup key; names the delivery the job sends.
            dedup_key: Option<String>,
        }
        let mut rows = page(limit).min(self.result_row_cap());
        let page_rows = loop {
            let mut query = jobs::Entity::find()
                .select_only()
                .column(jobs::Column::Id)
                .column(jobs::Column::RunAfter)
                .column(jobs::Column::LeaseGeneration)
                .column(jobs::Column::DedupKey)
                .filter(jobs::Column::Kind.eq(JobKind::NotifySend.as_str()))
                .filter(jobs::Column::State.eq(JobState::Pending.as_str()));
            if let Some(after) = after {
                query = query.filter(jobs::Column::Id.gt(after));
            }
            match query
                .order_by_asc(jobs::Column::Id)
                .limit(rows)
                .into_model::<PendingSendRow>()
                .all(&self.db)
                .await
            {
                Ok(found) => break found,
                Err(err)
                    if rows > 1
                        && bookclerk_plugin_abi::LimitExceeded::from_message(&err.to_string())
                            .is_some() =>
                {
                    rows /= 2;
                }
                Err(err) => return Err(LibraryError::Orm(err)),
            }
        };
        let next = if u64::try_from(page_rows.len()).unwrap_or(u64::MAX) < rows {
            None
        } else {
            page_rows.last().map(|row| row.id.clone())
        };
        let stalled: Vec<PendingSendRow> = page_rows
            .into_iter()
            .filter(|job| {
                DateTime::parse_from_rfc3339(&job.run_after)
                    .is_ok_and(|ready| ready.with_timezone(&Utc) < ready_before)
            })
            .collect();
        if stalled.is_empty() {
            return Ok((Vec::new(), next));
        }
        let prefix = crate::models::notify_send_dedup_key("");
        let delivery_ids: Vec<String> = stalled
            .iter()
            .filter_map(|job| job.dedup_key.as_deref()?.strip_prefix(prefix.as_str()))
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let deliveries: BTreeMap<String, notify_deliveries::Model> =
            super::library_model::find_chunked::<notify_deliveries::Entity, _>(
                self,
                &delivery_ids,
                |chunk| {
                    notify_deliveries::Entity::find()
                        .filter(notify_deliveries::Column::Id.is_in(chunk.to_vec()))
                        .filter(
                            notify_deliveries::Column::State.eq(DeliveryState::Pending.as_str()),
                        )
                },
            )
            .await?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        Ok((
            stalled
                .into_iter()
                .map(|job| {
                    // A delivery linked to another job is not this job's.
                    let delivery = job
                        .dedup_key
                        .as_deref()
                        .and_then(|key| key.strip_prefix(prefix.as_str()))
                        .and_then(|id| deliveries.get(id))
                        .filter(|d| d.job_id.as_deref().is_none_or(|linked| linked == job.id));
                    StalledSend {
                        delivery_id: delivery.map(|d| d.id.clone()),
                        notifier: delivery.map(|d| d.plugin_instance_id.clone()),
                        job_id: job.id,
                        run_after: job.run_after,
                        lease_generation: job.lease_generation,
                    }
                })
                .collect(),
            next,
        ))
    }

    /// Cancels one stalled send job and fails its pending delivery in a
    /// single atomic write, so a claim racing the sweep either wins (and
    /// nothing changes) or loses (and the failure is counted toward the
    /// destination's pause), and the cancel never lands without the count.
    /// A delivery whose destination is already paused is dropped instead,
    /// with no attempt or failure counted, as a send would drop it. Only
    /// call this for a job whose notifier runs nowhere; a job that is only
    /// waiting for a worker is not a notifier failure.
    ///
    /// # Returns
    ///
    /// What happened; [`StallOutcome::NotStalled`] when the job was
    /// claimed, retried or ended since it was read.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn fail_stalled_notify_send(
        &self,
        stalled: &StalledSend,
        reason: &str,
    ) -> Result<StallOutcome> {
        let reason = sanitize_reason(reason);
        let now = super::now_str();
        let mut statements = vec![batch::exec(
            "UPDATE jobs SET state = 'cancelled', cancel_requested = 1, \
             error_kind = 'notify_stalled', error_message = ?, lease_owner = NULL, \
             lease_expires_at = NULL, finished_at = ?, updated_at = ? \
             WHERE id = ? AND state = 'pending' AND run_after = ? AND lease_generation = ?",
            vec![
                batch::text(&reason),
                batch::text(&now),
                batch::text(&now),
                batch::text(&stalled.job_id),
                batch::text(&stalled.run_after),
                batch::int(stalled.lease_generation),
            ],
        )];
        // Each delivery write applies only when this batch's own cancel
        // took (the job now carries this commit's stamp), and only to a
        // pending delivery that is this job's: linked to it, or not linked
        // to any job yet (its link write failed).
        let gate = "EXISTS (SELECT 1 FROM jobs WHERE id = ? AND state = 'cancelled' \
                    AND error_kind = 'notify_stalled' AND updated_at = ?)";
        let ours = "state = 'pending' AND (job_id IS NULL OR job_id = ?)";
        if let Some(delivery_id) = &stalled.delivery_id {
            // A paused destination's delivery is dropped, as a send would
            // drop it: no attempt and no failure are counted.
            statements.push(batch::exec(
                &format!(
                    "UPDATE notify_deliveries SET state = 'dropped', reason = ?, updated_at = ? \
                     WHERE id = ? AND {ours} AND destination_id IN (SELECT id FROM \
                     notify_destinations WHERE paused_at IS NOT NULL) AND {gate}"
                ),
                vec![
                    batch::text("the destination is paused"),
                    batch::text(&now),
                    batch::text(delivery_id),
                    batch::text(&stalled.job_id),
                    batch::text(&stalled.job_id),
                    batch::text(&now),
                ],
            ));
            // The rest only sees the delivery while it is still pending,
            // so after the drop above none of it applies.
            statements.push(batch::exec(
                &format!(
                    "UPDATE notify_destinations SET failure_count = failure_count + 1, \
                     updated_at = ? WHERE paused_at IS NULL AND id IN (SELECT destination_id \
                     FROM notify_deliveries WHERE id = ? AND {ours}) AND {gate}"
                ),
                vec![
                    batch::text(&now),
                    batch::text(delivery_id),
                    batch::text(&stalled.job_id),
                    batch::text(&stalled.job_id),
                    batch::text(&now),
                ],
            ));
            statements.push(batch::exec(
                &format!(
                    "UPDATE notify_destinations SET paused_at = ?, paused_reason = ? \
                     WHERE paused_at IS NULL AND failure_count >= ? AND id IN \
                     (SELECT destination_id FROM notify_deliveries \
                     WHERE id = ? AND {ours}) AND {gate}"
                ),
                vec![
                    batch::text(&now),
                    batch::text(&reason),
                    batch::int(PAUSE_AFTER_FAILURES),
                    batch::text(delivery_id),
                    batch::text(&stalled.job_id),
                    batch::text(&stalled.job_id),
                    batch::text(&now),
                ],
            ));
            statements.push(batch::exec(
                &format!(
                    "UPDATE notify_deliveries SET state = 'failed', attempts = attempts + 1, \
                     reason = ?, updated_at = ? WHERE id = ? AND {ours} AND {gate}"
                ),
                vec![
                    batch::text(&reason),
                    batch::text(&now),
                    batch::text(delivery_id),
                    batch::text(&stalled.job_id),
                    batch::text(&stalled.job_id),
                    batch::text(&now),
                ],
            ));
        }
        statements.push(crate::control_plane::bump_if_stamped(
            crate::control_plane::LiveTopic::Jobs,
            &now,
        )?);
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-stalled-{}-{}", stalled.job_id, uuid::Uuid::new_v4()),
                statements,
            ))
            .await?;
        if batch::rows_affected(&reply, 0) != 1 {
            return Ok(StallOutcome::NotStalled);
        }
        if stalled.delivery_id.is_some() && batch::rows_affected(&reply, 1) == 1 {
            return Ok(StallOutcome::Dropped);
        }
        if stalled.delivery_id.is_some() && batch::rows_affected(&reply, 4) == 1 {
            return Ok(StallOutcome::Failed(
                if batch::rows_affected(&reply, 3) == 1 {
                    FailureEffect::Paused
                } else {
                    FailureEffect::Counted
                },
            ));
        }
        Ok(StallOutcome::Cancelled)
    }

    /// Drops a pending delivery no send job is linked to when a send job
    /// for it already ended: its enqueue succeeded but the link write did
    /// not, and the job was then cancelled or failed. Without this the
    /// sweep would queue a second send behind the operator's back, since
    /// the ended job no longer blocks a new one with the same dedup key.
    ///
    /// # Returns
    ///
    /// Whether the delivery was dropped (and must not be enqueued again).
    ///
    /// # Errors
    ///
    /// An error when the read or the batch fails.
    pub async fn drop_notify_delivery_if_job_ended(&self, delivery_id: &str) -> Result<bool> {
        let ended = jobs::Entity::find()
            .filter(jobs::Column::DedupKey.eq(crate::models::notify_send_dedup_key(delivery_id)))
            .filter(
                jobs::Column::State
                    .is_not_in([JobState::Pending.as_str(), JobState::Running.as_str()]),
            )
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some();
        if !ended {
            return Ok(false);
        }
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-unlinked-{delivery_id}-{}", uuid::Uuid::new_v4()),
                vec![batch::exec(
                    "UPDATE notify_deliveries SET state = 'dropped', \
                     reason = 'the send job ended without a result', updated_at = ? \
                     WHERE id = ? AND state = 'pending' AND job_id IS NULL",
                    vec![batch::text(&super::now_str()), batch::text(delivery_id)],
                )],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0) == 1)
    }

    /// Everything the send job of `delivery_id` needs; `None` when the row
    /// is gone (pruned, or its notification or destination removed).
    ///
    /// # Errors
    ///
    /// An error when a read fails or a stored value is malformed.
    pub async fn notify_send_work(&self, delivery_id: &str) -> Result<Option<SendWork>> {
        let Some(row) = notify_deliveries::Entity::find_by_id(delivery_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let Some(note) = notifications::Entity::find_by_id(&row.notification_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let dest = notify_destinations::Entity::find_by_id(&row.destination_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(destination)
            .transpose()?;
        Ok(Some(SendWork {
            delivery_id: row.id,
            state: parse_state(&row.state)?,
            attempts: row.attempts,
            plugin_instance_id: row.plugin_instance_id,
            notification: stored(note)?,
            destination: dest,
        }))
    }

    /// Records a delivered send: the delivery is done and its destination's
    /// failure count is cleared.
    ///
    /// # Returns
    ///
    /// `false` when the delivery was no longer pending (a replayed job).
    ///
    /// # Arguments
    ///
    /// * `fence` - The send job's claim; the write is skipped (and answers
    ///   as if the delivery were no longer pending) once that claim has lost
    ///   the job.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn record_notify_delivered(
        &self,
        delivery_id: &str,
        provider_id: Option<&str>,
        fence: Option<&JobFence>,
    ) -> Result<bool> {
        let now = super::now_str();
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-delivered-{delivery_id}-{}", uuid::Uuid::new_v4()),
                vec![
                    fenced(
                        "UPDATE notify_destinations SET failure_count = 0, updated_at = ? \
                         WHERE id IN (SELECT destination_id FROM notify_deliveries \
                         WHERE id = ? AND state = 'pending')",
                        vec![batch::text(&now), batch::text(delivery_id)],
                        fence,
                    ),
                    fenced(
                        "UPDATE notify_deliveries SET state = 'delivered', \
                         attempts = attempts + 1, provider_id = ?, reason = NULL, updated_at = ? \
                         WHERE id = ? AND state = 'pending'",
                        vec![
                            batch::text_opt(provider_id),
                            batch::text(&now),
                            batch::text(delivery_id),
                        ],
                        fence,
                    ),
                ],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 1) == 1)
    }

    /// Records a send attempt the notifier asked to retry.
    ///
    /// # Returns
    ///
    /// The attempts made, this one included; `None` when the delivery was
    /// no longer pending.
    ///
    /// # Arguments
    ///
    /// * `fence` - The send job's claim; the write is skipped (and answers
    ///   as if the delivery were no longer pending) once that claim has lost
    ///   the job.
    ///
    /// # Errors
    ///
    /// An error when the batch or the read fails.
    pub async fn record_notify_retry(
        &self,
        delivery_id: &str,
        reason: &str,
        fence: Option<&JobFence>,
    ) -> Result<Option<i64>> {
        let reason = sanitize_reason(reason);
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-retry-{delivery_id}-{}", uuid::Uuid::new_v4()),
                vec![fenced(
                    "UPDATE notify_deliveries SET attempts = attempts + 1, reason = ?, \
                     updated_at = ? WHERE id = ? AND state = 'pending'",
                    vec![
                        batch::text(&reason),
                        batch::text(&super::now_str()),
                        batch::text(delivery_id),
                    ],
                    fence,
                )],
            ))
            .await?;
        if batch::rows_affected(&reply, 0) != 1 {
            return Ok(None);
        }
        Ok(notify_deliveries::Entity::find_by_id(delivery_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|row| row.attempts))
    }

    /// Records a send the notifier refused for good (or that ran out of
    /// attempts): the delivery fails and its destination's failure count
    /// rises, pausing it at [`PAUSE_AFTER_FAILURES`].
    ///
    /// # Returns
    ///
    /// Whether this failure paused the destination; `None` when the
    /// delivery was no longer pending.
    ///
    /// # Arguments
    ///
    /// * `fence` - The send job's claim; the write is skipped (and answers
    ///   as if the delivery were no longer pending) once that claim has lost
    ///   the job.
    /// * `attempt` - Whether this answer spends an attempt of its own.
    ///
    /// # Errors
    ///
    /// An error when the batch or the read fails.
    pub async fn record_notify_failed(
        &self,
        delivery_id: &str,
        reason: &str,
        attempt: FailedAttempt,
        fence: Option<&JobFence>,
    ) -> Result<Option<FailureEffect>> {
        let reason = sanitize_reason(reason);
        let now = super::now_str();
        let spent = match attempt {
            FailedAttempt::New => 1,
            FailedAttempt::AlreadyCounted => 0,
        };
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-failed-{delivery_id}-{}", uuid::Uuid::new_v4()),
                vec![
                    fenced(
                        "UPDATE notify_destinations SET failure_count = failure_count + 1, \
                         updated_at = ? WHERE id IN (SELECT destination_id FROM \
                         notify_deliveries WHERE id = ? AND state = 'pending')",
                        vec![batch::text(&now), batch::text(delivery_id)],
                        fence,
                    ),
                    fenced(
                        "UPDATE notify_destinations SET paused_at = ?, paused_reason = ? \
                         WHERE paused_at IS NULL AND failure_count >= ? AND id IN \
                         (SELECT destination_id FROM notify_deliveries \
                         WHERE id = ? AND state = 'pending')",
                        vec![
                            batch::text(&now),
                            batch::text(&reason),
                            batch::int(PAUSE_AFTER_FAILURES),
                            batch::text(delivery_id),
                        ],
                        fence,
                    ),
                    fenced(
                        "UPDATE notify_deliveries SET state = 'failed', \
                         attempts = attempts + ?, reason = ?, updated_at = ? \
                         WHERE id = ? AND state = 'pending'",
                        vec![
                            batch::int(spent),
                            batch::text(&reason),
                            batch::text(&now),
                            batch::text(delivery_id),
                        ],
                        fence,
                    ),
                ],
            ))
            .await?;
        if batch::rows_affected(&reply, 2) != 1 {
            return Ok(None);
        }
        Ok(Some(if batch::rows_affected(&reply, 1) == 1 {
            FailureEffect::Paused
        } else {
            FailureEffect::Counted
        }))
    }

    /// Records a delivery that was not sent (its destination is paused or
    /// gone, or its notifier instance is gone). The destination's failure
    /// count is untouched.
    ///
    /// # Returns
    ///
    /// `false` when the delivery was no longer pending.
    ///
    /// # Arguments
    ///
    /// * `fence` - The send job's claim; the write is skipped (and answers
    ///   as if the delivery were no longer pending) once that claim has lost
    ///   the job.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn record_notify_dropped(
        &self,
        delivery_id: &str,
        reason: &str,
        fence: Option<&JobFence>,
    ) -> Result<bool> {
        let reason = sanitize_reason(reason);
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-dropped-{delivery_id}-{}", uuid::Uuid::new_v4()),
                vec![fenced(
                    "UPDATE notify_deliveries SET state = 'dropped', reason = ?, updated_at = ? \
                     WHERE id = ? AND state = 'pending'",
                    vec![
                        batch::text(&reason),
                        batch::text(&super::now_str()),
                        batch::text(delivery_id),
                    ],
                    fence,
                )],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0) == 1)
    }

    /// Creates a destination.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] for a bad label, a channel with an owner or
    /// an address, a personal destination without an owner, or one past
    /// the owner's [`MAX_ROUTES_PER_TYPE`] addresses;
    /// [`LibraryError::NotFound`] when the owner does not exist; an error
    /// when the batch fails.
    pub async fn create_notify_destination(&self, new: &NewDestination) -> Result<Destination> {
        check_label(&new.label)?;
        if new.plugin_instance_id.trim().is_empty() {
            return Err(LibraryError::Invalid(
                "a destination names its notifier instance".into(),
            ));
        }
        match (new.kind, new.user_id) {
            (DestinationKind::Channel, None) if new.address_json.is_empty() => {}
            (DestinationKind::Channel, _) => {
                return Err(LibraryError::Invalid(
                    "a channel destination has no owner and no address".into(),
                ));
            }
            (DestinationKind::Personal, Some(user)) => {
                self.require_user(user).await?;
            }
            (DestinationKind::Personal, None) => {
                return Err(LibraryError::Invalid(
                    "a personal destination belongs to a patron".into(),
                ));
            }
        }
        // A stored address must read back on every database: refuse one
        // past the address limit or the advertised cell limit instead of
        // storing a row that later reads fail on.
        let cell_cap = self.db_capabilities().max_cell_bytes;
        let max = match usize::try_from(cell_cap) {
            Ok(0) | Err(_) => crate::notifications::MAX_ADDRESS_BYTES,
            Ok(cap) => cap.min(crate::notifications::MAX_ADDRESS_BYTES),
        };
        if new.address_json.len() > max {
            return Err(LibraryError::Invalid(format!(
                "the address is {} bytes; this library stores at most {max}",
                new.address_json.len()
            )));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = super::now_str();
        // A patron's default fanout is every one of their addresses on a
        // default-on notifier, written in the routing batch with the
        // notification and inbox rows: the cap keeps that batch within the
        // strictest database's statement limit. The count is part of the
        // insert, so two requests racing for the last slot cannot both
        // take it. A channel's owner is NULL, which no count matches.
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-destination-{id}"),
                vec![batch::exec(
                    "INSERT INTO notify_destinations (id, plugin_instance_id, kind, user_id, \
                     label, address_json, failure_count, created_at, updated_at) \
                     SELECT ?, ?, ?, ?, ?, ?, 0, ?, ? WHERE ? = 'channel' OR \
                     (SELECT COUNT(*) FROM notify_destinations \
                     WHERE kind = 'personal' AND user_id = ?) < ?",
                    vec![
                        batch::text(&id),
                        batch::text(&new.plugin_instance_id),
                        batch::text(new.kind.as_str()),
                        batch::int_opt(new.user_id),
                        batch::text(new.label.trim()),
                        batch::text(&new.address_json),
                        batch::text(&now),
                        batch::text(&now),
                        batch::text(new.kind.as_str()),
                        batch::int_opt(new.user_id),
                        batch::int(i64::try_from(MAX_ROUTES_PER_TYPE).unwrap_or(i64::MAX)),
                    ],
                )],
            ))
            .await?;
        if batch::rows_affected(&reply, 0) != 1 {
            return Err(LibraryError::Invalid(format!(
                "a patron keeps at most {MAX_ROUTES_PER_TYPE} notification addresses"
            )));
        }
        // Built from what was written rather than read back, so a failed
        // read after the commit cannot answer an error for a destination
        // that exists (and invite a retry that makes a second one).
        Ok(Destination {
            id,
            plugin_instance_id: new.plugin_instance_id.clone(),
            kind: new.kind,
            user_id: new.user_id,
            label: new.label.trim().to_string(),
            address_json: new.address_json.clone(),
            failure_count: 0,
            paused_at: None,
            paused_reason: None,
            created_at: now,
        })
    }

    /// One destination by id.
    ///
    /// # Errors
    ///
    /// An error when the read fails or the row is malformed.
    pub async fn notify_destination(&self, id: &str) -> Result<Option<Destination>> {
        notify_destinations::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(destination)
            .transpose()
    }

    /// Every library channel, oldest first.
    ///
    /// # Errors
    ///
    /// An error when the read fails.
    pub async fn notify_channels(&self) -> Result<Vec<Destination>> {
        crate::store::keyset::read_keyset(
            self,
            notify_destinations::Entity::find()
                .filter(notify_destinations::Column::Kind.eq(DestinationKind::Channel.as_str())),
            &[
                notify_destinations::Column::CreatedAt,
                notify_destinations::Column::Id,
            ],
        )
        .await?
        .into_iter()
        .map(destination)
        .collect()
    }

    /// One patron's personal destinations, oldest first.
    ///
    /// # Errors
    ///
    /// An error when the read fails.
    pub async fn personal_destinations(&self, user_id: i64) -> Result<Vec<Destination>> {
        crate::store::keyset::read_keyset(
            self,
            notify_destinations::Entity::find()
                .filter(notify_destinations::Column::Kind.eq(DestinationKind::Personal.as_str()))
                .filter(notify_destinations::Column::UserId.eq(user_id)),
            &[
                notify_destinations::Column::CreatedAt,
                notify_destinations::Column::Id,
            ],
        )
        .await?
        .into_iter()
        .map(destination)
        .collect()
    }

    /// Renames a destination.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] for a bad label;
    /// [`LibraryError::NotFound`] when the destination does not exist.
    pub async fn rename_notify_destination(&self, id: &str, label: &str) -> Result<()> {
        check_label(label)?;
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-rename-{id}-{}", uuid::Uuid::new_v4()),
                vec![batch::exec(
                    "UPDATE notify_destinations SET label = ?, updated_at = ? WHERE id = ?",
                    vec![
                        batch::text(label.trim()),
                        batch::text(&super::now_str()),
                        batch::text(id),
                    ],
                )],
            ))
            .await?;
        if batch::rows_affected(&reply, 0) == 0 {
            return Err(LibraryError::not_found(EntityKind::NotifyDestination, id));
        }
        Ok(())
    }

    /// Resumes a paused destination and clears its failure count.
    ///
    /// # Errors
    ///
    /// [`LibraryError::NotFound`] when the destination does not exist.
    pub async fn resume_notify_destination(&self, id: &str) -> Result<()> {
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-resume-{id}-{}", uuid::Uuid::new_v4()),
                vec![batch::exec(
                    "UPDATE notify_destinations SET paused_at = NULL, paused_reason = NULL, \
                     failure_count = 0, updated_at = ? WHERE id = ?",
                    vec![batch::text(&super::now_str()), batch::text(id)],
                )],
            ))
            .await?;
        if batch::rows_affected(&reply, 0) == 0 {
            return Err(LibraryError::not_found(EntityKind::NotifyDestination, id));
        }
        Ok(())
    }

    /// Deletes a destination with its routes, patron targets and delivery
    /// log rows.
    ///
    /// # Errors
    ///
    /// [`LibraryError::NotFound`] when the destination does not exist.
    pub async fn delete_notify_destination(&self, id: &str) -> Result<()> {
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-delete-{id}-{}", uuid::Uuid::new_v4()),
                vec![
                    batch::exec(
                        "DELETE FROM notify_routes WHERE destination_id = ?",
                        vec![batch::text(id)],
                    ),
                    batch::exec(
                        "DELETE FROM notify_patron_targets WHERE destination_id = ?",
                        vec![batch::text(id)],
                    ),
                    batch::exec(
                        "DELETE FROM notify_deliveries WHERE destination_id = ?",
                        vec![batch::text(id)],
                    ),
                    batch::exec(
                        "DELETE FROM notify_destinations WHERE id = ?",
                        vec![batch::text(id)],
                    ),
                ],
            ))
            .await?;
        if batch::rows_affected(&reply, 3) == 0 {
            return Err(LibraryError::not_found(EntityKind::NotifyDestination, id));
        }
        Ok(())
    }

    /// Every operational route, by type; an "In app" route comes first in
    /// its type's list under [`IN_APP_DESTINATION`].
    ///
    /// # Errors
    ///
    /// An error when the read fails or a stored severity is malformed.
    pub async fn notify_routes(&self) -> Result<BTreeMap<String, Vec<Route>>> {
        let mut out: BTreeMap<String, Vec<Route>> = BTreeMap::new();
        for row in crate::store::keyset::read_keyset(
            self,
            notify_routes::Entity::find(),
            &[
                notify_routes::Column::NotificationType,
                notify_routes::Column::DestinationId,
            ],
        )
        .await?
        {
            out.entry(row.notification_type).or_default().push(Route {
                destination_id: row.destination_id,
                min_severity: parse_severity(&row.min_severity)?,
            });
        }
        for row in crate::store::keyset::read_keyset(
            self,
            notify_inbox_routes::Entity::find(),
            &[notify_inbox_routes::Column::NotificationType],
        )
        .await?
        {
            // The built-in target leads its type's list.
            out.entry(row.notification_type).or_default().insert(
                0,
                Route {
                    destination_id: IN_APP_DESTINATION.to_owned(),
                    min_severity: parse_severity(&row.min_severity)?,
                },
            );
        }
        Ok(out)
    }

    /// Replaces the routes of one operational type. A route to
    /// [`IN_APP_DESTINATION`] is the staff inbox, not a stored destination.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] for too many routes, a repeated destination
    /// or one that is not a channel; [`LibraryError::NotFound`] when a
    /// destination does not exist.
    pub async fn set_notify_routes(&self, notification_type: &str, routes: &[Route]) -> Result<()> {
        if routes.len() > MAX_ROUTES_PER_TYPE {
            return Err(LibraryError::Invalid(format!(
                "a type has at most {MAX_ROUTES_PER_TYPE} routes"
            )));
        }
        let mut seen = BTreeSet::new();
        for route in routes {
            if !seen.insert(route.destination_id.as_str()) {
                return Err(LibraryError::Invalid(format!(
                    "destination {} is routed twice",
                    route.destination_id
                )));
            }
            if route.destination_id == IN_APP_DESTINATION {
                continue;
            }
            let dest = self
                .notify_destination(&route.destination_id)
                .await?
                .ok_or_else(|| {
                    LibraryError::not_found(EntityKind::NotifyDestination, &route.destination_id)
                })?;
            if dest.kind != DestinationKind::Channel {
                return Err(LibraryError::Invalid(format!(
                    "destination {} is personal; routes go to channels",
                    dest.id
                )));
            }
        }
        let now = super::now_str();
        let mut statements = vec![
            batch::exec(
                "DELETE FROM notify_route_revisions WHERE notification_type = ?",
                vec![batch::text(notification_type)],
            ),
            batch::exec(
                "INSERT INTO notify_route_revisions (notification_type, revision) VALUES (?, ?)",
                vec![
                    batch::text(notification_type),
                    batch::text(&uuid::Uuid::new_v4().to_string()),
                ],
            ),
            batch::exec(
                "DELETE FROM notify_routes WHERE notification_type = ?",
                vec![batch::text(notification_type)],
            ),
            batch::exec(
                "DELETE FROM notify_inbox_routes WHERE notification_type = ?",
                vec![batch::text(notification_type)],
            ),
        ];
        for route in routes {
            if route.destination_id == IN_APP_DESTINATION {
                statements.push(batch::exec(
                    "INSERT INTO notify_inbox_routes (notification_type, min_severity, \
                     created_at) VALUES (?, ?, ?)",
                    vec![
                        batch::text(notification_type),
                        batch::text(route.min_severity.wire_name()),
                        batch::text(&now),
                    ],
                ));
                continue;
            }
            statements.push(batch::exec(
                "INSERT INTO notify_routes (notification_type, destination_id, min_severity, \
                 created_at) VALUES (?, ?, ?, ?)",
                vec![
                    batch::text(notification_type),
                    batch::text(&route.destination_id),
                    batch::text(route.min_severity.wire_name()),
                    batch::text(&now),
                ],
            ));
        }
        self.execute_host_batch(batch::request(
            &format!("notify-routes-{}", uuid::Uuid::new_v4()),
            statements,
        ))
        .await?;
        Ok(())
    }

    /// Every patron offer, by type.
    ///
    /// # Errors
    ///
    /// An error when the read fails.
    pub async fn patron_offers(&self) -> Result<BTreeMap<String, Vec<PatronOffer>>> {
        let mut out: BTreeMap<String, Vec<PatronOffer>> = BTreeMap::new();
        for row in crate::store::keyset::read_keyset(
            self,
            notify_patron_offers::Entity::find(),
            &[
                notify_patron_offers::Column::NotificationType,
                notify_patron_offers::Column::PluginInstanceId,
            ],
        )
        .await?
        {
            out.entry(row.notification_type)
                .or_default()
                .push(PatronOffer {
                    plugin_instance_id: row.plugin_instance_id,
                    default_on: row.default_on != 0,
                });
        }
        Ok(out)
    }

    /// Replaces the instances offered to patrons for one patron type.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] for too many offers or a repeated instance.
    pub async fn set_patron_offers(
        &self,
        notification_type: &str,
        offers: &[PatronOffer],
    ) -> Result<()> {
        if offers.len() > MAX_ROUTES_PER_TYPE {
            return Err(LibraryError::Invalid(format!(
                "a type has at most {MAX_ROUTES_PER_TYPE} offers"
            )));
        }
        let mut seen = BTreeSet::new();
        for offer in offers {
            if offer.plugin_instance_id.trim().is_empty()
                || !seen.insert(offer.plugin_instance_id.as_str())
            {
                return Err(LibraryError::Invalid(format!(
                    "instance `{}` is empty or offered twice",
                    offer.plugin_instance_id
                )));
            }
        }
        let now = super::now_str();
        let mut statements = vec![batch::exec(
            "DELETE FROM notify_patron_offers WHERE notification_type = ?",
            vec![batch::text(notification_type)],
        )];
        for offer in offers {
            statements.push(batch::exec(
                "INSERT INTO notify_patron_offers (notification_type, plugin_instance_id, \
                 default_on, created_at) VALUES (?, ?, ?, ?)",
                vec![
                    batch::text(notification_type),
                    batch::text(&offer.plugin_instance_id),
                    batch::int(i64::from(offer.default_on)),
                    batch::text(&now),
                ],
            ));
        }
        self.execute_host_batch(batch::request(
            &format!("notify-offers-{}", uuid::Uuid::new_v4()),
            statements,
        ))
        .await?;
        Ok(())
    }

    /// One patron's choices, by type.
    ///
    /// # Errors
    ///
    /// An error when a read fails.
    pub async fn patron_choices(&self, user_id: i64) -> Result<Vec<PatronChoice>> {
        let mut targets: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in crate::store::keyset::read_keyset(
            self,
            notify_patron_targets::Entity::find()
                .filter(notify_patron_targets::Column::UserId.eq(user_id)),
            &[
                notify_patron_targets::Column::NotificationType,
                notify_patron_targets::Column::DestinationId,
            ],
        )
        .await?
        {
            targets
                .entry(row.notification_type)
                .or_default()
                .push(row.destination_id);
        }
        Ok(crate::store::keyset::read_keyset(
            self,
            notify_patron_prefs::Entity::find()
                .filter(notify_patron_prefs::Column::UserId.eq(user_id)),
            &[notify_patron_prefs::Column::NotificationType],
        )
        .await?
        .into_iter()
        .map(|row| PatronChoice {
            destination_ids: targets.remove(&row.notification_type).unwrap_or_default(),
            notification_type: row.notification_type,
            enabled: row.enabled != 0,
        })
        .collect())
    }

    /// Stores one patron's choice for one patron type, replacing the last.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] for too many or repeated destinations, or,
    /// when the choice is enabled, one on a notifier not offered for the
    /// type;
    /// [`LibraryError::NotFound`] when a destination does not exist.
    pub async fn set_patron_choice(&self, user_id: i64, choice: &PatronChoice) -> Result<()> {
        if choice.destination_ids.len() > MAX_ROUTES_PER_TYPE {
            return Err(LibraryError::Invalid(format!(
                "a type goes to at most {MAX_ROUTES_PER_TYPE} destinations"
            )));
        }
        // Only a notifier offered for the type can carry it: routing skips
        // any other, so such a pick would quietly leave the inbox alone.
        let offered: BTreeSet<String> = crate::store::keyset::read_keyset(
            self,
            notify_patron_offers::Entity::find().filter(
                notify_patron_offers::Column::NotificationType.eq(&choice.notification_type),
            ),
            &[notify_patron_offers::Column::PluginInstanceId],
        )
        .await?
        .into_iter()
        .map(|o| o.plugin_instance_id)
        .collect();
        let mut seen = BTreeSet::new();
        for id in &choice.destination_ids {
            if !seen.insert(id.as_str()) {
                return Err(LibraryError::Invalid(format!(
                    "destination {id} is chosen twice"
                )));
            }
            let dest = self
                .notify_destination(id)
                .await?
                .ok_or_else(|| LibraryError::not_found(EntityKind::NotifyDestination, id))?;
            if dest.kind != DestinationKind::Personal || dest.user_id != Some(user_id) {
                // Someone else's destination answers like a missing one.
                return Err(LibraryError::not_found(EntityKind::NotifyDestination, id));
            }
            if choice.enabled && !offered.contains(&dest.plugin_instance_id) {
                return Err(LibraryError::Invalid(format!(
                    "destination {id} is on a notifier not offered for `{}`",
                    choice.notification_type
                )));
            }
        }
        let now = super::now_str();
        let mut statements = vec![
            batch::exec(
                "DELETE FROM notify_patron_targets WHERE user_id = ? AND notification_type = ?",
                vec![batch::int(user_id), batch::text(&choice.notification_type)],
            ),
            batch::exec(
                "DELETE FROM notify_patron_prefs WHERE user_id = ? AND notification_type = ?",
                vec![batch::int(user_id), batch::text(&choice.notification_type)],
            ),
            batch::exec(
                "INSERT INTO notify_patron_prefs (user_id, notification_type, enabled, \
                 updated_at) VALUES (?, ?, ?, ?)",
                vec![
                    batch::int(user_id),
                    batch::text(&choice.notification_type),
                    batch::int(i64::from(choice.enabled)),
                    batch::text(&now),
                ],
            ),
        ];
        for id in &choice.destination_ids {
            statements.push(batch::exec(
                "INSERT INTO notify_patron_targets (user_id, notification_type, destination_id) \
                 VALUES (?, ?, ?)",
                vec![
                    batch::int(user_id),
                    batch::text(&choice.notification_type),
                    batch::text(id),
                ],
            ));
        }
        self.execute_host_batch(batch::request(
            &format!("notify-choice-{user_id}-{}", uuid::Uuid::new_v4()),
            statements,
        ))
        .await?;
        Ok(())
    }

    /// Drops one patron's choice for a type, so the defaults apply again.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn clear_patron_choice(&self, user_id: i64, notification_type: &str) -> Result<()> {
        self.execute_host_batch(batch::request(
            &format!("notify-choice-clear-{user_id}-{}", uuid::Uuid::new_v4()),
            vec![
                batch::exec(
                    "DELETE FROM notify_patron_targets WHERE user_id = ? \
                     AND notification_type = ?",
                    vec![batch::int(user_id), batch::text(notification_type)],
                ),
                batch::exec(
                    "DELETE FROM notify_patron_prefs WHERE user_id = ? AND notification_type = ?",
                    vec![batch::int(user_id), batch::text(notification_type)],
                ),
            ],
        ))
        .await?;
        Ok(())
    }

    /// One page of the delivery log, newest first.
    ///
    /// # Arguments
    ///
    /// * `destination_id` - Only this destination's rows, when set.
    /// * `before` - Where the page starts (the previous page's last row).
    /// * `limit` - Page size, clamped to `1..=`[`MAX_NOTIFY_PAGE`].
    ///
    /// # Errors
    ///
    /// An error when a read fails or a stored value is malformed.
    pub async fn notify_deliveries(
        &self,
        destination_id: Option<&str>,
        before: Option<&NotifyCursor>,
        limit: u64,
    ) -> Result<NotifyPage<DeliveryRecord>> {
        let read = |after: Option<&NotifyCursor>, rows: u64| {
            let mut query = notify_deliveries::Entity::find()
                .order_by_desc(notify_deliveries::Column::CreatedAt)
                .order_by_desc(notify_deliveries::Column::Id)
                .limit(rows);
            if let Some(dest) = destination_id {
                query = query.filter(notify_deliveries::Column::DestinationId.eq(dest));
            }
            if let Some(cursor) = after {
                query = query.filter(before_cursor(
                    notify_deliveries::Column::CreatedAt,
                    notify_deliveries::Column::Id,
                    cursor,
                ));
            }
            query
        };
        let window = self.page_window(limit);
        let rows = read(before, window.read)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let (rows, mut more) = split_page(rows, window.size);
        if window.needs_probe(rows.len(), more) {
            if let Some(last) = rows.last() {
                let next = NotifyCursor {
                    created_at: last.created_at.clone(),
                    id: last.id.clone(),
                };
                more = read(Some(&next), 1)
                    .one(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                    .is_some();
            }
        }
        let ids: Vec<String> = rows
            .iter()
            .map(|r| r.notification_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let notes: BTreeMap<String, notifications::Model> = self
            .notifications_by_id(&ids)
            .await?
            .into_iter()
            .map(|n| (n.id.clone(), n))
            .collect();
        let items = rows
            .into_iter()
            .map(|row| {
                let note = notes.get(&row.notification_id);
                Ok(DeliveryRecord {
                    notification_type: note
                        .map(|n| n.notification_type.clone())
                        .unwrap_or_default(),
                    title: note.map(|n| n.title.clone()).unwrap_or_default(),
                    state: parse_state(&row.state)?,
                    id: row.id,
                    notification_id: row.notification_id,
                    destination_id: row.destination_id,
                    plugin_instance_id: row.plugin_instance_id,
                    attempts: row.attempts,
                    provider_id: row.provider_id,
                    reason: row.reason,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(NotifyPage { items, more })
    }

    /// One page of a patron's inbox, newest first.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The patron.
    /// * `before` - Where the page starts (the previous page's last entry;
    ///   its id is the notification id).
    /// * `limit` - Page size, clamped to `1..=`[`MAX_NOTIFY_PAGE`].
    ///
    /// # Errors
    ///
    /// An error when a read fails or a stored value is malformed.
    pub async fn inbox(
        &self,
        user_id: i64,
        before: Option<&NotifyCursor>,
        limit: u64,
    ) -> Result<NotifyPage<InboxEntry>> {
        let read = |after: Option<&NotifyCursor>, rows: u64| {
            let mut query = notify_inbox::Entity::find()
                .filter(notify_inbox::Column::UserId.eq(user_id))
                .order_by_desc(notify_inbox::Column::CreatedAt)
                .order_by_desc(notify_inbox::Column::NotificationId)
                .limit(rows);
            if let Some(cursor) = after {
                query = query.filter(before_cursor(
                    notify_inbox::Column::CreatedAt,
                    notify_inbox::Column::NotificationId,
                    cursor,
                ));
            }
            query
        };
        let window = self.page_window(limit);
        let rows = read(before, window.read)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let (rows, mut more) = split_page(rows, window.size);
        if window.needs_probe(rows.len(), more) {
            if let Some(last) = rows.last() {
                let next = NotifyCursor {
                    created_at: last.created_at.clone(),
                    id: last.notification_id.clone(),
                };
                more = read(Some(&next), 1)
                    .one(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                    .is_some();
            }
        }
        let ids: Vec<String> = rows.iter().map(|r| r.notification_id.clone()).collect();
        let mut notes: BTreeMap<String, notifications::Model> = self
            .notifications_by_id(&ids)
            .await?
            .into_iter()
            .map(|n| (n.id.clone(), n))
            .collect();
        let mut out = Vec::new();
        for row in rows {
            if let Some(note) = notes.remove(&row.notification_id) {
                out.push(InboxEntry {
                    notification: stored(note)?,
                    read_at: row.read_at,
                });
            }
        }
        Ok(NotifyPage { items: out, more })
    }

    /// How many entries in a patron's inbox are unread.
    ///
    /// # Errors
    ///
    /// An error when the read fails.
    pub async fn inbox_unread(&self, user_id: i64) -> Result<u64> {
        use sea_orm::PaginatorTrait;
        notify_inbox::Entity::find()
            .filter(notify_inbox::Column::UserId.eq(user_id))
            .filter(notify_inbox::Column::ReadAt.is_null())
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Marks one inbox entry read, or every unread one when
    /// `notification_id` is `None`.
    ///
    /// # Returns
    ///
    /// How many entries changed.
    ///
    /// # Errors
    ///
    /// [`LibraryError::NotFound`] when the named entry is not in this
    /// patron's inbox; an error when the batch fails.
    pub async fn mark_inbox_read(
        &self,
        user_id: i64,
        notification_id: Option<&str>,
    ) -> Result<u64> {
        let now = super::now_str();
        let statement = match notification_id {
            Some(id) => {
                let found = notify_inbox::Entity::find()
                    .filter(notify_inbox::Column::UserId.eq(user_id))
                    .filter(notify_inbox::Column::NotificationId.eq(id))
                    .one(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?;
                if found.is_none() {
                    return Err(LibraryError::not_found(EntityKind::Notification, id));
                }
                batch::exec(
                    "UPDATE notify_inbox SET read_at = ? WHERE user_id = ? \
                     AND notification_id = ? AND read_at IS NULL",
                    vec![batch::text(&now), batch::int(user_id), batch::text(id)],
                )
            }
            None => batch::exec(
                "UPDATE notify_inbox SET read_at = ? WHERE user_id = ? AND read_at IS NULL",
                vec![batch::text(&now), batch::int(user_id)],
            ),
        };
        // Only a read that changed a row tells this patron's other pages.
        let bumped = bump_if_any(
            LiveTopic::Notifications,
            batch::exec(
                "SELECT 1 FROM notify_inbox WHERE user_id = ? AND read_at = ?",
                vec![batch::int(user_id), batch::text(&now)],
            ),
        );
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-read-{user_id}-{}", uuid::Uuid::new_v4()),
                vec![statement, bumped],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0))
    }

    /// Deletes notifications older than [`NOTIFICATION_RETENTION_DAYS`] as
    /// of `now`, with their inbox entries and delivery-log rows.
    ///
    /// # Returns
    ///
    /// How many notifications were deleted.
    ///
    /// # Errors
    ///
    /// An error when the batch fails; nothing is deleted then.
    pub async fn prune_notifications(&self, now: DateTime<Utc>) -> Result<u64> {
        let cutoff = (now - Duration::days(NOTIFICATION_RETENTION_DAYS)).to_rfc3339();
        let reply = self
            .execute_host_batch(batch::request(
                &format!("notify-prune-{}", uuid::Uuid::new_v4()),
                vec![
                    batch::exec(
                        "DELETE FROM notify_inbox WHERE notification_id IN \
                         (SELECT id FROM notifications WHERE created_at < ?)",
                        vec![batch::text(&cutoff)],
                    ),
                    batch::exec(
                        "DELETE FROM notify_deliveries WHERE notification_id IN \
                         (SELECT id FROM notifications WHERE created_at < ?)",
                        vec![batch::text(&cutoff)],
                    ),
                    batch::exec(
                        "DELETE FROM notifications WHERE created_at < ?",
                        vec![batch::text(&cutoff)],
                    ),
                ],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 2))
    }
}
