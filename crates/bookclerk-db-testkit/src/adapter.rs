//! Conformance suite for database adapter plugins.
//!
//! The rest of this kit serves code that *uses* a database. This module
//! checks the adapter itself: it drives one empty logical unit through the
//! public [`Database`] / [`AdapterDatabaseSession`] surface every adapter
//! exports, the way the host drives a plugin's `[[databases]]` binding, and
//! reports which parts of the SQL-v1 contract the adapter keeps.
//!
//! The same suite runs against an adapter value in process and against the
//! RPC client of a spawned guest ([`bookclerk_plugin_abi::DatabaseClient`]
//! implements [`Database`] too), so a first-party adapter and an outside one
//! meet one bar. It checks:
//!
//! - **capabilities**: the advertisement meets the host minimums, declares
//!   schema migrations, and is the same on every call and every session.
//! - **bootstrap**: the host's binding bootstrap and a plugin migration apply
//!   as typed atomic batches.
//! - **values**: every [`DbValue`] variant round-trips through declared
//!   columns as the same variant (`Int64` extremes, `Float64`, multibyte and
//!   `b64:`-prefixed text, bytes, `Boolean`, typed NULL).
//! - **atomic_batch**: a batch whose last statement fails leaves nothing of
//!   the earlier statements behind.
//! - **max_statements** / **max_binds**: a batch of exactly `maxStatements`
//!   statements and a statement with exactly `maxBinds` binds are accepted,
//!   so the advertised numbers are real. Where `maxBinds` binds cannot fit
//!   `maxPayloadBytes` / `maxRequestBytes` as host admission measures them,
//!   `max_binds` sends the most binds that do: the largest statement a
//!   guest can send.
//! - **result_rows**: `maxResultRows` rows read inline, one more is refused
//!   by the adapter with a typed `limit_exceeded` error instead of being
//!   truncated or returned.
//! - **cell_bytes** / **result_bytes**: a cell as wide as one bind can
//!   carry under the byte caps (at most 32 KiB) round-trips, a cell built
//!   past `maxCellBytes` in SQL and a result past the inline byte caps are
//!   refused with a typed `limit_exceeded` error, and the refusal comes
//!   from the adapter, not only from the host's own reply check.
//! - **streamed**: an adapter that advertises a streamed budget answers
//!   `executeStreamed` with results past the inline caps; one that does not
//!   answers `unsupported`.
//! - **receipt_replay** / **receipt_conflict**: a retried batch with the
//!   same operation id replays its first reply without applying again (the
//!   retry after a lost reply), and the same id with a different request is
//!   refused with `conflict`.
//! - **close**: the session closes cleanly.
//!
//! Open the adapter on an empty unit and call [`assert_adapter_conforms`]
//! from a test, or [`run_adapter_suite`] to inspect the [`AdapterReport`].

use std::cell::Cell;
use std::fmt;

use bookclerk_db_exec::{
    stamp_adapter_execute, GUEST_RECEIPT_STUB_SUFFIX, GUEST_RECEIPT_WRAP_PREFIX,
};
use bookclerk_library::migrations::{
    host_sql_type_env, plugin_apply_statements, plugin_binding_type_env,
    prove_plugin_migration_sequence, PluginMigrationSequence,
};
use bookclerk_library::sql_plan::execute_guest_atomic_with;
use bookclerk_library::{binding_bootstrap_plan, require_schema_migrations, SchemaState};
use bookclerk_plugin_abi::{
    AdapterDatabaseSession, Database, DbCapabilities, DbPlanStatementKind, DbResultSelection,
    DbType, DbValue, ExecuteReply, ExecuteRequest, ExecuteStreamLimits, GuestSqlPolicy,
    PluginError, PluginErrorCode, PluginMigration, PluginMigrationOp, TypedDbStatement,
};

use crate::fixtures::multibyte_text;

/// One check of the suite, in the order the suite runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Case {
    /// The advertisement meets the host minimums and is stable.
    Capabilities,
    /// The binding bootstrap and a plugin migration apply.
    Bootstrap,
    /// Every value variant round-trips as the same variant.
    Values,
    /// A failed batch rolls back the statements before the failure.
    AtomicBatch,
    /// A batch of exactly `maxStatements` statements is accepted.
    MaxStatements,
    /// The largest statement the bind and byte caps admit is accepted.
    MaxBinds,
    /// `maxResultRows` rows read; one more is a typed refusal.
    ResultRows,
    /// A wide written cell round-trips; one past `maxCellBytes` is refused.
    CellBytes,
    /// A result past the inline byte caps is a typed refusal.
    ResultBytes,
    /// `executeStreamed` matches the advertised streamed budget.
    Streamed,
    /// A retry with the same operation id replays without reapplying.
    ReceiptReplay,
    /// The same operation id with another request is a conflict.
    ReceiptConflict,
    /// The session closes cleanly.
    Close,
}

impl Case {
    /// Every case, in run order.
    pub const ALL: [Self; 13] = [
        Self::Capabilities,
        Self::Bootstrap,
        Self::Values,
        Self::AtomicBatch,
        Self::MaxStatements,
        Self::MaxBinds,
        Self::ResultRows,
        Self::CellBytes,
        Self::ResultBytes,
        Self::Streamed,
        Self::ReceiptReplay,
        Self::ReceiptConflict,
        Self::Close,
    ];

    /// The case's stable snake_case name, as reports print it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Capabilities => "capabilities",
            Self::Bootstrap => "bootstrap",
            Self::Values => "values",
            Self::AtomicBatch => "atomic_batch",
            Self::MaxStatements => "max_statements",
            Self::MaxBinds => "max_binds",
            Self::ResultRows => "result_rows",
            Self::CellBytes => "cell_bytes",
            Self::ResultBytes => "result_bytes",
            Self::Streamed => "streamed",
            Self::ReceiptReplay => "receipt_replay",
            Self::ReceiptConflict => "receipt_conflict",
            Self::Close => "close",
        }
    }
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How one case ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The adapter kept this part of the contract.
    Passed,
    /// The adapter broke it; the text says how.
    Failed(String),
    /// The case could not run: an earlier case it depends on failed, or the
    /// advertisement makes it unreachable (for example a cell cap so small
    /// that no row-capped result can pass the byte cap). The text says why.
    Skipped(String),
}

/// The result of one suite run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterReport {
    /// The advertisement the first session answered, when it answered one.
    pub capabilities: Option<DbCapabilities>,
    /// One entry per [`Case`], in run order.
    pub outcomes: Vec<(Case, Outcome)>,
}

