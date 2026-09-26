//! Test-only native guest for the `native_gateway` smoke (never staged or shipped).
//!
//! Exports one `probe` CLI command. `op=connect` dials through the SDK socket
//! proxy (`bookclerk_plugin_sdk::net::connect`) and echoes `payload`;
//! `op=ambient` tries a direct `std::net` TCP connect, which the nested
//! deny-network jail must refuse. Results are one JSON object on stdout.

#![allow(clippy::missing_docs_in_private_items)]

use std::time::Duration;

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    manifest_capabilities, serve, Bindings, CliArgKind, CliArgSpec, CliCommandSpec,
    CliInvokeParams, CliInvokeResult, CliSchema, ConnectOptions, Entrypoints, Invocation,
    PluginCli, PluginDescribe, PluginError, PluginWorker, ScalarLimits, SocketAddress,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PLUGIN_ID: &str = "native_gateway_probe";
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const AMBIENT_TIMEOUT: Duration = Duration::from_secs(3);

fn arg_spec(name: &str) -> CliArgSpec {
    CliArgSpec {
        name: name.into(),
        long: Some(name.into()),
        short: None,
        kind: CliArgKind::String,
        required: false,
        default: None,
        about: None,
        positional: false,
    }
}

fn cli_schema() -> CliSchema {
    CliSchema {
        commands: vec![CliCommandSpec {
            name: "probe".into(),
            about: Some("Network probe".into()),
            args: ["op", "host", "port", "payload"]
                .into_iter()
                .map(arg_spec)
                .collect(),
        }],
    }
}

/// The installed `plugin.toml` beside this executable: ports are chosen at
/// test time, so describe() cannot embed the manifest at compile time.
fn installed_manifest() -> Result<String, PluginError> {
    let exe = std::env::current_exe()
        .map_err(|err| PluginError::internal(format!("current_exe: {err}")))?;
    let path = exe
        .parent()
        .ok_or_else(|| PluginError::internal("executable has no parent"))?
        .join("plugin.toml");
    std::fs::read_to_string(&path)
        .map_err(|err| PluginError::internal(format!("read {}: {err}", path.display())))
}

struct Root;

#[async_trait(?Send)]
impl PluginWorker for Root {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        if let Ok(ms) = std::env::var("BOOKCLERK_PROBE_DESCRIBE_DELAY_MS") {
            if let Ok(ms) = ms.parse::<u64>() {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            }
        }
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: PLUGIN_ID.into(),
            display_name: Some("Native gateway probe".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(&installed_manifest()?)?,
            cli: cli_schema(),
            ..PluginDescribe::default()
        })
    }

    async fn open(
        &self,
        _invocation: Invocation,
        _bindings: Bindings,
    ) -> Result<Entrypoints, PluginError> {
        Ok(Entrypoints {
            cli: Some(Box::new(Probe)),
            ..Entrypoints::default()
        })
    }
}

struct Probe;

fn arg<'a>(params: &'a CliInvokeParams, name: &str) -> &'a str {
    params
        .args
        .iter()
        .find(|a| a.name == name)
        .map_or("", |a| a.value.as_str())
}

async fn mediated(host: &str, port: u16, payload: &str) -> Result<String, String> {
    let address = SocketAddress {
        hostname: host.into(),
        port,
    };
    let mut socket = tokio::time::timeout(
        IO_TIMEOUT,
        bookclerk_plugin_sdk::net::connect(address, ConnectOptions::default()),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|err| err.to_string())?;
    let stream = socket.stream();
    let round_trip = async {
        stream.write_all(payload.as_bytes()).await?;
        stream.flush().await?;
        let mut echoed = vec![0_u8; payload.len()];
        stream.read_exact(&mut echoed).await?;
        Ok::<_, std::io::Error>(echoed)
    };
    let echoed = tokio::time::timeout(IO_TIMEOUT, round_trip)
        .await
        .map_err(|_| "round trip timed out".to_string())?
        .map_err(|err| format!("round trip: {err}"))?;
    String::from_utf8(echoed).map_err(|err| format!("echo not utf-8: {err}"))
}

