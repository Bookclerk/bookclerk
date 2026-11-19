//! Opt-in diagnostics: recent-log ring buffer, crash + error-burst upload.
//!
//! # Audience
//!
//! Host binaries that call [`crate::logging::init_tracing_with`]. Guests never
//! depend on this module. See `docs/server/diagnostics.md`.

use bookclerk_tls::ureq;
use std::collections::VecDeque;
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::redact::{redact_str, sanitize_for_remote_upload, RedactingVisitor};
use crate::settings::DiagnosticsConfig;

/// Snapshot of a single redacted log event kept for crash / error uploads.
#[derive(Debug, Clone, Serialize)]
pub struct BufferedEvent {
    /// Event time as Unix milliseconds (wall clock).
    pub ts_unix_ms: u64,
    /// Tracing level name (`ERROR`, `WARN`, `INFO`, …).
    pub level: String,
    /// Tracing target / module path.
    pub target: String,
    /// Redacted primary message text.
    pub message: String,
    /// Additional redacted structured fields as `(name, value)` pairs.
    pub fields: Vec<(String, String)>,
}

/// Target of the events that mark a slow response.
///
/// Events on this target count toward the `slow_burst` trigger instead of
/// `warn_burst`, so slowness on a starved machine and real faults reach the
/// collector as different kinds of report. The daemon's request log
/// (`bookclerkd`) is the writer today.
pub const SLOW_TARGET: &str = "bookclerk::slow";

/// Most WARN and ERROR events kept beside the ring.
///
/// Dependencies' TRACE and DEBUG events can turn the main ring over in a
/// fraction of a second, so the events that tripped a burst are kept here as
/// well and merged back into the report in order.
const NOTABLE_CAPACITY: usize = 64;

/// Most events one upload may carry: the collector refuses a report with
/// more (`validateClientPayload` in `tools/diagnostics-collector/worker.ts`).
pub const MAX_UPLOAD_EVENTS: usize = 500;

/// Most span fields copied onto one event, innermost span first.
const MAX_SPAN_FIELDS: usize = 8;

/// Span fields that stay in local logs but are not copied onto ring events:
/// a raw request path can carry IDs, and the `route` template beside it
/// names the same endpoint without them.
const SPAN_FIELDS_KEPT_LOCAL: [&str; 1] = ["path"];

/// Field names of `log`-crate records that describe the record itself
/// (`log.target` becomes the event's target; the rest are dropped).
const LOG_RECORD_FIELDS: [&str; 4] = ["log.target", "log.module_path", "log.file", "log.line"];

/// Which kind of Bookclerk process installed the subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessRole {
    /// The long-running daemon (`bookclerkd`).
    #[default]
    Daemon,
    /// The desktop window, which runs the host in-process.
    Desktop,
    /// The command-line tool (`bookclerk`).
    Cli,
}

impl ProcessRole {
    /// Name reported in uploads and check-ins (`daemon`, `desktop`, `cli`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Desktop => "desktop",
            Self::Cli => "cli",
        }
    }
}

/// JSON body POSTed to the diagnostics collector `/submit` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct UploadPayload {
    /// Why this upload was produced (`crash`, `error_burst`, `slow_burst`,
    /// `cli_error`, …).
    pub trigger: String,
    /// What sent it: `daemon`, `desktop`, `cli`, or `demo` for a copy of the
    /// hosted demo.
    pub deployment: String,
    /// The install's random ID, sent only when `diagnostics.share_usage` is
    /// on. The collector keeps only a monthly-salted hash of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    /// Bookclerk version string recorded at subscriber install.
    pub version: String,
    /// Host OS (`linux`, `macos`, `windows`, …).
    pub os: String,
    /// CPU architecture (`x86_64`, `aarch64`, …).
    pub arch: String,
    /// Best-effort Linux distro id when detectable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    /// `rustc` release baked in at build time (or `"unknown"`).
    pub rustc_release: String,
    /// `rustc` channel baked in at build time (or `"unknown"`).
    pub rustc_channel: String,
    /// Upload assembly time as Unix milliseconds.
    pub archived_at_unix_ms: u64,
    /// Redacted ring-buffer snapshot (oldest first).
    pub events: Vec<BufferedEvent>,
}

/// In-memory ring of redacted events plus burst clocks for automatic upload.
struct RingState {
    /// Oldest-first ring of redacted log events with their sequence numbers
    /// (evicts from the front at capacity).
    events: VecDeque<(u64, BufferedEvent)>,
    /// Oldest-first WARN and ERROR events with their sequence numbers, kept
    /// after the main ring has dropped them (at most [`NOTABLE_CAPACITY`]).
    notable: VecDeque<(u64, BufferedEvent)>,
    /// Sequence number the next event gets; orders the merge of both rings.
    next_seq: u64,
    /// Maximum events retained (at least 1).
    capacity: usize,
    /// Monotonic instants of ERROR events inside the burst window.
    error_timestamps: VecDeque<Instant>,
    /// Monotonic instants of WARN events inside the burst window.
    warn_timestamps: VecDeque<Instant>,
    /// Monotonic instants of slow-response events ([`SLOW_TARGET`]) inside
    /// the warn-burst window.
    slow_timestamps: VecDeque<Instant>,
    /// Instant of the last successful upload, used for the 300s burst cooldown.
    last_upload: Option<Instant>,
}