impl AdapterReport {
    /// True when no case failed. Skipped cases do not fail the report.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures().next().is_none()
    }

    /// The failed cases and their reasons.
    pub fn failures(&self) -> impl Iterator<Item = (Case, &str)> {
        self.outcomes
            .iter()
            .filter_map(|(case, outcome)| match outcome {
                Outcome::Failed(reason) => Some((*case, reason.as_str())),
                Outcome::Passed | Outcome::Skipped(_) => None,
            })
    }

    /// The outcome of `case`.
    #[must_use]
    pub fn outcome(&self, case: Case) -> Option<&Outcome> {
        self.outcomes
            .iter()
            .find(|(c, _)| *c == case)
            .map(|(_, outcome)| outcome)
    }
}

impl fmt::Display for AdapterReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (case, outcome) in &self.outcomes {
            match outcome {
                Outcome::Passed => writeln!(f, "pass  {case}")?,
                Outcome::Failed(reason) => writeln!(f, "FAIL  {case}: {reason}")?,
                Outcome::Skipped(reason) => writeln!(f, "skip  {case}: {reason}")?,
            }
        }
        Ok(())
    }
}

/// Runs the suite and panics with the report when a case fails.
///
/// `database` must be open on an empty unit: the suite creates its own
/// tables there.
///
/// # Panics
///
/// Panics when [`AdapterReport::passed`] is false, printing every case.
pub async fn assert_adapter_conforms(database: &dyn Database) -> AdapterReport {
    let report = run_adapter_suite(database).await;
    assert!(
        report.passed(),
        "database adapter breaks the SQL-v1 contract:\n{report}"
    );
    report
}

/// Runs every [`Case`] against `database` and reports each outcome.
///
/// `database` must be open on an empty unit: the suite applies the host's
/// binding bootstrap and its own plugin migration there, then writes and
/// reads only its own `conformance_*` tables. Cases never panic; a broken
/// adapter shows up as [`Outcome::Failed`].
pub async fn run_adapter_suite(database: &dyn Database) -> AdapterReport {
    let mut outcomes = Vec::with_capacity(Case::ALL.len());
    let session = match database.open_session().await {
        Ok(session) => session,
        Err(err) => {
            outcomes.push((Case::Capabilities, failed(format!("openSession: {err}"))));
            skip_rest(&mut outcomes, "openSession failed");
            return AdapterReport {
                capabilities: None,
                outcomes,
            };
        }
    };
    let caps = match session.capabilities().await {
        Ok(caps) => caps,
        Err(err) => {
            outcomes.push((Case::Capabilities, failed(format!("capabilities: {err}"))));
            skip_rest(&mut outcomes, "capabilities failed");
            return AdapterReport {
                capabilities: None,
                outcomes,
            };
        }
    };
    let capabilities = check_capabilities(database, session.as_ref(), &caps).await;
    let caps_ok = capabilities.is_ok();
    outcomes.push((Case::Capabilities, outcome(capabilities)));
    if !caps_ok {
        skip_rest(
            &mut outcomes,
            "the advertisement fails capability negotiation",
        );
        return AdapterReport {
            capabilities: Some(caps),
            outcomes,
        };
    }

    let mut run = SuiteRun::new(session, caps.clone());
    let bootstrap = run.bootstrap().await;
    let bootstrapped = bootstrap.is_ok();
    outcomes.push((Case::Bootstrap, outcome(bootstrap)));
    if !bootstrapped {
        skip_rest(&mut outcomes, "bootstrap failed (is the unit empty?)");
        return AdapterReport {
            capabilities: Some(caps),
            outcomes,
        };
    }

    outcomes.push((Case::Values, outcome(run.values().await)));
    outcomes.push((Case::AtomicBatch, outcome(run.atomic_batch().await)));
    outcomes.push((Case::MaxStatements, outcome(run.max_statements().await)));
    outcomes.push((Case::MaxBinds, run.max_binds().await));
    outcomes.push((Case::ResultRows, run.result_rows().await));
    let seeded_rows = run.seeded_rows;
    outcomes.push((Case::CellBytes, run.cell_bytes().await));
    outcomes.push((Case::ResultBytes, run.result_bytes().await));
    outcomes.push((Case::Streamed, run.streamed(seeded_rows).await));
    let replay = run.receipt_replay().await;
    let replayed = replay.is_ok();
    outcomes.push((Case::ReceiptReplay, outcome(replay)));
    outcomes.push((
        Case::ReceiptConflict,
        if replayed {
            outcome(run.receipt_conflict().await)
        } else {
            Outcome::Skipped("receipt_replay failed".into())
        },
    ));
    outcomes.push((
        Case::Close,
        outcome(
            run.session
                .close()
                .await
                .map_err(|err| format!("close: {err}")),
        ),
    ));
    AdapterReport {
        capabilities: Some(caps),
        outcomes,
    }
}

/// [`Outcome::Failed`] with `reason`.
fn failed(reason: impl Into<String>) -> Outcome {
    Outcome::Failed(reason.into())
}

/// Maps a check result onto its outcome.
fn outcome(result: Result<(), String>) -> Outcome {
    match result {
        Ok(()) => Outcome::Passed,
        Err(reason) => Outcome::Failed(reason),
    }
}

/// Marks every case not yet in `outcomes` as skipped for `reason`.
fn skip_rest(outcomes: &mut Vec<(Case, Outcome)>, reason: &str) {
    for case in Case::ALL {
        if !outcomes.iter().any(|(c, _)| *c == case) {
            outcomes.push((case, Outcome::Skipped(reason.into())));
        }
    }
}

/// The advertisement meets the host minimums, declares schema migrations
/// and is stable across calls and sessions.
async fn check_capabilities(
    database: &dyn Database,
    session: &dyn AdapterDatabaseSession,
    caps: &DbCapabilities,
) -> Result<(), String> {
    if let Some(reason) = caps.capability_failure_reason_opt() {
        return Err(reason);
    }
    require_schema_migrations(caps).map_err(|err| err.to_string())?;
    let again = session
        .capabilities()
        .await
        .map_err(|err| format!("second capabilities call: {err}"))?;
    if &again != caps {
        return Err("a second capabilities call on the same session answered differently".into());
    }
    let other = database
        .open_session()
        .await
        .map_err(|err| format!("second openSession: {err}"))?;
    let other_caps = other
        .capabilities()
        .await
        .map_err(|err| format!("capabilities on a second session: {err}"));
    let closed = other
        .close()
        .await
        .map_err(|err| format!("close of a second session: {err}"));
    if &other_caps? != caps {
        return Err("a second session advertised different capabilities".into());
    }
    closed
}

