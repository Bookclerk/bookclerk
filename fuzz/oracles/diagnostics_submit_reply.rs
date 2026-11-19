//! Shared oracle for the `diagnostics_submit_reply` fuzz target and the
//! `bookclerk-config` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The diagnostics collector answers an accepted report with a JSON body
//! whose `report_id` the host keeps in the install's report history
//! (`diagnostics/reports.json`) and shows the operator. The body reaches
//! [`parse_submit_reply`] as the bytes the server sent. The invariant,
//! checked against an independent reading through `serde_json::Value` and a
//! byte-by-byte UUID check: the parser accepts exactly the bodies of at most
//! [`MAX_SUBMIT_REPLY_BYTES`] bytes that are a JSON object whose
//! `report_id` is a string holding a lowercase hyphenated version-4 UUID,
//! and returns that string unchanged.

use bookclerk_config::{parse_submit_reply, SubmitReplyError, MAX_SUBMIT_REPLY_BYTES};

/// Whether `text` is a lowercase hyphenated version-4 UUID, read group by
/// group rather than by position.
fn oracle_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    if groups.len() != 5 || groups.iter().zip(lens).any(|(g, n)| g.len() != n) {
        return false;
    }
    let hex = |g: &str| g.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
    groups.iter().all(|g| hex(g))
        && groups[2].starts_with('4')
        && groups[3].starts_with(['8', '9', 'a', 'b'])
}

/// Runs the oracle on one input.
///
/// # Arguments
///
/// * `data` - A collector reply body.
///
/// # Panics
///
/// When the parser disagrees with the oracle.
pub fn check(data: &[u8]) {
    let got = parse_submit_reply(data);
    if data.len() > MAX_SUBMIT_REPLY_BYTES {
        assert_eq!(got, Err(SubmitReplyError::TooLong));
        return;
    }
    let field = serde_json::from_slice::<serde_json::Value>(data)
        .ok()
        .and_then(|v| {
            v.get("report_id")
                .and_then(|id| id.as_str().map(str::to_owned))
        });
    match field {
        None => assert_eq!(got, Err(SubmitReplyError::NotJson)),
        Some(id) if oracle_uuid(&id) => {
            assert_eq!(got.map(|r| r.as_str().to_owned()), Ok(id));
        }
        Some(_) => assert_eq!(got, Err(SubmitReplyError::BadReportId)),
    }
}
