//! Retention for the tables no other duty prunes: expired credentials,
//! spent invitation and enrollment tokens, finished uploads, abandoned
//! storage-scan inventories, and old audit rows.
//!
//! Each [`RecordPruneStep`] is one bounded delete keyed on a timestamp the
//! table already carries, so a step needs no index beyond the table's own
//! and no read of the rows it removes. The windows are fixed here rather
//! than configured: each is far longer than the row can still be used (a
//! token past its expiry, a revoked refresh token, an upload past its
//! token's life), and the grace periods only keep recent history visible.
//!
//! Timestamps are RFC 3339 text in UTC, compared as strings. The writers
//! of these tables use either `+00:00` or `Z` suffixes; the two differ only
//! after the seconds field, which no day-sized window can notice.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};

use super::LibraryStore;
use crate::entities::{
    claim_tickets, configuration_audit, configuration_changes, edition_files,
    node_enrollment_links, node_upload_sessions, oidc_auth_codes, oidc_refresh_tokens,
    portal_sessions, security_audit_events, user_invites,
};
use crate::error::{LibraryError, Result};

/// Days an expired claim ticket, user invite, or node enrollment link stays
/// listed (as expired or redeemed) before it is deleted.
pub const TOKEN_HISTORY_DAYS: i64 = 30;

/// Days a committed or aborted host upload is kept past its token's expiry.
///
/// No request can use it after expiry; the day only spares a commit replay
/// racing the expiry a confusing "unknown upload". Open uploads are never
/// deleted here: an abort still needs the row to tell the receiving node
/// which copies to delete.
pub const UPLOAD_HISTORY_DAYS: i64 = 1;

/// Days without a heartbeat after which a jobless storage-scan generation
/// (a CLI scan) is treated as abandoned.
pub const STORAGE_SCAN_STALE_DAYS: i64 = 1;

/// Days `security_audit_events` and `configuration_audit` rows are kept.
pub const AUDIT_RETENTION_DAYS: i64 = 365;

/// Days `configuration_changes` notices are kept. They only signal that a
/// document changed; the document and its audit row hold the content.
pub const CONFIGURATION_CHANGE_RETENTION_DAYS: i64 = 30;

/// One delete of [`LibraryStore::prune_record_step`], in run order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordPruneStep {
    /// OIDC authorization codes past their expiry (they are single-use
    /// and minutes long; consumed codes are refused by `consumed_at`
    /// until then).
    OidcAuthCodes,
    /// OIDC refresh tokens that expired or were revoked; neither is ever
    /// accepted again.
    OidcRefreshTokens,
    /// User portal sessions past their expiry.
    PortalSessions,
    /// Claim tickets expired more than [`TOKEN_HISTORY_DAYS`] ago.
    ClaimTickets,
    /// User invites expired more than [`TOKEN_HISTORY_DAYS`] ago.
    UserInvites,
    /// Node enrollment links expired more than [`TOKEN_HISTORY_DAYS`] ago.
    EnrollmentLinks,
    /// Committed or aborted uploads expired more than
    /// [`UPLOAD_HISTORY_DAYS`] ago.
    UploadSessions,
    /// Storage-scan generations whose job is terminal or gone, and jobless
    /// ones silent for [`STORAGE_SCAN_STALE_DAYS`], with their rows.
    StorageScans,
    /// Security audit events older than [`AUDIT_RETENTION_DAYS`].
    SecurityAudit,
    /// Configuration audit rows older than [`AUDIT_RETENTION_DAYS`].
    ConfigurationAudit,
    /// Configuration change notices older than
    /// [`CONFIGURATION_CHANGE_RETENTION_DAYS`].
    ConfigurationChanges,
}

impl RecordPruneStep {
    /// Every step, in the order a maintenance pass runs them.
    pub const ALL: [Self; 11] = [
        Self::OidcAuthCodes,
        Self::OidcRefreshTokens,
        Self::PortalSessions,
        Self::ClaimTickets,
        Self::UserInvites,
        Self::EnrollmentLinks,
        Self::UploadSessions,
        Self::StorageScans,
        Self::SecurityAudit,
        Self::ConfigurationAudit,
        Self::ConfigurationChanges,
    ];
}

/// RFC 3339 text for `days` before `now`.
fn days_before(now: DateTime<Utc>, days: i64) -> String {
    (now - Duration::days(days)).to_rfc3339()
}