impl RingState {
    /// Allocates an empty ring with `capacity` clamped to `1..=512`.
    fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity.min(512)),
            notable: VecDeque::with_capacity(NOTABLE_CAPACITY),
            next_seq: 0,
            capacity: capacity.max(1),
            error_timestamps: VecDeque::new(),
            warn_timestamps: VecDeque::new(),
            slow_timestamps: VecDeque::new(),
            last_upload: None,
        }
    }

    /// Appends an event, dropping the oldest when the ring is full. A WARN
    /// or ERROR event is also kept in the notable ring.
    ///
    /// # Arguments
    ///
    /// * `event` - The redacted event.
    /// * `notable` - Whether it is a WARN or ERROR event.
    fn push(&mut self, event: BufferedEvent, notable: bool) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if notable {
            if self.notable.len() >= NOTABLE_CAPACITY {
                self.notable.pop_front();
            }
            self.notable.push_back((seq, event.clone()));
        }
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back((seq, event));
    }

    /// Oldest-first copy for crash or burst upload: the ring, plus the WARN
    /// and ERROR events the ring has already dropped, in the order they
    /// happened. At most [`MAX_UPLOAD_EVENTS`]: the dropped WARN and ERROR
    /// events are kept and the ring's oldest events give way.
    fn snapshot(&self) -> Vec<BufferedEvent> {
        let oldest_in_ring = self.events.front().map_or(u64::MAX, |(seq, _)| *seq);
        let mut out: Vec<(u64, BufferedEvent)> = self
            .notable
            .iter()
            .filter(|(seq, _)| *seq < oldest_in_ring)
            .cloned()
            .collect();
        let room = MAX_UPLOAD_EVENTS.saturating_sub(out.len());
        let skip = self.events.len().saturating_sub(room);
        out.extend(self.events.iter().skip(skip).cloned());
        out.into_iter().map(|(_, event)| event).collect()
    }
}

/// Process-global diagnostics handle installed by [`crate::logging::init_tracing_with`].
#[derive(Clone)]
pub struct DiagnosticsHandle {
    /// Shared process state behind the cloneable handle.
    inner: Arc<DiagnosticsInner>,
}

/// Config, version, ring, and upload serialization for the process-global handle.
struct DiagnosticsInner {
    /// Operator knobs for ring size, burst thresholds, and collector URL.
    config: DiagnosticsConfig,
    /// Bookclerk version string recorded when the subscriber was installed.
    version: String,
    /// Mutex-protected ring and burst clocks (recovers from poison).
    ring: Mutex<RingState>,
    /// Advisory flag so burst paths do not spawn a thread per event.
    upload_in_flight: AtomicBool,
    /// Serializes the HTTP upload body so crash/explicit waits never double-POST.
    upload_gate: Mutex<()>,
    /// Cooldown between automatic uploads (seconds).
    upload_cooldown_secs: u64,
    /// Monotonic counter of successful/attempted uploads (tests / status).
    uploads_attempted: AtomicU64,
    /// What kind of process this is, reported in uploads.
    role: ProcessRole,
    /// The install's random ID, set once the files directory is known and
    /// only when `share_usage` is on.
    install_id: OnceLock<String>,
    /// Live `diagnostics.share_reports`: starts from the config and follows
    /// Settings changes on reload ([`DiagnosticsHandle::set_sharing`]).
    share_reports: AtomicBool,
    /// Live `diagnostics.share_usage`, like `share_reports`.
    share_usage: AtomicBool,
    /// The files directory, once known, where accepted reports are recorded
    /// (`diagnostics/reports.json`, see [`crate::record_report`]).
    files_dir: OnceLock<PathBuf>,
}

impl DiagnosticsHandle {
    /// A new handle with ring capacity from config (at least 1). The
    /// process installs one with `install_global`; tests build their own.
    pub fn new(config: DiagnosticsConfig, version: impl Into<String>, role: ProcessRole) -> Self {
        let capacity = config.ring_buffer_capacity.max(1) as usize;
        let share_reports = AtomicBool::new(config.share_reports);
        let share_usage = AtomicBool::new(config.share_usage);
        Self {
            inner: Arc::new(DiagnosticsInner {
                config,
                version: version.into(),
                ring: Mutex::new(RingState::new(capacity)),
                upload_in_flight: AtomicBool::new(false),
                upload_gate: Mutex::new(()),
                upload_cooldown_secs: 300,
                uploads_attempted: AtomicU64::new(0),
                role,
                install_id: OnceLock::new(),
                share_reports,
                share_usage,
                files_dir: OnceLock::new(),
            }),
        }
    }

    /// Whether automatic upload is configured and ready (token/url present).
    #[must_use]
    pub fn upload_enabled(&self) -> bool {
        self.inner.share_reports.load(Ordering::Relaxed)
            && !self.inner.config.effective_collector_url().is_empty()
    }

    /// Whether the weekly usage check-in and the install ID are on right now
    /// (`diagnostics.share_usage`, following Settings changes).
    #[must_use]
    pub fn share_usage(&self) -> bool {
        self.inner.share_usage.load(Ordering::Relaxed)
    }

    /// Applies new `diagnostics.share_reports` / `share_usage` values without
    /// a restart (the daemon calls it after a config reload).
    ///
    /// # Arguments
    ///
    /// * `share_reports` - Whether reports are uploaded from now on.
    /// * `share_usage` - Whether uploads carry the install ID and the
    ///   check-in runs from now on.
    pub fn set_sharing(&self, share_reports: bool, share_usage: bool) {
        self.inner
            .share_reports
            .store(share_reports, Ordering::Relaxed);
        self.inner.share_usage.store(share_usage, Ordering::Relaxed);
        // Turning reports on live also arms crash reports, as starting with
        // them on does; the hook reports through the global handle only.
        if self.is_global() && self.upload_enabled() && self.inner.config.upload_on_crash {
            install_panic_hook();
        }
    }

