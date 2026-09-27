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

/// Hex session challenge from the environment. Does not touch the proxy.
fn session_challenge() -> serde_json::Value {
    match std::env::var(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV) {
        Ok(hex) => serde_json::json!({
            "ok": true,
            "present": true,
            "hex": hex.trim(),
        }),
        Err(err) => serde_json::json!({
            "ok": false,
            "present": false,
            "error": err.to_string(),
        }),
    }
}

/// Spawn a child with the session challenge removed and let it try every
/// endpoint it was told about.
///
/// `payload` is `inherit`: the child receives the still-waiting proxy and
/// writes 32 zero bytes, the same shape as the socket-proxy unit test. The
/// live proxy must not treat that as a completed challenge.
fn unrelated_challenge(payload: &str) -> serde_json::Value {
    let spec = match std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV) {
        Ok(spec) => spec,
        Err(err) => {
            return challenge_failure(&format!("proxy endpoint is unset: {err}"));
        }
    };
    let write_spec = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV).ok();
    if payload != "inherit" {
        if let Err(err) = disarm_inherit(&spec, write_spec.as_deref()) {
            return challenge_failure(&format!("could not stop endpoint inheritance: {err}"));
        }
    }
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => return challenge_failure(&err.to_string()),
    };
    let mut cmd = std::process::Command::new(win32_child_image(&exe));
    cmd.arg("--endpoint-challenge")
        .arg(&spec)
        .env_remove(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    if let Some(write_spec) = &write_spec {
        cmd.arg(write_spec);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => return challenge_failure(&format!("unrelated child spawn: {err}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return challenge_failure(&format!(
            "unrelated child status {}: {stderr}",
            output.status
        ));
    }
    match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        Ok(value) => value,
        Err(err) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            challenge_failure(&format!(
                "unrelated child stdout is not JSON ({err}): {} {stderr}",
                String::from_utf8_lossy(&output.stdout)
            ))
        }
    }
}

/// Spawn a child that does not inherit this session's proxy and have it
/// write a CONNECT toward `host:port` on the advertised endpoint.
///
/// The child never receives `inherit`. Disarming the endpoint first is what
/// keeps the already-authenticated proxy out of the child.
fn unrelated_drive(host: &str, port: u16) -> serde_json::Value {
    let spec = match std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV) {
        Ok(spec) => spec,
        Err(err) => return challenge_failure(&format!("proxy endpoint is unset: {err}")),
    };
    let write_spec = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV).ok();
    if let Err(err) = disarm_inherit(&spec, write_spec.as_deref()) {
        return challenge_failure(&format!("could not stop endpoint inheritance: {err}"));
    }
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => return challenge_failure(&err.to_string()),
    };
    let mut cmd = std::process::Command::new(win32_child_image(&exe));
    cmd.arg("--endpoint-connect")
        .arg(&spec)
        .arg(write_spec.as_deref().unwrap_or(""))
        .arg(host)
        .arg(port.to_string())
        .env_remove(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => return challenge_failure(&format!("unrelated child spawn: {err}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return challenge_failure(&format!(
            "unrelated child status {}: {stderr}",
            output.status
        ));
    }
    match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        Ok(value) => value,
        Err(err) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            challenge_failure(&format!(
                "unrelated child stdout is not JSON ({err}): {} {stderr}",
                String::from_utf8_lossy(&output.stdout)
            ))
        }
    }
}

/// Failure JSON for a child that did not complete a challenge.
fn challenge_failure(error: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "completed": false,
        "unsupported": false,
        "challenge_env": false,
        "wrote": false,
        "closed": false,
        "opened_stream": false,
        "error": error,
        "attempts": [],
    })
}

/// Image path for a child of this probe. Windows AppContainer rejects `\\?\`.
fn win32_child_image(path: &std::path::Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        win32_spawn_path(path)
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// Clear inheritability so a non-`inherit` child cannot see the live proxy.
fn disarm_inherit(spec: &str, write_spec: Option<&str>) -> Result<(), String> {
    disarm_one(spec)?;
    if let Some(write_spec) = write_spec {
        disarm_one(write_spec)?;
    }
    Ok(())
}

/// Mark one `fd:` / `handle:` endpoint non-inheritable.
fn disarm_one(spec: &str) -> Result<(), String> {
    if let Some(rest) = spec.strip_prefix("fd:") {
        #[cfg(unix)]
        {
            let fd: i32 = rest
                .parse()
                .map_err(|err| format!("bad fd {spec}: {err}"))?;
            return set_cloexec(fd);
        }
        #[cfg(not(unix))]
        {
            let _ = rest;
            return Err("fd: endpoints are Unix-only".into());
        }
    }
    if let Some(rest) = spec.strip_prefix("handle:") {
        #[cfg(windows)]
        {
            let value: u64 = rest
                .parse()
                .map_err(|err| format!("bad handle {spec}: {err}"))?;
            return clear_handle_inherit(value);
        }
        #[cfg(not(windows))]
        {
            let _ = rest;
            return Err("handle: endpoints are Windows-only".into());
        }
    }
    Err(format!(
        "endpoint spec must be fd:<n> or handle:<n>, got {spec}"
    ))
}

/// Write `hex` (another session's challenge) on this process's proxy link.
///
/// Used before this guest has started its mux, so the bytes are the proxy's
/// session challenge. A mismatch closes the link and does not open a stream.
async fn present_foreign_challenge(hex: &str) -> serde_json::Value {
    let bytes = match decode_challenge(hex) {
        Ok(bytes) => bytes,
        Err(err) => return observe_error(&err),
    };
    let spec = match std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV) {
        Ok(spec) => spec,
        Err(err) => return observe_error(&format!("proxy endpoint is unset: {err}")),
    };
    let write_spec = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV).ok();
    let target = write_spec.as_deref().unwrap_or(spec.as_str());
    observe_endpoint(target, &bytes)
}

