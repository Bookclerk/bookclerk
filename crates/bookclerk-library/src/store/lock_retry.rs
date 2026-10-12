//! Repeat a library read while a peer writer holds the SQLite file.

use std::future::Future;
use std::time::Duration;

use crate::error::{LibraryError, Result};

/// Rereads while another connection holds the database file.
const READ_LOCK_ATTEMPTS: u32 = 8;

/// True when `err` is SQLite file-lock contention from an overlapping writer.
///
/// Matches the search rebuild helper: only `SQLITE_BUSY` and `SQLITE_LOCKED`.
/// A guest `maxResultBytes` failure is [`LibraryError::Unavailable`] on some
/// paths and must not be slept on as a lock.
pub(crate) fn is_lock_contention(err: &LibraryError) -> bool {
    let upper = err.to_string().to_ascii_uppercase();
    upper.contains("SQLITE_BUSY") || upper.contains("SQLITE_LOCKED")
}

/// Runs `op` until it succeeds or the lock-contention budget is spent.
///
/// The first wait is 20 ms and doubles, capped at 250 ms. `op` is started
/// again after each wait. A failure that is not file-lock contention returns
/// immediately.
pub(crate) async fn retry_read_lock<T, F, Fut>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut pause = Duration::from_millis(20);
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if is_lock_contention(&err) && attempt + 1 < READ_LOCK_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(Duration::from_millis(250));
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_lock_contention;
    use crate::error::LibraryError;

    #[test]
    fn result_cap_is_not_a_database_lock() {
        let size = LibraryError::Unavailable(
            "query result is 300000 bytes; maxResultBytes is 262144".into(),
        );
        assert!(
            !is_lock_contention(&size),
            "a result-byte cap must not be retried as SQLITE_BUSY"
        );
        let busy = LibraryError::Unavailable("SQLITE_BUSY (5): database is locked".into());
        assert!(is_lock_contention(&busy));
        let locked = LibraryError::Other(anyhow::anyhow!("SQLITE_LOCKED"));
        assert!(is_lock_contention(&locked));
    }
}
