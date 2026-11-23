//! Unit tests for the settings, the entrypoint and the message layout.

// Test code: a failed expectation is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use bookclerk_plugin_sdk::{
    NotificationAlertAction, NotificationField, NotificationMessage, NotificationSeverity,
    NotifyDestination, PluginErrorCode, SettingNetwork, SettingNetworkGrant,
};
use serde_json::json;
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::*;
use crate::checkers::Checkers;
use crate::client::BoxIo;
use crate::compose::{html_body, link_host, link_label, plain_body, simple_html};

/// A grant whose `server` setting is in `state` at `destination`.
fn grant(state: SettingGrantState, destination: &str) -> NetworkGrant {
    NetworkGrant {
        outbound: true,
        settings: vec![SettingNetworkGrant {
            key: SERVER_SETTING.into(),
            network: SettingNetwork::Tcp,
            destination: destination.into(),
            state,
            error: String::new(),
        }],
        ..NetworkGrant::default()
    }
}

/// An approved grant for `smtp.example.com:587`.
fn approved() -> NetworkGrant {
    grant(SettingGrantState::Approved, "smtp.example.com:587")
}

/// Complete settings for a STARTTLS server with a sign-in.
fn config() -> Value {
    json!({
        "server": "smtp.example.com",
        "security": "starttls",
        "username": " clerk ",
        "from_address": "Clerk@Example.com",
        "from_name": "Bookclerk",
        "to": "ops@example.com, oncall@example.com",
    })
}

#[test]
fn settings_are_read_from_config_secrets_and_the_grant() {
    let settings = Settings::read(&config(), &json!({ "password": "pw" }), &approved()).unwrap();
    assert_eq!(settings.server.hostname, "smtp.example.com");
    assert_eq!(settings.server.port, 587);
    assert_eq!(settings.security, Security::StartTls);
    let creds = settings.credentials.unwrap();
    assert_eq!(
        (creds.username.as_str(), creds.password.as_str()),
        ("clerk", "pw")
    );
    assert_eq!(settings.from, "Clerk@example.com");
    assert_eq!(settings.from_name, "Bookclerk");

    let mut bare = config();
    bare["security"] = json!("");
    bare["username"] = json!("");
    let settings = Settings::read(&bare, &json!({}), &approved()).unwrap();
    assert_eq!(settings.security, Security::StartTls);
    assert!(settings.credentials.is_none());
}

#[test]
fn unusable_settings_say_why() {
    let secrets = json!({});
    let code = |config: &Value, network: &NetworkGrant| {
        Settings::read(config, &secrets, network).unwrap_err().code
    };
    assert_eq!(
        code(&config(), &NetworkGrant::default()),
        PluginErrorCode::InvalidParams
    );
    assert_eq!(
        code(&config(), &grant(SettingGrantState::Unset, "")),
        PluginErrorCode::InvalidParams
    );
    assert_eq!(
        code(&config(), &grant(SettingGrantState::Invalid, "")),
        PluginErrorCode::InvalidParams
    );
    assert_eq!(
        code(
            &config(),
            &grant(SettingGrantState::Pending, "smtp.example.com:587")
        ),
        PluginErrorCode::NetworkNotGranted
    );
    assert_eq!(
        code(
            &config(),
            &grant(SettingGrantState::Approved, "smtp.example.com")
        ),
        PluginErrorCode::InvalidParams
    );
    let mut none = config();
    none["security"] = json!("none");
    assert_eq!(code(&none, &approved()), PluginErrorCode::InvalidParams);
    let mut ssl = config();
    ssl["security"] = json!("ssl");
    assert_eq!(code(&ssl, &approved()), PluginErrorCode::InvalidParams);
    let mut no_from = config();
    no_from["from_address"] = json!("clerk");
    assert_eq!(code(&no_from, &approved()), PluginErrorCode::InvalidParams);
}

/// A rendered message.
fn message() -> NotificationMessage {
    NotificationMessage {
        notification_id: "n-7".into(),
        notification_type: "invite".into(),
        severity: NotificationSeverity::Info,
        title: "You're invited".into(),
        text: "Pat invited you.\n\nUse the link below.".into(),
        markdown: None,
        html: None,
        link: Some("https://library.example.com/claim?t=a&b".into()),
        fields: vec![NotificationField {
            name: "Library".into(),
            value: "Home <books>".into(),
        }],
        group_key: None,
        alert_action: NotificationAlertAction::None,
        alert_key: None,
        sensitive: true,
        library_origin: Some("https://library.example.com/".into()),
    }
}

