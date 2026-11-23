//! Unit tests for the byte mover, against a scripted server on an
//! in-memory pipe.

// Test code: a failed expectation is the test failing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::rc::Rc;

use bookclerk_plugin_sdk::PluginErrorCode;
use tokio::io::{duplex, AsyncBufReadExt, BufReader, DuplexStream};

use super::*;
use crate::smtp::{Credentials, SessionOptions};

/// A transport over one in-memory pipe; `start_tls` keeps the pipe and
/// records the upgrade.
struct PipeTransport {
    /// The client end, taken by `connect`.
    client: RefCell<Option<DuplexStream>>,
    /// What happened, in order.
    log: Rc<RefCell<Vec<String>>>,
    /// Make `connect` fail.
    refuse: bool,
}

#[async_trait(?Send)]
impl Transport for PipeTransport {
    async fn connect(&self, server: &HostPort, security: Security) -> Result<BoxIo, PluginError> {
        self.log.borrow_mut().push(format!(
            "connect {}:{} {security:?}",
            server.hostname, server.port
        ));
        if self.refuse {
            return Err(PluginError::unavailable("the server could not be reached"));
        }
        Ok(Box::new(
            self.client.borrow_mut().take().expect("one connection"),
        ))
    }

    async fn start_tls(&self, hostname: &str, io: BoxIo) -> Result<BoxIo, PluginError> {
        self.log.borrow_mut().push(format!("tls {hostname}"));
        Ok(io)
    }
}

