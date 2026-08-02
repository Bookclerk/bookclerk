//! Windows AppContainer probe used by jail integration tests.
//!
//! Reports TokenIsAppContainer, attempts path read/write, and prints cwd / temp
//! environment as JSON. On non-Windows hosts this binary exits with an error.
//!
//! Ordering: optional initial wait → probes → JSON report → optional signal →
//! optional post-work wait / peer-exit wait.

#![cfg_attr(windows, allow(unsafe_code))]

use std::process::ExitCode;

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
fn run() -> Result<(), String> {
    use std::env;
    use std::fs;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::time::Duration;

    let mut args = env::args().skip(1);
    let mut wait_before: Option<PathBuf> = None;
    let mut wait_after: Option<PathBuf> = None;
    let mut wait_gone: Option<PathBuf> = None;
    let mut wait_timeout = Duration::from_secs(45);
    let mut reads: Vec<PathBuf> = Vec::new();
    let mut writes: Vec<PathBuf> = Vec::new();
    let mut signal: Option<PathBuf> = None;

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
            "--signal" => {
                signal = Some(PathBuf::from(args.next().ok_or("--signal needs a path")?));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
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

    let report = serde_json::json!({
        "is_app_container": is_app_container,
        "cwd": cwd.display().to_string(),
        "localappdata": localappdata.as_ref().map(|p| p.display().to_string()),
        "temp": temp.as_ref().map(|p| p.display().to_string()),
        "tmp": tmp.as_ref().map(|p| p.display().to_string()),
        "reads": read_results,
        "writes": write_results,
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