/// The suite's plugin migration: the tables every later case uses.
fn suite_migrations() -> Vec<PluginMigration> {
    vec![PluginMigration {
        id: "conformance-v1".into(),
        operations: vec![
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS conformance_values (id INTEGER PRIMARY KEY NOT NULL, \
                 i INTEGER, f REAL, t TEXT, y BLOB, b BOOLEAN, n INTEGER)"
                    .into(),
            ),
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS conformance_rows (id INTEGER PRIMARY KEY NOT NULL, \
                 body TEXT NOT NULL)"
                    .into(),
            ),
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS conformance_cells (id INTEGER PRIMARY KEY NOT NULL, \
                 body TEXT NOT NULL)"
                    .into(),
            ),
        ],
    }]
}

/// The first id the receipt cases write, above every id the row cases use.
const RECEIPT_ID: i64 = 9_000_000_001;

/// Statements a seeding batch leaves under `maxStatements` for the host's
/// receipt wrap around a guest batch: the prune / prior-select prefix and
/// the receipt stub after the guest statements.
const SEED_WRAP_HEADROOM: usize = GUEST_RECEIPT_WRAP_PREFIX + GUEST_RECEIPT_STUB_SUFFIX;

/// The most rows the suite seeds to reach `maxResultRows`; a larger cap is
/// not exercised inline.
const MAX_SEEDED_ROWS: u32 = 50_000;

/// The most copies of the written cell the suite concatenates in SQL to
/// pass `maxCellBytes`.
const MAX_CELL_COPIES: usize = 256;

/// The widest cell the suite writes through binds.
const MAX_WRITTEN_CELL: usize = 32 * 1024;

/// Statement and request headroom the suite leaves under the byte caps for
/// SQL text and envelope framing.
const BYTE_HEADROOM: usize = 1024;

/// How long [`NativeAdapter::spawn`] waits for the guest to answer
/// `describe` and `open`.
const GUEST_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// One run's open session, advertisement and binding policy.
struct SuiteRun {
    /// The session every case runs on.
    session: Box<dyn AdapterDatabaseSession>,
    /// The session's advertisement.
    caps: DbCapabilities,
    /// The proven suite migration, applied by [`Self::bootstrap`].
    registered: Option<PluginMigrationSequence>,
    /// Counter for unique operation ids.
    next_op: u64,
    /// Rows `1..=n` the row-cap case seeded into `conformance_rows`.
    seeded_rows: Option<u32>,
}

impl SuiteRun {
    /// A run on `session` under `caps`.
    fn new(session: Box<dyn AdapterDatabaseSession>, caps: DbCapabilities) -> Self {
        Self {
            session,
            caps,
            registered: None,
            next_op: 0,
            seeded_rows: None,
        }
    }

    /// A fresh operation id for a batch labelled `label`.
    fn op(&mut self, label: &str) -> String {
        self.next_op += 1;
        format!("conformance-{label}-{}", self.next_op)
    }

    /// Rows the row-cap case seeds: one past `maxResultRows`.
    fn row_cap_seed(&self) -> u32 {
        self.caps.max_result_rows.saturating_add(1)
    }

    /// The binding-owned guest policy over the applied suite schema.
    fn policy(&self) -> GuestSqlPolicy {
        GuestSqlPolicy::binding_owned().with_sql_types(self.policy_env())
    }

    /// Runs one host-authored batch on the session, stamped against `env`.
    async fn host(
        &self,
        request: ExecuteRequest,
        env: &bookclerk_plugin_abi::SqlTypeEnv,
    ) -> Result<ExecuteReply, PluginError> {
        let envelope = stamp_adapter_execute(request, env)
            .map_err(|err| PluginError::invalid_params(err.to_string()))?;
        self.session.execute(envelope).await
    }

    /// Runs one guest batch through the host's binding path: guest
    /// authorization, receipt wrap, the adapter's `execute`, reply
    /// validation and unwrap.
    async fn guest(
        &mut self,
        label: &str,
        statements: Vec<TypedDbStatement>,
    ) -> Result<ExecuteReply, PluginError> {
        let operation_id = self.op(label);
        self.guest_with_id(operation_id, statements).await
    }

    /// [`Self::guest`] with an explicit operation id.
    async fn guest_with_id(
        &self,
        operation_id: String,
        statements: Vec<TypedDbStatement>,
    ) -> Result<ExecuteReply, PluginError> {
        let policy = self.policy();
        let session = self.session.as_ref();
        execute_guest_atomic_with(
            request(operation_id, statements),
            &self.caps,
            &policy,
            |envelope| session.execute(envelope),
        )
        .await
    }

    /// [`Self::guest`] through `executeStreamed`, held to the streamed
    /// budget as the host holds a binding's streamed read.
    async fn guest_streamed(
        &mut self,
        label: &str,
        statements: Vec<TypedDbStatement>,
    ) -> Result<ExecuteReply, PluginError> {
        let streamed = self
            .caps
            .streamed()
            .ok_or_else(|| PluginError::unsupported("the adapter advertises no streamed budget"))?;
        let limits = ExecuteStreamLimits::from_capabilities(&self.caps);
        let operation_id = self.op(label);
        let policy = self.policy();
        let session = self.session.as_ref();
        execute_guest_atomic_with(
            request(operation_id, statements),
            &streamed,
            &policy,
            |envelope| async move {
                session
                    .execute_streamed(envelope)
                    .await?
                    .collect(limits)
                    .await
            },
        )
        .await
    }

    /// Applies the binding bootstrap and the suite migration.
    async fn bootstrap(&mut self) -> Result<(), String> {
        let stmts = binding_bootstrap_plan(&SchemaState::Uninitialized)
            .map_err(|err| format!("binding bootstrap plan: {err}"))?
            .unwrap_or_default();
        let bootstrap = request(
            "conformance-binding-bootstrap".into(),
            stmts.into_iter().map(discard).collect(),
        );
        self.host(bootstrap, &host_sql_type_env())
            .await
            .map_err(|err| format!("binding bootstrap batch: {err}"))?;
        let registered = prove_plugin_migration_sequence(suite_migrations())
            .map_err(|err| format!("suite migration proof: {err}"))?;
        for (ordinal, migration) in registered.migrations.iter().enumerate() {
            let ordinal_i64 = i64::try_from(ordinal).unwrap_or(i64::MAX);
            let apply = request(
                format!("conformance-migrate-{}", migration.id),
                plugin_apply_statements(ordinal_i64, migration)
                    .into_iter()
                    .map(discard)
                    .collect(),
            );
            self.host(apply, &plugin_binding_type_env(&registered, ordinal))
                .await
                .map_err(|err| format!("plugin migration {}: {err}", migration.id))?;
        }
        self.registered = Some(registered);
        Ok(())
    }

