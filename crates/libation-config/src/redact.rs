//! Secret / sensitive-data redaction for log sinks and diagnostics uploads.
//!
//! Redaction applies to **every** sink (stderr, journald, upload payloads). It is
//! not configurable — secrets must never leave the process in cleartext logs.
//!
//! Strategy (strongest first):
//! 1. **Exact values** registered from config/env/auth (passwords, OAuth tokens, AWS keys)
//! 2. Sensitive **field-name** denylist
//! 3. **Pattern** matching for shapes we know even when not yet registered

use std::collections::BTreeSet;
use std::fmt::{self, Write as _};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use tracing::field::{Field, Visit};

/// Replacement token used wherever a secret or sensitive value is scrubbed.
pub const REDACTED: &str = "[REDACTED]";

/// Minimum length for exact-value registration (avoids wiping short common strings).
const MIN_SECRET_LEN: usize = 6;

fn exact_secrets() -> &'static Mutex<BTreeSet<String>> {
    static CELL: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Register a known secret so it is always scrubbed from logs and diagnostics.
///
/// Call this whenever a passphrase, OAuth token, AWS key, etc. enters process
/// memory. Values shorter than 6 characters are ignored.
///
/// Also registers common encodings (percent-encoding, `+` for spaces) so a secret
/// cannot slip through via URL/query embedding.
pub fn register_secret(value: impl AsRef<str>) {
    let trimmed = value.as_ref().trim();
    if trimmed.len() < MIN_SECRET_LEN {
        return;
    }
    let Ok(mut guard) = exact_secrets().lock() else {
        return;
    };
    insert_secret_variants(&mut guard, trimmed);
}

fn insert_secret_variants(guard: &mut BTreeSet<String>, raw: &str) {
    guard.insert(raw.to_string());
    // Percent-encode (keep unreserved chars) — catches query-string embedding.
    let pct = percent_encode_minimal(raw);
    if pct.len() >= MIN_SECRET_LEN && pct != raw {
        guard.insert(pct);
    }
    // Form-encoding often uses + for spaces.
    if raw.contains(' ') {
        let plus = raw.replace(' ', "+");
        if plus.len() >= MIN_SECRET_LEN {
            guard.insert(plus.clone());
            let pct_plus = percent_encode_minimal(&plus);
            if pct_plus.len() >= MIN_SECRET_LEN {
                guard.insert(pct_plus);
            }
        }
    }
}

