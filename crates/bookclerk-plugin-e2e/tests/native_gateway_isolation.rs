//! Concurrent native-behind-workerd sessions keep separate grants and state.
//!
//! This checks filesystem state and egress policy. It does not treat a numeric
//! fd or handle value as cross-process identity. Overlapping launches prove
//! that with the per-session proxy challenge.

#[path = "native_gateway/harness.rs"]
mod ng_harness;

use std::time::SystemTime;

use ng_harness::{
    error_text, open_session, probe, session_dirs_under, step, wait_for_exit, Install, Listener,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sessions_keep_separate_grants_and_state() {
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
    let fd_a = proxy_a.split(':').next_back().unwrap_or("");
    let fd_b = proxy_b.split(':').next_back().unwrap_or("");
    let foreign = probe(&session_a, "touch_fd", 0, "250").await;
    assert_eq!(
        foreign["ok"], false,
        "session A treated fd 250 as an open inherited proxy: {foreign}"
    );
    if fd_a != fd_b {
        let borrowed = probe(&session_a, "touch_fd", 0, fd_b).await;
        assert_eq!(
            borrowed["ok"], false,
            "session A opened B's proxy descriptor number {fd_b}: {borrowed}"
        );
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
