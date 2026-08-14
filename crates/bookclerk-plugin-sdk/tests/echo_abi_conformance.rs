//! Shared ABI conformance expectations for native + workerd Echo guests.
//!
//! These checks are schema/constant level so they do not require spawning a
//! workerd binary. Runtime Echo guests (native `BookclerkPlugin` + workerd
//! `modules/index.js`) must implement the same method names and handshake shape.
#![allow(clippy::missing_panics_doc)]

use bookclerk_plugin_abi::{
    methods, HandshakeResult, HostToPluginEvent, API_VERSION, METHOD_NAMES,
};
use bookclerk_plugin_sdk::PROTOCOL_NAME;

#[test]
fn abi_version_is_one() {
    assert_eq!(API_VERSION, 1);
}

#[test]
fn logical_protocol_is_workers_rpc() {
    assert_eq!(PROTOCOL_NAME, "workers-rpc");
}

#[test]
fn core_methods_present() {
    for name in [
        methods::handshake::NAME,
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
fn echo_handshake_shape_roundtrips() {
    let hs = HandshakeResult {
        api_version: API_VERSION,
        id: "echo".into(),
        kind: "integration".into(),
        display_name: Some("Echo Integration".into()),
        capabilities: vec![
            "health".into(),
            "diagnose".into(),
            "onEvent".into(),
            "cli".into(),
        ],
        ..HandshakeResult::default()
    };
    let v = serde_json::to_value(&hs).unwrap();
    assert_eq!(v["apiVersion"], 1);
    assert_eq!(v["id"], "echo");
    let back: HandshakeResult = serde_json::from_value(v).unwrap();
    assert_eq!(back.capabilities, hs.capabilities);
}

#[test]
fn on_event_book_acquired_wire_shape() {
    let event =
        HostToPluginEvent::BookAcquired(bookclerk_plugin_abi::events::BookAcquiredPayload {
            title_id: "t1".into(),
            source: "audible".into(),
            asin: Some("B00".into()),
            isbn: None,
            path_keys: vec!["Books/t1.m4b".into()],
        });
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["type"], "book_acquired");
    assert_eq!(v["payload"]["titleId"], "t1");
}

/// Kind/db wire DTOs use camelCase (see `fixtures/wire/` goldens + #130).
#[test]
fn kind_db_wire_dto_camel_case() {
    use bookclerk_plugin_abi::{DbConnectParams, ExecResultDto, LoginParams};

    let login = LoginParams {
        plugin_data_dir: "/tmp/p".into(),
        marketplace: "us".into(),
        label: None,
        email: None,
        password: None,
        force: false,
        callback_bind: None,
        callback_ipc: Some("/tmp/oauth.sock".into()),
        callback_public_base: None,
        external: false,
        response_url: None,
        show_qr: false,
        timeout_secs: None,
        extra: serde_json::json!({}),
    };
    let v = serde_json::to_value(&login).unwrap();
    assert!(v.get("pluginDataDir").is_some());
    assert!(v.get("callbackIpc").is_some());
    assert!(v.get("plugin_data_dir").is_none());
    assert!(v.get("callback_ipc").is_none());

    let connect = DbConnectParams::Sqlite {
        plugin_data_dir: "/tmp/p".into(),
        sqlite_path: Some("/tmp/library.db".into()),
    };
    let cv = serde_json::to_value(&connect).unwrap();
    assert_eq!(cv["backend"], "sqlite");
    assert!(cv.get("pluginDataDir").is_some());
    assert!(cv.get("sqlitePath").is_some());
    assert!(cv.get("sqlite_path").is_none());

    let exec = ExecResultDto {
        last_insert_id: 1,
        rows_affected: 1,
    };
    let ev = serde_json::to_value(&exec).unwrap();
    assert!(ev.get("lastInsertId").is_some());
    assert!(ev.get("rowsAffected").is_some());
    assert!(ev.get("last_insert_id").is_none());

    let begin = bookclerk_plugin_abi::DbBeginParams {
        parent_txn_id: Some("txn-1".into()),
    };
    let bv = serde_json::to_value(&begin).unwrap();
    assert_eq!(bv["parentTxnId"], "txn-1");
    assert!(bv.get("parent_txn_id").is_none());
}