fn percent_encode_minimal(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// True if `haystack` still contains any registered exact secret (post-redaction check).
#[must_use]
pub fn contains_registered_secret(haystack: &str) -> bool {
    let Ok(guard) = exact_secrets().lock() else {
        return false;
    };
    guard.iter().any(|s| haystack.contains(s.as_str()))
}

/// Register several secrets at once.
pub fn register_secrets<I, S>(values: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for v in values {
        register_secret(v);
    }
}

/// Register secrets commonly present in the process environment / config.
///
/// Safe to call multiple times. Does not log values.
pub fn register_secrets_from_env() {
    const KEYS: &[&str] = &[
        "LIBATION_AUTH_PASSWORD",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "LIBATION_DIAGNOSTICS_GITHUB_TOKEN",
        "GITHUB_TOKEN",
    ];
    for key in KEYS {
        if let Ok(v) = std::env::var(key) {
            register_secret(v);
        }
    }
}

/// Test helper: clear the exact-secret registry.
#[cfg(test)]
pub fn clear_registered_secrets() {
    if let Ok(mut guard) = exact_secrets().lock() {
        guard.clear();
    }
}

fn redact_exact_values(input: &str) -> String {
    let Ok(guard) = exact_secrets().lock() else {
        return input.to_string();
    };
    if guard.is_empty() {
        return input.to_string();
    }
    // Longest first so a token that is a prefix of another still fully redacts.
    let mut secrets: Vec<&str> = guard.iter().map(String::as_str).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = input.to_string();
    for secret in secrets {
        if out.contains(secret) {
            out = out.replace(secret, REDACTED);
        }
    }
    out
}

/// Returns true when a tracing field name should never be logged in cleartext.
#[must_use]
pub fn is_sensitive_field(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(
        n.as_str(),
        "password"
            | "passwd"
            | "passphrase"
            | "secret"
            | "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "authorization"
            | "cookie"
            | "set_cookie"
            | "api_key"
            | "apikey"
            | "client_secret"
            | "private_key"
            | "aws_secret_access_key"
            | "aws_access_key_id"
            | "secret_access_key"
            | "access_key"
            | "otp"
            | "totp"
            | "mfa_code"
            | "adrm_key"
            | "aax_key"
            | "voucher"
            | "license"
            | "license_response"
            | "widevine_key"
            | "widevine"
            | "cdm"
            | "wvd"
            | "github_token"
            | "gh_token"
            | "adp_token"
            | "device_private_key"
    ) || n.contains("password")
        || n.contains("passwd")
        || n.contains("secret")
        || n.ends_with("_token")
        || n.ends_with("_key")
        || n.contains("authorization")
        || n.contains("cookie")
        || n.contains("voucher")
        || n.contains("license")
}

/// Field names scrubbed from **remote** diagnostics uploads
/// in addition to [`is_sensitive_field`]. Local journal/stderr may still show them.
#[must_use]
pub fn is_upload_identifying_field(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(
        n.as_str(),
        "title"
            | "subtitle"
            | "author"
            | "authors"
            | "narrator"
            | "narrators"
            | "path"
            | "dest"
            | "password_file"
            | "account"
            | "email"
            | "username"
            | "user"
    ) || n.contains("password_file")
        || n.ends_with("_path")
}

/// Scrub known secrets: exact registered values, then pattern matches.
#[must_use]
pub fn redact_str(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = redact_exact_values(input);
    for re in secret_patterns() {
        out = re.replace_all(&out, REDACTED).into_owned();
    }
    out
}

/// Redact a field value: sensitive names are fully replaced; others are scrubbed.
#[must_use]
pub fn redact_field_value(name: &str, value: &str) -> String {
    if is_sensitive_field(name) {
        REDACTED.to_string()
    } else {
        redact_str(value)
    }
}

/// Extra sanitization for payloads that leave the machine (collector / B2).
///
/// Emails are **partially** masked (see [`mask_email`]) so triage can still see
/// rough account shape without shipping a clear address. Other identifying
/// fields (title, path, account, …) remain fully [`REDACTED`].
#[must_use]
pub fn sanitize_for_remote_upload(name: &str, value: &str) -> String {
    if is_sensitive_field(name) {
        return REDACTED.to_string();
    }
    let n = name.trim().to_ascii_lowercase();
    if n == "email" {
        return mask_email(value);
    }
    if is_upload_identifying_field(name) {
        return REDACTED.to_string();
    }
    let mut out = redact_str(value);
    out = redact_emails_in_text(&out);
    out = redact_home_paths(&out);
    out = redact_auth_paths(&out);
    truncate_for_upload(&out, MAX_UPLOAD_FIELD_CHARS)
}

/// Partial email mask for remote uploads.
///
/// Example: `address@sub.domain.tld` → `a*****s@***.d****n.tld`
///
/// - Local part: keep first and last character; middle replaced with `*`.
/// - Domain: fully mask every label except the registrable name (second-to-last)
///   and the final TLD. The registrable label keeps ends; the TLD is unchanged.
/// - Values that are not `local@domain` become [`REDACTED`].
#[must_use]
pub fn mask_email(email: &str) -> String {
    let trimmed = email.trim();
    let Some((local, domain)) = trimmed.split_once('@') else {
        return REDACTED.to_string();
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return REDACTED.to_string();
    }
    let labels: Vec<&str> = domain.split('.').filter(|l| !l.is_empty()).collect();
    if labels.is_empty() {
        return REDACTED.to_string();
    }
    format!("{}@{}", mask_keep_ends(local), mask_domain_labels(&labels))
}

fn mask_keep_ends(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    match chars.len() {
        0 => String::new(),
        1 => "*".to_string(),
        2 => format!("{}*", chars[0]),
        n => {
            let middle = "*".repeat(n - 2);
            format!("{}{}{}", chars[0], middle, chars[n - 1])
        }
    }
}

fn mask_domain_labels(labels: &[&str]) -> String {
    match labels.len() {
        0 => REDACTED.to_string(),
        1 => mask_keep_ends(labels[0]),
        2 => format!("{}.{}", mask_keep_ends(labels[0]), labels[1]),
        n => {
            let mut parts: Vec<String> =
                labels[..n - 2].iter().map(|_| "***".to_string()).collect();
            parts.push(mask_keep_ends(labels[n - 2]));
            parts.push(labels[n - 1].to_string());
            parts.join(".")
        }
    }
}

fn redact_emails_in_text(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)\b[A-Z0-9._%+\-]+@[A-Z0-9.\-]+\.[A-Z]{2,}\b").expect("email regex")
    });
    re.replace_all(input, |caps: &regex::Captures| mask_email(&caps[0]))
        .into_owned()
}

