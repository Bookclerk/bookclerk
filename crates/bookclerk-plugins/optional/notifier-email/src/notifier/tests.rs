//! Unit tests for the settings, the entrypoint and the message layout.

// Test code: a failed expectation is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use bookclerk_plugin_sdk::{
    NotificationAlertAction, NotificationField, NotificationMessage, NotificationSeverity,
    NotifyDestination, PluginErrorCode, SettingNetwork, SettingNetworkGrant,
};
use serde_json::json;
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::*;
use crate::client::BoxIo;
use crate::compose::{html_body, plain_body};

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
    }
}

#[test]
fn bodies_carry_text_fields_and_the_link_once() {
    assert_eq!(
        plain_body(&message()),
        "Pat invited you.\n\nUse the link below.\n\nLibrary: Home <books>\n\nhttps://library.example.com/claim?t=a&b\n"
    );
    let html = html_body(&message());
    assert!(
        html.contains("<p>Pat invited you.</p>\n<p>Use the link below.</p>"),
        "{html}"
    );
    assert!(
        html.contains("<th align=\"left\">Library</th><td>Home &lt;books&gt;</td>"),
        "{html}"
    );
    assert!(html.contains(
        "<a href=\"https://library.example.com/claim?t=a&amp;b\">https://library.example.com/claim?t=a&amp;b</a>"
    ));
    // A link the text or HTML already holds is not repeated.
    let mut inline = message();
    inline.text = "Open https://library.example.com/claim?t=a&b now".into();
    inline.html =
        Some("<p><a href=\"https://library.example.com/claim?t=a&amp;b\">Open</a></p>".into());
    inline.fields.clear();
    assert_eq!(
        plain_body(&inline),
        "Open https://library.example.com/claim?t=a&b now\n"
    );
    let html = html_body(&inline);
    assert_eq!(html.matches("href=").count(), 1, "{html}");
    let mut plain = message();
    plain.link = None;
    plain.fields.clear();
    plain.text = "one\r\ntwo\n\n\n".into();
    assert_eq!(plain_body(&plain), "one\r\ntwo\n");
    assert!(html_body(&plain).contains("<p>one<br>two</p>\n</body>"));
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
    let text_part = mail.split("\r\n\r\n").nth(2).unwrap();
    let text = BASE64
        .decode(
            text_part
                .split("\r\n--")
                .next()
                .unwrap()
                .replace("\r\n", ""),
        )
        .unwrap();
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