/// Decode [`bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN`] bytes of hex.
fn decode_challenge(
    hex: &str,
) -> Result<[u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN], String> {
    let hex = hex.trim();
    let len = bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN;
    if hex.len() != len * 2 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("challenge must be {len} bytes of hex"));
    }
    let mut out = [0u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|err| format!("challenge hex: {err}"))?;
    }
    Ok(out)
}

/// Write `bytes` to `spec` and report whether a stream or HTTP 200 appeared.
fn observe_endpoint(spec: &str, bytes: &[u8]) -> serde_json::Value {
    if let Some(rest) = spec.strip_prefix("fd:") {
        #[cfg(unix)]
        {
            let fd: i32 = match rest.parse() {
                Ok(fd) => fd,
                Err(err) => return observe_error(&format!("bad fd {spec}: {err}")),
            };
            return observe_fd(fd, bytes);
        }
        #[cfg(not(unix))]
        {
            let _ = (rest, bytes);
            return observe_error("fd: endpoints are Unix-only");
        }
    }
    if let Some(rest) = spec.strip_prefix("handle:") {
        #[cfg(windows)]
        {
            let value: u64 = match rest.parse() {
                Ok(value) => value,
                Err(err) => return observe_error(&format!("bad handle {spec}: {err}")),
            };
            return observe_handle(value, bytes);
        }
        #[cfg(not(windows))]
        {
            let _ = (rest, bytes);
            return observe_error("handle: endpoints are Windows-only");
        }
    }
    observe_error(&format!("unsupported endpoint spec {spec}"))
}

/// JSON for an attempt that did not run.
fn observe_error(error: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "completed": false,
        "unsupported": false,
        "opened_stream": false,
        "closed": false,
        "refused": false,
        "wrote": false,
        "error": error,
    })
}

/// Classify a finished attempt. `unsupported` stays false: a platform that
/// cannot run the probe uses a different op result.
fn observe_result(
    wrote: bool,
    closed: bool,
    opened_stream: bool,
    error: Option<String>,
) -> serde_json::Value {
    let refused = !opened_stream && (closed || error.is_some());
    serde_json::json!({
        "ok": opened_stream,
        "completed": opened_stream,
        "unsupported": false,
        "opened_stream": opened_stream,
        "closed": closed,
        "refused": refused,
        "wrote": wrote,
        "error": error,
    })
}

/// `HTTP/1.x 200` in `bytes` is a completed CONNECT, not a mux close.
fn saw_http_200(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.contains("HTTP/1.1 200") || text.contains("HTTP/1.0 200")
}

