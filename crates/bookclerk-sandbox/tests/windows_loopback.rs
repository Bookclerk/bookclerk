//! E3: same-AppContainer loopback (Windows). Gates native-behind-workerd.
//!
//! Host↔container loopback is blocked; this proves a child spawned inside an
//! `OutboundListen` container can bind `127.0.0.1:0` and connect to itself.

#![cfg(windows)]

use std::ffi::OsString;
use std::path::Path;

use bookclerk_sandbox::{Enforcement, NetPolicy, Policy};

const PROBE: &str = env!("CARGO_BIN_EXE_bookclerk-loopback-probe");

fn spawn_enforcement_demanded() -> bool {
    std::env::var("BOOKCLERK_SANDBOX_REQUIRE_SPAWN_ENFORCEMENT")
        .is_ok_and(|value| !value.trim().is_empty())
}

#[test]
fn outbound_listen_appcontainer_loopback_round_trips() {
    let caps = bookclerk_sandbox::capabilities();
    assert!(
        caps.spawn_filesystem || !spawn_enforcement_demanded(),
        "spawn enforcement demanded but unavailable: {} [{}]",
        caps.detail,
        caps.backend
    );
    if !caps.spawn_filesystem {
        eprintln!("skipping: spawn_filesystem unavailable");
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let allowed = root.path().join("allowed");
    std::fs::create_dir_all(&allowed).expect("allowed");
    let policy = Policy::new("test:e3-sandbox-loopback")
        .writes([allowed])
        .net(NetPolicy::OutboundListen)
        .allow_exec(true)
        .system_paths(false)
        .enforcement(Enforcement::Required);

    let session = bookclerk_sandbox::spawn::AppContainerSession::create("test:e3-sandbox-loopback")
        .expect("session");
    let code = bookclerk_sandbox::spawn::run_appcontainer(
        &policy,
        Path::new(PROBE),
        &[] as &[OsString],
        Some(&session),
    )
    .expect("loopback probe inside AppContainer");
    assert_eq!(code, 0, "same-container loopback must succeed");
}