async fn hold_until_eof(host: &str, port: u16, payload: &str) -> Result<(), String> {
    let address = SocketAddress {
        hostname: host.into(),
        port,
    };
    let mut socket = tokio::time::timeout(
        IO_TIMEOUT,
        bookclerk_plugin_sdk::net::connect(address, ConnectOptions::default()),
    )
    .await
    .map_err(|_| "connect timed out".to_string())?
    .map_err(|err| err.to_string())?;
    let stream = socket.stream();
    stream
        .write_all(payload.as_bytes())
        .await
        .map_err(|err| format!("write: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| format!("flush: {err}"))?;
    let mut echoed = vec![0_u8; payload.len()];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut echoed))
        .await
        .map_err(|_| "echo timed out".to_string())?
        .map_err(|err| format!("echo: {err}"))?;
    let mut extra = [0_u8; 8];
    let _ = stream.read(&mut extra).await;
    Ok(())
}

/// Report whether `payload` is an open fd in this process.
///
/// `F_GETFD` fails with `EBADF` when the number is not open here. That is the
/// inherited-fd check: another process's descriptor number is not this
/// session's proxy. The call does not close or write the descriptor.
fn touch_fd(payload: &str) -> serde_json::Value {
    #[cfg(unix)]
    {
        let fd: i32 = match payload.parse() {
            Ok(fd) => fd,
            Err(err) => {
                return serde_json::json!({ "ok": false, "error": format!("bad fd: {err}") })
            }
        };
        match fcntl_getfd(fd) {
            Ok(()) => serde_json::json!({ "ok": true, "open": true }),
            Err(err) => serde_json::json!({
                "ok": false,
                "open": false,
                "error": err,
            }),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = payload;
        serde_json::json!({ "ok": false, "error": "fd identity is Unix-only" })
    }
}

/// `fcntl(fd, F_GETFD)`. `EBADF` means the number is not an open descriptor.
#[cfg(unix)]
#[allow(unsafe_code)]
fn fcntl_getfd(fd: i32) -> Result<(), String> {
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    const F_GETFD: i32 = 1;
    let rc = unsafe { fcntl(fd, F_GETFD, 0) };
    if rc >= 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error().to_string())
}

/// One `ping` process, not `cmd /c ping`, so a Job slot is a single process.
fn spawn_ping() -> serde_json::Value {
    #[cfg(windows)]
    {
        match std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                let pid = child.id();
                // Leave the process running so it keeps its Job slot. Dropping
                // the handle does not terminate it.
                drop(child);
                serde_json::json!({ "ok": true, "pid": pid })
            }
            Err(err) => serde_json::json!({
                "ok": false,
                "error": err.to_string(),
                "os": err.raw_os_error(),
            }),
        }
    }
    #[cfg(not(windows))]
    {
        serde_json::json!({ "ok": false, "error": "ping Job slots are Windows-only" })
    }
}

/// Inner or outer Job limits visible to this process.
///
/// `QueryInformationJobObject(NULL)` sees the job associated with the caller.
/// Nested membership can make that query fail; `limit_query_error` carries
/// the Win32 code and the test decides which cap it observed.
fn job_limits() -> serde_json::Value {
    #[cfg(windows)]
    {
        query_windows_job()
    }
    #[cfg(not(windows))]
    {
        serde_json::json!({ "ok": false, "error": "Job limits are Windows-only" })
    }
}

