//! Reading a sent diagnostics report back from the collector, and deleting
//! the collector's copy, with the report's access code.
//!
//! The collector's `POST /lookup` takes `{report_id, access_code}` and
//! answers with the stored report, the GitHub issues tracking it and the
//! fix, if one is recorded; `POST /lookup/delete` removes the stored report
//! and keeps its one-line index entry and issue links. A wrong code and an
//! unknown ID get the same `404`. The daemon (`GET` and `DELETE
//! /api/diagnostics/reports/{id}`) and the CLI (`bookclerk diagnostics show`
//! and `delete`) call these on the operator's behalf. See
//! `docs/server/diagnostics.md#reading-a-report-back`.
//!
//! The collector's answers are untrusted input: [`parse_lookup_reply`] and
//! [`parse_delete_reply`] are the sans-IO boundary the
//! `diagnostics_lookup_reply` fuzz target drives.
//!
//! # Audience
//!
//! Host binaries. Guests never depend on this module.

use std::time::Duration;

use bookclerk_tls::ureq;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::report_history::{AccessCode, ReportId};
use crate::settings::DiagnosticsConfig;

/// Longest collector answer, in bytes, that [`parse_lookup_reply`] and
/// [`parse_delete_reply`] read. A stored report is at most 256 KiB, and the
/// answer caps every event, so a real answer is well under this.
pub const MAX_LOOKUP_REPLY_BYTES: usize = 1024 * 1024;

/// How long a lookup or delete request may take in all.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Severity of one logged event in a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EventLevel {
    /// `ERROR`.
    Error,
    /// `WARN`.
    Warn,
    /// `INFO`.
    Info,
    /// `DEBUG`.
    Debug,
    /// `TRACE`.
    Trace,
}

impl EventLevel {
    /// Reads the collector's spelling.
    ///
    /// # Arguments
    ///
    /// * `text` - `ERROR`, `WARN`, `INFO`, `DEBUG` or `TRACE`.
    ///
    /// # Returns
    ///
    /// The level, or `None` for anything else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "ERROR" => Self::Error,
            "WARN" => Self::Warn,
            "INFO" => Self::Info,
            "DEBUG" => Self::Debug,
            "TRACE" => Self::Trace,
            _ => return None,
        })
    }

    /// The level as the collector and the CLI spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
}

/// One logged event in a stored report, already redacted by the install
/// that sent it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LookupEvent {
    /// When it happened, Unix milliseconds, or `0` when the report had none.
    pub ts_unix_ms: u64,
    /// Its severity.
    pub level: EventLevel,
    /// Where it was logged (the tracing target).
    pub target: String,
    /// The message.
    pub message: String,
    /// `(name, value)` pairs recorded with it.
    pub fields: Vec<(String, String)>,
}

/// A stored report as the collector returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LookupReport {
    /// The Bookclerk version that sent it.
    pub version: String,
    /// Why it was sent.
    pub trigger: String,
    /// The sender's operating system.
    pub os: String,
    /// The sender's CPU architecture.
    pub arch: String,
    /// The sender's Linux distribution, or empty.
    pub distro: String,
    /// The events it carried, oldest first.
    pub events: Vec<LookupEvent>,
}

/// Whether a GitHub issue is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueState {
    /// Still open.
    Open,
    /// Closed.
    Closed,
}

/// A GitHub issue that tracks the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LookupIssue {
    /// The issue number.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// Whether it is open.
    pub state: IssueState,
    /// Its page; always an `https://` link.
    pub url: String,
}

/// The fix recorded for the report's tracking issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LookupFix {
    /// The pull request that fixed it, when known.
    pub pr_number: Option<u64>,
    /// That pull request's page (`https://`), when known.
    pub pr_url: Option<String>,
    /// The first release that contains the fix, when one has shipped.
    pub fixed_in: Option<String>,
}

/// The collector's answer to a lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LookupAnswer {
    /// The report's ID; always the one that was asked for.
    pub report_id: ReportId,
    /// When the collector received it, Unix milliseconds.
    pub received_at_unix_ms: u64,
    /// When its sender deleted the stored copy, Unix milliseconds, or
    /// `None` while it is stored.
    pub deleted_at_unix_ms: Option<u64>,
    /// The stored report, or `None` once deleted (or when the collector
    /// could not read it).
    pub report: Option<LookupReport>,
    /// Issues tracking it, the tracking issue first.
    pub issues: Vec<LookupIssue>,
    /// The tracking issue's fix, if one is recorded.
    pub fix: Option<LookupFix>,
}

