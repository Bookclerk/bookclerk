//! Shared oracle for the `mux_peer` fuzz target and the `bookclerk-plugin-sdk`
//! corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is the byte stream a peer sends over the multiplexed link
//! (`bookclerk_plugin_sdk::mux`): length-prefixed `Open`, `Data`, `Close` and
//! `Window` frames in any state, order or shape. It is fed to a server-side
//! [`Mux`] whose local side behaves like a host: it opens one stream toward
//! the peer, accepts every stream the peer opens and drains it to end of
//! stream. The invariants are liveness and bounds, not "no panic" alone:
//!
//! - once the peer's bytes end the mux settles within [`SETTLE`]: every
//!   `accept` returns, every accepted stream reaches end of stream, and both
//!   mux tasks finish after `shutdown`;
//! - no more payload is delivered than the peer sent and no more streams are
//!   accepted than the peer could have opened;
//! - no mux task outlives its runtime.

use std::io::Cursor;
use std::time::Duration;

use bookclerk_plugin_sdk::mux::{live_mux_task_count, Mux};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

/// Largest peer stream worth replaying; the daily batch grows inputs well
/// past a single frame but a multi-megabyte stream buys no new states.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

/// How long the mux may take to wind down after the peer's bytes are
/// exhausted. Generous for CI; a healthy mux settles in microseconds.
pub const SETTLE: Duration = Duration::from_secs(5);

/// Smallest wire frame: `u32` length, type byte and `u32` connection id.
pub const MIN_FRAME_BYTES: usize = 9;

/// What the local side observed while the peer's bytes were consumed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Streams the peer opened that the local side accepted.
    pub accepted: usize,
    /// Payload bytes delivered on those streams, all of them together.
    pub received: usize,
    /// Whether the local open and the first write toward the peer succeeded.
    pub local_write_ok: bool,
}

/// Feeds `data` to a server mux as the peer's byte stream, drains every
/// accepted stream, shuts the mux down and checks the invariants listed in
/// the module documentation. Inputs over [`MAX_INPUT_BYTES`] are skipped and
/// yield the default outcome.
///
/// # Panics
///
/// When an invariant fails: the mux does not settle within [`SETTLE`] after
/// the peer's bytes end, more payload is delivered than the peer sent, more
/// streams are accepted than the peer could have opened, or a mux task
/// outlives the runtime.
#[must_use]
pub fn check(data: &[u8]) -> Outcome {
    if data.len() > MAX_INPUT_BYTES {
        return Outcome::default();
    }
    let runtime = runtime();
    let outcome = runtime.block_on(drive(data));
    drop(runtime);
    assert_eq!(
        live_mux_task_count(),
        0,
        "mux tasks outlived their runtime after {outcome:?}"
    );
    outcome
}

/// Single-threaded runtime with the timer the settle checks need.
// Harness setup: a runtime that cannot be built is the harness failing, not an
// input the mux must tolerate.
#[allow(clippy::expect_used)]
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio current-thread runtime")
}

/// Runs the local side against the peer's bytes and returns what it saw.
async fn drive(data: &[u8]) -> Outcome {
    let mux = Mux::server(Cursor::new(data.to_vec()), tokio::io::sink());
    let mut outcome = Outcome::default();
    // A locally opened stream gives peer `Window` frames a live target and
    // exercises the writer alongside the reader.
    let local = match mux.open().await {
        Ok(mut stream) => {
            outcome.local_write_ok = stream.write_all(b"host").await.is_ok();
            Some(stream)
        }
        Err(_) => None,
    };
    let mut drains = Vec::new();
    loop {
        match timeout(SETTLE, mux.accept()).await {
            Ok(Ok(mut stream)) => {
                outcome.accepted += 1;
                drains.push(tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut total = 0usize;
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => total += n,
                        }
                    }
                    total
                }));
            }
            // The reader exited: the peer's bytes ended or a frame was rejected.
            Ok(Err(_)) => break,
            Err(_) => {
                panic!("mux accept did not settle within {SETTLE:?} after the peer's bytes ended")
            }
        }
    }
    for drain in drains {
        match timeout(SETTLE, drain).await {
            Ok(Ok(received)) => outcome.received += received,
            Ok(Err(err)) => panic!("stream drain task failed: {err}"),
            Err(_) => panic!("an accepted stream did not reach end of stream within {SETTLE:?}"),
        }
    }
    drop(local);
    mux.shutdown();
    assert!(
        timeout(SETTLE, mux.closed()).await.is_ok(),
        "mux tasks did not finish within {SETTLE:?} after shutdown"
    );
    assert!(
        outcome.received <= data.len(),
        "delivered {} payload bytes from a {}-byte peer stream",
        outcome.received,
        data.len()
    );
    assert!(
        outcome.accepted <= data.len() / MIN_FRAME_BYTES,
        "accepted {} streams from a {}-byte peer stream",
        outcome.accepted,
        data.len()
    );
    outcome
}
