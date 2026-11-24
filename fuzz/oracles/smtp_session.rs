//! Shared oracle for the `smtp_session` fuzz target and the
//! `bookclerk-plugin-notifier-email` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! Byte 0 picks the case; the rest is the input.
//!
//! - **Even: a mail server's byte stream.** It is read reply by reply with
//!   `read_reply` and each reply is fed to a `Session` (byte 1 picks the
//!   security mode and whether credentials are set; a TLS upgrade the
//!   session asks for succeeds at once). The oracle restates the reply
//!   grammar with its own line splitter and checks `read_reply` agrees on
//!   every prefix it takes: incomplete, refused, or the same code, lines and
//!   length. For the session: it never panics; every write is ASCII and ends
//!   in CRLF, with no bare CR or LF inside a command line; nothing but EHLO
//!   and STARTTLS is written before TLS when the mode needs TLS; `AUTH` and
//!   the password are written only over TLS; `MAIL`, `RCPT` and `DATA`
//!   follow the order of the protocol; `DATA`'s payload is the
//!   dot-stuffed message; and the session ends at most once.
//! - **Odd: a patron's address document or the `to` setting.** The rest is
//!   read as text (invalid UTF-8 replaced). `parse_address` accepts exactly
//!   when the oracle's own address rules (written with string operations,
//!   not the guest's helpers) accept the JSON object's one `email` field,
//!   and the saved document parses back to the normalised address, which is
//!   stable when parsed again. `parse_recipients` gives only addresses
//!   `parse_email` accepts, at most 10, without repeats.

use bookclerk_plugin_notifier_email::smtp::{
    dot_stuff, parse_address, parse_email, parse_recipients, read_reply, Credentials, Reply,
    Security, Session, SessionOptions, Step, MAX_ADDRESS_BYTES, MAX_CHANNEL_RECIPIENTS,
    MAX_REPLY_LINES, MAX_REPLY_LINE_BYTES,
};

/// A reply as the oracle reads it: the code, each line's text, and the
/// number of bytes it took.
type OracleReply = (u16, Vec<Vec<u8>>, usize);

/// The oracle's reading of the first reply in `buf`: `Ok(None)` while it is
/// incomplete, `Err(())` when refused, else the reply and its length.
fn oracle_reply(buf: &[u8]) -> Result<Option<OracleReply>, ()> {
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut code = None;
    let mut start = 0;
    loop {
        let rest = &buf[start..];
        let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
            return if rest.len() > MAX_REPLY_LINE_BYTES {
                Err(())
            } else {
                Ok(None)
            };
        };
        let mut line = rest[..nl].to_vec();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.len() > MAX_REPLY_LINE_BYTES || line.len() < 3 {
            return Err(());
        }
        let digits = std::str::from_utf8(&line[..3]).map_err(|_| ())?;
        let n: u16 = digits.parse().map_err(|_| ())?;
        if !digits.chars().all(|c| c.is_ascii_digit()) || !(200..600).contains(&n) {
            return Err(());
        }
        if code.is_some() && code != Some(n) {
            return Err(());
        }
        code = Some(n);
        let more = match line.get(3) {
            None => false,
            Some(b' ') => false,
            Some(b'-') => true,
            Some(_) => return Err(()),
        };
        lines.push(line.get(4..).unwrap_or_default().to_vec());
        start += nl + 1;
        if !more {
            return Ok(Some((n, lines, start)));
        }
        if lines.len() >= MAX_REPLY_LINES {
            return Err(());
        }
    }
}

/// The message every fuzzed session sends.
const MESSAGE: &str = "Subject: x\r\n\r\n.first\r\nbody\r\n";

