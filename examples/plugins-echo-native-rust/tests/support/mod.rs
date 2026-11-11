//! Shared test helpers: the kit's binding under the selected profile, and a
//! wrapper around it that changes the advertisement or fails chosen calls.

// Test code: unwrap/expect are fine here, and not every test file uses
// every helper.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bookclerk_db_testkit::{selected_profile, BindingDatabase, Profile};
use bookclerk_plugin_echo_native_rust::notes::migrations;
use bookclerk_plugin_sdk::{
    DatabaseBinding, DbCapabilities, DbValue, ExecuteReply, ExecuteRequest, ExecuteStream,
    GuestDatabase, LimitExceeded, PluginError,
};

/// The kit's `DB` binding with the notes table, under the profile
/// `bookclerk-plugin test` names (`minimum` when the suite runs alone).
pub struct Served {
    /// The binding the code under test gets.
    pub db: DatabaseBinding,
    /// The profile's advertisement.
    pub caps: DbCapabilities,
}

/// Opens [`Served`] under the selected profile.
pub async fn served() -> Served {
    served_under(selected_profile().unwrap()).await
}

/// Opens [`Served`] under `profile`.
pub async fn served_under(profile: Profile) -> Served {
    let unit = BindingDatabase::open(profile, migrations()).await.unwrap();
    let caps = unit.capabilities().clone();
    Served {
        db: DatabaseBinding::from_session(Arc::new(unit)),
        caps,
    }
}

/// A refusal a page whose bytes pass the inline cap gets.
pub fn too_wide() -> PluginError {
    PluginError::limit_exceeded(LimitExceeded::new("maxResultBytes", 10, 20))
}

/// Decides whether a keyed page read (`after`, `size`) is refused.
type Refusal = Box<dyn Fn(i64, i64) -> Option<PluginError> + Send + Sync>;

/// The kit's binding with a scripted edge.
///
/// It advertises no streamed budget and `max_rows` rows a page when asked
/// to, records every keyed page read as `(after, size)`, refuses the reads
/// `refuse` picks, and fails the `fail_on`-th insert batch (counted from 1).
pub struct Scripted {
    /// The served unit every call that is not scripted reaches.
    inner: BindingDatabase,
    /// The advertised `maxResultRows`, with streaming off; `None` keeps the
    /// profile's advertisement.
    max_rows: Option<u32>,
    /// Picks the page reads to refuse.
    refuse: Refusal,
    /// The insert batch that fails, and its error.
    fail_on: Option<(usize, PluginError)>,
    /// Insert batches seen so far.
    batches: Mutex<usize>,
    /// Every keyed page read, as `(after, size)`.
    pub reads: Mutex<Vec<(i64, i64)>>,
}

impl Scripted {
    /// Wraps a fresh unit under `profile`.
    pub async fn open(profile: Profile) -> Self {
        Self {
            inner: BindingDatabase::open(profile, migrations()).await.unwrap(),
            max_rows: None,
            refuse: Box::new(|_, _| None),
            fail_on: None,
            batches: Mutex::new(0),
            reads: Mutex::new(Vec::new()),
        }
    }

    /// Advertises `max_rows` rows a page and no streamed reads.
    pub fn paging(mut self, max_rows: u32) -> Self {
        self.max_rows = Some(max_rows);
        self
    }

    /// Refuses the page reads `refuse` answers with an error.
    pub fn refusing(
        mut self,
        refuse: impl Fn(i64, i64) -> Option<PluginError> + Send + Sync + 'static,
    ) -> Self {
        self.refuse = Box::new(refuse);
        self
    }

    /// Fails insert batch `n` (from 1) with `err`.
    pub fn failing_batch(mut self, n: usize, err: PluginError) -> Self {
        self.fail_on = Some((n, err));
        self
    }
}

impl Scripted {
    /// Records `request` and answers with the scripted error, if any.
    fn script(&self, request: &ExecuteRequest) -> Result<(), PluginError> {
        let first = request.statements.first();
        if first.is_some_and(|s| s.sql.starts_with("INSERT")) {
            let n = {
                let mut batches = self.batches.lock().unwrap();
                *batches += 1;
                *batches
            };
            if let Some((fail_on, err)) = &self.fail_on {
                if *fail_on == n {
                    return Err(err.clone());
                }
            }
        }
        if let Some(stmt) = first.filter(|s| s.sql.contains("WHERE id > ?")) {
            if let [DbValue::Int64(after), DbValue::Int64(size)] = stmt.parameters.as_slice() {
                self.reads.lock().unwrap().push((*after, *size));
                if let Some(err) = (self.refuse)(*after, *size) {
                    return Err(err);
                }
            }
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl GuestDatabase for Scripted {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteReply, PluginError> {
        self.script(&request)?;
        self.inner.execute(request).await
    }

    async fn execute_streamed(
        &self,
        request: ExecuteRequest,
    ) -> Result<ExecuteStream, PluginError> {
        self.script(&request)?;
        self.inner.execute_streamed(request).await
    }

    async fn limits(&self) -> Result<DbCapabilities, PluginError> {
        let mut caps = self.inner.capabilities().clone();
        if let Some(max_rows) = self.max_rows {
            caps.max_result_rows = max_rows;
            caps.max_streamed_result_bytes = 0;
            caps.max_streamed_result_rows = 0;
        }
        Ok(caps)
    }
}

/// `scripted` as the binding the code under test gets, keeping a handle to
/// read what it recorded.
pub fn bind(scripted: Scripted) -> (DatabaseBinding, Arc<Scripted>) {
    let scripted = Arc::new(scripted);
    let session: Arc<dyn GuestDatabase> = scripted.clone();
    (DatabaseBinding::from_session(session), scripted)
}
