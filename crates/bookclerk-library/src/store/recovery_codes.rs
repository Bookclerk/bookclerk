//! Second-factor recovery codes (`docs/adr/notifications.md` decision 8):
//! issuing a set, reading one code's hash, and dropping the set when the
//! account no longer has a second factor. Using a code happens inside a
//! password reset's redemption
//! ([`LibraryStore::redeem_password_reset`]).

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};

use super::LibraryStore;
use crate::control_plane::batch;
use crate::entities::recovery_codes;
use crate::error::{LibraryError, Result};
use crate::recovery_code::RecoveryCode;

/// SQL true while user `?` has a second factor: an authenticator app or at
/// least one passkey. Recovery codes exist only alongside one.
pub(crate) const HAS_SECOND_FACTOR: &str = "EXISTS (SELECT 1 FROM users WHERE users.id = ? \
     AND (users.totp_enabled <> 0 OR EXISTS (SELECT 1 FROM webauthn_credentials \
     WHERE webauthn_credentials.user_id = users.id)))";

/// How a new set of codes meets the user's current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryCodeIssue {
    /// Store the set only when the user has no codes: the set a first
    /// second factor comes with.
    First,
    /// Replace whatever codes the user has.
    Replace,
}

impl LibraryStore {
    /// Stores a new set of recovery codes for `user_id`, in one atomic
    /// write with an audit row (`recovery_codes_issued`).
    ///
    /// The set is stored only while the user has a second factor, and for
    /// [`RecoveryCodeIssue::First`] only when they have no codes yet.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user the codes recover.
    /// * `codes` - The set, from [`RecoveryCode::generate_set`]; it must
    ///   not be empty and its selectors must differ.
    /// * `issue` - Whether the set replaces the current one.
    /// * `now` - When the set was made.
    ///
    /// # Returns
    ///
    /// Whether the set was stored. When it was not, nothing changed.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] for an empty set, an error when a code
    /// cannot be hashed, or when the write fails. A lost reply
    /// ([`LibraryError::Unavailable`]) may still have stored the set; the
    /// patron can make another.
    pub async fn issue_recovery_codes(
        &self,
        user_id: i64,
        codes: &[RecoveryCode],
        issue: RecoveryCodeIssue,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let Some(first) = codes.first() else {
            return Err(LibraryError::Invalid(
                "a recovery code set must not be empty".into(),
            ));
        };
        let created_at = now.to_rfc3339();
        let hashes = codes
            .iter()
            .map(RecoveryCode::hash)
            .collect::<Result<Vec<_>>>()?;
        // Statements after the first insert run only when it stored its
        // row: its hash is salted, so no other row can match it.
        let inserted = || {
            (
                "EXISTS (SELECT 1 FROM recovery_codes WHERE user_id = ? AND selector = ? \
                 AND code_hash = ?)",
                vec![
                    batch::int(user_id),
                    batch::text(first.selector()),
                    batch::text(&hashes[0]),
                ],
            )
        };
        let mut statements = Vec::with_capacity(codes.len() + 2);
        match issue {
            RecoveryCodeIssue::Replace => statements.push(batch::exec(
                &format!("DELETE FROM recovery_codes WHERE user_id = ? AND {HAS_SECOND_FACTOR}"),
                vec![batch::int(user_id), batch::int(user_id)],
            )),
            RecoveryCodeIssue::First => {}
        }
        let first_index = statements.len();
        let none_yet = match issue {
            RecoveryCodeIssue::First => {
                " AND NOT EXISTS (SELECT 1 FROM recovery_codes WHERE user_id = ?)"
            }
            RecoveryCodeIssue::Replace => "",
        };
        let mut first_params = vec![
            batch::int(user_id),
            batch::text(first.selector()),
            batch::text(&hashes[0]),
            batch::text(&created_at),
            batch::int(user_id),
        ];
        if issue == RecoveryCodeIssue::First {
            first_params.push(batch::int(user_id));
        }
        statements.push(batch::exec(
            &format!(
                "INSERT INTO recovery_codes (user_id, selector, code_hash, created_at) \
                 SELECT ?, ?, ?, ? WHERE {HAS_SECOND_FACTOR}{none_yet}"
            ),
            first_params,
        ));
        for (code, hash) in codes.iter().zip(&hashes).skip(1) {
            let (guard, mut params) = inserted();
            let mut leading = vec![
                batch::int(user_id),
                batch::text(code.selector()),
                batch::text(hash),
                batch::text(&created_at),
            ];
            leading.append(&mut params);
            statements.push(batch::exec(
                &format!(
                    "INSERT INTO recovery_codes (user_id, selector, code_hash, created_at) \
                     SELECT ?, ?, ?, ? WHERE {guard}"
                ),
                leading,
            ));
        }
        let (guard, mut params) = inserted();
        let mut leading = vec![
            batch::text(&created_at),
            batch::text(&format!("user:{user_id}")),
        ];
        leading.append(&mut params);
        statements.push(batch::exec(
            &format!(
                "INSERT INTO security_audit_events (at, actor, action, detail_json) \
                 SELECT ?, ?, 'recovery_codes_issued', NULL WHERE {guard}"
            ),
            leading,
        ));
        let operation_id = format!("recovery-codes-{user_id}-{}", uuid::Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(batch::rows_affected(&reply, first_index) == 1)
    }

    /// How many unused recovery codes `user_id` has.
    ///
    /// # Errors
    ///
    /// An error when the library cannot be read.
    pub async fn count_recovery_codes(&self, user_id: i64) -> Result<u64> {
        recovery_codes::Entity::find()
            .filter(recovery_codes::Column::UserId.eq(user_id))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// The stored hash of `user_id`'s unused code with this selector.
    ///
    /// # Errors
    ///
    /// An error when the library cannot be read.
    pub async fn recovery_code_hash(&self, user_id: i64, selector: &str) -> Result<Option<String>> {
        Ok(recovery_codes::Entity::find()
            .filter(recovery_codes::Column::UserId.eq(user_id))
            .filter(recovery_codes::Column::Selector.eq(selector))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(|row| row.code_hash))
    }

    /// Deletes `user_id`'s recovery codes when they no longer have a second
    /// factor, after an authenticator or passkey was removed.
    ///
    /// # Returns
    ///
    /// How many codes were deleted (none while a factor remains).
    ///
    /// # Errors
    ///
    /// An error when the write fails.
    pub async fn drop_recovery_codes_without_factor(&self, user_id: i64) -> Result<u64> {
        let operation_id = format!("recovery-codes-drop-{user_id}-{}", uuid::Uuid::new_v4());
        let reply = self
            .execute_host_batch(batch::request(
                &operation_id,
                vec![batch::exec(
                    &format!(
                        "DELETE FROM recovery_codes WHERE user_id = ? AND NOT {HAS_SECOND_FACTOR}"
                    ),
                    vec![batch::int(user_id), batch::int(user_id)],
                )],
            ))
            .await?;
        Ok(batch::rows_affected(&reply, 0))
    }

    /// Deletes the recovery codes of a user whose row is gone, after
    /// [`LibraryStore::delete_user`]; the user-delete plan has no room
    /// left under the smallest adapter's `maxStatements`. Codes of a user
    /// who still exists are left alone.
    ///
    /// # Errors
    ///
    /// An error when the write fails.
    pub(crate) async fn drop_recovery_codes_of_deleted_user(&self, user_id: i64) -> Result<()> {
        let operation_id = format!("recovery-codes-orphans-{user_id}-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(
                "DELETE FROM recovery_codes WHERE user_id = ? \
                 AND NOT EXISTS (SELECT 1 FROM users WHERE users.id = ?)",
                vec![batch::int(user_id), batch::int(user_id)],
            )],
        ))
        .await?;
        Ok(())
    }
}
