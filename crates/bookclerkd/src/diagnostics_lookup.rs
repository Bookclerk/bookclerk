//! Reading a sent diagnostics report back from the collector, and deleting
//! the collector's copy, on the operator's behalf.
//!
//! `GET /api/diagnostics/reports/{id}` and `DELETE
//! /api/diagnostics/reports/{id}` take the report's access code from the
//! install's report history (`diagnostics/reports.json`) and call the
//! collector's `/lookup` routes with it (`bookclerk_config::fetch_report`,
//! `delete_report`), so the web app's CSP and the collector's closed CORS
//! stay as they are. They sit with the report list in the operator-only
//! group: Owners and Administrators, the people who change diagnostics
//! settings. See `docs/server/diagnostics.md#reading-a-report-back`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bookclerk_config::{
    AccessCode, EventLevel, IssueState, LookupAnswer, LookupError, ReportId, ReportRecord,
};
use serde::Serialize;

use crate::http_error::ErrorJson;
use crate::AppState;

/// Severity of one event in a report.
#[derive(Debug, Clone, Copy, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum ReportEventLevel {
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

impl From<EventLevel> for ReportEventLevel {
    fn from(level: EventLevel) -> Self {
        match level {
            EventLevel::Error => Self::Error,
            EventLevel::Warn => Self::Warn,
            EventLevel::Info => Self::Info,
            EventLevel::Debug => Self::Debug,
            EventLevel::Trace => Self::Trace,
        }
    }
}

/// One field recorded with an event.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ReportEventField {
    /// The field's name.
    #[schema(example = "attempt")]
    name: String,
    /// Its value, redacted before the report was sent.
    #[schema(example = "2")]
    value: String,
}

/// One logged event in a stored report.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ReportEventView {
    /// When it happened, Unix milliseconds, or `0` when the report had none.
    #[schema(example = 1_763_519_999_000_u64)]
    ts_unix_ms: u64,
    /// Its severity.
    #[schema(example = "ERROR")]
    level: ReportEventLevel,
    /// Where it was logged (the tracing target).
    #[schema(example = "bookclerk_library::scan")]
    target: String,
    /// The message, redacted before the report was sent.
    #[schema(example = "scan failed")]
    message: String,
    /// Fields recorded with it.
    fields: Vec<ReportEventField>,
}

/// The stored report as the collector keeps it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct StoredReportView {
    /// The Bookclerk version that sent it.
    #[schema(example = "0.9.0")]
    version: String,
    /// Why it was sent.
    #[schema(example = "error_burst")]
    trigger: String,
    /// The sender's operating system.
    #[schema(example = "linux")]
    os: String,
    /// The sender's CPU architecture.
    #[schema(example = "x86_64")]
    arch: String,
    /// The sender's Linux distribution, or empty.
    #[schema(example = "debian")]
    distro: String,
    /// Its events, oldest first.
    events: Vec<ReportEventView>,
}

/// Whether an issue is open.
#[derive(Debug, Clone, Copy, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReportIssueState {
    /// Still open.
    Open,
    /// Closed.
    Closed,
}

/// A GitHub issue tracking the report.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ReportIssueView {
    /// The issue number.
    #[schema(example = 1204)]
    number: u64,
    /// Its title.
    #[schema(example = "Scan fails with timeout")]
    title: String,
    /// Whether it is open.
    #[schema(example = "open")]
    state: ReportIssueState,
    /// Its page (always `https://`).
    #[schema(example = "https://github.com/bookclerk/bookclerk/issues/1204")]
    url: String,
}

/// The fix recorded for the report's tracking issue.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct ReportFixView {
    /// The pull request that fixed it, when known.
    #[schema(example = 1210)]
    pr_number: Option<u64>,
    /// That pull request's page (`https://`), when known.
    #[schema(example = "https://github.com/bookclerk/bookclerk/pull/1210")]
    pr_url: Option<String>,
    /// The first release with the fix, when one has shipped.
    #[schema(example = "0.9.1")]
    fixed_in: Option<String>,
}

/// Body of `GET /api/diagnostics/reports/{id}`: the report as the collector
/// holds it, and what became of it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DiagnosticsReportLookup {
    /// The report's ID.
    #[schema(example = "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f")]
    report_id: String,
    /// When the collector received it, Unix milliseconds.
    #[schema(example = 1_763_520_000_000_u64)]
    received_at_unix_ms: u64,
    /// When its stored copy was deleted, Unix milliseconds, or null.
    #[schema(example = json!(null))]
    deleted_at_unix_ms: Option<u64>,
    /// The stored report, or null once deleted.
    report: Option<StoredReportView>,
    /// Issues tracking it, the tracking issue first.
    issues: Vec<ReportIssueView>,
    /// The tracking issue's fix, or null.
    fix: Option<ReportFixView>,
}

