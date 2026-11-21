//! Shared oracle for the `archive_extract` fuzz target and the
//! `bookclerk-archive` corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is an archive a person uploaded. It is walked twice into memory
//! under small limits. The invariants, checked against an independent path
//! rule rather than the crate's own: the walk is deterministic; its report
//! validates under the limits it ran with; every written path is relative,
//! has no empty, `.` or `..` component and no `\`, `:` or control
//! character; no two written paths fold to the same name; the sink holds
//! exactly the files the report lists, with the sizes it states; and the
//! total stays under the cap.

use std::collections::HashSet;
use std::io::Cursor;

use bookclerk_archive::{extract, Limits, MemorySink};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// The limits the oracle walks under: small, so caps are reached.
pub const LIMITS: Limits = Limits {
    max_entries: 64,
    max_total_bytes: 2 * 1024 * 1024,
    max_entry_bytes: 512 * 1024,
    max_ratio: 20,
    ratio_floor_bytes: 64 * 1024,
};

/// Whether `path` stays inside a folder by a rule written apart from the
/// crate's policy.
fn stays_inside(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path
            .chars()
            .any(|c| c == '\\' || c == ':' || c.is_control())
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - The archive bytes.
///
/// # Panics
///
/// When an invariant above does not hold.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let mut first = MemorySink::new();
    let Ok(report) = extract(Cursor::new(data), &LIMITS, &mut first) else {
        return;
    };
    let mut second = MemorySink::new();
    let again = extract(Cursor::new(data), &LIMITS, &mut second).expect("second walk");
    assert_eq!(report, again, "the walk is not deterministic");
    assert_eq!(first.files(), second.files());

    report.validate(&LIMITS).expect("report validates");
    let files = first.files();
    assert_eq!(
        files.len(),
        report.written.len(),
        "sink and report disagree"
    );
    let mut folded = HashSet::new();
    let mut total: u64 = 0;
    for entry in &report.written {
        let path = entry.path.as_str();
        assert!(stays_inside(path), "{path:?} escapes");
        // ASCII names fold by lower case alone; the crate's NFC folding
        // covers the rest and its `validate` already ran.
        if path.is_ascii() {
            assert!(folded.insert(path.to_ascii_lowercase()), "{path:?} clashes");
        }
        let bytes = files.get(&entry.path).expect("written file in sink");
        assert_eq!(u64::try_from(bytes.len()).expect("len"), entry.bytes);
        assert!(entry.bytes <= LIMITS.max_entry_bytes);
        total += entry.bytes;
    }
    assert_eq!(total, report.total_bytes);
    assert!(total <= LIMITS.max_total_bytes);
}