    /// Whether this handle is the process-global one ([`install_global`]).
    fn is_global(&self) -> bool {
        GLOBAL_DIAGNOSTICS
            .get()
            .is_some_and(|g| Arc::ptr_eq(&g.inner, &self.inner))
    }

    /// Tells the handle where the files directory is, so accepted reports
    /// are recorded in the install's report history. The first call wins.
    ///
    /// # Arguments
    ///
    /// * `files_dir` - The Bookclerk files directory.
    pub fn set_files_dir(&self, files_dir: &Path) {
        let _ = self.inner.files_dir.set(files_dir.to_path_buf());
    }

    /// Snapshot of the in-memory diagnostics ring (newest last).
    #[must_use]
    pub fn snapshot_events(&self) -> Vec<BufferedEvent> {
        let guard = self.inner.ring.lock().unwrap_or_else(|e| e.into_inner());
        guard.snapshot()
    }

    /// Bookclerk version string recorded at subscriber install.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.inner.version
    }

    /// Attaches the install's random ID to later uploads. Does nothing unless
    /// `diagnostics.share_usage` is on; the first ID set stays.
    ///
    /// # Arguments
    ///
    /// * `id` - The install ID ([`crate::InstallId`]).
    pub fn set_install_id(&self, id: &crate::InstallId) {
        if self.share_usage() {
            let _ = self.inner.install_id.set(id.as_str().to_string());
        }
    }

    /// The install ID an upload carries: none unless usage sharing is on
    /// (and never for a demo copy). Loaded, or made, from the files
    /// directory the first time it is needed, so a report sent right after
    /// usage sharing is turned on, or from a CLI run, carries it without
    /// waiting for the daemon's check-in thread.
    fn upload_install_id(&self) -> Option<String> {
        if !self.share_usage() || self.inner.config.demo {
            return None;
        }
        if let Some(id) = self.inner.install_id.get() {
            return Some(id.clone());
        }
        let dir = self.inner.files_dir.get()?;
        let id = crate::InstallId::load_or_create(dir).ok()?;
        Some(
            self.inner
                .install_id
                .get_or_init(|| id.as_str().to_string())
                .clone(),
        )
    }

    /// What uploads report as `deployment`: `demo` for a copy of the hosted
    /// demo, otherwise the process role.
    #[must_use]
    pub fn deployment(&self) -> &'static str {
        if self.inner.config.demo {
            "demo"
        } else {
            self.inner.role.as_str()
        }
    }

    /// Number of upload attempts since init (successful or failed HTTP tries).
    #[must_use]
    pub fn uploads_attempted(&self) -> u64 {
        self.inner.uploads_attempted.load(Ordering::Relaxed)
    }

    /// Record a redacted event into the ring buffer and maybe trigger a burst upload.
    pub fn record_event(&self, event: &Event<'_>) {
        self.record_event_with_spans(event, Vec::new());
    }

    /// Record a redacted event with fields copied from the spans it happened
    /// in, and maybe trigger a burst upload.
    ///
    /// `log`-crate records take their own target (`log.target`) and lose the
    /// fields that describe the record itself (module path, file, line).
    ///
    /// # Arguments
    ///
    /// * `event` - The tracing event.
    /// * `span_fields` - Redacted `(span.field, value)` pairs, innermost span
    ///   first (see `DiagnosticsLayer`).
    pub fn record_event_with_spans(&self, event: &Event<'_>, span_fields: Vec<(String, String)>) {
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        let mut visitor = RedactingVisitor::default();
        event.record(&mut visitor);
        let mut fields: Vec<(String, String)> = visitor
            .fields
            .into_iter()
            .filter(|(k, _)| !LOG_RECORD_FIELDS.contains(&k.as_str()))
            .collect();
        fields.extend(span_fields);

        let buffered = BufferedEvent {
            ts_unix_ms: unix_now_ms(),
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            message: visitor.message.unwrap_or_default(),
            fields,
        };

        let is_error = *meta.level() == Level::ERROR;
        let is_slow = !is_error && meta.target() == SLOW_TARGET;
        let is_warn = !is_slow && *meta.level() == Level::WARN;
        let should_upload = {
            let mut guard = self.inner.ring.lock().unwrap_or_else(|e| e.into_inner());
            guard.push(buffered, is_error || *meta.level() == Level::WARN);
            if !self.upload_enabled() {
                false
            } else if is_slow && self.inner.config.upload_on_warn_burst {
                let last = guard.last_upload;
                note_level_and_check_burst(
                    &mut guard.slow_timestamps,
                    last,
                    self.inner.config.warn_burst_threshold,
                    self.inner.config.warn_burst_window_secs,
                )
            } else if is_error && self.inner.config.upload_on_error_burst {
                let last = guard.last_upload;
                note_level_and_check_burst(
                    &mut guard.error_timestamps,
                    last,
                    self.inner.config.error_burst_threshold,
                    self.inner.config.error_burst_window_secs,
                )
            } else if is_warn && self.inner.config.upload_on_warn_burst {
                let last = guard.last_upload;
                note_level_and_check_burst(
                    &mut guard.warn_timestamps,
                    last,
                    self.inner.config.warn_burst_threshold,
                    self.inner.config.warn_burst_window_secs,
                )
            } else {
                false
            }
        };

        if should_upload {
            let trigger = if is_error {
                "error_burst"
            } else if is_slow {
                "slow_burst"
            } else {
                "warn_burst"
            };
            self.spawn_upload(trigger, true);
        }
    }

    /// Request an upload of the current ring (e.g. after a failed daemon job).
    ///
    /// Explicit failure uploads bypass the burst cooldown so a recent burst cannot
    /// suppress `cli_error` / `job_failed` reports. If another upload is in flight,
    /// this waits briefly for the slot rather than dropping the request.
    pub fn request_upload(&self, trigger: &'static str) {
        if !self.upload_enabled() {
            return;
        }
        if self.try_claim_in_flight(Duration::from_secs(30)) {
            self.spawn_claimed(trigger);
        } else {
            // Still deliver via the serialized gate so the report is not dropped.
            self.upload_blocking(trigger);
        }
    }

    /// Best-effort upload of the current ring buffer (blocking).
    ///
    /// Used by the panic hook, CLI exit path, and tests. Waits for any in-flight
    /// upload to finish (serialized upload gate) so crash reports
    /// are not dropped and concurrent POSTs cannot overlap.
    pub fn upload_blocking(&self, trigger: &str) {
        if !self.upload_enabled() {
            return;
        }
        // Mark busy so burst spawn_upload bails; gate serializes the HTTP body.
        self.inner.upload_in_flight.store(true, Ordering::SeqCst);
        self.run_upload(trigger);
    }

    /// Try to claim `upload_in_flight`, waiting up to `timeout`.
    fn try_claim_in_flight(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .inner
                .upload_in_flight
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Claim `upload_in_flight` then spawn (burst path — no wait if busy).
    fn spawn_upload(&self, trigger: &'static str, honor_cooldown: bool) {
        if !self.upload_enabled() {
            return;
        }
        if honor_cooldown {
            let guard = self.inner.ring.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(last) = guard.last_upload {
                if last.elapsed() < Duration::from_secs(self.inner.upload_cooldown_secs) {
                    return;
                }
            }
        }
        if self
            .inner
            .upload_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.spawn_claimed(trigger);
    }

    /// Spawns a background upload after `upload_in_flight` is already claimed; releases the flag if spawn fails.
    fn spawn_claimed(&self, trigger: &'static str) {
        let handle = self.clone();
        if std::thread::Builder::new()
            .name("bookclerk-diag-upload".into())
            .spawn(move || handle.run_upload(trigger))
            .is_err()
        {
            self.inner.upload_in_flight.store(false, Ordering::SeqCst);
        }
    }

    /// Serializes HTTP, sanitizes events, and POSTs; refuses the body if a registered secret remains.
    fn run_upload(&self, trigger: &str) {
        // Serialize HTTP so a crash/explicit waiter never overlaps a burst POST.
        let _gate = self
            .inner
            .upload_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let events = {
            let guard = self.inner.ring.lock().unwrap_or_else(|e| e.into_inner());
            guard.snapshot()
        };
        // Extra pass before anything leaves the process (GitHub issue or HTTP).
        let events: Vec<BufferedEvent> =
            events.into_iter().map(sanitize_event_for_upload).collect();

        let payload = UploadPayload {
            trigger: trigger.to_string(),
            deployment: self.deployment().to_string(),
            install_id: self.upload_install_id(),
            version: self.inner.version.clone(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            distro: crate::platform::detect_distro(),
            rustc_release: option_env!("BOOKCLERK_RUSTC_RELEASE")
                .unwrap_or("unknown")
                .to_string(),
            rustc_channel: option_env!("BOOKCLERK_RUSTC_CHANNEL")
                .unwrap_or("unknown")
                .to_string(),
            archived_at_unix_ms: unix_now_ms(),
            events: events.clone(),
        };

        self.inner.uploads_attempted.fetch_add(1, Ordering::Relaxed);
        let result = post_http_payload(&self.inner.config, &payload);

        // Only successful uploads start the burst cooldown.
        if result.is_ok() {
            if let Ok(mut guard) = self.inner.ring.lock() {
                guard.last_upload = Some(Instant::now());
            }
        }
        self.inner.upload_in_flight.store(false, Ordering::SeqCst);

        // Avoid tracing recursion from the upload path: report on stderr.
        match result {
            Ok(reply) => self.remember_report(trigger, &reply),
            Err(err) => eprintln!("bookclerk: diagnostics upload failed ({trigger}): {err}"),
        }
    }

    /// Records an accepted report in the install's report history.
    ///
    /// # Arguments
    ///
    /// * `trigger` - Why the report was sent.
    /// * `reply` - The collector's reply body.
    fn remember_report(&self, trigger: &str, reply: &[u8]) {
        let report_id = match crate::parse_submit_reply(reply) {
            Ok(id) => id,
            Err(err) => {
                eprintln!("bookclerk: diagnostics report sent but not recorded: {err}");
                return;
            }
        };
        let Some(files_dir) = self.inner.files_dir.get() else {
            return;
        };
        let record = crate::ReportRecord {
            report_id,
            trigger: trigger.to_string(),
            version: self.inner.version.clone(),
            sent_at_unix_ms: unix_now_ms(),
        };
        if let Err(err) = crate::record_report(files_dir, record) {
            eprintln!("bookclerk: diagnostics report sent but not recorded: {err}");
        }
    }
}

/// Records a timestamp and returns true when the burst threshold is hit and cooldown has elapsed.
fn note_level_and_check_burst(
    timestamps: &mut VecDeque<Instant>,
    last_upload: Option<Instant>,
    threshold: u32,
    window_secs: u64,
) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs(window_secs.max(1));
    timestamps.push_back(now);
    while timestamps
        .front()
        .is_some_and(|t| now.duration_since(*t) > window)
    {
        timestamps.pop_front();
    }
    let threshold = threshold.max(1) as usize;
    if timestamps.len() < threshold {
        return false;
    }
    if let Some(last) = last_upload {
        if last.elapsed() < Duration::from_secs(300) {
            return false;
        }
    }
    timestamps.clear();
    true
}

