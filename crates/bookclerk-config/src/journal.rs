//! OS log facility sinks — journald (Linux), os_log (macOS), Event Log (Windows).
//!
//! Bookclerk does **not** manage log files or rotation; the OS facility owns retention.

#![allow(unsafe_code)] // Windows Event Log Register/Report/DeregisterEventSource FFI

use std::fmt::Write as _;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Write;

use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use crate::redact::RedactingVisitor;

#[cfg(target_os = "linux")]
/// systemd journal datagram socket path (`/run/systemd/journal/socket`).
const JOURNALD_PATH: &str = "/run/systemd/journal/socket";

/// Which OS facility was attached (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsLogFacility {
    /// Linux systemd journal (`/run/systemd/journal/socket`).
    Journald,
    /// Apple Unified Logging (`os_log`).
    OsLog,
    /// Windows Event Log Application channel.
    EventLog,
}

/// Structured sink that writes **redacted** events to the platform log facility.
pub struct OsLogLayer {
    /// Platform backend chosen at construction (journald, os_log, or Event Log).
    inner: OsLogInner,
    /// Journald `SYSLOG_IDENTIFIER`; also included in Event Log / os_log lines.
    syslog_identifier: String,
}

/// Concrete OS log handle; one variant is compiled in per target.
enum OsLogInner {
    #[cfg(target_os = "linux")]
    /// Linux systemd journal via a connected datagram socket.
    Journald {
        #[cfg(unix)]
        /// Unbound datagram socket used to `send_to` the journal socket.
        socket: std::os::unix::net::UnixDatagram,
    },
    #[cfg(target_os = "macos")]
    /// Unified Logging (`os_log`) backend for macOS.
    OsLog {
        /// Logger handle used to emit structured OS log records.
        logger: oslog::OsLog,
    },
    #[cfg(windows)]
    /// Windows Event Log backend.
    EventLog {
        /// Registered Event Log source handle.
        handle: EventLogHandle,
    },
}

/// Newtype so the Event Log `HANDLE` (`*mut c_void`) can sit in a tracing
/// `Layer` (requires `Send + Sync`). Reporting from other threads is supported
/// by the Win32 Event Log API for a registered source handle.
#[cfg(windows)]
struct EventLogHandle(
    /// Raw Win32 Event Log source `HANDLE`.
    windows_sys::Win32::Foundation::HANDLE,
);

#[cfg(windows)]
// SAFETY: `ReportEventW` / `DeregisterEventSource` accept a source HANDLE from
// any thread; we only deregister once in `Drop`.
unsafe impl Send for EventLogHandle {}
#[cfg(windows)]
unsafe impl Sync for EventLogHandle {}

impl OsLogLayer {
    /// Connect to the platform facility. Returns `Err` when unavailable.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn new(syslog_identifier: impl Into<String>) -> io::Result<Self> {
        let syslog_identifier = syslog_identifier.into();
        let inner = open_inner(&syslog_identifier)?;
        Ok(Self {
            inner,
            syslog_identifier,
        })
    }

    /// Which platform sink this layer is writing to.
    #[must_use]
    pub fn facility(&self) -> OsLogFacility {
        match &self.inner {
            #[cfg(target_os = "linux")]
            OsLogInner::Journald { .. } => OsLogFacility::Journald,
            #[cfg(target_os = "macos")]
            OsLogInner::OsLog { .. } => OsLogFacility::OsLog,
            #[cfg(windows)]
            OsLogInner::EventLog { .. } => OsLogFacility::EventLog,
        }
    }
}

