//! Native-behind-workerd spawn and teardown fail closed.

#[path = "native_gateway/harness.rs"]
mod ng_harness;

use std::ffi::OsString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bookclerk_plugin_host::{
    reconcile_grants_from_disk, PluginGrantStore, PluginSession, SessionServices,
    HOST_SHARED_ACCOUNT, WORKERD_BIN_ENV,
};
use bookclerk_plugin_sdk::CliInvokeParams;
use ng_harness::{
    kill_pid, linux_fd_count, linux_session_cgroup, open_session, probe, process_alive,
    session_dirs_under, step, wait_for_exit, Install, Listener, ProcessTree, SETTLE_TIMEOUT,
    SPAWN_TIMEOUT,
};

/// Serializes tests in this binary. `missing_workerd_fails_closed` replaces
/// process `BOOKCLERK_WORKERD_BIN`, which every other spawn reads.
async fn workerd_bin_lock() -> tokio::sync::OwnedMutexGuard<()> {
    use std::sync::{Arc, OnceLock};
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_that_exits_immediately_fails_closed() {
    let _env = workerd_bin_lock().await;
    let listener = Listener::bind(true).await;
    let install = Install::new(listener.port);
    let extra = [("BOOKCLERK_PROBE_EXIT", OsString::from("1"))];
    let plugin = install.plugin();
    let result = tokio::time::timeout(
        SPAWN_TIMEOUT,
        PluginSession::spawn_with(
            &plugin,
            &install.config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &extra,
            SessionServices::default(),
        ),
    )
    .await
    .unwrap_or_else(|_| {
        ng_harness::fail_deadline(&format!("spawn timed out after {SPAWN_TIMEOUT:?}"))
    });
    let err = match result {
        Ok(_) => panic!("immediate-exit guest must fail closed"),
        Err(err) => err,
    };
    step(&format!("immediate-exit refused: {err}"));
    assert!(
        session_dirs_under(install.files_dir()).is_empty(),
        "session dir leaked after failed spawn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_workerd_fails_closed() {
    let _env = workerd_bin_lock().await;
    let listener = Listener::bind(true).await;
    let install = Install::new(listener.port);
    let missing = install.files_dir().join("no-such-workerd");
    let previous = std::env::var_os(WORKERD_BIN_ENV);
    std::env::set_var(WORKERD_BIN_ENV, &missing);
    let plugin = install.plugin();
    let result = tokio::time::timeout(
        SPAWN_TIMEOUT,
        PluginSession::spawn_with(
            &plugin,
            &install.config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &[],
            SessionServices::default(),
        ),
    )
    .await;
    match previous {
        Some(value) => std::env::set_var(WORKERD_BIN_ENV, value),
        None => std::env::remove_var(WORKERD_BIN_ENV),
    }
    let err = match result {
        Ok(Ok(_)) => panic!("spawn succeeded without a workerd binary"),
        Ok(Err(err)) => err,
        Err(_) => panic!("spawn timed out after {SPAWN_TIMEOUT:?}"),
    };
    step(&format!("missing workerd refused: {err}"));
    assert!(
        err.to_string().contains("workerd") || err.to_string().contains("front door"),
        "error should name the missing front door: {err}"
    );
    assert!(session_dirs_under(install.files_dir()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killing_gateway_exits_the_guest() {
    let _env = workerd_bin_lock().await;
    let listener = Listener::bind(true).await;
    let install = Install::new(listener.port);
    let session = install.spawn().await;
    open_session(&session).await;
    let gateway = session.gateway_pid().expect("gateway");
    let guest = session.guest_pid().expect("guest");
    kill_pid(gateway);
    wait_for_exit(gateway).await;
    wait_for_exit(guest).await;
    drop(session);
    step("guest exited after gateway kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killing_guest_errors_rpc_and_exits_gateway() {
    let _env = workerd_bin_lock().await;
    let listener = Listener::bind(true).await;
    let install = Install::new(listener.port);
    let session = install.spawn().await;
    open_session(&session).await;
    let gateway = session.gateway_pid().expect("gateway");
    let guest = session.guest_pid().expect("guest");
    kill_pid(guest);
    wait_for_exit(guest).await;
    let rpc = tokio::time::timeout(ng_harness::RPC_TIMEOUT, session.describe())
        .await
        .unwrap_or_else(|_| ng_harness::fail_deadline("describe hung after the guest was killed"));
    assert!(rpc.is_err(), "RPC must fail after the guest is killed");
    wait_for_exit(gateway).await;
    drop(session);
    step("gateway exited after guest kill");
}

fn revoke_grant(install: &Install) {
    // Edit the persisted row directly. Rediscovering the package hashes the
    // staged guest and can outlast the dial pause, so the fence would land
    // after `TcpStream::connect`.
    let mut grants = PluginGrantStore::load(install.files_dir()).expect("load grants");
    let mut grant = grants.grants.first().cloned().expect("installed grant");
    grant.extra_processes = Some(1);
    grant.domains.insert("revoked.example".into());
    // `upsert` fences a live session before the file is flushed.
    grants.upsert(grant);
    grants.save(install.files_dir()).expect("save grants");
    reconcile_grants_from_disk(install.files_dir());
}

struct ClearDialDelay;
impl Drop for ClearDialDelay {
    fn drop(&mut self) {
        std::env::remove_var("BOOKCLERK_TEST_PROXY_DIAL_DELAY_MS");
    }
}

/// Echo listener that counts accepts and clean EOFs.
struct EofListener {
    port: u16,
    accepts: Arc<AtomicUsize>,
    eofs: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl EofListener {
    async fn bind() -> Self {
        let listener = tokio::net::TcpListener::bind((ng_harness::LOOPBACK, 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let eofs = Arc::new(AtomicUsize::new(0));
        let accepts_task = Arc::clone(&accepts);
        let eofs_task = Arc::clone(&eofs);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                accepts_task.fetch_add(1, Ordering::SeqCst);
                let eofs_task = Arc::clone(&eofs_task);
                tokio::spawn(async move {
                    let mut buf = [0_u8; 64];
                    loop {
                        match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                            Ok(0) | Err(_) => {
                                eofs_task.fetch_add(1, Ordering::SeqCst);
                                break;
                            }
                            Ok(n) => {
                                if tokio::io::AsyncWriteExt::write_all(&mut stream, &buf[..n])
                                    .await
                                    .is_err()
                                {
                                    eofs_task.fetch_add(1, Ordering::SeqCst);
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        Self {
            port,
            accepts,
            eofs,
            task,
        }
    }
}

impl Drop for EofListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grant_revision_bump_fences_the_live_session() {
    let _env = workerd_bin_lock().await;
    let listener = EofListener::bind().await;
    let install = Install::new(listener.port);
    let session = install.spawn().await;
    open_session(&session).await;
    let gateway = session.gateway_pid().expect("gateway");
    let guest = session.guest_pid().expect("guest");
    let files = install.files_dir().to_path_buf();
    let tree = ProcessTree::capture();
    let workerd = tree.pinned_workerd(gateway).unwrap_or_else(|| {
        panic!(
            "pinned workerd missing under gateway {gateway}\n{}",
            tree.describe(gateway)
        )
    });

    let params = CliInvokeParams {
        command: "probe".into(),
        args: vec![
            bookclerk_plugin_sdk::CliArg {
                name: "op".into(),
                value: "hold".into(),
            },
            bookclerk_plugin_sdk::CliArg {
                name: "host".into(),
                value: ng_harness::LOOPBACK.into(),
            },
            bookclerk_plugin_sdk::CliArg {
                name: "port".into(),
                value: listener.port.to_string(),
            },
            bookclerk_plugin_sdk::CliArg {
                name: "payload".into(),
                value: "hold".into(),
            },
        ],
    };
    let hung = tokio::spawn({
        let session_call = async move { session.cli_invoke(params).await };
        session_call
    });
    let ready = Instant::now() + std::time::Duration::from_secs(15);
    while listener.accepts.load(Ordering::SeqCst) < 1 {
        assert!(Instant::now() < ready, "held TCP stream was not accepted");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // `session` was moved into the hung task. Revoke fences that RPC directly.
    // The install (and its parent) stay alive for the directory assertion.
    revoke_grant(&install);
    let rpc = tokio::time::timeout(ng_harness::RPC_TIMEOUT, hung)
        .await
        .unwrap_or_else(|_| ng_harness::fail_deadline("held RPC did not return after revoke"))
        .expect("rpc task");
    assert!(
        rpc.is_err(),
        "hung RPC must fail when the grant changes: {rpc:?}"
    );
    let eof_deadline = Instant::now() + ng_harness::SETTLE_TIMEOUT;
    while listener.eofs.load(Ordering::SeqCst) < 1 {
        assert!(
            Instant::now() < eof_deadline,
            "proxied stream stayed open after revoke"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        listener.accepts.load(Ordering::SeqCst),
        1,
        "proxy accepted another connection after revoke"
    );
    wait_for_exit(gateway).await;
    wait_for_exit(guest).await;
    wait_until_session_dirs_gone(&files, workerd).await;
    step("grant revision bump closed the stream and the session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_during_dial_does_not_open_the_upstream() {
    let _env = workerd_bin_lock().await;
    let _delay = ClearDialDelay;
    std::env::set_var("BOOKCLERK_TEST_PROXY_DIAL_DELAY_MS", "2000");
    let listener = EofListener::bind().await;
    let install = Install::new(listener.port);
    let session = install.spawn().await;
    open_session(&session).await;
    let gateway = session.gateway_pid().expect("gateway");
    let guest = session.guest_pid().expect("guest");
    let files = install.files_dir().to_path_buf();
    let tree = ProcessTree::capture();
    let workerd = tree.pinned_workerd(gateway).unwrap_or_else(|| {
        panic!(
            "pinned workerd missing under gateway {gateway}\n{}",
            tree.describe(gateway)
        )
    });
    let port = listener.port;
    let hung = tokio::spawn(async move {
        session
            .cli_invoke(CliInvokeParams {
                command: "probe".into(),
                args: vec![
                    bookclerk_plugin_sdk::CliArg {
                        name: "op".into(),
                        value: "connect".into(),
                    },
                    bookclerk_plugin_sdk::CliArg {
                        name: "host".into(),
                        value: ng_harness::LOOPBACK.into(),
                    },
                    bookclerk_plugin_sdk::CliArg {
                        name: "port".into(),
                        value: port.to_string(),
                    },
                    bookclerk_plugin_sdk::CliArg {
                        name: "payload".into(),
                        value: "dial".into(),
                    },
                ],
            })
            .await
    });
    let started = Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    revoke_grant(&install);
    let rpc = tokio::time::timeout(ng_harness::RPC_TIMEOUT, hung)
        .await
        .unwrap_or_else(|_| ng_harness::fail_deadline("dial RPC did not return after revoke"))
        .expect("rpc task");
    assert!(
        rpc.is_err(),
        "dial RPC must fail when revoked after {}ms: {rpc:?}",
        started.elapsed().as_millis()
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        listener.accepts.load(Ordering::SeqCst),
        0,
        "dial was committed after the session was revoked"
    );
    wait_for_exit(gateway).await;
    wait_for_exit(guest).await;
    wait_until_session_dirs_gone(&files, workerd).await;
    step("revoke during dial closed the session without an upstream accept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_during_initial_describe_fails_the_spawn() {
    let _env = workerd_bin_lock().await;
    let listener = EofListener::bind().await;
    let install = Install::new(listener.port);
    let plugin = install.plugin();
    let config = install.config.clone();
    let files = install.files_dir().to_path_buf();
    let spawned = tokio::spawn(async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            PluginSession::spawn_with(
                &plugin,
                &config,
                serde_json::json!({}),
                HOST_SHARED_ACCOUNT,
                &[("BOOKCLERK_PROBE_DESCRIBE_DELAY_MS", OsString::from("8000"))],
                SessionServices::default(),
            ),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    revoke_grant(&install);
    let result = spawned.await.expect("spawn task");
    let result = result.unwrap_or_else(|_| ng_harness::fail_deadline("startup revoke hung"));
    assert!(
        result.is_err(),
        "spawn must fail when authority changes during describe"
    );
    assert!(
        session_dirs_under(&files).is_empty(),
        "startup failure left a session dir"
    );
    assert_eq!(listener.accepts.load(Ordering::SeqCst), 0);
    step("revoke during describe failed the spawn and removed the session dir");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_and_concurrent_spawn_cycles_do_not_leak() {
    let _env = workerd_bin_lock().await;
    let fds_before = linux_fd_count();
    let listener = Listener::bind(true).await;
    let install = Install::new(listener.port);
    assert!(
        install.files_dir().is_dir(),
        "installation parent missing before churn"
    );
    for i in 0..20 {
        finish_clean_session(&install, &listener, &format!("cycle-{i}")).await;
    }
    tokio::join!(
        finish_clean_session(&install, &listener, "par-0"),
        finish_clean_session(&install, &listener, "par-1"),
        finish_clean_session(&install, &listener, "par-2"),
        finish_clean_session(&install, &listener, "par-3"),
    );
    assert!(
        install.files_dir().is_dir(),
        "installation parent was removed before the session-dir check"
    );
    assert!(
        session_dirs_under(install.files_dir()).is_empty(),
        "churn left session dirs under {}",
        install.files_dir().display()
    );
    if let (Some(before), Some(after)) = (fds_before, linux_fd_count()) {
        assert!(
            after <= before + 8,
            "fd leak: before={before} after={after}"
        );
    }
    step("20 sequential + 4 concurrent cycles left no session dirs or pids");
    drop(install);
}

/// One spawn on a shared installation. The install stays alive so an empty
/// session-dir listing means production cleanup removed the directory.
async fn finish_clean_session(install: &Install, listener: &Listener, label: &str) {
    let session = install.spawn().await;
    open_session(&session).await;
    let outcome = probe(&session, "connect", listener.port, label).await;
    assert_eq!(outcome["ok"], true, "{label}: {outcome}");
    let gateway = session.gateway_pid().expect("gateway");
    let guest = session.guest_pid().expect("guest");
    let tree = ProcessTree::capture();
    let workerd = tree.pinned_workerd(gateway).unwrap_or_else(|| {
        panic!(
            "{label}: pinned workerd missing under gateway {gateway}\n{}",
            tree.describe(gateway)
        )
    });
    let descendant = guest_descendant(&session, guest, label).await;
    assert!(process_alive(workerd), "{label}: pinned workerd {workerd}");
    assert!(
        process_alive(descendant),
        "{label}: guest descendant {descendant}"
    );
    assert_ne!(workerd, gateway, "{label}: workerd pid is the supervisor");
    assert_ne!(
        descendant, guest,
        "{label}: guest descendant pid is the supervisor"
    );
    let cgroup = linux_session_cgroup(guest).or_else(|| linux_session_cgroup(gateway));
    #[cfg(windows)]
    let sid = session.package_sid().map(str::to_string);
    let session_dir = session.session_dir().map(std::path::Path::to_path_buf);
    drop(session);
    wait_for_exit(gateway).await;
    wait_for_exit(guest).await;
    wait_for_exit(workerd).await;
    wait_for_exit(descendant).await;
    assert!(
        install.files_dir().is_dir(),
        "{label}: installation parent disappeared"
    );
    if let Some(dir) = session_dir {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while dir.exists() && Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !dir.exists(),
            "{label}: session dir remains {}",
            dir.display()
        );
    }
    match cgroup {
        Some(dir) => {
            let deadline = Instant::now() + SETTLE_TIMEOUT;
            while dir.exists() && Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let procs = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default();
            assert!(
                !dir.exists(),
                "{label}: cgroup {} remains with members:\n{procs}",
                dir.display()
            );
        }
        None => note_missing_cgroup(),
    }
    #[cfg(windows)]
    if let Some(sid) = sid.as_deref() {
        let plugin = install
            .files_dir()
            .join("plugins")
            .join(ng_harness::PLUGIN_ID);
        for path in [install.files_dir(), plugin.as_path()] {
            let mentioned = bookclerk_sandbox::spawn::dacl_mentions_sid(path, sid)
                .unwrap_or_else(|err| panic!("{label}: DACL read {}: {err}", path.display()));
            assert!(
                !mentioned,
                "{label}: package SID {sid} remains on {}",
                path.display()
            );
        }
    }
}

/// Child of the guest supervisor. Unix `exec` replaces the jail, so the probe
/// forks a `pause` sleeper. Windows keeps `bookclerk-jail` and the probe is
/// already that child.
async fn guest_descendant(session: &PluginSession, guest: u32, label: &str) -> u32 {
    let tree = ProcessTree::capture();
    if let Some(pid) = tree.live_descendant(guest) {
        return pid;
    }
    let outcome = probe(session, "descendant", 0, "").await;
    assert_eq!(outcome["ok"], true, "{label}: descendant probe {outcome}");
    let pid = u32::try_from(outcome["pid"].as_u64().expect("descendant pid"))
        .expect("descendant pid fits u32");
    let tree = ProcessTree::capture();
    assert!(
        tree.descendants(guest).contains(&pid),
        "{label}: sleeper {pid} is not under guest {guest}\n{}",
        tree.describe(guest)
    );
    pid
}

/// Waits until production cleanup removes the session directory.
///
/// `files` is the installation parent and must still exist. An empty session
/// listing then means the directory was removed, not that the fixture was
/// dropped. Pinned `workerd` keeps the directory as its cwd until it exits.
async fn wait_until_session_dirs_gone(files: &std::path::Path, workerd: u32) {
    wait_for_exit(workerd).await;
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        assert!(
            files.is_dir(),
            "installation parent disappeared before session cleanup"
        );
        if session_dirs_under(files).is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session dir leaked under {}",
            files.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn note_missing_cgroup() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(false);
    if ONCE.swap(true, Ordering::SeqCst) {
        return;
    }
    eprintln!(
        "native_gateway: delegated cgroup unavailable; process-group kill is the fallback \
         and does not cover a descendant that calls setsid"
    );
}

/// Required isolation must fail before either sibling starts when the outer
/// session Job cannot be created or configured.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outer_session_job_failure_leaves_no_session_and_no_proxy() {
    let _env = workerd_bin_lock().await;
    let listener = Listener::bind(false).await;
    let install = Install::new(listener.port);
    struct ClearJobFail;
    impl Drop for ClearJobFail {
        fn drop(&mut self) {
            std::env::remove_var("BOOKCLERK_TEST_FAIL_SESSION_JOB");
        }
    }
    for mode in ["create", "configure"] {
        let _clear = ClearJobFail;
        std::env::set_var("BOOKCLERK_TEST_FAIL_SESSION_JOB", mode);
        let plugin = install.plugin();
        let spawned = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            PluginSession::spawn_with(
                &plugin,
                &install.config,
                serde_json::json!({}),
                HOST_SHARED_ACCOUNT,
                &[],
                SessionServices::default(),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{mode}: spawn hung"));
        // `PluginSession` is not `Debug`, so this cannot use `expect_err`.
        let Err(err) = spawned else {
            panic!("{mode}: required isolation returned a session");
        };
        std::env::remove_var("BOOKCLERK_TEST_FAIL_SESSION_JOB");
        let text = err.to_string();
        assert!(text.contains("outer session Job"), "{mode}: {text}");
        assert!(text.contains(mode), "{mode}: {text}");
        let dirs = session_dirs_under(install.files_dir());
        assert!(dirs.is_empty(), "{mode} left session dirs {dirs:?}");
        assert_eq!(
            listener.accepts(),
            0,
            "{mode} accepted a proxied connection"
        );
    }
}

struct ClearHoldEnv;

impl Drop for ClearHoldEnv {
    fn drop(&mut self) {
        std::env::remove_var("BOOKCLERK_TEST_STARTUP_HOLD_DIR");
        std::env::remove_var("BOOKCLERK_TEST_DESCRIBE_HOLD_DIR");
    }
}

fn hold_pid(dir: &std::path::Path, name: &str) -> Option<u32> {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

fn held_pids(dir: &std::path::Path) -> Vec<u32> {
    let mut pids = Vec::new();
    for name in ["gateway_pid", "guest_pid"] {
        let Some(pid) = hold_pid(dir, name) else {
            continue;
        };
        pids.push(pid);
        pids.extend(ProcessTree::capture().descendants(pid));
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

async fn wait_until_holding(dir: &std::path::Path) {
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while !dir.join("holding").is_file() {
        if Instant::now() >= deadline {
            let _ = std::fs::write(dir.join("release"), b"1");
            ng_harness::fail_deadline(&format!("hold was not reached under {}", dir.display()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn assert_hold_cleaned(
    files: &std::path::Path,
    hold: &std::path::Path,
    pids: &[u32],
    cgroup: Option<std::path::PathBuf>,
    regs_before: usize,
    mux_before: usize,
    label: &str,
) {
    for pid in pids {
        let deadline = Instant::now() + ng_harness::EXIT_TIMEOUT;
        while process_alive(*pid) {
            if Instant::now() >= deadline {
                ng_harness::fail_deadline(&format!("{label}: pid {pid} still running"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        if session_dirs_under(files).is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            ng_harness::fail_deadline(&format!(
                "{label}: session dir leaked under {}",
                files.display()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if let Some(dir) = cgroup {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while dir.exists() && Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(!dir.exists(), "{label}: cgroup {} remains", dir.display());
    }
    let reg_deadline = Instant::now() + SETTLE_TIMEOUT;
    while bookclerk_plugin_host::live_session_count() != regs_before {
        if Instant::now() >= reg_deadline {
            ng_harness::fail_deadline(&format!(
                "{label}: live sessions {} != {regs_before}",
                bookclerk_plugin_host::live_session_count()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mux_deadline = Instant::now() + SETTLE_TIMEOUT;
    while bookclerk_plugin_sdk::mux::live_mux_task_count() > mux_before {
        if Instant::now() >= mux_deadline {
            ng_harness::fail_deadline(&format!(
                "{label}: mux tasks {} > {mux_before}",
                bookclerk_plugin_sdk::mux::live_mux_task_count()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    #[cfg(windows)]
    {
        let sid = std::fs::read_to_string(hold.join("package_sid"))
            .unwrap_or_else(|err| panic!("{label}: package sid missing after the hold: {err}"));
        let sid = sid.trim();
        assert!(!sid.is_empty(), "{label}: empty package sid");
        let plugin = files.join("plugins").join(ng_harness::PLUGIN_ID);
        for path in [files, plugin.as_path()] {
            let mentioned = bookclerk_sandbox::spawn::dacl_mentions_sid(path, sid)
                .unwrap_or_else(|err| panic!("{label}: DACL read {}: {err}", path.display()));
            assert!(
                !mentioned,
                "{label}: package SID {sid} remains on {}",
                path.display()
            );
        }
    }
    #[cfg(not(windows))]
    let _ = hold;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_spawn_during_describe_reaps_the_session() {
    let _env = workerd_bin_lock().await;
    let _clear = ClearHoldEnv;
    let listener = Listener::bind(false).await;
    let install = Install::new(listener.port);
    let hold = tempfile::tempdir().expect("describe hold dir");
    std::env::set_var("BOOKCLERK_TEST_DESCRIBE_HOLD_DIR", hold.path());
    let regs_before = bookclerk_plugin_host::live_session_count();
    let mux_before = bookclerk_plugin_sdk::mux::live_mux_task_count();
    let plugin = install.plugin();
    let config = install.config.clone();
    let spawned = tokio::spawn(async move {
        PluginSession::spawn_with(
            &plugin,
            &config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &[],
            SessionServices::default(),
        )
        .await
    });
    wait_until_holding(hold.path()).await;
    if bookclerk_plugin_host::live_session_count() != regs_before + 1 {
        spawned.abort();
        let _ = std::fs::write(hold.path().join("release"), b"1");
        ng_harness::fail_deadline(&format!(
            "describe hold was not registered: {} vs {regs_before}",
            bookclerk_plugin_host::live_session_count()
        ));
    }
    let pids = held_pids(hold.path());
    if pids.is_empty() {
        spawned.abort();
        let _ = std::fs::write(hold.path().join("release"), b"1");
        ng_harness::fail_deadline("describe hold published no sibling pids");
    }
    let cgroup = hold_pid(hold.path(), "gateway_pid")
        .and_then(linux_session_cgroup)
        .or_else(|| hold_pid(hold.path(), "guest_pid").and_then(linux_session_cgroup));
    spawned.abort();
    let _ = spawned.await;
    assert_hold_cleaned(
        install.files_dir(),
        hold.path(),
        &pids,
        cgroup,
        regs_before,
        mux_before,
        "abort during describe",
    )
    .await;
    assert_eq!(listener.accepts(), 0, "aborted describe accepted a proxy");
    step("abort during describe reaped siblings, mux tasks, and the registration");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_before_register_fails_startup() {
    let _env = workerd_bin_lock().await;
    let _clear = ClearHoldEnv;
    let listener = Listener::bind(false).await;
    let install = Install::new(listener.port);
    let hold = tempfile::tempdir().expect("startup hold dir");
    std::env::set_var("BOOKCLERK_TEST_STARTUP_HOLD_DIR", hold.path());
    let regs_before = bookclerk_plugin_host::live_session_count();
    let mux_before = bookclerk_plugin_sdk::mux::live_mux_task_count();
    let plugin = install.plugin();
    let config = install.config.clone();
    let spawned = tokio::spawn(async move {
        PluginSession::spawn_with(
            &plugin,
            &config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &[],
            SessionServices::default(),
        )
        .await
    });
    wait_until_holding(hold.path()).await;
    if bookclerk_plugin_host::live_session_count() != regs_before {
        let _ = std::fs::write(hold.path().join("release"), b"1");
        ng_harness::fail_deadline("startup hold registered the session before describe");
    }
    let pids = held_pids(hold.path());
    if pids.is_empty() {
        let _ = std::fs::write(hold.path().join("release"), b"1");
        ng_harness::fail_deadline("startup hold published no sibling pids");
    }
    let cgroup = hold_pid(hold.path(), "gateway_pid")
        .and_then(linux_session_cgroup)
        .or_else(|| hold_pid(hold.path(), "guest_pid").and_then(linux_session_cgroup));
    revoke_grant(&install);
    std::fs::write(hold.path().join("release"), b"1").expect("release startup hold");
    let joined = tokio::time::timeout(SPAWN_TIMEOUT, spawned).await;
    let result = match joined {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => ng_harness::fail_deadline(&format!("startup task panicked: {err}")),
        Err(_) => ng_harness::fail_deadline("revoke-before-register spawn hung"),
    };
    match result {
        Ok(_) => ng_harness::fail_deadline("spawn succeeded after a pre-registration revoke"),
        Err(err) => {
            let text = err.to_string();
            if !text.contains("fenced") {
                ng_harness::fail_deadline(&format!("expected a fenced startup failure: {text}"));
            }
        }
    }
    assert_hold_cleaned(
        install.files_dir(),
        hold.path(),
        &pids,
        cgroup,
        regs_before,
        mux_before,
        "revoke before register",
    )
    .await;
    assert_eq!(
        listener.accepts(),
        0,
        "pre-registration revoke accepted a proxy"
    );
    step("revoke before register failed startup and removed the session");
}

/// `pids.max` is the pinned infrastructure thread budget plus `extraProcesses`.
///
/// Startup must finish on a multi-core host. The next thread is denied only
/// when a delegated cgroup was actually applied. A missing leaf is recorded
/// and is not an enforcement pass. GitHub-hosted runners often cannot
/// delegate; that skip stays a separate result from the denial assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_pids_max_denies_the_next_thread() {
    let _env = workerd_bin_lock().await;
    let listener = Listener::bind(false).await;
    let install = Install::new(listener.port);
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cpus < 2 {
        eprintln!(
            "native_gateway: single-core host; the multi-core pids.max case was not asserted"
        );
        return;
    }
    let session = install.spawn().await;
    open_session(&session).await;
    let guest = session.guest_pid().expect("guest");
    let Some(cgroup) = linux_session_cgroup(guest)
        .or_else(|| session.gateway_pid().and_then(linux_session_cgroup))
    else {
        eprintln!(
            "native_gateway: delegated cgroup unavailable; pids.max enforcement was not asserted"
        );
        drop(session);
        return;
    };
    let text = std::fs::read_to_string(cgroup.join("pids.max"))
        .unwrap_or_else(|err| panic!("read pids.max: {err}"));
    let applied: u32 = text
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("pids.max {text:?}"));
    let expected = bookclerk_sandbox::INFRASTRUCTURE_THREAD_BUDGET + 2;
    assert_eq!(
        applied, expected,
        "pids.max {applied} is not the infrastructure budget {expected} plus default extra 2"
    );
    assert_ne!(
        applied,
        5 + 2,
        "pids.max must not copy the Windows outer cap"
    );
    assert_ne!(
        applied,
        3 + 2,
        "pids.max must not copy the payload process count"
    );
    let outcome = probe(&session, "exhaust_threads", 0, &applied.to_string()).await;
    let created = outcome["created"].as_u64().unwrap_or(0);
    let error = outcome["error"].as_str().unwrap_or("");
    assert!(
        outcome["ok"] == true && created > 0 && created < u64::from(applied),
        "next thread was not denied under pids.max {applied}: {outcome}"
    );
    assert!(
        error.contains("os error 11") || error.to_ascii_lowercase().contains("temporarily"),
        "denial should be EAGAIN, got {error}"
    );
    step(&format!(
        "pids.max {applied} on {cpus} cpus denied thread {} ({error})",
        created + 1
    ));
    drop(session);
}

/// Persisted `extraProcesses` for the installed probe, read on the next spawn.
#[cfg(windows)]
fn set_extra_processes(install: &Install, extra: Option<u32>) {
    let mut grants = PluginGrantStore::load(install.files_dir()).expect("load grants");
    let mut grant = grants.grants.first().cloned().expect("installed grant");
    grant.extra_processes = extra;
    grants.upsert(grant);
    grants.save(install.files_dir()).expect("save grants");
    reconcile_grants_from_disk(install.files_dir());
}

/// Real `PluginSession::spawn_with` of the gateway and guest.
///
/// Extras 0, 1, and the default each allow that many direct children inside
/// the guest Job. Each child is this probe holding one slot (`--hold-job-slot`),
/// not `cmd /c` and not `ping.exe` (AppContainer CreateProcess of that System32
/// image is access-denied, and Bookclerk does not ACE System32). The next child
/// is denied. A CPU rate on the queried Job means the outer session Job; a
/// missing rate means the inner Job, whose cap is one guest plus the extra
/// allowance.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_job_extras_deny_the_next_direct_ping() {
    let _env = workerd_bin_lock().await;
    for extra in [Some(0_u32), Some(1), None] {
        let children = extra.unwrap_or(bookclerk_plugin_host::PLUGIN_JAIL_EXTRA_PROCESSES_DEFAULT);
        let inner = 1 + children;
        let outer = bookclerk_plugin_host::windows_session_active_processes(children);
        let listener = Listener::bind(false).await;
        let install = Install::new(listener.port);
        set_extra_processes(&install, extra);
        let session = install.spawn().await;
        open_session(&session).await;
        let job = probe(&session, "job_limits", 0, "").await;
        assert_eq!(job["in_job"], true, "guest is not in a Job: {job}");
        if let Some(limit) = job["active_limit"].as_u64() {
            let limit = u32::try_from(limit).unwrap_or(u32::MAX);
            if job["cpu_rate"].is_null() {
                assert_eq!(
                    limit, inner,
                    "a Job with CPU off must be the inner guest cap {inner}: {job}"
                );
            } else {
                assert_eq!(
                    limit, outer,
                    "a Job with a CPU rate must be the outer session cap {outer}: {job}"
                );
                assert_eq!(job["cpu_hard_cap"], true, "{job}");
                let expect = bookclerk_sandbox::windows_job_cpu_rate(
                    bookclerk_plugin_host::PLUGIN_JAIL_CPU_RATE_DEFAULT,
                    bookclerk_sandbox::host_logical_cpus(),
                );
                assert_eq!(
                    job["cpu_rate"].as_u64(),
                    Some(u64::from(expect)),
                    "outer CpuRate {job}"
                );
            }
        }
        for n in 0..children {
            let started = probe(&session, "spawn_ping", 0, "").await;
            assert_eq!(
                started["ok"], true,
                "job-slot child {n} of {children} was refused: {started}; job {job}"
            );
            let observed = probe(&session, "job_limits", 0, "").await;
            if observed["cpu_rate"].is_null() {
                if let Some(active) = observed["active_processes"].as_u64() {
                    assert_eq!(
                        active,
                        u64::from(1 + n + 1),
                        "each granted child is one process in the inner Job: {observed}"
                    );
                }
            }
        }
        let denied = probe(&session, "spawn_ping", 0, "").await;
        let os = denied["os"].as_u64().unwrap_or(0);
        assert_eq!(
            denied["ok"], false,
            "child past the grant started: {denied}"
        );
        assert!(
            os == 5 || os == 1816,
            "expected ERROR_ACCESS_DENIED (5) or ERROR_NOT_ENOUGH_QUOTA (1816), got {denied}"
        );
        step(&format!(
            "windows extra {children} inner {inner} outer {outer} denied ping os {os}: {job}"
        ));
        drop(session);
    }
}

/// Seatbelt guest IPC: the host OAuth callback tunnel and the `.s.PGSQL.5432` mediator.
///
/// Skip only when Seatbelt cannot be applied and enforcement is not demanded.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seatbelt_oauth_callback_and_postgres_mediator_use_guest_ipc() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _env = workerd_bin_lock().await;
    let listener = Listener::bind(false).await;
    let install = Install::new(listener.port);
    let plugin = install.plugin();
    let spawned = tokio::time::timeout(
        SPAWN_TIMEOUT,
        PluginSession::spawn_with(
            &plugin,
            &install.config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &[],
            SessionServices::default(),
        ),
    )
    .await;
    let session = match spawned {
        Ok(Ok(session)) => session,
        Ok(Err(err)) => {
            let text = err.to_string();
            let seatbelt = text.to_ascii_lowercase().contains("seatbelt")
                || text.to_ascii_lowercase().contains("sandbox_init");
            let demanded = std::env::var("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT")
                .is_ok_and(|value| !value.trim().is_empty());
            if seatbelt && !demanded {
                eprintln!(
                    "native_gateway: Seatbelt could not be applied ({text}); \
                     oauth callback and postgres mediator were not asserted"
                );
                return;
            }
            ng_harness::fail_deadline(&format!("spawn failed: {text}"));
        }
        Err(_) => ng_harness::fail_deadline("seatbelt spawn timed out"),
    };
    open_session(&session).await;
    let ipc = session
        .guest_ipc_dir()
        .unwrap_or_else(|| ng_harness::fail_deadline("jailed guest has no IPC directory"))
        .to_path_buf();
    let proxy = bookclerk_plugin_host::CallbackProxy::start(None, &ipc, session.package_sid())
        .await
        .unwrap_or_else(|err| {
            ng_harness::fail_deadline(&format!("oauth callback listener failed: {err}"))
        });
    let endpoint = proxy.ipc_endpoint.clone();
    let tcp_addr = proxy.bind_addr();
    let pg_path = ipc.join(".s.PGSQL.5432");
    let guest = async { probe(&session, "serve_ipc", 0, &endpoint).await };
    let host = async {
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !pg_path.exists() {
            if Instant::now() >= deadline {
                ng_harness::fail_deadline(
                    "postgres mediator socket was not bound in the guest IPC directory",
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let mut pg = tokio::net::UnixStream::connect(&pg_path)
            .await
            .unwrap_or_else(|err| {
                ng_harness::fail_deadline(&format!("connect postgres mediator: {err}"))
            });
        pg.write_all(b"startup").await.expect("startup");
        let mut ack = [0u8; 4];
        pg.read_exact(&mut ack).await.expect("PGOK");
        assert_eq!(&ack, b"PGOK", "mediator socket did not accept a client");
        let mut tcp = tokio::net::TcpStream::connect(tcp_addr)
            .await
            .unwrap_or_else(|err| ng_harness::fail_deadline(&format!("oauth TCP: {err}")));
        tcp.write_all(b"oauth-ok").await.expect("oauth write");
        let mut echo = [0u8; 8];
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tcp.read_exact(&mut echo),
        )
        .await
        .unwrap_or_else(|_| ng_harness::fail_deadline("oauth echo timed out"))
        .unwrap_or_else(|err| ng_harness::fail_deadline(&format!("oauth echo: {err}")));
        assert_eq!(&echo, b"oauth-ok");
        let _ = tcp.shutdown().await;
    };
    let (outcome, ()) = tokio::join!(guest, host);
    assert_eq!(outcome["ok"], true, "{outcome}");
    assert_eq!(outcome["oauth"], "oauth-ok", "{outcome}");
    assert_eq!(outcome["postgres"], "startup", "{outcome}");
    assert!(
        pg_path.starts_with(&ipc),
        "mediator socket {} is outside {}",
        pg_path.display(),
        ipc.display()
    );
    step("seatbelt guest IPC served the oauth callback and the postgres mediator");
    drop(proxy);
    drop(session);
}
