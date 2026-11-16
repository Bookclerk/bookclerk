//! Which Audiobookshelf users the host has already been told about.
//!
//! The host asks for new users (`poll_events`) on its own schedule, and the
//! guest may sleep between asks. So the set of users already reported is
//! kept in the plugin's [`LISTENING_STATE_BINDING`] database, as a hash of
//! each ABS user id, and a guest started later compares the server's user
//! list against it. Without the database the set lives in memory and is
//! seeded again whenever the guest starts, so a user created while the
//! guest slept is not reported.
//!
//! [`LISTENING_STATE_BINDING`]: crate::LISTENING_STATE_BINDING

use std::collections::HashSet;

use bookclerk_plugin_sdk::{
    DatabaseBinding, DbValue, PluginError, PluginMigration, PluginMigrationOp,
};
use bookclerk_scan_delta::read_keyed;
use sha2::{Digest, Sha256};

use crate::client::AbsUser;

/// The `person` row written after a complete seed. It sorts before every
/// key (keys are lowercase hex), and a set without it has not been seeded:
/// a seed cut short is redone in full, so no user the server already had is
/// ever reported.
const SEEDED: &str = "!seeded";

/// The `database_migrations` entry that adds the user-watch table.
///
/// `abs_watch_users` holds one row per ABS user already reported or seeded,
/// plus a marker row once the first seed completed; its `person` column is
/// a hash of the ABS user id, never the id.
#[must_use]
pub fn migration() -> PluginMigration {
    PluginMigration {
        id: "abs-user-watch-v1".into(),
        operations: vec![PluginMigrationOp::Schema(
            "CREATE TABLE IF NOT EXISTS abs_watch_users (person TEXT NOT NULL PRIMARY KEY)".into(),
        )],
    }
}

/// Where the reported-user set lives for one call.
pub(crate) struct UserWatch<'a> {
    /// The plugin database, when the operator granted it.
    db: Option<&'a DatabaseBinding>,
    /// The in-memory set, used only without `db`.
    memory: &'a mut HashSet<String>,
}

impl<'a> UserWatch<'a> {
    /// A watch over `db` when granted, else over `memory`.
    ///
    /// # Arguments
    ///
    /// * `db` - The plugin's `LISTENING_STATE` database, when granted.
    /// * `memory` - The guest's in-memory set of user keys.
    pub(crate) fn new(db: Option<&'a DatabaseBinding>, memory: &'a mut HashSet<String>) -> Self {
        Self { db, memory }
    }

    /// Records every user in `users` as known when the set was never seeded,
    /// so the users a server already has are never reported.
    ///
    /// With the database, the seed is complete only once its marker row is
    /// written after every user row; a seed that fails part way leaves no
    /// marker, and the next seed records every user again.
    ///
    /// # Returns
    ///
    /// The number of users recorded (0 when the set was already seeded).
    ///
    /// # Errors
    ///
    /// Returns the database's error.
    pub(crate) async fn seed(&mut self, users: &[AbsUser]) -> Result<usize, PluginError> {
        match self.db {
            Some(db) => {
                if known(db).await?.seeded {
                    return Ok(0);
                }
                let mut keys: Vec<String> = users.iter().map(|user| key(&user.id)).collect();
                let count = keys.len();
                keys.push(SEEDED.to_string());
                insert(db, &keys).await?;
                Ok(count)
            }
            None => {
                if !self.memory.is_empty() {
                    return Ok(0);
                }
                self.memory.extend(users.iter().map(|user| key(&user.id)));
                Ok(self.memory.len())
            }
        }
    }

