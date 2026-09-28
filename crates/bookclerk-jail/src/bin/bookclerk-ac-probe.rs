//! Windows AppContainer probe used by jail integration tests.
//!
//! Reports TokenIsAppContainer, path read/write, cwd/temp, optional TEMP
//! create/read/delete, and child-spawn helpers for Job Object tests. On
//! non-Windows hosts this binary exits with an error.

#![cfg_attr(windows, allow(unsafe_code))]

use std::process::ExitCode;

/// Entry point for the AppContainer probe (Windows-only body).
fn main() -> ExitCode {
    #[cfg(not(windows))]
    {
        eprintln!("bookclerk-ac-probe: Windows only");
        ExitCode::from(2)
    }
    #[cfg(windows)]
    {
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("bookclerk-ac-probe: {err}");
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(windows)]
/// Parses CLI flags and reports AppContainer / filesystem probe results as JSON.
///
/// # Errors
///
/// Returns an error when arguments are invalid or a probe step fails.
fn run() -> Result<(), String> {
    use std::env;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    let mut args = env::args().skip(1);
    let mut wait_before: Option<PathBuf> = None;
    let mut wait_after: Option<PathBuf> = None;
    let mut wait_gone: Option<PathBuf> = None;
    let mut wait_timeout = Duration::from_secs(45);
    let mut reads: Vec<PathBuf> = Vec::new();
    let mut writes: Vec<PathBuf> = Vec::new();
    let mut deny_reads: Vec<PathBuf> = Vec::new();
    let mut signal: Option<PathBuf> = None;
    let mut temp_roundtrip = false;
    let mut spawn_child = false;
    let mut exit_immediately = false;
    let mut hold_ms: Option<u64> = None;
    let mut after_hold: Option<PathBuf> = None;
    let mut loopback_self = false;
    let mut connect_tcp: Option<(String, u16)> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wait-before" => {
                wait_before = Some(PathBuf::from(
                    args.next().ok_or("--wait-before needs a path")?,
                ));
            }
            "--wait-after" => {
                wait_after = Some(PathBuf::from(
                    args.next().ok_or("--wait-after needs a path")?,
                ));
            }
            "--wait-gone" => {
                wait_gone = Some(PathBuf::from(
                    args.next().ok_or("--wait-gone needs a path")?,
                ));
            }
            "--wait-timeout-ms" => {
                let ms: u64 = args
                    .next()
                    .ok_or("--wait-timeout-ms needs a value")?
                    .parse()
                    .map_err(|err| format!("bad --wait-timeout-ms: {err}"))?;
                wait_timeout = Duration::from_millis(ms);
            }
            "--read" => {
                reads.push(PathBuf::from(args.next().ok_or("--read needs a path")?));
            }
            "--write" => {
                writes.push(PathBuf::from(args.next().ok_or("--write needs a path")?));
            }
            "--deny-read" => {
                deny_reads.push(PathBuf::from(
                    args.next().ok_or("--deny-read needs a path")?,
                ));
            }
            "--signal" => {
                signal = Some(PathBuf::from(args.next().ok_or("--signal needs a path")?));
            }
            "--temp-roundtrip" => {
                temp_roundtrip = true;
            }
            "--spawn-child" => {
                spawn_child = true;
            }
            "--exit-immediately" => {
                exit_immediately = true;
            }
            "--hold-ms" => {
                hold_ms = Some(
                    args.next()
                        .ok_or("--hold-ms needs a value")?
                        .parse()
                        .map_err(|err| format!("bad --hold-ms: {err}"))?,
                );
            }
            "--after-hold" => {
                after_hold = Some(PathBuf::from(
                    args.next().ok_or("--after-hold needs a path")?,
                ));
            }
            "--loopback-self" => loopback_self = true,
            "--connect-tcp" => {
                let host = args.next().ok_or("--connect-tcp needs host")?;
                let port: u16 = args
                    .next()
                    .ok_or("--connect-tcp needs port")?
                    .parse()
                    .map_err(|err| format!("bad --connect-tcp port: {err}"))?;
                connect_tcp = Some((host, port));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if exit_immediately {
        return Ok(());
    }

    if let Some((host, port)) = connect_tcp {
        use std::net::TcpStream;
        let mut stream = TcpStream::connect((host.as_str(), port))
            .map_err(|err| format!("connect-tcp {host}:{port}: {err}"))?;
        stream
            .write_all(b"ping")
            .map_err(|err| format!("connect-tcp write: {err}"))?;
        let mut buf = [0u8; 4];
        stream
            .read_exact(&mut buf)
            .map_err(|err| format!("connect-tcp read: {err}"))?;
        if &buf != b"ping" {
            return Err(format!("connect-tcp echo mismatch: {buf:?}"));
        }
        return Ok(());
    }

    if loopback_self {
        return run_loopback_self();
    }

    if let Some(path) = &wait_before {
        wait_until(
            || path.exists(),
            wait_timeout,
            &format!("wait-before {}", path.display()),
        )?;
    }

    let is_app_container = token_is_app_container()?;
    let cwd = env::current_dir().map_err(|err| err.to_string())?;
    let localappdata = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let temp = env::var_os("TEMP").map(PathBuf::from);
    let tmp = env::var_os("TMP").map(PathBuf::from);

    let mut read_results = Vec::new();
    for path in &reads {
        read_results.push(serde_json::json!({
            "path": path.display().to_string(),
            "ok": fs::read(path).is_ok(),
        }));
    }

    let mut write_results = Vec::new();
    for path in &writes {
        let marker = path.join("probe-write.txt");
        let ok = fs::write(&marker, b"probe").is_ok();
        if ok {
            let _ = fs::remove_file(&marker);
        }
        write_results.push(serde_json::json!({
            "path": path.display().to_string(),
            "ok": ok,
        }));
    }

    let mut deny_results = Vec::new();
    for path in &deny_reads {
        deny_results.push(serde_json::json!({
            "path": path.display().to_string(),
            "ok": fs::read(path).is_ok(),
        }));
    }

    let mut temp_ok = None;
    if temp_roundtrip {
        let dir = temp
            .clone()
            .ok_or_else(|| "TEMP unset for --temp-roundtrip".to_string())?;
        fs::create_dir_all(&dir)
            .map_err(|err| format!("TEMP create_dir_all {}: {err}", dir.display()))?;
        let marker = dir.join("bookclerk-temp-roundtrip.txt");
        fs::write(&marker, b"temp-ok")
            .map_err(|err| format!("TEMP write {}: {err}", marker.display()))?;
        let body = fs::read(&marker).map_err(|err| format!("TEMP read: {err}"))?;
        fs::remove_file(&marker).map_err(|err| format!("TEMP delete: {err}"))?;
        temp_ok = Some(body == b"temp-ok");
    }

    let mut child_pid = None;
    if spawn_child {
        let child = Command::new(env::current_exe().map_err(|err| err.to_string())?)
            .arg("--hold-ms")
            .arg("5000")
            .spawn()
            .map_err(|err| format!("spawn child: {err}"))?;
        child_pid = Some(child.id());
        // Intentionally leak the Child handle so the descendant stays alive for
        // Job membership checks by the host test.
        std::mem::forget(child);
    }

    let report = serde_json::json!({
        "is_app_container": is_app_container,
        "pid": std::process::id(),
        "cwd": cwd.display().to_string(),
        "localappdata": localappdata.as_ref().map(|p| p.display().to_string()),
        "temp": temp.as_ref().map(|p| p.display().to_string()),
        "tmp": tmp.as_ref().map(|p| p.display().to_string()),
        "reads": read_results,
        "writes": write_results,
        "deny_reads": deny_results,
        "temp_roundtrip_ok": temp_ok,
        "child_pid": child_pid,
    });

    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{report}").map_err(|err| err.to_string())?;
    stdout.flush().map_err(|err| err.to_string())?;

    if let Some(path) = &signal {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, b"ready").map_err(|err| format!("signal {}: {err}", path.display()))?;
    }

    if let Some(ms) = hold_ms {
        std::thread::sleep(Duration::from_millis(ms));
    }

    if let Some(path) = &after_hold {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, b"done").map_err(|err| format!("after-hold {}: {err}", path.display()))?;
    }

    if let Some(path) = &wait_after {
        wait_until(
            || path.exists(),
            wait_timeout,
            &format!("wait-after {}", path.display()),
        )?;
    }

    if let Some(path) = &wait_gone {
        wait_until(
            || !path.exists(),
            wait_timeout,
            &format!("wait-gone {}", path.display()),
        )?;
        if let Some(first) = writes.first() {
            let marker = first.join("probe-after-peer.txt");
            fs::write(&marker, b"still-ok")
                .map_err(|err| format!("post-peer write failed: {err}"))?;
            let _ = fs::remove_file(&marker);
        }
        let follow_up = serde_json::json!({
            "phase": "after-peer-exit",
            "writes_still_ok": true,
        });
        writeln!(stdout, "{follow_up}").map_err(|err| err.to_string())?;
        stdout.flush().map_err(|err| err.to_string())?;
    }

    Ok(())
}