/// Query the caller's Job for an active-process cap and CPU rate.
#[cfg(windows)]
#[allow(unsafe_code)]
fn query_windows_job() -> serde_json::Value {
    #[repr(C)]
    struct BasicLimit {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        _pad_flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        _pad_active: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[repr(C)]
    struct IoCounters {
        read_operation: u64,
        write_operation: u64,
        other_operation: u64,
        read_transfer: u64,
        write_transfer: u64,
        other_transfer: u64,
    }
    #[repr(C)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory: usize,
        job_memory: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }
    #[repr(C)]
    struct CpuRate {
        flags: u32,
        rate: u32,
    }
    extern "system" {
        fn QueryInformationJobObject(
            job: *mut core::ffi::c_void,
            class: i32,
            info: *mut core::ffi::c_void,
            len: u32,
            returned: *mut u32,
        ) -> i32;
        fn IsProcessInJob(
            process: *mut core::ffi::c_void,
            job: *mut core::ffi::c_void,
            result: *mut i32,
        ) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn GetLastError() -> u32;
    }
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION: i32 = 15;
    const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: u32 = 0x8;
    const CPU_RATE_CONTROL_ENABLE: u32 = 0x1;
    const CPU_RATE_CONTROL_HARD_CAP: u32 = 0x4;
    unsafe {
        let mut in_job = 0i32;
        let in_ok = IsProcessInJob(GetCurrentProcess(), core::ptr::null_mut(), &mut in_job);
        let mut extended = std::mem::zeroed::<ExtendedLimit>();
        let limit_ok = QueryInformationJobObject(
            core::ptr::null_mut(),
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            &mut extended as *mut ExtendedLimit as *mut _,
            std::mem::size_of::<ExtendedLimit>() as u32,
            core::ptr::null_mut(),
        );
        let limit_error = if limit_ok == 0 { GetLastError() } else { 0 };
        let mut cpu = std::mem::zeroed::<CpuRate>();
        let cpu_ok = QueryInformationJobObject(
            core::ptr::null_mut(),
            JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION,
            &mut cpu as *mut CpuRate as *mut _,
            std::mem::size_of::<CpuRate>() as u32,
            core::ptr::null_mut(),
        );
        let cpu_error = if cpu_ok == 0 { GetLastError() } else { 0 };
        let cpu_enabled = cpu_ok != 0 && (cpu.flags & CPU_RATE_CONTROL_ENABLE) != 0;
        let active_limit = if limit_ok != 0
            && (extended.basic.limit_flags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS) != 0
        {
            Some(extended.basic.active_process_limit)
        } else {
            None
        };
        serde_json::json!({
            "ok": true,
            "in_job": in_ok != 0 && in_job != 0,
            "active_limit": active_limit,
            "limit_query_error": limit_error,
            "cpu_rate": if cpu_enabled { Some(cpu.rate) } else { None::<u32> },
            "cpu_hard_cap": cpu_enabled && (cpu.flags & CPU_RATE_CONTROL_HARD_CAP) != 0,
            "cpu_query_error": cpu_error,
        })
    }
}

/// OAuth callback tunnel plus the PostgreSQL mediator socket in the guest IPC directory.
///
/// `payload` is the host callback socket (`cb.sock` or a Windows pipe). The
/// guest binds `{GUEST_IPC_DIR}/.s.PGSQL.5432`, accepts one client, and accepts
/// one tunneled browser stream. This is the callback and mediator sockets, not
/// a pathname echo.
async fn serve_ipc(callback: &str) -> serde_json::Value {
    #[cfg(unix)]
    {
        serve_ipc_unix(callback).await
    }
    #[cfg(not(unix))]
    {
        let _ = callback;
        serde_json::json!({ "ok": false, "error": "guest IPC sockets are Unix-only" })
    }
}

