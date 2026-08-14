//! Framing / protocol constant conformance for the guest SDK.
#![allow(clippy::missing_panics_doc)]

use bookclerk_plugin_sdk::{
    methods, HOST_API_VERSION_MAX, HOST_API_VERSION_MIN, MAX_RPC_LINE_BYTES, PLUGIN_API_VERSION,
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
fn host_api_version_range_covers_current() {
    assert_eq!(HOST_API_VERSION_MIN, 1);
    assert_eq!(HOST_API_VERSION_MAX, 1);
    assert_eq!(PLUGIN_API_VERSION, 1);
    const {
        assert!(PLUGIN_API_VERSION >= HOST_API_VERSION_MIN);
        assert!(PLUGIN_API_VERSION <= HOST_API_VERSION_MAX);
    }
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