/// Child entry: no session-challenge env, try the advertised endpoints.
fn child_endpoint_challenge(spec: &str, write_spec: Option<&str>) -> serde_json::Value {
    let challenge_env = std::env::var_os(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV).is_some();
    let zeros = [0u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    let mut attempts = Vec::new();
    attempts.push(labeled_attempt(spec, &zeros));
    if let Some(write_spec) = write_spec {
        if write_spec != spec {
            attempts.push(labeled_attempt(write_spec, &zeros));
        }
    }
    #[cfg(unix)]
    attempts.extend(extra_socket_attempts(&zeros, spec));
    let completed = attempts.iter().any(|attempt| attempt["completed"] == true);
    let wrote = attempts.iter().any(|attempt| attempt["wrote"] == true);
    let closed = attempts.iter().any(|attempt| attempt["closed"] == true);
    let opened_stream = attempts
        .iter()
        .any(|attempt| attempt["opened_stream"] == true);
    serde_json::json!({
        "ok": completed,
        "completed": completed,
        "unsupported": false,
        "challenge_env": challenge_env,
        "wrote": wrote,
        "closed": closed,
        "opened_stream": opened_stream,
        "attempts": attempts,
    })
}

/// One endpoint attempt, tagged with its spec.
fn labeled_attempt(spec: &str, bytes: &[u8]) -> serde_json::Value {
    let mut outcome = observe_endpoint(spec, bytes);
    outcome["endpoint"] = serde_json::Value::String(spec.to_string());
    outcome
}

/// `CONNECT` request bytes. This is a stream open, not a session challenge.
fn connect_request(host: &str, port: u16) -> Vec<u8> {
    format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n").into_bytes()
}

/// Write a CONNECT to every spec in `payload` (one `fd:` / `handle:` per line).
///
/// `payload` is another session's already-authenticated proxy, not this
/// process's challenge. `unsupported` stays false: a bad fd or handle is an
/// invalid endpoint.
fn drive_foreign_endpoint(payload: &str, host: &str, port: u16) -> serde_json::Value {
    let bytes = connect_request(host, port);
    let mut attempts = Vec::new();
    for spec in payload.split(['\n', ',']) {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        attempts.push(drive_one(spec, &bytes));
    }
    if attempts.is_empty() {
        let mut err = observe_error("no endpoint spec");
        err["invalid_endpoint"] = serde_json::Value::Bool(true);
        return err;
    }
    summarize_drives(&attempts)
}

/// Child entry: CONNECT toward `host:port` on endpoints this process was told
/// about. The parent has already cleared inherit, and this child has no
/// session challenge.
fn child_endpoint_connect(
    spec: &str,
    write_spec: Option<&str>,
    host: &str,
    port: u16,
) -> serde_json::Value {
    let bytes = connect_request(host, port);
    let mut attempts = vec![drive_one(spec, &bytes)];
    if let Some(write_spec) = write_spec {
        if !write_spec.is_empty() && write_spec != spec {
            attempts.push(drive_one(write_spec, &bytes));
        }
    }
    summarize_drives(&attempts)
}

/// One CONNECT attempt. Windows does not call `GetHandleInformation` on the
/// foreign value: that API kills an AppContainer guest when the value is not
/// a handle. `DuplicateHandle` failure is an invalid endpoint.
fn drive_one(spec: &str, bytes: &[u8]) -> serde_json::Value {
    if let Some(rest) = spec.strip_prefix("fd:") {
        #[cfg(unix)]
        {
            let mut outcome = observe_endpoint(spec, bytes);
            let _ = rest;
            tag_invalid_endpoint(&mut outcome);
            outcome["endpoint"] = serde_json::Value::String(spec.to_string());
            return outcome;
        }
        #[cfg(not(unix))]
        {
            let _ = (rest, bytes);
            let mut err = observe_error("fd: endpoints are Unix-only");
            err["invalid_endpoint"] = serde_json::Value::Bool(true);
            err["endpoint"] = serde_json::Value::String(spec.to_string());
            return err;
        }
    }
    if let Some(rest) = spec.strip_prefix("handle:") {
        #[cfg(windows)]
        {
            let value: u64 = match rest.parse() {
                Ok(value) => value,
                Err(err) => {
                    let mut outcome = observe_error(&format!("bad handle {spec}: {err}"));
                    outcome["invalid_endpoint"] = serde_json::Value::Bool(true);
                    outcome["endpoint"] = serde_json::Value::String(spec.to_string());
                    return outcome;
                }
            };
            let mut outcome = drive_handle_connect(value, bytes);
            outcome["endpoint"] = serde_json::Value::String(spec.to_string());
            return outcome;
        }
        #[cfg(not(windows))]
        {
            let _ = (rest, bytes);
            let mut err = observe_error("handle: endpoints are Windows-only");
            err["invalid_endpoint"] = serde_json::Value::Bool(true);
            err["endpoint"] = serde_json::Value::String(spec.to_string());
            return err;
        }
    }
    let mut err = observe_error(&format!("unsupported endpoint spec {spec}"));
    err["invalid_endpoint"] = serde_json::Value::Bool(true);
    err["endpoint"] = serde_json::Value::String(spec.to_string());
    err
}

/// A failed attempt that never wrote and never opened a stream is an invalid
/// endpoint. `unsupported` is left as the attempt recorded it.
fn tag_invalid_endpoint(outcome: &mut serde_json::Value) {
    let wrote = outcome["wrote"] == true;
    let opened = outcome["opened_stream"] == true;
    outcome["invalid_endpoint"] = serde_json::Value::Bool(!wrote && !opened);
}

/// Combine CONNECT attempts. `unsupported` is true only when every attempt
/// says so, which these attempts do not.
fn summarize_drives(attempts: &[serde_json::Value]) -> serde_json::Value {
    let opened_stream = attempts
        .iter()
        .any(|attempt| attempt["opened_stream"] == true);
    let wrote = attempts.iter().any(|attempt| attempt["wrote"] == true);
    let closed = attempts.iter().any(|attempt| attempt["closed"] == true);
    let refused = attempts.iter().any(|attempt| attempt["refused"] == true);
    let invalid_endpoint = attempts
        .iter()
        .any(|attempt| attempt["invalid_endpoint"] == true);
    let unsupported = !attempts.is_empty()
        && attempts
            .iter()
            .all(|attempt| attempt["unsupported"] == true);
    serde_json::json!({
        "ok": opened_stream,
        "completed": opened_stream,
        "unsupported": unsupported,
        "opened_stream": opened_stream,
        "closed": closed,
        "refused": refused,
        "wrote": wrote,
        "invalid_endpoint": invalid_endpoint,
        "attempts": attempts,
    })
}

/// Probe a live handle the host did not place in this process.
///
/// `GetHandleInformation` on the inherited proxy must succeed first. That
/// separates "API missing" (`unsupported`) from "not a handle here"
/// (`denied`). The unlisted value is checked with `DuplicateHandle`, which
/// returns access denied or invalid handle without terminating the process.
/// The proxy handle is queried again after the foreign value.
fn unlisted_handle(payload: &str) -> serde_json::Value {
    #[cfg(windows)]
    {
        unlisted_handle_windows(payload)
    }
    #[cfg(not(windows))]
    {
        let _ = payload;
        serde_json::json!({
            "ok": false,
            "unsupported": true,
            "denied": false,
            "proxy_usable": false,
            "proxy_usable_after": false,
            "sentinels": [],
            "error": "unlisted-handle sentinels are Windows-only",
        })
    }
}

/// `FD_CLOEXEC` so a later `exec` does not receive `fd`.
#[cfg(unix)]
#[allow(unsafe_code)]
fn set_cloexec(fd: i32) -> Result<(), String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// Write `bytes` to `fd` and wait briefly for a close or an HTTP 200.
#[cfg(unix)]
fn observe_fd(fd: i32, bytes: &[u8]) -> serde_json::Value {
    if let Err(err) = write_all_fd(fd, bytes) {
        return observe_result(false, is_closed_io(&err), false, Some(err));
    }
    match read_fd_timeout(fd, 1000) {
        Ok(buf) if buf.is_empty() => observe_result(true, true, false, Some("link closed".into())),
        Ok(buf) if saw_http_200(&buf) => observe_result(true, false, true, None),
        Ok(buf) => observe_result(
            true,
            false,
            false,
            Some(format!("no stream ({} bytes)", buf.len())),
        ),
        Err(err) => observe_result(true, is_closed_io(&err), false, Some(err)),
    }
}

/// Other open sockets besides `primary`. Stdio stays untouched.
#[cfg(unix)]
fn extra_socket_attempts(bytes: &[u8], primary: &str) -> Vec<serde_json::Value> {
    let primary_fd = primary
        .strip_prefix("fd:")
        .and_then(|rest| rest.parse::<i32>().ok());
    let mut out = Vec::new();
    for fd in 3..64 {
        if Some(fd) == primary_fd || !fd_is_socket(fd) {
            continue;
        }
        out.push(labeled_attempt(&format!("fd:{fd}"), bytes));
        if out.len() == 4 {
            break;
        }
    }
    out
}

/// `fstat` says `fd` is a socket.
#[cfg(unix)]
#[allow(unsafe_code)]
fn fd_is_socket(fd: i32) -> bool {
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } < 0 {
        return false;
    }
    (stat.st_mode & libc::S_IFMT) == libc::S_IFSOCK
}

