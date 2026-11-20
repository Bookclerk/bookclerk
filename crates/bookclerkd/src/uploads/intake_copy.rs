//! Resumable copy of one drop folder file into staging.
//!
//! A read from storage can break partway, as an S3 drop folder's stream
//! may. The copy keeps what it already staged and asks the backend again
//! from the staged length, a few times with backoff, before it gives up.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Reads tried for one file: the first plus three resumptions.
pub(crate) const COPY_ATTEMPTS: u32 = 4;

/// A body read from storage, from the requested offset.
pub(crate) type Body = Pin<Box<dyn AsyncRead + Send>>;

/// Why a resumable copy stopped without the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyFailure {
    /// The file could not be read, or staging could not be written, after
    /// every attempt.
    Unavailable,
    /// The file no longer has the size it settled with.
    Changed,
}

/// Copies `size` bytes into `path`, resuming from the staged length when a
/// read breaks.
///
/// `open(offset, length)` returns the body from byte `offset`, at most
/// `length` bytes long. Each attempt asks for one byte past the settled
/// size, so a file that grew is seen.
///
/// # Arguments
///
/// * `open` - Opens the source at an offset; an error counts as a broken
///   read and is retried.
/// * `path` - The staging file, created or truncated on the first attempt.
/// * `size` - The size the file settled with, in bytes.
/// * `backoff` - Wait before the first resumption; it doubles after each.
///
/// # Errors
///
/// [`CopyFailure::Changed`] when the source holds more or fewer bytes than
/// `size`; [`CopyFailure::Unavailable`] when staging cannot be written or
/// every attempt broke.
pub(crate) async fn copy_resumable<F, Fut>(
    mut open: F,
    path: &Path,
    size: u64,
    backoff: Duration,
) -> Result<(), CopyFailure>
where
    F: FnMut(u64, u64) -> Fut,
    Fut: Future<Output = std::io::Result<Body>>,
{
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|_| CopyFailure::Unavailable)?;
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|_| CopyFailure::Unavailable)?;
    let mut staged: u64 = 0;
    let mut wait = backoff;
    for attempt in 0..COPY_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(wait).await;
            wait = wait.saturating_mul(2);
        }
        // One byte past the settled size is enough to see it grew.
        let want = size.saturating_add(1).saturating_sub(staged);
        let Ok(body) = open(staged, want).await else {
            continue;
        };
        let mut limited = body.take(want);
        let (copied, broke) = copy_counting(&mut limited, &mut file).await;
        staged = staged.saturating_add(copied);
        if staged > size {
            return Err(CopyFailure::Changed);
        }
        if broke == Broke::Write {
            return Err(CopyFailure::Unavailable);
        }
        if broke == Broke::Read {
            continue;
        }
        if staged != size {
            return Err(CopyFailure::Changed);
        }
        file.sync_all()
            .await
            .map_err(|_| CopyFailure::Unavailable)?;
        return Ok(());
    }
    Err(CopyFailure::Unavailable)
}

/// Which side of a copy stopped it early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broke {
    /// The copy reached the end of the body.
    No,
    /// The body returned an error.
    Read,
    /// The staging file returned an error.
    Write,
}