impl From<LookupAnswer> for DiagnosticsReportLookup {
    fn from(a: LookupAnswer) -> Self {
        Self {
            report_id: a.report_id.as_str().to_string(),
            received_at_unix_ms: a.received_at_unix_ms,
            deleted_at_unix_ms: a.deleted_at_unix_ms,
            report: a.report.map(|r| StoredReportView {
                version: r.version,
                trigger: r.trigger,
                os: r.os,
                arch: r.arch,
                distro: r.distro,
                events: r
                    .events
                    .into_iter()
                    .map(|e| ReportEventView {
                        ts_unix_ms: e.ts_unix_ms,
                        level: e.level.into(),
                        target: e.target,
                        message: e.message,
                        fields: e
                            .fields
                            .into_iter()
                            .map(|(name, value)| ReportEventField { name, value })
                            .collect(),
                    })
                    .collect(),
            }),
            issues: a
                .issues
                .into_iter()
                .map(|i| ReportIssueView {
                    number: i.number,
                    title: i.title,
                    state: match i.state {
                        IssueState::Open => ReportIssueState::Open,
                        IssueState::Closed => ReportIssueState::Closed,
                    },
                    url: i.url,
                })
                .collect(),
            fix: a.fix.map(|f| ReportFixView {
                pr_number: f.pr_number,
                pr_url: f.pr_url,
                fixed_in: f.fixed_in,
            }),
        }
    }
}

/// Body of `DELETE /api/diagnostics/reports/{id}`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct DiagnosticsReportDeleted {
    /// The report's ID.
    #[schema(example = "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f")]
    report_id: String,
    /// When its stored copy was deleted, Unix milliseconds.
    #[schema(example = 1_763_600_000_000_u64)]
    deleted_at_unix_ms: u64,
}

/// A JSON error answer.
fn error(status: StatusCode, slug: &'static str, message: impl Into<String>) -> Response {
    (status, Json(ErrorJson::new(status, slug, message.into()))).into_response()
}

/// The answer for a failed collector call.
fn collector_error(err: &LookupError) -> Response {
    let message = err.to_string();
    match err {
        LookupError::NotFound => error(StatusCode::NOT_FOUND, "report_not_found", message),
        LookupError::RateLimited => error(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message),
        LookupError::NotConfigured | LookupError::Unavailable => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "lookup_unavailable",
            message,
        ),
        LookupError::Status(_) | LookupError::Transport(_) | LookupError::BadReply(_) => {
            error(StatusCode::BAD_GATEWAY, "collector_error", message)
        }
    }
}

/// What a lookup or delete needs from the install.
struct Target {
    /// Diagnostics settings, for the collector URL.
    config: bookclerk_config::DiagnosticsConfig,
    /// The files directory holding the report history.
    files_dir: std::path::PathBuf,
    /// The report.
    report_id: ReportId,
    /// Its code.
    code: AccessCode,
}

/// Finds the report and its code in the history.
///
/// # Errors
///
/// The answer to send instead: `400` for a malformed ID, `404` when the
/// report is not in the history or has no code.
async fn target(state: &AppState, id: &str) -> Result<Target, Response> {
    let Some(report_id) = ReportId::parse(&id.trim().to_ascii_lowercase()) else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_report_id",
            "a report ID is a version-4 UUID",
        ));
    };
    let cfg = state.config.read().await;
    let config = cfg.diagnostics.clone();
    let Some(files_dir) = cfg.paths.as_ref().map(|p| p.files_dir.clone()) else {
        return Err(error(
            StatusCode::NOT_FOUND,
            "report_unknown",
            "this install has no report history",
        ));
    };
    drop(cfg);
    let (dir, id) = (files_dir.clone(), report_id.clone());
    let record: Option<ReportRecord> =
        tokio::task::spawn_blocking(move || bookclerk_config::find_report(&dir, &id))
            .await
            .unwrap_or_default();
    let Some(record) = record else {
        return Err(error(
            StatusCode::NOT_FOUND,
            "report_unknown",
            "this install has not sent a report with that ID",
        ));
    };
    let Some(code) = record.access_code else {
        return Err(error(
            StatusCode::NOT_FOUND,
            "no_access_code",
            "the collector gave this report no access code, so it cannot be read back",
        ));
    };
    Ok(Target {
        config,
        files_dir,
        report_id,
        code,
    })
}

