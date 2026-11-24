//! Account mail (`docs/adr/notifications.md` decisions 5 and 6): the
//! notifier instance it goes through, its messages and their deliveries,
//! and the links it carries.
//!
//! Account mail is never routed and never an event. Each message is one
//! `account_mail` row, sent by its own `notify_send` job
//! ([`crate::models::JobPayload::account_mail`]). The link a message
//! carries is minted by that job when it sends
//! ([`LibraryStore::mint_invite_ticket`],
//! [`LibraryStore::mint_email_verification`]): only the token's hash is
//! stored, and a retry mints a new token that voids the last, so no link
//! is ever written to the database, a job payload or a log.

use bookclerk_plugin_abi::notifications::sanitize_reason;
use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::notifications::fenced;
use super::LibraryStore;
use crate::control_plane::batch;
use crate::entities::{
    account_mail, account_mail_settings, email_verifications, jobs, password_resets, users,
};
use crate::error::{LibraryError, Result};
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, JobFence, JobKind, JobPayload, JobState, JobTrigger,
};
use crate::notifications::{
    AccountMail, AccountMailKind, AccountMailSettings, DeliveryState, FailedAttempt, MintedToken,
    PasswordResetFactor, PasswordResetToken, RedeemedReset, VerifyEmailToken,
    ACCOUNT_MAIL_RETENTION_DAYS, EMAIL_VERIFICATION_HOURS, INVITE_DAYS, PASSWORD_RESET_MINUTES,
};
use crate::token_hash::hash_token;
use crate::NotifyQueueLimits;

/// A message from its row.
///
/// # Errors
///
/// [`LibraryError::Invalid`] when a stored kind or state is unknown.
fn mail(row: account_mail::Model) -> Result<AccountMail> {
    Ok(AccountMail {
        kind: AccountMailKind::parse(&row.kind).ok_or_else(|| {
            LibraryError::Invalid(format!(
                "stored account-mail kind `{}` is unknown",
                row.kind
            ))
        })?,
        state: DeliveryState::parse(&row.state).ok_or_else(|| {
            LibraryError::Invalid(format!(
                "stored account-mail state `{}` is unknown",
                row.state
            ))
        })?,
        id: row.id,
        user_id: row.user_id,
        address: row.address,
        plugin_instance_id: row.plugin_instance_id,
        attempts: row.attempts,
    })
}

/// A fresh link token: 32 random bytes as lowercase hex.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The `bookclerk_slots` key that serializes rate-limited queueing of one
/// kind of account mail for one user.
///
/// The row is never deleted: a batch waiting on the bump must find the
/// row the holder bumped, or two later batches could run unserialized.
///
/// # Arguments
///
/// * `kind` - The kind of message.
/// * `user_id` - The user the message goes to.
#[must_use]
pub fn account_mail_slot(kind: AccountMailKind, user_id: i64) -> String {
    format!("account-mail:{}:{user_id}", kind.as_str())
}

/// SQL condition: the user still has the address a link is minted for, so
/// a send that read an old address mints nothing and voids nothing.
/// Binds: user id, address.
const STILL_ADDRESSED: &str = "EXISTS (SELECT 1 FROM users WHERE id = ? AND email = ?)";

/// SQL condition: the user still has the address, verified, so a reset
/// link is minted only for a verified address and a send that read an old
/// one mints nothing and voids nothing. Binds: user id, address.
const STILL_VERIFIED: &str = "EXISTS (SELECT 1 FROM users WHERE id = ? AND email = ? \
                              AND email_verified_at IS NOT NULL)";

/// The `expires_at` a reset link holds while one redemption batch has
/// claimed it. No minted link expires then, and an expired link never
/// passes the claim, so the marker names only a claim made inside the
/// running batch (the same batch deletes the row before it commits).
const RESET_CLAIM: &str = "1970-01-01T00:00:00+00:00";