/// Write every byte. `EINTR` retries. Any other error is the caller's result.
#[cfg(unix)]
#[allow(unsafe_code)]
fn write_all_fd(fd: i32, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.to_string());
        }
        if n == 0 {
            return Err("write returned 0".into());
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

/// Poll `fd` then read whatever is pending. Timeout is "not accepted".
#[cfg(unix)]
#[allow(unsafe_code)]
fn read_fd_timeout(fd: i32, timeout_ms: i32) -> Result<Vec<u8>, String> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            return read_fd_timeout(fd, timeout_ms);
        }
        return Err(err.to_string());
    }
    if rc == 0 {
        return Err("challenge was not accepted".into());
    }
    if pfd.revents & libc::POLLNVAL != 0 {
        return Err("endpoint is not a pollable descriptor".into());
    }
    let mut buf = [0u8; 256];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err("challenge was not accepted".into());
        }
        return Err(err.to_string());
    }
    Ok(buf[..n as usize].to_vec())
}

/// Broken pipe / reset / peer-closed, on Unix and Windows error text.
fn is_closed_io(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("connection abort")
        || lower.contains("not connected")
        || lower.contains("pipe has been ended")
        || lower.contains("os error 32")
        || lower.contains("os error 104")
        || lower.contains("os error 107")
        || lower.contains("os error 109")
        || lower.contains("os error 232")
}

