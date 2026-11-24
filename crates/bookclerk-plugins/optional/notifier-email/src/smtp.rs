//! SMTP submission, with no I/O of its own.
//!
//! Everything the notifier reads from outside is parsed here: the server's
//! replies (RFC 5321 section 4.2), its EHLO capabilities, a patron's address
//! document and the operator's addresses. The message is built here too
//! (RFC 5322 headers, MIME `multipart/alternative` with quoted-printable
//! text, `multipart/related` for inline images, RFC 2047 encoded words), and [`Session`] is the client's state machine:
//! it is fed one reply at a time and answers with the next thing to write, a
//! TLS upgrade, or the end. [`crate::client`] only moves bytes, so the unit
//! tests and the fuzzer drive the same code the guest runs.
//!
//! No error text carries an address, a credential, the message or the
//! server's own reply text: those may be personal or secret. Errors name the
//! step and the reply code only.

use std::collections::BTreeSet;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use bookclerk_plugin_sdk::{PluginError, PluginErrorCode};

/// Longest reply line accepted, in bytes (RFC 5321 allows 512; servers run
/// longer).
pub const MAX_REPLY_LINE_BYTES: usize = 2048;

/// Most lines one reply may have.
pub const MAX_REPLY_LINES: usize = 128;

/// Longest address accepted, in characters (RFC 5321's path limit less the
/// brackets).
pub const MAX_EMAIL_CHARS: usize = 254;

/// Longest local part accepted, in characters (RFC 5321 section 4.5.3.1.1).
pub const MAX_LOCAL_PART_CHARS: usize = 64;

/// Longest address document accepted, in bytes (the ABI's
/// `maxNotifierAddressBytes`).
pub const MAX_ADDRESS_BYTES: usize = bookclerk_plugin_sdk::MAX_NOTIFIER_ADDRESS_BYTES as usize;

/// Most recipients the `to` setting may list.
pub const MAX_CHANNEL_RECIPIENTS: usize = 10;

/// Longest display name kept in `From`, in characters.
pub const MAX_FROM_NAME_CHARS: usize = 128;

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// Plain at first, upgraded by `STARTTLS` before anything else is sent
    /// (usually port 587). TLS is required: a server that does not offer
    /// it gets nothing.
    StartTls,
    /// TLS from the first byte (usually port 465).
    Tls,
    /// No TLS, for a relay on a network the operator trusts; never with
    /// credentials.
    None,
}

impl Security {
    /// Parses the `security` setting.
    ///
    /// # Errors
    ///
    /// `invalid_params` for anything but `starttls`, `tls` or `none`.
    pub fn parse(value: &str) -> Result<Self, PluginError> {
        match value {
            "starttls" => Ok(Self::StartTls),
            "tls" => Ok(Self::Tls),
            "none" => Ok(Self::None),
            _ => Err(invalid("the security setting is not starttls, tls or none")),
        }
    }
}

/// An `invalid_params` failure.
///
/// # Arguments
///
/// * `message` - What is wrong, without the value.
fn invalid(message: impl Into<String>) -> PluginError {
    PluginError::invalid_params(message)
}

/// An `unavailable` failure: retried by the host.
///
/// # Arguments
///
/// * `message` - What went wrong, without reply text.
fn unavailable(message: impl Into<String>) -> PluginError {
    PluginError::unavailable(message)
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/// One complete server reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Three-digit reply code, 200 to 599.
    pub code: u16,
    /// Each line's text after the code and separator, in order (invalid
    /// UTF-8 replaced); at least one.
    pub lines: Vec<String>,
}

