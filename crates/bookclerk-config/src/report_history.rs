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

/// Longest a writer waits for another's lock on a small diagnostics file
/// (the report history, the install ID) before giving up.
pub(crate) const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Takes an exclusive lock on `file`, waiting at most `wait`.
///
/// # Arguments
///
/// * `file` - The open lock file.
/// * `wait` - Longest wait for another holder to let go.
///
/// # Errors
///
/// Returns [`std::io::ErrorKind::TimedOut`] when the lock is still held after
/// `wait`, or the error locking failed with.
pub(crate) fn lock_within(file: &std::fs::File, wait: std::time::Duration) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::Error(err)) => return Err(err),
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "another process holds the lock",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// Adds a report to the history, keeping the newest [`MAX_REPORT_HISTORY`].
///
/// The daemon and a CLI run can both upload from the same files directory,
/// so the read, append and replace happen under an exclusive lock on
/// `diagnostics/reports.json.lock`: a second writer waits for the first and
/// then appends to what the first wrote, rather than both appending to the
/// same old list and one replacement dropping the other's report.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `record` - The report the collector just accepted.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or the history cannot be
/// written.
pub fn record_report(files_dir: &Path, record: ReportRecord) -> std::io::Result<()> {
    let path = history_path(files_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))?;
    // Released when `lock` drops at the end of this function.
    lock_within(&lock, LOCK_WAIT)?;
    let mut rows = load_report_history(files_dir);
    rows.push(record);
    let excess = rows.len().saturating_sub(MAX_REPORT_HISTORY);
    rows.drain(..excess);
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
    fn a_held_lock_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let open = || {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let holder = open();
        lock_within(&holder, LOCK_WAIT).unwrap();
        let err = lock_within(&open(), std::time::Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        drop(holder);
        lock_within(&open(), std::time::Duration::from_millis(50)).unwrap();
    }

    #[test]
    fn concurrent_writers_keep_every_report() {
        let dir = tempfile::tempdir().unwrap();
        let ids: Vec<String> = (0..16)
            .map(|i| format!("0e9ced79-df3c-4dae-872b-2e5772b06e{i:02x}"))
            .collect();
        std::thread::scope(|s| {
            for (i, id) in ids.iter().enumerate() {
                let dir = dir.path();
                s.spawn(move || {
                    let record = ReportRecord {
                        report_id: ReportId::parse(id).unwrap(),
                        trigger: "cli_manual".into(),
                        version: "0.0.0-test".into(),
                        sent_at_unix_ms: u64::try_from(i).unwrap() + 1,
                    };
                    // Threads share a pid and so a staging name; only the
                    // lock keeps their writes apart, as it does processes.
                    record_report(dir, record).unwrap();
                });
            }
        });
        let mut got: Vec<String> = load_report_history(dir.path())
            .into_iter()
            .map(|r| r.report_id.as_str().to_string())
            .collect();
        got.sort();
        assert_eq!(got, ids);
    }

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
