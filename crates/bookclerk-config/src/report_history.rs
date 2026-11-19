//! The install's own history of diagnostics reports it sent.
//!
//! Each accepted upload gets a `report_id` from the collector (the UUID that
//! names the stored object and appears in the GitHub issue the dashboard
//! files for it). The host keeps the last [`MAX_REPORT_HISTORY`] of them in
//! `diagnostics/reports.json` under the files directory, so the install's
//! owner can search GitHub for their issue or give a maintainer the ID. The
//! CLI lists them with `bookclerk diagnostics reports` and the web app's
//! Operator settings show them (`GET /api/diagnostics/reports`). See
//! `docs/server/diagnostics.md#your-report-history`.
//!
//! The collector's reply is untrusted input: [`parse_submit_reply`] is the
//! sans-IO boundary the `diagnostics_submit_reply` fuzz target drives.
//!
//! # Audience
//!
//! Host binaries. Guests never depend on this module.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File, relative to the files directory, that holds the report history.
pub const REPORT_HISTORY_FILE: &str = "diagnostics/reports.json";

/// How many reports the history keeps; older ones are dropped first.
pub const MAX_REPORT_HISTORY: usize = 200;

/// Longest collector reply, in bytes, that [`parse_submit_reply`] reads.
/// The real reply is about 200 bytes.
pub const MAX_SUBMIT_REPLY_BYTES: usize = 4096;

/// A report ID the collector assigned: a version-4 UUID in lowercase
/// hyphenated form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ReportId(String);

impl ReportId {
    /// Parses a report ID.
    ///
    /// # Arguments
    ///
    /// * `text` - Candidate text. Unlike the install ID file, no
    ///   surrounding whitespace is allowed.
    ///
    /// # Returns
    ///
    /// The ID, or `None` unless the text is a lowercase hyphenated
    /// version-4 UUID.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        is_v4_uuid(text).then(|| Self(text.to_string()))
    }

    /// The ID as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ReportId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("not a report ID"))
    }
}

/// Whether `text` is exactly a lowercase hyphenated version-4 UUID
/// (RFC 9562 version nibble `4`, variant `10xx`).
///
/// # Arguments
///
/// * `text` - Candidate text.
#[must_use]
pub(crate) fn is_v4_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            14 => *b == b'4',
            19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        })
}

/// Why a collector reply was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitReplyError {
    /// The reply is longer than [`MAX_SUBMIT_REPLY_BYTES`].
    TooLong,
    /// The reply is not a JSON object with a string `report_id`.
    NotJson,
    /// `report_id` is present but not a version-4 UUID.
    BadReportId,
}

impl std::fmt::Display for SubmitReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLong => "collector reply too long",
            Self::NotJson => "collector reply has no report_id",
            Self::BadReportId => "collector reply's report_id is not a UUID",
        })
    }
}

impl std::error::Error for SubmitReplyError {}

/// Reads the report ID from the collector's `POST /submit` reply.
///
/// # Arguments
///
/// * `body` - The reply body as received.
///
/// # Returns
///
/// The report ID.
///
/// # Errors
///
/// Returns [`SubmitReplyError`] when the body is too long, is not a JSON
/// object with a string `report_id`, or that value is not a version-4 UUID.
pub fn parse_submit_reply(body: &[u8]) -> Result<ReportId, SubmitReplyError> {
    if body.len() > MAX_SUBMIT_REPLY_BYTES {
        return Err(SubmitReplyError::TooLong);
    }
    // A generic JSON value rather than a derived struct: other fields are
    // ignored and, as in any JSON reader, a repeated key keeps its last value.
    let reply: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| SubmitReplyError::NotJson)?;
    let report_id = reply
        .get("report_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(SubmitReplyError::NotJson)?;
    ReportId::parse(report_id).ok_or(SubmitReplyError::BadReportId)
}

/// One report this install sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRecord {
    /// The collector's ID for the report.
    pub report_id: ReportId,
    /// Why it was sent (`crash`, `error_burst`, `cli_error`, …).
    pub trigger: String,
    /// The Bookclerk version that sent it.
    pub version: String,
    /// When the collector accepted it, Unix milliseconds.
    pub sent_at_unix_ms: u64,
}

/// Path of the history file under `files_dir`.
fn history_path(files_dir: &Path) -> PathBuf {
    files_dir.join(REPORT_HISTORY_FILE)
}