const MAX_UPLOAD_FIELD_CHARS: usize = 2_000;
const MAX_UPLOAD_MESSAGE_CHARS: usize = 4_000;

fn truncate_for_upload(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let truncated: String = input.chars().take(max_chars).collect();
    format!("{truncated}…")
}

/// Cap message length used in remote diagnostics events.
#[must_use]
pub fn truncate_upload_message(message: &str) -> String {
    truncate_for_upload(message, MAX_UPLOAD_MESSAGE_CHARS)
}

fn redact_home_paths(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)(/home/|/Users/|\\Users\\)([^/\\]+)").expect("home path regex")
    });
    re.replace_all(input, |caps: &regex::Captures| {
        format!("{}{}", &caps[1], REDACTED)
    })
    .into_owned()
}

fn redact_auth_paths(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)(Accounts[/\\])([^/\\]+)(\.auth|\.wvd)").expect("auth path regex")
    });
    re.replace_all(input, |caps: &regex::Captures| {
        format!("{}{}{}", &caps[1], REDACTED, &caps[3])
    })
    .into_owned()
}

fn secret_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        const SOURCES: &[&str] = &[
            r"(?i)\bAtna\|[A-Za-z0-9._\-+/=]+",
            r"(?i)\bAtnr\|[A-Za-z0-9._\-+/=]+",
            r"(?i)\bBearer\s+[A-Za-z0-9._\-+/=]+",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"(?i)(aws_secret_access_key\s*[=:]\s*)\S+",
            r"\bgh[pousr]_[A-Za-z0-9_]{20,}\b",
            r"\bgithub_pat_[A-Za-z0-9_]{20,}\b",
            r"(?i)([?&](?:password|passwd|token|access_token|refresh_token|api_key|key|secret)=)[^&\s]+",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            r"(?i)(otp|totp|mfa)([=: ])\S+",
        ];
        SOURCES
            .iter()
            .map(|s| Regex::new(s).expect("static redaction regex"))
            .collect()
    })
}

/// Collects event fields into a map while applying redaction.
#[derive(Default)]
pub struct RedactingVisitor {
    pub message: Option<String>,
    pub fields: Vec<(String, String)>,
}

impl Visit for RedactingVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_display(field, &value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_display(field, &value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_display(field, &value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_display(field, &value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let name = field.name();
        let redacted = redact_field_value(name, value);
        if name == "message" {
            self.message = Some(redacted);
        } else {
            self.fields.push((name.to_string(), redacted));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let mut raw = String::new();
        let _ = write!(raw, "{value:?}");
        let name = field.name();
        let redacted = redact_field_value(name, &raw);
        if name == "message" {
            self.message = Some(redacted);
        } else {
            self.fields.push((name.to_string(), redacted));
        }
    }
}

impl RedactingVisitor {
    fn record_display(&mut self, field: &Field, value: &dyn fmt::Display) {
        let raw = value.to_string();
        let name = field.name();
        let redacted = redact_field_value(name, &raw);
        if name == "message" {
            self.message = Some(redacted);
        } else {
            self.fields.push((name.to_string(), redacted));
        }
    }
}

/// [`std::io::Write`] wrapper that runs [`redact_str`] before forwarding.
///
/// Bytes are buffered until a newline (or flush/drop) so a secret split across
/// multiple `write()` calls is still scrubbed as one unit. Pending data is also
/// flushed if the buffer exceeds [`MAX_PENDING_LINE`] to bound memory.
pub struct RedactingWriter<W: std::io::Write> {
    inner: W,
    pending: Vec<u8>,
}

/// Cap for incomplete-line buffering (tracing events are usually one short line).
const MAX_PENDING_LINE: usize = 64 * 1024;

impl<W: std::io::Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }

    fn write_redacted_chunk(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        match std::str::from_utf8(chunk) {
            Ok(s) => self.inner.write_all(redact_str(s).as_bytes()),
            Err(_) => self.inner.write_all(REDACTED.as_bytes()),
        }
    }

    fn flush_pending(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.pending);
        self.write_redacted_chunk(&chunk)
    }

    fn drain_complete_lines(&mut self) -> std::io::Result<()> {
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=nl).collect();
            self.write_redacted_chunk(&line)?;
        }
        if self.pending.len() > MAX_PENDING_LINE {
            self.flush_pending()?;
        }
        Ok(())
    }
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let len = buf.len();
        self.pending.extend_from_slice(buf);
        self.drain_complete_lines()?;
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_pending()?;
        self.inner.flush()
    }
}

