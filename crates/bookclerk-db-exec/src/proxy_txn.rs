//! Fail-closed acknowledgements for SeaORM's infallible proxy transaction hooks.
//!
//! `ProxyDatabaseTrait::{begin, commit, rollback}` return `()`. A failed
//! `BEGIN` that is only logged leaves later statements in autocommit; a failed
//! `COMMIT` still looks like success to [`sea_orm::DatabaseTransaction::commit`].
//! Proxies record a per-task fault instead and refuse subsequent statements.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::ThreadId;
use std::time::{SystemTime, UNIX_EPOCH};

use bookclerk_plugin_abi::DbColumn;
use sea_orm::DbErr;
use tokio::task::{try_id, Id as TaskId};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
/// Per-task identity used to pin sticky begin/commit faults.
enum TaskKey {
    /// Tokio task id when running inside the runtime.
    Tokio(TaskId),
    /// OS thread id when no Tokio task is available.
    Thread(ThreadId),
}

/// Current task key: Tokio id if present, otherwise the OS thread.
fn task_key() -> TaskKey {
    match try_id() {
        Some(id) => TaskKey::Tokio(id),
        None => TaskKey::Thread(std::thread::current().id()),
    }
}

/// Sticky begin/commit fault messages keyed by task (fail-closed after a hook error).
static FAULTS: LazyLock<Mutex<HashMap<TaskKey, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Remaining injected `BEGIN` failures for tests, keyed by task.
static INJECT_BEGIN: LazyLock<Mutex<HashMap<TaskKey, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Remaining injected `COMMIT` failures for tests, keyed by task.
static INJECT_COMMIT: LazyLock<Mutex<HashMap<TaskKey, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Remaining injected savepoint `RELEASE` failures for tests, keyed by task.
static INJECT_SAVEPOINT_RELEASE: LazyLock<Mutex<HashMap<TaskKey, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Remaining injected savepoint `ROLLBACK TO` failures for tests, keyed by task.
static INJECT_SAVEPOINT_ROLLBACK: LazyLock<Mutex<HashMap<TaskKey, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Process-wide savepoint `RELEASE` injections so a txn worker task can observe
/// faults armed by the test task that opened the transaction.
static GLOBAL_SAVEPOINT_RELEASE: AtomicU32 = AtomicU32::new(0);
/// Process-wide savepoint `ROLLBACK TO` injections (see [`GLOBAL_SAVEPOINT_RELEASE`]).
static GLOBAL_SAVEPOINT_ROLLBACK: AtomicU32 = AtomicU32::new(0);
/// Injected atomic session interrupt (cancel or deadline) for tests.
static INJECT_INTERRUPT: LazyLock<Mutex<HashMap<TaskKey, InjectedInterrupt>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Rows seen by the last completed atomic attempt (tests).
static LAST_QUERY_ROWS_SEEN: AtomicUsize = AtomicUsize::new(0);

tokio::task_local! {
    static TASK_EXEC_BUDGET: Arc<ExecBudget>;
}

/// Request-scoped deadline and query row cap for one atomic attempt.
///
/// Stored on the Tokio task that owns the attempt and copied onto the engine
/// connection after it acquires the exclusive lease, so concurrent attempts
/// cannot overwrite each other.
#[derive(Debug)]
pub struct ExecBudget {
    /// Guest-visible deadline (`0` = none).
    deadline_unix_ms: AtomicU64,
    /// Advertised query row cap (`0` = unlimited).
    query_row_cap: AtomicUsize,
    /// Rows materialized by the current capped query.
    query_rows_seen: AtomicUsize,
    /// Engine column metadata for the in-flight query (crosses `spawn_blocking`).
    positional_columns: Mutex<Option<Vec<DbColumn>>>,
}

impl ExecBudget {
    /// Builds a budget for one atomic attempt.
    #[must_use]
    pub fn new(deadline_unix_ms: Option<u64>, query_row_cap: u32) -> Arc<Self> {
        Arc::new(Self {
            deadline_unix_ms: AtomicU64::new(deadline_unix_ms.unwrap_or(0)),
            query_row_cap: AtomicUsize::new(usize::try_from(query_row_cap).unwrap_or(0)),
            query_rows_seen: AtomicUsize::new(0),
            positional_columns: Mutex::new(None),
        })
    }

