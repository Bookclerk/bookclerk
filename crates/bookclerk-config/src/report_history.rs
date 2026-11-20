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
//! The collector also answers with an access code for the report
//! ([`AccessCode`]); the history keeps it beside the ID, and it is the
//! credential the install shows to read the report back or delete it
//! (`crate::report_lookup`). The file is written with owner-only
//! permissions, and every code is registered with log redaction.
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

/// Crockford's base32 alphabet, which access codes are written in: digits
/// and letters without I, L, O and U.
pub const ACCESS_CODE_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Characters in an access code, without the group separators.
pub const ACCESS_CODE_LENGTH: usize = 16;

/// A report's access code: the credential that reads the report back from
/// the collector or deletes it. Canonical form: exactly
/// [`ACCESS_CODE_LENGTH`] characters of [`ACCESS_CODE_ALPHABET`], no
/// separators. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct AccessCode(String);

impl AccessCode {
    /// Parses a canonical access code, as the collector sends it.
    ///
    /// # Arguments
    ///
    /// * `text` - Candidate text.
    ///
    /// # Returns
    ///
    /// The code, or `None` unless the text is exactly
    /// [`ACCESS_CODE_LENGTH`] alphabet characters.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == ACCESS_CODE_LENGTH
            && text
                .bytes()
                .all(|b| ACCESS_CODE_ALPHABET.as_bytes().contains(&b)))
        .then(|| Self(text.to_string()))
    }

    /// The canonical code.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The code as people read it, in hyphenated groups of four
    /// (`K7QM-3XRD-PW9T-H2CF`).
    #[must_use]
    pub fn display(&self) -> String {
        self.0
            .as_bytes()
            .chunks(4)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Registers both spellings of the code with log redaction, so it never
    /// shows in logs or in a later diagnostics report.
    fn register_redaction(&self) {
        crate::redact::register_secret(&self.0);
        crate::redact::register_secret(self.display());
    }
}

impl std::fmt::Debug for AccessCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessCode(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for AccessCode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| serde::de::Error::custom("not an access code"))
    }
}

/// What the collector's `POST /submit` reply names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitReply {
    /// The report's ID.
    pub report_id: ReportId,
    /// Its access code, or `None` when the collector issued none (no
    /// dashboard database, its daily cap reached, or an older collector) or
    /// sent one that is not a canonical code.
    pub access_code: Option<AccessCode>,
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

/// Reads the report ID and access code from the collector's `POST /submit`
/// reply.
///
/// # Arguments
///
/// * `body` - The reply body as received.
///
/// # Returns
///
/// The report ID, and the access code when `access_code` is a string
/// holding a canonical code (anything else there is ignored, since the
/// report is stored either way).
///
/// # Errors
///
/// Returns [`SubmitReplyError`] when the body is too long, is not a JSON
/// object with a string `report_id`, or that value is not a version-4 UUID.
pub fn parse_submit_reply(body: &[u8]) -> Result<SubmitReply, SubmitReplyError> {
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
    let report_id = ReportId::parse(report_id).ok_or(SubmitReplyError::BadReportId)?;
    let access_code = reply
        .get("access_code")
        .and_then(serde_json::Value::as_str)
        .and_then(AccessCode::parse);
    Ok(SubmitReply {
        report_id,
        access_code,
    })
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
    /// The code that reads it back or deletes it, when the collector
    /// issued one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_code: Option<AccessCode>,
    /// When this install deleted the collector's stored copy, Unix
    /// milliseconds, or `None` while it is stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_unix_ms: Option<u64>,
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
/// next report rewrites it. Every access code read is registered with log
/// redaction.
#[must_use]
pub fn load_report_history(files_dir: &Path) -> Vec<ReportRecord> {
    let Ok(text) = std::fs::read(history_path(files_dir)) else {
        return Vec::new();
    };
    let rows: Vec<ReportRecord> = serde_json::from_slice::<Vec<serde_json::Value>>(&text)
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| serde_json::from_value(row).ok())
                .collect()
        })
        .unwrap_or_default();
    for code in rows.iter().filter_map(|r| r.access_code.as_ref()) {
        code.register_redaction();
    }
    rows
}

