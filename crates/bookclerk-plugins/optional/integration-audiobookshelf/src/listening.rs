//! Sync AudioBookshelf media progress into `listening_progress`.

use std::collections::HashMap;

use bookclerk_integrations::{
    upsert_listening_snapshots, IntegrationError, ListeningProgressSnapshot, Result,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_sdk::{
    DatabaseBinding, DbValue, PluginError, PluginMigration, PluginMigrationOp, PreparedStatement,
};
use bookclerk_scan_delta::read_keyed;
use chrono::{DateTime, TimeZone, Utc};
use sha2::{Digest, Sha256};

use crate::client::{AbsApiClient, AbsMediaProgress};

/// `listening_progress.provider` value written for AudioBookshelf snapshots (`audiobookshelf`).
const PROVIDER: &str = "audiobookshelf";

/// `[[databases]]` binding the plugin keeps listening-sync memory in.
pub const LISTENING_STATE_BINDING: &str = "LISTENING_STATE";

/// How long a user's memory may skip unchanged rows before the next sync
/// sends that user's whole snapshot again. The resend picks up rows the host
/// could not store earlier (a book the library did not have yet, a person
/// not linked yet) and any write the host lost.
const FULL_RESEND_MS: i64 = 24 * 60 * 60 * 1000;

/// How long a user's memory is kept without a sync (a removed user ages
/// out), in milliseconds.
const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// What one ABS user's rows looked like when they were last sent.
#[derive(Debug, Default)]
struct UserMemory {
    /// When this user's whole snapshot was last sent (UTC Unix milliseconds).
    full_at_ms: i64,
    /// Fingerprint of each library item's progress row as last sent.
    items: HashMap<String, i64>,
}

/// The `database_migrations` registration for [`LISTENING_STATE_BINDING`].
///
/// Two tables: `abs_listening_items` holds the fingerprint of each progress
/// row as last sent (user, library item, fingerprint), and
/// `abs_listening_users` when each user's whole snapshot was last sent and
/// when the user was last synced. The `person` column holds a hash of the ABS
/// user id, never the id.
#[must_use]
pub fn migrations() -> Vec<PluginMigration> {
    vec![PluginMigration {
        id: "abs-listening-v1".into(),
        operations: vec![
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS abs_listening_items (\
                 person TEXT NOT NULL, \
                 item TEXT NOT NULL, \
                 fingerprint INTEGER NOT NULL, \
                 PRIMARY KEY (person, item))"
                    .into(),
            ),
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS abs_listening_users (\
                 person TEXT NOT NULL PRIMARY KEY, \
                 full_at_ms INTEGER NOT NULL, \
                 touched_at_ms INTEGER NOT NULL)"
                    .into(),
            ),
            PluginMigrationOp::Schema(
                "CREATE INDEX IF NOT EXISTS abs_listening_users_touched \
                 ON abs_listening_users (touched_at_ms)"
                    .into(),
            ),
        ],
    }]
}

/// The `database_migrations` answer for `binding`.
///
/// # Returns
///
/// [`migrations`] for [`LISTENING_STATE_BINDING`], nothing for any other
/// binding.
#[must_use]
pub fn migrations_for(binding: &str) -> Vec<PluginMigration> {
    if binding == LISTENING_STATE_BINDING {
        migrations()
    } else {
        Vec::new()
    }
}

/// Collect listening-progress snapshots from ABS without writing the library DB.
///
/// Used by both the in-process sync path and the external plugin guest RPC.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn collect_listening_snapshots(
    client: &AbsApiClient,
) -> Result<Vec<ListeningProgressSnapshot>> {
    let now_ms = Utc::now().timestamp_millis();
    let mut snapshots = Vec::new();
    for user in client.list_users().await? {
        if let Some((mut rows, _)) = collect_user(client, &user.id, None, now_ms).await {
            snapshots.append(&mut rows);
        }
    }
    Ok(snapshots)
}

