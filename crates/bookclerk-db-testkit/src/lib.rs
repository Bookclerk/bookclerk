//! Contract conformance test kit for code that talks to a Bookclerk database
//! plugin.
//!
//! Production hosts reach every database through an external adapter guest
//! that refuses interactive transactions and enforces the limits it
//! advertised. A plain in-memory SQLite connection does neither, so a test
//! that passes on one can still fail on the hosted demo. This kit opens an
//! in-memory library whose connection behaves like the guest:
//!
//! - `begin()` on the connection is refused the way the adapter refuses it,
//!   and the refusal is counted ([`TestDatabase::refused_begins`]).
//! - Every host batch goes through an executor that applies the
//!   [`Profile`]'s capability advertisement on the request and on the reply,
//!   answering a typed `limit_exceeded` error.
//! - Lost replies and lock contention can be injected
//!   ([`TestDatabase::lose_next_replies`], [`TestDatabase::lock_next_requests`]).
//!
//! [`fixtures`] generates realistic rows near the caps, and [`paging`] checks
//! that a paged listing returns what the full listing returns. Ordinary
//! SeaORM reads through [`TestDatabase::connection`] are held to the engine
//! proxy's first-party caps, as on the shipped sqlite adapter.

use bookclerk_library::test_support::ProfileStore;
use bookclerk_library::{LibraryError, LibraryStore};
use bookclerk_plugin_abi::DbCapabilities;
use bookclerk_plugin_database_sqlite::sqlite::SqliteProxy;
use sea_orm::{DatabaseConnection, DbBackend, DbErr};

pub mod binding;
pub mod fixtures;

pub use binding::BindingDatabase;

pub use bookclerk_library::test_support::{paging, Profile, INTERACTIVE_TRANSACTIONS_REFUSED};

/// The environment variable `bookclerk-plugin test` sets to the profile of
/// the current run.
pub const PROFILE_ENV: &str = "BOOKCLERK_DB_TESTKIT_PROFILE";

/// A `BOOKCLERK_DB_TESTKIT_PROFILE` value that names no profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownProfile(
    /// The value as the environment carried it.
    pub String,
);

impl std::fmt::Display for UnknownProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown test database profile {:?} in {PROFILE_ENV} (one of {})",
            self.0,
            Profile::ALL
                .iter()
                .map(|profile| profile.name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for UnknownProfile {}

/// The profile of the current `bookclerk-plugin test` run, or
/// [`Profile::Minimum`] when the suite runs on its own.
///
/// A Rust suite opens its [`TestDatabase`] or [`BindingDatabase`] under
/// this profile so each of the command's runs exercises a different
/// advertisement, as the TypeScript and Python kits do by default.
///
/// # Errors
///
/// Returns [`UnknownProfile`] when the variable is set to a name outside
/// [`Profile::ALL`].
pub fn selected_profile() -> Result<Profile, UnknownProfile> {
    selected_profile_in(std::env::var_os(PROFILE_ENV))
}

/// [`selected_profile`] over an explicit variable value.
fn selected_profile_in(value: Option<std::ffi::OsString>) -> Result<Profile, UnknownProfile> {
    let Some(value) = value else {
        return Ok(Profile::Minimum);
    };
    let name = value.to_string_lossy();
    if name.is_empty() {
        return Ok(Profile::Minimum);
    }
    Profile::parse(&name).ok_or_else(|| UnknownProfile(name.into_owned()))
}

/// An in-memory library that behaves like an external database plugin.
///
/// Open one with [`TestDatabase::open`] and run the code under test against
/// [`TestDatabase::store`].
#[derive(Debug)]
pub struct TestDatabase {
    /// The profile store on in-memory SQLite.
    inner: ProfileStore,
}

impl TestDatabase {
    /// Opens an empty in-memory library with the host schema under `profile`.
    ///
    /// # Errors
    ///
    /// Returns the engine or schema error when the database cannot be opened.
    pub async fn open(profile: Profile) -> bookclerk_library::Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|err| LibraryError::Orm(DbErr::Custom(format!("sqlite open: {err}"))))?;
        let engine = SqliteProxy::new(conn)
            .map_err(|err| LibraryError::Orm(DbErr::Custom(format!("sqlite proxy: {err}"))))?;
        let inner = ProfileStore::open(engine, DbBackend::Sqlite, profile).await?;
        Ok(Self { inner })
    }

    /// Opens one database per [`Profile::ALL`] entry.
    ///
    /// # Errors
    ///
    /// Returns the first open error.
    pub async fn open_all() -> bookclerk_library::Result<Vec<Self>> {
        let mut out = Vec::with_capacity(Profile::ALL.len());
        for profile in Profile::ALL {
            out.push(Self::open(profile).await?);
        }
        Ok(out)
    }

    /// The enforced profile.
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.inner.profile()
    }

    /// The enforced capability advertisement.
    #[must_use]
    pub fn capabilities(&self) -> &DbCapabilities {
        self.inner.capabilities()
    }

    /// The library store under test.
    #[must_use]
    pub fn store(&self) -> &LibraryStore {
        self.inner.store()
    }

    /// The begin-refusing connection, for reads a test makes directly.
    #[must_use]
    pub fn connection(&self) -> &DatabaseConnection {
        self.inner.connection()
    }

    /// How many interactive transactions the code under test tried to open.
    ///
    /// A host write that works on an external adapter leaves this at zero.
    #[must_use]
    pub fn refused_begins(&self) -> u32 {
        self.inner.refused_begins()
    }

    /// Statement counts of every batch sent so far, in arrival order,
    /// including batches a fault turned away.
    #[must_use]
    pub fn batch_sizes(&self) -> Vec<usize> {
        self.inner.batch_sizes()
    }

    /// Runs one host-authored batch through the profile; see
    /// [`ProfileStore::execute_batch`].
    ///
    /// # Errors
    ///
    /// Returns the typed `limit_exceeded` error when the request or the
    /// reply exceeds a cap, `invalid_params` when the batch does not
    /// typecheck against the host schema, and `unavailable` for an injected
    /// fault or an engine failure.
    pub async fn execute_batch(
        &self,
        request: bookclerk_plugin_abi::ExecuteRequest,
    ) -> Result<bookclerk_plugin_abi::ExecuteReply, bookclerk_plugin_abi::PluginError> {
        self.inner.execute_batch(request).await
    }

    /// The next `n` batches are applied but their replies are lost: the
    /// caller sees `unavailable`, the way a dropped RPC reply looks, and a
    /// retry with the same operation id must not repeat the write.
    pub fn lose_next_replies(&self, n: u32) {
        self.inner.lose_next_replies(n);
    }

    /// The next `n` batches are refused with SQLite lock contention before
    /// anything is applied, the way a busy file lock answers.
    pub fn lock_next_requests(&self, n: u32) {
        self.inner.lock_next_requests(n);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn the_selected_profile_is_the_variable_or_minimum() {
        assert_eq!(selected_profile_in(None), Ok(Profile::Minimum));
        assert_eq!(
            selected_profile_in(Some(std::ffi::OsString::new())),
            Ok(Profile::Minimum)
        );
        for profile in Profile::ALL {
            assert_eq!(
                selected_profile_in(Some(profile.name().into())),
                Ok(profile)
            );
        }
        let err = selected_profile_in(Some("huge".into())).expect_err("unknown");
        assert_eq!(err, UnknownProfile("huge".into()));
        assert!(
            err.to_string().contains("first-party, d1, minimum"),
            "{err}"
        );
    }
}