/// Checks the server-stream case.
// Oracle: the options are fixed valid literals, and a refused upgrade the
// session asked for is an invariant breaking.
#[allow(clippy::expect_used, clippy::panic)]
fn check_session(data: &[u8]) {
    let Some((&mode, stream)) = data.split_first() else {
        return;
    };
    let security = match mode % 3 {
        0 => Security::StartTls,
        1 => Security::Tls,
        _ => Security::None,
    };
    let credentials = (mode & 0x80 != 0 && security != Security::None).then(|| Credentials {
        username: "user".into(),
        password: "s3cret-pass".into(),
    });
    let has_creds = credentials.is_some();
    let mut session = Session::new(SessionOptions {
        security,
        helo_name: "example.com".into(),
        credentials,
        from: "from@example.com".into(),
        recipients: vec!["a@example.com".into(), "b@example.com".into()],
        message: MESSAGE.into(),
    })
    .expect("valid options");
    let password_b64 = "czNjcmV0LXBhc3M=";
    let mut buf = stream;
    let mut started_tls = 0;
    let mut sent = Vec::<String>::new();
    loop {
        let ours = read_reply(buf);
        let theirs = oracle_reply(buf);
        let (reply, used) = match (ours, theirs) {
            (Ok(None), Ok(None)) => return,
            (Err(_), Err(())) => return,
            (Ok(Some((reply, used))), Ok(Some((code, lines, len)))) => {
                assert_eq!(reply.code, code, "reply code");
                assert_eq!(used, len, "reply length");
                let expected: Vec<String> = lines
                    .iter()
                    .map(|l| String::from_utf8_lossy(l).into_owned())
                    .collect();
                assert_eq!(reply.lines, expected, "reply lines");
                (reply, used)
            }
            (ours, theirs) => panic!("read_reply {ours:?} but the oracle says {theirs:?}"),
        };
        buf = &buf[used..];
        let step = match session.on_reply(&reply) {
            Ok(step) => step,
            Err(_) => return,
        };
        let step = if step == Step::StartTls {
            started_tls += 1;
            assert_eq!(started_tls, 1, "TLS started twice");
            assert_eq!(
                security,
                Security::StartTls,
                "STARTTLS outside starttls mode"
            );
            assert!(!session.is_tls());
            session.tls_started().expect("upgrade asked for")
        } else {
            step
        };
        let (data, done) = match step {
            Step::Write(data) => (data, false),
            Step::Done(data) => (data, true),
            Step::StartTls => panic!("two upgrades in a row"),
        };
        assert!(data.is_ascii() && data.ends_with("\r\n"), "write {data:?}");
        let is_payload = sent.last().is_some_and(|last| last == "DATA");
        if is_payload {
            assert_eq!(data, dot_stuff(MESSAGE), "DATA payload");
        } else {
            let line = &data[..data.len() - 2];
            assert!(!line.contains(['\r', '\n']), "command line {data:?}");
            let verb = line.split(' ').next().unwrap_or_default().to_owned();
            if security != Security::None && !session.is_tls() {
                assert!(verb == "EHLO" || verb == "STARTTLS", "{verb} before TLS");
            }
            if verb == "AUTH" || line == password_b64 {
                assert!(has_creds && session.is_tls(), "credentials without TLS");
            }
            let expected_before = match verb.as_str() {
                "MAIL" => vec!["EHLO", "AUTH"],
                "RCPT" => vec!["MAIL", "RCPT"],
                "DATA" => vec!["RCPT"],
                _ => vec![],
            };
            if !expected_before.is_empty() {
                let prev = sent.last().map(String::as_str).unwrap_or_default();
                assert!(
                    expected_before.iter().any(|p| prev.starts_with(p)) || prev == password_b64,
                    "{verb} after {prev}"
                );
            }
            if verb == "RCPT" && sent.last().is_some_and(|p| p.starts_with("RCPT")) {
                assert_eq!(line, "RCPT TO:<b@example.com>");
            }
            sent.push(if line.starts_with("AUTH") || line.starts_with("EHLO") {
                verb
            } else {
                line.to_owned()
            });
        }
        if is_payload {
            sent.push("payload".into());
        }
        if done {
            assert_eq!(data, "QUIT\r\n");
            assert!(session.finished());
            assert!(
                sent.iter().any(|s| s == "payload"),
                "done before the message"
            );
            assert!(session
                .on_reply(&Reply {
                    code: 250,
                    lines: vec![String::new()]
                })
                .is_err());
            return;
        }
    }
}

/// Whether `c` is RFC 5322 `atext`, by the oracle's own list.
fn atext(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~-".contains(c)
}

/// The oracle's address rules: the normalised address, or `None`.
fn oracle_email(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() || text.chars().count() > 254 {
        return None;
    }
    let at = text.rfind('@')?;
    let (local, domain) = (&text[..at], &text[at + 1..]);
    if local.is_empty() || local.len() > 64 {
        return None;
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return None;
    }
    if !local.chars().all(|c| c == '.' || atext(c)) {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    for label in &labels {
        if label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-') {
            return None;
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
    }
    if labels.last()?.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("{local}@{}", domain.to_lowercase()))
}

/// Checks the address-document case.
// Oracle: a saved document that does not parse back is an invariant breaking.
#[allow(clippy::expect_used, clippy::panic)]
fn check_address(text: &str) {
    let expected = if text.len() > MAX_ADDRESS_BYTES {
        None
    } else {
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(serde_json::Value::Object(map)) if map.len() == 1 => match map.get("email") {
                Some(serde_json::Value::String(email)) => oracle_email(email),
                _ => None,
            },
            _ => None,
        }
    };
    match (parse_address(text), expected) {
        (Ok(got), Some(email)) => {
            assert_eq!(got.email, email);
            let back: serde_json::Value =
                serde_json::from_str(&got.address_json).expect("saved JSON");
            assert_eq!(back, serde_json::json!({ "email": email }));
            assert_eq!(
                parse_address(&got.address_json)
                    .expect("stable")
                    .address_json,
                got.address_json
            );
        }
        (Err(err), None) => assert!(!err.message.is_empty()),
        (got, expected) => panic!("parse_address {got:?} but the oracle says {expected:?}"),
    }
    if let Ok(list) = parse_recipients(text) {
        assert!(!list.is_empty() && list.len() <= MAX_CHANNEL_RECIPIENTS);
        for (i, email) in list.iter().enumerate() {
            assert_eq!(parse_email(email, "x").as_ref().ok(), Some(email));
            assert_eq!(oracle_email(email).as_ref(), Some(email));
            assert!(!list[..i].contains(email), "repeat");
        }
    }
}

/// Checks one input against every invariant.
///
/// # Panics
///
/// When an invariant breaks.
pub fn check(data: &[u8]) {
    let Some((&case, rest)) = data.split_first() else {
        return;
    };
    if case % 2 == 0 {
        check_session(rest);
    } else {
        check_address(&String::from_utf8_lossy(rest));
    }
}