/// Reads a sent report back from the collector with its access code: the
/// stored report (redacted, as sent), the GitHub issues tracking it and the
/// fix, if one is recorded.
#[utoipa::path(
    get,
    path = "/api/diagnostics/reports/{id}",
    params(("id" = String, Path, description = "The report's ID, from `GET /api/diagnostics/reports`.")),
    responses(
        (status = 200, description = "The report as the collector holds it.", body = DiagnosticsReportLookup),
        (status = 400, description = "The ID is not a version-4 UUID (`invalid_report_id`).", body = ErrorJson),
        (status = 404, description = "This install has not sent that report (`report_unknown`), has no code for it (`no_access_code`), or the collector no longer has it (`report_not_found`).", body = ErrorJson),
        (status = 429, description = "Too many lookups of this report lately; try again in a minute (`rate_limited`).", body = ErrorJson),
        (status = 502, description = "The collector could not be reached or answered badly (`collector_error`).", body = ErrorJson),
        (status = 503, description = "No collector is configured, or it offers no lookups (`lookup_unavailable`).", body = ErrorJson),
    ),
)]
pub(crate) async fn get_diagnostics_report(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let t = match target(&state, &id).await {
        Ok(t) => t,
        Err(answer) => return answer,
    };
    let fetched = tokio::task::spawn_blocking(move || {
        bookclerk_config::fetch_report(&t.config, &t.report_id, &t.code)
    })
    .await
    .unwrap_or_else(|e| Err(LookupError::Transport(e.to_string())));
    match fetched {
        Ok(answer) => Json(DiagnosticsReportLookup::from(answer)).into_response(),
        Err(err) => collector_error(&err),
    }
}