#[test]
fn bodies_carry_text_fields_and_the_link_once() {
    assert_eq!(
        plain_body(&message(), "Riverside Library"),
        "Pat invited you.\n\nUse the link below.\n\nLibrary: Home <books>\n\nOpen Riverside Library: https://library.example.com/claim?t=a&b\n\n-- \nSent by your library at https://library.example.com/ with Bookclerk (https://bookclerk.app). This is an automatic message.\n"
    );
    let (html, branded) = html_body(&message(), "Riverside Library");
    assert!(branded);
    assert!(
        html.to_ascii_lowercase()
            .starts_with("<!doctype html><html lang=\"en\""),
        "{html}"
    );
    assert!(html.contains("<title>You&#39;re invited</title>"), "{html}");
    assert!(html.contains("Pat invited you."), "{html}");
    assert!(html.contains("Use the link below."), "{html}");
    assert!(html.contains(">Library</th>"), "{html}");
    assert!(html.contains("Home &lt;books&gt;"), "{html}");
    assert!(html.contains("Riverside Library"), "{html}");
    assert!(html.contains("/>Bookclerk</div>"), "{html}");
    assert!(html.contains("href=\"https://bookclerk.app\""), "{html}");
    assert!(
        html.contains("href=\"https://library.example.com/\""),
        "{html}"
    );
    assert!(html.contains("src=\"cid:mark@bookclerk\""), "{html}");
    // The link is a button with a short label naming the library; the long
    // URL is only in the href.
    assert!(html.contains("href=\"https://library.example.com/claim?t=a&amp;b\""));
    assert!(html.contains(">Open Riverside Library</a>"), "{html}");
    assert_eq!(html.matches("claim?t=a").count(), 1, "{html}");
    // Nothing is fetched to show it: no web fonts or remote images.
    assert!(!html.contains("fonts.googleapis"), "{html}");
    assert!(!html.contains("src=\"http"), "{html}");
    // A link the text or HTML already holds is not repeated.
    let mut inline = message();
    inline.text = "Open https://library.example.com/claim?t=a&b now".into();
    inline.html =
        Some("<p><a href=\"https://library.example.com/claim?t=a&amp;b\">Open</a></p>".into());
    inline.fields.clear();
    assert_eq!(
        plain_body(&inline, "Riverside Library"),
        "Open https://library.example.com/claim?t=a&b now\n\n-- \nSent by your library at https://library.example.com/ with Bookclerk (https://bookclerk.app). This is an automatic message.\n"
    );
    let (html, _) = html_body(&inline, "");
    assert_eq!(
        html.matches("href=\"https://library.example.com/claim")
            .count(),
        1,
        "{html}"
    );
    let mut plain = message();
    plain.link = None;
    plain.fields.clear();
    plain.text = "one\r\ntwo\n\n\n".into();
    assert_eq!(plain_body(&plain, "x"), "one\r\ntwo\n\n-- \nSent by your library at https://library.example.com/ with Bookclerk (https://bookclerk.app). This is an automatic message.\n");
    let (h, _) = html_body(&plain, "x");
    assert!(
        h.contains("one<br>two") || h.contains("one<br />two"),
        "{h}"
    );
    let simple = simple_html(&message(), "Bookclerk");
    assert!(simple.starts_with("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>You&#39;re invited</title>"));
    assert!(simple.contains("<p>Pat invited you.</p>\n<p>Use the link below.</p>"));
    assert!(simple.contains("<th align=\"left\">Library</th><td>Home &lt;books&gt;</td>"));
    assert!(simple
        .contains("<a href=\"https://library.example.com/claim?t=a&amp;b\">Open your library</a>"));
}

#[test]
fn link_labels_name_the_library() {
    assert_eq!(
        link_host("https://a.example:8443/x?y#z"),
        Some("a.example:8443")
    );
    assert_eq!(link_host("https://user@a.example/"), Some("a.example"));
    assert_eq!(link_host("not a url"), None);
    assert_eq!(link_host("https:///path"), None);
    assert_eq!(link_label(" Riverside Library "), "Open Riverside Library");
    assert_eq!(link_label("bookclerk"), "Open your library");
    assert_eq!(link_label(""), "Open your library");
}

