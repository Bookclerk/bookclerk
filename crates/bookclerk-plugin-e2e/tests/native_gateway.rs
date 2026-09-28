//! Native-behind-workerd gateway smoke on the production launch path.
//!
//! Installs the test-only `native_gateway_probe` guest into a temporary files
//! dir, grants exactly one loopback TCP port, and spawns it through
//! [`PluginSession::spawn_with`] with `Isolation::Required`: host discovery and
//! launch planning, sibling `bookclerk-jail` processes, `bookclerk-workerd` +
//! pinned `workerd`, and the inherited socket-proxy link. Nothing here skips:
//! a missing helper, runtime or confinement backend fails the test.
//!
//! Run with `cargo test -p bookclerk-plugin-e2e --test native_gateway` after
//! `cargo build -p bookclerk-jail -p bookclerk-workerd` and `cargo ensure-workerd`.

#[path = "native_gateway/harness.rs"]
mod ng_harness;

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bookclerk_plugin_host::Entrypoint;
use ng_harness::{
    error_text, open_session, probe, session_dirs_under, step, wait_for_exit, Install, Listener,
    LOOPBACK, RPC_TIMEOUT, SPAWN_TIMEOUT,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_guest_reaches_only_granted_tcp_through_the_front_door() {
    let allowed = Listener::bind(true).await;
    let denied = Listener::bind(false).await;
    assert_ne!(allowed.port, denied.port);
    let install = Install::new(allowed.port);
    step(&format!(
        "granted {LOOPBACK}:{} only; ungranted live listener on {}",
        allowed.port, denied.port
    ));

    let started = Instant::now();
    let session = install.spawn().await;
    step(&format!("spawned in {:?}", started.elapsed()));
    assert!(
        session.session_key().contains("native-behind-workerd"),
        "guest must run behind the workerd front door: {}",
        session.session_key()
    );
    let gateway_pid = session.gateway_pid().expect("gateway pid");
    let guest_pid = session.guest_pid().expect("guest pid");
    let session_dir = session
        .session_dir()
        .expect("host-owned session directory")
        .to_path_buf();
    assert!(
        session_dir.is_dir(),
        "session dir missing at {}",
        session_dir.display()
    );
    assert_ne!(gateway_pid, guest_pid, "siblings must be distinct pids");

    let plugin = install.plugin();
    let describe = tokio::time::timeout(RPC_TIMEOUT, session.describe())
        .await
        .unwrap_or_else(|_| {
            ng_harness::fail_deadline(&format!("describe timed out after {RPC_TIMEOUT:?}"))
        })
        .expect("describe");
    assert_eq!(describe.id, ng_harness::PLUGIN_ID);
    assert_eq!(
        describe.capabilities,
        plugin.manifest.capabilities(),
        "describe() capabilities must equal the installed plugin.toml"
    );
    open_session(&session).await;
    assert!(session.has_entrypoint(Entrypoint::Cli));
    step("describe + open ok");

    #[cfg(windows)]
    {
        let sid = session
            .package_sid()
            .expect("host must create the guest AppContainer and propagate its SID");
        assert!(sid.starts_with("S-1-15-2-"), "unexpected Package SID {sid}");
        step(&format!("guest AppContainer Package SID {sid}"));
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let payload = format!("bookclerk-native-gateway-{}-{nanos}", std::process::id());
    let outcome = probe(&session, "connect", allowed.port, &payload).await;
    assert_eq!(
        outcome["ok"], true,
        "mediated connect to the granted endpoint failed: {outcome}"
    );
    assert_eq!(outcome["echo"].as_str(), Some(payload.as_str()));
    assert!(
        allowed.wait_for_accepts(1).await,
        "proxy never dialed the granted listener"
    );
    step(&format!("granted round trip ok ({} bytes)", payload.len()));

    let outcome = probe(&session, "connect", denied.port, &payload).await;
    assert_eq!(
        outcome["ok"], false,
        "ungranted port was reachable: {outcome}"
    );
    let error = error_text(&outcome);
    assert!(
        error.contains("socket proxy refused") && error.contains("403"),
        "ungranted port must be a proxy policy denial (403), got: {error}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(denied.accepts(), 0, "proxy dialed the ungranted listener");
    let _ = tokio::net::TcpStream::connect((LOOPBACK, denied.port))
        .await
        .expect("harness reaches the ungranted listener");
    assert!(
        denied.wait_for_accepts(1).await,
        "ungranted listener was not live, so the denial proves nothing"
    );
    step(&format!("ungranted port denied by policy: {error}"));

    let before = allowed.accepts();
    let outcome = probe(&session, "ambient", allowed.port, "").await;
    assert_eq!(
        outcome["ok"], false,
        "direct TCP from the guest jail reached the host: {outcome}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        allowed.accepts(),
        before,
        "ambient connect reached the listener"
    );
    let again = probe(&session, "connect", allowed.port, &payload).await;
    assert_eq!(
        again["ok"], true,
        "granted endpoint went down during the ambient check: {again}"
    );
    assert!(allowed.wait_for_accepts(before + 1).await);
    step(&format!(
        "ambient TCP blocked while granted endpoint live: {}",
        error_text(&outcome)
    ));

    let env = probe(&session, "env_keys", 0, "").await;
    assert_eq!(env["ok"], true, "env_keys failed: {env}");
    let keys = env["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    for key in &keys {
        let upper = key.to_ascii_uppercase();
        assert!(
            !upper.starts_with("BOOKCLERK_WORKERD_GRANT_"),
            "guest env leaked grant key {key}"
        );
        assert!(
            !upper.starts_with("BOOKCLERK_JAIL_"),
            "guest env leaked jail key {key}"
        );
        assert_ne!(upper, "BOOKCLERK_GATEWAY_GUEST_RPC");
        assert_ne!(upper, "BOOKCLERK_GATEWAY_GUEST_RPC_WRITE");
        assert_ne!(upper, "BOOKCLERK_GATEWAY_PROXY");
        assert_ne!(upper, "BOOKCLERK_GATEWAY_PROXY_WRITE");
        assert_ne!(upper, "BOOKCLERK_WORKERD_STATE_DIR");
        assert_ne!(upper, "BOOKCLERK_NATIVE_BACKEND");
    }
    let proxy = env["socket_proxy"].as_str().unwrap_or("");
    assert!(
        proxy.starts_with("fd:") || proxy.starts_with("handle:"),
        "SOCKET_PROXY must be an inherited link, got {proxy:?}"
    );
    #[cfg(windows)]
    assert!(
        keys.iter()
            .any(|key| key.eq_ignore_ascii_case("BOOKCLERK_SOCKET_PROXY_WRITE")),
        "guest missing SOCKET_PROXY_WRITE: {keys:?}"
    );
    step(&format!("guest env contract ok ({proxy})"));

    let grants = install.grants_path();
    let denied_grants = probe(&session, "read_path", 0, &grants.display().to_string()).await;
    assert_eq!(
        denied_grants["ok"], false,
        "guest read plugin-grants.json: {denied_grants}"
    );
    let secret = session_dir.join("host-secret.txt");
    std::fs::write(&secret, b"gateway-private").expect("write session secret");
    assert!(
        secret.is_file(),
        "harness must be able to write session dir"
    );
    let denied_session = probe(&session, "read_path", 0, &secret.display().to_string()).await;
    assert_eq!(
        denied_session["ok"], false,
        "guest read a file in the gateway session dir: {denied_session}"
    );
    step("guest cannot read gateway state or plugin-grants.json");

    drop(session);
    wait_for_exit(gateway_pid).await;
    wait_for_exit(guest_pid).await;
    step(&format!(
        "gateway pid {gateway_pid} and guest pid {guest_pid} exited after drop"
    ));
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while session_dir.exists() && Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !session_dir.exists(),
        "session dir still present at {}",
        session_dir.display()
    );
    assert!(
        session_dirs_under(install.files_dir()).is_empty(),
        "leaked session-* directories under {}",
        install.files_dir().display()
    );
    drop(install);
    step(&format!("total {:?}", started.elapsed()));
}
