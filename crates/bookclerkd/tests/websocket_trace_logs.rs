//! The daemon's log setup keeps WebSocket frame payloads out of every sink.
//!
//! The WebSocket library traces each frame it reads and writes with the
//! payload spelled in hex, and a sink could be set to keep TRACE. A clerk
//! socket's `copy.granted` frame carries a bearer token as text, so those
//! records must reach no sink. This is its own test binary because the
//! `log` logger and the subscriber are process-wide: it installs the
//! production setup, moves a real frame through the library and reads the
//! ring back.

// Test code: unwrap/expect are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use bookclerk_config::{init_tracing_with, register_secret, TracingOptions};
use futures::{SinkExt as _, StreamExt as _};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing_log::log;

/// Stands in for a copy token inside a frame's text.
const TOKEN: &str = "copy-token-4f1c9a7e2b6d4c08a3e5";

/// Lower-case hex of `text`'s bytes, as the library's frame trace spells a
/// payload.
fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

/// With the production log setup installed, a frame that carries a token
/// leaves no trace in the diagnostics ring, in plain text or in hex, while
/// another library's INFO record is still carried (its TRACE is outside the
/// ring's filter).
#[tokio::test]
async fn a_frame_payload_reaches_no_log_sink() {
    let logging = init_tracing_with(TracingOptions {
        enable_journald: false,
        ..TracingOptions::default()
    });
    // The library's records are refused before they are built; another
    // target's are carried.
    assert!(!log::log_enabled!(
        target: "tungstenite::protocol::frame",
        log::Level::Trace
    ));
    assert!(log::log_enabled!(target: "hyper::proto", log::Level::Info));

    register_secret(TOKEN);
    let (library_end, clerk_end) = tokio::io::duplex(64 * 1024);
    let mut library = WebSocketStream::from_raw_socket(library_end, Role::Server, None).await;
    let mut clerk = WebSocketStream::from_raw_socket(clerk_end, Role::Client, None).await;
    // Library to clerk, the direction of `copy.granted`: a server's frames
    // are not masked, so the payload is on the wire as written.
    let frame = format!(r#"{{"type":"copy.granted","token":"{TOKEN}"}}"#);
    library.send(Message::text(frame.clone())).await.unwrap();
    let received = clerk.next().await.unwrap().unwrap();
    assert_eq!(received.into_text().unwrap().as_str(), frame);

    log::info!(target: "hyper::proto", "a carried record");
    log::trace!(target: "hyper::proto", "a dropped record");

    let events = logging.diagnostics.snapshot_events();
    let written = format!("{events:?}").to_lowercase();
    assert!(written.contains("a carried record"), "{written}");
    assert!(!written.contains("a dropped record"), "{written}");
    assert!(!written.contains(&TOKEN.to_lowercase()), "{written}");
    assert!(!written.contains(&hex(TOKEN)), "{written}");
    // `log` records keep their own target in the ring; none is the
    // library's.
    assert!(
        events.iter().any(|event| event.target == "hyper::proto"),
        "{written}"
    );
    let from_the_library = events
        .iter()
        .any(|event| event.target == "tungstenite" || event.target.starts_with("tungstenite::"));
    assert!(!from_the_library, "{written}");
}
