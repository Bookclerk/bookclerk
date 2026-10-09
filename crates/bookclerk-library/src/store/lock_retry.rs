//! Repeat a library read while a peer writer holds the SQLite file.

use std::future::Future;
use std::time::Duration;

use crate::error::{LibraryError, Result};

/// Rereads while another connection holds the database file.
const READ_LOCK_ATTEMPTS: u32 = 8;

/// True when `err` is SQLite file-lock contention from an overlapping writer.
pub(crate) fn is_lock_contention(err: &LibraryError) -> bool {
    match err {
        LibraryError::Unavailable(_) => true,
        other => {
            let upper = other.to_string().to_ascii_uppercase();
            upper.contains("SQLITE_BUSY") || upper.contains("SQLITE_LOCKED")
        }
    }
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