/// Collect the listening-progress rows that changed since the last sync.
///
/// With `memory`, each user's progress rows are fingerprinted (progress,
/// position, duration, finished flag and last update). A row whose
/// fingerprint matches what the last sync sent is skipped, along with the
/// library item lookup it would need. Once a day per user
/// ([`FULL_RESEND_MS`]) every row is sent again. Without `memory` every row
/// is sent, as [`collect_listening_snapshots`] does.
///
/// The host upserts what it receives, so skipping a row it already holds
/// changes nothing it stores.
///
/// Every read and write is sized from the binding's limits (keyed pages
/// when the session does not stream, `chunk()`ed batches), so the same code
/// runs on every database the host may grant. Memory of users not synced
/// for seven days is dropped.
///
/// # Arguments
///
/// * `client` - The ABS API client.
/// * `memory` - The plugin's `LISTENING_STATE` database, when granted.
/// * `now` - The time of this sync.
///
/// # Errors
///
/// Returns an error when ABS cannot list its users, or a database call
/// fails.
pub async fn collect_listening_changes(
    client: &AbsApiClient,
    memory: Option<&DatabaseBinding>,
    now: DateTime<Utc>,
) -> Result<Vec<ListeningProgressSnapshot>> {
    let now_ms = now.timestamp_millis();
    if let Some(db) = memory {
        prune(db, now_ms).await.map_err(memory_error)?;
    }
    let mut snapshots = Vec::new();
    for user in client.list_users().await? {
        let key = key_segment(&user.id);
        let previous = match memory {
            Some(db) => read_memory(db, &key, now_ms).await.map_err(memory_error)?,
            None => None,
        };
        let Some((mut rows, next)) =
            collect_user(client, &user.id, previous.as_ref(), now_ms).await
        else {
            continue;
        };
        snapshots.append(&mut rows);
        if let Some(db) = memory {
            write_memory(db, &key, previous.as_ref(), &next, now_ms)
                .await
                .map_err(memory_error)?;
        }
    }
    Ok(snapshots)
}

/// One ABS user's progress rows that differ from `previous`, and the memory
/// to keep for the next sync; `None` when ABS cannot return the user (their
/// memory then stays as it was).
///
/// # Arguments
///
/// * `client` - The ABS API client.
/// * `user_id` - The ABS user.
/// * `previous` - What the last sync sent for this user, if it still counts.
/// * `now_ms` - The time of this sync (UTC Unix milliseconds).
async fn collect_user(
    client: &AbsApiClient,
    user_id: &str,
    previous: Option<&UserMemory>,
    now_ms: i64,
) -> Option<(Vec<ListeningProgressSnapshot>, UserMemory)> {
    let detail = match client.get_user(user_id).await {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(user_id = %user_id, error = %err, "ABS get_user failed");
            return None;
        }
    };
    let mut next = UserMemory {
        full_at_ms: previous.map_or(now_ms, |m| m.full_at_ms),
        items: HashMap::new(),
    };
    let mut snapshots = Vec::new();

    for prog in detail.media_progress {
        let fp = progress_fingerprint(&detail.id, &prog);
        next.items.insert(prog.library_item_id.clone(), fp);
        if previous.is_some_and(|m| m.items.get(&prog.library_item_id) == Some(&fp)) {
            continue;
        }
        let mut title = None;
        let mut authors = None;
        let mut asin = None;
        let mut isbn = None;

        match client.get_library_item(&prog.library_item_id).await {
            Ok(item) => {
                if let Some(meta) = item.media.and_then(|m| m.metadata) {
                    title = meta.title;
                    authors = meta.author_name;
                    asin = meta.asin.map(|s| s.to_ascii_uppercase());
                    isbn = meta.isbn;
                }
            }
            Err(err) => {
                tracing::debug!(
                    item_id = %prog.library_item_id,
                    error = %err,
                    "ABS get_library_item failed"
                );
            }
        }

        let last_listened_at = prog
            .last_update
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single());

        snapshots.push(ListeningProgressSnapshot {
            external_user_id: detail.id.clone(),
            external_item_id: prog.library_item_id.clone(),
            identity_id: None,
            title,
            authors,
            asin,
            isbn,
            progress: prog.progress,
            current_time_seconds: prog.current_time,
            duration_seconds: prog.duration,
            is_finished: prog.is_finished,
            last_listened_at,
        });
    }
    Some((snapshots, next))
}

