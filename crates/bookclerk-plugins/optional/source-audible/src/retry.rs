//! Resume an interrupted Audible transfer instead of failing the acquire.
//!
//! audible-rs stages every download in `<dest>.part` and resumes it with a
//! `Range` request on the next call (version-gated by the license's
//! `version_tag`), but a call that loses its connection, stalls or ends
//! short just fails. [`resume_transfer`] calls it again while the failure
//! is one another attempt makes progress on
//! (`audible_rs::downloader::DownloadError::is_transient`), so a dropped
//! connection on a multi-gigabyte title picks up where it stopped.

use std::future::Future;
use std::time::Duration;

use audible_rs::downloader::DownloadError;

/// Attempts per transfer, the first included.
pub(crate) const TRANSFER_ATTEMPTS: u32 = 6;

/// Wait before the first retry; it doubles after each one.
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(1);

/// Longest wait between attempts.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// Runs `transfer` until it succeeds, fails for a reason another attempt
/// would not fix, or has run [`TRANSFER_ATTEMPTS`] times.
///
/// # Arguments
///
/// * `transfer` - One attempt; its argument is `true` for the first, so a
///   caller that discards a stale partial (`force`) does so only then and
///   later attempts resume what the first one wrote.
///
/// # Errors
///
/// Returns the last attempt's [`DownloadError`].
pub(crate) async fn resume_transfer<T, F, Fut>(transfer: F) -> Result<T, DownloadError>
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = Result<T, DownloadError>>,
{
    resume_transfer_after(FIRST_RETRY_WAIT, transfer).await
}

/// [`resume_transfer`] with the first wait given (tests pass zero).
async fn resume_transfer_after<T, F, Fut>(
    first_wait: Duration,
    mut transfer: F,
) -> Result<T, DownloadError>
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = Result<T, DownloadError>>,
{
    let mut wait = first_wait;
    let mut attempt = 1;
    loop {
        match transfer(attempt == 1).await {
            Err(err) if err.is_transient() && attempt < TRANSFER_ATTEMPTS => {
                tracing::warn!(attempt, error = %err, "Audible transfer interrupted; resuming");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(MAX_RETRY_WAIT);
                attempt += 1;
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transient_failures_are_retried_and_others_are_not() {
        let mut calls = Vec::new();
        let out = resume_transfer_after(Duration::ZERO, |first| {
            calls.push(first);
            let n = calls.len();
            async move {
                if n < 3 {
                    Err(DownloadError::Stalled { written: 10 })
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(out.ok(), Some(3));
        assert_eq!(calls, vec![true, false, false]);

        let mut tries = 0;
        let out: Result<(), _> = resume_transfer_after(Duration::ZERO, |_| {
            tries += 1;
            async { Err(DownloadError::MultipartTitle("parts".into())) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(tries, 1);

        let mut tries = 0;
        let out: Result<(), _> = resume_transfer_after(Duration::ZERO, |_| {
            tries += 1;
            async {
                Err(DownloadError::ShortTransfer {
                    written: 1,
                    expected: 2,
                })
            }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(tries, TRANSFER_ATTEMPTS);
    }
}
