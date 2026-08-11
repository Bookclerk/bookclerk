//! Proves the media worker is a real jail around real codec work.
//!
//! `bookclerk-sandbox` already tests that an enforced policy blocks undeclared
//! paths. What matters here is the composition: that the worker actually
//! applies a policy (self-confine or spawn-time AppContainer), that the policy
//! is narrow enough to exclude paths the job did not declare, and — just as
//! important — that it is wide enough for LAME, FDK-AAC, and the MP4 muxer to
//! finish the job.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use bookclerk_media::{
    package_m4b_from_pcm, Confinement, FixupRequest, MediaJob, MediaJobReply, JAIL_BIN_ENV,
    JAIL_BIN_NAME, WORKER_ENFORCEMENT_ENV,
};
use bookclerk_sandbox::{Enforcement, NetPolicy, Spec, SPEC_ENV};

const WORKER: &str = env!("CARGO_BIN_EXE_bookclerk-media-worker");

/// Whether this host can confine a media worker (self-confine or spawn-time).
///
/// `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT` demands self-confine (`filesystem`).
/// `BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT` demands any guest confinement
/// (`can_confine_guest`), which is what Windows AppContainer satisfies.
fn confinement_available() -> bool {
    let caps = bookclerk_sandbox::capabilities();
    let self_demanded = std::env::var("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT")
        .is_ok_and(|value| !value.trim().is_empty());
    let spawn_demanded = std::env::var("BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT")
        .is_ok_and(|value| !value.trim().is_empty());
    assert!(
        caps.filesystem || !self_demanded,
        "BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT is set but this host cannot \
         self-confine: {} [{}]",
        caps.detail,
        caps.backend
    );
    assert!(
        caps.can_confine_guest() || !spawn_demanded,
        "BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT is set but this host cannot \
         confine a guest: {} [{}]",
        caps.detail,
        caps.backend
    );
    if needs_spawn_jail() {
        assert!(
            jail_bin().is_some() || !spawn_demanded,
            "spawn enforcement demanded but {JAIL_BIN_NAME} was not found beside the worker"
        );
        return jail_bin().is_some();
    }
    caps.filesystem
}

fn needs_spawn_jail() -> bool {
    let caps = bookclerk_sandbox::capabilities();
    caps.spawn_filesystem && !caps.filesystem
}

fn jail_bin() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(JAIL_BIN_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let worker = PathBuf::from(WORKER);
    let dir = worker.parent()?;
    let name = format!("{JAIL_BIN_NAME}{}", std::env::consts::EXE_SUFFIX);
    [dir.join(&name), dir.join("..").join(&name)]
        .into_iter()
        .find(|candidate| candidate.is_file())
}

/// Write a small but genuine M4B so the codecs have real work to do.
fn make_audiobook(path: &Path) {
    let sample_rate = 44_100;
    let seconds = 2;
    // A quiet sine rather than digital silence, so the encoder cannot take a
    // trivial path through the data.
    let pcm: Vec<i16> = (0..sample_rate * seconds)
        .map(|n| {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let value = ((n as f32 / 40.0).sin() * 2_000.0) as i16;
            value
        })
        .collect();
    package_m4b_from_pcm(
        &pcm,
        u32::try_from(sample_rate).expect("sample rate fits u32"),
        1,
        path,
        &[("One".to_string(), 0), ("Two".to_string(), 1_000)],
    )
    .expect("build fixture audiobook");
}

fn media_spec(job: &MediaJob, confinement: Confinement) -> Spec {
    let enforcement = match confinement {
        Confinement::Required => Enforcement::Required,
        Confinement::BestEffort => Enforcement::BestEffort,
        Confinement::Off => Enforcement::Disabled,
    };
    Spec {
        label: format!("media-worker:{}", job.label()),
        reads: job.read_paths(),
        writes: job.write_dirs(),
        net: NetPolicy::Deny,
        allow_exec: true,
        system_paths: true,
        enforcement,
        preserve_fds: vec![],
        windows_profile_name: None,
        memory_bytes: None,
        active_processes: None,
        cpu_rate_percent: None,
    }
}

/// Outcome of one worker invocation (JSON reply is optional — launcher may fail closed).
struct WorkerRun {
    status: ExitStatus,
    reply: Option<MediaJobReply>,
    stdout: String,
    stderr: String,
}