/// Drops the memory of every user not synced for [`RETENTION_MS`].
///
/// # Errors
///
/// Returns the binding's error.
async fn prune(db: &DatabaseBinding, now_ms: i64) -> std::result::Result<(), PluginError> {
    let expired = DbValue::Int64(now_ms.saturating_sub(RETENTION_MS));
    let statements = vec![
        db.prepare(
            "DELETE FROM abs_listening_items WHERE person IN \
             (SELECT person FROM abs_listening_users WHERE touched_at_ms < ?)",
        )
        .bind(vec![expired.clone()])
        .as_run(),
        db.prepare("DELETE FROM abs_listening_users WHERE touched_at_ms < ?")
            .bind(vec![expired])
            .as_run(),
    ];
    write(db, statements).await
}

/// A user's memory, or `None` when there is none or its last whole send is
/// older than [`FULL_RESEND_MS`] (the next sync then sends that user's
/// whole snapshot).
///
/// # Errors
///
/// Returns the binding's error, and `internal` for a row of the wrong shape.
async fn read_memory(
    db: &DatabaseBinding,
    user: &str,
    now_ms: i64,
) -> std::result::Result<Option<UserMemory>, PluginError> {
    let row = db
        .prepare("SELECT full_at_ms FROM abs_listening_users WHERE person = ?")
        .bind(vec![text(user)])
        .first(None)
        .await?;
    let full_at_ms = match row.and_then(|r| r.into_iter().find(|(name, _)| name == "full_at_ms")) {
        Some((_, DbValue::Int64(at))) => at,
        Some(_) => {
            return Err(PluginError::internal(
                "listening user row has no full_at_ms",
            ))
        }
        None => return Ok(None),
    };
    if now_ms.saturating_sub(full_at_ms) >= FULL_RESEND_MS {
        return Ok(None);
    }
    let rows = read_keyed(
        db,
        "SELECT item, fingerprint FROM abs_listening_items \
         WHERE person = ? AND item > ? ORDER BY item LIMIT ?",
        vec![text(user)],
        "item",
    )
    .await?;
    let mut items = HashMap::with_capacity(rows.len());
    for row in rows {
        match (row.get("item"), row.get("fingerprint")) {
            (Some(DbValue::Text(item)), Some(DbValue::Int64(fp))) => {
                items.insert(item.clone(), *fp);
            }
            _ => {
                return Err(PluginError::internal(
                    "listening item row has the wrong shape",
                ))
            }
        }
    }
    Ok(Some(UserMemory { full_at_ms, items }))
}

/// Records what this sync sent for one user.
///
/// After a whole send (`previous` is `None`) the user's rows are replaced
/// and the whole-send time moves to `now_ms`; after a partial one only the
/// rows that changed are written. The user row is written last, so a sync
/// cut short leaves either the old whole-send time or none, and a row the
/// memory holds is never newer than what was sent.
///
/// # Errors
///
/// Returns the binding's error.
async fn write_memory(
    db: &DatabaseBinding,
    user: &str,
    previous: Option<&UserMemory>,
    next: &UserMemory,
    now_ms: i64,
) -> std::result::Result<(), PluginError> {
    let mut statements = Vec::new();
    if previous.is_none() {
        statements.push(
            db.prepare("DELETE FROM abs_listening_items WHERE person = ?")
                .bind(vec![text(user)])
                .as_run(),
        );
    }
    let mut items: Vec<(&String, &i64)> = next
        .items
        .iter()
        .filter(|(item, fp)| previous.is_none_or(|m| m.items.get(*item) != Some(*fp)))
        .collect();
    items.sort();
    for (item, fp) in items {
        if previous.is_some() {
            statements.push(
                db.prepare(
                    "UPDATE abs_listening_items SET fingerprint = ? \
                     WHERE person = ? AND item = ?",
                )
                .bind(vec![DbValue::Int64(*fp), text(user), text(item)])
                .as_run(),
            );
        }
        statements.push(
            db.prepare(
                "INSERT OR IGNORE INTO abs_listening_items (person, item, fingerprint) \
                 VALUES (?, ?, ?)",
            )
            .bind(vec![text(user), text(item), DbValue::Int64(*fp)])
            .as_run(),
        );
    }
    statements.push(
        db.prepare(
            "UPDATE abs_listening_users SET full_at_ms = ?, touched_at_ms = ? WHERE person = ?",
        )
        .bind(vec![
            DbValue::Int64(next.full_at_ms),
            DbValue::Int64(now_ms),
            text(user),
        ])
        .as_run(),
    );
    statements.push(
        db.prepare(
            "INSERT OR IGNORE INTO abs_listening_users (person, full_at_ms, touched_at_ms) \
             VALUES (?, ?, ?)",
        )
        .bind(vec![
            text(user),
            DbValue::Int64(next.full_at_ms),
            DbValue::Int64(now_ms),
        ])
        .as_run(),
    );
    write(db, statements).await
}