    /// The users in `users` not reported before, now recorded as known.
    ///
    /// Reporting is at least once: when recording them fails part way, the
    /// users are still returned, and those whose rows were not written are
    /// reported again by the next poll (the host then mints a second claim
    /// ticket for the same person rather than none).
    ///
    /// # Errors
    ///
    /// Returns the database's error when the known set cannot be read.
    pub(crate) async fn take_new(
        &mut self,
        users: Vec<AbsUser>,
    ) -> Result<Vec<AbsUser>, PluginError> {
        match self.db {
            Some(db) => {
                let mut seen = known(db).await?.keys;
                let mut keys = Vec::new();
                let mut fresh = Vec::new();
                for user in users {
                    let user_key = key(&user.id);
                    if seen.insert(user_key.clone()) {
                        keys.push(user_key);
                        fresh.push(user);
                    }
                }
                if let Err(err) = insert(db, &keys).await {
                    tracing::warn!(
                        %err,
                        "ABS user watch could not record new users; the next poll reports them again"
                    );
                }
                Ok(fresh)
            }
            None => Ok(users
                .into_iter()
                .filter(|user| self.memory.insert(key(&user.id)))
                .collect()),
        }
    }
}

/// What the watch table holds.
struct Known {
    /// Every user key.
    keys: HashSet<String>,
    /// Whether the first seed completed.
    seeded: bool,
}

/// Every user key the database holds, and whether it was seeded.
///
/// # Errors
///
/// Returns the binding's error, and `internal` for a row of the wrong shape.
async fn known(db: &DatabaseBinding) -> Result<Known, PluginError> {
    let rows = read_keyed(
        db,
        "SELECT person FROM abs_watch_users WHERE person > ? ORDER BY person LIMIT ?",
        Vec::new(),
        "person",
    )
    .await?;
    let mut keys: HashSet<String> = rows
        .into_iter()
        .map(|row| match row.get("person") {
            Some(DbValue::Text(person)) => Ok(person.clone()),
            _ => Err(PluginError::internal("watch user row has the wrong shape")),
        })
        .collect::<Result<_, _>>()?;
    let seeded = keys.remove(SEEDED);
    Ok(Known { keys, seeded })
}

/// Records `keys` as known, in batches that fit one request each.
///
/// # Errors
///
/// Returns the binding's error.
async fn insert(db: &DatabaseBinding, keys: &[String]) -> Result<(), PluginError> {
    let statements = keys
        .iter()
        .map(|person| {
            db.prepare("INSERT OR IGNORE INTO abs_watch_users (person) VALUES (?)")
                .bind(vec![DbValue::Text(person.clone())])
                .as_run()
        })
        .collect();
    for batch in db.chunk(statements, None).await? {
        db.batch(batch, None).await?;
    }
    Ok(())
}