/// Takes the first complete reply off the front of what the server has sent.
///
/// Lines end in CRLF (a bare LF is accepted too). Every line of a reply has
/// the same code; all but the last put a hyphen after it. A line is a code
/// of `2`-`5` and two digits, then a space, a hyphen or nothing.
///
/// # Arguments
///
/// * `buffer` - Unread bytes from the server.
///
/// # Returns
///
/// The reply and how many bytes it took, or `None` while it is incomplete.
///
/// # Errors
///
/// `unavailable` when the bytes are not a reply, a line is longer than
/// [`MAX_REPLY_LINE_BYTES`], a reply mixes codes or has more than
/// [`MAX_REPLY_LINES`] lines.
pub fn read_reply(buffer: &[u8]) -> Result<Option<(Reply, usize)>, PluginError> {
    let mut lines = Vec::new();
    let mut code: Option<u16> = None;
    let mut at = 0;
    loop {
        let Some(offset) = buffer[at..].iter().position(|&b| b == b'\n') else {
            if buffer.len() - at > MAX_REPLY_LINE_BYTES {
                return Err(unavailable("the server sent a reply line that is too long"));
            }
            return Ok(None);
        };
        let end = at + offset;
        let mut line = &buffer[at..end];
        if let Some(stripped) = line.strip_suffix(b"\r") {
            line = stripped;
        }
        if line.len() > MAX_REPLY_LINE_BYTES {
            return Err(unavailable("the server sent a reply line that is too long"));
        }
        let (line_code, separator, text) = split_reply_line(line)
            .ok_or_else(|| unavailable("the server sent something that is not an SMTP reply"))?;
        if code.is_some_and(|code| code != line_code) {
            return Err(unavailable("the server sent a reply with mixed codes"));
        }
        code = Some(line_code);
        lines.push(String::from_utf8_lossy(text).into_owned());
        at = end + 1;
        if separator != Some(b'-') {
            return Ok(Some((
                Reply {
                    code: line_code,
                    lines,
                },
                at,
            )));
        }
        if lines.len() >= MAX_REPLY_LINES {
            return Err(unavailable("the server sent a reply with too many lines"));
        }
    }
}

/// Splits one reply line into its code, separator and text.
///
/// # Arguments
///
/// * `line` - One line without its line ending.
///
/// # Returns
///
/// The code, the separator (`b' '`, `b'-'`, or `None` for a bare code) and
/// the text, or `None` when the line is not a reply line.
fn split_reply_line(line: &[u8]) -> Option<(u16, Option<u8>, &[u8])> {
    let [a, b, c, rest @ ..] = line else {
        return None;
    };
    if !(b'2'..=b'5').contains(a) || !b.is_ascii_digit() || !c.is_ascii_digit() {
        return None;
    }
    let code = u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0');
    match rest {
        [] => Some((code, None, rest)),
        [sep @ (b' ' | b'-'), text @ ..] => Some((code, Some(*sep), text)),
        _ => None,
    }
}

/// What an EHLO reply says the server supports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `STARTTLS` is offered.
    pub starttls: bool,
    /// SASL mechanisms from `AUTH`, upper-case.
    pub auth: BTreeSet<String>,
    /// `SIZE` limit in bytes, when the server states one above zero.
    pub size: Option<u64>,
}