    /// Every value variant round-trips as the same variant.
    async fn values(&mut self) -> Result<(), String> {
        let bytes = vec![0u8, 127, 128, 255, 42];
        let rows: [(i64, i64, bool, String); 2] = [
            (1, i64::MIN, true, multibyte_text(200)),
            (2, i64::MAX, false, "b64:AAAA".into()),
        ];
        for (id, int, boolean, text) in &rows {
            let reply = self
                .guest(
                    "values-insert",
                    vec![exec(
                        "INSERT INTO conformance_values (id, i, f, t, y, b, n) \
                         VALUES (?, ?, ?, ?, ?, ?, ?)",
                        vec![
                            DbValue::Int64(*id),
                            DbValue::Int64(*int),
                            DbValue::Float64(-1.25e-3),
                            DbValue::Text(text.clone()),
                            DbValue::Bytes(bytes.clone()),
                            DbValue::Boolean(*boolean),
                            DbValue::Null(DbType::Int64),
                        ],
                    )],
                )
                .await
                .map_err(|err| format!("insert row {id}: {err}"))?;
            let affected = reply.statements.first().map_or(0, |s| s.rows_affected);
            if affected != 1 {
                return Err(format!("insert row {id} reported {affected} rows affected"));
            }
        }
        for (id, int, boolean, text) in rows {
            let reply = self
                .guest(
                    "values-select",
                    vec![query(
                        "SELECT i, f, t, y, b, n FROM conformance_values WHERE id = ?",
                        vec![DbValue::Int64(id)],
                        1,
                    )],
                )
                .await
                .map_err(|err| format!("select row {id}: {err}"))?;
            let got = single_row(&reply).map_err(|err| format!("select row {id}: {err}"))?;
            let want = [
                DbValue::Int64(int),
                DbValue::Float64(-1.25e-3),
                DbValue::Text(text),
                DbValue::Bytes(bytes.clone()),
                DbValue::Boolean(boolean),
                DbValue::Null(DbType::Int64),
            ];
            if got != want {
                return Err(format!("row {id} read back {got:?}, wrote {want:?}"));
            }
        }
        Ok(())
    }

    /// A batch whose second write fails leaves the first write unapplied,
    /// and the first write alone then applies, so the failure came from the
    /// second statement rather than from admitting the batch.
    async fn atomic_batch(&mut self) -> Result<(), String> {
        let insert = |body: &str| {
            exec(
                "INSERT INTO conformance_rows (id, body) VALUES (?, ?)",
                vec![DbValue::Int64(-1), DbValue::Text(body.into())],
            )
        };
        if self
            .guest("atomic-dup", vec![insert("first"), insert("second")])
            .await
            .is_ok()
        {
            return Err("a batch with a duplicate primary key succeeded".into());
        }
        let count = self.count_rows("id = -1").await?;
        if count != 0 {
            return Err(format!(
                "the failed batch left {count} row(s) of its first statement behind"
            ));
        }
        self.guest("atomic-first", vec![insert("first")])
            .await
            .map_err(|err| {
                format!("the duplicate's first statement alone was refused too: {err}")
            })?;
        let count = self.count_rows("id = -1").await?;
        if count != 1 {
            return Err(format!("the first statement alone left {count} rows"));
        }
        Ok(())
    }

    /// A host batch of exactly `maxStatements` statements is accepted.
    async fn max_statements(&mut self) -> Result<(), String> {
        let n = self.caps.max_statements;
        let statements = (0..n)
            .map(|_| query("SELECT 1 AS one", Vec::new(), 1))
            .collect::<Vec<_>>();
        let operation_id = self.op("max-statements");
        let reply = self
            .host(request(operation_id, statements), &self.policy_env())
            .await
            .map_err(|err| format!("{n} statements (maxStatements): {err}"))?;
        if reply.statements.len() != usize::try_from(n).unwrap_or(usize::MAX) {
            return Err(format!(
                "{n} statements answered {} results",
                reply.statements.len()
            ));
        }
        Ok(())
    }

    /// The largest statement a guest can send is accepted: as many binds as
    /// `maxBinds` allows and the byte caps leave room for, as host admission
    /// measures them.
    async fn max_binds(&mut self) -> Outcome {
        let n = self.caps.max_binds;
        let mut fits = match self.binds_statement(1) {
            Ok((statement, true)) => (1, statement),
            Ok((_, false)) => {
                return Outcome::Skipped(
                    "one bind does not fit maxPayloadBytes / maxRequestBytes".into(),
                )
            }
            Err(err) => return failed(err),
        };
        // The largest count in (fits.0, high] that fits, by bisection: the
        // encoded size grows with the count.
        let mut high = n;
        while fits.0 < high {
            let mid = fits.0 + (high - fits.0).div_ceil(2);
            match self.binds_statement(mid) {
                Ok((statement, true)) => fits = (mid, statement),
                Ok((_, false)) => high = mid - 1,
                Err(err) => return failed(err),
            }
        }
        let (count, statement) = fits;
        let what = if count == n {
            format!("{count} binds (maxBinds)")
        } else {
            format!("{count} binds (the most maxPayloadBytes / maxRequestBytes admit under maxBinds {n})")
        };
        let operation_id = self.op("max-binds");
        let env = self.policy_env();
        match self
            .host(request(operation_id, vec![statement]), &env)
            .await
        {
            Ok(_) => Outcome::Passed,
            Err(err) => failed(format!("{what}: {err}")),
        }
    }

    /// A one-row read with `count` binds, and whether it fits the byte caps
    /// as host admission measures them: canonical SQL plus each encoded bind
    /// against `maxPayloadBytes`, the encoded request against
    /// `maxRequestBytes`. Lowering the SQL is the adapter's own business.
    fn binds_statement(&self, count: u32) -> Result<(TypedDbStatement, bool), String> {
        let placeholders = usize::try_from(count).unwrap_or(usize::MAX);
        let sql = format!(
            "SELECT COUNT(*) AS n FROM conformance_rows WHERE id IN ({})",
            vec!["?"; placeholders].join(", ")
        );
        let parameters: Vec<DbValue> = (0..count)
            .map(|i| DbValue::Int64(i64::from(i) + 1))
            .collect();
        let mut payload = sql.len();
        for param in &parameters {
            let bytes = bookclerk_plugin_abi::encoded_db_value_bytes(param)
                .map_err(|err| format!("encoding a bind: {err}"))?;
            payload = payload.saturating_add(bytes.len());
        }
        let statement = query(&sql, parameters, 1);
        let req = request("conformance-max-binds".into(), vec![statement]);
        let request_bytes = bookclerk_plugin_abi::encoded_execute_request_bytes(&req)
            .map_err(|err| format!("encoding a {count}-bind request: {err}"))?
            .len();
        let payload_cap = usize::try_from(self.caps.max_payload_bytes).unwrap_or(usize::MAX);
        let request_cap = usize::try_from(self.caps.max_request_bytes).unwrap_or(usize::MAX);
        let fits = payload <= payload_cap && request_bytes <= request_cap;
        let statement = req.statements.into_iter().next().ok_or("no statement")?;
        Ok((statement, fits))
    }