/// Redacts and truncates message, target, and fields before anything leaves the process.
fn sanitize_event_for_upload(mut event: BufferedEvent) -> BufferedEvent {
    use crate::redact::truncate_upload_message;
    event.message = truncate_upload_message(&sanitize_for_remote_upload("message", &event.message));
    event.target = redact_str(&event.target);
    event.fields = event
        .fields
        .into_iter()
        .map(|(k, v)| {
            let value = sanitize_for_remote_upload(&k, &v);
            (k, value)
        })
        .collect();
    event
}

/// POSTs the JSON payload; fail-closed if a registered secret is still visible after redaction.
///
/// # Returns
///
/// The collector's reply body, read up to [`crate::MAX_SUBMIT_REPLY_BYTES`]
/// plus one byte so an overlong reply is still refused by the parser.
///
/// # Errors
///
/// Returns an error when serialization, redaction checks, or the HTTP request fails.
fn post_http_payload(
    config: &DiagnosticsConfig,
    payload: &UploadPayload,
) -> Result<Vec<u8>, String> {
    use crate::redact::contains_registered_secret;

    let body = serde_json::to_string(payload).map_err(|e| e.to_string())?;
    let body = redact_str(&body);
    // Hard stop: never upload if a registered secret is still visible.
    if contains_registered_secret(&body) {
        return Err(
            "refusing diagnostics upload: registered secret still present after redaction".into(),
        );
    }
    let url = config.effective_submit_url();
    if url.is_empty() {
        return Err("diagnostics collector_url is empty".into());
    }
    let mut response = ureq::post(&url)
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            &format!("bookclerk-diagnostics/{}", payload.version),
        )
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .send(body.as_str())
        .map_err(|e| format!("diagnostics collector upload failed: {e}"))?;
    let limit = u64::try_from(crate::MAX_SUBMIT_REPLY_BYTES + 1).unwrap_or(u64::MAX);
    // The report is stored once the collector answered; a reply that cannot
    // be read only costs the history entry.
    Ok(response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .unwrap_or_default())
}

