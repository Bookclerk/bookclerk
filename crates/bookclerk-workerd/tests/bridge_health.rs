//! The launcher's bridge health check ignores proxy environment variables
//! (#763).
//!
//! This file is its own test binary because it changes the process
//! environment, which `ureq` reads when an agent is built; the proxy test
//! runs every step in order so nothing else reads the environment meanwhile.

// Test code: expect is fine here.
#![allow(clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bookclerk_workerd::bridge_http::BridgeHttp;

/// The bearer token the test sends, to check where it arrives.
const TOKEN: &str = "Bearer test-bridge-token";

/// Accepts one connection, records its request head and answers `200 ok`.
///
/// # Returns
///
/// The listener's address and a channel that yields the request head.
fn one_shot_server() -> (SocketAddr, mpsc::Receiver<String>) {
    one_shot_server_with(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
}

/// Accepts one connection, records its request head and writes `reply`.
///
/// # Arguments
///
/// * `reply` - The raw HTTP response to send.
///
/// # Returns
///
/// The listener's address and a channel that yields the request head.
fn one_shot_server_with(reply: &'static [u8]) -> (SocketAddr, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let head = read_head(&mut stream);
            let _ = stream.write_all(reply);
            let _ = tx.send(head);
        }
    });
    (addr, rx)
}

/// A plaintext proxy that accepts one `CONNECT`, answers it and records
/// what the client sends through the tunnel.
///
/// # Returns
///
/// The proxy's address and a channel that yields the `CONNECT` head followed
/// by the tunnelled request head.
fn one_shot_proxy() -> (SocketAddr, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let connect = read_head(&mut stream);
            let _ = stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
            let tunnelled = read_head(&mut stream);
            let _ = tx.send(format!("{connect}{tunnelled}"));
        }
    });
    (addr, rx)
}

/// Reads a request head (up to the blank line).
///
/// # Arguments
///
/// * `stream` - The accepted connection.
///
/// # Returns
///
/// The head as text.
fn read_head(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// With `ALL_PROXY` / `HTTPS_PROXY` / `HTTP_PROXY` pointing at a listener
/// and no `NO_PROXY`, a default agent hands the token to that "proxy" (the
/// oracle), while [`BridgeHttp::health`], the launcher's check, reaches the
/// bridge listener directly.
#[test]
fn bridge_requests_skip_an_inherited_proxy() {
    let (proxy, proxy_rx) = one_shot_proxy();
    let proxy_url = format!("http://{proxy}");
    for name in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(name);
    }
    for name in ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY"] {
        std::env::set_var(name, &proxy_url);
    }

    // Oracle: an agent with ureq's default proxy handling sends a loopback
    // request, token included, to the proxy, so the environment is armed.
    let (target, _target_rx) = one_shot_server();
    let url = format!("http://{target}/health");
    let _ = bookclerk_tls::ureq::Agent::config_builder()
        .tls_config(bookclerk_tls::ureq_tls_config())
        .build()
        .new_agent()
        .get(&url)
        .header("Authorization", TOKEN)
        .call();
    let head = proxy_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the default agent goes through the proxy");
    assert!(head.starts_with("CONNECT "), "proxy saw: {head}");
    assert!(head.contains(TOKEN), "proxy saw: {head}");

    // The launcher's health check reaches the listener directly.
    let (proxy, proxy_rx) = one_shot_proxy();
    let proxy_url = format!("http://{proxy}");
    for name in ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY"] {
        std::env::set_var(name, &proxy_url);
    }
    let (target, target_rx) = one_shot_server();
    let bridge = BridgeHttp {
        port: target.port(),
        token: "test-bridge-token".to_owned(),
    };
    runtime().block_on(bridge.health()).expect("health");
    let head = target_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the listener got the request");
    assert!(head.starts_with("GET /health "), "listener saw: {head}");
    assert!(head.contains(TOKEN), "listener saw: {head}");
    assert!(
        proxy_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "the proxy got no connection"
    );

    // A refused token and a failing bridge are errors the launcher retries on.
    let (target, _rx) =
        one_shot_server_with(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
    let err = runtime()
        .block_on(
            BridgeHttp {
                port: target.port(),
                token: "wrong".to_owned(),
            }
            .health(),
        )
        .expect_err("401 is an error");
    assert_eq!(err.to_string(), "bridge unauthorized");
    let (target, _rx) = one_shot_server_with(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 8\r\n\r\nstarting",
    );
    let err = runtime()
        .block_on(
            BridgeHttp {
                port: target.port(),
                token: "test-bridge-token".to_owned(),
            }
            .health(),
        )
        .expect_err("503 is an error");
    assert_eq!(err.to_string(), "bridge HTTP 503: starting");
}

/// A current-thread Tokio runtime for one `block_on`.
///
/// # Returns
///
/// The runtime.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}
