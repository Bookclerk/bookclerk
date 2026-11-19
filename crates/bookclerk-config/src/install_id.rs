//! The install's random ID and the opt-in weekly usage check-in.
//!
//! With `diagnostics.share_usage` on, a host sends the collector one small
//! check-in a week (`POST /checkin`): a random install ID, the Bookclerk
//! version, the platform and the kind of process. The ID is generated from
//! the operating system's random source on first use, never derived from
//! hardware, accounts or the network, and kept in `install-id` under the
//! files directory; `bookclerk diagnostics reset-id` replaces it. The
//! collector keeps only a hash of the ID salted with a secret that changes
//! every month, so installs can be counted but not followed from one month to
//! the next. See `docs/server/diagnostics.md#usage-check-ins`.
//!
//! # Audience
//!
//! Host binaries. Guests never depend on this module.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bookclerk_tls::ureq;
use serde::Serialize;

use crate::settings::DiagnosticsConfig;

/// File under the files directory that holds the install ID.
pub const INSTALL_ID_FILE: &str = "install-id";

/// File under the files directory that holds when the last check-in was
/// accepted, as Unix milliseconds.
pub const CHECKIN_STAMP_FILE: &str = "install-checkin";

/// Time between check-ins: one week, in milliseconds.
pub const CHECKIN_INTERVAL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A random install ID: a version-4 UUID in lowercase hyphenated form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallId(String);

impl InstallId {
    /// Generates a new random ID from the operating system's random source.
    ///
    /// # Errors
    ///
    /// Returns an error when the random source is unavailable.
    pub fn generate() -> std::io::Result<Self> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        // RFC 9562 version 4, variant 10xx.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self(format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        )))
    }

    /// Parses an ID as [`InstallId::generate`] writes it.
    ///
    /// # Arguments
    ///
    /// * `text` - Candidate text; surrounding whitespace is ignored.
    ///
    /// # Returns
    ///
    /// The ID, or `None` unless the text is a lowercase hyphenated version-4
    /// UUID.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim();
        let bytes = t.as_bytes();
        if bytes.len() != 36 {
            return None;
        }
        for (i, b) in bytes.iter().enumerate() {
            let ok = match i {
                8 | 13 | 18 | 23 => *b == b'-',
                14 => *b == b'4',
                19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
                _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
            };
            if !ok {
                return None;
            }
        }
        Some(Self(t.to_string()))
    }

    /// The ID as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reads the ID from the files directory, creating it on first use (or
    /// when the file does not hold a valid ID).
    ///
    /// # Arguments
    ///
    /// * `files_dir` - The Bookclerk files directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or written.
    pub fn load_or_create(files_dir: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(id_path(files_dir)) {
            Ok(text) => match Self::parse(&text) {
                Some(id) => Ok(id),
                None => Self::reset(files_dir),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::reset(files_dir),
            Err(err) => Err(err),
        }
    }

    /// Replaces the ID with a new random one and forgets the last check-in,
    /// so the new ID checks in on the next pass.
    ///
    /// # Arguments
    ///
    /// * `files_dir` - The Bookclerk files directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub fn reset(files_dir: &Path) -> std::io::Result<Self> {
        let id = Self::generate()?;
        write_atomically(&id_path(files_dir), id.as_str())?;
        if let Err(err) = std::fs::remove_file(files_dir.join(CHECKIN_STAMP_FILE)) {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(err);
            }
        }
        Ok(id)
    }
}

/// Path of the ID file.
fn id_path(files_dir: &Path) -> PathBuf {
    files_dir.join(INSTALL_ID_FILE)
}

/// Writes `text` to `path` through a temporary file and an atomic rename.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    crate::replace_file(&tmp, path)
}

/// Body of `POST /checkin`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CheckIn {
    /// The install's random ID.
    pub install_id: String,
    /// Bookclerk version.
    pub version: String,
    /// Operating system (`linux`, `macos`, `windows`, …).
    pub os: String,
    /// CPU architecture (`x86_64`, `aarch64`, …).
    pub arch: String,
    /// Kind of process: `daemon` or `desktop`.
    pub kind: String,
}

/// Whether a check-in is due.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `now_ms` - Current Unix time in milliseconds.
///
/// # Returns
///
/// True when no check-in was recorded, the record is unreadable, or the last
/// one is at least [`CHECKIN_INTERVAL_MS`] old (or in the future, after a
/// clock change).
#[must_use]
pub fn checkin_due(files_dir: &Path, now_ms: u64) -> bool {
    let Ok(text) = std::fs::read_to_string(files_dir.join(CHECKIN_STAMP_FILE)) else {
        return true;
    };
    let Ok(last) = text.trim().parse::<u64>() else {
        return true;
    };
    last > now_ms || now_ms - last >= CHECKIN_INTERVAL_MS
}