/// Why a collector answer was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupReplyError {
    /// Longer than [`MAX_LOOKUP_REPLY_BYTES`].
    TooLong,
    /// Not a JSON object.
    NotJson,
    /// A field is missing, of the wrong type, or a link is not `https://`.
    BadShape,
    /// The answer names a report other than the one asked for.
    OtherReport,
}

impl std::fmt::Display for LookupReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLong => "collector answer too long",
            Self::NotJson => "collector answer is not a JSON object",
            Self::BadShape => "collector answer is malformed",
            Self::OtherReport => "collector answered for another report",
        })
    }
}

impl std::error::Error for LookupReplyError {}

/// Shorthand for a field reader's result.
type Field<T> = Result<T, LookupReplyError>;

/// A required string field.
fn string(obj: &Map<String, Value>, key: &str) -> Field<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(LookupReplyError::BadShape)
}

/// A required non-negative integer field.
fn uint(obj: &Map<String, Value>, key: &str) -> Field<u64> {
    obj.get(key)
        .and_then(Value::as_u64)
        .ok_or(LookupReplyError::BadShape)
}

/// A field that may be absent or `null`, read with `read` otherwise.
fn nullable<T>(
    obj: &Map<String, Value>,
    key: &str,
    read: impl FnOnce(&Value) -> Option<T>,
) -> Field<Option<T>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => read(v).map(Some).ok_or(LookupReplyError::BadShape),
    }
}

/// An `https://` link, or `None`.
fn https(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|s| s.starts_with("https://"))
        .map(str::to_owned)
}

/// A required array field, each element read with `read`.
fn array<T>(
    obj: &Map<String, Value>,
    key: &str,
    read: impl Fn(&Value) -> Field<T>,
) -> Field<Vec<T>> {
    obj.get(key)
        .and_then(Value::as_array)
        .ok_or(LookupReplyError::BadShape)?
        .iter()
        .map(read)
        .collect()
}

/// An object, or [`LookupReplyError::BadShape`].
fn object(v: &Value) -> Field<&Map<String, Value>> {
    v.as_object().ok_or(LookupReplyError::BadShape)
}

/// One event.
fn event(v: &Value) -> Field<LookupEvent> {
    let e = object(v)?;
    let level = e
        .get("level")
        .and_then(Value::as_str)
        .and_then(EventLevel::parse)
        .ok_or(LookupReplyError::BadShape)?;
    let fields = array(e, "fields", |f| match f.as_array().map(Vec::as_slice) {
        Some([Value::String(k), Value::String(v)]) => Ok((k.clone(), v.clone())),
        _ => Err(LookupReplyError::BadShape),
    })?;
    Ok(LookupEvent {
        ts_unix_ms: uint(e, "ts_unix_ms")?,
        level,
        target: string(e, "target")?,
        message: string(e, "message")?,
        fields,
    })
}

/// The stored report.
fn report(v: &Value) -> Field<LookupReport> {
    let r = object(v)?;
    Ok(LookupReport {
        version: string(r, "version")?,
        trigger: string(r, "trigger")?,
        os: string(r, "os")?,
        arch: string(r, "arch")?,
        distro: string(r, "distro")?,
        events: array(r, "events", event)?,
    })
}

/// One issue.
fn issue(v: &Value) -> Field<LookupIssue> {
    let i = object(v)?;
    let state = match i.get("state").and_then(Value::as_str) {
        Some("open") => IssueState::Open,
        Some("closed") => IssueState::Closed,
        _ => return Err(LookupReplyError::BadShape),
    };
    Ok(LookupIssue {
        number: uint(i, "number")?,
        title: string(i, "title")?,
        state,
        url: i
            .get("url")
            .and_then(https)
            .ok_or(LookupReplyError::BadShape)?,
    })
}

/// The fix.
fn fix(v: &Value) -> Field<LookupFix> {
    let f = object(v)?;
    Ok(LookupFix {
        pr_number: nullable(f, "pr_number", Value::as_u64)?,
        pr_url: nullable(f, "pr_url", https)?,
        fixed_in: nullable(f, "fixed_in", |v| v.as_str().map(str::to_owned))?,
    })
}

