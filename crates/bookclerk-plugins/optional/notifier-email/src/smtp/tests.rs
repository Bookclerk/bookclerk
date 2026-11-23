//! Unit tests for the SMTP parsing, message building and session.

// Test code: a failed expectation is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// Asserts `result` failed with `code`, returning the message.
fn fails_with<T: std::fmt::Debug>(result: Result<T, PluginError>, code: PluginErrorCode) -> String {
    let err = result.expect_err("expected a failure");
    assert_eq!(err.code, code, "{}", err.message);
    err.message
}

/// A reply.
fn reply(code: u16, lines: &[&str]) -> Reply {
    Reply {
        code,
        lines: if lines.is_empty() {
            vec![String::new()]
        } else {
            lines.iter().map(|&l| l.to_owned()).collect()
        },
    }
}

#[test]
fn replies_are_read_whole_one_at_a_time() {
    assert_eq!(read_reply(b"").unwrap(), None);
    assert_eq!(read_reply(b"250-a\r\n250").unwrap(), None);
    assert_eq!(
        read_reply(b"220 ready\r\nrest").unwrap(),
        Some((reply(220, &["ready"]), 11))
    );
    assert_eq!(
        read_reply(b"250-mx.example\r\n250-STARTTLS\r\n250 SIZE 10\r\n").unwrap(),
        Some((reply(250, &["mx.example", "STARTTLS", "SIZE 10"]), 43))
    );
    // A bare code, a bare LF and an empty text are all accepted.
    assert_eq!(read_reply(b"250\n").unwrap(), Some((reply(250, &[""]), 4)));
    assert_eq!(
        read_reply(b"354 \r\n").unwrap().unwrap().0,
        reply(354, &[""])
    );
    // Invalid UTF-8 in the text is replaced, never refused.
    assert_eq!(
        read_reply(b"250 \xff\r\n").unwrap().unwrap().0.lines,
        vec!["\u{fffd}"]
    );
}

#[test]
fn malformed_replies_are_refused() {
    let long = format!("250 {}\r\n", "x".repeat(MAX_REPLY_LINE_BYTES));
    for bad in [
        &b"hello\r\n"[..],
        b"25 x\r\n",
        b"199 x\r\n",
        b"600 x\r\n",
        b"250x\r\n",
        b"2a0 x\r\n",
        b"250-a\r\n251 b\r\n",
        long.as_bytes(),
    ] {
        fails_with(read_reply(bad), PluginErrorCode::Unavailable);
    }
    fails_with(
        read_reply("2".repeat(MAX_REPLY_LINE_BYTES + 1).as_bytes()),
        PluginErrorCode::Unavailable,
    );
    fails_with(
        read_reply("250-x\r\n".repeat(MAX_REPLY_LINES + 1).as_bytes()),
        PluginErrorCode::Unavailable,
    );
}

#[test]
fn ehlo_capabilities_are_read_leniently() {
    let caps = parse_ehlo(&reply(
        250,
        &[
            "mx.example greets you",
            "starttls",
            "AUTH LOGIN plain XOAUTH2",
            "AUTH=CRAM-MD5",
            "SIZE 35882577",
            "8BITMIME",
        ],
    ));
    assert!(caps.starttls);
    assert_eq!(
        caps.auth.iter().map(String::as_str).collect::<Vec<_>>(),
        ["CRAM-MD5", "LOGIN", "PLAIN", "XOAUTH2"]
    );
    assert_eq!(caps.size, Some(35_882_577));
    // The first line is the greeting, never a keyword.
    assert!(!parse_ehlo(&reply(250, &["STARTTLS"])).starttls);
    assert_eq!(parse_ehlo(&reply(250, &["x", "SIZE 0"])).size, None);
    assert_eq!(parse_ehlo(&reply(250, &["x", "SIZE big"])).size, None);
    assert_eq!(parse_ehlo(&reply(250, &["x", "SIZE"])).size, None);
    assert!(parse_ehlo(&reply(250, &["x", "AUTH $$$"])).auth.is_empty());
    assert!(parse_ehlo(&reply(250, &["x", ""])).auth.is_empty());
}