impl LibraryStore {
    /// The instance account mail goes through; `None` when none is chosen.
    ///
    /// # Errors
    ///
    /// An error when the read fails.
    pub async fn account_mail_settings(&self) -> Result<Option<AccountMailSettings>> {
        Ok(account_mail_settings::Entity::find_by_id(1)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|row| AccountMailSettings {
                plugin_instance_id: row.plugin_instance_id,
                tested_at: row
                    .tested_at
                    .as_deref()
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc)),
            }))
    }

    /// Chooses the instance account mail goes through, or none.
    ///
    /// Choosing another instance clears the record of a delivered test;
    /// choosing the same one again keeps it.
    ///
    /// # Arguments
    ///
    /// * `plugin_instance_id` - The notifier instance; `None` turns account
    ///   mail off. The caller checks that it can carry account mail.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn set_account_mail_instance(&self, plugin_instance_id: Option<&str>) -> Result<()> {
        let statements = match plugin_instance_id {
            None => vec![batch::exec(
                "DELETE FROM account_mail_settings WHERE id = 1",
                vec![],
            )],
            Some(id) => {
                let now = super::now_str();
                vec![
                    batch::exec(
                        "UPDATE account_mail_settings SET plugin_instance_id = ?, \
                         tested_at = NULL, updated_at = ? \
                         WHERE id = 1 AND plugin_instance_id <> ?",
                        vec![batch::text(id), batch::text(&now), batch::text(id)],
                    ),
                    batch::exec(
                        "INSERT INTO account_mail_settings (id, plugin_instance_id, updated_at) \
                         SELECT 1, ?, ? WHERE NOT EXISTS \
                         (SELECT 1 FROM account_mail_settings WHERE id = 1)",
                        vec![batch::text(id), batch::text(&now)],
                    ),
                ]
            }
        };
        self.execute_host_batch(batch::request(
            &format!("account-mail-settings-{}", uuid::Uuid::new_v4()),
            statements,
        ))
        .await?;
        Ok(())
    }

    /// Records a message for `user_id` at `address` and enqueues its send.
    ///
    /// # Returns
    ///
    /// The message id. A queue that is full leaves the message pending
    /// without a job; the notification sweep enqueues it later.
    ///
    /// # Errors
    ///
    /// An error when the write fails.
    pub async fn queue_account_mail(
        &self,
        kind: AccountMailKind,
        user_id: i64,
        address: &str,
        plugin_instance_id: &str,
        limits: NotifyQueueLimits,
    ) -> Result<String> {
        self.queue_account_mail_unless_since(
            kind,
            user_id,
            address,
            plugin_instance_id,
            limits,
            None,
        )
        .await?
        .ok_or_else(|| LibraryError::Other(anyhow::anyhow!("account mail was not recorded")))
    }

    /// [`Self::queue_account_mail`], refused when a message of the same
    /// kind for the user was recorded after `since`.
    ///
    /// With `since`, the batch first takes the user's `bookclerk_slots`
    /// write lock for that kind (`account_mail_slot`), so on every
    /// database profile (`READ COMMITTED` PostgreSQL included) the guard
    /// reads what an overlapping request committed, and two overlapping
    /// requests cannot both pass it.
    ///
    /// # Returns
    ///
    /// The message id, or `None` when a recent message refused it.
    ///
    /// # Errors
    ///
    /// An error when the write fails.
    pub async fn queue_account_mail_unless_since(
        &self,
        kind: AccountMailKind,
        user_id: i64,
        address: &str,
        plugin_instance_id: &str,
        limits: NotifyQueueLimits,
        since: Option<DateTime<Utc>>,
    ) -> Result<Option<String>> {
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let now = super::now_str();
        let mut parameters = vec![
            batch::text(&id),
            batch::text(kind.as_str()),
            batch::int(user_id),
            batch::text(address),
            batch::text(plugin_instance_id),
            batch::text(&now),
            batch::text(&now),
        ];
        let mut statements = Vec::new();
        let guard = match since {
            Some(since) => {
                let slot = account_mail_slot(kind, user_id);
                statements.push(batch::exec(
                    "INSERT OR IGNORE INTO bookclerk_slots (slot_key, bump) \
                     SELECT ?, 0 WHERE NOT EXISTS (\
                        SELECT 1 FROM bookclerk_slots WHERE slot_key = ?\
                     )",
                    vec![batch::text(&slot), batch::text(&slot)],
                ));
                statements.push(batch::exec(
                    "UPDATE bookclerk_slots SET bump = bump + 1 WHERE slot_key = ?",
                    vec![batch::text(&slot)],
                ));
                parameters.push(batch::int(user_id));
                parameters.push(batch::text(kind.as_str()));
                parameters.push(batch::text(&since.to_rfc3339()));
                "NOT EXISTS (SELECT 1 FROM account_mail WHERE user_id = ? AND kind = ? \
                 AND created_at > ?)"
            }
            None => "1 = 1",
        };
        let insert_index = statements.len();
        statements.push(batch::exec(
            &format!(
                "INSERT INTO account_mail (id, kind, user_id, address, \
                 plugin_instance_id, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ? WHERE {guard}"
            ),
            parameters,
        ));
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-queue-{id}"),
                statements,
            ))
            .await?;
        if batch::rows_affected(&reply, insert_index) != 1 {
            return Ok(None);
        }
        if let Err(err) = self
            .enqueue_account_mail_send(&id, plugin_instance_id, limits)
            .await
        {
            tracing::warn!(
                error = %err,
                mail = %id,
                "account-mail send enqueue failed; the sweep retries it"
            );
        }
        Ok(Some(id))
    }

    /// Enqueues the send job of one message and records its id.
    ///
    /// # Returns
    ///
    /// The job id, or `None` when the queue is full.
    ///
    /// # Errors
    ///
    /// An error when the enqueue or the write fails.
    pub async fn enqueue_account_mail_send(
        &self,
        mail_id: &str,
        plugin_instance_id: &str,
        limits: NotifyQueueLimits,
    ) -> Result<Option<String>> {
        let outcome = self
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::NotifySend,
                payload: JobPayload {
                    plugin_instance_id: Some(plugin_instance_id.to_string()),
                    requires: vec![plugin_instance_id.to_string()],
                    trigger: JobTrigger::Scheduler,
                    account_mail: Some(mail_id.to_string()),
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
        self.execute_host_batch(batch::request(
            &format!("account-mail-job-{mail_id}-{}", uuid::Uuid::new_v4()),
            vec![batch::exec(
                "UPDATE account_mail SET job_id = ?, updated_at = ? \
                 WHERE id = ? AND state = 'pending'",
                vec![
                    batch::text(&job_id),
                    batch::text(&super::now_str()),
                    batch::text(mail_id),
                ],
            )],
        ))
        .await?;
        Ok(Some(job_id))
    }

    /// Enqueues the job that answers a self-service password-reset
    /// request for `address`: it looks the account up and queues the reset
    /// mail when a user has that address verified.
    ///
    /// # Returns
    ///
    /// Whether a job was enqueued or already waits for the address;
    /// `false` when the queue is full.
    ///
    /// # Errors
    ///
    /// An error when the enqueue fails.
    pub async fn enqueue_password_reset_request(
        &self,
        address: &str,
        plugin_instance_id: &str,
        limits: NotifyQueueLimits,
    ) -> Result<bool> {
        let outcome = self
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::NotifySend,
                payload: JobPayload {
                    plugin_instance_id: Some(plugin_instance_id.to_string()),
                    requires: vec![plugin_instance_id.to_string()],
                    trigger: JobTrigger::Scheduler,
                    password_reset_request: Some(address.to_string()),
                    ..JobPayload::default()
                },
                priority: 0,
                max_attempts: limits.max_attempts.max(1),
                max_pending: limits.max_pending,
                run_after: None,
            })
            .await?;
        Ok(match outcome {
            EnqueueOutcome::Created { .. } | EnqueueOutcome::Duplicate { .. } => true,
            EnqueueOutcome::QueueFull => false,
        })
    }

    /// One message; `None` when it is gone (pruned, or its user deleted).
    ///
    /// # Errors
    ///
    /// An error when the read fails or a stored value is malformed.
    pub async fn account_mail(&self, id: &str) -> Result<Option<AccountMail>> {
        account_mail::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(mail)
            .transpose()
    }

    /// When the newest message of `kind` for `user_id` was queued, for
    /// throttling patrons who ask again.
    ///
    /// # Errors
    ///
    /// An error when the read fails.
    pub async fn last_account_mail_at(
        &self,
        user_id: i64,
        kind: AccountMailKind,
    ) -> Result<Option<DateTime<Utc>>> {
        Ok(account_mail::Entity::find()
            .filter(account_mail::Column::UserId.eq(user_id))
            .filter(account_mail::Column::Kind.eq(kind.as_str()))
            .order_by_desc(account_mail::Column::CreatedAt)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .and_then(|row| DateTime::parse_from_rfc3339(&row.created_at).ok())
            .map(|at| at.with_timezone(&Utc)))
    }

    /// Pending messages no send job was recorded for, oldest first.
    ///
    /// # Errors
    ///
    /// An error when the read fails or a stored value is malformed.
    pub async fn unscheduled_account_mail(&self, limit: u64) -> Result<Vec<AccountMail>> {
        account_mail::Entity::find()
            .filter(account_mail::Column::State.eq(DeliveryState::Pending.as_str()))
            .filter(account_mail::Column::JobId.is_null())
            .order_by_asc(account_mail::Column::CreatedAt)
            .order_by_asc(account_mail::Column::Id)
            .limit(limit.clamp(1, 1000).min(self.result_row_cap()))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .into_iter()
            .map(mail)
            .collect()
    }

    /// Drops pending messages whose send job ended without recording a
    /// result, as [`Self::drop_orphaned_notify_deliveries`] does for
    /// deliveries.
    ///
    /// # Returns
    ///
    /// How many messages were dropped.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn drop_orphaned_account_mail(&self) -> Result<u64> {
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-orphans-{}", uuid::Uuid::new_v4()),
                vec![batch::exec(
                    "UPDATE account_mail SET state = 'dropped', \
                     reason = 'the send job ended without a result', updated_at = ? \
                     WHERE state = 'pending' AND job_id IS NOT NULL AND job_id NOT IN \
                     (SELECT id FROM jobs WHERE state IN ('pending', 'running'))",
                    vec![batch::text(&super::now_str())],
                )],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0))
    }

    /// Drops a pending message that has no job id when a send job for it
    /// already ended: its enqueue succeeded but the link write did not,
    /// and the job was then cancelled or failed. As
    /// [`Self::drop_notify_delivery_if_job_ended`] does for deliveries, this
    /// keeps the sweep from sending a message an operator cancelled, since
    /// the ended job no longer blocks a new one with the same dedup key.
    ///
    /// # Returns
    ///
    /// Whether the message was dropped (and must not be enqueued again).
    ///
    /// # Errors
    ///
    /// An error when the read or the batch fails.
    pub async fn drop_account_mail_if_job_ended(&self, mail_id: &str) -> Result<bool> {
        let ended = jobs::Entity::find()
            .filter(jobs::Column::DedupKey.eq(crate::models::account_mail_dedup_key(mail_id)))
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
                &format!("account-mail-unlinked-{mail_id}-{}", uuid::Uuid::new_v4()),
                vec![batch::exec(
                    "UPDATE account_mail SET state = 'dropped', \
                     reason = 'the send job ended without a result', updated_at = ? \
                     WHERE id = ? AND state = 'pending' AND job_id IS NULL",
                    vec![batch::text(&super::now_str()), batch::text(mail_id)],
                )],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0) == 1)
    }

    /// Records a delivered message. A delivered test marks the instance
    /// tested, when it is still the account-mail instance.
    ///
    /// # Returns
    ///
    /// `false` when the message was no longer pending.
    ///
    /// # Arguments
    ///
    /// * `fence` - The send job's claim; nothing is written once it lost
    ///   the job.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn record_account_mail_delivered(
        &self,
        id: &str,
        fence: Option<&JobFence>,
    ) -> Result<bool> {
        let now = super::now_str();
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-delivered-{id}-{}", uuid::Uuid::new_v4()),
                vec![
                    fenced(
                        "UPDATE account_mail_settings SET tested_at = ?, updated_at = ? \
                         WHERE id = 1 AND plugin_instance_id IN (SELECT plugin_instance_id \
                         FROM account_mail WHERE id = ? AND kind = 'test' AND state = 'pending')",
                        vec![batch::text(&now), batch::text(&now), batch::text(id)],
                        fence,
                    ),
                    fenced(
                        "UPDATE account_mail SET state = 'delivered', attempts = attempts + 1, \
                         reason = NULL, updated_at = ? WHERE id = ? AND state = 'pending'",
                        vec![batch::text(&now), batch::text(id)],
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
    /// The attempts made, this one included; `None` when the message was
    /// no longer pending.
    ///
    /// # Errors
    ///
    /// An error when the batch or the read fails.
    pub async fn record_account_mail_retry(
        &self,
        id: &str,
        reason: &str,
        fence: Option<&JobFence>,
    ) -> Result<Option<i64>> {
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-retry-{id}-{}", uuid::Uuid::new_v4()),
                vec![fenced(
                    "UPDATE account_mail SET attempts = attempts + 1, reason = ?, \
                     updated_at = ? WHERE id = ? AND state = 'pending'",
                    vec![
                        batch::text(&sanitize_reason(reason)),
                        batch::text(&super::now_str()),
                        batch::text(id),
                    ],
                    fence,
                )],
            ))
            .await?;
        if batch::rows_affected(&reply, 0) != 1 {
            return Ok(None);
        }
        Ok(account_mail::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|row| row.attempts))
    }

    /// Records a message that failed for good, or that was not sent
    /// (`state` [`DeliveryState::Dropped`]).
    ///
    /// # Returns
    ///
    /// `false` when the message was no longer pending.
    ///
    /// # Arguments
    ///
    /// * `state` - [`DeliveryState::Failed`] or [`DeliveryState::Dropped`].
    /// * `attempt` - Whether this answer spends an attempt of its own.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] for another state; an error when the batch
    /// fails.
    pub async fn record_account_mail_ended(
        &self,
        id: &str,
        state: DeliveryState,
        reason: &str,
        attempt: FailedAttempt,
        fence: Option<&JobFence>,
    ) -> Result<bool> {
        match state {
            DeliveryState::Failed | DeliveryState::Dropped => {}
            DeliveryState::Pending | DeliveryState::Delivered => {
                return Err(LibraryError::Invalid(format!(
                    "an account-mail send does not end as {}",
                    state.as_str()
                )));
            }
        }
        let spent = match attempt {
            FailedAttempt::New => 1,
            FailedAttempt::AlreadyCounted => 0,
        };
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-ended-{id}-{}", uuid::Uuid::new_v4()),
                vec![fenced(
                    "UPDATE account_mail SET state = ?, attempts = attempts + ?, reason = ?, \
                     updated_at = ? WHERE id = ? AND state = 'pending'",
                    vec![
                        batch::text(state.as_str()),
                        batch::int(spent),
                        batch::text(&sanitize_reason(reason)),
                        batch::text(&super::now_str()),
                        batch::text(id),
                    ],
                    fence,
                )],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0) == 1)
    }

    /// Mints the claim ticket an invite carries: a new ticket for the
    /// user's local sign-in that remembers the address it was mailed to,
    /// in the same batch that voids the user's other unredeemed tickets
    /// (so only the newest invite works).
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user; nothing is minted or voided unless their
    ///   stored address is still `emailed_to`.
    /// * `identity_id` - The user's local portal identity.
    /// * `emailed_to` - The address the invite goes to.
    /// * `created_by` - Audit label of whoever asked for the invite.
    /// * `fence` - The send job's claim; nothing is minted or voided once
    ///   it lost the job, so a stale send cannot void a newer link.
    ///
    /// # Returns
    ///
    /// The raw ticket, for the link; `None` when the fence no longer holds
    /// or the user's address changed.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn mint_invite_ticket(
        &self,
        user_id: i64,
        identity_id: i64,
        emailed_to: &str,
        created_by: &str,
        fence: Option<&JobFence>,
    ) -> Result<Option<MintedToken>> {
        let raw = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-invite-{}", uuid::Uuid::new_v4()),
                vec![
                    fenced(
                        &format!(
                            "DELETE FROM claim_tickets WHERE identity_id = ? \
                             AND redeemed_at IS NULL AND {STILL_ADDRESSED}"
                        ),
                        vec![
                            batch::int(identity_id),
                            batch::int(user_id),
                            batch::text(emailed_to),
                        ],
                        fence,
                    ),
                    fenced(
                        &format!(
                            "INSERT INTO claim_tickets (token_hash, identity_id, expires_at, \
                             created_by, created_at, emailed_to) SELECT ?, ?, ?, ?, ?, ? \
                             WHERE {STILL_ADDRESSED}"
                        ),
                        vec![
                            batch::text(&hash_token(&raw)),
                            batch::int(identity_id),
                            batch::text(&(now + Duration::days(INVITE_DAYS)).to_rfc3339()),
                            batch::text(created_by),
                            batch::text(&now.to_rfc3339()),
                            batch::text(emailed_to),
                            batch::int(user_id),
                            batch::text(emailed_to),
                        ],
                        fence,
                    ),
                ],
            ))
            .await?;
        Ok((batch::rows_affected(&reply, 1) == 1).then(|| MintedToken::new(raw)))
    }

    /// Mints the link a verification message carries, voiding the user's
    /// last one.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user.
    /// * `email` - The address it goes to; the link verifies it only while
    ///   the user still has it.
    /// * `fence` - The send job's claim; nothing is minted or voided once
    ///   it lost the job, so a stale send cannot void a newer link.
    ///
    /// # Returns
    ///
    /// The raw token, for the link; `None` when the fence no longer holds
    /// or the user's address is no longer `email`.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn mint_email_verification(
        &self,
        user_id: i64,
        email: &str,
        fence: Option<&JobFence>,
    ) -> Result<Option<MintedToken>> {
        let raw = random_token();
        let now = Utc::now();
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-verify-{}", uuid::Uuid::new_v4()),
                vec![
                    fenced(
                        &format!(
                            "DELETE FROM email_verifications WHERE user_id = ? \
                             AND {STILL_ADDRESSED}"
                        ),
                        vec![batch::int(user_id), batch::int(user_id), batch::text(email)],
                        fence,
                    ),
                    fenced(
                        &format!(
                            "INSERT INTO email_verifications (token_hash, user_id, email, \
                             expires_at, created_at) SELECT ?, ?, ?, ?, ? WHERE {STILL_ADDRESSED}"
                        ),
                        vec![
                            batch::text(&hash_token(&raw)),
                            batch::int(user_id),
                            batch::text(email),
                            batch::text(
                                &(now + Duration::hours(EMAIL_VERIFICATION_HOURS)).to_rfc3339(),
                            ),
                            batch::text(&now.to_rfc3339()),
                            batch::int(user_id),
                            batch::text(email),
                        ],
                        fence,
                    ),
                ],
            ))
            .await?;
        Ok((batch::rows_affected(&reply, 1) == 1).then(|| MintedToken::new(raw)))
    }

    /// Uses a verification link: the user's address is verified when the
    /// link is live and the user still has the address it was sent to.
    /// The link is used up either way.
    ///
    /// # Arguments
    ///
    /// * `token` - The raw token from the link.
    /// * `now` - The current time.
    ///
    /// # Returns
    ///
    /// The verified user's id; `None` for a link that is unknown, used,
    /// expired, or for an address the user no longer has.
    ///
    /// # Errors
    ///
    /// An error when the read or the batch fails.
    pub async fn use_email_verification(
        &self,
        token: &VerifyEmailToken,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        let token_hash = hash_token(token.expose());
        let Some(row) = email_verifications::Entity::find_by_id(&token_hash)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let live = DateTime::parse_from_rfc3339(&row.expires_at)
            .is_ok_and(|expires| expires.with_timezone(&Utc) > now);
        let mut statements = vec![batch::exec(
            "DELETE FROM email_verifications WHERE token_hash = ?",
            vec![batch::text(&token_hash)],
        )];
        if live {
            statements.push(batch::exec(
                "UPDATE users SET email_verified_at = ?, updated_at = ? \
                 WHERE id = ? AND email = ? AND EXISTS \
                 (SELECT 1 FROM email_verifications WHERE token_hash = ?)",
                vec![
                    batch::text(&now.to_rfc3339()),
                    batch::text(&now.to_rfc3339()),
                    batch::int(row.user_id),
                    batch::text(&row.email),
                    batch::text(&token_hash),
                ],
            ));
            // The update runs before the delete so it still sees the row:
            // a second use of the same link finds it gone.
            statements.swap(0, 1);
            statements.push(crate::control_plane::bump_if_any(
                crate::control_plane::LiveTopic::Users,
                batch::query(
                    "SELECT 1 FROM users WHERE id = ? AND email_verified_at = ?",
                    vec![batch::int(row.user_id), batch::text(&now.to_rfc3339())],
                    1,
                ),
            ));
        }
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-verified-{}", uuid::Uuid::new_v4()),
                statements,
            ))
            .await?;
        Ok((live && batch::rows_affected(&reply, 0) == 1).then_some(row.user_id))
    }

    /// Marks the user's address verified when it is still `email`: used
    /// when an invite mailed to that address is redeemed.
    ///
    /// # Returns
    ///
    /// Whether the address was verified.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn verify_user_email_if(&self, user_id: i64, email: &str) -> Result<bool> {
        let now = super::now_str();
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-invite-verified-{}", uuid::Uuid::new_v4()),
                vec![
                    batch::exec(
                        "UPDATE users SET email_verified_at = ?, updated_at = ? \
                         WHERE id = ? AND email = ? AND email_verified_at IS NULL",
                        vec![
                            batch::text(&now),
                            batch::text(&now),
                            batch::int(user_id),
                            batch::text(email),
                        ],
                    ),
                    crate::control_plane::bump_if_any(
                        crate::control_plane::LiveTopic::Users,
                        batch::query(
                            "SELECT 1 FROM users WHERE id = ? AND email_verified_at = ?",
                            vec![batch::int(user_id), batch::text(&now)],
                            1,
                        ),
                    ),
                ],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0) == 1)
    }

    /// Mints the link a password-reset message carries, voiding the
    /// user's last one.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user.
    /// * `email` - The address it goes to; nothing is minted unless the
    ///   user still has it, verified.
    /// * `fence` - The send job's claim; nothing is minted or voided once
    ///   it lost the job, so a stale send cannot void a newer link.
    ///
    /// # Returns
    ///
    /// The raw token, for the link; `None` when the fence no longer holds
    /// or the user's address is no longer `email`, verified.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn mint_password_reset(
        &self,
        user_id: i64,
        email: &str,
        fence: Option<&JobFence>,
    ) -> Result<Option<MintedToken>> {
        let raw = random_token();
        let now = Utc::now();
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-reset-{}", uuid::Uuid::new_v4()),
                vec![
                    fenced(
                        &format!(
                            "DELETE FROM password_resets WHERE user_id = ? AND {STILL_VERIFIED}"
                        ),
                        vec![batch::int(user_id), batch::int(user_id), batch::text(email)],
                        fence,
                    ),
                    fenced(
                        &format!(
                            "INSERT INTO password_resets (token_hash, user_id, email, \
                             expires_at, created_at) SELECT ?, ?, ?, ?, ? WHERE {STILL_VERIFIED}"
                        ),
                        vec![
                            batch::text(&hash_token(&raw)),
                            batch::int(user_id),
                            batch::text(email),
                            batch::text(
                                &(now + Duration::minutes(PASSWORD_RESET_MINUTES)).to_rfc3339(),
                            ),
                            batch::text(&now.to_rfc3339()),
                            batch::int(user_id),
                            batch::text(email),
                        ],
                        fence,
                    ),
                ],
            ))
            .await?;
        Ok((batch::rows_affected(&reply, 1) == 1).then(|| MintedToken::new(raw)))
    }

    /// The user a password-reset link is for, without using it up.
    ///
    /// # Arguments
    ///
    /// * `token` - The raw token from the link.
    /// * `now` - The current time.
    ///
    /// # Returns
    ///
    /// The user's id; `None` for a link that is unknown, used, expired, or
    /// for an address the user no longer has verified.
    ///
    /// # Errors
    ///
    /// An error when a read fails.
    pub async fn password_reset_user(
        &self,
        token: &PasswordResetToken,
        now: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        let Some(row) = password_resets::Entity::find_by_id(hash_token(token.expose()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        let live = DateTime::parse_from_rfc3339(&row.expires_at)
            .is_ok_and(|expires| expires.with_timezone(&Utc) > now);
        if !live {
            return Ok(None);
        }
        let verified = users::Entity::find_by_id(row.user_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .is_some_and(|user| {
                user.email.as_deref() == Some(row.email.as_str())
                    && user.email_verified_at.is_some()
            });
        Ok(verified.then_some(row.user_id))
    }

    /// Redeems a reset link: sets the new password, signs the account out
    /// everywhere and spends the link, all in one atomic batch.
    ///
    /// The first statement claims the link: it marks the row with
    /// [`RESET_CLAIM`] only while the link is unexpired, is for `user_id`,
    /// still names the account's verified address, the account is not
    /// disabled, and its second factor is still `factor`. Every later
    /// write is guarded only by that claim, and the claimed row is deleted
    /// last, so the claim is never visible outside the batch. When the
    /// claim matches nothing, nothing changes. A second redemption of the
    /// same link waits on the claimed row's lock and then finds it gone, so
    /// only one applies even under `READ COMMITTED`, and a factor enrolled
    /// after the claim cannot strand a half-applied reset.
    ///
    /// The operation id comes from the link and the new hash, so a retry
    /// after a lost reply resends the same batch, while a later attempt
    /// with another password gets an id of its own. When the retry finds
    /// the link already spent, the stored hash tells whether the lost
    /// batch was this one.
    ///
    /// # Arguments
    ///
    /// * `token` - The link's token.
    /// * `user_id` - The account the link was checked for.
    /// * `factor` - The second factor the account required when checked.
    /// * `password_hash` - The new Argon2id hash. Its salt makes it unique
    ///   to this attempt.
    /// * `now` - The current time.
    ///
    /// # Returns
    ///
    /// The redeemed reset, with how many portal sessions were signed out
    /// (unknown when the reply was lost and the retry found this reset
    /// already applied, since the count went with the reply), or `None`
    /// when the link no longer works for this account and factor (nothing
    /// was changed).
    ///
    /// # Errors
    ///
    /// An error when the batch fails; nothing was changed unless the error
    /// is [`LibraryError::Unavailable`] after a retry.
    pub async fn redeem_password_reset(
        &self,
        token: &PasswordResetToken,
        user_id: i64,
        factor: PasswordResetFactor,
        password_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<RedeemedReset>> {
        let token_hash = hash_token(token.expose());
        let factor_sql = match factor {
            PasswordResetFactor::None => {
                "users.totp_enabled = 0 AND NOT EXISTS (SELECT 1 FROM webauthn_credentials \
                 WHERE webauthn_credentials.user_id = users.id)"
            }
            PasswordResetFactor::Totp => "users.totp_enabled <> 0",
            PasswordResetFactor::Passkey => {
                "users.totp_enabled = 0 AND EXISTS (SELECT 1 FROM webauthn_credentials \
                 WHERE webauthn_credentials.user_id = users.id)"
            }
        };
        let claimed = || {
            (
                "EXISTS (SELECT 1 FROM password_resets \
                 WHERE password_resets.token_hash = ? AND password_resets.expires_at = ?)",
                [batch::text(&token_hash), batch::text(RESET_CLAIM)],
            )
        };
        let guarded = |sql: &str, mut leading: Vec<bookclerk_plugin_abi::DbValue>| {
            let (claim_sql, claim_params) = claimed();
            leading.extend(claim_params);
            batch::exec(&format!("{sql} AND {claim_sql}"), leading)
        };
        // `expires_at` is always written as UTC RFC 3339, so it compares
        // as text, and the claim marker sorts before every real expiry.
        let statements = vec![
            batch::exec(
                &format!(
                    "UPDATE password_resets SET expires_at = ? \
                     WHERE token_hash = ? AND user_id = ? AND expires_at > ? \
                     AND EXISTS (SELECT 1 FROM users WHERE users.id = password_resets.user_id \
                     AND users.email = password_resets.email \
                     AND users.email_verified_at IS NOT NULL AND users.status <> 'disabled' \
                     AND {factor_sql})"
                ),
                vec![
                    batch::text(RESET_CLAIM),
                    batch::text(&token_hash),
                    batch::int(user_id),
                    batch::text(&now.to_rfc3339()),
                ],
            ),
            guarded(
                "UPDATE users SET password_hash = ?, \
                 security_version = security_version + 1, updated_at = ? WHERE id = ?",
                vec![
                    batch::text(password_hash),
                    batch::text(&now.to_rfc3339()),
                    batch::int(user_id),
                ],
            ),
            guarded(
                "DELETE FROM operator_sessions WHERE elevated_from_user_id = ?",
                vec![batch::int(user_id)],
            ),
            guarded(
                "DELETE FROM portal_sessions WHERE identity_id IN \
                 (SELECT id FROM portal_identities WHERE user_id = ?)",
                vec![batch::int(user_id)],
            ),
            batch::exec(
                "DELETE FROM password_resets WHERE token_hash = ? AND expires_at = ?",
                vec![batch::text(&token_hash), batch::text(RESET_CLAIM)],
            ),
        ];
        // A hash of the stored hash and the new one, so the id names this
        // attempt without repeating a stored value in logs.
        let operation_id = format!(
            "password-reset-redeem-{}",
            &hash_token(&format!("{token_hash}:{password_hash}"))[..32]
        );
        let request = batch::request(&operation_id, statements);
        let redeemed = |reply: &bookclerk_plugin_abi::ExecuteReply| {
            (batch::rows_affected(reply, 4) == 1).then(|| RedeemedReset {
                revoked_sessions: Some(batch::rows_affected(reply, 3)),
            })
        };
        match self.execute_host_batch(request.clone()).await {
            Ok(reply) => Ok(redeemed(&reply)),
            Err(LibraryError::Unavailable(_)) => {
                let reply = self.execute_host_batch(request).await?;
                if let Some(done) = redeemed(&reply) {
                    return Ok(Some(done));
                }
                let applied =
                    self.get_user_password_hash(user_id).await?.as_deref() == Some(password_hash);
                Ok(applied.then_some(RedeemedReset {
                    revoked_sessions: None,
                }))
            }
            Err(err) => Err(err),
        }
    }

    /// Deletes account-mail records older than
    /// [`ACCOUNT_MAIL_RETENTION_DAYS`] that are no longer pending, and
    /// verification and password-reset links that expired.
    ///
    /// # Returns
    ///
    /// How many account-mail records were deleted.
    ///
    /// # Errors
    ///
    /// An error when the batch fails.
    pub async fn prune_account_mail(&self, now: DateTime<Utc>) -> Result<u64> {
        let cutoff = (now - Duration::days(ACCOUNT_MAIL_RETENTION_DAYS)).to_rfc3339();
        // Expiry is filtered in the database, so the step reads no rows
        // and stays within any result cap. `expires_at` is always written
        // here as UTC RFC 3339, so it compares as text.
        let statements = vec![
            batch::exec(
                "DELETE FROM account_mail WHERE state <> 'pending' AND created_at < ?",
                vec![batch::text(&cutoff)],
            ),
            batch::exec(
                "DELETE FROM email_verifications WHERE expires_at <= ?",
                vec![batch::text(&now.to_rfc3339())],
            ),
            batch::exec(
                "DELETE FROM password_resets WHERE expires_at <= ?",
                vec![batch::text(&now.to_rfc3339())],
            ),
        ];
        let reply = self
            .execute_host_batch(batch::request(
                &format!("account-mail-prune-{}", uuid::Uuid::new_v4()),
                statements,
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0))
    }
}