/// Drop `HANDLE_FLAG_INHERIT` so `CreateProcess` will not pass `value` on.
#[cfg(windows)]
#[allow(unsafe_code)]
fn clear_handle_inherit(value: u64) -> Result<(), String> {
    let handle = handle_ptr(value)?;
    let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
    if ok == 0 {
        return Err(format!(
            "SetHandleInformation: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Write `bytes` to an overlapped pipe handle and wait for a close or a 200.
#[cfg(windows)]
fn observe_handle(value: u64, bytes: &[u8]) -> serde_json::Value {
    let handle = match handle_ptr(value) {
        Ok(handle) => handle,
        Err(err) => return observe_error(&err),
    };
    let info = handle_information(handle);
    if !info.ok {
        return observe_result(false, false, false, Some(info.error));
    }
    if let Err(err) = overlapped_write(handle, bytes) {
        return observe_result(false, is_closed_io(&err), false, Some(err));
    }
    match overlapped_read(handle, 1000) {
        Ok(buf) if buf.is_empty() => observe_result(true, true, false, Some("link closed".into())),
        Ok(buf) if saw_http_200(&buf) => observe_result(true, false, true, None),
        Ok(buf) => observe_result(
            true,
            false,
            false,
            Some(format!("no stream ({} bytes)", buf.len())),
        ),
        Err(err) => observe_result(true, is_closed_io(&err), false, Some(err)),
    }
}

/// `DuplicateHandle` on every sentinel the host created before spawn.
///
/// The inherited proxy is queried with `GetHandleInformation` first and
/// again at the end (it is a real handle). Each sentinel is `DuplicateHandle`
/// only. A successful duplicate is `SetEvent` on that copy so the host can
/// see whether the omitted object was inherited. Numeric collisions are
/// reported, not skipped.
#[cfg(windows)]
fn unlisted_handle_windows(payload: &str) -> serde_json::Value {
    let proxy_spec = match std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV) {
        Ok(spec) => spec,
        Err(err) => {
            return serde_json::json!({
                "ok": false,
                "unsupported": false,
                "denied": false,
                "proxy_usable": false,
                "proxy_usable_after": false,
                "sentinels": [],
                "error": format!("proxy endpoint is unset: {err}"),
            })
        }
    };
    let proxy_value = match proxy_spec
        .strip_prefix("handle:")
        .and_then(|rest| rest.parse::<u64>().ok())
    {
        Some(value) => value,
        None => {
            return serde_json::json!({
                "ok": false,
                "unsupported": false,
                "denied": false,
                "proxy_usable": false,
                "proxy_usable_after": false,
                "sentinels": [],
                "error": format!("proxy endpoint is not handle:<n>: {proxy_spec}"),
            })
        }
    };
    let proxy = match handle_ptr(proxy_value) {
        Ok(handle) => handle,
        Err(err) => {
            return serde_json::json!({
                "ok": false,
                "unsupported": false,
                "denied": false,
                "proxy_usable": false,
                "proxy_usable_after": false,
                "sentinels": [],
                "error": err,
            })
        }
    };
    let first = handle_information(proxy);
    if !first.ok {
        return serde_json::json!({
            "ok": false,
            "unsupported": false,
            "denied": false,
            "proxy_usable": false,
            "proxy_usable_after": false,
            "os": first.os,
            "sentinels": [],
            "error": first.error,
        });
    }
    let mut sentinels = Vec::new();
    for part in payload.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Ok(value) = part.parse::<u64>() else {
            sentinels.push(serde_json::json!({
                "value": part,
                "duplicated": false,
                "denied": false,
                "os": 0,
                "error": "sentinel is not a handle value",
            }));
            continue;
        };
        let Ok(handle) = handle_ptr(value) else {
            sentinels.push(serde_json::json!({
                "value": value,
                "duplicated": false,
                "denied": false,
                "os": 0,
                "error": "sentinel does not fit a handle",
            }));
            continue;
        };
        sentinels.push(probe_sentinel(value, handle));
    }
    let again = handle_information(proxy);
    let denied = !sentinels.is_empty() && sentinels.iter().all(|row| row["denied"] == true);
    let os = sentinels
        .iter()
        .find_map(|row| row["os"].as_u64())
        .unwrap_or(0);
    serde_json::json!({
        "ok": false,
        "unsupported": false,
        "denied": denied,
        "proxy_usable": true,
        "proxy_usable_after": again.ok,
        "os": os,
        "sentinels": sentinels,
        "error": if denied { "" } else { "a sentinel was not denied" },
    })
}

/// `DuplicateHandle` one omitted sentinel. Never `GetHandleInformation`.
#[cfg(windows)]
#[allow(unsafe_code)]
fn probe_sentinel(value: u64, handle: *mut core::ffi::c_void) -> serde_json::Value {
    match duplicate_raw(handle) {
        Ok(copy) => {
            let _close = CloseEvent(copy);
            let signaled = unsafe { SetEvent(copy) } != 0;
            serde_json::json!({
                "value": value,
                "duplicated": true,
                "denied": false,
                "signaled_copy": signaled,
                "os": 0,
                "error": "",
            })
        }
        Err(info) => {
            let denied = info.os == 5 || info.os == 6;
            serde_json::json!({
                "value": value,
                "duplicated": false,
                "denied": denied,
                "signaled_copy": false,
                "os": info.os,
                "error": info.error,
            })
        }
    }
}

/// CONNECT through a handle without `GetHandleInformation`.
///
/// Failure to duplicate is an invalid endpoint (the value is not open here).
/// A duplicate that is this process's stdio, this session's own proxy, or
/// not a pipe is also an invalid endpoint and is not written. Overlapped
/// I/O on those objects kills the guest: the number collided with a local
/// handle, and the bytes would land on the authenticated mux or the RPC
/// pipes. A duplicate that is some other pipe is a candidate for the other
/// session's proxy, so the CONNECT is written on that copy.
#[cfg(windows)]
fn drive_handle_connect(value: u64, bytes: &[u8]) -> serde_json::Value {
    let handle = match handle_ptr(value) {
        Ok(handle) => handle,
        Err(err) => {
            let mut outcome = observe_error(&err);
            outcome["invalid_endpoint"] = serde_json::Value::Bool(true);
            return outcome;
        }
    };
    let copy = match duplicate_raw(handle) {
        Ok(copy) => copy,
        Err(info) => return invalid_foreign_handle(&info.error, info.os),
    };
    let _close = CloseEvent(copy);
    if is_stdio_value(value) {
        return invalid_foreign_handle("handle value is a standard handle", 0);
    }
    if session_owns_handle(value) {
        return invalid_foreign_handle("handle value collides with this session proxy", 0);
    }
    if !is_pipe_handle(copy) {
        return invalid_foreign_handle("duplicated handle is not a pipe", 0);
    }
    let mut outcome = if let Err(err) = overlapped_write(copy, bytes) {
        observe_result(false, is_closed_io(&err), false, Some(err))
    } else {
        match overlapped_read(copy, 1000) {
            Ok(buf) if buf.is_empty() => {
                observe_result(true, true, false, Some("link closed".into()))
            }
            Ok(buf) if saw_http_200(&buf) => observe_result(true, false, true, None),
            Ok(buf) => observe_result(
                true,
                false,
                false,
                Some(format!("no stream ({} bytes)", buf.len())),
            ),
            Err(err) => observe_result(true, is_closed_io(&err), false, Some(err)),
        }
    };
    tag_invalid_endpoint(&mut outcome);
    outcome
}

/// JSON for a foreign handle that did not open a stream.
#[cfg(windows)]
fn invalid_foreign_handle(error: &str, os: u32) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "completed": false,
        "unsupported": false,
        "opened_stream": false,
        "closed": false,
        "refused": false,
        "wrote": false,
        "invalid_endpoint": true,
        "os": os,
        "error": error,
    })
}