/// Reads an answer body into its top-level object, checking its size and
/// that it names `expected`.
fn top(body: &[u8], expected: &ReportId) -> Field<Map<String, Value>> {
    if body.len() > MAX_LOOKUP_REPLY_BYTES {
        return Err(LookupReplyError::TooLong);
    }
    let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(body) else {
        return Err(LookupReplyError::NotJson);
    };
    if string(&obj, "report_id")? != expected.as_str() {
        return Err(LookupReplyError::OtherReport);
    }
    Ok(obj)
}

/// Reads the collector's answer to `POST /lookup`.
///
/// Every field the collector sends is required with its type, except that
/// `deleted_at_unix_ms`, `report`, `fix` and the fix's fields may be absent
/// or `null`; other keys are ignored and, as in any JSON reader, a repeated
/// key keeps its last value.
///
/// # Arguments
///
/// * `body` - The answer body as received.
/// * `expected` - The report that was asked for.
///
/// # Returns
///
/// The answer.
///
/// # Errors
///
/// Returns [`LookupReplyError`] when the body is too long, not a JSON
/// object, malformed (including an issue or pull request link that is not
/// `https://`), or names another report.
pub fn parse_lookup_reply(
    body: &[u8],
    expected: &ReportId,
) -> Result<LookupAnswer, LookupReplyError> {
    let obj = top(body, expected)?;
    Ok(LookupAnswer {
        report_id: expected.clone(),
        received_at_unix_ms: uint(&obj, "received_at_unix_ms")?,
        deleted_at_unix_ms: nullable(&obj, "deleted_at_unix_ms", Value::as_u64)?,
        report: match obj.get("report") {
            None | Some(Value::Null) => None,
            Some(v) => Some(report(v)?),
        },
        issues: array(&obj, "issues", issue)?,
        fix: match obj.get("fix") {
            None | Some(Value::Null) => None,
            Some(v) => Some(fix(v)?),
        },
    })
}

/// Reads the collector's answer to `POST /lookup/delete`.
///
/// # Arguments
///
/// * `body` - The answer body as received.
/// * `expected` - The report whose deletion was asked for.
///
/// # Returns
///
/// When the stored copy was deleted, Unix milliseconds (the first deletion's
/// time when it was already gone).
///
/// # Errors
///
/// Returns [`LookupReplyError`] when the body is too long, not a JSON
/// object, has no integer `deleted_at_unix_ms`, or names another report.
pub fn parse_delete_reply(body: &[u8], expected: &ReportId) -> Result<u64, LookupReplyError> {
    uint(&top(body, expected)?, "deleted_at_unix_ms")
}

/// Why a lookup or delete did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// No collector URL is configured.
    NotConfigured,
    /// The collector knows no report with this ID and code (`404`); the two
    /// cases are not told apart.
    NotFound,
    /// Too many attempts for this report lately (`429`); try again in a
    /// minute.
    RateLimited,
    /// The collector cannot serve lookups (`503`: no dashboard database).
    Unavailable,
    /// The collector answered with another error status.
    Status(u16),
    /// The request did not complete.
    Transport(String),
    /// The collector's answer was refused.
    BadReply(LookupReplyError),
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("no diagnostics collector is configured"),
            Self::NotFound => f.write_str("the collector has no report with this ID and code"),
            Self::RateLimited => {
                f.write_str("too many attempts for this report; try again in a minute")
            }
            Self::Unavailable => f.write_str("this collector does not offer report lookups"),
            Self::Status(code) => write!(f, "the collector answered with status {code}"),
            Self::Transport(err) => write!(f, "the collector could not be reached: {err}"),
            Self::BadReply(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LookupError {}

/// POSTs the report ID and code to a lookup route.
///
/// # Arguments
///
/// * `url` - The route.
/// * `report_id` - The report.
/// * `code` - Its access code.
///
/// # Returns
///
/// The answer body of a `200`, read up to [`MAX_LOOKUP_REPLY_BYTES`] plus
/// one byte so an overlong answer is still refused by the parser.
///
/// # Errors
///
/// [`LookupError`] for a missing URL, a failed request or an error status.
fn post_lookup(url: &str, report_id: &ReportId, code: &AccessCode) -> Result<Vec<u8>, LookupError> {
    if url.is_empty() {
        return Err(LookupError::NotConfigured);
    }
    let body = serde_json::json!({
        "report_id": report_id.as_str(),
        "access_code": code.as_str(),
    })
    .to_string();
    let mut response = ureq::post(url)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            &format!("bookclerk-diagnostics/{}", env!("CARGO_PKG_VERSION")),
        )
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(LOOKUP_TIMEOUT))
        .build()
        .send(body.as_str())
        .map_err(|e| LookupError::Transport(e.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        404 => return Err(LookupError::NotFound),
        429 => return Err(LookupError::RateLimited),
        503 => return Err(LookupError::Unavailable),
        other => return Err(LookupError::Status(other)),
    }
    let limit = u64::try_from(MAX_LOOKUP_REPLY_BYTES + 1).unwrap_or(u64::MAX);
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|e| LookupError::Transport(e.to_string()))
}