#[cfg(windows)]
/// Bind `127.0.0.1:0` and have a same-container child connect back (E3).
fn run_loopback_self() -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Command;
    use std::time::Duration;

    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|err| format!("bind loopback: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("local_addr: {err}"))?
        .port();
    listener
        .set_nonblocking(false)
        .map_err(|err| format!("set blocking: {err}"))?;
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    let mut child = Command::new(&exe)
        .arg("--connect-tcp")
        .arg("127.0.0.1")
        .arg(port.to_string())
        .spawn()
        .map_err(|err| format!("spawn connect child: {err}"))?;
    let (mut stream, _) = listener
        .accept()
        .map_err(|err| format!("accept loopback: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("read timeout: {err}"))?;
    let mut buf = [0u8; 4];
    stream
        .read_exact(&mut buf)
        .map_err(|err| format!("loopback read: {err}"))?;
    stream
        .write_all(&buf)
        .map_err(|err| format!("loopback write: {err}"))?;
    let status = child
        .wait()
        .map_err(|err| format!("wait connect child: {err}"))?;
    if !status.success() {
        return Err(format!("connect child failed: {status:?}"));
    }
    println!(
        "{}",
        serde_json::json!({
            "loopback_ok": true,
            "port": port,
            "is_app_container": token_is_app_container()?,
        })
    );
    Ok(())
}

#[cfg(windows)]
/// Polls `pred` until it succeeds or `timeout` elapses.
///
/// # Errors
///
/// Returns an error labeled `label` when the timeout expires.
fn wait_until(
    mut pred: impl FnMut() -> bool,
    timeout: std::time::Duration,
    label: &str,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    while !pred() {
        if start.elapsed() > timeout {
            return Err(format!("timed out: {label}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(windows)]
/// Returns whether the current process token is an AppContainer token.
///
/// # Errors
///
/// Returns an error when Win32 token queries fail.
fn token_is_app_container() -> Result<bool, String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenIsAppContainer, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|err| format!("OpenProcessToken failed: {err}"))?;
        let mut is_ac: u32 = 0;
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenIsAppContainer,
            Some((&mut is_ac as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        ok.map_err(|err| format!("GetTokenInformation(TokenIsAppContainer) failed: {err}"))?;
        Ok(is_ac != 0)
    }
}