impl<W: std::io::Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        let _ = self.flush_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_field_names() {
        assert!(is_sensitive_field("password"));
        assert!(is_sensitive_field("refresh_token"));
        assert!(is_sensitive_field("AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive_field("client_secret"));
        assert!(!is_sensitive_field("asin"));
        assert!(!is_sensitive_field("title"));
    }

    #[test]
    fn exact_registered_secret_is_redacted() {
        clear_registered_secrets();
        register_secret("super-unique-passphrase-xyz");
        let out = redact_str("using super-unique-passphrase-xyz for auth");
        assert!(!out.contains("super-unique-passphrase-xyz"));
        assert!(out.contains(REDACTED));
        clear_registered_secrets();
    }

    #[test]
    fn percent_encoded_secret_is_also_redacted() {
        clear_registered_secrets();
        register_secret("p@ss word/leak");
        let encoded = "p%40ss%20word%2Fleak";
        let out = redact_str(&format!("q={encoded}"));
        assert!(!out.to_ascii_lowercase().contains("p%40ss"));
        assert!(out.contains(REDACTED));
        clear_registered_secrets();
    }

    #[test]
    fn contains_registered_secret_detects_leaks() {
        clear_registered_secrets();
        register_secret("detect-me-secret-value");
        assert!(contains_registered_secret("x detect-me-secret-value y"));
        assert!(!contains_registered_secret(&redact_str(
            "x detect-me-secret-value y"
        )));
        clear_registered_secrets();
    }

    #[test]
    fn redacts_audible_tokens_in_message() {
        let s = "login failed token=Atna|abcDEF123 refresh=Atnr|xyz";
        let out = redact_str(s);
        assert!(!out.contains("Atna|abcDEF123"));
        assert!(!out.contains("Atnr|xyz"));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn redacts_bearer_and_aws() {
        let s = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.xx AWS=AKIAIOSFODNN7EXAMPLE";
        let out = redact_str(s);
        assert!(!out.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn sensitive_field_fully_redacted() {
        assert_eq!(
            redact_field_value("refresh_token", "Atnr|not-a-real-token"),
            REDACTED
        );
        assert_eq!(
            redact_field_value("asin", "B00TEST123"),
            "B00TEST123".to_string()
        );
    }

    #[test]
    fn writer_scrubs_secrets() {
        let mut buf = Vec::new();
        {
            let mut w = RedactingWriter::new(&mut buf);
            use std::io::Write;
            writeln!(w, "got Bearer super-secret-token-value ok").unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("super-secret-token-value"));
        assert!(s.contains(REDACTED));
    }

    #[test]
    fn writer_scrubs_secret_split_across_writes() {
        clear_registered_secrets();
        register_secret("split-secret-value-xyz");
        let mut buf = Vec::new();
        {
            let mut w = RedactingWriter::new(&mut buf);
            use std::io::Write;
            w.write_all(b"prefix split-secret").unwrap();
            w.write_all(b"-value-xyz suffix\n").unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("split-secret-value-xyz"));
        assert!(s.contains(REDACTED));
        clear_registered_secrets();
    }

    #[test]
    fn sanitize_strips_titles_and_home_paths() {
        assert_eq!(sanitize_for_remote_upload("title", "My Book"), REDACTED);
        let path = sanitize_for_remote_upload("message", "/home/alice/Accounts/bob.auth");
        assert!(!path.contains("alice"));
        assert!(path.contains(REDACTED));
    }

    #[test]
    fn mask_email_matches_documented_shape() {
        assert_eq!(
            mask_email("address@sub.domain.tld"),
            "a*****s@***.d****n.tld"
        );
        assert_eq!(mask_email("user@example.com"), "u**r@e*****e.com");
        assert_eq!(mask_email("ab@cd.io"), "a*@c*.io");
        assert_eq!(mask_email("a@b.co"), "*@*.co");
        assert_eq!(mask_email("not-an-email"), REDACTED);
        assert_eq!(mask_email("missing-domain@"), REDACTED);
    }

    #[test]
    fn sanitize_partially_masks_email_field_and_inline() {
        assert_eq!(
            sanitize_for_remote_upload("email", "address@sub.domain.tld"),
            "a*****s@***.d****n.tld"
        );
        let msg = sanitize_for_remote_upload("message", "login failed for address@sub.domain.tld");
        assert!(msg.contains("a*****s@***.d****n.tld"));
        assert!(!msg.contains("address@sub.domain.tld"));
        // Non-email identifying fields stay fully redacted.
        assert_eq!(
            sanitize_for_remote_upload("account", "alice@example.com"),
            REDACTED
        );
    }
}
