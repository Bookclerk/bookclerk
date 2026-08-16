//! Runs exactly one media job inside a jail derived from that job.
//!
//! The process reads a JSON [`MediaJob`] on stdin, confines itself to the paths
//! the job declared, runs it, and writes a JSON [`MediaJobReply`] to stdout.
//! One job per process, so the allowlist is only ever as wide as a single
//! book's inputs and one output directory.
//!
//! On Linux and macOS confinement happens here, before any media is touched and
//! while the process is still single-threaded — which is why this is a separate
//! binary rather than a thread in the host.
//!
//! On Windows the host launches this binary through `bookclerk-jail`, which
//! applies an AppContainer at `CreateProcess`. This process then skips
//! self-confine (unsupported) and relies on that spawn-time jail.
//!
//! Everything this binary needs is already resolved by the time it confines
//! itself, so the jail can deny the rest of the filesystem — including the
//! host's `master.key` and `library.db`.

use std::io::{Read, Write};
use std::process::ExitCode;

use bookclerk_media::{Confinement, MediaJob, MediaJobReply, WORKER_ENFORCEMENT_ENV};
use bookclerk_sandbox::{Enforcement, NetPolicy, Policy};

fn main() -> ExitCode {
    let job = match read_job() {
        Ok(job) => job,
        Err(err) => return fail(&format!("could not read job: {err}")),
    };

    // Output directories must exist before the jail is built: Landlock and
    // Seatbelt both reject a rule naming a path that is not there.
    if let Err(err) = job.prepare_output_dirs() {
        return fail(&format!("could not create output directory: {err}"));
    }

    if let Err(err) = confine(&job) {
        return fail(&err);
    }

    reply(&MediaJobReply::from(job.run()))
}

/// Reads one JSON [`MediaJob`] from stdin; empty input is an error.
fn read_job() -> Result<MediaJob, String> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|err| err.to_string())?;
    if buf.is_empty() {
        return Err("no job on stdin".to_string());
    }
    serde_json::from_slice(&buf).map_err(|err| err.to_string())
}

/// Build the jail from what the job declared and apply it.
fn confine(job: &MediaJob) -> Result<(), String> {
    let requested = std::env::var(WORKER_ENFORCEMENT_ENV)
        .ok()
        .and_then(|value| Confinement::from_env_value(&value))
        .unwrap_or_default();

    let enforcement = match requested {
        Confinement::Required => Enforcement::Required,
        Confinement::BestEffort => Enforcement::BestEffort,
        // The host decides whether unconfined work is acceptable. When it is,
        // it runs the job in-process instead of spawning us, so reaching this
        // arm means the operator explicitly turned isolation off.
        Confinement::Off => {
            eprintln!("bookclerk-media-worker: confinement disabled by {WORKER_ENFORCEMENT_ENV}");
            return Ok(());
        }
    };

    let caps = bookclerk_sandbox::capabilities();
    // Spawn-time AppContainer (Windows): the host already launched us through
    // bookclerk-jail. Self-confine would only report Unsupported and fail
    // Required, so trust the outer jail.
    if !caps.filesystem && caps.spawn_filesystem {
        eprintln!(
            "bookclerk-media-worker: relying on spawn-time AppContainer \
             (no self-confinement on this host)"
        );
        return Ok(());
    }

    let policy = Policy::new(format!("media-worker:{}", job.label()))
        .reads(job.read_paths())
        .writes(job.write_dirs())
        // Codecs read and write local files. They have no reason to reach the
        // network, and the host already fetched everything they need.
        .net(NetPolicy::Deny)
        .allow_exec(false)
        .enforcement(enforcement);

    let report = policy
        .confine_current_process()
        .map_err(|err| err.to_string())?;

    // Hosts pipe worker stderr and re-emit via tracing so daemon JSON logs stay
    // structured. Direct test spawns still see this human line on stderr.
    eprintln!("bookclerk-media-worker: {}", report.summary());
    Ok(())
}

/// Writes a JSON [`MediaJobReply`] to stdout; process exit is 0 only for [`MediaJobReply::Ok`].
fn reply(reply: &MediaJobReply) -> ExitCode {
    let encoded = match serde_json::to_vec(reply) {
        Ok(encoded) => encoded,
        Err(err) => {
            eprintln!("bookclerk-media-worker: could not encode reply: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = std::io::stdout();
    if let Err(err) = stdout.write_all(&encoded).and_then(|()| stdout.flush()) {
        eprintln!("bookclerk-media-worker: could not write reply: {err}");
        return ExitCode::FAILURE;
    }
    match reply {
        MediaJobReply::Ok(_) => ExitCode::SUCCESS,
        MediaJobReply::Err { .. } => ExitCode::FAILURE,
    }
}

/// Report a failure that happened before the job could run.
///
/// Still emits a well-formed reply so the host surfaces the reason rather than
/// "worker exited before replying".
fn fail(message: &str) -> ExitCode {
    reply(&MediaJobReply::Err {
        message: message.to_string(),
    })
}