/// Reads the extensions an EHLO reply lists (RFC 5321 section 4.1.1.1). The
/// first line is the server's greeting; each later line is a keyword and its
/// parameters. Unknown keywords are ignored.
///
/// # Arguments
///
/// * `reply` - A 250 reply to EHLO.
#[must_use]
pub fn parse_ehlo(reply: &Reply) -> Capabilities {
    let mut caps = Capabilities::default();
    for line in reply.lines.iter().skip(1) {
        // `AUTH=LOGIN PLAIN` is an old spelling some servers still send.
        let mut words = line.split([' ', '=']).filter(|w| !w.is_empty());
        let keyword = words.next().unwrap_or_default().to_ascii_uppercase();
        match keyword.as_str() {
            "STARTTLS" => caps.starttls = true,
            "AUTH" => {
                for mech in words {
                    if (1..=20).contains(&mech.len())
                        && mech
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    {
                        caps.auth.insert(mech.to_ascii_uppercase());
                    }
                }
            }
            "SIZE" => {
                if let Some(limit) = words.next() {
                    if (1..=15).contains(&limit.len()) && limit.bytes().all(|b| b.is_ascii_digit())
                    {
                        caps.size = limit.parse().ok().filter(|&n: &u64| n > 0);
                    }
                }
            }
            _ => {}
        }
    }
    caps
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Whether `b` is RFC 5322 `atext`, a character of a dot-atom.
fn is_atext(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+/=?^_`{|}~-".contains(&b)
}

/// Whether `label` is one DNS label: letters, digits and inner hyphens, 1
/// to 63 characters.
fn is_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
        && bytes.first() != Some(&b'-')
        && bytes.last() != Some(&b'-')
}

/// Parses one email address: an ASCII dot-atom local part, `@`, and a host
/// name of two or more labels whose last is not all digits. Quoted local
/// parts, address literals and non-ASCII addresses are refused, so the
/// address is safe inside `<...>` on a command line and in a header.
///
/// # Arguments
///
/// * `raw` - The address as written; surrounding white space is dropped.
/// * `what` - What the value is, for the error ("the from_address setting").
///
/// # Returns
///
/// The address with its domain in lower case.
///
/// # Errors
///
/// `invalid_params` when it is not such an address.
pub fn parse_email(raw: &str, what: &str) -> Result<String, PluginError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(invalid(format!("{what} is empty")));
    }
    if text.chars().count() > MAX_EMAIL_CHARS {
        return Err(invalid(format!(
            "{what} is longer than {MAX_EMAIL_CHARS} characters"
        )));
    }
    let Some((local, domain)) = text.rsplit_once('@') else {
        return Err(invalid(format!("{what} is not an email address")));
    };
    let local_ok = (1..=MAX_LOCAL_PART_CHARS).contains(&local.len())
        && local
            .split('.')
            .all(|atom| !atom.is_empty() && atom.bytes().all(is_atext));
    if !local_ok {
        return Err(invalid(format!(
            "{what} is not an email address this notifier can send to"
        )));
    }
    let labels: Vec<&str> = domain.split('.').collect();
    let host_ok = labels.len() >= 2
        && labels.iter().all(|label| is_label(label))
        && !labels
            .last()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()));
    if !host_ok {
        return Err(invalid(format!("{what} does not end in a host name")));
    }
    Ok(format!("{local}@{}", domain.to_ascii_lowercase()))
}

/// A patron's address, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    /// The document the host saves: `{"email": ...}` with the address
    /// normalised.
    pub address_json: String,
    /// The address.
    pub email: String,
}

/// Parses a patron's address document: a JSON object whose only field is
/// `email`, a string.
///
/// # Arguments
///
/// * `address_json` - The document as entered.
///
/// # Errors
///
/// `invalid_params` when the document or the address is unusable.
pub fn parse_address(address_json: &str) -> Result<Address, PluginError> {
    if address_json.len() > MAX_ADDRESS_BYTES {
        return Err(invalid(format!(
            "the address is longer than {MAX_ADDRESS_BYTES} bytes"
        )));
    }
    let doc: serde_json::Value =
        serde_json::from_str(address_json).map_err(|_| invalid("the address is not JSON"))?;
    let serde_json::Value::Object(fields) = doc else {
        return Err(invalid("the address is not a JSON object"));
    };
    if fields.keys().any(|key| key != "email") {
        return Err(invalid("the address has a field other than email"));
    }
    let Some(serde_json::Value::String(email)) = fields.get("email") else {
        return Err(invalid("the address needs an email"));
    };
    let email = parse_email(email, "the email address")?;
    Ok(Address {
        address_json: serde_json::json!({ "email": email }).to_string(),
        email,
    })
}

/// Parses the `to` setting: addresses separated by commas, semicolons or
/// white space.
///
/// # Arguments
///
/// * `raw` - The setting.
///
/// # Returns
///
/// The addresses, in order, without repeats.
///
/// # Errors
///
/// `invalid_params` when it names none, more than
/// [`MAX_CHANNEL_RECIPIENTS`], or a bad address.
pub fn parse_recipients(raw: &str) -> Result<Vec<String>, PluginError> {
    let parts: Vec<&str> = raw
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(invalid("the to setting names no address"));
    }
    if parts.len() > MAX_CHANNEL_RECIPIENTS {
        return Err(invalid(format!(
            "the to setting names more than {MAX_CHANNEL_RECIPIENTS} addresses"
        )));
    }
    let mut out: Vec<String> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let email = parse_email(part, &format!("address {} of the to setting", i + 1))?;
        if !out.contains(&email) {
            out.push(email);
        }
    }
    Ok(out)
}

/// A TCP destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    /// Host name or address, without brackets.
    pub hostname: String,
    /// Port, 1 to 65535.
    pub port: u16,
}

/// Splits a granted destination, `host:port` or `[v6]:port`, as the host
/// states it in the network grant.
///
/// # Arguments
///
/// * `destination` - The destination.
///
/// # Errors
///
/// `invalid_params` when it has no usable host or port.
pub fn split_host_port(destination: &str) -> Result<HostPort, PluginError> {
    let bad = || invalid("the server destination has no usable host and port");
    let (host, port) = destination.rsplit_once(':').ok_or_else(bad)?;
    let hostname = match host.strip_prefix('[') {
        Some(inner) => inner
            .strip_suffix(']')
            .filter(|v6| {
                !v6.is_empty()
                    && v6
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
            })
            .ok_or_else(bad)?,
        None if !host.is_empty() && !host.contains([':', '[', ']']) => host,
        None => return Err(bad()),
    };
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let port: u16 = port.parse().ok().filter(|&p| p > 0).ok_or_else(bad)?;
    Ok(HostPort {
        hostname: hostname.to_owned(),
        port,
    })
}

// ---------------------------------------------------------------------------
// The message
// ---------------------------------------------------------------------------

/// What goes into one email.
#[derive(Debug, Clone)]
pub struct MailContent<'a> {
    /// Sender address, already checked.
    pub from: &'a str,
    /// Sender display name; may be empty.
    pub from_name: &'a str,
    /// Recipient addresses, already checked; shown in `To`.
    pub to: &'a [String],
    /// Subject line.
    pub subject: &'a str,
    /// Plain-text body.
    pub text: &'a str,
    /// HTML body, when there is one.
    pub html: Option<&'a str>,
    /// Images the HTML shows by `cid:` reference; ignored without HTML.
    pub images: &'a [InlineImage<'a>],
    /// Stable id across retries, so receivers can drop a repeat; without
    /// angle brackets.
    pub message_id: &'a str,
    /// When it is sent, as an RFC 5322 date.
    pub date: &'a str,
    /// MIME boundary; random in the guest, fixed in tests.
    pub boundary: &'a str,
}

/// An image sent inside the message and shown by the HTML as
/// `cid:<content_id>`, so it appears even when the library is not reachable
/// from the recipient's network and no remote image is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineImage<'a> {
    /// The `Content-ID`, without angle brackets; ASCII letters, digits, `.`,
    /// `-`, `_` and one `@`.
    pub content_id: &'a str,
    /// Media type, such as `image/png`.
    pub content_type: &'a str,
    /// File name offered when the recipient saves it; ASCII.
    pub filename: &'a str,
    /// The image file's bytes.
    pub bytes: &'a [u8],
}

/// Drops control characters (line breaks included) and trims, so text from
/// a message can never start a new header.
///
/// # Arguments
///
/// * `text` - Any text.
///
/// # Returns
///
/// The text on one line, each run of control characters as one space.
#[must_use]
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out.trim().to_owned()
}

/// Whether `text` is printable ASCII only.
fn is_printable_ascii(text: &str) -> bool {
    text.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Encodes header text: printable ASCII as is, other text as RFC 2047 `B`
/// encoded words of at most 72 characters, folded onto continuation lines.
///
/// # Arguments
///
/// * `text` - Text on one line.
///
/// # Returns
///
/// The header value.
#[must_use]
pub fn encode_header_text(text: &str) -> String {
    if is_printable_ascii(text) {
        return text.to_owned();
    }
    // 45 UTF-8 bytes encode to 60 base64 characters, so each word stays at
    // 72 with its `=?UTF-8?B?` and `?=`; characters are never split.
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for c in text.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(chunk);
    }
    words
        .iter()
        .map(|word| format!("=?UTF-8?B?{}?=", BASE64.encode(word)))
        .collect::<Vec<_>>()
        .join("\r\n ")
}

/// Formats a `From` mailbox: the address alone, or a display name (quoted,
/// or as encoded words when not ASCII) and the address in angle brackets.
///
/// # Arguments
///
/// * `name` - Display name; may be empty.
/// * `address` - The checked address.
#[must_use]
pub fn mailbox(name: &str, address: &str) -> String {
    let clean: String = one_line(name).chars().take(MAX_FROM_NAME_CHARS).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return address.to_owned();
    }
    let display = if is_printable_ascii(clean) {
        let mut quoted = String::from("\"");
        for c in clean.chars() {
            if c == '"' || c == '\\' {
                quoted.push('\\');
            }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    } else {
        encode_header_text(clean)
    };
    format!("{display} <{address}>")
}

/// Formats a Unix time as an RFC 5322 date, in UTC.
///
/// # Arguments
///
/// * `unix_seconds` - Seconds since 1970-01-01T00:00:00Z.
///
/// # Returns
///
/// `Mon, 23 Nov 2026 05:00:00 +0000`.
#[must_use]
pub fn rfc5322_date(unix_seconds: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = unix_seconds.div_euclid(86_400);
    let secs = unix_seconds.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = DAYS[usize::try_from(days.rem_euclid(7)).unwrap_or(0)];
    let month_name = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
    format!(
        "{weekday}, {day:02} {month_name} {year} {:02}:{:02}:{:02} +0000",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// Longest encoded line [`encode_quoted_printable`] writes before a soft
/// break's `=` (RFC 2045 section 6.7 allows 76 with it).
const QP_LINE_CHARS: usize = 75;

/// Encodes text as quoted-printable (RFC 2045 section 6.7): printable ASCII
/// stays readable, so filters that dock base64-encoded text don't, and
/// every other byte becomes `=XX`. Lines end in CRLF and run at most 76
/// characters, breaking with a soft `=` where they would run longer; a
/// space or tab at a line's end is encoded, and so is the `F` of a line
/// that starts `From `, so no relay rewrites it.
///
/// # Arguments
///
/// * `body` - The text; any line ending becomes CRLF.
///
/// # Returns
///
/// The encoded body, ASCII only.
#[must_use]
pub fn encode_quoted_printable(body: &str) -> String {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len() + normalized.len() / 8);
    for (index, line) in normalized.split('\n').enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        let bytes = line.as_bytes();
        let mut width = 0;
        for (at, &byte) in bytes.iter().enumerate() {
            let last = at + 1 == bytes.len();
            let literal = match byte {
                b' ' | b'\t' => !last,
                b'=' => false,
                b'F' if at == 0 && line.starts_with("From ") => false,
                33..=126 => true,
                _ => false,
            };
            let token_width = if literal { 1 } else { 3 };
            if width + token_width > QP_LINE_CHARS {
                out.push_str("=\r\n");
                width = 0;
            }
            if literal {
                out.push(char::from(byte));
            } else {
                out.push_str(&format!("={byte:02X}"));
            }
            width += token_width;
        }
    }
    out
}

/// Encodes bytes as base64 lines of 76 characters joined by CRLF.
///
/// # Arguments
///
/// * `bytes` - Any bytes.
fn base64_lines(bytes: &[u8]) -> String {
    let encoded = BASE64.encode(bytes);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// One text MIME part: its headers and its body in quoted-printable.
///
/// # Arguments
///
/// * `content_type` - `text/plain` or `text/html`.
/// * `body` - The body; line endings become CRLF.
fn part(content_type: &str, body: &str) -> String {
    format!(
        "Content-Type: {content_type}; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{}",
        encode_quoted_printable(body)
    )
}

/// Keeps only the characters a `Content-ID` or file name may carry here, so
/// neither can break out of its header.
///
/// # Arguments
///
/// * `value` - The id or name.
fn header_token(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@'))
        .take(128)
        .collect()
}

/// One inline image part, in base64, named by its `Content-ID`.
///
/// # Arguments
///
/// * `image` - The image.
fn image_part(image: &InlineImage<'_>) -> String {
    let content_type: String = image
        .content_type
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '+'))
        .take(64)
        .collect();
    let filename = header_token(image.filename);
    format!(
        "Content-Type: {content_type}\r\nContent-Transfer-Encoding: base64\r\n\
         Content-ID: <{}>\r\nContent-Disposition: inline; filename=\"{filename}\"\r\n\r\n{}",
        header_token(image.content_id),
        base64_lines(image.bytes)
    )
}

/// Builds the message: RFC 5322 headers, then the text alone or a
/// `multipart/alternative` of the text and the HTML; with inline images,
/// the HTML and the images form a `multipart/related` inside it. Every
/// line is ASCII, ends in CRLF and runs at most 76 characters in a body.
///
/// # Arguments
///
/// * `mail` - What goes in it.
///
/// # Returns
///
/// The message, without SMTP's dot-stuffing.
#[must_use]
pub fn build_message(mail: &MailContent<'_>) -> String {
    let headers = [
        format!("From: {}", mailbox(mail.from_name, mail.from)),
        format!("To: {}", mail.to.join(", ")),
        format!("Subject: {}", encode_header_text(&one_line(mail.subject))),
        format!("Date: {}", mail.date),
        format!("Message-ID: <{}>", mail.message_id),
        "MIME-Version: 1.0".to_owned(),
        // RFC 3834: no vacation replies to a notification.
        "Auto-Submitted: auto-generated".to_owned(),
    ]
    .join("\r\n");
    let Some(html) = mail.html else {
        return format!("{headers}\r\n{}\r\n", part("text/plain", mail.text));
    };
    let b = mail.boundary;
    let html_part = if mail.images.is_empty() {
        part("text/html", html)
    } else {
        let r = format!("{b}_r");
        let mut related = format!(
            "Content-Type: multipart/related; boundary=\"{r}\"; type=\"text/html\"\r\n\r\n\
             --{r}\r\n{}",
            part("text/html", html)
        );
        for image in mail.images {
            related.push_str(&format!("\r\n--{r}\r\n{}", image_part(image)));
        }
        related.push_str(&format!("\r\n--{r}--"));
        related
    };
    format!(
        "{headers}\r\nContent-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n\
         --{b}\r\n{}\r\n--{b}\r\n{html_part}\r\n--{b}--\r\n",
        part("text/plain", mail.text),
    )
}

/// Makes a stable `Message-ID`: the notification and destination ids (only
/// their safe characters) at the sender's domain, so every retry of one send
/// carries the same id.
///
/// # Arguments
///
/// * `notification_id` - Host id of the notification.
/// * `destination_id` - Host id of the destination.
/// * `from` - The checked sender address.
///
/// # Returns
///
/// The id, without angle brackets.
#[must_use]
pub fn message_id(notification_id: &str, destination_id: &str, from: &str) -> String {
    let safe = |id: &str| {
        let kept: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .take(64)
            .collect();
        if kept.is_empty() {
            "x".to_owned()
        } else {
            kept
        }
    };
    let domain = from.rsplit_once('@').map_or(from, |(_, domain)| domain);
    format!(
        "{}.{}@{domain}",
        safe(notification_id),
        safe(destination_id)
    )
}

/// Escapes text for HTML.
///
/// # Arguments
///
/// * `text` - Any text.
///
/// # Returns
///
/// The text with `& < > " '` as entities.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Applies SMTP's transparency rule (RFC 5321 section 4.5.2) and the end of
/// data: a line starting with `.` gets another, and the data ends in
/// `CRLF.CRLF`.
///
/// # Arguments
///
/// * `message` - The message, every line ending in CRLF.
///
/// # Returns
///
/// What to write after the 354 reply to DATA.
#[must_use]
pub fn dot_stuff(message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 8);
    for line in message.split_inclusive("\r\n") {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
    }
    if !out.ends_with("\r\n") {
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

/// What the session asks the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write these ASCII bytes and wait for the next reply.
    Write(String),
    /// Upgrade the connection to TLS, then call [`Session::tls_started`].
    StartTls,
    /// Write these bytes (`QUIT`) and close: the message was accepted.
    Done(String),
}

/// Credentials for `AUTH`.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// User name.
    pub username: String,
    /// Password; never printed.
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// What one session sends.
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// How the connection is protected.
    pub security: Security,
    /// Name the client gives in EHLO.
    pub helo_name: String,
    /// Credentials, when the server needs them.
    pub credentials: Option<Credentials>,
    /// Envelope sender, checked.
    pub from: String,
    /// Envelope recipients, checked; at least one.
    pub recipients: Vec<String>,
    /// The message, built by [`build_message`].
    pub message: String,
}

/// Which reply the session waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The server's 220 greeting.
    Greeting,
    /// The answer to EHLO.
    Ehlo,
    /// The 220 that clears the way for TLS.
    StartTls,
    /// No reply: the caller is upgrading to TLS.
    Tls,
    /// The answer to `AUTH PLAIN`, or to the password after `AUTH LOGIN`.
    Auth,
    /// The prompt for the user name after `AUTH LOGIN`.
    AuthLogin,
    /// The prompt for the password after the user name.
    AuthUser,
    /// The answer to MAIL FROM.
    Mail,
    /// The answer to the latest RCPT TO.
    Rcpt,
    /// The 354 that clears the way for the message.
    Data,
    /// The answer to the message.
    Sent,
    /// Nothing: the message was accepted.
    Finished,
}

