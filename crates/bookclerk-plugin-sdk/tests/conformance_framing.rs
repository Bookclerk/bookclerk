//! Framing / protocol constant conformance for the guest SDK.
#![allow(clippy::missing_panics_doc)]

use bookclerk_plugin_sdk::{methods, MAX_RPC_LINE_BYTES, PRODUCT_API_VERSION, PROTOCOL_NAME};

#[test]
fn protocol_name_is_workers_rpc() {
    assert_eq!(PROTOCOL_NAME, "workers-rpc");
}

#[test]
fn max_rpc_line_bytes_is_16_mib() {
    assert_eq!(MAX_RPC_LINE_BYTES, 16 * 1024 * 1024);
}

#[test]
fn product_api_version_is_3() {
    assert_eq!(PRODUCT_API_VERSION, 3);
}

#[test]
fn shutdown_method_name_is_stable() {
    assert_eq!(methods::SHUTDOWN, "shutdown");
}

#[test]
fn camel_case_method_names() {
    assert_eq!(methods::EVENT, "event");
    assert_eq!(methods::PUBLISH, "publish");
    assert_eq!(methods::FETCH_TITLE, "fetchTitle");
    assert_eq!(methods::INVOKE, "invoke");
    assert_eq!(methods::LOGIN_START, "loginStart");
}