    /// The SQL types of the applied suite schema.
    fn policy_env(&self) -> bookclerk_plugin_abi::SqlTypeEnv {
        match &self.registered {
            Some(registered) => plugin_binding_type_env(registered, registered.migrations.len()),
            None => bookclerk_library::migrations::binding_bootstrap_type_env(),
        }
    }

    /// `COUNT(*)` of `conformance_rows` rows matching `predicate`.
    async fn count_rows(&mut self, predicate: &str) -> Result<i64, String> {
        let reply = self
            .guest(
                "count",
                vec![query(
                    &format!("SELECT COUNT(*) AS n FROM conformance_rows WHERE {predicate}"),
                    Vec::new(),
                    1,
                )],
            )
            .await
            .map_err(|err| format!("count rows where {predicate}: {err}"))?;
        match single_row(&reply)?.as_slice() {
            [DbValue::Int64(n)] => Ok(*n),
            other => Err(format!("COUNT(*) read back {other:?}")),
        }
    }

    /// Sends one over-cap guest read and expects the adapter itself to refuse
    /// it with a typed `limit_exceeded` naming one of `limits`.
    ///
    /// The host checks every reply against the caps too, so an adapter that
    /// returned the oversized result would still reach the guest as
    /// `limit_exceeded`; the suite records what the adapter answered to tell
    /// the two apart. `what` describes the read in failure messages.
    async fn expect_refusal(
        &mut self,
        label: &str,
        statement: TypedDbStatement,
        limits: &[&str],
        what: &str,
    ) -> Outcome {
        let operation_id = self.op(label);
        self.expect_refusal_as(operation_id, statement, limits, what)
            .await
    }

    /// [`Self::expect_refusal`] under an explicit operation id.
    async fn expect_refusal_as(
        &self,
        operation_id: String,
        statement: TypedDbStatement,
        limits: &[&str],
        what: &str,
    ) -> Outcome {
        let policy = self.policy();
        let session = self.session.as_ref();
        let adapter_answered = Cell::new(false);
        let answered = &adapter_answered;
        let reply = execute_guest_atomic_with(
            request(operation_id, vec![statement]),
            &self.caps,
            &policy,
            |envelope| async move {
                let reply = session.execute(envelope).await;
                answered.set(reply.is_ok());
                reply
            },
        )
        .await;
        match reply {
            Ok(reply) => failed(format!(
                "{what} succeeded with {} row(s) instead of a limit_exceeded error",
                reply.statements.first().map_or(0, |s| s.rows.len())
            )),
            Err(err) if adapter_answered.get() => failed(format!(
                "the adapter returned {what} instead of refusing it; only the host's reply \
                 check stopped it ({}: {})",
                err.code.as_str(),
                err.message
            )),
            Err(err) => expect_limit(&err, limits),
        }
    }

    /// Seeds `maxResultRows + 1` rows, reads exactly the cap, and expects a
    /// typed refusal for one more.
    async fn result_rows(&mut self) -> Outcome {
        let cap = self.caps.max_result_rows;
        let seed = self.row_cap_seed();
        if seed > MAX_SEEDED_ROWS {
            return Outcome::Skipped(format!(
                "maxResultRows {cap} is above the {MAX_SEEDED_ROWS} rows the suite seeds"
            ));
        }
        if let Err(err) = self.seed_rows(seed).await {
            return failed(err);
        }
        self.seeded_rows = Some(seed);
        let at_cap = self
            .guest(
                "rows-at-cap",
                vec![query(
                    &format!(
                        "SELECT id FROM conformance_rows WHERE id > 0 ORDER BY id LIMIT {cap}"
                    ),
                    Vec::new(),
                    cap,
                )],
            )
            .await;
        match at_cap {
            Ok(reply) => {
                let got = reply.statements.first().map_or(0, |s| s.rows.len());
                if got != usize::try_from(cap).unwrap_or(usize::MAX) {
                    return failed(format!(
                        "a {cap}-row read (maxResultRows) returned {got} rows"
                    ));
                }
            }
            Err(err) => return failed(format!("a {cap}-row read (maxResultRows): {err}")),
        }
        // The same read twice under one operation id: a retry must meet the
        // same typed refusal, not a receipt the refused attempt left behind.
        let over = query(
            "SELECT id FROM conformance_rows WHERE id > 0 AND body = 'r' ORDER BY id",
            Vec::new(),
            cap,
        );
        let operation_id = self.op("rows-over-cap");
        let what = format!("a {seed}-row read under maxResultRows {cap}");
        let first = self
            .expect_refusal_as(
                operation_id.clone(),
                over.clone(),
                &["maxResultRows"],
                &what,
            )
            .await;
        if first != Outcome::Passed {
            return first;
        }
        if let Outcome::Failed(reason) = self
            .expect_refusal_as(
                operation_id.clone(),
                over.clone(),
                &["maxResultRows"],
                &what,
            )
            .await
        {
            return failed(format!("a retry under the same operation id: {reason}"));
        }
        // Once the data no longer passes the cap, the same retry reads it:
        // the refused attempts left no unfinished receipt behind.
        if let Err(err) = self.relabel_rows("r", "s").await {
            return failed(err);
        }
        let retry = self.guest_with_id(operation_id, vec![over]).await;
        if let Err(err) = self.relabel_rows("s", "r").await {
            return failed(err);
        }
        match retry {
            Ok(reply) if reply.statements.first().is_some_and(|s| s.rows.is_empty()) => {
                Outcome::Passed
            }
            Ok(reply) => failed(format!(
                "a retry of the refused read after its rows changed returned {} rows, not 0",
                reply.statements.first().map_or(0, |s| s.rows.len())
            )),
            Err(err) => failed(format!(
                "a retry of the refused read after its rows changed under {cap}: {err}"
            )),
        }
    }

    /// Sets `body` from `from` to `to` on every seeded `conformance_rows` row.
    async fn relabel_rows(&mut self, from: &str, to: &str) -> Result<(), String> {
        self.guest(
            "rows-relabel",
            vec![exec(
                "UPDATE conformance_rows SET body = ? WHERE id > 0 AND body = ?",
                vec![DbValue::Text(to.into()), DbValue::Text(from.into())],
            )],
        )
        .await
        .map(drop)
        .map_err(|err| format!("relabelling seeded rows {from:?} to {to:?}: {err}"))
    }