    /// No deadline and no row cap.
    #[must_use]
    pub fn unlimited() -> Arc<Self> {
        Self::new(None, 0)
    }

    /// True when an armed deadline has elapsed.
    #[must_use]
    pub fn deadline_expired(&self) -> bool {
        let dl = self.deadline_unix_ms.load(Ordering::SeqCst);
        dl > 0 && unix_now_ms() >= dl
    }

    /// Remaining milliseconds until the armed deadline (`None` if unlimited).
    #[must_use]
    pub fn remaining_ms(&self) -> Option<u64> {
        let dl = self.deadline_unix_ms.load(Ordering::SeqCst);
        if dl == 0 {
            return None;
        }
        Some(dl.saturating_sub(unix_now_ms()).max(1))
    }

    /// Advertised query row cap (`None` if unlimited).
    #[must_use]
    pub fn cap(&self) -> Option<usize> {
        let n = self.query_row_cap.load(Ordering::SeqCst);
        (n > 0).then_some(n)
    }

    /// Sets the query row cap (`0` = unlimited).
    pub fn set_query_row_cap(&self, cap: usize) {
        self.query_row_cap.store(cap, Ordering::SeqCst);
    }

    /// Raw cap including `0` for unlimited.
    #[must_use]
    pub fn raw_query_row_cap(&self) -> usize {
        self.query_row_cap.load(Ordering::SeqCst)
    }

    /// Clears the per-query row counter.
    pub fn reset_rows_seen(&self) {
        self.query_rows_seen.store(0, Ordering::SeqCst);
    }

    /// Records one materialized query row; `true` when the caller should stop.
    #[must_use]
    pub fn note_row(&self) -> bool {
        let seen = self.query_rows_seen.fetch_add(1, Ordering::SeqCst) + 1;
        self.cap().is_some_and(|cap| seen > cap)
    }

    /// Rows materialized by the current capped query.
    #[must_use]
    pub fn rows_seen(&self) -> usize {
        self.query_rows_seen.load(Ordering::SeqCst)
    }

    /// Records rusqlite/engine column metadata for the in-flight statement.
    pub fn set_positional_columns(&self, columns: Vec<DbColumn>) {
        *self
            .positional_columns
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(columns);
    }

    /// Takes engine column metadata recorded for this request, if any.
    #[must_use]
    pub fn take_positional_columns(&self) -> Option<Vec<DbColumn>> {
        self.positional_columns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// Phase at which an injected atomic interrupt fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AtomicInterruptPhase {
    /// Before `BEGIN` / HTTP batch.
    BeforeBegin,
    /// Between statements during the transaction.
    BetweenStatements,
    /// Around `COMMIT` / HTTP return (ambiguous).
    AroundCommit,
}

/// Kind of injected atomic interrupt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AtomicInterruptKind {
    /// Session cancel.
    Cancel,
    /// Deadline elapsed.
    Deadline,
}

/// One test-injected interrupt.
#[derive(Clone, Copy, Debug)]
struct InjectedInterrupt {
    /// Phase that should observe the interrupt.
    phase: AtomicInterruptPhase,
    /// Cancel vs deadline.
    kind: AtomicInterruptKind,
    /// Times this phase is observed before the interrupt fires.
    skip: u32,
}

/// Locks the fault map, recovering a poisoned mutex so fail-closed still works.
fn lock_faults() -> std::sync::MutexGuard<'static, HashMap<TaskKey, String>> {
    FAULTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record that `BEGIN` failed so later statements cannot autocommit.
pub fn note_begin_failed(err: impl std::fmt::Display) {
    lock_faults().insert(task_key(), format!("database begin failed: {err}"));
}

/// Record that `COMMIT` failed so the caller cannot treat the txn as durable.
pub fn note_commit_failed(err: impl std::fmt::Display) {
    lock_faults().insert(task_key(), format!("database commit failed: {err}"));
}