/// `handle:<n>` from an endpoint spec.
#[cfg(windows)]
fn spec_handle_value(spec: &str) -> Option<u64> {
    spec.strip_prefix("handle:")
        .and_then(|rest| rest.trim().parse().ok())
}

/// This authenticated session's proxy already uses `value`.
///
/// The unrelated child has no session challenge, so a duplicate there is
/// still written: that child is checking whether the proxy was inherited.
#[cfg(windows)]
fn session_owns_handle(value: u64) -> bool {
    if std::env::var_os(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV).is_none() {
        return false;
    }
    let proxy = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_ENV)
        .ok()
        .and_then(|spec| spec_handle_value(&spec));
    let write = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV)
        .ok()
        .and_then(|spec| spec_handle_value(&spec));
    proxy == Some(value) || write == Some(value)
}

/// `value` is stdin, stdout, or stderr in this process.
#[cfg(windows)]
#[allow(unsafe_code)]
fn is_stdio_value(value: u64) -> bool {
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(kind) };
        if handle.is_null() || handle as isize == -1 {
            continue;
        }
        if handle as usize as u64 == value {
            return true;
        }
    }
    false
}

/// `FILE_TYPE_PIPE` on a handle this process already duplicated.
#[cfg(windows)]
#[allow(unsafe_code)]
fn is_pipe_handle(handle: *mut core::ffi::c_void) -> bool {
    const FILE_TYPE_PIPE: u32 = 0x0003;
    unsafe { GetFileType(handle) == FILE_TYPE_PIPE }
}

/// Pointer-sized handle value. Fails when `value` does not fit `usize`.
#[cfg(windows)]
fn handle_ptr(value: u64) -> Result<*mut core::ffi::c_void, String> {
    let n = usize::try_from(value).map_err(|_| format!("handle {value} does not fit usize"))?;
    Ok(n as *mut core::ffi::c_void)
}

/// Result of `GetHandleInformation`.
#[cfg(windows)]
struct HandleInfo {
    ok: bool,
    os: u32,
    error: String,
}

/// `DuplicateHandle` into this process. The caller closes `Ok`.
///
/// Failure keeps the Win32 code (`5` or `6` means the value is not a usable
/// handle here) and does not raise the Job invalid-handle exception.
#[cfg(windows)]
#[allow(unsafe_code)]
fn duplicate_raw(handle: *mut core::ffi::c_void) -> Result<*mut core::ffi::c_void, HandleInfo> {
    let mut copy = core::ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut copy,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(last_handle_info());
    }
    Ok(copy)
}

/// Win32 error from the most recent call.
#[cfg(windows)]
#[allow(unsafe_code)]
fn last_handle_info() -> HandleInfo {
    let err = std::io::Error::last_os_error();
    let os = u32::try_from(err.raw_os_error().unwrap_or(0)).unwrap_or(0);
    HandleInfo {
        ok: false,
        os,
        error: err.to_string(),
    }
}

/// Query `handle`. Failure keeps the Win32 code (`5` or `6` is a denial).
#[cfg(windows)]
#[allow(unsafe_code)]
fn handle_information(handle: *mut core::ffi::c_void) -> HandleInfo {
    let mut flags = 0_u32;
    let ok = unsafe { GetHandleInformation(handle, &mut flags) };
    if ok != 0 {
        return HandleInfo {
            ok: true,
            os: 0,
            error: String::new(),
        };
    }
    let err = std::io::Error::last_os_error();
    let os = u32::try_from(err.raw_os_error().unwrap_or(0)).unwrap_or(0);
    HandleInfo {
        ok: false,
        os,
        error: err.to_string(),
    }
}

/// Write every byte with an `OVERLAPPED` record. The product pipes are overlapped.
#[cfg(windows)]
#[allow(unsafe_code)]
fn overlapped_write(handle: *mut core::ffi::c_void, bytes: &[u8]) -> Result<(), String> {
    let n = overlapped_transfer(handle, bytes.as_ptr() as *mut u8, bytes.len(), true, 1000)?;
    if n != bytes.len() {
        return Err(format!("short write {n}"));
    }
    Ok(())
}

