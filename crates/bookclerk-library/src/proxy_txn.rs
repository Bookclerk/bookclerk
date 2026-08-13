//! Fail-closed acknowledgements for SeaORM's infallible proxy transaction hooks.
//!
//! `ProxyDatabaseTrait::{begin, commit, rollback}` return `()`. A failed
//! `BEGIN` that is only logged leaves later statements in autocommit; a failed
//! `COMMIT` still looks like success to [`sea_orm::DatabaseTransaction::commit`].
//! Proxies record a per-task fault instead, refuse subsequent statements, and
//! [`LibraryStore`](crate::LibraryStore) turns that fault into an error after
//! SeaORM returns.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::thread::ThreadId;

use sea_orm::DbErr;
use tokio::task::{try_id, Id as TaskId};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TaskKey {
    Tokio(TaskId),
    Thread(ThreadId),
}

fn task_key() -> TaskKey {
    match try_id() {
        Some(id) => TaskKey::Tokio(id),
        None => TaskKey::Thread(std::thread::current().id()),
    }
}

static FAULTS: LazyLock<Mutex<HashMap<TaskKey, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static INJECT_BEGIN: LazyLock<Mutex<HashMap<TaskKey, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static INJECT_COMMIT: LazyLock<Mutex<HashMap<TaskKey, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