#[test]
fn email_addresses_are_ascii_dot_atoms_at_a_host_name() {
    assert_eq!(
        parse_email(" Pat.Lee+books@Mail.Example.COM ", "a").unwrap(),
        "Pat.Lee+books@mail.example.com"
    );
    assert_eq!(
        parse_email("o'brien@xn--bcher-kva.example", "a").unwrap(),
        "o'brien@xn--bcher-kva.example"
    );
    let long_local = format!("{}@example.com", "a".repeat(65));
    let long_domain = format!("a@{}.com", "b".repeat(250));
    for bad in [
        "",
        "plain",
        "@example.com",
        "a@",
        "a@localhost",
        "a@1.2.3.4",
        "a@[1.2.3.4]",
        "\"a b\"@example.com",
        "a..b@example.com",
        ".a@example.com",
        "a.@example.com",
        "a b@example.com",
        "a@-x.example",
        "a@x-.example",
        "a@exa_mple.com",
        "jürgen@example.com",
        "a@example.com\r\nRCPT TO:<b@example.com>",
        "a>@example.com",
        &long_local,
        &long_domain,
    ] {
        fails_with(
            parse_email(bad, "the address"),
            PluginErrorCode::InvalidParams,
        );
    }
}

#[test]
fn errors_never_repeat_the_address() {
    let message = fails_with(
        parse_email("token123@", "the from_address setting"),
        PluginErrorCode::InvalidParams,
    );
    assert!(!message.contains("token123"), "{message}");
    assert!(message.contains("from_address"), "{message}");
    let message = fails_with(
        parse_address(r#"{"email":"token123"}"#),
        PluginErrorCode::InvalidParams,
    );
    assert!(!message.contains("token123"), "{message}");
}

#[test]
fn address_documents_hold_only_an_email() {
    assert_eq!(
        parse_address(r#"{"email":" Pat@Example.com "}"#).unwrap(),
        Address {
            address_json: r#"{"email":"Pat@example.com"}"#.into(),
            email: "Pat@example.com".into(),
        }
    );
    let huge = format!(
        r#"{{"email":"a@example.com{}"}}"#,
        " ".repeat(MAX_ADDRESS_BYTES)
    );
    for bad in [
        "nope",
        "[]",
        "null",
        "{}",
        r#"{"email":1}"#,
        r#"{"email":"a@example.com","name":"x"}"#,
        huge.as_str(),
    ] {
        fails_with(parse_address(bad), PluginErrorCode::InvalidParams);
    }
}

#[test]
fn the_to_setting_lists_up_to_ten_addresses() {
    assert_eq!(
        parse_recipients("a@example.com, B@Example.com;a@example.com\n c@example.org").unwrap(),
        ["a@example.com", "B@example.com", "c@example.org"]
    );
    fails_with(parse_recipients(""), PluginErrorCode::InvalidParams);
    fails_with(parse_recipients(" , "), PluginErrorCode::InvalidParams);
    fails_with(
        parse_recipients("a@example.com, nope"),
        PluginErrorCode::InvalidParams,
    );
    let many: Vec<String> = (0..=MAX_CHANNEL_RECIPIENTS)
        .map(|i| format!("u{i}@example.com"))
        .collect();
    fails_with(
        parse_recipients(&many.join(",")),
        PluginErrorCode::InvalidParams,
    );
}

#[test]
fn granted_destinations_split_into_host_and_port() {
    assert_eq!(
        split_host_port("smtp.example.com:587").unwrap(),
        HostPort {
            hostname: "smtp.example.com".into(),
            port: 587
        }
    );
    assert_eq!(
        split_host_port("[2001:db8::1]:465").unwrap(),
        HostPort {
            hostname: "2001:db8::1".into(),
            port: 465
        }
    );
    for bad in [
        "smtp.example.com",
        "a:0",
        "a:65536",
        "a:b",
        "a:",
        "[]:25",
        "[x]:25",
        "a:b:25",
        ":25",
        "a:123456",
        "",
    ] {
        fails_with(split_host_port(bad), PluginErrorCode::InvalidParams);
    }
}

#[test]
fn security_values() {
    assert_eq!(Security::parse("starttls").unwrap(), Security::StartTls);
    assert_eq!(Security::parse("tls").unwrap(), Security::Tls);
    assert_eq!(Security::parse("none").unwrap(), Security::None);
    fails_with(Security::parse("ssl"), PluginErrorCode::InvalidParams);
}

#[test]
fn header_text_never_breaks_a_line_and_non_ascii_is_encoded() {
    assert_eq!(one_line(" a\r\nBcc: x\u{2028}y\u{0}\u{1} "), "a Bcc: x y");
    assert_eq!(encode_header_text("Plain subject"), "Plain subject");
    let text = format!("Ünïcödé {} 📚", "ö".repeat(60));
    let encoded = encode_header_text(&text);
    let mut decoded = String::new();
    for line in encoded.split("\r\n") {
        let word = line.trim_start();
        assert!(
            word.starts_with("=?UTF-8?B?") && word.ends_with("?="),
            "{word}"
        );
        assert!(word.len() <= 75, "{word}");
        let b64 = &word[10..word.len() - 2];
        decoded.push_str(&String::from_utf8(BASE64.decode(b64).unwrap()).unwrap());
    }
    assert_eq!(decoded, text);
}

#[test]
fn from_mailboxes_quote_or_encode_the_name() {
    assert_eq!(mailbox("", "a@example.com"), "a@example.com");
    assert_eq!(mailbox(" \r\n ", "a@example.com"), "a@example.com");
    assert_eq!(
        mailbox("Book \"Clerk\" \\ Co", "a@example.com"),
        "\"Book \\\"Clerk\\\" \\\\ Co\" <a@example.com>"
    );
    assert_eq!(
        mailbox("Bücher", "a@example.com"),
        format!("=?UTF-8?B?{}?= <a@example.com>", BASE64.encode("Bücher"))
    );
    assert_eq!(
        mailbox("a\r\nBcc: b@example.com", "a@example.com"),
        "\"a Bcc: b@example.com\" <a@example.com>"
    );
}

#[test]
fn dates_ids_and_html_escaping() {
    assert_eq!(
        rfc5322_date(1_795_410_000),
        "Mon, 23 Nov 2026 05:00:00 +0000"
    );
    assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
    assert_eq!(rfc5322_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 +0000");
    assert_eq!(rfc5322_date(-1), "Wed, 31 Dec 1969 23:59:59 +0000");
    assert_eq!(
        message_id("n-1/x", "d 9", "a@Example.com"),
        "n-1x.d9@Example.com"
    );
    assert_eq!(message_id("", "!!", "a@example.com"), "x.x@example.com");
    assert_eq!(
        escape_html("<a href=\"x\">'&'</a>"),
        "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
    );
}

/// Decodes quoted-printable (RFC 2045 section 6.7), for checking bodies.
pub(crate) fn decode_qp(encoded: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let joined = encoded.replace("=\r\n", "");
    let bytes = joined.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[test]
fn quoted_printable_keeps_ascii_readable_and_lines_short() {
    assert_eq!(
        encode_quoted_printable("Hello\nworld\r."),
        "Hello\r\nworld\r\n."
    );
    assert_eq!(encode_quoted_printable("a=b é"), "a=3Db =C3=A9");
    assert_eq!(
        encode_quoted_printable("trailing \nTab\t"),
        "trailing=20\r\nTab=09"
    );
    assert_eq!(
        encode_quoted_printable("From here\nFrom"),
        "=46rom here\r\nFrom"
    );
    assert_eq!(encode_quoted_printable("x\n"), "x\r\n");
    let long = format!("{} {}", "a".repeat(100), "é".repeat(60));
    let encoded = encode_quoted_printable(&long);
    for line in encoded.split("\r\n") {
        assert!(line.len() <= 76, "{line}");
        assert!(line
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) || b == b' '));
        // A soft break never splits an `=XX`.
        let body = line.strip_suffix('=').unwrap_or(line);
        assert!(
            !body.ends_with('=') && body.as_bytes().get(body.len().wrapping_sub(2)) != Some(&b'='),
            "{line}"
        );
    }
    assert_eq!(decode_qp(&encoded), long.as_bytes());
}

#[test]
fn a_message_is_ascii_with_quoted_printable_bodies() {
    let to = vec!["pat@example.com".to_owned(), "lee@example.com".to_owned()];
    let html = format!("<p>{}</p>", "é".repeat(200));
    let base = MailContent {
        from: "clerk@example.com",
        from_name: "Bookclerk",
        to: &to,
        subject: "Your invite\r\nBcc: evil@example.com",
        text: "Hello\nworld\r.\n",
        html: None,
        images: &[],
        message_id: "n-1.d-1@example.com",
        date: "Mon, 23 Nov 2026 05:00:00 +0000",
        boundary: "=_b",
    };
    let plain = build_message(&base);
    assert!(plain.starts_with("From: \"Bookclerk\" <clerk@example.com>\r\n"));
    assert!(plain.contains("\r\nTo: pat@example.com, lee@example.com\r\n"));
    assert!(plain.contains("\r\nSubject: Your invite Bcc: evil@example.com\r\n"));
    assert!(!plain.contains("\r\nBcc:"));
    assert!(plain.contains("\r\nDate: Mon, 23 Nov 2026 05:00:00 +0000\r\n"));
    assert!(plain.contains("\r\nMessage-ID: <n-1.d-1@example.com>\r\n"));
    assert!(plain.contains("\r\nAuto-Submitted: auto-generated\r\n"));
    assert!(plain.contains(
        "\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n"
    ));
    let body = plain.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(body, "Hello\r\nworld\r\n.\r\n\r\n");
    let both = build_message(&MailContent {
        html: Some(&html),
        ..base
    });
    assert!(both.contains("\r\nContent-Type: multipart/alternative; boundary=\"=_b\"\r\n"));
    assert!(both.contains("\r\n--=_b\r\nContent-Type: text/html; charset=utf-8\r\n"));
    assert!(both.ends_with("\r\n--=_b--\r\n"));
    assert!(!both.contains("multipart/related"));
    let logo = [InlineImage {
        content_id: "mark@bookclerk>\r\nBcc: x",
        content_type: "image/png\r\nX: y",
        filename: "a\"b.png",
        bytes: &[0x89, b'P', b'N', b'G', 0, 1, 2, 255],
    }];
    let related = build_message(&MailContent {
        html: Some(&html),
        images: &logo,
        ..base
    });
    assert!(related.contains(
        "\r\n--=_b\r\nContent-Type: multipart/related; boundary=\"=_b_r\"; type=\"text/html\"\r\n\r\n--=_b_r\r\nContent-Type: text/html; charset=utf-8\r\n"
    ));
    assert!(related.contains(
        "\r\n--=_b_r\r\nContent-Type: image/pngXy\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <mark@bookclerkBccx>\r\nContent-Disposition: inline; filename=\"ab.png\"\r\n\r\niVBORwABAv8=\r\n--=_b_r--\r\n--=_b--\r\n"
    ), "{related}");
    assert!(!related.contains("\r\nBcc:"));
    for message in [&both, &related] {
        for line in message.split("\r\n") {
            assert!(line.bytes().all(|b| (0x20..=0x7e).contains(&b)), "{line}");
            assert!(line.len() <= 998);
        }
        assert!(!message.replace("\r\n", "").contains('\n'));
    }
}

#[test]
fn dot_stuffing_doubles_leading_dots_and_ends_the_data() {
    assert_eq!(dot_stuff("a\r\n.b\r\n..\r\n"), "a\r\n..b\r\n...\r\n.\r\n");
    assert_eq!(dot_stuff(".x"), "..x\r\n.\r\n");
    assert_eq!(dot_stuff(""), "\r\n.\r\n");
}

/// One scripted event for [`run`].
enum Event {
    /// A reply from the server.
    Reply(Reply),
    /// The TLS upgrade finished.
    Tls,
}

/// A reply event.
fn r(code: u16, lines: &[&str]) -> Event {
    Event::Reply(reply(code, lines))
}

/// Session options for the tests, overridable.
fn options() -> SessionOptions {
    SessionOptions {
        security: Security::StartTls,
        helo_name: "example.com".into(),
        credentials: Some(Credentials {
            username: "clerk".into(),
            password: "pw".into(),
        }),
        from: "clerk@example.com".into(),
        recipients: vec!["pat@example.com".into()],
        message: "Subject: x\r\n\r\nhi\r\n".into(),
    }
}

/// Runs a session against scripted events, returning what it wrote (TLS
/// marked as `<TLS>`) or the first failure.
fn run(opts: SessionOptions, events: Vec<Event>) -> Result<String, PluginError> {
    let mut session = Session::new(opts)?;
    let mut out = String::new();
    for event in events {
        let step = match event {
            Event::Reply(reply) => session.on_reply(&reply)?,
            Event::Tls => session.tls_started()?,
        };
        match step {
            Step::Write(data) | Step::Done(data) => out.push_str(&data),
            Step::StartTls => out.push_str("<TLS>\n"),
        }
    }
    Ok(out)
}

#[test]
fn a_starttls_session_signs_in_with_plain_after_the_upgrade() {
    let written = run(
        options(),
        vec![
            r(220, &["mx ready"]),
            r(250, &["mx", "STARTTLS", "AUTH LOGIN"]),
            r(220, &["go ahead"]),
            Event::Tls,
            r(250, &["mx", "AUTH PLAIN LOGIN", "SIZE 1000"]),
            r(235, &[]),
            r(250, &[]),
            r(250, &[]),
            r(354, &[]),
            r(250, &["queued"]),
        ],
    )
    .unwrap();
    assert_eq!(
        written,
        format!(
            "EHLO example.com\r\nSTARTTLS\r\n<TLS>\nEHLO example.com\r\nAUTH PLAIN {}\r\n\
             MAIL FROM:<clerk@example.com> SIZE=18\r\nRCPT TO:<pat@example.com>\r\nDATA\r\n\
             Subject: x\r\n\r\nhi\r\n.\r\nQUIT\r\n",
            BASE64.encode("\0clerk\0pw")
        )
    );
}

#[test]
fn login_implicit_tls_and_no_sign_in_all_work() {
    let login = run(
        SessionOptions {
            security: Security::Tls,
            ..options()
        },
        vec![
            r(220, &[]),
            r(250, &["mx", "AUTH LOGIN"]),
            r(334, &["VXNlcm5hbWU6"]),
            r(334, &["UGFzc3dvcmQ6"]),
            r(235, &[]),
        ],
    )
    .unwrap();
    assert_eq!(
        login,
        format!(
            "EHLO example.com\r\nAUTH LOGIN\r\n{}\r\n{}\r\nMAIL FROM:<clerk@example.com>\r\n",
            BASE64.encode("clerk"),
            BASE64.encode("pw")
        )
    );
    let open = run(
        SessionOptions {
            security: Security::None,
            credentials: None,
            recipients: vec!["a@example.com".into(), "b@example.com".into()],
            ..options()
        },
        vec![
            r(220, &[]),
            r(250, &["mx", "STARTTLS"]),
            r(250, &[]),
            r(251, &[]),
            r(250, &[]),
        ],
    )
    .unwrap();
    assert_eq!(
        open,
        "EHLO example.com\r\nMAIL FROM:<clerk@example.com>\r\nRCPT TO:<a@example.com>\r\n\
         RCPT TO:<b@example.com>\r\nDATA\r\n"
    );
}

#[test]
fn session_state_is_reported() {
    let mut session = Session::new(SessionOptions {
        security: Security::Tls,
        credentials: None,
        ..options()
    })
    .unwrap();
    assert!(session.is_tls());
    for reply in [
        reply(220, &[]),
        reply(250, &[]),
        reply(250, &[]),
        reply(250, &[]),
        reply(354, &[]),
    ] {
        session.on_reply(&reply).unwrap();
        assert!(!session.finished());
    }
    assert_eq!(
        session.on_reply(&reply(250, &[])).unwrap(),
        Step::Done("QUIT\r\n".into())
    );
    assert!(session.finished());
}

#[test]
fn credentials_never_go_out_before_tls() {
    fails_with(
        run(
            SessionOptions {
                security: Security::None,
                ..options()
            },
            vec![],
        ),
        PluginErrorCode::InvalidParams,
    );
    fails_with(
        run(
            SessionOptions {
                recipients: vec![],
                ..options()
            },
            vec![],
        ),
        PluginErrorCode::InvalidParams,
    );
    // A server (or a meddler) that hides STARTTLS gets no password.
    let message = fails_with(
        run(options(), vec![r(220, &[]), r(250, &["mx", "AUTH PLAIN"])]),
        PluginErrorCode::InvalidParams,
    );
    assert!(message.contains("STARTTLS"), "{message}");
    fails_with(
        run(
            SessionOptions {
                security: Security::Tls,
                ..options()
            },
            vec![r(220, &[]), r(250, &["mx", "AUTH CRAM-MD5"])],
        ),
        PluginErrorCode::InvalidParams,
    );
}

#[test]
fn refusals_map_to_retried_or_permanent_codes() {
    use PluginErrorCode::{Forbidden, InvalidParams, NotFound, Unavailable};
    let tls = || SessionOptions {
        security: Security::Tls,
        ..options()
    };
    let open = || SessionOptions {
        security: Security::Tls,
        credentials: None,
        ..options()
    };
    let cases: Vec<(SessionOptions, Vec<Event>, PluginErrorCode)> = vec![
        (options(), vec![r(421, &[])], Unavailable),
        (options(), vec![r(554, &[])], InvalidParams),
        (options(), vec![r(220, &[]), r(502, &[])], InvalidParams),
        (
            options(),
            vec![r(220, &[]), r(250, &["mx", "STARTTLS"]), r(454, &[])],
            Unavailable,
        ),
        (
            tls(),
            vec![r(220, &[]), r(250, &["mx", "AUTH PLAIN"]), r(535, &[])],
            Forbidden,
        ),
        (
            tls(),
            vec![r(220, &[]), r(250, &["mx", "AUTH PLAIN"]), r(454, &[])],
            Unavailable,
        ),
        (
            tls(),
            vec![r(220, &[]), r(250, &["mx", "AUTH LOGIN"]), r(535, &[])],
            Forbidden,
        ),
        (
            tls(),
            vec![
                r(220, &[]),
                r(250, &["mx", "AUTH LOGIN"]),
                r(334, &[]),
                r(535, &[]),
            ],
            Forbidden,
        ),
        (
            tls(),
            vec![
                r(220, &[]),
                r(250, &["mx", "AUTH LOGIN"]),
                r(334, &[]),
                r(334, &[]),
                r(535, &[]),
            ],
            Forbidden,
        ),
        (
            open(),
            vec![r(220, &[]), r(250, &[]), r(451, &[])],
            Unavailable,
        ),
        (
            open(),
            vec![r(220, &[]), r(250, &[]), r(250, &[]), r(550, &[])],
            NotFound,
        ),
        (
            open(),
            vec![r(220, &[]), r(250, &[]), r(250, &[]), r(552, &[])],
            InvalidParams,
        ),
        (
            open(),
            vec![
                r(220, &[]),
                r(250, &[]),
                r(250, &[]),
                r(250, &[]),
                r(554, &[]),
            ],
            InvalidParams,
        ),
        (
            open(),
            vec![
                r(220, &[]),
                r(250, &[]),
                r(250, &[]),
                r(250, &[]),
                r(354, &[]),
                r(452, &[]),
            ],
            Unavailable,
        ),
        (
            open(),
            vec![
                r(220, &[]),
                r(250, &[]),
                r(250, &[]),
                r(250, &[]),
                r(354, &[]),
                r(250, &[]),
                r(250, &[]),
            ],
            Unavailable,
        ),
        (
            open(),
            vec![r(220, &[]), r(250, &[]), r(334, &[])],
            Unavailable,
        ),
        (
            open(),
            vec![r(220, &[]), r(250, &["x", "SIZE 5"])],
            InvalidParams,
        ),
        (
            open(),
            vec![r(220, &[]), r(250, &[]), r(399, &[])],
            Unavailable,
        ),
        (
            options(),
            vec![
                r(220, &[]),
                r(250, &["mx", "STARTTLS"]),
                r(220, &[]),
                r(250, &[]),
            ],
            Unavailable,
        ),
        (options(), vec![Event::Tls], Unavailable),
    ];
    for (opts, events, code) in cases {
        fails_with(run(opts, events), code);
    }
}

#[test]
fn credentials_debug_is_redacted() {
    let shown = format!(
        "{:?}",
        Credentials {
            username: "clerk".into(),
            password: "hunter2".into(),
        }
    );
    assert!(
        !shown.contains("hunter2") && !shown.contains("clerk"),
        "{shown}"
    );
}