#[test]
fn only_invites_introduce_bookclerk() {
    let mut invite = message();
    assert!(!plain_body(&invite, "x").contains("New to Bookclerk?"));
    assert!(!html_body(&invite, "x").0.contains("New to Bookclerk?"));
    invite.notification_type = "account_invite".into();
    let text = plain_body(&invite, "x");
    assert!(text.contains("New to Bookclerk?"), "{text}");
    assert!(text.contains("multiple audiobook storefronts"), "{text}");
    let (html, _) = html_body(&invite, "x");
    assert!(html.contains("New to Bookclerk?"), "{html}");
}

#[test]
fn host_html_mjml_refuses_falls_back_to_the_plain_document() {
    let mut odd = message();
    odd.html = Some("<mj-section>".into());
    let (html, branded) = html_body(&odd, "x");
    if !branded {
        assert!(html.starts_with("<!doctype html>"));
    }
}

/// A transport that hands out one pipe and records what the server read.
struct Pipe(RefCell<Option<DuplexStream>>);

#[async_trait(?Send)]
impl Transport for Pipe {
    async fn connect(&self, _server: &HostPort, _security: Security) -> Result<BoxIo, PluginError> {
        Ok(Box::new(
            self.0.borrow_mut().take().expect("one connection"),
        ))
    }

    async fn start_tls(&self, _hostname: &str, io: BoxIo) -> Result<BoxIo, PluginError> {
        Ok(io)
    }
}

/// An accepting server with no sign-in and no TLS; returns the envelope
/// commands and the message it received.
async fn accepting(stream: DuplexStream, seen: Rc<RefCell<Vec<String>>>) {
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read);
    let _ = write.write_all(b"220 mx\r\n").await;
    let mut data = false;
    loop {
        let mut line = String::new();
        if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        if data {
            seen.borrow_mut().push(line.clone());
            if line == ".\r\n" {
                data = false;
                let _ = write.write_all(b"250 queued\r\n").await;
            }
            continue;
        }
        seen.borrow_mut().push(line.trim_end().to_owned());
        let reply: &[u8] = match line.split_whitespace().next().unwrap_or_default() {
            "DATA" => {
                data = true;
                b"354 go\r\n"
            }
            "QUIT" => return,
            _ => b"250 ok\r\n",
        };
        let _ = write.write_all(reply).await;
    }
}

/// Sends `message` to `destination` through an accepting server.
async fn send_through(
    destination: NotifyDestination,
) -> (Result<NotifySent, PluginError>, Vec<String>) {
    let mut open = config();
    open["security"] = json!("none");
    open["username"] = json!("");
    let settings = Settings::read(&open, &json!({}), &approved());
    let (client, far) = duplex(64 * 1024);
    let notifier = EmailNotifier::new(settings, Pipe(RefCell::new(Some(client))))
        .with_fixed("=_b", 1_795_410_000);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (result, ()) = tokio::join!(
        notifier.send(NotifySendParams {
            message: message(),
            destination,
        }),
        accepting(far, Rc::clone(&seen)),
    );
    let seen = seen.borrow().clone();
    (result, seen)
}

#[tokio::test]
async fn a_personal_send_goes_to_the_patron() {
    let (result, seen) = send_through(NotifyDestination {
        kind: NotifyDestinationKind::Personal,
        destination_id: "p1".into(),
        address_json: r#"{"email":"Pat@Example.org"}"#.into(),
    })
    .await;
    assert_eq!(result.unwrap(), NotifySent { provider_id: None });
    assert_eq!(seen[0], "EHLO example.com");
    assert_eq!(seen[1], "MAIL FROM:<Clerk@example.com>");
    assert_eq!(seen[2], "RCPT TO:<Pat@example.org>");
    assert_eq!(seen[3], "DATA");
    let mail: String = seen[4..].concat();
    assert!(mail.starts_with("From: \"Bookclerk\" <Clerk@example.com>\r\nTo: Pat@example.org\r\n"));
    assert!(mail.contains("\r\nSubject: You're invited\r\n"));
    assert!(mail.contains("\r\nDate: Mon, 23 Nov 2026 05:00:00 +0000\r\n"));
    assert!(mail.contains("\r\nMessage-ID: <n-7.p1@example.com>\r\n"));
    assert!(mail.contains("boundary=\"=_b\""));
    let text_part = mail
        .split_once("Content-Transfer-Encoding: quoted-printable\r\n\r\n")
        .unwrap()
        .1;
    let text = crate::smtp::tests::decode_qp(text_part.split("\r\n--=_b").next().unwrap());
    assert!(String::from_utf8(text)
        .unwrap()
        .contains("https://library.example.com/claim?t=a&b"));
    assert_eq!(seen.last().unwrap(), "QUIT");
}

