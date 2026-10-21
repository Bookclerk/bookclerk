//! A clean restart of the daemon on new bootstrap files.
//!
//! Joining a library as a clerk replaces this install's `[database]`,
//! `master.key` and `node-identity.json`. Those are read once at startup
//! (the database connection, the process DEK and the node id are fixed for
//! the life of the process), so the daemon does not hot-switch: [`request`]
//! asks `main` to stop its listeners gracefully, as on SIGTERM, and `main`
//! then [`reexec`]s the same binary with the same arguments and
//! environment. Nothing new goes on argv; the environment is the one the
//! operator started the daemon with.
//!
//! On Unix the process image is replaced in place (same PID, so a
//! supervisor sees no exit). Elsewhere a new process is started and this one
//! exits; the new one retries binding until the old listeners are gone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use tokio::sync::Notify;

/// Set once a restart was asked for.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Wakes `main`'s listeners when a restart is asked for.
static NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Asks the daemon to restart once in-flight requests finish.
pub fn request(reason: &'static str) {
    if !REQUESTED.swap(true, Ordering::SeqCst) {
        tracing::info!(reason, "daemon restart requested");
    }
    NOTIFY.notify_waiters();
}

/// True once [`request`] was called.
pub fn is_requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// Completes when a restart is asked for.
pub async fn requested() {
    loop {
        let notified = NOTIFY.notified();
        if is_requested() {
            return;
        }
        notified.await;
    }
}

/// Starts this binary again with the same arguments and environment.
///
/// # Errors
///
/// Returns the error when the binary cannot be found or started; on Unix a
/// successful call does not return.
pub fn reexec() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    tracing::info!(exe = %exe.display(), "restarting bookclerkd on the new bootstrap");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        Err(std::process::Command::new(exe).args(args).exec())
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new(exe).args(args).spawn()?;
        std::process::exit(0);
    }
}
