//! Concurrent native-behind-workerd sessions keep separate grants and state.
//!
//! Overlapping launches also exercise the live proxy challenge. Another
//! session's secret on this link closes it, and an unrelated child without
//! `BOOKCLERK_SESSION_CHALLENGE` cannot complete a handshake on an endpoint it
//! can see. A numeric fd or handle is not cross-process identity. On Windows,
//! `GetHandleInformation` on the inherited proxy must succeed so a missing API
//! is not reported as a denial. An unlisted live handle is checked with
//! `DuplicateHandle`: access denied or invalid handle is a denial.
//! `GetHandleInformation` on a value that is not a handle in the guest
//! terminated the AppContainer process.

#[path = "native_gateway/harness.rs"]
mod ng_harness;

use std::time::SystemTime;

use ng_harness::{
    error_text, open_session, probe, session_dirs_under, step, wait_for_exit, Install, Listener,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_keep_separate_grants_and_state() {
    // The proxy is still waiting for its 32-byte challenge until the first
    // connect. These two sessions exist only to exercise that handshake.
    let listener_gate_a = Listener::bind(true).await;
    let listener_gate_b = Listener::bind(true).await;
    let install_gate_a = Install::new(listener_gate_a.port);
    let install_gate_b = Install::new(listener_gate_b.port);
    let (gate_a, gate_b) = tokio::join!(install_gate_a.spawn(), install_gate_b.spawn());
    tokio::join!(open_session(&gate_a), open_session(&gate_b));

    let challenge_b = probe(&gate_b, "session_challenge", 0, "").await;
    assert_eq!(
        challenge_b["ok"], true,
        "B did not publish a challenge: {challenge_b}"
    );
    let challenge_hex = challenge_b["hex"].as_str().unwrap_or("").to_string();
    assert_eq!(
        challenge_hex.len(),
        64,
        "B challenge hex was not 32 bytes (len {})",
        challenge_hex.len()
    );
    let (foreign, child) = tokio::join!(
        probe(&gate_a, "present_challenge", 0, &challenge_hex),
        probe(&gate_b, "unrelated_challenge", 0, "inherit"),
    );
    assert_eq!(foreign["unsupported"], false, "{foreign}");
    assert_eq!(
        foreign["wrote"], true,
        "B's challenge was not written on A's link: {foreign}"
    );
    assert_eq!(foreign["opened_stream"], false, "{foreign}");
    assert_eq!(foreign["completed"], false, "{foreign}");
    assert!(
        foreign["closed"] == true || foreign["refused"] == true,
        "B's challenge on A's link must close or refuse: {foreign}"
    );
    assert_eq!(child["unsupported"], false, "{child}");
    assert_eq!(child["completed"], false, "{child}");
    assert_eq!(child["challenge_env"], false, "{child}");
    assert_eq!(child["opened_stream"], false, "{child}");
    let attempts = child["attempts"].as_array().expect("child attempts");
    assert!(!attempts.is_empty(), "{child}");
    for attempt in attempts {
        assert_eq!(attempt["completed"], false, "{child}");
        assert_eq!(attempt["opened_stream"], false, "{child}");
        let err = attempt["error"].as_str().unwrap_or("");
        assert!(!err.to_ascii_lowercase().contains("unsupported"), "{child}");
        assert!(!err.contains("Unix-only"), "{child}");
        assert!(!err.contains("fd identity"), "{child}");
    }
    #[cfg(unix)]
    {
        let primary = &attempts[0];
        assert_eq!(
            primary["wrote"], true,
            "unrelated child could not see the live proxy: {child}"
        );
        assert_eq!(
            primary["closed"], true,
            "live proxy did not close on the child's zeros: {child}"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        listener_gate_a.accepts(),
        0,
        "foreign challenge produced an accept"
    );
    assert_eq!(
        listener_gate_b.accepts(),
        0,
        "unrelated child produced an accept"
    );
    step(&format!(
        "live endpoint: B's challenge on A's link closed={} refused={} opened_stream=false; unrelated child completed=false wrote={} closed={}",
        foreign["closed"], foreign["refused"], child["wrote"], child["closed"]
    ));
    let gate_gateway_a = gate_a.gateway_pid().expect("gate A gateway");
    let gate_guest_a = gate_a.guest_pid().expect("gate A guest");
    let gate_gateway_b = gate_b.gateway_pid().expect("gate B gateway");
    let gate_guest_b = gate_b.guest_pid().expect("gate B guest");
    drop(gate_a);
    drop(gate_b);
    wait_for_exit(gate_gateway_a).await;
    wait_for_exit(gate_guest_a).await;
    wait_for_exit(gate_gateway_b).await;
    wait_for_exit(gate_guest_b).await;
    assert!(session_dirs_under(install_gate_a.files_dir()).is_empty());
    assert!(session_dirs_under(install_gate_b.files_dir()).is_empty());

    let listener_a = Listener::bind(true).await;
    let listener_b = Listener::bind(true).await;
    assert_ne!(listener_a.port, listener_b.port);
    let install_a = Install::new(listener_a.port);
    let install_b = Install::new(listener_b.port);

    let (session_a, session_b) = tokio::join!(install_a.spawn(), install_b.spawn());
    tokio::join!(open_session(&session_a), open_session(&session_b));

    let dir_a = session_a
        .session_dir()
        .expect("A session dir")
        .to_path_buf();
    let dir_b = session_b
        .session_dir()
        .expect("B session dir")
        .to_path_buf();
    assert_ne!(dir_a, dir_b);
    let secret_b = dir_b.join("host-secret.txt");
    std::fs::write(&secret_b, b"session-b-private").expect("write B secret");

    let env_a = probe(&session_a, "env_keys", 0, "").await;
    let env_b = probe(&session_b, "env_keys", 0, "").await;
    let proxy_a = env_a["socket_proxy"].as_str().unwrap_or("").to_string();
    let proxy_b = env_b["socket_proxy"].as_str().unwrap_or("").to_string();
    assert!(
        proxy_a.starts_with("fd:") || proxy_a.starts_with("handle:"),
        "A SOCKET_PROXY: {proxy_a}"
    );
    assert!(
        proxy_b.starts_with("fd:") || proxy_b.starts_with("handle:"),
        "B SOCKET_PROXY: {proxy_b}"
    );
    let handles = probe(&session_a, "unlisted_handle", 0, &unlisted_payload()).await;
    #[cfg(windows)]
    {
        assert_eq!(handles["unsupported"], false, "{handles}");
        assert_eq!(
            handles["proxy_usable"], true,
            "inherited proxy handle was not usable: {handles}"
        );
        assert_eq!(handles["denied"], true, "{handles}");
        let os = handles["os"].as_u64().unwrap_or(0);
        assert!(
            os == 5 || os == 6,
            "unlisted handle must be access-denied or invalid: {handles}"
        );
        step(&format!(
            "unlisted handle denied os {os}; inherited proxy handle still usable"
        ));
    }
    #[cfg(not(windows))]
    {
        assert_eq!(handles["unsupported"], true, "{handles}");
        assert_ne!(
            handles["denied"], true,
            "unsupported must not count as a denial: {handles}"
        );
        step("unlisted-handle probe unsupported on this platform; denial was not asserted");
    }
    step(&format!(
        "A session={} B session={} B proxy={proxy_b}",
        dir_a.display(),
        dir_b.display()
    ));

    let stolen = probe(&session_a, "read_path", 0, &secret_b.display().to_string()).await;
    assert_eq!(
        stolen["ok"], false,
        "session A read session B's gateway state: {stolen}"
    );

    let payload = format!(
        "iso-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let before_b = listener_b.accepts();
    let cross = probe(&session_a, "connect", listener_b.port, &payload).await;
    assert_eq!(
        cross["ok"], false,
        "A reached B's granted port through A's proxy: {cross}"
    );
    let error = error_text(&cross);
    assert!(
        error.contains("403") || error.contains("refused") || error.contains("denied"),
        "cross-session connect must be a policy denial, got: {error}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        listener_b.accepts(),
        before_b,
        "A's traffic produced accepts on B's listener"
    );

    let ok_a = probe(&session_a, "connect", listener_a.port, &payload).await;
    assert_eq!(ok_a["ok"], true, "A lost its own grant: {ok_a}");
    let ok_b = probe(&session_b, "connect", listener_b.port, &payload).await;
    assert_eq!(ok_b["ok"], true, "B lost its own grant: {ok_b}");
    assert!(listener_a.wait_for_accepts(1).await);
    assert!(listener_b.wait_for_accepts(before_b + 1).await);

    let gateway_a = session_a.gateway_pid().expect("A gateway");
    let guest_a = session_a.guest_pid().expect("A guest");
    let gateway_b = session_b.gateway_pid().expect("B gateway");
    let guest_b = session_b.guest_pid().expect("B guest");
    drop(session_a);
    drop(session_b);
    wait_for_exit(gateway_a).await;
    wait_for_exit(guest_a).await;
    wait_for_exit(gateway_b).await;
    wait_for_exit(guest_b).await;
    assert!(session_dirs_under(install_a.files_dir()).is_empty());
    assert!(session_dirs_under(install_b.files_dir()).is_empty());
    step("both sessions tore down cleanly");
}

/// Handle values that are live in this process and were not placed in the guest.
fn unlisted_payload() -> String {
    #[cfg(windows)]
    {
        live_host_handles()
    }
    #[cfg(not(windows))]
    {
        "0".to_string()
    }
}

/// Four live event handles. The guest did not inherit them.
#[cfg(windows)]
#[allow(unsafe_code)]
fn live_host_handles() -> String {
    extern "system" {
        fn CreateEventW(
            attrs: *mut core::ffi::c_void,
            manual: i32,
            initial: i32,
            name: *const u16,
        ) -> *mut core::ffi::c_void;
    }
    let mut values = Vec::new();
    for _ in 0..4 {
        let handle = unsafe { CreateEventW(core::ptr::null_mut(), 1, 0, core::ptr::null()) };
        assert!(!handle.is_null(), "CreateEventW failed");
        values.push((handle as usize as u64).to_string());
    }
    values.join(",")
}