    /// Inserts rows `1..=count` into `conformance_rows`.
    async fn seed_rows(&mut self, count: u32) -> Result<(), String> {
        let statements = (1..=count)
            .map(|id| {
                exec(
                    "INSERT INTO conformance_rows (id, body) VALUES (?, 'r')",
                    vec![DbValue::Int64(i64::from(id))],
                )
            })
            .collect();
        self.seed("rows-seed", statements, 0)
            .await
            .map_err(|err| format!("seeding {count} rows: {err}"))
    }

    /// Sends `statements` as few guest batches as the caps allow: as many
    /// per batch as the receipt wrap leaves under `maxStatements`, and as
    /// many of `bytes_each` bound bytes as fit `maxRequestBytes`.
    async fn seed(
        &mut self,
        label: &str,
        statements: Vec<TypedDbStatement>,
        bytes_each: usize,
    ) -> Result<(), PluginError> {
        let by_count = usize::try_from(self.caps.max_statements)
            .unwrap_or(usize::MAX)
            .saturating_sub(SEED_WRAP_HEADROOM);
        let request_cap = usize::try_from(self.caps.max_request_bytes).unwrap_or(usize::MAX);
        let by_bytes = request_cap
            .saturating_sub(BYTE_HEADROOM)
            .checked_div(bytes_each.saturating_add(BYTE_HEADROOM))
            .unwrap_or(usize::MAX);
        let per_batch = by_count.min(by_bytes).max(1);
        let mut statements = statements.into_iter().peekable();
        while statements.peek().is_some() {
            let batch = statements.by_ref().take(per_batch).collect();
            self.guest(label, batch).await?;
        }
        Ok(())
    }

    /// The widest cell the suite writes: `maxCellBytes`, bounded by what one
    /// bind can carry under the payload and request caps.
    fn writable_cell_bytes(&self) -> usize {
        let cell = usize::try_from(self.caps.max_cell_bytes).unwrap_or(usize::MAX);
        let payload = usize::try_from(self.caps.max_payload_bytes).unwrap_or(usize::MAX);
        let request = usize::try_from(self.caps.max_request_bytes).unwrap_or(usize::MAX);
        let result = usize::try_from(self.caps.max_result_bytes).unwrap_or(usize::MAX);
        cell.min(payload.saturating_sub(BYTE_HEADROOM))
            .min(request.saturating_sub(BYTE_HEADROOM))
            .min(result.saturating_sub(BYTE_HEADROOM))
            .min(MAX_WRITTEN_CELL)
    }

    /// A multibyte cell as wide as the caps allow round-trips; doubling it
    /// past `maxCellBytes` in SQL is a typed refusal.
    async fn cell_bytes(&mut self) -> Outcome {
        let width = self.writable_cell_bytes();
        let body = multibyte_text(width);
        if let Err(err) = self
            .guest(
                "cell-insert",
                vec![exec(
                    "INSERT INTO conformance_cells (id, body) VALUES (?, ?)",
                    vec![DbValue::Int64(1), DbValue::Text(body.clone())],
                )],
            )
            .await
        {
            return failed(format!("writing a {}-byte cell: {err}", body.len()));
        }
        let read = self
            .guest(
                "cell-select",
                vec![query(
                    "SELECT body FROM conformance_cells WHERE id = 1",
                    Vec::new(),
                    1,
                )],
            )
            .await;
        match read
            .map_err(|err| err.to_string())
            .and_then(|r| single_row(&r))
        {
            Ok(values) if values == [DbValue::Text(body.clone())] => {}
            Ok(values) => {
                return failed(format!(
                    "a {}-byte cell read back as {} value(s) of another shape or length",
                    body.len(),
                    values.len()
                ))
            }
            Err(err) => return failed(format!("reading a {}-byte cell: {err}", body.len())),
        }
        let cap = usize::try_from(self.caps.max_cell_bytes).unwrap_or(usize::MAX);
        // Doubling the cell until it passes the cap: k copies of `body`.
        let copies = cap / body.len().max(1) + 1;
        if copies > MAX_CELL_COPIES {
            return Outcome::Skipped(format!(
                "maxCellBytes {cap} is too far above the {}-byte writable cell to pass in SQL",
                body.len()
            ));
        }
        let expr = vec!["body"; copies].join(" || ");
        self.expect_refusal(
            "cell-over-cap",
            query(
                &format!("SELECT {expr} AS wide FROM conformance_cells WHERE id = 1"),
                Vec::new(),
                1,
            ),
            &["maxCellBytes", "maxResultBytes", "maxAtomicResultBytes"],
            &format!(
                "a {}-byte cell under maxCellBytes {cap}",
                body.len().saturating_mul(copies)
            ),
        )
        .await
    }

    /// Rows whose total passes the inline byte caps are a typed refusal
    /// inline.
    async fn result_bytes(&mut self) -> Outcome {
        let width = self.writable_cell_bytes();
        let cap = usize::try_from(
            self.caps
                .max_result_bytes
                .min(self.caps.max_atomic_result_bytes),
        )
        .unwrap_or(usize::MAX);
        let rows = cap / width.max(1) + 2;
        let row_cap = usize::try_from(self.caps.max_result_rows).unwrap_or(usize::MAX);
        if rows > row_cap || rows > 4096 {
            return Outcome::Skipped(format!(
                "{rows} rows of {width} bytes are needed to pass maxResultBytes {cap}, \
                 more than maxResultRows {row_cap} or the suite's 4096"
            ));
        }
        let body = "x".repeat(width);
        let seeds = (0..rows)
            .map(|i| {
                let id = 1_000 + i64::try_from(i).unwrap_or(i64::MAX);
                exec(
                    "INSERT INTO conformance_cells (id, body) VALUES (?, ?)",
                    vec![DbValue::Int64(id), DbValue::Text(body.clone())],
                )
            })
            .collect();
        if let Err(err) = self.seed("bytes-seed", seeds, width).await {
            return failed(format!("seeding {rows} cells of {width} bytes: {err}"));
        }
        let rows_u32 = u32::try_from(rows).unwrap_or(u32::MAX);
        self.expect_refusal(
            "bytes-over-cap",
            query(
                "SELECT body FROM conformance_cells WHERE id >= 1000 ORDER BY id",
                Vec::new(),
                rows_u32,
            ),
            &["maxResultBytes", "maxAtomicResultBytes"],
            &format!("{rows} cells of {width} bytes under maxResultBytes {cap}"),
        )
        .await
    }

