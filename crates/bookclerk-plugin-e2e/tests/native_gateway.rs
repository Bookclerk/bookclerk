//! Native-behind-workerd gateway smoke on the production launch path.
//!
//! Installs the test-only `native_gateway_probe` guest into a temporary files
//! dir, grants exactly one loopback TCP port, and spawns it through
//! [`PluginSession::spawn_with`] with `Isolation::Required`: host discovery and
//! launch planning, `bookclerk-jail`, `bookclerk-workerd` + pinned `workerd`,
//! the nested deny-network jail, and the platform socket proxy (Unix socket, or
//! a Package-SID-ACL'd named pipe on Windows). Nothing here skips: a missing
//! helper, runtime or confinement backend fails the test.
//!
//! Run with `cargo test -p bookclerk-plugin-e2e --test native_gateway` after
//! `cargo build -p bookclerk-jail -p bookclerk-workerd` and `cargo ensure-workerd`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, CliInvokeParams, DiscoveredPlugin, Entrypoint,
    PluginGrantStore, PluginSession, SessionServices, HOST_SHARED_ACCOUNT,
};
use bookclerk_plugin_sdk::{BindingValues, CliArg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const PLUGIN_ID: &str = "native_gateway_probe";
const SPAWN_TIMEOUT: Duration = Duration::from_secs(120);
const RPC_TIMEOUT: Duration = Duration::from_secs(45);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const LOOPBACK: &str = "127.0.0.1";

/// Harness-held loopback listener that counts accepts and aborts on drop.
struct Listener {
    port: u16,
    accepts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Listener {
    /// Binds an ephemeral loopback port; `echo` copies every byte back.
    async fn bind(echo: bool) -> Self {
        let listener = TcpListener::bind((LOOPBACK, 0))
            .await
            .expect("bind loopback listener");
        let port = listener.local_addr().expect("local addr").port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                if echo {
                    tokio::spawn(async move {
                        let mut buf = [0_u8; 4096];
                        while let Ok(n) = stream.read(&mut buf).await {
                            if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
        });
        Self {
            port,
            accepts,
            task,
        }
    }

    fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }

    /// Waits until at least `n` connections were accepted.
    async fn wait_for_accepts(&self, n: usize) -> bool {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while Instant::now() < deadline {
            if self.accepts() >= n {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.accepts() >= n
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Temporary files dir with the probe installed and granted `tcp_port` only.
struct Install {
    _files: tempfile::TempDir,
    config: Config,
}

impl Install {
    fn new(tcp_port: u16) -> Self {
        let files = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_files_dir(files.path().to_path_buf());
        let root = paths.files_dir.join("plugins").join(PLUGIN_ID);
        std::fs::create_dir_all(&root).expect("install dir");
        let exe_name = format!("{PLUGIN_ID}{}", std::env::consts::EXE_SUFFIX);
        std::fs::copy(probe_binary(), root.join(&exe_name)).expect("copy probe guest");
        std::fs::write(root.join("plugin.toml"), manifest(&exe_name, tcp_port)).expect("manifest");

        let mut config = Config {
            paths: Some(paths),
            ..Default::default()
        };
        config.plugins.isolation = Isolation::Required;
        let plugin = find_probe(&config);
        let mut grants = PluginGrantStore::default();
        grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
        grants
            .save(&config.paths().files_dir)
            .expect("write plugin-grants.json");
        Self {
            _files: files,
            config,
        }
    }
}

fn probe_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_native_gateway_probe"));
    assert!(path.is_file(), "probe guest missing at {}", path.display());
    path
}

fn manifest(exe_name: &str, tcp_port: u16) -> String {
    format!(
        "api_version = 3\n\
         id = \"{PLUGIN_ID}\"\n\
         name = \"Native gateway probe\"\n\
         version = \"0.0.0\"\n\
         runtime = \"native\"\n\
         command = \"./{exe_name}\"\n\
         entrypoints = [\"cli\"]\n\
         \n\
         [capabilities.network]\n\
         mode = \"outbound\"\n\
         tcp = [{{ host = \"{LOOPBACK}\", ports = [{tcp_port}] }}]\n\
         address_cidrs = [\"{LOOPBACK}/32\"]\n"
    )
}

fn find_probe(config: &Config) -> DiscoveredPlugin {
    discover_plugins(config)
        .expect("discover")
        .into_iter()
        .find(|found| found.manifest.id == PLUGIN_ID)
        .expect("probe plugin discovered")
}

/// One `probe` CLI round trip; returns the guest's JSON outcome.
async fn probe(session: &PluginSession, op: &str, port: u16, payload: &str) -> serde_json::Value {
    let arg = |name: &str, value: &str| CliArg {
        name: name.into(),
        value: value.into(),
    };
    let params = CliInvokeParams {
        command: "probe".into(),
        args: vec![
            arg("op", op),
            arg("host", LOOPBACK),
            arg("port", &port.to_string()),
            arg("payload", payload),
        ],
    };
    let result = tokio::time::timeout(RPC_TIMEOUT, session.cli_invoke(params))
        .await
        .unwrap_or_else(|_| panic!("probe {op}:{port} timed out after {RPC_TIMEOUT:?}"))
        .unwrap_or_else(|err| panic!("probe {op}:{port} RPC failed: {err}"));
    assert_eq!(result.exit_code, 0, "probe {op} stderr: {}", result.stderr);
    serde_json::from_str(&result.stdout)
        .unwrap_or_else(|err| panic!("probe {op} stdout is not JSON ({err}): {}", result.stdout))
}

fn error_text(outcome: &serde_json::Value) -> &str {
    outcome["error"].as_str().unwrap_or_default()
}

/// True while `pid` is a live (non-zombie) process.
fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X")
        })
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|out| {
                let stat = String::from_utf8_lossy(&out.stdout);
                let stat = stat.trim();
                !stat.is_empty() && !stat.starts_with('Z')
            })
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        false
    }
}

async fn wait_for_exit(pid: u32) {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "guest launcher pid {pid} still running {EXIT_TIMEOUT:?} after the session dropped"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn step(message: &str) {
    eprintln!("native_gateway: {message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_guest_reaches_only_granted_tcp_through_the_front_door() {
    let allowed = Listener::bind(true).await;
    let denied = Listener::bind(false).await;
    assert_ne!(allowed.port, denied.port);
    let install = Install::new(allowed.port);
    let plugin = find_probe(&install.config);
    step(&format!(
        "granted {LOOPBACK}:{} only; ungranted live listener on {}",
        allowed.port, denied.port
    ));

    let started = Instant::now();
    let session = tokio::time::timeout(
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
    .await
    .unwrap_or_else(|_| panic!("spawn timed out after {SPAWN_TIMEOUT:?}"))
    .unwrap_or_else(|err| panic!("spawn through the workerd front door failed: {err}"));
    step(&format!("spawned in {:?}", started.elapsed()));
    assert!(
        session.session_key().contains("native-behind-workerd"),
        "guest must run behind the workerd front door: {}",
        session.session_key()
    );
    let launcher_pid = session.guest_pid().expect("launcher pid");

    let describe = tokio::time::timeout(RPC_TIMEOUT, session.describe())
        .await
        .expect("describe timed out")
        .expect("describe");
    assert_eq!(describe.id, PLUGIN_ID);
    assert_eq!(
        describe.capabilities,
        plugin.manifest.capabilities(),
        "describe() capabilities must equal the installed plugin.toml"
    );
    tokio::time::timeout(RPC_TIMEOUT, session.open(BindingValues::default()))
        .await
        .expect("open timed out")
        .expect("open");
    assert!(session.has_entrypoint(Entrypoint::Cli));
    step("describe + open ok");

    #[cfg(windows)]
    {
        let sid = session
            .package_sid()
            .expect("host must pre-create the nested AppContainer and propagate its SID");
        assert!(sid.starts_with("S-1-15-2-"), "unexpected Package SID {sid}");
        step(&format!("nested AppContainer Package SID {sid}"));
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
    tokio::time::sleep(Duration::from_millis(200)).await;
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
        "direct TCP from the nested jail reached the host: {outcome}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
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

    drop(session);
    wait_for_exit(launcher_pid).await;
    step(&format!("launcher pid {launcher_pid} exited after drop"));
    drop(install);
    step(&format!("total {:?}", started.elapsed()));
}