/// Key for an ABS user id: 16 hex digits of a hash, so the database never
/// holds the id.
fn key(user_id: &str) -> String {
    use std::fmt::Write as _;
    let mut hash = Sha256::new();
    hash.update(b"bookclerk-abs-watch-key-v1\0");
    hash.update(user_id.as_bytes());
    hash.finalize()[..8]
        .iter()
        .fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bookclerk_db_testkit::{BindingDatabase, Profile};

    use super::*;

    /// A fresh database holding only the watch table, under `profile`.
    async fn database(profile: Profile) -> DatabaseBinding {
        let unit = BindingDatabase::open(profile, vec![migration()])
            .await
            .unwrap();
        DatabaseBinding::from_session(Arc::new(unit))
    }

    /// ABS users with the given ids.
    fn users(ids: &[&str]) -> Vec<AbsUser> {
        ids.iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({ "id": id, "username": id })).unwrap()
            })
            .collect()
    }

    /// The ids of `users`.
    fn ids(users: &[AbsUser]) -> Vec<&str> {
        users.iter().map(|user| user.id.as_str()).collect()
    }

    #[tokio::test]
    async fn the_database_remembers_reported_users_across_guests() {
        for profile in Profile::ALL {
            let db = database(profile).await;
            // The first guest seeds the users the server already has.
            let mut memory = HashSet::new();
            let mut first = UserWatch::new(Some(&db), &mut memory);
            assert_eq!(first.seed(&users(&["root", "ada"])).await.unwrap(), 2);
            assert!(memory.is_empty(), "the database holds the set");

            // A later guest seeds nothing and reports only the new user.
            let mut memory = HashSet::new();
            let mut later = UserWatch::new(Some(&db), &mut memory);
            assert_eq!(later.seed(&users(&["root"])).await.unwrap(), 0);
            let fresh = later
                .take_new(users(&["root", "ada", "grace", "grace"]))
                .await
                .unwrap();
            assert_eq!(ids(&fresh), ["grace"], "{profile:?}");
            assert!(later
                .take_new(users(&["root", "ada", "grace"]))
                .await
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn many_users_are_read_back_across_paged_reads() {
        for profile in Profile::ALL {
            let db = database(profile).await;
            let names: Vec<String> = (0..300).map(|i| format!("user-{i}")).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let mut memory = HashSet::new();
            let mut watch = UserWatch::new(Some(&db), &mut memory);
            assert_eq!(watch.seed(&users(&refs)).await.unwrap(), 300);
            let mut with_new = refs.clone();
            with_new.push("newcomer");
            let fresh = watch.take_new(users(&with_new)).await.unwrap();
            assert_eq!(ids(&fresh), ["newcomer"], "{profile:?}");
        }
    }

    #[tokio::test]
    async fn a_seed_cut_short_is_redone_in_full() {
        for profile in Profile::ALL {
            let db = database(profile).await;
            // A seed that wrote one user row and stopped before its marker.
            insert(&db, &[key("root")]).await.unwrap();
            let mut memory = HashSet::new();
            let mut watch = UserWatch::new(Some(&db), &mut memory);
            assert_eq!(watch.seed(&users(&["root", "ada"])).await.unwrap(), 2);
            assert_eq!(watch.seed(&users(&["root", "ada"])).await.unwrap(), 0);
            let fresh = watch.take_new(users(&["root", "ada"])).await.unwrap();
            assert!(
                fresh.is_empty(),
                "{profile:?}: a baseline user was reported"
            );
        }
    }

    #[tokio::test]
    async fn new_users_are_reported_even_when_recording_them_fails() {
        // A table that accepts only the marker and `root`, so recording any
        // other user fails.
        let strict = PluginMigration {
            id: "abs-user-watch-strict".into(),
            operations: vec![PluginMigrationOp::Schema(format!(
                "CREATE TABLE IF NOT EXISTS abs_watch_users (person TEXT NOT NULL PRIMARY KEY \
                 CHECK (person IN ('{SEEDED}', '{}')))",
                key("root")
            ))],
        };
        for profile in Profile::ALL {
            let unit = BindingDatabase::open(profile, vec![strict.clone()])
                .await
                .unwrap();
            let db = DatabaseBinding::from_session(Arc::new(unit));
            let mut memory = HashSet::new();
            let mut watch = UserWatch::new(Some(&db), &mut memory);
            assert_eq!(watch.seed(&users(&["root"])).await.unwrap(), 1);
            let fresh = watch.take_new(users(&["root", "ada"])).await.unwrap();
            assert_eq!(ids(&fresh), ["ada"], "{profile:?}");
            let again = watch.take_new(users(&["root", "ada"])).await.unwrap();
            assert_eq!(
                ids(&again),
                ["ada"],
                "{profile:?}: an unrecorded user is reported again"
            );
        }
    }

    #[tokio::test]
    async fn without_a_database_the_set_lives_in_memory() {
        let mut memory = HashSet::new();
        let mut watch = UserWatch::new(None, &mut memory);
        assert_eq!(watch.seed(&users(&["root"])).await.unwrap(), 1);
        assert_eq!(watch.seed(&users(&["root", "ada"])).await.unwrap(), 0);
        let fresh = watch.take_new(users(&["root", "ada"])).await.unwrap();
        assert_eq!(ids(&fresh), ["ada"]);
        assert_eq!(memory.len(), 2);
    }

    #[test]
    fn keys_hide_the_user_id() {
        let user_key = key("8f2c-user");
        assert_eq!(user_key.len(), 16);
        assert!(!user_key.contains("8f2c"));
        assert_eq!(user_key, key("8f2c-user"));
        assert_ne!(user_key, key("other"));
    }
}
