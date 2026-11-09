//! The Rust SDK's row-at-a-time read (`PreparedStatement::iterate`) against
//! a plugin binding under every profile: it hands out exactly the rows
//! `all()` returns, in order, and fails exactly where `all()` fails.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use bookclerk_db_testkit::{BindingDatabase, Profile};
use bookclerk_plugin_abi::{
    DbValue, PluginError, PluginErrorCode, PluginMigration, PluginMigrationOp,
};
use bookclerk_plugin_sdk::DatabaseBinding;

/// The plugin's registration: one table of notes.
fn migrations() -> Vec<PluginMigration> {
    vec![PluginMigration {
        id: "notes-v1".into(),
        operations: vec![PluginMigrationOp::Schema(
            "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL)".into(),
        )],
    }]
}

/// Writes notes `0..count`, each with a `body_len`-byte body, in batches
/// `chunk()` sizes to the profile.
async fn write_notes(db: &DatabaseBinding, count: i64, body_len: usize) {
    let body = "n".repeat(body_len);
    let statements = (0..count)
        .map(|id| {
            db.prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
                .bind(vec![DbValue::Int64(id), DbValue::Text(body.clone())])
                .as_run()
        })
        .collect();
    for batch in db.chunk(statements, None).await.unwrap() {
        db.batch(batch, None).await.unwrap();
    }
}

/// Every row `iterate()` hands out, or the error it stops with.
async fn iterate_all(db: &DatabaseBinding) -> Result<Vec<HashMap<String, DbValue>>, PluginError> {
    let mut stream = db
        .prepare("SELECT id, body FROM notes ORDER BY id")
        .iterate(None);
    let mut rows = Vec::new();
    while let Some(row) = stream.next().await {
        rows.push(row?);
    }
    Ok(rows)
}

#[tokio::test]
async fn iterate_matches_all_under_every_profile() {
    for profile in Profile::ALL {
        // Past every profile's inline result cap, so a streaming profile
        // must stream and the minimum profile (one row, no stream) fails.
        let (count, body_len) = match profile {
            Profile::FirstParty | Profile::D1 => (1_500, 400),
            Profile::Minimum => (3, 16),
        };
        let unit = BindingDatabase::open(profile, migrations()).await.unwrap();
        let db = DatabaseBinding::from_session(Arc::new(unit));
        write_notes(&db, count, body_len).await;

        let all = db
            .prepare("SELECT id, body FROM notes ORDER BY id")
            .all(None)
            .await
            .map(|result| result.results.unwrap_or_default());
        let iterated = iterate_all(&db).await;
        match (all, iterated) {
            (Ok(all), Ok(iterated)) => {
                assert_eq!(
                    iterated.len(),
                    usize::try_from(count).unwrap(),
                    "{profile:?}"
                );
                assert_eq!(iterated, all, "{profile:?}");
            }
            (Err(all), Err(iterated)) => {
                assert_eq!(iterated.code, PluginErrorCode::LimitExceeded, "{profile:?}");
                assert_eq!(iterated.limit(), all.limit(), "{profile:?}");
            }
            (all, iterated) => panic!("{profile:?}: all() {all:?} but iterate() {iterated:?}"),
        }
        assert_eq!(
            matches!(profile, Profile::Minimum),
            iterate_all(&db).await.is_err(),
            "{profile:?}: only the minimum profile refuses the read"
        );

        // One row fits every profile.
        let mut one = db
            .prepare("SELECT id, body FROM notes WHERE id = ?")
            .bind(vec![DbValue::Int64(1)])
            .iterate(None);
        let row = one.next().await.unwrap().unwrap();
        assert_eq!(row.get("id"), Some(&DbValue::Int64(1)), "{profile:?}");
        assert!(one.next().await.is_none(), "{profile:?}");
    }
}