/// Maps a reply that is not the one expected to a failure.
///
/// # Arguments
///
/// * `code` - The reply code.
/// * `step` - The step, for the error ("the recipient").
fn refusal(code: u16, step: &str) -> PluginError {
    match code {
        421 => unavailable(format!("the server closed the session at {step} (421)")),
        400..=499 => unavailable(format!("the server deferred {step} ({code})")),
        500..=599 => invalid(format!("the server refused {step} ({code})")),
        _ => unavailable(format!("the server answered {step} unexpectedly ({code})")),
    }
}

/// Maps a refused AUTH to a failure.
///
/// # Arguments
///
/// * `code` - The reply code.
fn auth_refusal(code: u16) -> PluginError {
    match code {
        530 | 534 | 535 | 538 => PluginError::new(
            PluginErrorCode::Forbidden,
            format!("the server refused the user name or password ({code})"),
        ),
        _ => refusal(code, "AUTH"),
    }
}

/// The client side of one SMTP submission: greeting, EHLO, STARTTLS and
/// EHLO again, AUTH, MAIL, one RCPT per recipient, DATA, the message, QUIT.
///
/// Credentials are sent only once the connection is TLS, so a server that
/// does not offer STARTTLS (or a meddler that strips it) gets none;
/// [`Security::None`] with credentials is refused up front.
#[derive(Debug)]
pub struct Session {
    /// The options.
    opts: SessionOptions,
    /// Which reply comes next.
    state: State,
    /// The connection is TLS (implicit, or upgraded by STARTTLS).
    tls: bool,
    /// Recipients accepted so far.
    accepted: usize,
    /// `SIZE` from the latest EHLO.
    size: Option<u64>,
}