    /// `executeStreamed` matches the advertisement: `unsupported` without a
    /// streamed budget, results past the inline caps with one.
    async fn streamed(&mut self, seeded_rows: Option<u32>) -> Outcome {
        if !self.caps.streams_results() {
            let operation_id = self.op("streamed-unsupported");
            let env = self.policy_env();
            let envelope = match stamp_adapter_execute(
                request(operation_id, vec![query("SELECT 1 AS one", Vec::new(), 1)]),
                &env,
            ) {
                Ok(envelope) => envelope,
                Err(err) => return failed(format!("stamping SELECT 1: {err}")),
            };
            return match self.session.execute_streamed(envelope).await {
                Ok(_) => failed(
                    "executeStreamed answered although the adapter advertises no streamed budget",
                ),
                Err(err) if err.code == PluginErrorCode::Unsupported => Outcome::Passed,
                Err(err) => failed(format!(
                    "executeStreamed without a streamed budget answered {} instead of unsupported",
                    err.code.as_str()
                )),
            };
        }
        let inline = self
            .guest(
                "streamed-inline",
                vec![query(
                    "SELECT id, body FROM conformance_rows WHERE id BETWEEN 1 AND 10 ORDER BY id",
                    Vec::new(),
                    10,
                )],
            )
            .await;
        let streamed = self
            .guest_streamed(
                "streamed-small",
                vec![query(
                    "SELECT id, body FROM conformance_rows WHERE id BETWEEN 1 AND 10 ORDER BY id",
                    Vec::new(),
                    10,
                )],
            )
            .await;
        match (inline, streamed) {
            (Ok(inline), Ok(streamed)) => {
                if inline.statements.first().map(|s| &s.rows)
                    != streamed.statements.first().map(|s| &s.rows)
                {
                    return failed("executeStreamed returned other rows than execute for one read");
                }
            }
            (Err(err), _) => return failed(format!("inline read: {err}")),
            (_, Err(err)) => return failed(format!("executeStreamed: {err}")),
        }
        let Some(seeded) = seeded_rows else {
            return Outcome::Skipped("result_rows seeded no rows past maxResultRows".into());
        };
        let inline_cap = self.caps.max_result_rows;
        let want = seeded.min(self.caps.max_streamed_result_rows);
        if want <= inline_cap {
            return Outcome::Skipped(format!(
                "maxStreamedResultRows {} leaves no streamed read past maxResultRows {inline_cap}",
                self.caps.max_streamed_result_rows
            ));
        }
        let read = self
            .guest_streamed(
                "streamed-past-inline",
                vec![query(
                    &format!(
                        "SELECT id FROM conformance_rows WHERE id > 0 ORDER BY id LIMIT {want}"
                    ),
                    Vec::new(),
                    want,
                )],
            )
            .await;
        match read {
            Ok(reply) => {
                let got = reply.statements.first().map_or(0, |s| s.rows.len());
                if got == usize::try_from(want).unwrap_or(usize::MAX) {
                    Outcome::Passed
                } else {
                    failed(format!(
                        "a streamed read of {want} rows (past maxResultRows {inline_cap}) \
                         returned {got}"
                    ))
                }
            }
            Err(err) => failed(format!(
                "a streamed read of {want} rows (past maxResultRows {inline_cap}): {err}"
            )),
        }
    }

    /// The batch the receipt cases send: insert one row with `body`, then
    /// read it back.
    fn receipt_batch(body: &str) -> Vec<TypedDbStatement> {
        vec![
            exec(
                "INSERT INTO conformance_rows (id, body) VALUES (?, ?)",
                vec![DbValue::Int64(RECEIPT_ID), DbValue::Text(body.into())],
            ),
            query(
                "SELECT body FROM conformance_rows WHERE id = ?",
                vec![DbValue::Int64(RECEIPT_ID)],
                1,
            ),
        ]
    }

    /// A retry with the same operation id replays the first reply and does
    /// not apply the insert twice (a second apply would hit the primary key).
    async fn receipt_replay(&mut self) -> Result<(), String> {
        let operation_id = "conformance-receipt".to_string();
        let first = self
            .guest_with_id(operation_id.clone(), Self::receipt_batch("first"))
            .await
            .map_err(|err| format!("first attempt: {err}"))?;
        let retry = self
            .guest_with_id(operation_id, Self::receipt_batch("first"))
            .await
            .map_err(|err| format!("a retry with the same operation id was not replayed: {err}"))?;
        let rows = |reply: &ExecuteReply| {
            reply
                .statements
                .iter()
                .map(|s| (s.rows_affected, s.rows.clone()))
                .collect::<Vec<_>>()
        };
        if rows(&first) != rows(&retry) {
            return Err("the replayed reply differs from the first reply".into());
        }
        let count = self.count_rows(&format!("id = {RECEIPT_ID}")).await?;
        if count != 1 {
            return Err(format!("the retried insert left {count} rows"));
        }
        Ok(())
    }

    /// The same operation id with a different request is a `conflict`, and
    /// the first write stays.
    async fn receipt_conflict(&mut self) -> Result<(), String> {
        let attempt = self
            .guest_with_id(
                "conformance-receipt".into(),
                Self::receipt_batch("different"),
            )
            .await;
        match attempt {
            Ok(_) => return Err("a different request under a used operation id succeeded".into()),
            Err(err) if err.code == PluginErrorCode::Conflict => {}
            Err(err) => {
                return Err(format!(
                    "a different request under a used operation id answered {} instead of \
                     conflict: {}",
                    err.code.as_str(),
                    err.message
                ))
            }
        }
        let reply = self
            .guest(
                "receipt-body",
                vec![query(
                    "SELECT body FROM conformance_rows WHERE id = ?",
                    vec![DbValue::Int64(RECEIPT_ID)],
                    1,
                )],
            )
            .await
            .map_err(|err| format!("reading the receipt row: {err}"))?;
        match single_row(&reply)?.as_slice() {
            [DbValue::Text(body)] if body == "first" => Ok(()),
            other => Err(format!(
                "the receipt row reads {other:?} after the conflict"
            )),
        }
    }
}

/// A native adapter guest binary spawned directly and reached over the
/// Cap'n Proto boundary the host speaks, for running the suite against the
/// shipped artifact.
///
/// The guest runs unconfined and without the `bookclerk-workerd` front door
/// (the direct diagnostic transport), so this is for tests only. Create and
/// use it inside a [`tokio::task::LocalSet`]: the RPC system runs as a local
/// task.
pub struct NativeAdapter {
    /// The guest process, killed on drop.
    child: tokio::process::Child,
    /// The guest's `databaseAdapter` entrypoint.
    database: bookclerk_plugin_abi::DatabaseClient,
}