/// Reads the report history, oldest first.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
///
/// # Returns
///
/// The recorded reports. A missing file is an empty history; an unreadable
/// or damaged one is too, because the history is a convenience and the
/// next report rewrites it.
#[must_use]
pub fn load_report_history(files_dir: &Path) -> Vec<ReportRecord> {
    let Ok(text) = std::fs::read(history_path(files_dir)) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<serde_json::Value>>(&text)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| serde_json::from_value(row).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Adds a report to the history, keeping the newest [`MAX_REPORT_HISTORY`].
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `record` - The report the collector just accepted.
///
/// # Errors
///
/// Returns an error when the history cannot be written.
pub fn record_report(files_dir: &Path, record: ReportRecord) -> std::io::Result<()> {
    let mut rows = load_report_history(files_dir);
    rows.push(record);
    let excess = rows.len().saturating_sub(MAX_REPORT_HISTORY);
    rows.drain(..excess);
    let path = history_path(files_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A per-process staging name: the daemon and a CLI run may both write.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let text = serde_json::to_vec_pretty(&rows).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, text)?;
    crate::replace_file(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid report ID for the tests.
    const ID: &str = "0e9ced79-df3c-4dae-872b-2e5772b06e69";

    #[test]
    fn a_collector_reply_yields_its_report_id() {
        let body = format!(
            r#"{{"ok":true,"report_id":"{ID}","key":"diagnostics/v/{ID}.json","received_at_unix_ms":1}}"#
        );
        assert_eq!(
            parse_submit_reply(body.as_bytes()).map(|id| id.as_str().to_string()),
            Ok(ID.to_string())
        );
    }

    #[test]
    fn other_replies_are_refused() {
        assert_eq!(parse_submit_reply(b"ok"), Err(SubmitReplyError::NotJson));
        assert_eq!(parse_submit_reply(b"{}"), Err(SubmitReplyError::NotJson));
        assert_eq!(
            parse_submit_reply(br#"{"report_id":7}"#),
            Err(SubmitReplyError::NotJson)
        );
        assert_eq!(
            parse_submit_reply(br#"{"report_id":"../etc/passwd"}"#),
            Err(SubmitReplyError::BadReportId)
        );
        assert_eq!(
            parse_submit_reply(ID.to_uppercase().as_bytes()),
            Err(SubmitReplyError::NotJson)
        );
        let long = vec![b' '; MAX_SUBMIT_REPLY_BYTES + 1];
        assert_eq!(parse_submit_reply(&long), Err(SubmitReplyError::TooLong));
    }

    #[test]
    fn report_ids_are_strict_v4_uuids() {
        assert!(ReportId::parse(ID).is_some());
        for bad in [
            "",
            " 0e9ced79-df3c-4dae-872b-2e5772b06e69",
            "0E9CED79-DF3C-4DAE-872B-2E5772B06E69",
            "0e9ced79-df3c-1dae-872b-2e5772b06e69",
            "0e9ced79-df3c-4dae-c72b-2e5772b06e69",
            "0e9ced79df3c4dae872b2e5772b06e69",
        ] {
            assert!(ReportId::parse(bad).is_none(), "{bad}");
        }
    }

    /// A record for `n`, with a distinct last hex digit pair.
    fn record(n: u64) -> ReportRecord {
        ReportRecord {
            report_id: ReportId::parse(&format!("0e9ced79-df3c-4dae-872b-2e5772b0{n:04x}"))
                .expect("valid id"),
            trigger: "crash".into(),
            version: "1.0.0".into(),
            sent_at_unix_ms: n,
        }
    }

    #[test]
    fn the_history_keeps_the_newest_reports() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_report_history(dir.path()).is_empty());
        for n in 0..(MAX_REPORT_HISTORY as u64 + 5) {
            record_report(dir.path(), record(n)).expect("record");
        }
        let rows = load_report_history(dir.path());
        assert_eq!(rows.len(), MAX_REPORT_HISTORY);
        assert_eq!(rows.first().map(|r| r.sent_at_unix_ms), Some(5));
        assert_eq!(
            rows.last().map(|r| r.sent_at_unix_ms),
            Some(MAX_REPORT_HISTORY as u64 + 4)
        );
    }

    #[test]
    fn a_damaged_history_reads_as_empty_and_damaged_rows_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(REPORT_HISTORY_FILE);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "not json").expect("write");
        assert!(load_report_history(dir.path()).is_empty());
        std::fs::write(
            &path,
            format!(
                r#"[{{"report_id":"bad"}},{{"report_id":"{ID}","trigger":"crash","version":"1","sent_at_unix_ms":3}}]"#
            ),
        )
        .expect("write");
        let rows = load_report_history(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].report_id.as_str(), ID);
    }
}