/// Wall-clock Unix milliseconds, or `0` if the clock is before the epoch.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Tracing layer that feeds the diagnostics ring buffer.
pub struct DiagnosticsLayer {
    /// Ring handle that receives every tracing event.
    handle: DiagnosticsHandle,
}

impl DiagnosticsLayer {
    #[must_use]
    /// Wraps an existing handle as a tracing layer.
    pub fn new(handle: DiagnosticsHandle) -> Self {
        Self { handle }
    }
}

/// Redacted fields a span was created or later recorded with, kept in the
/// span's extensions so events inside it can carry them.
#[derive(Default)]
struct SpanFields {
    /// `(field, redacted value)` pairs in recording order.
    fields: Vec<(String, String)>,
}

impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = RedactingVisitor::default();
        attrs.record(&mut visitor);
        span.extensions_mut().insert(SpanFields {
            fields: visitor.fields,
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut visitor = RedactingVisitor::default();
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        if let Some(stored) = extensions.get_mut::<SpanFields>() {
            for (key, value) in visitor.fields {
                match stored.fields.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = value,
                    None => stored.fields.push((key, value)),
                }
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut span_fields = Vec::new();
        if let Some(scope) = ctx.event_scope(event) {
            'spans: for span in scope {
                let extensions = span.extensions();
                let Some(stored) = extensions.get::<SpanFields>() else {
                    continue;
                };
                for (key, value) in &stored.fields {
                    if SPAN_FIELDS_KEPT_LOCAL.contains(&key.as_str()) {
                        continue;
                    }
                    if span_fields.len() >= MAX_SPAN_FIELDS {
                        break 'spans;
                    }
                    span_fields.push((format!("{}.{key}", span.name()), value.clone()));
                }
            }
        }
        self.handle.record_event_with_spans(event, span_fields);
    }
}

/// Process-global handle; first [`install_global`] wins.
static GLOBAL_DIAGNOSTICS: OnceLock<DiagnosticsHandle> = OnceLock::new();

/// Install (or replace is not allowed — first wins) the process-global diagnostics handle
/// and a panic hook that uploads when configured.
pub fn install_global(handle: DiagnosticsHandle) {
    let _ = GLOBAL_DIAGNOSTICS.set(handle.clone());
    if handle.upload_enabled() && handle.inner.config.upload_on_crash {
        install_panic_hook();
    }
}

/// Accessor for the process-global diagnostics handle.
#[must_use]
pub fn global() -> Option<&'static DiagnosticsHandle> {
    GLOBAL_DIAGNOSTICS.get()
}

