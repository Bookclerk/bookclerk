//! Shared harness for `native_gateway*` integration tests.

#![allow(dead_code)]
#![allow(clippy::missing_docs_in_private_items)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, CliInvokeParams, DiscoveredPlugin, PluginGrantStore,
    PluginSession, SessionServices, HOST_SHARED_ACCOUNT,
};
use bookclerk_plugin_sdk::{BindingValues, CliArg};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub const PLUGIN_ID: &str = "native_gateway_probe";
/// Outer bound for `PluginSession::spawn_with`.
///
/// On Windows this must outlast the host jail-ready wait (180s), which itself
/// outlasts the DACL mutex wait (120s), so a queued sibling launch fails in
/// the jail instead of being killed by this deadline first.
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(240);
pub const RPC_TIMEOUT: Duration = Duration::from_secs(45);
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
pub const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
pub const LOOPBACK: &str = "127.0.0.1";

/// Harness-held loopback listener that counts accepts and aborts on drop.
pub struct Listener {
    pub port: u16,
    accepts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Listener {
    /// Binds an ephemeral loopback port; `echo` copies every byte back.
    pub async fn bind(echo: bool) -> Self {
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

    pub fn accepts(&self) -> usize {
        self.accepts.load(Ordering::SeqCst)
    }

    /// Waits until at least `n` connections were accepted.
    pub async fn wait_for_accepts(&self, n: usize) -> bool {
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
pub struct Install {
    _files: tempfile::TempDir,
    pub config: Config,
}

impl Install {
    pub fn new(tcp_port: u16) -> Self {
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

    pub fn files_dir(&self) -> &Path {
        &self.config.paths().files_dir
    }

    pub fn grants_path(&self) -> PathBuf {
        self.files_dir().join("plugin-grants.json")
    }

    pub fn plugin(&self) -> DiscoveredPlugin {
        find_probe(&self.config)
    }

    pub async fn spawn(&self) -> PluginSession {
        self.spawn_with_env(&[]).await
    }

    pub async fn spawn_with_env(&self, extra_env: &[(&str, std::ffi::OsString)]) -> PluginSession {
        let plugin = self.plugin();
        tokio::time::timeout(
            SPAWN_TIMEOUT,
            PluginSession::spawn_with(
                &plugin,
                &self.config,
                serde_json::json!({}),
                HOST_SHARED_ACCOUNT,
                extra_env,
                SessionServices::default(),
            ),
        )
        .await
        .unwrap_or_else(|_| fail_deadline(&format!("spawn timed out after {SPAWN_TIMEOUT:?}")))
        .unwrap_or_else(|err| panic!("spawn through the workerd front door failed: {err}"))
    }
}

/// End the test process when an RPC deadline expires.
///
/// A panic leaves the Cap'n Proto vat thread blocked inside the guest call,
/// so the test binary never exits and the CI job runs until the workflow
/// timeout. Exiting fails the same assertion and lets the job report it.
pub fn fail_deadline(what: &str) -> ! {
    let diag = bookclerk_plugin_host::recent_spawn_diagnostics();
    if !diag.is_empty() {
        eprintln!("native_gateway: startup diagnostics before exit:\n{diag}");
    }
    let platform = bookclerk_sandbox::recent_platform_spawn_diagnostics();
    if !platform.is_empty() {
        eprintln!("native_gateway: platform startup diagnostics before exit:\n{platform}");
    }
    eprintln!("native_gateway: {what}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    std::process::exit(1);
}

pub fn probe_binary() -> PathBuf {
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
pub async fn probe(
    session: &PluginSession,
    op: &str,
    port: u16,
    payload: &str,
) -> serde_json::Value {
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
        .unwrap_or_else(|_| {
            fail_deadline(&format!(
                "probe {op}:{port} timed out after {RPC_TIMEOUT:?}"
            ))
        })
        .unwrap_or_else(|err| panic!("probe {op}:{port} RPC failed: {err}"));
    assert_eq!(result.exit_code, 0, "probe {op} stderr: {}", result.stderr);
    serde_json::from_str(&result.stdout)
        .unwrap_or_else(|err| panic!("probe {op} stdout is not JSON ({err}): {}", result.stdout))
}

pub fn error_text(outcome: &serde_json::Value) -> &str {
    outcome["error"].as_str().unwrap_or_default()
}

/// True while `pid` is a live (non-zombie) process.
pub fn process_alive(pid: u32) -> bool {
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

pub async fn wait_for_exit(pid: u32) {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "pid {pid} still running {EXIT_TIMEOUT:?} after the session dropped"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status();
    }
}

pub fn linux_fd_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/fd")
            .ok()
            .map(|rd| rd.filter_map(Result::ok).count())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Wait until host `RemoveOnDrop` finishes removing session directories.
///
/// Process exit returns before that retry loop. The assertion still fails
/// when a directory remains after [`SETTLE_TIMEOUT`].
pub async fn assert_no_session_dirs(files: &Path) {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let left = session_dirs_under(files);
        if left.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "session dirs remain under {}: {}",
            files.display(),
            left.iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn session_dirs_under(files: &Path) -> Vec<PathBuf> {
    let root = files.join("plugin-state");
    let mut found = Vec::new();
    let Ok(walk) = std::fs::read_dir(&root) else {
        return found;
    };
    for entry in walk.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(inner) = std::fs::read_dir(&path) else {
            continue;
        };
        for child in inner.filter_map(Result::ok) {
            let name = child.file_name();
            if name.to_string_lossy().starts_with("session-") {
                found.push(child.path());
            }
        }
    }
    found
}

pub async fn open_session(session: &PluginSession) {
    tokio::time::timeout(RPC_TIMEOUT, session.open(BindingValues::default()))
        .await
        .unwrap_or_else(|_| fail_deadline(&format!("open timed out after {RPC_TIMEOUT:?}")))
        .expect("open");
}

pub fn step(message: &str) {
    eprintln!("native_gateway: {message}");
    // CI stderr is block-buffered, and `process::exit` does not flush it.
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

struct Proc {
    pid: u32,
    ppid: u32,
    name: String,
}

/// One snapshot of pid, parent, and executable name.
pub struct ProcessTree {
    procs: Vec<Proc>,
}

impl ProcessTree {
    /// Capture the current process table.
    pub fn capture() -> Self {
        Self {
            procs: capture_processes(),
        }
    }

    /// Pids whose parent chain reaches `root`, excluding `root`.
    pub fn descendants(&self, root: u32) -> Vec<u32> {
        use std::collections::{HashMap, HashSet};
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for proc in &self.procs {
            children.entry(proc.ppid).or_default().push(proc.pid);
        }
        let mut out = Vec::new();
        let mut stack = children.get(&root).cloned().unwrap_or_default();
        let mut seen = HashSet::new();
        while let Some(pid) = stack.pop() {
            if pid == root || !seen.insert(pid) {
                continue;
            }
            out.push(pid);
            if let Some(next) = children.get(&pid) {
                stack.extend(next.iter().copied());
            }
        }
        out
    }

    /// The pinned `workerd` binary under `root` (`workerd`, not `bookclerk-workerd`).
    pub fn pinned_workerd(&self, root: u32) -> Option<u32> {
        let descendants = self.descendants(root);
        descendants.into_iter().find(|pid| {
            self.procs
                .iter()
                .any(|proc| proc.pid == *pid && is_pinned_workerd_name(&proc.name))
        })
    }

    /// First descendant of `root` that is still running.
    pub fn live_descendant(&self, root: u32) -> Option<u32> {
        self.descendants(root)
            .into_iter()
            .find(|pid| process_alive(*pid))
    }

    /// Text form of `root` and its descendants, for assertion failures.
    pub fn describe(&self, root: u32) -> String {
        let mut lines = vec![format!("root {root}")];
        for pid in self.descendants(root) {
            let name = self
                .procs
                .iter()
                .find(|proc| proc.pid == pid)
                .map(|proc| proc.name.as_str())
                .unwrap_or("?");
            lines.push(format!("  {pid} {name}"));
        }
        lines.join("\n")
    }
}

fn is_pinned_workerd_name(name: &str) -> bool {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base == "workerd" || base.eq_ignore_ascii_case("workerd.exe")
}

/// `bookclerk-session-*` cgroup leaf containing `pid`, when Linux delegated one.
pub fn linux_session_cgroup(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        for line in text.lines() {
            let Some(path) = line.split(':').next_back() else {
                continue;
            };
            if !path.contains("bookclerk-session-") {
                continue;
            }
            let mut acc = PathBuf::from("/sys/fs/cgroup");
            for part in path.trim_start_matches('/').split('/') {
                if part.is_empty() {
                    continue;
                }
                acc.push(part);
                if part.starts_with("bookclerk-session-") {
                    return Some(acc);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "linux")]
fn capture_processes() -> Vec<Proc> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Some(ppid) = linux_ppid(pid) else {
            continue;
        };
        out.push(Proc {
            pid,
            ppid,
            name: linux_exe_name(pid),
        });
    }
    out
}

#[cfg(target_os = "linux")]
fn linux_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(target_os = "linux")]
fn linux_exe_name(pid: u32) -> String {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return String::new();
    };
    let first = bytes.split(|byte| *byte == 0).next().unwrap_or(&[]);
    let text = String::from_utf8_lossy(first);
    text.rsplit('/').next().unwrap_or("").to_string()
}

#[cfg(target_os = "macos")]
fn capture_processes() -> Vec<Proc> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,ppid=,command="])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let name = parts
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        out.push(Proc { pid, ppid, name });
    }
    out
}

#[cfg(windows)]
fn capture_processes() -> Vec<Proc> {
    let Ok(output) = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process | ForEach-Object { '{0}|{1}|{2}' -f $_.ProcessId,$_.ParentProcessId,$_.Name }",
        ])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.trim().splitn(3, '|');
        let Some(pid) = parts.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let name = parts.next().unwrap_or("").to_string();
        out.push(Proc { pid, ppid, name });
    }
    out
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn capture_processes() -> Vec<Proc> {
    Vec::new()
}