/// The report with this ID in the history, if it is there.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `report_id` - The report.
///
/// # Returns
///
/// The newest record with that ID, or `None`.
#[must_use]
pub fn find_report(files_dir: &Path, report_id: &ReportId) -> Option<ReportRecord> {
    load_report_history(files_dir)
        .into_iter()
        .rev()
        .find(|r| &r.report_id == report_id)
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

/// Rewrites the history under its lock.
///
/// The daemon and a CLI run can both upload from the same files directory,
/// so the read, change and replace happen under an exclusive lock on
/// `diagnostics/reports.json.lock`: a second writer waits for the first and
/// then changes what the first wrote, rather than both changing the same
/// old list and one replacement dropping the other's change. The file is
/// written with owner-only permissions (Unix), since it holds access codes.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `change` - Edits the rows, oldest first; returns whether to write.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or the history cannot be
/// written.
fn update_history(
    files_dir: &Path,
    change: impl FnOnce(&mut Vec<ReportRecord>) -> bool,
) -> std::io::Result<()> {
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
    if !change(&mut rows) {
        return Ok(());
    }
    // A per-process staging name: the daemon and a CLI run may both write.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let text = serde_json::to_vec_pretty(&rows).map_err(std::io::Error::other)?;
    write_private(&tmp, &text)?;
    crate::replace_file(&tmp, &path)
}

/// Writes `bytes` to a new file at `path` that only its owner can read
/// (mode `0600` on Unix; elsewhere the files directory's own access applies).
///
/// # Errors
///
/// Returns the error creating or writing the file failed with.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // A staging file left by an older build may predate the mode above.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

/// Adds a report to the history, keeping the newest [`MAX_REPORT_HISTORY`].
///
/// Its access code, if any, is registered with log redaction first.
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
    if let Some(code) = &record.access_code {
        code.register_redaction();
    }
    update_history(files_dir, |rows| {
        rows.push(record);
        let excess = rows.len().saturating_sub(MAX_REPORT_HISTORY);
        rows.drain(..excess);
        true
    })
}

/// Notes in the history that the collector's copy of a report was deleted.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `report_id` - The report.
/// * `deleted_at_unix_ms` - When the collector deleted it.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or the history cannot be
/// written. A report not in the history is not an error.
pub fn mark_report_deleted(
    files_dir: &Path,
    report_id: &ReportId,
    deleted_at_unix_ms: u64,
) -> std::io::Result<()> {
    update_history(files_dir, |rows| {
        let mut changed = false;
        for row in rows.iter_mut().filter(|r| &r.report_id == report_id) {
            if row.deleted_at_unix_ms.is_none() {
                row.deleted_at_unix_ms = Some(deleted_at_unix_ms);
                changed = true;
            }
        }
        changed
    })
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
                        access_code: None,
                        deleted_at_unix_ms: None,
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
            parse_submit_reply(body.as_bytes())
                .map(|r| (r.report_id.as_str().to_string(), r.access_code)),
            Ok((ID.to_string(), None))
        );
        let body = format!(r#"{{"report_id":"{ID}","access_code":"K7QM3XRDPW9TH2CF"}}"#);
        let reply = parse_submit_reply(body.as_bytes()).expect("reply");
        let code = reply.access_code.expect("code");
        assert_eq!(code.as_str(), "K7QM3XRDPW9TH2CF");
        assert_eq!(code.display(), "K7QM-3XRD-PW9T-H2CF");
        assert_eq!(format!("{code:?}"), "AccessCode(<redacted>)");
        for bad in [
            r#""K7QM-3XRD-PW9T-H2CF""#,
            r#""k7qm3xrdpw9th2cf""#,
            r#""U7QM3XRDPW9TH2CF""#,
            "7",
        ] {
            let body = format!(r#"{{"report_id":"{ID}","access_code":{bad}}}"#);
            assert_eq!(
                parse_submit_reply(body.as_bytes()).map(|r| r.access_code),
                Ok(None),
                "{bad}"
            );
        }
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
            access_code: None,
            deleted_at_unix_ms: None,
        }
    }

    #[test]
    fn codes_and_deletions_round_trip_in_a_private_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut first = record(1);
        first.access_code = AccessCode::parse("K7QM3XRDPW9TH2CF");
        record_report(dir.path(), first.clone()).expect("record");
        record_report(dir.path(), record(2)).expect("record");
        assert!(crate::redact::redact_str("code K7QM-3XRD-PW9T-H2CF").contains(crate::REDACTED));
        let id = first.report_id.clone();
        assert_eq!(find_report(dir.path(), &id), Some(first.clone()));
        mark_report_deleted(dir.path(), &id, 99).expect("mark");
        mark_report_deleted(dir.path(), &id, 100).expect("mark again");
        let found = find_report(dir.path(), &id).expect("found");
        assert_eq!(found.deleted_at_unix_ms, Some(99));
        assert_eq!(found.access_code, first.access_code);
        let text = std::fs::read_to_string(dir.path().join(REPORT_HISTORY_FILE)).expect("read");
        // A row without a code or deletion writes neither key.
        assert_eq!(text.matches("access_code").count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(REPORT_HISTORY_FILE))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
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