impl NativeAdapter {
    /// Spawns `binary` with `envs`, then `describe`s it and `open`s its
    /// `databaseAdapter` entrypoint with `values` (the host-private connect
    /// params, for example from
    /// [`bookclerk_plugin_abi::db::binding_values_from_params`]).
    ///
    /// # Errors
    ///
    /// Returns the spawn, describe or open failure, or a message when the
    /// guest exports no `databaseAdapter`.
    pub async fn spawn(
        binary: &std::path::Path,
        envs: &[(&str, &std::ffi::OsStr)],
        values: bookclerk_plugin_abi::BindingValues,
    ) -> Result<Self, String> {
        let mut command = tokio::process::Command::new(binary);
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| format!("spawn {}: {err}", binary.display()))?;
        let stdin = child.stdin.take().ok_or("the guest has no stdin")?;
        let stdout = child.stdout.take().ok_or("the guest has no stdout")?;
        let (client, rpc) = bookclerk_plugin_abi::connect_plugin(stdout, stdin, 64 * 1024);
        tokio::task::spawn_local(rpc);
        tokio::time::timeout(GUEST_STARTUP_TIMEOUT, client.describe())
            .await
            .map_err(|_| "describe timed out".to_string())?
            .map_err(|err| format!("describe: {err}"))?;
        let invocation = bookclerk_plugin_abi::Invocation {
            id: "database".into(),
            ..Default::default()
        };
        let open = client.open(
            &invocation,
            bookclerk_plugin_abi::HostBindings {
                values,
                ..Default::default()
            },
        );
        let opened = tokio::time::timeout(GUEST_STARTUP_TIMEOUT, open)
            .await
            .map_err(|_| "open timed out".to_string())?
            .map_err(|err| format!("open: {err}"))?;
        let database = opened
            .database_adapter
            .ok_or("the guest exports no `databaseAdapter`")?;
        Ok(Self { child, database })
    }

    /// The guest's adapter, as the host holds it.
    #[must_use]
    pub fn database(&self) -> &dyn Database {
        &self.database
    }

    /// Kills the guest.
    pub async fn shutdown(mut self) {
        // The guest may already have exited; either way it is gone.
        let _ = self.child.kill().await;
    }
}

/// [`Outcome::Passed`] when `err` is a typed `limit_exceeded` naming one of
/// `limits`, otherwise a failure saying what came back.
fn expect_limit(err: &PluginError, limits: &[&str]) -> Outcome {
    if err.code != PluginErrorCode::LimitExceeded {
        return failed(format!(
            "expected limit_exceeded ({}), got {}: {}",
            limits.join(" / "),
            err.code.as_str(),
            err.message
        ));
    }
    match err.limit() {
        Some(limit) if limits.contains(&limit.limit.as_str()) => Outcome::Passed,
        Some(limit) => failed(format!(
            "limit_exceeded names {} (expected {})",
            limit.limit,
            limits.join(" / ")
        )),
        None => failed(format!(
            "limit_exceeded carries no typed limit: {}",
            err.message
        )),
    }
}

/// The single row of a one-statement reply.
fn single_row(reply: &ExecuteReply) -> Result<Vec<DbValue>, String> {
    let stmt = reply
        .statements
        .first()
        .ok_or_else(|| "the reply has no statement result".to_string())?;
    match stmt.rows.as_slice() {
        [row] => Ok(row.values.clone()),
        rows => Err(format!("expected one row, got {}", rows.len())),
    }
}

/// A batch with no request hash: the host stamps it.
fn request(operation_id: String, statements: Vec<TypedDbStatement>) -> ExecuteRequest {
    ExecuteRequest {
        operation_id,
        request_hash: String::new(),
        statements,
        deadline_unix_ms: 0,
    }
}

/// A write reporting rows affected.
fn exec(sql: &str, parameters: Vec<DbValue>) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.into(),
        parameters,
        kind: DbPlanStatementKind::Execute,
        max_rows: 0,
        result_selection: DbResultSelection::AffectedRows,
    }
}

/// A read capped at `max_rows`.
fn query(sql: &str, parameters: Vec<DbValue>, max_rows: u32) -> TypedDbStatement {
    TypedDbStatement {
        sql: sql.into(),
        parameters,
        kind: DbPlanStatementKind::Select,
        max_rows,
        result_selection: DbResultSelection::Rows,
    }
}

/// A host DDL or bookkeeping statement whose result is discarded.
fn discard(sql: String) -> TypedDbStatement {
    TypedDbStatement {
        sql,
        parameters: Vec::new(),
        kind: DbPlanStatementKind::Execute,
        max_rows: 0,
        result_selection: DbResultSelection::Discard,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::LimitExceeded;

    #[test]
    fn case_names_are_unique_and_in_run_order() {
        let names: Vec<_> = Case::ALL.iter().map(|c| c.name()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        assert_eq!(Case::ALL[0], Case::Capabilities);
        assert_eq!(Case::ALL[Case::ALL.len() - 1], Case::Close);
    }

    #[test]
    fn skipped_cases_do_not_fail_a_report() {
        let mut outcomes = vec![(Case::Capabilities, Outcome::Passed)];
        skip_rest(&mut outcomes, "test");
        let report = AdapterReport {
            capabilities: None,
            outcomes,
        };
        assert!(report.passed());
        assert_eq!(report.outcomes.len(), Case::ALL.len());
        assert_eq!(
            report.outcome(Case::Close),
            Some(&Outcome::Skipped("test".into()))
        );
        assert!(report.to_string().contains("skip  close: test"));
    }

    #[test]
    fn a_failed_case_fails_the_report_and_is_listed() {
        let report = AdapterReport {
            capabilities: None,
            outcomes: vec![
                (Case::Capabilities, Outcome::Passed),
                (Case::Values, failed("bytes came back as text")),
            ],
        };
        assert!(!report.passed());
        assert_eq!(
            report.failures().collect::<Vec<_>>(),
            vec![(Case::Values, "bytes came back as text")]
        );
        assert!(report
            .to_string()
            .contains("FAIL  values: bytes came back as text"));
    }

    #[test]
    fn limit_errors_must_be_typed_and_name_an_expected_cap() {
        let typed = PluginError::limit_exceeded(LimitExceeded::new("maxResultRows", 10, 11));
        assert_eq!(expect_limit(&typed, &["maxResultRows"]), Outcome::Passed);
        assert!(matches!(
            expect_limit(&typed, &["maxCellBytes"]),
            Outcome::Failed(reason) if reason.contains("names maxResultRows")
        ));
        let untyped = PluginError::unavailable("too many rows");
        assert!(matches!(
            expect_limit(&untyped, &["maxResultRows"]),
            Outcome::Failed(reason) if reason.contains("got unavailable")
        ));
    }
}
