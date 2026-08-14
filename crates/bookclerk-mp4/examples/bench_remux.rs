//! Time a copy remux over an audiobook-sized progressive file.
//!
//! The sample copy is the whole cost of a remux, and a sample is small — a few
//! hundred bytes — so this measures whether the copy is dominated by per-sample
//! syscalls. Run as:
//!
//! ```text
//! cargo run --release -p bookclerk-mp4 --example bench_remux -- [hours]
//! ```
//!
//! Reported throughput is over payload bytes, on a warm page cache; compare runs
//! on one machine rather than across machines.

use std::time::Instant;

use bookclerk_mp4::fixture::ProgressiveFixture;
use bookclerk_mp4::{remux_progressive, CopySamples, RemuxOptions};

/// AAC-LC at 44.1 kHz is 1024 samples a frame, so ~43 frames a second, and 372
/// bytes a frame is about 128 kbit/s — a typical retail audiobook.
const FRAMES_PER_SECOND: usize = 43;
/// Synthetic AAC frame size in bytes (~128 kbit/s at 43 frames/s).
const BYTES_PER_FRAME: usize = 372;

fn main() {
    let hours: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(3);
    let count = FRAMES_PER_SECOND * 3600 * hours;

    let dir = std::env::temp_dir().join("bench_remux");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let input = dir.join("in.m4a");
    let output = dir.join("out.m4b");

    let fixture = ProgressiveFixture::default().with_samples(
        (0..count)
            .map(|i| vec![(i % 251) as u8; BYTES_PER_FRAME])
            .collect(),
    );
    let mib = (count * BYTES_PER_FRAME) as f64 / (1024.0 * 1024.0);
    fixture.write(&input).expect("write fixture");
    println!("{hours}h book: {count} samples, {mib:.1} MiB of payload");

    for pass in 0..3 {
        let started = Instant::now();
        remux_progressive(&input, &output, &RemuxOptions::default(), &mut CopySamples)
            .expect("remux");
        let elapsed = started.elapsed();
        println!(
            "pass {pass}: {elapsed:?} ({:.0} MiB/s)",
            mib / elapsed.as_secs_f64()
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