/// Whether this task has a sticky begin/commit fault.
#[must_use]
pub fn is_txn_broken() -> bool {
    lock_faults().contains_key(&task_key())
}

/// `DbErr` for refusing statements after a failed begin/commit.
#[must_use]
pub fn txn_broken_err() -> DbErr {
    let msg = lock_faults()
        .get(&task_key())
        .cloned()
        .unwrap_or_else(|| "database transaction is broken".into());
    DbErr::Custom(msg)
}

/// Take and clear this task's sticky fault (after SeaORM commit/rollback).
#[must_use]
pub fn take_txn_fault() -> Option<String> {
    lock_faults().remove(&task_key())
}

/// Queue `n` injected `BEGIN` failures for this task.
pub fn inject_begin_failures(n: u32) {
    INJECT_BEGIN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(task_key(), n);
}

/// Queue `n` injected `COMMIT` failures for this task.
pub fn inject_commit_failures(n: u32) {
    INJECT_COMMIT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(task_key(), n);
}

/// Queue `n` injected nested-savepoint `RELEASE` failures for this task.
///
/// Also arms a process-wide counter so a txn worker (different Tokio task)
/// can observe the fault.
pub fn inject_savepoint_release_failures(n: u32) {
    INJECT_SAVEPOINT_RELEASE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(task_key(), n);
    GLOBAL_SAVEPOINT_RELEASE.store(n, Ordering::SeqCst);
}

/// Queue `n` injected nested-savepoint `ROLLBACK TO` failures for this task.
///
/// Also arms a process-wide counter so a txn worker (different Tokio task)
/// can observe the fault.
pub fn inject_savepoint_rollback_failures(n: u32) {
    INJECT_SAVEPOINT_ROLLBACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(task_key(), n);
    GLOBAL_SAVEPOINT_ROLLBACK.store(n, Ordering::SeqCst);
}

/// Consume one injected begin failure if any remain for this task.
#[must_use]
pub fn consume_begin_injection() -> bool {
    consume_injection(&INJECT_BEGIN)
}

/// Consume one injected commit failure if any remain for this task.
#[must_use]
pub fn consume_commit_injection() -> bool {
    consume_injection(&INJECT_COMMIT)
}

/// Consume one injected savepoint `RELEASE` failure if any remain for this task.
#[must_use]
pub fn consume_savepoint_release_injection() -> bool {
    consume_injection_or_global(&INJECT_SAVEPOINT_RELEASE, &GLOBAL_SAVEPOINT_RELEASE)
}

/// Consume one injected savepoint `ROLLBACK TO` failure if any remain for this task.
#[must_use]
pub fn consume_savepoint_rollback_injection() -> bool {
    consume_injection_or_global(&INJECT_SAVEPOINT_ROLLBACK, &GLOBAL_SAVEPOINT_ROLLBACK)
}

/// Queue an atomic session interrupt for this task (cancel or deadline at `phase`).
pub fn inject_atomic_interrupt(phase: AtomicInterruptPhase, kind: AtomicInterruptKind) {
    inject_atomic_interrupt_after(phase, kind, 0);
}

/// Like [`inject_atomic_interrupt`], ignoring `skip` observations of `phase` first.
pub fn inject_atomic_interrupt_after(
    phase: AtomicInterruptPhase,
    kind: AtomicInterruptKind,
    skip: u32,
) {
    INJECT_INTERRUPT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(task_key(), InjectedInterrupt { phase, kind, skip });
}

/// Consume an injected interrupt when the executor reaches `phase`.
#[must_use]
pub fn consume_atomic_interrupt(phase: AtomicInterruptPhase) -> Option<AtomicInterruptKind> {
    let mut map = INJECT_INTERRUPT.lock().unwrap_or_else(|e| e.into_inner());
    let key = task_key();
    match map.get_mut(&key).copied() {
        Some(inj) if inj.phase == phase => {
            if inj.skip > 0 {
                if let Some(slot) = map.get_mut(&key) {
                    slot.skip -= 1;
                }
                None
            } else {
                map.remove(&key);
                Some(inj.kind)
            }
        }
        _ => None,
    }
}

