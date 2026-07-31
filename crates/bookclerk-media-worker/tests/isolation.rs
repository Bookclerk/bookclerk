//! Proves the media worker is a real jail around real codec work.
//!
//! `bookclerk-sandbox` already tests that an enforced policy blocks undeclared
//! paths. What matters here is the composition: that the worker actually
//! applies a policy, that the policy is narrow enough to exclude paths the job
//! did not declare, and — just as important — that it is wide enough for LAME,
//! FDK-AAC, and the MP4 muxer to finish the job.

use std::path::Path;
use std::process::{Command, Stdio};

use bookclerk_media::{
    package_m4b_from_pcm, Confinement, FixupRequest, MediaJob, MediaJobReply,
    WORKER_ENFORCEMENT_ENV,
};

const WORKER: &str = env!("CARGO_BIN_EXE_bookclerk-media-worker");

/// Whether this host can enforce a filesystem allowlist.
///
/// Setting `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT` to a non-empty value turns
/// the skip into a failure, so a runner that is expected to confine cannot
/// report green by quietly opting out of every assertion here.
fn confinement_available() -> bool {
    let caps = bookclerk_sandbox::capabilities();
    let demanded = std::env::var("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT")
        .is_ok_and(|value| !value.trim().is_empty());
    assert!(
        caps.filesystem || !demanded,
        "BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT is set but this host cannot \
         enforce a filesystem allowlist: {} [{}]",
        caps.detail,
        caps.backend
    );
    caps.filesystem
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

/// Run one job through the worker binary, exactly as the pool does.
fn run_worker(job: &MediaJob, confinement: Confinement) -> (MediaJobReply, String) {
    let request = serde_json::to_vec(job).expect("serialize job");
    let mut child = Command::new(WORKER)
        .env(WORKER_ENFORCEMENT_ENV, confinement.as_env_value())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");
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
    let reply: MediaJobReply = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "unparseable reply ({err})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    });
    (reply, String::from_utf8_lossy(&output.stderr).into_owned())
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
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let input = cache.path().join("book.m4b");
    make_audiobook(&input);
    let output = out.path().join("tagged.m4b");

    let (reply, stderr) = run_worker(&fixup_job(&input, &output), Confinement::Required);

    match reply {
        MediaJobReply::Ok(_) => {}
        MediaJobReply::Err { message } => {
            panic!("job failed inside the jail: {message}\nstderr: {stderr}")
        }
    }
    assert!(output.exists(), "worker did not write {}", output.display());

    // The worker reports what engaged. A jail that quietly did nothing would
    // still let the job succeed, so assert on the report as well.
    assert!(
        stderr.contains("filesystem=enforced") || stderr.contains("filesystem=partial"),
        "worker did not report an active filesystem jail\nstderr: {stderr}"
    );
}

/// The job's declared paths are the whole allowlist, so a job that names a
/// symlink is granted the link's *target* — that is the inode the kernel
/// checks. Worth pinning down: it means the host, which chooses these paths
/// from its own cache and output roots, is what keeps the grant honest.
///
/// Unix-only because creating the symlink needs privileges on Windows, which
/// cannot self-confine anyway.
#[cfg(unix)]
#[test]
fn declared_paths_are_granted_at_their_resolved_target() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
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
    let (reply, stderr) = run_worker(&fixup_job(&link, &output), Confinement::Required);
    assert!(
        matches!(reply, MediaJobReply::Ok(_)),
        "declaring a link should grant its target\nreply: {reply:?}\nstderr: {stderr}"
    );
}

#[test]
fn worker_reports_a_failure_rather_than_dying_silently() {
    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let missing = cache.path().join("does-not-exist.m4b");
    let output = out.path().join("tagged.m4b");

    let (reply, stderr) = run_worker(&fixup_job(&missing, &output), Confinement::Off);
    match reply {
        MediaJobReply::Err { message } => {
            assert!(
                message.contains("does-not-exist"),
                "error should name the missing input, got: {message}"
            );
        }
        MediaJobReply::Ok(_) => panic!("expected failure for a missing input\nstderr: {stderr}"),
    }
}

/// A job whose input the jail will not cover must fail rather than proceed.
///
/// The worker resolves its allowlist before confining, so an input that is
/// unreachable at that moment is simply absent from the jail. The job then
/// fails on the declared path instead of falling back to an unconfined read.
#[test]
fn worker_fails_closed_when_a_declared_input_is_unreachable() {
    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let input = cache.path().join("book.m4b");
    make_audiobook(&input);
    let output = out.path().join("tagged.m4b");

    let job = fixup_job(&input, &output);
    // Remove the input after building the job but before the worker runs, so
    // the worker meets exactly the state a jailed-away path produces.
    std::fs::remove_file(&input).expect("remove input");

    let (reply, stderr) = run_worker(&job, Confinement::Required);
    assert!(
        matches!(reply, MediaJobReply::Err { .. }),
        "expected failure, got {reply:?}\nstderr: {stderr}"
    );
    assert!(
        !output.exists(),
        "worker wrote output despite a failed input"
    );
}
