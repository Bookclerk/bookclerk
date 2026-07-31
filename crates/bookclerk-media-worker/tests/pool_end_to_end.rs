//! Drives the real [`MediaPool`] through the real worker binary.
//!
//! The isolation tests spawn the worker directly. This one goes through the
//! path production uses — `bookclerk-media`'s public async API, the pool's
//! permit accounting, process spawn, and reply parsing — so a regression
//! anywhere in that chain shows up here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bookclerk_media::{
    package_m4b_from_pcm, Confinement, MediaJob, MediaPool, MediaPoolConfig, PackageM4bRequest,
};

const WORKER: &str = env!("CARGO_BIN_EXE_bookclerk-media-worker");

/// See `isolation.rs`: `BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT` makes an
/// unexpected skip a failure rather than silent green.
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

fn confined_pool(workers: usize, confinement: Confinement) -> MediaPool {
    MediaPool::new(MediaPoolConfig {
        workers,
        confinement,
        worker_bin: Some(PathBuf::from(WORKER)),
    })
}

/// The strictest mode this host can actually satisfy.
///
/// `Required` refuses every job where the platform has no self-confinement
/// primitive, which on Windows would turn a test of pool mechanics into a test
/// of the refusal path. Dropping to `best-effort` there keeps the real worker
/// process — spawn, reply parsing, permit accounting — under test.
fn supported_confinement() -> Confinement {
    if bookclerk_sandbox::capabilities().filesystem {
        Confinement::Required
    } else {
        Confinement::BestEffort
    }
}

fn make_audiobook(path: &Path, seconds: usize) {
    let sample_rate = 44_100usize;
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
        &[("One".to_string(), 0)],
    )
    .expect("build fixture audiobook");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_runs_a_real_encode_in_a_confined_worker() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let input = cache.path().join("book.m4b");
    make_audiobook(&input, 2);

    let pool = confined_pool(2, Confinement::Required);
    assert!(
        pool.is_isolated(),
        "pool should have found the worker binary"
    );

    let output = out.path().join("book.mp3");
    let result = pool
        .run(MediaJob::EncodeMp3 {
            input: input.clone(),
            output: output.clone(),
            lame: Box::default(),
            max_sample_rate: None,
        })
        .await
        .expect("encode through the pool");

    assert_eq!(result.output(), Some(output.as_path()));
    let encoded = std::fs::metadata(&output).expect("encoded file");
    assert!(
        encoded.len() > 1_000,
        "encoded MP3 is implausibly small: {} bytes",
        encoded.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_runs_jobs_concurrently_up_to_its_capacity() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let input = cache.path().join("book.m4b");
    make_audiobook(&input, 4);

    let pool = Arc::new(confined_pool(3, Confinement::Required));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for n in 0..6 {
        let pool = Arc::clone(&pool);
        let in_flight = Arc::clone(&in_flight);
        let peak = Arc::clone(&peak);
        let input = input.clone();
        let output = out.path().join(format!("book-{n}.mp3"));
        handles.push(tokio::spawn(async move {
            let running = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(running, Ordering::SeqCst);
            let result = pool
                .run(MediaJob::EncodeMp3 {
                    input,
                    output,
                    lame: Box::default(),
                    max_sample_rate: None,
                })
                .await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        }));
    }

    for handle in handles {
        handle
            .await
            .expect("task joined")
            .expect("job succeeded through the pool");
    }

    // Every job wrote its own file, so none of them clobbered another.
    for n in 0..6 {
        let path = out.path().join(format!("book-{n}.mp3"));
        assert!(path.exists(), "missing output for job {n}");
    }
    assert!(
        peak.load(Ordering::SeqCst) > 1,
        "jobs never overlapped; the pool is not running work concurrently"
    );
}

