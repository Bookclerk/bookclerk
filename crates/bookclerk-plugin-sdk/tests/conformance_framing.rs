//! Framing / protocol constant conformance for the guest SDK.
#![allow(clippy::missing_panics_doc)]

use bookclerk_plugin_sdk::{
    methods, v2::PRODUCT_API_VERSION, HOST_MANIFEST_API_VERSION_MAX, MAX_RPC_LINE_BYTES,
    PROTOCOL_NAME,
};

#[test]
fn protocol_name_is_workers_rpc() {
    assert_eq!(PROTOCOL_NAME, "workers-rpc");
}

#[test]
fn max_rpc_line_bytes_is_16_mib() {
    assert_eq!(MAX_RPC_LINE_BYTES, 16 * 1024 * 1024);
}

#[test]
fn product_api_version_is_2() {
    assert_eq!(PRODUCT_API_VERSION, 2);
    assert_eq!(HOST_MANIFEST_API_VERSION_MAX, 2);
}

#[test]
fn shutdown_method_name_is_stable() {
    assert_eq!(methods::SHUTDOWN, "shutdown");
}

#[test]
fn camel_case_method_names() {
    assert_eq!(methods::ON_EVENT, "onEvent");
    assert_eq!(methods::FETCH_TITLE, "fetchTitle");
    assert_eq!(methods::CLI_INVOKE, "cliInvoke");
    assert_eq!(methods::LOGIN_START, "loginStart");
}
