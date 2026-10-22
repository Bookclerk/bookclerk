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
//! - no more payload is delivered than the peer's complete `Data` frames
//!   carried and no more streams are accepted than the peer's complete
//!   `Open` frames asked for;
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

/// Frame type byte of a peer `Open`, as `bookclerk_plugin_sdk::mux` numbers it.
const TYPE_OPEN: u8 = 1;

/// Frame type byte of a peer `Data` frame.
const TYPE_DATA: u8 = 2;

/// What the peer's bytes offered, read independently of the mux: the
/// upper bounds the local side's observations must stay under.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Offered {
    /// Complete `Open` frames, duplicates and wrong-parity ids included.
    pub opens: usize,
    /// Payload bytes of complete `Data` frames, on any stream id.
    pub data_bytes: usize,
}

/// Walks the peer's byte stream frame by frame: a big-endian `u32` length
/// covering a type byte, a big-endian `u32` stream id and the payload. Stops
/// at the first incomplete frame or illegal length (shorter than the fixed
/// header), where the mux stops reading too, so nothing after it counts.
#[must_use]
pub fn offered(data: &[u8]) -> Offered {
    let mut out = Offered::default();
    let mut i = 0;
    while let Some(head) = data.get(i..i + MIN_FRAME_BYTES) {
        let len = usize::try_from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]))
            .unwrap_or(usize::MAX);
        if len < 5 {
            break;
        }
        let Some(end) = i.checked_add(4).and_then(|start| start.checked_add(len)) else {
            break;
        };
        if end > data.len() {
            break;
        }
        match head[4] {
            TYPE_OPEN => out.opens += 1,
            TYPE_DATA => out.data_bytes += len - 5,
            _ => {}
        }
        i = end;
    }
    out
}

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
/// the peer's bytes end, more payload is delivered than the peer's `Data`
/// frames carried, more streams are accepted than the peer's `Open` frames
/// asked for, or a mux task outlives the runtime.
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
    let offered = offered(data);
    assert!(
        outcome.received <= offered.data_bytes,
        "delivered {} payload bytes; the peer's Data frames carried {}",
        outcome.received,
        offered.data_bytes
    );
    assert!(
        outcome.accepted <= offered.opens,
        "accepted {} streams; the peer sent {} Open frames",
        outcome.accepted,
        offered.opens
    );
    outcome
}
