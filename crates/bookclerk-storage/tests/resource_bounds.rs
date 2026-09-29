//! Large-object and large-namespace measurements.
//!
//! These are ignored in the default suite. Run:
//!
//! ```text
//! cargo test -p bookclerk-storage --test resource_bounds -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Caps are fixed before the run. Absolute RSS cap is 1 GiB (the focused
//! #119 envelope). Allowed RSS delta between a 32 MiB and a 256 MiB transfer
//! is 32 MiB. The 256 MiB object is larger than that delta, so a pass means
//! resident memory did not track object size.

use std::fs::File;
use std::path::Path;
use std::time::Instant;

use bookclerk_storage::{
    transfer_object, LocalFsBackend, ObjectMeta, StorageBackend, TransferOptions,
};

const ABSOLUTE_CAP: u64 = 1024 * 1024 * 1024;
const ALLOWED_DELTA: u64 = 32 * 1024 * 1024;
const SMALL: u64 = 32 * 1024 * 1024;
const LARGE: u64 = 256 * 1024 * 1024;

fn vm_hwm_bytes() -> u64 {
    let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            return kb.saturating_mul(1024);
        }
    }
    0
}

fn cgroup_peak_bytes() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.peak",
        "/sys/fs/cgroup/memory.current",
    ] {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(n) = text.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

fn sparse_file(path: &Path, len: u64) {
    let file = File::create(path).unwrap();
    file.set_len(len).unwrap();
}

async fn transfer_sparse(len: u64) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src.bin");
    sparse_file(&src_path, len);
    let src = LocalFsBackend::new(dir.path().join("in")).unwrap();
    let dst = LocalFsBackend::new(dir.path().join("out")).unwrap();
    // Seed via put_file (kernel copy) so the transfer reads a real object.
    src.put_file(
        "obj.bin",
        &src_path,
        ObjectMeta {
            content_length: Some(len),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let before = vm_hwm_bytes();
    let started = Instant::now();
    transfer_object(
        &src,
        "obj.bin",
        &dst,
        "obj.bin",
        ObjectMeta {
            content_length: Some(len),
            ..Default::default()
        },
        &TransferOptions {
            max_attempts: 1,
            ..TransferOptions::default()
        },
    )
    .await
    .unwrap();
    let hwm = vm_hwm_bytes();
    let written = std::fs::metadata(dir.path().join("out").join("obj.bin")).unwrap();
    assert_eq!(
        written.len(),
        len,
        "transfer did not publish the full object"
    );
    eprintln!(
        "transfer len={len} elapsed_ms={} hwm_before={before} hwm_after={hwm} cgroup={:?}",
        started.elapsed().as_millis(),
        cgroup_peak_bytes()
    );
    hwm
}

#[tokio::test]
#[ignore = "large storage resource"]
async fn transfer_rss_independent_of_object_size() {
    let small = transfer_sparse(SMALL).await;
    let large = transfer_sparse(LARGE).await;
    let delta = large.abs_diff(small);
    eprintln!(
        "resource small_hwm={small} large_hwm={large} delta={delta} cap={ABSOLUTE_CAP} allowed_delta={ALLOWED_DELTA}"
    );
    assert!(
        small < ABSOLUTE_CAP,
        "small transfer RSS {small} exceeds 1 GiB"
    );
    assert!(
        large < ABSOLUTE_CAP,
        "large transfer RSS {large} exceeds 1 GiB"
    );
    assert!(
        delta <= ALLOWED_DELTA,
        "RSS changed by {delta} between {SMALL} and {LARGE} bytes (allowed {ALLOWED_DELTA})"
    );
}