/// Opens the platform facility, or returns `Unsupported` on other OSes.
fn open_inner(identifier: &str) -> io::Result<OsLogInner> {
    #[cfg(target_os = "linux")]
    {
        let _ = identifier;
        use std::os::unix::net::UnixDatagram;
        let socket = UnixDatagram::unbound()?;
        // Empty payload probes reachability; journald discards it.
        socket.send_to(&[], JOURNALD_PATH)?;
        Ok(OsLogInner::Journald { socket })
    }
    #[cfg(target_os = "macos")]
    {
        let logger = oslog::OsLog::new("dev.bookclerk", identifier);
        Ok(OsLogInner::OsLog { logger })
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::EventLog::RegisterEventSourceW;
        let wide: Vec<u16> = std::ffi::OsStr::new(identifier)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: identifier is a NUL-terminated wide string; server name is null (local).
        let handle = unsafe { RegisterEventSourceW(std::ptr::null(), wide.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(OsLogInner::EventLog {
            handle: EventLogHandle(handle),
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = identifier;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no OS log facility on this platform",
        ))
    }
}

#[cfg(windows)]
impl Drop for OsLogLayer {
    fn drop(&mut self) {
        let OsLogInner::EventLog { handle } = &self.inner;
        // SAFETY: handle from RegisterEventSourceW; dropped once.
        unsafe {
            windows_sys::Win32::System::EventLog::DeregisterEventSource(handle.0);
        }
    }
}

impl<S> Layer<S> for OsLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = RedactingVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message.unwrap_or_default();
        let mut composed = message.clone();
        for (name, value) in &visitor.fields {
            let _ = write!(composed, " {name}={value}");
        }

        match &self.inner {
            #[cfg(target_os = "linux")]
            OsLogInner::Journald { socket } => {
                let mut buf = Vec::with_capacity(256);
                put_priority_ascii(&mut buf, meta.level());
                put_wellformed(&mut buf, "TARGET", meta.target().as_bytes());
                put_length_encoded(&mut buf, "SYSLOG_IDENTIFIER", |b| {
                    let _ = write!(b, "{}", self.syslog_identifier);
                });
                if let Some(file) = meta.file() {
                    put_wellformed(&mut buf, "CODE_FILE", file.as_bytes());
                }
                if let Some(line) = meta.line() {
                    put_wellformed(&mut buf, "CODE_LINE", line.to_string().as_bytes());
                }
                put_length_encoded(&mut buf, "MESSAGE", |b| {
                    b.extend_from_slice(message.as_bytes());
                });
                for (name, value) in visitor.fields {
                    let field_name = if name.eq_ignore_ascii_case("message") {
                        "MESSAGE".to_string()
                    } else {
                        format!("F_{}", name)
                    };
                    put_length_encoded(&mut buf, &field_name, |b| {
                        b.extend_from_slice(value.as_bytes());
                    });
                }
                let _ = socket.send_to(&buf, JOURNALD_PATH);
            }
            #[cfg(target_os = "macos")]
            OsLogInner::OsLog { logger } => {
                let level = *meta.level();
                let text = format!(
                    "{}: [{}] {}",
                    self.syslog_identifier,
                    meta.target(),
                    composed
                );
                if level <= tracing::Level::ERROR {
                    logger.fault(&text);
                } else if level <= tracing::Level::WARN {
                    logger.error(&text);
                } else if level <= tracing::Level::INFO {
                    logger.default(&text);
                } else if level <= tracing::Level::DEBUG {
                    logger.info(&text);
                } else {
                    logger.debug(&text);
                }
            }
            #[cfg(windows)]
            OsLogInner::EventLog { handle } => {
                use std::os::windows::ffi::OsStrExt;
                use windows_sys::Win32::System::EventLog::{
                    ReportEventW, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE,
                    EVENTLOG_WARNING_TYPE,
                };
                let etype = match *meta.level() {
                    tracing::Level::ERROR => EVENTLOG_ERROR_TYPE,
                    tracing::Level::WARN => EVENTLOG_WARNING_TYPE,
                    _ => EVENTLOG_INFORMATION_TYPE,
                };
                let text = format!(
                    "{}: [{}] {}",
                    self.syslog_identifier,
                    meta.target(),
                    composed
                );
                let wide: Vec<u16> = std::ffi::OsStr::new(&text)
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                let ptrs = [wide.as_ptr()];
                // SAFETY: handle is live; strings pointer is valid for the call.
                unsafe {
                    ReportEventW(
                        handle.0,
                        etype,
                        0,
                        level_as_event_id(meta.level()),
                        std::ptr::null_mut(),
                        1,
                        0,
                        ptrs.as_ptr(),
                        std::ptr::null(),
                    );
                }
            }
        }
    }
}

#[cfg(windows)]
/// Maps a tracing level to a Windows Event Log event identifier.
fn level_as_event_id(level: &tracing::Level) -> u32 {
    match *level {
        tracing::Level::ERROR => 1,
        tracing::Level::WARN => 2,
        tracing::Level::INFO => 3,
        tracing::Level::DEBUG => 4,
        tracing::Level::TRACE => 5,
    }
}

#[cfg(target_os = "linux")]
/// Writes journald `PRIORITY` (3–7) from a tracing level.
fn put_priority_ascii(buf: &mut Vec<u8>, level: &tracing::Level) {
    let code: u8 = match *level {
        tracing::Level::ERROR => b'3',
        tracing::Level::WARN => b'4',
        tracing::Level::INFO => b'5',
        tracing::Level::DEBUG => b'6',
        tracing::Level::TRACE => b'7',
    };
    put_wellformed(buf, "PRIORITY", &[code]);
}

#[cfg(target_os = "linux")]
/// Appends a journald field in the native length-prefixed binary form.
fn put_wellformed(buf: &mut Vec<u8>, name: &str, value: &[u8]) {
    buf.extend_from_slice(name.as_bytes());
    buf.push(b'\n');
    buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
    buf.extend_from_slice(value);
    buf.push(b'\n');
}

#[cfg(target_os = "linux")]
/// Appends a journald field after sanitizing `name`, then length-prefixes the value.
fn put_length_encoded(buf: &mut Vec<u8>, name: &str, write_value: impl FnOnce(&mut Vec<u8>)) {
    sanitize_name(name, buf);
    buf.push(b'\n');
    buf.extend_from_slice(&[0; 8]);
    let start = buf.len();
    write_value(buf);
    let end = buf.len();
    buf[start - 8..start].copy_from_slice(&((end - start) as u64).to_le_bytes());
    buf.push(b'\n');
}

#[cfg(target_os = "linux")]
/// Uppercases a journald field name and strips characters journald would reject.
fn sanitize_name(name: &str, buf: &mut Vec<u8>) {
    buf.extend(
        name.bytes()
            .map(|c| if c == b'.' { b'_' } else { c })
            .skip_while(|&c| c == b'_')
            .filter(|&c| c == b'_' || char::from(c).is_ascii_alphanumeric())
            .map(|c| char::from(c).to_ascii_uppercase() as u8),
    );
}

/// Probe whether an OS log facility appears available.
#[must_use]
pub fn os_log_available() -> bool {
    OsLogLayer::new("bookclerk-probe").is_ok()
}

/// Back-compat alias.
#[must_use]
pub fn journald_available() -> bool {
    os_log_available()
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn sanitize_uppercases_and_strips() {
        let mut buf = Vec::new();
        super::sanitize_name("foo.bar-baz", &mut buf);
        assert_eq!(std::str::from_utf8(&buf).unwrap(), "FOO_BARBAZ");
    }
}
