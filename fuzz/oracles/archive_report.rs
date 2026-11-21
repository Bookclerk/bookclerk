//! Shared oracle for the `archive_report` fuzz target and the
//! `bookclerk-archive` corpus replay test (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The media worker that expands an archive parses untrusted bytes, so the
//! host treats its JSON report as hostile. The input is such a report. The
//! invariants: a report the host accepts (it deserialises and validates
//! under the oracle's limits) names only paths that stay inside the output
//! folder by an independent rule, in canonical form, none folding to
//! another; its sizes sum to its total and respect every cap; and it
//! re-serialises to a report that reads back the same.

use std::collections::HashSet;

use bookclerk_archive::{EntryPath, ExtractReport, Limits, MAX_REFUSED_NAME_CHARS};

/// Largest input worth checking.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// The limits reports are validated under.
pub const LIMITS: Limits = Limits {
    max_entries: 128,
    max_total_bytes: 1 << 40,
    max_entry_bytes: 1 << 34,
    max_ratio: 100,
    ratio_floor_bytes: 1 << 20,
};

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A JSON report, as the worker would print it.
///
/// # Panics
///
/// When an invariant above does not hold.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(report) = serde_json::from_slice::<ExtractReport>(data) else {
        return;
    };
    if report.validate(&LIMITS).is_err() {
        return;
    }
    let mut total: u64 = 0;
    let mut folded = HashSet::new();
    for entry in &report.written {
        let path = entry.path.as_str();
        assert!(!path.is_empty() && !path.starts_with('/'));
        assert!(!path
            .chars()
            .any(|c| c == '\\' || c == ':' || c.is_control()));
        assert!(path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."));
        assert_eq!(EntryPath::admit(path).expect("admits").as_str(), path);
        assert!(folded.insert(entry.path.fold_key()), "{path:?} clashes");
        assert!(entry.bytes <= LIMITS.max_entry_bytes);
        total = total.checked_add(entry.bytes).expect("sum");
    }
    assert_eq!(total, report.total_bytes);
    assert!(total <= LIMITS.max_total_bytes);
    for entry in &report.refused {
        assert!(entry.name.chars().count() <= MAX_REFUSED_NAME_CHARS);
    }
    let json = serde_json::to_vec(&report).expect("serialise");
    let back: ExtractReport = serde_json::from_slice(&json).expect("reads back");
    assert_eq!(back, report);
}