/// Run one job through the worker binary, matching production pool routing.
///
/// - [`Confinement::Off`]: invoke the worker directly (production uses
///   `Runner::InProcess` and does not require a sandbox allowlist).
/// - Required/BestEffort on spawn-only hosts: route through `bookclerk-jail`.
fn run_worker(job: &MediaJob, confinement: Confinement) -> WorkerRun {
    let request = serde_json::to_vec(job).expect("serialize job");
    let mut child = if confinement == Confinement::Off {
        Command::new(WORKER)
            .env(WORKER_ENFORCEMENT_ENV, confinement.as_env_value())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn worker")
    } else if needs_spawn_jail() {
        let jail = jail_bin().expect("bookclerk-jail beside worker for spawn-time confinement");
        Command::new(jail)
            .arg("--")
            .arg(WORKER)
            .env(WORKER_ENFORCEMENT_ENV, confinement.as_env_value())
            .env(
                SPEC_ENV,
                serde_json::to_string(&media_spec(job, confinement)).expect("encode spec"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn jailed worker")
    } else {
        Command::new(WORKER)
            .env(WORKER_ENFORCEMENT_ENV, confinement.as_env_value())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn worker")
    };
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("worker stdin")
            .write_all(&request)
            .expect("send job");
    }
    let output = child.wait_with_output().expect("await worker");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let reply = if stdout.trim().is_empty() {
        None
    } else {
        Some(serde_json::from_str(&stdout).unwrap_or_else(|err| {
            panic!("unparseable reply ({err})\nstdout: {stdout}\nstderr: {stderr}")
        }))
    };
    WorkerRun {
        status: output.status,
        reply,
        stdout,
        stderr,
    }
}

fn fixup_job(input: &Path, output: &Path) -> MediaJob {
    MediaJob::Fixup {
        request: Box::new(FixupRequest {
            input: input.to_path_buf(),
            output: output.to_path_buf(),
            title: "Confined Title".into(),
            author: Some("Author".into()),
            narrator: Some("Narrator".into()),
            cover: None,
            chapters: vec![("One".into(), 0), ("Two".into(), 1_000)],
            replace_chapters: true,
            subtitle: None,
            publisher: None,
            year: None,
            genre: None,
            series: None,
            series_index: None,
            asin: None,
            isbn: None,
            description: None,
            language: None,
            tool: None,
        }),
    }
}

#[test]
fn worker_completes_real_codec_work_under_required_confinement() {
    if !confinement_available() {
        eprintln!("skipping: no guest confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let input = cache.path().join("book.m4b");
    make_audiobook(&input);
    let output = out.path().join("tagged.m4b");

    let run = run_worker(&fixup_job(&input, &output), Confinement::Required);
    let reply = run.reply.expect("worker should emit a JSON reply");

    match reply {
        MediaJobReply::Ok(_) => {}
        MediaJobReply::Err { message } => {
            panic!(
                "job failed inside the jail: {message}\nstderr: {}",
                run.stderr
            )
        }
    }
    assert!(output.exists(), "worker did not write {}", output.display());

    // Self-confine reports filesystem=enforced; spawn-time AppContainer logs
    // that it is relying on the outer jail. Either proves a jail engaged.
    assert!(
        run.stderr.contains("filesystem=enforced")
            || run.stderr.contains("filesystem=partial")
            || run.stderr.contains("spawn-time AppContainer")
            || run.stderr.contains("AppContainer"),
        "worker/jail did not report an active filesystem jail\nstderr: {}",
        run.stderr
    );
}

/// The job's declared paths are the whole allowlist, so a job that names a
/// symlink is granted the link's *target* — that is the inode the kernel
/// checks. Worth pinning down: it means the host, which chooses these paths
/// from its own cache and output roots, is what keeps the grant honest.
///
/// Unix-only because creating the symlink needs privileges on Windows.
#[cfg(unix)]
#[test]
fn declared_paths_are_granted_at_their_resolved_target() {
    if !confinement_available() {
        eprintln!("skipping: no guest confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");

    let real = elsewhere.path().join("book.m4b");
    make_audiobook(&real);
    let link = cache.path().join("book.m4b");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let output = out.path().join("tagged.m4b");
    let run = run_worker(&fixup_job(&link, &output), Confinement::Required);
    let reply = run.reply.expect("JSON reply");
    assert!(
        matches!(reply, MediaJobReply::Ok(_)),
        "declaring a link should grant its target\nreply: {reply:?}\nstderr: {}",
        run.stderr
    );
}

#[test]
fn worker_reports_a_failure_rather_than_dying_silently() {
    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let missing = cache.path().join("does-not-exist.m4b");
    let output = out.path().join("tagged.m4b");

    // Confinement::Off must not route through bookclerk-jail: production uses
    // Runner::InProcess and never builds a sandbox allowlist for Off.
    let run = run_worker(&fixup_job(&missing, &output), Confinement::Off);
    let reply = run.reply.expect("unconfined worker should emit JSON");
    match reply {
        MediaJobReply::Err { message } => {
            assert!(
                message.contains("does-not-exist"),
                "error should name the missing input, got: {message}"
            );
        }
        MediaJobReply::Ok(_) => panic!(
            "expected failure for a missing input\nstderr: {}",
            run.stderr
        ),
    }
}

/// A job whose input the jail will not cover must fail rather than proceed.
///
/// On spawn-time hosts the launcher rejects missing allowlist paths before
/// CreateProcess (fail closed). On self-confine hosts the worker reports a
/// JSON error. Either way: nonzero failure, no output file, worker not run
/// unconfined past a vanished required path.
#[test]
fn worker_fails_closed_when_a_declared_input_is_unreachable() {
    if !confinement_available() {
        eprintln!("skipping: no guest confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let input = cache.path().join("book.m4b");
    make_audiobook(&input);
    let output = out.path().join("tagged.m4b");

    let job = fixup_job(&input, &output);
    // Remove the input after building the job but before the worker runs, so
    // the worker meets exactly the state a jailed-away path produces.
    std::fs::remove_file(&input).expect("remove input");

    let run = run_worker(&job, Confinement::Required);
    assert!(
        !output.exists(),
        "worker wrote output despite a failed input"
    );

    if needs_spawn_jail() {
        // Launcher-level fail-closed: no JSON reply, stderr names the path.
        assert!(
            !run.status.success(),
            "expected nonzero status from jail, got {:?}\nstderr: {}",
            run.status.code(),
            run.stderr
        );
        assert!(
            run.reply.is_none(),
            "launcher should not start the worker; stdout was: {}",
            run.stdout
        );
        assert!(
            run.stderr.contains("do not exist") || run.stderr.contains("book.m4b"),
            "stderr should name the missing allowlist path\nstderr: {}",
            run.stderr
        );
        assert!(
            !run.stderr.contains("continuing unconfined"),
            "Required must not fall back to an unconfined worker\nstderr: {}",
            run.stderr
        );
    } else {
        let reply = run.reply.expect("self-confine worker emits JSON");
        assert!(
            matches!(reply, MediaJobReply::Err { .. }),
            "expected failure, got {reply:?}\nstderr: {}",
            run.stderr
        );
    }
}