impl Session {
    /// Starts a session; the first reply fed in is the server's greeting.
    ///
    /// # Arguments
    ///
    /// * `opts` - What to send and how.
    ///
    /// # Errors
    ///
    /// `invalid_params` for credentials without TLS, or no recipient.
    pub fn new(opts: SessionOptions) -> Result<Self, PluginError> {
        if opts.security == Security::None && opts.credentials.is_some() {
            return Err(invalid(
                "a user name and password need security starttls or tls",
            ));
        }
        if opts.recipients.is_empty() {
            return Err(invalid("there is no recipient"));
        }
        let tls = opts.security == Security::Tls;
        Ok(Self {
            opts,
            state: State::Greeting,
            tls,
            accepted: 0,
            size: None,
        })
    }

    /// Whether the server accepted the message.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.state == State::Finished
    }

    /// Whether the connection is TLS now. Credentials are written only when
    /// it is.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// Takes the next reply.
    ///
    /// # Arguments
    ///
    /// * `reply` - A complete reply.
    ///
    /// # Returns
    ///
    /// What to do next.
    ///
    /// # Errors
    ///
    /// When the server refuses a step, offers no way to proceed, or answers
    /// out of turn.
    pub fn on_reply(&mut self, reply: &Reply) -> Result<Step, PluginError> {
        let code = reply.code;
        match self.state {
            State::Greeting => {
                if code != 220 {
                    return Err(refusal(code, "the greeting"));
                }
                Ok(self.ehlo())
            }
            State::Ehlo => {
                if code != 250 {
                    return Err(refusal(code, "EHLO"));
                }
                let caps = parse_ehlo(reply);
                self.size = caps.size;
                self.after_ehlo(&caps)
            }
            State::StartTls => {
                if code != 220 {
                    return Err(refusal(code, "STARTTLS"));
                }
                self.state = State::Tls;
                Ok(Step::StartTls)
            }
            State::Auth => {
                if code == 235 {
                    return self.mail();
                }
                Err(auth_refusal(code))
            }
            State::AuthLogin => {
                if code != 334 {
                    return Err(auth_refusal(code));
                }
                self.state = State::AuthUser;
                Ok(Step::Write(format!(
                    "{}\r\n",
                    BASE64.encode(&self.creds()?.username)
                )))
            }
            State::AuthUser => {
                if code != 334 {
                    return Err(auth_refusal(code));
                }
                self.state = State::Auth;
                Ok(Step::Write(format!(
                    "{}\r\n",
                    BASE64.encode(&self.creds()?.password)
                )))
            }
            State::Mail => {
                if code != 250 {
                    return Err(refusal(code, "the sender"));
                }
                Ok(self.rcpt())
            }
            State::Rcpt => match code {
                250 | 251 => {
                    self.accepted += 1;
                    if self.accepted < self.opts.recipients.len() {
                        Ok(self.rcpt())
                    } else {
                        self.state = State::Data;
                        Ok(Step::Write("DATA\r\n".to_owned()))
                    }
                }
                550 | 551 | 553 => Err(PluginError::not_found(format!(
                    "the server refused the recipient ({code})"
                ))),
                _ => Err(refusal(code, "the recipient")),
            },
            State::Data => {
                if code != 354 {
                    return Err(refusal(code, "DATA"));
                }
                self.state = State::Sent;
                Ok(Step::Write(dot_stuff(&self.opts.message)))
            }
            State::Sent => {
                if code != 250 {
                    return Err(refusal(code, "the message"));
                }
                self.state = State::Finished;
                Ok(Step::Done("QUIT\r\n".to_owned()))
            }
            State::Tls | State::Finished => Err(unavailable("the server sent a reply out of turn")),
        }
    }

    /// Continues once the connection is TLS: EHLO again, as RFC 3207
    /// requires, since what the server said before the upgrade can't be
    /// trusted.
    ///
    /// # Errors
    ///
    /// `unavailable` when no upgrade was asked for.
    pub fn tls_started(&mut self) -> Result<Step, PluginError> {
        if self.state != State::Tls {
            return Err(unavailable("TLS started out of turn"));
        }
        self.tls = true;
        Ok(self.ehlo())
    }

    /// Writes EHLO.
    fn ehlo(&mut self) -> Step {
        self.state = State::Ehlo;
        Step::Write(format!("EHLO {}\r\n", self.opts.helo_name))
    }

    /// The credentials, which only exist in the AUTH states.
    ///
    /// # Errors
    ///
    /// `unavailable` when there are none, which only a server answering out
    /// of turn could cause.
    fn creds(&self) -> Result<&Credentials, PluginError> {
        self.opts
            .credentials
            .as_ref()
            .ok_or_else(|| unavailable("the server sent a reply out of turn"))
    }

    /// Chooses what follows EHLO: STARTTLS, AUTH or MAIL.
    ///
    /// # Arguments
    ///
    /// * `caps` - What the server offers.
    ///
    /// # Errors
    ///
    /// `invalid_params` when TLS is required but not offered, or no AUTH
    /// mechanism fits.
    fn after_ehlo(&mut self, caps: &Capabilities) -> Result<Step, PluginError> {
        if self.opts.security == Security::StartTls && !self.tls {
            if !caps.starttls {
                return Err(invalid(
                    "the server does not offer STARTTLS; set security to tls or none",
                ));
            }
            self.state = State::StartTls;
            return Ok(Step::Write("STARTTLS\r\n".to_owned()));
        }
        let Some(creds) = &self.opts.credentials else {
            return self.mail();
        };
        // Never reached without TLS: `new` refuses credentials with
        // `Security::None`, and STARTTLS upgrades before it gets here.
        if !self.tls {
            return Err(PluginError::forbidden("credentials are only sent over TLS"));
        }
        if caps.auth.contains("PLAIN") {
            let token = BASE64.encode(format!("\0{}\0{}", creds.username, creds.password));
            self.state = State::Auth;
            return Ok(Step::Write(format!("AUTH PLAIN {token}\r\n")));
        }
        if caps.auth.contains("LOGIN") {
            // No initial response: LOGIN has no standard for one, so the
            // user name answers the server's first prompt.
            self.state = State::AuthLogin;
            return Ok(Step::Write("AUTH LOGIN\r\n".to_owned()));
        }
        Err(invalid(
            "the server offers neither AUTH PLAIN nor AUTH LOGIN",
        ))
    }

    /// Writes MAIL FROM, with the message size when the server takes `SIZE`.
    ///
    /// # Errors
    ///
    /// `invalid_params` when the message is larger than the server allows.
    fn mail(&mut self) -> Result<Step, PluginError> {
        let size = u64::try_from(self.opts.message.len()).unwrap_or(u64::MAX);
        let param = match self.size {
            Some(limit) if size > limit => {
                return Err(invalid(format!(
                    "the message is larger than the server's {limit}-byte limit"
                )));
            }
            Some(_) => format!(" SIZE={size}"),
            None => String::new(),
        };
        self.state = State::Mail;
        Ok(Step::Write(format!(
            "MAIL FROM:<{}>{param}\r\n",
            self.opts.from
        )))
    }

    /// Writes RCPT TO for the next recipient.
    fn rcpt(&mut self) -> Step {
        self.state = State::Rcpt;
        let to = self
            .opts
            .recipients
            .get(self.accepted)
            .map_or("", String::as_str);
        Step::Write(format!("RCPT TO:<{to}>\r\n"))
    }
}

#[cfg(test)]
pub(crate) mod tests;