impl LibraryStore {
    /// Runs one retention delete as of `now` and answers how many rows
    /// (for [`RecordPruneStep::StorageScans`], generations) it removed.
    ///
    /// # Arguments
    ///
    /// * `step` - Which table and rule to apply.
    /// * `now` - The instant the windows are measured back from; the
    ///   maintenance duty passes the current time, tests a fixed one.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read or delete fails; rows the
    /// step had not reached yet are left for the next pass.
    pub async fn prune_record_step(
        &self,
        step: RecordPruneStep,
        now: DateTime<Utc>,
    ) -> Result<u64> {
        let now_text = now.to_rfc3339();
        let token_cutoff = || days_before(now, TOKEN_HISTORY_DAYS);
        let audit_cutoff = || days_before(now, AUDIT_RETENTION_DAYS);
        let deleted = match step {
            RecordPruneStep::OidcAuthCodes => {
                oidc_auth_codes::Entity::delete_many()
                    .filter(oidc_auth_codes::Column::ExpiresAt.lte(now_text))
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::OidcRefreshTokens => {
                oidc_refresh_tokens::Entity::delete_many()
                    .filter(
                        Condition::any()
                            .add(oidc_refresh_tokens::Column::ExpiresAt.lte(now_text))
                            .add(oidc_refresh_tokens::Column::RevokedAt.is_not_null()),
                    )
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::PortalSessions => {
                portal_sessions::Entity::delete_many()
                    .filter(portal_sessions::Column::ExpiresAt.lte(now_text))
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::ClaimTickets => {
                claim_tickets::Entity::delete_many()
                    .filter(claim_tickets::Column::ExpiresAt.lte(token_cutoff()))
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::UserInvites => {
                user_invites::Entity::delete_many()
                    .filter(user_invites::Column::ExpiresAt.lte(token_cutoff()))
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::EnrollmentLinks => {
                node_enrollment_links::Entity::delete_many()
                    .filter(node_enrollment_links::Column::ExpiresAt.lte(token_cutoff()))
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::UploadSessions => {
                // A branch's catalog upload stays while a file record names
                // it as writer: it is how those files are known to be that
                // branch's (detached counts, Remove, and redelivery).
                let referenced = sea_orm::sea_query::Query::select()
                    .column(edition_files::Column::Writer)
                    .from(edition_files::Entity)
                    .and_where(edition_files::Column::Writer.is_not_null())
                    .to_owned();
                node_upload_sessions::Entity::delete_many()
                    .filter(node_upload_sessions::Column::State.is_in(["committed", "aborted"]))
                    .filter(
                        Condition::any()
                            .add(
                                node_upload_sessions::Column::JobId
                                    .ne(bookclerk_node_proto::CATALOG_ITEM),
                            )
                            .add(
                                node_upload_sessions::Column::UploadId.not_in_subquery(referenced),
                            ),
                    )
                    .filter(
                        node_upload_sessions::Column::ExpiresAt
                            .lte(days_before(now, UPLOAD_HISTORY_DAYS)),
                    )
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::StorageScans => {
                return self
                    .storage_scan_reclaim_abandoned(&days_before(now, STORAGE_SCAN_STALE_DAYS))
                    .await;
            }
            RecordPruneStep::SecurityAudit => {
                security_audit_events::Entity::delete_many()
                    .filter(security_audit_events::Column::At.lte(audit_cutoff()))
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::ConfigurationAudit => {
                configuration_audit::Entity::delete_many()
                    .filter(configuration_audit::Column::RecordedAt.lte(audit_cutoff()))
                    .exec(&self.db)
                    .await
            }
            RecordPruneStep::ConfigurationChanges => {
                configuration_changes::Entity::delete_many()
                    .filter(
                        configuration_changes::Column::CommittedAt
                            .lte(days_before(now, CONFIGURATION_CHANGE_RETENTION_DAYS)),
                    )
                    .exec(&self.db)
                    .await
            }
        };
        Ok(deleted.map_err(LibraryError::Orm)?.rows_affected)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, Utc};
    use sea_orm::{ConnectionTrait, EntityTrait, PaginatorTrait};

    use super::RecordPruneStep;
    use crate::entities::{
        claim_tickets, configuration_audit, configuration_changes, node_enrollment_links,
        node_upload_sessions, oidc_auth_codes, oidc_refresh_tokens, portal_sessions,
        security_audit_events, storage_scan_generations, user_invites,
    };
    use crate::test_support::{sqlite::memory, Profile};

    /// The fixed instant every window is measured back from.
    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// RFC 3339 text for `days` from [`now`] (negative is the past).
    fn at(days: i64) -> String {
        (now() + Duration::days(days)).to_rfc3339()
    }

    /// Inserts one row that each step must delete and one it must keep.
    async fn seed(db: &impl ConnectionTrait) {
        let t = at(-1000);
        let statements = [
            format!(
                "INSERT INTO users (id, role, created_at, updated_at) VALUES (1, 'member', '{t}', '{t}')"
            ),
            format!(
                "INSERT INTO portal_identities (id, provider, external_user_id, user_id, created_at) \
                 VALUES (1, 'local', 'u1', 1, '{t}')"
            ),
            format!(
                "INSERT INTO nodes (node_id, cluster_id, created_at, incarnation, heartbeat_at, \
                 software_version, schema_state, compatible) \
                 VALUES ('n1', 'c1', '{t}', 'i', '{t}', 'v', 's', 1)"
            ),
            // Codes: expired goes, live stays.
            format!(
                "INSERT INTO oidc_auth_codes (code_hash, client_id, user_id, redirect_uri, \
                 code_challenge, code_challenge_method, scope, expires_at, created_at) VALUES \
                 ('c-old', 'cl', 1, 'r', 'x', 'S256', 's', '{}', '{t}'), \
                 ('c-live', 'cl', 1, 'r', 'x', 'S256', 's', '{}', '{t}')",
                at(0),
                at(1)
            ),
            // Refresh tokens: expired and revoked go, live stays.
            format!(
                "INSERT INTO oidc_refresh_tokens (token_hash, client_id, user_id, scope, \
                 expires_at, revoked_at, created_at) VALUES \
                 ('r-old', 'cl', 1, 's', '{}', NULL, '{t}'), \
                 ('r-revoked', 'cl', 1, 's', '{}', '{t}', '{t}'), \
                 ('r-live', 'cl', 1, 's', '{}', NULL, '{t}')",
                at(-1),
                at(30),
                at(30)
            ),
            format!(
                "INSERT INTO portal_sessions (token_hash, identity_id, expires_at, created_at) VALUES \
                 ('p-old', 1, '{}', '{t}'), ('p-live', 1, '{}', '{t}')",
                at(-1),
                at(1)
            ),
            // Tokens keep TOKEN_HISTORY_DAYS of history past expiry.
            format!(
                "INSERT INTO claim_tickets (token_hash, expires_at, created_by, created_at) VALUES \
                 ('t-old', '{}', 'op', '{t}'), ('t-recent', '{}', 'op', '{t}')",
                at(-31),
                at(-29)
            ),
            format!(
                "INSERT INTO user_invites (token_hash, role, expires_at, redeemed_at, created_by, \
                 created_at) VALUES \
                 ('i-old', 'member', '{}', '{t}', 'op', '{t}'), \
                 ('i-recent', 'member', '{}', NULL, 'op', '{t}')",
                at(-31),
                at(-29)
            ),
            format!(
                "INSERT INTO node_enrollment_links (link_id, token_hash, for_user_id, created_by, \
                 created_at, expires_at) VALUES \
                 ('l-old', 'lh-old', 1, 'op', '{t}', '{}'), \
                 ('l-recent', 'lh-recent', 1, 'op', '{t}', '{}')",
                at(-31),
                at(-29)
            ),
            // Uploads: finished and expired a day ago go; an open one and a
            // recently expired one stay.
            format!(
                "INSERT INTO node_upload_sessions (upload_id, token_hash, node_id, job_id, \
                 lease_generation, receiving_node_id, book_uuid, primary_key, manifest_json, \
                 state, created_at, expires_at) VALUES \
                 ('u-committed', 'uh1', 'n1', 'j', 1, 'n0', 'b', 'k', '{{}}', 'committed', '{t}', '{old}'), \
                 ('u-aborted', 'uh2', 'n1', 'j', 1, 'n0', 'b', 'k', '{{}}', 'aborted', '{t}', '{old}'), \
                 ('u-open', 'uh3', 'n1', 'j', 1, 'n0', 'b', 'k', '{{}}', 'open', '{t}', '{old}'), \
                 ('u-recent', 'uh4', 'n1', 'j', 1, 'n0', 'b', 'k', '{{}}', 'committed', '{t}', '{recent}'), \
                 ('u-catalog', 'uh5', 'n1', 'catalog', 1, 'n0', 'h', 'k', '{{}}', 'committed', '{t}', '{old}'), \
                 ('u-catalog-gone', 'uh6', 'n1', 'catalog', 1, 'n0', 'h', 'k', '{{}}', 'committed', '{t}', '{old}')",
                old = at(-2),
                recent = (now() - Duration::hours(12)).to_rfc3339()
            ),
            format!(
                "INSERT INTO edition_files (destination, storage_key, owner_kind, owner_id, role, \
                 lossless, writer, written_at) VALUES \
                 ('local', 'A/b.m4b', 'edition', 'e1', 'audio', 0, 'u-catalog', '{t}')"
            ),
            // Scans: a jobless generation silent past the window goes, a
            // fresh one stays.
            format!(
                "INSERT INTO storage_scan_generations (scan_id, instance_id, updated_at) VALUES \
                 ('s-stale', 'local', '{}'), ('s-fresh', 'local', '{}')",
                at(-2),
                (now() - Duration::hours(1)).to_rfc3339()
            ),
            "INSERT INTO storage_scan_rows (scan_id, kind, identity, object_key) VALUES \
             ('s-stale', 'object', '', 'a.m4b'), ('s-fresh', 'object', '', 'b.m4b')"
                .to_string(),
            format!(
                "INSERT INTO security_audit_events (at, actor, action) VALUES \
                 ('{}', 'op', 'old'), ('{}', 'op', 'recent')",
                at(-366),
                at(-364)
            ),
            format!(
                "INSERT INTO configuration_audit (scope_type, scope_id, namespace, schema_version, \
                 revision, actor, recorded_at) VALUES \
                 ('cluster', '', 'host.events', 1, 1, 'op', '{}'), \
                 ('cluster', '', 'host.events', 1, 2, 'op', '{}')",
                at(-366),
                at(-364)
            ),
            format!(
                "INSERT INTO configuration_changes (scope_type, scope_id, namespace, revision, \
                 committed_at) VALUES \
                 ('cluster', '', 'host.events', 1, '{}'), \
                 ('cluster', '', 'host.events', 2, '{}')",
                at(-31),
                at(-29)
            ),
        ];
        for sql in statements {
            db.execute_unprepared(&sql)
                .await
                .unwrap_or_else(|err| panic!("{sql}: {err}"));
        }
    }

    #[tokio::test]
    async fn each_step_deletes_only_rows_past_its_window_under_every_profile() {
        for profile in Profile::ALL {
            let kit = memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            seed(kit.connection()).await;
            let mut removed = Vec::new();
            for step in RecordPruneStep::ALL {
                removed.push(store.prune_record_step(step, now()).await.unwrap());
            }
            assert_eq!(removed, [1, 2, 1, 1, 1, 1, 3, 1, 1, 1, 1], "{name}");
            assert_eq!(kit.refused_begins(), 0, "{name}");

            let db = kit.connection();
            let codes: Vec<String> = oidc_auth_codes::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.code_hash)
                .collect();
            assert_eq!(codes, ["c-live"], "{name}");
            let tokens: Vec<String> = oidc_refresh_tokens::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.token_hash)
                .collect();
            assert_eq!(tokens, ["r-live"], "{name}");
            let sessions: Vec<String> = portal_sessions::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.token_hash)
                .collect();
            assert_eq!(sessions, ["p-live"], "{name}");
            let tickets: Vec<String> = claim_tickets::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.token_hash)
                .collect();
            assert_eq!(tickets, ["t-recent"], "{name}");
            let invites: Vec<String> = user_invites::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.token_hash)
                .collect();
            assert_eq!(invites, ["i-recent"], "{name}");
            let links: Vec<String> = node_enrollment_links::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.link_id)
                .collect();
            assert_eq!(links, ["l-recent"], "{name}");
            let mut uploads: Vec<String> = node_upload_sessions::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.upload_id)
                .collect();
            uploads.sort();
            // A catalog upload a file record still names stays.
            assert_eq!(uploads, ["u-catalog", "u-open", "u-recent"], "{name}");
            let scans: Vec<String> = storage_scan_generations::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.scan_id)
                .collect();
            assert_eq!(scans, ["s-fresh"], "{name}");
            assert_eq!(store.storage_scan_generation_count().await.unwrap(), 1);
            let audit: Vec<String> = security_audit_events::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.action)
                .collect();
            assert_eq!(audit, ["recent"], "{name}");
            let revisions: Vec<i64> = configuration_audit::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.revision)
                .collect();
            assert_eq!(revisions, [2], "{name}");
            let changes: Vec<i64> = configuration_changes::Entity::find()
                .all(db)
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.revision)
                .collect();
            assert_eq!(changes, [2], "{name}");

            // A second pass finds nothing left to remove.
            for step in RecordPruneStep::ALL {
                assert_eq!(
                    store.prune_record_step(step, now()).await.unwrap(),
                    0,
                    "{name}"
                );
            }
            assert_eq!(
                configuration_changes::Entity::find()
                    .count(db)
                    .await
                    .unwrap(),
                1
            );
        }
    }
}