/// Read pending bytes. An empty buffer is EOF (the peer closed).
#[cfg(windows)]
#[allow(unsafe_code)]
fn overlapped_read(handle: *mut core::ffi::c_void, timeout_ms: u32) -> Result<Vec<u8>, String> {
    let mut buf = vec![0_u8; 256];
    let n = overlapped_transfer(handle, buf.as_mut_ptr(), buf.len(), false, timeout_ms)?;
    buf.truncate(n);
    Ok(buf)
}

/// One overlapped `ReadFile` or `WriteFile`. Timeout cancels the pending I/O.
#[cfg(windows)]
#[allow(unsafe_code)]
fn overlapped_transfer(
    handle: *mut core::ffi::c_void,
    buf: *mut u8,
    len: usize,
    write: bool,
    timeout_ms: u32,
) -> Result<usize, String> {
    let event = unsafe { CreateEventW(core::ptr::null_mut(), 1, 0, core::ptr::null()) };
    if event.is_null() {
        return Err(format!("CreateEventW: {}", std::io::Error::last_os_error()));
    }
    let _close = CloseEvent(event);
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event,
    };
    let len_u32 = u32::try_from(len).map_err(|_| format!("transfer length {len} exceeds u32"))?;
    let mut transferred = 0_u32;
    let ok = if write {
        unsafe {
            WriteFile(
                handle,
                buf.cast_const(),
                len_u32,
                &mut transferred,
                &mut overlapped,
            )
        }
    } else {
        unsafe { ReadFile(handle, buf, len_u32, &mut transferred, &mut overlapped) }
    };
    if ok != 0 {
        return Ok(transferred as usize);
    }
    let err = std::io::Error::last_os_error();
    let code = err.raw_os_error().unwrap_or(0);
    if code != ERROR_IO_PENDING {
        return Err(err.to_string());
    }
    let wait = unsafe { WaitForSingleObject(event, timeout_ms) };
    if wait == WAIT_TIMEOUT {
        unsafe {
            CancelIoEx(handle, &mut overlapped);
        }
        return Err("challenge was not accepted".into());
    }
    transferred = 0;
    let ok = unsafe { GetOverlappedResult(handle, &mut overlapped, &mut transferred, 0) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(transferred as usize)
}

/// Closes a Win32 event handle.
#[cfg(windows)]
struct CloseEvent(*mut core::ffi::c_void);

#[cfg(windows)]
impl Drop for CloseEvent {
    fn drop(&mut self) {
        if !self.0.is_null() {
            #[allow(unsafe_code)]
            unsafe {
                CloseHandle(self.0)
            };
        }
    }
}

/// `OVERLAPPED`. The offset pair is the 8-byte union; `event` follows it.
#[cfg(windows)]
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut core::ffi::c_void,
}

#[cfg(windows)]
const HANDLE_FLAG_INHERIT: u32 = 0x1;
#[cfg(windows)]
const DUPLICATE_SAME_ACCESS: u32 = 0x2;
#[cfg(windows)]
const ERROR_IO_PENDING: i32 = 997;
#[cfg(windows)]
const WAIT_TIMEOUT: u32 = 258;

#[cfg(windows)]
extern "system" {
    fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
    fn GetHandleInformation(handle: *mut core::ffi::c_void, flags: *mut u32) -> i32;
    fn GetCurrentProcess() -> *mut core::ffi::c_void;
    fn GetStdHandle(kind: u32) -> *mut core::ffi::c_void;
    fn GetFileType(handle: *mut core::ffi::c_void) -> u32;
    fn DuplicateHandle(
        source_process: *mut core::ffi::c_void,
        source: *mut core::ffi::c_void,
        target_process: *mut core::ffi::c_void,
        target: *mut *mut core::ffi::c_void,
        access: u32,
        inherit: i32,
        options: u32,
    ) -> i32;
    fn CreateEventW(
        attrs: *mut core::ffi::c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> *mut core::ffi::c_void;
    fn SetEvent(handle: *mut core::ffi::c_void) -> i32;
    fn WriteFile(
        handle: *mut core::ffi::c_void,
        buf: *const u8,
        len: u32,
        written: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn ReadFile(
        handle: *mut core::ffi::c_void,
        buf: *mut u8,
        len: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn GetOverlappedResult(
        handle: *mut core::ffi::c_void,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    fn WaitForSingleObject(handle: *mut core::ffi::c_void, millis: u32) -> u32;
    fn CancelIoEx(handle: *mut core::ffi::c_void, overlapped: *mut Overlapped) -> i32;
    fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
}

/// One Job slot: this probe, not `cmd /c` and not `ping.exe`.
///
/// `Stdio::null()` opens `\\.\NUL`. An AppContainer token is denied that
/// device, and `Command::spawn` returns `ERROR_ACCESS_DENIED` before
/// `CreateProcess`. Pipes are closed in the parent after spawn so the child
/// still has no console. `DETACHED_PROCESS` keeps `conhost.exe` out of the
/// Job; a console host would consume a second active-process slot.
fn spawn_ping() -> serde_json::Value {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        let image = match std::env::current_exe() {
            Ok(path) => path,
            Err(err) => {
                return serde_json::json!({
                    "ok": false,
                    "error": err.to_string(),
                    "os": err.raw_os_error(),
                });
            }
        };
        let image = win32_spawn_path(&image);
        let mut cmd = std::process::Command::new(&image);
        cmd.arg("--hold-job-slot")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(DETACHED_PROCESS);
        if let Some(dir) = image.parent() {
            cmd.current_dir(dir);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                // Close the parent's pipe ends. Dropping `Child` does not
                // terminate the process; it keeps the Job slot.
                drop(child.stdin.take());
                drop(child.stdout.take());
                drop(child.stderr.take());
                drop(child);
                serde_json::json!({
                    "ok": true,
                    "pid": pid,
                    "image": image.display().to_string(),
                })
            }
            Err(err) => serde_json::json!({
                "ok": false,
                "error": err.to_string(),
                "os": err.raw_os_error(),
                "image": image.display().to_string(),
            }),
        }
    }
    #[cfg(not(windows))]
    {
        serde_json::json!({ "ok": false, "error": "ping Job slots are Windows-only" })
    }
}

/// Strip a `\\?\` prefix. AppContainer `CreateProcess` rejects that form.
#[cfg(windows)]
fn win32_spawn_path(path: &std::path::Path) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(text.as_ref());
    std::path::PathBuf::from(stripped)
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
        #[repr(C)]
        struct BasicAccounting {
            total_user_time: i64,
            total_kernel_time: i64,
            this_period_total_user_time: i64,
            this_period_total_kernel_time: i64,
            total_page_fault_count: u32,
            total_processes: u32,
            active_processes: u32,
            total_terminated_processes: u32,
        }
        const JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: i32 = 1;
        let mut accounting = std::mem::zeroed::<BasicAccounting>();
        let accounting_ok = QueryInformationJobObject(
            core::ptr::null_mut(),
            JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
            &mut accounting as *mut BasicAccounting as *mut _,
            std::mem::size_of::<BasicAccounting>() as u32,
            core::ptr::null_mut(),
        );
        let accounting_error = if accounting_ok == 0 {
            GetLastError()
        } else {
            0
        };
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
            "active_processes": if accounting_ok != 0 {
                Some(accounting.active_processes)
            } else {
                None::<u32>
            },
            "accounting_error": accounting_error,
        })
    }
}