/// Decrements one injected failure for this task; returns true when a fault should fire.
fn consume_injection(map: &Mutex<HashMap<TaskKey, u32>>) -> bool {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    let key = task_key();
    match map.get_mut(&key) {
        Some(n) if *n > 0 => {
            *n -= 1;
            if *n == 0 {
                map.remove(&key);
            }
            true
        }
        _ => false,
    }
}

/// Consumes a task-local injection, then a process-wide counter.
///
/// Nested `executeAtomic` runs on a txn worker task, so tests that arm
/// savepoint faults from the caller still need the worker to observe them.
/// Both counters are decremented together so same-task unit tests do not
/// double-fire.
fn consume_injection_or_global(map: &Mutex<HashMap<TaskKey, u32>>, global: &AtomicU32) -> bool {
    let task_hit = consume_injection(map);
    let global_hit = loop {
        let n = global.load(Ordering::SeqCst);
        if n == 0 {
            break false;
        }
        match global.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break true,
            Err(_) => continue,
        }
    };
    task_hit || global_hit
}

/// Unix time in milliseconds.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Runs `fut` with `budget` as the request-scoped execution budget.
pub async fn with_exec_budget<F, Fut, T>(budget: Arc<ExecBudget>, fut: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    TASK_EXEC_BUDGET.scope(budget, fut()).await
}

/// Request-scoped budget for the current Tokio task, if an atomic attempt armed one.
#[must_use]
pub fn current_exec_budget() -> Option<Arc<ExecBudget>> {
    TASK_EXEC_BUDGET.try_with(Arc::clone).ok()
}

/// Suspends the execute-request row cap until the guard is dropped.
///
/// Catalog / `sqlite_master` loads are not guest result rows; they must not
/// fail a tight `maxResultRows` on the surrounding execute.
#[must_use]
pub fn suspend_execute_row_cap() -> Option<ExecuteRowCapGuard> {
    let budget = current_exec_budget()?;
    let prev = budget.raw_query_row_cap();
    budget.set_query_row_cap(0);
    Some(ExecuteRowCapGuard { budget, prev })
}

/// Restores [`ExecBudget::raw_query_row_cap`] when dropped.
pub struct ExecuteRowCapGuard {
    /// Budget whose row cap is restored on drop.
    budget: Arc<ExecBudget>,
    /// Cap to restore (`0` = unlimited).
    prev: usize,
}

impl Drop for ExecuteRowCapGuard {
    fn drop(&mut self) {
        self.budget.set_query_row_cap(self.prev);
    }
}

/// Records `seen` for [`query_rows_seen`] after an atomic attempt completes.
pub fn record_query_rows_seen(seen: usize) {
    LAST_QUERY_ROWS_SEEN.store(seen, Ordering::SeqCst);
}

/// Legacy no-op. Budgets are request-scoped via [`with_exec_budget`].
pub fn arm_exec_budget(_deadline_unix_ms: Option<u64>, _query_row_cap: u32) {}

/// Legacy no-op. Budgets are request-scoped via [`with_exec_budget`].
pub fn clear_exec_budget() {}

/// True when the current task's armed deadline has elapsed.
#[must_use]
pub fn exec_deadline_expired() -> bool {
    current_exec_budget().is_some_and(|b| b.deadline_expired())
}

/// Remaining milliseconds until the current task's deadline (`None` if unlimited).
#[must_use]
pub fn exec_deadline_remaining_ms() -> Option<u64> {
    current_exec_budget().and_then(|b| b.remaining_ms())
}

/// Advertised query row cap for the current task (`None` if unlimited).
#[must_use]
pub fn query_row_cap() -> Option<usize> {
    current_exec_budget().and_then(|b| b.cap())
}

/// Records one materialized query row against the current task budget.
#[must_use]
pub fn note_query_row() -> bool {
    current_exec_budget().is_some_and(|b| b.note_row())
}

/// Rows materialized by the last completed capped query (tests).
#[must_use]
pub fn query_rows_seen() -> usize {
    LAST_QUERY_ROWS_SEEN.load(Ordering::SeqCst)
}
