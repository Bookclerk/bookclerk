//! Shared oracle for the `media_keep_codec` fuzz target and the
//! `bookclerk-media` corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one audio part from a store download. With
//! `output.keep_codec_in_m4b` on, the media worker copies its Opus, FLAC,
//! ALAC, E-AC-3 or AC-4 audio into an M4B without re-encoding it. The
//! oracle packages the part twice over (two chapters) with
//! [`package_keeping_codec`] and checks:
//!
//! - a part it refuses leaves no output file behind;
//! - an accepted M4B has two chapters, the first at 0 ms and the second no
//!   earlier;
//! - the independent ISO-BMFF parser in `bookclerk-mp4` reads the output,
//!   finds one of the kept sample entries, and every sample lies inside the
//!   file;
//! - the second half of the sample table repeats the first exactly (sizes
//!   and durations), since both halves are the same part;
//! - packaging is deterministic: the same bytes give the same file.

use std::path::Path;

use bookclerk_media::package_keeping_codec;
use bookclerk_mp4::boxutil::FourCC;
use bookclerk_mp4::{parse_mp4, SampleEntryKind};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

/// Sample entries the keep path writes.
const KEPT: [[u8; 4]; 5] = [*b"Opus", *b"fLaC", *b"alac", *b"ec-3", *b"ac-4"];

/// Packages `part` twice into `dir/<name>.m4b` and returns the file's
/// bytes, or `None` when the part is refused.
fn package(dir: &Path, part: &Path, name: &str) -> Option<Vec<u8>> {
    let out = dir.join(format!("{name}.m4b"));
    let parts = vec![part.to_path_buf(), part.to_path_buf()];
    let titles = vec!["A".to_string(), "B".to_string()];
    match package_keeping_codec(&parts, &out, &titles) {
        Ok(Some(chapters)) => {
            assert_eq!(chapters.len(), 2, "one chapter per part");
            assert_eq!(chapters[0], ("A".to_string(), 0), "first chapter");
            assert!(chapters[1].1 >= chapters[0].1, "chapters out of order");
            check_output(&out);
            Some(std::fs::read(&out).expect("read output"))
        }
        Ok(None) => {
            assert!(!out.exists(), "a refused part left an output file");
            None
        }
        // A part that classified but failed while copying is an error the
        // worker reports; it must not panic.
        Err(_) => None,
    }
}

/// Checks the written M4B with the independent parser.
fn check_output(out: &Path) {
    let mp4 = parse_mp4(out).expect("the kept M4B parses");
    let SampleEntryKind::Other(FourCC(kind)) = mp4.audio.sample_entry_kind else {
        panic!("kept M4B has entry {:?}", mp4.audio.sample_entry_kind);
    };
    assert!(KEPT.contains(&kind), "unexpected entry {kind:?}");
    let len = std::fs::metadata(out).expect("metadata").len();
    let samples = &mp4.audio.samples;
    for sample in samples {
        let end = sample.offset.checked_add(u64::from(sample.size));
        assert!(end.is_some_and(|end| end <= len), "sample past the file");
    }
    assert_eq!(samples.len() % 2, 0, "two copies of one part");
    let (first, second) = samples.split_at(samples.len() / 2);
    for (a, b) in first.iter().zip(second) {
        assert_eq!((a.size, a.duration), (b.size, b.duration), "halves differ");
    }
}

/// Runs every check on `data`; returns whether the part was kept.
///
/// # Panics
///
/// When an invariant above does not hold.
pub fn check(data: &[u8]) -> bool {
    if data.len() > MAX_INPUT_BYTES {
        return false;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let part = dir.path().join("part");
    std::fs::write(&part, data).expect("write part");
    let first = package(dir.path(), &part, "first");
    let second = package(dir.path(), &part, "second");
    assert_eq!(first, second, "packaging is not deterministic");
    first.is_some()
}