/// Installs a once-only panic hook that records a redacted line and uploads `crash`.
fn install_panic_hook() {
    static HOOK_SET: AtomicBool = AtomicBool::new(false);
    if HOOK_SET
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        // Record a synthetic redacted panic line into the buffer, then upload.
        if let Some(handle) = GLOBAL_DIAGNOSTICS.get() {
            let msg = panic_message(info);
            let redacted = crate::redact::redact_str(&msg);
            if let Ok(mut guard) = handle.inner.ring.lock() {
                guard.push(
                    BufferedEvent {
                        ts_unix_ms: unix_now_ms(),
                        level: "ERROR".into(),
                        target: "bookclerk::panic".into(),
                        message: redacted,
                        fields: vec![],
                    },
                    true,
                );
            }
            handle.upload_blocking("crash");
        }
        prev(info);
    }));
}

/// Extracts a `&str` or `String` panic payload, else a placeholder.
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "box dyn Any".to_string()
    }
}

#[cfg(test)]
pub fn push_test_event(handle: &DiagnosticsHandle, level: &str, message: &str) {
    use crate::redact::redact_field_value;
    let mut guard = handle.inner.ring.lock().unwrap();
    let notable = level == "ERROR" || level == "WARN";
    guard.push(
        BufferedEvent {
            ts_unix_ms: unix_now_ms(),
            level: level.into(),
            target: "test".into(),
            message: redact_field_value("message", message),
            fields: vec![],
        },
        notable,
    );
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_caps_capacity() {
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                ring_buffer_capacity: 3,
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        for i in 0..5 {
            push_test_event(&handle, "INFO", &format!("msg-{i}"));
        }
        let snap = handle.inner.ring.lock().unwrap().snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].message, "msg-2");
        assert_eq!(snap[2].message, "msg-4");
    }

    #[test]
    fn upload_skipped_when_disabled() {
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig::default(),
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        push_test_event(&handle, "ERROR", "boom Atna|should-not-upload");
        handle.upload_blocking("test");
        assert_eq!(handle.uploads_attempted(), 0);
    }

    #[test]
    fn upload_posts_redacted_json() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(Mutex::new(String::new()));
        let body_thread = Arc::clone(&body);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(header_end) = find_header_end(&raw) {
                    let headers = std::str::from_utf8(&raw[..header_end]).unwrap_or("");
                    let content_len = headers.lines().find_map(|line| {
                        let line = line.trim();
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    });
                    if let Some(len) = content_len {
                        if raw.len() >= header_end + len {
                            break;
                        }
                    } else if n < buf.len() {
                        break;
                    }
                }
            }
            *body_thread.lock().unwrap() = String::from_utf8_lossy(&raw).into_owned();
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        });

        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                share_reports: true,
                collector_url: format!("http://{addr}"),
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        push_test_event(
            &handle,
            "ERROR",
            "refresh failed Atna|leak-me-please and Bearer abc.def.ghi",
        );
        if let Ok(mut guard) = handle.inner.ring.lock() {
            if let Some(ev) = guard.events.back_mut() {
                ev.1.fields
                    .push(("title".into(), "My Secret Audiobook Title".into()));
            }
        }
        handle.upload_blocking("test");
        server.join().unwrap();

        let captured = body.lock().unwrap().clone();
        assert!(
            captured.contains("POST /submit"),
            "expected HTTP POST /submit, got: {captured}"
        );
        assert!(
            !captured.contains("Atna|leak-me-please"),
            "secret leaked: {captured}"
        );
        assert!(
            !captured.contains("Bearer abc.def.ghi"),
            "secret leaked: {captured}"
        );
        assert!(!captured.contains("My Secret Audiobook Title"));
        assert!(
            captured.contains("[REDACTED]"),
            "expected redaction marker in body: {captured}"
        );
        assert_eq!(handle.uploads_attempted(), 1);
    }

    /// Accepts one HTTP request on a loopback port, reads it whole and
    /// answers with `reply` (a full HTTP response).
    fn serve_once(reply: String) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(end) = find_header_end(&raw) {
                    let headers = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                    let len = headers
                        .lines()
                        .find_map(|l| l.trim().strip_prefix("content-length:").map(str::trim))
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= end + len {
                        break;
                    }
                }
            }
            let _ = stream.write_all(reply.as_bytes());
        });
        (addr, server)
    }

    #[test]
    fn an_accepted_report_lands_in_the_history() {
        let id = "0e9ced79-df3c-4dae-872b-2e5772b06e69";
        let body = format!(r#"{{"ok":true,"report_id":"{id}","received_at_unix_ms":1}}"#);
        let (addr, server) = serve_once(format!(
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        let dir = tempfile::tempdir().unwrap();
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                share_reports: true,
                collector_url: format!("http://{addr}"),
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        handle.set_files_dir(dir.path());
        push_test_event(&handle, "ERROR", "boom");
        handle.upload_blocking("crash");
        server.join().unwrap();
        let rows = crate::load_report_history(dir.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].report_id.as_str(), id);
        assert_eq!(rows[0].trigger, "crash");
        assert_eq!(rows[0].version, "0.0.0-test");
    }

    #[test]
    fn a_reply_without_a_report_id_records_nothing() {
        let (addr, server) =
            serve_once("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".into());
        let dir = tempfile::tempdir().unwrap();
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                share_reports: true,
                collector_url: format!("http://{addr}"),
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        handle.set_files_dir(dir.path());
        push_test_event(&handle, "ERROR", "boom");
        handle.upload_blocking("crash");
        server.join().unwrap();
        assert_eq!(handle.uploads_attempted(), 1);
        assert!(crate::load_report_history(dir.path()).is_empty());
    }

    #[test]
    fn an_upload_loads_the_install_id_once_usage_sharing_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let handle =
            DiagnosticsHandle::new(DiagnosticsConfig::default(), "0.0.0-test", ProcessRole::Cli);
        assert_eq!(handle.upload_install_id(), None, "no files directory yet");
        handle.set_files_dir(dir.path());
        assert_eq!(handle.upload_install_id(), None, "usage sharing is off");
        assert!(!dir.path().join(crate::install_id::INSTALL_ID_FILE).exists());
        handle.set_sharing(true, true);
        let id = handle.upload_install_id().expect("loaded on first use");
        assert_eq!(
            crate::InstallId::load_or_create(dir.path())
                .unwrap()
                .as_str(),
            id
        );
        assert_eq!(handle.upload_install_id(), Some(id));
        handle.set_sharing(true, false);
        assert_eq!(handle.upload_install_id(), None, "turned off again");

        let demo = DiagnosticsHandle::new(
            DiagnosticsConfig {
                demo: true,
                share_usage: true,
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        demo.set_files_dir(dir.path());
        assert_eq!(
            demo.upload_install_id(),
            None,
            "a demo copy never carries one"
        );
    }

    #[test]
    fn sharing_follows_set_sharing() {
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                collector_url: "http://127.0.0.1:9".into(),
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        assert!(!handle.upload_enabled());
        assert!(!handle.share_usage());
        handle.set_sharing(true, true);
        assert!(handle.upload_enabled());
        assert!(handle.share_usage());
        let id = crate::InstallId::generate().unwrap();
        handle.set_install_id(&id);
        assert_eq!(
            handle.inner.install_id.get().map(String::as_str),
            Some(id.as_str())
        );
        handle.set_sharing(false, false);
        assert!(!handle.upload_enabled());
        assert!(!handle.share_usage());
        // Only the process-global handle arms the crash hook.
        assert!(!handle.is_global());
    }

    #[test]
    fn exact_config_secret_redacted_in_upload() {
        use crate::redact::{
            clear_registered_secrets, register_secret, secrets_registry_test_lock,
        };
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let _lock = secrets_registry_test_lock();
        clear_registered_secrets();
        register_secret("exact-config-passphrase-abc123");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(Mutex::new(String::new()));
        let body_thread = Arc::clone(&body);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(header_end) = find_header_end(&raw) {
                    let headers = std::str::from_utf8(&raw[..header_end]).unwrap_or("");
                    if let Some(len) = headers.lines().find_map(|line| {
                        line.trim()
                            .to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    }) {
                        if raw.len() >= header_end + len {
                            break;
                        }
                    }
                }
            }
            *body_thread.lock().unwrap() = String::from_utf8_lossy(&raw).into_owned();
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        });

        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                share_reports: true,
                collector_url: format!("http://{addr}"),
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        // Bypass message-field pattern path: push raw then sanitize on upload.
        if let Ok(mut guard) = handle.inner.ring.lock() {
            guard.push(
                BufferedEvent {
                    ts_unix_ms: 1,
                    level: "ERROR".into(),
                    target: "test".into(),
                    message: "auth used exact-config-passphrase-abc123".into(),
                    fields: vec![],
                },
                true,
            );
        }
        handle.upload_blocking("test");
        server.join().unwrap();
        clear_registered_secrets();

        let captured = body.lock().unwrap().clone();
        assert!(!captured.contains("exact-config-passphrase-abc123"));
        assert!(captured.contains("[REDACTED]"));
    }

    fn find_header_end(raw: &[u8]) -> Option<usize> {
        raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    /// A handle and a subscriber feeding it through [`DiagnosticsLayer`].
    fn layered(
        config: DiagnosticsConfig,
    ) -> (DiagnosticsHandle, impl Subscriber + Send + Sync + 'static) {
        use tracing_subscriber::layer::SubscriberExt;
        let handle = DiagnosticsHandle::new(config, "0.0.0-test", ProcessRole::Daemon);
        let subscriber = tracing_subscriber::registry()
            .with(DiagnosticsLayer::new(handle.clone()).with_filter(crate::ring_filter()));
        (handle, subscriber)
    }

    #[test]
    fn warnings_the_ring_dropped_still_reach_the_report_in_order() {
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                ring_buffer_capacity: 3,
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        push_test_event(&handle, "WARN", "first warning");
        push_test_event(&handle, "INFO", "noise-1");
        push_test_event(&handle, "ERROR", "the error");
        for i in 2..6 {
            push_test_event(&handle, "INFO", &format!("noise-{i}"));
        }
        let messages: Vec<String> = handle
            .snapshot_events()
            .into_iter()
            .map(|e| e.message)
            .collect();
        assert_eq!(
            messages,
            [
                "first warning",
                "the error",
                "noise-3",
                "noise-4",
                "noise-5"
            ],
            "dropped WARN/ERROR events come back, oldest first, without duplicates"
        );
    }

    #[test]
    fn the_notable_ring_is_bounded() {
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                ring_buffer_capacity: 1,
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        for i in 0..(NOTABLE_CAPACITY + 10) {
            push_test_event(&handle, "WARN", &format!("w-{i}"));
        }
        let snap = handle.snapshot_events();
        assert_eq!(snap.len(), NOTABLE_CAPACITY);
        assert_eq!(snap[0].message, "w-10");
        assert_eq!(
            snap[NOTABLE_CAPACITY - 1].message,
            format!("w-{}", NOTABLE_CAPACITY + 9)
        );
    }

    #[test]
    fn an_upload_snapshot_stays_within_the_collector_limit() {
        let handle = DiagnosticsHandle::new(
            DiagnosticsConfig {
                ring_buffer_capacity: 600,
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        push_test_event(&handle, "WARN", "early warning");
        for i in 0..700 {
            push_test_event(&handle, "INFO", &format!("noise-{i}"));
        }
        let snap = handle.snapshot_events();
        assert_eq!(snap.len(), MAX_UPLOAD_EVENTS);
        assert_eq!(
            snap[0].message, "early warning",
            "dropped warnings are kept"
        );
        assert_eq!(snap[1].message, "noise-201");
        assert_eq!(snap[MAX_UPLOAD_EVENTS - 1].message, "noise-699");
    }

    #[test]
    fn events_carry_the_fields_of_the_spans_they_happen_in() {
        let (handle, subscriber) = layered(DiagnosticsConfig::default());
        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!(target: "bookclerk::test", "job", job_id = 7);
            let _o = outer.enter();
            let inner = tracing::info_span!(
                target: "bookclerk::test",
                "http.request",
                method = "GET",
                path = "/api/works/0e9ced79",
                route = tracing::field::Empty
            );
            inner.record("route", "/api/works/{id}");
            let _i = inner.enter();
            tracing::warn!(target: "bookclerk::test", status = 500, "slow");
        });
        let event = handle.snapshot_events().pop().unwrap();
        assert_eq!(
            event.fields,
            vec![
                ("status".to_string(), "500".to_string()),
                ("http.request.method".to_string(), "GET".to_string()),
                (
                    "http.request.route".to_string(),
                    "/api/works/{id}".to_string()
                ),
                ("job.job_id".to_string(), "7".to_string()),
            ]
        );
    }

    #[test]
    fn dependency_trace_stays_out_of_the_ring() {
        let (handle, subscriber) = layered(DiagnosticsConfig::default());
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "h2::proto::streams", "frame");
            tracing::debug!(target: "hyper_util::client", "pool");
            tracing::info!(target: "hyper_util::client", "kept");
            tracing::trace!(target: "bookclerkd::api", "own trace");
            tracing::trace!(target: "bookclerk_library::jobs", "crate trace");
        });
        let messages: Vec<String> = handle
            .snapshot_events()
            .into_iter()
            .map(|e| e.message)
            .collect();
        assert_eq!(messages, ["kept", "own trace", "crate trace"]);
    }

    #[test]
    fn log_records_take_their_own_target_and_lose_the_record_fields() {
        let (handle, subscriber) = layered(DiagnosticsConfig::default());
        tracing::subscriber::with_default(subscriber, || {
            let record = log::Record::builder()
                .level(log::Level::Warn)
                .target("reqwest::retry")
                .module_path(Some("reqwest::retry"))
                .file(Some("/usr/local/cargo/registry/src/reqwest/src/retry.rs"))
                .line(Some(234))
                .args(format_args!("giving up"))
                .build();
            tracing_log::format_trace(&record).unwrap();
        });
        let event = handle.snapshot_events().pop().unwrap();
        assert_eq!(event.target, "reqwest::retry");
        assert_eq!(event.message, "giving up");
        assert!(
            event.fields.iter().all(|(k, _)| !k.starts_with("log.")),
            "{:?}",
            event.fields
        );
    }

    #[test]
    fn slow_responses_count_toward_their_own_burst() {
        let mut ring = RingState::new(10);
        let config = DiagnosticsConfig::default();
        for _ in 0..config.warn_burst_threshold - 1 {
            assert!(!note_level_and_check_burst(
                &mut ring.slow_timestamps,
                None,
                config.warn_burst_threshold,
                config.warn_burst_window_secs,
            ));
        }
        assert!(note_level_and_check_burst(
            &mut ring.slow_timestamps,
            None,
            config.warn_burst_threshold,
            config.warn_burst_window_secs,
        ));
        assert!(ring.warn_timestamps.is_empty());
    }

    #[test]
    fn slow_events_skip_the_warn_clock() {
        let (handle, subscriber) = layered(DiagnosticsConfig::default());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: SLOW_TARGET, latency_ms = 2_900_u64, "slow response");
            tracing::warn!(target: "bookclerk::test", "plain warning");
        });
        let guard = handle.inner.ring.lock().unwrap();
        assert_eq!(guard.warn_timestamps.len(), 0, "uploads are off");
        drop(guard);
        assert_eq!(handle.snapshot_events().len(), 2);
    }

    #[test]
    fn deployment_names_the_demo_or_the_role() {
        let demo = DiagnosticsHandle::new(
            DiagnosticsConfig {
                demo: true,
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Desktop,
        );
        assert_eq!(demo.deployment(), "demo");
        let desktop = DiagnosticsHandle::new(
            DiagnosticsConfig::default(),
            "0.0.0-test",
            ProcessRole::Desktop,
        );
        assert_eq!(desktop.deployment(), "desktop");
        assert_eq!(ProcessRole::Cli.as_str(), "cli");
        assert_eq!(ProcessRole::Daemon.as_str(), "daemon");
    }

    #[test]
    fn the_install_id_rides_only_with_share_usage() {
        let id = crate::InstallId::generate().unwrap();
        let off = DiagnosticsHandle::new(
            DiagnosticsConfig::default(),
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        off.set_install_id(&id);
        assert!(off.inner.install_id.get().is_none());
        let on = DiagnosticsHandle::new(
            DiagnosticsConfig {
                share_usage: true,
                ..DiagnosticsConfig::default()
            },
            "0.0.0-test",
            ProcessRole::Daemon,
        );
        on.set_install_id(&id);
        assert_eq!(
            on.inner.install_id.get().map(String::as_str),
            Some(id.as_str())
        );
    }
}
