//! Reads a media worker's stdout or stderr on a thread this crate owns.
//!
//! On Windows, Tokio reads a child's anonymous pipes with blocking
//! `ReadFile` calls on the runtime's blocking pool, and a runtime cannot be
//! dropped until every blocking-pool thread returns. The tag and cover
//! workers are long-lived and belong to the process-wide pool, so they
//! outlive the runtime that started them: dropping that runtime (the end of
//! a `#[tokio::test]`, or a host shutting down) then waits forever on a read
//! from a worker that is still running. Reading on a dedicated thread keeps
//! the pipe out of the runtime, so the runtime shuts down and the thread
//! ends by itself once the worker exits or its reader is dropped.
//!
//! The same path is used everywhere so every platform reads worker output
//! the same way.

use std::io::Read;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;

/// Bytes read from the pipe per chunk.
const CHUNK_BYTES: usize = 8 * 1024;

/// Chunks the thread may read ahead of the async reader before it waits.
const CHUNKS_AHEAD: usize = 4;

/// A worker pipe end that can become a blocking file.
pub(crate) trait WorkerPipe: Send + 'static {
    /// Takes the pipe out of the async runtime as a blocking file.
    ///
    /// # Errors
    ///
    /// Returns the I/O error from detaching the pipe from the runtime.
    fn into_blocking_file(self) -> std::io::Result<std::fs::File>;
}

/// Implements [`WorkerPipe`] for one of Tokio's child pipe ends.
macro_rules! worker_pipe {
    ($pipe:ty) => {
        impl WorkerPipe for $pipe {
            fn into_blocking_file(self) -> std::io::Result<std::fs::File> {
                #[cfg(unix)]
                let owned = self.into_owned_fd()?;
                #[cfg(windows)]
                let owned = self.into_owned_handle()?;
                Ok(std::fs::File::from(owned))
            }
        }
    };
}

worker_pipe!(tokio::process::ChildStdout);
worker_pipe!(tokio::process::ChildStderr);

/// An [`AsyncRead`] over a worker pipe that a dedicated thread reads.
///
/// Ends (reads return 0 bytes) when the worker closes the pipe; reports the
/// thread's read error once, then ends. Dropping it lets the thread exit
/// after its current read.
pub(crate) struct ThreadPipe {
    /// Chunks (or the read error) from the thread.
    chunks: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    /// The chunk being handed out.
    current: Vec<u8>,
    /// Bytes of `current` already handed out.
    taken: usize,
    /// Set once the pipe ended or failed.
    ended: bool,
}

impl ThreadPipe {
    /// Starts the thread that reads `pipe`.
    ///
    /// # Arguments
    ///
    /// * `name` - The thread's name, for debuggers and panics.
    /// * `pipe` - The worker's stdout or stderr.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the pipe cannot be detached from the
    /// runtime or the thread cannot be started.
    pub(crate) fn spawn(name: String, pipe: impl WorkerPipe) -> std::io::Result<Self> {
        let mut file = pipe.into_blocking_file()?;
        let (tx, chunks) = mpsc::channel(CHUNKS_AHEAD);
        std::thread::Builder::new().name(name).spawn(move || loop {
            let mut chunk = vec![0u8; CHUNK_BYTES];
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    chunk.truncate(read);
                    if tx.blocking_send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    let _ = tx.blocking_send(Err(err));
                    break;
                }
            }
        })?;
        Ok(Self {
            chunks,
            current: Vec::new(),
            taken: 0,
            ended: false,
        })
    }
}

impl AsyncRead for ThreadPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            let this = &mut *self;
            if let Some(rest) = this
                .current
                .get(this.taken..)
                .filter(|rest| !rest.is_empty())
            {
                let n = rest.len().min(buf.remaining());
                buf.put_slice(rest.get(..n).unwrap_or_default());
                this.taken += n;
                return Poll::Ready(Ok(()));
            }
            if this.ended {
                return Poll::Ready(Ok(()));
            }
            match this.chunks.poll_recv(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.current = chunk;
                    this.taken = 0;
                }
                Poll::Ready(Some(Err(err))) => {
                    this.ended = true;
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(None) => {
                    this.ended = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Clears a worker's `alive` flag when dropped.
///
/// A reply reader holds one, so a reader that ends, or is cancelled because
/// its runtime shut down, leaves the worker marked dead and the next caller
/// starts a new one instead of writing to a worker nobody reads.
pub(crate) struct ClearOnDrop(pub(crate) Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

    /// A command that prints `text` and exits, on any platform.
    fn echo(text: &str) -> tokio::process::Command {
        #[cfg(windows)]
        {
            let mut command = tokio::process::Command::new("cmd");
            command.args(["/C", &format!("echo {text}")]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", &format!("echo {text}")]);
            command
        }
    }

    #[tokio::test]
    async fn a_thread_pipe_reads_a_child_to_its_end() {
        let mut child = echo("hello").stdout(Stdio::piped()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut pipe = ThreadPipe::spawn("test-pipe".into(), stdout).unwrap();
        let mut out = String::new();
        pipe.read_to_string(&mut out).await.unwrap();
        assert_eq!(out.trim(), "hello");
        assert!(child.wait().await.unwrap().success());
    }

    /// The regression: a child still running when its runtime is dropped
    /// must not keep the runtime from shutting down.
    #[test]
    fn a_live_child_does_not_hold_its_runtime_open() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (child, pipe) = runtime.block_on(async {
            #[cfg(windows)]
            let mut command = {
                let mut command = tokio::process::Command::new("cmd");
                command.args(["/C", "ping -n 60 127.0.0.1 >NUL"]);
                command
            };
            #[cfg(not(windows))]
            let mut command = {
                let mut command = tokio::process::Command::new("sh");
                command.args(["-c", "sleep 60"]);
                command
            };
            let mut child = command
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let mut pipe = ThreadPipe::spawn("test-pipe".into(), stdout).unwrap();
            // Park a read on the pipe, as the reply reader does.
            let reader = tokio::spawn(async move {
                let mut byte = [0u8; 1];
                let _ = pipe.read(&mut byte).await;
            });
            (child, reader)
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(runtime);
            let _ = done_tx.send(());
        });
        let dropped = done_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .is_ok();
        drop(pipe);
        drop(child);
        assert!(dropped, "dropping the runtime waited on the child's pipe");
    }
}