/// Records an accepted check-in.
///
/// # Arguments
///
/// * `files_dir` - The Bookclerk files directory.
/// * `now_ms` - When it was accepted, Unix milliseconds.
///
/// # Errors
///
/// Returns an error when the record cannot be written.
pub fn record_checkin(files_dir: &Path, now_ms: u64) -> std::io::Result<()> {
    write_atomically(&files_dir.join(CHECKIN_STAMP_FILE), &now_ms.to_string())
}

/// Sends one check-in to the collector.
///
/// # Arguments
///
/// * `config` - Diagnostics settings (collector URL).
/// * `checkin` - The body.
///
/// # Errors
///
/// Returns an error when no collector URL is configured, the body cannot be
/// serialized, or the request fails.
pub fn send_checkin(config: &DiagnosticsConfig, checkin: &CheckIn) -> Result<(), String> {
    let base = config.effective_collector_url();
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        return Err("diagnostics collector_url is empty".into());
    }
    let body = serde_json::to_string(checkin).map_err(|e| e.to_string())?;
    ureq::post(&format!("{base}/checkin"))
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            &format!("bookclerk-checkin/{}", checkin.version),
        )
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .send(body.as_str())
        .map_err(|e| format!("usage check-in failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_parse_and_differ() {
        let a = InstallId::generate().unwrap();
        let b = InstallId::generate().unwrap();
        assert_ne!(a, b);
        assert_eq!(InstallId::parse(a.as_str()), Some(a.clone()));
        assert_eq!(a.as_str().len(), 36);
        assert_eq!(&a.as_str()[14..15], "4");
    }

    #[test]
    fn parse_refuses_other_shapes() {
        for bad in [
            "",
            "not-a-uuid",
            "0e9ced79-df3c-1dae-872b-2e5772b06e69",
            "0E9CED79-DF3C-4DAE-872B-2E5772B06E69",
            "0e9ced79-df3c-4dae-c72b-2e5772b06e69",
            "0e9ced79df3c4dae872b2e5772b06e69aaaa",
        ] {
            assert_eq!(InstallId::parse(bad), None, "{bad}");
        }
        assert!(InstallId::parse(" 0e9ced79-df3c-4dae-872b-2e5772b06e69\n").is_some());
    }

    #[test]
    fn load_or_create_keeps_the_id_and_reset_replaces_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = InstallId::load_or_create(dir.path()).unwrap();
        assert_eq!(InstallId::load_or_create(dir.path()).unwrap(), first);
        record_checkin(dir.path(), 1_000).unwrap();
        let second = InstallId::reset(dir.path()).unwrap();
        assert_ne!(second, first);
        assert_eq!(InstallId::load_or_create(dir.path()).unwrap(), second);
        assert!(
            checkin_due(dir.path(), 1_000),
            "reset forgets the last check-in"
        );
    }

    #[test]
    fn an_unreadable_id_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(INSTALL_ID_FILE), "garbage").unwrap();
        let id = InstallId::load_or_create(dir.path()).unwrap();
        assert!(InstallId::parse(id.as_str()).is_some());
    }

    #[test]
    fn checkin_is_due_weekly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(checkin_due(dir.path(), 10));
        record_checkin(dir.path(), 10).unwrap();
        assert!(!checkin_due(dir.path(), 10 + CHECKIN_INTERVAL_MS - 1));
        assert!(checkin_due(dir.path(), 10 + CHECKIN_INTERVAL_MS));
        assert!(
            checkin_due(dir.path(), 5),
            "a stamp in the future counts as due"
        );
        std::fs::write(dir.path().join(CHECKIN_STAMP_FILE), "x").unwrap();
        assert!(checkin_due(dir.path(), 10));
    }

    #[test]
    fn send_checkin_posts_to_the_collector() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0_u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + len || n == 0 {
                        break;
                    }
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            String::from_utf8_lossy(&raw).into_owned()
        });
        let config = DiagnosticsConfig {
            collector_url: format!("http://{addr}/"),
            ..DiagnosticsConfig::default()
        };
        let checkin = CheckIn {
            install_id: "0e9ced79-df3c-4dae-872b-2e5772b06e69".into(),
            version: "1.2.3".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            kind: "daemon".into(),
        };
        send_checkin(&config, &checkin).unwrap();
        let raw = server.join().unwrap();
        assert!(raw.starts_with("POST /checkin "), "{raw}");
        assert!(raw.contains(r#""install_id":"0e9ced79-df3c-4dae-872b-2e5772b06e69""#));
        assert!(raw.contains(r#""kind":"daemon""#));
    }

    #[test]
    fn send_checkin_needs_a_collector() {
        let config = DiagnosticsConfig::default();
        let checkin = CheckIn {
            install_id: String::new(),
            version: String::new(),
            os: String::new(),
            arch: String::new(),
            kind: String::new(),
        };
        // Builds without a baked collector URL have none to send to.
        if config.effective_collector_url().is_empty() {
            assert!(send_checkin(&config, &checkin).is_err());
        }
    }
}