/// Bind `.s.PGSQL.5432` and accept the host callback tunnel.
#[cfg(unix)]
async fn serve_ipc_unix(callback: &str) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(dir) = std::env::var_os(bookclerk_plugin_sdk::GUEST_IPC_DIR_ENV) else {
        return serde_json::json!({
            "ok": false,
            "error": format!("{} is unset", bookclerk_plugin_sdk::GUEST_IPC_DIR_ENV),
        });
    };
    let pg_path = std::path::PathBuf::from(dir).join(".s.PGSQL.5432");
    let _ = std::fs::remove_file(&pg_path);
    let listener = match tokio::net::UnixListener::bind(&pg_path) {
        Ok(listener) => listener,
        Err(err) => {
            return serde_json::json!({
                "ok": false,
                "error": format!("postgres mediator bind {}: {err}", pg_path.display()),
            })
        }
    };
    let postgres = tokio::spawn(async move {
        let (mut sock, _) = listener
            .accept()
            .await
            .map_err(|err| format!("postgres mediator accept: {err}"))?;
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(std::time::Duration::from_secs(20), sock.read(&mut buf))
            .await
            .map_err(|_| "postgres mediator read timed out".to_string())?
            .map_err(|err| format!("postgres mediator read: {err}"))?;
        sock.write_all(b"PGOK")
            .await
            .map_err(|err| format!("postgres mediator write: {err}"))?;
        Ok::<Vec<u8>, String>(buf[..n].to_vec())
    });
    let stream = match tokio::net::UnixStream::connect(callback).await {
        Ok(stream) => stream,
        Err(err) => {
            postgres.abort();
            return serde_json::json!({
                "ok": false,
                "error": format!("callback IPC connect {callback}: {err}"),
            });
        }
    };
    let (reader, writer) = tokio::io::split(stream);
    let mut tunnel = bookclerk_plugin_sdk::TunnelGuest::new(reader, writer);
    let mut browser = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tunnel.accept(),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            postgres.abort();
            return serde_json::json!({ "ok": false, "error": format!("callback accept: {err}") });
        }
        Err(_) => {
            postgres.abort();
            return serde_json::json!({ "ok": false, "error": "callback accept timed out" });
        }
    };
    let mut oauth = [0u8; 64];
    let n = match tokio::time::timeout(std::time::Duration::from_secs(20), browser.read(&mut oauth))
        .await
    {
        Ok(Ok(n)) => n,
        Ok(Err(err)) => {
            postgres.abort();
            return serde_json::json!({ "ok": false, "error": format!("callback read: {err}") });
        }
        Err(_) => {
            postgres.abort();
            return serde_json::json!({ "ok": false, "error": "callback read timed out" });
        }
    };
    if let Err(err) = browser.write_all(&oauth[..n]).await {
        postgres.abort();
        return serde_json::json!({ "ok": false, "error": format!("callback echo: {err}") });
    }
    let pg = match postgres.await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => return serde_json::json!({ "ok": false, "error": err }),
        Err(err) => {
            return serde_json::json!({ "ok": false, "error": format!("postgres task: {err}") })
        }
    };
    serde_json::json!({
        "ok": true,
        "oauth": String::from_utf8_lossy(&oauth[..n]),
        "postgres": String::from_utf8_lossy(&pg),
    })
}

/// Spawn parked threads until `limit` or the kernel refuses another task.
///
/// `pids.max` counts threads. A refusal is the cgroup cap, reported as an
/// error string; the caller decides whether that is enforcement.
fn exhaust_threads(limit: &str) -> serde_json::Value {
    let limit: u32 = limit.parse().unwrap_or(0);
    let mut created = 0_u32;
    let mut error = None;
    let mut handles = Vec::new();
    for _ in 0..limit {
        match std::thread::Builder::new()
            .name("bookclerk-probe-park".into())
            .spawn(|| {
                std::thread::park();
            }) {
            Ok(handle) => {
                created += 1;
                handles.push(handle);
            }
            Err(err) => {
                error = Some(err.to_string());
                break;
            }
        }
    }
    // Detach the parked threads. They keep their `pids.max` slots until the
    // session is torn down.
    drop(handles);
    serde_json::json!({
        "ok": error.is_some(),
        "created": created,
        "error": error,
    })
}