/// Reads a sent report back from the collector.
///
/// Blocking: call it off an async runtime's worker threads.
///
/// # Arguments
///
/// * `config` - Diagnostics settings (for the collector URL).
/// * `report_id` - The report.
/// * `code` - Its access code.
///
/// # Returns
///
/// The collector's answer.
///
/// # Errors
///
/// [`LookupError`] when no collector is configured, the request fails, the
/// collector refuses it, or its answer is malformed.
pub fn fetch_report(
    config: &DiagnosticsConfig,
    report_id: &ReportId,
    code: &AccessCode,
) -> Result<LookupAnswer, LookupError> {
    let body = post_lookup(&config.effective_lookup_url(), report_id, code)?;
    parse_lookup_reply(&body, report_id).map_err(LookupError::BadReply)
}

/// Deletes the collector's stored copy of a sent report. Its one-line index
/// entry and issue links stay on the collector.
///
/// Blocking: call it off an async runtime's worker threads.
///
/// # Arguments
///
/// * `config` - Diagnostics settings (for the collector URL).
/// * `report_id` - The report.
/// * `code` - Its access code.
///
/// # Returns
///
/// When the copy was deleted, Unix milliseconds.
///
/// # Errors
///
/// [`LookupError`] when no collector is configured, the request fails, the
/// collector refuses it, or its answer is malformed.
pub fn delete_report(
    config: &DiagnosticsConfig,
    report_id: &ReportId,
    code: &AccessCode,
) -> Result<u64, LookupError> {
    let url = config.effective_lookup_url();
    let url = if url.is_empty() {
        url
    } else {
        format!("{url}/delete")
    };
    let body = post_lookup(&url, report_id, code)?;
    parse_delete_reply(&body, report_id).map_err(LookupError::BadReply)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report the tests ask for.
    const ID: &str = "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f";

    /// The parsed test ID.
    fn id() -> ReportId {
        ReportId::parse(ID).expect("id")
    }

    #[test]
    fn a_full_answer_reads() {
        let body = format!(
            r#"{{"report_id":"{ID}","received_at_unix_ms":5,"deleted_at_unix_ms":null,
            "report":{{"version":"0.9.0","trigger":"crash","os":"linux","arch":"x86_64","distro":"",
              "events":[{{"ts_unix_ms":1,"level":"WARN","target":"t","message":"m","fields":[["k","v"]]}}]}},
            "issues":[{{"number":7,"title":"Crash","state":"open","url":"https://github.com/o/r/issues/7"}}],
            "fix":{{"pr_number":9,"pr_url":"https://github.com/o/r/pull/9","fixed_in":null}}}}"#
        );
        let answer = parse_lookup_reply(body.as_bytes(), &id()).expect("answer");
        let report = answer.report.expect("report");
        assert_eq!(report.events[0].level, EventLevel::Warn);
        assert_eq!(
            report.events[0].fields,
            vec![("k".to_string(), "v".to_string())]
        );
        assert_eq!(answer.issues[0].state, IssueState::Open);
        assert_eq!(answer.fix.and_then(|f| f.pr_number), Some(9));
    }

    #[test]
    fn the_seed_answers_read() {
        let full = include_bytes!("../../../fuzz/corpus/diagnostics_lookup_reply/full.json");
        let answer = parse_lookup_reply(full, &id()).expect("full seed");
        assert_eq!(answer.issues.len(), 2);
        assert_eq!(answer.report.map(|r| r.events.len()), Some(2));
        let deleted = include_bytes!("../../../fuzz/corpus/diagnostics_lookup_reply/deleted.json");
        assert!(parse_lookup_reply(deleted, &id())
            .expect("deleted seed")
            .report
            .is_none());
    }

    #[test]
    fn bad_answers_are_refused() {
        let other = r#"{"report_id":"0e9ced79-df3c-4dae-872b-2e5772b06e69","received_at_unix_ms":1,"issues":[]}"#;
        assert_eq!(
            parse_lookup_reply(other.as_bytes(), &id()),
            Err(LookupReplyError::OtherReport)
        );
        let link = format!(
            r#"{{"report_id":"{ID}","received_at_unix_ms":1,"issues":[{{"number":1,"title":"x","state":"open","url":"javascript:alert(1)"}}]}}"#
        );
        assert_eq!(
            parse_lookup_reply(link.as_bytes(), &id()),
            Err(LookupReplyError::BadShape)
        );
        assert_eq!(
            parse_lookup_reply(b"[]", &id()),
            Err(LookupReplyError::NotJson)
        );
        let long = vec![b' '; MAX_LOOKUP_REPLY_BYTES + 1];
        assert_eq!(
            parse_delete_reply(&long, &id()),
            Err(LookupReplyError::TooLong)
        );
        let deleted = format!(r#"{{"report_id":"{ID}","deleted_at_unix_ms":42}}"#);
        assert_eq!(parse_delete_reply(deleted.as_bytes(), &id()), Ok(42));
    }

    #[test]
    fn no_collector_means_not_configured() {
        let config = DiagnosticsConfig {
            collector_url: String::new(),
            ..DiagnosticsConfig::default()
        };
        if !config.effective_lookup_url().is_empty() {
            // A build with a baked-in collector URL always has one.
            return;
        }
        let code = AccessCode::parse("K7QM3XRDPW9TH2CF").expect("code");
        assert_eq!(
            fetch_report(&config, &id(), &code),
            Err(LookupError::NotConfigured)
        );
        assert_eq!(
            delete_report(&config, &id(), &code),
            Err(LookupError::NotConfigured)
        );
    }

    /// Answers one HTTP request on a loopback port with `status` and `body`,
    /// and hands back the request it read.
    fn serve_once(status: &str, body: String) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let _ = stream.write_all(reply.as_bytes());
            String::from_utf8_lossy(&raw).to_string()
        });
        (url, server)
    }

    /// Diagnostics settings pointing at `url`.
    fn config(url: &str) -> DiagnosticsConfig {
        DiagnosticsConfig {
            collector_url: url.to_string(),
            ..DiagnosticsConfig::default()
        }
    }

    #[test]
    fn lookups_and_deletes_go_to_the_collector() {
        let code = AccessCode::parse("K7QM3XRDPW9TH2CF").expect("code");
        let answer = format!(r#"{{"report_id":"{ID}","received_at_unix_ms":5,"issues":[]}}"#);
        let (url, server) = serve_once("200 OK", answer);
        let got = fetch_report(&config(&url), &id(), &code).expect("answer");
        assert_eq!(got.received_at_unix_ms, 5);
        assert!(got.report.is_none());
        let request = server.join().expect("server");
        assert!(request.starts_with("POST /lookup HTTP/1.1"), "{request}");
        assert!(request.contains(&format!(
            r#"{{"access_code":"K7QM3XRDPW9TH2CF","report_id":"{ID}"}}"#
        )));

        let (url, server) = serve_once(
            "200 OK",
            format!(r#"{{"report_id":"{ID}","deleted_at_unix_ms":8}}"#),
        );
        assert_eq!(delete_report(&config(&url), &id(), &code), Ok(8));
        assert!(server
            .join()
            .expect("server")
            .starts_with("POST /lookup/delete HTTP/1.1"));

        for (status, want) in [
            ("404 Not Found", LookupError::NotFound),
            ("429 Too Many Requests", LookupError::RateLimited),
            ("503 Service Unavailable", LookupError::Unavailable),
            ("502 Bad Gateway", LookupError::Status(502)),
        ] {
            let (url, server) = serve_once(status, "{}".into());
            assert_eq!(fetch_report(&config(&url), &id(), &code), Err(want));
            server.join().expect("server");
        }
        let (url, server) = serve_once("200 OK", "{}".into());
        assert_eq!(
            delete_report(&config(&url), &id(), &code),
            Err(LookupError::BadReply(LookupReplyError::BadShape))
        );
        server.join().expect("server");
        for err in [
            LookupError::NotConfigured,
            LookupError::NotFound,
            LookupError::RateLimited,
            LookupError::Unavailable,
            LookupError::Status(500),
            LookupError::Transport("x".into()),
            LookupError::BadReply(LookupReplyError::OtherReport),
        ] {
            assert!(!err.to_string().is_empty());
        }
    }
}