/// A server that answers each line it reads with the next scripted reply,
/// and records what it read; the message after DATA counts as one line.
async fn scripted(
    stream: DuplexStream,
    greeting: &str,
    replies: Vec<&'static str>,
    log: Rc<RefCell<Vec<String>>>,
) {
    let (read, mut write) = tokio::io::split(stream);
    let mut lines = BufReader::new(read);
    if write.write_all(greeting.as_bytes()).await.is_err() {
        return;
    }
    let mut in_data = false;
    for reply in replies {
        let mut got = String::new();
        loop {
            let mut line = String::new();
            if lines.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            got.push_str(&line);
            if !in_data || got.ends_with("\r\n.\r\n") {
                break;
            }
        }
        log.borrow_mut().push(format!("C: {}", got.trim_end()));
        in_data = reply.starts_with("354");
        if write.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Session options for the tests.
fn session(security: Security) -> Session {
    Session::new(SessionOptions {
        security,
        helo_name: "example.com".into(),
        credentials: (security != Security::None).then(|| Credentials {
            username: "clerk".into(),
            password: "pw".into(),
        }),
        from: "clerk@example.com".into(),
        recipients: vec!["pat@example.com".into()],
        message: "Subject: x\r\n\r\n.hi\r\n".into(),
    })
    .unwrap()
}

/// The approved server.
fn server() -> HostPort {
    HostPort {
        hostname: "smtp.example.com".into(),
        port: 587,
    }
}

/// Runs one conversation against a scripted server.
async fn talk(
    security: Security,
    greeting: &str,
    replies: Vec<&'static str>,
) -> (Result<(), PluginError>, Vec<String>) {
    let (client, far) = duplex(64 * 1024);
    let log = Rc::new(RefCell::new(Vec::new()));
    let transport = PipeTransport {
        client: RefCell::new(Some(client)),
        log: Rc::clone(&log),
        refuse: false,
    };
    let mut session = session(security);
    let server = server();
    let (result, ()) = tokio::join!(
        converse(&transport, &server, security, &mut session, STEP_TIMEOUT),
        scripted(far, greeting, replies, Rc::clone(&log)),
    );
    let log = log.borrow().clone();
    (result, log)
}

#[tokio::test]
async fn a_starttls_submission_upgrades_before_signing_in() {
    let (result, log) = talk(
        Security::StartTls,
        "220 mx ready\r\n",
        vec![
            "250-mx\r\n250 STARTTLS\r\n",
            "220 go\r\n",
            "250-mx\r\n250 AUTH PLAIN\r\n",
            "235 ok\r\n",
            "250 ok\r\n",
            "250 ok\r\n",
            "354 go\r\n",
            "250 queued\r\n",
            "221 bye\r\n",
        ],
    )
    .await;
    result.unwrap();
    assert_eq!(
        log,
        [
            "connect smtp.example.com:587 StartTls",
            "C: EHLO example.com",
            "C: STARTTLS",
            "tls smtp.example.com",
            "C: EHLO example.com",
            &format!("C: AUTH PLAIN {}", BASE64_PLAIN),
            "C: MAIL FROM:<clerk@example.com>",
            "C: RCPT TO:<pat@example.com>",
            "C: DATA",
            "C: Subject: x\r\n\r\n..hi\r\n.",
            "C: QUIT",
        ]
    );
}

/// `AUTH PLAIN` for `clerk` / `pw`.
const BASE64_PLAIN: &str = "AGNsZXJrAHB3";

#[tokio::test]
async fn implicit_tls_asks_the_transport_for_it() {
    let (result, log) = talk(
        Security::Tls,
        "220 mx\r\n",
        vec![
            "250-mx\r\n250 AUTH PLAIN\r\n",
            "235 ok\r\n",
            "250 ok\r\n",
            "250 ok\r\n",
            "354 go\r\n",
            "250 queued\r\n",
        ],
    )
    .await;
    result.unwrap();
    assert_eq!(log[0], "connect smtp.example.com:587 Tls");
    assert!(!log.iter().any(|line| line.starts_with("tls ")));
}

#[tokio::test]
async fn data_sent_before_tls_is_refused() {
    // A 220 with a command smuggled after it in the clear (the STARTTLS
    // injection attack): the session stops before TLS.
    let (result, log) = talk(
        Security::StartTls,
        "220 mx\r\n",
        vec!["250-mx\r\n250 STARTTLS\r\n", "220 go\r\n250 injected\r\n"],
    )
    .await;
    let err = result.unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unavailable);
    assert!(err.message.contains("before TLS"), "{}", err.message);
    assert!(!log.iter().any(|line| line.starts_with("tls ")));
}

#[tokio::test]
async fn a_closed_connection_or_refusal_is_reported() {
    let (result, _) = talk(Security::StartTls, "220 mx\r\n", vec![]).await;
    assert_eq!(result.unwrap_err().code, PluginErrorCode::Unavailable);
    let (result, _) = talk(Security::StartTls, "554 go away\r\n", vec![]).await;
    assert_eq!(result.unwrap_err().code, PluginErrorCode::InvalidParams);
    let (result, _) = talk(Security::StartTls, "garbage\r\n", vec![]).await;
    assert_eq!(result.unwrap_err().code, PluginErrorCode::Unavailable);

    let transport = PipeTransport {
        client: RefCell::new(None),
        log: Rc::new(RefCell::new(Vec::new())),
        refuse: true,
    };
    let mut session = session(Security::StartTls);
    let err = converse(
        &transport,
        &server(),
        Security::StartTls,
        &mut session,
        STEP_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Unavailable);
}

#[tokio::test]
async fn a_flood_without_line_ends_is_refused() {
    let flood: &'static str = Box::leak("2".repeat(MAX_BUFFERED_BYTES + 10).into_boxed_str());
    let (result, _) = talk(Security::StartTls, flood, vec![]).await;
    assert_eq!(result.unwrap_err().code, PluginErrorCode::Unavailable);
}

#[tokio::test(start_paused = true)]
async fn a_silent_server_times_out() {
    let (client, far) = duplex(1024);
    let transport = PipeTransport {
        client: RefCell::new(Some(client)),
        log: Rc::new(RefCell::new(Vec::new())),
        refuse: false,
    };
    let mut session = session(Security::StartTls);
    let err = converse(
        &transport,
        &server(),
        Security::StartTls,
        &mut session,
        STEP_TIMEOUT,
    )
    .await
    .unwrap_err();
    drop(far);
    assert_eq!(err.code, PluginErrorCode::Unavailable);
    assert!(err.message.contains("in time"), "{}", err.message);
}

#[test]
fn refused_tunnels_name_the_grant() {
    assert_eq!(
        refused("socket proxy refused smtp.example.com:587: HTTP/1.1 403 Forbidden").code,
        PluginErrorCode::NetworkNotGranted
    );
    assert_eq!(
        refused("connection reset").code,
        PluginErrorCode::Unavailable
    );
}