/// OAuth callback tunnel plus the probe's Unix socket mock in the guest IPC directory.
///
/// `payload` is the host callback socket (`cb.sock` or a Windows pipe). The
/// guest binds `{GUEST_IPC_DIR}/.s.PGSQL.5432`, accepts one client, and accepts
/// one tunneled browser stream. `PGOK` is this mock, not the PostgreSQL adapter.
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

/// Bind the `.s.PGSQL.5432` mock socket and accept the host callback tunnel.
///
/// The four-byte `PGOK` reply is this probe, not a PostgreSQL adapter startup.
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
                "error": format!("probe unix socket mock bind {}: {err}", pg_path.display()),
            })
        }
    };
    let postgres = tokio::spawn(async move {
        let (mut sock, _) = listener
            .accept()
            .await
            .map_err(|err| format!("probe unix socket mock accept: {err}"))?;
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(std::time::Duration::from_secs(20), sock.read(&mut buf))
            .await
            .map_err(|_| "probe unix socket mock read timed out".to_string())?
            .map_err(|err| format!("probe unix socket mock read: {err}"))?;
        sock.write_all(b"PGOK")
            .await
            .map_err(|err| format!("probe unix socket mock write: {err}"))?;
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
    // Stay in the mux until the host has read the echo and half-closes. Returning
    // here drops the tunnel while those bytes may still be queued.
    let _ = browser.shutdown().await;
    let mut tail = [0u8; 8];
    let _ = tokio::time::timeout(std::time::Duration::from_secs(20), browser.read(&mut tail)).await;
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
                let proxy_write = std::env::var(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV).ok();
                serde_json::json!({
                    "ok": true,
                    "keys": keys,
                    "socket_proxy": proxy,
                    "socket_proxy_write": proxy_write,
                })
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
            "session_challenge" => session_challenge(),
            "unrelated_challenge" => unrelated_challenge(arg(&params, "payload")),
            "present_challenge" => present_foreign_challenge(arg(&params, "payload")).await,
            "drive_foreign" => drive_foreign_endpoint(arg(&params, "payload"), host, port),
            "unrelated_drive" => unrelated_drive(host, port),
            "unlisted_handle" => unlisted_handle(arg(&params, "payload")),
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
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--endpoint-challenge") {
        let spec = args.next().unwrap_or_default();
        let write_spec = args.next();
        println!("{}", child_endpoint_challenge(&spec, write_spec.as_deref()));
        return Ok(());
    }
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--endpoint-connect") {
        let spec = args.next().unwrap_or_default();
        let write_spec = args.next().unwrap_or_default();
        let host = args.next().unwrap_or_default();
        let port = args.next().unwrap_or_default().parse().unwrap_or(0);
        let write = if write_spec.is_empty() {
            None
        } else {
            Some(write_spec.as_str())
        };
        println!("{}", child_endpoint_connect(&spec, write, &host, port));
        return Ok(());
    }
    // One Job slot. The session Job kills this process when the test drops it.
    if std::env::args().any(|arg| arg == "--hold-job-slot") {
        std::thread::sleep(Duration::from_secs(180));
        return Ok(());
    }
    serve(Root).await?;
    Ok(())
}