/// Runs `statements` in batches that fit one request each.
///
/// # Errors
///
/// Returns the binding's error.
async fn write(
    db: &DatabaseBinding,
    statements: Vec<PreparedStatement>,
) -> std::result::Result<(), PluginError> {
    for batch in db.chunk(statements, None).await? {
        db.batch(batch, None).await?;
    }
    Ok(())
}

/// A listening-memory database error as an integration error.
fn memory_error(err: PluginError) -> IntegrationError {
    IntegrationError::message(format!("listening memory: {err}"))
}

/// A text bind value.
fn text(value: &str) -> DbValue {
    DbValue::Text(value.to_owned())
}

/// Fingerprint of one progress row: what the host stores from it, keyed to
/// the user and item.
fn progress_fingerprint(user_id: &str, prog: &AbsMediaProgress) -> i64 {
    let mut hash = Sha256::new();
    hash.update(b"bookclerk-abs-listening-v1\0");
    let fields = serde_json::json!([
        user_id,
        prog.library_item_id,
        prog.progress,
        prog.current_time,
        prog.duration,
        prog.is_finished,
        prog.last_update,
    ]);
    hash.update(fields.to_string().as_bytes());
    let digest = hash.finalize();
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(first)
}

/// Key segment for an ABS user id: 16 hex digits of a hash.
fn key_segment(user_id: &str) -> String {
    use std::fmt::Write as _;
    let mut hash = Sha256::new();
    hash.update(b"bookclerk-abs-listening-key-v1\0");
    hash.update(user_id.as_bytes());
    hash.finalize()[..8]
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Pull ABS user media progress into the library DB.
///
/// Best-effort matches rows to `book_uuid` / `work_id` via ASIN, ISBN, or title.
/// Prefer calling this through [`bookclerk_integrations::Integration::sync_listening_progress`]
/// on the registered ABS adapter rather than from host binaries.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn sync_listening_progress(
    library: &LibraryStore,
    client: &AbsApiClient,
) -> Result<usize> {
    let snapshots = collect_listening_snapshots(client).await?;
    upsert_listening_snapshots(library, PROVIDER, &snapshots).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bookclerk_db_testkit::{BindingDatabase, Profile};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// A fresh `LISTENING_STATE` database under `profile`.
    async fn memory(profile: Profile) -> DatabaseBinding {
        let unit = BindingDatabase::open(profile, migrations()).await.unwrap();
        DatabaseBinding::from_session(Arc::new(unit))
    }

    /// ABS server with one user whose rows have the given progress values.
    async fn server(progress: &[f64]) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/users"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "users": [{ "id": "u1", "username": "ada" }]
            })))
            .mount(&server)
            .await;
        let rows: Vec<serde_json::Value> = progress
            .iter()
            .enumerate()
            .map(|(i, p)| {
                serde_json::json!({
                    "id": format!("m{i}"),
                    "libraryItemId": format!("li{i:05}"),
                    "progress": p,
                    "lastUpdate": 1,
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/users/u1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "u1",
                "username": "ada",
                "mediaProgress": rows,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex("^/api/items/li[0-9]+$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "item",
                "media": { "metadata": { "title": "item", "asin": "b0test" } }
            })))
            .mount(&server)
            .await;
        server
    }

    fn items(rows: &[ListeningProgressSnapshot]) -> Vec<&str> {
        rows.iter().map(|r| r.external_item_id.as_str()).collect()
    }

    /// Item lookups the server answered so far.
    async fn item_lookups(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().starts_with("/api/items/"))
            .count()
    }

    async fn count(db: &DatabaseBinding, table: &str) -> i64 {
        let row = db
            .prepare(format!("SELECT count(*) AS n FROM {table}"))
            .first(None)
            .await
            .unwrap()
            .unwrap();
        match row.into_iter().find(|(name, _)| name == "n") {
            Some((_, DbValue::Int64(n))) => n,
            other => panic!("count came back as {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_second_sync_sends_only_changed_rows() {
        for profile in Profile::ALL {
            let db = memory(profile).await;
            let now = Utc.timestamp_millis_opt(10_000_000).single().unwrap();
            let first_server = server(&[0.1, 0.5]).await;
            let client = AbsApiClient::new(first_server.uri(), "k").unwrap();
            let first = collect_listening_changes(&client, Some(&db), now)
                .await
                .unwrap();
            assert_eq!(items(&first), ["li00000", "li00001"], "{profile:?}");
            assert_eq!(first[0].asin.as_deref(), Some("B0TEST"));

            // Nothing changed: nothing is sent and no item is looked up.
            let quiet = collect_listening_changes(&client, Some(&db), now)
                .await
                .unwrap();
            assert!(quiet.is_empty(), "{profile:?}");
            assert_eq!(item_lookups(&first_server).await, 2);

            // One row moved on, twice: each change is sent once.
            for p in [0.9, 0.95] {
                let moved_server = server(&[0.1, p]).await;
                let client = AbsApiClient::new(moved_server.uri(), "k").unwrap();
                let moved = collect_listening_changes(&client, Some(&db), now)
                    .await
                    .unwrap();
                assert_eq!(items(&moved), ["li00001"], "{profile:?}");
                assert_eq!(item_lookups(&moved_server).await, 1);
                let again = collect_listening_changes(&client, Some(&db), now)
                    .await
                    .unwrap();
                assert!(again.is_empty(), "{profile:?}");
            }

            // A day after the last full send, every row goes again, and the
            // memory holds only what ABS lists now.
            let later = now + chrono::Duration::milliseconds(FULL_RESEND_MS);
            let shrunk = server(&[0.1]).await;
            let client = AbsApiClient::new(shrunk.uri(), "k").unwrap();
            let resend = collect_listening_changes(&client, Some(&db), later)
                .await
                .unwrap();
            assert_eq!(items(&resend), ["li00000"], "{profile:?}");
            assert_eq!(count(&db, "abs_listening_items").await, 1);
        }
    }

    #[tokio::test]
    async fn many_rows_are_remembered_across_paged_reads() {
        for profile in Profile::ALL {
            let db = memory(profile).await;
            let now = Utc::now();
            let progress: Vec<f64> = (0..300).map(|i| f64::from(i) / 1000.0).collect();
            let abs = server(&progress).await;
            let client = AbsApiClient::new(abs.uri(), "k").unwrap();
            let first = collect_listening_changes(&client, Some(&db), now)
                .await
                .unwrap();
            assert_eq!(first.len(), 300, "{profile:?}");
            let quiet = collect_listening_changes(&client, Some(&db), now)
                .await
                .unwrap();
            assert!(quiet.is_empty(), "{profile:?}");
        }
    }

    #[tokio::test]
    async fn users_not_synced_for_a_week_are_forgotten() {
        for profile in Profile::ALL {
            let db = memory(profile).await;
            let now = Utc::now();
            let abs = server(&[0.1]).await;
            let client = AbsApiClient::new(abs.uri(), "k").unwrap();
            collect_listening_changes(&client, Some(&db), now)
                .await
                .unwrap();
            db.prepare("UPDATE abs_listening_users SET touched_at_ms = 0")
                .run(None)
                .await
                .unwrap();
            prune(&db, now.timestamp_millis()).await.unwrap();
            assert_eq!(count(&db, "abs_listening_items").await, 0, "{profile:?}");
            assert_eq!(count(&db, "abs_listening_users").await, 0, "{profile:?}");
        }
    }

    #[tokio::test]
    async fn without_memory_every_row_is_sent() {
        let abs = server(&[0.1, 0.5]).await;
        let client = AbsApiClient::new(abs.uri(), "k").unwrap();
        let now = Utc::now();
        for _ in 0..2 {
            let rows = collect_listening_changes(&client, None, now).await.unwrap();
            assert_eq!(items(&rows), ["li00000", "li00001"]);
        }
        assert_eq!(
            items(&collect_listening_snapshots(&client).await.unwrap()),
            ["li00000", "li00001"]
        );
    }

    #[test]
    fn the_migrations_answer_for_the_listening_binding_only() {
        assert_eq!(migrations_for(LISTENING_STATE_BINDING).len(), 1);
        assert!(migrations_for("SCAN_STATE").is_empty());
    }
}