#[tokio::test]
async fn a_channel_send_goes_to_the_to_setting() {
    let (result, seen) = send_through(NotifyDestination {
        kind: NotifyDestinationKind::Channel,
        destination_id: "c1".into(),
        address_json: String::new(),
    })
    .await;
    result.unwrap();
    assert_eq!(seen[2], "RCPT TO:<ops@example.com>");
    assert_eq!(seen[3], "RCPT TO:<oncall@example.com>");
}

#[tokio::test]
async fn health_and_validation_never_connect() {
    let unused = || Pipe(RefCell::new(None));
    let ok = EmailNotifier::new(Settings::read(&config(), &json!({}), &approved()), unused());
    assert_eq!(
        ok.health().await.unwrap(),
        HealthOk {
            ok: true,
            detail: String::new()
        }
    );
    let mut no_to = config();
    no_to["to"] = json!("  ");
    assert!(
        EmailNotifier::new(Settings::read(&no_to, &json!({}), &approved()), unused())
            .health()
            .await
            .unwrap()
            .ok
    );
    let mut bad_to = config();
    bad_to["to"] = json!("nope");
    let health = EmailNotifier::new(Settings::read(&bad_to, &json!({}), &approved()), unused())
        .health()
        .await
        .unwrap();
    assert!(
        !health.ok && health.detail.contains("to setting"),
        "{}",
        health.detail
    );
    // A fresh install with no server is healthy, just not set up; a send
    // still fails.
    let unset = EmailNotifier::new(
        Settings::read(&config(), &json!({}), &NetworkGrant::default()),
        unused(),
    );
    assert_eq!(
        unset.health().await.unwrap(),
        HealthOk {
            ok: true,
            detail: "no mail server set yet".to_owned()
        }
    );
    let pending = EmailNotifier::new(
        Settings::read(
            &config(),
            &json!({}),
            &grant(SettingGrantState::Pending, "x:1"),
        ),
        unused(),
    );
    let health = pending.health().await.unwrap();
    assert!(
        !health.ok && health.detail.contains("approval"),
        "{}",
        health.detail
    );
    let err = pending
        .send(NotifySendParams {
            message: message(),
            destination: NotifyDestination {
                kind: NotifyDestinationKind::Channel,
                destination_id: "c1".into(),
                address_json: String::new(),
            },
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::NetworkNotGranted);

    assert_eq!(
        ok.validate_destination(NotifyValidateParams {
            address_json: r#"{"email":" Pat@Example.org "}"#.into(),
        })
        .await
        .unwrap(),
        NotifyValidated {
            address_json: r#"{"email":"Pat@example.org"}"#.into(),
            label: Some("Pat@example.org".into()),
        }
    );
    assert_eq!(
        ok.validate_destination(NotifyValidateParams {
            address_json: "{}".into(),
        })
        .await
        .unwrap_err()
        .code,
        PluginErrorCode::InvalidParams
    );
}

#[test]
fn the_default_clock_and_boundary_are_live() {
    let notifier = EmailNotifier::new(
        Err(PluginError::invalid_params("x")),
        Pipe(RefCell::new(None)),
    );
    assert!((notifier.now)() > 1_700_000_000);
    let (a, b) = ((notifier.boundary)(), (notifier.boundary)());
    assert!(a.starts_with("=_bookclerk_") && a != b);
}

/// Canned checker answers; records what was asked.
struct Canned {
    /// Every call, in order.
    calls: Rc<RefCell<Vec<String>>>,
    /// mail-tester answers, first to last; the last repeats.
    mail_tester: RefCell<Vec<Result<String, String>>>,
}

#[async_trait(?Send)]
impl Checkers for Canned {
    async fn spamcheck(&self, raw: &str) -> Result<String, String> {
        self.calls.borrow_mut().push("spamcheck".into());
        assert!(raw.contains("\r\nTo: Clerk@example.com\r\n"), "{raw}");
        Ok(r#"{"success":true,"score":"0.1","rules":[],"report":""}"#.into())
    }

    async fn txt(&self, name: &str) -> Result<String, String> {
        self.calls.borrow_mut().push(format!("txt {name}"));
        Ok(if name.starts_with("_dmarc.") {
            r#"{"Status":3}"#.into()
        } else {
            r#"{"Status":0,"Answer":[{"type":16,"data":"\"v=spf1 mx -all\""}]}"#.into()
        })
    }

    async fn mail_tester(&self, id: &str) -> Result<String, String> {
        self.calls.borrow_mut().push(format!("mail-tester {id}"));
        let mut answers = self.mail_tester.borrow_mut();
        if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0].clone()
        }
    }

    async fn pause(&self, _wait: std::time::Duration) {}
}

