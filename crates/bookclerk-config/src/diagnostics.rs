//! Opt-in diagnostics: recent-log ring buffer, crash + error-burst upload.
//!
//! # Audience
//!
//! Host binaries that call [`crate::logging::init_tracing_with`]. Guests never
//! depend on this module. See `docs/diagnostics.md`.

use std::collections::VecDeque;
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

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

/// JSON body POSTed to the diagnostics collector `/submit` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct UploadPayload {
    /// Why this upload was produced (`crash`, `error_burst`, `cli_error`, …).
    pub trigger: String,
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
    /// Oldest-first ring of redacted log events (evicts from the front at capacity).
    events: VecDeque<BufferedEvent>,
    /// Maximum events retained (at least 1).
    capacity: usize,
    /// Monotonic instants of ERROR events inside the burst window.
    error_timestamps: VecDeque<Instant>,
    /// Monotonic instants of WARN events inside the burst window.
    warn_timestamps: VecDeque<Instant>,
    /// Instant of the last successful upload, used for the 300s burst cooldown.
    last_upload: Option<Instant>,
}

impl RingState {
    /// Allocates an empty ring with `capacity` clamped to `1..=512`.
    fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity.min(512)),
            capacity: capacity.max(1),
            error_timestamps: VecDeque::new(),
            warn_timestamps: VecDeque::new(),
            last_upload: None,
        }
    }

    /// Appends an event, dropping the oldest when the ring is full.
    fn push(&mut self, event: BufferedEvent) {
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// Oldest-first clone of the ring for crash or burst upload.
    fn snapshot(&self) -> Vec<BufferedEvent> {
        self.events.iter().cloned().collect()
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
}

impl DiagnosticsHandle {
    /// Installs a handle with ring capacity from config (at least 1).
    pub(crate) fn new(config: DiagnosticsConfig, version: impl Into<String>) -> Self {
        let capacity = config.ring_buffer_capacity.max(1) as usize;
        Self {
            inner: Arc::new(DiagnosticsInner {
                config,
                version: version.into(),
                ring: Mutex::new(RingState::new(capacity)),
                upload_in_flight: AtomicBool::new(false),
                upload_gate: Mutex::new(()),
                upload_cooldown_secs: 300,
                uploads_attempted: AtomicU64::new(0),
            }),
        }
    }

    /// Whether automatic upload is configured and ready (token/url present).
    #[must_use]
    pub fn upload_enabled(&self) -> bool {
        self.inner.config.upload_ready()
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

    /// Number of upload attempts since init (successful or failed HTTP tries).
    #[must_use]
    pub fn uploads_attempted(&self) -> u64 {
        self.inner.uploads_attempted.load(Ordering::Relaxed)
    }

    /// Record a redacted event into the ring buffer and maybe trigger an error-burst upload.
    pub fn record_event(&self, event: &Event<'_>) {
        let meta = event.metadata();
        let mut visitor = RedactingVisitor::default();
        event.record(&mut visitor);

        let buffered = BufferedEvent {
            ts_unix_ms: unix_now_ms(),
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            message: visitor.message.unwrap_or_default(),
            fields: visitor.fields,
        };

        let is_error = *meta.level() == Level::ERROR;
        let is_warn = *meta.level() == Level::WARN;
        let should_upload = {
            let mut guard = self.inner.ring.lock().unwrap_or_else(|e| e.into_inner());
            guard.push(buffered);
            if !self.upload_enabled() {
                false
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

        if let Err(err) = result {
            // Avoid tracing recursion from the upload path.
            eprintln!("bookclerk: diagnostics upload failed ({trigger}): {err}");
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
/// # Errors
///
/// Returns an error when serialization, redaction checks, or the HTTP request fails.
fn post_http_payload(
    config: &DiagnosticsConfig,
    payload: &UploadPayload,
) -> Result<String, String> {
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
    ureq::post(&url)
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
    Ok(url)
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

impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.handle.record_event(event);
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
                guard.push(BufferedEvent {
                    ts_unix_ms: unix_now_ms(),
                    level: "ERROR".into(),
                    target: "bookclerk::panic".into(),
                    message: redacted,
                    fields: vec![],
                });
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
    guard.push(BufferedEvent {
        ts_unix_ms: unix_now_ms(),
        level: level.into(),
        target: "test".into(),
        message: redact_field_value("message", message),
        fields: vec![],
    });
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
        let handle = DiagnosticsHandle::new(DiagnosticsConfig::default(), "0.0.0-test");
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
        );
        push_test_event(
            &handle,
            "ERROR",
            "refresh failed Atna|leak-me-please and Bearer abc.def.ghi",
        );
        if let Ok(mut guard) = handle.inner.ring.lock() {
            if let Some(ev) = guard.events.back_mut() {
                ev.fields
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
        );
        // Bypass message-field pattern path: push raw then sanitize on upload.
        if let Ok(mut guard) = handle.inner.ring.lock() {
            guard.push(BufferedEvent {
                ts_unix_ms: 1,
                level: "ERROR".into(),
                target: "test".into(),
                message: "auth used exact-config-passphrase-abc123".into(),
                fields: vec![],
            });
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
}