/// Packaging stages the concatenated AAC payload through a scratch file before
/// it can know the final sample table. That scratch has to land inside the
/// job's declared write directory: the jail grants the output's parent and
/// nothing else, so a scratch file in the system temp directory is denied and
/// packaging fails for every book.
///
/// Both packaging paths stage this way — lossless remux of AAC parts, and
/// transcode of anything else — so both are driven here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pool_packages_m4b_in_a_confined_worker() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let pool = confined_pool(2, Confinement::Required);

    // AAC parts take the lossless remux path.
    let mut parts = Vec::new();
    for n in 0..2 {
        let part = cache.path().join(format!("part-{n}.m4b"));
        make_audiobook(&part, 1);
        parts.push(part);
    }
    let remuxed = out.path().join("remuxed/book.m4b");
    let result = pool
        .run(MediaJob::PackageM4b {
            request: Box::new(PackageM4bRequest {
                parts: parts.clone(),
                output: remuxed.clone(),
                chapter_titles: vec!["One".into(), "Two".into()],
            }),
        })
        .await
        .expect("remux AAC parts through the pool");
    assert_eq!(result.output(), Some(remuxed.as_path()));
    assert!(
        std::fs::metadata(&remuxed).expect("remuxed file").len() > 1_000,
        "remuxed M4B is implausibly small"
    );

    // MP3 parts take the transcode path, which stages through its own scratch.
    let mut mp3_parts = Vec::new();
    for (n, part) in parts.iter().enumerate() {
        let mp3 = cache.path().join(format!("part-{n}.mp3"));
        pool.run(MediaJob::EncodeMp3 {
            input: part.clone(),
            output: mp3.clone(),
            lame: Box::default(),
            max_sample_rate: None,
        })
        .await
        .expect("encode fixture part");
        mp3_parts.push(mp3);
    }
    let transcoded = out.path().join("transcoded/book.m4b");
    pool.run(MediaJob::PackageM4b {
        request: Box::new(PackageM4bRequest {
            parts: mp3_parts,
            output: transcoded.clone(),
            chapter_titles: vec!["One".into(), "Two".into()],
        }),
    })
    .await
    .expect("transcode MP3 parts through the pool");
    assert!(
        std::fs::metadata(&transcoded)
            .expect("transcoded file")
            .len()
            > 1_000,
        "transcoded M4B is implausibly small"
    );

    // The scratch files are temporary, so packaging must not leave them behind
    // in the destination it was granted.
    for dir in [remuxed.parent().unwrap(), transcoded.parent().unwrap()] {
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .expect("read output dir")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter(|name| name != "book.m4b")
            .collect();
        assert!(
            leftovers.is_empty(),
            "packaging left scratch files in {}: {leftovers:?}",
            dir.display()
        );
    }
}

/// A config reload swaps the process-wide pool. Work already holding a handle to
/// the old one has to keep running on it — that is what makes the swap a drain
/// rather than an interruption.
///
/// The handle is taken before the swap and used after it, so the test proves the
/// ordering without depending on how long an encode happens to take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retired_pool_still_finishes_the_work_it_was_holding() {
    if !confinement_available() {
        eprintln!("skipping: no filesystem confinement on this host");
        return;
    }

    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let input = cache.path().join("book.m4b");
    make_audiobook(&input, 2);

    bookclerk_media::replace_pool(confined_pool(2, Confinement::Required));

    // Stands in for a job that has already started: it took its handle, so the
    // reload below cannot pull the pool out from under it.
    let running = bookclerk_media::pool();
    assert_eq!(running.capacity(), 2);

    let retired = bookclerk_media::replace_pool(confined_pool(5, Confinement::Required))
        .expect("a pool was installed");
    assert_eq!(
        bookclerk_media::pool().capacity(),
        5,
        "new work should see the reloaded pool"
    );
    assert_eq!(
        retired.capacity(),
        2,
        "the retired pool should be the one that was running"
    );

    // The retired pool is unreachable to new callers but still fully functional
    // for the job that held it, all the way through a real confined worker.
    let output = out.path().join("book.mp3");
    running
        .run(MediaJob::EncodeMp3 {
            input,
            output: output.clone(),
            lame: Box::default(),
            max_sample_rate: None,
        })
        .await
        .expect("a retired pool must finish the job it was holding");
    assert!(
        std::fs::metadata(&output).expect("encoded file").len() > 1_000,
        "encoded MP3 is implausibly small"
    );
}

/// Runs everywhere, including hosts that cannot confine: what it checks is that
/// a failing job reports its own error and leaves the permits intact, which is
/// pool behaviour rather than jail behaviour.
#[tokio::test]
async fn pool_surfaces_a_job_failure_without_killing_the_caller() {
    let cache = tempfile::tempdir().expect("tempdir");
    let out = tempfile::tempdir().expect("tempdir");
    let pool = confined_pool(1, supported_confinement());

    let err = pool
        .run(MediaJob::PackageM4b {
            request: Box::new(PackageM4bRequest {
                // Deliberately not audio, so the codec rejects it.
                parts: vec![cache.path().join("not-audio.mp3")],
                output: out.path().join("book.m4b"),
                chapter_titles: vec![],
            }),
        })
        .await
        .expect_err("packaging garbage should fail");
    assert!(
        err.to_string().contains("not-audio"),
        "error should name the offending part: {err}"
    );

    // The pool is still usable afterwards: a crashed or failing job must not
    // poison the permit accounting.
    let input = cache.path().join("book.m4b");
    make_audiobook(&input, 1);
    let output = out.path().join("ok.mp3");
    pool.run(MediaJob::EncodeMp3 {
        input,
        output: output.clone(),
        lame: Box::default(),
        max_sample_rate: None,
    })
    .await
    .expect("pool still works after a failed job");
    assert!(output.exists());
}
