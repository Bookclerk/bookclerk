//! Shared ABI conformance expectations for native + workerd Echo guests.
//!
//! These checks are schema/constant level so they do not require spawning a
//! workerd binary. Runtime Echo guests (native + workerd `modules/index.js`)
//! must implement the same capability names and `describe()` shape.
#![allow(clippy::missing_panics_doc)]

use bookclerk_plugin_abi::{methods, PluginDescribe, METHOD_NAMES, PRODUCT_API_VERSION};
use bookclerk_plugin_sdk::PROTOCOL_NAME;

#[test]
fn abi_version_is_two() {
    assert_eq!(PRODUCT_API_VERSION, 2);
}

#[test]
fn logical_protocol_is_workers_rpc() {
    assert_eq!(PROTOCOL_NAME, "workers-rpc");
}

#[test]
fn core_methods_present() {
    for name in [
        methods::shutdown::NAME,
        methods::health::NAME,
        methods::diagnose::NAME,
        methods::on_event::NAME,
        methods::cli_invoke::NAME,
    ] {
        assert!(
            METHOD_NAMES.contains(&name),
            "missing method catalog entry for {name}"
        );
    }
}

#[test]
fn echo_describe_shape_roundtrips() {
    let describe = PluginDescribe {
        api_version: PRODUCT_API_VERSION,
        id: "echo".into(),
        kind: "integration".into(),
        display_name: Some("Echo Integration".into()),
        capabilities: vec![
            "health".into(),
            "diagnose".into(),
            "onEvent".into(),
            "cli".into(),
        ],
        ..PluginDescribe::default()
    };
    let v = serde_json::to_value(&describe).unwrap();
    assert_eq!(v["apiVersion"], 2);
    assert_eq!(v["id"], "echo");
    assert!(v.get("metadataJson").is_none());
    let back: PluginDescribe = serde_json::from_value(v).unwrap();
    assert_eq!(back.capabilities, describe.capabilities);
    assert!(back.has_capability("onEvent"));
}

/// Typed method payloads project to camelCase JSON on the transport-private
/// bridges (bytes as base64).
#[test]
fn typed_payload_json_projection_is_camel_case() {
    use bookclerk_plugin_abi::LoginParams;

    let login = LoginParams {
        plugin_data_dir: "/tmp/p".into(),
        marketplace: "us".into(),
        callback_ipc: Some("/tmp/oauth.sock".into()),
        ..LoginParams::default()
    };
    let v = serde_json::to_value(&login).unwrap();
    assert!(v.get("pluginDataDir").is_some());
    assert!(v.get("callbackIpc").is_some());
    assert!(v.get("plugin_data_dir").is_none());
    assert!(v.get("callback_ipc").is_none());
}