/// Deletes the collector's stored copy of a sent report. The collector keeps
/// its one-line index entry and issue links, so the report still counts;
/// the install's history notes the deletion.
#[utoipa::path(
    delete,
    path = "/api/diagnostics/reports/{id}",
    params(("id" = String, Path, description = "The report's ID, from `GET /api/diagnostics/reports`.")),
    responses(
        (status = 200, description = "The stored copy is gone (deleting again answers the same).", body = DiagnosticsReportDeleted),
        (status = 400, description = "The ID is not a version-4 UUID (`invalid_report_id`).", body = ErrorJson),
        (status = 404, description = "This install has not sent that report (`report_unknown`), has no code for it (`no_access_code`), or the collector no longer has it (`report_not_found`).", body = ErrorJson),
        (status = 429, description = "Too many attempts for this report lately; try again in a minute (`rate_limited`).", body = ErrorJson),
        (status = 502, description = "The collector could not be reached, answered badly or could not delete the copy (`collector_error`).", body = ErrorJson),
        (status = 503, description = "No collector is configured, or it offers no lookups (`lookup_unavailable`).", body = ErrorJson),
    ),
)]
pub(crate) async fn delete_diagnostics_report(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let t = match target(&state, &id).await {
        Ok(t) => t,
        Err(answer) => return answer,
    };
    let report_id = t.report_id.clone();
    let deleted = tokio::task::spawn_blocking(move || {
        let at = bookclerk_config::delete_report(&t.config, &t.report_id, &t.code)?;
        // The collector's copy is gone either way; the history note is a
        // convenience for the Settings list.
        if let Err(err) = bookclerk_config::mark_report_deleted(&t.files_dir, &t.report_id, at) {
            tracing::warn!(error = %err, "could not note the deleted report in the history");
        }
        Ok(at)
    })
    .await
    .unwrap_or_else(|e| Err(LookupError::Transport(e.to_string())));
    match deleted {
        Ok(at) => Json(DiagnosticsReportDeleted {
            report_id: report_id.as_str().to_string(),
            deleted_at_unix_ms: at,
        })
        .into_response(),
        Err(err) => collector_error(&err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// A report ID for the tests.
    const ID: &str = "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f";

    /// Answers one HTTP request on a loopback port with `status` and `body`.
    fn serve_once(status: &'static str, body: String) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
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
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        });
        url
    }

    /// Daemon state over a fresh files directory whose collector is `url`.
    async fn state(dir: &std::path::Path, url: &str) -> Arc<AppState> {
        use bookclerk_library::LibraryStore;
        let mut cfg = bookclerk_config::Config::load(Some(dir.to_path_buf()), None).expect("cfg");
        cfg.diagnostics.collector_url = url.to_string();
        let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
            .await
            .expect("db");
        let store = LibraryStore::from_connection(db);
        Arc::new(crate::config_authority::control_plane_test_state(
            store, cfg,
        ))
    }

    /// Records the test report, with or without a code.
    fn record(dir: &std::path::Path, id: &str, code: Option<&str>) {
        bookclerk_config::record_report(
            dir,
            ReportRecord {
                report_id: ReportId::parse(id).expect("id"),
                trigger: "crash".into(),
                version: "1".into(),
                sent_at_unix_ms: 1,
                access_code: code.and_then(AccessCode::parse),
                deleted_at_unix_ms: None,
            },
        )
        .expect("record");
    }

    /// The status and JSON body of an answer.
    async fn read(answer: Response) -> (StatusCode, serde_json::Value) {
        let status = answer.status();
        let bytes = to_bytes(answer.into_body(), 1 << 20).await.expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    #[tokio::test]
    async fn a_report_is_read_back_and_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        record(dir.path(), ID, Some("K7QM3XRDPW9TH2CF"));
        let url = serve_once(
            "200 OK",
            format!(
                r#"{{"report_id":"{ID}","received_at_unix_ms":5,"report":{{"version":"1","trigger":"crash","os":"linux","arch":"x86_64","distro":"","events":[{{"ts_unix_ms":1,"level":"ERROR","target":"t","message":"m","fields":[["k","v"]]}}]}},"issues":[{{"number":7,"title":"Crash","state":"closed","url":"https://github.com/o/r/issues/7"}}],"fix":{{"pr_number":9,"pr_url":null,"fixed_in":"1.0.1"}}}}"#
            ),
        );
        let st = state(dir.path(), &url).await;
        let (status, body) =
            read(get_diagnostics_report(State(st), Path(ID.to_uppercase())).await).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["report"]["events"][0]["level"], "ERROR");
        assert_eq!(body["report"]["events"][0]["fields"][0]["name"], "k");
        assert_eq!(body["issues"][0]["state"], "closed");
        assert_eq!(body["fix"]["fixed_in"], "1.0.1");

        let url = serve_once(
            "200 OK",
            format!(r#"{{"report_id":"{ID}","deleted_at_unix_ms":8}}"#),
        );
        let st = state(dir.path(), &url).await;
        let (status, body) =
            read(delete_diagnostics_report(State(st), Path(ID.into())).await).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted_at_unix_ms"], 8);
        let id = ReportId::parse(ID).expect("id");
        assert_eq!(
            bookclerk_config::find_report(dir.path(), &id).and_then(|r| r.deleted_at_unix_ms),
            Some(8)
        );
    }

    #[tokio::test]
    async fn refusals_name_their_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let other = "0e9ced79-df3c-4dae-872b-2e5772b06e69";
        record(dir.path(), other, None);
        record(dir.path(), ID, Some("K7QM3XRDPW9TH2CF"));
        let st = state(dir.path(), "http://127.0.0.1:9").await;
        for (id, status, slug) in [
            ("nope", StatusCode::BAD_REQUEST, "invalid_report_id"),
            (
                "1f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f",
                StatusCode::NOT_FOUND,
                "report_unknown",
            ),
            (other, StatusCode::NOT_FOUND, "no_access_code"),
            (ID, StatusCode::BAD_GATEWAY, "collector_error"),
        ] {
            let (got, body) =
                read(get_diagnostics_report(State(st.clone()), Path(id.into())).await).await;
            assert_eq!((got, body["error"].as_str()), (status, Some(slug)), "{id}");
        }
        for (reply, status, slug) in [
            ("404 Not Found", StatusCode::NOT_FOUND, "report_not_found"),
            (
                "429 Too Many Requests",
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
            ),
            (
                "503 Service Unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
                "lookup_unavailable",
            ),
        ] {
            let st = state(dir.path(), &serve_once(reply, "{}".into())).await;
            let (got, body) =
                read(delete_diagnostics_report(State(st), Path(ID.into())).await).await;
            assert_eq!(
                (got, body["error"].as_str()),
                (status, Some(slug)),
                "{reply}"
            );
        }
    }
}