/// Fork a sleeper that stays in this process group. `exec` is denied, so the
/// child only calls `pause`. Windows observes the jail's child instead.
fn spawn_pause_descendant() -> serde_json::Value {
    #[cfg(unix)]
    {
        let pid = spawn_pause_child();
        if pid > 0 {
            serde_json::json!({ "ok": true, "pid": pid })
        } else {
            serde_json::json!({ "ok": false, "error": format!("fork returned {pid}") })
        }
    }
    #[cfg(not(unix))]
    {
        serde_json::json!({
            "ok": false,
            "error": "the Windows jail process keeps the probe as its child",
        })
    }
}

#[cfg(unix)]
#[allow(unsafe_code)] // `fork` + `pause`; the child never returns into the runtime.
fn spawn_pause_child() -> i32 {
    extern "C" {
        fn fork() -> i32;
        fn pause() -> i32;
    }
    unsafe {
        let pid = fork();
        if pid == 0 {
            loop {
                pause();
            }
        }
        pid
    }
}

fn ambient(host: &str, port: u16) -> Result<(), String> {
    let addr: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|err| format!("bad address: {err}"))?;
    std::net::TcpStream::connect_timeout(&addr, AMBIENT_TIMEOUT)
        .map(drop)
        .map_err(|err| format!("{:?}: {err}", err.kind()))
}

#[async_trait(?Send)]
impl PluginCli for Probe {
    async fn describe(&self) -> Result<CliSchema, PluginError> {
        Ok(cli_schema())
    }

    async fn invoke(&self, params: CliInvokeParams) -> Result<CliInvokeResult, PluginError> {
        if params.command != "probe" {
            return Ok(CliInvokeResult {
                exit_code: 2,
                stderr: format!("unknown command {}", params.command),
                ..CliInvokeResult::default()
            });
        }
        let host = arg(&params, "host");
        let port: u16 = arg(&params, "port").parse().unwrap_or(0);
        let outcome = match arg(&params, "op") {
            "connect" => match mediated(host, port, arg(&params, "payload")).await {
                Ok(echo) => serde_json::json!({ "ok": true, "echo": echo }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
            "ambient" => match ambient(host, port) {
                Ok(()) => serde_json::json!({ "ok": true }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
            "env_keys" => {
                let mut keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
                keys.sort();
                let proxy = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV).ok();
                serde_json::json!({ "ok": true, "keys": keys, "socket_proxy": proxy })
            }
            "hold" => match hold_until_eof(host, port, arg(&params, "payload")).await {
                Ok(()) => serde_json::json!({ "ok": true, "eof": true }),
                Err(error) => serde_json::json!({ "ok": false, "error": error }),
            },
            "block" => {
                let ms = arg(&params, "payload").parse::<u64>().unwrap_or(30_000);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                serde_json::json!({ "ok": true })
            }
            "descendant" => spawn_pause_descendant(),
            "exhaust_threads" => exhaust_threads(arg(&params, "payload")),
            "touch_fd" => touch_fd(arg(&params, "payload")),
            "spawn_ping" => spawn_ping(),
            "job_limits" => job_limits(),
            "serve_ipc" => serve_ipc(arg(&params, "payload")).await,
            "read_path" => {
                let path = arg(&params, "payload");
                match std::fs::read(path) {
                    Ok(bytes) => serde_json::json!({
                        "ok": true,
                        "len": bytes.len(),
                    }),
                    Err(err) => serde_json::json!({
                        "ok": false,
                        "error": format!("{:?}: {err}", err.kind()),
                    }),
                }
            }
            other => {
                return Err(PluginError::invalid_params(format!("unknown op `{other}`")));
            }
        };
        Ok(CliInvokeResult {
            exit_code: 0,
            stdout: outcome.to_string(),
            ..CliInvokeResult::default()
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("BOOKCLERK_PROBE_EXIT").is_some() {
        return Ok(());
    }
    serve(Root).await?;
    Ok(())
}