/// Copies `body` into `file` and returns the bytes written with which side,
/// if either, stopped it.
async fn copy_counting(
    body: &mut (impl AsyncRead + Unpin),
    file: &mut tokio::fs::File,
) -> (u64, Broke) {
    let mut buf = vec![0_u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = match body.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return (written, Broke::Read),
        };
        let Some(chunk) = buf.get(..n) else {
            return (written, Broke::Read);
        };
        if file.write_all(chunk).await.is_err() {
            return (written, Broke::Write);
        }
        written = written.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    if file.flush().await.is_err() {
        return (written, Broke::Write);
    }
    (written, Broke::No)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A reader that yields `data` then fails, as a dropped connection does.
    struct Breaking {
        /// Bytes left to yield before the error.
        data: std::io::Cursor<Vec<u8>>,
        /// Whether to fail at the end instead of ending cleanly.
        fail: bool,
    }

    impl AsyncRead for Breaking {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let fail = self.fail;
            let polled = Pin::new(&mut self.data).poll_read(cx, buf);
            if fail && buf.filled().len() == before {
                return std::task::Poll::Ready(Err(std::io::Error::other("connection reset")));
            }
            polled
        }
    }

    /// Opens `source` at an offset, breaking after `chunk` bytes for the
    /// first `breaks` opens, and records each offset asked for.
    fn opener(
        source: Vec<u8>,
        chunk: usize,
        breaks: usize,
        offsets: Arc<Mutex<Vec<u64>>>,
    ) -> impl FnMut(u64, u64) -> std::future::Ready<std::io::Result<Body>> {
        let mut opened = 0_usize;
        move |offset, length| {
            offsets.lock().expect("offsets").push(offset);
            let start = usize::try_from(offset).expect("offset");
            let end = usize::try_from(offset.saturating_add(length))
                .expect("end")
                .min(source.len());
            let mut slice = source.get(start..end).unwrap_or_default().to_vec();
            let fail = opened < breaks;
            if fail {
                slice.truncate(chunk);
            }
            opened += 1;
            let body: Body = Box::pin(Breaking {
                data: std::io::Cursor::new(slice),
                fail,
            });
            std::future::ready(Ok(body))
        }
    }

    #[tokio::test]
    async fn a_broken_read_resumes_from_the_staged_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("staged/1");
        let source: Vec<u8> = (0..10_000_u32)
            .map(|i| u8::try_from(i % 251).expect("byte"))
            .collect();
        let offsets = Arc::new(Mutex::new(Vec::new()));
        let open = opener(source.clone(), 3_000, 2, offsets.clone());
        copy_resumable(open, &path, 10_000, Duration::ZERO)
            .await
            .expect("copied");
        assert_eq!(std::fs::read(&path).expect("read"), source);
        assert_eq!(*offsets.lock().expect("offsets"), vec![0, 3_000, 6_000]);
    }

    #[tokio::test]
    async fn a_read_that_keeps_breaking_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("1");
        let offsets = Arc::new(Mutex::new(Vec::new()));
        let open = opener(vec![7; 10_000], 100, usize::MAX, offsets.clone());
        let failed = copy_resumable(open, &path, 10_000, Duration::ZERO).await;
        assert_eq!(failed, Err(CopyFailure::Unavailable));
        assert_eq!(
            offsets.lock().expect("offsets").len(),
            usize::try_from(COPY_ATTEMPTS).expect("attempts")
        );
    }

    #[tokio::test]
    async fn a_file_that_grew_or_shrank_is_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("1");
        let offsets = Arc::new(Mutex::new(Vec::new()));
        let grew = opener(vec![1; 120], 50, 1, offsets.clone());
        assert_eq!(
            copy_resumable(grew, &path, 100, Duration::ZERO).await,
            Err(CopyFailure::Changed)
        );
        let shrank = opener(vec![1; 80], 50, 1, offsets);
        assert_eq!(
            copy_resumable(shrank, &path, 100, Duration::ZERO).await,
            Err(CopyFailure::Changed)
        );
    }

    #[tokio::test]
    async fn an_open_error_is_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("1");
        let mut calls = 0_u32;
        let open = move |_offset: u64, _length: u64| {
            calls += 1;
            let result: std::io::Result<Body> = if calls == 1 {
                Err(std::io::Error::other("timeout"))
            } else {
                Ok(Box::pin(std::io::Cursor::new(vec![9_u8; 5])))
            };
            std::future::ready(result)
        };
        copy_resumable(open, &path, 5, Duration::ZERO)
            .await
            .expect("copied");
        assert_eq!(std::fs::read(&path).expect("read"), vec![9; 5]);
    }
}