/// Settings for an open relay with a mail-tester account.
fn checked_settings(account: &str) -> Result<Settings, PluginError> {
    let mut open = config();
    open["security"] = json!("none");
    open["username"] = json!("");
    open["mail_tester_account"] = json!(account);
    Settings::read(&open, &json!({}), &approved())
}

#[tokio::test]
async fn a_deliverability_check_runs_every_checker_and_merges_advice() {
    let (client, far) = duplex(64 * 1024);
    let calls = Rc::new(RefCell::new(Vec::new()));
    let notifier = EmailNotifier::new(
        checked_settings("acct"),
        Pipe(RefCell::new(Some(client))),
    )
    .with_fixed("=_b", 1_795_410_000)
    .with_checkers(
        Box::new(Canned {
            calls: Rc::clone(&calls),
            mail_tester: RefCell::new(vec![
                Ok(r#"{"status":false}"#.into()),
                Ok(r#"{"mark":9.6,"signature":{"title":"Not signed with DKIM","statusClass":"warning"}}"#.into()),
            ]),
        }),
        "tok",
    );
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (report, ()) = tokio::join!(
        notifier.check_deliverability(NotifyDeliverabilityParams { message: message() }),
        accepting(far, Rc::clone(&seen)),
    );
    let report = report.unwrap();
    assert_eq!(
        *calls.borrow(),
        [
            "spamcheck",
            "txt example.com",
            "txt _dmarc.example.com",
            "mail-tester acct-tok",
            "mail-tester acct-tok",
        ]
    );
    assert!(seen
        .borrow()
        .iter()
        .any(|line| line == "RCPT TO:<acct-tok@srv1.mail-tester.com>"));
    let outcomes: Vec<NotifyCheckOutcome> = report.checks.iter().map(|c| c.outcome).collect();
    assert_eq!(
        outcomes,
        [
            NotifyCheckOutcome::Passed,
            NotifyCheckOutcome::Warning,
            NotifyCheckOutcome::Passed
        ]
    );
    assert_eq!(
        report.checks[2].report_url.as_deref(),
        Some("https://www.mail-tester.com/acct-tok")
    );
    let titles: Vec<&str> = report.advice.iter().map(|a| a.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Publish a DMARC policy for example.com",
            "Not signed with DKIM",
            "The message content scores well",
            "example.com has an SPF record",
        ]
    );
}

#[tokio::test]
async fn a_check_without_an_account_skips_mail_tester_and_a_refused_send_is_advice() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let canned = |calls: &Rc<RefCell<Vec<String>>>| {
        Box::new(Canned {
            calls: Rc::clone(calls),
            mail_tester: RefCell::new(vec![Err("unused".into())]),
        })
    };
    let (client, _far) = duplex(64);
    let notifier = EmailNotifier::new(checked_settings(""), Pipe(RefCell::new(Some(client))))
        .with_checkers(canned(&calls), "tok");
    let report = notifier
        .check_deliverability(NotifyDeliverabilityParams { message: message() })
        .await
        .unwrap();
    assert_eq!(report.checks[2].outcome, NotifyCheckOutcome::Skipped);
    assert!(report.checks[2].summary.contains("account name"));
    assert_eq!(calls.borrow().len(), 3);

    // A server that hangs up refuses the test: mail-tester is not polled and
    // the advice says to fix the server settings first.
    let (client, far) = duplex(64);
    drop(far);
    let notifier = EmailNotifier::new(checked_settings("acct"), Pipe(RefCell::new(Some(client))))
        .with_checkers(canned(&calls), "tok");
    let report = notifier
        .check_deliverability(NotifyDeliverabilityParams { message: message() })
        .await
        .unwrap();
    assert_eq!(report.checks[2].outcome, NotifyCheckOutcome::Skipped);
    assert_eq!(report.advice[0].title, "Fix the mail server settings");
    assert_eq!(report.advice[0].outcome, NotifyCheckOutcome::Failed);
    assert!(!calls.borrow().iter().any(|c| c.starts_with("mail-tester")));

    assert!(checked_settings("bad name!").is_err());
    assert!(checked_settings(&"a".repeat(41)).is_err());
    let unset = EmailNotifier::new(
        Err(PluginError::invalid_params("x")),
        Pipe(RefCell::new(None)),
    );
    assert!(unset
        .check_deliverability(NotifyDeliverabilityParams { message: message() })
        .await
        .is_err());
}
